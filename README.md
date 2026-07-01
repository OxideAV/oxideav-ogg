# oxideav-ogg

Pure-Rust **Ogg** container (RFC 3533) — page framing, CRC32
checksumming, packet reassembly across page boundaries (including
multi-page packets and 'nil' pages), the full §4 grouping + chaining
topology, codec sniffing, metadata, and a muxer that emits compliant
Ogg for Vorbis, Opus, Theora, FLAC and Speex. Zero C dependencies.

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

For Vorbis, Opus, **Speex** and **FLAC** the demuxer parses the
identification header during `open` to populate the stream's
`sample_rate` / `channels` (and the nominal `bit_rate` where the header
carries one). The Speex header is the fixed 80-byte little-endian struct
of the Speex manual §7.3 / table 7.1
(`docs/audio/speex/speex-manual.pdf`): `rate` (offset 36),
`nb_channels` (offset 48, clamped to mono/stereo since Speex supports
only those), and `bitrate` (offset 52, with the encoder's `-1`
"unknown" sentinel suppressed). The FLAC parameters come from the
STREAMINFO block embedded in the `0x7F "FLAC"` mapping packet (RFC 9639
§8.2 / §10.1, `docs/audio/flac/rfc9639-flac.pdf`): the bit-packed
`u(20)` sample rate + `u(3)` (channels − 1) at STREAMINFO offset 10.
Because the Speex and FLAC granule is a *sample count* just like
Vorbis ("the granulepos is the number of the last sample encoded in
that packet" — Speex manual §7.3; "the number of the last sample" —
RFC 9639 §10.1), each stream is now stamped with a `1/sample_rate`
time-base rather than the `1/1_000_000` placeholder, so duration
estimates and seek targets translate the granule correctly instead of
mis-scaling it by the sample rate.

Each codec has a fixed number of header packets the demuxer absorbs
before delivering content packets (Vorbis 3, Opus 2, Theora 3, Speex
2). **FLAC** is the one mapping that declares its header-packet count
in-band: per RFC 9639 §10.1 (`docs/audio/flac/rfc9639-flac.pdf`) the
mapping packet's bytes 7..9 hold a big-endian "number of header packets
(excluding the first)", so the total is `1 + that count`. The demuxer
reads it and absorbs every metadata block (STREAMINFO, Vorbis comment,
padding, …) as a header rather than mis-delivering it as audio; a
declared `0` ("unknown") falls back to absorbing just the mapping
packet. The FLAC Vorbis-comment block (FLAC §8.1 block type 4) is then
parsed into `Demuxer::metadata()` like the other codecs' comment
packets.

### Multi-stream

Multiplexed Ogg (e.g., Theora video + Vorbis audio in the same `.ogv`)
is supported end-to-end: every BOS page yields its own `StreamInfo`,
packets are reassembled per-stream across interleaved pages, and the
muxer emits BOS pages for every stream before any non-BOS page as
required by RFC 3533 §6.

### Seeking

`seek_to(stream_index, pts)` performs a bounded bisection over the
file using granule-position timestamps on Ogg pages. Vorbis, FLAC
and Speex land on the greatest page whose granule is at or below the
target.

**Opus** is the same axis with a per-stream bias. Its `pts` is a
*PCM sample position* (playback time), but a page's on-wire granule
counts `PCM position + pre-skip` (`docs/audio/opus/rfc7845-ogg-opus.txt`
§4.3: "PCM sample position = granule position − pre-skip"), so the
comparison key is `granule − pre-skip` and the floor lands on the page
whose PCM position is at or below the target rather than `pre-skip /
48000` s early. The pre-skip is read once from the `OpusHead` ID
header (bytes 10..12, LE u16, §5.1 field 4) at open time and surfaced
via `OggDemuxer::opus_pre_skip(stream_index) -> Option<u16>` so a
downstream Opus decoder can discard the same leading samples it was
told to. The same bias is folded into the duration estimate (the
last-page granule converts to `(granule − pre-skip) / 48000` seconds),
so an Opus stream no longer over-reports its length by the pre-skip.
The seek's returned granule is still the landed page's *raw* on-wire
value.

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

#### Preroll-aware seek

`docs/container/ogg/ogg-skeleton-4.0.md` §"How to describe the logical
bitstreams within an Ogg container?" defines a per-track **preroll**:
"the number of past content packets to take into account when decoding
the current Ogg page, which is necessary for seeking (vorbis has
generally 2, speex 3)". A bare `seek_to` lands the input on the page
whose granule floors the target, but a codec with inter-packet state
(window overlap, prediction) produces wrong output for the first
packets if it resumes exactly there — it is missing the preroll
warm-up packets. `OggDemuxer::seek_to_with_preroll(stream_index, pts)`
runs the same landing as `seek_to`, then rewinds the resume byte
offset to an earlier page boundary so that at least `preroll` content
packets of the requested stream precede the landed page. The preroll
count comes from the stream's Skeleton `fisbone\0` (looked up by its
on-wire serial); the codec's `num_headers` identification / comment /
setup packets are excluded from the count so only *content* packets
warm the decoder. The returned granule is identical to `seek_to`'s —
the decode target is unchanged; the earlier pages are warm-up the
caller decodes and discards until it reaches the target. With no
Skeleton, no fisbone for the stream, a `preroll` of 0, or a landing
already at the stream's first content page, the call is byte-for-byte
identical to `seek_to`. `OggDemuxer::preroll_seek_count()` tallies the
calls that actually backed the offset up; `OggDemuxer::input_position()`
exposes the resume byte offset for callers that want to compare the two
seek variants.

#### Keyframe-aware seek

