//! Integration tests for the public chained-link diagnostic accessors
//! `OggDemuxer::link_count`, `OggDemuxer::stream_link_index`, and
//! `OggDemuxer::stream_serial` (RFC 3533 §4 + §6 field 5).
//!
//! These mirror the existing `hole_count` / `framing_error_count` /
//! `resync_count` / `seek_index_len` observability surface and let
//! external tooling reconstruct how a file partitions its streams across
//! chained links (links play sequentially; streams sharing a link
//! multiplex). The accessors only round-trip already-tracked internal
//! state — no new framing logic — but their absence previously forced
//! callers either to ignore chained partitioning or to re-scan every page
//! themselves.

use std::io::Cursor;

use oxideav_core::{Demuxer, ReadSeek};
use oxideav_ogg::page::{flags, lace, Page};

/// Minimal valid Vorbis identification packet (30 bytes).
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
    p.extend_from_slice(&0u32.to_le_bytes()); // vendor len
    p.extend_from_slice(&0u32.to_le_bytes()); // user count
    p.push(0x01); // framing bit
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
    let page = Page {
        flags: flags_byte,
        granule_position: granule,
        serial,
        seq_no: seq,
        lacing,
        data: packet.to_vec(),
    };
    page.to_bytes()
}

/// One logical Vorbis-in-Ogg link: BOS + comment + setup + `data_pages`
/// data pages, last one EOS-flagged.
fn build_link(serial: u32, payload_byte: u8, data_pages: usize) -> Vec<u8> {
    let mut out = Vec::new();
    let mut seq = 0u32;

    out.extend(build_page(
        flags::FIRST_PAGE,
        0,
        serial,
        seq,
        &vorbis_id_packet(2, 48_000),
    ));
    seq += 1;
    out.extend(build_page(0, 0, serial, seq, &vorbis_comment_packet()));
    seq += 1;
    out.extend(build_page(0, 0, serial, seq, &vorbis_setup_packet()));
    seq += 1;

    for i in 0..data_pages {
        let granule = 960 * (i as i64 + 1);
        let mut flag = 0u8;
        if i + 1 == data_pages {
            flag |= flags::LAST_PAGE;
        }
        let payload = vec![payload_byte, i as u8];
        out.extend(build_page(flag, granule, serial, seq, &payload));
        seq += 1;
    }
    out
}

/// Build a chained file: link A (serial 0xAAAA_AAAA, 3 data pages) followed
/// by link B (serial 0xBBBB_BBBB, 4 data pages).
fn build_two_link_chain() -> Vec<u8> {
    let mut out = Vec::new();
    out.extend(build_link(0xAAAA_AAAA, 0xAA, 3));
    out.extend(build_link(0xBBBB_BBBB, 0xBB, 4));
    out
}

/// Build a single-link multiplexed file: two BOS-section streams sharing
/// link 0 (serials 0x11111111 + 0x22222222), each with 2 data pages.
fn build_multiplexed_single_link() -> Vec<u8> {
    let mut out = Vec::new();
    let s_a = 0x1111_1111u32;
    let s_b = 0x2222_2222u32;

    // BOS section: BOS pages for both streams come first per RFC 3533 §6.
    out.extend(build_page(
        flags::FIRST_PAGE,
        0,
        s_a,
        0,
        &vorbis_id_packet(2, 48_000),
    ));
    out.extend(build_page(
        flags::FIRST_PAGE,
        0,
        s_b,
        0,
        &vorbis_id_packet(1, 44_100),
    ));

    // Comment + setup for both (their own pages, granule 0).
    out.extend(build_page(0, 0, s_a, 1, &vorbis_comment_packet()));
    out.extend(build_page(0, 0, s_b, 1, &vorbis_comment_packet()));
    out.extend(build_page(0, 0, s_a, 2, &vorbis_setup_packet()));
    out.extend(build_page(0, 0, s_b, 2, &vorbis_setup_packet()));

    // Interleaved data pages, last on each gets EOS.
    out.extend(build_page(0, 960, s_a, 3, &[0xAA, 0]));
    out.extend(build_page(0, 960, s_b, 3, &[0xBB, 0]));
    out.extend(build_page(flags::LAST_PAGE, 1920, s_a, 4, &[0xAA, 1]));
    out.extend(build_page(flags::LAST_PAGE, 1920, s_b, 4, &[0xBB, 1]));

    out
}

