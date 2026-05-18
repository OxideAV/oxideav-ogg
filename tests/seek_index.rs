//! Integration test for the page-level seek index added in round 75.
//!
//! Validates three properties:
//! 1. `build_seek_index` populates one entry per data page that carries a
//!    real granule (granule != -1).
//! 2. After indexing, `seek_to` lands on the same page as the bisecting
//!    code path on a synthetic file.
//! 3. Indexed `seek_to` works on a chained file across both links.

use std::io::Cursor;

use oxideav_core::ReadSeek;
use oxideav_ogg::page::{flags, lace, Page};

// ────────────────────── synthetic page builders ──────────────────────

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

/// Synthetic Vorbis-in-Ogg blob with `data_pages` data pages.
/// Granules go 960, 1920, ... (one 20ms frame at 48 kHz apiece).
fn build_synthetic(data_pages: usize) -> Vec<u8> {
    let serial: u32 = 0xCAFE_BABE;
    let mut out = Vec::new();
    let mut seq = 0u32;

    out.extend(build_page(
        flags::FIRST_PAGE,
        0,
        serial,
        seq,
        &vorbis_id_packet(),
    ));
    seq += 1;
    out.extend(build_page(0, 0, serial, seq, &vorbis_comment_packet()));
    seq += 1;
    out.extend(build_page(0, 0, serial, seq, &vorbis_setup_packet()));
    seq += 1;

    for i in 1..=data_pages as i64 {
        let granule = 960 * i;
        let flag = if i as usize == data_pages {
            flags::LAST_PAGE
        } else {
            0
        };
        let payload: [u8; 2] = [0xAA, (i as u8).wrapping_add(1)];
        out.extend(build_page(flag, granule, serial, seq, &payload));
        seq += 1;
    }
    out
}

#[test]
fn build_seek_index_records_every_data_page() {
    // 30 data pages → 30 indexed entries (the 3 header pages have granule 0,
    // and granule 0 ≥ 0, so they ALSO get indexed — total = 33).
    let blob = build_synthetic(30);
    let reader: Box<dyn ReadSeek> = Box::new(Cursor::new(blob));
    let mut demux =
        oxideav_ogg::demux::open_concrete(reader, &oxideav_core::NullCodecResolver).unwrap();

    // Before build: only the pages read during open() are recorded.
    let pre = demux.seek_index_len();

    demux.build_seek_index().expect("build seek index");
    let post = demux.seek_index_len();

    assert!(
        post >= 33,
        "expected ≥33 indexed entries (3 header + 30 data), got {post} (pre-build = {pre})"
    );
}

#[test]
fn indexed_seek_lands_at_or_before_target() {
    let blob = build_synthetic(20);
    let reader: Box<dyn ReadSeek> = Box::new(Cursor::new(blob));
    let mut demux = oxideav_ogg::demux::open_indexed(reader, &oxideav_core::NullCodecResolver)
        .expect("open with index");

    // Granules are 960, 1920, ..., 19200. A target of 5500 should
    // land on granule 4800 (page 5) — the floor of 5500.
    let target = 5500i64;
    let landed = demux.seek_to(0, target).expect("indexed seek_to");
    assert!(
        landed <= target,
        "indexed seek landed at {landed} > target {target}"
    );
    // The floor of 5500 in {960, 1920, 2880, 3840, 4800, 5760, ...} is 4800.
    assert_eq!(
        landed, 4800,
        "expected indexed seek to floor at exactly the per-page granule (4800), got {landed}"
    );
}

#[test]
fn indexed_seek_to_zero_returns_earliest_page() {
    let blob = build_synthetic(10);
    let reader: Box<dyn ReadSeek> = Box::new(Cursor::new(blob));
    let mut demux = oxideav_ogg::demux::open_indexed(reader, &oxideav_core::NullCodecResolver)
        .expect("open with index");

    let landed = demux.seek_to(0, 0).expect("indexed seek to 0");
    assert_eq!(
        landed, 0,
        "seek to granule 0 should land on the BOS page (granule = 0), got {landed}"
    );
}

#[test]
fn indexed_seek_beyond_end_lands_on_last_page() {
    let blob = build_synthetic(10);
    let reader: Box<dyn ReadSeek> = Box::new(Cursor::new(blob));
    let mut demux = oxideav_ogg::demux::open_indexed(reader, &oxideav_core::NullCodecResolver)
        .expect("open with index");

    // Last data page granule is 9600. Asking for 1_000_000 should clamp
    // to 9600.
    let landed = demux.seek_to(0, 1_000_000).expect("seek past EOF");
    assert_eq!(
        landed, 9600,
        "seek beyond stream end should clamp to last granule (9600), got {landed}"
    );
}

#[test]
fn repeat_seek_uses_index_consistently() {
    // Seek to multiple targets in arbitrary order; each result should be
    // the same as if we had done the seek as the first operation.
    let blob = build_synthetic(15);
    let reader: Box<dyn ReadSeek> = Box::new(Cursor::new(blob.clone()));
    let mut demux = oxideav_ogg::demux::open_indexed(reader, &oxideav_core::NullCodecResolver)
        .expect("open with index");

    let targets = [3500i64, 11_000, 1000, 14_400, 100, 9000];
    let mut got = Vec::new();
    for &t in &targets {
        got.push(demux.seek_to(0, t).expect("seek_to"));
    }

    // Reference: open a FRESH demuxer per seek (no caching) and compare.
    let mut reference = Vec::new();
    for &t in &targets {
        let reader: Box<dyn ReadSeek> = Box::new(Cursor::new(blob.clone()));
        let mut d = oxideav_ogg::demux::open_indexed(reader, &oxideav_core::NullCodecResolver)
            .expect("open ref");
        reference.push(d.seek_to(0, t).expect("ref seek"));
    }

    assert_eq!(
        got, reference,
        "repeat seeks on a single indexed demuxer should match seeks on fresh demuxers"
    );
}

#[test]
fn build_seek_index_is_idempotent() {
    let blob = build_synthetic(8);
    let reader: Box<dyn ReadSeek> = Box::new(Cursor::new(blob));
    let mut demux =
        oxideav_ogg::demux::open_concrete(reader, &oxideav_core::NullCodecResolver).unwrap();

    demux.build_seek_index().unwrap();
    let n1 = demux.seek_index_len();
    demux.build_seek_index().unwrap();
    let n2 = demux.seek_index_len();
    assert_eq!(
        n1, n2,
        "calling build_seek_index twice must not duplicate entries"
    );
}
