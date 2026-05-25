#![no_main]

//! Continued-packet edge-case panic-hardening harness for `oxideav-ogg`.
//!
//! The other three fuzz targets — `page_parse`, `demux_recapture`,
//! `granule_walk` — feed totally arbitrary attacker bytes into the
//! demuxer. That is fine for the page-layer parser and the BOS walk,
//! but in practice the vast majority of random buffers are rejected
//! before reaching the per-stream packet-reassembly machinery
//! (continued-flag cross-check, 255-lacing partial-packet buffering,
//! `pending_valid` orphan-drop, page-loss hole accounting). This
//! target builds a *structurally valid* Ogg byte stream out of fuzz-
//! derived parameters — a Vorbis BOS header section plus N body
//! pages with attacker-chosen lacing patterns, continued-bit values,
//! page-sequence-number jumps and segment-table corruption — so the
//! reassembly machinery itself gets hammered.
//!
//! The construction-then-mutation approach is the same idea as
//! libFuzzer's structured grammars: a coarse "shape" template that
//! exercises a specific code path, with attacker-driven detail under
//! the shape. The mutator still gets to flip header bits, reshape
//! the segment table, or repeat / drop pages — anything that triggers
//! one of the continued-flag / orphan / hole edge transitions.
//!
//! Surfaces exercised:
//!
//! * [`oxideav_ogg::demux::open_concrete`] on the constructed stream —
//!   BOS walk + header collection through valid + malformed-by-design
//!   inputs.
//! * [`oxideav_core::Demuxer::next_packet`] drain across pages whose
//!   `continued`-bit, page-sequence-number, lacing-terminator state,
//!   and 255-segment boundaries are chosen by the fuzzer.
//! * Counter accessors `hole_count` / `framing_error_count` /
//!   `resync_count` after the drain. They are interrogated, not
//!   compared to ground truth — the only oracle is "must not panic"
//!   per the clean-room wall (no libogg, no Xiph reference, no
//!   ffmpeg cross-decoder is allowed).
//!
//! Soft invariant that IS checked: every delivered packet's
//! `stream_index` must be inside the bound the demuxer just reported
//! via `streams().len()`. A demuxer that emits an out-of-bound stream
//! index is a guarantee violation regardless of the malformed input.
//!
//! Per-iteration the harness allocates at most a few KB of page
//! bytes. The page count, lacing values and payload sizes are all
//! masked into small ranges so a pathological fuzz input cannot
//! demand a megabyte allocation. This keeps the fuzz iteration
//! budget bounded the same way the other three targets do.

use std::io::Cursor;

use libfuzzer_sys::fuzz_target;
use oxideav_core::{Demuxer as _, Error, NullCodecResolver, ReadSeek};
use oxideav_ogg::demux;
use oxideav_ogg::page::{flags, lace, Page};

/// Stream serial used for the constructed Vorbis logical bitstream.
/// A constant is fine — the demuxer just keys on it; the fuzzer can
/// still corrupt the on-page serial bytes via a header mutation.
const SERIAL: u32 = 0xFEED_F00D;

/// Cap the body-page count we synthesise per iteration. Each page is
/// at most ~1 KiB so the total stays small; libFuzzer keeps the input
/// corpus comfortably bounded.
const MAX_BODY_PAGES: usize = 24;

/// Cap the per-iteration packet drain so a successfully-built stream
/// with a pathological resync loop cannot stall a fuzz iteration.
const MAX_PACKETS: usize = 4096;

