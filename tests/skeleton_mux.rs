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
    /// Byte offset of the page's `OggS` capture pattern in the buffer.
    offset: usize,
    /// Byte offset one past the end of the page (start of the next page).
    end: usize,
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
            offset: off,
            end: body_end,
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

/// Build the standard one-Vorbis-stream fixture used by the fishead
/// backfill tests below.
fn vorbis_stream_fixture() -> StreamInfo {
    let id = vorbis_id_packet(2, 48_000);
    let com = vorbis_comment_packet();
    let setup = vorbis_setup_packet();
    let extradata = xiph_lace_three(&[&id, &com, &setup]);
    single_stream(0, "vorbis", extradata, TimeBase::new(1, 48_000))
}

#[test]
fn secondary_header_pages_all_precede_skeleton_eos() {
    // ogg-skeleton-4.0.md §"Further restrictions": "the secondary header
    // pages of all logical bitstreams come next, including Skeleton's
    // secondary header packets (the fisbone and index packets)" and only
    // then "the Skeleton EOS page ends the control section of the Ogg
    // stream before any content pages of any of the other logical
    // bitstreams appear". The Vorbis setup-header page (packet type 0x05)
    // is the last content secondary header the muxer writes; it must sit
    // before the Skeleton EOS page on the wire, not be deferred into the
    // content section.
    let stream = vorbis_stream_fixture();
    let mut skel = Skeleton::new();
    skel.set_head(FisHead::new(Version::V4_0));
    skel.push_bone(FisBone::new(0, Rational::new(48_000, 1)));

    let bytes = mux_with_skeleton(vec![stream], skel, 3);
    let pages = walk_pages(&bytes);
    let skel_serial = pages[0].serial;

    let eos_index = pages
        .iter()
        .position(|p| p.serial == skel_serial && p.flags & 0x04 != 0)
        .expect("Skeleton EOS page emitted");
    let setup_index = pages
        .iter()
        .position(|p| p.body.first() == Some(&0x05) && p.body[1..].starts_with(b"vorbis"))
        .expect("Vorbis setup-header page emitted");
    let comment_index = pages
        .iter()
        .position(|p| p.body.first() == Some(&0x03) && p.body[1..].starts_with(b"vorbis"))
        .expect("Vorbis comment-header page emitted");
    assert!(
        setup_index < eos_index,
        "Vorbis setup page (#{setup_index}) must precede the Skeleton EOS (#{eos_index})"
    );
    assert!(
        comment_index < eos_index,
        "Vorbis comment page (#{comment_index}) must precede the Skeleton EOS (#{eos_index})"
    );
    // Every page after the Skeleton EOS belongs to a content stream and
    // finishes a data packet (granule > 0 in this fixture).
    for (i, p) in pages.iter().enumerate().skip(eos_index + 1) {
        assert_ne!(p.serial, skel_serial, "no Skeleton page after the EOS");
        assert!(
            p.granule > 0,
            "page #{i} after the Skeleton EOS must be a content data page"
        );
    }
}

