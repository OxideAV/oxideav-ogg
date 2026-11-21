//! Muxer page policy, flags, and streaming behaviour.
//!
//! * Default pagination: a content page closes at 4096 body bytes or
//!   one second of stream time, whichever comes first — the "usually
//!   4-8 kB" band of RFC 3533 for ordinary bitrates, bounded latency
//!   for low ones. Both targets are configurable, and source page
//!   boundaries (`unit_boundary`) can be kept or ignored.
//! * A page's granule position is always the end position of the last
//!   packet finishing on it (RFC 3533 §6 field 4): the size / duration
//!   rules never close a page on a packet whose position is unknown.
//! * BOS on exactly the first page of each stream, EOS on exactly the
//!   last, `continued` exactly where a packet spans (§6 field 3).
//! * Output is append-only: a sink that cannot seek works, and a
//!   multiplexed stream that falls silent does not make the muxer hold
//!   the rest of the file in memory.

use std::io::{Cursor, Seek, SeekFrom, Write};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use oxideav_core::{CodecId, CodecParameters, Muxer, Packet, StreamInfo, TimeBase, WriteSeek};
use oxideav_ogg::framing::parse_pages;
use oxideav_ogg::mux::{self, OggMuxer};
use oxideav_ogg::page::Page;
use oxideav_ogg::validate::validate;

fn vorbis_stream(index: u32, rate: u32) -> StreamInfo {
    let mut id = vec![0x01];
    id.extend_from_slice(b"vorbis");
    id.extend_from_slice(&0u32.to_le_bytes());
    id.push(2);
    id.extend_from_slice(&rate.to_le_bytes());
    id.extend_from_slice(&[0; 12]);
    id.extend_from_slice(&[0xB8, 0x01]);
    let mut com = vec![0x03];
    com.extend_from_slice(b"vorbis");
    com.extend_from_slice(&[0, 0, 0, 0, 0, 0, 0, 0, 1]);
    let mut setup = vec![0x05];
    setup.extend_from_slice(b"vorbis");
    setup.extend_from_slice(&[0; 32]);
    let mut params = CodecParameters::audio(CodecId::new("vorbis"));
    params.channels = Some(2);
    params.sample_rate = Some(rate);
    params.extradata = mux::xiph_lace(&[&id, &com, &setup]).unwrap();
    StreamInfo {
        index,
        time_base: TimeBase::new(1, rate as i64),
        duration: None,
        start_time: Some(0),
        params,
    }
}

#[derive(Clone, Default)]
struct Shared(Arc<Mutex<Cursor<Vec<u8>>>>);

impl Shared {
    fn bytes(&self) -> Vec<u8> {
        self.0.lock().unwrap().get_ref().clone()
    }
}

impl Write for Shared {
    fn write(&mut self, b: &[u8]) -> std::io::Result<usize> {
        self.0.lock().unwrap().write(b)
    }
    fn flush(&mut self) -> std::io::Result<()> {
        Ok(())
    }
}

impl Seek for Shared {
    fn seek(&mut self, p: SeekFrom) -> std::io::Result<u64> {
        self.0.lock().unwrap().seek(p)
    }
}

/// An append-only sink: every seek (even a position query) fails.
#[derive(Clone, Default)]
struct AppendOnly(Arc<Mutex<Vec<u8>>>);

impl Write for AppendOnly {
    fn write(&mut self, b: &[u8]) -> std::io::Result<usize> {
        self.0.lock().unwrap().extend_from_slice(b);
        Ok(b.len())
    }
    fn flush(&mut self) -> std::io::Result<()> {
        Ok(())
    }
}

impl Seek for AppendOnly {
    fn seek(&mut self, _: SeekFrom) -> std::io::Result<u64> {
        Err(std::io::Error::other("append-only sink"))
    }
}

