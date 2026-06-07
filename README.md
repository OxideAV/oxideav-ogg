# oxideav-ogg

Pure-Rust **Ogg** container (RFC 3533) — page framing, CRC32
checksumming, packet reassembly across page boundaries, multi-stream
(multiplexed logical bitstream) demux, codec sniffing, metadata, and
a muxer that emits compliant Ogg for Vorbis, Opus, Theora, FLAC and
Speex. Zero C dependencies.

Part of the [oxideav](https://github.com/OxideAV/oxideav-workspace)
framework but usable standalone.

## Installation

```toml
[dependencies]
oxideav-core = "0.1"
oxideav-codec = "0.1"
oxideav-container = "0.1"
oxideav-ogg = "0.0"
```

## Quick use

Ogg is codec-agnostic: this crate only frames packets. Pair it with
a codec crate (`oxideav-vorbis`, `oxideav-opus`, `oxideav-theora`,
`oxideav-flac`, ...) to decode the payloads.

```rust
use oxideav_codec::CodecRegistry;
use oxideav_container::ContainerRegistry;
use oxideav_core::Frame;

let mut codecs = CodecRegistry::new();
let mut containers = ContainerRegistry::new();
oxideav_vorbis::register(&mut codecs);
oxideav_ogg::register(&mut containers);

let input: Box<dyn oxideav_container::ReadSeek> = Box::new(
    std::io::Cursor::new(std::fs::read("song.ogg")?),
);
let mut dmx = containers.open("ogg", input)?;
let stream = &dmx.streams()[0];
let mut dec = codecs.make_decoder(&stream.params)?;

loop {
    match dmx.next_packet() {
        Ok(pkt) => {
            dec.send_packet(&pkt)?;
            while let Ok(Frame::Audio(af)) = dec.receive_frame() {
                // af.samples carries decoded PCM in the codec's native layout.
            }
        }
        Err(oxideav_core::Error::Eof) => break,
        Err(e) => return Err(e.into()),
    }
}
# Ok::<(), Box<dyn std::error::Error>>(())
```

### Codec detection

On BOS (beginning-of-stream) the demuxer inspects the first packet of
each logical bitstream and assigns a `CodecId`:

| first-packet signature                | `CodecId`  |
|---------------------------------------|------------|
| `0x01` + `"vorbis"`                   | `vorbis`   |
| `"OpusHead"`                          | `opus`     |
| `0x7F` + `"FLAC"`                     | `flac`     |
| `0x80` + `"theora"`                   | `theora`   |
| `"Speex   "` (8 bytes incl. spaces)   | `speex`    |

All other streams are reported as `CodecId::new("unknown")` so the
registry can still walk them; decode will fail for unregistered codecs.

### Multi-stream

Multiplexed Ogg (e.g., Theora video + Vorbis audio in the same `.ogv`)
is supported end-to-end: every BOS page yields its own `StreamInfo`,
packets are reassembled per-stream across interleaved pages, and the
muxer emits BOS pages for every stream before any non-BOS page as
required by RFC 3533 §6.

### Seeking

`seek_to(stream_index, pts)` performs a bounded bisection over the
file using granule-position timestamps on Ogg pages. Vorbis, Opus,
FLAC and Speex land on the greatest page whose granule is at or
below the target.

For Theora, the page's raw granule packs `(keyframe_idx << shift) |
frame_offset`, so the comparison axis is the frame number
`(g >> shift) + (g & ((1 << shift) - 1))` rather than the raw
granule value. The required `shift` and frame rate come from the
per-stream Skeleton 4.0 `fisbone\0` (`granuleshift` and
`granule_rate`, per `docs/container/ogg/ogg-skeleton-4.0.md`): the
user's `pts` (microseconds) is rescaled into frame-rate units via
[`TimeBase::rescale`] to produce the target frame number, and the
bisection — both its index-floor lookup and its forward
`find_next_page_for_serial` scan — compares mapped frame numbers
against that target. The returned granule is the actual on-wire
value of the landed page, so a downstream Theora decoder can
recover the keyframe-index / offset pair as usual. A Theora stream
that lacks a `fisbone\0` (or whose `fisbone\0` has
`granuleshift == 0`) still returns `Error::Unsupported`: without
the Skeleton-borne shift+rate we cannot translate `pts` to a frame
number, and the conservative choice is to refuse rather than
silently misinterpret the raw granule as a target. Codecs other
than the five listed above continue to return `Error::Unsupported`.

For workloads with many seeks (scrubbing, looped playback) call
`oxideav_ogg::demux::open_indexed` instead of `open`. It does a
one-shot full-file page-header scan up front (header + segment table
only — payloads are skipped via relative seek) and records every
`(granule, page_offset)` per logical stream. Each subsequent
`seek_to` becomes an O(log n) binary-search lookup followed by a
single seek, instead of an O(log n) bisection that re-reads page
clusters on every call. The index is built into the concrete
`OggDemuxer`; the boxed `Demuxer` returned by `open_indexed`
benefits transparently. `open_concrete` is also available for
callers that want to call `build_seek_index` / `seek_index_len`
explicitly. Pages with granule `-1` (RFC 3533 §6 "no packets finish
on this page") are excluded from the index because they carry no
seek-target information.

### Muxing

The muxer packs incoming packets into pages with a proper CRC32,
granule-position carry-through from `Packet::pts`, a page flush on
each `unit_boundary` packet, and an EOS flag on the last page of
each stream:

```rust
use oxideav_container::{ContainerRegistry, WriteSeek};
use oxideav_core::{CodecParameters, CodecId, Packet, StreamInfo, TimeBase};

let mut containers = ContainerRegistry::new();
oxideav_ogg::register(&mut containers);

let mut params = CodecParameters::audio(CodecId::new("vorbis"));
params.channels = Some(2);
params.sample_rate = Some(48_000);
params.extradata = /* xiph-laced id+comment+setup */ vec![];
let streams = vec![StreamInfo {
    index: 0,
    time_base: TimeBase::new(1, 48_000),
    duration: None,
    start_time: Some(0),
    params,
}];

let out: Box<dyn WriteSeek> = Box::new(std::fs::File::create("out.ogg")?);
let mut mux = containers.make_muxer("ogg", out, &streams)?;
mux.write_header()?;
// ... mux.write_packet(&pkt)? ...
mux.write_trailer()?;
# Ok::<(), Box<dyn std::error::Error>>(())
```

Per-stream header packets are reconstructed from each stream's
`extradata`: for Vorbis and Theora the 3-packet sequence is parsed
out of the Xiph-laced blob; for Opus the single `OpusHead` packet is
augmented with a minimal empty `OpusTags` comment block.

### Metadata

Vorbis-comment blocks (Vorbis packet #2, OpusTags, Theora comment
packet) are parsed during `open` and surfaced via
`Demuxer::metadata()` as lowercase `(key, value)` pairs plus a
`vendor` entry. Duration is estimated from the last page's granule
position translated to microseconds.

### Chained streams (RFC 3533 §4)

A *chained* Ogg file is the back-to-back concatenation of independent
logical bitstreams: each link starts with its own BOS page(s) and ends
with an EOS-flagged page. The demuxer registers every mid-file BOS as
a new logical stream (so packets from subsequent links aren't silently
dropped) and assigns each link a sequential `link_index` (the initial
BOS section is link 0; each subsequent BOS-after-non-BOS increments
the counter).

When `build_seek_index` runs, it parses each chained link's
identification packet on the fly to learn the link's codec + sample
rate, then recomputes total `duration_micros` as the **sum** of
per-link durations. Multiplexed (single-link) files keep their
previous max-over-streams duration semantics. So a chained file
containing two 60 s Vorbis songs reports 120 s, while a multiplexed
file with a 60 s Vorbis audio track + 60 s Theora video track still
reports 60 s.

For external tooling that needs to reconstruct the link partitioning
itself, `OggDemuxer` exposes three accessors alongside the existing
`hole_count` / `framing_error_count` / `resync_count` diagnostics:
`link_count() -> u32` returns the number of distinct chained links
seen so far (`1` for a single-link file, growing as new BOS-after-non-
BOS pages are observed); `stream_link_index(stream_index) -> Option<u32>`
returns which link a given public stream belongs to; and
`stream_serial(stream_index) -> Option<u32>` returns the raw on-wire
`bitstream_serial_number` (RFC 3533 §6 field 5) for callers that need
to correlate the dense `StreamInfo::index` enumeration with the
page-header serials a byte-level scanner observes.

### Skeleton metadata bitstream (Xiph Skeleton 3.0 / 4.0)

Ogg files often carry an **Ogg Skeleton** logical bitstream as their
very first BOS — a metadata stream that describes the *other* logical
bitstreams in the same physical stream (per-track MIME type, role,
name, granule rate, preroll, granuleshift, basetime, presentation
time, and — in version 4.0 — a keyframe index). Skeleton itself
carries no content packets; its packets all live in the header pages
and its EOS empty packet closes the control section before any
content pages appear.

`oxideav_ogg::skeleton` provides decode + encode for all three packet
types — `fishead\0` ident header, `fisbone\0` per-track secondary
header, and (4.0 only) `index\0` keyframe-index packet — plus the
Skeleton 4.0 variable-byte integer codec used by index deltas. The
demuxer auto-detects a `fishead\0` BOS, parses the header, routes
subsequent fisbone / index packets through Skeleton's reassembly path,
and surfaces the aggregate state via `OggDemuxer::skeleton()`:

```rust
use oxideav_core::{NullCodecResolver, ReadSeek};

let input: Box<dyn ReadSeek> = Box::new(
    std::io::Cursor::new(std::fs::read("multi.ogv")?),
);
let codecs = NullCodecResolver;
let dmx = oxideav_ogg::demux::open_concrete(input, &codecs)?;
if let Some(sk) = dmx.skeleton() {
    // Skeleton present — Vorbis/Theora/etc. tracks each have a fisbone
    // describing them, looked up by their own on-wire serial number.
    for bone in &sk.bones {
        let content_type = bone.header("Content-Type").unwrap_or("?");
        let role = bone.header("Role").unwrap_or("?");
        println!("serial {:08x}  {}  role={}", bone.serial, content_type, role);
    }
    // 4.0 streams may additionally carry a per-track keyframe index.
    for idx in &sk.indexes {
        println!("serial {:08x}  keypoints={}", idx.serial, idx.keypoints.len());
    }
}
# Ok::<(), Box<dyn std::error::Error>>(())
```

The Skeleton stream is **not** exposed in `Demuxer::streams()` — it
has no content packets and exists only to describe the other streams.
Files without Skeleton behave exactly as before: `skeleton()` returns
`None` and no other behaviour changes.

#### Mux-side: attach a Skeleton at open time

`oxideav_ogg::mux::open_with_skeleton(output, streams, Some(skel))`
emits a Skeleton metadata bitstream alongside the content streams,
following the encapsulation order spelled out in
`docs/container/ogg/ogg-skeleton-3.0.md` /
`docs/container/ogg/ogg-skeleton-4.0.md`:

1. The Skeleton `fishead\0` BOS is the very first BOS page of the
   physical stream so a decoder can identify Skeleton straight away
   without having to look past Vorbis / Theora / Opus magic first.
2. The BOS pages of all other logical bitstreams follow.
3. Each `fisbone\0` secondary header rides on its own page, alongside
   the content codecs' remaining secondary header packets.
4. Any 4.0 `index\0` packets ride alongside the fisbones, one per page.
5. An empty-payload Skeleton EOS page closes the control section
   before the first content data page is written.

`Skeleton::serial` controls which serial the muxer uses for the
Skeleton bitstream; leaving it `None` lets the muxer pick one past the
largest content-stream serial (so it cannot collide). The
`open` factory continues to produce Skeleton-free output byte-for-byte
by delegating to `open_with_skeleton(_, _, None)`.

For encode-side use, every type round-trips through `to_bytes` /
`parse`:

- `FisHead::to_bytes` emits a 64-byte 3.0 layout or an 80-byte 4.0
  layout based on `self.version` (the 4.0 additions are the
  *Segment length in bytes* and *Content byte offset* fields at
  bytes 64..80, used by players to validate the index and to bound
  chained-segment seeking).
- `FisBone::to_bytes` emits the 52-byte fixed prefix followed by
  CRLF-delimited HTTP-style message header fields. `set_header` /
  `header` provide case-insensitive lookup for the spec's compulsory
  4.0 fields (`Content-Type`, `Role`, `Name`) plus the larger field
  registry in `docs/container/ogg/ogg-skeleton-message-headers.wiki`.
- **Typed message-header accessors** parse three of those wiki-documented
  fields into structured values:
  `FisBone::role()` returns an `Option<Role>` whose `kind` is one of
  the 24 enumerated `RoleKind` variants for `text/* | video/* | audio/*`
  tracks (forward-compatible / vendor tags round-trip as
  `RoleKind::Other(String)`); the wiki's documented parameter form
  `video/alternate;angle=nw` is split into `Role::parameters` and
  looked up case-insensitively via `Role::parameter("angle")`.
  `FisBone::languages()` returns an `Option<Vec<&str>>` of trimmed
  BCP-47-shaped tags split on `,` per the wiki's `Language: en-US, fr`
  example, with the dominating language first and empty fragments
  dropped.
  `FisBone::altitude()` returns an `Option<Result<i64>>` for the
  CSS-z-index-style stack-order field documented in
  `docs/container/ogg/ogg-skeleton-message-headers.wiki` §Altitude
  ("Altitude: -150" worked example, "unlimited negative and positive
  numbers" wording): the outer `Option` distinguishes "header absent"
  from "header present", the inner `Result` surfaces a parse error for
  malformed / non-integer / out-of-`i64`-range values so the caller
  can decide whether to skip the field or reject the packet. Higher
  altitude values render in front of lower ones per the wiki.
  `FisBone::display_hint()` returns an `Option<Result<DisplayHint>>`
  for the parametric rendering-hint field documented in
  `docs/container/ogg/ogg-skeleton-message-headers.wiki` §Display-hint.
  The wiki enumerates three documented hint forms — `pip(x,y[,w,h])`
  (picture-in-picture, with the 2-arg `pip(20%,20%)` and 4-arg
  `pip(40,40,690,60)` worked examples), `mask(img[,x,y[,w,h]])` (video
  mask with URL plus optional placement coordinates), and
  `transparent(p%)` (uniform 0..=100 transparency) — and the
  accessor parses each into a structured [`DisplayHint`] variant.
  Coordinates carry their pixel-vs-percent distinction via
  [`DisplayCoord`] (the wiki's "x, y, w, and h can be specified in
  percentage" clause). Forward-compatible / vendor hint tags map to
  `DisplayHint::Other { tag, arguments }` per the wiki's
  "Currently proposed hints are:" soft-enumeration wording. The outer
  `Option` distinguishes "header absent" from "header present"; the
  inner `Result` surfaces parse errors (missing parentheses, wrong
  argument count for a documented tag, non-numeric coordinate, or a
  `transparent` percent outside `0..=100`) so the caller can decide
  whether to skip the field or reject the packet.
- `SkelIndex::to_bytes` re-deltifies keypoint offsets and timestamps
  relative to the previous entry and emits each as a Skeleton 4.0
  variable-byte integer (7 bits per byte, high bit set on the
  terminator, little-endian). `SkelIndex::parse` reverses both layers.
- **Time-domain typed accessors** convert the on-wire numerator-space
  integers in an `index\0` packet into seconds and provide spec-aligned
  time-keyed lookup:
  `KeyPoint::seconds(timestamp_denominator)` for one keypoint;
  `SkelIndex::keypoint_seconds(i)`,
  `SkelIndex::first_sample_seconds()`,
  `SkelIndex::last_sample_seconds()`, and
  `SkelIndex::duration_seconds()` for the indexed-segment endpoints
  (each returning `Option<f64>` so the spec's "denominator 0 means
  unknown" rule from
  `docs/container/ogg/ogg-skeleton-4.0.md` §"Keyframe index packets"
  surfaces as `None` rather than NaN). `SkelIndex::is_sorted_by_offset()`
  validates the spec's increasing-offset invariant; and
  `SkelIndex::keypoint_for_time(seconds)` is an `O(log n)` binary search
  that returns the index of the last keypoint with presentation time
  `<= target_seconds`. That answer is the per-stream half of the seek
  algorithm in §"Keyframe indexes for faster seeking" ("first construct
  the set which contains every active streams' last keypoint which has
  time less than or equal to the seek target time"); the caller then
  takes the minimum byte-offset across all per-stream answers. The
  search runs in pure-integer numerator space so floating-point rounding
  around boundary timestamps cannot mis-classify the target; negative
  timestamps (streams whose `presentation_time` precedes granule 0) are
  handled with sign preserved.
- `oxideav_ogg::skeleton::{read_vbi_u64, write_vbi_u64}` are exposed
  publicly so callers writing seek-tooling against raw `index\0`
  packets don't have to re-implement the encoding.

When a Skeleton 4.0 `index\0` packet is present for the requested
stream, [`Demuxer::seek_to`] skips both the page-level bisection scan
and even the [`OggDemuxer::build_seek_index`] full-file scan: the
target pts is rescaled into the index's `timestamp_denominator` units
via [`TimeBase::rescale`], the keypoint table is binary-searched for
the largest timestamp at or below the target, and the demuxer jumps
straight to that keypoint's byte offset. Fast-path firings are counted
on `OggDemuxer::skeleton_index_seek_count()`. Files without a
Skeleton index — Skeleton 3.0 streams, 4.0 streams that omit the
index, or seeks against a stream whose serial is uncovered — fall
back to the existing bisection path unchanged.

Before the fast path commits to a keypoint, the demuxer runs the
three validity checks the Skeleton 4.0 spec requires (per
`docs/container/ogg/ogg-skeleton-4.0.md` §"Keyframe indexes for
faster seeking"):

1. the `fishead` BOS packet's *Segment length in bytes* field equals
   the actual file size (a one-shot lazy check on the first seek;
   encoders that left this field at `0` opt out, which is the
   prevailing pattern);
2. the keypoint's stored byte offset starts an `OggS` capture
   pattern (i.e. it lands on a page boundary, not mid-payload);
3. the page at that offset has `bitstream_serial_number` equal to
   the keypoint's stream serial.

A failed check silently disables the fast path for the call and
falls through to the existing page-level `index_floor` / bisection
seek — the seek itself still completes correctly, just paying the
slower I/O cost. The number of rejections (per spec: "you must
gracefully fall-back to a bisection search or other seek algorithm
when the index is not present, or when it is invalid") is exposed
on `OggDemuxer::skeleton_index_invalid_count()` so callers can
surface "this file's Skeleton index is stale" diagnostics without
losing the seek.

When a multi-stream Ogg carries a Skeleton index for more than one
concurrent stream (e.g. a Theora video track + a Vorbis audio
track), the fast path implements the per-spec multi-stream
minimisation: "first construct the set which contains every active
streams' last keypoint which has time less than or equal to the
seek target time. This tells you a known point on every stream
which lies before the seek target. Then from that set of key
points, select the key point with the smallest byte offset." The
demuxer anchors the lookup on the requested stream's index (which
fixes the returned granule via the requested stream's own
time-base), then iterates every *other* index in the Skeleton
state, rescales the target into that index's `timestamp_denominator`
units, and tracks the smallest byte offset among the floor
keypoints. Landing on the smallest offset guarantees decoding can
resume cleanly for every multiplexed stream — a naive lookup that
consulted only the requested stream's index would land past
another concurrent stream's required keyframe, leaving its decoder
unable to recover. The per-keypoint validity check (#3 above) is
performed against the *winning* stream's serial — the page at the
chosen offset must belong to that stream, not necessarily the
originally-requested one.

Spec reference: `docs/container/ogg/ogg-skeleton-3.0.md`,
`docs/container/ogg/ogg-skeleton-4.0.md`,
`docs/container/ogg/ogg-skeleton-message-headers.wiki`. The 4.0 page
recommends emitting 4.0 in preference to 3.0 when possible, and notes
that decoders must always fall back to bisection when the index is
absent or fails validation (length / page-boundary checks).

### Page-loss detection (RFC 3533 §6)

Every Ogg page header carries a `page_sequence_number` that "is
increasing on each logical bitstream separately" so "the decoder can
identify page loss" (RFC 3533 §6 field 6). The demuxer tracks each
logical stream's expected next sequence number and detects a *hole*
whenever a consumed page's sequence number is not exactly
`previous + 1` (the counter is a wrapping `u32`; BOS pages legitimately
restart it). A single discontinuity counts as one hole however many
pages went missing.

A hole is not papered over: if a packet was mid-reassembly across page
boundaries, its buffered partial bytes are discarded, and the orphaned
continuation fragment on the page after the gap (a packet tail whose
head was lost) is dropped rather than spliced into a corrupt packet.
Packets that are fully present after the hole are still delivered, so
every packet the demuxer hands downstream stays individually
well-formed. `OggDemuxer::hole_count()` exposes the running tally (0 for
a clean file) for diagnostics; the count reflects pages consumed via
`next_packet`, not the header-only `build_seek_index` scan.

### Continued-flag framing consistency (RFC 3533 §6 field 3)

The `header_type` byte's bit `0x01` is the `continued` flag: "set: page
contains data of a packet continued from the previous page; unset: page
contains a fresh packet." It is a normative declaration about reassembly,
so the demuxer cross-checks it against its own pending-packet state on
every page and treats a disagreement as a framing error — *independent*
of any `page_sequence_number` gap. Two cases are caught:

- the bit is **set** but no partial packet is buffered (the head either
  never arrived or already terminated) — the leading segment is an
  orphaned continuation tail and is dropped, not spliced;
- the bit is **unset** but a partial packet *is* buffered (the previous
  page ended on a 255-lacing segment, promising a continuation) — this
  page abandons the partial by declaring a fresh packet, so the orphaned
  head is dropped.

This surfaces corruption *within* an otherwise sequence-consistent page
run (e.g. a damaged final segment that flipped a lacing terminator) that
the page-loss counter cannot see. `OggDemuxer::framing_error_count()`
exposes the tally (0 for a clean file); it is disjoint from
`hole_count()` — a discontinuity already charged to a page-loss hole on
the same page is not double-counted as a framing error.

### Page-sync recapture (RFC 3533 §3, §6 field 1)

RFC 3533 §3 lists "recapture after a parsing error" as a core design
requirement of Ogg, and §6 field 1 (`capture_pattern`) spells out how the
`OggS` magic enables it: "It helps a decoder to find the page boundaries
and regain synchronisation after parsing a corrupted stream. Once the
capture pattern is found, the decoder verifies page sync and integrity
by computing and comparing the checksum." Two failure modes trigger
recapture rather than aborting the stream:

- **Garbage spliced between pages** — the bytes at the next page
  boundary do not start with `OggS`. The demuxer rewinds to the start
  of the bad read and scans forward byte-by-byte for the next valid
  page.
- **Checksum mismatch** — the bytes start with `OggS` (so the apparent
  page header parsed) but the CRC32 over the assembled page does not
  verify. The demuxer steps one byte past the bad capture (so it does
  not re-lock onto the same garbage) and resumes the same forward
  scan. False-positive captures sitting inside other pages' payloads
  are weeded out by their own CRC failures.

The scan keeps walking until it finds an `OggS` whose full page (header
+ segment table + body) re-parses cleanly with a matching checksum, then
resumes normal demux from there. Embedded `OggS` byte sequences inside
intact packet payloads are never seen by the resync scanner because
normal page-by-page reading is driven by the previous page's length —
the scanner only runs when a parse has already failed.

`OggDemuxer::resync_count()` exposes the running tally of successful
recoveries (0 for a clean file). Each recovery counts as one resync
regardless of how many bytes had to be skipped. The counter is distinct
from `hole_count()`: byte-level corruption that destroys whole pages
ticks both (the resync for the corruption, the hole for the missing
page-sequence number); garbage that sits *between* page boundaries
ticks only the resync counter because no `page_sequence_number` was
lost.

### Standalone page CRC validation (RFC 3533 §6 field 7)

`oxideav_ogg::crc` exposes a small byte-slice API for verifying any
single Ogg page's stored checksum without paying for full packet
reassembly:

- `validate_page_crc(page_bytes) -> Option<bool>` — `Some(true)` if
  the stored CRC matches, `Some(false)` if not, `None` if the slice
  is shorter than the 26 bytes needed to even reach the CRC field.
- `compute_page_checksum(page_bytes) -> Option<u32>` — recomputes
  the CRC over the full page with bytes 22..26 treated as zero, per
  RFC 3533 §6 field 7 ("a 32 bit CRC checksum of the page including
  header with zero CRC field and page content; the generator polynomial
  is 0x04c11db7").
- `read_page_checksum(page_bytes) -> Option<u32>` — extracts the
  little-endian u32 stored in the page's CRC field.
- Constants `CRC_FIELD_OFFSET = 22` and `CRC_FIELD_LEN = 4` for callers
  that want to inspect the field directly.

This is the same polynomial and zero-field convention `Page::parse`
already uses internally for its mandatory CRC check; the standalone
helpers are convenient for stream-scanner tools that walk pages but
do not need the segment table decoded into packets.

### Fuzzing

A cargo-fuzz harness under `fuzz/` (panic-freedom only, no
cross-decoder oracle — the clean-room wall holds at the spec
and our own source) hammers five surfaces with attacker bytes:

- `page_parse` — `Page::parse` at every byte offset, plus the
  standalone `crc::validate_page_crc` / `read_page_checksum` /
  `compute_page_checksum` helpers and the `page::lace` segment-table
  builder. Every `Ok` parse is checked against its serializer for
  inverse-pair byte equality.
- `demux_recapture` — `demux::open` + `Demuxer::next_packet` end to
  end, exercising RFC 3533 §3 / §6 field 1 capture-pattern resync,
  §6 field 3 continued-flag framing-consistency, and §6 field 6
  page-loss detection. The `hole_count` / `framing_error_count` /
  `resync_count` accessors are queried after the drain.
- `granule_walk` — `demux::open_concrete` + `build_seek_index` +
  `seek_to` at fuzz-derived granule values across every reported
  stream, covering both the dense index lookup and the bisection
  fallback's byte scanner.
- `continued_edge` — the per-stream packet-reassembly machinery
  (RFC 3533 §6 field 3 continued-flag cross-check, 255-lacing
  partial-packet buffering, `pending_valid` orphan-drop, §6 field 6
  page-loss hole accounting) is hard to reach with totally random
  bytes because the BOS walk rejects most of them. This target
  **constructs** a valid Vorbis BOS + comment + setup header
  section then synthesises N body pages with attacker-driven
  lacing patterns (including the exact-multiple-of-255 boundary,
  continuation-without-terminator, segment-table truncation),
  attacker-driven `continued` / `first` / `last` flag bits,
  attacker-driven page-sequence-number deltas (zero = duplicate,
  large = fabricated hole), and an optional single-byte global
  mutation that triggers CRC-failure resync. The reassembly path
  is therefore reached on essentially every iteration.
- `skeleton_parse` — the four other targets virtually never reach
  the Skeleton packet parsers because random fuzz buffers almost
  never begin with `fishead\0` / `fisbone\0` / `index\0`. This
  target calls [`skeleton::FisHead::parse`],
  [`skeleton::FisBone::parse`] and [`skeleton::SkelIndex::parse`]
  directly on the fuzz buffer (asserting inverse-pair equality
  with `to_bytes` on every successful parse), roundtrips the
  variable-byte integer codec
  ([`skeleton::write_vbi_u64`] → [`skeleton::read_vbi_u64`]) on
  fuzz-derived `u64`s, and also wraps the buffer in a synthetic
  Skeleton BOS page handed to [`demux::open_concrete`] so the
  demuxer's auto-detect aggregation (`OggDemuxer::skeleton()`)
  fires too. The 42-byte `index\0` packet whose on-wire
  `n_keypoints` field declares a 4-billion-entry table is the
  prototype case the parser was hardened against: capacity is
  now clamped by `(packet.len() - 42) / 2` (the minimum two
  bytes per delta-encoded keypoint) so a tiny attacker packet
  cannot pre-allocate gigabytes.

Run from `fuzz/` with `cargo +nightly fuzz run <target>`; no target
runs as part of the per-PR CI shim (the org reusable workflow does
not invoke `cargo fuzz`), so the harness is a long-running offline
hardening tool rather than a gate.

### Benchmarks

A Criterion harness at `benches/framing.rs` measures the framing
hot paths so future optimisation rounds can A/B-test their changes.
Everything is self-contained — every byte fed into a measured
routine is synthesised in-bench (via `Page::to_bytes` for raw page
scenarios, via the muxer for the end-to-end demux scenarios), so
no `docs/` fixtures or external `.ogg` files are read. Scenarios:

- `crc/checksum/{64,4096,65536}` — the raw `crc::checksum` table-
  lookup loop with byte-throughput reporting.
- `crc/validate_page_crc/{short,max}` — the RFC 3533 §6 field 7
  standalone helper over a single-segment short page and the max-
  size 255×255 page (~65 KiB).
- `page/parse/{short,multi_segment,max}` and
  `page/to_bytes/{short,multi_segment,max}` — the parse ↔ serialize
  pair at the three legal-extreme sizes. `parse` validates the CRC
  by streaming through `crc::compute_page_checksum` over the
  borrowed page bytes (with the field at offset 22..26 treated as
  zero per RFC 3533 §6 field 7) rather than cloning the page into a
  scratch `Vec<u8>` so the four CRC bytes can be filled with zeros
  before re-checksumming; the allocation+memcpy was on the hot path
  of every `next_packet`, and for a max-size 65 KiB page it was a
  full second copy of the page body. r172 took `page/parse/max`
  from ~411 MiB/s to ~493 MiB/s, `page/parse/multi_segment` from
  ~426 MiB/s to ~488 MiB/s, and `page/parse/short` from ~416 MiB/s
  to ~489 MiB/s. r192 then replaced the byte-at-a-time CRC loop
  with a **slice-by-4** advancement that consumes four input bytes
  per iteration through four pre-shifted tables (the underlying
  generator polynomial 0x04C11DB7 is unchanged; tables `T1..T3` are
  each derived from `T0` by one extra zero-byte rank in the same
  recurrence the scalar loop already used), and replaced the
  per-byte `(22..26).contains(&i)` range check inside
  `compute_page_checksum` with a straight-line three-segment split
  (`[..22]`, four-zero CRC-field substitute, `[26..]`). Combined,
  r192 takes `page/parse/max` from ~493 MiB/s to ~1.2-1.4 GiB/s
  (~2.5-3× on top of r172), `page/parse/multi_segment` to
  ~1.2 GiB/s, and `page/parse/short` to ~1.3 GiB/s. The recurrence
  and the
  rank-table derivations are pinned in unit tests against a
  verbatim scalar oracle on lengths 0..65 535 so a future tweak that
  miscomputes a table is caught at the lib-test stage.
- `page/lace/{short,exact_255,large}` — the segment-table builder,
  with the exact-multiple-of-255 zero-terminator branch covered.
- `demux/walk/vorbis_12pkt` — open + drain a 12-packet synthetic
  Vorbis stream end-to-end via `next_packet`.
- `demux/build_index/vorbis_12pkt` — the page-header-only scan
  that powers O(log n) `seek_to`.

Run with `cargo bench -p oxideav-ogg --bench framing`. Like the
cargo-fuzz harness, this is an offline tool — the per-PR CI shim
does not invoke `cargo bench`.

## License

MIT — see [LICENSE](LICENSE).
