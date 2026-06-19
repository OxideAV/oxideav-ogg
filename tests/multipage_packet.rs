//! Integration tests for **clean** reassembly of packets that span several
//! Ogg pages (RFC 3533 §5):
//!
//! > As Ogg pages have a maximum size of about 64 kBytes, sometimes a packet
//! > has to be distributed over several pages. To simplify that process, Ogg
//! > divides each packet into 255 byte long chunks plus a final shorter chunk.
//!
//! and §6 field 3 (the `continued` header flag) plus the lacing rule from §5:
//!
//! > a lacing value of 255 implies that a second lacing value follows in the
//! > packet, and a value of less than 255 marks the end of the packet ... A
//! > packet of 255 bytes (or a multiple of 255 bytes) is terminated by a
//! > lacing value of 0.
//!
//! The existing `page_loss.rs` tests cover the *lossy* multi-page case (a
//! middle page dropped → the spanning packet is discarded, not spliced). These
//! tests cover the *clean* case: a packet whose bytes are split across 2, 3,
//! and 4 pages must reassemble **byte-for-byte**, with the `continued` flag
//! set on every page after the first and the terminating segment on the last
//! page. The exact-multiple-of-255 boundary (where the zero-terminator lands
//! at the very start of a fresh page) is included because that is the case the
//! §5 "terminated by a lacing value of 0" rule exists for.

use std::io::Cursor;

use oxideav_core::ReadSeek;
use oxideav_ogg::page::{flags, Page};

fn vorbis_id_packet() -> Vec<u8> {
    let mut p = Vec::with_capacity(30);
    p.push(0x01);
    p.extend_from_slice(b"vorbis");
    p.extend_from_slice(&0u32.to_le_bytes());
    p.push(2);
    p.extend_from_slice(&48_000u32.to_le_bytes());
    p.extend_from_slice(&0i32.to_le_bytes());
    p.extend_from_slice(&128_000i32.to_le_bytes());
    p.extend_from_slice(&0i32.to_le_bytes());
    p.push(0xB8);
    p.push(0x01);
    assert_eq!(p.len(), 30);
    p
}

fn vorbis_comment_packet() -> Vec<u8> {
    let mut p = Vec::new();
    p.push(0x03);
    p.extend_from_slice(b"vorbis");
    p.extend_from_slice(&0u32.to_le_bytes());
    p.extend_from_slice(&0u32.to_le_bytes());
    p.push(0x01);
    p
}

fn vorbis_setup_packet() -> Vec<u8> {
    let mut p = Vec::new();
    p.push(0x05);
    p.extend_from_slice(b"vorbis");
    p.extend_from_slice(&[0u8; 16]);
    p
}

const SERIAL: u32 = 0x600D_F00D;

