//! Criterion benchmarks for the Ogg framing hot paths.
//!
//! Round 172 (depth-mode benchmarks): `oxideav-ogg` has been
//! feature-complete since v0.1.4 — page framing, CRC-32, packet
//! reassembly, multi-stream demux, chained-link duration, page-loss
//! holes, continued-flag framing-consistency, page-sync recapture,
//! standalone CRC helpers, and a four-target cargo-fuzz harness all
//! landed. Per the workspace "saturated → fuzz/bench/profile" memo,
//! this round wires up `criterion` benches so future optimisation
//! rounds can A/B-test their changes against fixed scenarios.
//!
//! Everything is **self-contained**: every byte fed into a measured
//! routine is synthesised in-bench (either with the crate's own
//! `Page::to_bytes` serializer or via the muxer driving a synthetic
//! Vorbis stream). No `docs/` fixtures or external `.ogg` files are
//! read, so the harness runs from a fresh checkout with no extra
//! setup.
//!
//! ## Scenarios
//!
//! - **`crc/checksum/{64,4096,65536}`** — raw `crc::checksum` over a
//!   xorshift-filled byte slice of N bytes. Throughput in bytes/sec
//!   measures the polynomial-0x04C11DB7 table-lookup loop in
//!   isolation.
//! - **`crc/validate_page_crc/{small,large}`** — the
//!   public `validate_page_crc` helper over a complete page (header +
//!   segment table + body) at two representative sizes (one-segment
//!   short page; max-segment 255×255 ≈ 65 KiB page). Exercises the
//!   "treat bytes 22..26 as zero" branch the standalone helper takes.
//! - **`page/parse/{short,multi_segment,max}`** — `Page::parse` on
//!   pre-built page bytes (one short packet; one packet spanning 3
//!   segments; one full-255-segment page with each segment 255 bytes,
//!   the largest single page Ogg permits).
//! - **`page/to_bytes/{short,multi_segment,max}`** — `Page::to_bytes`
//!   on the same three pages.
//! - **`page/lace/{short,exact_255,large}`** — `page::lace` builder
//!   for packet lengths 100 / 255 / 65 280 bytes (the exact-multiple-
//!   of-255 case forces the zero-terminator append).
//! - **`demux/walk/vorbis_12pkt`** — open + drain a 12-packet
//!   synthetic Vorbis stream via `demux::open` + `next_packet` to EOF.
//!   This is the end-to-end "consume one second of audio" headline
//!   number future micro-optimisations on the page-reader →
//!   reassembly path should keep an eye on.
//! - **`demux/build_index/vorbis_12pkt`** — `open_concrete` + an
//!   explicit `build_seek_index` over the same blob, measuring the
//!   page-header scan (no payload reads) that powers O(log n) seeks.
//! - **`skeleton/fishead/{parse,to_bytes}`** — Skeleton 4.0
//!   `fishead\0` ident packet (80 bytes) parse + serialize. Covers the
//!   little-endian rational decoding (presentation time, basetime) and
//!   the 4.0-only segment-length / content-byte-offset trailing
//!   fields.
//! - **`skeleton/fisbone/{parse,to_bytes}`** — Skeleton `fisbone\0`
//!   secondary header packet carrying the three compulsory 4.0
//!   message-header fields (`Content-Type`, `Role`, `Name`) plus a
//!   `Title` extension. Hot path for the demuxer's per-content-stream
//!   metadata pickup.
//! - **`skeleton/index/{parse,to_bytes,parse_512kp}`** — Skeleton 4.0
//!   `index\0` keyframe-index packet at three sizes: a 4-keypoint
//!   smoke index (one variable-byte-integer pair per entry), a
//!   64-keypoint index, and a 512-keypoint index that drives the
//!   `read_vbi_u64` / `write_vbi_u64` codec across the full encoder
//!   range. This is the headline scenario for index-accelerated
//!   `seek_to` setup cost on a long-form file.
//! - **`skeleton/vbi/{write,read}`** — raw `write_vbi_u64` and
//!   `read_vbi_u64` over a u64 value derived from a deterministic
//!   xorshift, in isolation, so the encoder-side and decoder-side
//!   throughput of the variable-byte-integer codec is measurable
//!   independently of the index packet wrapping.
//!
//! Run with:
//!     cargo bench -p oxideav-ogg --bench framing

