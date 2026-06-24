//! Per-packet `PacketFlags::keyframe` semantics on demux delivery.
//!
//! RFC 3533 framing carries no explicit per-packet keyframe bit. The only
//! random-access signal is the granuleshift-packed `granulepos`
//! (`docs/container/ogg/ogg-skeleton-4.0.md`: the granuleshift is "the number
//! of lower bits from the granulepos field that are used to provide position
//! information for sub-seekable units (like the keyframe shift in theora)").
//!
//! * Audio mappings (Vorbis / Opus / FLAC / Speex) declare granuleshift 0 —
//!   every packet is an independent random-access point, so every delivered
//!   content packet is a keyframe.
//! * Theora declares a non-zero keyframe shift — the last frame finishing on
//!   a page is a keyframe exactly when its low `shift` bits (the offset since
//!   the last keyframe) are zero. A non-granule-bearing packet on such a
//!   track cannot be proven a keyframe and is flagged `false`.
//!
//! Spec: `docs/container/ogg/rfc3533-ogg.txt`,
//! `docs/container/ogg/ogg-skeleton-4.0.md`.

use std::io::Cursor;

use oxideav_core::{Demuxer, Error, NullCodecResolver, ReadSeek};

use oxideav_ogg::page::{flags, lace, Page};
use oxideav_ogg::skeleton::{FisBone, FisHead, Rational, Version};

const SKEL_SERIAL: u32 = 0x5BE1E70F;
const THEORA_SERIAL: u32 = 0x71EB1A11;
const VORBIS_SERIAL: u32 = 0x00C0FFEE;

fn single_packet_page(
    packet: &[u8],
    flags_byte: u8,
    serial: u32,
    seq_no: u32,
    granule: i64,
) -> Vec<u8> {
    Page {
        flags: flags_byte,
        granule_position: granule,
        serial,
        seq_no,
        lacing: lace(packet.len()),
        data: packet.to_vec(),
    }
    .to_bytes()
}

/// A page carrying two whole packets (each terminated) so the intermediate
/// (non-last) packet path is exercised.
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
    Page {
        flags: flags_byte,
        granule_position: granule,
        serial,
        seq_no,
        lacing,
        data,
    }
    .to_bytes()
}

fn theora_id_packet() -> Vec<u8> {
    let mut p = Vec::with_capacity(42);
    p.push(0x80);
    p.extend_from_slice(b"theora");
    p.extend_from_slice(&[0u8; 35]);
    p
}

fn theora_comment_packet() -> Vec<u8> {
    let mut p = Vec::new();
    p.push(0x81);
    p.extend_from_slice(b"theora");
    p.extend_from_slice(&0u32.to_le_bytes());
    p.extend_from_slice(&0u32.to_le_bytes());
    p
}

fn theora_setup_packet() -> Vec<u8> {
    let mut p = Vec::new();
    p.push(0x82);
    p.extend_from_slice(b"theora");
    p.extend_from_slice(&[0u8; 24]);
    p
}

fn vorbis_id_packet() -> Vec<u8> {
    // 0x01 "vorbis" + version(4) + channels(1) + rate(4) + bitrates(12) +
    // blocksizes(1) + framing(1). 30 bytes total — enough for parse_vorbis_id.
    let mut p = vec![0x01];
    p.extend_from_slice(b"vorbis");
    p.extend_from_slice(&0u32.to_le_bytes()); // version
    p.push(2); // channels
    p.extend_from_slice(&48_000u32.to_le_bytes()); // sample rate
    p.extend_from_slice(&0u32.to_le_bytes()); // bitrate max
    p.extend_from_slice(&128_000u32.to_le_bytes()); // bitrate nominal
    p.extend_from_slice(&0u32.to_le_bytes()); // bitrate min
    p.push(0xB8); // blocksizes
    p.push(0x01); // framing
    p
}

fn vorbis_comment_packet() -> Vec<u8> {
    let mut p = vec![0x03];
    p.extend_from_slice(b"vorbis");
    p.extend_from_slice(&0u32.to_le_bytes()); // vendor length
    p.extend_from_slice(&0u32.to_le_bytes()); // user comment count
    p.push(0x01); // framing
    p
}

