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
below the target. Theora and unknown streams return
`Error::Unsupported` — Theora's granule encoding packs keyframe
distance into the timestamp and needs codec-aware translation.

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

A cargo-fuzz harness under `fuzz/` (panic-freedom only, no oracle —
the clean-room wall bars libogg / Xiph / ffmpeg as cross-decoders)
hammers four surfaces with attacker bytes:

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

Run from `fuzz/` with `cargo +nightly fuzz run <target>`; no target
runs as part of the per-PR CI shim (the org reusable workflow does
not invoke `cargo fuzz`), so the harness is a long-running offline
hardening tool rather than a gate.

## License

MIT — see [LICENSE](LICENSE).