fn raw_page(flags_byte: u8, granule: i64, seq: u32, lacing: Vec<u8>, data: Vec<u8>) -> Vec<u8> {
    let total: usize = lacing.iter().map(|&v| v as usize).sum();
    assert_eq!(total, data.len(), "lacing sum must match data length");
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

/// Emit the three Vorbis header pages (seq 0..=2); returns next seq (3).
fn header_pages(out: &mut Vec<u8>) -> u32 {
    out.extend(raw_page(
        flags::FIRST_PAGE,
        0,
        0,
        vec![30],
        vorbis_id_packet(),
    ));
    let c = vorbis_comment_packet();
    out.extend(raw_page(0, 0, 1, vec![c.len() as u8], c));
    let s = vorbis_setup_packet();
    out.extend(raw_page(0, 0, 2, vec![s.len() as u8], s));
    3
}

/// Split `packet` into per-page byte chunks of `chunk` bytes each (the last
/// chunk may be shorter), emitting one page per chunk. The first page is
/// fresh; every subsequent page sets the `continued` flag. Only the final
/// page carries a terminating lacing segment (< 255) and the granule.
///
/// Within a page, `chunk` bytes are laced as `floor(chunk/255)` 255-segments
/// plus the remainder. A non-final page must end on a 255 segment (so the
/// packet continues), so `chunk` is required to be a positive multiple of 255
/// for every page except the last.
fn emit_spanning_packet(
    out: &mut Vec<u8>,
    packet: &[u8],
    chunk: usize,
    granule: i64,
    first_seq: u32,
    eos: bool,
) -> u32 {
    assert!(
        chunk % 255 == 0 && chunk > 0,
        "non-final chunk must be k*255"
    );
    let mut seq = first_seq;
    let mut off = 0usize;
    let mut first = true;
    while off < packet.len() {
        let remaining = packet.len() - off;
        let is_last_page = remaining <= chunk || remaining < 255;
        let take = if is_last_page { remaining } else { chunk };
        let slice = packet[off..off + take].to_vec();

        // Build the lacing table for this page's slice.
        let mut lacing = Vec::new();
        if is_last_page {
            // Final page: full 255-segments then a terminator < 255 (or a
            // 0 terminator when `take` is an exact multiple of 255).
            let full = take / 255;
            let rem = take % 255;
            lacing.resize(lacing.len() + full, 255u8);
            lacing.push(rem as u8); // rem < 255, or 0 for exact-multiple
        } else {
            // Continuing page: all 255-segments, no terminator.
            assert_eq!(take % 255, 0);
            lacing.resize(lacing.len() + take / 255, 255u8);
        }

        let mut fl = 0u8;
        if !first {
            fl |= flags::CONTINUED;
        }
        let g = if is_last_page && eos {
            fl |= flags::LAST_PAGE;
            granule
        } else if is_last_page {
            granule
        } else {
            -1 // no packet finishes on a continuing page (RFC 3533 §6 field 4)
        };

        out.extend(raw_page(fl, g, seq, lacing, slice));
        seq += 1;
        off += take;
        first = false;
    }
    seq
}

/// Drain a built blob and return only the data packets (headers absorbed).
fn drain(bytes: Vec<u8>) -> Vec<Vec<u8>> {
    let reader: Box<dyn ReadSeek> = Box::new(Cursor::new(bytes));
    let mut dmx = oxideav_ogg::demux::open(reader, &oxideav_core::NullCodecResolver)
        .expect("open multipage ogg");
    let mut packets = Vec::new();
    while let Ok(p) = dmx.next_packet() {
        packets.push(p.data);
    }
    packets
}

#[test]
fn packet_spanning_two_pages_reassembles_byte_exact() {
    // 300-byte packet → page 1 = 255 bytes (lacing [255]), page 2 = 45 bytes
    // (lacing [45], terminates). Byte i = (i * 7 + 3) & 0xff for a distinctive
    // pattern.
    let packet: Vec<u8> = (0..300).map(|i| ((i * 7 + 3) & 0xff) as u8).collect();
    let mut out = Vec::new();
    let seq = header_pages(&mut out);
    emit_spanning_packet(&mut out, &packet, 255, 960, seq, true);

    let packets = drain(out);
    assert_eq!(packets.len(), 1, "one reassembled packet");
    assert_eq!(
        packets[0], packet,
        "2-page packet must reassemble byte-exact"
    );
}

#[test]
fn packet_spanning_three_pages_reassembles_byte_exact() {
    // 600-byte packet, 255 bytes per page → pages of 255, 255, 90 bytes.
    let packet: Vec<u8> = (0..600).map(|i| ((i * 13 + 1) & 0xff) as u8).collect();
    let mut out = Vec::new();
    let seq = header_pages(&mut out);
    emit_spanning_packet(&mut out, &packet, 255, 960, seq, true);

    let packets = drain(out);
    assert_eq!(packets.len(), 1);
    assert_eq!(
        packets[0], packet,
        "3-page packet must reassemble byte-exact"
    );
}

#[test]
fn packet_spanning_four_pages_with_multi_segment_pages() {
    // A large packet split 510 bytes per page (2 full 255-segments each) →
    // exercises multi-segment continuing pages, not just single-255 pages.
    // 1800-byte packet → pages of 510, 510, 510, 270.
    let packet: Vec<u8> = (0..1800).map(|i| ((i * 5 + 9) & 0xff) as u8).collect();
    let mut out = Vec::new();
    let seq = header_pages(&mut out);
    emit_spanning_packet(&mut out, &packet, 510, 960, seq, true);

    let packets = drain(out);
    assert_eq!(packets.len(), 1);
    assert_eq!(
        packets[0], packet,
        "4-page packet must reassemble byte-exact"
    );
}

#[test]
fn packet_exact_multiple_of_255_terminates_on_fresh_page() {
    // The §5 zero-terminator case: a packet whose length is an exact multiple
    // of 255 ends with a lacing value of 0. We force that terminator onto a
    // FRESH page so the final page is a continued page carrying ONLY the
    // 1-byte zero-lacing terminator and no data — the boundary case the
    // "terminated by a lacing value of 0" rule exists for.
    //
    //   packet = 510 bytes (2 * 255).
    //   page 1: lacing [255, 255], data = all 510 bytes, unterminated.
    //   page 2: lacing [0], data = empty — the zero terminator, CONTINUED.
    let packet: Vec<u8> = (0..510).map(|i| ((i * 3 + 2) & 0xff) as u8).collect();
    let mut out = Vec::new();
    let seq = header_pages(&mut out);

    // Page 1: the whole 510 bytes as two 255-segments, no terminator.
    out.extend(raw_page(0, -1, seq, vec![255, 255], packet.clone()));
    // Page 2: a single zero lacing value (the terminator), CONTINUED, EOS.
    out.extend(raw_page(
        flags::CONTINUED | flags::LAST_PAGE,
        960,
        seq + 1,
        vec![0],
        Vec::new(),
    ));

    let packets = drain(out);
    assert_eq!(packets.len(), 1, "the zero-terminator completes one packet");
    assert_eq!(
        packets[0], packet,
        "exact-multiple-of-255 packet reassembles with the terminator on a fresh page"
    );
}

#[test]
fn multiple_whole_packets_then_one_spanning_packet() {
    // Mix: two small whole packets on their own pages, then one 700-byte
    // packet spanning three pages, all on the same stream. Verifies the
    // demuxer doesn't lose its place switching between whole-packet pages and
    // spanning-packet pages.
    let small_a = vec![0x11u8; 40];
    let small_b = vec![0x22u8; 80];
    let big: Vec<u8> = (0..700).map(|i| ((i * 11 + 4) & 0xff) as u8).collect();

    let mut out = Vec::new();
    let mut seq = header_pages(&mut out);
    out.extend(raw_page(0, 100, seq, vec![40], small_a.clone()));
    seq += 1;
    out.extend(raw_page(0, 200, seq, vec![80], small_b.clone()));
    seq += 1;
    emit_spanning_packet(&mut out, &big, 255, 960, seq, true);

    let packets = drain(out);
    assert_eq!(packets.len(), 3, "two small + one big packet");
    assert_eq!(packets[0], small_a);
    assert_eq!(packets[1], small_b);
    assert_eq!(packets[2], big, "spanning packet after whole packets");
}
