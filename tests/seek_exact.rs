//! Seek exactness and cost.
//!
//! Every seek is checked against an oracle built by a plain walk of the
//! file: the landing page is the LAST content page of the stream whose
//! granule (mapped onto the seek axis) is at or before the target, so
//! the page after it is the one whose packets cover the target and the
//! landing page's own packets are the pre-roll. Opus targets are moved
//! 80 ms (3840 samples at 48 kHz) earlier first. Demuxing after the
//! seek resumes with exactly the landing page's packets and reports no
//! damage (a landing in the middle of the stream is not a hole).
//!
//! Covered layouts: one stream, two multiplexed streams (RFC 3533 §4
//! grouping), a chained file (§4 chaining), a stream with a packet
//! continued across the landing page, and a stream whose first content
//! granule is far from zero. The I/O a cold seek performs is bounded.

use std::io::{Read, Seek, SeekFrom};
use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};

use oxideav_core::{Demuxer, Error, NullCodecResolver};
use oxideav_ogg::demux::OggDemuxer;
use oxideav_ogg::page::{Page, flags, lace};

fn rng(state: &mut u32) -> u32 {
    *state ^= *state << 13;
    *state ^= *state >> 17;
    *state ^= *state << 5;
    *state
}

fn vorbis_headers(rate: u32) -> [Vec<u8>; 3] {
    let mut id = vec![0x01];
    id.extend_from_slice(b"vorbis");
    id.extend_from_slice(&0u32.to_le_bytes());
    id.push(2);
    id.extend_from_slice(&rate.to_le_bytes());
    id.extend_from_slice(&0i32.to_le_bytes());
    id.extend_from_slice(&128_000i32.to_le_bytes());
    id.extend_from_slice(&0i32.to_le_bytes());
    id.push(0xB8);
    id.push(0x01);
    let mut comment = vec![0x03];
    comment.extend_from_slice(b"vorbis");
    comment.extend_from_slice(&[0u8; 8]);
    comment.push(0x01);
    let mut setup = vec![0x05];
    setup.extend_from_slice(b"vorbis");
    setup.extend_from_slice(&[0x33; 300]);
    [id, comment, setup]
}

fn opus_headers(pre_skip: u16) -> [Vec<u8>; 2] {
    let mut head = Vec::new();
    head.extend_from_slice(b"OpusHead");
    head.push(1);
    head.push(2);
    head.extend_from_slice(&pre_skip.to_le_bytes());
    head.extend_from_slice(&48_000u32.to_le_bytes());
    head.extend_from_slice(&0i16.to_le_bytes());
    head.push(0);
    let mut tags = Vec::new();
    tags.extend_from_slice(b"OpusTags");
    tags.extend_from_slice(&[0u8; 8]);
    [head, tags]
}

/// One logical bitstream as a list of `(time in seconds, page bytes)`.
struct Track {
    serial: u32,
    seq: u32,
    pages: Vec<(f64, Vec<u8>)>,
}

impl Track {
    fn new(serial: u32) -> Self {
        Self {
            serial,
            seq: 0,
            pages: Vec::new(),
        }
    }

    fn push(&mut self, time: f64, flags_byte: u8, granule: i64, lacing: Vec<u8>, data: Vec<u8>) {
        let bytes = Page {
            flags: flags_byte,
            granule_position: granule,
            serial: self.serial,
            seq_no: self.seq,
            lacing,
            data,
        }
        .to_bytes();
        self.seq += 1;
        self.pages.push((time, bytes));
    }

    fn header(&mut self, first: bool, packet: &[u8]) {
        let f = if first { flags::FIRST_PAGE } else { 0 };
        self.push(-1.0, f, 0, lace(packet.len()), packet.to_vec());
    }

