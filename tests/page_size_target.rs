//! Muxer soft page-size target (`OggMuxer::set_page_target_bytes`):
//! data packets written WITHOUT `unit_boundary` flags must still land
//! on RFC-band pages when a target is set, the historical no-target
//! default must be unchanged, and the resulting file must round-trip
//! through the demuxer.
//!
//! The policy matters beyond politeness: black-box testing of the
//! staged Vorbis fixtures with an independent reference decoder
//! showed that a stream whose first
//! audio-bearing page is also its EOS page decodes short by
//! `blocksize0 / 2` samples, while any ≥2-audio-page split decodes to
//! the full declared length. A page target guarantees the split for
//! any stream longer than the target.

use std::io::Cursor;

use oxideav_core::{CodecId, CodecParameters, Packet, StreamInfo, TimeBase};
use oxideav_core::{Muxer, NullCodecResolver, ReadSeek, WriteSeek};
use oxideav_ogg::framing::parse_pages;

fn vorbis_id_packet(channels: u8, sample_rate: u32) -> Vec<u8> {
    let mut p = Vec::with_capacity(30);
    p.push(0x01);
    p.extend_from_slice(b"vorbis");
    p.extend_from_slice(&0u32.to_le_bytes());
    p.push(channels);
    p.extend_from_slice(&sample_rate.to_le_bytes());
    p.extend_from_slice(&0i32.to_le_bytes());
    p.extend_from_slice(&128000i32.to_le_bytes());
    p.extend_from_slice(&0i32.to_le_bytes());
    p.push(0xB8);
    p.push(0x01);
    p
}

fn vorbis_stream() -> StreamInfo {
    let id = vorbis_id_packet(1, 44_100);
    let comment = {
        let mut p = vec![0x03];
        p.extend_from_slice(b"vorbis");
        p.extend_from_slice(&[0, 0, 0, 0, 0, 0, 0, 0, 1]);
        p
    };
    let setup = {
        let mut p = vec![0x05];
        p.extend_from_slice(b"vorbis");
        p.extend_from_slice(&[0u8; 24]);
        p
    };
    let mut params = CodecParameters::audio(CodecId::new("vorbis"));
    params.channels = Some(1);
    params.sample_rate = Some(44_100);
    params.extradata = oxideav_ogg::mux::xiph_lace(&[&id, &comment, &setup]).unwrap();
    StreamInfo {
        index: 0,
        time_base: TimeBase::new(1, 44_100),
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

/// Mux 100 × 600-byte packets with no `unit_boundary` flags, with an
/// optional page target, and return the wire bytes.
fn mux_without_boundaries(target: Option<usize>) -> Vec<u8> {
    let stream = vorbis_stream();
    let shared = SharedBuf::default();
    let out: Box<dyn WriteSeek> = Box::new(shared.clone());
    let mut mux = oxideav_ogg::mux::open_concrete(out, std::slice::from_ref(&stream)).unwrap();
    mux.set_page_target_bytes(target);
    mux.write_header().unwrap();
    for i in 1..=100i64 {
        let mut pkt = Packet::new(0, stream.time_base, vec![(i & 0x7f) as u8 & 0xFE; 600]);
        pkt.pts = Some(512 * i);
        mux.write_packet(&pkt).unwrap();
    }
    mux.write_trailer().unwrap();
    drop(mux);
    let guard = shared.0.lock().unwrap();
    guard.get_ref().clone()
}

#[test]
fn page_target_paginates_data_written_without_unit_boundaries() {
    let bytes = mux_without_boundaries(Some(4096));
    let pages = parse_pages(&bytes).expect("pages parse");
    // Header section: BOS id page + comment/setup page(s) at granule 0.
    // Data pages follow; all but the last must sit in the target band.
    let data_pages: Vec<_> = pages.iter().filter(|p| p.granule_position > 0).collect();
    assert!(
        data_pages.len() >= 10,
        "target must split 60 kB of packets into many pages (got {})",
        data_pages.len()
    );
    for (i, page) in data_pages.iter().enumerate() {
        if i + 1 < data_pages.len() {
            assert!(
                (4096..4096 + 600).contains(&page.data.len()),
                "data page {i} body {} outside the target band",
                page.data.len()
            );
        }
    }
    // Granules are non-decreasing and the final page carries the last pts.
    let grans: Vec<i64> = data_pages.iter().map(|p| p.granule_position).collect();
    assert!(grans.windows(2).all(|w| w[0] <= w[1]));
    assert_eq!(*grans.last().unwrap(), 512 * 100);
    // The EOS page is NOT the first audio-bearing page (the layout the
    // black-box reference-decoder run flagged as lossy).
    assert!(pages.last().unwrap().is_last());
    assert!(
        data_pages.len() >= 2,
        "EOS page must not be the first audio page"
    );

    // Round-trip: the demuxer returns all 100 packets.
    let reader: Box<dyn ReadSeek> = Box::new(Cursor::new(bytes));
    let mut demux = oxideav_ogg::demux::open(reader, &NullCodecResolver).unwrap();
    let mut count = 0;
    while demux.next_packet().is_ok() {
        count += 1;
    }
    assert_eq!(count, 100);
}

#[test]
fn no_target_keeps_the_historical_mega_page_default() {
    let bytes = mux_without_boundaries(None);
    let pages = parse_pages(&bytes).expect("pages parse");
    // Without a target the 600-byte packets accumulate to the
    // 255-segment limit — pages far above the 4-8 kB band.
    assert!(
        pages.iter().any(|p| p.data.len() > 8192),
        "no-target pagination must be unchanged"
    );
    // Still a valid stream: full round-trip.
    let reader: Box<dyn ReadSeek> = Box::new(Cursor::new(bytes));
    let mut demux = oxideav_ogg::demux::open(reader, &NullCodecResolver).unwrap();
    let mut count = 0;
    while demux.next_packet().is_ok() {
        count += 1;
    }
    assert_eq!(count, 100);
}

#[test]
fn unit_boundary_still_wins_over_the_target_without_nil_pages() {
    // unit_boundary on every packet + a huge target: one packet per
    // page exactly as before, and no empty (nil) pages sneak in.
    let stream = vorbis_stream();
    let shared = SharedBuf::default();
    let out: Box<dyn WriteSeek> = Box::new(shared.clone());
    let mut mux = oxideav_ogg::mux::open_concrete(out, std::slice::from_ref(&stream)).unwrap();
    mux.set_page_target_bytes(Some(1)); // aggressive target
    mux.write_header().unwrap();
    for i in 1..=5i64 {
        let mut pkt = Packet::new(0, stream.time_base, vec![0x22; 64]);
        pkt.pts = Some(512 * i);
        pkt.flags.unit_boundary = true;
        mux.write_packet(&pkt).unwrap();
    }
    mux.write_trailer().unwrap();
    drop(mux);
    let bytes = {
        let guard = shared.0.lock().unwrap();
        guard.get_ref().clone()
    };
    let pages = parse_pages(&bytes).unwrap();
    for (i, p) in pages.iter().enumerate() {
        assert!(!p.lacing.is_empty(), "page {i} is a spurious nil page");
    }
    let data_pages = pages.iter().filter(|p| p.granule_position > 0).count();
    assert_eq!(data_pages, 5, "one page per unit_boundary packet");
}
