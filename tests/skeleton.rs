//! End-to-end Ogg Skeleton demux tests.
//!
//! These tests synthesise an Ogg physical stream whose first BOS page
//! carries a `fishead\0` ident packet (Skeleton 3.0 or 4.0), followed
//! by a Vorbis content stream's BOS, then Skeleton fisbones / 4.0
//! index packets, the Skeleton EOS empty page, and finally a few
//! Vorbis data pages. The demuxer is opened against the synthesised
//! bytes and its `skeleton()` accessor is checked to confirm the
//! head, fisbones and indexes were parsed.
//!
//! Skeleton spec: `docs/container/ogg/ogg-skeleton-3.0.md`,
//! `docs/container/ogg/ogg-skeleton-4.0.md`.

use std::io::Cursor;

use oxideav_core::{NullCodecResolver, ReadSeek};

use oxideav_ogg::page::{flags, lace, Page};
use oxideav_ogg::skeleton::{self, FisBone, FisHead, Rational, SkelIndex, Version};

/// Build a single page carrying `packet` whole (no continuation),
/// with the given flags and serial. `seq_no` and `granule` are passed
/// through to the page header.
fn single_packet_page(
    packet: &[u8],
    flags_byte: u8,
    serial: u32,
    seq_no: u32,
    granule: i64,
) -> Vec<u8> {
    let p = Page {
        flags: flags_byte,
        granule_position: granule,
        serial,
        seq_no,
        lacing: lace(packet.len()),
        data: packet.to_vec(),
    };
    p.to_bytes()
}

/// Build a minimal valid Vorbis identification packet (30 bytes), per
/// the field layout already used by `tests/page_crc.rs`.
fn vorbis_id_packet(channels: u8, sample_rate: u32) -> Vec<u8> {
    let mut p = Vec::with_capacity(30);
    p.push(0x01);
    p.extend_from_slice(b"vorbis");
    p.extend_from_slice(&0u32.to_le_bytes()); // version
    p.push(channels);
    p.extend_from_slice(&sample_rate.to_le_bytes());
    p.extend_from_slice(&0i32.to_le_bytes()); // br_max
    p.extend_from_slice(&128_000i32.to_le_bytes()); // br_nom
    p.extend_from_slice(&0i32.to_le_bytes()); // br_min
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
    p.push(0x01);
    p
}

fn vorbis_setup_packet() -> Vec<u8> {
    let mut p = Vec::new();
    p.push(0x05);
    p.extend_from_slice(b"vorbis");
    p.extend_from_slice(&[0u8; 32]);
    p
}

const SKEL_SERIAL: u32 = 0xCAFEBABE;
const VORBIS_SERIAL: u32 = 0x12345678;

