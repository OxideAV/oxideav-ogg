//! Integration tests for page-sync recapture / resynchronisation
//! (RFC 3533 §3 "recapture after a parsing error" and §6 field 1
//! `capture_pattern`: the `OggS` magic "helps a decoder to find the page
//! boundaries and regain synchronisation after parsing a corrupted stream.
//! Once the capture pattern is found, the decoder verifies page sync and
//! integrity by computing and comparing the checksum.").
//!
//! These tests inject byte-level corruption into otherwise-valid Ogg
//! streams and verify the demuxer:
//!   1. recovers (does not fail the whole stream),
//!   2. delivers every packet that survives the corruption,
//!   3. ticks `OggDemuxer::resync_count` exactly once per recovery,
//!   4. does NOT splice or fabricate data across the damaged region
//!      (orphaned packet fragments are dropped, not glued together).

use std::io::Cursor;

use oxideav_core::{Demuxer, ReadSeek};
use oxideav_ogg::page::{flags, lace, Page};

// ─────────────────────────── packet builders ───────────────────────────
// Mirror the minimal Vorbis identification scaffolding used by other
// integration tests so the demuxer's BOS sniff succeeds.

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

const SERIAL: u32 = 0xCAFE_BABE;

/// Emit the three Vorbis header pages (sequence 0..=2) and return the next
/// sequence number a data page should use.
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

/// Locate the byte offset of the Nth `OggS` capture pattern in `bytes`
/// (zero-indexed). Panics if there are fewer than `n + 1` captures.
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
fn clean_stream_has_zero_resyncs() {
    // Baseline: a correctly-framed file must not tick the resync counter.
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
    let mut delivered = 0;
    while dmx.next_packet().is_ok() {
        delivered += 1;
    }
    assert_eq!(delivered, 4);
    assert_eq!(dmx.resync_count(), 0, "no corruption → no resyncs");
}

#[test]
fn garbage_spliced_between_pages_recovers() {
    // Construct a clean stream, then inject a block of arbitrary bytes
    // between the third data page (seq=5) and the fourth (seq=6). The
    // demuxer should skip the garbage, recover at the next `OggS`, count
    // exactly one resync, and deliver every undamaged packet.
    //
    // Page seq=5 is unaffected and its packet must arrive. Page seq=6's
    // header sits immediately after the garbage and must be re-acquired.
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
    // Insert garbage right before the page with seq = seq_base + 3 (the
    // 4th data page). Find its `OggS` (it's the (3 + 3 + 1) = 7th overall:
    // 3 header pages + 3 prior data pages = 6 prior captures, so index 6
    // in zero-indexed terms).
    let target = nth_oggs_offset(&out, 6);
    let mut corrupted = Vec::with_capacity(out.len() + 200);
    corrupted.extend_from_slice(&out[..target]);
    corrupted.extend(std::iter::repeat(0xAB).take(150));
    corrupted.extend_from_slice(&out[target..]);

    let reader: Box<dyn ReadSeek> = Box::new(Cursor::new(corrupted));
    let mut dmx = oxideav_ogg::demux::open_concrete(reader, &oxideav_core::NullCodecResolver)
        .expect("open ogg with mid-stream garbage");
    let mut delivered = 0;
    while dmx.next_packet().is_ok() {
        delivered += 1;
    }
    // Every data packet survives — the garbage sat between pages, not
    // inside one.
    assert_eq!(
        delivered, 4,
        "all four packets delivered across the garbage gap"
    );
    assert_eq!(
        dmx.resync_count(),
        1,
        "exactly one recovery for the injected garbage"
    );
    // No page_sequence_number gap → hole counter stays at zero.
    assert_eq!(dmx.hole_count(), 0, "garbage between pages is not a hole");
}

