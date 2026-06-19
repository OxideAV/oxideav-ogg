//! Integration tests for RFC 3533 §4 **'nil' (zero-segment) pages**.
//!
//! §4 defines a nil page explicitly:
//!
//! > Eos pages may be 'nil' pages, that is, pages containing no content but
//! > simply a page header with position information and the eos flag set in
//! > the page header.
//!
//! and §5 reiterates that a zero-length packet is not an error:
//!
//! > Note also that a 'nil' (zero length) packet is not an error; it consists
//! > of nothing more than a lacing value of zero in the header.
//!
//! A nil EOS page therefore carries the stream's *final* granule position but
//! `number_page_segments = 0` (no segment table, no body). The demuxer must
//! (a) parse it without error, (b) deliver **no** spurious packet for it, and
//! (c) still read its granule for the duration estimate — a nil EOS page is a
//! common real-world way encoders flush the closing granulepos after the last
//! data packet already terminated on an earlier page.
//!
//! These complement the page-layer unit tests (which round-trip a nil page
//! through `Page::parse`/`to_bytes`) by exercising the full demuxer path.

use std::io::Cursor;

use oxideav_core::ReadSeek;
use oxideav_ogg::page::{flags, lace, Page};

fn vorbis_id_packet(channels: u8, sample_rate: u32) -> Vec<u8> {
    let mut p = Vec::with_capacity(30);
    p.push(0x01);
    p.extend_from_slice(b"vorbis");
    p.extend_from_slice(&0u32.to_le_bytes());
    p.push(channels);
    p.extend_from_slice(&sample_rate.to_le_bytes());
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

fn build_page(flags_byte: u8, granule: i64, serial: u32, seq: u32, packet: &[u8]) -> Vec<u8> {
    let lacing = lace(packet.len());
    Page {
        flags: flags_byte,
        granule_position: granule,
        serial,
        seq_no: seq,
        lacing,
        data: packet.to_vec(),
    }
    .to_bytes()
}

/// A nil page: `number_page_segments = 0`, no body, carrying only the page
/// header with its granule + flags (RFC 3533 §4).
fn build_nil_page(flags_byte: u8, granule: i64, serial: u32, seq: u32) -> Vec<u8> {
    let page = Page {
        flags: flags_byte,
        granule_position: granule,
        serial,
        seq_no: seq,
        lacing: Vec::new(),
        data: Vec::new(),
    };
    let bytes = page.to_bytes();
    // A nil page is exactly the 27-byte header (no segment table, no body).
    assert_eq!(bytes.len(), 27, "nil page must be a bare 27-byte header");
    assert_eq!(bytes[26], 0, "nil page must declare zero page segments");
    bytes
}

/// Build a single-stream Vorbis-in-Ogg file whose closing granulepos rides on
/// a **nil EOS page** rather than the last data page. The last data page is
/// NOT eos-flagged and carries granule 2880; the nil page that follows is
/// eos-flagged and carries the final granule 3840.
fn build_with_nil_eos(serial: u32) -> Vec<u8> {
    let mut out = Vec::new();
    let mut seq = 0u32;

    out.extend(build_page(
        flags::FIRST_PAGE,
        0,
        serial,
        seq,
        &vorbis_id_packet(2, 48_000),
    ));
    seq += 1;
    out.extend(build_page(0, 0, serial, seq, &vorbis_comment_packet()));
    seq += 1;
    out.extend(build_page(0, 0, serial, seq, &vorbis_setup_packet()));
    seq += 1;

    // Three data pages, none eos-flagged. granules 960, 1920, 2880.
    for i in 0..3u32 {
        let granule = 960 * (i as i64 + 1);
        let payload = vec![0xAB, i as u8];
        out.extend(build_page(0, granule, serial, seq, &payload));
        seq += 1;
    }

    // Nil EOS page: granule 3840, eos flag, no body.
    out.extend(build_nil_page(flags::LAST_PAGE, 3840, serial, seq));

    out
}

#[test]
fn nil_eos_page_yields_no_packet_and_demuxes_cleanly() {
    let bytes = build_with_nil_eos(0xABAB_ABAB);
    let reader: Box<dyn ReadSeek> = Box::new(Cursor::new(bytes));
    let mut demux = oxideav_ogg::demux::open(reader, &oxideav_core::NullCodecResolver)
        .expect("demux file with nil EOS page");

    let mut packets = Vec::new();
    while let Ok(p) = demux.next_packet() {
        packets.push(p);
    }

    // Exactly the three data packets — the nil EOS page contributes none.
    assert_eq!(
        packets.len(),
        3,
        "nil EOS page must not produce a spurious packet"
    );
    for (i, p) in packets.iter().enumerate() {
        assert_eq!(p.data, vec![0xAB, i as u8], "data packet {i} corrupted");
    }
}

#[test]
fn nil_eos_page_granule_drives_duration() {
    // The nil EOS page (granule 3840) is the rightmost page, so the open-time
    // end-of-file scan should read its granule for the duration even though it
    // carries no packet. 3840 samples @ 48000 Hz = 80.000 ms.
    let bytes = build_with_nil_eos(0xABAB_ABAB);
    let reader: Box<dyn ReadSeek> = Box::new(Cursor::new(bytes));
    let demux = oxideav_ogg::demux::open(reader, &oxideav_core::NullCodecResolver)
        .expect("demux file with nil EOS page");

    let dur = demux
        .duration_micros()
        .expect("duration from nil EOS page granule");
    let expected = 80_000i64;
    assert!(
        (dur - expected).abs() <= 2,
        "duration should come from the nil EOS page granule 3840 @48kHz = 80ms (±2µs), got {dur}µs"
    );
}

#[test]
fn nil_page_round_trips_through_parse() {
    // A nil page parses back to zero packet segments and preserves its
    // granule/flags — the page-layer guarantee the demuxer relies on.
    let bytes = build_nil_page(flags::LAST_PAGE, 12345, 0xDEAD_BEEF, 9);
    let (page, consumed) = Page::parse(&bytes).expect("parse nil page");
    assert_eq!(consumed, 27, "nil page consumes exactly its 27-byte header");
    assert_eq!(page.granule_position, 12345);
    assert_eq!(page.serial, 0xDEAD_BEEF);
    assert_eq!(page.seq_no, 9);
    assert!(page.is_last(), "eos flag preserved");
    assert!(page.lacing.is_empty(), "nil page has no segment table");
    assert!(page.data.is_empty(), "nil page has no body");
    assert!(
        page.packet_segments().is_empty(),
        "nil page yields zero packet segments"
    );
}

#[test]
fn nil_page_mid_stream_carries_continuation_granule_only() {
    // RFC 3533 §6 field 4: a page's granule "MAY contain the total number of
    // PCM samples encoded after including all frames finished on this page."
    // A page that finishes no packet uses granule -1; but a nil page MAY also
    // appear with a real granule to flush position information. Either way it
    // delivers no packet. Here a nil page with granule -1 (no packet finishes)
    // sits between two data pages and must be transparent to reassembly.
    let serial = 0x5151_5151u32;
    let mut out = Vec::new();
    let mut seq = 0u32;
    out.extend(build_page(
        flags::FIRST_PAGE,
        0,
        serial,
        seq,
        &vorbis_id_packet(1, 48_000),
    ));
    seq += 1;
    out.extend(build_page(0, 0, serial, seq, &vorbis_comment_packet()));
    seq += 1;
    out.extend(build_page(0, 0, serial, seq, &vorbis_setup_packet()));
    seq += 1;
    // data page 1
    out.extend(build_page(0, 960, serial, seq, &[0x11, 0x00]));
    seq += 1;
    // nil page with granule -1 (no packets finish on this page) mid-stream
    out.extend(build_nil_page(0, -1, serial, seq));
    seq += 1;
    // data page 2, eos
    out.extend(build_page(
        flags::LAST_PAGE,
        1920,
        serial,
        seq,
        &[0x11, 0x01],
    ));

    let reader: Box<dyn ReadSeek> = Box::new(Cursor::new(out));
    let mut demux = oxideav_ogg::demux::open(reader, &oxideav_core::NullCodecResolver)
        .expect("demux with mid-stream nil page");

    let mut packets = Vec::new();
    while let Ok(p) = demux.next_packet() {
        packets.push(p);
    }
    assert_eq!(
        packets.len(),
        2,
        "two data packets across a transparent mid-stream nil page"
    );
    assert_eq!(packets[0].data, vec![0x11, 0x00]);
    assert_eq!(packets[1].data, vec![0x11, 0x01]);
}
