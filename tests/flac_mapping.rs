//! Integration tests for the FLAC-in-Ogg mapping (RFC 9639 §10.1,
//! `docs/audio/flac/rfc9639-flac.pdf`).
//!
//! The first packet of a FLAC-in-Ogg logical bitstream is a mapping header:
//!   `0x7F "FLAC"` (5) + 2-byte mapping version + 2-byte BE header-packet
//!   count (*excluding* the first packet) + `"fLaC"` + a 4-byte STREAMINFO
//!   metadata block header + the 34-byte STREAMINFO block.
//! The total number of header packets is `1 + declared count`; the demuxer
//! must absorb exactly that many before delivering audio frames, and must
//! pull the Vorbis-comment metadata block (FLAC §8.1 block type 4) out of
//! one of those header packets. A declared count of 0 ("unknown") falls back
//! to absorbing just the mapping packet.

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

/// The FLAC-in-Ogg mapping (first) packet declaring `header_packets`
/// additional header packets after this one (RFC 9639 §10.1).
fn flac_mapping_packet(header_packets: u16) -> Vec<u8> {
    let mut p = Vec::new();
    p.push(0x7F);
    p.extend_from_slice(b"FLAC");
    p.extend_from_slice(&[0x01, 0x00]); // mapping version 1.0
    p.extend_from_slice(&header_packets.to_be_bytes()); // BE count
    p.extend_from_slice(b"fLaC"); // signature
                                  // STREAMINFO metadata block header: type 0, last-block bit 0 (more
                                  // follow), 24-bit BE length = 34.
    p.extend_from_slice(&[0x00, 0x00, 0x00, 34]);
    // 34-byte STREAMINFO body (contents irrelevant to the container).
    p.extend_from_slice(&[0u8; 34]);
    p
}

/// A FLAC Vorbis-comment metadata block packet (§8.1 block type 4): a 4-byte
/// block header then the standard vorbis_comment payload (vendor +
/// user-comment list, no `0x03 "vorbis"` prefix, no trailing framing bit).
fn flac_vorbis_comment_packet(last: bool, comments: &[(&str, &str)]) -> Vec<u8> {
    let mut body = Vec::new();
    let vendor = b"oxideav-test";
    body.extend_from_slice(&(vendor.len() as u32).to_le_bytes());
    body.extend_from_slice(vendor);
    body.extend_from_slice(&(comments.len() as u32).to_le_bytes());
    for (k, v) in comments {
        let entry = format!("{k}={v}");
        body.extend_from_slice(&(entry.len() as u32).to_le_bytes());
        body.extend_from_slice(entry.as_bytes());
    }
    let type_byte = 4u8 | if last { 0x80 } else { 0 };
    let len = body.len() as u32;
    let mut p = vec![type_byte, (len >> 16) as u8, (len >> 8) as u8, len as u8];
    p.extend_from_slice(&body);
    p
}

/// A FLAC padding metadata block packet (§8.1 block type 1), last block.
fn flac_padding_packet() -> Vec<u8> {
    let pad = [0u8; 8];
    let len = pad.len() as u32;
    let mut p = vec![
        0x80 | 1, // last-block bit + type 1
        (len >> 16) as u8,
        (len >> 8) as u8,
        len as u8,
    ];
    p.extend_from_slice(&pad);
    p
}

/// Build a FLAC-in-Ogg blob: mapping packet, then `extra_headers`, then
/// `data_pages` audio-frame pages.
fn build_flac(serial: u32, extra_headers: &[Vec<u8>], data_pages: i64) -> Vec<u8> {
    let mut out = Vec::new();
    let mut seq = 0u32;
    out.extend(build_page(
        flags::FIRST_PAGE,
        0,
        serial,
        seq,
        &flac_mapping_packet(extra_headers.len() as u16),
    ));
    seq += 1;
    for h in extra_headers {
        out.extend(build_page(0, 0, serial, seq, h));
        seq += 1;
    }
    for i in 1..=data_pages {
        let last = i == data_pages;
        let flag = if last { flags::LAST_PAGE } else { 0 };
        // FLAC granule = last interchannel sample number; 4096 samples/frame.
        out.extend(build_page(flag, 4096 * i, serial, seq, &[0xF8, i as u8]));
        seq += 1;
    }
    out
}

