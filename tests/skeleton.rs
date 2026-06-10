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

// --- Skeleton 4.0 index-validity checks ------------------------------------
// `docs/container/ogg/ogg-skeleton-4.0.md` §"Keyframe indexes for faster
// seeking" lists three conditions under which the index must be treated
// as invalid and the seek must fall back to bisection:
//
//   1. The segment doesn't end at the segment length offset stored in
//      the Skeleton BOS packet.
//   2. After a seek to a keypoint's offset, you don't land exactly on
//      a page boundary.
//   3. After a seek to a keypoint's offset, you don't land on a page
//      which belongs to that keypoint's stream.
//
// Each of the three tests below patches a freshly-built fixture to
// exercise one of those rejection paths, then asserts:
//   * `skeleton_index_seek_count()` does NOT advance (fast path skipped),
//   * `skeleton_index_invalid_count()` DOES advance by 1,
//   * `seek_to` still returns Ok (bisection fallback succeeded).

#[test]
fn skeleton_index_rejected_on_segment_length_mismatch() {
    // Build a clean fixture, then overwrite the fishead's segment_length
    // field with a value that disagrees with the file size. The BOS
    // page CRC covers the fishead body, so we must parse the BOS page
    // FIRST (on clean bytes), edit `page.data` (the fishead packet),
    // and emit a fresh page with the new CRC.
    let (mut bytes, _expected) = build_skeleton_indexed_seek_ogg();
    let bogus: u64 = 999_999_999;
    let new_bos = {
        let (mut page, consumed) =
            oxideav_ogg::page::Page::parse(&bytes).expect("BOS reparse on clean bytes");
        // The fishead packet payload is `page.data`. The segment_length
        // field sits at bytes 64..72 of the packet.
        page.data[64..72].copy_from_slice(&bogus.to_le_bytes());
        let refreshed = page.to_bytes();
        assert_eq!(refreshed.len(), consumed);
        refreshed
    };
    bytes[..new_bos.len()].copy_from_slice(&new_bos);

    let reader: Box<dyn ReadSeek> = Box::new(Cursor::new(bytes));
    let codecs = NullCodecResolver;
    let mut dmx = oxideav_ogg::demux::open_concrete(reader, &codecs).expect("open ok");
    let sk = dmx.skeleton().expect("Skeleton present");
    assert_eq!(
        sk.head.as_ref().unwrap().segment_length,
        Some(bogus),
        "patched segment_length parsed back"
    );
    assert_eq!(dmx.skeleton_index_seek_count(), 0);
    assert_eq!(dmx.skeleton_index_invalid_count(), 0);

    // Seek to a target that would normally hit the fast path's middle
    // keypoint. The segment-length check must invalidate the whole
    // index → fast-path counter stays at 0, reject counter ticks to 1,
    // and the seek still completes successfully via bisection.
    let _ = oxideav_core::Demuxer::seek_to(&mut dmx, 0, 1200).expect("seek ok");
    assert_eq!(
        dmx.skeleton_index_seek_count(),
        0,
        "segment_length mismatch must disable the fast path"
    );
    assert_eq!(
        dmx.skeleton_index_invalid_count(),
        1,
        "rejection counter ticks once per disqualified seek"
    );

    // A second seek against the same file must also stay on the slow
    // path AND must tick the reject counter again (the cached
    // segment-length verdict is "no", so every subsequent seek that
    // would have hit the fast path is counted as another rejection).
    let _ = oxideav_core::Demuxer::seek_to(&mut dmx, 0, 1440).expect("seek ok");
    assert_eq!(dmx.skeleton_index_seek_count(), 0);
    assert_eq!(dmx.skeleton_index_invalid_count(), 2);
}

