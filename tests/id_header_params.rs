//! Integration tests for identification-header parameter extraction in the
//! Ogg demuxer: the Speex Ogg header (Speex manual §7.3 / table 7.1,
//! `docs/audio/speex/speex-manual.pdf`) and the FLAC-in-Ogg STREAMINFO block
//! (RFC 9639 §8.2 / §10.1, `docs/audio/flac/rfc9639-flac.pdf`).
//!
//! Both mappings carry a *sample-count* granule, so once the ID header
//! reveals the sample rate the demuxer must (a) populate
//! `params.sample_rate` / `params.channels` and (b) stamp the stream with a
//! `1/sample_rate` time-base — otherwise the granule is mis-read against the
//! `1/1_000_000` placeholder and both duration and seek land wrong.

use std::io::Cursor;

use oxideav_core::{NullCodecResolver, ReadSeek};
use oxideav_ogg::page::{flags, lace, Page};

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

/// An Ogg/Speex identification header (Speex manual table 7.1): a fixed
/// 80-byte little-endian struct.
fn speex_header(rate: u32, channels: u32, bitrate: i32) -> Vec<u8> {
    let mut p = Vec::with_capacity(80);
    p.extend_from_slice(b"Speex   "); // speex_string (8)
    p.extend_from_slice(&[0u8; 20]); // speex_version (20)
    p.extend_from_slice(&1u32.to_le_bytes()); // speex_version_id
    p.extend_from_slice(&80u32.to_le_bytes()); // header_size
    p.extend_from_slice(&rate.to_le_bytes()); // rate
    p.extend_from_slice(&0u32.to_le_bytes()); // mode (narrowband)
    p.extend_from_slice(&4u32.to_le_bytes()); // mode_bitstream_version
    p.extend_from_slice(&channels.to_le_bytes()); // nb_channels
    p.extend_from_slice(&(bitrate as u32).to_le_bytes()); // bitrate
    p.extend_from_slice(&160u32.to_le_bytes()); // frame_size
    p.extend_from_slice(&0u32.to_le_bytes()); // vbr
    p.extend_from_slice(&1u32.to_le_bytes()); // frames_per_packet
    p.extend_from_slice(&0u32.to_le_bytes()); // extra_headers
    p.extend_from_slice(&0u32.to_le_bytes()); // reserved1
    p.extend_from_slice(&0u32.to_le_bytes()); // reserved2
    debug_assert_eq!(p.len(), 80);
    p
}

/// A Speex comment packet — the bare vorbis_comment layout (no magic
/// prefix), per the Speex manual §7.3.
fn speex_comment(comments: &[(&str, &str)]) -> Vec<u8> {
    let mut p = Vec::new();
    let vendor = b"oxideav-test";
    p.extend_from_slice(&(vendor.len() as u32).to_le_bytes());
    p.extend_from_slice(vendor);
    p.extend_from_slice(&(comments.len() as u32).to_le_bytes());
    for (k, v) in comments {
        let entry = format!("{k}={v}");
        p.extend_from_slice(&(entry.len() as u32).to_le_bytes());
        p.extend_from_slice(entry.as_bytes());
    }
    p
}

/// Build a complete Speex-in-Ogg blob: ID header, comment header, then
/// `data_pages` audio pages with sample-count granules.
fn build_speex(serial: u32, rate: u32, channels: u32, bitrate: i32, data_pages: i64) -> Vec<u8> {
    build_speex_with_comments(serial, rate, channels, bitrate, data_pages, &[])
}

fn build_speex_with_comments(
    serial: u32,
    rate: u32,
    channels: u32,
    bitrate: i32,
    data_pages: i64,
    comments: &[(&str, &str)],
) -> Vec<u8> {
    let mut out = Vec::new();
    let mut seq = 0u32;
    out.extend(build_page(
        flags::FIRST_PAGE,
        0,
        serial,
        seq,
        &speex_header(rate, channels, bitrate),
    ));
    seq += 1;
    out.extend(build_page(0, 0, serial, seq, &speex_comment(comments)));
    seq += 1;
    for i in 1..=data_pages {
        let last = i == data_pages;
        let flag = if last { flags::LAST_PAGE } else { 0 };
        // Speex granule = sample number of last sample in the packet.
        out.extend(build_page(flag, 160 * i, serial, seq, &[0xAA, i as u8]));
        seq += 1;
    }
    out
}

