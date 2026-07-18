//! Integration tests for the demuxer's per-event damage ledger
//! (`OggDemuxer::damage_events` / `damage_event_total`) and the two
//! tolerance behaviours added with it:
//!
//! * a file that ends *inside* a page (partial transfer) demuxes to a
//!   clean EOF after delivering every complete page, with a
//!   `TruncatedTail` ledger entry — instead of surfacing an I/O-shaped
//!   error;
//! * a page whose `stream_structure_version` is not 0 (RFC 3533 §6
//!   field 2) is skipped through the §3 recapture path like any other
//!   corrupt page, instead of aborting the whole demux.
//!
//! The ledger is the per-event companion of the aggregate
//! `hole_count` / `framing_error_count` / `resync_count` /
//! `duplicate_serial_count` counters: same detection sites, plus
//! position information (byte offset, serial, page sequence) and
//! bounded retention (`MAX_DAMAGE_EVENTS` cap with a running total).

use std::io::Cursor;

use oxideav_core::{Demuxer, Error, ReadSeek};
use oxideav_ogg::demux::{DamageKind, MAX_DAMAGE_EVENTS};
use oxideav_ogg::page::{flags, lace, Page};

// ─────────────────────────── stream builders ───────────────────────────

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
    p
}

fn vorbis_comment_packet() -> Vec<u8> {
    let mut p = vec![0x03];
    p.extend_from_slice(b"vorbis");
    p.extend_from_slice(&0u32.to_le_bytes());
    p.extend_from_slice(&0u32.to_le_bytes());
    p.push(0x01);
    p
}