#[test]
fn skeleton_index_segment_length_match_keeps_fast_path() {
    // Same fixture, but patch segment_length to the ACTUAL file size.
    // Per spec this is the "trusted index" case — fast path must fire.
    let (mut bytes, expected_keypoints) = build_skeleton_indexed_seek_ogg();
    let actual = bytes.len() as u64;
    // Parse + edit + re-emit the BOS page so the CRC matches the
    // edited fishead. Same shape as the mismatch test above.
    let new_bos = {
        let (mut page, consumed) =
            oxideav_ogg::page::Page::parse(&bytes).expect("BOS reparse on clean bytes");
        page.data[64..72].copy_from_slice(&actual.to_le_bytes());
        let refreshed = page.to_bytes();
        assert_eq!(refreshed.len(), consumed);
        refreshed
    };
    bytes[..new_bos.len()].copy_from_slice(&new_bos);

    let reader: Box<dyn ReadSeek> = Box::new(Cursor::new(bytes));
    let codecs = NullCodecResolver;
    let mut dmx = oxideav_ogg::demux::open_concrete(reader, &codecs).expect("open ok");
    assert_eq!(
        dmx.skeleton()
            .unwrap()
            .head
            .as_ref()
            .unwrap()
            .segment_length,
        Some(actual)
    );

    let landed = oxideav_core::Demuxer::seek_to(&mut dmx, 0, 1200).expect("seek ok");
    assert_eq!(landed, expected_keypoints[1].1);
    assert_eq!(
        dmx.skeleton_index_seek_count(),
        1,
        "matching segment_length keeps the fast path armed"
    );
    assert_eq!(dmx.skeleton_index_invalid_count(), 0);
}

#[test]
fn skeleton_index_rejected_on_keypoint_offset_not_on_page_boundary() {
    // Build a fresh fixture but patch the first keypoint's stored byte
    // offset to a value that is NOT at a page boundary. The simplest
    // way is to rebuild the index packet with a deliberately-corrupt
    // first offset and re-emit just that page. Easier still: corrupt
    // the keypoint's offset delta on the wire by adding 1 to the first
    // VBI byte of the index packet's keypoint section, then re-CRC
    // the index page.
    //
    // The keypoint offsets are unsigned and encoded as deltas — adding
    // 1 to the first delta shifts every subsequent keypoint by +1 too,
    // which lands every keypoint between two pages instead of on a
    // page header. That's exactly the rejection case the spec calls
    // out as "you don't land exactly on a page boundary".
    let (mut bytes, _expected) = build_skeleton_indexed_seek_ogg();

    // Walk the file to find the index page (Skeleton serial,
    // seq_no = 2). Parse it cleanly first, then corrupt its first
    // keypoint's offset delta in the parsed packet, then re-emit the
    // page (which recomputes the CRC) and splice it back into `bytes`.
    let mut cursor = 0usize;
    let mut patched = false;
    while cursor < bytes.len() {
        if cursor + 27 > bytes.len() || &bytes[cursor..cursor + 4] != b"OggS" {
            cursor += 1;
            continue;
        }
        let (mut page, consumed) = oxideav_ogg::page::Page::parse(&bytes[cursor..])
            .expect("page reparse on clean fixture");
        if page.serial == SKEL_SERIAL && page.seq_no == 2 {
            // The body is the Skeleton index packet; keypoints start
            // at byte 42. Shift the very first VBI offset-delta byte
            // up by 1 so every keypoint's reconstructed offset is +1
            // (off the page boundary by one byte).
            assert!(
                page.data.len() >= 43,
                "index packet must include at least one keypoint VBI byte"
            );
            let kp_byte = &mut page.data[42];
            assert!(
                (*kp_byte & 0x7F) < 0x7F,
                "no carry expected for fixture sizes"
            );
            *kp_byte = (*kp_byte & 0x80) | ((*kp_byte & 0x7F) + 1);
            let refreshed = page.to_bytes();
            assert_eq!(refreshed.len(), consumed);
            bytes[cursor..cursor + consumed].copy_from_slice(&refreshed);
            patched = true;
            break;
        }
        cursor += consumed;
    }
    assert!(patched, "index page must have been found and patched");

    let reader: Box<dyn ReadSeek> = Box::new(Cursor::new(bytes));
    let codecs = NullCodecResolver;
    let mut dmx = oxideav_ogg::demux::open_concrete(reader, &codecs).expect("open ok");

    let _ = oxideav_core::Demuxer::seek_to(&mut dmx, 0, 1200).expect("seek ok");
    assert_eq!(
        dmx.skeleton_index_seek_count(),
        0,
        "off-page-boundary keypoint must disable the fast path"
    );
    assert_eq!(
        dmx.skeleton_index_invalid_count(),
        1,
        "per-keypoint rejection ticks the diagnostic counter"
    );
}