use std::io::{Cursor, Seek, Write};
use std::sync::{Arc, Mutex};

use criterion::{black_box, criterion_group, criterion_main, BenchmarkId, Criterion, Throughput};

use oxideav_core::{CodecId, CodecParameters, Packet, ReadSeek, StreamInfo, TimeBase, WriteSeek};
use oxideav_ogg::crc::{self, validate_page_crc};
use oxideav_ogg::page::{self, flags, Page};
use oxideav_ogg::skeleton::{
    self, FisBone, FisHead, Rational as SkRational, SkelIndex, Version as SkVersion,
};

/// Cheap deterministic xorshift32 — fills test buffers with non-zero
/// non-DC bytes so the CRC loop has to do real table lookups (a pure-
/// zero input would hide nothing but still doesn't reflect realistic
/// page contents).
fn xorshift32(state: &mut u32) -> u32 {
    *state ^= *state << 13;
    *state ^= *state >> 17;
    *state ^= *state << 5;
    *state
}

fn filled_bytes(n: usize, seed: u32) -> Vec<u8> {
    let mut s = seed.max(1);
    let mut out = Vec::with_capacity(n);
    while out.len() < n {
        let w = xorshift32(&mut s);
        out.extend_from_slice(&w.to_le_bytes());
    }
    out.truncate(n);
    out
}

/// Build a single Ogg page that carries one packet of `payload_len`
/// bytes. The page header gets a deterministic granule / serial /
/// seq_no so every bench iteration sees identical byte content.
fn build_single_packet_page(payload_len: usize, seq_no: u32) -> Vec<u8> {
    let data = filled_bytes(payload_len, 0xC0DE_FACE ^ seq_no);
    let lacing = page::lace(payload_len);
    let p = Page {
        flags: flags::FIRST_PAGE,
        granule_position: 1_000 + seq_no as i64,
        serial: 0x1234_5678,
        seq_no,
        lacing,
        data,
    };
    p.to_bytes()
}

/// Build the maximum-size single page: 255 segments × 255 bytes
/// each = 65 025 bytes of body + 27 + 255 header = 65 307 bytes.
/// This forces the parser through its largest legal segment table.
fn build_max_page() -> Vec<u8> {
    let lacing = vec![255u8; 255];
    let data = filled_bytes(255 * 255, 0xDEAD_BEEF);
    let p = Page {
        flags: 0,
        granule_position: 999_999,
        serial: 0xAABB_CCDD,
        seq_no: 42,
        lacing,
        data,
    };
    p.to_bytes()
}

// ---------------------------------------------------------------------------
// Vorbis-stream synthesis (mirrors tests/page_crc.rs so the bench is
// self-contained — building the bytes inside the bench setup avoids
// committing any binary fixture).
// ---------------------------------------------------------------------------

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

#[derive(Clone, Default)]
struct SharedBuf(Arc<Mutex<Cursor<Vec<u8>>>>);

impl Write for SharedBuf {
    fn write(&mut self, b: &[u8]) -> std::io::Result<usize> {
        self.0.lock().unwrap().write(b)
    }
    fn flush(&mut self) -> std::io::Result<()> {
        self.0.lock().unwrap().flush()
    }
}

impl Seek for SharedBuf {
    fn seek(&mut self, p: std::io::SeekFrom) -> std::io::Result<u64> {
        self.0.lock().unwrap().seek(p)
    }
}