#[test]
fn fishead_backfills_segment_length_and_content_byte_offset() {
    // ogg-skeleton-4.0.md: the 4.0 fishead carries "the length of the
    // indexed segment in bytes" (used to detect a stale index: "if it
    // doesn't match the length stored in the Skeleton header packet, you
    // know that either the index is out of date, or the file has been
    // chained since indexing") and "the offset of the first non header
    // page in the Ogg segment". Leaving both at the constructor's 0
    // ("unknown") must make the muxer backfill the measured values at
    // trailer time.
    let stream = vorbis_stream_fixture();
    let mut skel = Skeleton::new();
    skel.set_head(FisHead::new(Version::V4_0));
    skel.push_bone(FisBone::new(0, Rational::new(48_000, 1)));

    let bytes = mux_with_skeleton(vec![stream], skel, 3);
    let pages = walk_pages(&bytes);
    let skel_serial = pages[0].serial;

    // Expected content byte offset: the first byte after the Skeleton
    // EOS page (the EOS closes the control section, so the next page is
    // the first non-header page).
    let eos = pages
        .iter()
        .find(|p| p.serial == skel_serial && p.flags & 0x04 != 0)
        .expect("Skeleton EOS page emitted");
    let expected_offset = eos.end as u64;
    // Sanity: a page really starts there and it is a content data page.
    let first_content = pages
        .iter()
        .find(|p| p.offset as u64 == expected_offset)
        .expect("a page starts at the content byte offset");
    assert_ne!(first_content.serial, skel_serial);
    assert!(first_content.granule > 0);

    let reader: Box<dyn ReadSeek> = Box::new(Cursor::new(bytes.clone()));
    let dmx = oxideav_ogg::demux::open_concrete(reader, &oxideav_core::NullCodecResolver)
        .expect("demux backfilled output");
    let head = dmx
        .skeleton()
        .expect("Skeleton recovered")
        .head
        .as_ref()
        .expect("fishead parsed")
        .clone();
    assert_eq!(
        head.segment_length,
        Some(bytes.len() as u64),
        "segment length must equal the physical segment size"
    );
    assert_eq!(
        head.content_byte_offset,
        Some(expected_offset),
        "content byte offset must point at the first non-header page"
    );
}

#[test]
fn fishead_caller_preset_fields_are_preserved() {
    // A caller that pre-measured the fields (e.g. a remux of a known
    // segment) wins over the muxer's own measurement: non-zero values
    // pass through verbatim, byte-for-byte.
    let stream = vorbis_stream_fixture();
    let mut head = FisHead::new(Version::V4_0);
    head.segment_length = Some(99_999);
    head.content_byte_offset = Some(777);
    let mut skel = Skeleton::new();
    skel.set_head(head);
    skel.push_bone(FisBone::new(0, Rational::new(48_000, 1)));

    let bytes = mux_with_skeleton(vec![stream], skel, 2);

    let reader: Box<dyn ReadSeek> = Box::new(Cursor::new(bytes));
    let dmx = oxideav_ogg::demux::open_concrete(reader, &oxideav_core::NullCodecResolver)
        .expect("demux preset output");
    let recovered = dmx
        .skeleton()
        .expect("Skeleton recovered")
        .head
        .as_ref()
        .expect("fishead parsed")
        .clone();
    assert_eq!(recovered.segment_length, Some(99_999));
    assert_eq!(recovered.content_byte_offset, Some(777));
}

#[test]
fn fishead_3_0_is_never_patched() {
    // The 3.0 fishead layout is 64 bytes and has no segment-length /
    // content-byte-offset fields — the backfill must leave it alone.
    let stream = vorbis_stream_fixture();
    let mut skel = Skeleton::new();
    skel.set_head(FisHead::new(Version::V3_0));
    skel.push_bone(FisBone::new(0, Rational::new(48_000, 1)));

    let bytes = mux_with_skeleton(vec![stream], skel, 2);
    let pages = walk_pages(&bytes);
    assert!(pages[0].body.starts_with(FISHEAD_MAGIC));
    assert_eq!(pages[0].body.len(), 64, "3.0 fishead stays 64 bytes");

    let reader: Box<dyn ReadSeek> = Box::new(Cursor::new(bytes));
    let dmx = oxideav_ogg::demux::open_concrete(reader, &oxideav_core::NullCodecResolver)
        .expect("demux 3.0 output");
    let recovered = dmx
        .skeleton()
        .expect("Skeleton recovered")
        .head
        .as_ref()
        .expect("fishead parsed")
        .clone();
    assert_eq!(recovered.version, Version::V3_0);
    assert_eq!(recovered.segment_length, None);
    assert_eq!(recovered.content_byte_offset, None);
}