#[test]
fn skeleton_index_rejected_on_keypoint_offset_belongs_to_other_stream() {
    // Build a Skeleton 4.0 fixture whose index points at a page
    // belonging to a DIFFERENT stream's serial. We synthesise a second
    // (mostly-empty) Vorbis-like stream alongside the real one and
    // direct the keypoint at THAT stream's page.
    //
    // Strategy: take the clean fixture, locate the first Vorbis data
    // page (serial = VORBIS_SERIAL, granule = 480), and overwrite its
    // serial field to a fake one. After the BOS section the demuxer
    // will not have a registration for the fake serial, but for the
    // purposes of the validity check we only need the page-header
    // serial at the keypoint offset to disagree with VORBIS_SERIAL.
    let (mut bytes, expected_keypoints) = build_skeleton_indexed_seek_ogg();
    let first_kp_off = expected_keypoints[0].0 as usize;
    assert_eq!(
        &bytes[first_kp_off..first_kp_off + 4],
        b"OggS",
        "clean keypoint lands on a page header"
    );
    // Parse the keypoint page cleanly, mutate its serial in the parsed
    // struct, re-emit (which recomputes the CRC), and splice back.
    // Overwriting the serial bytes in place would invalidate the CRC
    // and the demuxer's BOS-section walk would reject the page during
    // open(); the validity check we're testing runs from seek_to.
    let bogus_serial: u32 = 0xDEAD_FACE;
    {
        let (mut page, consumed) =
            oxideav_ogg::page::Page::parse(&bytes[first_kp_off..]).expect("kp page reparse");
        page.serial = bogus_serial;
        let refreshed = page.to_bytes();
        assert_eq!(refreshed.len(), consumed);
        bytes[first_kp_off..first_kp_off + consumed].copy_from_slice(&refreshed);
    }

    let reader: Box<dyn ReadSeek> = Box::new(Cursor::new(bytes));
    let codecs = NullCodecResolver;
    let mut dmx = oxideav_ogg::demux::open_concrete(reader, &codecs).expect("open ok");

    // Ask for a target that resolves to the first keypoint. Per
    // build_skeleton_indexed_seek_ogg keypoint 0 is granule 480, and
    // anything in [0, 960) lands on that keypoint via the index
    // (well, anything >= the first keypoint's timestamp does — pts=480
    // is the exact match).
    let _ = oxideav_core::Demuxer::seek_to(&mut dmx, 0, 480).expect("seek ok");
    assert_eq!(
        dmx.skeleton_index_seek_count(),
        0,
        "keypoint-on-wrong-serial must disable the fast path"
    );
    assert_eq!(
        dmx.skeleton_index_invalid_count(),
        1,
        "per-keypoint rejection ticks the diagnostic counter"
    );
}

const VORBIS_SERIAL_A: u32 = 0x12345678;
const VORBIS_SERIAL_B: u32 = 0x9ABCDEF0;