fn vorbis_setup_packet() -> Vec<u8> {
    let mut p = vec![0x05];
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

const SERIAL: u32 = 0xCAFE_BABE;

/// Emit the three Vorbis header pages (sequence 0..=2) and return the
/// next sequence number a data page should use.
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

/// A complete single-stream file with `n` one-page data packets
/// (payload `[0xD0, i]`), EOS on the last.
fn clean_file(n: u32) -> Vec<u8> {
    let mut out = Vec::new();
    let seq_base = header_pages(&mut out);
    for i in 0..n {
        let flag = if i + 1 == n { flags::LAST_PAGE } else { 0 };
        out.extend(whole_page(
            flag,
            960 * (i as i64 + 1),
            SERIAL,
            seq_base + i,
            &[0xD0, i as u8],
        ));
    }
    out
}

fn open(bytes: Vec<u8>) -> oxideav_ogg::demux::OggDemuxer {
    let reader: Box<dyn ReadSeek> = Box::new(Cursor::new(bytes));
    oxideav_ogg::demux::open_concrete(reader, &oxideav_core::NullCodecResolver).expect("open ogg")
}

/// Drain every packet, panicking on any error other than EOF; returns
/// the payloads.
fn drain(dmx: &mut oxideav_ogg::demux::OggDemuxer) -> Vec<Vec<u8>> {
    let mut out = Vec::new();
    loop {
        match dmx.next_packet() {
            Ok(p) => out.push(p.data),
            Err(Error::Eof) => return out,
            Err(e) => panic!("unexpected demux error: {e}"),
        }
    }
}

/// Byte offset of the Nth `OggS` capture pattern (zero-indexed).
fn nth_oggs_offset(bytes: &[u8], n: usize) -> usize {
    let mut found = 0;
    for i in 0..bytes.len().saturating_sub(4) {
        if &bytes[i..i + 4] == b"OggS" {
            if found == n {
                return i;
            }
            found += 1;
        }
    }
    panic!("only found {found} 'OggS' captures, wanted index {n}");
}

// ─────────────────────────── tests ───────────────────────────

#[test]
fn clean_file_has_an_empty_ledger() {
    let mut dmx = open(clean_file(4));
    assert_eq!(drain(&mut dmx).len(), 4);
    assert!(dmx.damage_events().is_empty(), "clean file → empty ledger");
    assert_eq!(dmx.damage_event_total(), 0);
}

#[test]
fn junk_splice_records_a_resync_event_with_the_landing_offset() {
    let clean = clean_file(4);
    // Splice junk in front of data page 1 (the 5th page, capture #4).
    let splice_at = nth_oggs_offset(&clean, 4);
    let mut bytes = clean[..splice_at].to_vec();
    bytes.extend_from_slice(b"#### spliced junk ####");
    let landing = bytes.len() as u64; // the damaged region ends here
    bytes.extend_from_slice(&clean[splice_at..]);

    let mut dmx = open(bytes);
    let packets = drain(&mut dmx);
    assert_eq!(packets.len(), 4, "no packet is lost to inter-page junk");
    assert_eq!(dmx.resync_count(), 1);
    let events = dmx.damage_events();
    assert_eq!(events.len(), 1, "exactly one ledger entry: {events:?}");
    assert_eq!(events[0].kind, DamageKind::Resync);
    assert_eq!(
        events[0].byte_offset,
        Some(landing),
        "resync event records the landing page's offset"
    );
    assert_eq!(events[0].serial, Some(SERIAL));
    assert_eq!(events[0].page_seq, Some(4), "landed on data page seq 4");
    assert_eq!(dmx.damage_event_total(), 1);
}

#[test]
fn dropped_page_records_a_hole_event() {
    let clean = clean_file(4);
    // Excise data page 1 (capture #4) entirely.
    let start = nth_oggs_offset(&clean, 4);
    let end = nth_oggs_offset(&clean, 5);
    let mut bytes = clean[..start].to_vec();
    bytes.extend_from_slice(&clean[end..]);

    let mut dmx = open(bytes);
    let packets = drain(&mut dmx);
    assert_eq!(packets.len(), 3, "only the excised page's packet is lost");
    assert_eq!(dmx.hole_count(), 1);
    let events = dmx.damage_events();
    assert_eq!(events.len(), 1, "{events:?}");
    assert_eq!(events[0].kind, DamageKind::Hole);
    assert_eq!(events[0].serial, Some(SERIAL));
    assert_eq!(
        events[0].page_seq,
        Some(5),
        "the hole is observed on the page after the gap"
    );
    assert_eq!(
        events[0].byte_offset, None,
        "page-model events carry no offset"
    );
}

#[test]
fn abandoned_partial_records_a_framing_error_event() {
    // Page A ends on a 255 lacing (promising a continuation); page B
    // declares a fresh packet. The orphaned head must be dropped and a
    // FramingError event recorded.
    let mut bytes = Vec::new();
    let seq = header_pages(&mut bytes);
    bytes.extend(
        Page {
            flags: 0,
            granule_position: -1,
            serial: SERIAL,
            seq_no: seq,
            lacing: vec![255],
            data: vec![0xEE; 255],
        }
        .to_bytes(),
    );
    bytes.extend(whole_page(
        flags::LAST_PAGE,
        960,
        SERIAL,
        seq + 1,
        &[0xD0, 7],
    ));

    let mut dmx = open(bytes);
    let packets = drain(&mut dmx);
    assert_eq!(packets, vec![vec![0xD0, 7]], "orphaned head dropped");
    assert_eq!(dmx.framing_error_count(), 1);
    let events = dmx.damage_events();
    assert_eq!(events.len(), 1, "{events:?}");
    assert_eq!(events[0].kind, DamageKind::FramingError);
    assert_eq!(events[0].serial, Some(SERIAL));
    assert_eq!(events[0].page_seq, Some(seq + 1));
}

#[test]
fn orphaned_continuation_records_a_framing_error_event() {
    // Page B claims to continue a packet nobody left open.
    let mut bytes = Vec::new();
    let seq = header_pages(&mut bytes);
    bytes.extend(whole_page(0, 960, SERIAL, seq, &[0xD0, 1]));
    bytes.extend(whole_page(
        flags::CONTINUED | flags::LAST_PAGE,
        1920,
        SERIAL,
        seq + 1,
        &[0xD0, 2],
    ));

    let mut dmx = open(bytes);
    let packets = drain(&mut dmx);
    assert_eq!(packets, vec![vec![0xD0, 1]], "orphaned tail dropped");
    assert_eq!(dmx.framing_error_count(), 1);
    let events = dmx.damage_events();
    assert_eq!(events.len(), 1, "{events:?}");
    assert_eq!(events[0].kind, DamageKind::FramingError);
}

#[test]
fn truncated_tail_is_a_clean_eof_with_a_ledger_entry() {
    let clean = clean_file(4);
    let last_page = nth_oggs_offset(&clean, 6) as u64; // final data page
                                                       // Cut points inside the final page: mid-header, mid-segment-table,
                                                       // and mid-body.
    for cut in [last_page + 12, last_page + 27, clean.len() as u64 - 1] {
        let bytes = clean[..cut as usize].to_vec();
        let mut dmx = open(bytes);
        let packets = drain(&mut dmx); // must reach Eof, not an error
        assert_eq!(
            packets.len(),
            3,
            "every complete page before the cut at {cut} is delivered"
        );
        let events = dmx.damage_events();
        assert_eq!(events.len(), 1, "cut at {cut}: {events:?}");
        assert_eq!(events[0].kind, DamageKind::TruncatedTail);
        assert_eq!(
            events[0].byte_offset,
            Some(last_page),
            "the event pins the incomplete page's start"
        );
    }
}

#[test]
fn version_damaged_page_is_skipped_via_recapture() {
    // Flip data page 1's stream_structure_version to 1 and re-seal its
    // CRC so the version check (not the checksum) is what trips: the
    // demuxer must skip the page through the §3 recapture path and
    // deliver everything else.
    let clean = clean_file(4);
    let off = nth_oggs_offset(&clean, 4);
    let mut bytes = clean.clone();
    bytes[off + 4] = 1;
    // Re-seal: zero the CRC field, recompute over the whole page.
    let page_len = nth_oggs_offset(&clean, 5) - off;
    for b in &mut bytes[off + 22..off + 26] {
        *b = 0;
    }
    let crc = oxideav_ogg::crc::checksum(&bytes[off..off + page_len]);
    bytes[off + 22..off + 26].copy_from_slice(&crc.to_le_bytes());

    let mut dmx = open(bytes);
    let packets = drain(&mut dmx);
    assert_eq!(
        packets.len(),
        3,
        "only the version-damaged page's packet is lost"
    );
    assert_eq!(dmx.resync_count(), 1, "skipped via recapture");
    // Ledger: the resync landing, then the hole where the skipped
    // page's sequence number is missed.
    let kinds: Vec<DamageKind> = dmx.damage_events().iter().map(|e| e.kind).collect();
    assert_eq!(kinds, vec![DamageKind::Resync, DamageKind::Hole]);
}

#[test]
fn continued_packet_damage_is_isolated_to_one_packet() {
    // Packet B spans data pages 1 and 2 (255-byte head + tail); damage
    // the continuation page's body. Packets A and C must survive
    // byte-for-byte; only B is lost.
    let mut bytes = Vec::new();
    let seq = header_pages(&mut bytes);
    let packet_a = vec![0xA1; 40];
    let packet_b: Vec<u8> = (0..400).map(|i| (i & 0xff) as u8).collect();
    let packet_c = vec![0xC3; 25];
    bytes.extend(whole_page(0, 960, SERIAL, seq, &packet_a));
    // Page seq+1: first 255 bytes of B (no packet completes).
    bytes.extend(
        Page {
            flags: 0,
            granule_position: -1,
            serial: SERIAL,
            seq_no: seq + 1,
            lacing: vec![255],
            data: packet_b[..255].to_vec(),
        }
        .to_bytes(),
    );
    // Page seq+2: tail of B.
    let tail_off = bytes.len();
    bytes.extend(
        Page {
            flags: flags::CONTINUED,
            granule_position: 1920,
            serial: SERIAL,
            seq_no: seq + 2,
            lacing: lace(packet_b.len() - 255),
            data: packet_b[255..].to_vec(),
        }
        .to_bytes(),
    );
    bytes.extend(whole_page(
        flags::LAST_PAGE,
        2880,
        SERIAL,
        seq + 3,
        &packet_c,
    ));

    // Corrupt one byte of the continuation page's body.
    bytes[tail_off + 40] ^= 0xFF;

    let mut dmx = open(bytes);
    let packets = drain(&mut dmx);
    assert_eq!(
        packets,
        vec![packet_a, packet_c],
        "A and C intact; only the damaged B is dropped"
    );
    assert_eq!(dmx.resync_count(), 1);
    assert_eq!(dmx.hole_count(), 1, "the skipped page is also a seq hole");
    let kinds: Vec<DamageKind> = dmx.damage_events().iter().map(|e| e.kind).collect();
    assert_eq!(kinds, vec![DamageKind::Resync, DamageKind::Hole]);
}

#[test]
fn duplicate_serial_records_a_ledger_event() {
    // A second BOS on the live serial after data pages (an RFC 3533 §4
    // unique-serial violation the demuxer recovers from by restarting
    // the serial in place).
    let mut bytes = Vec::new();
    let seq = header_pages(&mut bytes);
    bytes.extend(whole_page(0, 960, SERIAL, seq, &[0xD0, 1]));
    // Restart: full header set again under the same serial.
    header_pages(&mut bytes);
    bytes.extend(whole_page(flags::LAST_PAGE, 960, SERIAL, 3, &[0xD0, 9]));

    let mut dmx = open(bytes);
    let _ = drain(&mut dmx);
    assert!(dmx.duplicate_serial_count() >= 1);
    assert!(
        dmx.damage_events()
            .iter()
            .any(|e| e.kind == DamageKind::DuplicateSerial && e.serial == Some(SERIAL)),
        "ledger must carry the duplicate-serial event: {:?}",
        dmx.damage_events()
    );
}

#[test]
fn ledger_retention_is_capped_but_the_total_keeps_counting() {
    // 100 data pages whose sequence numbers all jump by 2: one hole
    // per page — far past the retention cap.
    let mut bytes = Vec::new();
    let seq = header_pages(&mut bytes);
    let holes = 100u32;
    let mut s = seq;
    for i in 0..holes {
        s += 2; // every page skips one sequence number
        let flag = if i + 1 == holes { flags::LAST_PAGE } else { 0 };
        bytes.extend(whole_page(
            flag,
            960 * (i as i64 + 1),
            SERIAL,
            s,
            &[0xD0, i as u8],
        ));
    }

    let mut dmx = open(bytes);
    let packets = drain(&mut dmx);
    assert_eq!(packets.len(), holes as usize, "packets still delivered");
    assert_eq!(dmx.hole_count(), holes as u64);
    assert_eq!(
        dmx.damage_events().len(),
        MAX_DAMAGE_EVENTS,
        "retention is capped"
    );
    assert_eq!(
        dmx.damage_event_total(),
        holes as u64,
        "the total keeps counting past the cap"
    );
}
