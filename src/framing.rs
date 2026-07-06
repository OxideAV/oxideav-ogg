//! Buffer-level packet ⇄ page framing (RFC 3533 §4–§6).
//!
//! The crate's [`crate::mux`] / [`crate::demux`] modules speak the
//! [`oxideav_core::Muxer`] / [`oxideav_core::Demuxer`] traits over
//! seekable I/O, derive granule positions from `Packet::pts`, and
//! reconstruct codec header packets from `StreamInfo::extradata`. That
//! is the right layer for container-level consumers, but codec crates
//! often want something smaller: "here are my packets and their granule
//! positions, give me the bytes of one logical bitstream" (and the
//! inverse). This module is that layer — the packet→segment→page
//! encapsulation of RFC 3533 §5–§6 with no I/O, no `StreamInfo`, and no
//! codec knowledge:
//!
//! * **Write side** — [`PageWriter`] runs the packet→page
//!   encapsulation for one logical bitstream: packets are chopped into
//!   255-byte lacing segments, pages auto-emit when the 255-segment
//!   table fills mid-packet (setting the next page's `continued` flag,
//!   RFC 3533 §6 field 3), the page-level granule position records the
//!   stamp of the **last packet completed on the page** (`-1` when no
//!   packet completes, §6 field 4), the first page is marked BOS and
//!   [`PageWriter::finish`] marks the last page EOS — re-stamping an
//!   already-emitted final page in place (CRC repatched) when nothing
//!   is pending.
//! * **Read side** — [`PacketAssembler`] reassembles one logical
//!   bitstream's packets from its pages (a lacing value `< 255` ends a
//!   packet; `255` continues it, possibly across a page boundary via
//!   the `continued` flag), strictly validating serial consistency and
//!   the `continued`-flag/mid-packet agreement. [`parse_pages`] /
//!   [`pages_to_packets`] are whole-buffer conveniences on top.
//!
//! The strictness is deliberate and complements the demuxer: where
//! [`crate::demux::OggDemuxer`] *tolerates* holes and framing damage
//! (dropping partials, resyncing, counting the events), the assembler
//! *errors* on the first inconsistency — the behaviour a codec crate
//! wants when validating a stream it just produced.
//!
//! This layer was proven in `oxideav-vorbis`, whose §A.2 encapsulation
//! work needed exactly this API; it was moved here so every Ogg-mapped
//! codec crate can share it.

use oxideav_core::{Error, Result};

use crate::page::{flags, Page};

/// Maximum number of lacing segments per page (RFC 3533 §6: the
/// segment count is one byte).
pub const MAX_PAGE_SEGMENTS: usize = 255;

/// Parse every page of a physical bitstream, in order.
///
/// Pages from all logical bitstreams (grouped or chained, RFC 3533 §4)
/// are returned in their physical interleave order; run one
/// [`PacketAssembler`] per serial to recover each stream's packets.
///
/// Errors surface at the page where they occur — including
/// [`Error::NeedMore`] when the buffer ends inside a page. Callers
/// that need hole tolerance should use [`crate::demux`] instead.
pub fn parse_pages(data: &[u8]) -> Result<Vec<Page>> {
    let mut pages = Vec::new();
    let mut pos = 0usize;
    while pos < data.len() {
        let (page, used) = Page::parse(&data[pos..])?;
        pages.push(page);
        pos += used;
    }
    Ok(pages)
}

/// Convenience: parse a single-logical-bitstream physical stream to its
/// packet sequence. Fails on grouped/multiplexed input (a second
/// serial) or any framing inconsistency.
pub fn pages_to_packets(data: &[u8]) -> Result<Vec<Vec<u8>>> {
    let pages = parse_pages(data)?;
    let mut assembler = PacketAssembler::new();
    let mut packets = Vec::new();
    for page in &pages {
        packets.extend(assembler.push_page(page)?);
    }
    Ok(packets)
}