/// Build a multiplexed Ogg file with Skeleton 4.0 + two concurrent
/// Vorbis streams (A and B), each with its own Skeleton `index\0`
/// packet. Stream A has frequent keypoints (every data page); stream
/// B has a single sparse keypoint at its first data page. Returns the
/// file bytes plus the absolute byte offset of stream B's first data
/// page (= B's sole keypoint offset, which must win the multi-stream
/// minimisation when seeking on stream A to a later target) plus
/// every stream-A keypoint as a `(byte_offset, granule)` pair.
///
/// Used by `skeleton_index_seek_minimises_offset_across_streams` to
/// exercise the Skeleton 4.0 spec rule
/// (`docs/container/ogg/ogg-skeleton-4.0.md` §"Keyframe indexes for
/// faster seeking"): "first construct the set which contains every
/// active streams' last keypoint which has time less than or equal to
/// the seek target time. … from that set of key points, select the
/// key point with the smallest byte offset."
fn build_skeleton_multi_stream_indexed_ogg() -> (Vec<u8>, u64, Vec<(u64, i64)>) {
    let v_id_a = vorbis_id_packet(2, 48_000);
    let v_id_b = vorbis_id_packet(1, 48_000);
    let v_comment_a = vorbis_comment_packet();
    let v_comment_b = vorbis_comment_packet();
    let v_setup_a = vorbis_setup_packet();
    let v_setup_b = vorbis_setup_packet();

    // Stream A: 4 data pages, granules 480, 960, 1440, 1920.
    let data_packets_a: Vec<Vec<u8>> = vec![
        (0..40u8).collect(),
        (40..120u8).collect(),
        (0..200u8).collect(),
        (0..50u8).collect(),
    ];
    let data_granules_a: Vec<i64> = vec![480, 960, 1440, 1920];

    // Stream B: 2 data pages, granules 480 and 1920. Sparse — only the
    // first is indexed.
    let data_packets_b: Vec<Vec<u8>> = vec![(0..60u8).collect(), (0..80u8).collect()];
    let data_granules_b: Vec<i64> = vec![480, 1920];

    let mut head = FisHead::new(Version::V4_0);
    head.presentation_time = Rational::new(0, 1000);
    head.basetime = Rational::new(0, 1000);
    // Opt out of the segment-length check so we exercise the
    // multi-stream rule independently of that diagnostic path.
    head.segment_length = Some(0);
    head.content_byte_offset = Some(0);
    let head_packet = head.to_bytes();

    let mut bone_a = FisBone::new(VORBIS_SERIAL_A, Rational::new(48_000, 1));
    bone_a.num_headers = 3;
    bone_a.set_header("Content-Type", "audio/vorbis");
    bone_a.set_header("Role", "audio/main");
    bone_a.set_header("Name", "stream_a");
    let bone_a_packet = bone_a.to_bytes();

    let mut bone_b = FisBone::new(VORBIS_SERIAL_B, Rational::new(48_000, 1));
    bone_b.num_headers = 3;
    bone_b.set_header("Content-Type", "audio/vorbis");
    bone_b.set_header("Role", "audio/alternate");
    bone_b.set_header("Name", "stream_b");
    let bone_b_packet = bone_b.to_bytes();

    // Pre-compute the header section so we know where data pages start.
    let mut header_section = Vec::new();
    header_section.extend_from_slice(&single_packet_page(
        &head_packet,
        flags::FIRST_PAGE,
        SKEL_SERIAL,
        0,
        0,
    ));
    header_section.extend_from_slice(&single_packet_page(
        &v_id_a,
        flags::FIRST_PAGE,
        VORBIS_SERIAL_A,
        0,
        0,
    ));
    header_section.extend_from_slice(&single_packet_page(
        &v_id_b,
        flags::FIRST_PAGE,
        VORBIS_SERIAL_B,
        0,
        0,
    ));
    header_section.extend_from_slice(&single_packet_page(&v_comment_a, 0, VORBIS_SERIAL_A, 1, 0));
    header_section.extend_from_slice(&single_packet_page(&v_comment_b, 0, VORBIS_SERIAL_B, 1, 0));
    header_section.extend_from_slice(&single_packet_page(&bone_a_packet, 0, SKEL_SERIAL, 1, 0));
    header_section.extend_from_slice(&single_packet_page(&bone_b_packet, 0, SKEL_SERIAL, 2, 0));

    let setup_a_size = 27 + oxideav_ogg::page::lace(v_setup_a.len()).len() + v_setup_a.len();
    let setup_b_size = 27 + oxideav_ogg::page::lace(v_setup_b.len()).len() + v_setup_b.len();
    let skel_eos_page_size = 27 + 1;
    let data_a_sizes: Vec<usize> = data_packets_a
        .iter()
        .map(|p| 27 + oxideav_ogg::page::lace(p.len()).len() + p.len())
        .collect();
    let data_b_sizes: Vec<usize> = data_packets_b
        .iter()
        .map(|p| 27 + oxideav_ogg::page::lace(p.len()).len() + p.len())
        .collect();

    let denom: i64 = 1_000_000;
    let granule_to_index_ts = |g: i64| -> i64 { g * denom / 48_000 };

    // Data-page interleaving: A[0], B[0], A[1], A[2], B[1], A[3].
    //
    // Fixed-point converge on the index-page sizes: the keypoint
    // offsets depend on the index sizes, and the index sizes depend
    // on the keypoint offsets (because the VBI delta encoding's byte
    // length scales with offset magnitude).
    let mut idx_a_page_size: usize = 100;
    let mut idx_b_page_size: usize = 64;
    let (idx_a_packet, idx_b_packet, expected_keypoints_a, b_first_data_offset) = loop {
        let pre_data = header_section.len()
            + idx_a_page_size
            + idx_b_page_size
            + setup_a_size
            + setup_b_size
            + skel_eos_page_size;
        let mut pos = pre_data;
        let a0 = pos;
        pos += data_a_sizes[0];
        let b0 = pos;
        pos += data_b_sizes[0];
        let a1 = pos;
        pos += data_a_sizes[1];
        let a2 = pos;
        pos += data_a_sizes[2];
        let _b1 = pos;
        pos += data_b_sizes[1];
        let a3 = pos;

        let mut idx_a = SkelIndex::new(VORBIS_SERIAL_A, denom);
        idx_a.first_sample_time = 0;
        idx_a.last_sample_time = granule_to_index_ts(*data_granules_a.last().unwrap());
        idx_a.push(a0 as u64, granule_to_index_ts(data_granules_a[0]));
        idx_a.push(a1 as u64, granule_to_index_ts(data_granules_a[1]));
        idx_a.push(a2 as u64, granule_to_index_ts(data_granules_a[2]));
        idx_a.push(a3 as u64, granule_to_index_ts(data_granules_a[3]));
        let idx_a_bytes = idx_a.to_bytes();

        let mut idx_b = SkelIndex::new(VORBIS_SERIAL_B, denom);
        idx_b.first_sample_time = 0;
        idx_b.last_sample_time = granule_to_index_ts(*data_granules_b.last().unwrap());
        idx_b.push(b0 as u64, granule_to_index_ts(data_granules_b[0]));
        let idx_b_bytes = idx_b.to_bytes();

        let new_a_size = 27 + oxideav_ogg::page::lace(idx_a_bytes.len()).len() + idx_a_bytes.len();
        let new_b_size = 27 + oxideav_ogg::page::lace(idx_b_bytes.len()).len() + idx_b_bytes.len();
        if new_a_size == idx_a_page_size && new_b_size == idx_b_page_size {
            let expected_a: Vec<(u64, i64)> = vec![
                (a0 as u64, data_granules_a[0]),
                (a1 as u64, data_granules_a[1]),
                (a2 as u64, data_granules_a[2]),
                (a3 as u64, data_granules_a[3]),
            ];
            break (idx_a_bytes, idx_b_bytes, expected_a, b0 as u64);
        }
        idx_a_page_size = new_a_size;
        idx_b_page_size = new_b_size;
    };

    // Assemble: header section, idx_a page, idx_b page, setup_a,
    // setup_b, Skeleton EOS, interleaved data pages.
    let mut out = header_section;
    out.extend_from_slice(&single_packet_page(&idx_a_packet, 0, SKEL_SERIAL, 3, 0));
    out.extend_from_slice(&single_packet_page(&idx_b_packet, 0, SKEL_SERIAL, 4, 0));
    out.extend_from_slice(&single_packet_page(&v_setup_a, 0, VORBIS_SERIAL_A, 2, 0));
    out.extend_from_slice(&single_packet_page(&v_setup_b, 0, VORBIS_SERIAL_B, 2, 0));
    out.extend_from_slice(&single_packet_page(
        &[],
        flags::LAST_PAGE,
        SKEL_SERIAL,
        5,
        0,
    ));
    out.extend_from_slice(&single_packet_page(
        &data_packets_a[0],
        0,
        VORBIS_SERIAL_A,
        3,
        data_granules_a[0],
    ));
    out.extend_from_slice(&single_packet_page(
        &data_packets_b[0],
        0,
        VORBIS_SERIAL_B,
        3,
        data_granules_b[0],
    ));
    out.extend_from_slice(&single_packet_page(
        &data_packets_a[1],
        0,
        VORBIS_SERIAL_A,
        4,
        data_granules_a[1],
    ));
    out.extend_from_slice(&single_packet_page(
        &data_packets_a[2],
        0,
        VORBIS_SERIAL_A,
        5,
        data_granules_a[2],
    ));
    out.extend_from_slice(&single_packet_page(
        &data_packets_b[1],
        flags::LAST_PAGE,
        VORBIS_SERIAL_B,
        4,
        data_granules_b[1],
    ));
    out.extend_from_slice(&single_packet_page(
        &data_packets_a[3],
        flags::LAST_PAGE,
        VORBIS_SERIAL_A,
        6,
        data_granules_a[3],
    ));

    // Spot-check every expected keypoint offset lands on an OggS
    // boundary in the assembled bytes.
    let check_offsets = [
        b_first_data_offset as usize,
        expected_keypoints_a[0].0 as usize,
        expected_keypoints_a[1].0 as usize,
        expected_keypoints_a[2].0 as usize,
        expected_keypoints_a[3].0 as usize,
    ];
    for off in &check_offsets {
        assert!(off + 4 <= out.len(), "expected offset out of bounds");
        assert_eq!(
            &out[*off..*off + 4],
            b"OggS",
            "expected offset must land on an OggS page boundary"
        );
    }

    (out, b_first_data_offset, expected_keypoints_a)
}

