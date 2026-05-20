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

## License

MIT — see [LICENSE](LICENSE).
