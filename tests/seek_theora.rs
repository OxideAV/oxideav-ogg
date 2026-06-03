//! Codec-aware `seek_to` for Theora streams paired with a Skeleton
//! `fisbone\0` (round 227).
//!
//! Theora encodes its granule position as `(keyframe_idx << shift) |
//! offset_from_keyframe`, so the raw granule value cannot be compared
//! directly against a microsecond `pts`. The demuxer's seek path needs
//! the per-stream `granuleshift` and `granule_rate` (carried by the
//! Skeleton 4.0 `fisbone\0` per `docs/container/ogg/ogg-skeleton-4.0.md`)
//! to translate `pts` into a frame number and to map each indexed
//! page's granule back to its frame number for comparison. Round 227
//! adds that translation; this file synthesises an Ogg stream with the
//! ingredients listed above and verifies seeks land on the right
//! pages.
//!
//! No real Theora packets are decoded — the codec sniffer keys off the
//! first 7 bytes of the id packet (`0x80 "theora"`) and the
//! header_packet_count tells the demuxer to skip 3 setup packets before
//! delivering data packets, so the test packets just need the right
//! magic bytes.
//!
//! Spec: `docs/container/ogg/rfc3533-ogg.txt`,
//! `docs/container/ogg/ogg-skeleton-4.0.md`.

use std::io::Cursor;

use oxideav_core::{Demuxer, NullCodecResolver, ReadSeek};

use oxideav_ogg::page::{flags, lace, Page};
use oxideav_ogg::skeleton::{FisBone, FisHead, Rational, Version};

const SKEL_SERIAL: u32 = 0x5BE1E70F;
const THEORA_SERIAL: u32 = 0x71EB1A11;

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

/// Theora id packet (`0x80 "theora"` + 35 placeholder bytes). Only the
/// first 7 bytes are inspected by the demuxer's codec sniffer; the
/// remaining bytes are required only to make the packet a plausible
/// length so the header-packet-count walk doesn't trip on a degenerate
/// id.
fn theora_id_packet() -> Vec<u8> {
    let mut p = Vec::with_capacity(42);
    p.push(0x80);
    p.extend_from_slice(b"theora");
    p.extend_from_slice(&[0u8; 35]);
    p
}

/// Theora comment packet (`0x81 "theora"` + empty vendor + empty user
/// comment list, matching the Vorbis comment block layout reused by
/// Theora per RFC 5215 §4.2.2).
fn theora_comment_packet() -> Vec<u8> {
    let mut p = Vec::new();
    p.push(0x81);
    p.extend_from_slice(b"theora");
    p.extend_from_slice(&0u32.to_le_bytes()); // vendor length
    p.extend_from_slice(&0u32.to_le_bytes()); // user comment count
    p
}

/// Theora setup packet (`0x82 "theora"` + a few bytes). Not decoded.
fn theora_setup_packet() -> Vec<u8> {
    let mut p = Vec::new();
    p.push(0x82);
    p.extend_from_slice(b"theora");
    p.extend_from_slice(&[0u8; 24]);
    p
}