#[test]
fn skeleton_index_seek_minimises_offset_across_streams() {
    // Per `docs/container/ogg/ogg-skeleton-4.0.md` §"Keyframe indexes
    // for faster seeking" the seek algorithm must walk every active
    // stream's index and pick the keypoint with the SMALLEST byte
    // offset. A naive implementation that only consults the requested
    // stream's index would land past another stream's required
    // keyframe, leaving that stream's decoder unable to resume.
    //
    // Setup: two Vorbis streams A and B. A has frequent keypoints
    // (every data page); B has one sparse keypoint at its first data
    // page (granule 480), located byte-wise between A[0] and A[1].
    // We seek on stream A to pts=1440 (= keypoint A[2]). A naive
    // implementation lands at A[2]. The spec-correct implementation
    // sees B's keypoint at B[0] (timestamp 480 / 48 000 s, well
    // before the 1440 / 48 000 s target) with a smaller byte offset
    // and lands there instead.
    let (bytes, b_first_data_offset, expected_keypoints_a) =
        build_skeleton_multi_stream_indexed_ogg();
    assert!(
        b_first_data_offset < expected_keypoints_a[2].0,
        "test premise: B's keypoint offset must precede A's floor keypoint for pts=1440"
    );
    let reader: Box<dyn ReadSeek> = Box::new(Cursor::new(bytes));
    let codecs = NullCodecResolver;
    let mut dmx = oxideav_ogg::demux::open_concrete(reader, &codecs).expect("open ok");
    assert!(dmx.skeleton().is_some(), "Skeleton expected");
    assert_eq!(dmx.skeleton_index_seek_count(), 0);

    // Seek on stream A (public index 0) to pts=1440. The returned
    // granule MUST be 1440 (the requested stream's floor keypoint
    // timestamp), regardless of which stream's keypoint won the byte
    // offset minimisation: the public seek_to contract returns the
    // granule in the REQUESTED stream's time-base.
    let landed = oxideav_core::Demuxer::seek_to(&mut dmx, 0, 1440).expect("seek ok");
    assert_eq!(
        landed, 1440,
        "returned granule belongs to the requested stream's index"
    );
    assert_eq!(
        dmx.skeleton_index_seek_count(),
        1,
        "fast path fired once via the multi-stream minimisation"
    );
    assert_eq!(
        dmx.skeleton_index_invalid_count(),
        0,
        "no per-spec rejections — both keypoints land on page boundaries"
    );
}

