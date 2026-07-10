//! A content BOS reusing the Skeleton bitstream's serial, and a second
//! `fishead\0` BOS on another serial — both RFC 3533 §4 / Skeleton
//! encapsulation violations the demuxer must absorb without inventing
//! phantom streams.
//!
//! Found by the `chain_graph` structure-aware fuzz target: a Vorbis
//! BOS page whose `bitstream_serial_number` equalled the Skeleton
//! stream's serial was registered as a public content stream even
//! though every page carrying that serial routes to the Skeleton
//! metadata path — a `streams()` entry that could never receive a
//! packet, and one that broke the Skeleton "Track order" bijection
//! (`track_order_index(track_order_serial(t)) != t`).

use std::io::Cursor;

use oxideav_core::{Demuxer, Error, ReadSeek};
use oxideav_ogg::demux;
use oxideav_ogg::page::{flags, lace, Page};
use oxideav_ogg::skeleton::{FisBone, FisHead, Rational, Version};

const SKELETON_SERIAL: u32 = 0x0051_3E1E;
const VORBIS_SERIAL: u32 = 0x00A0_0001;

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
    p
}

fn vorbis_comment_packet() -> Vec<u8> {
    let mut p = vec![0x03];
    p.extend_from_slice(b"vorbis");
    p.extend_from_slice(&[0, 0, 0, 0, 0, 0, 0, 0, 1]);
    p
}

fn vorbis_setup_packet() -> Vec<u8> {
    let mut p = vec![0x05];
    p.extend_from_slice(b"vorbis");
    p.extend_from_slice(&[0u8; 16]);
    p
}

