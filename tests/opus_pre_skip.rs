//! Integration tests for the Ogg Opus **pre-skip** granule-position
//! semantics (`docs/audio/opus/rfc7845-ogg-opus.txt`).
//!
//! RFC 7845 §4.3 defines the playback mapping as
//!   `PCM sample position = granule position − pre-skip`
//! where pre-skip is the 16-bit LE value in the `OpusHead` ID header at
//! byte offset 10..12 (§5.1 field 4), in 48 kHz samples. The encoder pads
//! the start of the stream with `pre-skip` samples of decoder warm-up that
//! a player decodes but discards; the raw final granule therefore
//! over-counts the real audio by exactly `pre-skip` samples. A demuxer that
//! reports duration straight from the raw final granule over-reports by
//! `pre-skip / 48000` seconds — these tests pin that the demuxer subtracts
//! it.

use std::io::Cursor;

use oxideav_core::{Demuxer, NullCodecResolver, ReadSeek};
use oxideav_ogg::page::{flags, lace, Page};

// ─────────────────────── synthetic Opus header packets ───────────────────

/// Build an `OpusHead` ID header (RFC 7845 §5.1) with the given channel
/// count and pre-skip. Mapping family 0, 19-byte (no channel-mapping table)
/// layout — the minimal valid header.
fn opus_head(channels: u8, pre_skip: u16) -> Vec<u8> {
    let mut p = Vec::with_capacity(19);
    p.extend_from_slice(b"OpusHead"); // magic, 8 bytes
    p.push(1); // version
    p.push(channels); // output channel count
    p.extend_from_slice(&pre_skip.to_le_bytes()); // pre-skip, bytes 10..12
    p.extend_from_slice(&48_000u32.to_le_bytes()); // input sample rate, 12..16
    p.extend_from_slice(&0i16.to_le_bytes()); // output gain, 16..18
    p.push(0); // mapping family, byte 18
    assert_eq!(p.len(), 19);
    p
}

/// Minimal `OpusTags` comment header (RFC 7845 §5.2): magic + empty vendor
/// string + zero user-comment count.
fn opus_tags() -> Vec<u8> {
    let mut p = Vec::new();
    p.extend_from_slice(b"OpusTags");
    p.extend_from_slice(&0u32.to_le_bytes()); // vendor length 0
    p.extend_from_slice(&0u32.to_le_bytes()); // user comment count 0
    p
}

fn build_page(flags_byte: u8, granule: i64, serial: u32, seq: u32, packet: &[u8]) -> Vec<u8> {
    Page {
        flags: flags_byte,
        granule_position: granule,
        serial,
        seq_no: seq,
        lacing: lace(packet.len()),
        data: packet.to_vec(),
    }
    .to_bytes()
}

/// Build a complete Opus-in-Ogg logical bitstream: `OpusHead` BOS page,
/// `OpusTags` page, then `data_pages` audio pages whose granule steps by
/// 960 (one 20 ms frame at 48 kHz) starting from `pre_skip` so the first
/// audio page's PCM sample position is the first real frame.
fn build_opus(serial: u32, pre_skip: u16, data_pages: i64, channels: u8) -> Vec<u8> {
    let mut out = Vec::new();
    let mut seq = 0u32;
    out.extend(build_page(
        flags::FIRST_PAGE,
        0,
        serial,
        seq,
        &opus_head(channels, pre_skip),
    ));
    seq += 1;
    out.extend(build_page(0, 0, serial, seq, &opus_tags()));
    seq += 1;
    for i in 1..=data_pages {
        // RFC 7845 §4.3 worked example: granule = pre_skip + accumulated
        // 48 kHz samples. Each page adds one 960-sample frame.
        let granule = pre_skip as i64 + 960 * i;
        let last = i == data_pages;
        let flag = if last { flags::LAST_PAGE } else { 0 };
        out.extend(build_page(flag, granule, serial, seq, &[0xCC, i as u8]));
        seq += 1;
    }
    out
}

// ───────────────────────────── tests ─────────────────────────────────