/// Reassemble one logical bitstream's packets from its pages
/// (RFC 3533 §5–§6: a lacing value `< 255` ends a packet, `255`
/// continues it; the `continued` header flag carries a packet across a
/// page boundary).
///
/// The assembler locks onto the serial of the first page it sees and
/// rejects pages from other logical bitstreams — callers demuxing a
/// grouped physical stream run one assembler per serial.
#[derive(Debug, Clone, Default)]
pub struct PacketAssembler {
    serial: Option<u32>,
    pending: Vec<u8>,
    mid_packet: bool,
}

impl PacketAssembler {
    /// Fresh assembler with no locked serial and no partial packet.
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    /// The serial this assembler locked onto, if any page has been
    /// pushed yet.
    #[must_use]
    pub fn serial(&self) -> Option<u32> {
        self.serial
    }

    /// `true` while a packet is split across a page boundary and its
    /// continuation has not yet arrived.
    #[must_use]
    pub fn mid_packet(&self) -> bool {
        self.mid_packet
    }

    /// Discard any partial packet and unlock the serial (stream reset /
    /// chain-link boundary).
    pub fn reset(&mut self) {
        self.serial = None;
        self.pending.clear();
        self.mid_packet = false;
    }

    /// Feed one page; returns the packets that *complete* on it, in
    /// order.
    ///
    /// # Errors
    ///
    /// `Error::InvalidData` for a page from another logical bitstream
    /// (serial mismatch) or when the page's `continued` flag
    /// contradicts the assembler's mid-packet state — a fresh page
    /// while a packet is still open, or a continuation page with no
    /// packet open (packet loss / corrupt framing).
    pub fn push_page(&mut self, page: &Page) -> Result<Vec<Vec<u8>>> {
        match self.serial {
            None => self.serial = Some(page.serial),
            Some(s) if s != page.serial => {
                return Err(Error::InvalidData(format!(
                    "Ogg packet assembly: page serial {:#010x} != locked stream serial {:#010x}",
                    page.serial, s
                )));
            }
            Some(_) => {}
        }
        if page.is_continued() != self.mid_packet {
            return Err(Error::InvalidData(format!(
                "Ogg packet assembly: continuity broken at page sequence {} \
                 (continued flag {}, mid-packet {})",
                page.seq_no,
                page.is_continued(),
                self.mid_packet
            )));
        }
        let mut packets = Vec::new();
        let mut pos = 0usize;
        for &lace in &page.lacing {
            let l = lace as usize;
            self.pending.extend_from_slice(&page.data[pos..pos + l]);
            pos += l;
            if l < 255 {
                packets.push(std::mem::take(&mut self.pending));
                self.mid_packet = false;
            } else {
                self.mid_packet = true;
            }
        }
        Ok(packets)
    }
}

/// Packet→page encapsulation for one logical bitstream (RFC 3533
/// §4–§6), writing to an in-memory buffer.
///
/// Push packets in order with their codec-defined granule positions;
/// the writer chops each packet into 255-byte lacing segments,
/// auto-emits a page whenever the 255-entry segment table fills
/// mid-packet (marking the following page `continued`), and stamps
/// each page's granule position with the stamp of the last packet
/// completed on it (`-1` when none completes — the "page entirely
/// spanned by one packet" case, §6 field 4). [`Self::flush_page`]
/// forces a page boundary (e.g. the Vorbis §A.2 rules that the
/// identification header sits alone on the first page and the setup
/// header finishes its page); [`Self::finish`] closes the stream,
/// marking the final page EOS — patching the flag and CRC into an
/// already-emitted page in place when nothing is pending.
///
/// The first emitted page is automatically marked BOS (§6 field 3).
///
/// For multi-stream files, header/extradata handling, or streaming to
/// a `Write` sink, use [`crate::mux`] instead.
#[derive(Debug, Clone)]
pub struct PageWriter {
    serial: u32,
    sequence: u32,
    lacing: Vec<u8>,
    body: Vec<u8>,
    /// Granule stamp of the last packet completed on the pending page.
    page_granule: Option<i64>,
    /// The pending page starts with a continued packet.
    pending_continued: bool,
    /// The next emitted page is the stream's first (gets BOS).
    bos_pending: bool,
    /// Byte range of the most recently emitted page in `out`.
    last_page_range: Option<(usize, usize)>,
    /// Soft page-size target in body bytes: auto-emit the pending page
    /// once a packet completes at or past this size. `None` = no
    /// policy (pages emit only at the 255-segment limit or on an
    /// explicit flush).
    page_target: Option<usize>,
    out: Vec<u8>,
}

