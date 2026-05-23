//! Integration tests for page-loss (hole) detection via the
//! `page_sequence_number` field (RFC 3533 §6 field 6: the per-stream
//! sequence number exists "so the decoder can identify page loss").
//!
//! The demuxer tracks each logical stream's expected next sequence number.
//! When a consumed page's `seq_no` skips ahead, one or more pages were
//! dropped — a "hole". The demuxer must:
//!   1. count the hole (`OggDemuxer::hole_count`), and
//!   2. NOT splice the two halves of a packet that spanned the lost page(s)
//!      into one corrupt packet; the orphaned continuation tail is dropped.
//!
//! Packets fully present after the hole must still be delivered.

use std::io::Cursor;

use oxideav_core::{Demuxer, ReadSeek};
use oxideav_ogg::page::{flags, lace, Page};

// ─────────────────────────── packet builders ───────────────────────────

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

/// Build a page carrying a single whole packet (the common case).
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

/// Build a page with an explicit lacing table and raw data — used to craft
/// pages whose final segment is a 255 lacing value (an unterminated packet
/// that continues onto the next page).
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

/// Emit the three Vorbis header pages (id/comment/setup), each on its own
/// page with sequence numbers 0..=2. Returns the bytes and the next seq.
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
fn no_holes_on_clean_stream() {
    // Four data pages, each a whole packet, sequence 3..=6, no gaps.
    let mut out = Vec::new();
    let seq_base = header_pages(&mut out);
    for i in 0..4u32 {
        let granule = 960 * (i as i64 + 1);
        let flag = if i == 3 { flags::LAST_PAGE } else { 0 };
        out.extend(whole_page(
            flag,
            granule,
            SERIAL,
            seq_base + i,
            &[0xD0, i as u8],
        ));
    }

    let reader: Box<dyn ReadSeek> = Box::new(Cursor::new(out));
    let mut dmx = oxideav_ogg::demux::open_concrete(reader, &oxideav_core::NullCodecResolver)
        .expect("open clean ogg");

    let mut data_packets = Vec::new();
    while let Ok(p) = dmx.next_packet() {
        data_packets.push(p);
    }
    assert_eq!(data_packets.len(), 4, "all four data packets delivered");
    assert_eq!(dmx.hole_count(), 0, "clean stream has zero holes");
}

#[test]
fn dropped_whole_page_is_counted_once() {
    // Five data pages numbered 3..=7, but we omit the page with seq 5 from
    // the byte stream. The demuxer sees seq jump 4 → 6 and registers one
    // hole. Each data page is a self-contained packet, so no packet spans
    // the hole — every surviving packet is delivered intact.
    let mut out = Vec::new();
    let seq_base = header_pages(&mut out);
    for i in 0..5u32 {
        let seq = seq_base + i;
        if seq == 5 {
            continue; // drop this page entirely
        }
        let granule = 960 * (i as i64 + 1);
        let flag = if i == 4 { flags::LAST_PAGE } else { 0 };
        out.extend(whole_page(flag, granule, SERIAL, seq, &[0xD0, i as u8]));
    }

    let reader: Box<dyn ReadSeek> = Box::new(Cursor::new(out));
    let mut dmx = oxideav_ogg::demux::open_concrete(reader, &oxideav_core::NullCodecResolver)
        .expect("open holey ogg");

    let mut data_packets = Vec::new();
    while let Ok(p) = dmx.next_packet() {
        data_packets.push(p);
    }
    // Four surviving data pages → four packets (the dropped page's packet
    // is simply absent; no garbage spliced in).
    assert_eq!(
        data_packets.len(),
        4,
        "four surviving data packets delivered"
    );
    assert_eq!(dmx.hole_count(), 1, "one gap → one hole");
}

