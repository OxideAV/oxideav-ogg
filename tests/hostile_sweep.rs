//! Deterministic hostile-input sweeps over the demuxer and the
//! whole-file validator.
//!
//! Fuzzing (the `fuzz/` crate) explores randomly; these sweeps are the
//! reproducible CI-side complement: seeded, exhaustive-where-cheap
//! mutation batteries over real muxer/framing output, run on every CI
//! build. For every mutated buffer the demuxer must:
//!
//! * never panic (the sweep itself is the assertion),
//! * never fabricate data — the sum of delivered packet bytes on the
//!   linear path is bounded by the input length (payload bytes come
//!   from page bodies, each consumed at most once),
//! * always terminate — a drain-iteration cap catches livelocks,
//! * keep its damage ledger bounded (`MAX_DAMAGE_EVENTS`).
//!
//! `validate::validate` runs over every mutated buffer too: it must
//! never panic and its issue list must respect its own `MAX_ISSUES`
//! cap.
//!
//! Sweeps: exhaustive truncation at every byte length, exhaustive
//! single-byte corruption at every offset, per-page CRC-field damage,
//! and a seeded multi-mutation battery (flips, junk insertion, span
//! deletion, span duplication) from a fixed xorshift64* seed.

use std::io::Cursor;

use oxideav_core::{
    CodecId, CodecParameters, Demuxer, Error, Packet, ReadSeek, StreamInfo, TimeBase, WriteSeek,
};
use oxideav_ogg::demux::MAX_DAMAGE_EVENTS;
use oxideav_ogg::page::{flags, lace, Page};
use oxideav_ogg::validate::{validate, MAX_ISSUES};

// ─────────────────────────── deterministic PRNG ───────────────────────────

/// xorshift64* — tiny, deterministic, good enough for mutation fuzz.
struct Rng(u64);

impl Rng {
    fn new(seed: u64) -> Self {
        Rng(seed.max(1))
    }
    fn next(&mut self) -> u64 {
        let mut x = self.0;
        x ^= x >> 12;
        x ^= x << 25;
        x ^= x >> 27;
        self.0 = x;
        x.wrapping_mul(0x2545_F491_4F6C_DD1D)
    }
    fn below(&mut self, n: usize) -> usize {
        (self.next() % n.max(1) as u64) as usize
    }
}

// ─────────────────────────── base corpus ───────────────────────────

fn vorbis_id_packet() -> Vec<u8> {
    let mut p = vec![0x01];
    p.extend_from_slice(b"vorbis");
    p.extend_from_slice(&0u32.to_le_bytes());
    p.push(2);
    p.extend_from_slice(&48_000u32.to_le_bytes());
    p.extend_from_slice(&[0; 12]);
    p.extend_from_slice(&[0xB8, 0x01]);
    p
}

