//! Integration tests for the RFC 3533 §4 unique-serial-number MUST.
//!
//! §4 requires serial uniqueness for both topologies:
//!
//! > Each grouped logical bitstream MUST have a unique serial number within
//! > the scope of the physical bitstream.
//!
//! > Each chained logical bitstream MUST have a unique serial number within
//! > the scope of the physical bitstream.
//!
//! A conforming encoder never reuses a `bitstream_serial_number`. When a
//! malformed file does, the demuxer must not silently splice the colliding
//! bitstreams' packets together (the old behaviour) — it detects the
//! collision, restarts the serial in place so each delivered packet still
//! belongs to a single bitstream, and tallies the violation on
//! `OggDemuxer::duplicate_serial_count()`.

use std::io::Cursor;

use oxideav_core::{Demuxer, ReadSeek};
use oxideav_ogg::page::{flags, lace, Page};

/// Minimal valid Vorbis identification packet (30 bytes).
fn vorbis_id_packet(channels: u8, sample_rate: u32) -> Vec<u8> {
    let mut p = Vec::with_capacity(30);
    p.push(0x01); // packet type
    p.extend_from_slice(b"vorbis");
    p.extend_from_slice(&0u32.to_le_bytes()); // version
    p.push(channels);
    p.extend_from_slice(&sample_rate.to_le_bytes());
    p.extend_from_slice(&0i32.to_le_bytes()); // br_max
    p.extend_from_slice(&128_000i32.to_le_bytes()); // br_nom
    p.extend_from_slice(&0i32.to_le_bytes()); // br_min
    p.push(0xB8); // blocksize nibbles
    p.push(0x01); // framing bit
    assert_eq!(p.len(), 30);
    p
}

fn vorbis_comment_packet() -> Vec<u8> {
    let mut p = Vec::new();
    p.push(0x03);
    p.extend_from_slice(b"vorbis");
    p.extend_from_slice(&0u32.to_le_bytes()); // vendor len
    p.extend_from_slice(&0u32.to_le_bytes()); // user count
    p.push(0x01); // framing bit
    p
}

fn vorbis_setup_packet() -> Vec<u8> {
    let mut p = Vec::new();
    p.push(0x05);
    p.extend_from_slice(b"vorbis");
    p.extend_from_slice(&[0u8; 16]);
    p
}

/// Build a single Ogg page carrying one packet.
fn build_page(flags_byte: u8, granule: i64, serial: u32, seq: u32, packet: &[u8]) -> Vec<u8> {
    let page = Page {
        flags: flags_byte,
        granule_position: granule,
        serial,
        seq_no: seq,
        lacing: lace(packet.len()),
        data: packet.to_vec(),
    };
    page.to_bytes()
}

/// Emit one Vorbis logical link's full page run with an explicit serial and a
/// caller-chosen per-data-page payload byte. Returns the bytes. `data_pages`
/// data pages each carry a 2-byte `[payload_byte, i]` packet; the last gets
/// the EOS flag. Sequence numbers are per-stream and start at 0 (a BOS legally
/// restarts the counter), matching the chained_streams.rs fixture shape.
fn build_link(serial: u32, payload_byte: u8, data_pages: usize) -> Vec<u8> {
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

    for i in 0..data_pages {
        let granule = 960 * (i as i64 + 1);
        let mut flag = 0u8;
        if i + 1 == data_pages {
            flag |= flags::LAST_PAGE;
        }
        let payload = vec![payload_byte, i as u8];
        out.extend(build_page(flag, granule, serial, seq, &payload));
        seq += 1;
    }
    out
}

fn open(bytes: Vec<u8>) -> oxideav_ogg::demux::OggDemuxer {
    let reader: Box<dyn ReadSeek> = Box::new(Cursor::new(bytes));
    oxideav_ogg::demux::open_concrete(reader, &oxideav_core::NullCodecResolver).expect("demux ogg")
}

/// Drain every packet, bucketing payloads by stream index.
fn drain(demux: &mut oxideav_ogg::demux::OggDemuxer) -> Vec<(u32, Vec<u8>)> {
    let mut out = Vec::new();
    while let Ok(p) = demux.next_packet() {
        out.push((p.stream_index, p.data.to_vec()));
    }
    out
}

#[test]
fn clean_chain_reports_zero_duplicate_serials() {
    // Two links with DISTINCT serials — fully conforming.
    let mut bytes = build_link(0xAAAA_AAAA, 0xAA, 3);
    bytes.extend(build_link(0xBBBB_BBBB, 0xBB, 4));
    let mut demux = open(bytes);
    let _ = drain(&mut demux);
    assert_eq!(
        demux.duplicate_serial_count(),
        0,
        "a conforming chained file must report zero serial collisions"
    );
    assert_eq!(demux.streams().len(), 2);
}