fuzz_target!(|data: &[u8]| {
    // Need at least a few bytes to drive the body-page synthesis;
    // otherwise just hand the bytes to the demuxer raw as an extra
    // panic check on the BOS path with empty input.
    if data.len() < 4 {
        let reader: Box<dyn ReadSeek> = Box::new(Cursor::new(data.to_vec()));
        let _ = demux::open_concrete(reader, &NullCodecResolver);
        return;
    }

    // ----------------------------------------------------------------
    // Build a Vorbis header section (BOS + comment + setup) so the
    // demuxer's BOS walk and header-collection loop accept the stream.
    // The bodies that follow are where the interesting fuzzing happens.
    // ----------------------------------------------------------------
    let mut buf: Vec<u8> = Vec::with_capacity(2048);
    buf.extend_from_slice(&header_pages());

    // ----------------------------------------------------------------
    // Synthesise body pages from fuzz bytes. Each body page consumes
    // a small "page descriptor" prefix from the input:
    //   - flag mutator byte (bit 0..2 = continued/first/last hints,
    //     bit 3..7 = attacker-driven extra)
    //   - lacing pattern selector
    //   - payload size nibbles
    //   - sequence-number delta (lets the fuzzer fabricate holes or
    //     duplicate sequence numbers)
    //   - granule delta
    // ----------------------------------------------------------------
    let mut cursor = 0usize;
    let mut seq: u32 = 3; // headers consumed seq 0..=2
    let mut granule: i64 = 0;

    for _ in 0..MAX_BODY_PAGES {
        // Need 6 bytes of descriptor.
        if data.len().saturating_sub(cursor) < 6 {
            break;
        }
        let desc = &data[cursor..cursor + 6];
        cursor += 6;

        // Low three bits map directly to the RFC 3533 §6 field 3
        // header_type flags (CONTINUED | FIRST | LAST). The fuzzer is
        // additionally allowed to OR in attacker bits in the high
        // nibble via the 0x80 escape — those bits are reserved per the
        // RFC but must not panic the parser, so we exercise them too.
        let mut page_flags = desc[0] & 0x07;
        if desc[0] & 0x80 != 0 {
            page_flags |= desc[0] & 0xF8;
        }

        // Lacing pattern selector. Six shapes:
        //   0 — single small segment (terminated)
        //   1 — one 255 segment alone (continuation, no terminator)
        //   2 — [255, small] (single packet ending inside the page)
        //   3 — [small, small] (two whole packets on one page)
        //   4 — [255, 255, 0] (packet exactly = 510 bytes ending here)
        //   5 — [255, 255] (still-unterminated big packet)
        //   6 — empty page (lacing = 0 segments, payload-less keepalive)
        //   7 — segment-table corruption: lacing length declared but
        //       data truncated by one byte (drives the page-reader
        //       NeedMore / resync branch).
        let pattern = desc[1] & 0x07;
        let small_a = (desc[2] as usize) % 200; // <255, terminates
        let small_b = (desc[3] as usize) % 200;

        let (lacing, payload) = match pattern {
            0 => {
                let l = lace(small_a);
                let p = vec![desc[2]; small_a];
                (l, p)
            }
            1 => (vec![255], vec![desc[2]; 255]),
            2 => {
                // [255, small_a] — 255 + small_a byte packet, terminates.
                let mut l = vec![255u8];
                l.push(small_a as u8);
                let mut p = vec![desc[2]; 255];
                p.extend(std::iter::repeat_n(desc[3], small_a));
                (l, p)
            }
            3 => {
                // [small_a, small_b] — two whole packets, both terminate.
                let l = vec![small_a as u8, small_b as u8];
                let mut p = vec![desc[2]; small_a];
                p.extend(std::iter::repeat_n(desc[3], small_b));
                (l, p)
            }
            4 => {
                // [255, 255, 0] — packet of exactly 510 bytes, terminates
                // on the zero-lacing tail (RFC 3533 §6 exact-multiple-of-255
                // boundary case).
                (vec![255u8, 255, 0], vec![desc[2]; 510])
            }
            5 => {
                // [255, 255] — still unterminated, promises continuation
                // into the next page.
                (vec![255u8, 255], vec![desc[2]; 510])
            }
            6 => (Vec::new(), Vec::new()),
            _ => {
                // Pattern 7: build a normal page, then corrupt its
                // length so the page reader sees a truncated body.
                // We emit the corruption by writing a manual page
                // header below; for now mark with empty lacing and
                // handle the truncation when we serialise.
                let mut l = vec![small_a as u8];
                let p = vec![desc[2]; small_a];
                if small_a == 0 {
                    l = vec![1];
                }
                (l, p)
            }
        };

        // Sequence delta — 0 means duplicate (fuzz the duplicate-seq
        // path, which is a hole of u32::MAX since seq.wrapping_add(1)
        // is not equal to seq), 1 means normal, anything else
        // fabricates a forward jump (a real hole). We mask the delta
        // small to avoid wrapping all the way around.
        let seq_delta = desc[4] & 0x0F;
        seq = seq.wrapping_add(seq_delta as u32);

        // Granule delta. Signed-i8 cast so the fuzzer can also set
        // granule to -1 ("no packets finish on this page") via 0xFF.
        granule = granule.wrapping_add((desc[5] as i8) as i64);

        // Serialise the page. For pattern 7 we additionally truncate
        // the *serialised* bytes so the page reader hits a short read
        // inside what looks like a valid page header.
        let page = Page {
            flags: page_flags,
            granule_position: granule,
            serial: SERIAL,
            seq_no: seq,
            lacing,
            data: payload,
        };
        // `to_bytes` would panic if `data.len() != sum(lacing)`; the
        // patterns above are constructed to satisfy that invariant,
        // so the only path that asserts is pattern 7's truncation,
        // which we perform after serialisation. Belt-and-braces:
        // check the invariant before calling to_bytes, and if it
        // somehow drifted, fall back to skipping this page.
        let lacing_sum: usize = page.lacing.iter().map(|&v| v as usize).sum();
        if page.data.len() != lacing_sum || page.lacing.len() > 255 {
            continue;
        }
        let mut bytes = page.to_bytes();

        if pattern == 7 && !bytes.is_empty() {
            // Truncate one byte off the body so the page reader gets
            // a short read for what looks like a valid page header.
            bytes.pop();
        }

        // Inject the page into the stream.
        buf.extend_from_slice(&bytes);
    }

    // ----------------------------------------------------------------
    // Optional global mutation pass: flip one byte in the assembled
    // buffer at a fuzz-derived offset. This adds CRC-failure resync
    // exercise on top of the structurally-malformed-by-design pages
    // above, so the harness also drives the
    // `crc_field_mismatch → resync_to_next_page` path.
    // ----------------------------------------------------------------
    if cursor + 3 <= data.len() && !buf.is_empty() {
        let mut_byte = data[cursor];
        let mut_off = u16::from_le_bytes([data[cursor + 1], data[cursor + 2]]) as usize;
        let off = mut_off % buf.len();
        buf[off] ^= mut_byte;
    }

    // ----------------------------------------------------------------
    // Drive the constructed stream through the demuxer. Every accessor
    // must come back without panicking; out-of-bound stream indexes on
    // delivered packets are a hard invariant.
    // ----------------------------------------------------------------
    let reader: Box<dyn ReadSeek> = Box::new(Cursor::new(buf));
    let mut dmx = match demux::open_concrete(reader, &NullCodecResolver) {
        Ok(d) => d,
        Err(_) => {
            // The mutation may have damaged the BOS header section so
            // badly that open rejected it; that's fine, the other
            // fuzz targets cover the BOS walk thoroughly.
            return;
        }
    };

    let _ = dmx.format_name();
    let _ = dmx.streams();
    let _ = dmx.metadata();
    let stream_bound = dmx.streams().len();

    for _ in 0..MAX_PACKETS {
        match dmx.next_packet() {
            Ok(pkt) => {
                assert!(
                    (pkt.stream_index as usize) < stream_bound,
                    "demuxer emitted packet for stream {} but streams.len()={}",
                    pkt.stream_index,
                    stream_bound,
                );
            }
            Err(Error::Eof) => break,
            Err(_) => break,
        }
    }

    // Counter accessors after the drain. Their values are not asserted
    // — the input is intentionally malformed — but each accessor must
    // simply return without panicking.
    let _ = dmx.hole_count();
    let _ = dmx.framing_error_count();
    let _ = dmx.resync_count();
});

