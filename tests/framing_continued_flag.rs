//! Integration tests for `continued`-flag framing-consistency checking
//! (RFC 3533 §6 field 3, header_type bit 0x01).
//!
//! The continued bit is a normative declaration about packet reassembly:
//!   * set — the page's first segment continues a packet from the previous
//!     page.
//!   * unset — the page begins a fresh packet.
//!
//! When the bit disagrees with the demuxer's own reassembly state, the page
//! is malformed *independent* of any `page_sequence_number` gap. The demuxer
//! must:
//!   1. count the inconsistency (`OggDemuxer::framing_error_count`), and
//!   2. drop the affected fragment rather than splice mismatched halves
//!      into one corrupt packet.
//!
//! These cases are distinct from page-loss holes (which the sequence-number
//! counter catches): here the sequence numbers are perfectly consecutive but
//! the lacing/continued framing is internally inconsistent — exactly the
//! signature of a corrupted final segment that flipped a terminator.

use std::io::Cursor;

use oxideav_core::{Demuxer, ReadSeek};
use oxideav_ogg::page::{flags, lace, Page};

fn vorbis_id_packet() -> Vec<u8> {
    let mut p = Vec::with_capacity(30);
    p.push(0x01);
    p.extend_from_slice(b"vorbis");
    p.extend_from_slice(&0u32.to_le_bytes());
    p.push(2);
    p.extend_from_slice(&48_000u32.to_le_bytes());
    p.extend_from_slice(&0i32.to_le_bytes());
    p.extend_from_slice(&128_000i32.to_le_bytes());
    p.extend_from_slice(&0i32.to_le_bytes());
    p.push(0xB8);
    p.push(0x01);
    assert_eq!(p.len(), 30);
    p
}

fn vorbis_comment_packet() -> Vec<u8> {
    let mut p = Vec::new();
    p.push(0x03);
    p.extend_from_slice(b"vorbis");
    p.extend_from_slice(&0u32.to_le_bytes());
    p.extend_from_slice(&0u32.to_le_bytes());
    p.push(0x01);
    p
}

fn vorbis_setup_packet() -> Vec<u8> {
    let mut p = Vec::new();
    p.push(0x05);
    p.extend_from_slice(b"vorbis");
    p.extend_from_slice(&[0u8; 16]);
    p
}

fn whole_page(flags_byte: u8, granule: i64, serial: u32, seq: u32, packet: &[u8]) -> Vec<u8> {
    Page {
        flags: flags_byte,
        granule_position: granule,
        serial,
        seq_no: seq,
        lacing: lace(packet.len()),
        data: packet.to_vec(),
    }
    .to_bytes()
}

fn raw_page(
    flags_byte: u8,
    granule: i64,
    serial: u32,
    seq: u32,
    lacing: Vec<u8>,
    data: Vec<u8>,
) -> Vec<u8> {
    Page {
        flags: flags_byte,
        granule_position: granule,
        serial,
        seq_no: seq,
        lacing,
        data,
    }
    .to_bytes()
}

const SERIAL: u32 = 0xFEED_F00D;

/// Emit the three Vorbis header pages, seq 0..=2; returns the next seq (3).
fn header_pages(out: &mut Vec<u8>) -> u32 {
    out.extend(whole_page(
        flags::FIRST_PAGE,
        0,
        SERIAL,
        0,
        &vorbis_id_packet(),
    ));
    out.extend(whole_page(0, 0, SERIAL, 1, &vorbis_comment_packet()));
    out.extend(whole_page(0, 0, SERIAL, 2, &vorbis_setup_packet()));
    3
}

