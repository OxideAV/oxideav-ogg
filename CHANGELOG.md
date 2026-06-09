# Changelog

All notable changes to this project will be documented in this file.

The format is based on [Keep a Changelog](https://keepachangelog.com/en/1.0.0/),
and this project adheres to [Semantic Versioning](https://semver.org/spec/v2.0.0.html).

## [Unreleased]

### Added

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
