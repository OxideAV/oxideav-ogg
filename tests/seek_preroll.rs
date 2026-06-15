//! Preroll-aware seek tests for `OggDemuxer::seek_to_with_preroll`.
//!
//! `docs/container/ogg/ogg-skeleton-4.0.md` §"How to describe the logical
//! bitstreams within an Ogg container?" defines a per-track **preroll**:
//! "the number of past content packets to take into account when decoding
//! the current Ogg page, which is necessary for seeking (vorbis has
//! generally 2, speex 3)". A bare `seek_to` lands the input on the page
//! whose granule floors the target, but a decoder resuming there is
//! missing the preroll packets of warm-up context. `seek_to_with_preroll`
//! backs the resume offset up so those packets precede the landed page.
//!
//! These tests synthesise a Skeleton 4.0 + Vorbis physical stream (no
//! `index\0` packet, so `seek_to` exercises the bisection path) with a
//! fisbone declaring `preroll = 2` and `num_headers = 3`, then compare
//! the resume byte offset of `seek_to` against `seek_to_with_preroll`.

use std::io::Cursor;

use oxideav_core::{Demuxer, NullCodecResolver, ReadSeek};

use oxideav_ogg::page::{flags, lace, Page};
use oxideav_ogg::skeleton::{FisBone, FisHead, Rational, Version};

const SKEL_SERIAL: u32 = 0xCAFEBABE;
const VORBIS_SERIAL: u32 = 0x12345678;

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

/// Build a Skeleton 4.0 + Vorbis stream with `preroll` declared on the
/// fisbone and four content data pages. Returns the file bytes plus the
/// absolute byte offsets of the four content data pages (in file order).
fn build_preroll_ogg(preroll: u32, num_headers: u32) -> (Vec<u8>, Vec<u64>) {
    let v_id = vorbis_id_packet(2, 48_000);
    let v_comment = vorbis_comment_packet();
    let v_setup = vorbis_setup_packet();

    let mut head = FisHead::new(Version::V4_0);
    head.presentation_time = Rational::new(0, 1000);
    head.basetime = Rational::new(0, 1000);
    head.segment_length = Some(0); // opt out of the index segment-length check
    head.content_byte_offset = Some(0);
    let head_packet = head.to_bytes();

    let mut bone = FisBone::new(VORBIS_SERIAL, Rational::new(48_000, 1));
    bone.num_headers = num_headers;
    bone.preroll = preroll;
    bone.set_header("Content-Type", "audio/vorbis");
    bone.set_header("Role", "audio/main");
    bone.set_header("Name", "main_audio");
    let bone_packet = bone.to_bytes();

    // Header section: fishead BOS, Vorbis id BOS, comment, fisbone, setup,
    // Skeleton EOS empty.
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

    // Four content data pages, each a single packet, monotonically
    // increasing granules.
    let data_packets: Vec<Vec<u8>> = vec![
        (0..40u8).collect(),
        (40..120u8).collect(),
        (0..200u8).collect(),
        (0..50u8).collect(),
    ];
    let data_granules: Vec<i64> = vec![480, 960, 1440, 1920];

    let mut data_offsets = Vec::with_capacity(4);
    let n = data_packets.len();
    for (i, (pkt, gr)) in data_packets.iter().zip(data_granules.iter()).enumerate() {
        data_offsets.push(out.len() as u64);
        let flag = if i + 1 == n { flags::LAST_PAGE } else { 0 };
        out.extend_from_slice(&single_packet_page(
            pkt,
            flag,
            VORBIS_SERIAL,
            3 + i as u32,
            *gr,
        ));
    }

    // Sanity: every recorded data offset starts an OggS page.
    for off in &data_offsets {
        let off = *off as usize;
        assert_eq!(
            &out[off..off + 4],
            b"OggS",
            "data offset must be a page boundary"
        );
    }

    (out, data_offsets)
}

fn open(bytes: Vec<u8>) -> oxideav_ogg::demux::OggDemuxer {
    let reader: Box<dyn ReadSeek> = Box::new(Cursor::new(bytes));
    let codecs = NullCodecResolver;
    oxideav_ogg::demux::open_concrete(reader, &codecs).expect("open ok")
}

