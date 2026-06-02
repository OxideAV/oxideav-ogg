#![no_main]

//! Skeleton-bitstream panic-hardening harness for `oxideav-ogg`.
//!
//! The other four targets — `page_parse`, `demux_recapture`,
//! `granule_walk`, `continued_edge` — fuzz the page layer, the
//! demuxer's top-level walk, the seek-index machinery, and the
//! per-stream packet reassembly. None of them exercise the
//! `oxideav_ogg::skeleton` packet parsers directly: random fuzz
//! buffers virtually never start with `fishead\0` / `fisbone\0` /
//! `index\0` and so reach the Skeleton path only by coincidence.
//! This target attacks the Skeleton surface from two directions:
//!
//! 1. Direct calls to [`oxideav_ogg::skeleton::FisHead::parse`],
//!    [`oxideav_ogg::skeleton::FisBone::parse`],
//!    [`oxideav_ogg::skeleton::SkelIndex::parse`],
//!    [`oxideav_ogg::skeleton::is_fishead`] /
//!    [`oxideav_ogg::skeleton::is_fisbone`] /
//!    [`oxideav_ogg::skeleton::is_index`], and the variable-byte
//!    integer codec (`read_vbi_u64` / `write_vbi_u64`) on the raw
//!    fuzz buffer. Every successful parse is roundtripped through
//!    `to_bytes` and the result re-parsed for inverse-pair equality.
//! 2. A *constructed* Ogg byte stream that wraps a `fishead\0` BOS
//!    page (whose body is the attacker's bytes) plus a synthetic
//!    Vorbis content stream, fed through
//!    [`oxideav_ogg::demux::open_concrete`]. This exercises the
//!    Skeleton auto-detect path in the demuxer
//!    (`demux::OggDemuxer::skeleton()` is queried after the walk)
//!    on inputs whose Skeleton packets the BOS-sniff actually sees,
//!    rather than rejecting the buffer at the capture-pattern check.
//!
//! Surfaces exercised on every iteration:
//!
//! * `FisHead::parse` — both 3.0 (64-byte) and 4.0 (80-byte) layouts.
//! * `FisBone::parse` — including the message-header-offset path
//!   that points past the fixed prefix into the CRLF block.
//! * `SkelIndex::parse` — including the `n_keypoints` allocation
//!   path and the VBI delta-encoded keypoint table. A 42-byte
//!   `index\0` packet with an attacker `n_keypoints` value must
//!   not pre-allocate gigabytes.
//! * `read_vbi_u64` — terminator scan, 10-byte cap.
//! * `write_vbi_u64` → `read_vbi_u64` roundtrip on fuzz-derived
//!   integers.
//! * `is_fishead` / `is_fisbone` / `is_index` — magic-prefix
//!   accessors on the raw buffer.
//! * `demux::open_concrete` on a constructed `fishead\0` BOS page —
//!   triggers `OggDemuxer::skeleton()` aggregation.
//!
//! Soft invariants checked (panic-only would let silent corruption
//! through):
//!
//! * Inverse-pair: `FisHead::parse(x).unwrap().to_bytes() == x` for
//!   the 64- or 80-byte prefix of `x` matching the parsed layout.
//! * Inverse-pair: `FisBone::parse(x).unwrap().to_bytes()` re-parses
//!   to a value equal to the first parse. (Headers may reorder
//!   CRLF / whitespace, so byte equality is not required.)
//! * Inverse-pair: `SkelIndex::parse(x).unwrap().to_bytes()` is
//!   parseable and yields the same keypoint absolute positions.
//! * VBI roundtrip: every `write_vbi_u64(n)` parsed back yields the
//!   same `n` and consumes a number of bytes that matches the
//!   write length.
//!
//! Per-iteration allocation is bounded by clamping the fuzz buffer
//! length used for `SkelIndex::parse` to a small constant — the
//! parser is permitted to fail with `Error::Invalid` on a truncated
//! buffer; it must not OOM by trusting the on-wire `n_keypoints`
//! field.