#[test]
fn header_count_from_mapping_absorbs_all_metadata_blocks() {
    // 2 extra header packets (vorbis comment + padding) → 3 header packets
    // total. The first audio frame must be delivered, not eaten as a header.
    let extra = vec![
        flac_vorbis_comment_packet(false, &[("TITLE", "Clean Room")]),
        flac_padding_packet(),
    ];
    let blob = build_flac(0x0F1A_C001, &extra, 4);

    let reader: Box<dyn ReadSeek> = Box::new(Cursor::new(blob));
    let mut demux = oxideav_ogg::demux::open(reader, &NullCodecResolver).expect("open");
    assert_eq!(demux.streams()[0].params.codec_id.as_str(), "flac");

    // First delivered packet is the first *audio* frame (0xF8 marker), not a
    // metadata block. A demuxer that absorbed only 1 header would mis-deliver
    // the vorbis-comment block as content.
    let first = demux.next_packet().expect("first content packet");
    assert_eq!(first.data[0], 0xF8, "first content packet is a FLAC frame");

    let mut count = 1;
    while demux.next_packet().is_ok() {
        count += 1;
    }
    assert_eq!(count, 4, "all 4 audio frames delivered, no headers leaked");
}

#[test]
fn vorbis_comment_metadata_is_extracted() {
    let extra = vec![flac_vorbis_comment_packet(
        true,
        &[("TITLE", "Clean Room"), ("ARTIST", "OxideAV")],
    )];
    let blob = build_flac(0x0F1A_C002, &extra, 2);
    let reader: Box<dyn ReadSeek> = Box::new(Cursor::new(blob));
    let demux = oxideav_ogg::demux::open(reader, &NullCodecResolver).expect("open");
    let md = demux.metadata();
    assert!(
        md.iter().any(|(k, v)| k == "title" && v == "Clean Room"),
        "title extracted from FLAC vorbis-comment block, got {md:?}",
    );
    assert!(
        md.iter().any(|(k, v)| k == "artist" && v == "OxideAV"),
        "artist extracted, got {md:?}",
    );
    assert!(
        md.iter().any(|(k, v)| k == "vendor" && v == "oxideav-test"),
        "vendor extracted, got {md:?}",
    );
}

#[test]
fn unknown_header_count_falls_back_to_one() {
    // Declared count 0 ("unknown" per §10.1). The demuxer absorbs just the
    // mapping packet; the next packet (here a vorbis-comment block) is then
    // delivered as content — the conservative fallback the spec allows.
    let mapping = flac_mapping_packet(0);
    let mut blob = Vec::new();
    let serial = 0x0F1A_C003u32;
    let mut seq = 0u32;
    blob.extend(build_page(flags::FIRST_PAGE, 0, serial, seq, &mapping));
    seq += 1;
    // One more packet then audio; with count 0 the demuxer only treats the
    // mapping packet as a header.
    let vc = flac_vorbis_comment_packet(true, &[("TITLE", "X")]);
    blob.extend(build_page(0, 0, serial, seq, &vc));
    seq += 1;
    blob.extend(build_page(flags::LAST_PAGE, 4096, serial, seq, &[0xF8, 1]));

    let reader: Box<dyn ReadSeek> = Box::new(Cursor::new(blob));
    let mut demux = oxideav_ogg::demux::open(reader, &NullCodecResolver).expect("open");
    // First delivered packet is the vorbis-comment block (treated as content
    // because the count was unknown), proving the fallback absorbed only 1.
    let first = demux.next_packet().expect("first content packet");
    assert_eq!(
        first.data[0] & 0x7F,
        4,
        "with unknown count, only the mapping packet is a header",
    );
}
