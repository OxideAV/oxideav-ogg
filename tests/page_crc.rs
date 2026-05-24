//! Page-level CRC-32 verification per RFC 3533 §6 field 7.
//!
//! These tests build a real multi-stream multi-page Ogg blob via the
//! muxer (so every byte was computed and laid out the way a real Ogg
//! writer would do it), then walk the blob page-by-page and confirm
//! every embedded CRC matches the one recomputed over the same bytes
//! with the CRC field zeroed.
//!
//! A second pass flips a single byte in one page's payload and
//! confirms `validate_page_crc` catches the corruption.

use std::io::Cursor;

use oxideav_core::{CodecId, CodecParameters, Packet, StreamInfo, TimeBase};
use oxideav_core::{ReadSeek, WriteSeek};

use oxideav_ogg::crc::{compute_page_checksum, read_page_checksum, validate_page_crc};

/// Walk a complete Ogg blob and call `each(page_index, page_bytes)` once
/// per page, in file order. Stops at the first byte that is not a valid
/// page header (typically end-of-file). Returns the number of pages
/// walked.
fn for_each_page(bytes: &[u8], mut each: impl FnMut(usize, &[u8])) -> usize {
    let mut off = 0usize;
    let mut idx = 0usize;
    while off + 27 <= bytes.len() {
        if &bytes[off..off + 4] != b"OggS" {
            break;
        }
        let n_segs = bytes[off + 26] as usize;
        let header_len = 27 + n_segs;
        if off + header_len > bytes.len() {
            break;
        }
        let data_len: usize = bytes[off + 27..off + header_len]
            .iter()
            .map(|&v| v as usize)
            .sum();
        let total = header_len + data_len;
        if off + total > bytes.len() {
            break;
        }
        each(idx, &bytes[off..off + total]);
        idx += 1;
        off += total;
    }
    idx
}

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
    // 12 packets of varying size — produce multiple data pages so the
    // CRC pass exercises more than just the BOS page.
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

#[test]
fn every_page_in_a_muxed_stream_passes_crc() {
    let bytes = build_vorbis_ogg();
    assert!(bytes.len() > 27, "muxer produced nothing");
    assert_eq!(&bytes[0..4], b"OggS");

    let mut checked = 0usize;
    let walked = for_each_page(&bytes, |idx, page| {
        let stored = read_page_checksum(page).expect("page too short");
        let computed = compute_page_checksum(page).expect("page too short");
        assert_eq!(
            stored, computed,
            "page #{idx} CRC mismatch: stored={stored:08x} computed={computed:08x}",
        );
        assert_eq!(
            validate_page_crc(page),
            Some(true),
            "page #{idx} validate_page_crc returned false"
        );
        checked += 1;
    });
    assert_eq!(walked, checked);
    // BOS + at least one data page + EOS — multi-page coverage.
    assert!(walked >= 3, "expected at least 3 pages, walked {walked}");
}

#[test]
fn page_crc_also_round_trips_through_the_demuxer() {
    // The point here is to confirm Page::parse's CRC check (which is
    // the consumer-facing path) and the new standalone validate_page_crc
    // helper agree on every page.
    let bytes = build_vorbis_ogg();

    // Demuxer should accept the stream end-to-end. If any page's CRC
    // were wrong, the demuxer would have to recover via its resync path
    // — and on a clean muxed stream the resync count must be zero.
    let reader: Box<dyn ReadSeek> = Box::new(Cursor::new(bytes.clone()));
    let mut demux = oxideav_ogg::demux::open(reader, &oxideav_core::NullCodecResolver)
        .expect("demux clean muxed output");
    // Drain all packets — exercises every page in the stream.
    let mut n = 0;
    while let Ok(_p) = demux.next_packet() {
        n += 1;
    }
    assert_eq!(n, 12, "expected 12 data packets, got {n}");

    // And the standalone validator must agree page-by-page.
    for_each_page(&bytes, |idx, page| {
        assert_eq!(
            validate_page_crc(page),
            Some(true),
            "page #{idx} disagrees with demuxer",
        );
    });
}

#[test]
fn validate_page_crc_catches_payload_bit_flip() {
    let mut bytes = build_vorbis_ogg();

    // Locate the second page (skip BOS) and find an offset inside its
    // segment-data region so we can flip exactly one byte.
    let mut page_offsets: Vec<(usize, usize)> = Vec::new();
    for_each_page(&bytes, |_idx, page| {
        // We need the page's absolute offset, not the slice contents.
        // Compute from the slice's pointer relative to the start of
        // `bytes`. SAFETY: `page` is always a subslice of `bytes`.
        let off = page.as_ptr() as usize - bytes.as_ptr() as usize;
        page_offsets.push((off, page.len()));
    });
    assert!(
        page_offsets.len() >= 2,
        "need >= 2 pages to flip the second; got {}",
        page_offsets.len()
    );
    let (target_off, target_len) = page_offsets[1];
    // Find the first byte of the payload (header is 27 + n_segs).
    let n_segs = bytes[target_off + 26] as usize;
    let payload_start = target_off + 27 + n_segs;
    assert!(
        payload_start < target_off + target_len,
        "page has empty payload"
    );

    // Sanity check: page validates BEFORE the flip.
    assert_eq!(
        validate_page_crc(&bytes[target_off..target_off + target_len]),
        Some(true),
    );

    // Flip one bit and confirm validation now fails.
    bytes[payload_start] ^= 0x01;
    assert_eq!(
        validate_page_crc(&bytes[target_off..target_off + target_len]),
        Some(false),
        "validator failed to catch a single-bit payload flip",
    );
}

#[test]
fn validate_page_crc_catches_header_bit_flip() {
    let mut bytes = build_vorbis_ogg();

    // First page (BOS).
    let n_segs0 = bytes[26] as usize;
    let header_len0 = 27 + n_segs0;
    let data_len0: usize = bytes[27..header_len0].iter().map(|&v| v as usize).sum();
    let total0 = header_len0 + data_len0;

    // Sanity check.
    assert_eq!(validate_page_crc(&bytes[..total0]), Some(true));

    // Flip one bit in the granule_position field (byte 6).
    bytes[6] ^= 0x10;
    assert_eq!(
        validate_page_crc(&bytes[..total0]),
        Some(false),
        "validator failed to catch a granule-position flip",
    );
}
