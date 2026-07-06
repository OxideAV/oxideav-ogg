//! Ogg muxer: pack incoming packets into pages.
//!
//! Strategy: maintain one buffered page per logical stream. Pack a packet by
//! appending its bytes and lacing values. Flush the page whenever it reaches
//! the 255-segment limit, when an explicit flush is requested, or at trailer
//! time. Granule positions come from `Packet::pts` for non-header packets.

use std::collections::{HashMap, HashSet};
use std::io::{Seek, SeekFrom, Write};

use oxideav_core::{CodecId, Error, Packet, Result, StreamInfo};
use oxideav_core::{Muxer, WriteSeek};

use crate::codec_id;
use crate::page::{self, flags, lace, Page};
use crate::skeleton::{KeyPoint, SkelIndex, Skeleton};

pub fn open(output: Box<dyn WriteSeek>, streams: &[StreamInfo]) -> Result<Box<dyn Muxer>> {
    open_inner(output, streams, None, None)
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
    open_inner(output, streams, skeleton, None)
}

/// Per-stream limits for muxer-built Skeleton 4.0 keyframe indexes
/// (see [`open_with_skeleton_indexed`]).
///
/// `docs/container/ogg/ogg-skeleton-4.0.md` §"Keyframe index packets":
/// "The exact number of keyframes used to construct key points in the
/// index is up to the indexer, but to limit the index size, we recommend
/// including at most one key point per every 64KB of data, or every
/// 1000ms, whichever is least frequent." The defaults implement exactly
/// that recommendation; tests and high-keypoint-density use cases can
/// relax both gaps.
#[derive(Clone, Copy, Debug)]
pub struct AutoIndexConfig {
    /// Maximum number of keypoints recorded per content stream. The
    /// placeholder `index\0` packet written in the header section
    /// reserves `42 + 20 * max_keypoints` bytes (42-byte fixed prefix
    /// per the 4.0 spec, plus the worst-case two 10-byte variable-byte
    /// integers per keypoint), so this bounds the on-wire size paid up
    /// front. Keyframes past the cap are silently not indexed — the
    /// spec allows a partial index ("a keyframe index may not index
    /// all keyframes in the Ogg segment").
    ///
    /// Must be at least 1 and at most 3249 (the largest reservation
    /// that still fits a single maximal Ogg page of 255×255 body
    /// bytes, since every Skeleton packet rides on its own page).
    pub max_keypoints: usize,
    /// Minimum number of bytes between two consecutive keypoints of
    /// the same stream. Spec-recommended default: 64 KiB.
    pub min_keypoint_byte_gap: u64,
    /// Minimum presentation-time distance, in milliseconds, between
    /// two consecutive keypoints of the same stream. Spec-recommended
    /// default: 1000 ms.
    pub min_keypoint_time_gap_ms: u64,
}

impl Default for AutoIndexConfig {
    fn default() -> Self {
        Self {
            max_keypoints: 128,
            min_keypoint_byte_gap: 64 * 1024,
            min_keypoint_time_gap_ms: 1000,
        }
    }
}

/// Largest permitted `AutoIndexConfig::max_keypoints`: the reserved
/// packet (42 + 20·n bytes) must fit the 255×255-byte body of a single
/// Ogg page because every Skeleton packet is emitted on its own page.
const AUTO_INDEX_MAX_KEYPOINTS_LIMIT: usize = (255 * 255 - 42) / 20;

/// Open an Ogg muxer that emits a Skeleton metadata bitstream AND
/// builds a Skeleton 4.0 keyframe `index\0` packet per content stream
/// while muxing.
///
/// The 4.0 spec places every index packet in the segment's header
/// pages ("all the Skeleton track's index packets appear in the header
/// pages of the Ogg segment", so "all the keyframe indexes are
/// immediately available once the header packets have been read"), but
/// a keypoint's byte offset and the segment's first/last sample times
/// are only knowable after the content has been written. The muxer
/// therefore reserves a fixed-size placeholder `index\0` page per
/// auto-indexed stream in `write_header` (between the fisbones and the
/// Skeleton EOS, per the §"Further restrictions" ordering), records a
/// keypoint whenever a page carrying a keyframe-flagged packet
/// ([`oxideav_core::packet::PacketFlags::keyframe`]) hits the wire, and
/// rewrites each placeholder page in place at `write_trailer` time —
/// same page length, CRC recomputed per RFC 3533 §6 field 7, exactly
/// like the fishead segment-length / content-byte-offset backfill.
///
/// Keypoint semantics (per §"Keyframe indexes for faster seeking" /
/// §"Keyframe index packets"):
/// * a keypoint's offset is the first byte of a page of the indexed
///   stream — here, the page on which the keyframe packet *starts*,
///   which is "the last page which lies before all data required to
///   decode the keyframe" for a packet-aligned muxer;
/// * its timestamp numerator is the presentation time of the first
///   keyframe starting on that page, expressed over the stream
///   time-base denominator (`timestamp_denominator = time_base.den`,
///   numerator = `pts × time_base.num`);
/// * keypoints are thinned per [`AutoIndexConfig`]: a candidate is
///   accepted only when BOTH the byte gap and the time gap since the
///   previously accepted keypoint are met ("whichever is least
///   frequent"), and never beyond `max_keypoints`;
/// * the index's first/last-sample-time numerators are taken from the
///   first and last content-packet pts observed on the stream (Ogg
///   granule semantics make the final granule the segment's end
///   position for the audio mappings this muxer writes).
///
/// For streams without the concept of a keyframe (Vorbis &c.), set
/// the keyframe flag on every independently decodable packet — the
/// gap gating then produces exactly the spec's "periodic samples"
/// indexing for keyframe-less streams.
///
/// Streams whose serial already has a caller-supplied [`SkelIndex`]
/// attached to `skeleton.indexes` are passed through verbatim and not
/// auto-indexed (a pre-measured remux knows better).
///
/// Because the rewritten packet must keep the placeholder's byte
/// length (in-place page rewrites cannot move the pages that follow),
/// the bytes after the final encoded keypoint remain zero. Those tail
/// bytes lie outside every field the 4.0 index layout defines — a
/// reader consumes exactly *n* keypoints starting at byte 42 — so any
/// conforming parser ignores them.
///
/// Errors: the attached fishead (when present) must be version 4.0 or
/// later (`index\0` packets are a 4.0 feature; a 3.0 fishead has no
/// segment-length field to validate them against), `max_keypoints`
/// must be in `1..=3249`, and every content stream's time base must
/// be positive (the spec's "presentation time denominator … must not
/// be 0" rule for field 4 of the index packet).
pub fn open_with_skeleton_indexed(
    output: Box<dyn WriteSeek>,
    streams: &[StreamInfo],
    skeleton: Skeleton,
    config: AutoIndexConfig,
) -> Result<Box<dyn Muxer>> {
    if config.max_keypoints == 0 || config.max_keypoints > AUTO_INDEX_MAX_KEYPOINTS_LIMIT {
        return Err(Error::invalid(format!(
            "Ogg muxer: AutoIndexConfig::max_keypoints must be in 1..={AUTO_INDEX_MAX_KEYPOINTS_LIMIT} (got {})",
            config.max_keypoints
        )));
    }
    if let Some(head) = skeleton.head.as_ref() {
        if !head.version.at_least(crate::skeleton::Version::V4_0) {
            return Err(Error::invalid(
                "Ogg muxer: Skeleton keyframe index packets require a 4.0 fishead",
            ));
        }
    }
    for s in streams {
        if s.time_base.0.num <= 0 || s.time_base.0.den <= 0 {
            return Err(Error::invalid(format!(
                "Ogg muxer: stream {} has a non-positive time base; the Skeleton index timestamp denominator must not be 0",
                s.index
            )));
        }
    }
    open_inner(output, streams, Some(skeleton), Some(config))
}

