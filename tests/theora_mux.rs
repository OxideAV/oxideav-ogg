//! Theora-in-Ogg MUX conformance (`docs/video/theora/Theora.pdf`
//! Appendix A + `docs/container/ogg/rfc3533-ogg.txt`).
//!
//! The muxer receives codec-level packets — 0-based frame-index pts and
//! keyframe flags, exactly what the demuxer emits and what a Theora
//! encoder produces — and must lay them out per the spec's Ogg mapping:
//!
//! * §A.2.1: the identification header alone on the BOS page; the
//!   comment header beginning the second page; a page break between the
//!   last header packet and the first frame data packet; granule 0 on
//!   all header pages;
//! * §A.2.2: a granule marking the last frame that finishes on each
//!   data page; EOS on the final page;
//! * §A.2.3: the `(keyframe << KFGSHIFT) | frames-since-keyframe`
//!   split granule packing with the frame-count origin (VREV ≥ 1);
//! * §A.3.2: among the grouped BOS pages "the first page to occur MUST
//!   be the Theora page" when audio streams are multiplexed alongside.

use std::io::Cursor;

use oxideav_core::{
    CodecId, CodecParameters, Demuxer, Muxer, NullCodecResolver, Packet, ReadSeek, StreamInfo,
    TimeBase, WriteSeek,
};
use oxideav_ogg::page::Page;
use oxideav_ogg::theora::TheoraIdHeader;

fn id_header() -> TheoraIdHeader {
    TheoraIdHeader {
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
    }
}

fn comment_packet() -> Vec<u8> {
    let mut p = vec![0x81];
    p.extend_from_slice(b"theora");
    p.extend_from_slice(&0u32.to_le_bytes());
    p.extend_from_slice(&0u32.to_le_bytes());
    p
}

fn setup_packet() -> Vec<u8> {
    let mut p = vec![0x82];
    p.extend_from_slice(b"theora");
    p.extend_from_slice(&[0u8; 24]);
    p
}

fn theora_stream(index: u32) -> StreamInfo {
    let mut params = CodecParameters::video(CodecId::new("theora"));
    params.extradata =
        oxideav_ogg::mux::xiph_lace(&[&id_header().to_bytes(), &comment_packet(), &setup_packet()])
            .unwrap();
    StreamInfo {
        index,
        time_base: TimeBase::new(1, 25),
        duration: None,
        start_time: Some(0),
        params,
    }
}

fn vorbis_stream(index: u32) -> StreamInfo {
    // Minimal valid Vorbis header trio (only the ID header is parsed by
    // the container layer).
    let mut id = vec![0x01];
    id.extend_from_slice(b"vorbis");
    id.extend_from_slice(&0u32.to_le_bytes());
    id.push(2);
    id.extend_from_slice(&48_000u32.to_le_bytes());
    id.extend_from_slice(&[0; 12]);
    id.extend_from_slice(&[0xB8, 0x01]);
    let mut comment = vec![0x03];
    comment.extend_from_slice(b"vorbis");
    comment.extend_from_slice(&[0, 0, 0, 0, 0, 0, 0, 0, 1]);
    let mut setup = vec![0x05];
    setup.extend_from_slice(b"vorbis");
    setup.extend_from_slice(&[0; 16]);
    let mut params = CodecParameters::audio(CodecId::new("vorbis"));
    params.channels = Some(2);
    params.sample_rate = Some(48_000);
    params.extradata = oxideav_ogg::mux::xiph_lace(&[&id, &comment, &setup]).unwrap();
    StreamInfo {
        index,
        time_base: TimeBase::new(1, 48_000),
        duration: None,
        start_time: Some(0),
        params,
    }
}