Preroll handles inter-*packet* warm-up (audio window overlap); a Theora
track needs inter-*frame* warm-up — it cannot begin decoding at an arbitrary
inter-frame and must resume from the last keyframe at or before the target.
The granule packing already encodes that keyframe: the high bits are the
keyframe index, the low `granuleshift` bits the offset since it
(`docs/container/ogg/ogg-skeleton-4.0.md`).
`OggDemuxer::seek_to_keyframe(stream_index, pts)` runs the normal `seek_to`,
reads the landed page's keyframe index, and — when the landing isn't already
a keyframe — re-seeks to that keyframe's own frame so forward decoding starts
on an intra page. Unlike `seek_to_with_preroll`, the **returned granule
changes**: it is the keyframe page's on-wire granule (its offset half zero),
and the caller decodes forward, discarding frames until it reaches the
requested `pts`. A granuleshift-0 mapping (every audio codec — each packet is
already a random-access point) or a landing already on a keyframe makes the
call identical to `seek_to`.

### Per-packet timing & flags

Ogg's only timing signal is a page's `granulepos`, which RFC 3533 §6 pins to
"the last packet completed on that page". The demuxer therefore stamps the
**last packet finishing on a page** with that granule as its `pts`/`dts`;
earlier packets on the same page get `None` (a container-aware consumer that
needs intermediate timestamps derives them from codec-level knowledge, e.g.
Opus TOC parsing). The final packet on each page is also flagged
`PacketFlags::unit_boundary` so a re-muxer can recreate similar page
boundaries.

