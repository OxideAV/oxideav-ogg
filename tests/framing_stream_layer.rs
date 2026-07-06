//! Cross-checks between the buffer-level `framing` module and the
//! full-container mux/demux stack: a stream written with
//! `framing::PageWriter` must be readable by `demux::open`, and the
//! muxer's on-disk output must reassemble cleanly through
//! `framing::PacketAssembler`.

use std::io::Cursor;

use oxideav_core::{CodecId, CodecParameters, Packet, StreamInfo, TimeBase};
use oxideav_core::{NullCodecResolver, ReadSeek, WriteSeek};
use oxideav_ogg::framing::{pages_to_packets, parse_pages, PacketAssembler, PageWriter};

/// Minimal valid Vorbis identification packet (30 bytes).
fn vorbis_id_packet(channels: u8, sample_rate: u32) -> Vec<u8> {
    let mut p = Vec::with_capacity(30);
    p.push(0x01);
    p.extend_from_slice(b"vorbis");
    p.extend_from_slice(&0u32.to_le_bytes()); // version
    p.push(channels);
    p.extend_from_slice(&sample_rate.to_le_bytes());
    p.extend_from_slice(&0i32.to_le_bytes()); // br_max
    p.extend_from_slice(&128000i32.to_le_bytes()); // br_nom
    p.extend_from_slice(&0i32.to_le_bytes()); // br_min
    p.push(0xB8);
    p.push(0x01);
    assert_eq!(p.len(), 30);
    p
}

fn vorbis_comment_packet() -> Vec<u8> {
    let mut p = vec![0x03];
    p.extend_from_slice(b"vorbis");
    p.extend_from_slice(&0u32.to_le_bytes()); // vendor len
    p.extend_from_slice(&0u32.to_le_bytes()); // user count
    p.push(0x01);
    p
}

fn vorbis_setup_packet(len: usize) -> Vec<u8> {
    let mut p = vec![0x05];
    p.extend_from_slice(b"vorbis");
    p.resize(len.max(7), 0xA5);
    p
}

/// A logical bitstream written packet-by-packet with `PageWriter`
/// (headers at granule 0 with §A.2-style page breaks, audio packets
/// with PCM granules) must demux through the container-level
/// `demux::open` with the right codec sniff, packet count, and pts.
#[test]
fn page_writer_stream_is_demuxable_by_the_container_demuxer() {
    let mut w = PageWriter::new(0x0DA7_A5E7);
    w.push_packet(&vorbis_id_packet(2, 48_000), 0);
    w.flush_page(); // id header alone on the BOS page
    w.push_packet(&vorbis_comment_packet(), 0);
    w.push_packet(&vorbis_setup_packet(200), 0);
    w.flush_page(); // setup finishes its page; audio begins fresh
    for i in 1..=5i64 {
        w.push_packet(&[0xAB; 64], 960 * i);
        w.flush_page();
    }
    let bytes = w.finish();

    // Page-level sanity through the framing helpers themselves.
    let pages = parse_pages(&bytes).expect("pages parse");
    assert!(pages[0].is_first());
    assert!(pages.last().unwrap().is_last());
    assert_eq!(pages.last().unwrap().granule_position, 4800);
    assert_eq!(pages_to_packets(&bytes).expect("packets assemble").len(), 8);

    // Container-level read-back.
    let reader: Box<dyn ReadSeek> = Box::new(Cursor::new(bytes));
    let mut demux =
        oxideav_ogg::demux::open(reader, &NullCodecResolver).expect("demux PageWriter output");
    let streams = demux.streams();
    assert_eq!(streams.len(), 1);
    assert_eq!(streams[0].params.codec_id.as_str(), "vorbis");
    assert_eq!(streams[0].params.channels, Some(2));
    assert_eq!(streams[0].params.sample_rate, Some(48_000));
    let mut pts = Vec::new();
    while let Ok(p) = demux.next_packet() {
        pts.push(p.pts);
    }
    assert_eq!(pts.len(), 5, "5 audio packets after the 3 headers");
    assert_eq!(*pts.last().unwrap(), Some(4800));
}

/// The container muxer's on-disk bytes must reassemble through the
/// strict `PacketAssembler` with no continuity or serial complaints,
/// and yield the same packet payloads that were written — including a
/// packet large enough to span pages.
#[test]
fn muxer_output_reassembles_through_packet_assembler() {
    let id = vorbis_id_packet(1, 44_100);
    let com = vorbis_comment_packet();
    let setup = vorbis_setup_packet(80);
    let extradata = oxideav_ogg::mux::xiph_lace(&[&id, &com, &setup]).expect("three packets lace");

    let mut params = CodecParameters::audio(CodecId::new("vorbis"));
    params.channels = Some(1);
    params.sample_rate = Some(44_100);
    params.extradata = extradata;
    let stream = StreamInfo {
        index: 0,
        time_base: TimeBase::new(1, 44_100),
        duration: None,
        start_time: Some(0),
        params,
    };

    let mut audio: Vec<Vec<u8>> = (1..=4u8).map(|i| vec![i & 0xFE; 300]).collect();
    audio.push(vec![0x0C; 70_000]); // spans pages (≥ 256 lacing segments)

    let buf = std::sync::Arc::new(std::sync::Mutex::new(Cursor::new(Vec::new())));
    #[derive(Clone)]
    struct Shared(std::sync::Arc<std::sync::Mutex<Cursor<Vec<u8>>>>);
    impl std::io::Write for Shared {
        fn write(&mut self, b: &[u8]) -> std::io::Result<usize> {
            self.0.lock().unwrap().write(b)
        }
        fn flush(&mut self) -> std::io::Result<()> {
            self.0.lock().unwrap().flush()
        }
    }
    impl std::io::Seek for Shared {
        fn seek(&mut self, p: std::io::SeekFrom) -> std::io::Result<u64> {
            self.0.lock().unwrap().seek(p)
        }
    }
    let out: Box<dyn WriteSeek> = Box::new(Shared(buf.clone()));
    let mut mux = oxideav_ogg::mux::open(out, std::slice::from_ref(&stream)).unwrap();
    mux.write_header().unwrap();
    for (i, data) in audio.iter().enumerate() {
        let mut pkt = Packet::new(0, stream.time_base, data.clone());
        pkt.pts = Some(441 * (i as i64 + 1));
        pkt.flags.unit_boundary = true;
        mux.write_packet(&pkt).unwrap();
    }
    mux.write_trailer().unwrap();
    drop(mux);
    let bytes = buf.lock().unwrap().get_ref().clone();

    let pages = parse_pages(&bytes).expect("muxer pages parse");
    let mut asm = PacketAssembler::new();
    let mut packets = Vec::new();
    for page in &pages {
        packets.extend(asm.push_page(page).expect("continuity holds"));
    }
    assert!(!asm.mid_packet(), "no packet left open at EOS");
    // 3 headers + the audio payloads, byte-exact.
    assert_eq!(packets.len(), 3 + audio.len());
    assert_eq!(packets[0], id);
    assert_eq!(packets[1], com);
    assert_eq!(packets[2], setup);
    for (i, a) in audio.iter().enumerate() {
        assert_eq!(&packets[3 + i], a, "audio packet {i}");
    }
}