#[test]
fn chained_link_reusing_a_serial_is_detected_and_recovered() {
    // §4 chaining violation: link B reuses link A's serial 0xAAAA_AAAA. A
    // conforming file would give B a fresh serial; this one does not.
    let mut bytes = build_link(0xAAAA_AAAA, 0xAA, 3);
    bytes.extend(build_link(0xAAAA_AAAA, 0xBB, 4));
    let mut demux = open(bytes);
    let packets = drain(&mut demux);

    assert_eq!(
        demux.duplicate_serial_count(),
        1,
        "the reused serial on link B is one §4 unique-serial violation"
    );

    // The two links share one stream slot (at the container layer the reused
    // serial is indistinguishable), but no packet is a frankenpacket spliced
    // from both links. Every delivered packet is a clean 2-byte payload whose
    // first byte is either link A's 0xAA or link B's 0xBB — never a mixture.
    let payloads: Vec<u8> = packets.iter().map(|(_, d)| d[0]).collect();
    assert!(
        payloads.iter().all(|&b| b == 0xAA || b == 0xBB),
        "no packet may mix the two links' bytes: {payloads:02x?}"
    );
    // Link A contributed 3 data packets (0xAA) and link B 4 (0xBB).
    let a = payloads.iter().filter(|&&b| b == 0xAA).count();
    let b = payloads.iter().filter(|&&b| b == 0xBB).count();
    assert_eq!(
        (a, b),
        (3, 4),
        "both links' data packets are delivered intact"
    );

    // The restart opens a new chained link, so the serial spans two links.
    assert_eq!(demux.link_count(), 2, "the reused-serial BOS opens link 1");
}

#[test]
fn grouped_streams_sharing_a_serial_are_detected() {
    // §4 grouping violation: two BOS pages in the INITIAL bos section share
    // serial 0xCAFE. Build the section by hand: BOS(0xCAFE, vorbis) then a
    // SECOND BOS(0xCAFE, vorbis) — both before any data page — then one data
    // page to close the bos section, then an EOS.
    let mut bytes = Vec::new();
    bytes.extend(build_page(
        flags::FIRST_PAGE,
        0,
        0xCAFE,
        0,
        &vorbis_id_packet(2, 48_000),
    ));
    // Second grouped BOS reusing the same serial — the violation.
    bytes.extend(build_page(
        flags::FIRST_PAGE,
        0,
        0xCAFE,
        0,
        &vorbis_id_packet(1, 44_100),
    ));
    // Remaining headers + a data page so the stream is walkable.
    bytes.extend(build_page(0, 0, 0xCAFE, 1, &vorbis_comment_packet()));
    bytes.extend(build_page(0, 0, 0xCAFE, 2, &vorbis_setup_packet()));
    bytes.extend(build_page(flags::LAST_PAGE, 960, 0xCAFE, 3, &[0xDD, 0x00]));

    let mut demux = open(bytes);
    assert_eq!(
        demux.duplicate_serial_count(),
        1,
        "the second grouped BOS on the same serial is one violation (caught at open)"
    );
    // Only one stream slot exists for the colliding serial.
    assert_eq!(demux.streams().len(), 1);

    let packets = drain(&mut demux);
    // The data packet is delivered cleanly; the duplicate BOS's ident packet
    // was re-captured as a header (not emitted as content).
    assert_eq!(packets.len(), 1, "exactly the one data packet is delivered");
    assert_eq!(packets[0].1, vec![0xDD, 0x00]);
}

#[test]
fn build_seek_index_counts_the_collision_once() {
    // The header-only scan is the authoritative file-wide source. Running it
    // on a reused-serial chain must report exactly one collision, and a
    // subsequent next_packet drain must NOT double-count it.
    let mut bytes = build_link(0xAAAA_AAAA, 0xAA, 3);
    bytes.extend(build_link(0xAAAA_AAAA, 0xBB, 4));
    let mut demux = open(bytes);

    demux.build_seek_index().expect("build seek index");
    assert_eq!(
        demux.duplicate_serial_count(),
        1,
        "the header scan tallies the reused serial once"
    );

    let _ = drain(&mut demux);
    assert_eq!(
        demux.duplicate_serial_count(),
        1,
        "the next_packet drain must not double-count a collision the index scan already saw"
    );
}

#[test]
fn open_indexed_does_not_double_count() {
    // open_indexed runs build_seek_index immediately after open, then the
    // caller drains via next_packet. The collision must be counted once.
    let mut bytes = build_link(0x1111_1111, 0x11, 2);
    bytes.extend(build_link(0x1111_1111, 0x22, 2));
    let reader: Box<dyn ReadSeek> = Box::new(Cursor::new(bytes));
    let mut demux = oxideav_ogg::demux::open_indexed(reader, &oxideav_core::NullCodecResolver)
        .expect("open_indexed ogg");
    // Drain through the boxed Demuxer interface.
    while demux.next_packet().is_ok() {}
    // The concrete accessor isn't reachable through the boxed trait, so
    // re-open with open_concrete + build_seek_index to assert the count.
    let mut bytes2 = build_link(0x1111_1111, 0x11, 2);
    bytes2.extend(build_link(0x1111_1111, 0x22, 2));
    let mut c = open(bytes2);
    c.build_seek_index().expect("build seek index");
    while c.next_packet().is_ok() {}
    assert_eq!(
        c.duplicate_serial_count(),
        1,
        "exactly one collision across index-build + drain"
    );
}

#[test]
fn three_way_serial_reuse_counts_each_repeat() {
    // A serial reused across THREE chained links: the first use is legal, the
    // two re-uses are each a violation, so the tally is 2.
    let mut bytes = build_link(0x7777_7777, 0x01, 2);
    bytes.extend(build_link(0x7777_7777, 0x02, 2));
    bytes.extend(build_link(0x7777_7777, 0x03, 2));
    let mut demux = open(bytes);
    let _ = drain(&mut demux);
    assert_eq!(
        demux.duplicate_serial_count(),
        2,
        "two re-uses of the same serial are two violations"
    );
    assert_eq!(demux.link_count(), 3, "each reused-serial BOS opens a link");
}
