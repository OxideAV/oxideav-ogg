//! Chained-stream metadata: a chained link whose headers complete *mid-file*
//! (after `open()` returns) must still surface its Vorbis-comment tags for
//! every supported mapping — not just Vorbis / Opus / Theora.
//!
//! The open-time metadata sweep parses all five mappings (Vorbis, Opus,
//! Theora, Speex, FLAC). The per-stream path that runs when a later chained
//! link's header packets finish during `next_packet` draining used to handle
//! only Vorbis / Opus / Theora, silently dropping a chained Speex or FLAC
//! link's tags. Both paths now share one parser.
//!
//! RFC 3533 §4 (`docs/container/ogg/rfc3533-ogg.txt`): a chained physical
//! bitstream is the back-to-back concatenation of independent logical
//! bitstreams, each with its own BOS/EOS. The second link's BOS lives
//! mid-file and is only seen once its first page is drained.

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

// ---- Link 0: a minimal Vorbis bitstream (so the FLAC/Speex link is genuinely
//      a *mid-file* chained link discovered during drain, not the open-time
//      first link). ----

fn vorbis_id_packet() -> Vec<u8> {
    let mut p = vec![0x01];
    p.extend_from_slice(b"vorbis");
    p.extend_from_slice(&0u32.to_le_bytes()); // version
    p.push(2); // channels
    p.extend_from_slice(&48_000u32.to_le_bytes()); // sample rate
    p.extend_from_slice(&0u32.to_le_bytes());
    p.extend_from_slice(&128_000u32.to_le_bytes());
    p.extend_from_slice(&0u32.to_le_bytes());
    p.push(0xB8);
    p.push(0x01);
    p
}

fn vorbis_comment_packet(comments: &[(&str, &str)]) -> Vec<u8> {
    let mut p = vec![0x03];
    p.extend_from_slice(b"vorbis");
    let vendor = b"vorbis-link";
    p.extend_from_slice(&(vendor.len() as u32).to_le_bytes());
    p.extend_from_slice(vendor);
    p.extend_from_slice(&(comments.len() as u32).to_le_bytes());
    for (k, v) in comments {
        let e = format!("{k}={v}");
        p.extend_from_slice(&(e.len() as u32).to_le_bytes());
        p.extend_from_slice(e.as_bytes());
    }
    p.push(0x01); // framing bit
    p
}

fn vorbis_setup_packet() -> Vec<u8> {
    let mut p = vec![0x05];
    p.extend_from_slice(b"vorbis");
    p.extend_from_slice(&[0u8; 16]);
    p
}

fn build_vorbis_link(serial: u32, data_pages: i64) -> Vec<u8> {
    let mut out = Vec::new();
    let mut seq = 0u32;
    out.extend(build_page(
        flags::FIRST_PAGE,
        0,
        serial,
        seq,
        &vorbis_id_packet(),
    ));
    seq += 1;
    out.extend(build_page(
        0,
        0,
        serial,
        seq,
        &vorbis_comment_packet(&[("TITLE", "Link Zero")]),
    ));
    seq += 1;
    out.extend(build_page(0, 0, serial, seq, &vorbis_setup_packet()));
    seq += 1;
    for i in 1..=data_pages {
        let last = i == data_pages;
        let flag = if last { flags::LAST_PAGE } else { 0 };
        out.extend(build_page(flag, 960 * i, serial, seq, &[0xAB, i as u8]));
        seq += 1;
    }
    out
}

// ---- FLAC link helpers (mirrors tests/flac_mapping.rs). ----

fn flac_mapping_packet(header_packets: u16) -> Vec<u8> {
    let mut p = vec![0x7F];
    p.extend_from_slice(b"FLAC");
    p.extend_from_slice(&[0x01, 0x00]); // mapping version 1.0
    p.extend_from_slice(&header_packets.to_be_bytes());
    p.extend_from_slice(b"fLaC");
    p.extend_from_slice(&[0x00, 0x00, 0x00, 34]); // STREAMINFO header
    p.extend_from_slice(&[0u8; 34]);
    p
}

fn flac_vorbis_comment_packet(last: bool, comments: &[(&str, &str)]) -> Vec<u8> {
    let mut body = Vec::new();
    let vendor = b"flac-link";
    body.extend_from_slice(&(vendor.len() as u32).to_le_bytes());
    body.extend_from_slice(vendor);
    body.extend_from_slice(&(comments.len() as u32).to_le_bytes());
    for (k, v) in comments {
        let e = format!("{k}={v}");
        body.extend_from_slice(&(e.len() as u32).to_le_bytes());
        body.extend_from_slice(e.as_bytes());
    }
    let type_byte = 4u8 | if last { 0x80 } else { 0 };
    let len = body.len() as u32;
    let mut p = vec![type_byte, (len >> 16) as u8, (len >> 8) as u8, len as u8];
    p.extend_from_slice(&body);
    p
}

fn build_flac_link(serial: u32, comment: &[(&str, &str)], data_pages: i64) -> Vec<u8> {
    let extra = vec![flac_vorbis_comment_packet(true, comment)];
    let mut out = Vec::new();
    let mut seq = 0u32;
    out.extend(build_page(
        flags::FIRST_PAGE,
        0,
        serial,
        seq,
        &flac_mapping_packet(extra.len() as u16),
    ));
    seq += 1;
    for h in &extra {
        out.extend(build_page(0, 0, serial, seq, h));
        seq += 1;
    }
    for i in 1..=data_pages {
        let last = i == data_pages;
        let flag = if last { flags::LAST_PAGE } else { 0 };
        out.extend(build_page(flag, 4096 * i, serial, seq, &[0xF8, i as u8]));
        seq += 1;
    }
    out
}