fn vorbis_extradata() -> Vec<u8> {
    let id = vorbis_id_packet();
    let mut com = vec![0x03];
    com.extend_from_slice(b"vorbis");
    com.extend_from_slice(&[0, 0, 0, 0, 0, 0, 0, 0, 1]);
    let mut setup = vec![0x05];
    setup.extend_from_slice(b"vorbis");
    setup.extend_from_slice(&[0; 24]);
    oxideav_ogg::mux::xiph_lace(&[&id, &com, &setup]).unwrap()
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

/// A muxed single-Vorbis file with `n` data packets — real muxer
/// output, so the sweep stresses exactly what we ship.
fn muxed_vorbis(n: usize) -> Vec<u8> {
    let mut params = CodecParameters::audio(CodecId::new("vorbis"));
    params.channels = Some(2);
    params.sample_rate = Some(48_000);
    params.extradata = vorbis_extradata();
    let stream = StreamInfo {
        index: 0,
        time_base: TimeBase::new(1, 48_000),
        duration: None,
        start_time: Some(0),
        params,
    };
    let shared = SharedBuf::default();
    let out: Box<dyn WriteSeek> = Box::new(shared.clone());
    let mut muxer = oxideav_ogg::mux::open(out, std::slice::from_ref(&stream)).unwrap();
    muxer.write_header().unwrap();
    for i in 1..=n as i64 {
        let mut pkt = Packet::new(0, stream.time_base, vec![(i & 0x7f) as u8; 90]);
        pkt.pts = Some(960 * i);
        pkt.flags.unit_boundary = true;
        muxer.write_packet(&pkt).unwrap();
    }
    muxer.write_trailer().unwrap();
    drop(muxer);
    let guard = shared.0.lock().unwrap();
    guard.get_ref().clone()
}

/// A hand-framed two-stream grouped file with a page-spanning packet
/// (exercises continuation handling under damage).
fn grouped_two_streams() -> Vec<u8> {
    let mut out = Vec::new();
    let (a, b) = (0x1111_1111u32, 0x2222_2222u32);
    // BOS pages (Vorbis id on A; a second Vorbis id on B).
    for (serial, _idx) in [(a, 0u32), (b, 1u32)] {
        out.extend(
            Page {
                flags: flags::FIRST_PAGE,
                granule_position: 0,
                serial,
                seq_no: 0,
                lacing: lace(vorbis_id_packet().len()),
                data: vorbis_id_packet(),
            }
            .to_bytes(),
        );
    }
    // One spanning packet on A (255-byte head, 60-byte tail), one small
    // packet on B, EOS everywhere.
    out.extend(
        Page {
            flags: 0,
            granule_position: -1,
            serial: a,
            seq_no: 1,
            lacing: vec![255],
            data: vec![0xAB; 255],
        }
        .to_bytes(),
    );
    out.extend(
        Page {
            flags: flags::CONTINUED,
            granule_position: 960,
            serial: a,
            seq_no: 2,
            lacing: vec![60],
            data: vec![0xAB; 60],
        }
        .to_bytes(),
    );
    out.extend(
        Page {
            flags: 0,
            granule_position: 960,
            serial: b,
            seq_no: 1,
            lacing: vec![33],
            data: vec![0xBC; 33],
        }
        .to_bytes(),
    );
    for (serial, seq) in [(a, 3u32), (b, 2u32)] {
        out.extend(
            Page {
                flags: flags::LAST_PAGE,
                granule_position: 1920,
                serial,
                seq_no: seq,
                lacing: vec![0],
                data: Vec::new(),
            }
            .to_bytes(),
        );
    }
    out
}

/// A two-link chained file (link boundaries under damage).
fn chained_two_links() -> Vec<u8> {
    let mut out = Vec::new();
    for serial in [0x0AAA_AAA0u32, 0x0BBB_BBB0u32] {
        out.extend(
            Page {
                flags: flags::FIRST_PAGE,
                granule_position: 0,
                serial,
                seq_no: 0,
                lacing: lace(vorbis_id_packet().len()),
                data: vorbis_id_packet(),
            }
            .to_bytes(),
        );
        for i in 0..3u32 {
            let fl = if i == 2 { flags::LAST_PAGE } else { 0 };
            out.extend(
                Page {
                    flags: fl,
                    granule_position: 960 * (i as i64 + 1),
                    serial,
                    seq_no: i + 1,
                    lacing: vec![20],
                    data: vec![(serial & 0xff) as u8; 20],
                }
                .to_bytes(),
            );
        }
    }
    out
}

// ─────────────────────────── the harness ───────────────────────────

/// Drain a demuxer over `bytes` with hostile-input invariants:
/// termination cap, no fabricated bytes, bounded ledger.
fn drive(bytes: &[u8], what: &str) {
    let reader: Box<dyn ReadSeek> = Box::new(Cursor::new(bytes.to_vec()));
    let mut dmx = match oxideav_ogg::demux::open_concrete(reader, &oxideav_core::NullCodecResolver)
    {
        Ok(d) => d,
        Err(_) => return, // a rejected open is a valid outcome
    };
    let mut delivered = 0usize;
    let mut iterations = 0u32;
    loop {
        iterations += 1;
        assert!(
            iterations <= 100_000,
            "{what}: drain did not terminate within the iteration cap"
        );
        match dmx.next_packet() {
            Ok(p) => delivered += p.data.len(),
            Err(Error::Eof) => break,
            Err(_) => break, // any error terminates the linear walk
        }
    }
    assert!(
        delivered <= bytes.len(),
        "{what}: delivered {delivered} payload bytes from a {}-byte input",
        bytes.len()
    );
    assert!(
        dmx.damage_events().len() <= MAX_DAMAGE_EVENTS,
        "{what}: damage ledger exceeded its retention cap"
    );
}

/// Validator invariants on arbitrary bytes: terminates, bounded issues.
fn check_validate(bytes: &[u8], what: &str) {
    let report = validate(bytes);
    assert!(
        report.issues.len() <= MAX_ISSUES,
        "{what}: validator issue list exceeded its cap"
    );
}

fn corpus() -> Vec<(&'static str, Vec<u8>)> {
    vec![
        ("muxed vorbis", muxed_vorbis(8)),
        ("grouped two streams", grouped_two_streams()),
        ("chained two links", chained_two_links()),
    ]
}

// ─────────────────────────── sweeps ───────────────────────────

#[test]
fn truncation_sweep_every_length() {
    for (what, base) in corpus() {
        for cut in 0..=base.len() {
            let slice = &base[..cut];
            drive(slice, what);
            check_validate(slice, what);
        }
    }
}

#[test]
fn single_byte_corruption_sweep_every_offset() {
    for (what, base) in corpus() {
        for off in 0..base.len() {
            let mut mutated = base.clone();
            mutated[off] ^= 0xFF;
            drive(&mutated, what);
            check_validate(&mutated, what);
        }
    }
}

#[test]
fn crc_field_damage_on_every_page() {
    for (what, base) in corpus() {
        // Walk true page extents (the base files are well-formed).
        let mut pos = 0usize;
        while pos + 27 <= base.len() {
            assert_eq!(
                &base[pos..pos + 4],
                b"OggS",
                "{what}: corpus walk lost sync"
            );
            let n_segs = base[pos + 26] as usize;
            let body: usize = base[pos + 27..pos + 27 + n_segs]
                .iter()
                .map(|&v| v as usize)
                .sum();
            // Flip one CRC byte of this page.
            let mut mutated = base.clone();
            mutated[pos + 22] ^= 0x5A;
            drive(&mutated, what);
            check_validate(&mutated, what);
            pos += 27 + n_segs + body;
        }
    }
}

#[test]
fn seeded_multi_mutation_battery() {
    let mut rng = Rng::new(0x0417_0417_0417_0417);
    for (what, base) in corpus() {
        for _round in 0..2500 {
            let mut mutated = base.clone();
            let mutations = 1 + rng.below(8);
            for _ in 0..mutations {
                match rng.below(4) {
                    0 => {
                        // Byte flip.
                        let off = rng.below(mutated.len());
                        mutated[off] ^= (rng.next() & 0xff) as u8 | 1;
                    }
                    1 => {
                        // Junk insertion (may inject fake captures).
                        let off = rng.below(mutated.len() + 1);
                        let len = 1 + rng.below(40);
                        let junk: Vec<u8> = (0..len)
                            .map(|i| {
                                if i % 5 == 0 {
                                    b'O'
                                } else {
                                    (rng.next() & 0xff) as u8
                                }
                            })
                            .collect();
                        mutated.splice(off..off, junk);
                    }
                    2 => {
                        // Span deletion.
                        if mutated.len() > 8 {
                            let off = rng.below(mutated.len() - 4);
                            let len = 1 + rng.below((mutated.len() - off).min(200));
                            mutated.drain(off..off + len);
                        }
                    }
                    _ => {
                        // Span duplication (repeats pages / headers).
                        if !mutated.is_empty() {
                            let off = rng.below(mutated.len());
                            let len = 1 + rng.below((mutated.len() - off).min(120));
                            let span = mutated[off..off + len].to_vec();
                            let at = rng.below(mutated.len() + 1);
                            mutated.splice(at..at, span);
                        }
                    }
                }
            }
            drive(&mutated, what);
            check_validate(&mutated, what);
        }
    }
}

#[test]
fn header_section_mutation_battery() {
    // Focus the mutations on the first 200 bytes (BOS + identification
    // headers) where stream registration, codec sniffing, and header
    // budgeting live.
    let mut rng = Rng::new(0x0BAD_F00D_0BAD_F00D);
    for (what, base) in corpus() {
        let span = base.len().min(200);
        for _round in 0..2000 {
            let mut mutated = base.clone();
            for _ in 0..1 + rng.below(4) {
                let off = rng.below(span);
                mutated[off] = (rng.next() & 0xff) as u8;
            }
            drive(&mutated, what);
            check_validate(&mutated, what);
        }
    }
}
