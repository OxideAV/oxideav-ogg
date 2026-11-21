//! Live / unseekable sources, bounded reassembly, and hostile garbage.
//!
//! * `open` + linear demux never seek the underlying source, so an
//!   input whose `seek` always fails (a pipe, a socket) demuxes exactly
//!   like a file — including across spliced garbage, a checksum-damaged
//!   page and a truncated tail (RFC 3533 §3 "recapture after a parsing
//!   error", §6 fields 1 and 7).
//! * The end of the input is read only when the caller asks for the
//!   duration.
//! * A packet continued over an endless run of pages is dropped at the
//!   reassembly cap instead of being buffered without bound.
//! * Garbage dense with capture patterns is crossed in a number of
//!   reads proportional to its length.

use std::io::{Read, Seek, SeekFrom};
use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};

use oxideav_core::{Demuxer, Error, NullCodecResolver, ReadSeek};
use oxideav_ogg::demux::{DamageKind, OggDemuxer};
use oxideav_ogg::page::{Page, flags, lace};

const SERIAL: u32 = 0x4C49_5645;

fn vorbis_id() -> Vec<u8> {
    let mut p = vec![0x01];
    p.extend_from_slice(b"vorbis");
    p.extend_from_slice(&0u32.to_le_bytes());
    p.push(2);
    p.extend_from_slice(&48_000u32.to_le_bytes());
    p.extend_from_slice(&0i32.to_le_bytes());
    p.extend_from_slice(&128_000i32.to_le_bytes());
    p.extend_from_slice(&0i32.to_le_bytes());
    p.push(0xB8);
    p.push(0x01);
    p
}

fn vorbis_comment() -> Vec<u8> {
    let mut p = vec![0x03];
    p.extend_from_slice(b"vorbis");
    p.extend_from_slice(&[0u8; 8]);
    p.push(0x01);
    p
}

fn vorbis_setup() -> Vec<u8> {
    let mut p = vec![0x05];
    p.extend_from_slice(b"vorbis");
    p.extend_from_slice(&[0x33; 40]);
    p
}

fn page(flags_byte: u8, granule: i64, seq: u32, packets: &[&[u8]]) -> Vec<u8> {
    let mut lacing = Vec::new();
    let mut data = Vec::new();
    for p in packets {
        lacing.extend(lace(p.len()));
        data.extend_from_slice(p);
    }
    Page {
        flags: flags_byte,
        granule_position: granule,
        serial: SERIAL,
        seq_no: seq,
        lacing,
        data,
    }
    .to_bytes()
}

/// Header pages, then `n` data pages of two packets each (granule
/// advancing 1024 per page). Returns the pages individually.
fn stream_pages(n: u32) -> Vec<Vec<u8>> {
    let mut out = vec![
        page(flags::FIRST_PAGE, 0, 0, &[&vorbis_id()]),
        page(0, 0, 1, &[&vorbis_comment()]),
        page(0, 0, 2, &[&vorbis_setup()]),
    ];
    for i in 0..n {
        let a = vec![(i % 251) as u8; 90 + (i as usize % 7) * 13];
        let b = vec![(i % 13) as u8 ^ 0x5A; 200];
        let f = if i + 1 == n { flags::LAST_PAGE } else { 0 };
        out.push(page(f, 1024 * (i as i64 + 1), 3 + i, &[&a, &b]));
    }
    out
}

/// A source that can only be read forward: every seek fails.
struct Pipe {
    data: Vec<u8>,
    pos: usize,
    /// Deliver at most this many bytes per `read` (short reads are
    /// normal on a socket).
    chunk: usize,
}

impl Read for Pipe {
    fn read(&mut self, buf: &mut [u8]) -> std::io::Result<usize> {
        let n = buf.len().min(self.chunk).min(self.data.len() - self.pos);
        buf[..n].copy_from_slice(&self.data[self.pos..self.pos + n]);
        self.pos += n;
        Ok(n)
    }
}

impl Seek for Pipe {
    fn seek(&mut self, _: SeekFrom) -> std::io::Result<u64> {
        Err(std::io::Error::other("not seekable"))
    }
}

#[derive(Default)]
struct Io {
    reads: AtomicU64,
    seeks: AtomicU64,
    end_seeks: AtomicU64,
}

/// A seekable source that counts what the demuxer does to it.
struct Counted {
    data: Vec<u8>,
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
        Ok(n)
    }
}