    /// `n` content pages of 1–`max_packets` packets; granule advances
    /// `step` per packet from `start`; packet bodies `lo..hi` bytes.
    #[allow(clippy::too_many_arguments)]
    fn content(
        &mut self,
        n: usize,
        start: i64,
        step: i64,
        rate: f64,
        max_packets: u32,
        (lo, hi): (u32, u32),
        seed: u32,
    ) {
        let mut s = seed;
        let mut granule = start;
        for i in 0..n {
            let mut lacing = Vec::new();
            let mut data = Vec::new();
            for _ in 0..1 + rng(&mut s) % max_packets {
                let len = (lo + rng(&mut s) % (hi - lo)) as usize;
                lacing.extend(lace(len));
                // Bodies sprinkled with capture patterns: a seek probe
                // must not mistake them for pages.
                let mut body: Vec<u8> = (0..len).map(|_| rng(&mut s) as u8).collect();
                if len >= 40 {
                    body[8..12].copy_from_slice(b"OggS");
                    body[12] = 0;
                }
                data.extend(body);
                granule += step;
            }
            let f = if i + 1 == n { flags::LAST_PAGE } else { 0 };
            self.push(granule as f64 / rate, f, granule, lacing, data);
        }
    }
}

/// Interleave tracks: all BOS pages, then the other header pages, then
/// content pages in time order.
fn multiplex(tracks: Vec<Track>) -> Vec<u8> {
    let mut out = Vec::new();
    for t in &tracks {
        out.extend_from_slice(&t.pages[0].1);
    }
    let mut content: Vec<(f64, usize, &Vec<u8>)> = Vec::new();
    for (ti, t) in tracks.iter().enumerate() {
        for (time, bytes) in &t.pages[1..] {
            if *time < 0.0 {
                out.extend_from_slice(bytes);
            } else {
                content.push((*time, ti, bytes));
            }
        }
    }
    content.sort_by(|a, b| a.0.partial_cmp(&b.0).unwrap().then(a.1.cmp(&b.1)));
    for (_, _, bytes) in content {
        out.extend_from_slice(bytes);
    }
    out
}

/// `(offset, granule, completed packets)` of every content page of
/// `serial` on which a packet finishes, by a plain walk; `headers` is
/// the stream's header-packet count.
fn oracle(file: &[u8], serial: u32, headers: usize) -> PageTable {
    let mut out = Vec::new();
    let mut off = 0usize;
    let mut packets = 0usize;
    while off + 27 <= file.len() {
        let n = file[off + 26] as usize;
        let lacing = &file[off + 27..off + 27 + n];
        let body: usize = lacing.iter().map(|&v| v as usize).sum();
        let s = u32::from_le_bytes(file[off + 14..off + 18].try_into().unwrap());
        let g = i64::from_le_bytes(file[off + 6..off + 14].try_into().unwrap());
        if s == serial {
            let done = lacing.iter().filter(|&&v| v < 255).count();
            let before = packets;
            packets += done;
            if g >= 0 && packets > headers {
                out.push((off as u64, g, packets - before.max(headers)));
            }
        }
        off += 27 + n + body;
    }
    out
}

#[derive(Default)]
struct Io {
    reads: AtomicU64,
    bytes: AtomicU64,
}

struct Counted {
    data: Arc<Vec<u8>>,
    pos: u64,
    io: Arc<Io>,
}

impl Read for Counted {
    fn read(&mut self, buf: &mut [u8]) -> std::io::Result<usize> {
        let start = (self.pos as usize).min(self.data.len());
        let n = buf.len().min(self.data.len() - start);
        buf[..n].copy_from_slice(&self.data[start..start + n]);
        self.pos += n as u64;
        self.io.reads.fetch_add(1, Ordering::Relaxed);
        self.io.bytes.fetch_add(n as u64, Ordering::Relaxed);
        Ok(n)
    }
}