/// Build a 12-data-packet Vorbis Ogg blob via the muxer. Returns the
/// owned byte vector. Mirrors `tests/page_crc.rs::build_vorbis_ogg`.
fn build_vorbis_ogg() -> Vec<u8> {
    let id = vorbis_id_packet(2, 48_000);
    let com = vorbis_comment_packet();
    let setup = vorbis_setup_packet();
    let extradata = xiph_lace_three(&[&id, &com, &setup]);

    let mut params = CodecParameters::audio(CodecId::new("vorbis"));
    params.channels = Some(2);
    params.sample_rate = Some(48_000);
    params.extradata = extradata;
    let stream = StreamInfo {
        index: 0,
        time_base: TimeBase::new(1, 48_000),
        duration: None,
        start_time: Some(0),
        params,
    };

    let shared = SharedBuf::default();
    let writer: Box<dyn WriteSeek> = Box::new(shared.clone());
    let mut muxer = oxideav_ogg::mux::open(writer, std::slice::from_ref(&stream)).unwrap();
    muxer.write_header().unwrap();
    for i in 1..=12i64 {
        let payload: Vec<u8> = (0..((i as usize) * 17 + 3))
            .map(|j| (j as u8) ^ 0x5A)
            .collect();
        let granule = 960 * i;
        let mut pkt = Packet::new(0, stream.time_base, payload);
        pkt.pts = Some(granule);
        pkt.dts = Some(granule);
        pkt.flags.keyframe = true;
        pkt.flags.unit_boundary = true;
        muxer.write_packet(&pkt).unwrap();
    }
    muxer.write_trailer().unwrap();
    drop(muxer);
    let guard = shared.0.lock().unwrap();
    guard.get_ref().clone()
}

// ---------------------------------------------------------------------------
// Bench groups
// ---------------------------------------------------------------------------

fn bench_crc(c: &mut Criterion) {
    let mut g = c.benchmark_group("crc");
    for &n in &[64usize, 4096, 65536] {
        let buf = filled_bytes(n, 0x1234_5678);
        g.throughput(Throughput::Bytes(n as u64));
        g.bench_with_input(BenchmarkId::new("checksum", n), &buf, |b, buf| {
            b.iter(|| crc::checksum(black_box(buf.as_slice())));
        });
    }

    let short_page = build_single_packet_page(128, 1);
    let max_page = build_max_page();
    g.throughput(Throughput::Bytes(short_page.len() as u64));
    g.bench_function(BenchmarkId::new("validate_page_crc", "short"), |b| {
        b.iter(|| validate_page_crc(black_box(&short_page)));
    });
    g.throughput(Throughput::Bytes(max_page.len() as u64));
    g.bench_function(BenchmarkId::new("validate_page_crc", "max"), |b| {
        b.iter(|| validate_page_crc(black_box(&max_page)));
    });
    g.finish();
}

