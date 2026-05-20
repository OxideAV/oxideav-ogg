//! Integration tests for chained-Ogg duration: RFC 3533 §4 chained logical
//! bitstreams play back-to-back, so total stream duration is the SUM of
//! per-link durations, not the MAX over streams (which is the right answer
//! for the *multiplex* case where multiple serials share one link).
//!
//! These tests exercise `OggDemuxer::build_seek_index`, which walks the
//! whole file once and recomputes `duration_micros` correctly for both
//! chained and multiplexed shapes.

use std::io::Cursor;

use oxideav_core::Demuxer;
use oxideav_core::ReadSeek;
use oxideav_ogg::page::{flags, lace, Page};

// ─────────────────────── synthetic header packets ───────────────────────

fn vorbis_id_packet(channels: u8, sample_rate: u32) -> Vec<u8> {
    let mut p = Vec::with_capacity(30);
    p.push(0x01);
    p.extend_from_slice(b"vorbis");
    p.extend_from_slice(&0u32.to_le_bytes()); // version
    p.push(channels);
    p.extend_from_slice(&sample_rate.to_le_bytes());
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

fn build_page(flags_byte: u8, granule: i64, serial: u32, seq: u32, packet: &[u8]) -> Vec<u8> {
    let lacing = lace(packet.len());
    Page {
        flags: flags_byte,
        granule_position: granule,
        serial,
        seq_no: seq,
        lacing,
        data: packet.to_vec(),
    }
    .to_bytes()
}

/// One full Vorbis-in-Ogg link with `data_pages` data pages whose granules
/// step by 960 (one 20 ms frame at 48 kHz).
fn build_link(serial: u32, marker: u8, data_pages: usize, sample_rate: u32) -> Vec<u8> {
    let mut out = Vec::new();
    let mut seq = 0u32;
    out.extend(build_page(
        flags::FIRST_PAGE,
        0,
        serial,
        seq,
        &vorbis_id_packet(2, sample_rate),
    ));
    seq += 1;
    out.extend(build_page(0, 0, serial, seq, &vorbis_comment_packet()));
    seq += 1;
    out.extend(build_page(0, 0, serial, seq, &vorbis_setup_packet()));
    seq += 1;
    for i in 1..=data_pages as i64 {
        let granule = 960 * i;
        let last = i as usize == data_pages;
        let flag = if last { flags::LAST_PAGE } else { 0 };
        out.extend(build_page(flag, granule, serial, seq, &[marker, i as u8]));
        seq += 1;
    }
    out
}

// ───────────────────────────── tests ─────────────────────────────────

#[test]
fn chained_duration_sums_per_link() {
    // Link A: 3 data pages → last granule 2880 → 60 ms at 48 kHz.
    // Link B: 5 data pages → last granule 4800 → 100 ms at 48 kHz.
    // Total expected: 160 ms (160_000 µs).
    let mut blob = Vec::new();
    blob.extend(build_link(0xAAAA_AAAA, 0xAA, 3, 48_000));
    blob.extend(build_link(0xBBBB_BBBB, 0xBB, 5, 48_000));

    let reader: Box<dyn ReadSeek> = Box::new(Cursor::new(blob));
    let mut demux =
        oxideav_ogg::demux::open_concrete(reader, &oxideav_core::NullCodecResolver).unwrap();
    // build_seek_index walks the whole file and registers link B's BOS,
    // then recomputes duration_micros as the sum of per-link durations.
    demux.build_seek_index().expect("build seek index");

    let dur = demux.duration_micros().expect("duration recorded");
    // Tolerate ±2 µs of f64 round-trip noise (i.e. 159_998..=160_002).
    assert!(
        (dur - 160_000).abs() <= 2,
        "chained duration should sum per-link (60ms + 100ms = 160ms ± 2µs), got {dur}",
    );
}

#[test]
fn three_link_chain_duration_sums() {
    // Three back-to-back links of 2, 4, 1 data pages = 40 + 80 + 20 ms.
    let mut blob = Vec::new();
    blob.extend(build_link(0x1111_1111, 0x11, 2, 48_000));
    blob.extend(build_link(0x2222_2222, 0x22, 4, 48_000));
    blob.extend(build_link(0x3333_3333, 0x33, 1, 48_000));

    let reader: Box<dyn ReadSeek> = Box::new(Cursor::new(blob));
    let mut demux =
        oxideav_ogg::demux::open_concrete(reader, &oxideav_core::NullCodecResolver).unwrap();
    demux.build_seek_index().unwrap();

    let dur = demux.duration_micros().expect("duration recorded");
    assert!(
        (dur - 140_000).abs() <= 2,
        "three-link chain duration should sum to 40+80+20 = 140 ms (± 2 µs), got {dur}",
    );
}

#[test]
fn multiplexed_single_link_duration_uses_max() {
    // Two streams within ONE link (multiplexed). Both BOS pages come
    // before any data page, so both share link_index 0. Total duration
    // should be MAX(80ms vorbis @ 48kHz, 120ms vorbis @ 44.1 kHz) =
    // 120 ms — NOT the sum (which would be 200 ms).
    let serial_a: u32 = 0xAAAA_AAAA;
    let serial_b: u32 = 0xBBBB_BBBB;
    let mut blob = Vec::new();
    let mut seq_a = 0u32;
    let mut seq_b = 0u32;

    // BOS section: both BOS pages before any non-BOS.
    blob.extend(build_page(
        flags::FIRST_PAGE,
        0,
        serial_a,
        seq_a,
        &vorbis_id_packet(2, 48_000),
    ));
    seq_a += 1;
    blob.extend(build_page(
        flags::FIRST_PAGE,
        0,
        serial_b,
        seq_b,
        &vorbis_id_packet(2, 44_100),
    ));
    seq_b += 1;

    // Header packets for both.
    blob.extend(build_page(0, 0, serial_a, seq_a, &vorbis_comment_packet()));
    seq_a += 1;
    blob.extend(build_page(0, 0, serial_a, seq_a, &vorbis_setup_packet()));
    seq_a += 1;
    blob.extend(build_page(0, 0, serial_b, seq_b, &vorbis_comment_packet()));
    seq_b += 1;
    blob.extend(build_page(0, 0, serial_b, seq_b, &vorbis_setup_packet()));
    seq_b += 1;

    // Data pages, interleaved. Stream A: 4 pages at 48 kHz → 80 ms.
    // Stream B: 3 pages at 44.1 kHz with granule step 882 (20 ms) →
    // last granule 2646 ≈ 60 ms. Multiplex max ~ 80 ms.
    // Override to make the test crisp: B has 6 pages → 6 × 882 = 5292
    // → 120_000 µs.
    for i in 1..=4i64 {
        blob.extend(build_page(0, 960 * i, serial_a, seq_a, &[0xAA, i as u8]));
        seq_a += 1;
    }
    // EOS marker on the last A page.
    {
        let last_off_search = blob.len();
        // Patch the most recent A page's flag to LAST_PAGE.
        let last_a_off = find_last_oggs_with_serial(&blob[..last_off_search], serial_a).unwrap();
        blob[last_a_off + 5] |= flags::LAST_PAGE;
        recompute_crc(&mut blob, last_a_off);
    }
    for i in 1..=6i64 {
        blob.extend(build_page(0, 882 * i, serial_b, seq_b, &[0xBB, i as u8]));
        seq_b += 1;
    }
    {
        let last_off_search = blob.len();
        let last_b_off = find_last_oggs_with_serial(&blob[..last_off_search], serial_b).unwrap();
        blob[last_b_off + 5] |= flags::LAST_PAGE;
        recompute_crc(&mut blob, last_b_off);
    }

    let reader: Box<dyn ReadSeek> = Box::new(Cursor::new(blob));
    let mut demux =
        oxideav_ogg::demux::open_concrete(reader, &oxideav_core::NullCodecResolver).unwrap();
    demux.build_seek_index().unwrap();

    // Both streams sit in link 0 (multiplexed): duration = max =
    // max(80_000, 120_000) = 120_000 µs.
    let dur = demux.duration_micros().expect("duration recorded");
    assert!(
        (dur - 120_000).abs() <= 2,
        "multiplexed (single-link) duration should be MAX over streams (120 ms ± 2 µs), got {dur}",
    );
}

// Helpers for the multiplex test.

fn find_last_oggs_with_serial(buf: &[u8], wanted_serial: u32) -> Option<usize> {
    let mut found: Option<usize> = None;
    let mut off = 0usize;
    while off + 27 <= buf.len() && &buf[off..off + 4] == b"OggS" {
        let serial =
            u32::from_le_bytes([buf[off + 14], buf[off + 15], buf[off + 16], buf[off + 17]]);
        if serial == wanted_serial {
            found = Some(off);
        }
        let n_segs = buf[off + 26] as usize;
        let lacing_start = off + 27;
        let data_start = lacing_start + n_segs;
        let data_len: usize = buf[lacing_start..data_start]
            .iter()
            .map(|&v| v as usize)
            .sum();
        off = data_start + data_len;
    }
    found
}

fn recompute_crc(buf: &mut [u8], page_off: usize) {
    let n_segs = buf[page_off + 26] as usize;
    let lacing_start = page_off + 27;
    let data_start = lacing_start + n_segs;
    let data_len: usize = buf[lacing_start..data_start]
        .iter()
        .map(|&v| v as usize)
        .sum();
    let page_end = data_start + data_len;
    buf[page_off + 22..page_off + 26].fill(0);
    let crc = oxideav_ogg::crc::checksum(&buf[page_off..page_end]);
    buf[page_off + 22..page_off + 26].copy_from_slice(&crc.to_le_bytes());
}