impl PageWriter {
    /// Fresh writer for a logical bitstream with the given serial.
    #[must_use]
    pub fn new(serial: u32) -> Self {
        Self {
            serial,
            sequence: 0,
            lacing: Vec::new(),
            body: Vec::new(),
            page_granule: None,
            pending_continued: false,
            bos_pending: true,
            last_page_range: None,
            page_target: None,
            out: Vec::new(),
        }
    }

    /// Set a soft page-size target: whenever a packet *completes* with
    /// the pending page's body at or past `bytes`, the page is emitted
    /// automatically, keeping pages near the "usually 4-8 kB" band
    /// RFC 3533 describes (a packet that overshoots the target still
    /// lands whole; only the 255-segment table forces a mid-packet
    /// split). `4096` is a good general-purpose value.
    ///
    /// Beyond politeness, small pages measurably improve player
    /// interop: black-box testing against ffmpeg showed that an
    /// Ogg/Vorbis stream whose audio packets all sit on a *single*
    /// page (so the first audio-bearing page is also the EOS page)
    /// decodes short by `blocksize0 / 2` samples (128 samples across
    /// twelve staged fixtures), while any stream whose audio spans two
    /// or more pages decodes to its full declared length. A page
    /// target makes the degenerate single-audio-page layout impossible
    /// for any stream longer than the target.
    #[must_use]
    pub fn with_page_target(mut self, bytes: usize) -> Self {
        self.page_target = Some(bytes);
        self
    }

    /// Change (or clear) the soft page-size target — the in-place
    /// counterpart of [`Self::with_page_target`]. Takes effect from
    /// the next completed packet; the pending page is not flushed
    /// retroactively.
    pub fn set_page_target(&mut self, bytes: Option<usize>) {
        self.page_target = bytes;
    }

    /// The bytes of every page emitted so far (pending partial-page
    /// data is *not* included until a flush or auto-emit).
    #[must_use]
    pub fn written(&self) -> &[u8] {
        &self.out
    }

    /// Number of pages emitted so far.
    #[must_use]
    pub fn pages_emitted(&self) -> u32 {
        self.sequence
    }

    /// Body bytes accumulated on the pending (not yet emitted) page.
    /// Callers use this to apply a page-size policy tighter than the
    /// 255-segment hard limit (RFC 3533 describes pages as "usually
    /// 4-8 kB"): flush once this crosses the target.
    #[must_use]
    pub fn pending_body_len(&self) -> usize {
        self.body.len()
    }