/// A FLAC-in-Ogg mapping packet with a STREAMINFO carrying the given
/// `sample_rate` (20 bits) and `channels` (3 bits, stored as n-1), packed
/// big-endian per RFC 9639 §8.2.
fn flac_mapping_with_streaminfo(sample_rate: u32, channels: u32, bps: u32) -> Vec<u8> {
    let mut p = Vec::new();
    p.push(0x7F);
    p.extend_from_slice(b"FLAC");
    p.extend_from_slice(&[0x01, 0x00]); // mapping version 1.0
    p.extend_from_slice(&0u16.to_be_bytes()); // 0 extra header packets
    p.extend_from_slice(b"fLaC");
    p.extend_from_slice(&[0x00, 0x00, 0x00, 34]); // STREAMINFO block header (type 0, len 34)

    // 34-byte STREAMINFO. Bytes 10..14 hold sample_rate(20)|chan-1(3)|bps-1(5)
    // |high 4 bits of total samples, big-endian.
    let mut si = vec![0u8; 34];
    let packed: u32 = ((sample_rate & 0xF_FFFF) << 12)
        | (((channels - 1) & 0x7) << 9)
        | (((bps - 1) & 0x1F) << 4);
    si[10..14].copy_from_slice(&packed.to_be_bytes());
    p.extend_from_slice(&si);
    p
}

fn build_flac(serial: u32, sample_rate: u32, channels: u32, bps: u32, data_pages: i64) -> Vec<u8> {
    let mut out = Vec::new();
    let mut seq = 0u32;
    out.extend(build_page(
        flags::FIRST_PAGE,
        0,
        serial,
        seq,
        &flac_mapping_with_streaminfo(sample_rate, channels, bps),
    ));
    seq += 1;
    for i in 1..=data_pages {
        let last = i == data_pages;
        let flag = if last { flags::LAST_PAGE } else { 0 };
        out.extend(build_page(flag, 4096 * i, serial, seq, &[0xF8, i as u8]));
        seq += 1;
    }
    out
}

#[test]
fn speex_header_populates_rate_channels_bitrate() {
    let blob = build_speex(0x5BEE_0001, 16_000, 2, 24_600, 4);
    let reader: Box<dyn ReadSeek> = Box::new(Cursor::new(blob));
    let demux = oxideav_ogg::demux::open(reader, &NullCodecResolver).expect("open");
    let p = &demux.streams()[0].params;
    assert_eq!(p.codec_id.as_str(), "speex");
    assert_eq!(p.sample_rate, Some(16_000));
    assert_eq!(p.channels, Some(2));
    assert_eq!(p.bit_rate, Some(24_600));
    // The granule is a sample count, so time_base must be 1/rate, not 1/1e6.
    assert_eq!(demux.streams()[0].time_base.0.num, 1);
    assert_eq!(demux.streams()[0].time_base.0.den, 16_000);
}

#[test]
fn speex_unknown_bitrate_is_not_surfaced() {
    // The Speex encoder writes -1 as the "unknown" bitrate sentinel; the
    // demuxer must not surface a 4-billion bit_rate from a sign-flipped read.
    let blob = build_speex(0x5BEE_0002, 8_000, 1, -1, 2);
    let reader: Box<dyn ReadSeek> = Box::new(Cursor::new(blob));
    let demux = oxideav_ogg::demux::open(reader, &NullCodecResolver).expect("open");
    let p = &demux.streams()[0].params;
    assert_eq!(p.sample_rate, Some(8_000));
    assert_eq!(p.channels, Some(1));
    assert_eq!(p.bit_rate, None, "-1 sentinel must not be surfaced");
}

