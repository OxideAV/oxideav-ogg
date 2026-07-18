//! Muxer CI gate: every physical bitstream the muxer can produce must
//! pass the whole-file RFC 3533 conformance validator
//! (`oxideav_ogg::validate`) with zero issues.
//!
//! The battery spans the muxer's whole configuration space — single
//! streams of every recognised codec mapping, grouped (concurrent) and
//! chained (sequential) multiplexing per RFC 3533 §4, mixed
//! grouping+chaining, Skeleton 3.0/4.0 control bitstreams (including
//! muxer-built 4.0 keyframe indexes, whose header-section placeholder
//! pages are rewritten in place after the content is known), oversize
//! packets that span pages, nil packets, and the soft page-size target.
//!
//! The tail tests prove the gate has teeth: surgically damaged copies
//! of a muxer output must trip the exact rule the damage violates.

use std::io::Cursor;

use oxideav_core::{CodecId, CodecParameters, Muxer, Packet, StreamInfo, TimeBase, WriteSeek};
use oxideav_ogg::mux::{self, AutoIndexConfig};
use oxideav_ogg::skeleton::{FisBone, FisHead, Rational, Skeleton, Version};
use oxideav_ogg::validate::{validate, Rule};

// ---------------------------------------------------------------------
// Shared output buffer (the muxer takes ownership of its WriteSeek).
// ---------------------------------------------------------------------

#[derive(Clone, Default)]
struct SharedBuf(std::sync::Arc<std::sync::Mutex<Cursor<Vec<u8>>>>);

impl SharedBuf {
    fn bytes(&self) -> Vec<u8> {
        self.0.lock().unwrap().get_ref().clone()
    }
}

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

// ---------------------------------------------------------------------
// Codec-mapping header builders (minimal valid identification headers;
// the container layer only sniffs them).
// ---------------------------------------------------------------------

fn vorbis_extradata() -> Vec<u8> {
    let mut id = vec![0x01];
    id.extend_from_slice(b"vorbis");
    id.extend_from_slice(&0u32.to_le_bytes());
    id.push(2);
    id.extend_from_slice(&48_000u32.to_le_bytes());
    id.extend_from_slice(&[0; 12]);
    id.extend_from_slice(&[0xB8, 0x01]);
    let mut com = vec![0x03];
    com.extend_from_slice(b"vorbis");
    com.extend_from_slice(&[0, 0, 0, 0, 0, 0, 0, 0, 1]);
    let mut setup = vec![0x05];
    setup.extend_from_slice(b"vorbis");
    setup.extend_from_slice(&[0; 32]);
    mux::xiph_lace(&[&id, &com, &setup]).unwrap()
}

fn opus_extradata() -> Vec<u8> {
    let mut head = Vec::with_capacity(19);
    head.extend_from_slice(b"OpusHead");
    head.push(1); // version
    head.push(2); // channels
    head.extend_from_slice(&312u16.to_le_bytes()); // pre-skip
    head.extend_from_slice(&48_000u32.to_le_bytes()); // input sample rate
    head.extend_from_slice(&0i16.to_le_bytes()); // output gain
    head.push(0); // mapping family
    head
}

fn speex_extradata() -> Vec<u8> {
    // Speex header: "Speex   " + version string area + fixed fields;
    // only the leading magic is sniffed by the container layer.
    let mut h = Vec::with_capacity(80);
    h.extend_from_slice(b"Speex   ");
    h.resize(80, 0);
    h
}

fn flac_extradata() -> Vec<u8> {
    // FLAC-in-Ogg identification packet: 0x7F "FLAC" major minor,
    // header-count, "fLaC", then a STREAMINFO block.
    let mut h = vec![0x7F];
    h.extend_from_slice(b"FLAC");
    h.extend_from_slice(&[1, 0]); // mapping version 1.0
    h.extend_from_slice(&0u16.to_be_bytes()); // trailing header packets
    h.extend_from_slice(b"fLaC");
    h.extend_from_slice(&[0x80, 0, 0, 34]); // last-block STREAMINFO header
    h.extend_from_slice(&[0u8; 34]);
    h
}

fn theora_extradata() -> Vec<u8> {
    let id = oxideav_ogg::theora::TheoraIdHeader {
        vmaj: 3,
        vmin: 2,
        vrev: 1,
        fmbw: 20,
        fmbh: 15,
        picw: 320,
        pich: 240,
        picx: 0,
        picy: 0,
        frn: 25,
        frd: 1,
        parn: 0,
        pard: 0,
        cs: 0,
        nombr: 0,
        qual: 40,
        kfgshift: 6,
        pf: 0,
    };
    let mut com = vec![0x81];
    com.extend_from_slice(b"theora");
    com.extend_from_slice(&0u32.to_le_bytes());
    com.extend_from_slice(&0u32.to_le_bytes());
    let mut setup = vec![0x82];
    setup.extend_from_slice(b"theora");
    setup.extend_from_slice(&[0u8; 24]);
    mux::xiph_lace(&[&id.to_bytes(), &com, &setup]).unwrap()
}