fn vorbis_setup_packet() -> Vec<u8> {
    let mut p = vec![0x05];
    p.extend_from_slice(b"vorbis");
    p.extend_from_slice(&[0u8; 8]);
    p
}

/// Theora file with a Skeleton fisbone (granuleshift = 6, 30 fps) and four
/// data pages whose granules describe frames {30, 60, 96, 128}. Only the
/// last (granule 8192 = (128<<6)|0) is a keyframe under the shift packing.
fn build_theora_with_fisbone() -> Vec<u8> {
    let granules: Vec<i64> = vec![30, 60, 4128, 8192];

    let mut head = FisHead::new(Version::V4_0);
    head.presentation_time = Rational::new(0, 1);
    head.basetime = Rational::new(0, 1);
    head.segment_length = Some(0);
    head.content_byte_offset = Some(0);
    let head_packet = head.to_bytes();

    let mut bone = FisBone::new(THEORA_SERIAL, Rational::new(30, 1));
    bone.num_headers = 3;
    bone.granuleshift = 6;
    bone.set_header("Content-Type", "video/theora");
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
        &theora_id_packet(),
        flags::FIRST_PAGE,
        THEORA_SERIAL,
        0,
        0,
    ));
    out.extend_from_slice(&single_packet_page(
        &theora_comment_packet(),
        0,
        THEORA_SERIAL,
        1,
        0,
    ));
    out.extend_from_slice(&single_packet_page(&bone_packet, 0, SKEL_SERIAL, 1, 0));
    out.extend_from_slice(&single_packet_page(
        &theora_setup_packet(),
        0,
        THEORA_SERIAL,
        2,
        0,
    ));
    out.extend_from_slice(&single_packet_page(
        &[],
        flags::LAST_PAGE,
        SKEL_SERIAL,
        2,
        0,
    ));

    for (i, gr) in granules.iter().enumerate() {
        let pkt: Vec<u8> = vec![0xAB; 12 + i];
        let is_last = i + 1 == granules.len();
        let flag = if is_last { flags::LAST_PAGE } else { 0 };
        out.extend_from_slice(&single_packet_page(
            &pkt,
            flag,
            THEORA_SERIAL,
            3 + i as u32,
            *gr,
        ));
    }
    out
}

#[test]
fn theora_keyframe_flag_follows_granuleshift_offset() {
    let bytes = build_theora_with_fisbone();
    let reader: Box<dyn ReadSeek> = Box::new(Cursor::new(bytes));
    let codecs = NullCodecResolver;
    let mut dmx = oxideav_ogg::demux::open_concrete(reader, &codecs).expect("open ok");
    assert_eq!(dmx.streams()[0].params.codec_id.as_str(), "theora");
    assert!(dmx.skeleton().is_some(), "Skeleton fisbone expected");

    // Expected keyframe flags per data page, by granule:
    //   30   = (0<<6)|30  -> offset 30 != 0 -> inter
    //   60   = (0<<6)|60  -> offset 60 != 0 -> inter
    //   4128 = (64<<6)|32 -> offset 32 != 0 -> inter
    //   8192 = (128<<6)|0 -> offset 0       -> KEYFRAME
    let expected_keyframe = [false, false, false, true];
    let mut got = Vec::new();
    loop {
        match Demuxer::next_packet(&mut dmx) {
            Ok(pkt) => got.push(pkt.flags.keyframe),
            Err(Error::Eof) => break,
            Err(e) => panic!("unexpected demux error: {e}"),
        }
    }
    assert_eq!(
        got, expected_keyframe,
        "Theora keyframe flags must track the granule offset-since-keyframe"
    );

    // The per-stream granuleshift accessor surfaces the fisbone's value the
    // keyframe decision is derived from.
    assert_eq!(
        dmx.stream_granuleshift(0),
        Some(6),
        "Theora stream reports its fisbone granuleshift"
    );
    assert_eq!(
        dmx.stream_granuleshift(99),
        None,
        "out-of-range stream index reports None"
    );
}