// ---------------------------------------------------------------------
// Muxer-built Skeleton 4.0 keyframe indexes
// (`oxideav_ogg::mux::open_with_skeleton_indexed`).
//
// ogg-skeleton-4.0.md §"Keyframe index packets": index packets live in
// the segment's header pages, but keypoint offsets / first-last sample
// times are only knowable after the content is written — the muxer
// reserves a fixed-size placeholder page in write_header and rewrites
// it in place at write_trailer (same page length, CRC recomputed),
// exactly like the fishead segment-length / content-byte-offset
// backfill.
// ---------------------------------------------------------------------

/// Mux `packets` (`(stream_index, pts, keyframe)` triples, each a
/// unit-boundary content packet) through `open_with_skeleton_indexed`.
fn mux_indexed(
    streams: Vec<StreamInfo>,
    skel: Skeleton,
    cfg: oxideav_ogg::mux::AutoIndexConfig,
    packets: &[(u32, i64, bool)],
) -> Vec<u8> {
    let shared = SharedBuf::default();
    let writer: Box<dyn WriteSeek> = Box::new(shared.clone());
    let mut muxer = oxideav_ogg::mux::open_with_skeleton_indexed(writer, &streams, skel, cfg)
        .expect("open_with_skeleton_indexed");
    muxer.write_header().unwrap();
    for &(stream_index, pts, keyframe) in packets {
        let tb = streams[stream_index as usize].time_base;
        let mut pkt = Packet::new(stream_index, tb, vec![0xAA, pts as u8]);
        pkt.pts = Some(pts);
        pkt.dts = pkt.pts;
        pkt.flags.keyframe = keyframe;
        pkt.flags.unit_boundary = true;
        muxer.write_packet(&pkt).unwrap();
    }
    muxer.write_trailer().unwrap();
    drop(muxer);
    let guard = shared.0.lock().unwrap();
    guard.get_ref().clone()
}

/// Permissive gating for tests: every keyframe page becomes a keypoint
/// (subject only to `max_keypoints`).
fn permissive_cfg(max_keypoints: usize) -> oxideav_ogg::mux::AutoIndexConfig {
    oxideav_ogg::mux::AutoIndexConfig {
        max_keypoints,
        min_keypoint_byte_gap: 0,
        min_keypoint_time_gap_ms: 0,
    }
}

fn skel_with_bone() -> Skeleton {
    let mut skel = Skeleton::new();
    skel.set_head(FisHead::new(Version::V4_0));
    let mut bone = FisBone::new(0, Rational::new(48_000, 1));
    bone.set_header("Content-Type", "audio/vorbis");
    skel.push_bone(bone);
    skel
}

#[test]
fn auto_index_records_keypoints_and_demux_fast_path_seeks() {
    let stream = vorbis_stream_fixture();
    let packets: Vec<(u32, i64, bool)> = (1..=6).map(|i| (0u32, 960 * i, true)).collect();
    let bytes = mux_indexed(vec![stream], skel_with_bone(), permissive_cfg(16), &packets);

    // On-wire expectations: the content stream's data pages, in order.
    let pages = walk_pages(&bytes);
    let skel_serial = pages[0].serial;
    let content_data_offsets: Vec<u64> = pages
        .iter()
        .filter(|p| p.serial != skel_serial && p.granule > 0)
        .map(|p| p.offset as u64)
        .collect();
    assert_eq!(content_data_offsets.len(), 6);

    // The recovered index carries one keypoint per keyframe page, in
    // increasing-offset order, timestamped over the stream time-base
    // denominator.
    let reader: Box<dyn ReadSeek> = Box::new(Cursor::new(bytes.clone()));
    let dmx = oxideav_ogg::demux::open_concrete(reader, &oxideav_core::NullCodecResolver)
        .expect("demux auto-indexed output");
    let sk = dmx.skeleton().expect("Skeleton recovered");
    assert_eq!(sk.indexes.len(), 1);
    let idx = &sk.indexes[0];
    assert_eq!(idx.serial, 0, "content stream serial");
    assert_eq!(idx.timestamp_denominator, 48_000);
    assert_eq!(idx.first_sample_time, 960);
    assert_eq!(idx.last_sample_time, 5_760);
    assert_eq!(idx.keypoints.len(), 6);
    for (i, kp) in idx.keypoints.iter().enumerate() {
        assert_eq!(kp.timestamp, 960 * (i as i64 + 1));
        assert_eq!(
            kp.offset, content_data_offsets[i],
            "keypoint {i} must point at the first byte of its content page"
        );
    }
    // The backfilled fishead makes validity check #1 run in enforcing
    // mode (segment length == physical size), not via the 0 opt-out.
    let head = sk.head.as_ref().unwrap();
    assert_eq!(head.segment_length, Some(bytes.len() as u64));

    // End-to-end: seek_to resolves via the Skeleton index fast path and
    // decoding resumes at the keypoint's packet.
    let reader: Box<dyn ReadSeek> = Box::new(Cursor::new(bytes));
    let mut dmx = oxideav_ogg::demux::open_concrete(reader, &oxideav_core::NullCodecResolver)
        .expect("demux for seek");
    use oxideav_core::Demuxer as _;
    let landed = dmx.seek_to(0, 2_880).expect("indexed seek");
    assert_eq!(landed, 2_880);
    assert_eq!(dmx.skeleton_index_seek_count(), 1, "fast path must fire");
    assert_eq!(dmx.skeleton_index_invalid_count(), 0);
    let pkt = dmx.next_packet().expect("packet after seek");
    assert_eq!(pkt.stream_index, 0);
    assert_eq!(pkt.pts, Some(2_880));
}