fn open_inner(
    output: Box<dyn WriteSeek>,
    streams: &[StreamInfo],
    skeleton: Option<Skeleton>,
    auto_index: Option<AutoIndexConfig>,
) -> Result<Box<dyn Muxer>> {
    Ok(Box::new(open_concrete_inner(
        output, streams, skeleton, auto_index,
    )?))
}

/// Open a concrete [`OggMuxer`] rather than a boxed `dyn Muxer`.
///
/// The concrete type exposes chained-stream muxing via
/// [`OggMuxer::begin_new_link`] — RFC 3533 §4 sequential multiplexing —
/// which the object-safe [`Muxer`] trait cannot express (a new link takes
/// a fresh `&[StreamInfo]`). It is the write-side companion to
/// [`crate::demux::open_concrete`]: a chained file the demuxer partitions
/// into per-link streams can be reproduced link-for-link. Callers that do
/// not need chaining should prefer [`open`].
pub fn open_concrete(output: Box<dyn WriteSeek>, streams: &[StreamInfo]) -> Result<OggMuxer> {
    open_concrete_inner(output, streams, None, None)
}

fn open_concrete_inner(
    output: Box<dyn WriteSeek>,
    streams: &[StreamInfo],
    skeleton: Option<Skeleton>,
    auto_index: Option<AutoIndexConfig>,
) -> Result<OggMuxer> {
    let mut used_serials: HashSet<u32> = HashSet::new();
    let (per_stream, stream_order, max_serial) = build_link_writers(streams, &mut used_serials);
    let skeleton_writer = skeleton.map(|sk| {
        let serial = match sk.serial {
            Some(s) => s,
            None => {
                // Pick a serial past the largest content serial, skipping
                // any already claimed so it cannot collide (RFC 3533 §4
                // unique-serial MUST).
                let mut cand = max_serial.wrapping_add(1);
                while used_serials.contains(&cand) {
                    cand = cand.wrapping_add(1);
                }
                cand
            }
        };
        used_serials.insert(serial);
        SkeletonWriter {
            skel: sk,
            serial,
            seq_no: 0,
            emitted_head: None,
            content_byte_offset_measured: None,
        }
    });
    // Build one auto-index collector per content stream that does not
    // already carry a caller-supplied index for its serial. Only
    // meaningful when a Skeleton is attached (the index packets ride
    // the Skeleton logical bitstream).
    let mut auto_states: Vec<AutoIndexState> = Vec::new();
    if let (Some(cfg), Some(sw)) = (auto_index, skeleton_writer.as_ref()) {
        for s in streams {
            let serial = per_stream[&s.index].serial;
            if sw.skel.indexes.iter().any(|idx| idx.serial == serial) {
                continue;
            }
            auto_states.push(AutoIndexState {
                stream_index: s.index,
                serial,
                timestamp_denominator: s.time_base.0.den,
                tb_num: s.time_base.0.num,
                reserved_packet_len: 42 + 20 * cfg.max_keypoints,
                config: cfg,
                placeholder: None,
                keypoints: Vec::new(),
                first_ts: None,
                last_ts: None,
            });
        }
    }
    Ok(OggMuxer {
        output,
        streams: streams.to_vec(),
        per_stream,
        stream_order,
        header_written: false,
        trailer_written: false,
        skeleton: skeleton_writer,
        auto_index: auto_states,
        used_serials,
        link_index: 0,
        content_started: false,
    })
}