/// Build a synthetic Ogg file containing:
///   * Skeleton 4.0 `fishead\0` BOS;
///   * Theora `0x80 "theora"` BOS;
///   * Theora comment header;
///   * Skeleton `fisbone\0` for the Theora serial, carrying
///     granuleshift = 6 and granule_rate = 30/1 (30 fps);
///   * Theora setup header;
///   * Skeleton empty-packet EOS;
///   * Four Theora data pages with granules describing frames
///     {30, 60, 96, 128} under the keyframe-shift packing.
///
/// Returns the file bytes plus the `(granule, frame_no, micros)` table
/// the seek tests rely on.
#[allow(clippy::type_complexity)]
fn build_skeleton_theora_ogg() -> (Vec<u8>, Vec<(i64, i64, i64)>) {
    // Granules under shift=6:
    //   frame 30   -> (0<<6)|30   = 30
    //   frame 60   -> (0<<6)|60   = 60
    //   frame 96   -> (64<<6)|32  = 4128
    //   frame 128  -> (128<<6)|0  = 8192
    // 30 fps -> 1 frame = 1_000_000 / 30 us = 33_333.33 us.
    let granules: Vec<i64> = vec![30, 60, 4128, 8192];
    let frame_nos: Vec<i64> = vec![30, 60, 96, 128];
    let micros: Vec<i64> = frame_nos.iter().map(|f| f * 1_000_000 / 30).collect();
    let table: Vec<(i64, i64, i64)> = granules
        .iter()
        .zip(frame_nos.iter())
        .zip(micros.iter())
        .map(|((g, f), us)| (*g, *f, *us))
        .collect();

    // ---- Skeleton 4.0 fishead. segment_length=0 opts out of the
    //      Skeleton-index length check; this file carries no `index\0`
    //      packet anyway (the round-227 seek path is the bisection, not
    //      the index fast-path).
    let mut head = FisHead::new(Version::V4_0);
    head.presentation_time = Rational::new(0, 1);
    head.basetime = Rational::new(0, 1);
    head.segment_length = Some(0);
    head.content_byte_offset = Some(0);
    let head_packet = head.to_bytes();

    // ---- Skeleton fisbone for the Theora content stream. granuleshift
    //      = 6 sets the boundary between "keyframe index" and "frames
    //      since keyframe" halves of the granule packing. granule_rate
    //      = 30/1 declares 30 fps.
    let mut bone = FisBone::new(THEORA_SERIAL, Rational::new(30, 1));
    bone.num_headers = 3;
    bone.granuleshift = 6;
    bone.set_header("Content-Type", "video/theora");
    bone.set_header("Role", "video/main");
    bone.set_header("Name", "main_video");
    let bone_packet = bone.to_bytes();

    let t_id = theora_id_packet();
    let t_comment = theora_comment_packet();
    let t_setup = theora_setup_packet();

    let mut out = Vec::new();
    // RFC 3533 §6: every BOS page must precede any non-BOS page. The
    // Skeleton spec further mandates that the Skeleton BOS comes first
    // (so a player can identify Skeleton without dispatching to a
    // content-codec parser), then content BOSes in their preferred
    // order. After the BOS section, all remaining header packets are
    // emitted before the first content page.
    out.extend_from_slice(&single_packet_page(
        &head_packet,
        flags::FIRST_PAGE,
        SKEL_SERIAL,
        0,
        0,
    ));
    out.extend_from_slice(&single_packet_page(
        &t_id,
        flags::FIRST_PAGE,
        THEORA_SERIAL,
        0,
        0,
    ));
    out.extend_from_slice(&single_packet_page(&t_comment, 0, THEORA_SERIAL, 1, 0));
    out.extend_from_slice(&single_packet_page(&bone_packet, 0, SKEL_SERIAL, 1, 0));
    out.extend_from_slice(&single_packet_page(&t_setup, 0, THEORA_SERIAL, 2, 0));
    // Skeleton EOS empty packet.
    out.extend_from_slice(&single_packet_page(
        &[],
        flags::LAST_PAGE,
        SKEL_SERIAL,
        2,
        0,
    ));

    // ---- Theora data pages, each carrying a single placeholder
    //      packet so the granule_position landed on the page is exactly
    //      the table's value.
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

    (out, table)
}