`PacketFlags::keyframe` follows the granuleshift packing the Skeleton 4.0
`fisbone\0` declares (`docs/container/ogg/ogg-skeleton-4.0.md`: the
granuleshift is "the number of lower bits from the granulepos field that are
used to provide position information for sub-seekable units (like the keyframe
shift in theora)"):

- **Audio mappings** (Vorbis / Opus / FLAC / Speex) declare granuleshift `0`
  — every packet is an independent random-access point, so every delivered
  content packet is a keyframe.
- **Theora** declares a non-zero keyframe shift. The granule splits into a
  keyframe index (high bits) and an offset-since-keyframe (low `shift` bits);
  the last-on-page packet is a keyframe exactly when that offset is zero. A
  non-granule-bearing packet on a shifted track cannot be proven a keyframe
  and is flagged `false` rather than mislabelled random-access.
- A Theora stream **with no fisbone** (granuleshift unknown, defaulting to 0)
  keeps the conservative all-keyframe flagging so random access is never
  under-reported.

This flag flows end-to-end into the muxer-built Skeleton 4.0 keyframe index
(`open_with_skeleton_indexed`, below), which records a keypoint per
keyframe-flagged page — so a demux→remux of a Theora track produces an index
of its *keyframes*, not one entry per frame.

`OggDemuxer::stream_granuleshift(stream_index)` surfaces the per-stream
granuleshift the keyframe decision is derived from (`Some(0)` for an audio
mapping or a stream with no fisbone, the declared shift for a Theora stream
with one), alongside `opus_pre_skip` / `stream_serial` / `stream_link_index`,
for callers that want to unpack a page's raw granule into its
`(keyframe_index, offset)` halves themselves.

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

A packet larger than one Ogg page — ≥ 65025 bytes laces to ≥ 256
segments and a page holds at most 255 (RFC 3533 §6 field 4) — is
**distributed over several pages** (§5). The muxer flushes any partial
page first, then fills successive pages 255 segments at a time; each
intermediate page ends on a 255-valued segment so the next page carries
the `continued` flag (§6 field 3), and the packet's terminator lands on
the final page. This mirrors the demuxer's multi-page reassembly, so a
large Vorbis setup codebook or Theora keyframe round-trips byte-exact
through mux → demux (`tests/large_packet_mux.rs`).

### Metadata

Vorbis-comment blocks (Vorbis packet #2, OpusTags, Theora comment
packet, the FLAC §8.1 type-4 block, and the **Speex** comment header)
are parsed during `open` and surfaced via `Demuxer::metadata()` as
lowercase `(key, value)` pairs plus a `vendor` entry. The Speex comment
header (the 2nd packet) is the one with no magic prefix: the Speex
manual §7.3 (`docs/audio/speex/speex-manual.pdf`) specifies it as the
bare Vorbis-comment structure, so the demuxer parses it directly rather
than skipping an identifier. Duration is estimated from the last page's
granule position translated to microseconds.

For a Vorbis / FLAC / Speex mapping the granule *is* a sample count and
the stream's time-base is the granule rate, so the last-page granule
converts to seconds directly. **Opus** is the same up to its pre-skip
bias: its granule counts `PCM position + pre-skip`, so the demuxer
subtracts the pre-skip (`(granule − pre-skip) / 48000` seconds) before
reporting duration — see the Seeking section above. **Theora is the
exception**: its `granulepos` is the packed `(keyframe_idx << shift) |
frame_offset` value (`docs/container/ogg/ogg-skeleton-4.0.md` §"What
decoding-related information is needed?"), not a frame count, and the
demuxer stamps Theora streams with the `1/1_000_000` placeholder
time-base because Ogg framing alone never reveals the frame rate.
Reading that raw granule as either microseconds or a frame count both
mis-report the duration. When the file carries a Skeleton `fisbone\0`
for the stream, the duration estimate (both the `open`-time
end-of-file scan and the `build_seek_index` recompute) routes the
last-page granule through the fisbone's `granule_to_seconds`
(`extract_granules` to undo the keyframe shift, then `granules /
granulerate`), so a Theora track reports its real playback length. A
`granuleshift == 0` fisbone collapses the extraction to a pass-through,
so the same path stays correct for audio mappings that happen to carry
a fisbone; streams with no Skeleton (or an unusable granule rate) fall
back to the stream time-base unchanged.

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

#### Mixed grouping + chaining (the full §4 topology)

RFC 3533 §4 defines the most general legal physical bitstream as a
*chain of groups* of concurrently-multiplexed bitstreams ("It is
possible to consecutively chain groups of concurrently multiplexed
bitstreams. The groups, when unchained, MUST stand on their own as a
valid concurrently multiplexed bitstream"), with the worked example
`|*A*|*B*|*C*|A|A|C|B|…|#B#|#C#|*D*|D|…|#D#|` — link 0 grouping three
bitstreams A/B/C whose BOS pages all precede any data page and whose
EOS pages need not be contiguous, then link 1 chaining a fourth
bitstream D. The demuxer handles this end to end: every grouped stream
in a link shares that link's `link_index`; a new link's grouped BOS
pages (those before any of *its* data pages) likewise share one
`link_index` rather than splitting into extra links; interleaved data
pages reassemble per-serial without cross-contamination; a grouped
stream that ends (`#A#`) long before the rest of its group does not
trip link-boundary detection; and a seek into one stream of a grouped
link walks only that serial's pages, landing on its granule floor
unperturbed by the interleaved pages of the other grouped streams.
Chained duration sums per link with each grouped link contributing the
**max** over its concurrently-multiplexed streams (so
`max(A,B,C) + D`). `tests/chained_grouped.rs` pins all of this.

#### Mux-side: writing chained links

The write-side counterpart of the demuxer's chained-read path is
`oxideav_ogg::mux::open_concrete(output, streams) -> OggMuxer`, the
concrete muxer whose `begin_new_link(streams)` starts a new chain link.
(The object-safe `Muxer` trait can't express it — a new link takes a
fresh `&[StreamInfo]` — so the concrete type is the entry point, mirroring
the demuxer's `open_concrete`.) `begin_new_link` finalizes the current
link, draining and EOS-terminating every one of its logical bitstreams so
"the eos page of a given logical bitstream is immediately followed by the
bos page of the next" (RFC 3533 §4), then writes the new link's BOS +
secondary-header pages. Because the slice may hold more than one stream, a
new link can itself be a **group** of concurrently-multiplexed streams —
so the muxer emits the full §4 *chain-of-groups* topology, not just plain
back-to-back single-stream links.

Serials are tracked file-wide: any collision (later links reusing
`StreamInfo::index` `0`, the common case) is bumped to the next free value,
honouring the §4 "unique serial number within the scope of the physical
bitstream" MUST — a muxed chain demuxes back with `duplicate_serial_count()
== 0`. `OggMuxer::link_index()` returns the current link (matching the
demuxer's `stream_link_index` on read-back) and `stream_serial(index)`
the current link's on-wire serial. A new link may only begin after a
content data page (the demuxer keys link boundaries on BOS-after-non-BOS),
and chaining is mutually exclusive with an attached Skeleton (its control
section + trailer-time segment-length backfill describe a single link);
both are guarded with errors. `tests/chained_mux.rs` round-trips 2- and
3-link chains, a grouped-then-chained topology (`max(group) + link1`
duration), and global serial uniqueness through mux → demux.

#### 'Nil' pages and multi-page packets (§4 / §5)

A *nil page* (§4: "containing no content but simply a page header with
position information and the eos flag set") has
`number_page_segments = 0` — no segment table, no body. A nil EOS page
carries the stream's closing granulepos after the last data packet has
already terminated on an earlier page; the demuxer reads its granule
for the duration estimate yet delivers no spurious packet for it, and a
mid-stream nil page (granule `-1`, "no packets finish on this page") is
transparent to packet reassembly (`tests/nil_page.rs`).

A packet larger than a page is "distributed over several pages" (§5)
via 255-byte lacing chunks terminated by a value `< 255` (or `0` for an
exact multiple of 255, which may land alone on a fresh continued page).
`tests/multipage_packet.rs` asserts byte-exact reassembly of packets
spanning 2, 3, and 4 pages (including two-segment 510-byte continuing
pages and the exact-multiple-of-255 fresh-page zero-terminator) — the
path real >64 KB Vorbis setup and Theora keyframe packets exercise.
The lossy counterpart (a spanning packet whose middle page is dropped,
discarded rather than spliced) lives in `tests/page_loss.rs`.

#### Unique serial-number enforcement (§4)

RFC 3533 §4 makes serial uniqueness a normative **MUST** for both
topologies: "Each grouped logical bitstream MUST have a unique serial
number within the scope of the physical bitstream" and, identically,
"Each chained logical bitstream MUST have a unique serial number within
the scope of the physical bitstream." A conforming encoder never reuses a
`bitstream_serial_number`, so this never fires on a well-formed file.

A malformed file that does reuse a serial — two grouped streams sharing
one, or a chained link reusing a prior link's — used to silently merge:
the duplicate BOS fell through to the reassembly path, splicing the new
bitstream's packets onto the colliding stream's stale pending bytes and
reading its identification packet as content. The demuxer now detects the
collision and **restarts the serial in place**: it drops the prior
occupant's buffered partial packet, resets the page-sequence tracker (so
the restart's pages are not mis-read as page loss), re-arms header capture
from the duplicate BOS's own codec, and re-files the serial under the new
BOS's link index. Every packet then delivered for that serial still
belongs to a single bitstream — the most recent one to claim the serial —
so a downstream decoder never receives a frankenpacket assembled from two
streams. `OggDemuxer::duplicate_serial_count()` exposes the running tally
(0 for a conforming file). Detection fires on all three walkers (the
`open` BOS walk, the `next_packet` data path, and the `build_seek_index`
header scan); the header scan, which visits every page exactly once, is
the authoritative file-wide source, so the two walkers never double-count
the same collision. `tests/duplicate_serial.rs` pins the grouping
violation, the chaining violation, three-way reuse, the index-scan /
drain non-double-count, and the conforming-file zero case.

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

To build the `Skeleton` argument from the streams you're about to mux,
`Skeleton::from_streams(&streams, Version::V4_0)` derives a `fishead`
plus one `fisbone` per content stream — serial from the stream `index`
(matching the muxer's serial assignment), granule rate as the inverse of
the stream `time_base`, header-packet count from the codec, and
`Content-Type` for the two codecs the in-tree spec states verbatim
(`audio/vorbis`, `video/theora`). It is the write-side companion to the
demuxer's `skeleton()` read path: a Skeleton parsed on demux can be
rebuilt from `StreamInfo` and re-emitted. Per-track authoring fields the
Ogg framing cannot reveal (`granuleshift`, `preroll`, `Role`, `Name`, and
the MIME of un-mirrored codecs) are left at their defaults for the caller
to set — via the typed writers below — before muxing.

When the attached fishead is 4.0, `write_trailer` **backfills** the
*Segment length in bytes* and *Content byte offset* fields with the
measured values and rewrites the BOS page in place (same page length,
CRC recomputed per RFC 3533 §6 field 7): segment length is the
physical size of the finished segment — the value decoders compare
against the file to detect a stale index — and content byte offset is
the offset of the first non-header page, recorded as the control
section closes. The backfill is per-field and conservative: a value
the caller pre-set to non-zero passes through verbatim, only
`None`/`0` ("unknown") fields are filled in, and a 3.0 fishead (whose
64-byte layout has no such fields) is never touched. As a result the
demuxer's own Skeleton-index validity check #1 (below) passes in
enforcing mode — rather than via the `segment_length == 0` opt-out —
on files this muxer produces. The trailer-time patch also drains each
content stream's held-back header page before the Skeleton EOS is
written, so every content secondary-header page (e.g. the Vorbis
setup page) physically precedes the Skeleton EOS per the 4.0 spec's
§"Further restrictions" encapsulation order.

#### Mux-side: muxer-built keyframe indexes

`oxideav_ogg::mux::open_with_skeleton_indexed(output, streams, skel,
AutoIndexConfig::default())` makes the muxer build the Skeleton 4.0
`index\0` packet for each content stream itself, instead of requiring
the caller to know every keypoint up front. The 4.0 spec places index
packets in the segment's header pages ("all the keyframe indexes are
immediately available once the header packets have been read"), but a
keypoint's byte offset and the segment's first/last sample times are
only knowable after the content is written — so the muxer reserves a
fixed-size placeholder `index\0` page per stream at `write_header`
(between the fisbones and the Skeleton EOS, per §"Further
restrictions"), records a keypoint whenever a page carrying a
keyframe-flagged packet (`PacketFlags::keyframe`) hits the wire, and
rewrites each placeholder in place at `write_trailer` — same page
length, CRC recomputed per RFC 3533 §6 field 7, the same mechanism as
the fishead segment-length/content-byte-offset backfill above.
Keypoint timestamps are numerators over the stream time-base
denominator; the index's first/last-sample-time fields are filled
from the first/last content-packet pts. `AutoIndexConfig` carries the
spec's thinning recommendation ("at most one key point per every 64KB
of data, or every 1000ms, whichever is least frequent") as
`min_keypoint_byte_gap` / `min_keypoint_time_gap_ms` defaults plus a
`max_keypoints` reservation cap (`42 + 20·n` bytes per stream; a
partial index is explicitly allowed by the spec). Bytes past the
final encoded keypoint stay zero — they lie beyond the *n* keypoints
the layout defines, so readers never consume them. Streams whose
serial already carries a caller-supplied `SkelIndex` pass through
verbatim. The result feeds the demuxer's own fast-path `seek_to`
below end-to-end, with validity check #1 passing in enforcing mode.

For encode-side use, every type round-trips through `to_bytes` /
`parse`:

- `FisHead::to_bytes` emits a 64-byte 3.0 layout or an 80-byte 4.0
  layout based on `self.version` (the 4.0 additions are the
  *Segment length in bytes* and *Content byte offset* fields at
  bytes 64..80, used by players to validate the index and to bound
  chained-segment seeking).
- **Typed UTC accessor** for the `fishead` 20-byte UTC slot (bytes
  44..63), which `docs/container/ogg/ogg-skeleton-4.0.md` §"What
  decoding-related information is needed?" defines as the granule-0 →
  real-world-clock-time mapping ("allowing to remember e.g. the recording
  or broadcast time of some content"). `FisHead::utc_str()` returns the
  NUL/whitespace-stripped slot text (`Option<String>`, `None` for an empty
  slot) for callers that want a verbatim reading. `FisHead::utc_time()`
  parses the documented `YYYYMMDDTHHMMSS.sssZ` ISO-8601 *basic* convention
  into a structured [`Utc`] — `{ year, month, day, hour, minute, second,
  fraction }` — following the same three-way `Option<Result<…>>` contract
  as `content_type()` / `altitude()` / `display_hint()`: `None` (slot
  empty), `Some(Ok(utc))` (slot follows the convention), `Some(Err(_))`
  (non-empty but off-convention — the spec mandates the field's *meaning*,
  not a byte layout, so such a slot is surfaced through `utc_str()` rather
  than rejected). Fractional seconds round-trip verbatim (trailing zeros
  preserved) and a positive leap second (`:60`) is accepted per ISO 8601;
  `Utc::to_string_basic` re-emits the convention.
- `FisBone::to_bytes` emits the 52-byte fixed prefix followed by
  CRLF-delimited HTTP-style message header fields. `set_header` /
  `header` provide case-insensitive lookup for the spec's compulsory
  4.0 fields (`Content-Type`, `Role`, `Name`) plus the larger field
  registry in `docs/container/ogg/ogg-skeleton-message-headers.wiki`.
- **Typed message-header accessors** parse seven of those wiki-documented
  fields into structured values:
  `FisBone::content_type()` returns an `Option<Result<ContentType>>` for the
  only **mandatory** Skeleton-4 per-track field
  (`docs/container/ogg/ogg-skeleton-message-headers.wiki` §Content-type,
  worked-out as `"Content-Type: audio/vorbis"` in
  `docs/container/ogg/ogg-skeleton-4.0.md` §3): the MIME `type/subtype`
  pair is split into a `ContentTypeKind` (`Audio` / `Video` / `Text` /
  `Image` / `Application`, with unknown buckets surfaced as
  `ContentTypeKind::Other(String)`) plus a preserved `subtype` string
  and an RFC 2045 `;key=value` parameter list (so
  `audio/ogg;codecs=opus` round-trips with `parameter("codecs")`
  returning `Some("opus")`). Case-insensitive on bucket match,
  subtype compare, and parameter lookup per RFC 2045 § 5.1; the
  outer `Option` distinguishes "header absent" from "header present"
  and the inner `Result` surfaces malformed-MIME parse errors so the
  caller can decide whether to skip the field or reject the packet.
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
  `FisBone::title()` returns an `Option<Title>` for the free-text
  track-description field documented in
  `docs/container/ogg/ogg-skeleton-message-headers.wiki` §Title
  ("A free text field to provide a description of the track content.",
  worked example `Title: "the French audio track for the movie"`).
  The wiki's example shows the value wrapped in literal double quotes
  without prescribing whether they belong to the on-wire value or are
  a typographic convention; `Title::raw` returns the trimmed value
  verbatim (quotes preserved) for round-trip use, and `Title::display`
  strips a single balanced pair of surrounding `"…"` quotes when
  present so callers that follow the wiki-example reading get a
  quote-free string. Title is optional per the wiki (only
  `Content-Type` is mandatory), so the accessor returns
  `Option<Title>` rather than `Option<Result<Title>>` — every
  well-formed `Title:` header parses successfully because the field
  is unstructured by spec.
  `FisBone::name()` returns an `Option<Name>` for the stable
  per-track identifier documented in
  `docs/container/ogg/ogg-skeleton-message-headers.wiki` §Name
  ("This field provides the opportunity to associate a free text
  string with the track to allow direct addressing of the track
  through its name", worked example `track[name="Madonna_singing"]`).
  The wiki specifies an XML 1.0 `NCName`-shaped grammar verbatim
  for the allowed character set: `Name::raw` returns the trimmed
  value exactly as the header carries it (whitespace dropped — same
  HTTP-style framing tolerance as the other typed accessors) for
  round-trip use, and `Name::is_well_formed` returns the grammar
  check against the two §Name allow-lists (first-character set and
  following-character set). Callers that want to surface the value
  to a `track[name=…]` resolver gate on `is_well_formed` before
  publishing the name. The wiki's per-stream uniqueness rule ("The
  name needs to be unique between all the track names, otherwise it
  is undefined which of the tracks is retrieved when addressing by
  name") is a file-level invariant enforced by callers via
  `Skeleton::bone_for_serial`, not inside this per-value parser.
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

#### Granulepos → playback time

`docs/container/ogg/ogg-skeleton-4.0.md` §"What decoding-related
information is needed?" defines a two-step mapping from a content page's
raw on-wire `granulepos` to a playback time: "the granulepos of a data
page must first be parsed to extract a granule value … This value can
then be mapped to time by calculating `granules / granulerate`." Three
accessors implement it:

- `FisBone::extract_granules(granulepos) -> i64` undoes this track's
  granuleshift packing — "the number of lower bits from the granulepos
  field that are used to provide position information for sub-seekable
  units (like the keyframe shift in theora)". For a `granuleshift == 0`
  track (Vorbis / Opus / FLAC / Speex — every audio mapping) the
  granulepos *is* the granule value and passes through unchanged; for a
  Theora-style packed granulepos the high bits hold the last keyframe
  index and the low `granuleshift` bits the offset since it, so the
  absolute granule value is `(g >> shift) + (g & ((1 << shift) - 1))`.
  The RFC 3533 §6 `-1` "no packets finish on this page" sentinel passes
  through verbatim and a degenerate `granuleshift >= 63` yields `0`
  rather than overflowing the mask.
- `FisBone::granule_to_seconds(granulepos) -> Option<f64>` is the
  per-track value: `extract_granules` then a divide by the fisbone's
  `granule_rate` rational (Hz for audio, fps for video). Returns `None`
  for the `-1` sentinel or an unusable (non-positive numerator /
  denominator) rate so the spec's zero-denominator "unknown" convention
  surfaces as `None` rather than a NaN or a negative time. The value is
  relative to granule 0 and excludes the fishead basetime.
- `Skeleton::granule_to_seconds(serial, granulepos) -> Option<f64>` is
  the **absolute** mapping: it looks up the fisbone for `serial`, takes
  its per-track seconds, and adds the fishead's **basetime** — which
  "provides a mapping for granule position 0 (for all logical
  bitstreams) to a playback time" (the spec's pro-video "starts at
  01:00:00" case). Basetime is a per-file rational shared by every
  logical bitstream, so it is added once on top; an unknown
  (denominator-0) or absent basetime contributes a `0.0` offset rather
  than blocking the mapping. Returns `None` when no fisbone describes
  `serial` or the per-track mapping is `None`.

#### Substream / cut-in time mapping

`docs/container/ogg/ogg-skeleton-4.0.md` §"How to allow the creation of
substreams from an Ogg physical bitstream?" describes how a subpart cut
out of a larger Ogg file (the spec's `?t=7-59` Web cut) keeps its content
pages — "including the framing and granule positions" — byte-for-byte
intact, and records two extra fields so a player can reconstruct the
*original* timeline rather than restarting at 0: the fisbone's
**basegranule** ("the granule number with which this logical bitstream
starts in the remuxed stream … provides … the accurate start time of its
data stream") and the fishead's **presentation time** ("the actual cut-in
time and all logical bitstreams are meant to start presenting from this
time onwards"). Both were already parsed and round-tripped; five
accessors now consume them:

- `FisBone::start_seconds() -> Option<f64>` — the per-track data start
  time `basegranule / granulerate`. The basegranule names a granule
  *number*, not an on-wire `granulepos`, so no granuleshift extraction is
  applied; a negative basegranule (kept data preceding granule 0) keeps
  its sign; an unusable rate yields `None`.
- `FisBone::granule_to_seconds_since_start(granulepos) -> Option<f64>` —
  a page's elapsed time within the kept segment,
  `(extract_granules(granulepos) - basegranule) / granulerate`. For an
  un-cut stream (basegranule 0) this equals `granule_to_seconds`; a page
  whose granule precedes the basegranule (a surviving preroll page) maps
  to a negative elapsed time. `None` on the `-1` sentinel / unusable rate.
- `Skeleton::presentation_seconds() -> Option<f64>` — the fishead cut-in
  time. `None` when no fishead has been recorded (the cut-in is then
  unknown); a zero-denominator presentation time is the un-cut default of
  `0.0`.
- `Skeleton::presentation_seconds_checked() -> Option<f64>` /
  `Skeleton::basetime_seconds() -> Option<f64>` — the same two fishead
  anchors with the module's three-way "unknown vs. zero" contract:
  `Rational::to_seconds_checked()` returns `None` for the spec's
  zero-denominator "unknown" marker and `Some(0.0)` for an explicit `0/N`
  time-zero, so a caller can tell "no anchor recorded / unknown" apart from
  "the anchor is exactly zero" (the lossy `presentation_seconds()` and the
  granule-mapping accessors deliberately fold both to `0.0`).
- `Skeleton::stream_start_seconds(serial) -> Option<f64>` — the
  **file-absolute** data start: `FisBone::start_seconds` plus the fishead
  **basetime**. `None` for an unknown serial or unusable rate; an
  absent/zero-denominator basetime contributes a `0.0` offset. The demuxer
  folds this value into each stream's `StreamInfo::start_time` at open time
  (converted into the stream's own `time_base` ticks), so a non-zero
  basetime/basegranule — e.g. analog video digitised with its original
  `01:00:00` anchor — places the content on the intended timeline rather
  than reporting `start_time = 0`. The duration accumulator stays
  basetime-free, so `duration == end - start` still holds.
- `Skeleton::substream_granule_to_seconds(serial, granulepos) ->
  Option<f64>` — a page's position on the cut segment's own playback
  timeline, `presentation_time + (extract_granules(granulepos) -
  basegranule) / granulerate`. This is distinct from
  `Skeleton::granule_to_seconds`, which answers the basetime/granule-0
  mapping: choose `granule_to_seconds` for "what base-time does granule 0
  correspond to", and this for "where on the cut segment's playback bar
  does this page land". The basetime intentionally does *not* leak into
  the substream timeline. `None` when no fishead describes the cut-in,
  the serial is uncovered, the granulepos is the `-1` sentinel, or the
  rate is unusable.

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

1. the `fishead` BOS packet's *Segment length in bytes* field is
   consistent with the file (a one-shot lazy check on the first seek;
   encoders that left this field at `0` opt out, which is the
   prevailing pattern). For a single-link file the declared length must
   equal the file size; for a **chained** file the declared length is
   shorter — per `docs/container/ogg/ogg-skeleton-4.0.md` "a new
   \"link\" in a \"chain\" can start at the end of the segment" — so a
   shorter declared length is accepted exactly when a fresh `OggS` page
   (the next link's BOS) begins at that offset. A declared length past
   EOF, or one that lands mid-page, disqualifies the index (the segment
   was modified since indexing);
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

The two cross-stream algorithms the §"Keyframe indexes for faster
seeking" section defines are also exposed as reusable methods on the
parsed `Skeleton` (alongside the public `read_vbi_u64` / `write_vbi_u64`
codec) so external seek tooling can run them without re-implementing the
demuxer's internal seek path:

- `Skeleton::indexed_duration_seconds()` returns the indexed segment's
  total duration without decoding any content — the spec's "you can
  calculate the duration as the end time of the last active stream minus
  the start time of first active stream". It takes the minimum
  first-sample time and the maximum last-sample time across every index
  whose `timestamp_denominator` is known (non-zero), combined on a common
  seconds axis so indexes with differing denominators (a 1/48000 audio
  track alongside a 1/1000 video track) mix correctly. An index with an
  unknown (zero) denominator contributes neither endpoint; with no usable
  index (Skeleton 3.0, an index-free 4.0 stream) it returns `None`. This
  is the file-level companion to the per-index
  `SkelIndex::duration_seconds`.
- `Skeleton::seek_offset_for_time(target_seconds)` returns the byte
  offset a multi-stream seek should jump to: for each index with a known
  denominator it takes that stream's last keypoint at or before the
  target (`SkelIndex::keypoint_for_time`), then returns the smallest
  `KeyPoint::offset` across that set — so decoding from there up to the
  target passes a keyframe at or before the target on every concurrently
  active stream. Returns `None` when the target precedes every stream's
  first keypoint or no usable index exists, so the caller falls back to
  bisection per the spec's graceful-fallback rule. This is the same
  minimisation `Demuxer::seek_to` performs internally, surfaced for
  callers driving a seek against a parsed `Skeleton` directly.

Spec reference: `docs/container/ogg/ogg-skeleton-3.0.md`,
`docs/container/ogg/ogg-skeleton-4.0.md`,
`docs/container/ogg/ogg-skeleton-message-headers.wiki`. The 4.0 page
recommends emitting 4.0 in preference to 3.0 when possible, and notes
that decoders must always fall back to bisection when the index is
absent or fails validation (length / page-boundary checks).

#### Track-order addressing (`SkeletonHeaders` §"Track order")

The Skeleton message-headers wiki defines a stable way to address
tracks by an index: tracks are numbered "by the order in which the bos
pages of the tracks appear in the Ogg stream", with the Skeleton BOS
occupying `track[0]` when present (`track[1]` the first content track,
and so on per the wiki's worked example). `OggDemuxer` exposes this as
three accessors: `track_order_len()` returns the number of addressable
slots (content streams plus the Skeleton bitstream, which is not in
`streams()`); `track_order_serial(n)` maps a `track[n]` index to its
logical bitstream's on-wire `bitstream_serial_number`; and
`track_order_index(serial)` is the reverse. Because content streams'
dense `StreamInfo::index` is already assigned in BOS-discovery order,
the mapping is `track[n] -> content stream index n-1` for a
Skeleton-bearing file and `track[n] -> content stream index n` for a
Skeleton-free file (the wiki only reserves `track[0]` for Skeleton when
Skeleton is present). The returned serial round-trips through
`Skeleton::bone_for_serial`, so a caller walking
`0..track_order_len()` recovers each track's fisbone metadata in the
spec-defined order — the basis for a `track[name=…]` / `track[n]`
resolver. Spec reference:
`docs/container/ogg/ogg-skeleton-message-headers.wiki` §"Track order".

#### Track addressing by name / role / language (`SkeletonHeaders`)

The per-fisbone typed accessors above answer "what does *this* track
declare"; four `Skeleton`-level resolvers answer the inverse "which
track(s) match", consuming those accessors at the file level — the
content-negotiation use the message-headers wiki was written for
(differentiating and addressing tracks "e.g. from a JavaScript API"):

- `Skeleton::bone_for_name(name)` resolves the wiki §Name
  `track[name="…"]` addressing form to the unique fisbone carrying that
  `Name` header. The §Name grammar mirrors XML 1.0 `NCName`, so matching
  is **case-sensitive** (`Madonna_singing` ≠ `madonna_singing`), unlike
  the case-insensitive HTTP-style *header field-name* lookup. Crucially,
  it enforces the wiki's uniqueness rule — "The name needs to be unique
  between all the track names, otherwise it is undefined which of the
  tracks is retrieved when addressing by name" — by returning `None` when
  **two or more** fisbones declare the same name, rather than arbitrarily
  picking the first, so a caller can never silently address the wrong
  track. (This is the file-level invariant the `FisBone::name()` per-value
  parser explicitly left to callers.)
- `Skeleton::bones_for_name(name)` is the ambiguity-observing companion:
  it returns *all* fisbones with that name (at most one in a well-formed
  file, more than one in a file that violates the uniqueness rule), so a
  caller can surface a "duplicate track name" diagnostic instead of having
  the match collapse to `None`.
- `Skeleton::bones_with_role(role)` is a multi-track query — the wiki
  §Role notes "The same role can be used across multiple tracks" (e.g.
  every `audio/dub` track to populate a language picker). The role tag is
  matched up to the first `;` (ignoring any `;key=value` parameters) and
  case-insensitively, so a `"video/alternate"` query matches both
  `video/alternate` and `video/alternate;angle=nw`.
- `Skeleton::bones_with_language(tag)` answers "which tracks carry content
  in this language". The wiki §Language documents a comma-separated list
  with the dominating language first (`Language: en-US, fr`); a track
  matches if `tag` appears **anywhere** in its list, not only as the
  dominant first entry, matched case-insensitively per BCP 47.
- `Skeleton::bones_with_dominant_language(tag)` is the dominant-only
  counterpart: it matches a track only when `tag` is its **first**
  (`FisBone::dominant_language`) tag — so a `Language: fr, en` dub matches
  a `"fr"` query but **not** an `"en"` one (`en` is a secondary tag there).
  This answers "which tracks are *primarily* in this language" (e.g.
  choosing the default audio track for a user's locale), the complement to
  `bones_with_language`'s "any content in this language" (e.g. a language
  picker). The wiki §Language gives the first list entry distinguished
  meaning — "the dominating language specified as the first language. It is
  possible to specify less non-dominating languages as a list after the
  main language" — which `FisBone::dominant_language()` surfaces as an
  `Option<&str>` (`None` when the header is absent or expands to zero
  tags). Matching is case-insensitive per BCP 47 §2.1.1.

- `Skeleton::bones_with_content_kind(kind)` and
  `Skeleton::bones_with_content_type(mime)` are the MIME-based companions,
  consuming the per-track `FisBone::content_type()` accessor at the file
  level. `docs/container/ogg/ogg-skeleton-message-headers.wiki`
  §Content-type designates `Content-Type` as Skeleton 4's only **mandatory**
  per-track header ("the mime type of the track"), so MIME lookup is the
  broadest content-negotiation query the field exists for.
  `bones_with_content_kind(&ContentTypeKind)` buckets tracks by their
  top-level MIME kind — "which tracks are audio / video / text" — comparing
  the well-known buckets (`Audio` / `Video` / `Text` / `Image` /
  `Application`) by variant and an `Other(token)` kind case-insensitively
  on its preserved token. `bones_with_content_type(mime)` is the narrow
  codec-specific query — "which tracks are `audio/vorbis`" — matching the
  full `type/subtype` pair case-insensitively per RFC 2045 §5.1; any
  `;key=value` parameters on the query *and* on the track are ignored (so a
  bare `audio/ogg` query matches an `audio/ogg;codecs=opus` track), and a
  `mime` argument with no `/` matches nothing (use `bones_with_content_kind`
  for top-level matching). Both skip tracks whose `Content-Type` header is
  absent or fails to parse as a MIME type.

All six return fisbones in BOS declaration order, skip tracks lacking
the queried header, and trim surrounding whitespace on the lookup key.
Spec reference:
`docs/container/ogg/ogg-skeleton-message-headers.wiki` §"Name", §"Role",
§"Language", §"Content-type".

#### Track stack order (`SkeletonHeaders` §"Altitude")

`Skeleton::bones_by_stack_order()` returns every fisbone ordered by its
**stack order**, bottom-most (drawn first / furthest behind) to front-most
(drawn last / on top) — the file-level companion to the per-track
`FisBone::altitude()` accessor and the input a compositor painting a
multitrack file (PIP overlay, sign-language video on top of the main
video, a mask) consumes: walk the returned slice front-to-back and paint
each track in turn. `docs/container/ogg/ogg-skeleton-message-headers.wiki`
§Altitude defines the field as "the stack order of the tracks ... an
element with greater stack order is always in front of an element with a
lower stack order", taking "the same numerical values as the z-index in
CSS, unlimited negative and positive numbers". The §Altitude default rule
is honoured: a track with **no** `Altitude` header whose `Role` is a
`*/main` role (`audio/main` / `video/main`) sorts strictly below every
other track ("By default, a 'main' track is always displayed bottom-most
unless otherwise defined"), while any track carrying an explicit `Altitude`
("otherwise defined") is placed purely by that signed value — even a
negative one, since the explicit z-index is authoritative — and a non-main
track with no `Altitude` defaults to the CSS `auto` level of `0`. The sort
is **stable** so equal-altitude tracks retain BOS declaration order (the
same ordering the §"Track order" addressing uses), and an `Altitude` header
that is present but malformed (a non-integer or out-of-`i64`-range value)
is treated as "no explicit altitude" — dropped to the default rule rather
than failing the whole query, matching the skip-malformed tolerance of the
other `Skeleton`-level resolvers. Spec reference:
`docs/container/ogg/ogg-skeleton-message-headers.wiki` §"Altitude".

#### Typed message-header writers (write-side symmetry)

Every typed *reader* above (`FisBone::content_type()` / `role()` /
`display_hint()` / `languages()` / `altitude()` / `title()` / `name()` and
`FisHead::utc_time()`) now has an inverse *writer* so callers build a fisbone
from structured values rather than hand-formatting the on-wire string:
`FisBone::set_content_type` / `set_role` / `set_display_hint` /
`set_languages` / `set_altitude` / `set_title` / `set_name` /
`remove_header`, plus `FisHead::set_utc` / `set_utc_str`. The value types
carry the serialisers the setters use — `ContentType::to_wire`,
`Role::to_wire`, `DisplayHint::to_wire`, `DisplayCoord::to_wire` (all also
`Display`), and the pre-existing `Utc::to_string_basic` — so a written value
reads back equal: `bone.set_role(&r); bone.role() == Some(r)`. `set_languages`
joins the tags `", "`-separated with the dominating language first (the wiki
§Language `en-US, fr` shape), drops blank fragments, and removes the header
entirely for an empty list; `set_utc_str` zero-pads the fixed 20-byte
`fishead` slot and refuses (rather than truncates) an over-long anchor. This
closes the demux→mux round-trip loop end to end: a `Skeleton` parsed by the
demuxer can be reconstructed field-for-field and handed straight to
`mux::open_with_skeleton`. Spec reference:
`docs/container/ogg/ogg-skeleton-message-headers.wiki`,
`docs/container/ogg/ogg-skeleton-4.0.md`.

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
  large = fabricated hole), an optional duplicate-BOS page reusing
  the stream serial (RFC 3533 §4 unique-serial violation, driving the
  `restart_serial_on_duplicate_bos` recovery path), and an optional
  single-byte global mutation that triggers CRC-failure resync. The
  reassembly path is therefore reached on essentially every iteration.
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
  pair at the three legal-extreme sizes. `parse` validates the CRC by
  streaming through `crc::compute_page_checksum` over the borrowed page
  bytes (field at offset 22..26 treated as zero per RFC 3533 §6 field 7)
  rather than cloning the page into a scratch buffer. The checksum loop
  is a slice-by-4 advancement (four input bytes per iteration through
  four pre-shifted tables derived from the 0x04C11DB7 generator
  polynomial); the table derivations are pinned in unit tests against a
  verbatim scalar oracle on lengths 0..65 535.
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