impl Seek for Counted {
    fn seek(&mut self, to: SeekFrom) -> std::io::Result<u64> {
        self.pos = match to {
            SeekFrom::Start(o) => o,
            SeekFrom::Current(d) => self.pos.checked_add_signed(d).unwrap(),
            SeekFrom::End(d) => (self.data.len() as u64).checked_add_signed(d).unwrap(),
        };
        Ok(self.pos)
    }
}

fn open(file: &Arc<Vec<u8>>, io: &Arc<Io>) -> OggDemuxer {
    oxideav_ogg::demux::open_concrete(
        Box::new(Counted {
            data: file.clone(),
            pos: 0,
            io: io.clone(),
        }),
        &NullCodecResolver,
    )
    .expect("open")
}

/// Seek `stream` of a fresh demuxer to each target and compare with the
/// oracle. `bias` maps a granule onto the pts axis (`pts = granule −
/// bias`), `preroll` is subtracted from the target before the search.
/// Returns the worst `(reads, bytes)` one seek cost.
#[allow(clippy::too_many_arguments)]
fn check_seeks(
    file: &Arc<Vec<u8>>,
    stream: u32,
    table: &[(u64, i64, usize)],
    bias: i64,
    preroll: i64,
    targets: &[i64],
    prepare: impl Fn(&mut OggDemuxer),
) -> (u64, u64) {
    let io = Arc::new(Io::default());
    let mut worst = (0, 0);
    for &pts in targets {
        let mut d = open(file, &io);
        prepare(&mut d);
        let before = (
            io.reads.load(Ordering::Relaxed),
            io.bytes.load(Ordering::Relaxed),
        );
        let landed = d.seek_to(stream, pts).expect("seek");
        let cost = (
            io.reads.load(Ordering::Relaxed) - before.0,
            io.bytes.load(Ordering::Relaxed) - before.1,
        );
        worst = (worst.0.max(cost.0), worst.1.max(cost.1));

        let search = pts - preroll + bias;
        let i = table.partition_point(|&(_, g, _)| g <= search);
        let (off, granule, packets) = table[i.saturating_sub(1)];
        assert_eq!(landed, granule, "pts {pts}: landing granule");
        assert_eq!(
            d.input_position().unwrap(),
            off,
            "pts {pts}: landing offset"
        );

        // The landing page's packets come out first: the last of them
        // carries the page granule, the ones before it none.
        let mut seen = 0usize;
        loop {
            match d.next_packet() {
                Ok(p) if p.stream_index == stream => {
                    seen += 1;
                    if seen == packets {
                        assert_eq!(p.pts, Some(granule), "pts {pts}: pre-roll page granule");
                        break;
                    }
                    assert_eq!(p.pts, None, "pts {pts}: packet {seen} of {packets}");
                }
                Ok(_) => {}
                Err(Error::Eof) => panic!("pts {pts}: stream ended before the landing page"),
                Err(e) => panic!("pts {pts}: {e}"),
            }
        }
        assert_eq!(d.hole_count(), 0, "pts {pts}");
        assert_eq!(d.framing_error_count(), 0, "pts {pts}");
        assert_eq!(d.duplicate_serial_count(), 0, "pts {pts}");
        assert_eq!(d.resync_count(), 0, "pts {pts}");
    }
    worst
}

fn targets(last: i64, n: usize, seed: u32) -> Vec<i64> {
    let mut s = seed;
    let mut out = vec![0, 1, last - 1, last, last + 5000, -100];
    out.extend((0..n).map(|_| (rng(&mut s) as i64 * 131) % last));
    out
}

/// `(offset, granule, completed packets)` rows of the oracle.
type PageTable = Vec<(u64, i64, usize)>;

fn single_vorbis(pages: usize, start: i64) -> (Arc<Vec<u8>>, PageTable) {
    let mut t = Track::new(0x5EEC_0001);
    let h = vorbis_headers(44_100);
    t.header(true, &h[0]);
    t.header(false, &h[1]);
    t.header(false, &h[2]);
    t.content(pages, start, 512, 44_100.0, 12, (30, 600), 0xA11CE);
    let file = Arc::new(multiplex(vec![t]));
    let table = oracle(&file, 0x5EEC_0001, 3);
    assert_eq!(table.len(), pages);
    (file, table)
}

