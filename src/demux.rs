//! Ogg demuxer: page reader → per-stream packet reassembly.

use std::collections::HashMap;
use std::io::{Read, SeekFrom};

use oxideav_core::{
    CodecId, CodecParameters, CodecResolver, Error, MediaType, Packet, Result, StreamInfo, TimeBase,
};
use oxideav_core::{Demuxer, ReadSeek};

use crate::codec_id;
use crate::page::{self, Page};
use crate::skeleton::{self, FisBone, FisHead, SkelIndex, Skeleton};

/// Open an Ogg bitstream.
pub fn open(input: Box<dyn ReadSeek>, _codecs: &dyn CodecResolver) -> Result<Box<dyn Demuxer>> {
    let mut state = OggDemuxer::new(input);
    state.read_bos_section()?;
    state.read_until_headers_collected()?;
    state.populate_extradata();
    state.populate_metadata();
    state.anchor_start_times_from_skeleton();
    state.populate_duration();
    Ok(Box::new(state))
}

/// Open an Ogg bitstream and pre-build the page-level seek index by
/// scanning every page header in the file once. The index makes
/// subsequent [`Demuxer::seek_to`] calls O(log n) lookup + a single seek
/// instead of a log-n bisection that re-reads the file each time. Pages
/// with granule `-1` (no packet boundary) are skipped per RFC 3533 §6.
///
/// Returns the same boxed [`Demuxer`] as [`open`]; the index lives
/// inside the concrete type and accelerates seek_to transparently.
pub fn open_indexed(
    input: Box<dyn ReadSeek>,
    codecs: &dyn CodecResolver,
) -> Result<Box<dyn Demuxer>> {
    let mut state = open_concrete(input, codecs)?;
    state.build_seek_index()?;
    Ok(Box::new(state))
}

/// Open an Ogg bitstream and return the concrete [`OggDemuxer`] type
/// (rather than a boxed trait object). Useful for callers that want to
/// invoke [`OggDemuxer::build_seek_index`] / [`OggDemuxer::seek_index_len`]
/// on demand without going through trait downcasts.
pub fn open_concrete(input: Box<dyn ReadSeek>, _codecs: &dyn CodecResolver) -> Result<OggDemuxer> {
    let mut state = OggDemuxer::new(input);
    state.read_bos_section()?;
    state.read_until_headers_collected()?;
    state.populate_extradata();
    state.populate_metadata();
    state.anchor_start_times_from_skeleton();
    state.populate_duration();
    Ok(state)
}

/// Per-codec strategy for the bisection-path comparison axis in
/// [`OggDemuxer::seek_to`]. The bisection works by comparing each page's
/// "ordering key" (`key_of(granule)`) against a target key derived from
/// the user-supplied `pts`. For most codecs the granule itself is the
/// key; Theora layers a `(keyframe << shift) | offset` packing on top of
/// its native granule space so the comparison axis is the underlying
/// frame number, derived from the Skeleton 4.0 `fisbone` per-stream
/// `granuleshift` + `granule_rate`.
#[derive(Clone, Copy)]
struct SeekKey {
    target_key: i64,
    flavor: SeekKeyFlavor,
}

#[derive(Clone, Copy)]
enum SeekKeyFlavor {
    /// `key_of(g) == g - granule_offset`. For Vorbis / FLAC / Speex the
    /// offset is 0, so the page's raw granule IS the comparison key and
    /// `pts` IS the target granule. For **Opus** the offset is the stream's
    /// pre-skip: the user-supplied `pts` is a *PCM sample position*
    /// (playback time, RFC 7845 §4.3), but a page's on-wire granule counts
    /// 48 kHz samples *including* the pre-skip padding, so the comparison
    /// axis is `granule − pre-skip = PCM sample position`. Subtracting the
    /// offset on the page side lines both sides up in PCM-position space, so
    /// a `seek_to(pts)` lands on the page whose PCM position floors the
    /// target rather than `pre-skip / 48000` s early.
    Identity { granule_offset: i64 },
    /// `key_of(g) == (g >> shift) + (g & ((1 << shift) - 1))`. Used by
    /// Theora: the encoded granule packs a keyframe index in the upper
    /// `64-shift` bits and a frame offset from that keyframe in the
    /// lower `shift` bits. The sum of the two is the absolute frame
    /// number. With `shift == 0` the offset half is empty and the key
    /// collapses to the raw granule, which is also a valid frame
    /// number — so the same codec path can also drive seeks on
    /// pre-Skeleton (shift-unset) fisbones, though that's vanishingly
    /// rare in practice (Theora streams in the wild almost always
    /// declare a non-zero shift).
    TheoraFrame { shift: u32 },
}

impl SeekKey {
    fn identity(target: i64) -> Self {
        Self {
            target_key: target,
            flavor: SeekKeyFlavor::Identity { granule_offset: 0 },
        }
    }

    /// Identity axis offset by a per-stream granule bias — used for Opus,
    /// whose pages carry `PCM position + pre-skip` (RFC 7845 §4.3). The
    /// target stays in PCM-position space; the page side has the bias
    /// subtracted by `key_of`.
    fn identity_offset(target: i64, granule_offset: i64) -> Self {
        Self {
            target_key: target,
            flavor: SeekKeyFlavor::Identity { granule_offset },
        }
    }

    fn theora_frame(target_frame: i64, shift: u32) -> Self {
        Self {
            target_key: target_frame,
            flavor: SeekKeyFlavor::TheoraFrame { shift },
        }
    }

    /// Map an on-wire page granule into the comparison key.
    fn key_of(&self, granule: i64) -> i64 {
        match self.flavor {
            // The `-1` "no packet finishes on this page" sentinel (RFC 3533
            // §6) passes through so the comparator keeps treating it as
            // "smaller than every real page" — matching `theora_frame_no`'s
            // sentinel handling. For a real granule, subtract the per-stream
            // bias (0 for Vorbis / FLAC / Speex; the pre-skip for Opus) so
            // the axis is the PCM sample position, clamped at 0 so a page
            // whose granule is below the bias (a pre-skip-region page) does
            // not wrap negative and mis-sort.
            SeekKeyFlavor::Identity { granule_offset } => {
                if granule < 0 {
                    granule
                } else {
                    (granule - granule_offset).max(0)
                }
            }
            SeekKeyFlavor::TheoraFrame { shift } => theora_frame_no(granule, shift),
        }
    }
}

/// Decode the Theora granule packing `granule = (frame_idx_of_last_keyframe
/// << shift) | (frame_offset_from_last_keyframe)` into the absolute frame
/// index. Per `docs/container/ogg/ogg-skeleton-4.0.md` (granuleshift is
/// "the number of lower bits from the granulepos field that are used to
/// provide position information for sub-seekable units (like the
/// keyframe shift in theora)"), the sum of the two halves is the
/// absolute frame number.
///
/// Negative granules (`-1` "no packet finishes on this page" per RFC 3533
/// §6) are returned as-is so that the bisection comparator treats them
/// as "smaller than every real page" — matching the existing
/// "skip pages with granule -1" convention used elsewhere in this
/// module.
///
/// A `shift >= 63` is clamped to a frame number of `0`; the spec
/// doesn't write down a maximum value and a shift past `63` would mean
/// every bit is "offset" with no room for a keyframe index, which is
/// nonsensical. Round 227 prefers a degenerate but well-defined output
/// over a panic so a misbuilt or attacker-edited fisbone cannot crash
/// the seek path.
fn theora_frame_no(granule: i64, shift: u32) -> i64 {
    if granule < 0 {
        return granule;
    }
    if shift == 0 {
        return granule;
    }
    if shift >= 63 {
        return 0;
    }
    let g = granule as u64;
    let kf = (g >> shift) as i64;
    let off = (g & ((1u64 << shift) - 1)) as i64;
    kf.saturating_add(off)
}

/// Whether the last frame finishing on a page with raw on-wire `granule` is a
/// keyframe, for a track whose Skeleton fisbone declares `granuleshift`.
///
/// `docs/container/ogg/ogg-skeleton-4.0.md` defines the granuleshift as the
/// "number of lower bits from the granulepos field that are used to provide
/// position information for sub-seekable units (like the keyframe shift in
/// theora)": the high `64 - shift` bits hold the index of the last keyframe and
/// the low `shift` bits hold the offset of the current frame *since* that
/// keyframe. The frame is therefore a keyframe exactly when that offset is
/// zero — the frame coincides with the keyframe it counts from.
///
/// For a `shift == 0` track (every audio mapping — Vorbis / Opus / FLAC /
/// Speex — packs no keyframe index, and every packet is an independent
/// random-access point) this returns `true`. The `-1` "no packet finishes on
/// this page" sentinel and a degenerate `shift >= 63` also return `true`
/// (conservatively random-access, matching the seek axis's pass-through of
/// those values) so callers never under-report a random-access point.
fn granule_is_keyframe(granule: i64, shift: u32) -> bool {
    if granule < 0 || shift == 0 || shift >= 63 {
        return true;
    }
    (granule as u64) & ((1u64 << shift) - 1) == 0
}

/// Outcome of the Skeleton 4.0 keyframe-index fast-path lookup.
///
/// Carries the byte offset chosen by the multi-stream minimisation
/// (per `docs/container/ogg/ogg-skeleton-4.0.md` §"Keyframe indexes
/// for faster seeking"), the serial of the stream whose keypoint won
/// that minimisation (used by the per-keypoint validity check, which
/// expects the page at the offset to belong to that serial — not
/// necessarily the originally-requested stream), and the granule the
/// public `seek_to` contract should return, which is always expressed
/// in the requested stream's own time-base units.
struct SkeletonIndexSeek {
    byte_offset: u64,
    winning_serial: u32,
    returned_granule: i64,
}

struct LogicalStream {
    /// Index into the public `streams` vec.
    public_index: usize,
    /// Buffered partial-packet bytes from a previous page that ended without
    /// a terminator (lacing 255). Concatenated with the next page's leading
    /// segments to form a complete packet.
    pending: Vec<u8>,
    /// Number of header packets still to be absorbed (not delivered).
    headers_remaining: usize,
    /// Header packets accumulated so far — used to populate codec-specific
    /// extradata on the stream's `CodecParameters` once they're all in.
    header_packets: Vec<Vec<u8>>,
    granule_seen: i64,
    /// Chained-link index this stream belongs to. RFC 3533 §4: chained Ogg
    /// is the concatenation of independent logical bitstreams; the first
    /// link is index 0, each subsequent BOS-after-non-BOS group increments
    /// the counter. Streams sharing a link play concurrently (multiplex);
    /// streams in different links play sequentially.
    link_index: u32,
    /// `page_sequence_number` of the last page consumed for this stream, or
    /// `None` before the first page. RFC 3533 §6 field 6: this counter
    /// "is increasing on each logical bitstream separately" and exists "so
    /// the decoder can identify page loss." A consumed page whose `seq_no`
    /// is not exactly `last_seq + 1` signals one or more dropped pages — a
    /// "hole" — which the demuxer must not paper over by silently splicing
    /// the broken packet halves together.
    last_seq: Option<u32>,
    /// True when the packet currently buffered in `pending` was left
    /// unterminated by a page that ended with a 255-lacing segment AND that
    /// page was not itself a continuation orphaned by a hole. Cleared once
    /// the packet completes or a hole invalidates the partial bytes.
    pending_valid: bool,
    /// True once `process_page` has handled this serial's BOS page (the
    /// initial header-capture drain). `read_bos_section` registers a stream
    /// and queues its BOS page, which `process_page` then re-processes during
    /// header collection — that first re-processing is normal and sets this
    /// flag. A BOS page for a serial whose stream already has this flag set
    /// is a genuine RFC 3533 §4 unique-serial violation (a second grouped or
    /// chained BOS reusing the serial), not the expected initial drain.
    bos_processed: bool,
}

/// Concrete Ogg demuxer state. Most callers should use the boxed
/// [`Demuxer`] returned by [`open`] / [`open_indexed`]; this type is
/// public so consumers who want the inherent [`build_seek_index`] /
/// [`seek_index_len`] API can hold it directly.
pub struct OggDemuxer {
    input: Box<dyn ReadSeek>,
    streams: Vec<StreamInfo>,
    state_by_serial: HashMap<u32, LogicalStream>,
    /// Pages we've already read but not yet drained for packets.
    page_queue: std::collections::VecDeque<Page>,
    /// Packets ready to emit, in insertion order across all streams.
    out_queue: std::collections::VecDeque<Packet>,
    /// True once we've read past the BOS section and into the data pages.
    eof_reached: bool,
    metadata: Vec<(String, String)>,
    duration_micros: i64,
    /// Per-serial sorted page-level seek index. Each value is a list of
    /// `(granule, page_offset)` ordered by `granule`. Pages with granule
    /// `-1` (no packet terminates on the page) are NOT indexed because
    /// they carry no usable seek timestamp. Built lazily as pages flow
    /// through the demuxer, and may be densely pre-populated by
    /// [`OggDemuxer::build_seek_index`].
    seek_index: HashMap<u32, Vec<(i64, u64)>>,
    /// True once `build_seek_index` has run successfully, so we don't
    /// repeat the full-file scan on later calls.
    seek_index_built: bool,
    /// Next chained-link index to assign on the next BOS-after-non-BOS.
    /// Initialised to 0; the very first BOS section all shares link 0,
    /// and the counter increments on each subsequent BOS that follows
    /// at least one non-BOS page (the RFC 3533 §4 "link boundary").
    next_link_index: u32,
    /// True once we've seen any non-BOS page in the current link. Resets
    /// to false when a new link begins (so its multiplexed BOS pages all
    /// share the same link index).
    seen_nonbos_in_current_link: bool,
    /// Number of page-loss "holes" detected during demux. RFC 3533 §6
    /// field 6: a logical stream's `page_sequence_number` increases by one
    /// per page; a jump signals dropped pages. Each gap (regardless of how
    /// many pages went missing) counts as one hole. Surfaced via
    /// [`OggDemuxer::hole_count`] for diagnostics and tests.
    holes: u64,
    /// Number of `continued`-flag (RFC 3533 §6 field 3, header_type bit
    /// 0x01) framing inconsistencies detected during demux. The bit is a
    /// normative declaration about packet reassembly: "set: page contains
    /// data of a packet continued from the previous page; unset: page
    /// contains a fresh packet." A page whose bit disagrees with the
    /// demuxer's own reassembly state is a framing error independent of any
    /// `page_sequence_number` gap:
    ///   * bit SET but no valid partial packet is buffered — the leading
    ///     segment is an orphaned continuation tail whose head never arrived
    ///     (or arrived but already terminated); it is dropped, not spliced.
    ///   * bit UNSET but a partial packet IS buffered — the previous page
    ///     promised a continuation (it ended on a 255-lacing segment) yet
    ///     this page abandons it by declaring a fresh packet; the orphaned
    ///     partial head is dropped.
    ///
    /// Discontinuities that the hole counter already accounted for in the
    /// same page are NOT double-counted here. Surfaced via
    /// [`OggDemuxer::framing_error_count`].
    framing_errors: u64,
    /// Number of times the demuxer had to *recapture* page sync after a
    /// parsing error. RFC 3533 §3 lists "recapture after a parsing error"
    /// as a design requirement, and §6 field 1 (capture_pattern) says the
    /// `OggS` magic "helps a decoder to find the page boundaries and regain
    /// synchronisation after parsing a corrupted stream. Once the capture
    /// pattern is found, the decoder verifies page sync and integrity by
    /// computing and comparing the checksum." When a read finds bytes that
    /// are not a valid page (garbage spliced between pages, or a page whose
    /// CRC fails), the demuxer scans forward byte-by-byte for the next
    /// `OggS` whose header + body parse with a matching checksum, then
    /// resumes there. Each such recovery — however many bytes were skipped —
    /// counts as one resync. Surfaced via [`OggDemuxer::resync_count`].
    resyncs: u64,
    /// Parsed Skeleton metadata (fishead + fisbones + 4.0 indexes). Set
    /// when the demuxer sees a `fishead\0` BOS page as the very first
    /// BOS of the file. `None` for streams without Skeleton.
    ///
    /// The Skeleton logical bitstream is NOT exposed via the public
    /// `streams()` list — it has no content packets and exists purely
    /// to describe the *other* logical bitstreams. Callers retrieve
    /// its parsed state via [`OggDemuxer::skeleton`].
    skeleton: Option<Skeleton>,
    /// `bitstream_serial_number` of the Skeleton logical bitstream, if
    /// any. Used internally to route Skeleton stream packets away from
    /// the public stream-packet path.
    skeleton_serial: Option<u32>,
    /// Buffered partial-packet bytes for the Skeleton logical bitstream.
    /// Skeleton packets typically fit in a single page, but the framing
    /// itself does not preclude them spanning page boundaries.
    skeleton_pending: Vec<u8>,
    /// `last_seq` tracker for the Skeleton stream (matches the
    /// `LogicalStream::last_seq` semantics of content streams).
    skeleton_last_seq: Option<u32>,
    /// Set once the demuxer consumes the Skeleton EOS page — the empty
    /// packet that closes the control section before any content pages
    /// appear (`ogg-skeleton-{3,4}.0.md`). The initial open() flow waits
    /// for this so callers can read [`OggDemuxer::skeleton`] immediately
    /// after `open` and see every fisbone / index packet.
    skeleton_eos_seen: bool,
    /// Number of times [`seek_to`](oxideav_core::Demuxer::seek_to)
    /// satisfied a request directly from a Skeleton 4.0 `index\0` packet
    /// (`docs/container/ogg/ogg-skeleton-4.0.md`) without paying for
    /// either page-bisection or the [`build_seek_index`] full-file scan.
    /// Stays at 0 when no Skeleton index is present for the requested
    /// stream's serial. Surfaced via
    /// [`OggDemuxer::skeleton_index_seek_count`].
    skeleton_index_seeks: u64,
    /// Number of times the Skeleton 4.0 fast-path keypoint was rejected
    /// because the per-spec validity checks failed
    /// (`docs/container/ogg/ogg-skeleton-4.0.md` §"Keyframe indexes for
    /// faster seeking" — three conditions: segment length mismatch,
    /// keypoint offset not on a page boundary, keypoint offset's page
    /// belongs to a different serial). Each rejection forces the seek
    /// to fall back to the page-level index_floor / bisection path,
    /// which is correct but pays the slower I/O cost. A non-zero count
    /// surfaces "this file's Skeleton index is stale or corrupted"
    /// without losing the seek result. Surfaced via
    /// [`OggDemuxer::skeleton_index_invalid_count`].
    skeleton_index_rejects: u64,
    /// Cached result of validating the Skeleton 4.0 BOS `Segment length
    /// in bytes` field against the actual file length. Computed lazily
    /// on the first [`Demuxer::seek_to`] call after `open`, then reused
    /// across subsequent calls.
    ///
    /// * `None` — not yet computed, or no Skeleton / no 4.0 segment-length
    ///   field present (3.0 streams; 4.0 streams that left segment_length
    ///   at 0 to opt out of this check).
    /// * `Some(true)` — segment-length matches the file, index believed
    ///   trustworthy at the file level.
    /// * `Some(false)` — segment-length mismatches the file, the spec
    ///   says the index is invalid; fast path is skipped.
    skeleton_segment_length_ok: Option<bool>,
    /// Number of [`OggDemuxer::seek_to_with_preroll`] calls that actually
    /// backed the resume offset up to honour a non-zero per-track preroll
    /// (`docs/container/ogg/ogg-skeleton-4.0.md` §"How to describe the
    /// logical bitstreams within an Ogg container?": "the preroll: the
    /// number of past content packets to take into account when decoding
    /// the current Ogg page, which is necessary for seeking"). Stays at 0
    /// when no Skeleton fisbone records a preroll for the requested
    /// stream, when the recorded preroll is 0, or when the landed page is
    /// already the stream's first content page (no earlier page to back
    /// up to). Surfaced via [`OggDemuxer::preroll_seek_count`].
    preroll_seeks: u64,
    /// Number of `bitstream_serial_number` collisions the demuxer has
    /// observed — a violation of the RFC 3533 §4 normative rule that
    /// "Each grouped logical bitstream MUST have a unique serial number
    /// within the scope of the physical bitstream" (and the identical
    /// rule for chained bitstreams). A collision is a BOS page whose
    /// serial is already live in `state_by_serial`: either two grouped
    /// streams in the same link sharing a serial, or a chained link
    /// reusing a serial a prior link already used. The demuxer recovers
    /// by treating the second BOS as a logical restart of that serial
    /// (its stale reassembly buffer is dropped and `last_seq` reset so
    /// the new bitstream's packets are not spliced onto the old one's
    /// pending bytes), and the link index is updated to the new BOS's
    /// link so subsequent diagnostics attribute the serial to the link
    /// it now belongs to. Surfaced via
    /// [`OggDemuxer::duplicate_serial_count`].
    duplicate_serials: u64,
    /// Per-serial Opus **pre-skip** (`docs/audio/opus/rfc7845-ogg-opus.txt`
    /// §5.1 field 4: "the number of samples (at 48 kHz) to discard from the
    /// decoder output when starting playback, and also the number to
    /// subtract from a page's granule position to calculate its PCM sample
    /// position"). Read from the `OpusHead` ID header at BOS time, in the
    /// 48 kHz granule units Opus always uses. RFC 7845 §4.3 makes the
    /// granule→time mapping `PCM sample position = granule position −
    /// pre-skip`; without subtracting it the demuxer over-reports an Opus
    /// stream's duration by `pre-skip / 48000` seconds (a non-Opus stream
    /// never has an entry here, so its granule passes through unchanged).
    opus_pre_skip: HashMap<u32, u16>,
}

