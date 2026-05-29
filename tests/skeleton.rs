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