#[test]
fn single_stream_seeks_are_exact_and_bounded() {
    let (file, table) = single_vorbis(2500, 0);
    assert!(file.len() > 4_000_000, "{}", file.len());
    let last = table.last().unwrap().1;
    let worst = check_seeks(&file, 0, &table, 0, 0, &targets(last, 300, 7), |_| {});
    // A cold seek: the end-of-input probe, one guess, a short walk.
    assert!(worst.0 <= 5, "worst seek took {} reads", worst.0);
    assert!(worst.1 <= 400 * 1024, "worst seek read {} bytes", worst.1);
}

#[test]
fn stream_starting_far_from_granule_zero() {
    // First content granule is ~10 minutes in (a capture joined late).
    let (file, table) = single_vorbis(800, 26_460_000);
    let last = table.last().unwrap().1;
    let mut t = targets(last, 100, 99);
    t.extend([26_460_000, 26_460_513, 26_500_000, 1000]);
    check_seeks(&file, 0, &table, 0, 0, &t, |_| {});
}

#[test]
fn opus_seeks_land_80_ms_early_on_the_pcm_axis() {
    let pre_skip = 3_333u16;
    let mut t = Track::new(0x0B05_0001);
    let h = opus_headers(pre_skip);
    t.header(true, &h[0]);
    t.header(false, &h[1]);
    // 20 ms packets, at most three per page: the 80 ms pre-roll spans
    // several pages.
    t.content(3000, pre_skip as i64, 960, 48_000.0, 3, (60, 300), 0xB0B);
    let file = Arc::new(multiplex(vec![t]));
    let table = oracle(&file, 0x0B05_0001, 2);
    let last = table.last().unwrap().1 - pre_skip as i64;
    check_seeks(
        &file,
        0,
        &table,
        pre_skip as i64,
        3840,
        &targets(last, 200, 3),
        |_| {},
    );
}

#[test]
fn multiplexed_streams_seek_independently() {
    let mut a = Track::new(0x00AA_0001);
    let h = vorbis_headers(44_100);
    a.header(true, &h[0]);
    a.header(false, &h[1]);
    a.header(false, &h[2]);
    a.content(1500, 0, 1024, 44_100.0, 6, (100, 900), 11);
    let mut b = Track::new(0x00BB_0002);
    let h = vorbis_headers(8_000);
    b.header(true, &h[0]);
    b.header(false, &h[1]);
    b.header(false, &h[2]);
    // A much sparser stream: few, small pages between long runs of `a`.
    b.content(120, 0, 8_000, 8_000.0, 2, (20, 80), 22);
    let file = Arc::new(multiplex(vec![a, b]));
    let ta = oracle(&file, 0x00AA_0001, 3);
    let tb = oracle(&file, 0x00BB_0002, 3);
    check_seeks(
        &file,
        0,
        &ta,
        0,
        0,
        &targets(ta.last().unwrap().1, 150, 5),
        |_| {},
    );
    check_seeks(
        &file,
        1,
        &tb,
        0,
        0,
        &targets(tb.last().unwrap().1, 150, 6),
        |_| {},
    );
}