impl OggDemuxer {
    fn new(input: Box<dyn ReadSeek>) -> Self {
        Self {
            input,
            streams: Vec::new(),
            state_by_serial: HashMap::new(),
            page_queue: std::collections::VecDeque::new(),
            out_queue: std::collections::VecDeque::new(),
            eof_reached: false,
            metadata: Vec::new(),
            duration_micros: 0,
            seek_index: HashMap::new(),
            seek_index_built: false,
            next_link_index: 0,
            seen_nonbos_in_current_link: false,
            holes: 0,
            framing_errors: 0,
            resyncs: 0,
            skeleton: None,
            skeleton_serial: None,
            skeleton_pending: Vec::new(),
            skeleton_last_seq: None,
            skeleton_eos_seen: false,
            skeleton_index_seeks: 0,
            skeleton_index_rejects: 0,
            skeleton_segment_length_ok: None,
            preroll_seeks: 0,
            duplicate_serials: 0,
            opus_pre_skip: HashMap::new(),
        }
    }

    /// Parsed Ogg Skeleton metadata bitstream, if the file's first BOS
    /// page was a `fishead\0` ident packet (Skeleton 3.0 / 4.0).
    ///
    /// Skeleton is the metadata logical bitstream that describes the
    /// other logical bitstreams in the same physical stream — per-track
    /// MIME type, role, name, granule rate, preroll, and (4.0 only) a
    /// keyframe index. The Skeleton stream itself has no content
    /// packets, so it is not exposed in [`Demuxer::streams`]; callers
    /// read its parsed state through this accessor instead.
    ///
    /// Returns `None` for files without Skeleton; the demuxer otherwise
    /// behaves identically (Skeleton is purely additive).
    pub fn skeleton(&self) -> Option<&Skeleton> {
        self.skeleton.as_ref()
    }

    /// Record a page's `(granule, byte_offset)` into the per-serial seek
    /// index, keeping each serial's list sorted by granule. Pages whose
    /// granule is `-1` (RFC 3533 §6: "no packets finish on this page")
    /// carry no seek-target information and are skipped. Duplicate
    /// inserts (same offset already present at the same granule) are
    /// suppressed so re-scans are idempotent.
    fn index_record(&mut self, serial: u32, granule: i64, offset: u64) {
        if granule < 0 {
            return;
        }
        let entries = self.seek_index.entry(serial).or_default();
        match entries.binary_search_by(|(g, o)| g.cmp(&granule).then_with(|| o.cmp(&offset))) {
            Ok(_) => {} // already present
            Err(pos) => entries.insert(pos, (granule, offset)),
        }
    }

    /// Look up the seek index for the largest entry on `serial` whose
    /// *mapped* key, computed by `key_of`, is `<= target_key`. Returns
    /// `(granule, page_offset)` if any such entry exists. Used by the
    /// codec-aware seek path so a comparison axis other than the raw
    /// granule (e.g. Theora frame number derived from the encoded
    /// `(keyframe << shift) | offset` granule layout) can drive the
    /// floor lookup.
    ///
    /// The seek_index is sorted by raw granule, and the codec-aware
    /// mapping (currently only Theora's `(g >> shift) + (g & mask)`)
    /// is monotonically non-decreasing as a function of raw granule
    /// for any single logical stream (proof in `seek_to`'s comment
    /// block), so a linear scan from the right finds the floor in the
    /// raw-granule order and the first entry with `key_of(g) <=
    /// target_key` is the rightmost mapped-floor entry. The walk is
    /// bounded by the index length and runs at most once per `seek_to`
    /// call, so even the linear path is cheap.
    fn index_floor_by<F>(&self, serial: u32, target_key: i64, key_of: F) -> Option<(i64, u64)>
    where
        F: Fn(i64) -> i64,
    {
        let entries = self.seek_index.get(&serial)?;
        if entries.is_empty() {
            return None;
        }
        // Walk right-to-left since `entries` is sorted by raw granule
        // and the mapped key is monotonic in raw granule per-stream:
        // the first entry whose mapped key is `<= target_key` is the
        // rightmost mapped-floor entry.
        for &(g, off) in entries.iter().rev() {
            if key_of(g) <= target_key {
                return Some((g, off));
            }
        }
        None
    }

    /// Walk every page header in the file once, recording
    /// `(serial, granule, page_offset)` into the seek index. Only the
    /// 27-byte page header + N-byte segment table are read; payload is
    /// skipped over with a single relative seek per page, so cost is
    /// O(pages) seeks, not O(bytes).
    ///
    /// After this returns, [`seek_to`] becomes O(log n) lookup + one
    /// seek for any covered timestamp on any logical stream. Pages
    /// whose granule is `-1` (no packet boundary, RFC 3533 §6) are
    /// skipped because they carry no seek-target information.
    ///
    /// On error the index is left partially populated — subsequent
    /// `seek_to` calls remain correct (they fall back to bisection for
    /// uncovered targets); only the speedup is incomplete.
    pub fn build_seek_index(&mut self) -> Result<()> {
        let saved_pos = self.input.stream_position()?;
        let end = self.input.seek(SeekFrom::End(0))?;
        if end == 0 {
            self.input.seek(SeekFrom::Start(saved_pos))?;
            self.seek_index_built = true;
            return Ok(());
        }

        // Scan from byte 0 — every Ogg page starts with `OggS`.
        let mut cursor: u64 = 0;
        // Serials whose BOS page we've already seen *within this scan*. The
        // scan re-walks the whole file (including the initial BOS pages that
        // `open` already registered), so `state_by_serial.contains_key` cannot
        // distinguish a genuine RFC 3533 §4 serial collision from re-meeting a
        // page we registered at open time. A serial appearing as a BOS twice
        // *in this single scan* is the real violation; this set scopes the
        // collision check to the scan so the re-walk does not over-count.
        let mut bos_serials_this_scan: std::collections::HashSet<u32> =
            std::collections::HashSet::new();
        // This scan visits every page in the file exactly once, so it is the
        // authoritative source for the file-wide duplicate-serial tally. Reset
        // the counter to the scan's own findings rather than adding to whatever
        // `next_packet` (which may have partially drained the file) accumulated,
        // so a caller that runs `build_seek_index` gets the true file-wide count
        // and `open_indexed` (build immediately after open) does not double-count
        // duplicates the subsequent `next_packet` walk re-observes.
        self.duplicate_serials = 0;
        // Re-use a chunk buffer to find `OggS` captures cheaply.
        const CHUNK: u64 = 64 * 1024;
        while cursor < end {
            self.input.seek(SeekFrom::Start(cursor))?;
            let want = CHUNK.min(end - cursor) as usize;
            let mut buf = vec![0u8; want];
            let mut filled = 0usize;
            while filled < buf.len() {
                match self.input.read(&mut buf[filled..]) {
                    Ok(0) => break,
                    Ok(n) => filled += n,
                    Err(e) if e.kind() == std::io::ErrorKind::Interrupted => continue,
                    Err(e) => return Err(e.into()),
                }
            }
            buf.truncate(filled);
            if buf.is_empty() {
                break;
            }

            // Find the first OggS capture in the chunk.
            let mut i = 0usize;
            let mut advanced = false;
            while i + 4 <= buf.len() {
                if &buf[i..i + 4] != b"OggS" {
                    i += 1;
                    continue;
                }
                let page_off = cursor + i as u64;

                // Re-seek to the candidate, read header + segment table.
                self.input.seek(SeekFrom::Start(page_off))?;
                let mut hdr = [0u8; 27];
                if self.input.read(&mut hdr)? != 27 {
                    // Truncated tail — we're done.
                    self.input.seek(SeekFrom::Start(saved_pos))?;
                    self.seek_index_built = true;
                    return Ok(());
                }
                if hdr[0..4] != page::CAPTURE_PATTERN {
                    // False positive inside some payload byte stream
                    // (rare, but possible). Skip past this byte and
                    // keep scanning.
                    i += 1;
                    continue;
                }
                let flags_byte = hdr[5];
                let granule = i64::from_le_bytes([
                    hdr[6], hdr[7], hdr[8], hdr[9], hdr[10], hdr[11], hdr[12], hdr[13],
                ]);
                let serial = u32::from_le_bytes([hdr[14], hdr[15], hdr[16], hdr[17]]);
                let n_segs = hdr[26] as usize;
                let mut lacing = vec![0u8; n_segs];
                if self.input.read(&mut lacing)? != n_segs {
                    self.input.seek(SeekFrom::Start(saved_pos))?;
                    self.seek_index_built = true;
                    return Ok(());
                }
                let data_len: u64 = lacing.iter().map(|&v| v as u64).sum();
                self.index_record(serial, granule, page_off);

                // Chained-link discovery (RFC 3533 §4): a BOS page for an
                // unfamiliar serial encountered during the scan is the
                // start of a new logical bitstream. If we've already seen
                // any non-BOS page since the most recent link change, the
                // new BOS opens a new chained link; otherwise it joins
                // the current link's multiplex. Pre-registering the
                // stream here means a post-scan duration calculation can
                // attribute that link's last granule to a known time
                // base, even before any packet flows through the demuxer.
                let is_bos = flags_byte & page::flags::FIRST_PAGE != 0;
                if is_bos {
                    // A BOS serial seen for the SECOND time in this scan is an
                    // RFC 3533 §4 unique-serial violation. (`state_by_serial`
                    // alone can't tell this apart from re-meeting an
                    // open()-registered initial BOS, since the scan re-walks
                    // the whole file — hence the scan-scoped set.) Count it,
                    // open a new link if it followed a data page, and do NOT
                    // re-register a duplicate stream slot.
                    let is_skeleton = Some(serial) == self.skeleton_serial;
                    if !is_skeleton && !bos_serials_this_scan.insert(serial) {
                        if self.seen_nonbos_in_current_link {
                            self.next_link_index = self.next_link_index.saturating_add(1);
                            self.seen_nonbos_in_current_link = false;
                        }
                        self.duplicate_serials += 1;
                    } else if !self.state_by_serial.contains_key(&serial) {
                        // Read the page payload so we can parse the first
                        // packet for codec_id + sample_rate. Identification
                        // packets must fit in a single page per the various
                        // codec mapping RFCs, so we only need this page's
                        // data, not any continuation.
                        let mut data = vec![0u8; data_len as usize];
                        if self.input.read(&mut data)? != data_len as usize {
                            self.input.seek(SeekFrom::Start(saved_pos))?;
                            self.seek_index_built = true;
                            return Ok(());
                        }
                        let synth = Page {
                            flags: flags_byte,
                            granule_position: granule,
                            serial,
                            seq_no: u32::from_le_bytes([hdr[18], hdr[19], hdr[20], hdr[21]]),
                            lacing: lacing.clone(),
                            data,
                        };
                        if self.seen_nonbos_in_current_link {
                            self.next_link_index = self.next_link_index.saturating_add(1);
                            self.seen_nonbos_in_current_link = false;
                        }
                        // Best-effort: ignore registration failure (a malformed
                        // BOS shouldn't abort the seek-index build).
                        let _ = self.register_stream(&synth);
                    }
                } else {
                    self.seen_nonbos_in_current_link = true;
                }

                // Jump cursor past the entire page body and resume
                // chunked OggS-hunting from there.
                cursor = page_off + 27 + n_segs as u64 + data_len;
                advanced = true;
                break;
            }
            if !advanced {
                // No OggS found in this chunk — advance past it (minus
                // a 3-byte tail to catch captures straddling the
                // boundary).
                if buf.len() < 4 {
                    break;
                }
                cursor += buf.len() as u64 - 3;
            }
        }

        self.input.seek(SeekFrom::Start(saved_pos))?;
        self.seek_index_built = true;
        // Now that every link's BOS has been registered and every page's
        // granule is indexed, recompute total duration as the sum of
        // per-link max granule (chained playback is sequential) rather
        // than the max-over-streams (which only works for a single
        // multiplexed link).
        self.populate_duration_from_index();
        Ok(())
    }

    /// Recompute `duration_micros` from the (fully populated) seek index.
    /// For chained Ogg files (multiple links), playback is sequential —
    /// total duration is the SUM of each link's duration, where each
    /// link's duration is the max-over-its-streams of last-granule
    /// translated through that stream's time_base. For non-chained
    /// (single-link) files, the result reduces to max-over-streams,
    /// matching `populate_duration`'s near-tail scan.
    fn populate_duration_from_index(&mut self) {
        // Group serials by link_index.
        let mut by_link: HashMap<u32, Vec<u32>> = HashMap::new();
        for (serial, state) in &self.state_by_serial {
            by_link.entry(state.link_index).or_default().push(*serial);
        }
        let mut total_micros: i64 = 0;
        for serials in by_link.values() {
            let mut link_micros: i64 = 0;
            for serial in serials {
                let Some(entries) = self.seek_index.get(serial) else {
                    continue;
                };
                let last_granule = entries.last().map(|(g, _)| *g).unwrap_or(0);
                if !self.state_by_serial.contains_key(serial) {
                    continue;
                };
                let us = self.granule_to_micros_for_serial(*serial, last_granule);
                if us > link_micros {
                    link_micros = us;
                }
            }
            total_micros = total_micros.saturating_add(link_micros);
        }
        if total_micros > 0 {
            self.duration_micros = total_micros;
        }
    }

    /// Total number of indexed pages across all logical streams. Useful
    /// for tests and external profiling. Excludes pages whose granule
    /// position is `-1`.
    pub fn seek_index_len(&self) -> usize {
        self.seek_index.values().map(|v| v.len()).sum()
    }

    /// Number of page-loss "holes" the demuxer has detected so far across
    /// all logical streams (RFC 3533 §6 field 6: a stream's
    /// `page_sequence_number` increments by one per page, so a jump in the
    /// counter signals dropped pages). Each gap counts once regardless of
    /// how many consecutive pages went missing. The count only reflects
    /// pages actually consumed via `next_packet` (or absorbed during the
    /// header phase) — `build_seek_index`'s header-only scan does not run
    /// packet reassembly and therefore does not contribute.
    ///
    /// Zero for a clean file. A non-zero value tells a caller the byte
    /// stream was truncated or corrupted between pages; any packet that
    /// spanned a hole was dropped rather than spliced from mismatched
    /// halves, so the surviving packets stay individually well-formed.
    pub fn hole_count(&self) -> u64 {
        self.holes
    }

    /// Number of `continued`-flag framing inconsistencies the demuxer has
    /// detected so far (RFC 3533 §6 field 3, header_type bit 0x01). The bit
    /// declares whether a page's first segment continues a packet from the
    /// previous page ("set") or begins a fresh packet ("unset"). A page
    /// whose bit contradicts the demuxer's reassembly state — a continuation
    /// claimed with no partial packet to resume, or a fresh-packet page that
    /// silently abandons a partial the previous page promised to continue —
    /// is a framing error. The offending fragment is dropped rather than
    /// spliced, so every delivered packet stays individually well-formed.
    ///
    /// This is independent of [`hole_count`](Self::hole_count): a
    /// `page_sequence_number` gap that already cleared the pending buffer in
    /// the same page is attributed to the hole, not double-counted here. A
    /// non-zero value with a zero hole count signals corruption *within* an
    /// otherwise sequence-consistent page run (e.g. a damaged final segment
    /// that flipped a lacing terminator).
    ///
    /// Zero for a clean file. Like `hole_count`, the tally reflects only
    /// pages consumed via `next_packet` (or absorbed during the header
    /// phase); the header-only `build_seek_index` scan does not contribute.
    pub fn framing_error_count(&self) -> u64 {
        self.framing_errors
    }

    /// Number of times the demuxer had to *recapture* page sync after a
    /// parsing error. RFC 3533 §3 lists recapture as a core design
    /// requirement and §6 field 1 describes how the `OggS` magic enables it:
    /// "It helps a decoder to find the page boundaries and regain
    /// synchronisation after parsing a corrupted stream. Once the capture
    /// pattern is found, the decoder verifies page sync and integrity by
    /// computing and comparing the checksum."
    ///
    /// Each recovery — whether triggered by garbage spliced between pages
    /// (capture pattern missing at the expected offset) or by a checksum
    /// failure (the apparent page header was valid `OggS` but the body did
    /// not verify) — counts as one resync regardless of how many bytes had
    /// to be skipped. This is distinct from [`hole_count`](Self::hole_count):
    /// a sequence-number gap is a logical page-loss event, whereas a resync
    /// is the demuxer's response to *byte-level* corruption it had to walk
    /// past. The two can fire on the same file (the byte corruption
    /// destroyed N pages, so the next valid page also reports a hole), in
    /// which case both counters tick. The tally reflects pages consumed via
    /// `next_packet` (or absorbed during the header phase); the
    /// header-only `build_seek_index` scan does not contribute.
    pub fn resync_count(&self) -> u64 {
        self.resyncs
    }

    /// Number of `bitstream_serial_number` collisions detected — BOS pages
    /// whose serial was already live in the file when they arrived. RFC 3533
    /// §4 makes serial uniqueness a normative MUST: "Each grouped logical
    /// bitstream MUST have a unique serial number within the scope of the
    /// physical bitstream" and, identically, "Each chained logical bitstream
    /// MUST have a unique serial number within the scope of the physical
    /// bitstream." A conforming encoder never reuses a serial, so this is
    /// zero for every well-formed file.
    ///
    /// Two malformed shapes tick the counter:
    ///
    /// * a second grouped BOS in the same link declaring a serial an earlier
    ///   grouped BOS already claimed (a grouping violation), and
    /// * a chained link whose BOS reuses a serial a prior link already used
    ///   (a chaining violation).
    ///
    /// Rather than abort or silently splice the new bitstream's packets onto
    /// the colliding stream's stale reassembly buffer, the demuxer treats the
    /// duplicate BOS as a logical restart of that serial: it drops any partial
    /// packet the previous occupant left buffered, resets the page-sequence
    /// tracker so the restart's pages do not register as page-loss holes, and
    /// re-files the serial under the new BOS's link index. Every packet the
    /// demuxer then delivers for that serial still belongs to a single
    /// bitstream — the most recent one to claim the serial — so downstream
    /// decoders never receive a frankenpacket assembled from two streams.
    ///
    /// The tally reflects BOS pages seen via `next_packet`, the initial
    /// `open` BOS walk, or the `build_seek_index` header scan. A non-zero
    /// value lets a caller surface "this file reuses logical-stream serials,
    /// which the Ogg spec forbids" without losing the demux.
    pub fn duplicate_serial_count(&self) -> u64 {
        self.duplicate_serials
    }