#[test]
fn auto_index_thins_keypoints_per_byte_and_time_gaps() {
    // Byte gating: a gap larger than any page in this tiny fixture
    // accepts only the first candidate.
    let stream = vorbis_stream_fixture();
    let packets: Vec<(u32, i64, bool)> = (1..=6).map(|i| (0u32, 960 * i, true)).collect();
    let cfg = oxideav_ogg::mux::AutoIndexConfig {
        max_keypoints: 16,
        min_keypoint_byte_gap: 1 << 20,
        min_keypoint_time_gap_ms: 0,
    };
    let bytes = mux_indexed(vec![stream], skel_with_bone(), cfg, &packets);
    let reader: Box<dyn ReadSeek> = Box::new(Cursor::new(bytes));
    let dmx = oxideav_ogg::demux::open_concrete(reader, &oxideav_core::NullCodecResolver).unwrap();
    assert_eq!(dmx.skeleton().unwrap().indexes[0].keypoints.len(), 1);

    // Time gating: packets 20 ms apart against a 1000 ms minimum gap —
    // again only the first candidate survives.
    let stream = vorbis_stream_fixture();
    let packets: Vec<(u32, i64, bool)> = (1..=6).map(|i| (0u32, 960 * i, true)).collect();
    let cfg = oxideav_ogg::mux::AutoIndexConfig {
        max_keypoints: 16,
        min_keypoint_byte_gap: 0,
        min_keypoint_time_gap_ms: 1000,
    };
    let bytes = mux_indexed(vec![stream], skel_with_bone(), cfg, &packets);
    let reader: Box<dyn ReadSeek> = Box::new(Cursor::new(bytes));
    let dmx = oxideav_ogg::demux::open_concrete(reader, &oxideav_core::NullCodecResolver).unwrap();
    assert_eq!(dmx.skeleton().unwrap().indexes[0].keypoints.len(), 1);

    // Same 1000 ms minimum but packets a full second apart: every
    // candidate clears the gate.
    let stream = vorbis_stream_fixture();
    let packets: Vec<(u32, i64, bool)> = (1..=4).map(|i| (0u32, 48_000 * i, true)).collect();
    let bytes = mux_indexed(vec![stream], skel_with_bone(), cfg, &packets);
    let reader: Box<dyn ReadSeek> = Box::new(Cursor::new(bytes));
    let dmx = oxideav_ogg::demux::open_concrete(reader, &oxideav_core::NullCodecResolver).unwrap();
    assert_eq!(dmx.skeleton().unwrap().indexes[0].keypoints.len(), 4);
}

