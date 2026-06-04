//! Ogg muxer: pack incoming packets into pages.
//!
//! Strategy: maintain one buffered page per logical stream. Pack a packet by
//! appending its bytes and lacing values. Flush the page whenever it reaches
//! the 255-segment limit, when an explicit flush is requested, or at trailer
//! time. Granule positions come from `Packet::pts` for non-header packets.

use std::collections::HashMap;
use std::io::Write;

use oxideav_core::{CodecId, Error, Packet, Result, StreamInfo};
use oxideav_core::{Muxer, WriteSeek};

use crate::codec_id;
use crate::page::{self, flags, lace, Page};
use crate::skeleton::Skeleton;

pub fn open(output: Box<dyn WriteSeek>, streams: &[StreamInfo]) -> Result<Box<dyn Muxer>> {
    open_with_skeleton(output, streams, None)
}

/// Open an Ogg muxer with an optional Skeleton metadata bitstream.
///
/// When `skeleton` is `Some`, the muxer emits a Skeleton logical bitstream
/// before any content bitstream pages, per the encapsulation order
/// described in `docs/container/ogg/ogg-skeleton-3.0.md` and
/// `docs/container/ogg/ogg-skeleton-4.0.md`:
///
/// 1. The Skeleton `fishead\0` BOS is the very first BOS page in the
///    physical stream so decoders can identify it straight away.
/// 2. The BOS pages of all other logical bitstreams follow (existing
///    `write_header` flow, unchanged).
/// 3. Secondary header pages — Skeleton's `fisbone\0` packets plus
///    every content codec's remaining headers — interleave next.
/// 4. Skeleton 4.0 `index\0` packets, if any, ride alongside the
///    fisbones in the secondary-header section.
/// 5. The Skeleton EOS page (an empty-payload packet sitting on its
///    own page) closes the control section before any content data
///    page appears.
///
/// Each Skeleton packet is emitted on its own page with the carrier's
/// own serial number and a monotonically increasing sequence number,
/// matching the per-packet pagination the existing 3.0 / 4.0 streams
/// in the wild use.
///
/// If `skeleton.serial` is `None`, a serial is derived (one past the
/// largest content stream's derived serial) so it cannot collide with
/// any content bitstream the muxer is already writing.
///
/// If `skeleton` is `None`, this function reduces to [`open`] — no
/// Skeleton bytes are written and the output is byte-identical to the
/// pre-Skeleton muxer.
pub fn open_with_skeleton(
    output: Box<dyn WriteSeek>,
    streams: &[StreamInfo],
    skeleton: Option<Skeleton>,
) -> Result<Box<dyn Muxer>> {
    let mut per_stream = HashMap::with_capacity(streams.len());
    let mut max_serial: u32 = 0;
    for s in streams {
        let serial = derive_serial(s);
        max_serial = max_serial.max(serial);
        let headers_remaining = codec_id::header_packet_count(&s.params.codec_id);
        per_stream.insert(
            s.index,
            StreamWriter {
                serial,
                seq_no: 0,
                buffered: PageBuilder::new(),
                headers_remaining,
                bos_emitted: false,
                pending_bytes: None,
            },
        );
    }
    let skeleton_writer = skeleton.map(|sk| {
        let serial = sk.serial.unwrap_or(max_serial.wrapping_add(1));
        SkeletonWriter {
            skel: sk,
            serial,
            seq_no: 0,
        }
    });
    Ok(Box::new(OggMuxer {
        output,
        streams: streams.to_vec(),
        per_stream,
        stream_order: streams.iter().map(|s| s.index).collect(),
        header_written: false,
        trailer_written: false,
        skeleton: skeleton_writer,
    }))
}

/// Derive a stable serial number for a stream. Real-world muxers use random
/// 32-bit numbers; we use the stream index for determinism (which makes
/// remux output byte-stable when the input numbering is also dense from 0).
fn derive_serial(s: &StreamInfo) -> u32 {
    s.index
}

struct OggMuxer {
    output: Box<dyn WriteSeek>,
    /// Stream descriptors retained so write_header can reconstruct the
    /// codec-specific setup packets from each stream's extradata.
    streams: Vec<StreamInfo>,
    per_stream: HashMap<u32, StreamWriter>,
    stream_order: Vec<u32>,
    header_written: bool,
    trailer_written: bool,
    /// Optional Skeleton metadata bitstream. When present, its fishead
    /// BOS is emitted first (before any content BOS) and its EOS page
    /// is emitted after the last content secondary header, before any
    /// content data page is written — per the encapsulation order in
    /// `docs/container/ogg/ogg-skeleton-{3,4}.0.md`.
    skeleton: Option<SkeletonWriter>,
}