    /// Number of distinct chained links the demuxer has observed so far
    /// (RFC 3533 §4). The initial BOS section is link 0, so a single-link
    /// (multiplexed or pure-mono) file always reports `1`. A back-to-back
    /// concatenation of two independent logical bitstreams reports `2`,
    /// and so on.
    ///
    /// The tally grows as the demuxer encounters BOS-after-non-BOS pages
    /// via `next_packet` or `build_seek_index`. Before any pages have been
    /// processed past `open` this only counts links registered during the
    /// initial BOS walk, which is always `1` for any file with at least
    /// one stream. Run `build_seek_index` (or drain the file with
    /// `next_packet`) for the file-wide total.
    ///
    /// Together with [`stream_link_index`](Self::stream_link_index) and
    /// [`stream_serial`](Self::stream_serial) this lets external tooling
    /// reconstruct the RFC 3533 §4 link partitioning of the file.
    pub fn link_count(&self) -> u32 {
        // `next_link_index` is the index that WOULD be assigned to the
        // next BOS-after-non-BOS — so the number of distinct links seen
        // so far is one greater than the highest assigned index, but
        // only if any stream has been registered. With zero registered
        // streams (the impossible-in-practice empty-file case), we report
        // 0. Otherwise the count is `next_link_index + 1`, since the
        // initial BOS section is link 0 and `next_link_index` is bumped
        // EACH time a new link starts AFTER the first one.
        if self.state_by_serial.is_empty() {
            0
        } else {
            self.next_link_index.saturating_add(1)
        }
    }

    /// Chained-link index assigned to the public stream at `stream_index`
    /// (RFC 3533 §4). Streams that share a link index play concurrently
    /// (the link multiplexes them); streams in different links play
    /// sequentially. The initial BOS section is link 0; each subsequent
    /// BOS-after-non-BOS increments the counter.
    ///
    /// Returns `None` for an out-of-range index. A non-chained
    /// (single-link) file reports `Some(0)` for every stream.
    pub fn stream_link_index(&self, stream_index: u32) -> Option<u32> {
        // O(n) over registered streams — n is the count of logical
        // bitstreams, which is small (typically 1..=4) even on multiplexed
        // files, so we don't bother building a reverse map.
        self.state_by_serial
            .values()
            .find(|s| self.streams[s.public_index].index == stream_index)
            .map(|s| s.link_index)
    }

    /// Ogg `bitstream_serial_number` (RFC 3533 §6 field 5) of the public
    /// stream at `stream_index`. The serial uniquely identifies a logical
    /// bitstream within the file — every page belonging to a given stream
    /// carries the same serial in its header.
    ///
    /// Returns `None` for an out-of-range index. Exposed so external
    /// tooling can correlate `oxideav-ogg`'s `StreamInfo::index` (which is
    /// a dense `0..N` enumeration assigned in BOS-discovery order) with
    /// the raw on-wire serials a page-level scanner would observe.
    pub fn stream_serial(&self, stream_index: u32) -> Option<u32> {
        self.state_by_serial
            .iter()
            .find(|(_, s)| self.streams[s.public_index].index == stream_index)
            .map(|(serial, _)| *serial)
    }

    /// Per-stream **granuleshift** — the number of low bits of a page's
    /// `granulepos` that carry the offset-since-keyframe for a sub-seekable
    /// (keyframe-bearing) mapping, as declared by the stream's Skeleton 4.0
    /// `fisbone\0` (`docs/container/ogg/ogg-skeleton-4.0.md`: "the number of
    /// lower bits from the granulepos field that are used to provide position
    /// information for sub-seekable units (like the keyframe shift in
    /// theora)").
    ///
    /// Returns `Some(0)` for an audio mapping (Vorbis / Opus / FLAC / Speex —
    /// every packet is a random-access point) and for any stream the demuxer
    /// found no `fisbone\0` for; a Theora stream with a fisbone reports its
    /// declared keyframe shift. `None` for an out-of-range `stream_index`.
    /// Exposed alongside [`opus_pre_skip`](Self::opus_pre_skip) so callers
    /// reasoning about per-packet [`oxideav_core::packet::PacketFlags::keyframe`]
    /// — which the demuxer derives from this shift — can unpack a page's raw
    /// granule into its `(keyframe_index, offset)` halves themselves.
    pub fn stream_granuleshift(&self, stream_index: u32) -> Option<u8> {
        let serial = self.stream_serial(stream_index)?;
        Some(
            self.skeleton
                .as_ref()
                .and_then(|sk| sk.bone_for_serial(serial))
                .map(|b| b.granuleshift)
                .unwrap_or(0),
        )
    }

    /// Number of tracks under the Skeleton "Track order" addressing
    /// scheme (`docs/container/ogg/ogg-skeleton-message-headers.wiki`
    /// §"Track order").
    ///
    /// The wiki defines a stable way to address tracks by an index:
    /// "the means to number through the tracks is by the order in which
    /// the bos pages of the tracks appear in the Ogg stream", with the
    /// worked example listing `track[0]: Skeleton BOS`, `track[1]:
    /// Theora BOS for main video`, `track[2]: Vorbis BOS for main
    /// audio`, and so on. The count therefore includes the Skeleton
    /// logical bitstream (when present) plus every content stream.
    ///
    /// Equals [`streams().len()`](oxideav_core::Demuxer::streams) for a
    /// Skeleton-free file, and one more than that when a `fishead\0`
    /// Skeleton BOS is present (the Skeleton occupies `track[0]` but is
    /// not a content stream, so it never appears in `streams()`).
    pub fn track_order_len(&self) -> u32 {
        let content = self.streams.len() as u32;
        if self.skeleton_serial.is_some() {
            content.saturating_add(1)
        } else {
            content
        }
    }

    /// Resolve a Skeleton "Track order" index to the logical bitstream's
    /// on-wire `bitstream_serial_number`
    /// (`docs/container/ogg/ogg-skeleton-message-headers.wiki`
    /// §"Track order").
    ///
    /// Per the wiki's worked example, `track[0]` is the Skeleton BOS
    /// (when the file carries one), then each content track follows in
    /// the order its BOS page appears. Because this crate assigns each
    /// content stream's dense [`StreamInfo::index`](oxideav_core::StreamInfo)
    /// in BOS-discovery order, a Skeleton-bearing file maps
    /// `track[0] -> Skeleton serial` and `track[n] -> content stream
    /// index n-1` for `n >= 1`; a Skeleton-free file maps
    /// `track[n] -> content stream index n` directly (the wiki only
    /// reserves `track[0]` for Skeleton when Skeleton is present).
    ///
    /// Returns `None` for an out-of-range index (`>= track_order_len`).
    ///
    /// The returned serial round-trips through
    /// [`Skeleton::bone_for_serial`](crate::skeleton::Skeleton::bone_for_serial)
    /// so a caller walking `0..track_order_len()` can recover each
    /// track's per-track fisbone metadata in the spec-defined order.
    pub fn track_order_serial(&self, track_index: u32) -> Option<u32> {
        if track_index >= self.track_order_len() {
            return None;
        }
        match self.skeleton_serial {
            Some(skel) => {
                if track_index == 0 {
                    Some(skel)
                } else {
                    self.stream_serial(track_index - 1)
                }
            }
            None => self.stream_serial(track_index),
        }
    }

    /// Reverse of [`track_order_serial`](Self::track_order_serial): map a
    /// logical bitstream's on-wire serial back to its Skeleton
    /// "Track order" index
    /// (`docs/container/ogg/ogg-skeleton-message-headers.wiki`
    /// §"Track order").
    ///
    /// Passing the Skeleton stream's own serial returns `Some(0)` when a
    /// `fishead\0` Skeleton BOS is present; passing a content stream's
    /// serial returns its `track[n]` index. Returns `None` for a serial
    /// the demuxer never observed as a BOS.
    pub fn track_order_index(&self, serial: u32) -> Option<u32> {
        if self.skeleton_serial == Some(serial) {
            return Some(0);
        }
        // Content stream: find its dense public index, then offset by one
        // when a Skeleton occupies track[0].
        let public_index = self
            .state_by_serial
            .iter()
            .find(|(s, _)| **s == serial)
            .map(|(_, st)| self.streams[st.public_index].index)?;
        if self.skeleton_serial.is_some() {
            Some(public_index + 1)
        } else {
            Some(public_index)
        }
    }

    /// Number of seek requests this demuxer satisfied directly from a
    /// Skeleton 4.0 `index\0` keyframe-index packet, bypassing both the
    /// per-page seek-index `index_floor` check and the bisection
    /// fallback. The Skeleton spec
    /// (`docs/container/ogg/ogg-skeleton-4.0.md`) carries an optional
    /// per-stream `index\0` packet whose keypoints are
    /// `(byte_offset, timestamp)` pairs in sorted order; when present,
    /// [`Demuxer::seek_to`](oxideav_core::Demuxer::seek_to) finds the
    /// floor keypoint for the target timestamp in O(log n) and jumps
    /// straight to its byte offset.
    ///
    /// Stays at 0 when no Skeleton index is available for the requested
    /// stream's serial (every other code path keeps working — only the
    /// fast-path counter holds). A non-zero value tells callers that a
    /// previous `seek_to` returned without paying for any page scanning.
    ///
    /// When the file carries indexes for multiple concurrent streams,
    /// the fast path applies the
    /// `docs/container/ogg/ogg-skeleton-4.0.md` §"Keyframe indexes for
    /// faster seeking" multi-stream rule: "first construct the set
    /// which contains every active streams' last keypoint which has
    /// time less than or equal to the seek target time. … select the
    /// key point with the smallest byte offset." Each such successful
    /// lookup is a single tick on this counter, regardless of how many
    /// streams' indexes participated in the minimisation.
    pub fn skeleton_index_seek_count(&self) -> u64 {
        self.skeleton_index_seeks
    }

    /// Number of times a [`Demuxer::seek_to`] call attempted the
    /// Skeleton 4.0 fast path but found the per-spec validity checks
    /// failed and fell back to the page-level
    /// [`index_floor`](OggDemuxer) / bisection path.
    ///
    /// Per `docs/container/ogg/ogg-skeleton-4.0.md` §"Keyframe indexes
    /// for faster seeking" the three rejection conditions are:
    ///
    /// 1. The `fishead` BOS packet's *Segment length in bytes* field
    ///    (bytes 64..72) disagrees with the actual file size, meaning
    ///    the indexed segment has been rewritten or chained against
    ///    since the index was built.
    /// 2. After seeking to a keypoint's stored offset, the bytes there
    ///    do not start with the `OggS` capture pattern — the keypoint
    ///    no longer lands on a page boundary.
    /// 3. After seeking to a keypoint's stored offset, the page there
    ///    is from a different `bitstream_serial_number` than the
    ///    keypoint's `index\0` packet declares — the stream layout has
    ///    shifted under the index.
    ///
    /// Each rejection counts once. The seek itself still completes via
    /// the slower bisection path, so the counter is purely diagnostic.
    /// A value of 0 across a long run means every Skeleton index seen
    /// has been internally consistent with its file.
    pub fn skeleton_index_invalid_count(&self) -> u64 {
        self.skeleton_index_rejects
    }

    /// Number of [`OggDemuxer::seek_to_with_preroll`] calls that actually
    /// backed the resume offset up by at least one page to satisfy a
    /// non-zero per-track preroll.
    ///
    /// `docs/container/ogg/ogg-skeleton-4.0.md` §"How to describe the
    /// logical bitstreams within an Ogg container?" defines the fisbone
    /// **preroll** field as "the number of past content packets to take
    /// into account when decoding the current Ogg page, which is necessary
    /// for seeking (vorbis has generally 2, speex 3)". The counter stays
    /// at 0 when no fisbone records a preroll for the requested stream,
    /// when the recorded preroll is 0, or when the landed page is already
    /// at or near the stream's first content page so there is no earlier
    /// page to back up to. Each call that does move the resume offset
    /// earlier counts once, regardless of how many pages it walked back.
    pub fn preroll_seek_count(&self) -> u64 {
        self.preroll_seeks
    }

    /// Opus **pre-skip** for the content stream at `stream_index`, in 48 kHz
    /// samples, or `None` for a non-Opus stream (or an unknown index).
    ///
    /// `docs/audio/opus/rfc7845-ogg-opus.txt` §5.1 field 4 defines pre-skip
    /// as "the number of samples (at 48 kHz) to discard from the decoder
    /// output when starting playback, and also the number to subtract from a
    /// page's granule position to calculate its PCM sample position". The
    /// demuxer reads it from the `OpusHead` ID header at open time and
    /// already folds it into its own duration estimate (RFC 7845 §4.3:
    /// `PCM sample position = granule position − pre-skip`); this accessor
    /// exposes the raw value so a downstream Opus decoder can discard the
    /// same leading samples it was told to, without re-parsing the header.
    pub fn opus_pre_skip(&self, stream_index: u32) -> Option<u16> {
        let serial = self.stream_serial(stream_index)?;
        self.opus_pre_skip.get(&serial).copied()
    }

    /// Current byte position of the underlying input.
    ///
    /// After a [`Demuxer::seek_to`](oxideav_core::Demuxer::seek_to) or
    /// [`OggDemuxer::seek_to_with_preroll`] call this is the page boundary
    /// the next [`Demuxer::next_packet`](oxideav_core::Demuxer::next_packet)
    /// read resumes from, so callers (and a preroll-aware caller comparing
    /// the two seek variants) can observe where the resume offset landed.
    pub fn input_position(&mut self) -> Result<u64> {
        Ok(self.input.stream_position()?)
    }

    /// Seek as [`Demuxer::seek_to`](oxideav_core::Demuxer::seek_to) does,
    /// then move the resume byte offset earlier so that a decoder reading
    /// forward from it sees at least `preroll` content packets of the
    /// requested stream **before** the page `seek_to` would have landed on.
    ///
    /// `docs/container/ogg/ogg-skeleton-4.0.md` §"How to describe the
    /// logical bitstreams within an Ogg container?" specifies a per-track
    /// **preroll**: "the number of past content packets to take into
    /// account when decoding the current Ogg page, which is necessary for
    /// seeking (vorbis has generally 2, speex 3)". A decoder that resumes
    /// exactly on the landed page is missing that warm-up context; codecs
    /// with inter-packet state (window overlap, prediction) therefore
    /// produce incorrect output for the first packets after a bare
    /// `seek_to`. This method consumes the preroll the stream's Skeleton
    /// fisbone declares (looked up by the stream's on-wire serial via
    /// [`Skeleton::bone_for_serial`](crate::skeleton::Skeleton::bone_for_serial))
    /// and rewinds to an earlier page boundary so those packets are
    /// available.
    ///
    /// The returned granule is identical to what [`Demuxer::seek_to`]
    /// would return — the *decode target* is unchanged; the earlier
    /// pages are warm-up the caller is expected to decode and discard
    /// (the spec's "take into account when decoding" packets) until it
    /// reaches the target granule.
    ///
    /// Behaves exactly like `seek_to` (same input position, same return
    /// value) when:
    ///   * the file has no Skeleton, or no fisbone for this stream's
    ///     serial;
    ///   * the fisbone's `preroll` is 0 (the audio mappings without
    ///     inter-packet warm-up, and the common encoder default); or
    ///   * the landed page is already the stream's first content page (or
    ///     within fewer than `preroll` content packets of it), so there
    ///     is no earlier page to back up to.
    pub fn seek_to_with_preroll(&mut self, stream_index: u32, pts: i64) -> Result<i64> {
        // Resolve the serial first so we can read the preroll before the
        // seek perturbs any state.
        let wanted_serial = self.serial_for_stream(stream_index).ok_or_else(|| {
            Error::unsupported(format!("Ogg: no logical stream for index {stream_index}"))
        })?;
        let preroll = self.preroll_for_serial(wanted_serial);
        let num_headers = self.num_headers_for_serial(wanted_serial);

        // Run the standard seek. This sets the input position to the
        // landed page boundary, flushes per-stream state, and returns the
        // landed granule.
        let landed_granule = self.seek_to(stream_index, pts)?;
        if preroll == 0 {
            return Ok(landed_granule);
        }
        // After `seek_to`, the input cursor sits at the landed page
        // boundary; that offset is the target the decoder must reach.
        let landed_off = self.input.stream_position()?;
        if landed_off == 0 {
            // Already at the very start of the file — no earlier page.
            return Ok(landed_granule);
        }

        // Collect every page of the requested stream that starts strictly
        // before the landed offset, in file order, alongside the number
        // of content packets each page terminates. The resume page is the
        // earliest page such that the content-packet count from it (up to,
        // but excluding, the landed page) is at least `preroll`.
        // `collect_stream_pages_until` walks the input forward and leaves
        // the cursor mid-file, so every no-back-up path below must restore
        // the cursor to `landed_off` (the page `seek_to` chose) before
        // returning — otherwise the next `next_packet` would resume from a
        // stale position.
        let pages = self.collect_stream_pages_until(wanted_serial, landed_off, num_headers)?;
        if pages.is_empty() {
            // No earlier content page of this stream — `seek_to`'s
            // landing stands.
            self.input.seek(SeekFrom::Start(landed_off))?;
            return Ok(landed_granule);
        }
        let mut acc: u64 = 0;
        let mut resume_off = pages.last().map(|&(off, _)| off).unwrap_or(landed_off);
        for &(off, terminated) in pages.iter().rev() {
            resume_off = off;
            acc = acc.saturating_add(terminated as u64);
            if acc >= preroll as u64 {
                break;
            }
        }

        if resume_off >= landed_off {
            // Nothing earlier to move to (shouldn't happen given the
            // strict-`<` collection, but keep the contract defensive).
            self.input.seek(SeekFrom::Start(landed_off))?;
            return Ok(landed_granule);
        }

        // Re-seek to the earlier resume page and re-flush demuxer state so
        // forward reads start cleanly from the preroll pages.
        self.input.seek(SeekFrom::Start(resume_off))?;
        self.page_queue.clear();
        self.out_queue.clear();
        for state in self.state_by_serial.values_mut() {
            state.pending.clear();
            state.granule_seen = 0;
        }
        self.eof_reached = false;
        self.preroll_seeks = self.preroll_seeks.saturating_add(1);
        Ok(landed_granule)
    }