#[test]
fn auto_index_reservation_caps_keypoints_and_keeps_page_length() {
    // max_keypoints = 2 reserves a 42 + 2·20 = 82-byte packet; the
    // five keyframe pages must be thinned to the reservation, and the
    // backfilled page must keep the placeholder's byte length (the
    // in-place rewrite cannot move the pages that follow). Every page
    // CRC — including the two rewritten ones — must still validate.
    let stream = vorbis_stream_fixture();
    let packets: Vec<(u32, i64, bool)> = (1..=5).map(|i| (0u32, 960 * i, true)).collect();
    let bytes = mux_indexed(vec![stream], skel_with_bone(), permissive_cfg(2), &packets);

    let pages = walk_pages(&bytes);
    let skel_serial = pages[0].serial;
    let index_page = pages
        .iter()
        .find(|p| p.serial == skel_serial && p.body.starts_with(b"index\0"))
        .expect("index page emitted");
    assert_eq!(
        index_page.body.len(),
        82,
        "backfilled packet keeps the reserved length (zero tail past the n-th keypoint)"
    );
    let idx = SkelIndex::parse(&index_page.body).expect("padded index packet parses");
    assert_eq!(idx.keypoints.len(), 2);
    assert!(idx.is_sorted_by_offset());

    for p in walk_pages(&bytes) {
        assert_eq!(
            oxideav_ogg::crc::validate_page_crc(&bytes[p.offset..p.end]),
            Some(true),
            "page at offset {} must carry a valid CRC after the index backfill",
            p.offset
        );
    }
}

#[test]
fn auto_index_skips_stream_with_caller_supplied_index() {
    // A caller-supplied index for the stream's serial passes through
    // verbatim — no placeholder, no auto keypoints.
    let stream = vorbis_stream_fixture();
    let mut skel = skel_with_bone();
    let mut custom = SkelIndex::new(0, 1_000);
    custom.first_sample_time = 7;
    custom.last_sample_time = 9_001;
    custom.keypoints.push(KeyPoint {
        offset: 123,
        timestamp: 456,
    });
    skel.push_index(custom.clone());
    let packets: Vec<(u32, i64, bool)> = (1..=4).map(|i| (0u32, 960 * i, true)).collect();
    let bytes = mux_indexed(vec![stream], skel, permissive_cfg(8), &packets);

    let reader: Box<dyn ReadSeek> = Box::new(Cursor::new(bytes));
    let dmx = oxideav_ogg::demux::open_concrete(reader, &oxideav_core::NullCodecResolver).unwrap();
    let sk = dmx.skeleton().unwrap();
    assert_eq!(sk.indexes.len(), 1, "no second auto-built index");
    assert_eq!(sk.indexes[0], custom);
}

#[test]
fn auto_index_without_keyframes_backfills_empty_index() {
    // No keyframe-flagged packets: the index packet is still emitted
    // (n = 0) with the first/last-sample-time fields filled from the
    // observed pts, and seeking falls back to bisection per the spec's
    // graceful-fallback rule.
    let stream = vorbis_stream_fixture();
    let packets: Vec<(u32, i64, bool)> = (1..=5).map(|i| (0u32, 960 * i, false)).collect();
    let bytes = mux_indexed(vec![stream], skel_with_bone(), permissive_cfg(8), &packets);

    let reader: Box<dyn ReadSeek> = Box::new(Cursor::new(bytes));
    let mut dmx =
        oxideav_ogg::demux::open_concrete(reader, &oxideav_core::NullCodecResolver).unwrap();
    {
        let idx = &dmx.skeleton().unwrap().indexes[0];
        assert!(idx.keypoints.is_empty());
        assert_eq!(idx.first_sample_time, 960);
        assert_eq!(idx.last_sample_time, 4_800);
        assert_eq!(idx.timestamp_denominator, 48_000);
    }
    use oxideav_core::Demuxer as _;
    let landed = dmx.seek_to(0, 2_880).expect("bisection fallback seek");
    assert!(landed <= 2_880);
    assert_eq!(
        dmx.skeleton_index_seek_count(),
        0,
        "empty index has no floor keypoint — fast path must not fire"
    );
}