/// Build the per-stream writers for one chain link, assigning each a
/// globally-unique serial (RFC 3533 §4: "Each chained logical bitstream
/// MUST have a unique serial number within the scope of the physical
/// bitstream"). Serials already claimed by an earlier link — or by another
/// stream of this link whose derived serial collides — are bumped to the
/// next free value. Returns the writer map, the stream order, and the
/// largest serial assigned (so the Skeleton serial can sit past it).
fn build_link_writers(
    streams: &[StreamInfo],
    used_serials: &mut HashSet<u32>,
) -> (HashMap<u32, StreamWriter>, Vec<u32>, u32) {
    let mut per_stream = HashMap::with_capacity(streams.len());
    let mut max_serial: u32 = 0;
    for s in streams {
        let mut serial = derive_serial(s);
        while used_serials.contains(&serial) {
            serial = serial.wrapping_add(1);
        }
        used_serials.insert(serial);
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
    let stream_order = streams.iter().map(|s| s.index).collect();
    (per_stream, stream_order, max_serial)
}

/// Derive a stable serial number for a stream. Real-world muxers use random
/// 32-bit numbers; we use the stream index for determinism (which makes
/// remux output byte-stable when the input numbering is also dense from 0).
fn derive_serial(s: &StreamInfo) -> u32 {
    s.index
}

/// Concrete Ogg muxer.
///
/// Returned by [`open_concrete`]; the [`open`] / [`open_with_skeleton`] /
/// [`open_with_skeleton_indexed`] factories box it as a `dyn Muxer`. The
/// concrete type additionally exposes [`OggMuxer::begin_new_link`] for
/// writing RFC 3533 §4 chained (sequentially multiplexed) physical
/// bitstreams, which the object-safe [`Muxer`] trait cannot express.
pub struct OggMuxer {
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
    /// Per-stream Skeleton 4.0 keyframe-index collectors (one per
    /// content stream when [`open_with_skeleton_indexed`] was used;
    /// empty otherwise). Each reserves a placeholder `index\0` page in
    /// the header section and is backfilled in place at trailer time.
    auto_index: Vec<AutoIndexState>,
    /// Every `bitstream_serial_number` claimed so far across *all* chain
    /// links (RFC 3533 §4: serials MUST be unique within the scope of the
    /// physical bitstream). A new link's derived serials that collide are
    /// bumped to the next free value.
    used_serials: HashSet<u32>,
    /// Zero-based index of the chain link currently being written. `0`
    /// for a plain (single-link) file; incremented by
    /// [`OggMuxer::begin_new_link`].
    link_index: u32,
    /// Whether any content data page (post-header) has been written for
    /// the current link. A new link may only begin after content — the
    /// demuxer detects a link boundary as a BOS page following a non-BOS
    /// (data) page, so a link with no data page would not be recognised
    /// as a separate link on read-back.
    content_started: bool,
}

/// Keyframe-index collector for one content stream — the WRITE-side
/// counterpart of the demuxer's Skeleton 4.0 `index\0` fast-path seek.
struct AutoIndexState {
    stream_index: u32,
    /// On-wire `bitstream_serial_number` of the indexed content stream
    /// (Skeleton index packets are keyed by serial, field 2 of the
    /// `index\0` layout).
    serial: u32,
    /// Field 4 of the index packet: "The presentation time denominator
    /// for this stream … All timestamps, including keypoint timestamps,
    /// first and last sample timestamps are fractions of seconds over
    /// this denominator. This must not be 0." Taken from the stream
    /// time base denominator so that `pts × tb.num` is directly the
    /// numerator.
    timestamp_denominator: i64,
    tb_num: i64,
    /// Byte length of the placeholder packet reserved in the header
    /// section: 42-byte fixed prefix + 20 bytes per `max_keypoints`
    /// (two worst-case 10-byte variable-byte integers per keypoint).
    reserved_packet_len: usize,
    config: AutoIndexConfig,
    /// `(wire offset, page sequence number)` of the placeholder page,
    /// recorded when `write_header` emits it.
    placeholder: Option<(u64, u32)>,
    /// Accepted keypoints in increasing-offset order (the spec's
    /// "stored in increasing order by offset" invariant holds by
    /// construction — pages of one stream hit the wire in order).
    keypoints: Vec<KeyPoint>,
    /// First/last content-packet pts observed, in numerator units
    /// (`pts × tb_num`); fills the index packet's first-sample-time /
    /// last-sample-time fields at trailer time.
    first_ts: Option<i64>,
    last_ts: Option<i64>,
}

impl AutoIndexState {
    /// Record a content-packet pts (numerator units) for the
    /// first/last-sample-time fields.
    fn note_pts(&mut self, ts: i64) {
        if self.first_ts.is_none() {
            self.first_ts = Some(ts);
        }
        self.last_ts = Some(ts);
    }

    /// Offer a keypoint candidate: a page of this stream just hit the
    /// wire at `offset` and the first keyframe-flagged packet starting
    /// on it has presentation-time numerator `ts`. Accepts the first
    /// candidate unconditionally; subsequent candidates must satisfy
    /// BOTH the byte gap and the time gap relative to the previously
    /// accepted keypoint (the spec's "at most one key point per every
    /// 64KB of data, or every 1000ms, whichever is least frequent"
    /// recommendation), and the `max_keypoints` reservation cap.
    fn offer_keypoint(&mut self, offset: u64, ts: i64) {
        if self.keypoints.len() >= self.config.max_keypoints {
            return;
        }
        if let Some(last) = self.keypoints.last() {
            // Defensive monotonicity: never record a keypoint that
            // would violate the increasing-offset (and thus
            // increasing-time) ordering invariant.
            if offset <= last.offset || ts < last.timestamp {
                return;
            }
            if offset - last.offset < self.config.min_keypoint_byte_gap {
                return;
            }
            // (ts - last.ts) / den seconds >= gap_ms / 1000
            // ⇔ (ts - last.ts) × 1000 >= gap_ms × den   (den > 0)
            let dt = (ts - last.timestamp) as i128;
            if dt * 1000
                < self.config.min_keypoint_time_gap_ms as i128 * self.timestamp_denominator as i128
            {
                return;
            }
        }
        self.keypoints.push(KeyPoint {
            offset,
            timestamp: ts,
        });
    }
}

/// A finalized page held back by the EOS-deferral mechanism, plus the
/// metadata the auto-indexer needs once the page's wire offset is known.
struct PendingPage {
    bytes: Vec<u8>,
    /// pts of the first keyframe-flagged packet that *starts* on this
    /// page, if any — the keypoint timestamp source.
    first_keyframe_pts: Option<i64>,
}

struct SkeletonWriter {
    skel: Skeleton,
    serial: u32,
    seq_no: u32,
    /// The fishead actually written into the BOS page — the caller's
    /// head, or the default-constructed 4.0 head when the attached
    /// `Skeleton` carried none. Retained so `write_trailer` can
    /// backfill the 4.0 *Segment length in bytes* / *Content byte
    /// offset* fields in place once the whole segment is measured.
    emitted_head: Option<crate::skeleton::FisHead>,
    /// Byte offset of the first non-header page, recorded at the end of
    /// `write_header` (immediately after the Skeleton EOS page closes
    /// the control section). This is the value the 4.0 fishead's
    /// *Content byte offset* field declares per
    /// `docs/container/ogg/ogg-skeleton-4.0.md` ("the offset of the
    /// first non header page in the Ogg segment").
    content_byte_offset_measured: Option<u64>,
}

struct StreamWriter {
    serial: u32,
    seq_no: u32,
    buffered: PageBuilder,
    headers_remaining: usize,
    bos_emitted: bool,
    /// The most recently finalized page, held back until either
    /// another page is flushed (in which case it's written) or the trailer
    /// runs (in which case it gets EOS set and its CRC patched). This makes
    /// the EOS marker sit on a real data page instead of an empty trailing one.
    pending_bytes: Option<PendingPage>,
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
    /// pts of the first keyframe-flagged content packet appended to this
    /// page. Feeds the Skeleton 4.0 auto-index keypoint candidate once
    /// the page's wire offset is known (a keypoint's timestamp is "the
    /// presentation time … of the first key frame which starts on the
    /// page at the keypoint's offset").
    first_keyframe_pts: Option<i64>,
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

    /// Append a packet whose lacing exceeds one page's 255-segment limit,
    /// "distributed over several pages" per RFC 3533 §5. The builder is
    /// filled up to 255 segments at a time; each time it is full, a page is
    /// flushed and reassembly continues on the next page. Because every
    /// intermediate page ends on a 255-valued lacing segment (the packet is
    /// still open), `flush_page` marks the following page `continued`
    /// (RFC 3533 §6 field 3) automatically. The packet's terminator segment
    /// (`< 255`, or the trailing `0` for an exact multiple of 255) lands on
    /// the final page, which is left in the builder for the caller's normal
    /// flush logic (granule carry-through, `unit_boundary`, header-page
    /// flush) to finish.
    ///
    /// Precondition: the builder is empty (the caller flushes any partial
    /// page first), so the spanning packet starts on a fresh page boundary
    /// and its first page is never spuriously continued.
    fn append_packet_spanning(
        &mut self,
        stream_index: u32,
        lacing: &[u8],
        data: &[u8],
    ) -> Result<()> {
        let mut seg_off = 0usize; // index into `lacing`
        let mut data_off = 0usize; // byte index into `data`
        while seg_off < lacing.len() {
            let writer = self.writer_for(stream_index)?;
            let room = 255 - writer.buffered.lacing.len();
            // Take as many segments as fit on the current page.
            let take = room.min(lacing.len() - seg_off);
            let chunk = &lacing[seg_off..seg_off + take];
            let chunk_bytes: usize = chunk.iter().map(|&v| v as usize).sum();
            writer.buffered.lacing.extend_from_slice(chunk);
            writer
                .buffered
                .data
                .extend_from_slice(&data[data_off..data_off + chunk_bytes]);
            seg_off += take;
            data_off += chunk_bytes;
            // If there is still packet left, the page is full (255 segments)
            // — flush it as a non-forced page so the remainder spills onto a
            // fresh `continued` page. The very last chunk is *not* flushed
            // here; it stays in the builder for the caller to finalize.
            if seg_off < lacing.len() {
                self.flush_page(stream_index, false)?;
            }
        }
        Ok(())
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
        let head = {
            let sk = self.skeleton.as_ref().expect("skeleton writer present");
            sk.skel.head.clone().unwrap_or_else(|| {
                // Caller attached a Skeleton with no fishead — emit a
                // minimal 4.0 fishead (zero-valued presentation time /
                // basetime / segment-length / content-byte-offset)
                // so the BOS is structurally valid for downstream
                // parsers.
                crate::skeleton::FisHead::new(crate::skeleton::Version::V4_0)
            })
        };
        let head_bytes = head.to_bytes();
        self.skeleton
            .as_mut()
            .expect("skeleton writer present")
            .emitted_head = Some(head);
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
        // Auto-index placeholders: one fixed-size `index\0` page per
        // auto-indexed content stream, emitted in the secondary-header
        // section per the 4.0 spec ("Before the Skeleton EOS page in
        // the segment header pages come the Skeleton 4.0 keyframe index
        // packets"). The placeholder declares zero keypoints over the
        // stream's timestamp denominator and pads the body to the full
        // reservation; `backfill_auto_indexes` rewrites it in place at
        // trailer time once the keypoints are known. Wire offset and
        // page sequence number are recorded so the rewrite reproduces
        // an identical-length page.
        for i in 0..self.auto_index.len() {
            let placeholder = {
                let state = &self.auto_index[i];
                let mut data = SkelIndex::new(state.serial, state.timestamp_denominator).to_bytes();
                data.resize(state.reserved_packet_len, 0);
                data
            };
            let offset = self.output.stream_position()?;
            let seq_no = self
                .skeleton
                .as_ref()
                .expect("skeleton writer present")
                .seq_no;
            self.write_skeleton_page(&placeholder, 0)?;
            self.auto_index[i].placeholder = Some((offset, seq_no));
        }
        // Empty packet on its own EOS page closes the Skeleton control
        // section (per spec). A zero-byte packet lacing-encodes as a
        // single `0` lacing value (lace(0) → [0]); the on-wire page
        // therefore carries one segment whose body length is zero.
        self.write_skeleton_page(&[], flags::LAST_PAGE)
    }

    /// Rewrite every auto-index placeholder page in place with the
    /// keypoints collected while muxing.
    ///
    /// Mirrors [`Self::backfill_skeleton_fishead`]: the page is
    /// reconstructed exactly as the placeholder was emitted (same
    /// flags / granule 0 / serial / recorded sequence number) with the
    /// finished packet body, so the page byte length is unchanged and
    /// `Page::to_bytes` recomputes the CRC over the new bytes
    /// (RFC 3533 §6 field 7). The finished packet keeps the
    /// reservation length — bytes past the final encoded keypoint stay
    /// zero; they sit beyond the *n* keypoints the 4.0 layout defines
    /// (field 7: "*n* key points, starting with the first keypoint at
    /// byte 42"), so readers never consume them.
    fn backfill_auto_indexes(&mut self) -> Result<()> {
        if self.auto_index.is_empty() {
            return Ok(());
        }
        let skel_serial = match self.skeleton.as_ref() {
            Some(sk) => sk.serial,
            None => return Ok(()),
        };
        let end_pos = self.output.stream_position()?;
        for i in 0..self.auto_index.len() {
            let (page_off, seq_no, data) = {
                let state = &self.auto_index[i];
                let Some((page_off, seq_no)) = state.placeholder else {
                    continue;
                };
                let mut idx = SkelIndex::new(state.serial, state.timestamp_denominator);
                idx.first_sample_time = state.first_ts.unwrap_or(0);
                idx.last_sample_time = state.last_ts.unwrap_or(0);
                idx.keypoints = state.keypoints.clone();
                let mut data = idx.to_bytes();
                debug_assert!(
                    data.len() <= state.reserved_packet_len,
                    "auto-index reservation must hold max_keypoints worst-case encodings"
                );
                data.resize(state.reserved_packet_len, 0);
                (page_off, seq_no, data)
            };
            let page = Page {
                flags: 0,
                granule_position: 0,
                serial: skel_serial,
                seq_no,
                lacing: lace(data.len()),
                data,
            };
            let bytes = page.to_bytes();
            self.output.seek(SeekFrom::Start(page_off))?;
            self.output.write_all(&bytes)?;
        }
        self.output.seek(SeekFrom::Start(end_pos))?;
        Ok(())
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
        let first_keyframe_pts = writer.buffered.first_keyframe_pts.take();
        let new_bytes = page.to_bytes();

        // Write whatever was pending before, then queue the new bytes.
        let prev = writer.pending_bytes.take();
        if let Some(prev) = prev {
            self.write_pending_page(stream_index, prev)?;
        }
        let writer = self.writer_for(stream_index)?;
        writer.pending_bytes = Some(PendingPage {
            bytes: new_bytes,
            first_keyframe_pts,
        });
        Ok(())
    }

    /// Write a previously held-back page to the sink, recording its wire
    /// offset as a Skeleton 4.0 keypoint candidate when the page starts
    /// a keyframe-flagged packet and the stream is being auto-indexed.
    fn write_pending_page(&mut self, stream_index: u32, pending: PendingPage) -> Result<()> {
        let offset = self.output.stream_position()?;
        self.output.write_all(&pending.bytes)?;
        if let Some(pts) = pending.first_keyframe_pts {
            if let Some(state) = self
                .auto_index
                .iter_mut()
                .find(|s| s.stream_index == stream_index)
            {
                // Presentation-time numerator over the stream's
                // time-base denominator: pts × tb.num (saturating —
                // tb.num is 1 for every sample-rate-style base, so
                // the multiply is exact in practice).
                let ts = (pts as i128 * state.tb_num as i128)
                    .clamp(i64::MIN as i128, i64::MAX as i128) as i64;
                state.offer_keypoint(offset, ts);
            }
        }
        Ok(())
    }

    /// Backfill the Skeleton 4.0 fishead *Segment length in bytes* and
    /// *Content byte offset* fields with the measured values, rewriting
    /// the BOS page in place at trailer time.
    ///
    /// `docs/container/ogg/ogg-skeleton-4.0.md` §"Keyframe indexes for
    /// faster seeking" has decoders "check the length of the physical
    /// segment, and if it doesn't match the length stored in the
    /// Skeleton header packet, you know that either the index is out of
    /// date, or the file has been chained since indexing"; the BOS
    /// "also contains the offset of the first non header page in the
    /// Ogg segment" so a player "can skip forward to that offset, and
    /// start decoding from that offset forwards" when it wants to delay
    /// index loading. Neither value is knowable before the whole
    /// segment is written, hence the in-place patch here.
    ///
    /// The backfill is per-field and conservative:
    /// * a field the caller already set to a non-zero value is
    ///   preserved verbatim (a pre-measured remux knows better than we
    ///   do), only `None` / `0` ("unknown") values are filled in;
    /// * a 3.0 fishead is never touched — its 64-byte layout has no
    ///   such fields;
    /// * when nothing needs backfilling the BOS page is not rewritten
    ///   at all, keeping the output bytes identical to a straight
    ///   sequential write.
    ///
    /// The rewrite reconstructs the BOS page exactly as
    /// `write_skeleton_fishead_bos` emitted it (same flags / granule /
    /// serial / sequence number 0 / lacing) with the patched packet
    /// body, so the page length is unchanged and `Page::to_bytes`
    /// recomputes the CRC over the new bytes (RFC 3533 §6 field 7).
    fn backfill_skeleton_fishead(&mut self) -> Result<()> {
        let (serial, mut head, measured_offset) = {
            let Some(sk) = self.skeleton.as_ref() else {
                return Ok(());
            };
            let Some(head) = sk.emitted_head.as_ref() else {
                return Ok(());
            };
            if !head.version.at_least(crate::skeleton::Version::V4_0) {
                return Ok(());
            }
            (sk.serial, head.clone(), sk.content_byte_offset_measured)
        };
        let end_pos = self.output.stream_position()?;
        let mut changed = false;
        if head.segment_length.unwrap_or(0) == 0 && end_pos != 0 {
            head.segment_length = Some(end_pos);
            changed = true;
        }
        if head.content_byte_offset.unwrap_or(0) == 0 {
            if let Some(off) = measured_offset {
                head.content_byte_offset = Some(off);
                changed = true;
            }
        }
        if !changed {
            return Ok(());
        }
        let data = head.to_bytes();
        let page = Page {
            flags: flags::FIRST_PAGE,
            granule_position: 0,
            serial,
            seq_no: 0,
            lacing: lace(data.len()),
            data,
        };
        let bytes = page.to_bytes();
        // The fishead BOS is always the very first page of the physical
        // stream (it is the first thing write_header emits when a
        // Skeleton is attached), so the patch target offset is 0.
        self.output.seek(SeekFrom::Start(0))?;
        self.output.write_all(&bytes)?;
        self.output.seek(SeekFrom::Start(end_pos))?;
        if let Some(sk) = self.skeleton.as_mut() {
            sk.emitted_head = Some(head);
        }
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
            // Step 2.5: drain every content stream's held-back page so
            // all content secondary-header pages physically precede the
            // Skeleton EOS page. `docs/container/ogg/ogg-skeleton-4.0.md`
            // §"Further restrictions" orders the segment as "the
            // secondary header pages of all logical bitstreams come
            // next, including Skeleton's secondary header packets" and
            // then "the Skeleton EOS page ends the control section of
            // the Ogg stream before any content pages of any of the
            // other logical bitstreams appear". Without this drain the
            // EOS-deferral mechanism (pending_bytes) would hold the last
            // header page of each content stream (e.g. the Vorbis setup
            // page) and flush it only when the first content data page
            // arrives — after the Skeleton EOS, inside the content
            // section.
            let order = self.stream_order.clone();
            for idx in order {
                let pending = self.writer_for(idx)?.pending_bytes.take();
                if let Some(pending) = pending {
                    self.write_pending_page(idx, pending)?;
                }
            }
            self.write_skeleton_fisbones_and_eos()?;
            // Everything in the control section is now on the wire, so
            // the current position is the offset of the first non-header
            // page — the 4.0 fishead's *Content byte offset* value,
            // backfilled at trailer time.
            let pos = self.output.stream_position()?;
            self.skeleton
                .as_mut()
                .expect("skeleton writer present")
                .content_byte_offset_measured = Some(pos);
        }
        Ok(())
    }

    fn write_packet(&mut self, packet: &Packet) -> Result<()> {
        if !self.header_written {
            return Err(Error::other("Ogg muxer: write_header not called"));
        }
        let stream_index = packet.stream_index;
        let lacing_for_packet = lace(packet.data.len());

        let (is_header, needs_boundary_flush) = {
            let writer = self.writer_for(stream_index)?;
            (
                writer.headers_remaining > 0,
                writer.buffered.lacing.len() + lacing_for_packet.len() > 255
                    && !writer.buffered.is_empty(),
            )
        };

        // A single Ogg page holds at most 255 lacing segments (RFC 3533 §6
        // field 4). A packet ≥ 255×255 = 65025 bytes laces to ≥ 256 segments
        // (e.g. 65025 → 255 full + a 0 terminator), so it cannot fit one page
        // even when the page is otherwise empty — it MUST be "distributed
        // over several pages" (§5). `append_packet_spanning` fills the
        // current builder up to 255 segments, flushes a `continued`-marked
        // page (the flush sets `starts_continued` because its last segment is
        // 255), and repeats until the whole packet is laced. For the common
        // case (a packet that fits) it is a single append with no early flush.
        //
        // When the packet would not fit alongside the already-buffered
        // segments, flush the partial page first so the packet starts on a
        // fresh page boundary (and an oversized packet's spanning split
        // begins on an empty builder, as `append_packet_spanning` requires).
        if needs_boundary_flush {
            self.flush_page(stream_index, false)?;
        }

        if lacing_for_packet.len() > 255 {
            self.append_packet_spanning(stream_index, &lacing_for_packet, &packet.data)?;
        } else {
            let writer = self.writer_for(stream_index)?;
            writer.buffered.lacing.extend_from_slice(&lacing_for_packet);
            writer.buffered.data.extend_from_slice(&packet.data);
        }

        if is_header {
            // Header packets each get their own page with granule 0. A header
            // packet that already spanned pages above has left its final
            // partial page in the builder; flushing here closes it.
            let writer = self.writer_for(stream_index)?;
            writer.headers_remaining -= 1;
            writer.buffered.granule_position = 0;
            self.flush_page(stream_index, true)?;
            return Ok(());
        }

        let writer = self.writer_for(stream_index)?;
        // Audio/video packet. The page's granule_position is set from the
        // most recent pts seen on this page (this packet's pts wins if
        // present; otherwise the buffered value carries through). A new
        // page is flushed when the source signaled a page boundary via
        // `unit_boundary`. This separates *pts-per-packet* (decoders care)
        // from *page boundaries* (Ogg cares).
        if let Some(pts) = packet.pts {
            writer.buffered.granule_position = pts;
            // Auto-index bookkeeping: the packet starts on the page the
            // builder is currently filling, so a keyframe-flagged packet
            // makes this page a keypoint candidate once it reaches the
            // wire.
            if packet.flags.keyframe && writer.buffered.first_keyframe_pts.is_none() {
                writer.buffered.first_keyframe_pts = Some(pts);
            }
        }
        // Every content pts feeds the index packet's first/last-sample-
        // time fields (separate lookup — the per-stream writer borrow
        // must end before `auto_index` is touched).
        if let Some(pts) = packet.pts {
            if let Some(state) = self
                .auto_index
                .iter_mut()
                .find(|s| s.stream_index == stream_index)
            {
                let ts = (pts as i128 * state.tb_num as i128)
                    .clamp(i64::MIN as i128, i64::MAX as i128) as i64;
                state.note_pts(ts);
            }
        }
        // A content data page for this link now exists (even if still
        // buffered) — record it so `begin_new_link` knows the link has
        // real content preceding the next BOS.
        self.content_started = true;

        if packet.flags.unit_boundary {
            self.flush_page(stream_index, true)?;
        }

        Ok(())
    }

    fn write_trailer(&mut self) -> Result<()> {
        if self.trailer_written {
            return Ok(());
        }
        self.finalize_current_link()?;
        self.backfill_auto_indexes()?;
        self.backfill_skeleton_fishead()?;
        self.output.flush()?;
        self.trailer_written = true;
        Ok(())
    }
}