/// Build the three Vorbis header pages: identification (BOS), comment,
/// setup. These satisfy the demuxer's BOS walk + header-collection
/// loop so the synthesised body pages downstream actually reach the
/// per-stream reassembly machinery that this harness is targeting.
///
/// The header payloads are minimal but RFC-3533- / Vorbis-I-shaped
/// enough for `oxideav_ogg::codec_id::detect` and `parse_vorbis_id`
/// to accept them. Hand-written from RFC 5215 §2.1 / spec — no
/// external library code consulted.
fn header_pages() -> Vec<u8> {
    let mut out = Vec::with_capacity(512);

    // Page seq 0: identification packet (BOS).
    let id = vorbis_id_packet();
    out.extend(
        Page {
            flags: flags::FIRST_PAGE,
            granule_position: 0,
            serial: SERIAL,
            seq_no: 0,
            lacing: lace(id.len()),
            data: id,
        }
        .to_bytes(),
    );

    // Page seq 1: comment packet.
    let cmt = vorbis_comment_packet();
    out.extend(
        Page {
            flags: 0,
            granule_position: 0,
            serial: SERIAL,
            seq_no: 1,
            lacing: lace(cmt.len()),
            data: cmt,
        }
        .to_bytes(),
    );

    // Page seq 2: setup packet.
    let setup = vorbis_setup_packet();
    out.extend(
        Page {
            flags: 0,
            granule_position: 0,
            serial: SERIAL,
            seq_no: 2,
            lacing: lace(setup.len()),
            data: setup,
        }
        .to_bytes(),
    );

    out
}

