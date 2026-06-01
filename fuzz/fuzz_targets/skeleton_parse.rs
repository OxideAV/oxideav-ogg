#![no_main]

//! Skeleton-packet panic-hardening harness for `oxideav-ogg`.
//!
//! The four pre-existing harnesses (`page_parse`, `demux_recapture`,
//! `granule_walk`, `continued_edge`) exercise the RFC 3533 page layer
//! and the packet-reassembly demuxer, but they do not directly drive
//! the Xiph Skeleton header-packet parsers (`fishead\0` /
//! `fisbone\0` / `index\0`) on attacker bytes. Skeleton packets reach
//! `FisHead::parse` / `FisBone::parse` / `SkelIndex::parse` whenever a
//! file declares a Skeleton logical bitstream (the demuxer's BOS-walk
//! routes the matching packets to the parsers), so this is a real
//! attacker-byte surface and must also be panic-hardened.
//!
//! Surfaces exercised on every fuzz input:
//!
//! * [`oxideav_ogg::skeleton::FisHead::parse`] at every byte offset.
//! * [`oxideav_ogg::skeleton::FisBone::parse`] at every byte offset.
//! * [`oxideav_ogg::skeleton::SkelIndex::parse`] at every byte offset.
//! * [`oxideav_ogg::skeleton::read_vbi_u64`] on small windows derived
//!   from the input (the SkelIndex keypoint deltas funnel through it).
//! * The packet-type sniffers
//!   [`oxideav_ogg::skeleton::is_fishead`] /
//!   [`oxideav_ogg::skeleton::is_fisbone`] /
//!   [`oxideav_ogg::skeleton::is_index`] on the same windows.
//!
//! Round-trip invariant fuzzed on successfully-parsed packets:
//!
//! * `FisHead::parse → to_bytes → parse` is a fixed point — the
//!   second parse must succeed and produce an equal struct.
//! * `SkelIndex::parse → to_bytes → parse` is a fixed point.
//! * (For `FisBone` only the parse-side is fuzzed; the on-wire form
//!   carries a free-text HTTP-style message-header block that the
//!   spec lets a writer reshape, so `to_bytes` is not byte-identical
//!   to the input in general, only structurally equivalent.)
//!
//! Clean-room wall: spec sources are
//! `docs/container/ogg/ogg-skeleton-3.0.md` /
//! `docs/container/ogg/ogg-skeleton-4.0.md` /
//! `docs/container/ogg/ogg-skeleton-message-headers.wiki` and the
//! crate's own `src/skeleton.rs`. No libogg / libskeleton / libfishead
//! / ffmpeg consulted.

use libfuzzer_sys::fuzz_target;
use oxideav_ogg::skeleton::{
    is_fisbone, is_fishead, is_index, read_vbi_u64, write_vbi_u64, FisBone, FisHead, SkelIndex,
};

/// Cap the per-iteration parse-attempt offset count so an arbitrarily
/// long input cannot blow the libFuzzer per-iteration budget. 256
/// offsets covers every interesting alignment for the small packets
/// Skeleton emits (fishead = 64 / 80 bytes, fisbone fixed prefix = 52
/// bytes, index prefix = 42 bytes).
const MAX_PARSE_OFFSETS: usize = 256;

/// Cap on the keypoint count we will allow the fuzzer to demand
/// inside a synthesised index packet. The on-wire encoding stores
/// the count as a `u64`; a malicious value of `u64::MAX` would make
/// `SkelIndex::parse` loop until it runs out of bytes (which is fine
/// — every iteration consumes at least one byte from a bounded
/// packet) but we cap the synthesised count to keep the harness
/// iteration time predictable. The real parser still has to defend
/// itself against the un-capped value on the random-byte path
/// exercised in (1) below.
const MAX_SYNTH_KEYPOINTS: u64 = 4096;