#[test]
fn duration_subtracts_pre_skip() {
    // 50 data pages × 960 samples = 48000 samples of real audio = 1.000 s.
    // The on-wire final granule is `pre_skip + 48000`; a naive reading
    // would report (pre_skip + 48000) / 48000 s, over-reporting by
    // pre_skip/48000 = 11971/48000 ≈ 0.249 s.
    let pre_skip = 11_971u16;
    let blob = build_opus(0x0DDF_ACE0, pre_skip, 50, 2);

    let input: Box<dyn ReadSeek> = Box::new(Cursor::new(blob));
    let codecs = NullCodecResolver;
    let mut demux = oxideav_ogg::demux::open_concrete(input, &codecs).expect("open");
    demux.build_seek_index().expect("index");

    let dur = demux.duration_micros().expect("duration recorded");
    // Expect exactly 1.000 s (± rounding), NOT 1.249 s.
    assert!(
        (dur - 1_000_000).abs() <= 2,
        "duration should subtract pre-skip (1.000 s expected), got {dur} µs",
    );
}

#[test]
fn accessor_reports_pre_skip() {
    let blob = build_opus(0x0DDF_ACE1, 3_840, 5, 1);
    let input: Box<dyn ReadSeek> = Box::new(Cursor::new(blob));
    let codecs = NullCodecResolver;
    let demux = oxideav_ogg::demux::open_concrete(input, &codecs).expect("open");
    assert_eq!(demux.opus_pre_skip(0), Some(3_840));
    // Out-of-range index → None.
    assert_eq!(demux.opus_pre_skip(7), None);
}

#[test]
fn zero_pre_skip_is_transparent() {
    // A pre_skip of 0 (legal, RFC 7845 §4.2 "MAY be smaller than a single
    // packet") leaves the raw granule unchanged: 25 pages × 960 = 24000
    // samples = 0.500 s.
    let blob = build_opus(0x0DDF_ACE2, 0, 25, 2);
    let input: Box<dyn ReadSeek> = Box::new(Cursor::new(blob));
    let codecs = NullCodecResolver;
    let mut demux = oxideav_ogg::demux::open_concrete(input, &codecs).expect("open");
    demux.build_seek_index().expect("index");
    let dur = demux.duration_micros().expect("duration recorded");
    assert!(
        (dur - 500_000).abs() <= 2,
        "zero pre-skip should pass through unchanged (0.500 s), got {dur} µs",
    );
    assert_eq!(demux.opus_pre_skip(0), Some(0));
}

#[test]
fn seek_lands_on_pcm_position_floor_not_pre_skip_early() {
    // pre_skip = 11971. Page i carries raw granule `11971 + 960*i`, so its
    // PCM sample position (RFC 7845 §4.3) is `960*i`. The user seeks by PCM
    // position, so seeking to pts = 5000 samples must land on the page whose
    // PCM position floors 5000 — page 5 (PCM 4800 ≤ 5000 < page 6's 5760) —
    // and return that page's *raw* on-wire granule 11971 + 960*5 = 16771.
    // A demuxer that compared the raw granule against the raw `pts` would
    // instead floor 5000 directly and land ~pre_skip/48000 s = ~0.25 s early.
    let pre_skip = 11_971i64;
    let blob = build_opus(0x0DDF_ACE4, pre_skip as u16, 20, 2);
    let reader: Box<dyn ReadSeek> = Box::new(Cursor::new(blob));
    // Use the dense-index path (`build_seek_index`) so the exact floor lookup
    // (`index_floor_by`) drives the result — the byte-bisection fallback is
    // coarse on tiny synthetic files regardless of codec.
    let mut demux = oxideav_ogg::demux::open_concrete(reader, &NullCodecResolver)
        .expect("open concrete demuxer");
    demux.build_seek_index().expect("index");

    let target_pcm = 5_000i64;
    let landed = demux.seek_to(0, target_pcm).expect("seek_to ok");
    let expected_raw = pre_skip + 960 * 5;
    assert_eq!(
        landed, expected_raw,
        "Opus seek should land on the PCM-position floor page (raw granule {expected_raw}), got {landed}",
    );
    // The landed page's PCM position must be ≤ the target.
    assert!(
        landed - pre_skip <= target_pcm,
        "landed PCM position {} should be ≤ target {target_pcm}",
        landed - pre_skip,
    );
}