/// Mux `n` packets of `len` bytes, each advancing `step` ticks, after
/// `configure` adjusted the muxer; `flagged` sets `unit_boundary` and
/// `stamped` decides which packets carry a pts.
fn mux_stream(
    rate: u32,
    n: i64,
    len: usize,
    step: i64,
    flagged: bool,
    stamped: impl Fn(i64) -> bool,
    configure: impl Fn(&mut OggMuxer),
) -> Vec<u8> {
    let stream = vorbis_stream(0, rate);
    let shared = Shared::default();
    let out: Box<dyn WriteSeek> = Box::new(shared.clone());
    let mut m = mux::open_concrete(out, std::slice::from_ref(&stream)).unwrap();
    configure(&mut m);
    m.write_header().unwrap();
    for i in 1..=n {
        let mut pkt = Packet::new(0, stream.time_base, vec![(i % 200) as u8; len]);
        if stamped(i) {
            pkt.pts = Some(step * i);
        }
        pkt.flags.unit_boundary = flagged;
        m.write_packet(&pkt).unwrap();
    }
    m.write_trailer().unwrap();
    assert_eq!(m.bytes_written(), shared.bytes().len() as u64);
    drop(m);
    shared.bytes()
}

fn content_pages(bytes: &[u8]) -> Vec<Page> {
    let report = validate(bytes);
    assert!(report.is_clean(), "{:?}", report.issues);
    parse_pages(bytes)
        .unwrap()
        .into_iter()
        .filter(|p| p.granule_position != 0)
        .collect()
}

#[test]
fn defaults_are_4096_bytes_or_one_second() {
    let stream = vorbis_stream(0, 48_000);
    let out: Box<dyn WriteSeek> = Box::new(Shared::default());
    let m = mux::open_concrete(out, std::slice::from_ref(&stream)).unwrap();
    assert_eq!(m.page_target_bytes(), Some(4096));
    assert_eq!(m.page_target_duration(), Some(Duration::from_secs(1)));
    assert_eq!(mux::DEFAULT_PAGE_TARGET_BYTES, 4096);
    assert_eq!(mux::DEFAULT_PAGE_TARGET_DURATION, Duration::from_secs(1));
}

#[test]
fn ordinary_bitrate_fills_pages_to_the_size_target() {
    // 300-byte packets every 1024 samples at 48 kHz (~112 kbit/s): the
    // size target is reached long before a second has passed.
    let bytes = mux_stream(48_000, 2000, 300, 1024, false, |_| true, |_| {});
    let pages = content_pages(&bytes);
    assert!(pages.len() > 100);
    for p in &pages[..pages.len() - 1] {
        assert!(
            (4096..4096 + 300).contains(&p.data.len()),
            "page body {}",
            p.data.len()
        );
    }
    // Framing overhead stays near 1 %.
    let payload = 2000 * 300;
    let overhead = bytes.len() as f64 / payload as f64 - 1.0;
    assert!(overhead < 0.015, "overhead {overhead}");
}

#[test]
fn low_bitrate_closes_pages_on_the_duration_target() {
    // 20-byte packets every 20 ms (8 kbit/s): 4096 bytes would be four
    // seconds of audio, so the one-second rule decides.
    let bytes = mux_stream(48_000, 1000, 20, 960, false, |_| true, |_| {});
    let pages = content_pages(&bytes);
    let mut prev = 0i64;
    for p in &pages {
        let span = p.granule_position - prev;
        assert!(span <= 48_000, "page spans {span} samples");
        prev = p.granule_position;
    }
    assert_eq!(pages.len(), 20);
    assert!(pages.iter().all(|p| p.data.len() == 50 * 20));

    // A 100 ms target gives ten times as many pages (lower latency,
    // more overhead); no target at all gives 255-packet pages.
    let fast = mux_stream(
        48_000,
        1000,
        20,
        960,
        false,
        |_| true,
        |m| {
            m.set_page_target_duration(Some(Duration::from_millis(100)));
        },
    );
    assert_eq!(content_pages(&fast).len(), 200);
    assert!(fast.len() > bytes.len());
    let bulk = mux_stream(
        48_000,
        1000,
        20,
        960,
        false,
        |_| true,
        |m| {
            m.set_page_target_duration(None);
            m.set_page_target_bytes(None);
        },
    );
    let pages = content_pages(&bulk);
    assert_eq!(pages.len(), 4);
    assert_eq!(pages[0].lacing.len(), 255);
    assert!(bulk.len() < bytes.len());
}