#[test]
fn corrupted_page_body_is_skipped_with_resync() {
    // Build a 4-data-page stream, then flip a byte inside the second data
    // page's body (after its `OggS` header). The CRC will fail; the
    // demuxer must scan forward to the next valid page and continue.
    // The corrupted page is lost entirely.
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
            &[0xE0, i as u8],
        ));
    }
    // The second data page is the 5th `OggS` overall (index 4). Find it
    // and flip a byte well into its body (past the 27-byte header + the
    // 1-byte lacing table for a 2-byte packet = byte 30).
    let target = nth_oggs_offset(&out, 4);
    out[target + 30] ^= 0xFF;

    let reader: Box<dyn ReadSeek> = Box::new(Cursor::new(out));
    let mut dmx = oxideav_ogg::demux::open_concrete(reader, &oxideav_core::NullCodecResolver)
        .expect("open ogg with CRC-failing page");
    let mut delivered = 0;
    while dmx.next_packet().is_ok() {
        delivered += 1;
    }
    // 4 data pages → 1 was corrupted and dropped → 3 delivered.
    assert_eq!(
        delivered, 3,
        "three good packets delivered; corrupt one dropped"
    );
    assert_eq!(
        dmx.resync_count(),
        1,
        "exactly one CRC-recovery resync ticked"
    );
    // The corrupted page also created a page_sequence_number gap (seq
    // jumped past the bad page), so the hole counter should also tick.
    assert_eq!(
        dmx.hole_count(),
        1,
        "the missing page is also seen as a sequence-number hole"
    );
}

#[test]
fn unaligned_capture_in_payload_does_not_false_lock() {
    // The first data page carries a packet whose body contains the literal
    // bytes `OggS` (a false-positive capture pattern). The demuxer must
    // NOT try to resync onto that capture: normal reading from the page
    // header drives consumption, and the embedded `OggS` is just data.
    let mut out = Vec::new();
    let seq_base = header_pages(&mut out);
    let payload_with_oggs: Vec<u8> = {
        let mut v = vec![0xAA; 16];
        v.extend_from_slice(b"OggS"); // false-positive capture inside payload
        v.extend_from_slice(&[0xBB; 16]);
        v
    };
    out.extend(whole_page(0, 960, SERIAL, seq_base, &payload_with_oggs));
    out.extend(whole_page(
        flags::LAST_PAGE,
        1920,
        SERIAL,
        seq_base + 1,
        &[0xCC, 0xDD],
    ));

    let reader: Box<dyn ReadSeek> = Box::new(Cursor::new(out));
    let mut dmx = oxideav_ogg::demux::open_concrete(reader, &oxideav_core::NullCodecResolver)
        .expect("open ogg with payload that contains 'OggS'");
    let mut delivered: Vec<Vec<u8>> = Vec::new();
    while let Ok(p) = dmx.next_packet() {
        delivered.push(p.data);
    }
    assert_eq!(delivered.len(), 2, "both data packets delivered intact");
    assert_eq!(delivered[0], payload_with_oggs, "embedded 'OggS' preserved");
    assert_eq!(delivered[1], vec![0xCC, 0xDD]);
    assert_eq!(
        dmx.resync_count(),
        0,
        "false-positive capture must not trip resync"
    );
    assert_eq!(dmx.hole_count(), 0);
}

#[test]
fn trailing_garbage_after_last_page_terminates_cleanly() {
    // Tail-corruption case: append a block of garbage AFTER the last
    // legitimate page. No valid page follows, so the resync scan reaches
    // EOF without finding one. The demuxer must report EOF (not an error)
    // and not over-count resyncs.
    let mut out = Vec::new();
    let seq_base = header_pages(&mut out);
    for i in 0..3u32 {
        let granule = 960 * (i as i64 + 1);
        let flag = if i == 2 { flags::LAST_PAGE } else { 0 };
        out.extend(whole_page(
            flag,
            granule,
            SERIAL,
            seq_base + i,
            &[0xF0, i as u8],
        ));
    }
    out.extend(std::iter::repeat(0x5A).take(300));

    let reader: Box<dyn ReadSeek> = Box::new(Cursor::new(out));
    let mut dmx = oxideav_ogg::demux::open_concrete(reader, &oxideav_core::NullCodecResolver)
        .expect("open ogg with trailing garbage");
    let mut delivered = 0;
    while dmx.next_packet().is_ok() {
        delivered += 1;
    }
    assert_eq!(delivered, 3, "every real packet delivered before EOF");
    // The trailing garbage triggers exactly one resync attempt that finds
    // no further page and yields EOF. (Implementation-detail caveat: an
    // implementation that finishes after the last EOS page without
    // re-reading would report zero. We accept either, but never more
    // than one, since the failure mode under test is unbounded re-tries.)
    assert!(
        dmx.resync_count() <= 1,
        "tail garbage must not produce repeated resyncs; got {}",
        dmx.resync_count()
    );
}