    /// Keyframe-aware seek for a sub-seekable (keyframe-bearing) mapping.
    ///
    /// A bare [`Demuxer::seek_to`] lands on the page whose *frame number*
    /// floors the target — but a Theora-style codec cannot begin decoding at
    /// an arbitrary inter-frame: it must resume from the last keyframe at or
    /// before the target. The granule packing encodes exactly that keyframe:
    /// `docs/container/ogg/ogg-skeleton-4.0.md` defines the granuleshift as
    /// the count of low bits holding "position information for sub-seekable
    /// units (like the keyframe shift in theora)", so a page's granule splits
    /// into a keyframe index `g >> shift` (high bits) and an
    /// offset-since-keyframe `g & ((1 << shift) - 1)` (low bits). This method
    /// runs the normal `seek_to`, reads the landed page's keyframe index, and
    /// — when the landing isn't already on that keyframe — re-seeks to the
    /// keyframe's own frame so forward decoding starts on an intra page.
    ///
    /// The returned granule is the **keyframe page's** on-wire granule (its
    /// offset half is zero), and the input is positioned at that page, so the
    /// caller decodes forward from the keyframe and discards frames until it
    /// reaches the originally-requested `pts`. (This differs from
    /// [`seek_to_with_preroll`](Self::seek_to_with_preroll), whose return
    /// value is unchanged from `seek_to` because audio preroll is a fixed
    /// *count* of warm-up packets rather than a keyframe boundary.)
    ///
    /// Behaves exactly like `seek_to` (same input position, same return
    /// value) when the stream has granuleshift 0 — every audio mapping, where
    /// each packet is already an independent random-access point — or when the
    /// landed page is itself a keyframe. Returns the same `Error::Unsupported`
    /// as `seek_to` for a Theora stream lacking a usable Skeleton fisbone.
    pub fn seek_to_keyframe(&mut self, stream_index: u32, pts: i64) -> Result<i64> {
        let serial = self.serial_for_stream(stream_index).ok_or_else(|| {
            Error::unsupported(format!("Ogg: no logical stream for index {stream_index}"))
        })?;
        // Resolve the granuleshift + frame rate before the seek perturbs state.
        let bone = self
            .skeleton
            .as_ref()
            .and_then(|s| s.bone_for_serial(serial));
        let shift = bone.map(|b| b.granuleshift as u32).unwrap_or(0);
        let rate = bone.map(|b| (b.granule_rate.numerator, b.granule_rate.denominator));
        let stream_tb = self.streams[stream_index as usize].time_base;

        let landed = self.seek_to(stream_index, pts)?;

        // granuleshift 0 (audio, or no fisbone): every page is already a
        // random-access point. A `-1` landed granule (should not occur for a
        // successful seek) likewise passes through.
        if shift == 0 || shift >= 63 || landed < 0 {
            return Ok(landed);
        }
        let mask = (1i64 << shift) - 1;
        if landed & mask == 0 {
            // The landed page is itself the keyframe — nothing to back up to.
            return Ok(landed);
        }
        let keyframe_frame = landed >> shift;
        // Translate the keyframe's absolute frame number back into the
        // stream's time-base so we can re-seek to it. `frame_tb` is the
        // time-base in which one tick is one frame (1 frame = gr_den / gr_num
        // seconds); rescaling a frame count from it into the stream's
        // time-base yields the keyframe's pts. Without a usable rate we cannot
        // perform the translation, so fall back to the frame-floor landing.
        let Some((gr_num, gr_den)) = rate else {
            return Ok(landed);
        };
        if gr_num <= 0 || gr_den <= 0 {
            return Ok(landed);
        }
        let frame_tb = TimeBase::new(gr_den, gr_num);
        let keyframe_pts = frame_tb.rescale(keyframe_frame, stream_tb);
        // Re-seek to the keyframe. The frame-floor bisection lands on the
        // page whose frame number is `<= keyframe_frame`; since the keyframe
        // is an exact frame on the wire, that is the keyframe page itself.
        self.seek_to(stream_index, keyframe_pts)
    }

    /// Look up the requested stream's on-wire serial number.
    fn serial_for_stream(&self, stream_index: u32) -> Option<u32> {
        for (serial, state) in &self.state_by_serial {
            if self.streams[state.public_index].index == stream_index {
                return Some(*serial);
            }
        }
        None
    }

    /// Per-track preroll for `serial`, read from its Skeleton fisbone.
    /// Returns 0 when there is no Skeleton, no fisbone for the serial, or
    /// the fisbone records preroll 0 — every "behave like `seek_to`" path.
    fn preroll_for_serial(&self, serial: u32) -> u32 {
        self.skeleton
            .as_ref()
            .and_then(|s| s.bone_for_serial(serial))
            .map(|b| b.preroll)
            .unwrap_or(0)
    }

    /// Per-track header-packet count for `serial`, read from its Skeleton
    /// fisbone (`Number of header packets`, bytes 16..20 of `fisbone\0`).
    /// Returns 0 when there is no Skeleton or no fisbone for the serial.
    /// Used to exclude the codec's identification / comment / setup
    /// header packets from the preroll content-packet count — the spec's
    /// preroll counts "past *content* packets", not headers.
    fn num_headers_for_serial(&self, serial: u32) -> u32 {
        self.skeleton
            .as_ref()
            .and_then(|s| s.bone_for_serial(serial))
            .map(|b| b.num_headers)
            .unwrap_or(0)
    }

    /// Convert a logical stream's raw on-wire `granulepos` into a duration
    /// in microseconds.
    ///
    /// For an audio mapping (Vorbis / Opus / FLAC / Speex) the granulepos
    /// *is* the granule value — a monotone sample count — and the stream's
    /// time-base is the granule rate (`1/sample_rate`), so the raw value
    /// converts directly through `time_base.seconds_of`.
    ///
    /// Theora is the exception. Its granulepos is **not** a plain frame
    /// count: it is `(keyframe_idx << shift) | frame_offset` per
    /// `docs/container/ogg/ogg-skeleton-4.0.md` §"What decoding-related
    /// information is needed?" (granuleshift = "the number of lower bits
    /// from the granulepos field that are used to provide position
    /// information for sub-seekable units (like the keyframe shift in
    /// theora)"), and the demuxer stamps Theora streams with the
    /// `1/1_000_000` placeholder time-base because Ogg framing alone never
    /// reveals the frame rate. Both facts make the raw `time_base.seconds_of`
    /// path wrong for Theora — it neither unpacks the keyframe shift nor
    /// divides by the real frame rate.
    ///
    /// The Skeleton `fisbone\0` carries both missing pieces: its
    /// `granuleshift` and `granule_rate`. When a fisbone is present for the
    /// serial, [`FisBone::granule_to_seconds`] performs the full two-step
    /// mapping the §"What decoding-related information is needed?" section
    /// defines — `extract_granules` (undo the shift) then `granules /
    /// granulerate` — so the duration is the page's real playback time
    /// regardless of codec. A `granuleshift == 0` fisbone (every audio
    /// mapping, plus a Theora encoder that left the shift unset) collapses
    /// the extraction to a pass-through, so the fisbone path also returns
    /// the right answer for audio when one happens to be present.
    ///
    /// Streams with no Skeleton, no fisbone for the serial, or a fisbone
    /// whose `granule_rate` is unusable (`None` from `granule_to_seconds`)
    /// fall back to the stream's own time-base — the prior behaviour,
    /// correct for the audio mappings whose time-base is already the
    /// granule rate.
    /// Anchor each stream's `start_time` onto the playback timeline the
    /// Skeleton fishead defines.
    ///
    /// `docs/container/ogg/ogg-skeleton-4.0.md` §"What decoding-related
    /// information is needed?" defines the fishead **basetime** as "a
    /// mapping for granule position 0 (for all logical bitstreams) to a
    /// playback time" — the motivating example being analog video that
    /// "actually starts at a time of 1 hour" and wants to "retain this
    /// mapping on digitizing their content". For a remuxed substream the
    /// per-track **basegranule** further shifts where that track's own data
    /// begins; [`Skeleton::stream_start_seconds`] combines both
    /// (`basetime + basegranule / granulerate`).
    ///
    /// Without this, every stream reports `start_time = 0` regardless of
    /// the fishead anchor, so a player has no way to place the content on
    /// the intended timeline. We translate the seconds value into the
    /// stream's own `time_base` ticks (the unit `start_time` is expressed
    /// in) and store it. Streams with no Skeleton, no fisbone, an unusable
    /// granule rate, or a zero/absent basetime+basegranule keep their
    /// `start_time = 0` default — the un-cut, un-anchored common case.
    ///
    /// Only `start_time` is touched; the duration accumulator stays
    /// basetime-free (see [`Self::granule_to_micros_for_serial`]) so
    /// `duration == end - start` holds.
    fn anchor_start_times_from_skeleton(&mut self) {
        let Some(sk) = self.skeleton.as_ref() else {
            return;
        };
        // Snapshot (public_index, serial) pairs first so we can borrow
        // `self.streams` mutably below without aliasing `self.skeleton`.
        let anchors: Vec<(usize, f64)> = self
            .state_by_serial
            .iter()
            .filter_map(|(serial, state)| {
                sk.stream_start_seconds(*serial)
                    .filter(|s| *s != 0.0)
                    .map(|secs| (state.public_index, secs))
            })
            .collect();
        for (public_index, secs) in anchors {
            let Some(stream) = self.streams.get_mut(public_index) else {
                continue;
            };
            // Convert the seconds anchor into the stream's own time_base
            // ticks (the unit `start_time` is expressed in). `seconds_of(1)`
            // is the duration of one tick; dividing recovers the tick count.
            let tick = stream.time_base.seconds_of(1);
            if tick > 0.0 {
                stream.start_time = Some((secs / tick).round() as i64);
            }
        }
    }

    fn granule_to_micros_for_serial(&self, serial: u32, granule: i64) -> i64 {
        let Some(state) = self.state_by_serial.get(&serial) else {
            return 0;
        };
        // Opus pre-skip: the on-wire granule counts 48 kHz samples *including*
        // the encoder-delay padding, so the playback-relevant sample count is
        // `granule − pre-skip` (RFC 7845 §4.3). Subtract it before any
        // time conversion. The `-1` "no packets finish on this page" sentinel
        // is left untouched (it is not a sample count); a granule below the
        // pre-skip clamps to 0 — for an EOS page RFC 7845 §4.5 makes that a
        // legal "stream shorter than pre-skip" edge we report as zero-length
        // rather than negative.
        let granule = match self.opus_pre_skip.get(&serial) {
            Some(&ps) if granule >= 0 => (granule - ps as i64).max(0),
            _ => granule,
        };
        // This is the **track-relative** granule→time used by the duration
        // accumulator: it measures elapsed playback from the track's granule
        // 0, NOT the file-absolute playback time. The fishead basetime — the
        // spec's "mapping for granule position 0 (for all logical
        // bitstreams) to a playback time" (`docs/container/ogg/
        // ogg-skeleton-4.0.md`) — is a *timeline anchor*, not part of a
        // duration: a stream that runs granule 0..N is N/rate seconds long
        // regardless of where granule 0 sits on the playback clock. The
        // basetime is therefore applied to each stream's `start_time`
        // (see `register_stream`), and the duration here stays basetime-free
        // so duration == end - start.
        if let Some(secs) = self
            .skeleton
            .as_ref()
            .and_then(|s| s.bone_for_serial(serial))
            .and_then(|b| b.granule_to_seconds(granule))
        {
            return (secs * 1_000_000.0) as i64;
        }
        let stream = &self.streams[state.public_index];
        (stream.time_base.seconds_of(granule) * 1_000_000.0) as i64
    }

    /// Walk pages of `serial` from the start of the file up to (but not
    /// including) `until_off`, returning each *content* page's `(offset,
    /// terminated_packet_count)` in file order.
    ///
    /// `terminated_packet_count` is the number of packets that *end* on
    /// the page (a trailing 255-lacing partial that continues into the
    /// next page is not counted as terminated here) — the unit the spec's
    /// preroll field is measured in ("past content packets"). The first
    /// `skip_headers` terminated packets of the stream are the codec's
    /// header packets (id / comment / setup); pages are not collected
    /// until that many packets have terminated, and a page straddling the
    /// header→content boundary contributes only its content packets.
    /// Pages whose serial differs, or whose offset is `>= until_off`, are
    /// skipped. The scan reads only page headers + segment tables, never
    /// page bodies.
    fn collect_stream_pages_until(
        &mut self,
        serial: u32,
        until_off: u64,
        skip_headers: u32,
    ) -> Result<Vec<(u64, u32)>> {
        let mut out: Vec<(u64, u32)> = Vec::new();
        // Number of this stream's terminated packets consumed so far, used
        // to step past the `skip_headers` header packets.
        let mut packets_seen: u32 = 0;
        let mut cursor: u64 = 0;
        while cursor < until_off {
            self.input.seek(SeekFrom::Start(cursor))?;
            let mut hdr = [0u8; 27];
            let mut filled = 0usize;
            while filled < 27 {
                match self.input.read(&mut hdr[filled..]) {
                    Ok(0) => break,
                    Ok(n) => filled += n,
                    Err(e) if e.kind() == std::io::ErrorKind::Interrupted => continue,
                    Err(e) => return Err(e.into()),
                }
            }
            if filled < 27 || hdr[0..4] != page::CAPTURE_PATTERN {
                break;
            }
            let page_serial = u32::from_le_bytes([hdr[14], hdr[15], hdr[16], hdr[17]]);
            let n_segs = hdr[26] as usize;
            let mut lacing = vec![0u8; n_segs];
            let mut lfilled = 0usize;
            while lfilled < n_segs {
                match self.input.read(&mut lacing[lfilled..]) {
                    Ok(0) => break,
                    Ok(n) => lfilled += n,
                    Err(e) if e.kind() == std::io::ErrorKind::Interrupted => continue,
                    Err(e) => return Err(e.into()),
                }
            }
            if lfilled < n_segs {
                break;
            }
            let data_len: u64 = lacing.iter().map(|&v| v as u64).sum();
            // A packet terminates on this page for every lacing segment
            // < 255 (the same rule `Page::packet_segments` uses).
            let terminated = lacing.iter().filter(|&&v| v < 255).count() as u32;
            if page_serial == serial {
                // Skip the codec's header packets so only *content*
                // packets feed the preroll count.
                let before = packets_seen;
                packets_seen = packets_seen.saturating_add(terminated);
                if packets_seen > skip_headers {
                    // Number of content packets that ended on this page:
                    // everything past the header threshold.
                    let content_here = packets_seen - before.max(skip_headers);
                    out.push((cursor, content_here));
                }
            }
            let next_off = cursor + 27 + n_segs as u64 + data_len;
            if next_off <= cursor {
                break;
            }
            cursor = next_off;
        }
        Ok(out)
    }

    /// Lazily validate the Skeleton 4.0 BOS `Segment length in bytes`
    /// field against the actual file length, caching the result.
    ///
    /// Returns `true` if the index is allowed to fire at the file level:
    ///   * no Skeleton state is recorded, or
    ///   * the recorded `fishead` is a 3.0 header (no segment_length
    ///     field at all — fall back to per-seek keypoint validation), or
    ///   * a 4.0 header has `segment_length = 0` (encoder opted out of
    ///     this check), or
    ///   * the recorded segment_length equals the file size.
    ///
    /// Returns `false` only when a 4.0 fishead recorded a non-zero
    /// segment_length and that length doesn't match the file. Per the
    /// 4.0 spec ("if it doesn't match the length stored in the Skeleton
    /// header packet, you know that either the index is out of date,
    /// or the file has been chained since indexing") that is a hard
    /// disqualification of the entire Skeleton index — every index in
    /// every fisbone is treated as untrusted, and seeks fall through to
    /// bisection.
    fn skeleton_segment_length_check(&mut self, file_size: u64) -> bool {
        if let Some(cached) = self.skeleton_segment_length_ok {
            return cached;
        }
        let declared = match self.skeleton.as_ref().and_then(|s| s.head.as_ref()) {
            // No Skeleton, no head, or 3.0 head (no segment_length field):
            // there's nothing to disprove at the file level.
            None => {
                self.skeleton_segment_length_ok = Some(true);
                return true;
            }
            Some(h) => match h.segment_length {
                // 3.0 header (no field), or a 4.0 encoder that opted out
                // by writing 0 — nothing to check.
                None | Some(0) => {
                    self.skeleton_segment_length_ok = Some(true);
                    return true;
                }
                Some(d) => d,
            },
        };
        // A declared length longer than the whole file is impossible: the
        // indexed segment cannot extend past EOF. Hard-disqualify.
        if declared > file_size {
            self.skeleton_segment_length_ok = Some(false);
            return false;
        }
        // Exact match: the indexed segment is the whole (single-link) file.
        // This is the unchained common case.
        if declared == file_size {
            self.skeleton_segment_length_ok = Some(true);
            return true;
        }
        // `declared < file_size`. Per `docs/container/ogg/ogg-skeleton-4.0.md`
        // §"When using the index to seek …": the index is invalid if "The
        // segment doesn't end at the segment length offset stored in the
        // Skeleton BOS packet (note that a new \"link\" in a \"chain\" can
        // start at the end of the segment)". So a shorter-than-file declared
        // length is NOT automatically a mismatch — it is valid precisely
        // when the indexed segment ends at `declared` and a *new link* (a
        // fresh `OggS` BOS page) begins exactly there. We verify that an
        // Ogg page starts at byte `declared`; if so the chain boundary is
        // where the spec says it should be and the index stays trusted.
        // Anything else (no page boundary at `declared`) means the segment
        // has been modified since indexing — fall through to bisection.
        let ok = self.page_begins_at(declared);
        self.skeleton_segment_length_ok = Some(ok);
        ok
    }

    /// Return `true` if an Ogg page (the `OggS` capture pattern) begins at
    /// exactly `offset`. Only the 4-byte capture pattern is read; the page
    /// header, segment table and body are not consulted. Returns `false` on
    /// any I/O error or short read so a transient failure degrades to a
    /// bisection fall-back rather than surfacing as a seek error. Used by
    /// the Skeleton 4.0 segment-length chained-file check
    /// (`docs/container/ogg/ogg-skeleton-4.0.md`: "a new \"link\" in a
    /// \"chain\" can start at the end of the segment").
    fn page_begins_at(&mut self, offset: u64) -> bool {
        if self.input.seek(SeekFrom::Start(offset)).is_err() {
            return false;
        }
        let mut magic = [0u8; 4];
        let mut filled = 0usize;
        while filled < magic.len() {
            match self.input.read(&mut magic[filled..]) {
                Ok(0) => return false,
                Ok(n) => filled += n,
                Err(e) if e.kind() == std::io::ErrorKind::Interrupted => continue,
                Err(_) => return false,
            }
        }
        magic == page::CAPTURE_PATTERN
    }

    /// Verify that the bytes at `offset` start an Ogg page whose
    /// `bitstream_serial_number` equals `expected_serial`. Used to
    /// implement the Skeleton 4.0 per-seek validity check ("after a
    /// seek to a keypoint's offset, you don't land exactly on a page
    /// boundary" / "you don't land on a page which belongs to that
    /// keypoint's stream"; `docs/container/ogg/ogg-skeleton-4.0.md`).
    ///
    /// Only the 27-byte page header is read; the segment table and
    /// body are not consulted. Returns `false` on any I/O error so the
    /// caller falls back to bisection without surfacing transient
    /// failures as seek errors.
    fn verify_keypoint_landing(&mut self, offset: u64, expected_serial: u32) -> bool {
        if self.input.seek(SeekFrom::Start(offset)).is_err() {
            return false;
        }
        let mut hdr = [0u8; 27];
        let mut filled = 0usize;
        while filled < hdr.len() {
            match self.input.read(&mut hdr[filled..]) {
                Ok(0) => return false,
                Ok(n) => filled += n,
                Err(e) if e.kind() == std::io::ErrorKind::Interrupted => continue,
                Err(_) => return false,
            }
        }
        if hdr[0..4] != page::CAPTURE_PATTERN {
            return false;
        }
        let serial = u32::from_le_bytes([hdr[14], hdr[15], hdr[16], hdr[17]]);
        serial == expected_serial
    }

    /// Look up the rightmost keypoint with timestamp `<= target_index_ts`
    /// in a single [`SkelIndex`], returning `(byte_offset, kp_timestamp)`
    /// when one exists. Used as the inner step of both the single-stream
    /// floor lookup and the multi-stream minimum-offset minimisation
    /// (`docs/container/ogg/ogg-skeleton-4.0.md` §"Keyframe indexes for
    /// faster seeking" — "first construct the set which contains every
    /// active streams' last keypoint which has time less than or equal
    /// to the seek target time").
    fn keypoint_floor(index: &SkelIndex, target_index_ts: i64) -> Option<(u64, i64)> {
        if index.timestamp_denominator <= 0 || index.keypoints.is_empty() {
            return None;
        }
        let kps = &index.keypoints;
        let i = match kps.binary_search_by(|kp| kp.timestamp.cmp(&target_index_ts)) {
            Ok(i) => i,
            Err(0) => return None,
            Err(i) => i - 1,
        };
        let kp = kps[i];
        Some((kp.offset, kp.timestamp))
    }

