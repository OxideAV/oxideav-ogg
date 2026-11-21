//! One-hour throughput / seek-cost measurement for the Ogg muxer and
//! demuxer.
//!
//! Synthesises two one-hour packet streams entirely in memory — a
//! Vorbis-shaped one (44.1 kHz, alternating short / long block advances,
//! ~128 kbit/s) and an Opus-shaped one (48 kHz, 20 ms packets,
//! ~64 kbit/s, non-zero pre-skip) — then reports, per stream:
//!
//! * mux wall time and packets/s (into a `Vec<u8>` sink);
//! * `open` wall time and the I/O it performed;
//! * linear demux packets/s and MiB/s;
//! * peak heap in use while demuxing (a counting global allocator; the
//!   in-memory input file itself is excluded);
//! * for 200 pseudo-random seeks on a freshly opened demuxer: mean and
//!   worst number of `read` calls, bytes read and `seek` calls that
//!   reached the underlying source, and whether every seek landed on
//!   the last page ending at or before the target.
//!
//! The payload bytes are pseudo-random filler: the container never
//! interprets packet bodies, so only sizes and granule advances matter.
//!
//! Run with:
//!     cargo run --release --example throughput

use std::alloc::{GlobalAlloc, Layout, System};
use std::io::{Cursor, Read, Seek, SeekFrom, Write};
use std::sync::atomic::{AtomicU64, AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Instant;

use oxideav_core::{
    CodecId, CodecParameters, Demuxer, Error, NullCodecResolver, Packet, ReadSeek, StreamInfo,
    TimeBase, WriteSeek,
};

// ───────────────────────── heap accounting ─────────────────────────

struct Counting;

static LIVE: AtomicUsize = AtomicUsize::new(0);
static PEAK: AtomicUsize = AtomicUsize::new(0);
static ALLOCS: AtomicU64 = AtomicU64::new(0);

unsafe impl GlobalAlloc for Counting {
    unsafe fn alloc(&self, layout: Layout) -> *mut u8 {
        let p = unsafe { System.alloc(layout) };
        if !p.is_null() {
            let live = LIVE.fetch_add(layout.size(), Ordering::Relaxed) + layout.size();
            PEAK.fetch_max(live, Ordering::Relaxed);
            ALLOCS.fetch_add(1, Ordering::Relaxed);
        }
        p
    }
    unsafe fn dealloc(&self, ptr: *mut u8, layout: Layout) {
        unsafe { System.dealloc(ptr, layout) };
        LIVE.fetch_sub(layout.size(), Ordering::Relaxed);
    }
    unsafe fn realloc(&self, ptr: *mut u8, layout: Layout, new_size: usize) -> *mut u8 {
        let p = unsafe { System.realloc(ptr, layout, new_size) };
        if !p.is_null() {
            if new_size >= layout.size() {
                let d = new_size - layout.size();
                let live = LIVE.fetch_add(d, Ordering::Relaxed) + d;
                PEAK.fetch_max(live, Ordering::Relaxed);
            } else {
                LIVE.fetch_sub(layout.size() - new_size, Ordering::Relaxed);
            }
            ALLOCS.fetch_add(1, Ordering::Relaxed);
        }
        p
    }
}

#[global_allocator]
static GLOBAL: Counting = Counting;

// ───────────────────────── I/O accounting ─────────────────────────

#[derive(Default)]
struct IoCounters {
    reads: AtomicU64,
    bytes: AtomicU64,
    seeks: AtomicU64,
}

impl IoCounters {
    fn snapshot(&self) -> (u64, u64, u64) {
        (
            self.reads.load(Ordering::Relaxed),
            self.bytes.load(Ordering::Relaxed),
            self.seeks.load(Ordering::Relaxed),
        )
    }
}

/// A shared read-only view of the file that counts every `read` and
/// every `seek` that actually moves the position (position queries —
/// `SeekFrom::Current(0)` — are free on any real source and not
/// counted).
struct CountingReader {
    data: Arc<Vec<u8>>,
    pos: u64,
    io: Arc<IoCounters>,
}

impl Read for CountingReader {
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

impl Seek for CountingReader {
    fn seek(&mut self, to: SeekFrom) -> std::io::Result<u64> {
        let new = match to {
            SeekFrom::Start(o) => o as i128,
            SeekFrom::Current(d) => self.pos as i128 + d as i128,
            SeekFrom::End(d) => self.data.len() as i128 + d as i128,
        };
        if new < 0 {
            return Err(std::io::Error::other("seek before start"));
        }
        if !matches!(to, SeekFrom::Current(0)) {
            self.io.seeks.fetch_add(1, Ordering::Relaxed);
        }
        self.pos = new as u64;
        Ok(self.pos)
    }
}

#[derive(Clone, Default)]
struct SharedSink(Arc<Mutex<Cursor<Vec<u8>>>>);

impl Write for SharedSink {
    fn write(&mut self, b: &[u8]) -> std::io::Result<usize> {
        self.0.lock().unwrap().write(b)
    }
    fn flush(&mut self) -> std::io::Result<()> {
        Ok(())
    }
}

impl Seek for SharedSink {
    fn seek(&mut self, p: SeekFrom) -> std::io::Result<u64> {
        self.0.lock().unwrap().seek(p)
    }
}

// ───────────────────────── stream synthesis ─────────────────────────

fn xorshift(state: &mut u32) -> u32 {
    *state ^= *state << 13;
    *state ^= *state >> 17;
    *state ^= *state << 5;
    *state
}

fn xiph_lace_three(packets: [&[u8]; 3]) -> Vec<u8> {
    let mut out = vec![0x02];
    for p in &packets[..2] {
        let mut n = p.len();
        while n >= 255 {
            out.push(255);
            n -= 255;
        }
        out.push(n as u8);
    }
    for p in packets {
        out.extend_from_slice(p);
    }
    out
}

fn vorbis_stream() -> StreamInfo {
    let mut id = vec![0x01];
    id.extend_from_slice(b"vorbis");
    id.extend_from_slice(&0u32.to_le_bytes());
    id.push(2);
    id.extend_from_slice(&44_100u32.to_le_bytes());
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
    // A realistically sized setup header (codebooks are a few kB).
    let mut s = 0x5E7u32;
    setup.extend((0..4000).map(|_| xorshift(&mut s) as u8));
    let mut params = CodecParameters::audio(CodecId::new("vorbis"));
    params.channels = Some(2);
    params.sample_rate = Some(44_100);
    params.extradata = xiph_lace_three([&id, &comment, &setup]);
    StreamInfo {
        index: 0,
        time_base: TimeBase::new(1, 44_100),
        duration: None,
        start_time: Some(0),
        params,
    }
}

fn opus_stream(pre_skip: u16) -> StreamInfo {
    let mut head = Vec::with_capacity(19);
    head.extend_from_slice(b"OpusHead");
    head.push(1);
    head.push(2);
    head.extend_from_slice(&pre_skip.to_le_bytes());
    head.extend_from_slice(&48_000u32.to_le_bytes());
    head.extend_from_slice(&0i16.to_le_bytes());
    head.push(0);
    let mut params = CodecParameters::audio(CodecId::new("opus"));
    params.channels = Some(2);
    params.sample_rate = Some(48_000);
    params.extradata = head;
    StreamInfo {
        index: 0,
        time_base: TimeBase::new(1, 48_000),
        duration: None,
        start_time: Some(0),
        params,
    }
}

/// One hour of `(payload, end granule)` packets.
fn synth_packets(kind: &str) -> Vec<(Vec<u8>, i64)> {
    let mut s = 0xC0FFEEu32;
    let mut out = Vec::new();
    let mut granule = 0i64;
    match kind {
        "vorbis" => {
            // Short (256) and long (2048) blocks; the advance between two
            // packets is a quarter of each neighbour's block size.
            let mut prev = 256i64;
            let end = 3600 * 44_100i64;
            while granule < end {
                let long = !xorshift(&mut s).is_multiple_of(8);
                let cur = if long { 2048 } else { 256 };
                granule += prev / 4 + cur / 4;
                prev = cur;
                let len = if long {
                    300 + (xorshift(&mut s) % 200) as usize
                } else {
                    40 + (xorshift(&mut s) % 40) as usize
                };
                out.push(((0..len).map(|_| xorshift(&mut s) as u8).collect(), granule));
            }
        }
        _ => {
            granule = 312;
            let end = 3600 * 48_000i64 + 312;
            while granule < end {
                granule += 960;
                let len = 120 + (xorshift(&mut s) % 80) as usize;
                out.push(((0..len).map(|_| xorshift(&mut s) as u8).collect(), granule));
            }
        }
    }
    out
}

fn mux(stream: &StreamInfo, packets: &[(Vec<u8>, i64)]) -> Vec<u8> {
    let sink = SharedSink::default();
    let out: Box<dyn WriteSeek> = Box::new(sink.clone());
    let mut m = oxideav_ogg::mux::open(out, std::slice::from_ref(stream)).unwrap();
    m.write_header().unwrap();
    let mut pkt = Packet::new(0, stream.time_base, Vec::new());
    pkt.flags.keyframe = true;
    for (data, granule) in packets {
        pkt.data.clear();
        pkt.data.extend_from_slice(data);
        pkt.pts = Some(*granule);
        pkt.dts = Some(*granule);
        m.write_packet(&pkt).unwrap();
    }
    m.write_trailer().unwrap();
    drop(m);
    let mut guard = sink.0.lock().unwrap();
    std::mem::take(guard.get_mut())
}

fn open(data: &Arc<Vec<u8>>, io: &Arc<IoCounters>) -> oxideav_ogg::demux::OggDemuxer {
    let input: Box<dyn ReadSeek> = Box::new(CountingReader {
        data: data.clone(),
        pos: 0,
        io: io.clone(),
    });
    oxideav_ogg::demux::open_concrete(input, &NullCodecResolver).unwrap()
}

fn median(mut v: Vec<f64>) -> f64 {
    v.sort_by(|a, b| a.partial_cmp(b).unwrap());
    v[v.len() / 2]
}

/// `(offset, granule)` of every page carrying a granule, by a plain
/// header walk of the muxed bytes — the oracle the seeks are checked
/// against.
fn page_table(data: &[u8]) -> Vec<(u64, i64)> {
    let mut out = Vec::new();
    let mut off = 0usize;
    while off + 27 <= data.len() {
        let n = data[off + 26] as usize;
        let body: usize = data[off + 27..off + 27 + n]
            .iter()
            .map(|&v| v as usize)
            .sum();
        let g = i64::from_le_bytes(data[off + 6..off + 14].try_into().unwrap());
        if g >= 0 {
            out.push((off as u64, g));
        }
        off += 27 + n + body;
    }
    out
}

fn run(kind: &str) {
    let (stream, pre_skip, preroll, header_pages) = match kind {
        "vorbis" => (vorbis_stream(), 0i64, 0i64, 3usize),
        _ => (opus_stream(312), 312i64, 3840i64, 2usize),
    };
    let packets = synth_packets(kind);
    let n_packets = packets.len();

    // Mux: median of 5.
    let mut times = Vec::new();
    let mut file = Vec::new();
    let mut mux_peak = 0usize;
    for _ in 0..5 {
        let base = LIVE.load(Ordering::Relaxed);
        PEAK.store(base, Ordering::Relaxed);
        let t = Instant::now();
        file = mux(&stream, &packets);
        times.push(t.elapsed().as_secs_f64());
        // The sink itself (the whole output file) is part of the peak;
        // subtract its final capacity to isolate the muxer's own state.
        mux_peak = PEAK
            .load(Ordering::Relaxed)
            .saturating_sub(base)
            .saturating_sub(file.capacity());
    }
    let mux_s = median(times);
    drop(packets);
    let file = Arc::new(file);
    let pages = page_table(&file);
    println!(
        "[{kind}] 1 h: {n_packets} packets, {:.1} MiB, {} granule-bearing pages",
        file.len() as f64 / 1048576.0,
        pages.len()
    );
    println!(
        "[{kind}] mux: {:.1} ms ({:.2} M packets/s), muxer peak heap beyond the sink ~{} KiB",
        mux_s * 1e3,
        n_packets as f64 / mux_s / 1e6,
        mux_peak / 1024
    );

    // Open: median of 9.
    let io = Arc::new(IoCounters::default());
    let mut times = Vec::new();
    let mut open_io = (0, 0, 0);
    for _ in 0..9 {
        let before = io.snapshot();
        let t = Instant::now();
        let d = open(&file, &io);
        times.push(t.elapsed().as_secs_f64());
        let after = io.snapshot();
        open_io = (after.0 - before.0, after.1 - before.1, after.2 - before.2);
        drop(d);
    }
    println!(
        "[{kind}] open: {:.1} us, {} reads / {} bytes / {} seeks",
        median(times) * 1e6,
        open_io.0,
        open_io.1,
        open_io.2
    );

    // Whole-file index scan: median of 5.
    let mut times = Vec::new();
    let mut entries = 0usize;
    let mut scan_io = (0, 0, 0);
    for _ in 0..5 {
        let mut d = open(&file, &io);
        let before = io.snapshot();
        let t = Instant::now();
        d.build_seek_index().unwrap();
        times.push(t.elapsed().as_secs_f64());
        let after = io.snapshot();
        scan_io = (after.0 - before.0, after.1 - before.1, after.2 - before.2);
        entries = d.seek_index_len();
    }
    println!(
        "[{kind}] build_seek_index: {:.2} ms, {entries} entries, {} reads / {} seeks",
        median(times) * 1e3,
        scan_io.0,
        scan_io.2
    );

    // Linear demux: median of 5.
    let mut times = Vec::new();
    let mut got = 0usize;
    let mut peak = 0usize;
    let mut allocs = 0u64;
    let mut demux_io = (0, 0, 0);
    for _ in 0..5 {
        let before = io.snapshot();
        let base = LIVE.load(Ordering::Relaxed);
        PEAK.store(base, Ordering::Relaxed);
        let a0 = ALLOCS.load(Ordering::Relaxed);
        let t = Instant::now();
        let mut d = open(&file, &io);
        got = 0;
        loop {
            match d.next_packet() {
                Ok(p) => {
                    got += 1;
                    std::hint::black_box(&p);
                }
                Err(Error::Eof) => break,
                Err(e) => panic!("demux error: {e}"),
            }
        }
        times.push(t.elapsed().as_secs_f64());
        peak = PEAK.load(Ordering::Relaxed) - base;
        allocs = ALLOCS.load(Ordering::Relaxed) - a0;
        let after = io.snapshot();
        demux_io = (after.0 - before.0, after.1 - before.1, after.2 - before.2);
    }
    assert_eq!(got, n_packets, "every packet must come back");
    let s = median(times);
    println!(
        "[{kind}] demux: {:.1} ms ({:.2} M packets/s, {:.0} MiB/s), peak heap {} KiB, {:.2} allocations/packet, {} reads / {} seeks",
        s * 1e3,
        got as f64 / s / 1e6,
        file.len() as f64 / 1048576.0 / s,
        peak / 1024,
        allocs as f64 / got as f64,
        demux_io.0,
        demux_io.2
    );

    // Seeks: fresh demuxer per seek (no warm index), 200 targets.
    let last = pages.last().unwrap().1;
    let mut rng = 0xFEEDu32;
    let (mut sum_r, mut sum_b, mut sum_s) = (0u64, 0u64, 0u64);
    let (mut max_r, mut max_b) = (0u64, 0u64);
    let mut exact = 0usize;
    let mut times = Vec::new();
    const N: usize = 200;
    for _ in 0..N {
        let target_granule = (xorshift(&mut rng) as i64 * 7919) % last;
        let pts = target_granule - pre_skip;
        let mut d = open(&file, &io);
        let before = io.snapshot();
        let t = Instant::now();
        let landed = d.seek_to(0, pts).unwrap();
        times.push(t.elapsed().as_secs_f64());
        let after = io.snapshot();
        let (r, b, s) = (after.0 - before.0, after.1 - before.1, after.2 - before.2);
        sum_r += r;
        sum_b += b;
        sum_s += s;
        max_r = max_r.max(r);
        max_b = max_b.max(b);
        // Oracle: the last page whose granule is <= the target (or the
        // first granule-bearing data page when the target precedes it).
        // (An Opus seek lands 80 ms ahead of the target.)
        let search = target_granule - preroll;
        let i = pages.partition_point(|&(_, g)| g <= search);
        // Header pages (granule 0) are never a landing point: a target
        // before the first content page lands on that page.
        let want = pages[i.saturating_sub(1).max(header_pages)].1;
        if landed == want {
            exact += 1;
        }
    }
    println!(
        "[{kind}] seek x{N} (cold): mean {:.1} reads / {:.0} KiB / {:.1} seeks, worst {} reads / {} KiB, median {:.1} us, exact {exact}/{N}",
        sum_r as f64 / N as f64,
        sum_b as f64 / N as f64 / 1024.0,
        sum_s as f64 / N as f64,
        max_r,
        max_b / 1024,
        median(times) * 1e6
    );
}

fn main() {
    run("vorbis");
    run("opus");
}