fn bench_page(c: &mut Criterion) {
    let short = build_single_packet_page(128, 1);
    // Multi-segment: 600-byte packet → lacing [255, 255, 90].
    let multi = build_single_packet_page(600, 2);
    let max = build_max_page();

    let mut g = c.benchmark_group("page");

    // parse
    g.throughput(Throughput::Bytes(short.len() as u64));
    g.bench_function(BenchmarkId::new("parse", "short"), |b| {
        b.iter(|| Page::parse(black_box(&short)).unwrap());
    });
    g.throughput(Throughput::Bytes(multi.len() as u64));
    g.bench_function(BenchmarkId::new("parse", "multi_segment"), |b| {
        b.iter(|| Page::parse(black_box(&multi)).unwrap());
    });
    g.throughput(Throughput::Bytes(max.len() as u64));
    g.bench_function(BenchmarkId::new("parse", "max"), |b| {
        b.iter(|| Page::parse(black_box(&max)).unwrap());
    });

    // to_bytes — pre-build a Page once, then serialize each iter.
    let (short_p, _) = Page::parse(&short).unwrap();
    let (multi_p, _) = Page::parse(&multi).unwrap();
    let (max_p, _) = Page::parse(&max).unwrap();
    g.throughput(Throughput::Bytes(short.len() as u64));
    g.bench_function(BenchmarkId::new("to_bytes", "short"), |b| {
        b.iter(|| black_box(&short_p).to_bytes());
    });
    g.throughput(Throughput::Bytes(multi.len() as u64));
    g.bench_function(BenchmarkId::new("to_bytes", "multi_segment"), |b| {
        b.iter(|| black_box(&multi_p).to_bytes());
    });
    g.throughput(Throughput::Bytes(max.len() as u64));
    g.bench_function(BenchmarkId::new("to_bytes", "max"), |b| {
        b.iter(|| black_box(&max_p).to_bytes());
    });

    // lace
    for &(label, n) in &[("short", 100usize), ("exact_255", 255), ("large", 65_280)] {
        g.bench_function(BenchmarkId::new("lace", label), |b| {
            b.iter(|| page::lace(black_box(n)));
        });
    }
    g.finish();
}

fn bench_demux(c: &mut Criterion) {
    let blob = build_vorbis_ogg();
    let mut g = c.benchmark_group("demux");
    g.throughput(Throughput::Bytes(blob.len() as u64));
    g.bench_function(BenchmarkId::new("walk", "vorbis_12pkt"), |b| {
        b.iter(|| {
            let reader: Box<dyn ReadSeek> = Box::new(Cursor::new(blob.clone()));
            let mut demux =
                oxideav_ogg::demux::open(reader, &oxideav_core::NullCodecResolver).unwrap();
            let mut n = 0u64;
            while demux.next_packet().is_ok() {
                n += 1;
            }
            black_box(n)
        });
    });
    g.bench_function(BenchmarkId::new("build_index", "vorbis_12pkt"), |b| {
        b.iter(|| {
            let reader: Box<dyn ReadSeek> = Box::new(Cursor::new(blob.clone()));
            let mut demux =
                oxideav_ogg::demux::open_concrete(reader, &oxideav_core::NullCodecResolver)
                    .unwrap();
            demux.build_seek_index().unwrap();
            black_box(demux.seek_index_len())
        });
    });
    g.finish();
}

// ---------------------------------------------------------------------------
// Skeleton bench-input builders.
// ---------------------------------------------------------------------------

/// Build a Skeleton 4.0 `fishead\0` ident packet (80 bytes) with the
/// trailing `segment_length` / `content_byte_offset` fields filled in
/// so the bench exercises the 4.0 branch of `FisHead::parse`.
fn build_fishead_4_0() -> Vec<u8> {
    let mut h = FisHead::new(SkVersion::V4_0);
    h.presentation_time = SkRational::new(0, 1_000);
    h.basetime = SkRational::new(0, 1_000);
    h.segment_length = Some(0x1234_5678_9ABC_DEF0);
    h.content_byte_offset = Some(0x4096);
    h.utc = *b"20260601T000000.000Z";
    h.to_bytes()
}

/// Build a Skeleton `fisbone\0` packet carrying the three compulsory
/// 4.0 message-header fields plus a `Title` extension. Mirrors what
/// `tests/skeleton.rs` already builds, but exercised in the bench
/// harness so the metadata pickup hot path is measurable.
fn build_fisbone() -> Vec<u8> {
    let mut b = FisBone::new(0xCAFE_BABE, SkRational::new(48_000, 1));
    b.num_headers = 3;
    b.preroll = 2;
    b.granuleshift = 0;
    b.set_header("Content-Type", "audio/vorbis");
    b.set_header("Role", "audio/main");
    b.set_header("Name", "track 1");
    b.set_header("Title", "Bench Track");
    b.to_bytes()
}

