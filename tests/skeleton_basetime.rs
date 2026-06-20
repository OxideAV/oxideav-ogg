//! Skeleton 4.0 fishead basetime → stream `start_time` anchoring.
//!
//! `docs/container/ogg/ogg-skeleton-4.0.md` §"What decoding-related
//! information is needed?" defines the fishead **basetime** as "a mapping
//! for granule position 0 (for all logical bitstreams) to a playback
//! time" — the analog-video "starts at 01:00:00" example. The §"How to
//! allow the creation of substreams …" section adds the per-track
//! **basegranule**, "the granule number with which this logical bitstream
//! starts in the remuxed stream". The demuxer folds both onto each
//! stream's reported `start_time` so a player can place the content on the
//! intended timeline; the duration accumulator stays basetime-free so
//! `duration == end - start` still holds.

use std::io::Cursor;

use oxideav_core::{Demuxer, NullCodecResolver, ReadSeek};

use oxideav_ogg::page::{flags, lace, Page};
use oxideav_ogg::skeleton::{FisBone, FisHead, Rational, Version};

const SKEL_SERIAL: u32 = 0xCAFEBABE;
const VORBIS_SERIAL: u32 = 0x12345678;
const SAMPLE_RATE: u32 = 48_000;

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
    p.extend_from_slice(&0u32.to_le_bytes());
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
    p.extend_from_slice(&[0u8; 32]);
    p
}

/// Build a Skeleton-4.0 + Vorbis Ogg file with caller-supplied fishead
/// `basetime` and fisbone `basegranule`.
fn build_anchored_ogg(basetime: Rational, basegranule: i64, last_granule: i64) -> Vec<u8> {
    let mut head = FisHead::new(Version::V4_0);
    head.presentation_time = Rational::new(0, 1000);
    head.basetime = basetime;
    head.segment_length = Some(0);
    head.content_byte_offset = Some(0);
    let head_packet = head.to_bytes();

    let v_id = vorbis_id_packet(2, SAMPLE_RATE);

    let mut bone = FisBone::new(VORBIS_SERIAL, Rational::new(SAMPLE_RATE as i64, 1));
    bone.num_headers = 3;
    bone.preroll = 2;
    bone.granuleshift = 0;
    bone.basegranule = basegranule;
    bone.set_header("Content-Type", "audio/vorbis");
    let bone_packet = bone.to_bytes();

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
    out.extend_from_slice(&single_packet_page(&bone_packet, 0, SKEL_SERIAL, 1, 0));
    out.extend_from_slice(&single_packet_page(&v_setup, 0, VORBIS_SERIAL, 2, 0));
    out.extend_from_slice(&single_packet_page(
        &[],
        flags::LAST_PAGE,
        SKEL_SERIAL,
        2,
        0,
    ));
    let data_packet: Vec<u8> = (0..100u8).collect();
    out.extend_from_slice(&single_packet_page(
        &data_packet,
        flags::LAST_PAGE,
        VORBIS_SERIAL,
        3,
        last_granule,
    ));
    out
}

fn open(bytes: Vec<u8>) -> oxideav_ogg::demux::OggDemuxer {
    let input: Box<dyn ReadSeek> = Box::new(Cursor::new(bytes));
    oxideav_ogg::demux::open_concrete(input, &NullCodecResolver).expect("open")
}

#[test]
fn basetime_anchors_stream_start_time_in_timebase_ticks() {
    // 1-hour basetime, the spec's analog-video "starts at 01:00:00"
    // example. The Vorbis time_base is 1/48000, so 3600 s anchors at
    // 3600 * 48000 = 172_800_000 ticks.
    let bytes = build_anchored_ogg(Rational::new(3600, 1), 0, 96_000);
    let dmx = open(bytes);
    let stream = &dmx.streams()[0];
    assert_eq!(stream.start_time, Some(3600 * SAMPLE_RATE as i64));
}

#[test]
fn basegranule_adds_to_basetime_on_start_time() {
    // basetime 3600 s + basegranule 24000 (= 0.5 s at 48 kHz) anchors at
    // (3600 + 0.5) s = 3600.5 s = 172_824_000 ticks.
    let bytes = build_anchored_ogg(Rational::new(3600, 1), 24_000, 96_000);
    let dmx = open(bytes);
    let stream = &dmx.streams()[0];
    let expected = (3600 * SAMPLE_RATE as i64) + 24_000;
    assert_eq!(stream.start_time, Some(expected));
}

#[test]
fn zero_basetime_keeps_default_start_time() {
    // An explicit 0/N basetime + 0 basegranule is the un-cut default:
    // start_time stays at 0, not re-anchored.
    let bytes = build_anchored_ogg(Rational::new(0, 1000), 0, 96_000);
    let dmx = open(bytes);
    let stream = &dmx.streams()[0];
    assert_eq!(stream.start_time, Some(0));
}

#[test]
fn duration_stays_basetime_free() {
    // The data page's granule is 96_000 = 2.0 s at 48 kHz. A non-zero
    // basetime must NOT inflate the reported duration: duration is the
    // span end - start, independent of where granule 0 sits on the clock.
    let bytes = build_anchored_ogg(Rational::new(3600, 1), 0, 96_000);
    let dmx = open(bytes);
    let dur = dmx.duration_micros().expect("duration");
    // 96_000 / 48_000 = 2.0 s = 2_000_000 us, NOT 3602 s.
    assert_eq!(dur, 2_000_000);
}