/// Build an Ogg file with a Skeleton 4.0 bitstream + one Vorbis content
/// stream, returning the file bytes plus the FisHead/FisBone/SkelIndex
/// we expect the demuxer to recover.
fn build_skeleton_4_0_ogg() -> (Vec<u8>, FisHead, FisBone, SkelIndex) {
    // ---- Skeleton 4.0 BOS (fishead) ----
    let mut head = FisHead::new(Version::V4_0);
    head.presentation_time = Rational::new(0, 1000);
    head.basetime = Rational::new(0, 1000);
    head.segment_length = Some(1_234_567);
    head.content_byte_offset = Some(4096);
    let head_packet = head.to_bytes();

    // ---- Vorbis BOS ----
    let v_id = vorbis_id_packet(2, 48_000);

    // ---- Skeleton fisbone describing the Vorbis stream ----
    let mut bone = FisBone::new(VORBIS_SERIAL, Rational::new(48_000, 1));
    bone.num_headers = 3;
    bone.preroll = 2;
    bone.granuleshift = 0;
    bone.set_header("Content-Type", "audio/vorbis");
    bone.set_header("Role", "audio/main");
    bone.set_header("Name", "main_audio");
    let bone_packet = bone.to_bytes();

    // ---- Skeleton 4.0 index for the Vorbis stream ----
    let mut idx = SkelIndex::new(VORBIS_SERIAL, 1_000_000);
    idx.first_sample_time = 0;
    idx.last_sample_time = 1_000_000;
    idx.push(4096, 0);
    idx.push(4096 + 7843, 500_000);
    let idx_packet = idx.to_bytes();

    // ---- Vorbis comment + setup packets ----
    let v_comment = vorbis_comment_packet();
    let v_setup = vorbis_setup_packet();

    let mut out = Vec::new();

    // Pass 1: BOS pages, Skeleton first (per spec).
    out.extend_from_slice(&single_packet_page(
        &head_packet,
        flags::FIRST_PAGE,
        SKEL_SERIAL,
        0,
        0,
    ));
    out.extend_from_slice(&single_packet_page(
        &v_id,
        flags::FIRST_PAGE,
        VORBIS_SERIAL,
        0,
        0,
    ));

    // Pass 2: secondary headers. Mix Skeleton fisbones + Vorbis comment
    // + Vorbis setup. Order: Vorbis comment, fisbone, index, Vorbis
    // setup — exercises the interleaving the spec allows.
    out.extend_from_slice(&single_packet_page(&v_comment, 0, VORBIS_SERIAL, 1, 0));
    out.extend_from_slice(&single_packet_page(&bone_packet, 0, SKEL_SERIAL, 1, 0));
    out.extend_from_slice(&single_packet_page(&idx_packet, 0, SKEL_SERIAL, 2, 0));
    out.extend_from_slice(&single_packet_page(&v_setup, 0, VORBIS_SERIAL, 2, 0));

    // Skeleton EOS — empty packet by itself on the last Skeleton page.
    out.extend_from_slice(&single_packet_page(
        &[],
        flags::LAST_PAGE,
        SKEL_SERIAL,
        3,
        0,
    ));

    // One Vorbis content data page with a sample payload.
    let data_packet: Vec<u8> = (0..100u8).collect();
    out.extend_from_slice(&single_packet_page(
        &data_packet,
        flags::LAST_PAGE,
        VORBIS_SERIAL,
        3,
        960,
    ));

    (out, head, bone, idx)
}

#[test]
fn skeleton_demux_recovers_head_bone_and_index() {
    let (bytes, expected_head, expected_bone, expected_idx) = build_skeleton_4_0_ogg();

    let reader: Box<dyn ReadSeek> = Box::new(Cursor::new(bytes));
    let codecs = NullCodecResolver;
    let dmx = oxideav_ogg::demux::open_concrete(reader, &codecs).expect("open ok");

    let sk = dmx.skeleton().expect("Skeleton was detected");
    assert_eq!(sk.serial, Some(SKEL_SERIAL));
    assert_eq!(sk.head.as_ref().unwrap(), &expected_head);
    assert_eq!(sk.version(), Version::V4_0);
    assert_eq!(sk.bones.len(), 1);
    assert_eq!(&sk.bones[0], &expected_bone);
    assert_eq!(sk.indexes.len(), 1);
    assert_eq!(&sk.indexes[0], &expected_idx);

    // Skeleton is NOT a public stream — the only one demuxer reports
    // is the Vorbis content stream.
    assert_eq!(
        oxideav_core::Demuxer::streams(&dmx).len(),
        1,
        "Skeleton must not appear in the public streams() list"
    );
}

#[test]
fn skeleton_lookup_helpers_round_trip() {
    let (bytes, _, expected_bone, expected_idx) = build_skeleton_4_0_ogg();
    let reader: Box<dyn ReadSeek> = Box::new(Cursor::new(bytes));
    let codecs = NullCodecResolver;
    let dmx = oxideav_ogg::demux::open_concrete(reader, &codecs).expect("open ok");
    let sk = dmx.skeleton().expect("Skeleton present");

    let bone = sk
        .bone_for_serial(VORBIS_SERIAL)
        .expect("bone for vorbis serial");
    assert_eq!(bone, &expected_bone);
    assert_eq!(bone.header("Content-Type"), Some("audio/vorbis"));
    assert_eq!(bone.header("ROLE"), Some("audio/main")); // case-insensitive
    assert_eq!(bone.header("Name"), Some("main_audio"));
    assert!(sk.bone_for_serial(0xDEAD).is_none());

    let idx = sk
        .index_for_serial(VORBIS_SERIAL)
        .expect("index for vorbis serial");
    assert_eq!(idx, &expected_idx);
    assert_eq!(idx.keypoints.len(), 2);
    assert!(sk.index_for_serial(0xDEAD).is_none());
}

