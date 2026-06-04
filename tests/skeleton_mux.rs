//! Skeleton-mux integration tests.
//!
//! Exercise `oxideav_ogg::mux::open_with_skeleton` end-to-end and verify:
//! 1. The Skeleton fishead BOS is the very first BOS page in the output
//!    (per `docs/container/ogg/ogg-skeleton-{3,4}.0.md`).
//! 2. Content streams' BOS pages follow the Skeleton BOS.
//! 3. Skeleton fisbones / index packets and the Skeleton EOS page all
//!    precede any content data page (the spec's "control section ends
//!    before any data pages of the other logical bitstreams appear"
//!    rule).
//! 4. The demuxer's existing Skeleton path reconstructs the original
//!    fishead + fisbones + indexes from the muxed bytes (round-trip).

use std::io::Cursor;

use oxideav_core::{CodecId, CodecParameters, Packet, ReadSeek, StreamInfo, TimeBase, WriteSeek};
use oxideav_ogg::skeleton::{FisBone, FisHead, KeyPoint, Rational, SkelIndex, Skeleton, Version};

const FISHEAD_MAGIC: &[u8] = b"fishead\0";
const FISBONE_MAGIC: &[u8] = b"fisbone\0";

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

/// Xiph-lace three header packets into a single extradata blob — same
/// layout the muxer's `extract_codec_headers` inverts at write_header.
fn xiph_lace_three(packets: &[&[u8]]) -> Vec<u8> {
    assert_eq!(packets.len(), 3);
    let mut out = Vec::new();
    out.push(0x02);
    for n in [packets[0].len(), packets[1].len()] {
        let mut n = n;
        while n >= 255 {
            out.push(255);
            n -= 255;
        }
        out.push(n as u8);
    }
    for pkt in packets {
        out.extend_from_slice(pkt);
    }
    out
}

fn single_stream(index: u32, codec: &str, extradata: Vec<u8>, time_base: TimeBase) -> StreamInfo {
    let mut params = CodecParameters::audio(CodecId::new(codec));
    params.channels = Some(2);
    params.sample_rate = Some(48_000);
    params.extradata = extradata;
    StreamInfo {
        index,
        time_base,
        duration: None,
        start_time: Some(0),
        params,
    }
}

#[derive(Clone, Default)]
struct SharedBuf(std::sync::Arc<std::sync::Mutex<Cursor<Vec<u8>>>>);

impl std::io::Write for SharedBuf {
    fn write(&mut self, b: &[u8]) -> std::io::Result<usize> {
        self.0.lock().unwrap().write(b)
    }
    fn flush(&mut self) -> std::io::Result<()> {
        self.0.lock().unwrap().flush()
    }
}

impl std::io::Seek for SharedBuf {
    fn seek(&mut self, p: std::io::SeekFrom) -> std::io::Result<u64> {
        self.0.lock().unwrap().seek(p)
    }
}

fn mux_with_skeleton(streams: Vec<StreamInfo>, skel: Skeleton, content_packets: usize) -> Vec<u8> {
    let shared = SharedBuf::default();
    let writer: Box<dyn WriteSeek> = Box::new(shared.clone());
    let mut muxer = oxideav_ogg::mux::open_with_skeleton(writer, &streams, Some(skel))
        .expect("open_with_skeleton");
    muxer.write_header().unwrap();
    for i in 1..=content_packets as i64 {
        let mut pkt = Packet::new(0, streams[0].time_base, vec![0xAA, i as u8]);
        pkt.pts = Some(960 * i);
        pkt.dts = pkt.pts;
        pkt.flags.keyframe = true;
        pkt.flags.unit_boundary = true;
        muxer.write_packet(&pkt).unwrap();
    }
    muxer.write_trailer().unwrap();
    drop(muxer);
    let guard = shared.0.lock().unwrap();
    guard.get_ref().clone()
}