#[test]
fn bare_seek_lands_on_floor_page_without_preroll() {
    // Baseline: a plain `seek_to` lands the input cursor on the floor
    // page (granule 1440 = data page index 2).
    let (bytes, data_offsets) = build_preroll_ogg(2, 3);
    let mut dmx = open(bytes);
    let landed = Demuxer::seek_to(&mut dmx, 0, 1440).expect("seek ok");
    assert_eq!(landed, 1440);
    // The resume offset is exactly data page 2's boundary.
    assert_eq!(dmx.input_position().expect("pos"), data_offsets[2]);
}

#[test]
fn preroll_seek_backs_up_two_content_pages() {
    let (bytes, data_offsets) = build_preroll_ogg(2, 3);
    let mut dmx = open(bytes);
    assert!(dmx.skeleton().is_some(), "Skeleton fisbone expected");

    // Seek to granule 1440 (data page 2). preroll = 2 single-packet pages,
    // so the resume offset must back up to data page 0.
    let landed = dmx.seek_to_with_preroll(0, 1440).expect("seek ok");
    assert_eq!(
        landed, 1440,
        "decode target granule is unchanged by preroll"
    );
    assert_eq!(
        dmx.input_position().expect("pos"),
        data_offsets[0],
        "resume offset must back up exactly 2 content pages"
    );
    assert_eq!(dmx.preroll_seek_count(), 1, "preroll back-up fired once");
}

#[test]
fn preroll_zero_behaves_like_seek_to() {
    let (bytes, data_offsets) = build_preroll_ogg(0, 3);
    let mut dmx = open(bytes);
    let landed = dmx.seek_to_with_preroll(0, 1440).expect("seek ok");
    assert_eq!(landed, 1440);
    assert_eq!(
        dmx.input_position().expect("pos"),
        data_offsets[2],
        "preroll 0 leaves the landed offset unchanged"
    );
    assert_eq!(dmx.preroll_seek_count(), 0, "no back-up when preroll is 0");
}

#[test]
fn preroll_clamps_to_first_content_page() {
    // Seek to data page 1 (granule 960) with preroll 2: only one earlier
    // content page exists (data page 0), so the resume offset clamps there
    // and does not run into the header section.
    let (bytes, data_offsets) = build_preroll_ogg(2, 3);
    let mut dmx = open(bytes);
    let landed = dmx.seek_to_with_preroll(0, 960).expect("seek ok");
    assert_eq!(landed, 960);
    assert_eq!(
        dmx.input_position().expect("pos"),
        data_offsets[0],
        "resume offset clamps to the first content page, never the headers"
    );
    assert_eq!(dmx.preroll_seek_count(), 1);
}

#[test]
fn preroll_on_first_content_page_is_noop() {
    // Seeking at or before the first content page has no earlier content
    // page to back up to: `seek_to_with_preroll` must leave the resume
    // offset exactly where bare `seek_to` left it and not tick the
    // counter. (`seek_to` itself floors target 480 to a granule-0 page of
    // the Vorbis serial, which is a pre-existing bare-seek behaviour; the
    // point here is that the preroll layer is a strict no-op on top of
    // whatever `seek_to` produced when no earlier content page exists.)
    let (bytes_a, _) = build_preroll_ogg(2, 3);
    let mut bare = open(bytes_a);
    let _ = Demuxer::seek_to(&mut bare, 0, 480).expect("seek ok");
    let bare_off = bare.input_position().expect("pos");

    let (bytes_b, _) = build_preroll_ogg(2, 3);
    let mut dmx = open(bytes_b);
    let _ = dmx.seek_to_with_preroll(0, 480).expect("seek ok");
    assert_eq!(
        dmx.input_position().expect("pos"),
        bare_off,
        "preroll layer must not move the resume offset before the first content page"
    );
    assert_eq!(
        dmx.preroll_seek_count(),
        0,
        "first content page has no earlier page to back up to"
    );
}

