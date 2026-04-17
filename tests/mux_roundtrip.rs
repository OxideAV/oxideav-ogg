//! Integration tests for the Ogg muxer: round-trip mux -> demux verifying
//! page framing, multi-stream BOS ordering, granule carry-through, and
//! EOS marking on the last page.

use std::io::Cursor;

use oxideav_container::{ReadSeek, WriteSeek};
use oxideav_core::{CodecId, CodecParameters, Packet, StreamInfo, TimeBase};

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
    let mut p = Vec::new();
    p.push(0x03);
    p.extend_from_slice(b"vorbis");
    p.extend_from_slice(&0u32.to_le_bytes()); // vendor len
    p.extend_from_slice(&0u32.to_le_bytes()); // user count
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

/// Xiph-lace three header packets the way the Ogg demuxer's extradata
/// builder does — so we can round-trip through the muxer.
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

fn opus_head_packet(channels: u8, input_rate: u32) -> Vec<u8> {
    let mut p = Vec::with_capacity(19);
    p.extend_from_slice(b"OpusHead");
    p.push(1); // version
    p.push(channels);
    p.extend_from_slice(&0u16.to_le_bytes()); // pre-skip
    p.extend_from_slice(&input_rate.to_le_bytes());
    p.extend_from_slice(&0i16.to_le_bytes()); // output gain
    p.push(0); // channel mapping family
    assert_eq!(p.len(), 19);
    p
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

/// Shared buffer that can be written-to via WriteSeek AND inspected after
/// the muxer drops. The WriteSeek impl takes a clone of the `Arc` so the
/// muxer's requirement of `'static` output is satisfied.
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

/// Helper that muxes a fresh stream and returns the complete byte blob.
fn mux_to_bytes<F>(streams: Vec<StreamInfo>, feed: F) -> Vec<u8>
where
    F: FnOnce(&mut dyn oxideav_container::Muxer),
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

#[test]
fn mux_then_demux_vorbis_restores_metadata() {
    let id = vorbis_id_packet(2, 48_000);
    let com = vorbis_comment_packet();
    let setup = vorbis_setup_packet();
    let extradata = xiph_lace_three(&[&id, &com, &setup]);
    let stream = single_stream(0, "vorbis", extradata, TimeBase::new(1, 48_000));

    let bytes = mux_to_bytes(vec![stream.clone()], |m| {
        for i in 1..=5i64 {
            let granule = 960 * i;
            let mut pkt = Packet::new(0, stream.time_base, vec![0xAB, i as u8]);
            pkt.pts = Some(granule);
            pkt.dts = Some(granule);
            pkt.flags.keyframe = true;
            pkt.flags.unit_boundary = true;
            m.write_packet(&pkt).unwrap();
        }
    });

    assert_eq!(&bytes[0..4], b"OggS", "muxed output starts with OggS");

    // Re-open via the demuxer and check round-trip.
    let reader: Box<dyn ReadSeek> = Box::new(Cursor::new(bytes));
    let mut demux = oxideav_ogg::demux::open(reader).expect("demux muxed output");
    let streams = demux.streams();
    assert_eq!(streams.len(), 1);
    assert_eq!(streams[0].params.codec_id.as_str(), "vorbis");
    assert_eq!(streams[0].params.channels, Some(2));
    assert_eq!(streams[0].params.sample_rate, Some(48_000));

    let mut pkts = Vec::new();
    while let Ok(p) = demux.next_packet() {
        pkts.push(p);
    }
    assert_eq!(pkts.len(), 5, "expected 5 data packets round-tripped");
    // The last page carried granule 4800, which the demuxer pins on the
    // final packet of the page (a single packet per page in our stream).
    assert_eq!(pkts.last().unwrap().pts, Some(4800));
}

#[test]
fn mux_multi_stream_bos_pages_come_first() {
    // Stream 0: Vorbis at 48 kHz.
    let id = vorbis_id_packet(2, 48_000);
    let com = vorbis_comment_packet();
    let setup = vorbis_setup_packet();
    let vorbis_extra = xiph_lace_three(&[&id, &com, &setup]);
    let s0 = single_stream(0, "vorbis", vorbis_extra, TimeBase::new(1, 48_000));

    // Stream 1: Opus.
    let opus_extra = opus_head_packet(2, 48_000);
    let s1 = single_stream(1, "opus", opus_extra, TimeBase::new(1, 48_000));

    let bytes = mux_to_bytes(vec![s0.clone(), s1.clone()], |m| {
        // Interleave a few data packets across both streams.
        for i in 1..=3i64 {
            let mut p0 = Packet::new(0, s0.time_base, vec![0xAA, i as u8]);
            p0.pts = Some(960 * i);
            p0.dts = p0.pts;
            p0.flags.keyframe = true;
            p0.flags.unit_boundary = true;
            m.write_packet(&p0).unwrap();

            let mut p1 = Packet::new(1, s1.time_base, vec![0xBB, i as u8]);
            p1.pts = Some(960 * i);
            p1.dts = p1.pts;
            p1.flags.keyframe = true;
            p1.flags.unit_boundary = true;
            m.write_packet(&p1).unwrap();
        }
    });

    // Walk pages from the start and verify BOS pages for both streams
    // come BEFORE any non-BOS page (RFC 3533 §6).
    let mut seen_bos_serials: Vec<u32> = Vec::new();
    let mut first_nonbos_before_both_bos = false;
    let mut off = 0usize;
    while off + 27 <= bytes.len() {
        if &bytes[off..off + 4] != b"OggS" {
            break;
        }
        let flag = bytes[off + 5];
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
        let is_bos = flag & 0x02 != 0;
        if is_bos {
            if !seen_bos_serials.contains(&serial) {
                seen_bos_serials.push(serial);
            }
        } else if seen_bos_serials.len() < 2 {
            first_nonbos_before_both_bos = true;
            break;
        }
        off = data_start + data_len;
    }
    assert!(
        !first_nonbos_before_both_bos,
        "non-BOS page appeared before all BOS pages"
    );
    assert_eq!(
        seen_bos_serials.len(),
        2,
        "both streams' BOS must be present"
    );

    // Demux and check both streams reappear with the right codec IDs.
    let reader: Box<dyn ReadSeek> = Box::new(Cursor::new(bytes));
    let demux = oxideav_ogg::demux::open(reader).expect("demux multi-stream");
    let streams = demux.streams();
    assert_eq!(streams.len(), 2);
    let codecs: Vec<&str> = streams.iter().map(|s| s.params.codec_id.as_str()).collect();
    assert!(codecs.contains(&"vorbis"));
    assert!(codecs.contains(&"opus"));
}

#[test]
fn mux_last_page_sets_eos_flag() {
    let id = vorbis_id_packet(1, 44_100);
    let com = vorbis_comment_packet();
    let setup = vorbis_setup_packet();
    let extradata = xiph_lace_three(&[&id, &com, &setup]);
    let stream = single_stream(0, "vorbis", extradata, TimeBase::new(1, 44_100));

    let bytes = mux_to_bytes(vec![stream.clone()], |m| {
        for i in 1..=3i64 {
            let mut pkt = Packet::new(0, stream.time_base, vec![0x11, i as u8]);
            pkt.pts = Some(441 * i);
            pkt.dts = pkt.pts;
            pkt.flags.keyframe = true;
            pkt.flags.unit_boundary = true;
            m.write_packet(&pkt).unwrap();
        }
    });

    // Walk every page; the final one must have the EOS (LAST_PAGE) flag.
    let mut last_flag: u8 = 0;
    let mut off = 0usize;
    while off + 27 <= bytes.len() && &bytes[off..off + 4] == b"OggS" {
        last_flag = bytes[off + 5];
        let n_segs = bytes[off + 26] as usize;
        let lacing_start = off + 27;
        let data_start = lacing_start + n_segs;
        let data_len: usize = bytes[lacing_start..data_start]
            .iter()
            .map(|&v| v as usize)
            .sum();
        off = data_start + data_len;
    }
    assert!(last_flag & 0x04 != 0, "last page must have EOS flag set");
}