    /// Resolve `(byte_offset, returned_granule)` from the Skeleton 4.0
    /// keyframe-index packets when seeking on `serial`. `target_pts` is
    /// the seek target in the stream's own time-base units (same as the
    /// public [`Demuxer::seek_to`](oxideav_core::Demuxer::seek_to)
    /// contract). `time_base` is the requested stream's time base, used
    /// to convert `target_pts` into each index's `timestamp_denominator`
    /// units.
    ///
    /// Per `docs/container/ogg/ogg-skeleton-4.0.md` §"Keyframe indexes
    /// for faster seeking": "first construct the set which contains
    /// every active streams' last keypoint which has time less than or
    /// equal to the seek target time. This tells you a known point on
    /// every stream which lies before the seek target. Then from that
    /// set of key points, select the key point with the smallest byte
    /// offset." A seek that only consulted the requested stream's index
    /// would land past one or more other concurrent streams' required
    /// keyframes; selecting the minimum offset across every active
    /// stream's index guarantees decoding can resume cleanly for every
    /// multiplexed stream after the seek.
    ///
    /// Returns `None` if any of:
    ///   * no Skeleton state is recorded;
    ///   * no `index\0` packet was emitted for `serial` (this is the
    ///     anchor — without an index for the requested stream we have
    ///     no way to map back into its granule space);
    ///   * the requested serial's index is empty or has non-positive
    ///     timestamp_denominator;
    ///   * every keypoint in the requested serial's index sits past
    ///     `target_pts`.
    ///
    /// On `Some`, the returned `byte_offset` is the minimum across
    /// every active stream's index that resolved a floor keypoint at
    /// the seek target time; `winning_serial` identifies which stream
    /// owns that keypoint (used by the per-keypoint validity check at
    /// the call site to verify the byte at `byte_offset` is an `OggS`
    /// page belonging to that serial); `granule` is in the requested
    /// stream's time-base units (the result of seeking to the requested
    /// stream's own floor keypoint, even if a different stream's index
    /// won the minimisation).
    /// Build a [`SeekKey::TheoraFrame`] strategy for the Theora stream
    /// whose serial is `serial`, given the user's `pts` in the stream's
    /// `time_base` units. Returns `None` when the prerequisites for
    /// Theora granule translation are missing — no Skeleton was parsed,
    /// no `fisbone\0` was emitted for this serial, the fisbone's
    /// `granule_rate` numerator/denominator is non-positive, or the
    /// fisbone's `granuleshift` is zero (which collapses the Theora
    /// granule packing to a raw frame count; without a non-zero shift
    /// we have no way to tell whether the encoder was running with a
    /// shift of zero or simply forgot to set it, so the conservative
    /// choice is `None` and the caller returns `Unsupported`).
    ///
    /// On `Some`, the returned key's `target_key` is the absolute
    /// Theora frame number at or before `pts`: the user's `pts` is
    /// rescaled from `time_base` (microseconds, in practice) into
    /// frame-rate units `(gr_den, gr_num)` via [`TimeBase::rescale`].
    /// The bisection then compares each page's
    /// `(g >> shift) + (g & mask)` against this target frame number.
    fn theora_seek_key(&self, serial: u32, pts: i64, time_base: TimeBase) -> Option<SeekKey> {
        let bone = self.skeleton.as_ref()?.bone_for_serial(serial)?;
        let shift = bone.granuleshift as u32;
        if shift == 0 {
            return None;
        }
        let gr_num = bone.granule_rate.numerator;
        let gr_den = bone.granule_rate.denominator;
        if gr_num <= 0 || gr_den <= 0 {
            return None;
        }
        // Rescale `pts` (in `time_base` units) into "frames" by
        // converting to a time base of `1 frame = gr_den / gr_num
        // seconds`. `TimeBase::new(gr_den, gr_num)` rounds half-away-
        // from-zero, which can shift `target_frame` by one tick at the
        // boundary; that's within the seek_to contract ("greatest page
        // whose granule is at or below the target" lands on the nearest
        // page either way).
        let target_frame = time_base.rescale(pts, TimeBase::new(gr_den, gr_num));
        Some(SeekKey::theora_frame(target_frame, shift))
    }

    fn skeleton_index_seek(
        &self,
        serial: u32,
        target_pts: i64,
        time_base: TimeBase,
    ) -> Option<SkeletonIndexSeek> {
        let sk = self.skeleton.as_ref()?;
        let primary = sk.index_for_serial(serial)?;

        // Convert target_pts into the requested stream's index unit so
        // we can look up its floor keypoint (which fixes the returned
        // granule).
        let primary_tb = TimeBase::new(1, primary.timestamp_denominator);
        let primary_target_ts = time_base.rescale(target_pts, primary_tb);
        let (primary_off, primary_kp_ts) = Self::keypoint_floor(primary, primary_target_ts)?;

        // Translate the keypoint's index-unit timestamp back into a
        // stream-granule value so the public seek_to contract
        // ("returns the actual granule landed on") still holds.
        let returned_granule = primary_tb.rescale(primary_kp_ts, time_base);

        // Multi-stream minimisation: every other index in the Skeleton
        // contributes a candidate offset. Iterate, rescale into that
        // stream's own timestamp_denominator, find its floor keypoint,
        // and track the minimum byte offset across all candidates.
        let mut min_off = primary_off;
        let mut winning_serial = serial;
        for other in &sk.indexes {
            if other.serial == serial {
                continue;
            }
            // Reject pathological denominators defensively — a 0 or
            // negative denominator is undefined for rescale and would
            // be rejected by the inner floor lookup too.
            if other.timestamp_denominator <= 0 {
                continue;
            }
            let other_tb = TimeBase::new(1, other.timestamp_denominator);
            let other_target_ts = time_base.rescale(target_pts, other_tb);
            if let Some((cand_off, _)) = Self::keypoint_floor(other, other_target_ts) {
                if cand_off < min_off {
                    min_off = cand_off;
                    winning_serial = other.serial;
                }
            }
            // A stream whose index has no keypoint at or before the
            // target time is silently skipped — the spec says "every
            // ACTIVE stream's last keypoint" and we can't prove the
            // stream is inactive at this offset without re-scanning,
            // so falling back to the primary anchor when no floor
            // exists is the safe choice.
        }

        Some(SkeletonIndexSeek {
            byte_offset: min_off,
            winning_serial,
            returned_granule,
        })
    }

    /// Handle a BOS page whose `bitstream_serial_number` is already live in
    /// `state_by_serial` — a violation of the RFC 3533 §4 unique-serial MUST.
    /// Returns `true` if this was a collision (so the caller skips the normal
    /// fresh-registration path), `false` if `serial` is genuinely new.
    ///
    /// On a collision the existing logical stream is *restarted in place*: its
    /// pending partial-packet buffer is dropped and its page-sequence tracker
    /// reset (so the duplicate BOS's pages are not mis-read as the old
    /// occupant's continuation or as a page-loss hole), and the stream is
    /// re-filed under `new_link_index` so link diagnostics follow the serial
    /// to the link that now owns it. The public `StreamInfo` entry and codec
    /// parameters are left untouched: the colliding bitstream is, by the spec
    /// violation, indistinguishable at the container layer from the original,
    /// so we keep the single stream slot and just refuse to splice the two
    /// bitstreams' bytes together. The Skeleton bitstream's serial never
    /// collides through this path (it is recorded separately), so a `fishead`
    /// re-declaration is not treated here.
    fn restart_serial_on_duplicate_bos(&mut self, bos_page: &Page, new_link_index: u32) -> bool {
        let serial = bos_page.serial;
        if Some(serial) == self.skeleton_serial {
            return false;
        }
        if !self.state_by_serial.contains_key(&serial) {
            return false;
        }
        // Re-arm header capture so the restarted bitstream's identification /
        // comment / setup packets are re-read as headers (not emitted as
        // content frames), exactly as a fresh registration would. The codec
        // is taken from the duplicate BOS's own first packet — a chained link
        // may legally reuse a serial for a *different* codec, so we must not
        // assume the original stream's header count still applies.
        let header_count = bos_page
            .packet_segments()
            .first()
            .map(|seg| {
                let first = &bos_page.data[seg.data.clone()];
                codec_id::header_packet_count_from_first(&codec_id::detect(first), first)
            })
            .unwrap_or(0);
        // The `build_seek_index` header scan, when it has run, is the
        // authoritative file-wide source for the duplicate-serial tally
        // (it visits every page exactly once). Only the incremental
        // `next_packet` / `open`-walk path counts here, so the two walkers
        // never double-count the same collision. The state reset below
        // always runs — it is required for correct reassembly regardless of
        // which walker observed the duplicate.
        if !self.seek_index_built {
            self.duplicate_serials += 1;
        }
        let state = self
            .state_by_serial
            .get_mut(&serial)
            .expect("serial presence checked above");
        state.pending.clear();
        state.pending_valid = false;
        state.last_seq = None;
        state.link_index = new_link_index;
        state.headers_remaining = header_count;
        state.header_packets.clear();
        // The caller re-arms `bos_processed` after this returns; reset it here
        // so the restarted occupant's BOS is treated as freshly seen.
        state.bos_processed = false;
        true
    }

    /// Read pages until we leave the Beginning-Of-Stream section, registering
    /// every logical bitstream we discover. The pages we read are queued so
    /// `next_packet` can drain them in order.
    fn read_bos_section(&mut self) -> Result<()> {
        loop {
            let page = match self.read_page()? {
                Some(p) => p,
                None => {
                    self.eof_reached = true;
                    break;
                }
            };
            let is_bos = page.is_first();
            if is_bos && !self.state_by_serial.contains_key(&page.serial) {
                // Register each NEW serial's stream. A BOS reusing a serial
                // already registered in this same initial bos section is an
                // RFC 3533 §4 grouping violation; we do NOT register a second
                // `StreamInfo` for it here. The page is still queued, and
                // `process_page` (the single drain authority) recognises the
                // duplicate when it sees this serial's BOS for the second
                // time and counts / restarts it there.
                self.register_stream(&page)?;
            }
            self.page_queue.push_back(page);
            if !is_bos {
                // The first non-BOS page marks the end of the BOS section.
                break;
            }
        }
        if self.streams.is_empty() {
            return Err(Error::invalid("Ogg file contains no logical streams"));
        }
        Ok(())
    }

    fn register_stream(&mut self, bos_page: &Page) -> Result<()> {
        // The BOS page's first packet is the identification packet for the
        // codec. Identification packets must fit in a single BOS page (RFC
        // 5334 / codec mapping conventions).
        let segs = bos_page.packet_segments();
        if segs.is_empty() {
            return Err(Error::invalid("Ogg BOS page has no packets"));
        }
        let first = &bos_page.data[segs[0].data.clone()];
        // Skeleton BOS — `fishead\0` ident packet, Skeleton 3.0 / 4.0
        // (`docs/container/ogg/ogg-skeleton-{3,4}.0.md`). The Skeleton
        // logical bitstream has no content packets and is described as
        // ALWAYS the first BOS in the file; do not register it as a
        // public stream. Instead, parse the fishead and stash the serial
        // so subsequent fisbone / index packets can be routed away from
        // the regular packet-reassembly path.
        if skeleton::is_fishead(first) {
            // Re-encountering an already-recorded Skeleton BOS — for
            // example because `build_seek_index` re-walks every page
            // header in the file after `open` already drained the
            // header section — must not clobber the populated
            // `Skeleton` state with a fresh empty one (the previously
            // pushed `fisbone` / `index` packets would be lost,
            // turning the codec-aware seek path into a fall-through
            // `Unsupported` for any subsequent Theora seek). Idempotent
            // re-registration: the second time we see the Skeleton BOS
            // we already have `skeleton_serial == Some(this serial)`
            // and `skeleton.is_some()`, so just refresh
            // `skeleton_last_seq` and return.
            if self.skeleton_serial == Some(bos_page.serial) && self.skeleton.is_some() {
                self.skeleton_last_seq = Some(bos_page.seq_no);
                return Ok(());
            }
            let head = FisHead::parse(first)?;
            let mut sk = Skeleton::new();
            sk.serial = Some(bos_page.serial);
            sk.set_head(head);
            self.skeleton = Some(sk);
            self.skeleton_serial = Some(bos_page.serial);
            self.skeleton_last_seq = Some(bos_page.seq_no);
            return Ok(());
        }
        let codec_id = codec_id::detect(first);
        let public_index = self.streams.len();
        let mut params = guess_params(&codec_id, first)?;
        params.extradata = first.to_vec();

        // Opus carries its pre-skip in the OpusHead ID header (bytes 10..12,
        // LE u16, RFC 7845 §5.1 field 4). Record it per-serial so the
        // granule→time mapping can subtract it (RFC 7845 §4.3:
        // `PCM sample position = granule position − pre-skip`).
        if codec_id.as_str() == "opus" {
            if let Some(ps) = opus_pre_skip(first) {
                self.opus_pre_skip.insert(bos_page.serial, ps);
            }
        }

        let time_base = match codec_id.as_str() {
            // Vorbis / FLAC / Speex all carry a sample-count granule
            // (Vorbis I §4.3; FLAC RFC 9639 §10.1 "the number of the last
            // sample"; Speex manual §7.3 "the granulepos is the number of
            // the last sample encoded in that packet"), so the native
            // granule unit is `1/sample_rate` once the ID header reveals the
            // rate. Without a rate we cannot translate the granule to a time
            // and fall back to the 1 µs placeholder.
            "vorbis" | "flac" | "speex" => {
                if let Some(sr) = params.sample_rate {
                    TimeBase::new(1, sr as i64)
                } else {
                    TimeBase::new(1, 1_000_000)
                }
            }
            // Opus uses a 48 kHz timebase regardless of input sample rate.
            "opus" => TimeBase::new(1, 48_000),
            _ => TimeBase::new(1, 1_000_000),
        };

        self.streams.push(StreamInfo {
            index: public_index as u32,
            time_base,
            duration: None,
            start_time: Some(0),
            params,
        });
        self.state_by_serial.insert(
            bos_page.serial,
            LogicalStream {
                public_index,
                pending: Vec::new(),
                headers_remaining: codec_id::header_packet_count_from_first(&codec_id, first),
                header_packets: Vec::new(),
                granule_seen: 0,
                link_index: self.next_link_index,
                last_seq: None,
                pending_valid: false,
                bos_processed: false,
            },
        );
        Ok(())
    }

    fn read_page(&mut self) -> Result<Option<Page>> {
        // Read a page header (27 bytes), then enough to read the segment table
        // and data. We detect EOF by getting 0 bytes back from the very first
        // read; partial-page data is treated as truncation.
        let page_off = self.input.stream_position().unwrap_or(0);
        let mut hdr = [0u8; 27];
        if !read_exact_or_eof(&mut self.input, &mut hdr)? {
            return Ok(None);
        }
        if hdr[0..4] != page::CAPTURE_PATTERN {
            // RFC 3533 §3 "recapture after a parsing error" / §6 field 1: the
            // bytes here are not an `OggS` capture pattern. Rather than abort
            // the whole stream, scan forward for the next valid page and
            // resume there. Rewind to where this read began so the resync
            // scanner re-examines every byte (the bad capture may overlap a
            // genuine `OggS` that starts inside these 27 bytes).
            self.input.seek(SeekFrom::Start(page_off))?;
            return self.resync_to_next_page();
        }
        let n_segs = hdr[26] as usize;
        let mut lacing = vec![0u8; n_segs];
        self.input.read_exact(&mut lacing)?;
        let data_len: usize = lacing.iter().map(|&v| v as usize).sum();
        let mut data = vec![0u8; data_len];
        self.input.read_exact(&mut data)?;

        // Re-parse from the assembled bytes so CRC validation logic is shared.
        let mut full = Vec::with_capacity(27 + n_segs + data_len);
        full.extend_from_slice(&hdr);
        full.extend_from_slice(&lacing);
        full.extend_from_slice(&data);
        match Page::parse(&full) {
            Ok((page, consumed)) => {
                debug_assert_eq!(consumed, full.len());
                // Opportunistically populate the seek index from any page we
                // read during normal demux flow — costs O(log n) per page and
                // means a subsequent seek can skip bisection if the target
                // falls inside the already-scanned range.
                self.index_record(page.serial, page.granule_position, page_off);
                Ok(Some(page))
            }
            Err(Error::InvalidData(_)) => {
                // The `OggS` magic matched but the checksum did not — the page
                // header or body is corrupt (RFC 3533 §6 field 1: "the decoder
                // verifies page sync and integrity by computing and comparing
                // the checksum"). A false-positive capture pattern inside an
                // earlier page's payload reaches here too. Recapture: rewind
                // PAST this `OggS` (one byte forward, so the scanner doesn't
                // re-lock onto the same bad capture) and search for the next
                // page whose checksum validates.
                self.input.seek(SeekFrom::Start(page_off + 1))?;
                self.resync_to_next_page()
            }
            // A version or other structural error from a capture-pattern-bearing
            // header is genuine corruption we don't try to paper over here;
            // propagate it.
            Err(e) => Err(e),
        }
    }

    /// Scan forward from the current stream position for the next byte offset
    /// at which a complete, checksum-valid Ogg page begins, leave the input
    /// positioned just past that page, and return it.
    ///
    /// This is the implementation of RFC 3533 §3 "recapture after a parsing
    /// error": when a read lands on bytes that are not a valid page (garbage
    /// inserted between pages, or a page whose CRC fails), the demuxer walks
    /// the input one byte at a time looking for the `OggS` capture pattern,
    /// then attempts a full [`Page::parse`] (including CRC verification) at
    /// that offset. The first candidate that parses cleanly is the resync
    /// target; candidates whose checksum fails (false-positive captures sitting
    /// inside packet payloads) are skipped and the search continues from the
    /// byte after them. Returns `Ok(None)` at EOF if no valid page remains.
    ///
    /// Each successful recovery increments [`OggDemuxer::resync_count`].
    fn resync_to_next_page(&mut self) -> Result<Option<Page>> {
        const CHUNK: usize = 65536;
        let mut search_off = self.input.stream_position()?;
        let mut buf = vec![0u8; CHUNK];
        loop {
            self.input.seek(SeekFrom::Start(search_off))?;
            let got = read_some(&mut self.input, &mut buf)?;
            if got < 4 {
                // Fewer than a capture pattern's worth of bytes remain.
                return Ok(None);
            }
            let window = &buf[..got];
            let mut i = 0usize;
            while i + 4 <= got {
                if &window[i..i + 4] != b"OggS" {
                    i += 1;
                    continue;
                }
                let candidate_off = search_off + i as u64;
                // Try to parse a full page at this candidate offset.
                self.input.seek(SeekFrom::Start(candidate_off))?;
                match self.try_parse_page_at(candidate_off)? {
                    Some(page) => {
                        // `try_parse_page_at` left the input positioned just
                        // past the page. Count the recovery and return it.
                        self.resyncs += 1;
                        self.index_record(page.serial, page.granule_position, candidate_off);
                        return Ok(Some(page));
                    }
                    None => {
                        // False-positive capture (bad CRC or truncated): skip
                        // this `OggS` and keep scanning from the next byte.
                        i += 1;
                    }
                }
            }
            // No valid page in this window. Advance, retaining a 3-byte tail so
            // an `OggS` straddling the chunk boundary is not missed.
            if got < CHUNK {
                // Reached EOF without a valid page.
                return Ok(None);
            }
            search_off += (got - 3) as u64;
        }
    }