fn page(flags_byte: u8, granule: i64, serial: u32, seq: u32, packet: &[u8]) -> Vec<u8> {
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

/// Skeleton-bearing single-Vorbis file whose data section is followed
/// by a rogue Vorbis BOS reusing the SKELETON serial.
fn build_collision_file() -> Vec<u8> {
    let mut buf = Vec::new();
    // Skeleton fishead BOS — the very first BOS page.
    let head = FisHead::new(Version::V4_0);
    buf.extend(page(
        flags::FIRST_PAGE,
        0,
        SKELETON_SERIAL,
        0,
        &head.to_bytes(),
    ));
    // Vorbis BOS.
    buf.extend(page(
        flags::FIRST_PAGE,
        0,
        VORBIS_SERIAL,
        0,
        &vorbis_id_packet(),
    ));
    // Skeleton fisbone for the Vorbis stream, then the empty EOS page
    // closing the control section.
    let bone = FisBone::new(VORBIS_SERIAL, Rational::new(48_000, 1));
    buf.extend(page(0, 0, SKELETON_SERIAL, 1, &bone.to_bytes()));
    buf.extend(page(flags::LAST_PAGE, 0, SKELETON_SERIAL, 2, &[]));
    // Vorbis secondary headers.
    buf.extend(page(0, 0, VORBIS_SERIAL, 1, &vorbis_comment_packet()));
    buf.extend(page(0, 0, VORBIS_SERIAL, 2, &vorbis_setup_packet()));
    // One Vorbis data page.
    buf.extend(page(0, 4096, VORBIS_SERIAL, 3, &[0xAA; 64]));
    // ROGUE: a content (Vorbis-id) BOS reusing the Skeleton's serial.
    buf.extend(page(
        flags::FIRST_PAGE,
        0,
        SKELETON_SERIAL,
        3,
        &vorbis_id_packet(),
    ));
    // Final Vorbis data page (EOS).
    buf.extend(page(flags::LAST_PAGE, 8192, VORBIS_SERIAL, 4, &[0xBB; 64]));
    buf
}

fn drain(dmx: &mut demux::OggDemuxer) -> Vec<u32> {
    let mut indexes = Vec::new();
    loop {
        match dmx.next_packet() {
            Ok(pkt) => indexes.push(pkt.stream_index),
            Err(Error::Eof) => break,
            Err(e) => panic!("unexpected demux error: {e:?}"),
        }
    }
    indexes
}

#[test]
fn content_bos_on_skeleton_serial_is_not_a_phantom_stream() {
    let reader: Box<dyn ReadSeek> = Box::new(Cursor::new(build_collision_file()));
    let mut dmx = demux::open_concrete(reader, &oxideav_core::NullCodecResolver).unwrap();

    // Only the real Vorbis stream is public — the rogue BOS must not
    // register a stream that can never receive packets.
    assert_eq!(
        dmx.streams().len(),
        1,
        "no phantom stream for the rogue BOS"
    );
    assert_eq!(dmx.stream_serial(0), Some(VORBIS_SERIAL));

    let delivered = drain(&mut dmx);
    assert!(!delivered.is_empty());
    assert!(delivered.iter().all(|&i| i == 0));

    // The violation is surfaced on the duplicate-serial diagnostic.
    assert_eq!(
        dmx.duplicate_serial_count(),
        1,
        "rogue BOS on the Skeleton serial is a unique-serial violation"
    );

    // Track-order mapping stays a bijection: track 0 = Skeleton,
    // track 1 = the Vorbis stream.
    assert_eq!(dmx.track_order_len(), 2);
    assert_eq!(dmx.track_order_serial(0), Some(SKELETON_SERIAL));
    assert_eq!(dmx.track_order_serial(1), Some(VORBIS_SERIAL));
    assert_eq!(dmx.track_order_index(SKELETON_SERIAL), Some(0));
    assert_eq!(dmx.track_order_index(VORBIS_SERIAL), Some(1));

    // The Skeleton state survives the rogue page (its payload is not a
    // recognised Skeleton packet and is skipped).
    let sk = dmx.skeleton().expect("skeleton recorded");
    assert_eq!(sk.serial, Some(SKELETON_SERIAL));
    assert!(sk.bone_for_serial(VORBIS_SERIAL).is_some());
}

#[test]
fn second_fishead_bos_on_another_serial_keeps_the_first_skeleton() {
    const IMPOSTOR_SERIAL: u32 = 0x00BE_EF00;
    let mut buf = Vec::new();
    let head = FisHead::new(Version::V4_0);
    buf.extend(page(
        flags::FIRST_PAGE,
        0,
        SKELETON_SERIAL,
        0,
        &head.to_bytes(),
    ));
    // Impostor: a second fishead BOS on a different serial.
    let impostor = FisHead::new(Version::V3_0);
    buf.extend(page(
        flags::FIRST_PAGE,
        0,
        IMPOSTOR_SERIAL,
        0,
        &impostor.to_bytes(),
    ));
    buf.extend(page(
        flags::FIRST_PAGE,
        0,
        VORBIS_SERIAL,
        0,
        &vorbis_id_packet(),
    ));
    let bone = FisBone::new(VORBIS_SERIAL, Rational::new(48_000, 1));
    buf.extend(page(0, 0, SKELETON_SERIAL, 1, &bone.to_bytes()));
    buf.extend(page(flags::LAST_PAGE, 0, SKELETON_SERIAL, 2, &[]));
    buf.extend(page(0, 0, VORBIS_SERIAL, 1, &vorbis_comment_packet()));
    buf.extend(page(0, 0, VORBIS_SERIAL, 2, &vorbis_setup_packet()));
    buf.extend(page(flags::LAST_PAGE, 4096, VORBIS_SERIAL, 3, &[0xCC; 32]));

    let reader: Box<dyn ReadSeek> = Box::new(Cursor::new(buf));
    let mut dmx = demux::open_concrete(reader, &oxideav_core::NullCodecResolver).unwrap();

    // The first Skeleton wins; the impostor registers nothing.
    assert_eq!(dmx.streams().len(), 1);
    let sk = dmx.skeleton().expect("skeleton recorded");
    assert_eq!(sk.serial, Some(SKELETON_SERIAL));
    assert_eq!(sk.head.as_ref().map(|h| h.version), Some(Version::V4_0));
    assert!(
        sk.bone_for_serial(VORBIS_SERIAL).is_some(),
        "first skeleton's fisbones must survive the impostor"
    );

    let delivered = drain(&mut dmx);
    assert!(delivered.iter().all(|&i| i == 0));

    // Track order still reflects the (single, first) Skeleton.
    assert_eq!(dmx.track_order_len(), 2);
    assert_eq!(dmx.track_order_index(SKELETON_SERIAL), Some(0));
    assert_eq!(dmx.track_order_index(VORBIS_SERIAL), Some(1));
    assert_eq!(dmx.track_order_index(IMPOSTOR_SERIAL), None);
}
