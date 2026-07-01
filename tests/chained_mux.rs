//! Integration tests for the Ogg muxer's chained-stream (RFC 3533 §4
//! sequential-multiplexing) write path — `OggMuxer::begin_new_link`.
//!
//! A chained physical Ogg bitstream is the back-to-back concatenation of
//! independent logical bitstreams: each link starts with its own BOS
//! page(s) and ends with an EOS-flagged page, "the eos page of a given
//! logical bitstream is immediately followed by the bos page of the next"
//! (RFC 3533 §4). The muxer must terminate every stream of the current
//! link with EOS before writing the next link's BOS, and assign globally
//! unique serials across links ("Each chained logical bitstream MUST have
//! a unique serial number within the scope of the physical bitstream").
//!
//! Round-trip validation: mux N links, then demux and confirm the
//! demuxer partitions the result into N links with the right per-link
//! packets, matching what `tests/chained_streams.rs` pins for a
//! hand-built chain.

use std::io::Cursor;

use oxideav_core::{CodecId, CodecParameters, Packet, StreamInfo, TimeBase};
use oxideav_core::{Demuxer, Muxer, ReadSeek, WriteSeek};

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

fn vorbis_stream(index: u32, sample_rate: u32) -> StreamInfo {
    let id = vorbis_id_packet(2, sample_rate);
    let com = vorbis_comment_packet();
    let setup = vorbis_setup_packet();
    let extradata = xiph_lace_three(&[&id, &com, &setup]);
    let mut params = CodecParameters::audio(CodecId::new("vorbis"));
    params.channels = Some(2);
    params.sample_rate = Some(sample_rate);
    params.extradata = extradata;
    StreamInfo {
        index,
        time_base: TimeBase::new(1, sample_rate as i64),
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

/// Feed `n` content packets on `stream_index`, marker byte `marker`.
fn feed_link(m: &mut oxideav_ogg::mux::OggMuxer, stream: &StreamInfo, marker: u8, n: i64) {
    for i in 1..=n {
        let granule = 960 * i;
        let mut pkt = Packet::new(stream.index, stream.time_base, vec![marker, i as u8]);
        pkt.pts = Some(granule);
        pkt.dts = Some(granule);
        pkt.flags.keyframe = true;
        pkt.flags.unit_boundary = true;
        m.write_packet(&pkt).unwrap();
    }
}

#[test]
fn two_link_chain_round_trips() {
    let link_a = vorbis_stream(0, 48_000);
    let link_b = vorbis_stream(0, 44_100);

    let shared = SharedBuf::default();
    let writer: Box<dyn WriteSeek> = Box::new(shared.clone());
    let mut mux = oxideav_ogg::mux::open_concrete(writer, std::slice::from_ref(&link_a)).unwrap();
    mux.write_header().unwrap();
    assert_eq!(mux.link_index(), 0);
    feed_link(&mut mux, &link_a, 0xAA, 3);

    mux.begin_new_link(std::slice::from_ref(&link_b)).unwrap();
    assert_eq!(mux.link_index(), 1);
    feed_link(&mut mux, &link_b, 0xBB, 4);
    mux.write_trailer().unwrap();
    drop(mux);

    let bytes = shared.0.lock().unwrap().get_ref().clone();
    assert_eq!(&bytes[0..4], b"OggS", "chained output starts with OggS");

    // Demux and confirm two links, correct per-link packet counts + markers.
    let reader: Box<dyn ReadSeek> = Box::new(Cursor::new(bytes));
    let mut demux = oxideav_ogg::demux::open_concrete(reader, &oxideav_core::NullCodecResolver)
        .expect("demux chained mux output");

    let mut by_serial: std::collections::HashMap<u32, Vec<Packet>> =
        std::collections::HashMap::new();
    while let Ok(p) = demux.next_packet() {
        by_serial.entry(p.stream_index).or_default().push(p);
    }

    // Two logical streams, one per link.
    let streams = demux.streams();
    assert_eq!(streams.len(), 2, "both chained links registered");
    for s in streams {
        assert_eq!(s.params.codec_id.as_str(), "vorbis");
    }

    // The demuxer must have observed two distinct links.
    assert_eq!(demux.link_count(), 2, "demuxer sees two chained links");

    // Per-link packet counts.
    let mut counts: Vec<usize> = by_serial.values().map(|v| v.len()).collect();
    counts.sort_unstable();
    assert_eq!(counts, vec![3, 4], "link A gave 3 packets, link B gave 4");

    // Each stream's packets carry a single marker; both markers present.
    let markers: std::collections::HashSet<u8> =
        by_serial.values().map(|pkts| pkts[0].data[0]).collect();
    assert!(
        markers.contains(&0xAA) && markers.contains(&0xBB),
        "both link payloads delivered"
    );
    for pkts in by_serial.values() {
        let marker = pkts[0].data[0];
        for p in pkts {
            assert_eq!(p.data[0], marker, "markers must not mix across links");
        }
    }

    // Each public stream is filed under a distinct link index (0 and 1).
    let link_indices: std::collections::HashSet<u32> = (0..streams.len() as u32)
        .filter_map(|i| demux.stream_link_index(i))
        .collect();
    assert_eq!(
        link_indices,
        std::collections::HashSet::from([0, 1]),
        "streams span link 0 and link 1"
    );
}

#[test]
fn three_link_chain_round_trips() {
    let links: Vec<StreamInfo> = (0..3).map(|_| vorbis_stream(0, 48_000)).collect();
    let markers = [0xA0u8, 0xB0, 0xC0];
    let pkt_counts = [2i64, 5, 3];

    let shared = SharedBuf::default();
    let writer: Box<dyn WriteSeek> = Box::new(shared.clone());
    let mut mux = oxideav_ogg::mux::open_concrete(writer, std::slice::from_ref(&links[0])).unwrap();
    mux.write_header().unwrap();
    feed_link(&mut mux, &links[0], markers[0], pkt_counts[0]);
    for ((link, &marker), &count) in links
        .iter()
        .zip(markers.iter())
        .zip(pkt_counts.iter())
        .skip(1)
    {
        mux.begin_new_link(std::slice::from_ref(link)).unwrap();
        feed_link(&mut mux, link, marker, count);
    }
    assert_eq!(mux.link_index(), 2);
    mux.write_trailer().unwrap();
    drop(mux);

    let bytes = shared.0.lock().unwrap().get_ref().clone();
    let reader: Box<dyn ReadSeek> = Box::new(Cursor::new(bytes));
    let mut demux = oxideav_ogg::demux::open_concrete(reader, &oxideav_core::NullCodecResolver)
        .expect("demux 3-link chain");
    let mut counts: std::collections::HashMap<u8, usize> = std::collections::HashMap::new();
    while let Ok(p) = demux.next_packet() {
        *counts.entry(p.data[0]).or_default() += 1;
    }
    assert_eq!(demux.link_count(), 3, "three chained links");
    assert_eq!(counts.get(&0xA0), Some(&2));
    assert_eq!(counts.get(&0xB0), Some(&5));
    assert_eq!(counts.get(&0xC0), Some(&3));
}

#[test]
fn serials_are_globally_unique_across_links() {
    // All three links reuse StreamInfo::index 0, so the derived serial
    // collides — the muxer must bump each later link's serial to a fresh
    // value (RFC 3533 §4 unique-serial MUST).
    let links: Vec<StreamInfo> = (0..3).map(|_| vorbis_stream(0, 48_000)).collect();

    let shared = SharedBuf::default();
    let writer: Box<dyn WriteSeek> = Box::new(shared.clone());
    let mut mux = oxideav_ogg::mux::open_concrete(writer, std::slice::from_ref(&links[0])).unwrap();
    mux.write_header().unwrap();
    let mut seen = std::collections::HashSet::new();
    seen.insert(mux.stream_serial(0).unwrap());
    feed_link(&mut mux, &links[0], 0xAA, 2);
    for (k, link) in links.iter().enumerate().skip(1) {
        mux.begin_new_link(std::slice::from_ref(link)).unwrap();
        let serial = mux.stream_serial(0).unwrap();
        assert!(
            seen.insert(serial),
            "link {k} serial {serial:#010x} collides with an earlier link"
        );
        feed_link(&mut mux, link, 0xBB, 2);
    }
    mux.write_trailer().unwrap();
    drop(mux);

    // The chained file must still demux cleanly with a zero duplicate-serial
    // count (the muxer resolved the collision at write time).
    let bytes = shared.0.lock().unwrap().get_ref().clone();
    let reader: Box<dyn ReadSeek> = Box::new(Cursor::new(bytes));
    let mut demux = oxideav_ogg::demux::open_concrete(reader, &oxideav_core::NullCodecResolver)
        .expect("demux unique-serial chain");
    demux.build_seek_index().unwrap();
    while demux.next_packet().is_ok() {}
    assert_eq!(
        demux.duplicate_serial_count(),
        0,
        "muxer-assigned serials must not collide across links"
    );
}

#[test]
fn chained_mux_duration_sums_per_link() {
    // Link A: 3 packets @ 48 kHz, granule step 960 → last granule 2880 →
    // 60 ms. Link B: 5 packets @ 48 kHz → last granule 4800 → 100 ms.
    // The demuxer sums per-link durations for a chained file, so total
    // must be 160 ms — mirroring tests/chained_duration.rs but with a
    // muxer-produced file rather than a hand-built one.
    let link_a = vorbis_stream(0, 48_000);
    let link_b = vorbis_stream(0, 48_000);

    let shared = SharedBuf::default();
    let writer: Box<dyn WriteSeek> = Box::new(shared.clone());
    let mut mux = oxideav_ogg::mux::open_concrete(writer, std::slice::from_ref(&link_a)).unwrap();
    mux.write_header().unwrap();
    feed_link(&mut mux, &link_a, 0xAA, 3);
    mux.begin_new_link(std::slice::from_ref(&link_b)).unwrap();
    feed_link(&mut mux, &link_b, 0xBB, 5);
    mux.write_trailer().unwrap();
    drop(mux);

    let bytes = shared.0.lock().unwrap().get_ref().clone();
    let reader: Box<dyn ReadSeek> = Box::new(Cursor::new(bytes));
    let mut demux = oxideav_ogg::demux::open_concrete(reader, &oxideav_core::NullCodecResolver)
        .expect("demux chained mux for duration");
    demux.build_seek_index().expect("build seek index");
    let dur = demux.duration_micros().expect("duration recorded");
    assert!(
        (dur - 160_000).abs() <= 2,
        "chained mux duration should sum per-link (60+100 ms), got {dur}",
    );
}

#[test]
fn chained_grouped_link_round_trips() {
    // The most general RFC 3533 §4 topology: a link that GROUPS two
    // concurrently-multiplexed streams, chained to a second single-stream
    // link. begin_new_link takes a slice, so a grouped link is just a
    // multi-stream slice. On read-back the grouped link's two streams
    // share link 0, and the chained link's stream is link 1; chained
    // duration is max(group) + link1.
    let group0 = vorbis_stream(0, 48_000);
    let group1 = vorbis_stream(1, 48_000);
    let link1 = vorbis_stream(0, 48_000);

    let shared = SharedBuf::default();
    let writer: Box<dyn WriteSeek> = Box::new(shared.clone());
    let group = [group0.clone(), group1.clone()];
    let mut mux = oxideav_ogg::mux::open_concrete(writer, &group).unwrap();
    mux.write_header().unwrap();
    // Interleave the two grouped streams: stream 0 gets 4 packets (80 ms),
    // stream 1 gets 3 packets (60 ms).
    for i in 1..=4i64 {
        let granule = 960 * i;
        let mut pkt = Packet::new(0, group0.time_base, vec![0xA0, i as u8]);
        pkt.pts = Some(granule);
        pkt.flags.unit_boundary = true;
        mux.write_packet(&pkt).unwrap();
        if i <= 3 {
            let mut pkt1 = Packet::new(1, group1.time_base, vec![0xA1, i as u8]);
            pkt1.pts = Some(granule);
            pkt1.flags.unit_boundary = true;
            mux.write_packet(&pkt1).unwrap();
        }
    }
    // Chain a second, single-stream link: 5 packets (100 ms).
    mux.begin_new_link(std::slice::from_ref(&link1)).unwrap();
    feed_link(&mut mux, &link1, 0xB0, 5);
    mux.write_trailer().unwrap();
    drop(mux);

    let bytes = shared.0.lock().unwrap().get_ref().clone();
    let reader: Box<dyn ReadSeek> = Box::new(Cursor::new(bytes));
    let mut demux = oxideav_ogg::demux::open_concrete(reader, &oxideav_core::NullCodecResolver)
        .expect("demux grouped+chained mux output");

    let mut counts: std::collections::HashMap<u8, usize> = std::collections::HashMap::new();
    while let Ok(p) = demux.next_packet() {
        *counts.entry(p.data[0]).or_default() += 1;
    }
    // Two links: the group (link 0) + the chained link (link 1).
    assert_eq!(demux.link_count(), 2, "one grouped link + one chained link");
    // Three logical streams total (2 grouped + 1 chained).
    assert_eq!(demux.streams().len(), 3, "3 logical streams");
    assert_eq!(counts.get(&0xA0), Some(&4), "grouped stream 0: 4 packets");
    assert_eq!(counts.get(&0xA1), Some(&3), "grouped stream 1: 3 packets");
    assert_eq!(counts.get(&0xB0), Some(&5), "chained link: 5 packets");

    // The two grouped streams share link 0; the chained stream is link 1.
    demux.build_seek_index().unwrap();
    let dur = demux.duration_micros().expect("duration recorded");
    // max(80 ms group0, 60 ms group1) + 100 ms link1 = 180 ms.
    assert!(
        (dur - 180_000).abs() <= 2,
        "grouped+chained duration = max(group)+link1 = 80+100 = 180 ms, got {dur}",
    );
}

#[test]
fn begin_new_link_before_content_is_rejected() {
    let link = vorbis_stream(0, 48_000);
    let shared = SharedBuf::default();
    let writer: Box<dyn WriteSeek> = Box::new(shared.clone());
    let mut mux = oxideav_ogg::mux::open_concrete(writer, std::slice::from_ref(&link)).unwrap();
    mux.write_header().unwrap();
    // No content packet written yet — a link with only headers would not be
    // recognised as a distinct link on read-back.
    let err = mux.begin_new_link(std::slice::from_ref(&link));
    assert!(err.is_err(), "begin_new_link with no content must error");
}

#[test]
fn begin_new_link_before_header_is_rejected() {
    let link = vorbis_stream(0, 48_000);
    let shared = SharedBuf::default();
    let writer: Box<dyn WriteSeek> = Box::new(shared.clone());
    let mut mux = oxideav_ogg::mux::open_concrete(writer, std::slice::from_ref(&link)).unwrap();
    let err = mux.begin_new_link(std::slice::from_ref(&link));
    assert!(
        err.is_err(),
        "begin_new_link before write_header must error"
    );
}