impl Seek for Counted {
    fn seek(&mut self, to: SeekFrom) -> std::io::Result<u64> {
        self.pos = match to {
            SeekFrom::Start(o) => o,
            SeekFrom::Current(d) => self.pos.checked_add_signed(d).unwrap(),
            SeekFrom::End(d) => {
                self.io.end_seeks.fetch_add(1, Ordering::Relaxed);
                (self.data.len() as u64).checked_add_signed(d).unwrap()
            }
        };
        if !matches!(to, SeekFrom::Current(0)) {
            self.io.seeks.fetch_add(1, Ordering::Relaxed);
        }
        Ok(self.pos)
    }
}

fn open(input: Box<dyn ReadSeek>) -> OggDemuxer {
    oxideav_ogg::demux::open_concrete(input, &NullCodecResolver).expect("open")
}

fn drain(d: &mut OggDemuxer) -> Vec<(Option<i64>, usize)> {
    let mut out = Vec::new();
    loop {
        match d.next_packet() {
            Ok(p) => out.push((p.pts, p.data.len())),
            Err(Error::Eof) => return out,
            Err(e) => panic!("demux error: {e}"),
        }
    }
}

#[test]
fn unseekable_source_demuxes_like_a_file() {
    let bytes: Vec<u8> = stream_pages(400).concat();
    let mut file = open(Box::new(std::io::Cursor::new(bytes.clone())));
    let want = drain(&mut file);
    assert_eq!(want.len(), 800);

    for chunk in [1usize, 7, 4096, 1 << 20] {
        let mut live = open(Box::new(Pipe {
            data: bytes.clone(),
            pos: 0,
            chunk,
        }));
        assert_eq!(live.streams().len(), 1);
        assert_eq!(drain(&mut live), want, "chunk {chunk}");
        assert_eq!(live.damage_event_total(), 0);
        // No way to learn the length of a pipe.
        assert_eq!(live.duration_micros(), None);
        // A seek is refused, not a panic.
        assert!(live.seek_to(0, 0).is_err());
    }
}

#[test]
fn unseekable_source_survives_garbage_corruption_and_truncation() {
    let pages = stream_pages(60);
    let mut bytes = Vec::new();
    for (i, p) in pages.iter().enumerate() {
        if i == 10 {
            // Junk between two pages, with a capture pattern in it.
            bytes.extend_from_slice(b"\x00\x01junk OggS not a page \xFF\xFE OggOggS");
        }
        if i == 20 {
            // A page whose body was damaged in transit.
            let mut bad = p.clone();
            let last = bad.len() - 1;
            bad[last] ^= 0x40;
            bytes.extend_from_slice(&bad);
            continue;
        }
        bytes.extend_from_slice(p);
    }
    // Cut the stream in the middle of its final page.
    bytes.truncate(bytes.len() - 100);

    let mut live = open(Box::new(Pipe {
        data: bytes,
        pos: 0,
        chunk: 333,
    }));
    let got = drain(&mut live);
    // 60 data pages, minus the damaged one and the truncated one.
    assert_eq!(got.len(), 58 * 2);
    assert_eq!(live.resync_count(), 2, "junk + damaged page");
    assert_eq!(live.hole_count(), 1, "the damaged page is a hole");
    let kinds: Vec<DamageKind> = live.damage_events().iter().map(|e| e.kind).collect();
    assert!(kinds.contains(&DamageKind::TruncatedTail), "{kinds:?}");
    // Granules stay monotone across the damage: nothing was glued.
    let pts: Vec<i64> = got.iter().filter_map(|p| p.0).collect();
    assert!(pts.windows(2).all(|w| w[0] < w[1]));
}

#[test]
fn open_reads_the_start_only_and_duration_is_on_request() {
    let bytes: Vec<u8> = stream_pages(3000).concat();
    let len = bytes.len();
    assert!(len > 600_000);
    let io = Arc::new(Io::default());
    let d = open(Box::new(Counted {
        data: bytes,
        pos: 0,
        io: io.clone(),
    }));
    assert_eq!(io.seeks.load(Ordering::Relaxed), 0, "open must not seek");
    assert!(
        io.reads.load(Ordering::Relaxed) <= 2,
        "open reads one window"
    );

    // 3000 pages × 1024 samples at 48 kHz = 64 s.
    assert_eq!(d.duration_micros(), Some(64_000_000));
    assert_eq!(io.end_seeks.load(Ordering::Relaxed), 1);
    let (reads, seeks) = (
        io.reads.load(Ordering::Relaxed),
        io.seeks.load(Ordering::Relaxed),
    );
    // Asking again costs nothing.
    assert_eq!(d.duration_micros(), Some(64_000_000));
    assert_eq!(io.reads.load(Ordering::Relaxed), reads);
    assert_eq!(io.seeks.load(Ordering::Relaxed), seeks);
}

