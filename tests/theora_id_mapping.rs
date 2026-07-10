//! Theora-in-Ogg demux driven by the stream's own identification header —
//! no Skeleton required.
//!
//! The Theora spec's Ogg encapsulation appendix
//! (`docs/video/theora/Theora.pdf` §6.2 + §A.2) makes the container layer
//! depend on the ID header: KFGSHIFT + VREV define the granule-position
//! packing, FRN/FRD the frame rate, PICW/PICH the display size. This file
//! synthesises a conformant Theora logical stream (valid 42-byte ID
//! header, multi-packet data pages, split-granule marks) and pins:
//!
//! * stream description (dimensions, frame rate, pixel format, one-tick-
//!   per-frame time base);
//! * per-packet pts as 0-based absolute frame indices — for EVERY data
//!   packet, not just the granule-bearing last-on-page one (each Theora
//!   packet is exactly one frame, so the page granule anchors them all);
//! * keyframe flags derived from the granule packing;
//! * duration from the final page's unpacked frame count;
//! * Skeleton-free `seek_to` / `seek_to_keyframe`.
//!
//! Spec: `docs/container/ogg/rfc3533-ogg.txt`,
//! `docs/video/theora/Theora.pdf` (§6.2, §A.2).

use std::io::Cursor;

use oxideav_core::{Demuxer, NullCodecResolver, PixelFormat, ReadSeek, TimeBase};

use oxideav_ogg::page::{flags, lace, Page};
use oxideav_ogg::theora::{TheoraGranule, TheoraIdHeader};

const THEORA_SERIAL: u32 = 0x71EB1A11;

fn id_header() -> TheoraIdHeader {
    TheoraIdHeader {
        vmaj: 3,
        vmin: 2,
        vrev: 1,
        fmbw: 20, // 320 coded
        fmbh: 15, // 240 coded
        picw: 320,
        pich: 240,
        picx: 0,
        picy: 0,
        frn: 25,
        frd: 1,
        parn: 1,
        pard: 1,
        cs: 0,
        nombr: 400_000,
        qual: 40,
        kfgshift: 6,
        pf: 0,
    }
}

fn comment_packet() -> Vec<u8> {
    let mut p = vec![0x81];
    p.extend_from_slice(b"theora");
    p.extend_from_slice(&0u32.to_le_bytes()); // vendor length
    p.extend_from_slice(&0u32.to_le_bytes()); // user comment count
    p
}

fn setup_packet() -> Vec<u8> {
    let mut p = vec![0x82];
    p.extend_from_slice(b"theora");
    p.extend_from_slice(&[0u8; 24]);
    p
}

fn page_of(packets: &[&[u8]], flags_byte: u8, seq: u32, granule: i64) -> Vec<u8> {
    let mut lacing = Vec::new();
    let mut data = Vec::new();
    for p in packets {
        lacing.extend_from_slice(&lace(p.len()));
        data.extend_from_slice(p);
    }
    Page {
        flags: flags_byte,
        granule_position: granule,
        serial: THEORA_SERIAL,
        seq_no: seq,
        lacing,
        data,
    }
    .to_bytes()
}

/// Build a conformant Skeleton-free Theora file:
///
/// * BOS page: ID header alone (spec §A.2.1);
/// * page 1: comment + setup packets (may share a page, §A.2.1);
/// * page 2: frames 0,1,2 (keyframe at 0), granule `1|2`;
/// * page 3: frames 3,4, granule `1|4`;
/// * page 4: frame 5 alone — a mid-stream keyframe, granule `6|0`;
/// * page 5 (EOS): frames 6,7, granule `6|2`.
fn build_file() -> Vec<u8> {
    let g = id_header().granule();
    let frame = |n: usize| vec![0x40u8 + n as u8; 20 + n];
    let mut out = Vec::new();
    out.extend(page_of(&[&id_header().to_bytes()], flags::FIRST_PAGE, 0, 0));
    out.extend(page_of(&[&comment_packet(), &setup_packet()], 0, 1, 0));
    out.extend(page_of(
        &[&frame(0), &frame(1), &frame(2)],
        0,
        2,
        g.pack(2, 0).unwrap(),
    ));
    out.extend(page_of(
        &[&frame(3), &frame(4)],
        0,
        3,
        g.pack(4, 0).unwrap(),
    ));
    out.extend(page_of(&[&frame(5)], 0, 4, g.pack(5, 5).unwrap()));
    out.extend(page_of(
        &[&frame(6), &frame(7)],
        flags::LAST_PAGE,
        5,
        g.pack(7, 5).unwrap(),
    ));
    out
}