#[derive(Debug, Clone)]
struct PageInfo {
    flags: u8,
    granule: i64,
    serial: u32,
    body: Vec<u8>,
}

/// Naive walker: split the muxed buffer into pages and return their
/// `(flags, granule, serial, body)` tuples in on-wire order.
fn walk_pages(bytes: &[u8]) -> Vec<PageInfo> {
    let mut out = Vec::new();
    let mut off = 0usize;
    while off + 27 <= bytes.len() && &bytes[off..off + 4] == b"OggS" {
        let flags = bytes[off + 5];
        let granule = i64::from_le_bytes([
            bytes[off + 6],
            bytes[off + 7],
            bytes[off + 8],
            bytes[off + 9],
            bytes[off + 10],
            bytes[off + 11],
            bytes[off + 12],
            bytes[off + 13],
        ]);
        let serial = u32::from_le_bytes([
            bytes[off + 14],
            bytes[off + 15],
            bytes[off + 16],
            bytes[off + 17],
        ]);
        let n_segs = bytes[off + 26] as usize;
        let lacing_start = off + 27;
        let data_start = lacing_start + n_segs;
        if data_start > bytes.len() {
            break;
        }
        let data_len: usize = bytes[lacing_start..data_start]
            .iter()
            .map(|&v| v as usize)
            .sum();
        let body_end = data_start + data_len;
        if body_end > bytes.len() {
            break;
        }
        out.push(PageInfo {
            flags,
            granule,
            serial,
            body: bytes[data_start..body_end].to_vec(),
        });
        off = body_end;
    }
    out
}

#[test]
fn skeleton_fishead_is_first_bos_page() {
    let id = vorbis_id_packet(2, 48_000);
    let com = vorbis_comment_packet();
    let setup = vorbis_setup_packet();
    let extradata = xiph_lace_three(&[&id, &com, &setup]);
    let stream = single_stream(0, "vorbis", extradata, TimeBase::new(1, 48_000));

    let mut skel = Skeleton::new();
    skel.set_head(FisHead::new(Version::V4_0));
    skel.push_bone(FisBone::new(0, Rational::new(48_000, 1)));

    let bytes = mux_with_skeleton(vec![stream], skel, 3);
    let pages = walk_pages(&bytes);

    let first = pages.first().expect("at least one page");
    assert_eq!(
        first.flags & 0x02,
        0x02,
        "first page must carry the BOS flag"
    );
    assert!(
        first.body.starts_with(FISHEAD_MAGIC),
        "first BOS page must carry the Skeleton fishead packet"
    );

    // The very next BOS page must belong to the content stream (Vorbis).
    let next_bos = pages
        .iter()
        .skip(1)
        .find(|p| p.flags & 0x02 != 0)
        .expect("content BOS page follows");
    assert_ne!(
        next_bos.serial, first.serial,
        "content BOS has its own serial"
    );
    // Vorbis identification packet starts with 0x01 + "vorbis".
    assert_eq!(next_bos.body[0], 0x01);
    assert!(&next_bos.body[1..7] == b"vorbis");
}