#[test]
fn skeleton_absent_streams_still_demux() {
    // A plain Vorbis-only Ogg without Skeleton — the demuxer must still
    // open it cleanly, and `skeleton()` returns `None`.
    let v_id = vorbis_id_packet(2, 48_000);
    let v_comment = vorbis_comment_packet();
    let v_setup = vorbis_setup_packet();

    let mut out = Vec::new();
    out.extend_from_slice(&single_packet_page(
        &v_id,
        flags::FIRST_PAGE,
        VORBIS_SERIAL,
        0,
        0,
    ));
    out.extend_from_slice(&single_packet_page(&v_comment, 0, VORBIS_SERIAL, 1, 0));
    out.extend_from_slice(&single_packet_page(&v_setup, 0, VORBIS_SERIAL, 2, 0));
    let data_packet: Vec<u8> = (0..50u8).collect();
    out.extend_from_slice(&single_packet_page(
        &data_packet,
        flags::LAST_PAGE,
        VORBIS_SERIAL,
        3,
        960,
    ));

    let reader: Box<dyn ReadSeek> = Box::new(Cursor::new(out));
    let codecs = NullCodecResolver;
    let dmx = oxideav_ogg::demux::open_concrete(reader, &codecs).expect("open ok");
    assert!(
        dmx.skeleton().is_none(),
        "no Skeleton means skeleton() = None"
    );
    assert_eq!(oxideav_core::Demuxer::streams(&dmx).len(), 1);
}

#[test]
fn skeleton_3_0_head_parses_without_segment_length() {
    // Build a Skeleton 3.0 BOS — the fishead is 64 bytes, omitting the
    // segment-length / content-byte-offset fields the 4.0 layout adds.
    let mut head = FisHead::new(Version::V3_0);
    head.presentation_time = Rational::new(7, 1000);
    head.utc[..15].copy_from_slice(b"20260529T064100");
    let head_packet = head.to_bytes();
    assert_eq!(head_packet.len(), 64);

    let v_id = vorbis_id_packet(1, 44_100);
    let v_comment = vorbis_comment_packet();
    let v_setup = vorbis_setup_packet();

    let mut out = Vec::new();
    out.extend_from_slice(&single_packet_page(
        &head_packet,
        flags::FIRST_PAGE,
        SKEL_SERIAL,
        0,
        0,
    ));
    out.extend_from_slice(&single_packet_page(
        &v_id,
        flags::FIRST_PAGE,
        VORBIS_SERIAL,
        0,
        0,
    ));
    out.extend_from_slice(&single_packet_page(&v_comment, 0, VORBIS_SERIAL, 1, 0));
    out.extend_from_slice(&single_packet_page(&v_setup, 0, VORBIS_SERIAL, 2, 0));
    // Skeleton 3.0 has NO index packets — just the EOS empty packet.
    out.extend_from_slice(&single_packet_page(
        &[],
        flags::LAST_PAGE,
        SKEL_SERIAL,
        1,
        0,
    ));
    let data_packet: Vec<u8> = (0..50u8).collect();
    out.extend_from_slice(&single_packet_page(
        &data_packet,
        flags::LAST_PAGE,
        VORBIS_SERIAL,
        3,
        960,
    ));

    let reader: Box<dyn ReadSeek> = Box::new(Cursor::new(out));
    let codecs = NullCodecResolver;
    let dmx = oxideav_ogg::demux::open_concrete(reader, &codecs).expect("open ok");
    let sk = dmx.skeleton().expect("Skeleton present");
    let parsed_head = sk.head.as_ref().expect("head parsed");
    assert_eq!(parsed_head.version, Version::V3_0);
    assert_eq!(parsed_head.segment_length, None);
    assert_eq!(parsed_head.content_byte_offset, None);
    // No fisbones supplied in this minimal blob → bones vec empty, but
    // the indexes vec is also empty (3.0 has no index packets).
    assert!(sk.indexes.is_empty());
}