    /// Append one packet with its codec-defined granule position (the
    /// position of the stream *after* this packet; e.g. for Vorbis
    /// audio packets the end PCM sample position, and `0` for header
    /// packets).
    pub fn push_packet(&mut self, packet: &[u8], granulepos: i64) {
        // The pending page never ends mid-packet at entry (mid-packet
        // fills are emitted inside the loop below), so a full pending
        // table here just needs a plain emit first.
        if self.lacing.len() == MAX_PAGE_SEGMENTS {
            self.emit_page(false, false);
        }
        let mut remaining = packet;
        loop {
            while self.lacing.len() < MAX_PAGE_SEGMENTS {
                let take = remaining.len().min(255);
                self.lacing.push(take as u8);
                self.body.extend_from_slice(&remaining[..take]);
                remaining = &remaining[take..];
                if take < 255 {
                    // Final segment — the packet completes here.
                    self.page_granule = Some(granulepos);
                    // Soft page-size policy: emit once a completed
                    // packet leaves the page at/past the target.
                    if let Some(target) = self.page_target {
                        if self.body.len() >= target {
                            self.emit_page(false, false);
                        }
                    }
                    return;
                }
                // take == 255: the packet continues. When it has no
                // bytes left, the next iteration pushes the required
                // zero-length terminating segment.
            }
            // Segment table full mid-packet: emit and continue the
            // packet on the next page.
            self.emit_page(true, false);
        }
    }

    /// Force a page boundary: emit the pending partial page, if any.
    pub fn flush_page(&mut self) {
        if !self.lacing.is_empty() {
            self.emit_page(false, false);
        }
    }

    /// Close the stream: flush any pending data on a final page marked
    /// EOS (RFC 3533 §6 field 3). When nothing is pending, the most
    /// recently emitted page is re-stamped as EOS in place (CRC
    /// recomputed per §6 field 7). Returns the complete
    /// logical-bitstream bytes.
    #[must_use]
    pub fn finish(mut self) -> Vec<u8> {
        if !self.lacing.is_empty() {
            self.emit_page(false, true);
        } else if let Some((start, end)) = self.last_page_range {
            self.out[start + 5] |= flags::LAST_PAGE;
            let crc = crate::crc::compute_page_checksum(&self.out[start..end])
                .expect("emitted pages are at least 27 bytes");
            self.out[start + crate::crc::CRC_FIELD_OFFSET
                ..start + crate::crc::CRC_FIELD_OFFSET + crate::crc::CRC_FIELD_LEN]
                .copy_from_slice(&crc.to_le_bytes());
        }
        self.out
    }