/// Vorbis I identification packet, RFC 5215 §2.1 / Vorbis I spec §4.2.2:
/// type 0x01, "vorbis", version, channels, sample rate, three bitrate
/// fields, blocksize, framing bit. Sample rate 48 kHz, 2 channels.
fn vorbis_id_packet() -> Vec<u8> {
    let mut p = Vec::with_capacity(30);
    p.push(0x01);
    p.extend_from_slice(b"vorbis");
    p.extend_from_slice(&0u32.to_le_bytes()); // vorbis_version
    p.push(2); // audio_channels
    p.extend_from_slice(&48_000u32.to_le_bytes()); // audio_sample_rate
    p.extend_from_slice(&0i32.to_le_bytes()); // bitrate_maximum
    p.extend_from_slice(&128_000i32.to_le_bytes()); // bitrate_nominal
    p.extend_from_slice(&0i32.to_le_bytes()); // bitrate_minimum
    p.push(0xB8); // blocksize_0 | blocksize_1 packed
    p.push(0x01); // framing bit
    debug_assert_eq!(p.len(), 30);
    p
}

/// Minimal Vorbis comment packet: type 0x03, "vorbis", zero-length
/// vendor string, zero user comments, framing bit set.
fn vorbis_comment_packet() -> Vec<u8> {
    let mut p = Vec::new();
    p.push(0x03);
    p.extend_from_slice(b"vorbis");
    p.extend_from_slice(&0u32.to_le_bytes()); // vendor_length = 0
    p.extend_from_slice(&0u32.to_le_bytes()); // user_comment_list_length = 0
    p.push(0x01); // framing bit
    p
}

/// Minimal Vorbis setup packet placeholder: type 0x05, "vorbis", 16
/// zero bytes. Not a valid codebook section, but the demuxer doesn't
/// decode it — it just accumulates the packet as part of the
/// 3-packet header section before delivering data packets.
fn vorbis_setup_packet() -> Vec<u8> {
    let mut p = Vec::new();
    p.push(0x05);
    p.extend_from_slice(b"vorbis");
    p.extend_from_slice(&[0u8; 16]);
    p
}
