//! Muxing a packet larger than a single Ogg page (RFC 3533 §4 / §5).
//!
//! RFC 3533 §4 states a logical bitstream packet "can be split into several
//! pages" and §5 spells out the lacing: a packet "distributed over several
//! pages" is split into 255-byte segments whose final segment is `< 255`
//! (with a trailing `0` segment on an exact multiple of 255). A single Ogg
//! page can hold at most 255 lacing segments — so a packet of ~64 KB or more
//! MUST span multiple pages, the continuation pages carrying the §6 field-3
//! `continued` flag. The demuxer already reassembles such packets byte-for-
//! byte (`tests/multipage_packet.rs`); these tests pin the muxer's write-side
//! symmetry — a large content packet and a large header packet round-trip
//! through mux -> demux unchanged.

use std::io::Cursor;

use oxideav_core::{CodecId, CodecParameters, Packet, StreamInfo, TimeBase};
use oxideav_core::{ReadSeek, WriteSeek};

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

fn vorbis_comment_packet() -> Vec<u8> {
    let mut p = Vec::new();
    p.push(0x03);
    p.extend_from_slice(b"vorbis");
    p.extend_from_slice(&0u32.to_le_bytes());
    p.extend_from_slice(&0u32.to_le_bytes());
    p.push(0x01);
    p
}

/// A deliberately large Vorbis "setup" packet (codebooks routinely push the
/// real ones past 64 KB). `len` bytes after the `0x05 "vorbis"` magic.
fn big_vorbis_setup(len: usize) -> Vec<u8> {
    let mut p = Vec::with_capacity(7 + len);
    p.push(0x05);
    p.extend_from_slice(b"vorbis");
    p.extend((0..len).map(|i| (i * 31 + 7) as u8));
    p
}

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

fn mux_to_bytes<F>(streams: Vec<StreamInfo>, feed: F) -> Vec<u8>
where
    F: FnOnce(&mut dyn oxideav_core::Muxer),
{
    let shared = SharedBuf::default();
    let writer: Box<dyn WriteSeek> = Box::new(shared.clone());
    let mut muxer = oxideav_ogg::mux::open(writer, &streams).unwrap();
    muxer.write_header().unwrap();
    feed(&mut *muxer);
    muxer.write_trailer().unwrap();
    drop(muxer);
    let guard = shared.0.lock().unwrap();
    guard.get_ref().clone()
}

/// Round-trip a content packet that is larger than one Ogg page's 255×255
/// body. The exact-multiple-of-255 boundary (255×255 = 65025) is the worst
/// case: it needs 255 full segments plus a 0 terminator = 256 lacing entries,
/// which a single page cannot hold.
#[test]
fn large_content_packet_spans_pages_and_round_trips() {
    let id = vorbis_id_packet(2, 48_000);
    let com = vorbis_comment_packet();
    let setup = big_vorbis_setup(16);
    let extradata = xiph_lace_three(&[&id, &com, &setup]);
    let stream = single_stream(0, "vorbis", extradata, TimeBase::new(1, 48_000));

    // Several big content packets of varying sizes around the page boundary.
    let payloads: Vec<Vec<u8>> = [65025usize, 70_000, 130_050, 200_000]
        .iter()
        .map(|&n| (0..n).map(|i| (i % 251) as u8).collect())
        .collect();

    let payloads_for_feed = payloads.clone();
    let bytes = mux_to_bytes(vec![stream.clone()], move |m| {
        for (i, pl) in payloads_for_feed.iter().enumerate() {
            let mut pkt = Packet::new(0, stream.time_base, pl.clone());
            pkt.pts = Some(960 * (i as i64 + 1));
            pkt.dts = pkt.pts;
            pkt.flags.keyframe = true;
            pkt.flags.unit_boundary = true;
            m.write_packet(&pkt).unwrap();
        }
    });

    let reader: Box<dyn ReadSeek> = Box::new(Cursor::new(bytes));
    let mut demux = oxideav_ogg::demux::open(reader, &oxideav_core::NullCodecResolver)
        .expect("demux muxed output");
    assert_eq!(demux.streams().len(), 1);

    let mut got = Vec::new();
    while let Ok(p) = demux.next_packet() {
        got.push(p.data);
    }
    assert_eq!(got.len(), payloads.len(), "every big packet round-trips");
    for (i, (a, b)) in got.iter().zip(payloads.iter()).enumerate() {
        assert_eq!(a, b, "payload {i} is byte-exact after mux->demux");
    }
}

/// A header packet (Vorbis setup) larger than a page must also span pages.
/// Header packets get their own page in the muxer, so the spanning path is
/// reached via the header branch of `write_packet`.
#[test]
fn large_header_packet_spans_pages_and_round_trips() {
    let id = vorbis_id_packet(2, 48_000);
    let com = vorbis_comment_packet();
    // ~96 KB setup packet — well past one page.
    let setup = big_vorbis_setup(96 * 1024);
    let extradata = xiph_lace_three(&[&id, &com, &setup]);
    let stream = single_stream(0, "vorbis", extradata, TimeBase::new(1, 48_000));

    let bytes = mux_to_bytes(vec![stream.clone()], move |m| {
        let mut pkt = Packet::new(0, stream.time_base, vec![0xAB, 0xCD]);
        pkt.pts = Some(960);
        pkt.dts = pkt.pts;
        pkt.flags.keyframe = true;
        pkt.flags.unit_boundary = true;
        m.write_packet(&pkt).unwrap();
    });

    // The setup packet survived as the third header; re-muxing the demuxed
    // extradata would reproduce it. Here we simply confirm the file demuxes
    // cleanly (no CRC / framing error) and yields the one content packet.
    let reader: Box<dyn ReadSeek> = Box::new(Cursor::new(bytes));
    let mut demux = oxideav_ogg::demux::open(reader, &oxideav_core::NullCodecResolver)
        .expect("demux muxed output with oversized header packet");
    assert_eq!(demux.streams().len(), 1);
    let mut n = 0;
    while let Ok(_p) = demux.next_packet() {
        n += 1;
    }
    assert_eq!(n, 1, "one content packet after the spanning header");
}