/// Build an Ogg file whose Skeleton 4.0 `index\0` packet's keypoints
/// reference the exact byte offsets of the Vorbis data pages, so a
/// `seek_to` against a known timestamp jumps directly to a known page
/// without any bisection or `build_seek_index` work. Returns the file
/// bytes plus the keypoint table (offset, granule) the test asserts
/// against.
///
/// The file layout is:
///   [BOS Skeleton fishead]
///   [BOS Vorbis id]
///   [Vorbis comment][Skeleton fisbone][Skeleton index][Vorbis setup]
///   [Skeleton EOS empty]
///   [Vorbis data page 0, granule 480]
///   [Vorbis data page 1, granule 960]
///   [Vorbis data page 2, granule 1440]
///   [Vorbis data page 3, LAST, granule 1920]
///
/// The Skeleton index is denominated in microseconds
/// (timestamp_denominator = 1_000_000); for a 48 kHz Vorbis stream
/// each keypoint timestamp is the page's granule * (1_000_000 / 48_000).
fn build_skeleton_indexed_seek_ogg() -> (Vec<u8>, Vec<(u64, i64)>) {
    // ---- Vorbis BOS id / comment / setup ----
    let v_id = vorbis_id_packet(2, 48_000);
    let v_comment = vorbis_comment_packet();
    let v_setup = vorbis_setup_packet();

    // ---- Vorbis data packets ----
    let data_packets: Vec<Vec<u8>> = vec![
        (0..40u8).collect(),
        (40..120u8).collect(),
        (0..200u8).collect(),
        (0..50u8).collect(),
    ];
    let data_granules: Vec<i64> = vec![480, 960, 1440, 1920];

    // ---- Skeleton 4.0 fishead ----
    let mut head = FisHead::new(Version::V4_0);
    head.presentation_time = Rational::new(0, 1000);
    head.basetime = Rational::new(0, 1000);
    head.segment_length = Some(0); // patched after total size known
    head.content_byte_offset = Some(0); // patched after header size known
    let head_packet = head.to_bytes();

    // ---- Skeleton fisbone for Vorbis ----
    let mut bone = FisBone::new(VORBIS_SERIAL, Rational::new(48_000, 1));
    bone.num_headers = 3;
    bone.preroll = 2;
    bone.set_header("Content-Type", "audio/vorbis");
    bone.set_header("Role", "audio/main");
    bone.set_header("Name", "main_audio");
    let bone_packet = bone.to_bytes();

    // ---- Pre-compute header-section pages so we know the byte offset
    //      of the first data page. Skeleton index keypoints need
    //      absolute byte offsets into the file.
    let mut header_section = Vec::new();
    header_section.extend_from_slice(&single_packet_page(
        &head_packet,
        flags::FIRST_PAGE,
        SKEL_SERIAL,
        0,
        0,
    ));
    header_section.extend_from_slice(&single_packet_page(
        &v_id,
        flags::FIRST_PAGE,
        VORBIS_SERIAL,
        0,
        0,
    ));
    header_section.extend_from_slice(&single_packet_page(&v_comment, 0, VORBIS_SERIAL, 1, 0));
    header_section.extend_from_slice(&single_packet_page(&bone_packet, 0, SKEL_SERIAL, 1, 0));

    // ---- Now construct the Skeleton index packet. We need to know
    //      where each data page will land in the final file, which is
    //      `header_section.len() + index_page_size + setup_page_size
    //      + skeleton_eos_page_size + sum(prior data pages)`. The
    //      page-framing overhead is deterministic so we can compute it.
    //      Page header = 27 + n_segs, where each segment carries up to
    //      255 bytes. Use `lace(packet.len()).len()` to count segments.
    let setup_page_size = 27 + oxideav_ogg::page::lace(v_setup.len()).len() + v_setup.len();
    // 0-length packet -> single 0-byte segment in the lacing table; the
    // page itself is 27 header bytes + 1 lacing byte + 0 data bytes.
    let skel_eos_page_size = 27 + 1;
    let data_page_sizes: Vec<usize> = data_packets
        .iter()
        .map(|p| 27 + oxideav_ogg::page::lace(p.len()).len() + p.len())
        .collect();

    let denom: i64 = 1_000_000;
    // Stream time-base is (1, 48_000) so seconds_of(g) = g / 48_000 and
    // index timestamp = g * denom / 48_000.
    let granule_to_index_ts = |g: i64| -> i64 { g * denom / 48_000 };

    // The Skeleton index packet's byte size depends on its keypoint VBI
    // deltas, which depend on the keypoint absolute offsets, which depend
    // on... the index packet's byte size (because the first data page
    // sits past the index page). Resolve via fixed-point iteration:
    // guess a size, compute offsets, build the index, measure its real
    // size, retry until the guess matches reality. Converges in 1–2
    // iterations for sane keypoint counts.
    let mut index_page_size: usize = 100; // generous starting guess
    let (idx_packet, expected, first_data_offset) = loop {
        let first_data_offset =
            header_section.len() + index_page_size + setup_page_size + skel_eos_page_size;
        let mut idx = SkelIndex::new(VORBIS_SERIAL, denom);
        idx.first_sample_time = 0;
        idx.last_sample_time = granule_to_index_ts(*data_granules.last().unwrap());
        let mut cumulative = first_data_offset;
        let mut expected: Vec<(u64, i64)> = Vec::with_capacity(data_packets.len());
        for (i, gr) in data_granules.iter().enumerate() {
            idx.push(cumulative as u64, granule_to_index_ts(*gr));
            expected.push((cumulative as u64, *gr));
            cumulative += data_page_sizes[i];
        }
        let idx_packet = idx.to_bytes();
        let actual_index_page_size =
            27 + oxideav_ogg::page::lace(idx_packet.len()).len() + idx_packet.len();
        if actual_index_page_size == index_page_size {
            break (idx_packet, expected, first_data_offset);
        }
        index_page_size = actual_index_page_size;
    };
    let _ = first_data_offset; // only used for keypoint offsets above

    // ---- Final assembly ----
    let mut out = header_section;
    out.extend_from_slice(&single_packet_page(&idx_packet, 0, SKEL_SERIAL, 2, 0));
    out.extend_from_slice(&single_packet_page(&v_setup, 0, VORBIS_SERIAL, 2, 0));
    out.extend_from_slice(&single_packet_page(
        &[],
        flags::LAST_PAGE,
        SKEL_SERIAL,
        3,
        0,
    ));
    let n = data_packets.len();
    for (i, (pkt, gr)) in data_packets.iter().zip(data_granules.iter()).enumerate() {
        let is_last = i + 1 == n;
        let flag = if is_last { flags::LAST_PAGE } else { 0 };
        out.extend_from_slice(&single_packet_page(
            pkt,
            flag,
            VORBIS_SERIAL,
            3 + i as u32,
            *gr,
        ));
    }

    // Spot-check the expected offsets line up with the real file bytes:
    // each expected[i].0 must be the start of an OggS page header.
    for (off, _) in &expected {
        let off = *off as usize;
        assert!(off + 4 <= out.len(), "expected offset out of bounds");
        assert_eq!(
            &out[off..off + 4],
            b"OggS",
            "expected offset must land on an OggS page boundary"
        );
    }

    (out, expected)
}