fuzz_target!(|data: &[u8]| {
    // 1. Drive the three packet parsers at every byte offset. Each
    //    parser MUST return `Result` rather than panic / overflow /
    //    index out of bounds regardless of the byte content. We cap
    //    the number of offsets so a long input does not blow the
    //    per-iteration time budget.
    let max_off = data.len().min(MAX_PARSE_OFFSETS);
    for off in 0..=max_off {
        let window = &data[off..];

        // Cheap sniffers must not panic on any slice.
        let _ = is_fishead(window);
        let _ = is_fisbone(window);
        let _ = is_index(window);

        // FisHead parse + (on success) to_bytes round-trip.
        if let Ok(head) = FisHead::parse(window) {
            let rebuilt = head.to_bytes();
            // The re-parse of a serialised packet must succeed and
            // produce a struct that compares equal to the original.
            let reparsed =
                FisHead::parse(&rebuilt).expect("FisHead::to_bytes output failed to re-parse");
            assert_eq!(
                head, reparsed,
                "FisHead serialize/parse is not a fixed point"
            );
        }

        // FisBone parse only — `to_bytes` re-emits the message-header
        // block in registration order with CRLF terminators, which is
        // not byte-identical to an attacker-shaped on-wire layout
        // (different whitespace, missing trailing CRLF, …). We still
        // confirm the parse path itself is panic-free and that the
        // accessor surface (`header`) does not panic on any decoded
        // bone.
        if let Ok(bone) = FisBone::parse(window) {
            // Looking up a header with arbitrary case must not panic.
            let _ = bone.header("Content-Type");
            let _ = bone.header("role");
            let _ = bone.header("NAME");
            // The structural invariant: serialise + re-parse must
            // round-trip the non-message-header fields (serial,
            // num_headers, granule_rate, basegranule, preroll,
            // granuleshift). The message-header list may differ
            // because the writer normalises whitespace and ordering,
            // so we compare only the fixed-prefix fields.
            let rebuilt = bone.to_bytes();
            let reparsed =
                FisBone::parse(&rebuilt).expect("FisBone::to_bytes output failed to re-parse");
            assert_eq!(
                (
                    bone.serial,
                    bone.num_headers,
                    bone.granule_rate,
                    bone.basegranule,
                    bone.preroll,
                    bone.granuleshift,
                ),
                (
                    reparsed.serial,
                    reparsed.num_headers,
                    reparsed.granule_rate,
                    reparsed.basegranule,
                    reparsed.preroll,
                    reparsed.granuleshift,
                ),
                "FisBone fixed-prefix fields differ after serialize/parse"
            );
        }

        // SkelIndex parse + (on success) to_bytes round-trip.
        if let Ok(idx) = SkelIndex::parse(window) {
            let rebuilt = idx.to_bytes();
            let reparsed =
                SkelIndex::parse(&rebuilt).expect("SkelIndex::to_bytes output failed to re-parse");
            assert_eq!(
                idx, reparsed,
                "SkelIndex serialize/parse is not a fixed point"
            );
        }
    }

    // 2. Variable-byte-integer decoder. The on-wire keypoint encoding
    //    consumes one or more bytes terminated by a high-bit byte;
    //    the decoder must accept any byte slice without panicking.
    //    Drive it on 16 small windows derived from the input.
    let mut cursor = 0;
    for _ in 0..16 {
        if cursor >= data.len() {
            break;
        }
        let take = (data[cursor] as usize % 12).min(data.len() - cursor);
        let window = &data[cursor..cursor + take];
        let _ = read_vbi_u64(window);
        cursor += take.max(1);
    }

    // 3. Round-trip a few attacker-derived u64 values through
    //    write_vbi_u64 + read_vbi_u64 — the encoder is total over
    //    u64, and the decoder must recover the encoded value exactly.
    for chunk in data.chunks_exact(8).take(8) {
        let n = u64::from_le_bytes([
            chunk[0], chunk[1], chunk[2], chunk[3], chunk[4], chunk[5], chunk[6], chunk[7],
        ]);
        let mut buf = Vec::new();
        write_vbi_u64(&mut buf, n);
        let (decoded, used) = read_vbi_u64(&buf).expect("write_vbi_u64 output failed to decode");
        assert_eq!(decoded, n, "vbi round-trip lost value");
        assert_eq!(used, buf.len(), "vbi round-trip consumed wrong byte count");
    }

    // 4. Synthesise structurally-valid packets out of the fuzz input
    //    and feed them through the parsers. This is the same idea as
    //    the `continued_edge` target's structured-construction pass:
    //    most random bytes are rejected at the magic check, so the
    //    interior bounds-checking code paths only get serious
    //    exercise when the prefix matches.
    if data.len() >= 8 {
        // FisHead synthesis. The fuzzer chooses the version field
        // (which gates the 3.0-vs-4.0 size branch) and the trailing
        // 4.0-only fields directly from the input.
        let mut pkt = Vec::with_capacity(80);
        pkt.extend_from_slice(b"fishead\0");
        // version_major (forced into {3,4}), version_minor (0 or 1)
        pkt.extend_from_slice(&((data[0] % 2 + 3) as u16).to_le_bytes());
        pkt.extend_from_slice(&((data[1] % 2) as u16).to_le_bytes());
        // pad to a 64- or 80-byte packet using the input bytes
        let target = if pkt[8] == 4 { 80 } else { 64 };
        while pkt.len() < target {
            pkt.push(data[pkt.len() % data.len()]);
        }
        if let Ok(head) = FisHead::parse(&pkt) {
            let _ = head.to_bytes();
        }
    }

    if data.len() >= 52 {
        // FisBone synthesis: ship the magic, then attacker-chosen
        // message-header-offset (low byte) followed by raw input. The
        // parser must defend against an offset that points past the
        // packet end or before the fixed prefix.
        let mut pkt = Vec::with_capacity(data.len() + 8);
        pkt.extend_from_slice(b"fisbone\0");
        // message-header-offset = attacker-controlled u32
        let off = u32::from_le_bytes([data[0], data[1], data[2], data[3]]);
        pkt.extend_from_slice(&off.to_le_bytes());
        // Pad the rest of the fixed prefix with input bytes.
        pkt.extend_from_slice(&data[4..52.min(data.len())]);
        while pkt.len() < 52 {
            pkt.push(0);
        }
        // Append a chunk of input as the would-be message-header block.
        let trailing = (data[0] as usize) % data.len().clamp(1, 256);
        pkt.extend_from_slice(&data[..trailing.min(data.len())]);
        if let Ok(bone) = FisBone::parse(&pkt) {
            let _ = bone.to_bytes();
        }
    }

    if data.len() >= 42 {
        // SkelIndex synthesis: magic + serial + capped keypoint count
        // + denominator + first/last timestamp + raw deltas. The
        // deltas funnel through `read_vbi_u64`, so the trailing block
        // exercises the keypoint loop.
        let mut pkt = Vec::with_capacity(data.len() + 8);
        pkt.extend_from_slice(b"index\0");
        pkt.extend_from_slice(&data[0..4]); // serial
        let n_kp = u64::from_le_bytes([
            data[4], data[5], data[6], data[7], data[8], data[9], data[10], data[11],
        ]) % MAX_SYNTH_KEYPOINTS;
        pkt.extend_from_slice(&n_kp.to_le_bytes());
        // timestamp_denominator + first_sample_time + last_sample_time
        // — read 24 raw bytes from the input
        pkt.extend_from_slice(&data[12..36.min(data.len())]);
        while pkt.len() < 42 {
            pkt.push(1); // non-zero denominator default
        }
        // Append the rest of the input as keypoint-delta bytes.
        if data.len() > 36 {
            pkt.extend_from_slice(&data[36..]);
        }
        if let Ok(idx) = SkelIndex::parse(&pkt) {
            // Round-trip on the synthesised, successfully-parsed index.
            let rebuilt = idx.to_bytes();
            let reparsed =
                SkelIndex::parse(&rebuilt).expect("synthesised SkelIndex failed to re-parse");
            assert_eq!(idx, reparsed, "synthesised SkelIndex did not round-trip");
        }
    }
});