impl OggMuxer {
    /// Drain every current-link stream's buffered page and stamp the
    /// closing page of each with the EOS flag (RFC 3533 §6 field 5:
    /// "set: this page is the last page in the logical bitstream").
    ///
    /// Shared by [`Muxer::write_trailer`] (end of file) and
    /// [`OggMuxer::begin_new_link`] (end of a chain link): a chained
    /// physical bitstream requires "the eos page of a given logical
    /// bitstream is immediately followed by the bos page of the next"
    /// (RFC 3533 §4), so every link's streams must be EOS-terminated
    /// before the next link's BOS pages are written.
    fn finalize_current_link(&mut self) -> Result<()> {
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
            let pending = self.writer_for(idx)?.pending_bytes.take();
            if let Some(mut pending) = pending {
                let bytes = &mut pending.bytes;
                if bytes.len() >= 27 {
                    bytes[5] |= flags::LAST_PAGE;
                    // Zero out checksum field, recompute, patch back.
                    bytes[22..26].fill(0);
                    let crc = crate::crc::checksum(bytes);
                    bytes[22..26].copy_from_slice(&crc.to_le_bytes());
                }
                self.write_pending_page(idx, pending)?;
            }
        }
        Ok(())
    }

    /// Begin a new chain link (RFC 3533 §4 sequential multiplexing).
    ///
    /// Finalizes the current link — draining and EOS-terminating every one
    /// of its logical bitstreams so "the eos page of a given logical
    /// bitstream is immediately followed by the bos page of the next" —
    /// then writes the BOS + secondary-header pages of `streams` as a
    /// fresh link. Each new stream is assigned a serial that does not
    /// collide with any serial used by an earlier link ("Each chained
    /// logical bitstream MUST have a unique serial number within the scope
    /// of the physical bitstream"): a stream whose derived serial is
    /// already taken is bumped to the next free value.
    ///
    /// Subsequent [`Muxer::write_packet`] calls carry `stream_index`
    /// values addressing the *new* link's `StreamInfo::index` numbering.
    /// The demuxer recognises the boundary because the first BOS page of
    /// the new link follows the previous link's data pages (a
    /// BOS-after-non-BOS transition), assigning the new link its own
    /// `link_index`.
    ///
    /// Errors:
    /// * `write_header` has not run yet (a chain must have a first link);
    /// * the current link has written no content data page — a link with
    ///   only header pages would not be seen as a distinct link on
    ///   read-back (the demuxer keys link boundaries on BOS-after-data);
    /// * a Skeleton is attached — the muxer-built Skeleton control section
    ///   describes a single link's streams and its trailer-time
    ///   segment-length backfill assumes one link, so Skeleton + chaining
    ///   are mutually exclusive in this muxer;
    /// * `streams` is empty.
    pub fn begin_new_link(&mut self, streams: &[StreamInfo]) -> Result<()> {
        if !self.header_written {
            return Err(Error::other(
                "Ogg muxer: begin_new_link before write_header",
            ));
        }
        if self.trailer_written {
            return Err(Error::other(
                "Ogg muxer: begin_new_link after write_trailer",
            ));
        }
        if self.skeleton.is_some() || !self.auto_index.is_empty() {
            return Err(Error::invalid(
                "Ogg muxer: chained links are not supported alongside a Skeleton bitstream",
            ));
        }
        if !self.content_started {
            return Err(Error::invalid(
                "Ogg muxer: begin_new_link requires at least one content packet in the current link",
            ));
        }
        if streams.is_empty() {
            return Err(Error::invalid("Ogg muxer: begin_new_link with no streams"));
        }

        // 1. EOS-terminate every current-link stream. Its last data page
        //    is the non-BOS page the demuxer sees immediately before the
        //    next link's BOS.
        self.finalize_current_link()?;

        // 2. Re-arm per-stream writers for the new link with globally
        //    unique serials.
        let (per_stream, stream_order, _max) = build_link_writers(streams, &mut self.used_serials);
        self.per_stream = per_stream;
        self.stream_order = stream_order;
        self.streams = streams.to_vec();
        self.link_index = self.link_index.saturating_add(1);
        self.content_started = false;

        // 3. Write the new link's BOS + remaining header packets, exactly
        //    as write_header does for the first link (minus Skeleton,
        //    which chaining excludes).
        let stream_clone = self.streams.clone();
        let mut header_queues: Vec<(u32, oxideav_core::TimeBase, Vec<Vec<u8>>)> =
            Vec::with_capacity(stream_clone.len());
        for s in &stream_clone {
            let packets = extract_codec_headers(&s.params.codec_id, &s.params.extradata);
            header_queues.push((s.index, s.time_base, packets));
        }
        // BOS page per content stream, written immediately so all BOS
        // pages of the new link precede any of its data pages.
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
        // Remaining content-codec header packets — normal write_packet flow.
        for (idx, tb, packets) in &header_queues {
            for hp in packets.iter().skip(1) {
                let pkt = Packet::new(*idx, *tb, hp.clone());
                self.write_packet(&pkt)?;
            }
        }
        Ok(())
    }

    /// The zero-based index of the chain link currently being written
    /// (`0` before any [`begin_new_link`](Self::begin_new_link) call).
    /// After N successful `begin_new_link` calls this returns N, matching
    /// the demuxer's `stream_link_index` on read-back.
    pub fn link_index(&self) -> u32 {
        self.link_index
    }

    /// The on-wire `bitstream_serial_number` assigned to a stream of the
    /// *current* link, keyed by its `StreamInfo::index`. Returns `None`
    /// for an index not in the current link.
    pub fn stream_serial(&self, stream_index: u32) -> Option<u32> {
        self.per_stream.get(&stream_index).map(|w| w.serial)
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
        // Vorbis and Theora both store their 3-packet header sequence
        // (identification, comment, setup) as a single Xiph-laced blob in
        // `extradata` — the inverse of the demuxer's `xiph_lace_three`. Both
        // must be split back into their constituent packets so each rides on
        // the wire as a distinct Ogg packet; a Theora header blob muxed as one
        // packet would be unparseable by a Theora decoder.
        "vorbis" | "theora" => xiph_unlace(extradata).unwrap_or_default(),
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

/// Xiph-lace codec header packets into the single-blob `extradata`
/// format [`open`] expects for Vorbis and Theora streams (and the same
/// layout MP4/MKV use for those codecs): one byte `(packet_count - 1)`,
/// then for every packet but the last its length as a run of `0xFF`
/// bytes ending in a value `< 0xFF`, then the packets' bytes
/// back-to-back. The last packet's length is implicit (it runs to the
/// end of the blob).
///
/// This is the inverse of [`xiph_unlace`] and matches the `extradata`
/// the demuxer reports for such streams — so a codec crate that
/// produced its header packets (e.g. `oxideav-vorbis`'s
/// identification/comment/setup trio) can build a muxable
/// `StreamInfo` without hand-rolling the lacing.
///
/// Returns `None` when `packets` is empty or holds more than 256
/// entries (the count byte stores `packet_count - 1`).
#[must_use]
pub fn xiph_lace(packets: &[&[u8]]) -> Option<Vec<u8>> {
    if packets.is_empty() || packets.len() > 256 {
        return None;
    }
    let total: usize = packets.iter().map(|p| p.len()).sum();
    let mut out = Vec::with_capacity(1 + packets.len() + total);
    out.push((packets.len() - 1) as u8);
    for p in &packets[..packets.len() - 1] {
        let mut n = p.len();
        while n >= 255 {
            out.push(255);
            n -= 255;
        }
        out.push(n as u8);
    }
    for p in packets {
        out.extend_from_slice(p);
    }
    Some(out)
}

/// Parse a Xiph-laced header blob (Vorbis/Theora `extradata` layout)
/// back into its constituent packets — the inverse of [`xiph_lace`].
/// The first byte is `(packet_count - 1)`, followed by
/// `(packet_count - 1)` lacing records (each a series of `0xFF`
/// continuation bytes ending in a value `< 0xFF`); the last packet
/// runs to the end of the blob.
///
/// Returns `None` when the blob is empty or its declared sizes
/// overrun the buffer.
#[must_use]
pub fn xiph_unlace(buf: &[u8]) -> Option<Vec<Vec<u8>>> {
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

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn xiph_lace_round_trips_three_packets() {
        let id = vec![0x01u8; 30];
        let comment = vec![0x03u8; 300]; // needs a 255-continuation size byte
        let setup = vec![0x05u8; 700];
        let blob = xiph_lace(&[&id, &comment, &setup]).unwrap();
        assert_eq!(blob[0], 0x02); // 3 packets - 1
        let packets = xiph_unlace(&blob).unwrap();
        assert_eq!(packets, vec![id, comment, setup]);
    }

    #[test]
    fn xiph_lace_handles_exact_255_multiples_and_empties() {
        // A 255-byte packet laces its size as [255, 0].
        let a = vec![0xAAu8; 255];
        let b: Vec<u8> = Vec::new();
        let c = vec![0xCCu8; 3];
        let blob = xiph_lace(&[&a, &b, &c]).unwrap();
        let packets = xiph_unlace(&blob).unwrap();
        assert_eq!(packets, vec![a, b, c]);
        // Single packet: no size records, just the count byte.
        let solo = xiph_lace(&[&[1u8, 2, 3][..]]).unwrap();
        assert_eq!(solo, vec![0x00, 1, 2, 3]);
        assert_eq!(xiph_unlace(&solo).unwrap(), vec![vec![1u8, 2, 3]]);
    }

    #[test]
    fn xiph_lace_rejects_empty_and_oversize_inputs() {
        assert!(xiph_lace(&[]).is_none());
        let one = [0u8; 1];
        let too_many: Vec<&[u8]> = vec![&one; 257];
        assert!(xiph_lace(&too_many).is_none());
        // Unlace rejects an empty blob and a truncated size table.
        assert!(xiph_unlace(&[]).is_none());
        assert!(xiph_unlace(&[0x02, 255]).is_none());
    }

    #[test]
    fn xiph_lace_matches_extract_codec_headers() {
        // The muxer's extradata parser must accept what xiph_lace built.
        let id = {
            let mut p = vec![0x01u8];
            p.extend_from_slice(b"vorbis");
            p
        };
        let comment = vec![0x03u8; 12];
        let setup = vec![0x05u8; 40];
        let blob = xiph_lace(&[&id, &comment, &setup]).unwrap();
        let packets = extract_codec_headers(&CodecId::new("vorbis"), &blob);
        assert_eq!(packets, vec![id, comment, setup]);
    }
}