#[test]
fn skeleton_control_section_closes_before_content_data() {
    let id = vorbis_id_packet(2, 48_000);
    let com = vorbis_comment_packet();
    let setup = vorbis_setup_packet();
    let extradata = xiph_lace_three(&[&id, &com, &setup]);
    let stream = single_stream(0, "vorbis", extradata, TimeBase::new(1, 48_000));

    let mut skel = Skeleton::new();
    skel.set_head(FisHead::new(Version::V4_0));
    skel.push_bone(FisBone::new(0, Rational::new(48_000, 1)));

    let bytes = mux_with_skeleton(vec![stream], skel, 4);
    let pages = walk_pages(&bytes);

    // Locate the Skeleton serial (= first page's serial).
    let skel_serial = pages[0].serial;

    // The Skeleton EOS page must appear before any *content data* page.
    // "Content data" here means a page on a content stream's serial
    // carrying packet bytes that finish during the page — those carry
    // a non-zero granule, distinguishing them from the secondary
    // header pages (which the muxer pins at granule 0) that legitimately
    // share the control section with Skeleton's own fisbones per the
    // spec's "secondary header pages of all logical bitstreams come
    // next" rule.
    let mut skel_eos_index: Option<usize> = None;
    let mut early_content_data: Option<usize> = None;
    for (i, p) in pages.iter().enumerate() {
        if p.serial == skel_serial && p.flags & 0x04 != 0 {
            skel_eos_index = Some(i);
        }
        if skel_eos_index.is_none() && p.serial != skel_serial && p.granule > 0 {
            early_content_data = Some(i);
        }
    }
    let eos = skel_eos_index.expect("Skeleton EOS page emitted");
    assert!(
        early_content_data.is_none(),
        "no content data page (granule > 0) may appear before the Skeleton EOS"
    );

    // After EOS, content data pages do exist (the four packets we sent).
    let post_eos_data = pages[eos + 1..]
        .iter()
        .filter(|p| p.serial != skel_serial && p.granule > 0)
        .count();
    assert!(post_eos_data > 0, "content data pages must follow the EOS");

    // EOS page itself carries a zero-payload packet (lace(0) → [0]),
    // so its body is empty.
    assert!(
        pages[eos].body.is_empty(),
        "Skeleton EOS packet is empty per spec"
    );
}

#[test]
fn skeleton_fisbone_and_index_packets_emitted_each_on_own_page() {
    let id = vorbis_id_packet(2, 48_000);
    let com = vorbis_comment_packet();
    let setup = vorbis_setup_packet();
    let extradata = xiph_lace_three(&[&id, &com, &setup]);
    let stream = single_stream(0, "vorbis", extradata, TimeBase::new(1, 48_000));

    let mut skel = Skeleton::new();
    skel.set_head(FisHead::new(Version::V4_0));
    let mut bone = FisBone::new(0, Rational::new(48_000, 1));
    bone.set_header("Content-Type", "audio/vorbis");
    bone.set_header("Role", "audio/main");
    bone.set_header("Name", "main audio");
    skel.push_bone(bone);
    let mut idx = SkelIndex::new(0, 48_000);
    idx.first_sample_time = 0;
    idx.last_sample_time = 480_000;
    idx.keypoints.push(KeyPoint {
        offset: 200,
        timestamp: 0,
    });
    idx.keypoints.push(KeyPoint {
        offset: 5_000,
        timestamp: 240_000,
    });
    skel.push_index(idx.clone());

    let bytes = mux_with_skeleton(vec![stream], skel, 1);
    let pages = walk_pages(&bytes);
    let skel_serial = pages[0].serial;

    // Tally Skeleton-stream pages by payload class.
    let mut fishead_pages = 0;
    let mut fisbone_pages = 0;
    let mut index_pages = 0;
    let mut empty_pages = 0;
    for p in &pages {
        if p.serial != skel_serial {
            continue;
        }
        if p.body.starts_with(FISHEAD_MAGIC) {
            fishead_pages += 1;
        } else if p.body.starts_with(FISBONE_MAGIC) {
            fisbone_pages += 1;
        } else if p.body.starts_with(b"index\0") {
            index_pages += 1;
        } else if p.body.is_empty() {
            empty_pages += 1;
        }
    }
    assert_eq!(fishead_pages, 1);
    assert_eq!(fisbone_pages, 1);
    assert_eq!(index_pages, 1);
    assert_eq!(empty_pages, 1, "EOS empty-packet page");
}