#[test]
fn clean_stream_has_no_framing_errors() {
    // Sanity baseline: a normal stream — some whole, some legitimately
    // spanned packets — must report zero framing errors.
    let mut out = Vec::new();
    let seq = header_pages(&mut out);

    // A packet that legitimately spans two pages: head unterminated on
    // page seq, continuation (bit set) terminating on page seq+1. The
    // continued bit here is CORRECT, so it is NOT a framing error.
    let head = vec![0x11u8; 255];
    let tail = vec![0x22u8; 7];
    out.extend(raw_page(0, -1, SERIAL, seq, vec![255], head));
    out.extend(raw_page(
        flags::CONTINUED,
        960,
        SERIAL,
        seq + 1,
        vec![7],
        tail,
    ));
    // A trailing whole packet, fresh (bit unset), correct.
    out.extend(whole_page(flags::LAST_PAGE, 1920, SERIAL, seq + 2, &[0xAB]));

    let reader: Box<dyn ReadSeek> = Box::new(Cursor::new(out));
    let mut dmx = oxideav_ogg::demux::open_concrete(reader, &oxideav_core::NullCodecResolver)
        .expect("open clean ogg");

    let mut packets = Vec::new();
    while let Ok(p) = dmx.next_packet() {
        packets.push(p);
    }
    assert_eq!(dmx.hole_count(), 0, "no sequence gaps");
    assert_eq!(
        dmx.framing_error_count(),
        0,
        "correct continued framing is not an error"
    );
    // Two delivered packets: the spanned one + the trailing whole one.
    assert_eq!(packets.len(), 2, "both well-framed packets delivered");
    assert_eq!(packets[0].data.len(), 262, "spanned packet reassembled");
    assert_eq!(packets[1].data, vec![0xAB]);
}

#[test]
fn continued_bit_with_nothing_to_continue_is_an_error() {
    // Sequence numbers are perfectly consecutive (no hole), but a data page
    // sets the CONTINUED bit while the previous page terminated all of its
    // packets — there is no partial packet to resume. The leading fragment
    // is an orphaned continuation tail and must be dropped, counted as one
    // framing error (NOT a hole).
    let mut out = Vec::new();
    let seq = header_pages(&mut out);

    // Page seq: a clean whole packet (terminates; leaves nothing pending).
    out.extend(whole_page(0, 960, SERIAL, seq, &[0xC0, 0xC0]));
    // Page seq+1: CONTINUED set, but nothing is pending. Lacing [4] →
    // terminates. The 4 bytes are the orphaned tail of a phantom packet.
    out.extend(raw_page(
        flags::CONTINUED,
        1920,
        SERIAL,
        seq + 1,
        vec![4],
        vec![0x99, 0x99, 0x99, 0x99],
    ));
    // Page seq+2: a clean whole packet after the inconsistency, EOS.
    out.extend(whole_page(flags::LAST_PAGE, 2880, SERIAL, seq + 2, &[0xDD]));

    let reader: Box<dyn ReadSeek> = Box::new(Cursor::new(out));
    let mut dmx = oxideav_ogg::demux::open_concrete(reader, &oxideav_core::NullCodecResolver)
        .expect("open ogg");

    let mut packets = Vec::new();
    while let Ok(p) = dmx.next_packet() {
        packets.push(p);
    }
    assert_eq!(dmx.hole_count(), 0, "sequence numbers are consecutive");
    assert_eq!(
        dmx.framing_error_count(),
        1,
        "one continued-with-no-pending inconsistency"
    );
    // Two packets survive: the first whole packet and the post-error whole
    // packet. The orphaned 0x99 fragment is dropped, never spliced.
    assert_eq!(packets.len(), 2, "two well-formed packets delivered");
    assert_eq!(packets[0].data, vec![0xC0, 0xC0]);
    assert_eq!(packets[1].data, vec![0xDD]);
    for p in &packets {
        assert!(
            !p.data.contains(&0x99),
            "orphaned continuation tail leaked into a delivered packet"
        );
    }
}

