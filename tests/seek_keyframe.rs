//! Keyframe-aware seek for Theora (`OggDemuxer::seek_to_keyframe`).
//!
//! A bare `seek_to` lands on the page whose *frame number* floors the target,
//! which for a Theora stream may be an inter-frame the decoder cannot start
//! from. `seek_to_keyframe` reads the landed page's keyframe index out of the
//! granuleshift packing (`docs/container/ogg/ogg-skeleton-4.0.md`: the low
//! `shift` bits are the offset-since-keyframe, the high bits the keyframe
//! index) and re-seeks to that keyframe's own page so forward decoding starts
//! on an intra frame.
//!
//! Spec: `docs/container/ogg/rfc3533-ogg.txt`,
//! `docs/container/ogg/ogg-skeleton-4.0.md`.

use std::io::Cursor;

use oxideav_core::{NullCodecResolver, ReadSeek};
use oxideav_ogg::page::{flags, lace, Page};
use oxideav_ogg::skeleton::{FisBone, FisHead, Rational, Version};

const SKEL_SERIAL: u32 = 0x5BE1E70F;
const THEORA_SERIAL: u32 = 0x71EB1A11;
const SHIFT: u8 = 6;

fn single_packet_page(
    packet: &[u8],
    flags_byte: u8,
    serial: u32,
    seq: u32,
    granule: i64,
) -> Vec<u8> {
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

fn theora_id_packet() -> Vec<u8> {
    let mut p = vec![0x80];
    p.extend_from_slice(b"theora");
    p.extend_from_slice(&[0u8; 35]);
    p
}

fn theora_comment_packet() -> Vec<u8> {
    let mut p = vec![0x81];
    p.extend_from_slice(b"theora");
    p.extend_from_slice(&0u32.to_le_bytes());
    p.extend_from_slice(&0u32.to_le_bytes());
    p
}

fn theora_setup_packet() -> Vec<u8> {
    let mut p = vec![0x82];
    p.extend_from_slice(b"theora");
    p.extend_from_slice(&[0u8; 24]);
    p
}

/// Data pages, granule = (keyframe_index << 6) | offset, 30 fps:
///   frame 64  -> (64<<6)|0   = 4096   KEYFRAME
///   frame 70  -> (64<<6)|6   = 4102   inter (keyframe 64)
///   frame 128 -> (128<<6)|0  = 8192   KEYFRAME
///   frame 130 -> (128<<6)|2  = 8194   inter (keyframe 128)
const GRANULES: [i64; 4] = [4096, 4102, 8192, 8194];

fn build() -> Vec<u8> {
    let mut head = FisHead::new(Version::V4_0);
    head.presentation_time = Rational::new(0, 1);
    head.basetime = Rational::new(0, 1);
    head.segment_length = Some(0);
    head.content_byte_offset = Some(0);

    let mut bone = FisBone::new(THEORA_SERIAL, Rational::new(30, 1));
    bone.num_headers = 3;
    bone.granuleshift = SHIFT;
    bone.set_header("Content-Type", "video/theora");

    let mut out = Vec::new();
    out.extend(single_packet_page(
        &head.to_bytes(),
        flags::FIRST_PAGE,
        SKEL_SERIAL,
        0,
        0,
    ));
    out.extend(single_packet_page(
        &theora_id_packet(),
        flags::FIRST_PAGE,
        THEORA_SERIAL,
        0,
        0,
    ));
    out.extend(single_packet_page(
        &theora_comment_packet(),
        0,
        THEORA_SERIAL,
        1,
        0,
    ));
    out.extend(single_packet_page(&bone.to_bytes(), 0, SKEL_SERIAL, 1, 0));
    out.extend(single_packet_page(
        &theora_setup_packet(),
        0,
        THEORA_SERIAL,
        2,
        0,
    ));
    out.extend(single_packet_page(&[], flags::LAST_PAGE, SKEL_SERIAL, 2, 0));

    for (i, gr) in GRANULES.iter().enumerate() {
        let last = i + 1 == GRANULES.len();
        let flag = if last { flags::LAST_PAGE } else { 0 };
        out.extend(single_packet_page(
            &[0xAB; 16],
            flag,
            THEORA_SERIAL,
            3 + i as u32,
            *gr,
        ));
    }
    out
}

/// Microsecond pts for a given frame at 30 fps (stream time-base 1/1_000_000).
fn frame_pts_us(frame: i64) -> i64 {
    frame * 1_000_000 / 30
}

#[test]
fn seek_to_keyframe_lands_on_the_keyframe_page_not_the_inter_floor() {
    let bytes = build();
    let reader: Box<dyn ReadSeek> = Box::new(Cursor::new(bytes));
    let mut dmx = oxideav_ogg::demux::open_concrete(reader, &NullCodecResolver).expect("open");
    assert_eq!(dmx.streams()[0].params.codec_id.as_str(), "theora");

    use oxideav_core::Demuxer as _;
    // Target frame 70 (an inter-frame, keyframe index 64). A bare seek_to
    // lands on its own page (granule 4102, frame 70).
    let target = frame_pts_us(70);
    let floor = dmx.seek_to(0, target).expect("seek_to");
    assert_eq!(floor, 4102, "bare seek_to lands on the frame-70 inter page");

    // seek_to_keyframe instead lands on the keyframe page (frame 64,
    // granule 4096) so a decoder can start cleanly.
    let kf = dmx.seek_to_keyframe(0, target).expect("seek_to_keyframe");
    assert_eq!(kf, 4096, "seek_to_keyframe lands on the keyframe-64 page");
    assert_eq!(
        kf & ((1 << SHIFT) - 1),
        0,
        "landed granule's offset half is zero"
    );

    // After the keyframe seek, the next delivered packet is the keyframe page.
    let first = dmx.next_packet().expect("packet after keyframe seek");
    assert_eq!(
        first.pts,
        Some(4096),
        "decode resumes at the keyframe granule"
    );
    assert!(first.flags.keyframe, "and it is flagged a keyframe");
}

#[test]
fn seek_to_keyframe_is_identity_when_landing_is_already_a_keyframe() {
    let bytes = build();
    let reader: Box<dyn ReadSeek> = Box::new(Cursor::new(bytes));
    let mut dmx = oxideav_ogg::demux::open_concrete(reader, &NullCodecResolver).expect("open");

    use oxideav_core::Demuxer as _;
    // Target frame 128 exactly — it is itself a keyframe (granule 8192).
    let target = frame_pts_us(128);
    let floor = dmx.seek_to(0, target).expect("seek_to");
    assert_eq!(floor, 8192);
    let kf = dmx.seek_to_keyframe(0, target).expect("seek_to_keyframe");
    assert_eq!(
        kf, 8192,
        "already on a keyframe — no back-up, identical to seek_to"
    );
}

#[test]
fn seek_to_keyframe_on_audio_is_identical_to_seek_to() {
    // A Vorbis stream (granuleshift 0): every packet is a random-access point,
    // so seek_to_keyframe must behave exactly like seek_to.
    let mut out = Vec::new();
    let serial = 0x00C0FFEEu32;
    let mut id = vec![0x01u8];
    id.extend_from_slice(b"vorbis");
    id.extend_from_slice(&0u32.to_le_bytes());
    id.push(2);
    id.extend_from_slice(&48_000u32.to_le_bytes());
    id.extend_from_slice(&0u32.to_le_bytes());
    id.extend_from_slice(&128_000u32.to_le_bytes());
    id.extend_from_slice(&0u32.to_le_bytes());
    id.push(0xB8);
    id.push(0x01);
    let mut com = vec![0x03u8];
    com.extend_from_slice(b"vorbis");
    com.extend_from_slice(&0u32.to_le_bytes());
    com.extend_from_slice(&0u32.to_le_bytes());
    com.push(0x01);
    let mut setup = vec![0x05u8];
    setup.extend_from_slice(b"vorbis");
    setup.extend_from_slice(&[0u8; 8]);

    out.extend(single_packet_page(&id, flags::FIRST_PAGE, serial, 0, 0));
    out.extend(single_packet_page(&com, 0, serial, 1, 0));
    out.extend(single_packet_page(&setup, 0, serial, 2, 0));
    for i in 1..=4i64 {
        let last = i == 4;
        let flag = if last { flags::LAST_PAGE } else { 0 };
        out.extend(single_packet_page(
            &[0xAA; 8],
            flag,
            serial,
            2 + i as u32,
            960 * i,
        ));
    }

    let reader: Box<dyn ReadSeek> = Box::new(Cursor::new(out));
    let mut dmx = oxideav_ogg::demux::open_concrete(reader, &NullCodecResolver).expect("open");
    assert_eq!(dmx.streams()[0].params.codec_id.as_str(), "vorbis");

    use oxideav_core::Demuxer as _;
    let target = 2_500i64; // samples
    let a = dmx.seek_to(0, target).expect("seek_to");
    let b = dmx.seek_to_keyframe(0, target).expect("seek_to_keyframe");
    assert_eq!(a, b, "audio: keyframe seek == plain seek");
}