/// Build a Skeleton 4.0 `index\0` keyframe-index packet with `n`
/// keypoints. Each keypoint advances `(offset, timestamp)` by a
/// pseudo-random delta so the encoded variable-byte-integer pairs
/// cover the 1..10-byte range of the codec.
fn build_index(n: usize) -> Vec<u8> {
    let mut idx = SkelIndex::new(0xCAFE_BABE, 1_000);
    let mut state = 0xC0DE_FACEu32;
    let mut off: u64 = 0;
    let mut ts: i64 = 0;
    for _ in 0..n {
        let d = xorshift32(&mut state);
        // Mix in a 16..40-bit step so VBI lengths exercise 2..6 bytes
        // routinely and the occasional 8..10-byte encoding lands too.
        let off_step = 0x4000 + (d as u64 & 0xFFFF_FFFF);
        let ts_step = 960 + (d as i64 & 0xFFFF);
        off = off.wrapping_add(off_step);
        ts = ts.wrapping_add(ts_step);
        idx.push(off, ts);
    }
    idx.to_bytes()
}

fn bench_skeleton(c: &mut Criterion) {
    let mut g = c.benchmark_group("skeleton");

    let fishead = build_fishead_4_0();
    let fishead_decoded = FisHead::parse(&fishead).expect("parse fishead");
    g.throughput(Throughput::Bytes(fishead.len() as u64));
    g.bench_function(BenchmarkId::new("fishead", "parse"), |b| {
        b.iter(|| FisHead::parse(black_box(&fishead)).unwrap());
    });
    g.bench_function(BenchmarkId::new("fishead", "to_bytes"), |b| {
        b.iter(|| black_box(&fishead_decoded).to_bytes());
    });

    let fisbone = build_fisbone();
    let fisbone_decoded = FisBone::parse(&fisbone).expect("parse fisbone");
    g.throughput(Throughput::Bytes(fisbone.len() as u64));
    g.bench_function(BenchmarkId::new("fisbone", "parse"), |b| {
        b.iter(|| FisBone::parse(black_box(&fisbone)).unwrap());
    });
    g.bench_function(BenchmarkId::new("fisbone", "to_bytes"), |b| {
        b.iter(|| black_box(&fisbone_decoded).to_bytes());
    });

    for &(label, n) in &[("4kp", 4usize), ("64kp", 64), ("512kp", 512)] {
        let bytes = build_index(n);
        let decoded = SkelIndex::parse(&bytes).expect("parse index");
        g.throughput(Throughput::Bytes(bytes.len() as u64));
        g.bench_function(BenchmarkId::new("index", format!("parse_{label}")), |b| {
            b.iter(|| SkelIndex::parse(black_box(&bytes)).unwrap());
        });
        g.bench_function(
            BenchmarkId::new("index", format!("to_bytes_{label}")),
            |b| {
                b.iter(|| black_box(&decoded).to_bytes());
            },
        );
    }

    // Raw variable-byte-integer codec — encode + decode in isolation,
    // measured over a deterministic xorshift-derived u64 so every
    // iteration touches the same value range.
    let mut state = 0xC0DE_FACEu32;
    let probe: u64 = ((xorshift32(&mut state) as u64) << 32) | xorshift32(&mut state) as u64;
    g.bench_function(BenchmarkId::new("vbi", "write"), |b| {
        b.iter(|| {
            let mut out = Vec::with_capacity(10);
            skeleton::write_vbi_u64(&mut out, black_box(probe));
            out
        });
    });
    let mut encoded = Vec::with_capacity(10);
    skeleton::write_vbi_u64(&mut encoded, probe);
    g.bench_function(BenchmarkId::new("vbi", "read"), |b| {
        b.iter(|| skeleton::read_vbi_u64(black_box(&encoded)).unwrap());
    });

    g.finish();
}

criterion_group!(benches, bench_crc, bench_page, bench_demux, bench_skeleton);
criterion_main!(benches);