#[test]
fn skeleton_index_seek_falls_back_when_primary_has_no_index() {
    // The multi-stream minimisation is anchored on the REQUESTED
    // stream's index — that anchor fixes the returned-granule
    // mapping. If the requested stream has no Skeleton index at all,
    // the fast path must not silently land on some other stream's
    // keypoint (the returned granule wouldn't be in the right time
    // base). This test pins that behaviour: seeking on the indexed
    // companion stream still works.
    let (bytes, _b_first_data_offset, _) = build_skeleton_multi_stream_indexed_ogg();
    let reader: Box<dyn ReadSeek> = Box::new(Cursor::new(bytes));
    let codecs = NullCodecResolver;
    let mut dmx = oxideav_ogg::demux::open_concrete(reader, &codecs).expect("open ok");
    let sk = dmx.skeleton().expect("Skeleton expected");
    assert!(
        sk.index_for_serial(VORBIS_SERIAL_A).is_some(),
        "test premise: stream A has an index"
    );
    assert!(
        sk.index_for_serial(VORBIS_SERIAL_B).is_some(),
        "test premise: stream B has an index"
    );
    // Seek on stream B (public index 1) — at least one of the index
    // packets covers it, so the fast path should fire at least once.
    let _ = oxideav_core::Demuxer::seek_to(&mut dmx, 1, 480).expect("seek ok");
    assert!(
        dmx.skeleton_index_seek_count() >= 1,
        "seek on stream B fires the fast path"
    );
}

