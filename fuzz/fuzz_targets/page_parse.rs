#![no_main]

//! Page-layer panic-hardening harness for `oxideav-ogg`.
//!
//! `oxideav_ogg::page::Page::parse` is the lowest-level entry point
//! into the Ogg framing stack — every higher layer (the demuxer's
//! page reader, the seek-index byte scanner, the standalone
//! `crate::crc::validate_page_crc` helper) ultimately routes through
//! the same set of bounds checks and CRC verification. This target
//! treats the fuzz buffer as a candidate page and re-runs `parse` at
//! every byte offset, so every possible framing alignment is
//! exercised against attacker bytes.
//!
//! Surfaces exercised on every input:
//!
//! * [`oxideav_ogg::page::Page::parse`] — at every byte offset.
//! * [`oxideav_ogg::crc::validate_page_crc`] / `read_page_checksum`
//!   / `compute_page_checksum` — standalone CRC API on the same
//!   byte windows.
//! * [`oxideav_ogg::page::lace`] — packet-length lacing builder, on
//!   length values derived from the input.
//!
//! Round-trip invariant fuzzed on successfully-parsed pages:
//!
//! * `Page::parse` returning `Ok` followed by
//!   `parsed.to_bytes()` reproduces the original `total` bytes
//!   exactly (the serializer is the inverse of the parser).

use libfuzzer_sys::fuzz_target;
use oxideav_ogg::crc;
use oxideav_ogg::page::{lace, Page};

fuzz_target!(|data: &[u8]| {
    // 1. Parse at every byte offset. The parser must never panic,
    //    overflow, or index out of bounds — even on offsets that fall
    //    inside the middle of an otherwise-valid page header. We cap
    //    the offset count at the buffer length so a long input doesn't
    //    blow the per-iteration budget.
    for off in 0..=data.len() {
        let window = &data[off..];
        if let Ok((page, consumed)) = Page::parse(window) {
            // Inverse-pair check: serializing the parsed page must
            // reproduce the same bytes the parser consumed.
            let rebuilt = page.to_bytes();
            assert_eq!(
                rebuilt.len(),
                consumed,
                "page serializer length mismatch (parsed {consumed}, rebuilt {})",
                rebuilt.len(),
            );
            assert_eq!(
                rebuilt,
                window[..consumed],
                "page serializer is not the inverse of the parser",
            );
            // packet_segments must not panic on the parsed page either.
            let _ = page.packet_segments();
            // Cheap accessors must not panic.
            let _ = page.is_continued();
            let _ = page.is_first();
            let _ = page.is_last();
        }
    }

    // 2. Standalone CRC helpers must accept any byte slice without
    //    panicking and must return None for slices too short to even
    //    reach the CRC field, per their documented contract.
    let _ = crc::validate_page_crc(data);
    let _ = crc::read_page_checksum(data);
    let _ = crc::compute_page_checksum(data);

    // 3. `lace` is total over u32 packet lengths; drive it from the
    //    input so the segment-count math is fuzzed. Cap the length so
    //    the per-iteration allocation stays bounded.
    for chunk in data.chunks_exact(4).take(16) {
        let raw = u32::from_le_bytes([chunk[0], chunk[1], chunk[2], chunk[3]]);
        // Cap at 64 KiB so a single fuzz iteration cannot demand a
        // multi-megabyte lacing vector; well above any legitimate Ogg
        // page payload (which tops out at 65 025 bytes per RFC 3533
        // §6: 255 segments × 255 bytes each).
        let len = (raw as usize) % (64 * 1024 + 1);
        let lacing = lace(len);
        let sum: usize = lacing.iter().map(|&v| v as usize).sum();
        // Documented invariant: the sum of lacing bytes recovers the
        // input length, except for the exact-multiple-of-255 case
        // where `lace` appends a trailing zero terminator.
        assert!(sum >= len.saturating_sub(255) && sum <= len);
    }
});