#[test]
fn multi_page_gap_still_one_hole() {
    // Drop two consecutive pages (seq 5 and 6); seq jumps 4 → 7. A single
    // discontinuity, however many pages were lost, counts as one hole.
    let mut out = Vec::new();
    let seq_base = header_pages(&mut out);
    for i in 0..6u32 {
        let seq = seq_base + i;
        if seq == 5 || seq == 6 {
            continue;
        }
        let granule = 960 * (i as i64 + 1);
        let flag = if i == 5 { flags::LAST_PAGE } else { 0 };
        out.extend(whole_page(flag, granule, SERIAL, seq, &[0xD0, i as u8]));
    }

    let reader: Box<dyn ReadSeek> = Box::new(Cursor::new(out));
    let mut dmx = oxideav_ogg::demux::open_concrete(reader, &oxideav_core::NullCodecResolver)
        .expect("open holey ogg");
    while dmx.next_packet().is_ok() {}
    assert_eq!(
        dmx.hole_count(),
        1,
        "two consecutive dropped pages = one hole"
    );
}

#[test]
fn packet_spanning_lost_page_is_dropped_not_corrupted() {
    // Build a packet that spans THREE pages, then drop the middle one. The
    // surviving head + tail must NOT be spliced into one packet. Layout:
    //
    //   seq 3: page A — first 255 bytes of the big packet (lacing [255],
    //          unterminated → continues).
    //   seq 4: page B — middle 255 bytes (CONTINUED, lacing [255]). DROPPED.
    //   seq 5: page C — final 10 bytes (CONTINUED, lacing [10] → terminates).
    //   seq 6: page D — a clean whole packet AFTER the hole.
    //
    // With page B gone the demuxer sees seq 3 → 5: one hole. Page C's
    // leading continuation segment is the orphaned tail of the big packet
    // (its middle is lost), so it is discarded. Page D's whole packet is
    // delivered normally.
    let big_head = vec![0xA1u8; 255];
    let big_tail = vec![0xA3u8; 10];

    let mut out = Vec::new();
    let seq_base = header_pages(&mut out); // 3

    // Page A: head, unterminated (lacing single 255).
    out.extend(raw_page(
        0,
        -1,
        SERIAL,
        seq_base,
        vec![255],
        big_head.clone(),
    ));
    // Page B (seq_base+1): the middle — built but NOT appended (dropped).
    let _dropped = raw_page(
        flags::CONTINUED,
        -1,
        SERIAL,
        seq_base + 1,
        vec![255],
        vec![0xA2u8; 255],
    );
    // Page C (seq_base+2): final fragment, CONTINUED, lacing [10] terminates.
    out.extend(raw_page(
        flags::CONTINUED,
        960,
        SERIAL,
        seq_base + 2,
        vec![10],
        big_tail.clone(),
    ));
    // Page D (seq_base+3): a clean whole packet after the hole, EOS.
    out.extend(whole_page(
        flags::LAST_PAGE,
        1920,
        SERIAL,
        seq_base + 3,
        &[0xDD, 0xDD, 0xDD],
    ));

    let reader: Box<dyn ReadSeek> = Box::new(Cursor::new(out));
    let mut dmx = oxideav_ogg::demux::open_concrete(reader, &oxideav_core::NullCodecResolver)
        .expect("open spanning-hole ogg");

    let mut data_packets = Vec::new();
    while let Ok(p) = dmx.next_packet() {
        data_packets.push(p);
    }

    assert_eq!(dmx.hole_count(), 1, "one dropped middle page = one hole");
    // Exactly ONE data packet survives: the clean post-hole packet (page D).
    // The big spanning packet is gone — neither its head nor a head+tail
    // splice is delivered.
    assert_eq!(
        data_packets.len(),
        1,
        "only the post-hole whole packet survives (got {} packets)",
        data_packets.len()
    );
    assert_eq!(
        data_packets[0].data,
        vec![0xDD, 0xDD, 0xDD],
        "surviving packet is the clean post-hole packet, not a spliced fragment"
    );
    // The orphaned tail bytes (0xA3) must never appear in any delivered packet.
    for p in &data_packets {
        assert!(
            !p.data.contains(&0xA3),
            "orphaned continuation tail leaked into a delivered packet"
        );
    }
}