#[test]
fn preroll_seek_without_skeleton_behaves_like_seek_to() {
    // A plain Vorbis stream (no Skeleton, no fisbone) has no preroll
    // recorded, so seek_to_with_preroll is identical to seek_to.
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
    let data_packets: Vec<Vec<u8>> = vec![
        (0..40u8).collect(),
        (40..120u8).collect(),
        (0..200u8).collect(),
    ];
    let data_granules = [480i64, 960, 1440];
    let mut data_offsets = Vec::new();
    for (i, (pkt, gr)) in data_packets.iter().zip(data_granules.iter()).enumerate() {
        data_offsets.push(out.len() as u64);
        let flag = if i + 1 == data_packets.len() {
            flags::LAST_PAGE
        } else {
            0
        };
        out.extend_from_slice(&single_packet_page(
            pkt,
            flag,
            VORBIS_SERIAL,
            3 + i as u32,
            *gr,
        ));
    }

    let mut dmx = open(out);
    assert!(dmx.skeleton().is_none(), "no Skeleton in this fixture");
    let landed = dmx.seek_to_with_preroll(0, 1440).expect("seek ok");
    assert_eq!(landed, 1440);
    assert_eq!(dmx.input_position().expect("pos"), data_offsets[2]);
    assert_eq!(dmx.preroll_seek_count(), 0);
}

/// Build a page carrying two whole packets (two `< 255` lacing segments).
fn two_packet_page(
    a: &[u8],
    b: &[u8],
    flags_byte: u8,
    serial: u32,
    seq_no: u32,
    granule: i64,
) -> Vec<u8> {
    let mut lacing = lace(a.len());
    lacing.extend_from_slice(&lace(b.len()));
    let mut data = a.to_vec();
    data.extend_from_slice(b);
    let p = Page {
        flags: flags_byte,
        granule_position: granule,
        serial,
        seq_no,
        lacing,
        data,
    };
    p.to_bytes()
}

#[test]
fn preroll_counts_packets_not_pages_on_multi_packet_pages() {
    // Each content page carries TWO content packets. With preroll = 3 the
    // resume offset must back up far enough that at least 3 content packets
    // precede the landed page: landing on content page 2 (the 3rd page,
    // packets #5 and #6), 3 packets back covers content page 1 (packets #3,
    // #4) fully plus 1 packet of content page 0 — so the resume page is
    // content page 0.
    let v_id = vorbis_id_packet(2, 48_000);
    let v_comment = vorbis_comment_packet();
    let v_setup = vorbis_setup_packet();

    let mut head = FisHead::new(Version::V4_0);
    head.presentation_time = Rational::new(0, 1000);
    head.basetime = Rational::new(0, 1000);
    head.segment_length = Some(0);
    head.content_byte_offset = Some(0);
    let head_packet = head.to_bytes();

    let mut bone = FisBone::new(VORBIS_SERIAL, Rational::new(48_000, 1));
    bone.num_headers = 3;
    bone.preroll = 3;
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

    // Three content pages, each two packets. Granules at end of each page.
    let granules = [960i64, 1920, 2880];
    let mut content_offsets = Vec::new();
    for (i, gr) in granules.iter().enumerate() {
        content_offsets.push(out.len() as u64);
        let a: Vec<u8> = (0..30u8).collect();
        let b: Vec<u8> = (30..70u8).collect();
        let flag = if i + 1 == granules.len() {
            flags::LAST_PAGE
        } else {
            0
        };
        out.extend_from_slice(&two_packet_page(
            &a,
            &b,
            flag,
            VORBIS_SERIAL,
            3 + i as u32,
            *gr,
        ));
    }

    let mut dmx = open(out);
    // Seek to granule 2880 (content page 2). preroll = 3 packets → back up
    // to content page 0.
    let landed = dmx.seek_to_with_preroll(0, 2880).expect("seek ok");
    assert_eq!(landed, 2880);
    assert_eq!(
        dmx.input_position().expect("pos"),
        content_offsets[0],
        "3 preroll packets across 2-packet pages back up two pages to page 0"
    );
    assert_eq!(dmx.preroll_seek_count(), 1);
}