    /// Attempt to read and CRC-verify a single page whose capture pattern is
    /// known to sit at `off`. On success the input is positioned immediately
    /// after the page and the page is returned. On a CRC failure or truncation
    /// the input position is left unspecified (the caller reseeks) and `None`
    /// is returned. Used only by [`resync_to_next_page`]; it does NOT record
    /// the seek index (the caller does, once a candidate is accepted).
    fn try_parse_page_at(&mut self, off: u64) -> Result<Option<Page>> {
        self.input.seek(SeekFrom::Start(off))?;
        let mut hdr = [0u8; 27];
        if !read_exact_or_eof(&mut self.input, &mut hdr)? {
            return Ok(None);
        }
        if hdr[0..4] != page::CAPTURE_PATTERN {
            return Ok(None);
        }
        let n_segs = hdr[26] as usize;
        let mut lacing = vec![0u8; n_segs];
        if !read_exact_or_eof(&mut self.input, &mut lacing)? {
            return Ok(None);
        }
        let data_len: usize = lacing.iter().map(|&v| v as usize).sum();
        let mut data = vec![0u8; data_len];
        if !read_exact_or_eof(&mut self.input, &mut data)? {
            return Ok(None);
        }
        let mut full = Vec::with_capacity(27 + n_segs + data_len);
        full.extend_from_slice(&hdr);
        full.extend_from_slice(&lacing);
        full.extend_from_slice(&data);
        match Page::parse(&full) {
            Ok((page, _)) => Ok(Some(page)),
            Err(_) => Ok(None),
        }
    }

    /// After the BOS section, keep reading pages and absorbing header packets
    /// until every logical stream has gathered all of its expected setup
    /// packets (3 for Vorbis, 2 for Opus, …). Audio/video packets read in the
    /// process are still queued; they'll be delivered by `next_packet` later.
    fn read_until_headers_collected(&mut self) -> Result<()> {
        loop {
            let any_pending = self
                .state_by_serial
                .values()
                .any(|s| s.headers_remaining > 0);
            // If a Skeleton stream is present, keep reading until its
            // EOS page has been seen too: Skeleton's fisbones / 4.0
            // index packets are interleaved with content streams'
            // secondary headers, so a "content streams done" exit may
            // skip some of them. The Skeleton EOS page is guaranteed
            // (per `ogg-skeleton-{3,4}.0.md`) to come BEFORE any content
            // data page, so once it lands we know every fisbone /
            // index packet is in.
            let skeleton_pending = self.skeleton_serial.is_some() && !self.skeleton_eos_seen;
            if !any_pending && !skeleton_pending {
                return Ok(());
            }
            // Drain queued pages from the BOS phase first; only then read more.
            let page = if let Some(p) = self.page_queue.pop_front() {
                p
            } else {
                match self.read_page()? {
                    Some(p) => p,
                    None => return Ok(()), // EOF before all headers — best-effort.
                }
            };
            self.process_page(page)?;
        }
    }

    /// Build codec-specific extradata for each stream from its accumulated
    /// header packets and write it back to the stream's `CodecParameters`.
    fn populate_extradata(&mut self) {
        for state in self.state_by_serial.values() {
            let codec_id = self.streams[state.public_index].params.codec_id.clone();
            let extra = build_codec_private(&codec_id, &state.header_packets);
            if !extra.is_empty() {
                self.streams[state.public_index].params.extradata = extra;
            }
        }
    }

    /// Pull the Vorbis-comment block out of whichever stream carries it
    /// (Vorbis packet #2, Opus packet #2, Theora packet #2) and expose it
    /// as container metadata.
    fn populate_metadata(&mut self) {
        // Snapshot (codec_id, header_packets) per stream first so the shared
        // `parse_codec_comment` helper can borrow `self.metadata` mutably
        // without aliasing the `self.state_by_serial` / `self.streams` reads.
        let per_stream: Vec<(CodecId, Vec<Vec<u8>>)> = self
            .state_by_serial
            .values()
            .map(|state| {
                (
                    self.streams[state.public_index].params.codec_id.clone(),
                    state.header_packets.clone(),
                )
            })
            .collect();
        for (codec_id, packets) in per_stream {
            parse_codec_comment(&codec_id, &packets, &mut self.metadata);
        }
    }

    /// Seek to the end of the file and find the last page of the first
    /// audio-or-video stream to read its granule_position, which gives
    /// the total stream length in samples or video frames.
    fn populate_duration(&mut self) {
        let saved_pos = match self.input.stream_position() {
            Ok(p) => p,
            Err(_) => return,
        };
        let end = match self.input.seek(SeekFrom::End(0)) {
            Ok(e) => e,
            Err(_) => {
                let _ = self.input.seek(SeekFrom::Start(saved_pos));
                return;
            }
        };
        // Scan back up to 64 KB looking for the last 'OggS' capture pattern.
        let scan_back = end.min(64 * 1024);
        let start = end.saturating_sub(scan_back);
        if self.input.seek(SeekFrom::Start(start)).is_err() {
            return;
        }
        let mut buf = vec![0u8; scan_back as usize];
        if self.input.read_exact(&mut buf).is_err() {
            return;
        }
        // Find the rightmost OggS header and parse it.
        let mut last_granule_by_serial: HashMap<u32, i64> = HashMap::new();
        let mut i = 0usize;
        while i + 27 <= buf.len() {
            if &buf[i..i + 4] == b"OggS" && i + 27 + (buf[i + 26] as usize) <= buf.len() {
                let n_segs = buf[i + 26] as usize;
                let body_end_off = i + 27 + n_segs;
                let data_len: usize = buf[i + 27..body_end_off].iter().map(|&v| v as usize).sum();
                if body_end_off + data_len <= buf.len() {
                    let granule = i64::from_le_bytes([
                        buf[i + 6],
                        buf[i + 7],
                        buf[i + 8],
                        buf[i + 9],
                        buf[i + 10],
                        buf[i + 11],
                        buf[i + 12],
                        buf[i + 13],
                    ]);
                    let serial =
                        u32::from_le_bytes([buf[i + 14], buf[i + 15], buf[i + 16], buf[i + 17]]);
                    if granule >= 0 {
                        last_granule_by_serial.insert(serial, granule);
                    }
                    i = body_end_off + data_len;
                    continue;
                }
            }
            i += 1;
        }
        // Pick the longest duration across streams in their own time base.
        let mut best_micros = 0i64;
        for (serial, granule) in last_granule_by_serial {
            if !self.state_by_serial.contains_key(&serial) {
                continue;
            }
            let us = self.granule_to_micros_for_serial(serial, granule);
            if us > best_micros {
                best_micros = us;
            }
        }
        self.duration_micros = best_micros;
        let _ = self.input.seek(SeekFrom::Start(saved_pos));
    }

    /// Starting at byte offset `start`, scan forward up to `limit` for the
    /// next Ogg page whose bitstream_serial_number equals `wanted_serial`.
    /// Returns `Some((page_offset, granule_position))` on success, or
    /// `None` if no such page is found in the window. Only the page header
    /// and segment table are read — the payload is skipped over by a
    /// single relative seek, so the cost is O(pages scanned), not O(bytes).
    fn find_next_page_for_serial(
        &mut self,
        start: u64,
        limit: u64,
        wanted_serial: u32,
    ) -> Result<Option<(u64, i64)>> {
        if start >= limit {
            return Ok(None);
        }

        // Read a chunk starting at `start` to find the next OggS capture.
        // Then for each candidate, seek to it, read its header + segment
        // table, and check the serial.
        let chunk_size: u64 = 64 * 1024;
        let mut cursor = start;
        while cursor < limit {
            let read_len = chunk_size.min(limit - cursor);
            self.input.seek(SeekFrom::Start(cursor))?;
            let mut buf = vec![0u8; read_len as usize];
            let mut filled = 0usize;
            while filled < buf.len() {
                match self.input.read(&mut buf[filled..]) {
                    Ok(0) => break,
                    Ok(n) => filled += n,
                    Err(e) if e.kind() == std::io::ErrorKind::Interrupted => continue,
                    Err(e) => return Err(e.into()),
                }
            }
            buf.truncate(filled);
            if buf.is_empty() {
                return Ok(None);
            }

            // Find the next OggS capture in this buffer. We need at least
            // 27 bytes (header) to parse; if we find a match near the tail
            // we re-read from that offset to span the chunk boundary.
            let mut i = 0usize;
            while i + 4 <= buf.len() {
                if &buf[i..i + 4] != b"OggS" {
                    i += 1;
                    continue;
                }
                let page_off = cursor + i as u64;

                // Read the page header + segment table directly from the
                // input (cheaper than re-reading the whole data payload).
                self.input.seek(SeekFrom::Start(page_off))?;
                let mut hdr = [0u8; 27];
                if self.input.read(&mut hdr)? != 27 {
                    return Ok(None);
                }
                if hdr[0..4] != page::CAPTURE_PATTERN {
                    // False positive (byte alignment within some payload).
                    i += 1;
                    continue;
                }
                let serial = u32::from_le_bytes([hdr[14], hdr[15], hdr[16], hdr[17]]);
                let granule = i64::from_le_bytes([
                    hdr[6], hdr[7], hdr[8], hdr[9], hdr[10], hdr[11], hdr[12], hdr[13],
                ]);
                let n_segs = hdr[26] as usize;

                if serial == wanted_serial && granule >= 0 {
                    self.index_record(serial, granule, page_off);
                    return Ok(Some((page_off, granule)));
                }

                // Even when the page isn't a match, record its
                // `(granule, page_off)` in the index — it costs nothing
                // and accelerates future seeks that DO target that serial.
                self.index_record(serial, granule, page_off);

                // Not our stream (or an "indeterminate" granule = -1).
                // Skip past this page's body to find the next one.
                let mut lacing = vec![0u8; n_segs];
                if self.input.read(&mut lacing)? != n_segs {
                    return Ok(None);
                }
                let data_len: u64 = lacing.iter().map(|&v| v as u64).sum();
                let next_off = page_off + 27 + n_segs as u64 + data_len;
                if next_off >= limit {
                    return Ok(None);
                }
                // Re-align our scanning cursor to the page end and
                // re-load the chunk buffer from there on the next pass.
                cursor = next_off;
                break;
            }
            // If we walked the whole buffer without finding OggS, advance
            // past it (minus 3 bytes to keep a possible capture that
            // straddles the boundary).
            if i + 4 > buf.len() {
                if buf.len() < 4 {
                    return Ok(None);
                }
                cursor += buf.len() as u64 - 3;
            }
        }
        Ok(None)
    }

    /// Drain the next packet from the queued pages, possibly reading more.
    fn drain_next(&mut self) -> Result<Option<Packet>> {
        loop {
            if let Some(p) = self.out_queue.pop_front() {
                return Ok(Some(p));
            }
            // Need to consume another page.
            let page = match self.page_queue.pop_front() {
                Some(p) => p,
                None => match self.read_page()? {
                    Some(p) => p,
                    None => {
                        self.eof_reached = true;
                        return Ok(None);
                    }
                },
            };
            self.process_page(page)?;
        }
    }

    /// Absorb a Skeleton-stream page: reassemble packets across page
    /// boundaries (same 255-lacing rule as content streams), dispatch
    /// each completed packet to [`FisHead::parse`] / [`FisBone::parse`] /
    /// [`SkelIndex::parse`] based on its leading magic, and append the
    /// parsed result to `self.skeleton`. Empty packets (the Skeleton
    /// EOS marker per `ogg-skeleton-{3,4}.0.md`) are ignored.
    ///
    /// Skeleton packets that fail to parse are silently dropped: a
    /// malformed Skeleton must not abort the whole demux because the
    /// content streams are still well-formed on their own.
    fn process_skeleton_page(&mut self, page: Page) {
        // RFC 3533 §6 field 6: detect dropped pages on the Skeleton
        // stream too so a hole inside a partial fisbone discards the
        // partial bytes rather than splicing them with an unrelated
        // tail.
        let hole = !page.is_first()
            && matches!(
                self.skeleton_last_seq,
                Some(prev) if page.seq_no != prev.wrapping_add(1)
            );
        if hole {
            self.holes += 1;
            self.skeleton_pending.clear();
        }
        let was_continued = page.is_continued();
        let segs = page.packet_segments();
        let mut completed: Vec<Vec<u8>> = Vec::new();
        for (i, seg) in segs.iter().enumerate() {
            let payload = &page.data[seg.data.clone()];
            if i == 0 && was_continued {
                // Tail of a fisbone / index packet from the previous page.
                self.skeleton_pending.extend_from_slice(payload);
            } else {
                // Fresh packet starts here.
                self.skeleton_pending.clear();
                self.skeleton_pending.extend_from_slice(payload);
            }
            if seg.terminated {
                completed.push(std::mem::take(&mut self.skeleton_pending));
            }
        }
        if let Some(sk) = self.skeleton.as_mut() {
            for packet in completed {
                if packet.is_empty() {
                    // The Skeleton EOS marker is an empty packet per the
                    // 3.0 / 4.0 spec (`docs/container/ogg/ogg-skeleton-3.0.md`
                    // §"Ogg Skeleton version 3.0 Format Specification":
                    // "Its eos page is included into the stream before any
                    // data pages of the other logical bitstreams appear and
                    // contains a packet of length 0."). Nothing to record.
                    continue;
                }
                if skeleton::is_fisbone(&packet) {
                    if let Ok(bone) = FisBone::parse(&packet) {
                        sk.push_bone(bone);
                    }
                } else if skeleton::is_index(&packet) {
                    if let Ok(idx) = SkelIndex::parse(&packet) {
                        sk.push_index(idx);
                    }
                } else if skeleton::is_fishead(&packet) {
                    // The BOS fishead packet was already parsed and
                    // stashed in `register_stream`; a second fishead
                    // would be a spec violation (one Skeleton stream
                    // per file). Silently ignore to keep the demuxer
                    // generous.
                } else {
                    // Unknown Skeleton packet — silently skip. The
                    // I-D allows for forward-compatible extensions.
                }
            }
        }
        self.skeleton_last_seq = Some(page.seq_no);
        if page.is_last() {
            self.skeleton_eos_seen = true;
        }
    }

    fn process_page(&mut self, page: Page) -> Result<()> {
        // RFC 3533 §4 + Vorbis I §A.2: chained Ogg streams concatenate
        // independent logical bitstreams back-to-back, each with its own
        // BOS page. A BOS page that arrives AFTER any non-BOS page in the
        // current link starts a new chained link; BOS pages arriving while
        // `seen_nonbos_in_current_link` is still false join the current
        // link's BOS section (multiplex within one link). Either way the
        // (re)registered stream inherits the link index current at its BOS.
        //
        // BOS-page handling — three cases for a `is_first()` page (the
        // Skeleton bitstream's BOS is handled by its own path and excluded
        // here):
        //
        //  * unknown serial — a NEW logical bitstream begins (a chained
        //    link's first BOS, or a grouped BOS in the initial section);
        //    register it so its identification packet is captured as a
        //    header, not delivered as data.
        //  * known serial whose BOS we have NOT yet processed — the expected
        //    re-processing of an initial-section BOS that `read_bos_section`
        //    already registered and queued. Mark it processed; otherwise do
        //    nothing special (its headers are captured by the loop below).
        //  * known serial whose BOS we HAVE processed — an RFC 3533 §4
        //    unique-serial violation (a second grouped or chained BOS reusing
        //    the serial). It must NOT fall through to the reassembly loop with
        //    the prior occupant's state, which would splice the two
        //    bitstreams' packets together. Restart the serial in place (drop
        //    its stale buffer, reset its sequence tracker, re-arm header
        //    capture, re-file it under the new link) and count the violation.
        let is_skeleton_bos = Some(page.serial) == self.skeleton_serial;
        if page.is_first() && !is_skeleton_bos {
            let known = self.state_by_serial.contains_key(&page.serial);
            let is_duplicate = known
                && self
                    .state_by_serial
                    .get(&page.serial)
                    .map(|s| s.bos_processed)
                    .unwrap_or(false);
            // A new bitstream's BOS, or a duplicate BOS, that follows at least
            // one data page in the current link opens a new chained link. The
            // expected re-processing of an already-known, not-yet-processed
            // initial BOS does not (it is still the current link).
            if (!known || is_duplicate) && self.seen_nonbos_in_current_link {
                self.next_link_index = self.next_link_index.saturating_add(1);
                self.seen_nonbos_in_current_link = false;
            }
            if !known {
                self.register_stream(&page)?;
            } else if is_duplicate {
                self.restart_serial_on_duplicate_bos(&page, self.next_link_index);
            }
            // Mark this serial's BOS as processed so a later BOS for the same
            // serial is recognised as the duplicate it is. (After a restart,
            // `restart_serial_on_duplicate_bos` reset the flag, so this line
            // re-arms it for the restarted occupant.)
            if let Some(s) = self.state_by_serial.get_mut(&page.serial) {
                s.bos_processed = true;
            }
        } else if page.is_first() && is_skeleton_bos {
            // `register_stream` for a `fishead\0` BOS initialises
            // `skeleton_serial` rather than pushing a public stream; the BOS
            // page carries the fishead packet it already parsed, so route to
            // the Skeleton path below.
            if !self.state_by_serial.contains_key(&page.serial) {
                self.register_stream(&page)?;
            }
        } else if !page.is_first() {
            self.seen_nonbos_in_current_link = true;
        }
        // Route subsequent Skeleton stream pages (fisbones, 4.0
        // indexes, the EOS-empty packet) through the metadata path —
        // they are not content packets and would otherwise hit the
        // `Unknown serial` skip below.
        if Some(page.serial) == self.skeleton_serial {
            self.process_skeleton_page(page);
            return Ok(());
        }
        // Resolve this serial's codec id and granuleshift before taking the
        // mutable `state_by_serial` borrow below — both `self.streams[..]` and
        // `self.skeleton` are read here and would otherwise alias the
        // `&mut stream`. The granuleshift is the per-track packing the
        // Skeleton 4.0 fisbone declares ("the number of lower bits from the
        // granulepos field that are used to provide position information for
        // sub-seekable units (like the keyframe shift in theora)",
        // `docs/container/ogg/ogg-skeleton-4.0.md`). Audio mappings declare 0;
        // Theora declares the keyframe shift.
        if !self.state_by_serial.contains_key(&page.serial) {
            // Unknown serial that isn't a BOS — skip silently.
            return Ok(());
        }
        let page_granuleshift = self
            .skeleton
            .as_ref()
            .and_then(|sk| sk.bone_for_serial(page.serial))
            .map(|b| b.granuleshift as u32)
            .unwrap_or(0);
        let stream = self
            .state_by_serial
            .get_mut(&page.serial)
            .expect("serial present: checked above");
        let public_index = stream.public_index;
        let stream_idx = self.streams[public_index].index;
        let time_base = self.streams[public_index].time_base;
        let segs = page.packet_segments();
        let was_continued = page.is_continued();

        // RFC 3533 §6 field 6: page_sequence_number "is increasing on each
        // logical bitstream separately" so "the decoder can identify page
        // loss." A non-BOS page whose seq_no isn't exactly last_seq+1 means
        // one or more pages were dropped between them — a "hole". The
        // counter is a wrapping u32, so the only legal successor of `prev`
        // is `prev.wrapping_add(1)`. A BOS page legitimately restarts the
        // counter (typically at 0) and is never treated as a hole.
        let hole = !page.is_first()
            && match stream.last_seq {
                Some(prev) => page.seq_no != prev.wrapping_add(1),
                None => false,
            };
        if hole {
            self.holes += 1;
            // Any packet bytes buffered from before the gap can never be
            // completed: the page(s) that carried the rest are gone. Drop
            // them so we don't splice unrelated halves into one packet.
            stream.pending.clear();
            stream.pending_valid = false;
        }

        // RFC 3533 §6 field 3 (header_type bit 0x01): the `continued` bit is
        // a normative declaration about reassembly — "set: page contains data
        // of a packet continued from the previous page; unset: page contains
        // a fresh packet." Cross-check it against our own pending state to
        // catch framing inconsistencies the `page_sequence_number` counter
        // can't see (e.g. a corrupted final segment that flipped a lacing
        // terminator within an otherwise sequence-consistent run). We skip
        // this check entirely when a hole was just detected on this page:
        // the hole already cleared `pending` and is the more specific
        // explanation, so the mismatch must not be double-counted.
        //
        // A BOS page is exempt — it always starts a stream's first packet,
        // never continues one, and its `continued` bit is conventionally
        // unset. (A continued+BOS page would be self-contradictory, but the
        // BOS path registers the stream and treats its first packet as a
        // header regardless, so there is no partial to mis-splice.)
        if !hole && !page.is_first() {
            let have_pending = !stream.pending.is_empty() && stream.pending_valid;
            if was_continued && !have_pending {
                // Page claims to continue a packet we are not holding. Its
                // leading segment is an orphaned continuation tail (the head
                // either never arrived or already terminated). The
                // reassembly loop below discards it via `pending_valid`;
                // record the inconsistency here so it is visible.
                self.framing_errors += 1;
            } else if !was_continued && have_pending {
                // Previous page ended on a 255-lacing segment (promising a
                // continuation) but this page declares a fresh packet,
                // abandoning the partial. The reassembly loop's fresh-packet
                // branch drops the orphaned head defensively; count it.
                self.framing_errors += 1;
            }
        }

        // Collect every packet that terminates on this page; the page's
        // granule_position applies to the last such packet (per RFC 3533).
        // `pending_valid` tracks whether the bytes accumulating in `pending`
        // form the contiguous tail of a real packet (vs. an orphaned
        // continuation fragment whose head was lost to a hole).
        let mut completed: Vec<(Vec<u8>, bool)> = Vec::new();
        for (i, seg) in segs.iter().enumerate() {
            let payload = &page.data[seg.data.clone()];
            if i == 0 && was_continued {
                // Leading segment continues a packet from the previous page.
                // Only append if we actually hold that packet's valid head;
                // otherwise this fragment is the unrecoverable tail of a
                // packet orphaned by a page-loss hole and must be discarded.
                if stream.pending_valid {
                    stream.pending.extend_from_slice(payload);
                } else {
                    // Orphaned continuation: ensure the buffer is empty and
                    // leave it invalid so the fragment is dropped, not
                    // emitted, when it terminates.
                    stream.pending.clear();
                }
            } else {
                // A fresh packet starts here. Any leftover bytes in
                // `pending` at this point would be a partial packet the
                // previous page left unterminated AND that this page does
                // not continue (continuation bit unset, or this is not the
                // first segment) — that's a framing inconsistency, so drop
                // them defensively.
                stream.pending.clear();
                stream.pending.extend_from_slice(payload);
                stream.pending_valid = true;
            }
            if seg.terminated {
                let valid = stream.pending_valid;
                completed.push((std::mem::take(&mut stream.pending), valid));
                // The next packet (if any) on this page starts fresh.
                stream.pending_valid = false;
            }
        }
        // Invariant after the loop: if `pending` is non-empty it holds the
        // unterminated tail of a packet that began on this page (or extended
        // a valid one), so `pending_valid` is already true — the next page's
        // leading continuation segment may safely append to it. An orphaned
        // continuation fragment was cleared above, leaving `pending` empty
        // and `pending_valid` false, so its missing-head tail is never
        // mistaken for a resumable packet.
        // Drop completed packets that were assembled from orphaned
        // continuation fragments (head lost to a hole) — they're garbage.
        let completed: Vec<Vec<u8>> = completed
            .into_iter()
            .filter_map(|(data, valid)| if valid { Some(data) } else { None })
            .collect();

        let last_idx = completed.len().checked_sub(1);
        let mut headers_just_completed = false;
        for (i, data) in completed.into_iter().enumerate() {
            if stream.headers_remaining > 0 {
                stream.header_packets.push(data);
                stream.headers_remaining -= 1;
                if stream.headers_remaining == 0 {
                    headers_just_completed = true;
                }
                continue;
            }
            let is_last = Some(i) == last_idx;
            // pts on the last-on-page packet carries the page's granule
            // (Ogg's only timing signal); intermediate packets get None.
            // Container-aware muxers that need per-packet pts should derive
            // them from codec-specific knowledge (e.g. Opus TOC parsing).
            let pts = if is_last && page.granule_position >= 0 {
                Some(page.granule_position)
            } else {
                None
            };
            // Keyframe flag (RFC 3533 carries no per-packet keyframe bit; the
            // only random-access signal is the granuleshift-packed granulepos).
            //
            // * Audio mappings (granuleshift 0) — every packet is an
            //   independent random-access point, so every packet is a keyframe.
            // * Theora-style packed granule (granuleshift > 0) — only the
            //   last-on-page packet carries a granule, and it is a keyframe
            //   iff its offset-since-keyframe (the low `shift` bits) is zero
            //   (`granule_is_keyframe`). A non-granule-bearing packet on such a
            //   track cannot be proven a keyframe from framing alone, so it is
            //   flagged `false` rather than mislabelled random-access.
            let keyframe = if page_granuleshift == 0 {
                true
            } else if is_last && page.granule_position >= 0 {
                granule_is_keyframe(page.granule_position, page_granuleshift)
            } else {
                false
            };
            let mut pkt = Packet::new(stream_idx, time_base, data);
            pkt.pts = pts;
            pkt.dts = pts;
            pkt.flags.keyframe = keyframe;
            pkt.flags.unit_boundary = is_last;
            self.out_queue.push_back(pkt);
        }

        // Track the most recently observed granule for debugging/analysis. Not
        // used to assign per-packet pts any more.
        if page.granule_position >= 0 {
            stream.granule_seen = page.granule_position;
        }
        // Record this page's sequence number so the next page on this stream
        // can be checked for continuity (RFC 3533 §6 field 6 page loss).
        stream.last_seq = Some(page.seq_no);

        // For chained streams whose headers complete after the initial
        // open() phase, rebuild extradata + metadata now so downstream
        // codec decoders see the same payload they would for a non-chained
        // stream. (`populate_extradata`/`populate_metadata` only run once
        // at open time and would otherwise leave the new stream with just
        // its identification packet in `extradata`.)
        if headers_just_completed {
            self.populate_extradata_for(public_index);
            self.populate_metadata_for(public_index);
        }
        Ok(())
    }