#[test]
fn source_boundaries_are_kept_or_ignored_on_request() {
    // Every packet flagged: one page per packet by default …
    let kept = mux_stream(48_000, 400, 100, 960, true, |_| true, |_| {});
    assert_eq!(content_pages(&kept).len(), 400);
    // … and repaginated to the targets when told to ignore the flags.
    let merged = mux_stream(
        48_000,
        400,
        100,
        960,
        true,
        |_| true,
        |m| {
            m.set_keep_source_page_boundaries(false);
        },
    );
    let pages = content_pages(&merged);
    assert!(pages.len() < 12, "{} pages", pages.len());
    assert!(merged.len() < kept.len());
    assert_eq!(pages.last().unwrap().granule_position, 960 * 400);
}

#[test]
fn a_page_is_never_closed_by_choice_on_a_packet_without_a_position() {
    // Only every 7th packet carries a pts (what a demuxer hands over
    // for the last packet of each source page). Every page the policy
    // closes must end on one of those, so every granule position is
    // the true end of the last packet on the page.
    let bytes = mux_stream(48_000, 700, 600, 1024, false, |i| i % 7 == 0, |_| {});
    let pages = content_pages(&bytes);
    let mut packets = 0i64;
    for p in &pages {
        packets += p.lacing.iter().filter(|&&v| v < 255).count() as i64;
        assert_eq!(
            p.granule_position,
            1024 * packets,
            "after {packets} packets"
        );
        assert_eq!(packets % 7, 0);
    }
    assert_eq!(packets, 700);
    // 4096-byte target, 4200-byte groups: one group per page.
    assert_eq!(pages.len(), 100);

    // With a duration on every packet the positions in between are
    // known too, and pages close at the targets proper.
    let stream = vorbis_stream(0, 48_000);
    let shared = Shared::default();
    let out: Box<dyn WriteSeek> = Box::new(shared.clone());
    let mut m = mux::open_concrete(out, std::slice::from_ref(&stream)).unwrap();
    m.write_header().unwrap();
    for i in 1..=700i64 {
        let mut pkt = Packet::new(0, stream.time_base, vec![1; 600]);
        pkt.duration = Some(1024);
        if i == 1 {
            pkt.pts = Some(1024);
        }
        m.write_packet(&pkt).unwrap();
    }
    m.write_trailer().unwrap();
    let pages = content_pages(&shared.bytes());
    assert_eq!(pages.len(), 100);
    assert!(pages.iter().all(|p| p.data.len() == 4200));
    assert_eq!(pages[0].granule_position, 7 * 1024);
    assert_eq!(pages[99].granule_position, 700 * 1024);
}

#[test]
fn flags_mark_first_last_and_continued_pages_only() {
    // Two streams; one of them carries a packet spanning three pages.
    let a = vorbis_stream(0, 48_000);
    let b = vorbis_stream(1, 44_100);
    let shared = Shared::default();
    let out: Box<dyn WriteSeek> = Box::new(shared.clone());
    let mut m = mux::open(out, &[a.clone(), b.clone()]).unwrap();
    m.write_header().unwrap();
    for i in 1..=60i64 {
        let len = if i == 30 { 150_000 } else { 500 };
        let mut pkt = Packet::new(0, a.time_base, vec![i as u8; len]);
        pkt.pts = Some(4800 * i);
        m.write_packet(&pkt).unwrap();
        let mut pkt = Packet::new(1, b.time_base, vec![i as u8; 700]);
        pkt.pts = Some(4410 * i);
        m.write_packet(&pkt).unwrap();
    }
    m.write_trailer().unwrap();
    drop(m);
    let bytes = shared.bytes();
    let report = validate(&bytes);
    assert!(report.is_clean(), "{:?}", report.issues);
    let pages = parse_pages(&bytes).unwrap();
    for serial in [0u32, 1] {
        let mine: Vec<&Page> = pages.iter().filter(|p| p.serial == serial).collect();
        assert!(mine[0].is_first());
        assert!(mine.last().unwrap().is_last());
        assert_eq!(mine.iter().filter(|p| p.is_first()).count(), 1);
        assert_eq!(mine.iter().filter(|p| p.is_last()).count(), 1);
        for w in mine.windows(2) {
            assert_eq!(w[1].seq_no, w[0].seq_no + 1);
            let open = w[0].lacing.last() == Some(&255);
            assert_eq!(w[1].is_continued(), open, "seq {}", w[1].seq_no);
            if open && w[0].lacing.iter().all(|&v| v == 255) {
                assert_eq!(w[0].granule_position, -1, "no packet ends on the page");
            }
        }
        let continued = mine.iter().filter(|p| p.is_continued()).count();
        assert_eq!(continued, if serial == 0 { 2 } else { 0 });
    }
    // BOS pages first, then every other header page, then content in
    // time order.
    assert!(pages[0].is_first() && pages[1].is_first());
    let first_content = pages.iter().position(|p| p.granule_position != 0).unwrap();
    assert_eq!(first_content, 6);
    let mut last_time = 0.0f64;
    for p in &pages[first_content..] {
        if p.granule_position < 0 {
            continue;
        }
        let rate = if p.serial == 0 { 48_000.0 } else { 44_100.0 };
        let t = p.granule_position as f64 / rate;
        assert!(t >= last_time, "content pages out of time order");
        last_time = t;
    }
}