fn audio_stream(index: u32, codec: &str, extradata: Vec<u8>, rate: i64) -> StreamInfo {
    let mut params = CodecParameters::audio(CodecId::new(codec));
    params.channels = Some(2);
    params.sample_rate = Some(rate as u32);
    params.extradata = extradata;
    StreamInfo {
        index,
        time_base: TimeBase::new(1, rate),
        duration: None,
        start_time: Some(0),
        params,
    }
}

fn video_stream(index: u32) -> StreamInfo {
    let mut params = CodecParameters::video(CodecId::new("theora"));
    params.extradata = theora_extradata();
    StreamInfo {
        index,
        time_base: TimeBase::new(1, 25),
        duration: None,
        start_time: Some(0),
        params,
    }
}

fn audio_packet(stream: &StreamInfo, i: i64, len: usize) -> Packet {
    let mut pkt = Packet::new(stream.index, stream.time_base, vec![(i & 0x7f) as u8; len]);
    pkt.pts = Some(960 * i);
    pkt.dts = pkt.pts;
    pkt.flags.keyframe = true;
    pkt.flags.unit_boundary = true;
    pkt
}

fn video_packet(stream: &StreamInfo, frame: i64, keyframe: bool) -> Packet {
    // Theora data packets: MSB of byte 0 clear.
    let mut pkt = Packet::new(stream.index, stream.time_base, vec![0x00, frame as u8, 42]);
    pkt.pts = Some(frame);
    pkt.dts = pkt.pts;
    pkt.flags.keyframe = keyframe;
    pkt.flags.unit_boundary = true;
    pkt
}

/// Mux one link of audio packets and return the physical bitstream.
fn mux_single(stream: StreamInfo, packets: usize, packet_len: usize) -> Vec<u8> {
    let shared = SharedBuf::default();
    let out: Box<dyn WriteSeek> = Box::new(shared.clone());
    let mut muxer = mux::open(out, std::slice::from_ref(&stream)).unwrap();
    muxer.write_header().unwrap();
    for i in 1..=packets as i64 {
        muxer
            .write_packet(&audio_packet(&stream, i, packet_len))
            .unwrap();
    }
    muxer.write_trailer().unwrap();
    drop(muxer);
    shared.bytes()
}

/// Assert the validator finds a fully conformant file.
#[track_caller]
fn assert_clean(bytes: &[u8], what: &str) {
    let report = validate(bytes);
    assert!(
        report.is_clean(),
        "muxer output for {what} is not conformant:\n{report}"
    );
    assert!(report.pages > 0, "{what}: no pages were produced");
}

// ---------------------------------------------------------------------
// The gate: every muxer configuration validates clean.
// ---------------------------------------------------------------------

#[test]
fn single_vorbis_stream_validates_clean() {
    let bytes = mux_single(
        audio_stream(0, "vorbis", vorbis_extradata(), 48_000),
        12,
        64,
    );
    assert_clean(&bytes, "a single Vorbis stream");
}

#[test]
fn single_opus_stream_validates_clean() {
    let bytes = mux_single(audio_stream(0, "opus", opus_extradata(), 48_000), 12, 40);
    assert_clean(&bytes, "a single Opus stream");
}

#[test]
fn single_speex_stream_validates_clean() {
    let bytes = mux_single(audio_stream(0, "speex", speex_extradata(), 32_000), 8, 50);
    assert_clean(&bytes, "a single Speex stream");
}

#[test]
fn single_flac_stream_validates_clean() {
    let bytes = mux_single(audio_stream(0, "flac", flac_extradata(), 44_100), 8, 90);
    assert_clean(&bytes, "a single FLAC stream");
}

#[test]
fn nil_packets_validate_clean() {
    // RFC 3533 §5: "a 'nil' (zero length) packet is not an error".
    let stream = audio_stream(0, "vorbis", vorbis_extradata(), 48_000);
    let shared = SharedBuf::default();
    let out: Box<dyn WriteSeek> = Box::new(shared.clone());
    let mut muxer = mux::open(out, std::slice::from_ref(&stream)).unwrap();
    muxer.write_header().unwrap();
    for i in 1..=4i64 {
        muxer.write_packet(&audio_packet(&stream, i, 0)).unwrap();
    }
    muxer.write_trailer().unwrap();
    drop(muxer);
    assert_clean(&shared.bytes(), "nil (zero-length) packets");
}