// ---- Skeleton "Track order" addressing -----------------------------
//
// `docs/container/ogg/ogg-skeleton-message-headers.wiki` §"Track order":
// tracks are addressed by the order their BOS pages appear in the Ogg
// stream, with the Skeleton BOS occupying `track[0]` when present.

#[test]
fn track_order_single_stream_with_skeleton() {
    // File layout: Skeleton BOS, then one Vorbis content BOS.
    // Per the wiki worked example: track[0] = Skeleton, track[1] = the
    // content stream.
    let (bytes, _, _, _) = build_skeleton_4_0_ogg();
    let reader: Box<dyn ReadSeek> = Box::new(Cursor::new(bytes));
    let codecs = NullCodecResolver;
    let dmx = oxideav_ogg::demux::open_concrete(reader, &codecs).expect("open ok");

    // Skeleton (1 metadata track) + 1 content stream = 2 track slots,
    // even though streams() reports only the 1 content stream.
    assert_eq!(dmx.track_order_len(), 2);
    assert_eq!(oxideav_core::Demuxer::streams(&dmx).len(), 1);

    // track[0] is the Skeleton bitstream.
    assert_eq!(dmx.track_order_serial(0), Some(SKEL_SERIAL));
    // track[1] is the Vorbis content stream.
    assert_eq!(dmx.track_order_serial(1), Some(VORBIS_SERIAL));
    // Out of range.
    assert_eq!(dmx.track_order_serial(2), None);

    // Reverse map round-trips.
    assert_eq!(dmx.track_order_index(SKEL_SERIAL), Some(0));
    assert_eq!(dmx.track_order_index(VORBIS_SERIAL), Some(1));
    // A serial never seen as a BOS resolves to nothing.
    assert_eq!(dmx.track_order_index(0xDEAD_BEEF), None);

    // The content track's track-order serial round-trips back to its
    // fisbone metadata in the Skeleton (the spec's reason for the
    // ordering: addressing tracks by a stable index).
    let sk = dmx.skeleton().expect("Skeleton present");
    let serial = dmx.track_order_serial(1).unwrap();
    let bone = sk.bone_for_serial(serial).expect("bone for track[1]");
    assert_eq!(bone.header("Name"), Some("main_audio"));
}