struct SkeletonWriter {
    skel: Skeleton,
    serial: u32,
    seq_no: u32,
}

struct StreamWriter {
    serial: u32,
    seq_no: u32,
    buffered: PageBuilder,
    headers_remaining: usize,
    bos_emitted: bool,
    /// Bytes of the most recently finalized page, held back until either
    /// another page is flushed (in which case it's written) or the trailer
    /// runs (in which case it gets EOS set and its CRC patched). This makes
    /// the EOS marker sit on a real data page instead of an empty trailing one.
    pending_bytes: Option<Vec<u8>>,
}

#[derive(Default)]
struct PageBuilder {
    /// Lacing values for the page so far (≤ 255 entries).
    lacing: Vec<u8>,
    /// Concatenated segment data for the page so far.
    data: Vec<u8>,
    /// First-segment-on-page is the continuation of an unfinished packet
    /// from the previous page.
    starts_continued: bool,
    /// Granule position to record on this page — set to the most recent
    /// completed packet's pts. -1 means "no packet ends here".
    granule_position: i64,
}

impl PageBuilder {
    fn new() -> Self {
        Self {
            granule_position: -1,
            ..Default::default()
        }
    }

    fn is_empty(&self) -> bool {
        self.lacing.is_empty()
    }
}

impl OggMuxer {
    fn writer_for(&mut self, stream_index: u32) -> Result<&mut StreamWriter> {
        self.per_stream
            .get_mut(&stream_index)
            .ok_or_else(|| Error::invalid(format!("unknown stream index {stream_index}")))
    }

    /// Emit a single Skeleton-stream page carrying `packet_bytes` as one
    /// whole packet, with the supplied header flags. Granule is always 0
    /// (Skeleton itself defines no time-axis content) and the sequence
    /// number advances per call.
    fn write_skeleton_page(&mut self, packet_bytes: &[u8], page_flags: u8) -> Result<()> {
        let sk = self.skeleton.as_mut().expect("skeleton writer present");
        let lacing = lace(packet_bytes.len());
        let page = Page {
            flags: page_flags,
            granule_position: 0,
            serial: sk.serial,
            seq_no: sk.seq_no,
            lacing,
            data: packet_bytes.to_vec(),
        };
        sk.seq_no = sk.seq_no.wrapping_add(1);
        let bytes = page.to_bytes();
        self.output.write_all(&bytes)?;
        Ok(())
    }

    /// Emit the Skeleton fishead BOS page. Must run before any content
    /// stream's BOS page, per the Skeleton 3.0 / 4.0 encapsulation order.
    fn write_skeleton_fishead_bos(&mut self) -> Result<()> {
        let head_bytes = {
            let sk = self.skeleton.as_ref().expect("skeleton writer present");
            sk.skel
                .head
                .as_ref()
                .map(|h| h.to_bytes())
                .unwrap_or_else(|| {
                    // Caller attached a Skeleton with no fishead — emit a
                    // minimal 4.0 fishead (zero-valued presentation time /
                    // basetime / segment-length / content-byte-offset)
                    // so the BOS is structurally valid for downstream
                    // parsers.
                    crate::skeleton::FisHead::new(crate::skeleton::Version::V4_0).to_bytes()
                })
        };
        self.write_skeleton_page(&head_bytes, flags::FIRST_PAGE)
    }

    /// Emit every fisbone + index packet sitting on the attached
    /// Skeleton (one packet per page, each at granule 0), then close
    /// the Skeleton control section with an empty-payload EOS page.
    /// Per the spec, the EOS packet appears by itself on its own page.
    fn write_skeleton_fisbones_and_eos(&mut self) -> Result<()> {
        // Take ownership of the secondary-header byte sequences first so
        // we can hand each one to write_skeleton_page (which borrows
        // self.skeleton mutably for seq_no advancement).
        let payloads: Vec<Vec<u8>> = {
            let sk = self.skeleton.as_ref().expect("skeleton writer present");
            let mut out = Vec::with_capacity(sk.skel.bones.len() + sk.skel.indexes.len());
            for bone in &sk.skel.bones {
                out.push(bone.to_bytes());
            }
            for idx in &sk.skel.indexes {
                out.push(idx.to_bytes());
            }
            out
        };
        for payload in &payloads {
            self.write_skeleton_page(payload, 0)?;
        }
        // Empty packet on its own EOS page closes the Skeleton control
        // section (per spec). A zero-byte packet lacing-encodes as a
        // single `0` lacing value (lace(0) → [0]); the on-wire page
        // therefore carries one segment whose body length is zero.
        self.write_skeleton_page(&[], flags::LAST_PAGE)
    }