    /// Rebuild `params.extradata` for a single stream from its accumulated
    /// header packets. Used both during initial open and when a chained
    /// stream's headers complete mid-file.
    fn populate_extradata_for(&mut self, public_index: usize) {
        let codec_id = self.streams[public_index].params.codec_id.clone();
        let header_packets: Vec<Vec<u8>> = match self
            .state_by_serial
            .values()
            .find(|s| s.public_index == public_index)
        {
            Some(s) => s.header_packets.clone(),
            None => return,
        };
        let extra = build_codec_private(&codec_id, &header_packets);
        if !extra.is_empty() {
            self.streams[public_index].params.extradata = extra;
        }
    }

    /// Pull Vorbis-comment metadata out of the second header packet for the
    /// given stream and append it to the demuxer's metadata list.
    fn populate_metadata_for(&mut self, public_index: usize) {
        let codec_id = self.streams[public_index].params.codec_id.clone();
        // Take the stream's full header-packet list — FLAC's comment block can
        // be in any post-mapping header packet, not just the second — so the
        // chained / mid-file path covers every mapping the open-time
        // `populate_metadata` does (previously only vorbis/opus/theora,
        // dropping a chained Speex or FLAC link's tags).
        let header_packets: Option<Vec<Vec<u8>>> = self
            .state_by_serial
            .values()
            .find(|s| s.public_index == public_index)
            .map(|s| s.header_packets.clone());
        let Some(packets) = header_packets else {
            return;
        };
        parse_codec_comment(&codec_id, &packets, &mut self.metadata);
    }
}

/// Parse a logical bitstream's Vorbis-comment-style tags out of its captured
/// header packets and append them to `out`. Shared by the open-time
/// [`OggDemuxer::populate_metadata`] sweep and the per-stream
/// [`OggDemuxer::populate_metadata_for`] path used when a chained link's
/// headers complete mid-file, so every mapping behaves identically in both
/// the single-link and chained cases.
fn parse_codec_comment(codec_id: &CodecId, packets: &[Vec<u8>], out: &mut Vec<(String, String)>) {
    match codec_id.as_str() {
        "vorbis" if packets.len() >= 2 => {
            // 2nd packet starts with 0x03 "vorbis" (7 bytes) then the comment body.
            let p = &packets[1];
            if p.len() > 7 && &p[1..7] == b"vorbis" {
                parse_vorbis_comment(&p[7..], out);
            }
        }
        "opus" if packets.len() >= 2 => {
            // 2nd packet is OpusTags: 8-byte "OpusTags" magic, then the comment body.
            let p = &packets[1];
            if p.len() > 8 && &p[..8] == b"OpusTags" {
                parse_vorbis_comment(&p[8..], out);
            }
        }
        "theora" if packets.len() >= 2 => {
            // 2nd packet: 0x81 "theora" (7 bytes) then comment body.
            let p = &packets[1];
            if p.len() > 7 && &p[1..7] == b"theora" {
                parse_vorbis_comment(&p[7..], out);
            }
        }
        "speex" if packets.len() >= 2 => {
            // The Speex comment header is the 2nd packet. Unlike
            // Vorbis/Theora/Opus it carries no magic prefix: the Speex manual
            // §7.3 (`docs/audio/speex/speex-manual.pdf`) states "the second
            // packet contains the Speex comment header. The format used is the
            // Vorbis comment format" — i.e. the bare vorbis_comment structure
            // (vendor length + vendor + comment count + comments), with no
            // `0x03 "vorbis"`-style identifier and no trailing framing bit.
            // Parse it directly.
            parse_vorbis_comment(&packets[1], out);
        }
        "flac" => {
            // FLAC-in-Ogg carries each metadata block in its own header packet
            // after the mapping packet (RFC 9639 §10.1,
            // `docs/audio/flac/rfc9639-flac.pdf`). The Vorbis-comment block is
            // one of them: a 4-byte metadata block header (§8.1 — low 7 bits of
            // byte 0 are the block type; 4 = Vorbis comment) directly followed
            // by the standard vorbis_comment payload (vendor length + vendor +
            // comment count + …, with no `0x03 "vorbis"` prefix and no trailing
            // framing bit). Skip the mapping packet (index 0) and scan the
            // remaining header packets for the type-4 block.
            for p in packets.iter().skip(1) {
                if p.len() >= 4 && (p[0] & 0x7F) == 4 {
                    parse_vorbis_comment(&p[4..], out);
                    break;
                }
            }
        }
        _ => {}
    }
}

impl Demuxer for OggDemuxer {
    fn format_name(&self) -> &str {
        "ogg"
    }

    fn streams(&self) -> &[StreamInfo] {
        &self.streams
    }

    fn next_packet(&mut self) -> Result<Packet> {
        if let Some(p) = self.drain_next()? {
            return Ok(p);
        }
        Err(Error::Eof)
    }

    fn seek_to(&mut self, stream_index: u32, pts: i64) -> Result<i64> {
        if stream_index as usize >= self.streams.len() {
            return Err(Error::invalid(format!(
                "Ogg: stream index {stream_index} out of range"
            )));
        }

        // Find the serial for the requested stream.
        let mut wanted_serial: Option<u32> = None;
        for (serial, state) in &self.state_by_serial {
            if self.streams[state.public_index].index == stream_index {
                wanted_serial = Some(*serial);
                break;
            }
        }
        let wanted_serial = wanted_serial.ok_or_else(|| {
            Error::unsupported(format!("Ogg: no logical stream for index {stream_index}"))
        })?;

        // Build the codec-aware comparison axis for the bisection.
        //
        // For Vorbis / FLAC / Speex the stream's `time_base` already
        // matches the native granule unit (samples/Hz), so `pts` *is* the
        // target granule and every page's raw `granule_position` is the
        // comparison key — `target_key == pts`, `key_of(g) == g`.
        //
        // Opus is the same axis with a per-stream bias: its pages carry
        // `PCM position + pre-skip` (RFC 7845 §4.3,
        // `docs/audio/opus/rfc7845-ogg-opus.txt`), so `key_of(g) == g −
        // pre-skip` lines the page side up with the PCM-position `pts`.
        // A seek without this bias would land `pre-skip / 48000` s early.
        //
        // Theora packs a keyframe index and a per-keyframe offset into a
        // single granule value (`(kf << shift) | offset`) so the raw
        // granule isn't a usable comparison axis on its own. When a
        // Skeleton 4.0 `fisbone\0` is present for the stream's serial,
        // `bone.granuleshift` and `bone.granule_rate` give us enough to
        // translate: the comparison key is the frame number
        // `(g >> shift) + (g & mask)` and the target is the frame
        // number that corresponds to `pts` under the stream's time-base
        // (rescaled into frame-rate units via `TimeBase::rescale`).
        //
        // Theora's encoded granule values are strictly monotonic with the
        // frame number across a single logical stream (see the
        // implementation comment block on `index_floor_by` and on
        // `theora_frame_no` for the proof), so a binary search by frame
        // number is well-defined on the raw-granule-sorted seek index.
        //
        // Theora without a Skeleton fisbone, and any other unrecognised
        // codec, still returns `Unsupported` — there's no codec-agnostic
        // way to translate `pts` into a granule key without the
        // fisbone's `granuleshift` + `granule_rate`. This keeps the same
        // public contract for the pre-Skeleton cases the round 199
        // change covered.
        let codec_id = self.streams[stream_index as usize].params.codec_id.clone();
        let stream_tb_for_key = self.streams[stream_index as usize].time_base;
        let seek_key: SeekKey = match codec_id.as_str() {
            // Opus pages carry `PCM position + pre-skip` (RFC 7845 §4.3), so
            // the bisection axis offsets each page granule by the stream's
            // pre-skip to compare against the PCM-position `pts`. The other
            // audio mappings have no such bias (`identity` == offset 0).
            "opus" => SeekKey::identity_offset(
                pts,
                self.opus_pre_skip
                    .get(&wanted_serial)
                    .map(|&p| p as i64)
                    .unwrap_or(0),
            ),
            "vorbis" | "flac" | "speex" => SeekKey::identity(pts),
            "theora" => match self.theora_seek_key(wanted_serial, pts, stream_tb_for_key) {
                Some(key) => key,
                None => {
                    return Err(Error::unsupported(format!(
                        "Ogg: seek_to on stream {stream_index} ({}) requires a Skeleton fisbone with non-zero granuleshift and granule_rate",
                        codec_id.as_str()
                    )));
                }
            },
            _ => {
                return Err(Error::unsupported(format!(
                    "Ogg: seek_to not implemented for codec {}",
                    codec_id.as_str()
                )));
            }
        };
        let target_key = seek_key.target_key;

        // Determine the seekable byte range. `lo` is the current position
        // of the first page after the BOS/header section isn't known here —
        // we conservatively start at 0 and scan forward to the first OggS.
        let file_size = {
            let cur = self.input.stream_position()?;
            let end = self.input.seek(SeekFrom::End(0))?;
            self.input.seek(SeekFrom::Start(cur))?;
            end
        };
        if file_size == 0 {
            return Err(Error::unsupported("Ogg: empty input"));
        }

        // Fastest path: if a Skeleton 4.0 `index\0` packet was parsed for
        // this stream's serial, look up its floor keypoint by timestamp
        // and jump straight there. No bisection, no `build_seek_index`
        // pre-scan, no per-page tightening — the index promises the
        // keypoint is a valid seek target (per
        // `docs/container/ogg/ogg-skeleton-4.0.md`). Falls through to
        // the page-level index_floor / bisection below when no Skeleton
        // index is available for this stream's serial, OR when the
        // index fails the three per-spec validity checks:
        //
        //   1. the `fishead` `Segment length in bytes` is consistent with
        //      the file — it equals the file size (single-link file), or,
        //      for a chained file, names a shorter length at which a new
        //      link's `OggS` BOS page begins ("a new \"link\" in a
        //      \"chain\" can start at the end of the segment") — a one-shot
        //      lazy check on the first seek call;
        //   2. the keypoint's stored offset lands on an `OggS` page
        //      boundary;
        //   3. that page's `bitstream_serial_number` equals the
        //      keypoint's stream serial.
        //
        // Per the spec ("Be aware that you cannot assume that any or all
        // Ogg files will contain keyframe indexes, so when implementing
        // Ogg seeking, you must gracefully fall-back to a bisection
        // search or other seek algorithm when the index is not present,
        // or when it is invalid.") a failed check is silent — the
        // fall-through bisection still returns the right answer, just
        // more slowly. `skeleton_index_invalid_count()` exposes the
        // running tally of rejections for diagnostics.
        let stream_time_base = self.streams[stream_index as usize].time_base;
        if self.skeleton_segment_length_check(file_size) {
            if let Some(SkeletonIndexSeek {
                byte_offset,
                winning_serial,
                returned_granule,
            }) = self.skeleton_index_seek(wanted_serial, pts, stream_time_base)
            {
                // Per the Skeleton 4.0 multi-stream minimisation rule
                // (`docs/container/ogg/ogg-skeleton-4.0.md`
                // §"Keyframe indexes for faster seeking"), the winning
                // keypoint may belong to a stream OTHER than the one
                // the user asked to seek on. The per-spec validity
                // check ("you don't land on a page which belongs to
                // that keypoint's stream") therefore tests against
                // `winning_serial`, not against the user-requested
                // `wanted_serial`. The returned granule still belongs
                // to the requested stream's time base.
                if self.verify_keypoint_landing(byte_offset, winning_serial) {
                    self.input.seek(SeekFrom::Start(byte_offset))?;
                    self.page_queue.clear();
                    self.out_queue.clear();
                    for state in self.state_by_serial.values_mut() {
                        state.pending.clear();
                        state.granule_seen = 0;
                    }
                    self.eof_reached = false;
                    self.skeleton_index_seeks = self.skeleton_index_seeks.saturating_add(1);
                    return Ok(returned_granule);
                } else {
                    // Per-keypoint validity check failed: the bytes at
                    // `off` are not an `OggS` page belonging to the
                    // requested serial. The seek MUST still complete via
                    // bisection — reset to a known-good position before
                    // falling through.
                    self.skeleton_index_rejects = self.skeleton_index_rejects.saturating_add(1);
                    self.input.seek(SeekFrom::Start(0))?;
                }
            }
        } else {
            // Segment-length disagreement: per spec the whole Skeleton
            // index is untrusted. Count one rejection regardless of
            // whether a floor keypoint would have been found, so the
            // diagnostic counter reflects "we skipped the fast path
            // because of this file's BOS-level mismatch".
            if self
                .skeleton
                .as_ref()
                .and_then(|s| s.index_for_serial(wanted_serial))
                .is_some()
            {
                self.skeleton_index_rejects = self.skeleton_index_rejects.saturating_add(1);
            }
        }

        // Fast path: if the seek index already has a `(granule, offset)`
        // entry whose mapped key is `<= target_key`, jump to it directly.
        // The remaining linear-tail scan below still runs to tighten the
        // landing point against any indexed entries that were inserted
        // between the floor and the target — that scan reuses the index
        // too because `find_next_page_for_serial` records as it goes.
        // For codecs with `SeekKey::Identity` this collapses to the raw
        // `granule <= target_granule` semantics the pre-Theora path
        // already had; for Theora's `SeekKey::TheoraFrame` the floor
        // lookup runs through `(g >> shift) + (g & mask)` so the
        // largest indexed page whose *frame number* is at or before the
        // target frame wins.
        if let Some((g, off)) =
            self.index_floor_by(wanted_serial, target_key, |g| seek_key.key_of(g))
        {
            self.input.seek(SeekFrom::Start(off))?;
            self.page_queue.clear();
            self.out_queue.clear();
            for state in self.state_by_serial.values_mut() {
                state.pending.clear();
                state.granule_seen = 0;
            }
            self.eof_reached = false;
            // If `build_seek_index` ran, the index is dense and we know
            // there's no better page between `off` and the target —
            // return immediately. Otherwise (sparse index from incidental
            // scans), fall through to the bisection below to tighten.
            if self.seek_index_built {
                return Ok(g);
            }
            // Sparse case: seed the bisection so it can only improve the
            // landing point, never regress past `off`.
            // (Handled by falling through; the bisection's `landed`
            // tracker takes max() of each successful candidate.)
        }

        // Bisection state.
        let mut lo: u64 = 0;
        let mut hi: u64 = file_size;
        // Best-so-far: the last page with `key_of(granule) <= target_key`
        // belonging to the requested stream. Tuple of (page_offset,
        // granule, key).
        let mut landed: Option<(u64, i64, i64)> = None;
        let threshold: u64 = 64 * 1024;

        // Upper bound on iterations: log2(file_size) + a handful for the
        // linear tail when the range shrinks below `threshold`.
        let max_iters: u32 = 64;
        let mut iters: u32 = 0;

        while lo < hi && iters < max_iters {
            iters += 1;
            let mid = lo + (hi - lo) / 2;
            // Scan forward from `mid` for the next OggS page belonging to
            // the target stream.
            let scan_limit: u64 = 64 * 1024;
            let scan_end = (mid + scan_limit).min(file_size);
            let (page_off, granule) =
                match self.find_next_page_for_serial(mid, scan_end, wanted_serial)? {
                    Some(v) => v,
                    None => {
                        // No matching page in this window — give up on the
                        // upper half and tighten `hi`.
                        hi = mid;
                        continue;
                    }
                };

            let key = seek_key.key_of(granule);
            if key <= target_key {
                // This page is at or before target — remember it, try
                // later offsets.
                if landed.map(|(_, _, lk)| key >= lk).unwrap_or(true) {
                    landed = Some((page_off, granule, key));
                }
                // Advance past this page's header to avoid re-landing on
                // the same page forever.
                lo = page_off + 1;
            } else {
                // key > target — search the lower half.
                hi = page_off;
            }

            // Avoid pathological ping-pong on very small ranges.
            if hi.saturating_sub(lo) < threshold {
                break;
            }
        }

        // After bisection, do a bounded linear scan from `lo` toward `hi`
        // to tighten `landed` — this handles the final few pages inside
        // the threshold window without more bisection iterations.
        if landed.is_some() {
            let mut cursor = landed.map(|(off, _, _)| off + 1).unwrap_or(lo);
            let scan_end = hi.min(cursor + threshold * 2);
            while cursor < scan_end {
                match self.find_next_page_for_serial(cursor, scan_end, wanted_serial)? {
                    Some((off, g)) => {
                        let k = seek_key.key_of(g);
                        if k > target_key {
                            break;
                        }
                        if landed.map(|(_, _, lk)| k >= lk).unwrap_or(true) {
                            landed = Some((off, g, k));
                        }
                        cursor = off + 1;
                    }
                    None => break,
                }
            }
        } else {
            // Bisection never found a page <= target. Try scanning from
            // the start one time — this covers files where every page's
            // granule exceeds the target (e.g., seek to pts 0 on a stream
            // whose first page already has granule > 0).
            if let Some((off, g)) = self.find_next_page_for_serial(0, file_size, wanted_serial)? {
                let k = seek_key.key_of(g);
                // Even when this page's key is past target, returning it
                // is still the best we can do — the user asked to seek
                // before the first available page of the stream.
                landed = Some((off, g, k));
            }
        }

        let (landed_off, landed_granule, _landed_key) = landed.ok_or_else(|| {
            Error::unsupported(format!(
                "Ogg: no seekable page found for stream {stream_index}"
            ))
        })?;

        // Seek the underlying input to the page boundary and flush all
        // buffered demuxer state so playback resumes cleanly.
        self.input.seek(SeekFrom::Start(landed_off))?;
        self.page_queue.clear();
        self.out_queue.clear();
        for state in self.state_by_serial.values_mut() {
            state.pending.clear();
            state.granule_seen = 0;
        }
        self.eof_reached = false;

        Ok(landed_granule)
    }