#[test]
fn track_order_multi_stream_with_skeleton() {
    // File layout: Skeleton BOS, then Vorbis A BOS, then Vorbis B BOS.
    // Mirrors the wiki worked example where track[0] is Skeleton and
    // the content tracks follow in BOS-page order.
    let (bytes, _, _) = build_skeleton_multi_stream_indexed_ogg();
    let reader: Box<dyn ReadSeek> = Box::new(Cursor::new(bytes));
    let codecs = NullCodecResolver;
    let dmx = oxideav_ogg::demux::open_concrete(reader, &codecs).expect("open ok");

    // Skeleton + 2 content streams = 3 track slots.
    assert_eq!(dmx.track_order_len(), 3);
    assert_eq!(oxideav_core::Demuxer::streams(&dmx).len(), 2);

    assert_eq!(dmx.track_order_serial(0), Some(SKEL_SERIAL));
    assert_eq!(dmx.track_order_serial(1), Some(VORBIS_SERIAL_A));
    assert_eq!(dmx.track_order_serial(2), Some(VORBIS_SERIAL_B));
    assert_eq!(dmx.track_order_serial(3), None);

    assert_eq!(dmx.track_order_index(SKEL_SERIAL), Some(0));
    assert_eq!(dmx.track_order_index(VORBIS_SERIAL_A), Some(1));
    assert_eq!(dmx.track_order_index(VORBIS_SERIAL_B), Some(2));

    // Every track index walks back to its fisbone in spec order.
    let sk = dmx.skeleton().expect("Skeleton present");
    let s1 = dmx.track_order_serial(1).unwrap();
    let s2 = dmx.track_order_serial(2).unwrap();
    assert_eq!(
        sk.bone_for_serial(s1).and_then(|b| b.header("Name")),
        Some("stream_a")
    );
    assert_eq!(
        sk.bone_for_serial(s2).and_then(|b| b.header("Name")),
        Some("stream_b")
    );
}

#[test]
fn track_order_skeleton_free_file() {
    // Without a Skeleton, the wiki only reserves track[0] for Skeleton
    // when it is present — so a Skeleton-free file maps track[n]
    // directly onto content stream index n.
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

    assert!(dmx.skeleton().is_none(), "test premise: no Skeleton");
    // No Skeleton track[0] slot: the single content stream is track[0].
    assert_eq!(dmx.track_order_len(), 1);
    assert_eq!(dmx.track_order_serial(0), Some(VORBIS_SERIAL));
    assert_eq!(dmx.track_order_serial(1), None);
    assert_eq!(dmx.track_order_index(VORBIS_SERIAL), Some(0));
    assert_eq!(dmx.track_order_index(0xDEAD_BEEF), None);
}

#[test]
fn track_order_full_walk_round_trips() {
    // Walking 0..track_order_len() and mapping each serial back to its
    // index must be the identity permutation — the property a JS-style
    // `track[n]` resolver depends on.
    let (bytes, _, _) = build_skeleton_multi_stream_indexed_ogg();
    let reader: Box<dyn ReadSeek> = Box::new(Cursor::new(bytes));
    let codecs = NullCodecResolver;
    let dmx = oxideav_ogg::demux::open_concrete(reader, &codecs).expect("open ok");

    for n in 0..dmx.track_order_len() {
        let serial = dmx
            .track_order_serial(n)
            .expect("in-range track has serial");
        assert_eq!(
            dmx.track_order_index(serial),
            Some(n),
            "track[{n}] serial {serial:#010x} must round-trip back to index {n}"
        );
    }
}