#[test]
fn chained_links_seek_within_their_own_extent() {
    let mut a = Track::new(0xC4A1_0001);
    let h = vorbis_headers(44_100);
    a.header(true, &h[0]);
    a.header(false, &h[1]);
    a.header(false, &h[2]);
    a.content(700, 0, 1024, 44_100.0, 6, (100, 900), 31);
    let mut b = Track::new(0xC4A1_0002);
    let h = vorbis_headers(48_000);
    b.header(true, &h[0]);
    b.header(false, &h[1]);
    b.header(false, &h[2]);
    b.content(700, 0, 960, 48_000.0, 6, (100, 900), 32);
    let mut file = multiplex(vec![a]);
    let link_b_start = file.len() as u64;
    file.extend(multiplex(vec![b]));
    let file = Arc::new(file);
    let ta = oracle(&file, 0xC4A1_0001, 3);
    let tb = oracle(&file, 0xC4A1_0002, 3);
    assert!(tb[0].0 > link_b_start);

    // Without a prior scan the second link is unknown until demuxing
    // reaches it: read up to its first packet, then seek both streams.
    let reach_second_link = |d: &mut OggDemuxer| {
        while d.streams().len() < 2 {
            d.next_packet().expect("first link packets");
        }
    };
    check_seeks(
        &file,
        0,
        &ta,
        0,
        0,
        &targets(ta.last().unwrap().1, 60, 41),
        reach_second_link,
    );
    check_seeks(
        &file,
        1,
        &tb,
        0,
        0,
        &targets(tb.last().unwrap().1, 60, 42),
        reach_second_link,
    );
    // With the dense index, both links are known up front.
    let index = |d: &mut OggDemuxer| d.build_seek_index().expect("index");
    check_seeks(
        &file,
        0,
        &ta,
        0,
        0,
        &targets(ta.last().unwrap().1, 60, 43),
        index,
    );
    check_seeks(
        &file,
        1,
        &tb,
        0,
        0,
        &targets(tb.last().unwrap().1, 60, 44),
        index,
    );
}

#[test]
fn landing_on_a_page_that_opens_with_a_continued_packet() {
    // Every content page ends with the first 255 bytes of a packet that
    // finishes at the start of the next page, so every landing page
    // begins with a continuation whose head lies before the landing
    // point. That tail must be dropped silently — it is not damage.
    let serial = 0xC0_7100;
    let mut t = Track::new(serial);
    let h = vorbis_headers(44_100);
    t.header(true, &h[0]);
    t.header(false, &h[1]);
    t.header(false, &h[2]);
    let n = 600usize;
    for i in 0..n {
        let first = i == 0;
        let last = i + 1 == n;
        let mut lacing = Vec::new();
        let mut data = Vec::new();
        if !first {
            lacing.push(40); // tail of the packet begun on the previous page
            data.extend([0x77u8; 40]);
        }
        lacing.extend(lace(500));
        data.extend(vec![i as u8; 500]);
        if !last {
            lacing.push(255); // head of a packet that continues
            data.extend([0x11u8; 255]);
        }
        let mut f = if first { 0 } else { flags::CONTINUED };
        if last {
            f |= flags::LAST_PAGE;
        }
        let granule = 2048 * (i as i64 + 1);
        t.push(granule as f64 / 44_100.0, f, granule, lacing, data);
    }
    let file = Arc::new(multiplex(vec![t]));
    let io = Arc::new(Io::default());
    let mut s = 77u32;
    for _ in 0..100 {
        let page = 1 + rng(&mut s) as usize % (n - 2);
        let pts = 2048 * (page as i64 + 1) + 5;
        let mut d = open(&file, &io);
        assert_eq!(d.seek_to(0, pts).unwrap(), 2048 * (page as i64 + 1));
        // First packet out: the 500-byte packet that starts on the
        // landing page (the 40-byte tail before it was dropped).
        let p = d.next_packet().unwrap();
        assert_eq!(p.data, vec![page as u8; 500]);
        assert_eq!(p.pts, Some(2048 * (page as i64 + 1)));
        // Next: the packet spanning into the following page, whole.
        let p = d.next_packet().unwrap();
        assert_eq!(p.data.len(), 255 + 40);
        assert_eq!(d.framing_error_count(), 0);
        assert_eq!(d.hole_count(), 0);
        assert_eq!(d.damage_event_total(), 0);
    }
}