    fn emit_page(&mut self, next_continued: bool, eos: bool) {
        let mut page_flags = 0u8;
        if self.pending_continued {
            page_flags |= flags::CONTINUED;
        }
        if self.bos_pending {
            page_flags |= flags::FIRST_PAGE;
        }
        if eos {
            page_flags |= flags::LAST_PAGE;
        }
        let page = Page {
            flags: page_flags,
            granule_position: self.page_granule.unwrap_or(-1),
            serial: self.serial,
            seq_no: self.sequence,
            lacing: std::mem::take(&mut self.lacing),
            data: std::mem::take(&mut self.body),
        };
        self.bos_pending = false;
        self.pending_continued = next_continued;
        self.page_granule = None;
        self.sequence += 1;
        let start = self.out.len();
        let bytes = page
            .try_to_bytes()
            .expect("writer-built pages satisfy the lacing invariants");
        self.out.extend_from_slice(&bytes);
        self.last_page_range = Some((start, self.out.len()));
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Round-trip a packet sequence through PageWriter and the parse +
    /// assemble stack; returns (pages, packets).
    fn writer_roundtrip(packets: &[(Vec<u8>, i64)]) -> (Vec<Page>, Vec<Vec<u8>>) {
        let mut w = PageWriter::new(0xDEAD_BEEF);
        for (p, g) in packets {
            w.push_packet(p, *g);
        }
        let bytes = w.finish();
        let pages = parse_pages(&bytes).unwrap();
        let got = pages_to_packets(&bytes).unwrap();
        (pages, got)
    }

    #[test]
    fn writer_roundtrips_simple_packets() {
        let packets: Vec<(Vec<u8>, i64)> = vec![
            (vec![1u8; 30], 0),
            (vec![2u8; 300], 128),
            (vec![3u8; 1], 256),
        ];
        let (pages, got) = writer_roundtrip(&packets);
        assert_eq!(
            got,
            packets.iter().map(|(p, _)| p.clone()).collect::<Vec<_>>()
        );
        assert!(pages[0].is_first());
        assert!(pages.last().unwrap().is_last());
        assert_eq!(pages.last().unwrap().granule_position, 256);
    }

    #[test]
    fn exact_255_multiple_packet_gets_zero_lacing_terminator() {
        let (pages, got) = writer_roundtrip(&[(vec![9u8; 510], 42)]);
        assert_eq!(got, vec![vec![9u8; 510]]);
        // 255, 255, 0 — the zero terminator marks completion.
        let all_lacing: Vec<u8> = pages.iter().flat_map(|p| p.lacing.clone()).collect();
        assert_eq!(all_lacing, vec![255, 255, 0]);
    }

    #[test]
    fn zero_length_packet_is_a_single_zero_lacing() {
        let (pages, got) = writer_roundtrip(&[(Vec::new(), 5), (vec![1u8; 4], 6)]);
        assert_eq!(got, vec![Vec::new(), vec![1u8; 4]]);
        assert_eq!(pages[0].lacing[0], 0);
    }

    #[test]
    fn oversize_packet_spans_pages_with_continued_flag_and_minus_one_granule() {
        // 255 segments × 255 bytes = 65025 bytes fill page 0 exactly;
        // a 70000-byte packet must continue onto page 1.
        let (pages, got) = writer_roundtrip(&[(vec![7u8; 70_000], 99)]);
        assert_eq!(got, vec![vec![7u8; 70_000]]);
        assert!(pages.len() >= 2);
        assert!(!pages[0].is_continued());
        assert!(pages[1].is_continued(), "page 1 must continue the packet");
        assert_eq!(
            pages[0].granule_position, -1,
            "no packet completes on page 0"
        );
        assert_eq!(pages.last().unwrap().granule_position, 99);
        // Sequence numbers count up from 0.
        for (i, p) in pages.iter().enumerate() {
            assert_eq!(p.seq_no, i as u32);
        }
    }

    #[test]
    fn flush_page_forces_a_boundary() {
        let mut w = PageWriter::new(1);
        w.push_packet(&[1u8; 10], 0);
        w.flush_page();
        w.push_packet(&[2u8; 10], 1);
        let bytes = w.finish();
        let pages = parse_pages(&bytes).unwrap();
        assert_eq!(pages.len(), 2);
        assert_eq!(pages[0].lacing, vec![10]);
        assert_eq!(pages[1].lacing, vec![10]);
        assert!(!pages[1].is_continued());
    }

    #[test]
    fn finish_patches_eos_onto_an_already_flushed_final_page() {
        let mut w = PageWriter::new(1);
        w.push_packet(&[1u8; 10], 7);
        w.flush_page(); // page already emitted; finish() must patch it
        let bytes = w.finish();
        let pages = parse_pages(&bytes).unwrap();
        assert_eq!(pages.len(), 1);
        assert!(pages[0].is_last(), "EOS must be patched in place");
        assert_eq!(pages[0].granule_position, 7);
        // The patched page still CRC-verifies (parse_pages checked it).
    }

    #[test]
    fn empty_writer_finish_produces_no_pages() {
        let w = PageWriter::new(1);
        assert!(w.finish().is_empty());
    }

    #[test]
    fn page_target_keeps_pages_in_the_rfc_band() {
        // 100 × 600-byte packets under a 4096-byte target: every page
        // but the last carries 4096..=4695 body bytes (the target plus
        // at most one packet's overshoot) and the granule of the last
        // packet completed on it.
        let mut w = PageWriter::new(7).with_page_target(4096);
        for i in 0..100i64 {
            w.push_packet(&[i as u8; 600], (i + 1) * 512);
        }
        let bytes = w.finish();
        let pages = parse_pages(&bytes).unwrap();
        assert!(pages.len() > 10, "target must split the stream");
        let mut done = 0usize;
        for (i, page) in pages.iter().enumerate() {
            if i + 1 < pages.len() {
                assert!(
                    (4096..4096 + 600).contains(&page.data.len()),
                    "page {i} body {} outside the target band",
                    page.data.len()
                );
            }
            done += page.lacing.iter().filter(|&&l| l < 255).count();
            assert_eq!(page.granule_position, done as i64 * 512);
        }
        assert_eq!(pages_to_packets(&bytes).unwrap().len(), 100);

        // Without a target the same packets pile onto 255-segment
        // mega-pages, far above the band — the historical default.
        let mut w = PageWriter::new(7);
        for i in 0..100i64 {
            w.push_packet(&[i as u8; 600], (i + 1) * 512);
        }
        let pages = parse_pages(&w.finish()).unwrap();
        assert!(pages.iter().any(|p| p.data.len() > 8192));
    }

    #[test]
    fn pending_body_len_tracks_the_open_page() {
        let mut w = PageWriter::new(1);
        assert_eq!(w.pending_body_len(), 0);
        w.push_packet(&[0u8; 100], 1);
        assert_eq!(w.pending_body_len(), 100);
        w.flush_page();
        assert_eq!(w.pending_body_len(), 0);
        assert_eq!(w.pages_emitted(), 1);
    }

    #[test]
    fn assembler_rejects_serial_switch_and_broken_continuity() {
        let page_a = Page {
            flags: flags::FIRST_PAGE,
            granule_position: 0,
            serial: 1,
            seq_no: 0,
            lacing: vec![255],
            data: vec![0; 255],
        };
        let mut asm = PacketAssembler::new();
        assert!(asm.push_page(&page_a).unwrap().is_empty());
        assert!(asm.mid_packet());
        assert_eq!(asm.serial(), Some(1));

        let mut other = page_a.clone();
        other.serial = 2;
        assert!(matches!(asm.push_page(&other), Err(Error::InvalidData(_))));

        // A fresh (non-continued) page while mid-packet is a
        // continuity break.
        let mut fresh = page_a.clone();
        fresh.seq_no = 1;
        fresh.lacing = vec![3];
        fresh.data = vec![0; 3];
        assert!(matches!(asm.push_page(&fresh), Err(Error::InvalidData(_))));

        // reset() unlocks the serial and drops the partial.
        asm.reset();
        assert_eq!(asm.serial(), None);
        assert!(!asm.mid_packet());
        assert!(asm.push_page(&other).is_ok());
    }

    #[test]
    fn pages_to_packets_rejects_grouped_streams() {
        let mut w1 = PageWriter::new(1);
        w1.push_packet(&[1u8; 4], 0);
        let mut w2 = PageWriter::new(2);
        w2.push_packet(&[2u8; 4], 0);
        let mut interleaved = w1.finish();
        interleaved.extend_from_slice(&w2.finish());
        assert!(pages_to_packets(&interleaved).is_err());
        // Per-serial assemblers recover both streams.
        let pages = parse_pages(&interleaved).unwrap();
        let mut a1 = PacketAssembler::new();
        let mut a2 = PacketAssembler::new();
        let mut got1 = Vec::new();
        let mut got2 = Vec::new();
        for p in &pages {
            match p.serial {
                1 => got1.extend(a1.push_page(p).unwrap()),
                2 => got2.extend(a2.push_page(p).unwrap()),
                _ => unreachable!(),
            }
        }
        assert_eq!(got1, vec![vec![1u8; 4]]);
        assert_eq!(got2, vec![vec![2u8; 4]]);
    }

    #[test]
    fn parse_pages_surfaces_truncation() {
        let mut w = PageWriter::new(1);
        w.push_packet(&[1u8; 40], 0);
        let bytes = w.finish();
        assert!(matches!(
            parse_pages(&bytes[..bytes.len() - 1]),
            Err(Error::NeedMore)
        ));
    }
}
