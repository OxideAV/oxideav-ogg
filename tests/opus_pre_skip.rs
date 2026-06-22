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