#[test]
fn id_header_populates_stream_description() {
    let reader: Box<dyn ReadSeek> = Box::new(Cursor::new(build_file()));
    let dmx = oxideav_ogg::demux::open_concrete(reader, &NullCodecResolver).expect("open");
    assert_eq!(dmx.streams().len(), 1);
    let s = &dmx.streams()[0];
    assert_eq!(s.params.codec_id.as_str(), "theora");
    assert_eq!(s.params.width, Some(320));
    assert_eq!(s.params.height, Some(240));
    assert_eq!(s.params.pixel_format, Some(PixelFormat::Yuv420P));
    let fr = s.params.frame_rate.expect("frame rate from FRN/FRD");
    assert_eq!((fr.num, fr.den), (25, 1));
    assert_eq!(
        s.params.bit_rate,
        Some(400_000),
        "NOMBR surfaces as bit_rate"
    );
    // One tick = one frame.
    assert_eq!(s.time_base, TimeBase::new(1, 25));
    // KFGSHIFT is exposed even without any Skeleton fisbone.
    assert_eq!(dmx.stream_granuleshift(0), Some(6));
}

#[test]
fn every_data_packet_gets_frame_index_pts_and_proven_keyframe_flags() {
    let reader: Box<dyn ReadSeek> = Box::new(Cursor::new(build_file()));
    let mut dmx = oxideav_ogg::demux::open_concrete(reader, &NullCodecResolver).expect("open");
    let mut got: Vec<(Option<i64>, bool)> = Vec::new();
    while let Ok(pkt) = oxideav_core::Demuxer::next_packet(&mut dmx) {
        got.push((pkt.pts, pkt.flags.keyframe));
    }
    let pts: Vec<Option<i64>> = got.iter().map(|(p, _)| *p).collect();
    assert_eq!(
        pts,
        (0..8).map(Some).collect::<Vec<_>>(),
        "all 8 data packets carry 0-based frame indices, including the \
         non-granule-bearing mid-page ones"
    );
    let kf: Vec<bool> = got.iter().map(|(_, k)| *k).collect();
    assert_eq!(
        kf,
        vec![true, false, false, false, false, true, false, false],
        "keyframes at frames 0 and 5 exactly (granule offset-since-keyframe 0)"
    );
}

#[test]
fn duration_unpacks_the_final_granule_without_a_skeleton() {
    let reader: Box<dyn ReadSeek> = Box::new(Cursor::new(build_file()));
    let dmx = oxideav_ogg::demux::open_concrete(reader, &NullCodecResolver).expect("open");
    // 8 frames at 25 fps = 0.32 s. The raw final granule is 6|2 = 386 —
    // a naive granule-as-frame-count read would report 15.44 s.
    let dur = Demuxer::duration_micros(&dmx).expect("duration");
    assert_eq!(dur, 320_000, "8 frames / 25 fps");
}