    /// Finalize the buffered page for `stream_index`. The newly built page
    /// becomes the writer's *pending* page; whatever was previously pending
    /// gets written out to the underlying sink.
    fn flush_page(&mut self, stream_index: u32, force: bool) -> Result<()> {
        let writer = self
            .per_stream
            .get_mut(&stream_index)
            .ok_or_else(|| Error::invalid(format!("unknown stream index {stream_index}")))?;
        if writer.buffered.is_empty() && !force {
            return Ok(());
        }
        let mut page_flags = 0u8;
        if writer.buffered.starts_continued {
            page_flags |= flags::CONTINUED;
        }
        if !writer.bos_emitted {
            page_flags |= flags::FIRST_PAGE;
            writer.bos_emitted = true;
        }
        let page = Page {
            flags: page_flags,
            granule_position: writer.buffered.granule_position,
            serial: writer.serial,
            seq_no: writer.seq_no,
            lacing: std::mem::take(&mut writer.buffered.lacing),
            data: std::mem::take(&mut writer.buffered.data),
        };
        writer.seq_no = writer.seq_no.wrapping_add(1);
        writer.buffered.starts_continued = page.lacing.last().copied() == Some(255);
        writer.buffered.granule_position = -1;
        let new_bytes = page.to_bytes();

        // Write whatever was pending before, then queue the new bytes.
        if let Some(prev) = writer.pending_bytes.take() {
            self.output.write_all(&prev)?;
        }
        let writer = self.writer_for(stream_index)?;
        writer.pending_bytes = Some(new_bytes);
        Ok(())
    }
}

impl Muxer for OggMuxer {
    fn format_name(&self) -> &str {
        "ogg"
    }

    fn write_header(&mut self) -> Result<()> {
        if self.header_written {
            return Err(Error::other("Ogg muxer: write_header called twice"));
        }
        self.header_written = true;
        // RFC 3533 §6: every logical bitstream's BOS page must precede any
        // non-BOS page. We emit BOS pages directly here (bypassing the
        // per-stream pending-bytes mechanism used for EOS) so BOS pages for
        // all streams land at the very front of the output.
        let stream_clone = self.streams.clone();
        let mut header_queues: Vec<(u32, oxideav_core::TimeBase, Vec<Vec<u8>>)> =
            Vec::with_capacity(stream_clone.len());
        for s in &stream_clone {
            let packets = extract_codec_headers(&s.params.codec_id, &s.params.extradata);
            header_queues.push((s.index, s.time_base, packets));
        }
        // Step 0: Skeleton fishead BOS — the very first BOS page in the
        // physical stream, per `docs/container/ogg/ogg-skeleton-{3,4}.0.md`
        // ("the Skeleton bos page is the very first bos page in the Ogg
        // stream"). The fisbones / indexes / EOS for Skeleton are emitted
        // in step 3 once content BOS pages are out.
        if self.skeleton.is_some() {
            self.write_skeleton_fishead_bos()?;
        }
        // Step 1: BOS page per content stream, written immediately.
        for (idx, _tb, packets) in &header_queues {
            let Some(first) = packets.first() else {
                continue;
            };
            let writer = self.writer_for(*idx)?;
            if writer.headers_remaining == 0 {
                continue;
            }
            let lacing = lace(first.len());
            let page = Page {
                flags: flags::FIRST_PAGE,
                granule_position: 0,
                serial: writer.serial,
                seq_no: writer.seq_no,
                lacing,
                data: first.clone(),
            };
            writer.seq_no = writer.seq_no.wrapping_add(1);
            writer.bos_emitted = true;
            writer.headers_remaining -= 1;
            let bytes = page.to_bytes();
            self.output.write_all(&bytes)?;
        }
        // Step 2: remaining content-codec header packets — normal
        // write_packet flow.
        for (idx, tb, packets) in &header_queues {
            for hp in packets.iter().skip(1) {
                let pkt = Packet::new(*idx, *tb, hp.clone());
                self.write_packet(&pkt)?;
            }
        }
        // Step 3: Skeleton secondary headers (fisbone packets + any
        // 4.0 index packets) followed by the Skeleton EOS page. The
        // EOS must precede any content data page (the spec requires
        // it ends the control section "before any data pages of the
        // other logical bitstreams appear"). Subsequent write_packet
        // calls supply those content data pages.
        if self.skeleton.is_some() {
            self.write_skeleton_fisbones_and_eos()?;
        }
        Ok(())
    }