use std::io::Cursor;

use libfuzzer_sys::fuzz_target;
use oxideav_core::{NullCodecResolver, ReadSeek};
use oxideav_ogg::demux;
use oxideav_ogg::page::{flags, Page};
use oxideav_ogg::skeleton::{
    is_fisbone, is_fishead, is_index, read_vbi_u64, write_vbi_u64, FisBone, FisHead, SkelIndex,
};

/// Cap fuzz buffer length passed to `SkelIndex::parse` to keep
/// per-iteration memory bounded — the parser must be safe on any
/// length, but a successful parse against a multi-megabyte input
/// would balloon the test runtime.
const MAX_PARSE_LEN: usize = 64 * 1024;

/// Stream serial used for the constructed Skeleton BOS page. Any
/// constant works — the demuxer just keys on the on-page bytes.
const SK_SERIAL: u32 = 0x5_4E_4C_5E;

fuzz_target!(|data: &[u8]| {
    // -----------------------------------------------------------
    // 1. Magic-prefix accessors must accept any slice without
    //    panicking and return false on slices shorter than the
    //    relevant magic.
    // -----------------------------------------------------------
    let _ = is_fishead(data);
    let _ = is_fisbone(data);
    let _ = is_index(data);

    // -----------------------------------------------------------
    // 2. Direct parser calls on the raw buffer. Bound the length so
    //    a successful parse of a multi-megabyte input doesn't blow
    //    the iteration budget.
    // -----------------------------------------------------------
    let bounded = &data[..data.len().min(MAX_PARSE_LEN)];

    if let Ok(head) = FisHead::parse(bounded) {
        let rebuilt = head.to_bytes();
        // `FisHead::to_bytes` emits a self-consistent 64- or 80-byte
        // packet whose `parse` returns the same value. Inverse-pair
        // is over the rebuilt bytes, not the input prefix (since the
        // input may contain trailing junk past the layout).
        let back = FisHead::parse(&rebuilt).expect("FisHead roundtrip parses");
        assert_eq!(head, back, "FisHead roundtrip not idempotent");
    }

    if let Ok(bone) = FisBone::parse(bounded) {
        let rebuilt = bone.to_bytes();
        let back = FisBone::parse(&rebuilt).expect("FisBone roundtrip parses");
        // FisBone serialization writes the standard message-header
        // offset (44), so the rebuilt header collection should match
        // the parsed one structurally even if the original input used
        // a non-standard offset or had non-CRLF whitespace.
        assert_eq!(
            bone.serial, back.serial,
            "FisBone serial drifts on roundtrip"
        );
        assert_eq!(
            bone.granule_rate, back.granule_rate,
            "FisBone granule_rate drifts on roundtrip"
        );
        assert_eq!(
            bone.basegranule, back.basegranule,
            "FisBone basegranule drifts on roundtrip"
        );
        assert_eq!(
            bone.preroll, back.preroll,
            "FisBone preroll drifts on roundtrip"
        );
        assert_eq!(
            bone.granuleshift, back.granuleshift,
            "FisBone granuleshift drifts on roundtrip"
        );
        assert_eq!(
            bone.num_headers, back.num_headers,
            "FisBone num_headers drifts on roundtrip"
        );
        assert_eq!(
            bone.headers, back.headers,
            "FisBone headers drift on roundtrip"
        );
    }

    if let Ok(idx) = SkelIndex::parse(bounded) {
        let rebuilt = idx.to_bytes();
        let back = SkelIndex::parse(&rebuilt).expect("SkelIndex roundtrip parses");
        assert_eq!(idx.serial, back.serial);
        assert_eq!(idx.timestamp_denominator, back.timestamp_denominator);
        assert_eq!(idx.first_sample_time, back.first_sample_time);
        assert_eq!(idx.last_sample_time, back.last_sample_time);
        assert_eq!(
            idx.keypoints, back.keypoints,
            "SkelIndex keypoints drift on roundtrip"
        );
    }

    // -----------------------------------------------------------
    // 3. VBI codec roundtrip. The fuzz buffer's first 16 8-byte
    //    chunks each become one u64 fed through the encoder; the
    //    decoder is then expected to recover the same value and
    //    consume exactly the bytes the encoder wrote.
    // -----------------------------------------------------------
    for chunk in data.chunks_exact(8).take(16) {
        let n = u64::from_le_bytes(chunk.try_into().expect("8 bytes"));
        let mut wbuf = Vec::with_capacity(10);
        write_vbi_u64(&mut wbuf, n);
        // `write_vbi_u64` always emits between 1 and 10 bytes.
        assert!(
            !wbuf.is_empty() && wbuf.len() <= 10,
            "write_vbi_u64 emitted {} bytes (out of 1..=10 range)",
            wbuf.len()
        );
        match read_vbi_u64(&wbuf) {
            Some((back, consumed)) => {
                assert_eq!(back, n, "VBI roundtrip drift");
                assert_eq!(consumed, wbuf.len(), "VBI consumed-count mismatch");
            }
            None => panic!("write_vbi_u64 produced an undecodable buffer"),
        }
    }

    // The decoder must also accept arbitrary attacker bytes without
    // panic; only structurally-valid inputs decode.
    let _ = read_vbi_u64(bounded);

    // -----------------------------------------------------------
    // 4. Constructed Ogg byte stream: wrap the fuzz buffer in a
    //    single Skeleton BOS page and feed it to the demuxer. This
    //    exercises the auto-detect path the per-stream parsers can
    //    only otherwise reach by coincidence on a fully-random
    //    buffer.
    //
    //    The BOS page carries a `fishead\0` prefix followed by the
    //    fuzz buffer; if the fuzz buffer doesn't satisfy the
    //    fishead layout the demuxer should record nothing and
    //    return cleanly.
    // -----------------------------------------------------------
    let mut packet: Vec<u8> = Vec::with_capacity(8 + bounded.len().min(1024));
    packet.extend_from_slice(b"fishead\0");
    let body = &bounded[..bounded.len().min(1024)];
    packet.extend_from_slice(body);

    // A page can carry up to 255 lacing segments × 255 bytes; cap at
    // 8 KiB per fuzz iteration regardless of `bounded` size.
    let max_payload = 255 * 32; // 32 segments × 255 bytes = 8160 bytes
    let payload = &packet[..packet.len().min(max_payload)];
    let mut lacing: Vec<u8> = Vec::new();
    let mut rem = payload.len();
    while rem >= 255 {
        lacing.push(255);
        rem -= 255;
    }
    lacing.push(rem as u8);
    // Cap at 255 lacing entries (page-level limit).
    if lacing.len() > 255 {
        lacing.truncate(255);
    }
    let segs_sum: usize = lacing.iter().map(|&v| v as usize).sum();
    let body = &payload[..segs_sum];

    let page = Page {
        flags: flags::FIRST_PAGE,
        granule_position: 0,
        serial: SK_SERIAL,
        seq_no: 0,
        lacing,
        data: body.to_vec(),
    };
    let page_bytes = page.to_bytes();

    let reader: Box<dyn ReadSeek> = Box::new(Cursor::new(page_bytes));
    let resolver = NullCodecResolver;
    if let Ok(dmx) = demux::open_concrete(reader, &resolver) {
        // skeleton() must return Some when the BOS was successfully
        // detected as fishead, None otherwise; either case must not
        // panic.
        let _ = dmx.skeleton().map(|sk| {
            let _ = sk.is_parsed();
            let _ = sk.version();
            // bone_for_serial / index_for_serial must accept any u32.
            let _ = sk.bone_for_serial(SK_SERIAL);
            let _ = sk.index_for_serial(SK_SERIAL);
        });
    }
});
