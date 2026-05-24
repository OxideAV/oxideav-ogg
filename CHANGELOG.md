# Changelog

All notable changes to this project will be documented in this file.

The format is based on [Keep a Changelog](https://keepachangelog.com/en/1.0.0/),
and this project adheres to [Semantic Versioning](https://semver.org/spec/v2.0.0.html).

## [Unreleased]

## [0.1.4](https://github.com/OxideAV/oxideav-ogg/compare/v0.1.3...v0.1.4) - 2026-05-24

### Other

- continued-flag framing-consistency checking (RFC 3533 §6 field 3)
- page-loss (hole) detection via page_sequence_number (RFC 3533 §6)
- chained-link-aware duration via build_seek_index (RFC 3533 §4)
- page-level seek index (RFC 3533) — O(log n) seek_to via cached (granule, offset)

### Added

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