#[test]
fn oversize_packets_spanning_pages_validate_clean() {
    // 70000-byte and 65025-byte (exact 255-segment fill) packets force
    // continued pages and -1 granules on the spanned pages.
    let stream = audio_stream(0, "vorbis", vorbis_extradata(), 48_000);
    let shared = SharedBuf::default();
    let out: Box<dyn WriteSeek> = Box::new(shared.clone());
    let mut muxer = mux::open(out, std::slice::from_ref(&stream)).unwrap();
    muxer.write_header().unwrap();
    muxer
        .write_packet(&audio_packet(&stream, 1, 70_000))
        .unwrap();
    muxer
        .write_packet(&audio_packet(&stream, 2, 255 * 255))
        .unwrap();
    muxer.write_packet(&audio_packet(&stream, 3, 510)).unwrap();
    muxer.write_trailer().unwrap();
    drop(muxer);
    assert_clean(&shared.bytes(), "oversize packets spanning pages");
}

#[test]
fn page_size_target_validates_clean() {
    let stream = audio_stream(0, "vorbis", vorbis_extradata(), 48_000);
    let shared = SharedBuf::default();
    let out: Box<dyn WriteSeek> = Box::new(shared.clone());
    let mut muxer = mux::open_concrete(out, std::slice::from_ref(&stream)).unwrap();
    muxer.set_page_target_bytes(Some(1024));
    muxer.write_header().unwrap();
    for i in 1..=60i64 {
        muxer.write_packet(&audio_packet(&stream, i, 400)).unwrap();
    }
    muxer.write_trailer().unwrap();
    drop(muxer);
    assert_clean(&shared.bytes(), "a 1 KiB soft page-size target");
}

#[test]
fn grouped_theora_vorbis_validates_clean() {
    let video = video_stream(0);
    let audio = audio_stream(1, "vorbis", vorbis_extradata(), 48_000);
    let streams = vec![video.clone(), audio.clone()];
    let shared = SharedBuf::default();
    let out: Box<dyn WriteSeek> = Box::new(shared.clone());
    let mut muxer = mux::open(out, &streams).unwrap();
    muxer.write_header().unwrap();
    for i in 0..25i64 {
        muxer
            .write_packet(&video_packet(&video, i, i % 8 == 0))
            .unwrap();
        muxer
            .write_packet(&audio_packet(&audio, i + 1, 120))
            .unwrap();
    }
    muxer.write_trailer().unwrap();
    drop(muxer);
    assert_clean(&shared.bytes(), "grouped Theora+Vorbis");
}

#[test]
fn chained_links_validate_clean() {
    // Three sequential links (RFC 3533 §4 chaining) through
    // begin_new_link, with distinct codecs per link.
    let shared = SharedBuf::default();
    let out: Box<dyn WriteSeek> = Box::new(shared.clone());
    let link0 = audio_stream(0, "vorbis", vorbis_extradata(), 48_000);
    let mut muxer = mux::open_concrete(out, std::slice::from_ref(&link0)).unwrap();
    muxer.write_header().unwrap();
    for i in 1..=5i64 {
        muxer.write_packet(&audio_packet(&link0, i, 64)).unwrap();
    }
    let link1 = audio_stream(0, "opus", opus_extradata(), 48_000);
    muxer.begin_new_link(std::slice::from_ref(&link1)).unwrap();
    for i in 1..=5i64 {
        muxer.write_packet(&audio_packet(&link1, i, 48)).unwrap();
    }
    let link2 = audio_stream(0, "speex", speex_extradata(), 32_000);
    muxer.begin_new_link(std::slice::from_ref(&link2)).unwrap();
    for i in 1..=5i64 {
        muxer.write_packet(&audio_packet(&link2, i, 32)).unwrap();
    }
    muxer.write_trailer().unwrap();
    drop(muxer);
    let bytes = shared.bytes();
    let report = validate(&bytes);
    assert!(report.is_clean(), "chained links:\n{report}");
    assert_eq!(report.links, 3, "three chain links expected:\n{report}");
    assert_eq!(report.streams, 3);
}

#[test]
fn mixed_grouping_and_chaining_validates_clean() {
    // Link 0 groups video+audio; link 1 is a lone audio stream — the
    // RFC 3533 §4 "consecutively chain groups of concurrently
    // multiplexed bitstreams" diagram.
    let shared = SharedBuf::default();
    let out: Box<dyn WriteSeek> = Box::new(shared.clone());
    let video = video_stream(0);
    let audio = audio_stream(1, "vorbis", vorbis_extradata(), 48_000);
    let streams = vec![video.clone(), audio.clone()];
    let mut muxer = mux::open_concrete(out, &streams).unwrap();
    muxer.write_header().unwrap();
    for i in 0..10i64 {
        muxer
            .write_packet(&video_packet(&video, i, i % 4 == 0))
            .unwrap();
        muxer
            .write_packet(&audio_packet(&audio, i + 1, 100))
            .unwrap();
    }
    let link1 = audio_stream(0, "opus", opus_extradata(), 48_000);
    muxer.begin_new_link(std::slice::from_ref(&link1)).unwrap();
    for i in 1..=6i64 {
        muxer.write_packet(&audio_packet(&link1, i, 44)).unwrap();
    }
    muxer.write_trailer().unwrap();
    drop(muxer);
    let bytes = shared.bytes();
    let report = validate(&bytes);
    assert!(report.is_clean(), "mixed grouping+chaining:\n{report}");
    assert_eq!(report.links, 2, "two chain links expected:\n{report}");
    assert_eq!(report.streams, 3);
}