// ---- Speex link helpers. The Speex comment header is the 2nd packet, a bare
//      vorbis_comment structure with no magic prefix. ----

fn speex_id_packet() -> Vec<u8> {
    // Fixed 80-byte little-endian header. Only the magic + rate/channel fields
    // matter to the container; the rest is zero-filled.
    let mut p = Vec::with_capacity(80);
    p.extend_from_slice(b"Speex   "); // 8-byte magic (incl. trailing spaces)
    p.extend_from_slice(&[0u8; 20]); // speex_version string
    p.extend_from_slice(&1u32.to_le_bytes()); // speex_version_id @28
    p.extend_from_slice(&80u32.to_le_bytes()); // header_size @32
    p.extend_from_slice(&16_000u32.to_le_bytes()); // rate @36
    p.extend_from_slice(&0u32.to_le_bytes()); // mode @40
    p.extend_from_slice(&0u32.to_le_bytes()); // mode_bitstream_version @44
    p.extend_from_slice(&1u32.to_le_bytes()); // nb_channels @48
    p.extend_from_slice(&0u32.to_le_bytes()); // bitrate @52
    p.extend_from_slice(&160u32.to_le_bytes()); // frame_size @56
    p.extend_from_slice(&0u32.to_le_bytes()); // vbr @60
    p.extend_from_slice(&1u32.to_le_bytes()); // frames_per_packet @64
    p.extend_from_slice(&0u32.to_le_bytes()); // extra_headers @68
    p.extend_from_slice(&0u32.to_le_bytes()); // reserved1 @72
    p.extend_from_slice(&0u32.to_le_bytes()); // reserved2 @76
    assert_eq!(p.len(), 80);
    p
}

fn speex_comment_packet(comments: &[(&str, &str)]) -> Vec<u8> {
    let mut p = Vec::new();
    let vendor = b"speex-link";
    p.extend_from_slice(&(vendor.len() as u32).to_le_bytes());
    p.extend_from_slice(vendor);
    p.extend_from_slice(&(comments.len() as u32).to_le_bytes());
    for (k, v) in comments {
        let e = format!("{k}={v}");
        p.extend_from_slice(&(e.len() as u32).to_le_bytes());
        p.extend_from_slice(e.as_bytes());
    }
    p
}

fn build_speex_link(serial: u32, comment: &[(&str, &str)], data_pages: i64) -> Vec<u8> {
    let mut out = Vec::new();
    let mut seq = 0u32;
    out.extend(build_page(
        flags::FIRST_PAGE,
        0,
        serial,
        seq,
        &speex_id_packet(),
    ));
    seq += 1;
    out.extend(build_page(
        0,
        0,
        serial,
        seq,
        &speex_comment_packet(comment),
    ));
    seq += 1;
    for i in 1..=data_pages {
        let last = i == data_pages;
        let flag = if last { flags::LAST_PAGE } else { 0 };
        out.extend(build_page(flag, 160 * i, serial, seq, &[0x12, i as u8]));
        seq += 1;
    }
    out
}

fn drain<R: oxideav_core::Demuxer + ?Sized>(d: &mut R) {
    while d.next_packet().is_ok() {}
}

#[test]
fn chained_flac_link_metadata_surfaces_after_drain() {
    // Vorbis link 0 (open-time) chained to a FLAC link 1 (mid-file). The FLAC
    // link's headers complete during drain, exercising populate_metadata_for.
    let mut bytes = build_vorbis_link(0xAAAA_0001, 2);
    bytes.extend(build_flac_link(
        0x0F1A_C010,
        &[("TITLE", "Second Link"), ("ARTIST", "Chained FLAC")],
        3,
    ));
    let reader: Box<dyn ReadSeek> = Box::new(Cursor::new(bytes));
    let mut demux = oxideav_ogg::demux::open(reader, &NullCodecResolver).expect("open");
    drain(&mut *demux);
    let md = demux.metadata();
    assert!(
        md.iter().any(|(k, v)| k == "title" && v == "Link Zero"),
        "link 0 (Vorbis) tags present, got {md:?}"
    );
    assert!(
        md.iter().any(|(k, v)| k == "artist" && v == "Chained FLAC"),
        "chained FLAC link's artist must surface after drain, got {md:?}"
    );
    assert!(
        md.iter().any(|(k, v)| k == "vendor" && v == "flac-link"),
        "chained FLAC link's vendor must surface, got {md:?}"
    );
}

#[test]
fn chained_speex_link_metadata_surfaces_after_drain() {
    let mut bytes = build_vorbis_link(0xAAAA_0002, 2);
    bytes.extend(build_speex_link(
        0x5EE0_0010,
        &[("TITLE", "Speex Second"), ("LICENSE", "MIT")],
        3,
    ));
    let reader: Box<dyn ReadSeek> = Box::new(Cursor::new(bytes));
    let mut demux = oxideav_ogg::demux::open(reader, &NullCodecResolver).expect("open");
    drain(&mut *demux);
    let md = demux.metadata();
    assert!(
        md.iter().any(|(k, v)| k == "title" && v == "Speex Second"),
        "chained Speex link's title must surface after drain, got {md:?}"
    );
    assert!(
        md.iter().any(|(k, v)| k == "license" && v == "MIT"),
        "chained Speex link's license must surface, got {md:?}"
    );
    assert!(
        md.iter().any(|(k, v)| k == "vendor" && v == "speex-link"),
        "chained Speex link's vendor must surface, got {md:?}"
    );
}