/// Split a byte buffer back into raw pages.
fn pages_of(bytes: &[u8]) -> Vec<Page> {
    let mut out = Vec::new();
    let mut off = 0;
    while off < bytes.len() {
        let (page, used) = Page::parse(&bytes[off..]).expect("valid page");
        out.push(page);
        off += used;
    }
    out
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
impl SharedBuf {
    fn bytes(&self) -> Vec<u8> {
        self.0.lock().unwrap().get_ref().clone()
    }
}

/// Mux 8 frames (keyframes at 0 and 5, one page per frame) and return
/// the output bytes.
fn mux_8_frames() -> Vec<u8> {
    let stream = theora_stream(0);
    let shared = SharedBuf::default();
    let out: Box<dyn WriteSeek> = Box::new(shared.clone());
    let mut mux = oxideav_ogg::mux::open_concrete(out, std::slice::from_ref(&stream)).unwrap();
    mux.write_header().unwrap();
    for n in 0..8i64 {
        let mut pkt = Packet::new(0, TimeBase::new(1, 25), vec![0x40 + n as u8; 24]);
        pkt.pts = Some(n);
        pkt.dts = Some(n);
        pkt.flags.keyframe = n == 0 || n == 5;
        pkt.flags.unit_boundary = true;
        mux.write_packet(&pkt).unwrap();
    }
    mux.write_trailer().unwrap();
    drop(mux);
    shared.bytes()
}

#[test]
fn header_pagination_follows_the_encapsulation_rules() {
    let bytes = mux_8_frames();
    let pages = pages_of(&bytes);
    // §A.2.1: BOS page carries exactly the ID header, alone, granule 0.
    assert!(pages[0].is_first(), "first page has BOS set");
    assert_eq!(pages[0].granule_position, 0);
    assert_eq!(pages[0].packet_segments().len(), 1);
    assert_eq!(pages[0].data, id_header().to_bytes());
    // §A.2.1: the comment header begins the second page (granule 0).
    assert_eq!(pages[1].granule_position, 0);
    assert!(pages[1].data.starts_with(&[0x81]), "comment packet second");
    // A page break separates the last header packet from the first data
    // packet: the setup packet's page must not carry frame data.
    assert!(pages[2].data.starts_with(&[0x82]), "setup packet third");
    assert_eq!(pages[2].granule_position, 0);
    // First data page starts a fresh page.
    assert!(
        pages[3].data.starts_with(&[0x40]),
        "frame 0 on its own page"
    );
    // §A.2.2: EOS on the final page.
    assert!(pages.last().unwrap().is_last(), "EOS flag on final page");
}

#[test]
fn wire_granules_use_the_split_packing() {
    let g = id_header().granule();
    let bytes = mux_8_frames();
    let pages = pages_of(&bytes);
    // Pages 3.. are the 8 single-frame data pages; their granules must
    // be the packed (keyframe, offset) values with the frame-count
    // origin: 1|0, 1|1 … then 6|0, 6|1 … after the keyframe at frame 5.
    let granules: Vec<i64> = pages[3..].iter().map(|p| p.granule_position).collect();
    let expected: Vec<i64> = (0..8i64)
        .map(|n| {
            let kf = if n >= 5 { 5 } else { 0 };
            g.pack(n, kf).unwrap()
        })
        .collect();
    assert_eq!(granules, expected);
    // Spot-check the raw arithmetic ((1<<6)|n then (6<<6)|(n-5)).
    assert_eq!(granules[0], 64);
    assert_eq!(granules[4], 68);
    assert_eq!(granules[5], 384);
    assert_eq!(granules[7], 386);
}

#[test]
fn mux_demux_round_trip_preserves_frame_indices_and_keyframes() {
    let bytes = mux_8_frames();
    let reader: Box<dyn ReadSeek> = Box::new(Cursor::new(bytes));
    let mut dmx = oxideav_ogg::demux::open_concrete(reader, &NullCodecResolver).unwrap();
    assert_eq!(dmx.streams()[0].params.codec_id.as_str(), "theora");
    assert_eq!(dmx.streams()[0].params.width, Some(320));
    let mut got = Vec::new();
    while let Ok(p) = Demuxer::next_packet(&mut dmx) {
        got.push((p.pts, p.flags.keyframe));
    }
    let expected: Vec<(Option<i64>, bool)> =
        (0..8i64).map(|n| (Some(n), n == 0 || n == 5)).collect();
    assert_eq!(got, expected);
    assert_eq!(
        Demuxer::duration_micros(&dmx),
        Some(320_000),
        "8 frames / 25 fps survive the round trip"
    );
}

#[test]
fn frames_without_pts_use_the_running_frame_counter() {
    // An encoder that only sets keyframe flags (no pts) still gets
    // correct granules: each packet is one frame.
    let g = id_header().granule();
    let stream = theora_stream(0);
    let shared = SharedBuf::default();
    let out: Box<dyn WriteSeek> = Box::new(shared.clone());
    let mut mux = oxideav_ogg::mux::open_concrete(out, std::slice::from_ref(&stream)).unwrap();
    mux.write_header().unwrap();
    for n in 0..4i64 {
        let mut pkt = Packet::new(0, TimeBase::new(1, 25), vec![0x11; 16]);
        pkt.flags.keyframe = n == 0 || n == 2;
        pkt.flags.unit_boundary = true;
        mux.write_packet(&pkt).unwrap();
    }
    mux.write_trailer().unwrap();
    drop(mux);
    let pages = pages_of(&shared.bytes());
    let granules: Vec<i64> = pages[3..].iter().map(|p| p.granule_position).collect();
    assert_eq!(
        granules,
        vec![
            g.pack(0, 0).unwrap(),
            g.pack(1, 0).unwrap(),
            g.pack(2, 2).unwrap(),
            g.pack(3, 2).unwrap(),
        ]
    );
}

#[test]
fn keyframe_interval_overflowing_kfgshift_is_rejected() {
    // KFGSHIFT bounds the representable frames-since-keyframe; a caller
    // that never flags a keyframe within 2^shift frames cannot be muxed
    // conformantly and must get an error rather than a corrupt granule.
    let mut id = id_header();
    id.kfgshift = 2; // max offset 3
    let mut stream = theora_stream(0);
    stream.params.extradata =
        oxideav_ogg::mux::xiph_lace(&[&id.to_bytes(), &comment_packet(), &setup_packet()]).unwrap();
    let out: Box<dyn WriteSeek> = Box::new(SharedBuf::default());
    let mut mux = oxideav_ogg::mux::open_concrete(out, std::slice::from_ref(&stream)).unwrap();
    mux.write_header().unwrap();
    let mut err = None;
    for n in 0..8i64 {
        let mut pkt = Packet::new(0, TimeBase::new(1, 25), vec![0x11; 16]);
        pkt.pts = Some(n);
        pkt.flags.keyframe = n == 0;
        pkt.flags.unit_boundary = true;
        if let Err(e) = mux.write_packet(&pkt) {
            err = Some((n, e));
            break;
        }
    }
    let (n, e) = err.expect("offset overflow must surface");
    assert_eq!(n, 4, "frames 0..=3 fit (offsets 0..=3); frame 4 overflows");
    assert!(
        format!("{e}").contains("KFGSHIFT"),
        "actionable message: {e}"
    );
}

#[test]
fn theora_bos_page_precedes_audio_bos_regardless_of_stream_order() {
    // §A.3.2: "the first page to occur MUST be the Theora page" — even
    // when the caller lists the audio stream first.
    let streams = vec![vorbis_stream(0), theora_stream(1)];
    let shared = SharedBuf::default();
    let out: Box<dyn WriteSeek> = Box::new(shared.clone());
    let mut mux = oxideav_ogg::mux::open_concrete(out, &streams).unwrap();
    mux.write_header().unwrap();
    // One data packet per stream so the trailer has pages to close.
    let mut v = Packet::new(1, TimeBase::new(1, 25), vec![0x40; 16]);
    v.pts = Some(0);
    v.flags.keyframe = true;
    v.flags.unit_boundary = true;
    mux.write_packet(&v).unwrap();
    let mut a = Packet::new(0, TimeBase::new(1, 48_000), vec![0xAA; 32]);
    a.pts = Some(1920);
    a.flags.unit_boundary = true;
    mux.write_packet(&a).unwrap();
    mux.write_trailer().unwrap();
    drop(mux);
    let bytes = shared.bytes();
    let pages = pages_of(&bytes);
    assert!(
        pages[0].is_first() && pages[1].is_first(),
        "grouped BOS section"
    );
    assert!(
        pages[0].data.starts_with(b"\x80theora"),
        "Theora identification page is the physical stream's first page"
    );
    assert!(pages[1].data.starts_with(b"\x01vorbis"));
    // RFC 3533 §6 / spec §A.3.2: all header pages precede all data pages.
    let first_data = pages
        .iter()
        .position(|p| p.granule_position > 0)
        .expect("data pages exist");
    for p in &pages[..first_data] {
        assert!(p.granule_position <= 0);
    }
    // Round-trip: the demuxer sees both streams, video first.
    let reader: Box<dyn ReadSeek> = Box::new(Cursor::new(bytes));
    let dmx = oxideav_ogg::demux::open_concrete(reader, &NullCodecResolver).unwrap();
    assert_eq!(dmx.streams().len(), 2);
    assert_eq!(dmx.streams()[0].params.codec_id.as_str(), "theora");
    assert_eq!(dmx.streams()[1].params.codec_id.as_str(), "vorbis");
}

#[test]
fn demux_mux_demux_round_trip_is_loss_free() {
    // Full symmetry: demux the muxed file, feed the packets straight
    // back into a fresh muxer using the demuxer-reconstructed
    // extradata, demux again — frame indices, keyframe flags and
    // duration must survive both hops.
    let first = mux_8_frames();
    let reader: Box<dyn ReadSeek> = Box::new(Cursor::new(first));
    let mut dmx = oxideav_ogg::demux::open_concrete(reader, &NullCodecResolver).unwrap();
    let mut rebuilt = StreamInfo {
        index: 0,
        time_base: dmx.streams()[0].time_base,
        duration: None,
        start_time: Some(0),
        params: CodecParameters::video(CodecId::new("theora")),
    };
    rebuilt.params.extradata = dmx.streams()[0].params.extradata.clone();
    let mut packets = Vec::new();
    while let Ok(p) = Demuxer::next_packet(&mut dmx) {
        packets.push(p);
    }

    let shared = SharedBuf::default();
    let out: Box<dyn WriteSeek> = Box::new(shared.clone());
    let mut mux = oxideav_ogg::mux::open_concrete(out, std::slice::from_ref(&rebuilt)).unwrap();
    mux.write_header().unwrap();
    for p in &packets {
        mux.write_packet(p).unwrap();
    }
    mux.write_trailer().unwrap();
    drop(mux);
    let second = shared.bytes();

    let reader: Box<dyn ReadSeek> = Box::new(Cursor::new(second));
    let mut dmx2 = oxideav_ogg::demux::open_concrete(reader, &NullCodecResolver).unwrap();
    let mut got = Vec::new();
    while let Ok(p) = Demuxer::next_packet(&mut dmx2) {
        got.push((p.pts, p.flags.keyframe, p.data));
    }
    let expected: Vec<_> = packets
        .into_iter()
        .map(|p| (p.pts, p.flags.keyframe, p.data))
        .collect();
    assert_eq!(got, expected);
    assert_eq!(Demuxer::duration_micros(&dmx2), Some(320_000));
}