fn skeleton_mux(version: Version, indexed: bool) -> Vec<u8> {
    let stream = audio_stream(0, "vorbis", vorbis_extradata(), 48_000);
    let mut skel = Skeleton::new();
    skel.set_head(FisHead::new(version));
    skel.push_bone(FisBone::new(0, Rational::new(48_000, 1)));
    let shared = SharedBuf::default();
    let out: Box<dyn WriteSeek> = Box::new(shared.clone());
    let mut muxer = if indexed {
        mux::open_with_skeleton_indexed(
            out,
            std::slice::from_ref(&stream),
            skel,
            AutoIndexConfig {
                max_keypoints: 16,
                min_keypoint_byte_gap: 1,
                min_keypoint_time_gap_ms: 1,
            },
        )
        .unwrap()
    } else {
        mux::open_with_skeleton(out, std::slice::from_ref(&stream), Some(skel)).unwrap()
    };
    muxer.write_header().unwrap();
    for i in 1..=10i64 {
        muxer.write_packet(&audio_packet(&stream, i, 200)).unwrap();
    }
    muxer.write_trailer().unwrap();
    drop(muxer);
    shared.bytes()
}

#[test]
fn skeleton_3_0_validates_clean() {
    assert_clean(&skeleton_mux(Version::V3_0, false), "a Skeleton 3.0 file");
}

#[test]
fn skeleton_4_0_validates_clean() {
    assert_clean(&skeleton_mux(Version::V4_0, false), "a Skeleton 4.0 file");
}

#[test]
fn skeleton_4_0_with_auto_index_validates_clean() {
    // The auto-index path rewrites the header-section placeholder
    // `index\0` pages in place after write_trailer; the patched pages
    // must still CRC-verify and obey every structure rule.
    assert_clean(
        &skeleton_mux(Version::V4_0, true),
        "a Skeleton 4.0 file with muxer-built keyframe indexes",
    );
}

// ---------------------------------------------------------------------
// The gate has teeth: surgical damage trips the exact rule.
// ---------------------------------------------------------------------

#[test]
fn gate_detects_crc_damage() {
    let mut bytes = mux_single(audio_stream(0, "vorbis", vorbis_extradata(), 48_000), 6, 64);
    let mid = bytes.len() / 2;
    bytes[mid] ^= 0x01;
    let report = validate(&bytes);
    assert!(
        !report.is_clean(),
        "single-bit damage must not pass the gate"
    );
    assert!(
        report.has(Rule::CrcMismatch)
            || report.has(Rule::CapturePattern)
            || report.has(Rule::Truncated),
        "damage must surface as a page-integrity rule:\n{report}"
    );
}

#[test]
fn gate_detects_truncation() {
    let bytes = mux_single(audio_stream(0, "vorbis", vorbis_extradata(), 48_000), 6, 64);
    let cut = &bytes[..bytes.len() - 7];
    let report = validate(cut);
    assert!(report.has(Rule::Truncated), "truncated tail:\n{report}");
    assert!(report.has(Rule::MissingEos), "lost EOS page:\n{report}");
}

#[test]
fn gate_detects_dropped_page() {
    // Remove a whole middle page: sequence gap on its stream.
    let bytes = mux_single(audio_stream(0, "vorbis", vorbis_extradata(), 48_000), 8, 64);
    // Walk page extents.
    let mut offs = Vec::new();
    let mut pos = 0usize;
    while pos + 27 <= bytes.len() {
        assert_eq!(&bytes[pos..pos + 4], b"OggS");
        let n_segs = bytes[pos + 26] as usize;
        let body: usize = bytes[pos + 27..pos + 27 + n_segs]
            .iter()
            .map(|&v| v as usize)
            .sum();
        let total = 27 + n_segs + body;
        offs.push((pos, total));
        pos += total;
    }
    assert!(offs.len() >= 4, "need enough pages to drop one");
    let (drop_at, drop_len) = offs[offs.len() - 3];
    let mut damaged = bytes[..drop_at].to_vec();
    damaged.extend_from_slice(&bytes[drop_at + drop_len..]);
    let report = validate(&damaged);
    assert!(report.has(Rule::SequenceGap), "dropped page:\n{report}");
}
