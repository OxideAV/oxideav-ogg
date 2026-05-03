# Changelog

All notable changes to this project will be documented in this file.

The format is based on [Keep a Changelog](https://keepachangelog.com/en/1.0.0/),
and this project adheres to [Semantic Versioning](https://semver.org/spec/v2.0.0.html).

## [Unreleased]

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