/// Theora data pages with NO Skeleton fisbone — the demuxer cannot know the
/// granuleshift, so it has no basis to single out keyframes. The
/// conservative choice (granuleshift defaults to 0) flags every page's
/// granule-bearing packet as a random-access point.
fn build_theora_no_fisbone() -> Vec<u8> {
    let mut out = Vec::new();
    out.extend_from_slice(&single_packet_page(
        &theora_id_packet(),
        flags::FIRST_PAGE,
        THEORA_SERIAL,
        0,
        0,
    ));
    out.extend_from_slice(&single_packet_page(
        &theora_comment_packet(),
        0,
        THEORA_SERIAL,
        1,
        0,
    ));
    out.extend_from_slice(&single_packet_page(
        &theora_setup_packet(),
        0,
        THEORA_SERIAL,
        2,
        0,
    ));
    for (i, gr) in [30i64, 8192].iter().enumerate() {
        let is_last = i == 1;
        let flag = if is_last { flags::LAST_PAGE } else { 0 };
        out.extend_from_slice(&single_packet_page(
            &[0xCD; 16],
            flag,
            THEORA_SERIAL,
            3 + i as u32,
            *gr,
        ));
    }
    out
}

#[test]
fn theora_without_fisbone_flags_every_packet_keyframe() {
    let bytes = build_theora_no_fisbone();
    let reader: Box<dyn ReadSeek> = Box::new(Cursor::new(bytes));
    let codecs = NullCodecResolver;
    let mut dmx = oxideav_ogg::demux::open_concrete(reader, &codecs).expect("open ok");
    assert!(dmx.skeleton().is_none(), "no Skeleton in this file");
    let mut got = Vec::new();
    loop {
        match Demuxer::next_packet(&mut dmx) {
            Ok(pkt) => got.push(pkt.flags.keyframe),
            Err(Error::Eof) => break,
            Err(e) => panic!("unexpected demux error: {e}"),
        }
    }
    assert_eq!(
        got,
        vec![true, true],
        "without a fisbone the granuleshift is unknown; flag conservatively"
    );
}

/// Vorbis audio: granuleshift is 0, so every content packet is a keyframe,
/// including the intermediate (non-last-on-page) packet that carries no
/// granule.
fn build_vorbis_two_packet_page() -> Vec<u8> {
    let mut out = Vec::new();
    out.extend_from_slice(&single_packet_page(
        &vorbis_id_packet(),
        flags::FIRST_PAGE,
        VORBIS_SERIAL,
        0,
        0,
    ));
    out.extend_from_slice(&single_packet_page(
        &vorbis_comment_packet(),
        0,
        VORBIS_SERIAL,
        1,
        0,
    ));
    out.extend_from_slice(&single_packet_page(
        &vorbis_setup_packet(),
        0,
        VORBIS_SERIAL,
        2,
        0,
    ));
    // One data page with TWO whole packets. The granule applies to the last;
    // the first packet carries no granule. Both must be keyframes.
    out.extend_from_slice(&two_packet_page(
        &[0x11; 20],
        &[0x22; 24],
        flags::LAST_PAGE,
        VORBIS_SERIAL,
        3,
        1024,
    ));
    out
}

#[test]
fn vorbis_every_packet_is_keyframe_including_intermediate() {
    let bytes = build_vorbis_two_packet_page();
    let reader: Box<dyn ReadSeek> = Box::new(Cursor::new(bytes));
    let codecs = NullCodecResolver;
    let mut dmx = oxideav_ogg::demux::open_concrete(reader, &codecs).expect("open ok");
    assert_eq!(dmx.streams()[0].params.codec_id.as_str(), "vorbis");
    // An audio mapping with no fisbone reports granuleshift 0 (every packet a
    // random-access point).
    assert_eq!(dmx.stream_granuleshift(0), Some(0));

    let mut flags_seen = Vec::new();
    let mut pts_seen = Vec::new();
    loop {
        match Demuxer::next_packet(&mut dmx) {
            Ok(pkt) => {
                flags_seen.push(pkt.flags.keyframe);
                pts_seen.push(pkt.pts);
            }
            Err(Error::Eof) => break,
            Err(e) => panic!("unexpected demux error: {e}"),
        }
    }
    assert_eq!(
        flags_seen,
        vec![true, true],
        "audio: both packets on the page (incl. the non-granule first) are keyframes"
    );
    // Only the last-on-page packet carries the page granule as pts.
    assert_eq!(
        pts_seen,
        vec![None, Some(1024)],
        "granule is attributed to the last packet finishing on the page"
    );
}