#[test]
fn open_concrete_reports_single_link_before_drain() {
    // A multiplexed BOS section is fully visible at open(), so the link
    // count + every stream's link index should already be correct without
    // any further reads.
    let bytes = build_multiplexed_single_link();
    let reader: Box<dyn ReadSeek> = Box::new(Cursor::new(bytes));
    let demux = oxideav_ogg::demux::open_concrete(reader, &oxideav_core::NullCodecResolver)
        .expect("demux multiplexed ogg");

    assert_eq!(demux.streams().len(), 2);
    assert_eq!(
        demux.link_count(),
        1,
        "multiplexed file is one chained link"
    );
    assert_eq!(demux.stream_link_index(0), Some(0));
    assert_eq!(demux.stream_link_index(1), Some(0));
    assert_eq!(demux.stream_serial(0), Some(0x1111_1111));
    assert_eq!(demux.stream_serial(1), Some(0x2222_2222));
}

#[test]
fn chained_links_split_via_build_seek_index() {
    // Without draining, only the first link is visible at open(). Running
    // build_seek_index walks every page header and registers the second
    // link's BOS, after which link_count == 2 and the two streams sit in
    // different links.
    let bytes = build_two_link_chain();
    let reader: Box<dyn ReadSeek> = Box::new(Cursor::new(bytes));
    let mut demux = oxideav_ogg::demux::open_concrete(reader, &oxideav_core::NullCodecResolver)
        .expect("demux chained ogg");

    assert_eq!(
        demux.link_count(),
        1,
        "before build_seek_index, only link 0 is registered"
    );

    demux
        .build_seek_index()
        .expect("build_seek_index walks both links");

    assert_eq!(demux.streams().len(), 2);
    assert_eq!(demux.link_count(), 2);
    // Streams are dense-indexed 0..N in BOS-discovery order — link A first,
    // link B second.
    assert_eq!(demux.stream_link_index(0), Some(0));
    assert_eq!(demux.stream_link_index(1), Some(1));
    assert_eq!(demux.stream_serial(0), Some(0xAAAA_AAAA));
    assert_eq!(demux.stream_serial(1), Some(0xBBBB_BBBB));
}

#[test]
fn out_of_range_stream_index_returns_none() {
    let bytes = build_multiplexed_single_link();
    let reader: Box<dyn ReadSeek> = Box::new(Cursor::new(bytes));
    let demux = oxideav_ogg::demux::open_concrete(reader, &oxideav_core::NullCodecResolver)
        .expect("demux multiplexed ogg");

    assert_eq!(demux.streams().len(), 2);
    assert_eq!(demux.stream_link_index(99), None);
    assert_eq!(demux.stream_serial(99), None);
}

#[test]
fn three_link_chain_increments_link_count_per_link() {
    // Confirm link_count tracks every BOS-after-non-BOS event, not just
    // the first transition.
    let mut bytes = Vec::new();
    bytes.extend(build_link(0xA1A1_A1A1, 0xA1, 2));
    bytes.extend(build_link(0xB2B2_B2B2, 0xB2, 2));
    bytes.extend(build_link(0xC3C3_C3C3, 0xC3, 2));

    let reader: Box<dyn ReadSeek> = Box::new(Cursor::new(bytes));
    let mut demux = oxideav_ogg::demux::open_concrete(reader, &oxideav_core::NullCodecResolver)
        .expect("demux three-link chain");

    demux.build_seek_index().expect("seek index three links");

    assert_eq!(demux.streams().len(), 3);
    assert_eq!(demux.link_count(), 3);
    for (i, expected_serial) in [0xA1A1_A1A1u32, 0xB2B2_B2B2, 0xC3C3_C3C3]
        .iter()
        .enumerate()
    {
        assert_eq!(demux.stream_link_index(i as u32), Some(i as u32));
        assert_eq!(demux.stream_serial(i as u32), Some(*expected_serial));
    }
}

#[test]
fn next_packet_drain_grows_link_count_lazily() {
    // The same chain produced by build_seek_index can also be discovered
    // incrementally via next_packet — link_count grows as each new BOS is
    // observed.
    let bytes = build_two_link_chain();
    let reader: Box<dyn ReadSeek> = Box::new(Cursor::new(bytes));
    let mut demux = oxideav_ogg::demux::open_concrete(reader, &oxideav_core::NullCodecResolver)
        .expect("demux chained ogg");

    assert_eq!(demux.link_count(), 1);
    while let Ok(_p) = demux.next_packet() {}

    assert_eq!(demux.streams().len(), 2);
    assert_eq!(demux.link_count(), 2);
    assert_eq!(demux.stream_link_index(0), Some(0));
    assert_eq!(demux.stream_link_index(1), Some(1));
}