#[test]
fn skeleton_round_trip_via_demuxer() {
    let id = vorbis_id_packet(1, 44_100);
    let com = vorbis_comment_packet();
    let setup = vorbis_setup_packet();
    let extradata = xiph_lace_three(&[&id, &com, &setup]);
    let stream = single_stream(0, "vorbis", extradata, TimeBase::new(1, 44_100));

    let mut head = FisHead::new(Version::V4_0);
    head.presentation_time = Rational::new(0, 1);
    head.basetime = Rational::new(0, 1);
    let mut skel = Skeleton::new();
    skel.set_head(head.clone());
    let mut bone = FisBone::new(0, Rational::new(44_100, 1));
    bone.preroll = 2;
    bone.granuleshift = 0;
    bone.set_header("Content-Type", "audio/vorbis");
    bone.set_header("Role", "audio/main");
    bone.set_header("Name", "main audio");
    skel.push_bone(bone);

    let bytes = mux_with_skeleton(vec![stream], skel, 2);

    let reader: Box<dyn ReadSeek> = Box::new(Cursor::new(bytes));
    let dmx = oxideav_ogg::demux::open_concrete(reader, &oxideav_core::NullCodecResolver)
        .expect("demux skel-muxed output");

    let recovered = dmx.skeleton().expect("Skeleton recovered from muxed bytes");
    assert!(recovered.is_parsed(), "fishead parsed");
    let recovered_head = recovered.head.as_ref().unwrap();
    assert_eq!(recovered_head.version, Version::V4_0);
    assert_eq!(recovered_head.presentation_time, Rational::new(0, 1));
    assert_eq!(recovered.bones.len(), 1);
    let recovered_bone = &recovered.bones[0];
    assert_eq!(recovered_bone.granule_rate, Rational::new(44_100, 1));
    assert_eq!(recovered_bone.preroll, 2);
    assert_eq!(recovered_bone.header("Content-Type"), Some("audio/vorbis"));
    assert_eq!(recovered_bone.header("Role"), Some("audio/main"));
    assert_eq!(recovered_bone.header("Name"), Some("main audio"));
}

#[test]
fn open_without_skeleton_emits_no_skeleton_bytes() {
    // Baseline: the existing `open` factory (which now delegates to
    // `open_with_skeleton(_, _, None)`) must still produce a Skeleton-
    // free physical stream — no fishead BOS, no fisbone secondary
    // header, no empty EOS page. This pins the backward-compat behaviour
    // even though every byte still rides through the same code path.
    let id = vorbis_id_packet(2, 48_000);
    let com = vorbis_comment_packet();
    let setup = vorbis_setup_packet();
    let extradata = xiph_lace_three(&[&id, &com, &setup]);
    let stream = single_stream(0, "vorbis", extradata, TimeBase::new(1, 48_000));

    let shared = SharedBuf::default();
    let writer: Box<dyn WriteSeek> = Box::new(shared.clone());
    let mut muxer = oxideav_ogg::mux::open(writer, std::slice::from_ref(&stream)).unwrap();
    muxer.write_header().unwrap();
    for i in 1..=2i64 {
        let mut pkt = Packet::new(0, stream.time_base, vec![0xCC, i as u8]);
        pkt.pts = Some(960 * i);
        pkt.dts = pkt.pts;
        pkt.flags.keyframe = true;
        pkt.flags.unit_boundary = true;
        muxer.write_packet(&pkt).unwrap();
    }
    muxer.write_trailer().unwrap();
    drop(muxer);
    let bytes = shared.0.lock().unwrap().get_ref().clone();

    let pages = walk_pages(&bytes);
    assert!(!pages.is_empty());
    for p in &pages {
        assert!(
            !p.body.starts_with(FISHEAD_MAGIC),
            "no fishead must appear when Skeleton was not attached"
        );
        assert!(
            !p.body.starts_with(FISBONE_MAGIC),
            "no fisbone must appear when Skeleton was not attached"
        );
    }

    // Round-trip via the demuxer: skeleton() must be None.
    let reader: Box<dyn ReadSeek> = Box::new(Cursor::new(bytes));
    let dmx = oxideav_ogg::demux::open_concrete(reader, &oxideav_core::NullCodecResolver)
        .expect("demux skel-less output");
    assert!(dmx.skeleton().is_none());
}