#[test]
fn theora_bisection_seek_with_skeleton_fisbone_lands_on_floor_frame() {
    let (bytes, table) = build_skeleton_theora_ogg();
    let reader: Box<dyn ReadSeek> = Box::new(Cursor::new(bytes));
    let codecs = NullCodecResolver;
    let mut dmx = oxideav_ogg::demux::open_concrete(reader, &codecs).expect("open ok");
    // The Theora stream is the only one in `streams()` — Skeleton itself
    // is hidden from the public list because it carries no content
    // packets.
    assert_eq!(dmx.streams().len(), 1);
    assert_eq!(
        dmx.streams()[0].params.codec_id.as_str(),
        "theora",
        "codec sniffer must identify 0x80 'theora' as Theora",
    );
    assert!(dmx.skeleton().is_some(), "Skeleton fisbone expected");

    // Seek to a microsecond pts that sits between frame 60 (2_000_000 us)
    // and frame 96 (3_200_000 us). The bisection should land on the
    // largest indexed page whose frame number is at or below the target,
    // which is the granule=60 page (frame_no=60).
    let target_pts = 2_500_000i64;
    let landed = oxideav_core::Demuxer::seek_to(&mut dmx, 0, target_pts).expect("seek ok");
    assert_eq!(
        landed, table[1].0,
        "seek to {target_pts}us must land on granule {} (frame {})",
        table[1].0, table[1].1
    );

    // Seek to a pts between frame 96 (3_200_000 us) and frame 128
    // (4_266_666 us). Expected landing: granule 4128 (frame 96).
    let target_pts = 3_500_000i64;
    let landed = oxideav_core::Demuxer::seek_to(&mut dmx, 0, target_pts).expect("seek ok");
    assert_eq!(landed, table[2].0);

    // Seek to a pts past the last frame. We expect the seek to land on
    // the largest page (frame 128).
    let target_pts = 10_000_000i64;
    let landed = oxideav_core::Demuxer::seek_to(&mut dmx, 0, target_pts).expect("seek ok");
    assert_eq!(landed, table[3].0);

    // Seek to pts 0: the BOS / header pages carry granule_position == 0
    // (a Theora frame number of zero under the keyframe-shift packing
    // is also zero), so a target_frame of 0 satisfies `key_of(g) <=
    // target_key` for the very first page belonging to the Theora
    // serial. The exact landing is therefore granule 0 (some header
    // page), not the first data page — the seek_to contract is
    // "largest indexed page whose key is `<= target_key`" and a
    // granule-0 header page is the earliest such page on the wire.
    // The test verifies the seek did not error and landed on a granule
    // at or before any data page.
    let target_pts = 0i64;
    let landed = oxideav_core::Demuxer::seek_to(&mut dmx, 0, target_pts).expect("seek ok");
    assert!(
        landed <= table[0].0,
        "seek to pts 0 must land at or before the earliest data page (granule {})",
        table[0].0
    );
}

#[test]
fn theora_bisection_seek_after_build_seek_index_uses_index_floor() {
    // Pre-building the seek index forces the seek path through
    // `index_floor_by` instead of the bisection's `find_next_page_for_serial`
    // walk. Both paths must give the same answer for a Theora stream
    // with a Skeleton fisbone.
    let (bytes, table) = build_skeleton_theora_ogg();
    let reader: Box<dyn ReadSeek> = Box::new(Cursor::new(bytes));
    let codecs = NullCodecResolver;
    let mut dmx = oxideav_ogg::demux::open_concrete(reader, &codecs).expect("open ok");
    dmx.build_seek_index().expect("build index ok");
    assert!(dmx.seek_index_len() >= 4, "every data page indexed");

    let target_pts = 2_500_000i64;
    let landed = oxideav_core::Demuxer::seek_to(&mut dmx, 0, target_pts).expect("seek ok");
    assert_eq!(landed, table[1].0);
}