#[test]
fn skeleton_index_accelerated_seek_lands_on_indexed_keypoint() {
    let (bytes, expected_keypoints) = build_skeleton_indexed_seek_ogg();
    let reader: Box<dyn ReadSeek> = Box::new(Cursor::new(bytes));
    let codecs = NullCodecResolver;
    let mut dmx = oxideav_ogg::demux::open_concrete(reader, &codecs).expect("open ok");
    assert!(dmx.skeleton().is_some(), "Skeleton 4.0 index expected");
    assert_eq!(dmx.skeleton_index_seek_count(), 0, "no seeks yet");

    // Vorbis stream's time_base is (1, 48_000), so pts IS the granule.
    // Seek to a pts that sits between keypoints 1 and 2 — the Skeleton
    // index floor lookup should land on keypoint 1 (granule 960).
    let target_pts = 1200i64;
    let landed = oxideav_core::Demuxer::seek_to(&mut dmx, 0, target_pts).expect("seek ok");
    assert_eq!(
        landed, expected_keypoints[1].1,
        "should land on the floor keypoint's granule (960), not bisect past it"
    );
    assert_eq!(
        dmx.skeleton_index_seek_count(),
        1,
        "fast path should have fired exactly once"
    );

    // Exact-keypoint hit: seeking to granule 1440 should land at 1440 too.
    let landed = oxideav_core::Demuxer::seek_to(&mut dmx, 0, 1440).expect("seek ok");
    assert_eq!(landed, expected_keypoints[2].1);
    assert_eq!(dmx.skeleton_index_seek_count(), 2, "second fast-path seek");

    // Out-of-range below (pts < first keypoint timestamp) must fall
    // back to the bisection path — the index has no floor for it.
    // Build a fresh demuxer because seek_to's state-flushing on the
    // previous calls left the input near a data page, and we want a
    // clean assertion that the counter does NOT advance here.
    let (bytes2, _) = build_skeleton_indexed_seek_ogg();
    let reader2: Box<dyn ReadSeek> = Box::new(Cursor::new(bytes2));
    let mut dmx2 = oxideav_ogg::demux::open_concrete(reader2, &codecs).expect("open ok");
    // First keypoint's granule is 480; ask for 100 which sits below.
    let _ = oxideav_core::Demuxer::seek_to(&mut dmx2, 0, 100);
    assert_eq!(
        dmx2.skeleton_index_seek_count(),
        0,
        "below-first-keypoint targets must fall back to bisection, not fast path"
    );
}

