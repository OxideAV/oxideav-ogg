//! Multiplexed (grouped) A/V page layout rules
//! (`docs/container/ogg/rfc3533-ogg.txt` §4 + `docs/video/theora/Theora.pdf`
//! §A.3.2).
//!
//! Two rules the single-page EOS-deferral used to violate, caught by
//! black-box validation of muxed Theora+audio files:
//!
//! 1. "After the 'beginning of stream' pages, the header pages of each
//!    of the logical streams MUST be grouped together before any data
//!    pages occur." — the deferral held each stream's last header page
//!    (e.g. the Vorbis setup page) until that stream's first data
//!    flush, by which time the other stream's data pages were already
//!    on the wire.
//! 2. "the data pages are multiplexed together ... placed in the stream
//!    in increasing order by the time equivalents of their granule
//!    position fields" — pages were released in call order, so a
//!    stream's held-back page could land after another stream's much
//!    later pages.
//!
//! Also pinned: a stream that ends with every page already on the wire
//! closes with an RFC 3533 §4 nil EOS page ("pages containing no
//! content but simply a page header with position information and the
//! eos flag set").

use std::io::Cursor;

use oxideav_core::{
    CodecId, CodecParameters, Muxer, NullCodecResolver, Packet, ReadSeek, StreamInfo, TimeBase,
    WriteSeek,
};
use oxideav_ogg::page::{flags, Page};
use oxideav_ogg::theora::TheoraIdHeader;