#[test]
fn seek_to_works_without_skeleton_and_floors_by_frame_index() {
    let g = id_header().granule();
    let reader: Box<dyn ReadSeek> = Box::new(Cursor::new(build_file()));
    let mut dmx = oxideav_ogg::demux::open_concrete(reader, &NullCodecResolver).expect("open");
    // pts are frame indices (time base 1/25). Target frame 5 → the floor
    // page among last-frame keys {2, 4, 5, 7} is the keyframe page (5).
    let landed = Demuxer::seek_to(&mut dmx, 0, 5).expect("seek ok");
    assert_eq!(landed, g.pack(5, 5).unwrap());
    // Target frame 4 → page ending at frame 4.
    let landed = Demuxer::seek_to(&mut dmx, 0, 4).expect("seek ok");
    assert_eq!(landed, g.pack(4, 0).unwrap());
    // Past-the-end target → last page.
    let landed = Demuxer::seek_to(&mut dmx, 0, 1000).expect("seek ok");
    assert_eq!(landed, g.pack(7, 5).unwrap());
    // After a seek, delivery resumes with the landed page's first packet
    // carrying the right frame index.
    let pkt = Demuxer::next_packet(&mut dmx).expect("packet after seek");
    assert_eq!(pkt.pts, Some(6), "EOS page starts at frame 6");
}

#[test]
fn seek_to_keyframe_backs_up_to_the_governing_keyframe() {
    let g = id_header().granule();
    let reader: Box<dyn ReadSeek> = Box::new(Cursor::new(build_file()));
    let mut dmx = oxideav_ogg::demux::open_concrete(reader, &NullCodecResolver).expect("open");
    // Target frame 7 lands on the EOS page (granule 6|2, offset ≠ 0); the
    // keyframe half names frame 5, whose own page carries granule 6|0.
    let landed = dmx.seek_to_keyframe(0, 7).expect("seek_to_keyframe ok");
    assert_eq!(landed, g.pack(5, 5).unwrap(), "landed on the keyframe page");
    let pkt = Demuxer::next_packet(&mut dmx).expect("packet after seek");
    assert_eq!(pkt.pts, Some(5));
    assert!(
        pkt.flags.keyframe,
        "resumes decoding on the keyframe itself"
    );
}

#[test]
fn vrev0_streams_use_frame_index_granules() {
    // A VREV 0 (pre-3.2.1) stream marks granules with the frame *index*
    // (spec §A.2.3) — the first data page of a keyframe-led stream
    // carries 0|0, not 1|0. pts must come out identical to the VREV 1
    // arrangement.
    let mut id = id_header();
    id.vrev = 0;
    let g = id.granule();
    assert_eq!(
        g,
        TheoraGranule {
            shift: 6,
            count_from_one: false
        }
    );
    let frame = |n: usize| vec![0x40u8 + n as u8; 20 + n];
    let mut out = Vec::new();
    out.extend(page_of(&[&id.to_bytes()], flags::FIRST_PAGE, 0, 0));
    out.extend(page_of(&[&comment_packet(), &setup_packet()], 0, 1, 0));
    out.extend(page_of(
        &[&frame(0), &frame(1)],
        0,
        2,
        g.pack(1, 0).unwrap(), // 0|1 = raw 1
    ));
    out.extend(page_of(
        &[&frame(2)],
        flags::LAST_PAGE,
        3,
        g.pack(2, 2).unwrap(), // keyframe: 2|0 = raw 128
    ));

    let reader: Box<dyn ReadSeek> = Box::new(Cursor::new(out));
    let mut dmx = oxideav_ogg::demux::open_concrete(reader, &NullCodecResolver).expect("open");
    let mut got: Vec<(Option<i64>, bool)> = Vec::new();
    while let Ok(pkt) = Demuxer::next_packet(&mut dmx) {
        got.push((pkt.pts, pkt.flags.keyframe));
    }
    assert_eq!(
        got,
        vec![(Some(0), true), (Some(1), false), (Some(2), true)],
        "VREV 0 granules are frame indices; frame 0 is proven a keyframe by \
         the 0|1 anchor (its keyframe half names index 0)"
    );
    let dur = Demuxer::duration_micros(&dmx).expect("duration");
    assert_eq!(dur, 120_000, "3 frames / 25 fps");
}