#[test]
fn append_only_sink_gets_the_same_bytes() {
    let stream = vorbis_stream(0, 48_000);
    let run = |out: Box<dyn WriteSeek>| {
        let mut m = mux::open(out, std::slice::from_ref(&stream)).unwrap();
        m.write_header().unwrap();
        for i in 1..=500i64 {
            let mut pkt = Packet::new(0, stream.time_base, vec![i as u8; 333]);
            pkt.pts = Some(1024 * i);
            m.write_packet(&pkt).unwrap();
        }
        m.write_trailer().unwrap();
    };
    let seekable = Shared::default();
    run(Box::new(seekable.clone()));
    let pipe = AppendOnly::default();
    run(Box::new(pipe.clone()));
    assert_eq!(*pipe.0.lock().unwrap(), seekable.bytes());
}

#[test]
fn output_streams_out_while_muxing() {
    // Single stream: everything but the page being built and the one
    // held back for the EOS flag is on the wire at any moment.
    let stream = vorbis_stream(0, 48_000);
    let pipe = AppendOnly::default();
    let mut m = mux::open_concrete(Box::new(pipe.clone()), std::slice::from_ref(&stream)).unwrap();
    m.write_header().unwrap();
    let mut fed = 0u64;
    for i in 1..=5000i64 {
        let mut pkt = Packet::new(0, stream.time_base, vec![i as u8; 400]);
        pkt.pts = Some(1024 * i);
        m.write_packet(&pkt).unwrap();
        fed += 400;
        assert!(
            fed - m.bytes_written().min(fed) < 3 * 4500,
            "{} bytes fed, {} written",
            fed,
            m.bytes_written()
        );
    }
    m.write_trailer().unwrap();
}

#[test]
fn a_silent_stream_does_not_make_the_muxer_buffer_the_file() {
    // Stream 1 produces one page and then nothing; stream 0 keeps
    // going. Time-ordered release would wait for stream 1 forever; the
    // interleave bound lets stream 0's pages go once it is exceeded.
    let a = vorbis_stream(0, 48_000);
    let b = vorbis_stream(1, 48_000);
    let shared = Shared::default();
    let out: Box<dyn WriteSeek> = Box::new(shared.clone());
    let mut m = mux::open_concrete(out, &[a.clone(), b.clone()]).unwrap();
    m.set_interleave_buffer_bytes(256 * 1024);
    m.write_header().unwrap();
    let mut pkt = Packet::new(1, b.time_base, vec![9; 100]);
    pkt.pts = Some(960);
    pkt.flags.unit_boundary = true;
    m.write_packet(&pkt).unwrap();
    let mut fed = 0u64;
    for i in 1..=20_000i64 {
        let mut pkt = Packet::new(0, a.time_base, vec![i as u8; 500]);
        pkt.pts = Some(1024 * i);
        m.write_packet(&pkt).unwrap();
        fed += 500;
        assert!(
            fed.saturating_sub(m.bytes_written()) < 300 * 1024,
            "{} bytes fed, {} written",
            fed,
            m.bytes_written()
        );
    }
    m.write_trailer().unwrap();
    drop(m);
    let bytes = shared.bytes();
    assert!(bytes.len() > 10_000_000);
    let report = validate(&bytes);
    assert_eq!(report.error_count(), 0, "{:?}", report.issues);
}