fn theora_id() -> TheoraIdHeader {
    TheoraIdHeader {
        vmaj: 3,
        vmin: 2,
        vrev: 1,
        fmbw: 2,
        fmbh: 2,
        picw: 32,
        pich: 32,
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

fn theora_stream(index: u32) -> StreamInfo {
    let mut comment = vec![0x81];
    comment.extend_from_slice(b"theora");
    comment.extend_from_slice(&[0u8; 8]);
    let mut setup = vec![0x82];
    setup.extend_from_slice(b"theora");
    setup.extend_from_slice(&[0u8; 24]);
    let mut params = CodecParameters::video(CodecId::new("theora"));
    params.extradata =
        oxideav_ogg::mux::xiph_lace(&[&theora_id().to_bytes(), &comment, &setup]).unwrap();
    StreamInfo {
        index,
        time_base: TimeBase::new(1, 25),
        duration: None,
        start_time: Some(0),
        params,
    }
}

fn vorbis_stream(index: u32) -> StreamInfo {
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

/// Time equivalent (seconds) of a page granule in this test's fixed
/// two-stream setup: serial 0 = Theora (25 fps, shift 6, frame-count
/// origin), serial 1 = Vorbis (48 kHz sample count).
fn page_secs(p: &Page) -> f64 {
    let g = p.granule_position;
    if p.serial == 0 {
        let count = (g >> 6) + (g & 63);
        count as f64 / 25.0
    } else {
        g as f64 / 48_000.0
    }
}

/// Mux 8 video frames (25 fps) against sparse audio pages (~1 s apart),
/// interleaved in time order like a real A/V feed.
fn mux_av() -> Vec<u8> {
    let streams = vec![theora_stream(0), vorbis_stream(1)];
    let shared = SharedBuf::default();
    let out: Box<dyn WriteSeek> = Box::new(shared.clone());
    let mut mux = oxideav_ogg::mux::open_concrete(out, &streams).unwrap();
    mux.write_header().unwrap();
    // A near-t=0 audio packet first (like an encoder priming packet),
    // then the 8 video frames (0..0.32 s), then audio at 1 s and 2 s.
    let audio = |mux: &mut oxideav_ogg::mux::OggMuxer, samples: i64| {
        let mut a = Packet::new(1, TimeBase::new(1, 48_000), vec![0xAA; 64]);
        a.pts = Some(samples);
        a.flags.unit_boundary = true;
        mux.write_packet(&a).unwrap();
    };
    audio(&mut mux, 480);
    for n in 0..8i64 {
        let mut v = Packet::new(0, TimeBase::new(1, 25), vec![0x40 + n as u8; 24]);
        v.pts = Some(n);
        v.flags.keyframe = n == 0;
        v.flags.unit_boundary = true;
        mux.write_packet(&v).unwrap();
    }
    audio(&mut mux, 48_000);
    audio(&mut mux, 96_000);
    mux.write_trailer().unwrap();
    drop(mux);
    let bytes = shared.0.lock().unwrap().get_ref().clone();
    bytes
}

#[test]
fn all_header_pages_precede_all_data_pages() {
    let pages = pages_of(&mux_av());
    // Count header pages: BOS ×2 + comment/setup ×2 streams = 6, all
    // with granule 0; every data page has granule > 0 here.
    let first_data = pages
        .iter()
        .position(|p| p.granule_position > 0)
        .expect("data pages exist");
    assert_eq!(first_data, 6, "2 BOS + 4 secondary header pages first");
    for (i, p) in pages.iter().enumerate() {
        let is_header = i < first_data;
        assert_eq!(
            p.granule_position == 0,
            is_header,
            "page {i}: header/data sections must not interleave"
        );
    }
    // Both streams' setup pages sit inside the header section: pages
    // 2..6 are comment+setup for both serials.
    let header_serials: Vec<u32> = pages[2..6].iter().map(|p| p.serial).collect();
    assert!(header_serials.contains(&0) && header_serials.contains(&1));
}

#[test]
fn data_pages_are_released_in_increasing_time_order() {
    let pages = pages_of(&mux_av());
    let mut last = 0.0f64;
    for p in pages.iter().filter(|p| p.granule_position > 0) {
        let t = page_secs(p);
        assert!(
            t >= last,
            "page (serial {} granule {}) at {t}s appears after {last}s",
            p.serial,
            p.granule_position
        );
        last = t;
    }
    // And both streams closed with EOS.
    for serial in [0u32, 1] {
        assert!(
            pages.iter().any(|p| p.serial == serial && p.is_last()),
            "serial {serial} needs an EOS page"
        );
    }
}

#[test]
fn stream_with_no_data_packets_closes_with_a_nil_eos_page() {
    let streams = vec![theora_stream(0), vorbis_stream(1)];
    let shared = SharedBuf::default();
    let out: Box<dyn WriteSeek> = Box::new(shared.clone());
    let mut mux = oxideav_ogg::mux::open_concrete(out, &streams).unwrap();
    mux.write_header().unwrap();
    // Only the video stream gets data.
    for n in 0..3i64 {
        let mut v = Packet::new(0, TimeBase::new(1, 25), vec![0x40; 16]);
        v.pts = Some(n);
        v.flags.keyframe = n == 0;
        v.flags.unit_boundary = true;
        mux.write_packet(&v).unwrap();
    }
    mux.write_trailer().unwrap();
    drop(mux);
    let bytes = shared.0.lock().unwrap().get_ref().clone();
    let pages = pages_of(&bytes);
    // The audio stream's header pages all hit the wire during
    // write_header, so its EOS must be a nil page (0 segments).
    let audio_eos = pages
        .iter()
        .find(|p| p.serial == 1 && p.is_last())
        .expect("audio stream still closes with EOS");
    assert_eq!(audio_eos.lacing.len(), 0, "nil page: no segments");
    assert_eq!(audio_eos.data.len(), 0);
    assert!(pages.iter().any(|p| p.serial == 0 && p.is_last()));
    // The whole file still demuxes cleanly.
    let reader: Box<dyn ReadSeek> = Box::new(Cursor::new(bytes));
    let mut dmx = oxideav_ogg::demux::open(reader, &NullCodecResolver).unwrap();
    let mut frames = 0;
    while let Ok(p) = dmx.next_packet() {
        assert_eq!(p.stream_index, 0, "no phantom audio packets");
        frames += 1;
    }
    assert_eq!(frames, 3);
}

#[test]
fn single_stream_layout_is_unchanged_by_the_release_queue() {
    // A lone stream must keep the historical layout: pages in flush
    // order, EOS on the final data page (no nil page).
    let streams = vec![vorbis_stream(0)];
    let shared = SharedBuf::default();
    let out: Box<dyn WriteSeek> = Box::new(shared.clone());
    let mut mux = oxideav_ogg::mux::open_concrete(out, &streams).unwrap();
    mux.write_header().unwrap();
    for i in 1..=3i64 {
        let mut a = Packet::new(0, TimeBase::new(1, 48_000), vec![0xAA; 32]);
        a.pts = Some(960 * i);
        a.flags.unit_boundary = true;
        mux.write_packet(&a).unwrap();
    }
    mux.write_trailer().unwrap();
    drop(mux);
    let pages = pages_of(&shared.0.lock().unwrap().get_ref().clone());
    assert_eq!(pages.len(), 6, "BOS + comment + setup + 3 data pages");
    assert!(pages[0].is_first());
    let granules: Vec<i64> = pages.iter().map(|p| p.granule_position).collect();
    assert_eq!(granules, vec![0, 0, 0, 960, 1920, 2880]);
    assert!(pages[5].is_last(), "EOS rides the last data page");
    assert!(!pages[4].is_last());
    assert_eq!(
        pages
            .iter()
            .filter(|p| p.flags & flags::LAST_PAGE != 0)
            .count(),
        1
    );
}