#[test]
fn duration_trusts_only_checksummed_pages() {
    let mut bytes: Vec<u8> = stream_pages(50).concat();
    // Trailing junk shaped like a page header with a huge granule but no
    // valid checksum, plus a capture pattern inside it.
    let mut fake = page(flags::LAST_PAGE, i64::MAX / 2, 99, &[b"OggS fake"]);
    fake[24] ^= 0xFF;
    bytes.extend_from_slice(&fake);
    bytes.extend_from_slice(b"OggS");
    let d = open(Box::new(std::io::Cursor::new(bytes)));
    // 50 × 1024 / 48000 s.
    assert_eq!(d.duration_micros(), Some(50 * 1024 * 1_000_000 / 48_000));
}

#[test]
fn endless_continued_packet_is_dropped_at_the_cap() {
    let mut bytes: Vec<u8> = stream_pages(4)
        .into_iter()
        .map(|mut p| {
            // Clear the EOS flag of the last page and fix its checksum.
            if p[5] & flags::LAST_PAGE != 0 {
                p[5] &= !flags::LAST_PAGE;
                p[22..26].fill(0);
                let crc = oxideav_ogg::crc::checksum(&p);
                p[22..26].copy_from_slice(&crc.to_le_bytes());
            }
            p
        })
        .collect::<Vec<_>>()
        .concat();
    // One packet spread over 40 full pages (2.6 MB) that never ends
    // before the cap: every lacing value is 255.
    let mut seq = 7u32;
    for i in 0..40u32 {
        bytes.extend(
            Page {
                flags: if i == 0 { 0 } else { flags::CONTINUED },
                granule_position: -1,
                serial: SERIAL,
                seq_no: seq,
                lacing: vec![255; 255],
                data: vec![i as u8; 255 * 255],
            }
            .to_bytes(),
        );
        seq += 1;
    }
    // Its terminator, sharing a page with a normal packet.
    let mut lacing = vec![10u8];
    lacing.extend(lace(123));
    bytes.extend(
        Page {
            flags: flags::CONTINUED | flags::LAST_PAGE,
            granule_position: 9000,
            serial: SERIAL,
            seq_no: seq,
            lacing,
            data: vec![0xEE; 133],
        }
        .to_bytes(),
    );

    // Default cap (64 MiB): the 2.6 MB packet is delivered whole.
    let mut d = open(Box::new(std::io::Cursor::new(bytes.clone())));
    let got = drain(&mut d);
    assert_eq!(got.len(), 4 * 2 + 2);
    assert_eq!(got[8].1, 40 * 255 * 255 + 10);
    assert_eq!(d.oversized_packet_count(), 0);

    // 1 MiB cap: the packet is dropped once, quietly skipped to its end,
    // and the packet after it is intact.
    let mut d = open(Box::new(std::io::Cursor::new(bytes)));
    d.set_max_packet_bytes(1 << 20);
    assert_eq!(d.max_packet_bytes(), 1 << 20);
    let got = drain(&mut d);
    assert_eq!(got.len(), 4 * 2 + 1);
    assert_eq!(got[8], (Some(9000), 123));
    assert_eq!(d.oversized_packet_count(), 1);
    assert_eq!(d.framing_error_count(), 0);
    assert_eq!(d.hole_count(), 0);
    let kinds: Vec<DamageKind> = d.damage_events().iter().map(|e| e.kind).collect();
    assert_eq!(kinds, vec![DamageKind::OversizedPacket]);
}

#[test]
fn garbage_dense_with_capture_patterns_is_crossed_in_linear_reads() {
    let pages = stream_pages(20);
    let mut bytes: Vec<u8> = pages[..10].concat();
    // 1 MiB of back-to-back fake page headers, each claiming 255
    // segments.
    let mut fake = Vec::new();
    fake.extend_from_slice(b"OggS\0\0");
    fake.extend_from_slice(&[0u8; 8]);
    fake.extend_from_slice(&SERIAL.to_le_bytes());
    fake.extend_from_slice(&[0u8; 8]);
    fake.push(255);
    let garbage_len = 1 << 20;
    while bytes.len() < pages[..10].concat().len() + garbage_len {
        bytes.extend_from_slice(&fake);
    }
    bytes.extend(pages[10..].concat());
    let total = bytes.len() as u64;

    let io = Arc::new(Io::default());
    let mut d = open(Box::new(Counted {
        data: bytes,
        pos: 0,
        io: io.clone(),
    }));
    let got = drain(&mut d);
    assert_eq!(got.len(), 20 * 2);
    assert_eq!(d.resync_count(), 1);
    assert_eq!(io.seeks.load(Ordering::Relaxed), 0);
    // One refill per 64 KiB of input, plus slack.
    assert!(
        io.reads.load(Ordering::Relaxed) <= total / 65_536 + 4,
        "{} reads for {total} bytes",
        io.reads.load(Ordering::Relaxed)
    );
}