#[test]
fn theora_seek_without_skeleton_fisbone_still_rejects() {
    // Without a Skeleton fisbone the demuxer has no `granuleshift` /
    // `granule_rate` to translate `pts` into a Theora frame number, so
    // the codec-aware seek path must refuse rather than silently
    // misinterpret the raw granule as the target.
    //
    // We build the same byte sequence as `build_skeleton_theora_ogg`
    // but drop the Skeleton bitstream — the codec sniffer still tags
    // the first BOS as Theora, but `Skeleton::bone_for_serial` returns
    // `None`.
    let t_id = theora_id_packet();
    let t_comment = theora_comment_packet();
    let t_setup = theora_setup_packet();

    let mut out = Vec::new();
    out.extend_from_slice(&single_packet_page(
        &t_id,
        flags::FIRST_PAGE,
        THEORA_SERIAL,
        0,
        0,
    ));
    out.extend_from_slice(&single_packet_page(&t_comment, 0, THEORA_SERIAL, 1, 0));
    out.extend_from_slice(&single_packet_page(&t_setup, 0, THEORA_SERIAL, 2, 0));
    out.extend_from_slice(&single_packet_page(
        &[0xAB; 12],
        flags::LAST_PAGE,
        THEORA_SERIAL,
        3,
        30,
    ));

    let reader: Box<dyn ReadSeek> = Box::new(Cursor::new(out));
    let codecs = NullCodecResolver;
    let mut dmx = oxideav_ogg::demux::open_concrete(reader, &codecs).expect("open ok");
    assert!(dmx.skeleton().is_none(), "no Skeleton in this file");
    assert_eq!(dmx.streams()[0].params.codec_id.as_str(), "theora");

    let err = oxideav_core::Demuxer::seek_to(&mut dmx, 0, 1_000_000)
        .expect_err("must refuse without fisbone");
    let msg = format!("{err}");
    assert!(
        msg.contains("Skeleton fisbone"),
        "error must mention the missing fisbone, got: {msg}"
    );
}

#[test]
fn theora_seek_rejects_zero_granuleshift_fisbone() {
    // A Skeleton fisbone with granuleshift == 0 collapses the Theora
    // granule packing — the demuxer can't tell whether the encoder
    // genuinely set the shift to 0 or forgot to set it, so the
    // conservative choice is to refuse rather than silently produce a
    // seek that may land on the wrong frame.
    let mut head = FisHead::new(Version::V4_0);
    head.segment_length = Some(0);
    head.content_byte_offset = Some(0);
    let head_packet = head.to_bytes();

    let mut bone = FisBone::new(THEORA_SERIAL, Rational::new(30, 1));
    bone.num_headers = 3;
    bone.granuleshift = 0;
    bone.set_header("Content-Type", "video/theora");
    let bone_packet = bone.to_bytes();

    let t_id = theora_id_packet();
    let t_comment = theora_comment_packet();
    let t_setup = theora_setup_packet();

    let mut out = Vec::new();
    out.extend_from_slice(&single_packet_page(
        &head_packet,
        flags::FIRST_PAGE,
        SKEL_SERIAL,
        0,
        0,
    ));
    out.extend_from_slice(&single_packet_page(
        &t_id,
        flags::FIRST_PAGE,
        THEORA_SERIAL,
        0,
        0,
    ));
    out.extend_from_slice(&single_packet_page(&t_comment, 0, THEORA_SERIAL, 1, 0));
    out.extend_from_slice(&single_packet_page(&bone_packet, 0, SKEL_SERIAL, 1, 0));
    out.extend_from_slice(&single_packet_page(&t_setup, 0, THEORA_SERIAL, 2, 0));
    out.extend_from_slice(&single_packet_page(
        &[],
        flags::LAST_PAGE,
        SKEL_SERIAL,
        2,
        0,
    ));
    out.extend_from_slice(&single_packet_page(
        &[0xAB; 12],
        flags::LAST_PAGE,
        THEORA_SERIAL,
        3,
        30,
    ));

    let reader: Box<dyn ReadSeek> = Box::new(Cursor::new(out));
    let codecs = NullCodecResolver;
    let mut dmx = oxideav_ogg::demux::open_concrete(reader, &codecs).expect("open ok");
    let err = oxideav_core::Demuxer::seek_to(&mut dmx, 0, 1_000_000)
        .expect_err("granuleshift==0 must refuse");
    let msg = format!("{err}");
    assert!(
        msg.contains("Skeleton fisbone"),
        "error must mention the missing/zero-shift fisbone, got: {msg}"
    );
}
