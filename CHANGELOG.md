# Changelog

All notable changes to this project will be documented in this file.

The format is based on [Keep a Changelog](https://keepachangelog.com/en/1.0.0/),
and this project adheres to [Semantic Versioning](https://semver.org/spec/v2.0.0.html).

## [Unreleased]

### Added

- `validate` module: whole-file RFC 3533 conformance validation.
  `validate::validate(&[u8])` walks a complete physical bitstream and
  returns a typed `ConformanceReport` — pages/streams/links/junk-byte
  totals plus a capped per-issue list (`Rule`, `Severity`, byte
  offset, page ordinal, serial, detail) covering the §6 field rules
  (capture pattern, version 0, page extent vs segment table, CRC,
  granule-position semantics including the -1 reservation and
  per-stream monotonicity, sequence-number continuity) and the §4
  bitstream-structure rules (BOS/EOS placement, grouped-BOS
  contiguity, chain-link boundaries, serial uniqueness across
  grouping and chaining, page-after-EOS, missing EOS), plus the
  §5/§6-field-3 continued-flag/lacing agreement checks (continued
  BOS, orphaned or abandoned continuations, EOS ending mid-packet).
  Damage-tolerant: junk and CRC-failed spans produce one precise
  issue each, the walk rescans to the next checksum-valid page, and
  a damaged stream is re-baselined for exactly one page so a single
  flipped bit never cascades into sequence/continuity noise. Never
  panics on arbitrary bytes; memory is bounded (`MAX_ISSUES` cap
  with a suppressed-issue tally, no page-body copies)
- muxer CI gate (`tests/conformance_validator.rs`): every muxer
  configuration — all five codec mappings, nil and oversize packets,
  the soft page-size target, grouped Theora+Vorbis, three-link
  chains, mixed grouping+chaining, Skeleton 3.0/4.0 with and without
  muxer-built keyframe indexes — must validate with zero conformance
  issues, and surgically damaged copies must trip the exact rule the
  damage violates (CRC, truncation plus missing-EOS, dropped-page
  sequence gap)
- demuxer damage-event ledger: `OggDemuxer::damage_events()` returns
  the first `MAX_DAMAGE_EVENTS` (64) damage events observed on the
  linear demux path — the per-event companion of the aggregate
  `hole_count` / `framing_error_count` / `resync_count` /
  `duplicate_serial_count` counters. Each `DamageEvent` carries its
  `DamageKind` (hole, framing-error, resync, duplicate-serial,
  truncated-tail) plus whatever position information the observing
  code path had: the landing-page byte offset for resyncs, the
  incomplete page's offset for truncated tails, and serial/page-
  sequence attribution for page-model events. Retention is capped so
  hostile inputs cannot grow memory; `damage_event_total()` keeps
  counting past the cap
- deterministic hostile-input sweeps (`tests/hostile_sweep.rs`):
  exhaustive truncation at every byte length, exhaustive single-byte
  corruption at every offset, per-page CRC-field damage, a seeded
  multi-mutation battery (7500 rounds of flips / junk insertion /
  span deletion / span duplication from a fixed xorshift64* seed) and
  a 6000-round header-section battery, over a corpus of real muxer
  output plus hand-framed grouped and chained files. Every mutated
  buffer must demux without panicking, terminate under an iteration
  cap, never deliver more payload bytes than the input holds, keep
  the damage ledger within its cap — and validate without panicking
  within `MAX_ISSUES`
- black-box validator cross-checks (`tests/blackbox_validators.rs`):
  muxer outputs are written to temp files and fed to `oggz-validate`
  and `ffprobe` as opaque CLIs. `oggz-validate` must accept six
  layouts (single Vorbis and Opus, chained and Skeleton-4.0 variants
  of both); `ffprobe` must identify the Opus stream's codec, rate and
  channel count with an empty error stream on the single, chained,
  and Skeleton files. Both tools are optional at runtime — absent
  binaries skip the check with a stderr note so minimal CI images
  still pass (the in-tree `validate` module is the always-on gate)
- fuzz: new `validate` target (panic-freedom, `MAX_ISSUES` bound,
  junk-tally sanity, determinism, and `Display` rendering of the
  conformance report on arbitrary bytes — 8.3M executions clean on
  the landing slice), and the `mux_roundtrip` target now gates every
  fuzz-shaped muxer output through the whole-file validator and
  requires an empty damage ledger on the demux side (1.2M executions
  clean)

### Changed

- a file that ends inside a page (partial transfer) now demuxes to a
  clean EOF after delivering every complete page, with a
  `TruncatedTail` ledger entry — previously the partial tail surfaced
  as an error from `next_packet` (and a sub-27-byte trailing fragment
  did so even when it was plain junk)
- a page whose `stream_structure_version` is not 0 (RFC 3533 §6
  field 2 specifies only version 0) is now skipped through the §3
  recapture path like any other unusable page, counting one resync —
  previously a single flipped version bit on a CRC-resealed page
  aborted the whole demux

## [0.1.8](https://github.com/OxideAV/oxideav-ogg/compare/v0.1.7...v0.1.8) - 2026-07-10

### Other

- Theora mapping section + Skeleton-free seek and interleave notes
- Theora streams + cross-stream page-order invariant in mux_roundtrip
- cross-stream time-ordered page release + header-section drain
- Theora granule-position packing + spec-conformant page layout
- Theora streams driven by their own identification header
- identification-header + granule-position container mapping module
- bound the open()-time header-collection walk (8192-page budget)
- refuse phantom stream registration on Skeleton-serial BOS collisions
- four structure-aware targets — framing layer, mux round-trip, chain graphs, hostile seeks
- neutralize interop-note wording to an unnamed independent reference decoder
- opt-in soft page-size target (RFC 4-8 kB band) + ffmpeg interop note
- compile-tested whole-file mux->demux round-trip example
- buffer-level PageWriter/PacketAssembler layer + Page::try_to_bytes + xiph_lace helpers

### Added

- `theora` module: the Theora-in-Ogg container mapping's
  identification-header parser/builder (`TheoraIdHeader`, spec §6.2
  byte layout — dimensions, frame rate, aspect ratio, `KFGSHIFT`,
  pixel format) and the granule-position codec (`TheoraGranule`, spec
  §A.2.3): keyframe/offset split packing and unpacking with the
  version-dependent counting origin (frame-count for 3.2.1+, frame-
  index for VREV 0) folded into 0-based absolute frame indices. The
  spec's inconsistent mid-stream worked example is resolved against
  real reference bitstreams (consecutive keyframes carry 1|0, 2|0,
  3|0, …)
- Theora demux is now driven by the stream's own identification
  header, with no Skeleton required (previously Theora granule
  handling needed a Skeleton 4.0 fisbone): stream description
  (picture-region dimensions, FRN/FRD frame rate, pixel format, NOMBR
  bit rate, one-tick-per-frame time base), per-packet pts as 0-based
  absolute frame indices for EVERY data packet (the page granule
  anchors all frames finishing on it, not just the last), keyframe
  flags proven from the granule packing, duration from the unpacked
  final frame count, and `seek_to` / `seek_to_keyframe` on plain
  `.ogv` files. Verified against the reference bitstreams under
  `docs/video/theora/fixtures/` and cross-checked with `ffprobe`
  (frame indices, keyframe flags, dimensions, frame rate, and
  duration all agree). Streams whose ID header does not parse keep
  the historical raw-granule + fisbone behaviour
- Theora MUX support: the muxer now packs each data packet's
  `(keyframe << KFGSHIFT) | frames-since-keyframe` granule position
  from codec-level `(frame index, keyframe flag)` packets — the shift
  and version origin recovered from the ID header in the stream's
  extradata — with a per-stream frame counter for pts-less packets
  (each Theora packet is exactly one frame) and an actionable error
  when a keyframe interval overflows the `2^KFGSHIFT − 1` offset
  capacity. Header pagination follows §A.2.1 (ID header alone on the
  BOS page, comment beginning the second page, page break before the
  first frame packet), and the multiplexed mapping's §A.3.2 BOS rule
  is enforced: the Theora identification page is emitted first even
  when the caller lists audio streams ahead of the video (output for
  Theora-less files is unchanged byte-for-byte)
- `remux` and `merge` examples: rewrite any Ogg file through the
  demux→mux pair, and multiplex several files' streams into one
  grouped physical stream with time-ordered packet interleave

### Fixed

- multiplexed (grouped) MUX output now honours two layout rules the
  single-page EOS-deferral used to violate (found by black-box
  validation of Theora+Vorbis/Opus files against `oggz-validate` and
  an independent reference demuxer): every stream's header pages are
  drained onto the wire before any data page ("the header pages of
  each of the logical streams MUST be grouped together before any
  data pages occur" — previously the last header page, e.g. the
  Vorbis setup page, could land after another stream's data pages),
  and data pages are released in increasing granule-time order across
  streams via per-stream release queues with a cross-stream
  watermark. Single-stream output is byte-identical to before. A
  stream that ends with all pages already on the wire now closes with
  an RFC 3533 §4 nil EOS page instead of losing its EOS marker

- a content BOS page reusing the Skeleton bitstream's serial no longer
  registers a phantom public stream (a `streams()` entry that could
  never receive a packet — every page with that serial routes to the
  Skeleton metadata path — and that broke the Skeleton "Track order"
  serial↔index round-trip); the RFC 3533 §4 unique-serial violation is
  now surfaced on `duplicate_serial_count`. Found by the new
  `chain_graph` structure-aware fuzz target
- a second `fishead\0` BOS on another serial no longer clobbers the
  recorded Skeleton's fisbone/index state; the first Skeleton wins,
  matching the in-stream second-fishead handling
- `open()` no longer buffers the entire file's packets in memory when
  a hostile header section never completes (a Skeleton BOS whose EOS
  page never arrives, or a declared header count never satisfied):
  header collection is bounded by an 8192-page budget and continues
  best-effort, with no packet lost on the subsequent drain

### Added

- four structure-aware fuzz targets (`framing_layer`, `mux_roundtrip`,
  `chain_graph`, `seek_hostile`) covering the buffer-level framing
  round-trip, mux→demux self-consistency (byte-identical packets, zero
  damage counters), chained+grouped stream graphs with serial-reuse
  violations, and hostile-granule/lying-Skeleton-index seek storms

- `framing` module: buffer-level packet ⇄ page layer for one logical
  bitstream (`PageWriter`, `PacketAssembler`, `parse_pages`,
  `pages_to_packets`) — the no-I/O, no-`StreamInfo` API codec crates
  use to build/validate their own Ogg encapsulation, ported from the
  implementation proven in `oxideav-vorbis`
- `Page::try_to_bytes`: fallible serialization that reports RFC 3533
  §6 lacing-invariant violations as `Error::InvalidData` instead of
  panicking
- `mux::xiph_lace` / `mux::xiph_unlace`: public helpers to build and
  split the Xiph-laced Vorbis/Theora `extradata` blob the muxer and
  demuxer exchange
- compile-tested "write an .ogg, read it back" example (lib.rs
  doctest + README section): `mux::open` + `write_packet` on the
  write side, `demux::open` + `next_packet` on the read side
- opt-in soft page-size target: `OggMuxer::set_page_target_bytes`
  and `framing::PageWriter::with_page_target` / `set_page_target`
  flush a page once a packet completes at/past the target (RFC 3533
  "usually 4-8 kB" band); defaults unchanged. Motivated by black-box
  testing with an independent reference decoder: a stream whose
  first audio-bearing page is also
  its EOS page decodes short by blocksize0/2 samples, while any
  ≥2-audio-page split recovers the full declared length

## [0.1.7](https://github.com/OxideAV/oxideav-ogg/compare/v0.1.6...v0.1.7) - 2026-07-03

### Other

- add CI / crates.io / docs.rs / MIT-license badges
- chained link with a multi-page (spanning) Vorbis setup header
- chained-mux duration + grouped-link round-trips, README/CHANGELOG
- chained-stream muxing via OggMuxer::begin_new_link (RFC 3533 §4)
- full-fidelity demux->mux->demux Skeleton round-trip test
- Skeleton::from_streams builder (write-side companion to skeleton())
- mux spans a packet larger than one page across pages (RFC 3533 §4/§5)
- typed Skeleton message-header / UTC writers (write-side symmetry)
- keyframe-aware seek_to_keyframe for Theora (granuleshift keyframe-index)
- add OggDemuxer::stream_granuleshift accessor + README per-packet-flags section
- end-to-end Theora demux->remux keyframe-index test
- chained Speex/FLAC links surface Vorbis-comment metadata mid-file
- mux splits Theora Xiph-laced extradata into 3 header packets
- per-packet keyframe flag tracks granuleshift packing (Theora inter-frames no longer mislabelled)
- surface Speex comment header as container metadata
- parse Speex + FLAC ID headers for rate/channels + sample-rate time-base
- FLAC-in-Ogg header-packet count + Vorbis-comment metadata (RFC 9639 §10.1)
- seek_to honours Opus pre-skip on the bisection axis (RFC 7845 §4.3)
- Opus pre-skip granule semantics (RFC 7845 §4.3)
- chained-file-aware Skeleton 4.0 segment-length index-validity check
- anchor stream start_time onto the Skeleton fishead basetime/basegranule
- typed Skeleton fishead presentation/basetime accessors (unknown vs zero)
- tidy process_page BOS-handling comment (no behaviour change)
- fuzz the §4 duplicate-serial restart path (continued_edge)
- document §4 unique-serial-number enforcement in README
- integration tests for §4 unique-serial-number detection
- enforce RFC 3533 §4 unique-serial-number MUST (duplicate_serial_count)
- document §4 mixed grouping+chaining + nil-page / multi-page coverage in README
- byte-exact tests for clean multi-page packet reassembly (RFC 3533 §5)
- coverage for RFC 3533 §4 nil (zero-segment) pages
- end-to-end tests for RFC 3533 §4 mixed grouping + chaining topology
- Theora duration estimate unpacks granuleshift via Skeleton fisbone
- Skeleton content-type resolvers (bones_with_content_kind / bones_with_content_type)
- dominant-language addressing (wiki §Language dominant-first rule)
- file-level Skeleton 4.0 index helpers (indexed_duration_seconds + seek_offset_for_time)
- Skeleton track stack-order resolver (message-headers §Altitude)
- refresh to current status, drop per-round changelog cruft

### Fixed

- **Muxer now spans a packet larger than one Ogg page across multiple pages
  (RFC 3533 §4 / §5).** A single page holds at most 255 lacing segments, so a
  content or header packet of ~64 KB or more (255×255 = 65025 bytes laces to
  256 segments) could not fit one page — and the muxer previously fed all the
  segments into a single `Page`, tripping the `Page::to_bytes` 255-segment
  assertion (a panic). Real Vorbis setup codebooks and Theora keyframes
  routinely exceed this. `write_packet` now flushes the partial page first,
  then `append_packet_spanning` fills successive pages 255 segments at a time;
  every intermediate page ends on a 255-valued segment so `flush_page` marks
  the following page `continued` (§6 field 3) automatically, and the packet's
  terminator (`< 255`, or the trailing `0` for an exact multiple of 255) lands
  on the final page. The demuxer already reassembled such packets
  byte-for-byte (`tests/multipage_packet.rs`); this restores the write-side
  symmetry. `tests/large_packet_mux.rs` round-trips content packets at the
  65025 / 70000 / 130050 / 200000-byte sizes plus a 96 KB header packet through
  mux → demux byte-exact.

### Added

- **Chained-stream muxing (RFC 3533 §4 sequential multiplexing).**
  `mux::open_concrete(output, streams) -> OggMuxer` returns the concrete muxer
  (the object-safe `Muxer` trait cannot express a new link, which takes a fresh
  `&[StreamInfo]`), and `OggMuxer::begin_new_link(streams)` starts a new chain
  link: it finalizes the current link — draining and EOS-terminating every one
  of its logical bitstreams so "the eos page of a given logical bitstream is
  immediately followed by the bos page of the next" — then writes the new
  link's BOS + secondary-header pages. Because the slice may carry more than
  one stream, a link can itself be a group of concurrently-multiplexed streams,
  so the muxer emits the full §4 chain-of-groups topology. Serials are tracked
  file-wide and any collision (later links reusing `StreamInfo::index 0`) is
  bumped to the next free value, honouring the §4 unique-serial MUST — a muxed
  chain demuxes back with `duplicate_serial_count() == 0`. `link_index()` /
  `stream_serial(index)` expose the current-link state. A new link may only
  begin after a content data page (the demuxer keys link boundaries on
  BOS-after-non-BOS) and chaining is mutually exclusive with an attached
  Skeleton; both are guarded with errors. `tests/chained_mux.rs` round-trips
  2- and 3-link chains, a grouped-then-chained topology, and global serial
  uniqueness through mux → demux.

- **`Skeleton::from_streams(&[StreamInfo], Version)`** — build a complete
  Skeleton (a `fishead` plus one `fisbone` per content stream) directly from
  the demuxer's / caller's `StreamInfo` list, the write-side companion to the
  demuxer's `skeleton()` read path. Derives each fisbone's serial (the stream
  `index`, matching the muxer's `derive_serial`), granule rate (the inverse of
  the stream `time_base` — a `1/48000` audio base → `48000/1` Hz, a `1/fps`
  video base → its fps), number of header packets
  (`codec_id::header_packet_count`), and the `Content-Type` MIME for the two
  codecs the in-tree spec states verbatim (`audio/vorbis`, `video/theora`).
  Other codecs are left without a `Content-Type` for the caller to fill — the
  full codec→MIME registry lives in an external Xiph wiki page not mirrored
  under `docs/container/ogg/`, so guessing it would reach outside the
  clean-room allow-list. `granuleshift` / `preroll` / `Role` / `Name` are left
  at defaults (Ogg framing in a `StreamInfo` does not reveal them) for callers
  to set before handing the result to `mux::open_with_skeleton`. Pins:
  `tests/skeleton_mux.rs::from_streams_*` (mux→demux round-trip) plus src unit
  tests for the time-base inversion and 3.0/4.0 fishead selection.

- **Typed Skeleton message-header / UTC *writers* (write-side symmetry).** The
  module already carried a typed *reader* for every
  `docs/container/ogg/ogg-skeleton-message-headers.wiki` field and the
  `fishead` UTC slot; this adds the inverse setters so callers build a fisbone
  from structured values instead of hand-formatting wire strings:
  `FisBone::set_content_type` / `set_role` / `set_display_hint` /
  `set_languages` / `set_altitude` / `set_title` / `set_name` /
  `remove_header`, and `FisHead::set_utc` / `set_utc_str`. Each is the exact
  inverse of its reader (`ContentType::to_wire`, `Role::to_wire`,
  `DisplayHint::to_wire`, `DisplayCoord::to_wire`, plus `Display` impls on
  those types and `Utc::to_string_basic`): `bone.content_type()` /
  `role()` / `display_hint()` / `languages()` / `altitude()` / `title()` /
  `name()` and `head.utc_time()` return a value equal to the one written.
  `set_languages` joins tags `", "`-separated with the dominating language
  first (wiki §Language `en-US, fr` shape), dropping blank fragments and
  removing the header entirely when the list is empty. `set_utc_str` zero-pads
  the fixed 20-byte slot and refuses an over-long anchor rather than
  truncating. `tests/skeleton_setters.rs` pins each reader↔writer round-trip
  plus full `to_bytes`/`parse` survival.

- **`OggDemuxer::seek_to_keyframe(stream_index, pts)`** — keyframe-aware seek
  for sub-seekable (keyframe-bearing) mappings. A bare `seek_to` lands on the
  page whose frame number floors the target, which for Theora may be an
  inter-frame the decoder cannot start from. `seek_to_keyframe` reads the
  landed page's keyframe index out of the granuleshift packing
  (`docs/container/ogg/ogg-skeleton-4.0.md` — the low `shift` bits are the
  offset-since-keyframe, the high bits the keyframe index) and, when the
  landing isn't already a keyframe, re-seeks to that keyframe's own page so
  forward decoding starts on an intra frame. The returned granule is the
  keyframe page's (offset half zero); the caller decodes forward and discards
  frames until it reaches the requested `pts`. Granuleshift-0 mappings (every
  audio codec — each packet already a random-access point) and a landing
  already on a keyframe pass through identical to `seek_to`.
  `tests/seek_keyframe.rs` pins the inter→keyframe back-up, the already-on-
  keyframe identity, and the audio identity.

- **`OggDemuxer::stream_granuleshift(stream_index)`** surfaces the per-stream
  granuleshift the per-packet keyframe decision is derived from (`Some(0)` for
  an audio mapping or a stream with no fisbone, the Skeleton 4.0 `fisbone\0`
  declared shift for a Theora stream), alongside the existing `opus_pre_skip` /
  `stream_serial` / `stream_link_index` accessors, so callers can unpack a
  page's raw granule into its `(keyframe_index, offset)` halves themselves.
  README gains a "Per-packet timing & flags" section documenting the `pts` /
  `unit_boundary` / `keyframe` assignment rules.

- **End-to-end Theora demux→remux keyframe-index coverage.**
  `tests/theora_remux_index.rs` demuxes a Theora-in-Ogg file (Skeleton fisbone
  granuleshift 6, granules describing 2 keyframes among 4 frames), reuses the
  reconstructed extradata to remux the packets through
  `mux::open_with_skeleton_indexed`, and asserts the recovered Skeleton 4.0
  index records exactly the 2 true keyframes — not one keypoint per frame. The
  auto-index muxer keys off `PacketFlags::keyframe`, so this pins the
  per-packet keyframe fix flowing correctly through a full demux→mux pipeline.

### Fixed

- **Chained Speex / FLAC links now surface their Vorbis-comment metadata.**
  The open-time metadata sweep parsed all five mappings (Vorbis, Opus, Theora,
  Speex, FLAC), but the per-stream path that runs when a chained link's header
  packets complete *mid-file* (`populate_metadata_for`) handled only
  Vorbis / Opus / Theora, silently dropping a chained Speex or FLAC link's
  tags — and FLAC besides, since it only looked at the second header packet
  while a FLAC comment block can sit in any post-mapping packet. Both call
  sites now share one `parse_codec_comment(codec_id, &header_packets, …)`
  helper, so every mapping behaves identically in the single-link and chained
  cases. `tests/chained_metadata.rs` pins a Vorbis→FLAC and a Vorbis→Speex
  chain, asserting the second link's title / artist / vendor surface after
  the drain that discovers the mid-file BOS.

- **Theora mux now splits its Xiph-laced extradata into 3 header packets.**
  The README documented (and the demux build side already did) that "for
  Vorbis and Theora the 3-packet sequence is parsed out of the Xiph-laced
  blob", but `extract_codec_headers` only routed `"vorbis"` through
  `parse_xiph_lacing`; Theora fell through to the catch-all that emits the
  whole blob as a single Ogg packet. A Theora stream muxed via `open` /
  `open_with_skeleton` therefore wrote one malformed mega-packet (a
  `0x02`-lacing-prefixed blob) where a decoder expects the bare
  `0x80 "theora"` identification packet, so the result neither sniffed back
  as Theora nor reproduced the original extradata. Theora now shares the
  Vorbis split path. `tests/mux_roundtrip.rs::mux_then_demux_theora_splits_three_header_packets`
  pins the mux→demux round-trip (codec re-sniffs as `theora`, extradata
  byte-matches the original 3-packet blob, all data packets recovered).

- **Per-packet `PacketFlags::keyframe` now tracks the granuleshift packing
  instead of being blanket-`true`.** Every delivered content packet was
  unconditionally flagged a keyframe. For audio mappings (Vorbis / Opus /
  FLAC / Speex) that is correct — they declare granuleshift 0 and every packet
  is an independent random-access point — but for Theora it mislabelled every
  inter-frame as a random-access point. The granuleshift, carried by the
  Skeleton 4.0 `fisbone\0` (`docs/container/ogg/ogg-skeleton-4.0.md`: "the
  number of lower bits from the granulepos field that are used to provide
  position information for sub-seekable units (like the keyframe shift in
  theora)"), splits a page's granule into a keyframe index (high bits) and an
  offset-since-keyframe (low `shift` bits). The new `granule_is_keyframe`
  helper flags the last-on-page packet a keyframe exactly when that offset is
  zero; non-granule-bearing packets on a shifted track are flagged `false`
  rather than mislabelled. A Theora stream with no fisbone (granuleshift
  unknown, defaulting to 0) keeps the conservative all-keyframe flagging so
  random access is never under-reported. `tests/packet_keyframe.rs` pins the
  Theora-with-fisbone, Theora-without-fisbone, and Vorbis (every-packet,
  including the intermediate non-granule packet) cases; `src/demux.rs`
  unit-tests the helper's boundary cases (shift 0, the `-1` sentinel,
  `shift >= 63`, and agreement with `theora_frame_no`).

### Added

- **Speex comment header surfaced as container metadata.** The Speex 2nd
  header packet is the bare Vorbis-comment structure (Speex manual §7.3,
  `docs/audio/speex/speex-manual.pdf` — "the second packet contains the Speex
  comment header. The format used is the Vorbis comment format"), carrying no
  `0x03 "vorbis"`-style magic prefix unlike the Vorbis/Theora/Opus comment
  packets. The demuxer now parses it directly into `Demuxer::metadata()`,
  closing the one audio mapping whose tags were previously dropped.
  `tests/id_header_params.rs::speex_comment_header_populates_metadata` pins it.

- **Speex + FLAC identification headers parsed for `sample_rate` /
  `channels` (+ Speex `bit_rate`), with a sample-rate time-base.** The
  demuxer previously parsed only the Vorbis and Opus ID headers; Speex and
  FLAC streams were left with `sample_rate == None`, `channels == None`, and
  the `1/1_000_000` placeholder time-base. Both mappings carry a
  *sample-count* granule (Speex manual §7.3 / table 7.1,
  `docs/audio/speex/speex-manual.pdf` — "the granulepos is the number of the
  last sample encoded in that packet"; FLAC RFC 9639 §10.1,
  `docs/audio/flac/rfc9639-flac.pdf` — "the granule position is the number of
  the last sample contained in the last completed packet"), so the wrong
  time-base mis-scaled both duration estimates and seek targets. `parse_speex_id`
  reads the fixed 80-byte little-endian header (`rate` @36, `nb_channels` @48
  clamped to mono/stereo per the codec's own limits, `bitrate` @52 with the
  `-1` "unknown" sentinel suppressed); `parse_flac_id` reads the STREAMINFO
  block embedded in the mapping packet (RFC 9639 §8.2 Table 3 — `u(20)` sample
  rate, `u(3)` channels−1 bit-packed big-endian at STREAMINFO offset 10). Both
  now stamp the stream with a `1/sample_rate` time-base alongside Vorbis/FLAC,
  so a Speex or FLAC track reports its true length and seeks land on the right
  page. `tests/id_header_params.rs` pins rate/channel/bitrate extraction, the
  unknown-bitrate suppression, the corrupt-channel clamp, the high-sample-rate
  (192 kHz) STREAMINFO bit math, and the corrected sample-rate-based duration.

- **FLAC-in-Ogg header-packet count read from the mapping header (RFC 9639
  §10.1).** A FLAC-in-Ogg logical bitstream declares the number of header
  packets *after* the first in a 2-byte big-endian field at bytes 7..9 of the
  `0x7F "FLAC"` mapping packet (`docs/audio/flac/rfc9639-flac.pdf` §10.1:
  "Number of header packets (excluding the first header packet) as an
  unsigned number coded big-endian"). The demuxer previously hard-coded a
  conservative `1` header packet, so every metadata block past STREAMINFO
  (Vorbis comment, padding, seek table, picture, …) was mis-delivered as a
  content packet. The new `codec_id::header_packet_count_from_first` reads the
  field and absorbs `1 + declared` header packets; a declared `0` is the
  spec's explicit "unknown" marker and falls back to the old conservative
  `1`. As a result the first audio frame is the first packet the demuxer
  delivers, and the full metadata header section now lands in the stream's
  `extradata`.
- **FLAC-in-Ogg Vorbis-comment metadata is parsed.** Now that the metadata
  header packets are correctly absorbed, the demuxer scans them for the FLAC
  Vorbis-comment block (§8.1 metadata block type 4: a 4-byte block header
  whose low 7 type bits are 4, directly followed by the standard
  vorbis_comment payload — no `0x03 "vorbis"` prefix, no framing bit) and
  surfaces its tags via `Demuxer::metadata()`, matching the existing Vorbis /
  Opus / Theora metadata paths. New `tests/flac_mapping.rs` (header-count
  absorption, metadata extraction, unknown-count fallback) plus
  `codec_id` unit tests.
- **Opus pre-skip granule-position semantics (RFC 7845 §4.2 / §4.3 / §5.1).**
  An Ogg Opus stream's on-wire `granulepos` counts 48 kHz samples *including*
  the encoder-delay padding the decoder must warm up on but discard; the
  playback-relevant sample count is `granule − pre-skip`
  (`docs/audio/opus/rfc7845-ogg-opus.txt` §4.3: "PCM sample position =
  granule position − pre-skip"). The demuxer now reads the pre-skip from the
  `OpusHead` ID header (bytes 10..12, LE u16, §5.1 field 4) at BOS time and
  subtracts it in its granule→time mapping, so an Opus stream no longer
  over-reports its duration by `pre-skip / 48000` seconds (an 11 971-sample
  pre-skip on a 1 s stream previously reported ≈1.249 s). The new
  `OggDemuxer::opus_pre_skip(stream_index) -> Option<u16>` accessor exposes
  the raw value (`None` for a non-Opus or unknown stream) so a downstream
  Opus decoder can discard the same leading samples without re-parsing the
  header. The `-1` "no packets finish on this page" sentinel is left
  untouched, and a granule below the pre-skip clamps to 0 (RFC 7845 §4.5's
  legal "stream shorter than pre-skip" edge reports zero-length rather than
  negative). Non-Opus streams are unaffected — they never carry a pre-skip
  entry and pass through unchanged.
- **`seek_to` honours Opus pre-skip (RFC 7845 §4.3 / §4.6).** A `seek_to(pts)`
  on an Opus stream takes `pts` as a *PCM sample position* (playback time),
  but a page's on-wire granule counts `PCM position + pre-skip`, so the
  bisection / dense-index floor lookup now offsets each page granule by the
  stream's pre-skip before comparing it to the target. Without the offset a
  seek would land `pre-skip / 48000` s early. The returned granule is still
  the landed page's *raw* on-wire value (so a downstream decoder recovers the
  same granule the file carries); the offset only changes which page floors
  the target. Vorbis / FLAC / Speex keep the identity axis (offset 0) and are
  byte-for-byte unchanged. New `tests/opus_pre_skip.rs` covers the duration
  subtraction, the accessor, the zero-pre-skip pass-through, the non-Opus
  no-op, the PCM-position floor seek, and the pre-skip-vs-zero seek
  divergence.
- **The demuxer anchors each stream's `start_time` onto the Skeleton fishead
  playback timeline.** `docs/container/ogg/ogg-skeleton-4.0.md` §"What
  decoding-related information is needed?" defines the fishead **basetime** as
  "a mapping for granule position 0 (for all logical bitstreams) to a playback
  time" (the analog-video "starts at a time of 1 hour" example), and §"How to
  allow the creation of substreams …" adds the per-track **basegranule**, "the
  granule number with which this logical bitstream starts in the remuxed
  stream". Both were exposed by `Skeleton::stream_start_seconds` but the
  demuxer ignored them and reported `start_time = 0` for every stream. `open`
  / `open_concrete` now fold `basetime + basegranule / granulerate` into each
  stream's `start_time` (converted into the stream's own `time_base` ticks),
  so a player can place the content on the intended timeline. The basetime is
  a *timeline anchor*, not a duration component, so the duration accumulator
  stays basetime-free and `duration == end - start` continues to hold (a
  3600 s basetime on a 2 s stream still reports a 2 s duration). Streams with
  no Skeleton, no fisbone, an unusable granule rate, or a zero/absent
  basetime+basegranule keep the `start_time = 0` default — the un-cut common
  case. New `tests/skeleton_basetime.rs` covers the basetime-only anchor, the
  basetime+basegranule sum, the zero-anchor no-op, and the basetime-free
  duration invariant.

- **Skeleton 4.0 fishead time-anchor accessors that distinguish the spec's
  zero-denominator "unknown" marker from a genuine time zero.** The fishead's
  presentation time ("the actual cut-in time … all logical bitstreams are
  meant to start presenting from") and basetime ("a mapping for granule
  position 0 (for all logical bitstreams) to a playback time", per
  `docs/container/ogg/ogg-skeleton-4.0.md`) were parsed and serialized but had
  no typed read API — and the existing lossy `presentation_seconds` collapses
  a zero-denominator rational (the spec's "unknown" marker) to `0.0`,
  indistinguishable from a deliberate `0/N` time-zero anchor. New
  `Rational::to_seconds_checked() -> Option<f64>` makes the distinction at the
  rational level (`None` for a zero denominator, `Some(0.0)` for `0/N`), and
  `Skeleton::presentation_seconds_checked()` / `Skeleton::basetime_seconds()`
  surface the two fishead anchors with the same three-way contract used by the
  other typed Skeleton accessors. A caller can now tell "no cut-in time
  recorded / unknown" apart from "the cut-in time is exactly zero".

- **RFC 3533 §4 unique-serial-number enforcement (`duplicate_serial_count`).**
  §4 makes serial uniqueness a normative MUST for both topologies: "Each
  grouped logical bitstream MUST have a unique serial number within the scope
  of the physical bitstream" and, identically, "Each chained logical bitstream
  MUST have a unique serial number within the scope of the physical bitstream."
  The demuxer previously silently merged a BOS page whose serial was already
  live into the existing stream's reassembly state — splicing two distinct
  bitstreams' packets together and reading the duplicate's identification
  packet as content. It now detects the collision, recovers by restarting the
  colliding serial in place (drops the stale partial-packet buffer, resets the
  page-sequence tracker so the restart's pages are not mis-read as page loss,
  re-arms header capture from the duplicate BOS's own codec, and re-files the
  serial under the new BOS's link index), and exposes the running tally on
  `OggDemuxer::duplicate_serial_count()`. Detection fires on all three walkers
  (the `open` BOS section walk, the `next_packet` data-page path, and the
  `build_seek_index` header scan); the header scan, which visits every page
  exactly once, is the authoritative file-wide source and the two walkers
  never double-count the same collision. Zero for every conforming file.

- **Byte-exact coverage for clean multi-page packet reassembly (RFC 3533
  §5).** §5 specifies that a packet larger than a page "has to be distributed
  over several pages" by 255-byte lacing chunks, terminated by a value `< 255`
  (or `0` for an exact multiple of 255). The existing `page_loss.rs` tests
  covered the *lossy* spanning case (a middle page dropped → discard, not
  splice); new `tests/multipage_packet.rs` covers the *clean* case end to end:
  a packet split across 2, 3, and 4 pages (the 4-page case using 510-byte,
  two-segment continuing pages) reassembles **byte-for-byte**; the
  exact-multiple-of-255 boundary where the zero-terminator lands alone on a
  fresh continued page completes the packet correctly; and a spanning packet
  following two whole-packet pages on the same stream does not lose the
  demuxer's place. Validates the `continued`-flag (§6 field 3) + lacing (§5)
  reassembly path that real >64 KB Vorbis setup / Theora keyframe packets
  exercise.

- **Coverage for RFC 3533 §4 'nil' (zero-segment) pages.** §4 defines a nil
  page as "containing no content but simply a page header with position
  information and the eos flag set", and §5 reiterates that a zero-length
  packet "is not an error". New `tests/nil_page.rs` exercises the full demuxer
  path: a nil EOS page (granule + eos flag, `number_page_segments = 0`, no
  body) produces no spurious packet yet its granule still drives the open-time
  duration estimate (the common encoder pattern of flushing the closing
  granulepos after the last data packet already terminated); a mid-stream nil
  page with granule `-1` is transparent to packet reassembly; and a nil page
  round-trips through `Page::parse`/`to_bytes` to zero packet segments with its
  granule/flags preserved. The page layer and demuxer already handled this —
  these pin the §4 nil-page contract.

- **End-to-end coverage for the RFC 3533 §4 mixed grouping + chaining
  topology.** §4 defines the most general legal Ogg physical bitstream as a
  chain of *groups* of concurrently-multiplexed bitstreams ("It is possible
  to consecutively chain groups of concurrently multiplexed bitstreams"),
  with the worked example `|*A*|*B*|*C*|A|A|C|B|...|#B#|#C#|*D*|D|...|#D#|`
  (link 0 groups A/B/C, link 1 chains D). The existing chained tests only
  exercised one stream per link. New `tests/chained_grouped.rs` validates the
  demuxer across the full topology matrix: a grouped-then-single-chained file
  (all three grouped BOS pages register under `link_index` 0 at `open`, the
  chained stream registers under `link_index` 1 once its mid-file BOS is read,
  interleaved data pages reassemble per-serial with no cross-contamination,
  non-contiguous EOS pages do not trip link-boundary detection); a
  grouped-then-grouped file (both chained links are groups — the second link's
  grouping does not split into extra links); chained duration as
  `max(group) + next-link` (link-0 max over A/B/C plus link-1 D); and a seek
  into one stream of a grouped link landing on that serial's granule floor
  unperturbed by the other grouped streams' interleaved pages. The
  implementation already handled this shape via `process_page`'s mid-file
  BOS registration and `seen_nonbos_in_current_link` link-boundary tracking;
  these tests pin the behaviour.

### Fixed

- **Skeleton 4.0 index `Segment length in bytes` validity check is now
  chained-file-aware.** Per `docs/container/ogg/ogg-skeleton-4.0.md` §"When
  using the index to seek …" the index is invalid if "The segment doesn't end
  at the segment length offset stored in the Skeleton BOS packet (note that a
  new \"link\" in a \"chain\" can start at the end of the segment)". The check
  previously required `segment_length == file_size`, which is correct only for
  a single-link file — every *chained* file has a declared segment length
  shorter than the whole file (it names where the first link ends), so the
  strict equality wrongly disqualified the entire keyframe index and forced a
  bisection fall-back on every seek of any chained-plus-indexed file. The check
  now accepts a shorter declared length when a fresh `OggS` page (the next
  link's BOS) begins exactly at that offset — the chain boundary the spec
  describes — while still rejecting a declared length that overshoots EOF or
  lands mid-page (the segment was modified since indexing). Single-link
  exact-match and the over-EOF / mid-page rejections are unchanged. New
  `tests/skeleton.rs` cases cover the chained-boundary accept and the
  no-page-boundary reject.

- **Theora duration estimate now unpacks the granuleshift via the Skeleton
  fisbone.** A Theora `granulepos` is the packed
  `(keyframe_idx << shift) | frame_offset` value
  (`docs/container/ogg/ogg-skeleton-4.0.md` §"What decoding-related
  information is needed?"), and the demuxer stamps Theora streams with the
  `1/1_000_000` placeholder time-base because Ogg framing never reveals the
  frame rate. The duration estimate previously fed the raw last-page granule
  straight into the stream time-base, mis-reporting a Theora track's length
  (a granule of 8192 — frame 128 under shift=6 — was read as 0.008 s instead
  of 4.27 s at 30 fps). When a Skeleton `fisbone\0` describes the stream,
  both the `open`-time end-of-file scan (`populate_duration`) and the
  `build_seek_index` recompute (`populate_duration_from_index`) now route the
  last-page granule through the fisbone's `granule_to_seconds`
  (`extract_granules` to undo the keyframe shift, then `granules /
  granulerate`). A `granuleshift == 0` fisbone collapses the extraction to a
  pass-through, so audio mappings that carry a fisbone stay correct; streams
  with no Skeleton or an unusable granule rate fall back to the stream
  time-base unchanged. The seek path already unpacked Theora granules — this
  brings the duration path into agreement with it.

### Added

- **`Skeleton::bones_with_content_kind` + `Skeleton::bones_with_content_type`** —
  the file-level companions to the per-track `FisBone::content_type()`
  accessor, implementing the broadest content-negotiation query the
  `docs/container/ogg/ogg-skeleton-message-headers.wiki` §Content-type field
  (Skeleton 4's only *mandatory* per-track header) was written for. They
  complete the `bones_with_role` / `bones_with_language` resolver family with
  MIME-based lookup. `bones_with_content_kind(&ContentTypeKind)` buckets
  tracks by top-level MIME kind ("which tracks are audio / video / text"),
  comparing well-known buckets by variant and an `Other(token)` kind
  case-insensitively. `bones_with_content_type(mime)` matches the full
  `type/subtype` pair (e.g. `audio/vorbis`) case-insensitively per RFC 2045
  §5.1, ignoring `;parameters` on both the query and the track (so a bare
  `audio/ogg` query matches an `audio/ogg;codecs=opus` track); a `mime`
  argument with no subtype matches nothing. Both return fisbones in BOS
  declaration order and skip tracks whose `Content-Type` is absent or
  fails to parse as a MIME type.

- **`FisBone::dominant_language` + `Skeleton::bones_with_dominant_language`** —
  the dominant-only counterpart to the existing `FisBone::languages` /
  `Skeleton::bones_with_language` pair, implementing the distinguished
  meaning the wiki gives the first list entry in
  `docs/container/ogg/ogg-skeleton-message-headers.wiki` §Language ("The
  Language field will have the dominating language specified as the first
  language. It is possible to specify less non-dominating languages as a
  list after the main language."). `FisBone::dominant_language()` returns
  the first non-empty trimmed tag (`Option<&str>`, `None` when the header
  is absent or expands to zero tags). `Skeleton::bones_with_dominant_language(tag)`
  matches a track only when `tag` is its *first* tag — so a `Language: fr, en`
  dub matches a `"fr"` query but not an `"en"` one — distinguishing "which
  tracks are *primarily* in this language" (default-track selection) from
  the broad "which tracks carry any content in this language" (language
  picker) that `bones_with_language` answers. Case-insensitive per BCP 47
  §2.1.1.

- **`Skeleton::indexed_duration_seconds` + `Skeleton::seek_offset_for_time`** —
  two file-level Skeleton 4.0 index helpers that lift the cross-stream
  algorithms of `docs/container/ogg/ogg-skeleton-4.0.md` §"Keyframe indexes
  for faster seeking" out of the demuxer's internal `seek_to` and into
  reusable methods on the parsed `Skeleton` (the same audience the public
  `read_vbi_u64` / `write_vbi_u64` helpers serve — external seek tooling
  working against a parsed Skeleton).
  - `indexed_duration_seconds()` computes the indexed segment's total
    duration without decoding content: the spec says each `index\0` "stores
    the timestamps of the first and last samples in its track ... you can
    calculate the duration as the end time of the last active stream minus
    the start time of first active stream." The method takes the **minimum**
    first-sample time and the **maximum** last-sample time across every index
    whose `timestamp_denominator` is known (non-zero), combined on a common
    seconds axis so indexes with differing denominators (e.g. a 1/48000 audio
    track alongside a 1/1000 video track) mix correctly. An index with an
    unknown (zero) denominator — the spec's "value was unable to be determined
    at indexing time, and is unknown" case — contributes neither endpoint;
    when no index has a known denominator (Skeleton 3.0, an index-free 4.0
    stream) it returns `None`. File-level companion to the per-index
    `SkelIndex::duration_seconds`.
  - `seek_offset_for_time(target_seconds)` runs the per-spec multi-stream
    minimisation: "first construct the set which contains every active
    streams' last keypoint which has time less than or equal to the seek
    target time ... Then from that set of key points, select the key point
    with the smallest byte offset." For each index with a known denominator
    it takes the per-stream "last keypoint at or before the target" via
    `SkelIndex::keypoint_for_time`, then returns the minimum `KeyPoint::offset`
    across that set — the offset a multi-stream seek should jump to so that
    decoding up to the target passes a keyframe at or before the target on
    *every* concurrently-active stream (a naive single-stream lookup would
    land past another stream's required keyframe). Returns `None` when the
    target precedes every stream's first keypoint or there are no usable
    indexes, so the caller falls back to bisection per the spec's "you must
    gracefully fall-back" rule.
  7 new lib unit tests cover the single-index duration, the cross-stream
  min-first / max-last duration with differing denominators, the
  unknown-denominator skip, the no-index `None`, the multi-stream
  smallest-offset selection, the before-all-keypoints / no-index `None`, and
  the unknown-denominator-index skip in the offset resolver.

- **`Skeleton::bones_by_stack_order`** — file-level resolver that returns
  every fisbone ordered by its stack order, bottom-most (drawn first /
  furthest behind) to front-most (drawn last / on top), per
  `docs/container/ogg/ogg-skeleton-message-headers.wiki` §Altitude ("the
  stack order of the tracks ... an element with greater stack order is
  always in front of an element with a lower stack order"). This is the
  file-level companion to the per-track `FisBone::altitude()` accessor and
  the natural input to a compositor painting a multitrack file (PIP overlay,
  sign-language video, mask). The §Altitude default rule is honoured: a
  track with no `Altitude` header whose `Role` is `*/main` sorts strictly
  below every other track ("By default, a 'main' track is always displayed
  bottom-most unless otherwise defined"), while any track carrying an
  explicit `Altitude` is placed purely by that signed z-index value
  ("otherwise defined"), even a negative one; a non-main track with no
  `Altitude` defaults to the CSS `auto` level of `0`. The sort is **stable**
  so equal-altitude tracks retain BOS declaration order, and a
  present-but-malformed `Altitude` value falls back to the default rule
  rather than failing the query (the skip-malformed tolerance the other
  `Skeleton`-level resolvers use).

## [0.1.6](https://github.com/OxideAV/oxideav-ogg/compare/v0.1.5...v0.1.6) - 2026-06-15

### Other

- preroll-aware seek (Skeleton 4.0 fisbone preroll field)
- typed UTC accessor on FisHead (Skeleton fishead granule-0 wall-clock anchor)
- Skeleton-level track addressing by name / role / language
- substream / cut-in time mapping (basegranule + presentation time)
- granulepos->playback-time mapping (extract_granules / granule_to_seconds)
- build Skeleton 4.0 keyframe index packets while muxing (open_with_skeleton_indexed)
- backfill Skeleton 4.0 fishead segment-length + content-byte-offset at trailer; drain header pages before Skeleton EOS
- Skeleton Track-order addressing (track_order_len/serial/index)
- typed Name accessor on FisBone (Skeleton-4 XML-NCName-shaped track identifier)
- typed Title accessor on FisBone (Skeleton-4 free-text track description)
- typed Content-Type accessor on FisBone (Skeleton-4 mandatory message header)
- typed Display-hint accessor on FisBone (Skeleton-4 rendering-hint header)
- drop release-plz.toml — use release-plz defaults across the workspace
- typed Altitude accessor on FisBone (Skeleton-4 stack-order header)
- typed Role + Language accessors on FisBone (Skeleton-4 message headers)
- typed time-domain accessors on SkelIndex (keypoint_for_time + seconds wrappers)
- Skeleton-aware open_with_skeleton emits fishead BOS + fisbones + EOS
- Theora seek_to via Skeleton fisbone granuleshift + granule_rate
- Skeleton 4.0 multi-stream keyframe-index minimisation in seek_to
- gate Skeleton 4.0 index fast-path on per-spec validity checks
- fuzz parsers + bound SkelIndex allocation by remaining payload
- Skeleton 4.0 index-accelerated seek_to (no bisection, no full-file scan)

### Added

- **Preroll-aware seek: `OggDemuxer::seek_to_with_preroll` +
  `preroll_seek_count` / `input_position`.** Consumes the per-track
  **preroll** field that `docs/container/ogg/ogg-skeleton-4.0.md` §"How
  to describe the logical bitstreams within an Ogg container?" defines as
  "the number of past content packets to take into account when decoding
  the current Ogg page, which is necessary for seeking (vorbis has
  generally 2, speex 3)". The field was already parsed and round-tripped
  on `FisBone` but never used by the demuxer. A bare `seek_to` lands on
  the floor page for the target granule; a codec with inter-packet state
  (window overlap, prediction) resuming there is missing its preroll
  warm-up packets and produces wrong output for the first packets.
  `seek_to_with_preroll(stream_index, pts)` runs the same landing as
  `seek_to`, then rewinds the resume byte offset to an earlier page
  boundary so at least `preroll` content packets of the requested stream
  precede the landed page. The preroll comes from the stream's Skeleton
  `fisbone\0` (looked up by on-wire serial); the codec's `num_headers`
  identification/comment/setup packets (`fisbone` bytes 16..20) are
  excluded so only *content* packets are counted, and the count is taken
  in packets — a page carrying several terminated packets contributes all
  of them. The returned granule is identical to `seek_to`'s (the decode
  target is unchanged; the earlier pages are warm-up the caller decodes
  and discards). With no Skeleton, no fisbone for the stream, a `preroll`
  of 0, or a landing already at the stream's first content page, the call
  is identical to `seek_to`. `preroll_seek_count()` tallies the calls
  that actually moved the offset earlier; `input_position()` returns the
  current resume byte offset for callers comparing the two seek variants.
  7 integration tests in `tests/seek_preroll.rs` cover the
  back-up-two-pages case, the multi-packet-page packet-level count, the
  clamp-to-first-content-page case, the no-op cases (preroll 0, no
  Skeleton, first content page), and the bare-`seek_to` baseline.

- **Typed `fishead` UTC accessor: `FisHead::utc_str` / `FisHead::utc_time`
  + the `Utc` value type.** The Skeleton `fishead` ident packet carries a
  20-byte UTC slot (bytes 44..63) that
  20-byte UTC slot (bytes 44..63) that
  `docs/container/ogg/ogg-skeleton-4.0.md` §"What decoding-related
  information is needed?" defines as the granule-0 → real-world-clock-time
  mapping ("allowing to remember e.g. the recording or broadcast time of
  some content"). The slot was already parsed and round-tripped as a raw
  `[u8; 20]`; these accessors give it a typed view. `utc_str()` returns the
  NUL/whitespace-stripped slot text (`None` when empty) for callers that
  want a verbatim reading without committing to a date format. `utc_time()`
  parses the documented `YYYYMMDDTHHMMSS.sssZ` ISO-8601 *basic* convention
  into a structured `Utc { year, month, day, hour, minute, second,
  fraction }`, following the same three-way `Option<Result<…>>` contract as
  `content_type()` / `altitude()` / `display_hint()`: `None` for an empty
  slot, `Some(Ok)` for the convention, `Some(Err)` for a non-empty slot
  that doesn't match (the spec mandates the field's *meaning* but no byte
  layout, so a non-convention slot is surfaced via `utc_str` rather than
  rejected). Fractional seconds are preserved verbatim (trailing zeros
  intact) and a positive leap second (`:60`) is accepted per ISO 8601.
  `Utc::to_string_basic` re-emits the convention. Spec reference:
  `docs/container/ogg/ogg-skeleton-3.0.md` /
  `docs/container/ogg/ogg-skeleton-4.0.md` §"What decoding-related
  information is needed?".

- **Skeleton-level track addressing: `Skeleton::bone_for_name` /
  `Skeleton::bones_for_name` / `Skeleton::bones_with_role` /
  `Skeleton::bones_with_language`.** Implements the file-level track
  resolution the message-headers wiki
  (`docs/container/ogg/ogg-skeleton-message-headers.wiki`) describes but
  the per-value typed accessors deferred to callers. `bone_for_name`
  resolves the §Name `track[name="…"]` form and enforces its uniqueness
  rule ("otherwise it is undefined which of the tracks is retrieved") by
  returning `None` for an ambiguous (duplicated) name; `bones_for_name`
  is the ambiguity-observing companion. `bones_with_role` is a multi-track
  query (§Role: "The same role can be used across multiple tracks"),
  matching the role tag up to the first `;` case-insensitively so a bare
  `audio/dub` query matches `audio/dub;lang=fr`. `bones_with_language`
  matches a §Language tag anywhere in a track's comma-separated list
  (dominant or not), case-insensitively per BCP 47. All return fisbones
  in BOS declaration order. Five new tests in `tests/skeleton.rs`.

- **Skeleton substream / cut-in time mapping:
  `FisBone::start_seconds` / `FisBone::granule_to_seconds_since_start` /
  `Skeleton::presentation_seconds` / `Skeleton::stream_start_seconds` /
  `Skeleton::substream_granule_to_seconds`.** Implements the substream
  model `docs/container/ogg/ogg-skeleton-4.0.md` §"How to allow the
  creation of substreams from an Ogg physical bitstream?" describes: when
  a subpart is cut out of a larger Ogg file (the spec's `?t=7-59` Web cut
  example), the kept content pages retain their original `granulepos`
  values, the fisbone records the **basegranule** ("the granule number
  with which this logical bitstream starts in the remuxed stream …
  provides for each logical bitstream the accurate start time of its
  data stream"), and the fishead records the **presentation time** ("the
  actual cut-in time and all logical bitstreams are meant to start
  presenting from this time onwards, not from the time their data
  starts"). Both fields were already parsed and round-tripped but had no
  time-mapping accessor. `FisBone::start_seconds` returns the per-track
  data start time `basegranule / granulerate` (un-packed — the
  basegranule names a granule number, not a granuleshift-packed
  `granulepos`); `FisBone::granule_to_seconds_since_start` returns a
  page's elapsed time within the kept segment
  `(extract_granules(granulepos) - basegranule) / granulerate` (negative
  for a surviving preroll page that precedes the cut). At the file level,
  `Skeleton::presentation_seconds` surfaces the fishead cut-in time;
  `Skeleton::stream_start_seconds` adds the fishead **basetime** to the
  per-track start for the file-absolute data-start time; and
  `Skeleton::substream_granule_to_seconds(serial, granulepos)` returns a
  page's position on the cut segment's own playback timeline
  `presentation_time + (extract_granules(granulepos) - basegranule) /
  granulerate` — distinct from `Skeleton::granule_to_seconds`, which
  answers the basetime/granule-0 mapping. Zero-denominator ("unknown")
  rationals contribute `0.0` offsets per the spec convention; a missing
  fishead makes `presentation_seconds` (and thus
  `substream_granule_to_seconds`) `None` because the cut-in time is then
  unknown; the `-1` `granulepos` sentinel and unusable granule rates
  return `None`. 12 new tests (11 lib unit + 1 end-to-end integration that
  models the `?t=7-59` cut, serializes the Skeleton, demuxes it back from
  bytes, and checks every accessor against the parsed-from-wire state,
  including that basetime does not leak into the substream timeline).

- **Skeleton granulepos→playback-time mapping:
  `FisBone::extract_granules` / `FisBone::granule_to_seconds` /
  `Skeleton::granule_to_seconds`.** Implements the two-step granule-to-time
  mapping `docs/container/ogg/ogg-skeleton-4.0.md` §"What decoding-related
  information is needed?" spells out: a content page's raw `granulepos` is
  first parsed to extract a granule value by undoing the per-track
  granuleshift packing ("the number of lower bits from the granulepos
  field that are used to provide position information for sub-seekable
  units (like the keyframe shift in theora)"), then mapped to time as
  `granules / granulerate`. `FisBone::extract_granules` returns the
  granule value (`granulepos` unchanged when `granuleshift == 0`, the
  audio case; `(g >> shift) + (g & ((1 << shift) - 1))` for Theora-style
  packed granulepos; the RFC 3533 §6 `-1` "no packets finish on this
  page" sentinel and a degenerate `shift >= 63` are handled without
  overflow). `FisBone::granule_to_seconds` divides the granule value by
  the fisbone's `granule_rate` rational, returning `None` for the `-1`
  sentinel or an unusable (non-positive numerator/denominator) rate so
  the spec's zero-denominator "unknown" convention surfaces as `None`
  rather than a NaN. `Skeleton::granule_to_seconds(serial, granulepos)`
  is the full absolute mapping: the per-track value plus the fishead
  **basetime** ("provides a mapping for granule position 0 (for all
  logical bitstreams) to a playback time" — the pro-video "starts at
  01:00:00" case), added once on top because basetime is a per-file
  rational shared by every logical bitstream; an unknown
  (denominator-0) or absent basetime contributes a `0.0` offset rather
  than blocking the mapping. 13 new tests (12 lib unit + 1 end-to-end
  integration mapping a demuxed content page's on-wire granulepos
  through the parsed-from-bytes `Skeleton`) cover the unshifted audio
  passthrough, Theora shift-sum extraction, integer and non-integer
  (`30000/1001` NTSC) granule rates, the `-1` sentinel, the
  unusable-rate cases, the basetime offset (including unknown/absent
  basetime), and the unknown-serial path.

- **Muxer-built Skeleton 4.0 keyframe indexes:
  `mux::open_with_skeleton_indexed`.** The muxer can now construct the
  `index\0` packet for each content stream itself — the WRITE-side
  counterpart of the demuxer's index-accelerated `seek_to` — instead
  of only passing through caller-prebuilt indexes. Grounded in
  `docs/container/ogg/ogg-skeleton-4.0.md`: index packets must live in
  the segment's header pages ("all the Skeleton track's index packets
  appear in the header pages of the Ogg segment", so "all the keyframe
  indexes are immediately available once the header packets have been
  read"), but a keypoint's byte offset and the segment's first/last
  sample times are only knowable after the content is written. The
  muxer therefore (1) reserves a fixed-size placeholder `index\0` page
  per auto-indexed stream in `write_header`, emitted between the
  fisbones and the Skeleton EOS per the §"Further restrictions"
  ordering ("Before the Skeleton EOS page in the segment header pages
  come the Skeleton 4.0 keyframe index packets"); (2) records a
  keypoint whenever a page carrying a keyframe-flagged packet
  (`PacketFlags::keyframe`) hits the wire — offset = first byte of the
  page the keyframe packet starts on, timestamp numerator = the
  packet's pts over the stream time-base denominator (spec field 4's
  "must not be 0" denominator is validated at open); and (3) rewrites
  each placeholder page in place at `write_trailer` — same page byte
  length, CRC recomputed per RFC 3533 §6 field 7 — exactly the
  mechanism the r279 fishead segment-length / content-byte-offset
  backfill introduced. The new `AutoIndexConfig` carries the spec's
  thinning recommendation ("we recommend including at most one key
  point per every 64KB of data, or every 1000ms, whichever is least
  frequent") as `min_keypoint_byte_gap` / `min_keypoint_time_gap_ms`
  defaults (64 KiB / 1000 ms; a candidate must clear BOTH gaps) plus a
  `max_keypoints` cap sizing the reservation at `42 + 20·n` bytes
  (worst-case two 10-byte variable-byte integers per keypoint;
  bounded above at 3249 so the packet fits a single 255×255-byte
  page). A partial index is explicitly spec-legal ("a keyframe index
  may not index all keyframes in the Ogg segment"). Bytes past the
  final encoded keypoint remain zero — they lie beyond the *n*
  keypoints field 7 defines ("*n* key points, starting with the first
  keypoint at byte 42"), so conforming readers never consume them.
  The index's first/last-sample-time numerators are filled from the
  first/last observed content-packet pts; streams whose serial already
  carries a caller-supplied `SkelIndex` pass through verbatim and are
  not auto-indexed; a 3.0 fishead or an out-of-range `max_keypoints`
  is rejected at open. 7 new integration tests in
  `tests/skeleton_mux.rs` cover the full producer→consumer loop (mux
  with auto-index → demux → `seek_to` resolves via the Skeleton
  fast path with `skeleton_index_seek_count() == 1`, zero rejects,
  and validity check #1 running in enforcing mode thanks to the r279
  segment-length backfill — then `next_packet` resumes at the
  keypoint's packet), keypoint offsets landing byte-exactly on the
  content stream's data pages, byte-gap and time-gap thinning (only
  the first keypoint survives 20 ms-apart packets under the 1000 ms
  gate; 1 s-apart packets all pass), the `max_keypoints` reservation
  cap with the backfilled page keeping the placeholder's exact byte
  length and every page CRC validating after the rewrite,
  caller-supplied indexes passing through with no auto duplicate, a
  keyframe-less stream backfilling an empty (n = 0) index whose
  first/last-sample-time fields are still measured (seek then falls
  back to bisection per the spec's graceful-fallback rule), the
  multi-stream case emitting one index per content stream with every
  keypoint starting a page of its own stream, and rejection of the
  three invalid configurations (3.0 fishead, zero cap, cap past the
  single-page limit).

- **Mux-side Skeleton 4.0 fishead backfill (segment length + content
  byte offset) and control-section ordering fix.** Two changes to
  `mux::open_with_skeleton`, both grounded in
  `docs/container/ogg/ogg-skeleton-4.0.md`:
  1. **Trailer-time backfill.** The 4.0 fishead carries a *Segment
     length in bytes* field ("if it doesn't match the length stored in
     the Skeleton header packet, you know that either the index is out
     of date, or the file has been chained since indexing") and a
     *Content byte offset* field ("the offset of the first non header
     page in the Ogg segment", letting a player "skip forward to that
     offset, and start decoding from that offset forwards" when it
     delays index loading). Neither value is knowable before the
     segment is fully written, so the muxer previously emitted
     whatever the caller set — typically the constructor's `0`
     ("unknown"), which forced the demuxer's own Skeleton-index
     validity check #1 into its opt-out path on every file this muxer
     produced. `write_trailer` now measures both values (content byte
     offset is recorded as the control section closes in
     `write_header`; segment length is the final stream position) and
     rewrites the fishead BOS page in place — same page length, CRC
     recomputed per RFC 3533 §6 field 7. The backfill is per-field and
     conservative: caller-pre-set non-zero values pass through
     verbatim (a pre-measured remux knows better), only `None`/`0`
     fields are filled, a 3.0 fishead (64-byte layout, no such fields)
     is never touched, and when nothing needs filling the BOS page is
     not rewritten at all.
  2. **Secondary-header pages all precede the Skeleton EOS.** The
     spec's §"Further restrictions" orders the segment as "the
     secondary header pages of all logical bitstreams come next,
     including Skeleton's secondary header packets" and only then "the
     Skeleton EOS page ends the control section of the Ogg stream
     before any content pages of any of the other logical bitstreams
     appear". The muxer's EOS-deferral mechanism (`pending_bytes`)
     held back the last header page of each content stream (e.g. the
     Vorbis setup page) and flushed it only when the first content
     data page arrived — physically *after* the Skeleton EOS, inside
     the content section. `write_header` now drains every content
     stream's held-back page before writing the Skeleton fisbones +
     EOS, so the on-wire order matches the spec and the measured
     content byte offset really is the first non-header page.
  5 new integration tests in `tests/skeleton_mux.rs` cover the wire
  ordering (setup + comment pages before the Skeleton EOS, only
  content data pages after it), the backfilled values round-tripping
  through the demuxer (`segment_length` == physical size,
  `content_byte_offset` == offset of the first page after the Skeleton
  EOS, verified against a raw page walk), caller-pre-set non-zero
  fields surviving verbatim, the 3.0 fishead staying a byte-identical
  64-byte packet, and every page CRC (including the rewritten BOS)
  validating via `crc::validate_page_crc` after the patch.

- **Skeleton "Track order" addressing on `OggDemuxer`.** Three new
  accessors implement the stable per-track index addressing scheme
  documented in `docs/container/ogg/ogg-skeleton-message-headers.wiki`
  §"Track order" ("the means to number through the tracks is by the
  order in which the bos pages of the tracks appear in the Ogg
  stream", with the worked example listing `track[0]: Skeleton BOS`,
  `track[1]: Theora BOS for main video`, `track[2]: Vorbis BOS for
  main audio`, …):
  `OggDemuxer::track_order_len() -> u32` returns the number of
  addressable track slots — the content streams plus the Skeleton
  bitstream when a `fishead\0` BOS is present (the Skeleton occupies
  `track[0]` but is not a content stream, so it never appears in
  `streams()`); `OggDemuxer::track_order_serial(track_index) ->
  Option<u32>` resolves a `track[n]` index to the logical bitstream's
  on-wire `bitstream_serial_number`, mapping `track[0]` to the
  Skeleton serial and each subsequent index to the content stream
  whose BOS page appears next (the dense `StreamInfo::index` is
  already assigned in BOS-discovery order, so the mapping is
  `track[n] -> content stream index n-1` for a Skeleton-bearing file
  and `track[n] -> content stream index n` for a Skeleton-free file —
  the wiki only reserves `track[0]` for Skeleton when Skeleton is
  present); and `OggDemuxer::track_order_index(serial) -> Option<u32>`
  is the reverse map (the Skeleton serial → `Some(0)`, a content
  serial → its `track[n]` index, an unseen serial → `None`). The
  returned serial round-trips through `Skeleton::bone_for_serial` so a
  caller walking `0..track_order_len()` recovers each track's fisbone
  metadata in the spec-defined order — the property a JavaScript-style
  `track[name=…]` / `track[n]` resolver depends on. 4 new integration
  tests cover the single-stream-with-Skeleton layout (track[0] =
  Skeleton, track[1] = content + fisbone-`Name` round-trip), the
  multi-stream layout (Skeleton + two Vorbis tracks in BOS order, each
  walking back to its `stream_a` / `stream_b` fisbone), the
  Skeleton-free file (no reserved `track[0]` slot — the content stream
  is `track[0]`), out-of-range / unseen-serial returning `None`, and a
  full-walk round-trip asserting `track_order_index ∘
  track_order_serial` is the identity permutation over
  `0..track_order_len()`.

- **Typed `Name` accessor on Skeleton-4 `FisBone`.** A new
  `FisBone::name() -> Option<Name>` parses the stable per-track
  identifier message header documented in
  `docs/container/ogg/ogg-skeleton-message-headers.wiki` §Name
  ("This field provides the opportunity to associate a free text
  string with the track to allow direct addressing of the track
  through its name", worked example `track[name="Madonna_singing"]`)
  into a new `Name` struct. The wiki specifies the allowed character
  set verbatim — it is the XML 1.0 `NCName` production: the first
  character has to be one of `[A-Z] | "_" | [a-z] | [#xC0-#xD6] |
  [#xD8-#xF6] | [#xF8-#x2FF] | [#x370-#x37D] | [#x37F-#x1FFF] |
  [#x200C-#x200D] | [#x2070-#x218F] | [#x2C00-#x2FEF] |
  [#x3001-#xD7FF] | [#xF900-#xFDCF] | [#xFDF0-#xFFFD] |
  [#x10000-#xEFFFF]`, and any following character may additionally
  be one of `"-" | "." | [0-9] | #xB7 | [#x0300-#x036F] |
  [#x203F-#x2040]`. `Name::raw` returns the trimmed on-wire value
  (whitespace dropped — same HTTP-style framing tolerance as the
  other typed accessors) so the value round-trips back through
  `set_header` byte-for-byte, and `Name::is_well_formed` returns the
  grammar check against the two §Name allow-lists so callers that
  want to surface the value to a `track[name=…]` resolver can gate
  on validity before publishing the name. A small `Name::is_empty`
  predicate covers the present-but-blank shape a malformed encoder
  might emit. Name is optional per the wiki — only `Content-Type`
  is mandatory — so the accessor returns `Option<Name>`. The wiki's
  per-stream uniqueness rule ("The name needs to be unique between
  all the track names, otherwise it is undefined which of the tracks
  is retrieved when addressing by name") is a file-level invariant
  enforced by callers via `Skeleton::bone_for_serial`, not inside
  this per-value parser. 17 new lib unit tests cover the wiki worked
  example (`Madonna_singing` round-trip + well-formed predicate),
  surrounding-whitespace trimming, rejection of every documented
  first-character violation (digit prefix `9-track`, hyphen prefix
  `-track`, dot prefix `.hidden`, middle-dot prefix), acceptance of
  underscore-start (`_internal`) + letter-start with following-
  character mix (`track-2.audio_main`), internal-space rejection,
  every special punctuation rejection (`@ : / ( ) , = "`), the
  empty-after-trim shape returning `false`, a non-ASCII letter
  start (`épisode`, U+00E9 inside `[#xD8-#xF6]`), the
  following-character middle dot (`Bel·la`, U+00B7), header-absent
  returning `None`, case-insensitive header-name lookup (`NAME:`
  resolves through the same accessor), round-tripping through
  `FisBone::to_bytes` / `parse`, and `set_header` case-insensitive
  replace semantics reflected in the typed view.

- **Typed `Title` accessor on Skeleton-4 `FisBone`.** A new
  `FisBone::title() -> Option<Title>` parses the free-text
  track-description message header documented in
  `docs/container/ogg/ogg-skeleton-message-headers.wiki` §Title
  ("A free text field to provide a description of the track content.")
  into a new `Title` struct. The wiki's worked example
  `Title: "the French audio track for the movie"` is shown wrapped in
  literal double-quote characters; the wiki neither requires nor
  forbids them elsewhere in the message-header block, so `Title`
  exposes two complementary views to keep both readings reachable
  without losing information: `Title::raw` returns the trimmed value
  exactly as the header carries it (quotes preserved, surrounding
  whitespace dropped — same HTTP-style framing tolerance as `role()`,
  `languages()`, `altitude()`, `display_hint()`, and `content_type()`)
  so callers that round-trip back through `set_header` get the same
  on-wire bytes, and `Title::display` strips a single balanced pair
  of surrounding `"…"` quotes when present so callers that follow the
  wiki's worked-example reading get a quote-free string. A small
  `Title::is_empty` predicate covers the present-but-blank shape a
  malformed encoder might emit. Title is optional per the wiki —
  only `Content-Type` is mandatory — so the accessor returns
  `Option<Title>` (rather than `Option<Result<Title>>` like
  `content_type()`): every well-formed `Title:` header parses
  successfully because the field is unstructured by spec. 12 new
  lib unit tests cover the wiki worked example (outer-quote strip),
  an unquoted free-text value (display === raw), surrounding-
  whitespace trimming, the empty `""` collapse to an empty display
  string, an inner quote that must survive verbatim, unbalanced
  open-only / close-only quotes that must NOT be stripped, a
  single-byte `"` value (below the two-byte balanced-pair threshold),
  header-absent returning `None`, case-insensitive header-name
  lookup (`TITLE:` resolves through the same accessor),
  round-tripping through `FisBone::to_bytes` / `parse`, `set_header`
  case-insensitive replace semantics reflected in the typed view, and
  the all-whitespace blank-value case yielding empty raw + display +
  `is_empty()`.

- **Typed `Content-Type` accessor on Skeleton-4 `FisBone`.** A new
  `FisBone::content_type() -> Option<Result<ContentType>>` parses the
  only **mandatory** Skeleton 4 per-track message-header field
  (`docs/container/ogg/ogg-skeleton-message-headers.wiki` §Content-type,
  also worked-out as `"Content-Type: audio/vorbis"` in
  `docs/container/ogg/ogg-skeleton-4.0.md` §3 and the matching 3.0 doc)
  into a structured (`kind`, `subtype`, `parameters`) triple on a new
  `ContentType` struct. The MIME top-level `type` is bucketed by a new
  `ContentTypeKind` enum (`Audio` / `Video` / `Text` / `Image` /
  `Application`) with case-insensitive matching (RFC 2045 § 5.1: "the
  type, subtype, and parameter names are not case sensitive"); unknown
  top-level types round-trip as `ContentTypeKind::Other(String)`
  preserving the as-written token, so the wiki's "mime types don't
  always provide the right main content type (e.g. application/kate is
  semantically a text format)" pattern survives intact. Five
  convenience predicates (`is_audio` / `is_video` / `is_text` /
  `is_image` / `is_application`), an `as_wire` getter, a
  `ContentType::subtype_eq` case-insensitive subtype compare, and a
  `ContentType::parameter` case-insensitive parameter lookup mirror the
  surface of the existing `role()` accessor. RFC 2045 parameters
  (e.g. `audio/ogg;codecs=opus`, `video/mp4;codecs=avc1.42E01E;profiles=mp42`)
  are split on `;` and `=`, surrounding whitespace is trimmed on every
  token, empty segments are dropped, and `key`-only tokens become
  `(key, "")`. The outer `Option` distinguishes "header absent" (a
  non-conforming fisbone) from "header present", and the inner
  `Result` surfaces parse errors (empty value, missing `/`, empty
  `type` or `subtype`) so the caller can decide whether to skip the
  field or reject the packet. Header-name lookup is case-insensitive
  via the underlying `FisBone::header` path. 21 new lib unit tests
  cover the wiki worked examples (`audio/vorbis`, `video/theora`),
  every well-known top-level kind, case-insensitive bucket matching,
  unknown-type round-trips into `Other`, single + multi-parameter
  forms with order preservation, case-insensitive parameter and
  header-name lookup, surrounding-whitespace trimming on value /
  params, empty-segment tolerance (`;;`), the
  `application/x-ogg-skeleton` self-bitstream form, rejection of every
  malformed shape (missing `/`, empty value, empty `type`, empty
  subtype, blank value), `set_header`-driven replace semantics, and
  mutually-exclusive predicates.

- **Typed `Display-hint` accessor on Skeleton-4 `FisBone`.** A new
  `FisBone::display_hint() -> Option<Result<DisplayHint>>` parses the
  parametric rendering-hint message header documented in
  `docs/container/ogg/ogg-skeleton-message-headers.wiki` §Display-hint
  into one of four discriminated variants on a new `DisplayHint` enum:
  `Pip { x, y, width, height }` for the wiki's 2- and 4-arg
  picture-in-picture forms (`pip(20%,20%)` / `pip(40,40,690,60)`),
  `Mask { image, x, y, width, height }` for the 1-, 3- and 5-arg
  video-mask forms (`mask(url)` / `mask(url,30%,25%)` /
  `mask(url,20,20,400,320)`), `Transparent { percent }` for the
  uniform-transparency hint (`transparent(25%)` worked example,
  spec value range `0..=100`), and `Other { tag, arguments }` for
  forward-compatible / vendor hint tags per the wiki's
  "Currently proposed hints are:" soft-enumeration wording.
  Coordinates carry the wiki's pixel-vs-percent distinction via a
  new `DisplayCoord` enum (`Pixels(i32)` / `Percent(f32)`); each
  argument token is parsed independently so a `pip(50%,30,75%,20)`
  shape with mixed coordinate types round-trips. The outer `Option`
  distinguishes "header absent" from "header present", and the
  inner `Result` surfaces parse errors (missing parentheses, wrong
  argument count for a documented tag, non-numeric coordinate,
  decimal `transparent` percent, or a `transparent` value above 100)
  so callers can decide whether to skip the field or reject the
  packet. Surrounding whitespace on the value and on every argument
  token is trimmed — the same HTTP-style framing tolerance as
  `role()`, `languages()` and `altitude()`. Header-name lookup is
  case-insensitive via the underlying `FisBone::header` path; the
  hint tag itself is matched case-insensitively too (so `PIP(...)`
  parses as `Pip`). 23 new lib unit tests cover every wiki worked
  example for `pip` (2-arg percent, 4-arg pixel), `mask` (1-, 3-,
  5-arg URL forms with `http://` and `file://` schemes), and
  `transparent` (`25%`, `7%`, `0%`, `100%` boundaries), plus
  rejection of every malformed shape spelled out above and the
  Other fall-through for unknown tags.
- **Typed `Altitude` accessor on Skeleton-4 `FisBone`.** A new
  `FisBone::altitude() -> Option<Result<i64>>` parses the stack-order
  message-header field documented in
  `docs/container/ogg/ogg-skeleton-message-headers.wiki` §Altitude. The
  wiki defines Altitude as a CSS-z-index-style signed integer
  ("unlimited negative and positive numbers ... an element with greater
  stack order is always in front of an element with a lower stack
  order") with `Altitude: -150` as the worked example. The accessor's
  outer `Option` distinguishes "header absent" from "header present",
  and the inner `Result` surfaces parse errors (malformed
  non-integer values, decimals, or magnitudes that exceed `i64`) as
  `Err(_)` so callers can decide whether to skip the field or reject
  the packet, instead of silently clamping. Surrounding whitespace on
  the value is trimmed before parsing — the same HTTP-style framing
  tolerance as `role()` and `languages()`. Header-name lookup remains
  case-insensitive via the underlying `FisBone::header` path.
  11 new lib unit tests cover the wiki worked example (`-150`),
  positive / zero / boundary-value (`i64::MAX` / `i64::MIN`)
  round-trips, the surrounding-whitespace trim, the past-`i64::MAX`
  inner `Err`, the non-integer + blank + decimal inner `Err`,
  case-insensitive header-name lookup, and the `set_header`-driven
  replace semantics.
- **Typed `Role` + `Language` accessors for Skeleton-4 message headers.**
  Two new methods on `FisBone` give callers structured access to the
  two best-defined per-track message-header fields in
  `docs/container/ogg/ogg-skeleton-message-headers.wiki`, so they no
  longer have to lower-case-match the raw `header("Role")` /
  `header("Language")` strings themselves.
  - `FisBone::role() -> Option<Role>` parses the value into a tag
    (one of the 24 enumerated `RoleKind` variants for
    `text/* | video/* | audio/*`, mirroring every bullet in
    §Role; vendor / forward-compatible values surface as
    `RoleKind::Other(String)` so the wiki's "Other roles are
    possible, too" note round-trips without loss) plus an ordered
    list of `;key=value` parameters. The wiki's documented example
    `video/alternate;angle=nw` parses to
    `Role { kind: VideoAlternate, parameters: [("angle", "nw")] }`
    and is queryable case-insensitively via `Role::parameter`. Three
    convenience predicates (`RoleKind::is_text` / `is_video` /
    `is_audio`) and a `RoleKind::as_wire` getter keep the typed value
    a drop-in replacement for the raw string.
  - `FisBone::languages() -> Option<Vec<&str>>` parses the comma-
    separated tag list spelled out in §Language ("Language: en-US,
    fr"), preserving the dominating-language-first order, trimming
    surrounding whitespace on every tag and dropping empty fragments.
    No BCP-47 grammar validation is performed inside the parser
    because the wiki references the external BCP 47 / W3C LTLI
    grammar without enumerating it inside the Skeleton spec itself.
  16 new lib unit tests cover the full §Role enumeration (every
  text/* + video/* + audio/* bullet), the parameterised
  `video/alternate;angle=nw` example verbatim, the unknown-tag
  round-trip, case-insensitive tag + parameter + header-name lookup,
  whitespace tolerance, equals-less parameters, multi-parameter
  order preservation, the wiki's `Language: en-US, fr` example, the
  single-tag / blank-value / trailing-comma / surrounding-whitespace
  edge cases, and the "header absent" `Option::None` distinction.

- **Skeleton 4.0 time-domain typed accessors on `SkelIndex`.** Six new
  public methods convert the on-wire numerator-space integers into
  seconds and provide spec-aligned time-keyed lookup, replacing the
  per-call `kp.timestamp as f64 / idx.timestamp_denominator as f64`
  boilerplate callers had to write themselves:
  - `KeyPoint::seconds(timestamp_denominator)` — per-keypoint seconds
    conversion; matches `Rational::to_seconds` and returns 0.0 on a
    zero denominator.
  - `SkelIndex::keypoint_seconds(index)` — `Option<f64>` variant that
    distinguishes "unknown" (denominator 0 per
    `docs/container/ogg/ogg-skeleton-4.0.md` §"Keyframe index packets"
    point 4) from "zero seconds".
  - `SkelIndex::first_sample_seconds()` /
    `SkelIndex::last_sample_seconds()` — typed wrappers around the
    indexed segment's start/end sample times; `None` when the shared
    denominator is 0 ("unable to be determined at indexing time, and
    is unknown" per the spec).
  - `SkelIndex::duration_seconds()` — convenience for
    `last - first` per §"Keyframe indexes for faster seeking" ("you
    can calculate the duration as the end time of the last active
    stream minus the start time of first active stream").
  - `SkelIndex::is_sorted_by_offset()` — validates the §"Keyframe
    index packets" invariant ("The key points are stored in increasing
    order by offset (and thus by presentation time as well)") before
    the binary search trusts it.
  - `SkelIndex::keypoint_for_time(target_seconds)` — `O(log n)` binary
    search returning the index of the last keypoint whose presentation
    time is `<= target_seconds`. Implements the per-stream half of the
    spec's `§"Keyframe indexes for faster seeking"` algorithm: "first
    construct the set which contains every active streams' last
    keypoint which has time less than or equal to the seek target
    time." Works in pure-integer numerator space so floating-point
    rounding around boundary timestamps cannot mis-classify the
    target. Edge cases: target before every keypoint → `None`;
    `+inf` → last index; `-inf` and `NaN` → `None`; empty index or
    zero denominator → `None`; negative timestamps (streams whose
    `presentation_time` precedes granule 0) handled with sign
    preserved.
  Nine new lib unit tests cover exact-boundary lookups, between-
  keypoint targets, before-first / after-last edge cases, NaN /
  infinity inputs, empty / unknown-denominator indexes, single-
  keypoint indexes, and negative-timestamp streams.

- **Skeleton-aware mux: `oxideav_ogg::mux::open_with_skeleton`.** New
  factory accepts an optional [`skeleton::Skeleton`] and emits a
  Skeleton metadata bitstream alongside the content streams, per the
  encapsulation order in `docs/container/ogg/ogg-skeleton-3.0.md` /
  `ogg-skeleton-4.0.md`. The Skeleton `fishead\0` BOS is written as
  the very first BOS page of the physical stream so identification
  takes one read; content streams' BOS pages follow; then each
  `fisbone\0` secondary header and any 4.0 `index\0` packet is
  emitted on its own page; and an empty-payload Skeleton EOS page
  closes the control section before the first content data page.
  When `skeleton.serial` is unset, the muxer assigns a
  non-colliding serial (one past the largest content-stream serial).
  Existing `oxideav_ogg::mux::open` continues to produce byte-
  identical Skeleton-free output by delegating to
  `open_with_skeleton(_, _, None)`. The demuxer's existing Skeleton
  path round-trips the emitted fishead + fisbones + indexes
  verbatim — verified by the new `tests/skeleton_mux.rs` harness
  (BOS ordering, control-section boundary, per-page payload
  classification, round-trip, opt-out behaviour).

- **Codec-aware `seek_to` for Theora streams paired with a Skeleton
  `fisbone\0`.** Theora encodes its page granule as
  `(keyframe_idx << shift) | frame_offset_from_keyframe`, so the raw
  granule value is not a usable comparison axis for a bisection
  driven by a microsecond `pts`. Prior releases rejected every
  Theora `seek_to` with `Error::Unsupported`. When a Skeleton 4.0
  `fisbone\0` is present for the requested stream's serial (per
  `docs/container/ogg/ogg-skeleton-4.0.md`), the demuxer now uses
  the per-stream `granuleshift` and `granule_rate` carried by that
  fisbone: the user's `pts` is rescaled from the stream's
  `time_base` into frame-rate units via [`TimeBase::rescale`] to
  produce the target frame number, and the bisection compares
  `(g >> shift) + (g & ((1 << shift) - 1))` against that target.
  The returned granule is the actual on-wire value of the landed
  page so a downstream Theora decoder can recover the
  `(keyframe_idx, frame_offset)` pair as usual. A new
  `SeekKey::TheoraFrame` strategy is plumbed through both the
  `find_next_page_for_serial` scan and a new
  `OggDemuxer::index_floor_by` lookup so the codec-aware comparison
  axis drives both the pre-built seek-index path
  (`build_seek_index` / `open_indexed`) and the on-demand bisection
  path. Theora without a Skeleton fisbone, or with a fisbone whose
  `granuleshift == 0` (which would collapse the keyframe packing —
  indistinguishable from an encoder that forgot to set the shift),
  continues to return `Error::Unsupported`. Vorbis / Opus / FLAC /
  Speex still drive the bisection on the raw granule via the
  collapsed-to-identity `SeekKey::Identity` strategy, so their
  per-page byte-level behaviour is unchanged.

- **Idempotent Skeleton BOS re-registration in `register_stream`.**
  `OggDemuxer::build_seek_index` re-walks every page header in the
  file after `open` has already drained the BOS + header section,
  including a second visit to the Skeleton BOS. Prior to this
  release the second visit clobbered the in-memory `Skeleton` with
  a fresh empty one (re-running `FisHead::parse` and
  `Skeleton::new` from scratch), wiping the `fisbone\0` /
  `index\0` packets the demuxer had already pushed during the
  initial header walk. Round 227's Theora seek path relies on
  those packets being present *after* a `build_seek_index` call;
  the regression bound is locked in by the new
  `theora_bisection_seek_after_build_seek_index_uses_index_floor`
  test. `register_stream` now short-circuits when the
  `bitstream_serial_number` of the BOS being registered already
  matches `skeleton_serial` *and* a `Skeleton` is already recorded.

### Added (continued)

- **Skeleton 4.0 multi-stream keyframe-index minimisation in the
  fast-path `seek_to`.** `docs/container/ogg/ogg-skeleton-4.0.md`
  §"Keyframe indexes for faster seeking" prescribes that the seek
  algorithm "first construct the set which contains every active
  streams' last keypoint which has time less than or equal to the
  seek target time. … Then from that set of key points, select the
  key point with the smallest byte offset." Prior to this release
  the demuxer's Skeleton-index fast path consulted only the
  requested stream's `index\0` packet, which on a multi-stream file
  (Theora + Vorbis, dual-language audio, …) lands the seek past
  another concurrent stream's required keyframe and leaves that
  stream's decoder unable to resume. `OggDemuxer::seek_to` now
  anchors the lookup on the requested stream's index (which fixes
  the returned-granule mapping into that stream's time-base),
  iterates every *other* Skeleton index in the file, rescales the
  target time into each index's own `timestamp_denominator` units,
  and tracks the minimum byte offset across every floor keypoint.
  The per-keypoint validity check (`OggS` capture pattern +
  `bitstream_serial_number` equality) is then performed against
  the *winning* stream's serial, not the originally-requested
  stream's, so the spec's "after a seek to a keypoint's offset,
  you don't land on a page which belongs to that keypoint's
  stream" rule still gates the chosen offset correctly. Single-
  stream Skeleton-indexed files are byte-identical to r215 — the
  minimisation collapses to the requested stream's floor keypoint
  when no other index is present.

- **Skeleton 4.0 index-validity gating in the fast-path `seek_to`.**
  `docs/container/ogg/ogg-skeleton-4.0.md` §"Keyframe indexes for
  faster seeking" requires that a decoder treat a Skeleton 4.0
  keyframe index as invalid (and fall back to bisection) under any
  of three conditions: the `fishead` BOS `Segment length in bytes`
  field disagrees with the actual file size; a keypoint's stored
  byte offset does not land on a page boundary; or that page's
  `bitstream_serial_number` is not the keypoint's stream serial.
  Prior to this release the Ogg demuxer trusted every Skeleton 4.0
  index it parsed, which means a stale or rewritten file would
  silently jump to junk bytes and a content-mismatched offset
  would land on the wrong stream. `OggDemuxer::seek_to` now runs
  all three checks: (1) a one-shot lazy comparison of the
  recorded `segment_length` against `file_size` (encoders that
  leave `segment_length = 0` opt out of this check, which is the
  prevailing pattern for indexers that don't pre-measure); (2) a
  per-keypoint `OggS`-capture-pattern check at the candidate
  offset; (3) a serial-equality check against the page header
  parsed at that same offset. A failed check is silent — the
  seek still completes via the existing page-level `index_floor`
  / bisection path — but rejections are tallied by the new
  `OggDemuxer::skeleton_index_invalid_count()` accessor so callers
  can surface "this file's index is stale" without losing the
  seek result.

- **Skeleton-parser fuzz target.** A fifth cargo-fuzz harness,
  `skeleton_parse`, hammers `skeleton::FisHead::parse` /
  `FisBone::parse` / `SkelIndex::parse` directly on attacker
  bytes, roundtrips them through `to_bytes`, fuzzes the
  variable-byte integer codec (`write_vbi_u64` → `read_vbi_u64`)
  on fuzz-derived u64 values, and additionally wraps the buffer
  in a synthetic Skeleton BOS page handed to
  `demux::open_concrete` so the demuxer's auto-detect aggregation
  (`OggDemuxer::skeleton()`) is also exercised. None of the
  pre-existing four targets (`page_parse`, `demux_recapture`,
  `granule_walk`, `continued_edge`) reaches the Skeleton parsers
  reliably — random fuzz buffers virtually never begin with
  `fishead\0` / `fisbone\0` / `index\0`. Run with
  `cargo +nightly fuzz run skeleton_parse` from `fuzz/`.

- **Skeleton 4.0 index-accelerated `seek_to`.** When a Xiph
  Skeleton 4.0 `index\0` packet (`docs/container/ogg/ogg-skeleton-4.0.md`)
  was parsed for a content stream's serial, `seek_to` now resolves
  the target timestamp directly from the index's keypoint table —
  no page bisection, no `build_seek_index` pre-scan, no per-page
  tightening pass. The fast path:
  1. converts the target pts (stream time-base units) into the
     index's own timestamp denominator via `TimeBase::rescale`;
  2. binary-searches the (already sorted) keypoint table for the
     largest timestamp `<=` the target;
  3. seeks the input to the keypoint's byte offset and returns
     the keypoint's granule (back-converted through `rescale`).
  Falls through to the existing page-level `index_floor` /
  bisection path when no Skeleton index is available for the
  requested serial (Skeleton 3.0 files, 4.0 files that omit the
  index, or any stream whose serial isn't covered by the index).
  Surfaced via `OggDemuxer::skeleton_index_seek_count()` so callers
  and tests can confirm the fast path actually fired.

### Security

- **`SkelIndex::parse` bounded allocation.** The 42-byte
  `index\0` packet header includes an on-wire `n_keypoints`
  `u64` count. The previous capacity calculation
  (`Vec::with_capacity(n_keypoints.min(u32::MAX as u64) as usize)`)
  trusted that value up to `u32::MAX`, which an attacker could
  set to ~4 billion in a 42-byte packet — pre-allocating
  approximately 96 GB of `KeyPoint` storage before the parse
  loop ever discovered the truncation. The capacity is now
  clamped by the actual remaining payload bytes
  (`(packet.len() - 42) / 2`, since each delta-encoded keypoint
  is a pair of variable-byte integers consuming at minimum two
  bytes), so a tiny attacker packet declares only a tiny
  initial allocation and the parse fails with `Error::Invalid`
  on the truncated body rather than OOM-aborting. New unit
  test `index_capacity_bounded_by_remaining_payload` locks in
  the regression bound.

## [0.1.5](https://github.com/OxideAV/oxideav-ogg/compare/v0.1.4...v0.1.5) - 2026-05-30

### Other

- slice-by-4 CRC-32 + branch-free compute_page_checksum
- decode/encode Ogg Skeleton 3.0 + 4.0 (fishead / fisbone / index)
- avoid per-parse page clone in CRC validation
- public chained-link diagnostic accessors (RFC 3533 §4 + §6 field 5)
- criterion harness for the Ogg framing hot paths
- continued-packet edge target — structured Vorbis BOS + attacker-shaped body pages
- panic-hardening libFuzzer harnesses (page parse + recapture + granule walk)
- public page-level CRC-32 validation helpers (RFC 3533 §6 field 7)
- recapture page sync after a parsing error (RFC 3533 §3, §6 field 1)

### Performance

- **Slice-by-4 CRC-32 fast path.** `crc::checksum` and
  `crc::compute_page_checksum` now advance four input bytes per
  iteration through four pre-shifted advancement tables (`T0..T3`,
  each derived from the same generator polynomial 0x04C11DB7 the
  original byte table used, just one extra zero-byte rank deeper).
  A 0-to-3-byte scalar tail mops up the remainder. The recurrence is
  pinned in unit tests against a verbatim copy of the original scalar
  loop (oracle on lengths 0..65 535) so any future tweak to the
  tables catches a mismatch immediately. On M1 the framing benches
  measure:
  - `page/parse/max` ~493 MiB/s → ~1.2 GiB/s (~2.5×)
  - `page/parse/multi_segment` ~488 MiB/s → ~1.2 GiB/s
  - `page/parse/short` ~489 MiB/s → ~1.3 GiB/s
  - `crc/checksum/65536` ~1.4 GiB/s (previously bound by the
    byte-at-a-time loop)
- **Branch-free `compute_page_checksum`.** The original
  implementation tested `(22..26).contains(&i)` on every input byte
  to substitute the CRC field with zeros. r192 splits the page into
  three straight-line segments (`[..22]`, four-zero CRC-field
  substitute via `advance_four_zero_bytes`, `[26..]`) so the per-byte
  range check is gone. For a max-size 65 KiB page this removes
  65 535 range checks from the hot path.
- **`crc::continue_checksum(state, bytes)`** is a new public helper
  that lets callers feed the CRC state across multiple buffers
  without materialising a concatenated slice — the contract is
  `continue_checksum(0, bytes) == checksum(bytes)`. Verified by an
  associativity test that splits a known payload at every position
  0..200 and confirms both halves rejoin to the one-shot answer.
  Used internally by `compute_page_checksum` to splice the
  zero-CRC-field segment in.

### Added

- Ogg Skeleton metadata bitstream decoding — both versions 3.0 and 4.0
  per `docs/container/ogg/ogg-skeleton-{3,4}.0.md`. New
  `oxideav_ogg::skeleton` module exposes:
  - `Skeleton` — aggregate state for a Skeleton-bearing physical Ogg
    stream (fishead + fisbones + 4.0 indexes), with `bone_for_serial`
    and `index_for_serial` lookups by content-stream serial number.
  - `FisHead` — the `fishead\0` BOS ident packet (Skeleton version,
    presentation time + basetime rationals, UTC slot, and the 4.0-only
    segment-length / content-byte-offset fields). `parse` accepts both
    64-byte (3.0) and 80-byte (4.0) layouts; `to_bytes` emits whichever
    layout matches `self.version`.
  - `FisBone` — the `fisbone\0` secondary header (per-track serial,
    granule-rate rational, basegranule, preroll, granuleshift, and the
    HTTP-style message header fields). `set_header` /
    `header` provide case-insensitive lookup for `Content-Type`,
    `Role`, `Name`, plus any custom fields from the
    `docs/container/ogg/ogg-skeleton-message-headers.wiki` registry.
  - `SkelIndex` + `KeyPoint` — the 4.0 `index\0` keyframe-index
    packet, with delta-decoding of per-keypoint `(offset, timestamp)`
    on parse and delta-encoding on `to_bytes`.
  - `Rational`, `Version` (with `V3_0` / `V4_0` constants and an
    `at_least` ordering helper), and stream-side `is_fishead` /
    `is_fisbone` / `is_index` magic detectors.
  - Public `read_vbi_u64` / `write_vbi_u64` helpers for the Skeleton
    4.0 variable-byte integer encoding (7 bits per byte, terminator
    high-bit-set, little-endian), exercised against the
    `ogg-skeleton-4.0.md` worked example (integer 7843 → `0x23 0xBD`).
- `OggDemuxer::skeleton() -> Option<&Skeleton>`: the demuxer now
  recognises a `fishead\0` BOS as the very first BOS page, parses the
  ident header, routes subsequent fisbone / 4.0 index packets through
  Skeleton's reassembly path, and surfaces the aggregate state via this
  accessor. The Skeleton logical bitstream is **not** added to the
  public `streams()` list because it has no content packets; it exists
  purely to describe the *other* logical bitstreams. The demuxer's
  initial open loop now waits for the Skeleton EOS page (the empty
  packet that closes the control section before any content pages
  appear, per `docs/container/ogg/ogg-skeleton-{3,4}.0.md`) so callers
  can read `skeleton()` immediately after `open` and see every
  fisbone / index packet. Files without a Skeleton stream behave
  exactly as before — `skeleton()` returns `None`, no behaviour
  changes — so the addition is purely additive.

  Adds an integration test (`tests/skeleton.rs`) with five cases:
  the demuxer recovers the fishead, fisbone, and index from a
  hand-synthesised 4.0 Ogg-with-Skeleton; the lookup helpers
  (`bone_for_serial`, `index_for_serial`, case-insensitive
  `MessageHeader::header`) round-trip the same blob; a plain Vorbis
  file with no Skeleton still demuxes and reports `skeleton() = None`;
  a Skeleton 3.0 BOS (64-byte fishead, no index packets) parses with
  `segment_length == None` and `content_byte_offset == None`; and the
  public `is_fishead` / `is_fisbone` / `is_index` magic detectors
  match on the spec's magic bytes. All previously-passing tests still
  pass (page CRC, mux/demux roundtrip, chained-link diagnostics,
  page-loss / framing-error / resync counters, seek bisection + index)
  — Skeleton routing is gated on the file's first BOS being a fishead,
  so it is invisible on non-Skeleton inputs.

### Changed

- `page::Page::parse` now validates the RFC 3533 §6 field 7 CRC by
  streaming through `crc::compute_page_checksum` over the borrowed
  page bytes (which treats the 4-byte CRC field at offset 22..26 as
  zero per the spec) instead of cloning the whole page into a scratch
  `Vec<u8>`, zeroing those four bytes, and then calling `crc::checksum`
  on the clone. Functionally identical (same polynomial, same
  zero-CRC-field convention) but eliminates the per-page allocation +
  memcpy that was paid on every `Demuxer::next_packet` /
  `Page::parse` call. For a max-size 65 KiB page that was a second
  full copy of the page body; for short packet pages it was still a
  ~32-byte heap roundtrip per page. No public-API change — the
  function signature, the `Result<(Page, usize)>` return shape, and
  the `Error::InvalidData` mismatch error are all preserved
  byte-for-byte (the formatted error message is unchanged).

  Headline impact on the release-profile `benches/framing.rs`
  Criterion harness (M1, `--quick`):

  | scenario                  | before (r172) | after (this) |
  |---------------------------|---------------|--------------|
  | `page/parse/short`        | ~416 MiB/s    | ~489 MiB/s   |
  | `page/parse/multi_segment`| ~426 MiB/s    | ~488 MiB/s   |
  | `page/parse/max`          | ~411 MiB/s    | ~493 MiB/s   |

  The end-to-end `demux/walk/vorbis_12pkt` and
  `demux/build_index/vorbis_12pkt` scenarios benefit too, since
  every page header the demuxer consumes flows through this same
  `Page::parse` entry. All 54 unit + integration tests still pass
  (page-CRC validation, mux/demux roundtrip, chained-link
  diagnostics, page-loss / framing-error / resync counters, seek
  bisection + index, fuzz harnesses' invariants). The four
  cargo-fuzz targets keep compiling and running; the parse↔serialize
  inverse-pair invariant in `page_parse` continues to hold because
  CRC validation produces the same accept/reject decisions on the
  same inputs.

### Added

- Public chained-link diagnostic accessors on `OggDemuxer` so external
  tooling can reconstruct the RFC 3533 §4 link partitioning of a file
  without re-scanning every page itself. The demuxer already tracked
  per-stream `link_index` internally (to compute chained-link-aware
  duration in `build_seek_index`); these accessors round-trip that
  state through the public API alongside the existing `hole_count` /
  `framing_error_count` / `resync_count` / `seek_index_len` observability
  surface. New methods:
  - `link_count() -> u32` — number of distinct chained links the demuxer
    has observed so far. The initial BOS section is link 0, so a
    single-link (multiplexed or pure-mono) file always reports `1`; a
    back-to-back concatenation of two independent logical bitstreams
    reports `2`, and so on. Grows lazily as `next_packet` /
    `build_seek_index` walk subsequent BOS-after-non-BOS pages.
  - `stream_link_index(stream_index: u32) -> Option<u32>` — the chained
    link index assigned to a given public stream (returns `None` for an
    out-of-range index). Streams that share a link play concurrently
    (multiplex); streams in different links play sequentially.
  - `stream_serial(stream_index: u32) -> Option<u32>` — the on-wire
    Ogg `bitstream_serial_number` (RFC 3533 §6 field 5) of a given
    public stream, letting callers correlate `oxideav-ogg`'s dense
    `StreamInfo::index` enumeration with the raw page-header serial a
    byte-level scanner would see.

  Adds an integration test (`tests/chained_link_diagnostics.rs`) with
  five cases: a multiplexed BOS section reports `link_count == 1` with
  both streams in link 0 immediately after `open_concrete` (no drain);
  a two-link chain reports `link_count == 1` before `build_seek_index`
  and `link_count == 2` afterwards with the two streams split across
  links 0 and 1; a three-link chain reports `link_count == 3` with each
  stream in a distinct link; out-of-range indices return `None` for both
  accessors; and a chain discovered incrementally via `next_packet`
  grows `link_count` lazily as each new BOS is encountered. No framing
  logic changed — accessors only — so existing diagnostics, fuzz, and
  bench paths are untouched.
- Criterion benchmark harness at `benches/framing.rs` covering the
  Ogg framing hot paths so future optimisation rounds can A/B-test
  changes against fixed scenarios. The harness is self-contained:
  every byte fed into a measured routine is synthesised in-bench
  (via `Page::to_bytes` for raw page scenarios, via the muxer for the
  end-to-end demux scenarios) so no `docs/` fixtures or external
  `.ogg` files are read. Scenarios: `crc/checksum/{64,4096,65536}`
  (raw `crc::checksum` table-lookup loop, byte-throughput);
  `crc/validate_page_crc/{short,max}` (RFC 3533 §6 field 7 standalone
  helper over a single-segment short page and a max-size 255×255
  page); `page/parse/{short,multi_segment,max}` and
  `page/to_bytes/{short,multi_segment,max}` (the parse↔serialize pair
  at the three legal-extreme sizes); `page/lace/{short,exact_255,
  large}` (the segment-table builder, with the exact-multiple-of-255
  zero-terminator branch covered); `demux/walk/vorbis_12pkt` (open +
  drain a 12-packet synthetic Vorbis stream to EOF); and
  `demux/build_index/vorbis_12pkt` (the page-header-only scan that
  backs O(log n) `seek_to`). Run with
  `cargo bench -p oxideav-ogg --bench framing`. Headline numbers from
  a `--quick` smoke (M1, debug deps cached, release bench profile):
  CRC ~566 MiB/s on a 64-byte input and ~560 MiB/s sustained at
  64 KiB; `Page::parse` ~411–426 MiB/s across page sizes; `Page::to_bytes`
  ~434 MiB/s on the max page; end-to-end demux of the 12-packet
  Vorbis blob in ~8 µs (~220 MiB/s); `build_seek_index` of the same
  blob in ~2.8 µs (~643 MiB/s, payload-skipping). No `oxideav-ogg`
  surface changed — harness-only.
- Fourth cargo-fuzz target `continued_edge` (`fuzz/fuzz_targets/continued_edge.rs`)
  that targets the per-stream packet-reassembly machinery — RFC 3533 §6 field 3
  continued-flag cross-check, 255-lacing partial-packet buffering,
  `pending_valid` orphan-drop, §6 field 6 page-loss hole accounting — which
  the existing arbitrary-bytes targets (`page_parse`, `demux_recapture`,
  `granule_walk`) struggle to reach because most random buffers are rejected
  at the BOS walk before the reassembly loop ever runs. The new harness
  **constructs** a valid Vorbis BOS + comment + setup header section, then
  synthesises up to 24 body pages from fuzz-derived descriptors: eight lacing
  patterns including the exact-multiple-of-255 boundary (`[255, 255, 0]`),
  the bare continuation `[255]` with no terminator, segment-table truncation
  by one byte, and the empty page; attacker-driven `continued` / `first` /
  `last` flag bits with a reserved-high-bits escape; attacker-driven
  page-sequence-number deltas (0 = duplicate, 1 = normal, larger = fabricated
  hole, with wrapping); and an optional single-byte global mutation at a
  fuzz-derived offset that triggers CRC-failure resync on top of the
  structural fuzz. The reassembly path is therefore reached on essentially
  every iteration. Per-input allocation stays bounded (≤24 body pages × ~1 KiB)
  so the iteration budget matches the existing three targets; harness-only,
  no `oxideav-ogg` surface changed.
- `fuzz/` cargo-fuzz crate with three libFuzzer harnesses that hammer the Ogg
  framing surface end-to-end on attacker bytes: `page_parse` re-runs
  `Page::parse` at every byte offset (plus the standalone `crc::*` helpers and
  the `page::lace` segment-table builder, with a parse↔serialize inverse-pair
  invariant on every `Ok` parse); `demux_recapture` drives `demux::open` and
  `Demuxer::next_packet` through RFC 3533 §3 / §6 field 1 capture-pattern
  recovery, §6 field 3 continued-flag framing-consistency, and §6 field 6
  page-loss detection, then queries the `hole_count` / `framing_error_count` /
  `resync_count` accessors; `granule_walk` opens via `open_concrete`, runs
  `build_seek_index`, and probes `seek_to` at fuzz-derived granule values
  across every reported stream. Standard `[workspace] members = ["."]` /
  `cargo-fuzz = true` shape; `fuzz/Cargo.lock` is gitignored; no `oxideav-ogg`
  surface added or changed, harness-only.
- Public page-level CRC-32 validation helpers in `crc`: `validate_page_crc`,
  `compute_page_checksum`, `read_page_checksum`, and the `CRC_FIELD_OFFSET` /
  `CRC_FIELD_LEN` constants. The new API lets external tools verify page
  integrity per RFC 3533 §6 field 7 ("a 32 bit CRC checksum of the page
  including header with zero CRC field and page content; the generator
  polynomial is 0x04c11db7") without going through the full `Page::parse`
  packet-reassembly path. Adds an integration test
  (`tests/page_crc.rs`) that mux-builds a multi-page Vorbis stream, walks
  every page, and confirms each page's stored CRC matches the recomputed
  one — plus negative tests that flip a single byte in the payload or the
  header and confirm the validator catches the corruption.

## [0.1.4](https://github.com/OxideAV/oxideav-ogg/compare/v0.1.3...v0.1.4) - 2026-05-24

### Other

- continued-flag framing-consistency checking (RFC 3533 §6 field 3)
- page-loss (hole) detection via page_sequence_number (RFC 3533 §6)
- chained-link-aware duration via build_seek_index (RFC 3533 §4)
- page-level seek index (RFC 3533) — O(log n) seek_to via cached (granule, offset)

### Added

- Page-sync recapture / resynchronisation after a parsing error
  (RFC 3533 §3 "recapture after a parsing error" and §6 field 1
  `capture_pattern`: the `OggS` magic "helps a decoder to find the page
  boundaries and regain synchronisation after parsing a corrupted
  stream. Once the capture pattern is found, the decoder verifies page
  sync and integrity by computing and comparing the checksum.").
  Previously the demuxer hard-errored with `"Ogg: lost page sync"` when
  bytes between pages were not `OggS`, and propagated the CRC-mismatch
  `InvalidData` when a page header was syntactically valid but its body
  failed the checksum. Both failure modes now drive a forward scan for
  the next `OggS` whose full page re-parses with a matching CRC; demux
  resumes there. False-positive captures inside other pages' payloads
  are skipped because their checksums fail; the scanner runs only
  after a parse error, so embedded `OggS` bytes in *intact* payloads
  are never re-examined and cannot cause spurious resyncs.
- `OggDemuxer::resync_count`: returns the number of successful
  recoveries the demuxer has performed (0 for a clean file). Each
  recovery counts as one resync regardless of how many bytes had to
  be skipped. Distinct from `hole_count()` (a `page_sequence_number`
  gap) and `framing_error_count()` (a `continued`-flag mismatch within
  a sequence-consistent run): byte-level corruption that destroys
  whole pages ticks both `resync_count` and `hole_count`; garbage
  that sits between page boundaries with no sequence-number loss
  ticks only `resync_count`.
- Page-loss (hole) detection via the `page_sequence_number` field
  (RFC 3533 §6 field 6: the per-stream sequence number increases by one
  per page "so the decoder can identify page loss"). The demuxer tracks
  each logical stream's expected next sequence number; a consumed page
  whose `seq_no` is not exactly `last_seq + 1` (with wrapping) signals
  one or more dropped pages. Each gap counts as one hole regardless of
  how many pages went missing.
- Spanning-packet integrity across holes: when a packet was being
  reassembled across pages and a hole occurs, the buffered partial
  bytes are discarded and any orphaned continuation fragment on the
  next page (a packet tail whose head was lost) is dropped rather than
  spliced into a corrupt packet. Packets fully present after the hole
  are still delivered intact.
- `OggDemuxer::hole_count`: returns the number of page-loss holes
  detected so far across all logical streams (0 for a clean file).

- Continued-flag framing-consistency checking (RFC 3533 §6 field 3,
  header_type bit 0x01: "set: page contains data of a packet continued
  from the previous page; unset: page contains a fresh packet"). The
  demuxer now cross-checks the `continued` bit against its own packet
  reassembly state on every consumed page and flags a framing error when
  the two disagree, independent of any `page_sequence_number` gap:
  a page that sets the bit while no partial packet is buffered (orphaned
  continuation tail), or a page that clears the bit while a partial packet
  is still pending (the previous page promised a continuation, this page
  abandons it). In both cases the affected fragment is dropped rather than
  spliced, so every delivered packet stays individually well-formed. This
  catches corruption *within* a sequence-consistent page run (e.g. a
  damaged final segment that flipped a lacing terminator) that the
  page-loss counter cannot see.
- `OggDemuxer::framing_error_count`: returns the number of continued-flag
  framing inconsistencies detected so far (0 for a clean file). A
  discontinuity already attributed to a page-loss hole in the same page is
  not double-counted here, so `hole_count` and `framing_error_count` are
  disjoint tallies.

- Page-level seek index: `OggDemuxer::build_seek_index` walks every Ogg
  page header in the file once (header + segment table only, payloads
  skipped via relative seek) and records `(serial, granule, page_offset)`
  triples into a per-serial sorted vector. Pages with granule `-1`
  (RFC 3533 §6 "no packets finish on this page") are excluded as they
  carry no seek-target information.
- `oxideav_ogg::demux::open_indexed`: convenience constructor that calls
  `open` and then `build_seek_index` before handing back the boxed
  `Demuxer`. Subsequent `seek_to` calls jump straight to the floor entry
  for the target granule, skipping bisection entirely.
- `oxideav_ogg::demux::open_concrete`: returns the concrete `OggDemuxer`
  type (rather than `Box<dyn Demuxer>`) so callers that want to invoke
  `build_seek_index` / `seek_index_len` on demand don't need a downcast.
- Incidental index population: every page read by `read_page` and every
  page header skipped by `find_next_page_for_serial` is now recorded in
  the index, so even files opened with the plain `open()` accumulate an
  index as packets are drained. A subsequent `seek_to` on a previously-
  visited target lands in O(log n) without a re-bisection.
- Chained-link-aware duration: `build_seek_index` now registers
  mid-file BOS pages it encounters (RFC 3533 §4 chained logical
  bitstreams) by parsing each link's identification packet on the fly,
  then recomputes `duration_micros` as the SUM of per-link durations
  for chained files. Single-link multiplexed files keep their previous
  MAX-over-streams behaviour. A new `link_index` field on every
  registered stream tracks which chained link it belongs to (the
  initial BOS section is link 0; each subsequent BOS-after-non-BOS
  increments the counter).


## [0.1.3](https://github.com/OxideAV/oxideav-ogg/compare/v0.1.2...v0.1.3) - 2026-05-06

### Other

- drop stale REGISTRARS / with_all_features intra-doc links
- drop dead `linkme` dep
- auto-register via oxideav_core::register! macro (linkme distributed slice)
- unify entry point on register(&mut RuntimeContext) ([#502](https://github.com/OxideAV/oxideav-ogg/pull/502))

## [0.1.2](https://github.com/OxideAV/oxideav-ogg/compare/v0.1.1...v0.1.2) - 2026-05-03

### Other

- demux + chained_streams: silence rust-1.95 clippy lints
- cargo fmt rustfmt 1.95 chain join
- register chained-stream BOS pages mid-file (RFC 3533 §4)
- replace never-match regex with semver_check = false
- migrate to centralized OxideAV/.github reusable workflows
- pin release-plz to patch-only bumps

## [0.1.1](https://github.com/OxideAV/oxideav-ogg/compare/v0.1.0...v0.1.1) - 2026-04-25

### Other

- drop oxideav-codec/oxideav-container shims, import from oxideav-core
- release v0.0.4

## [0.1.0](https://github.com/OxideAV/oxideav-ogg/compare/v0.0.3...v0.1.0) - 2026-04-19

### Other

- promote to 0.1.0
- bump oxideav-container dep to "0.1"
- drop Cargo.lock — this crate is a library
- bump oxideav-core / oxideav-codec dep examples to "0.1"
- bump to oxideav-core 0.1.1 + codec 0.1.1
- bump oxideav-core + oxideav-codec deps to "0.1"
- thread &dyn CodecResolver through open()