    fn metadata(&self) -> &[(String, String)] {
        &self.metadata
    }

    fn duration_micros(&self) -> Option<i64> {
        if self.duration_micros > 0 {
            Some(self.duration_micros)
        } else {
            None
        }
    }
}

/// Parse a Vorbis-comment payload. The input does NOT include any codec
/// magic prefix — the caller must strip it first. Appends (lowercase key,
/// value) pairs to `out`.
fn parse_vorbis_comment(buf: &[u8], out: &mut Vec<(String, String)>) {
    let mut i = 0usize;
    if buf.len() < 4 {
        return;
    }
    let vlen = u32::from_le_bytes([buf[0], buf[1], buf[2], buf[3]]) as usize;
    i += 4;
    if i + vlen > buf.len() {
        return;
    }
    let vendor = String::from_utf8_lossy(&buf[i..i + vlen]).to_string();
    i += vlen;
    if !vendor.is_empty() {
        out.push(("vendor".into(), vendor));
    }
    if i + 4 > buf.len() {
        return;
    }
    let n = u32::from_le_bytes([buf[i], buf[i + 1], buf[i + 2], buf[i + 3]]) as usize;
    i += 4;
    for _ in 0..n {
        if i + 4 > buf.len() {
            break;
        }
        let clen = u32::from_le_bytes([buf[i], buf[i + 1], buf[i + 2], buf[i + 3]]) as usize;
        i += 4;
        if i + clen > buf.len() {
            break;
        }
        let entry = &buf[i..i + clen];
        i += clen;
        if let Some(eq) = entry.iter().position(|&b| b == b'=') {
            let key = String::from_utf8_lossy(&entry[..eq])
                .to_ascii_lowercase()
                .trim()
                .to_string();
            let value = String::from_utf8_lossy(&entry[eq + 1..]).trim().to_string();
            if !key.is_empty() && !value.is_empty() {
                out.push((key, value));
            }
        }
    }
}

/// Build initial codec parameters from a known identification packet.
fn guess_params(codec_id: &CodecId, first: &[u8]) -> Result<CodecParameters> {
    let mut p = match codec_id.as_str() {
        "vorbis" => CodecParameters::audio(codec_id.clone()),
        "opus" => CodecParameters::audio(codec_id.clone()),
        "flac" => CodecParameters::audio(codec_id.clone()),
        "theora" => CodecParameters::video(codec_id.clone()),
        "speex" => CodecParameters::audio(codec_id.clone()),
        _ => {
            let mut p = CodecParameters::audio(codec_id.clone());
            p.media_type = MediaType::Unknown;
            p
        }
    };

    match codec_id.as_str() {
        "vorbis" => parse_vorbis_id(&mut p, first)?,
        "opus" => parse_opus_id(&mut p, first)?,
        "speex" => parse_speex_id(&mut p, first)?,
        "flac" => parse_flac_id(&mut p, first)?,
        _ => {}
    }
    Ok(p)
}

fn parse_vorbis_id(p: &mut CodecParameters, packet: &[u8]) -> Result<()> {
    if packet.len() < 30 {
        return Err(Error::invalid("Vorbis identification header too short"));
    }
    // packet[0]=0x01, packet[1..7]="vorbis", packet[7..11]=version (must be 0).
    let version = u32::from_le_bytes([packet[7], packet[8], packet[9], packet[10]]);
    if version != 0 {
        return Err(Error::unsupported(format!(
            "unsupported Vorbis version {version}"
        )));
    }
    let channels = packet[11];
    let sample_rate = u32::from_le_bytes([packet[12], packet[13], packet[14], packet[15]]);
    let _br_max = i32::from_le_bytes([packet[16], packet[17], packet[18], packet[19]]);
    let br_nom = i32::from_le_bytes([packet[20], packet[21], packet[22], packet[23]]);
    let _br_min = i32::from_le_bytes([packet[24], packet[25], packet[26], packet[27]]);
    if channels == 0 || sample_rate == 0 {
        return Err(Error::invalid("Vorbis ID header has zero channels or rate"));
    }
    p.channels = Some(channels as u16);
    p.sample_rate = Some(sample_rate);
    if br_nom > 0 {
        p.bit_rate = Some(br_nom as u64);
    }
    Ok(())
}

/// Read the Opus **pre-skip** from an `OpusHead` identification packet.
///
/// `docs/audio/opus/rfc7845-ogg-opus.txt` §5.1 field 4: the pre-skip is a
/// 16-bit little-endian unsigned value at byte offset 10..12 of the header
/// (after the 8-byte `"OpusHead"` magic, the 1-byte version, and the
/// 1-byte output channel count). Returns `None` if `packet` is too short
/// to reach the field (a malformed or truncated header), in which case the
/// caller treats the stream as pre-skip-free.
fn opus_pre_skip(packet: &[u8]) -> Option<u16> {
    if packet.len() < 12 {
        return None;
    }
    Some(u16::from_le_bytes([packet[10], packet[11]]))
}

fn parse_opus_id(p: &mut CodecParameters, packet: &[u8]) -> Result<()> {
    if packet.len() < 19 {
        return Err(Error::invalid("Opus identification header too short"));
    }
    let channels = packet[9];
    let input_rate = u32::from_le_bytes([packet[12], packet[13], packet[14], packet[15]]);
    p.channels = Some(channels as u16);
    // Opus always decodes to 48 kHz; "input_sample_rate" is informational.
    p.sample_rate = Some(if input_rate > 0 { input_rate } else { 48_000 });
    Ok(())
}

/// Parse the **Ogg/Speex identification header** (the first packet of a
/// Speex-in-Ogg logical bitstream) for `rate`, `nb_channels`, and `bitrate`.
///
/// Per the Speex manual §7.3 / table 7.1
/// (`docs/audio/speex/speex-manual.pdf`) the header is a fixed 80-byte
/// struct whose integer fields are all little-endian:
///
/// | offset | size | field                    |
/// |--------|------|--------------------------|
/// | 0      | 8    | `speex_string` (`"Speex "`)|
/// | 8      | 20   | `speex_version`          |
/// | 28     | 4    | `speex_version_id`       |
/// | 32     | 4    | `header_size`            |
/// | 36     | 4    | `rate`                   |
/// | 40     | 4    | `mode`                   |
/// | 44     | 4    | `mode_bitstream_version` |
/// | 48     | 4    | `nb_channels`            |
/// | 52     | 4    | `bitrate`                |
/// | 56     | 4    | `frame_size`             |
/// | 60     | 4    | `vbr`                    |
/// | 64     | 4    | `frames_per_packet`      |
/// | 68     | 4    | `extra_headers`          |
/// | 72     | 4    | `reserved1`              |
/// | 76     | 4    | `reserved2`              |
///
/// `bitrate` is informational and signed (the manual encodes a `-1`
/// "unknown" sentinel as the encoder's default); only a strictly-positive
/// value is surfaced. `nb_channels` other than 1 or 2 is clamped to a sane
/// 1/2 because Speex itself only supports mono and stereo, and a corrupt
/// field must not propagate an absurd channel count downstream.
fn parse_speex_id(p: &mut CodecParameters, packet: &[u8]) -> Result<()> {
    // We need to reach `nb_channels` (offset 48..52) at minimum; the full
    // 80-byte header is the norm but we read only the fields we surface.
    if packet.len() < 52 {
        return Err(Error::invalid("Speex identification header too short"));
    }
    let rate = u32::from_le_bytes([packet[36], packet[37], packet[38], packet[39]]);
    let nb_channels = u32::from_le_bytes([packet[48], packet[49], packet[50], packet[51]]);
    if rate == 0 {
        return Err(Error::invalid("Speex ID header has zero sample rate"));
    }
    p.sample_rate = Some(rate);
    // Speex is mono or stereo only (manual §7.3). Treat any other value as a
    // corrupt field and clamp rather than report e.g. 65535 channels.
    p.channels = Some(if nb_channels == 2 { 2 } else { 1 });
    if packet.len() >= 56 {
        let bitrate = i32::from_le_bytes([packet[52], packet[53], packet[54], packet[55]]);
        if bitrate > 0 {
            p.bit_rate = Some(bitrate as u64);
        }
    }
    Ok(())
}

/// Parse the **FLAC-in-Ogg mapping packet** (the first packet of a
/// FLAC-in-Ogg logical bitstream) for `sample_rate` and `channels`,
/// reading the embedded STREAMINFO block.
///
/// RFC 9639 §10.1 (`docs/audio/flac/rfc9639-flac.pdf`, Table 24) lays the
/// mapping packet out as `0x7F "FLAC"` (5) + 2-byte mapping version +
/// 2-byte BE header-packet count + `"fLaC"` (4) + a 4-byte metadata block
/// header + the 34-byte STREAMINFO block. STREAMINFO therefore begins at
/// packet offset 17.
///
/// Within STREAMINFO (RFC 9639 §8.2, Table 3) the fields are bit-packed
/// big-endian: `u(16)` min block size, `u(16)` max block size, `u(24)` min
/// frame size, `u(24)` max frame size — i.e. 10 bytes — then `u(20)` sample
/// rate, `u(3)` (channels − 1), `u(5)` (bits per sample − 1). The
/// rate/channel/depth triple is the 4 bytes at STREAMINFO offset 10
/// (packet offset 27): the top 20 bits are the sample rate, the next 3 are
/// channels − 1, the next 5 are bits-per-sample − 1.
fn parse_flac_id(p: &mut CodecParameters, packet: &[u8]) -> Result<()> {
    // STREAMINFO starts at offset 17; we need through its byte 13 (the
    // rate/channels/bps triple ends at STREAMINFO offset 14 == packet 31).
    const STREAMINFO_OFF: usize = 17;
    if packet.len() < STREAMINFO_OFF + 14 {
        // Not enough bytes for STREAMINFO's rate/channel/depth field; leave
        // the params unpopulated rather than erroring — the mapping packet
        // is otherwise well-formed and downstream FLAC decode still works
        // off the raw header packets in `extradata`.
        return Ok(());
    }
    let si = &packet[STREAMINFO_OFF..];
    // Bytes 10..14 of STREAMINFO hold sample_rate(20) | channels-1(3) |
    // bps-1(5) | high 4 bits of total-samples. Read 4 bytes big-endian.
    let packed = u32::from_be_bytes([si[10], si[11], si[12], si[13]]);
    let sample_rate = packed >> 12; // top 20 bits
    let channels = ((packed >> 9) & 0x07) + 1; // next 3 bits, stored as (n-1)
    if sample_rate != 0 {
        p.sample_rate = Some(sample_rate);
    }
    p.channels = Some(channels as u16);
    Ok(())
}

/// Build the per-codec setup blob ("CodecPrivate" in Matroska, "esds"-equivalent
/// in MP4, etc.) from the header packets gathered out of an Ogg stream.
///
/// - Vorbis / Theora: Xiph-laced concatenation of all 3 header packets
///   (id, comment, setup) — one count byte (N-1) + Xiph-style sizes for the
///   first N-1 packets + packets concatenated. This is the layout the
///   corresponding decoders consume via `parse_xiph_extradata`.
/// - Opus: just the OpusHead identification packet (OpusTags discarded).
/// - Anything else: concatenate the headers and let the codec sort it out.
fn build_codec_private(codec_id: &CodecId, packets: &[Vec<u8>]) -> Vec<u8> {
    match codec_id.as_str() {
        "vorbis" | "theora" if packets.len() == 3 => xiph_lace_three(packets),
        "opus" => packets.first().cloned().unwrap_or_default(),
        _ => packets.iter().flatten().copied().collect(),
    }
}

/// Xiph-lace three header packets into the single-blob extradata format used
/// by Vorbis and Theora in MP4/MKV (and consumed by our per-codec decoders).
fn xiph_lace_three(packets: &[Vec<u8>]) -> Vec<u8> {
    debug_assert_eq!(packets.len(), 3);
    let mut out = Vec::with_capacity(
        1 + packets[0].len() / 255
            + 1
            + packets[1].len() / 255
            + 1
            + packets.iter().map(|p| p.len()).sum::<usize>(),
    );
    out.push(0x02); // 3 packets - 1
    out.extend(xiph_lace_size(packets[0].len()));
    out.extend(xiph_lace_size(packets[1].len()));
    out.extend_from_slice(&packets[0]);
    out.extend_from_slice(&packets[1]);
    out.extend_from_slice(&packets[2]);
    out
}

fn xiph_lace_size(mut n: usize) -> Vec<u8> {
    let mut v = Vec::with_capacity(n / 255 + 1);
    while n >= 255 {
        v.push(255);
        n -= 255;
    }
    v.push(n as u8);
    v
}

fn read_exact_or_eof(r: &mut dyn Read, buf: &mut [u8]) -> Result<bool> {
    let mut got = 0;
    while got < buf.len() {
        match r.read(&mut buf[got..]) {
            Ok(0) => {
                return if got == 0 {
                    Ok(false)
                } else {
                    Err(Error::invalid(format!(
                        "Ogg: truncated read ({}/{} bytes)",
                        got,
                        buf.len()
                    )))
                };
            }
            Ok(n) => got += n,
            Err(e) if e.kind() == std::io::ErrorKind::Interrupted => continue,
            Err(e) => return Err(e.into()),
        }
    }
    Ok(true)
}

/// Read up to `buf.len()` bytes into `buf`, returning the actual count read.
/// Unlike `read_exact_or_eof` this is satisfied by a short read short of EOF
/// (a single `read` syscall) and is appropriate for chunked scanning where
/// any prefix is acceptable. EOF is signalled by a return of zero. Interrupts
/// are retried transparently.
fn read_some(r: &mut dyn Read, buf: &mut [u8]) -> Result<usize> {
    loop {
        match r.read(buf) {
            Ok(n) => return Ok(n),
            Err(e) if e.kind() == std::io::ErrorKind::Interrupted => continue,
            Err(e) => return Err(e.into()),
        }
    }
}

#[cfg(test)]
mod granule_tests {
    use super::{granule_is_keyframe, theora_frame_no};

    #[test]
    fn shift_zero_is_always_keyframe() {
        // Audio mappings declare granuleshift 0; every packet is a
        // random-access point.
        assert!(granule_is_keyframe(0, 0));
        assert!(granule_is_keyframe(1024, 0));
        assert!(granule_is_keyframe(i64::MAX, 0));
    }

    #[test]
    fn negative_sentinel_is_keyframe() {
        // The RFC 3533 §6 "-1" no-packet-finishes sentinel passes through as a
        // conservative random-access point (no granule-bearing packet is ever
        // delivered for it, but the helper must not under-report).
        assert!(granule_is_keyframe(-1, 6));
        assert!(granule_is_keyframe(-1, 0));
    }

    #[test]
    fn theora_shift_offset_zero_is_keyframe() {
        // shift = 6: keyframe iff the low 6 bits (offset since keyframe) == 0.
        // (0<<6)|0 == 0, (128<<6)|0 == 8192 -> keyframes.
        assert!(granule_is_keyframe(0, 6));
        assert!(granule_is_keyframe(8192, 6));
        // (0<<6)|30 == 30, (64<<6)|32 == 4128 -> inter frames.
        assert!(!granule_is_keyframe(30, 6));
        assert!(!granule_is_keyframe(4128, 6));
    }

    #[test]
    fn degenerate_shift_is_keyframe_not_overflow() {
        // shift >= 63 leaves no room for a keyframe index; treat as a
        // random-access point rather than overflowing the 1<<shift mask.
        assert!(granule_is_keyframe(123, 63));
        assert!(granule_is_keyframe(123, 64));
    }

    #[test]
    fn keyframe_decision_agrees_with_frame_extraction() {
        // A frame is a keyframe exactly when its absolute frame number equals
        // the keyframe index it counts from, i.e. its offset half is zero.
        for shift in [1u32, 4, 6, 10] {
            for kf in [0i64, 1, 5, 200] {
                let mask = (1i64 << shift) - 1;
                for off in [0i64, 1, mask] {
                    let g = (kf << shift) | off;
                    let is_kf = granule_is_keyframe(g, shift);
                    assert_eq!(is_kf, off == 0, "g={g} shift={shift}");
                    if is_kf {
                        // The keyframe's absolute frame number is the kf index.
                        assert_eq!(theora_frame_no(g, shift), kf);
                    }
                }
            }
        }
    }
}