    fn write_packet(&mut self, packet: &Packet) -> Result<()> {
        if !self.header_written {
            return Err(Error::other("Ogg muxer: write_header not called"));
        }
        let stream_index = packet.stream_index;
        let lacing_for_packet = lace(packet.data.len());

        let writer = self.writer_for(stream_index)?;
        let is_header = writer.headers_remaining > 0;

        // Flush early if this packet's lacing wouldn't fit in 255 segments.
        if writer.buffered.lacing.len() + lacing_for_packet.len() > 255 {
            self.flush_page(stream_index, false)?;
        }

        let writer = self.writer_for(stream_index)?;
        writer.buffered.lacing.extend_from_slice(&lacing_for_packet);
        writer.buffered.data.extend_from_slice(&packet.data);

        if is_header {
            // Header packets each get their own page with granule 0.
            writer.headers_remaining -= 1;
            writer.buffered.granule_position = 0;
            self.flush_page(stream_index, true)?;
            return Ok(());
        }

        // Audio/video packet. The page's granule_position is set from the
        // most recent pts seen on this page (this packet's pts wins if
        // present; otherwise the buffered value carries through). A new
        // page is flushed when the source signaled a page boundary via
        // `unit_boundary`. This separates *pts-per-packet* (decoders care)
        // from *page boundaries* (Ogg cares).
        if let Some(pts) = packet.pts {
            writer.buffered.granule_position = pts;
        }
        if packet.flags.unit_boundary {
            self.flush_page(stream_index, true)?;
        }

        Ok(())
    }

    fn write_trailer(&mut self) -> Result<()> {
        if self.trailer_written {
            return Ok(());
        }
        let order = self.stream_order.clone();
        for idx in order {
            // Drain any in-progress builder into pending_bytes.
            let needs_flush = {
                let writer = self.writer_for(idx)?;
                !writer.buffered.is_empty()
            };
            if needs_flush {
                self.flush_page(idx, true)?;
            }
            // Whatever's in pending_bytes is the truly last page — set EOS,
            // recompute its CRC, write it.
            let writer = self.writer_for(idx)?;
            if let Some(mut bytes) = writer.pending_bytes.take() {
                if bytes.len() >= 27 {
                    bytes[5] |= flags::LAST_PAGE;
                    // Zero out checksum field, recompute, patch back.
                    bytes[22..26].fill(0);
                    let crc = crate::crc::checksum(&bytes);
                    bytes[22..26].copy_from_slice(&crc.to_le_bytes());
                }
                self.output.write_all(&bytes)?;
            }
        }
        self.output.flush()?;
        self.trailer_written = true;
        Ok(())
    }
}

// Keep imports honest for downstream consumers.
#[allow(dead_code)]
const _SANITY: () = {
    let _ = page::CAPTURE_PATTERN;
};

/// Inverse of `oxideav_ogg::demux::build_codec_private`: turn a stream's
/// extradata back into the per-codec sequence of header packets that an Ogg
/// stream needs at its start.
fn extract_codec_headers(codec_id: &CodecId, extradata: &[u8]) -> Vec<Vec<u8>> {
    if extradata.is_empty() {
        return Vec::new();
    }
    match codec_id.as_str() {
        "vorbis" => parse_xiph_lacing(extradata).unwrap_or_default(),
        "opus" => {
            // OpusHead followed by a synthetic minimal OpusTags. (Original
            // tags are dropped during demux — they're not load-bearing.)
            let head = extradata.to_vec();
            let mut tags = Vec::with_capacity(20);
            tags.extend_from_slice(b"OpusTags");
            tags.extend_from_slice(&0u32.to_le_bytes()); // vendor string length = 0
            tags.extend_from_slice(&0u32.to_le_bytes()); // user comment count = 0
            vec![head, tags]
        }
        _ => vec![extradata.to_vec()],
    }
}

/// Parse a Xiph-laced 3-packet header blob (Vorbis/Theora layout). The first
/// byte is `(packet_count - 1)`, followed by `(packet_count - 1)` lacing
/// records (each a series of 0xFF terminators ending in a value < 0xFF).
fn parse_xiph_lacing(buf: &[u8]) -> Option<Vec<Vec<u8>>> {
    if buf.is_empty() {
        return None;
    }
    let n_packets = buf[0] as usize + 1;
    let mut sizes = Vec::with_capacity(n_packets);
    let mut i = 1usize;
    for _ in 0..n_packets - 1 {
        let mut s = 0usize;
        loop {
            if i >= buf.len() {
                return None;
            }
            let b = buf[i];
            i += 1;
            s += b as usize;
            if b < 255 {
                break;
            }
        }
        sizes.push(s);
    }
    let used: usize = sizes.iter().sum();
    if i + used > buf.len() {
        return None;
    }
    let last_size = buf.len() - i - used;
    sizes.push(last_size);
    let mut packets = Vec::with_capacity(n_packets);
    for sz in sizes {
        if i + sz > buf.len() {
            return None;
        }
        packets.push(buf[i..i + sz].to_vec());
        i += sz;
    }
    Some(packets)
}