#[test]
fn auto_index_multi_stream_emits_one_index_per_content_stream() {
    let stream_a = vorbis_stream_fixture();
    let mut stream_b = vorbis_stream_fixture();
    stream_b.index = 1;
    let mut skel = Skeleton::new();
    skel.set_head(FisHead::new(Version::V4_0));
    skel.push_bone(FisBone::new(0, Rational::new(48_000, 1)));
    skel.push_bone(FisBone::new(1, Rational::new(48_000, 1)));

    let mut packets: Vec<(u32, i64, bool)> = Vec::new();
    for i in 1..=3 {
        packets.push((0, 960 * i, true));
        packets.push((1, 960 * i, true));
    }
    let bytes = mux_indexed(vec![stream_a, stream_b], skel, permissive_cfg(8), &packets);

    let pages = walk_pages(&bytes);
    let reader: Box<dyn ReadSeek> = Box::new(Cursor::new(bytes));
    let dmx = oxideav_ogg::demux::open_concrete(reader, &oxideav_core::NullCodecResolver).unwrap();
    let sk = dmx.skeleton().unwrap();
    assert_eq!(sk.indexes.len(), 2);
    for serial in [0u32, 1u32] {
        let idx = sk
            .index_for_serial(serial)
            .unwrap_or_else(|| panic!("index for serial {serial}"));
        assert_eq!(idx.keypoints.len(), 3);
        for kp in &idx.keypoints {
            let page = pages
                .iter()
                .find(|p| p.offset as u64 == kp.offset)
                .expect("keypoint lands on a page boundary");
            assert_eq!(
                page.serial, serial,
                "keypoint offset must start a page of its own stream"
            );
        }
    }
}

#[test]
fn auto_index_rejects_invalid_configurations() {
    // index packets are a Skeleton 4.0 feature — a 3.0 fishead has no
    // segment-length field to validate them against.
    let stream = vorbis_stream_fixture();
    let mut skel = Skeleton::new();
    skel.set_head(FisHead::new(Version::V3_0));
    let writer: Box<dyn WriteSeek> = Box::new(SharedBuf::default());
    assert!(oxideav_ogg::mux::open_with_skeleton_indexed(
        writer,
        std::slice::from_ref(&stream),
        skel,
        permissive_cfg(8),
    )
    .is_err());

    // max_keypoints = 0 reserves nothing to backfill.
    let writer: Box<dyn WriteSeek> = Box::new(SharedBuf::default());
    assert!(oxideav_ogg::mux::open_with_skeleton_indexed(
        writer,
        std::slice::from_ref(&stream),
        skel_with_bone(),
        permissive_cfg(0),
    )
    .is_err());

    // A reservation past the single-page body limit (255×255 bytes)
    // cannot ride on one Skeleton page.
    let writer: Box<dyn WriteSeek> = Box::new(SharedBuf::default());
    assert!(oxideav_ogg::mux::open_with_skeleton_indexed(
        writer,
        std::slice::from_ref(&stream),
        skel_with_bone(),
        permissive_cfg(4_000),
    )
    .is_err());
}

#[test]
fn backfilled_fishead_page_crc_is_valid() {
    // The in-place rewrite recomputes the page CRC over the patched
    // packet (RFC 3533 §6 field 7) — verify with the standalone
    // validator over every page in the output.
    let stream = vorbis_stream_fixture();
    let mut skel = Skeleton::new();
    skel.set_head(FisHead::new(Version::V4_0));
    skel.push_bone(FisBone::new(0, Rational::new(48_000, 1)));

    let bytes = mux_with_skeleton(vec![stream], skel, 3);
    for p in walk_pages(&bytes) {
        assert_eq!(
            oxideav_ogg::crc::validate_page_crc(&bytes[p.offset..p.end]),
            Some(true),
            "page at offset {} must carry a valid CRC after the backfill",
            p.offset
        );
    }
}