#[test]
fn speex_corrupt_channel_count_clamps() {
    // A corrupt nb_channels (Speex is mono/stereo only) clamps rather than
    // propagating an absurd value downstream.
    let blob = build_speex(0x5BEE_0003, 32_000, 9, 0, 1);
    let reader: Box<dyn ReadSeek> = Box::new(Cursor::new(blob));
    let demux = oxideav_ogg::demux::open(reader, &NullCodecResolver).expect("open");
    assert_eq!(demux.streams()[0].params.channels, Some(1));
}

#[test]
fn speex_duration_uses_sample_rate_time_base() {
    // 4 data pages, last granule = 160 * 4 = 640 samples at 16 kHz = 40 ms.
    let blob = build_speex(0x5BEE_0004, 16_000, 1, 0, 4);
    let reader: Box<dyn ReadSeek> = Box::new(Cursor::new(blob));
    let demux = oxideav_ogg::demux::open(reader, &NullCodecResolver).expect("open");
    let micros = demux.duration_micros().expect("duration");
    assert_eq!(micros, 40_000, "640 samples / 16000 Hz = 40 ms");
}

#[test]
fn flac_streaminfo_populates_rate_channels() {
    let blob = build_flac(0x0F1A_C010, 44_100, 2, 16, 4);
    let reader: Box<dyn ReadSeek> = Box::new(Cursor::new(blob));
    let demux = oxideav_ogg::demux::open(reader, &NullCodecResolver).expect("open");
    let p = &demux.streams()[0].params;
    assert_eq!(p.codec_id.as_str(), "flac");
    assert_eq!(p.sample_rate, Some(44_100));
    assert_eq!(p.channels, Some(2));
    assert_eq!(demux.streams()[0].time_base.0.den, 44_100);
}

#[test]
fn flac_high_sample_rate_round_trips() {
    // 192 kHz fits in the 20-bit STREAMINFO field; verify the shift math.
    let blob = build_flac(0x0F1A_C011, 192_000, 6, 24, 2);
    let reader: Box<dyn ReadSeek> = Box::new(Cursor::new(blob));
    let demux = oxideav_ogg::demux::open(reader, &NullCodecResolver).expect("open");
    let p = &demux.streams()[0].params;
    assert_eq!(p.sample_rate, Some(192_000));
    assert_eq!(p.channels, Some(6));
}

#[test]
fn flac_duration_uses_sample_rate_time_base() {
    // last granule = 4096 * 4 = 16384 samples at 44_100 Hz.
    let blob = build_flac(0x0F1A_C012, 44_100, 2, 16, 4);
    let reader: Box<dyn ReadSeek> = Box::new(Cursor::new(blob));
    let demux = oxideav_ogg::demux::open(reader, &NullCodecResolver).expect("open");
    let micros = demux.duration_micros().expect("duration");
    let expect = (16_384i64 * 1_000_000) / 44_100;
    assert_eq!(micros, expect);
}

#[test]
fn speex_comment_header_populates_metadata() {
    // The Speex 2nd packet is a bare vorbis_comment (no magic prefix). The
    // demuxer must surface it as container metadata like the other codecs.
    let blob = build_speex_with_comments(
        0x5BEE_0005,
        16_000,
        1,
        0,
        2,
        &[("TITLE", "Clean Room"), ("ARTIST", "OxideAV")],
    );
    let reader: Box<dyn ReadSeek> = Box::new(Cursor::new(blob));
    let demux = oxideav_ogg::demux::open(reader, &NullCodecResolver).expect("open");
    let md = demux.metadata();
    let get = |k: &str| md.iter().find(|(kk, _)| kk == k).map(|(_, v)| v.as_str());
    assert_eq!(get("title"), Some("Clean Room"));
    assert_eq!(get("artist"), Some("OxideAV"));
    assert_eq!(get("vendor"), Some("oxideav-test"));
}