#[test]
fn skeleton_index_seek_absent_when_no_index_packet() {
    // A Skeleton-bearing file whose Skeleton stream has a fishead +
    // fisbone but NO `index\0` packet (the 4.0 spec leaves the index
    // optional). Seek requests must work via the existing bisection
    // path, and the fast-path counter must stay at 0.
    let v_id = vorbis_id_packet(2, 48_000);
    let v_comment = vorbis_comment_packet();
    let v_setup = vorbis_setup_packet();
    let head = FisHead::new(Version::V4_0);
    let head_packet = head.to_bytes();
    let mut bone = FisBone::new(VORBIS_SERIAL, Rational::new(48_000, 1));
    bone.num_headers = 3;
    bone.set_header("Content-Type", "audio/vorbis");
    let bone_packet = bone.to_bytes();

    let mut out = Vec::new();
    out.extend_from_slice(&single_packet_page(
        &head_packet,
        flags::FIRST_PAGE,
        SKEL_SERIAL,
        0,
        0,
    ));
    out.extend_from_slice(&single_packet_page(
        &v_id,
        flags::FIRST_PAGE,
        VORBIS_SERIAL,
        0,
        0,
    ));
    out.extend_from_slice(&single_packet_page(&v_comment, 0, VORBIS_SERIAL, 1, 0));
    out.extend_from_slice(&single_packet_page(&bone_packet, 0, SKEL_SERIAL, 1, 0));
    out.extend_from_slice(&single_packet_page(&v_setup, 0, VORBIS_SERIAL, 2, 0));
    out.extend_from_slice(&single_packet_page(
        &[],
        flags::LAST_PAGE,
        SKEL_SERIAL,
        2,
        0,
    ));
    // One Vorbis data page so seek_to has something to land on.
    let data: Vec<u8> = (0..32u8).collect();
    out.extend_from_slice(&single_packet_page(
        &data,
        flags::LAST_PAGE,
        VORBIS_SERIAL,
        3,
        960,
    ));

    let reader: Box<dyn ReadSeek> = Box::new(Cursor::new(out));
    let codecs = NullCodecResolver;
    let mut dmx = oxideav_ogg::demux::open_concrete(reader, &codecs).expect("open ok");
    let sk = dmx.skeleton().expect("Skeleton present");
    assert!(sk.indexes.is_empty(), "no index packet was emitted");
    // Seek doesn't need to succeed; just confirm the fast-path counter
    // doesn't tick when no index is available.
    let _ = oxideav_core::Demuxer::seek_to(&mut dmx, 0, 480);
    assert_eq!(
        dmx.skeleton_index_seek_count(),
        0,
        "no index packet → fast path must not fire"
    );
}

#[test]
fn skeleton_detector_helpers_match_magic_bytes() {
    // Spot-check the public detector helpers — these aren't on a demux
    // path but sister tools may want them. (skeleton.rs has unit-test
    // coverage too; this is the public-API smoke check.)
    assert!(skeleton::is_fishead(b"fishead\0"));
    assert!(skeleton::is_fisbone(b"fisbone\0"));
    assert!(skeleton::is_index(b"index\0"));
    assert!(!skeleton::is_fishead(b"OpusHead"));
    assert!(!skeleton::is_fisbone(b"OggS"));
    assert!(!skeleton::is_index(b"fishead\0"));
}