#[test]
fn non_opus_stream_has_no_pre_skip() {
    // A Vorbis stream never has a pre-skip entry; the accessor returns None
    // and its granule passes through the duration path unchanged.
    let mut blob = Vec::new();
    let serial = 0x0DDF_ACE3u32;
    let mut seq = 0u32;
    // Vorbis ID packet (1 + "vorbis" + version + channels + rate + ...).
    let mut id = Vec::new();
    id.push(0x01);
    id.extend_from_slice(b"vorbis");
    id.extend_from_slice(&0u32.to_le_bytes());
    id.push(2);
    id.extend_from_slice(&48_000u32.to_le_bytes());
    id.extend_from_slice(&0i32.to_le_bytes());
    id.extend_from_slice(&128_000i32.to_le_bytes());
    id.extend_from_slice(&0i32.to_le_bytes());
    id.push(0xB8);
    id.push(0x01);
    blob.extend(build_page(flags::FIRST_PAGE, 0, serial, seq, &id));
    seq += 1;
    // comment
    let mut comment = vec![0x03];
    comment.extend_from_slice(b"vorbis");
    comment.extend_from_slice(&0u32.to_le_bytes());
    comment.extend_from_slice(&0u32.to_le_bytes());
    comment.push(0x01);
    blob.extend(build_page(0, 0, serial, seq, &comment));
    seq += 1;
    // setup
    let mut setup = vec![0x05];
    setup.extend_from_slice(b"vorbis");
    setup.extend_from_slice(&[0u8; 16]);
    blob.extend(build_page(0, 0, serial, seq, &setup));
    seq += 1;
    for i in 1..=10i64 {
        let last = i == 10;
        let flag = if last { flags::LAST_PAGE } else { 0 };
        blob.extend(build_page(flag, 960 * i, serial, seq, &[0xAA, i as u8]));
        seq += 1;
    }

    let input: Box<dyn ReadSeek> = Box::new(Cursor::new(blob));
    let codecs = NullCodecResolver;
    let mut demux = oxideav_ogg::demux::open_concrete(input, &codecs).expect("open");
    demux.build_seek_index().expect("index");
    assert_eq!(demux.opus_pre_skip(0), None);
    // 10 pages × 960 = 9600 samples = 0.200 s, unaffected.
    let dur = demux.duration_micros().expect("duration recorded");
    assert!(
        (dur - 200_000).abs() <= 2,
        "vorbis duration unchanged (0.200 s), got {dur} µs",
    );
}

#[test]
fn opus_seek_with_pre_skip_is_distinct_from_zero_pre_skip() {
    // Two otherwise-identical Opus streams differing only in pre-skip must
    // floor the same PCM-position target onto pages whose *raw* granules
    // differ by exactly the pre-skip — proving the seek axis is PCM
    // position, not the raw granule. Target 5000 → page 5 (PCM 4800).
    let target_pcm = 5_000i64;

    let blob_a = build_opus(0x0DDF_ACF1, 0, 20, 2);
    let reader_a: Box<dyn ReadSeek> = Box::new(Cursor::new(blob_a));
    let mut demux_a = oxideav_ogg::demux::open_concrete(reader_a, &NullCodecResolver).unwrap();
    demux_a.build_seek_index().unwrap();
    let landed_a = demux_a.seek_to(0, target_pcm).unwrap();

    let pre_skip_b = 3_840i64;
    let blob_b = build_opus(0x0DDF_ACF2, pre_skip_b as u16, 20, 2);
    let reader_b: Box<dyn ReadSeek> = Box::new(Cursor::new(blob_b));
    let mut demux_b = oxideav_ogg::demux::open_concrete(reader_b, &NullCodecResolver).unwrap();
    demux_b.build_seek_index().unwrap();
    let landed_b = demux_b.seek_to(0, target_pcm).unwrap();

    // Both land on the same PCM-position floor page (page 5), so the raw
    // granules differ by exactly the pre-skip.
    assert_eq!(landed_a, 960 * 5, "zero-pre-skip raw granule");
    assert_eq!(landed_b, pre_skip_b + 960 * 5, "pre-skip raw granule");
    assert_eq!(
        landed_b - landed_a,
        pre_skip_b,
        "raw granules differ by pre-skip"
    );
}