#[test]
fn fresh_page_abandoning_a_pending_packet_is_an_error() {
    // Sequence numbers are consecutive. The previous page ends on a
    // 255-lacing segment (unterminated → promises a continuation), but the
    // NEXT page declares a fresh packet (CONTINUED bit unset). The promised
    // continuation never comes; the buffered partial head must be dropped,
    // counted as one framing error (NOT a hole).
    let mut out = Vec::new();
    let seq = header_pages(&mut out);

    // Page seq: unterminated head (lacing single 255 → continues).
    out.extend(raw_page(0, -1, SERIAL, seq, vec![255], vec![0x55u8; 255]));
    // Page seq+1: fresh packet (continued UNSET) — abandons the partial.
    // Lacing [3] terminates a brand-new whole packet.
    out.extend(raw_page(
        0,
        2880,
        SERIAL,
        seq + 1,
        vec![3],
        vec![0x77, 0x77, 0x77],
    ));
    // Page seq+2: a clean whole packet, EOS.
    out.extend(whole_page(flags::LAST_PAGE, 3840, SERIAL, seq + 2, &[0xEE]));

    let reader: Box<dyn ReadSeek> = Box::new(Cursor::new(out));
    let mut dmx = oxideav_ogg::demux::open_concrete(reader, &oxideav_core::NullCodecResolver)
        .expect("open ogg");

    let mut packets = Vec::new();
    while let Ok(p) = dmx.next_packet() {
        packets.push(p);
    }
    assert_eq!(dmx.hole_count(), 0, "sequence numbers are consecutive");
    assert_eq!(
        dmx.framing_error_count(),
        1,
        "one fresh-page-abandons-pending inconsistency"
    );
    // The abandoned 0x55 head is dropped. The fresh 0x77 packet and the
    // trailing 0xEE packet survive.
    assert_eq!(packets.len(), 2, "two well-formed packets delivered");
    assert_eq!(packets[0].data, vec![0x77, 0x77, 0x77]);
    assert_eq!(packets[1].data, vec![0xEE]);
    for p in &packets {
        assert!(
            !p.data.contains(&0x55),
            "abandoned partial head leaked into a delivered packet"
        );
    }
}

#[test]
fn hole_does_not_double_count_as_framing_error() {
    // When a page-loss hole already cleared the pending buffer, a
    // continued-bit mismatch on the same page must be attributed to the hole
    // (hole_count == 1) and NOT also counted as a framing error
    // (framing_error_count stays 0). This mirrors the spanning-hole case but
    // asserts the two counters don't both fire for one event.
    let head = vec![0xA1u8; 255];
    let tail = vec![0xA3u8; 10];

    let mut out = Vec::new();
    let seq = header_pages(&mut out); // 3

    // Page seq: head, unterminated (continues).
    out.extend(raw_page(0, -1, SERIAL, seq, vec![255], head));
    // Page seq+1: the middle — DROPPED (built but not appended).
    let _dropped = raw_page(
        flags::CONTINUED,
        -1,
        SERIAL,
        seq + 1,
        vec![255],
        vec![0xA2u8; 255],
    );
    // Page seq+2: final fragment, CONTINUED, terminates. Sequence jumps
    // seq → seq+2: one hole. The continued bit is set but pending was
    // cleared by the hole → would look like a framing error, but the hole
    // owns this event.
    out.extend(raw_page(
        flags::CONTINUED,
        960,
        SERIAL,
        seq + 2,
        vec![10],
        tail,
    ));
    // Page seq+3: clean whole packet, EOS.
    out.extend(whole_page(flags::LAST_PAGE, 1920, SERIAL, seq + 3, &[0xDD]));

    let reader: Box<dyn ReadSeek> = Box::new(Cursor::new(out));
    let mut dmx = oxideav_ogg::demux::open_concrete(reader, &oxideav_core::NullCodecResolver)
        .expect("open ogg");

    while dmx.next_packet().is_ok() {}
    assert_eq!(dmx.hole_count(), 1, "the dropped middle page is one hole");
    assert_eq!(
        dmx.framing_error_count(),
        0,
        "the hole owns the discontinuity; no double-count as a framing error"
    );
}
