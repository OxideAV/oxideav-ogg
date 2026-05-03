//! Integration tests for the Ogg demuxer's handling of chained logical
//! bitstreams (RFC 3533 §4 + Vorbis I §A.2).
//!
//! A chained Ogg file is the concatenation of independent logical bitstreams.
//! Each link begins with its own BOS page and ends with an EOS-flagged page.
//! The demuxer must register every BOS it encounters — not only the ones in
//! the initial BOS section — so packets from later links aren't silently
//! dropped.

use std::io::Cursor;

use oxideav_core::ReadSeek;
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
    let lacing = lace(packet.len());
    let page = Page {
        flags: flags_byte,
        granule_position: granule,
        serial,
        seq_no: seq,
        lacing,
        data: packet.to_vec(),
    };
    page.to_bytes()
}

/// Build one logical Vorbis-in-Ogg link: BOS + comment + setup + N data pages,
/// plus an EOS-flagged final page. `data_pages` data pages each carry one
/// `payload`-sized packet; granule increments by 960 per page.
fn build_link(serial: u32, payload_byte: u8, data_pages: usize) -> Vec<u8> {
    let mut out = Vec::new();
    let mut seq = 0u32;

    // BOS page: identification packet.
    out.extend(build_page(
        flags::FIRST_PAGE,
        0,
        serial,
        seq,
        &vorbis_id_packet(2, 48_000),
    ));
    seq += 1;

    // Comment packet (own page, granule 0).
    out.extend(build_page(0, 0, serial, seq, &vorbis_comment_packet()));
    seq += 1;

    // Setup packet (own page, granule 0).
    out.extend(build_page(0, 0, serial, seq, &vorbis_setup_packet()));
    seq += 1;

    // Data pages — last one gets the EOS flag.
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

/// Build a chained Ogg file: link A (serial 0xAAAA_AAAA, 3 data pages, byte
/// 0xAA) immediately followed by link B (serial 0xBBBB_BBBB, 4 data pages,
/// byte 0xBB).
fn build_two_link_chain() -> Vec<u8> {
    let mut out = Vec::new();
    out.extend(build_link(0xAAAA_AAAA, 0xAA, 3));
    out.extend(build_link(0xBBBB_BBBB, 0xBB, 4));
    out
}

#[test]
fn chained_streams_register_both_links() {
    let bytes = build_two_link_chain();
    let reader: Box<dyn ReadSeek> = Box::new(Cursor::new(bytes));
    let mut demux = oxideav_ogg::demux::open(reader, &oxideav_core::NullCodecResolver)
        .expect("demux chained ogg");

    // After open(), only the first link's stream is visible — the second
    // link's BOS lives mid-file and isn't seen until its first page is
    // read by next_packet().
    assert_eq!(
        demux.streams().len(),
        1,
        "first link's stream registered at open()"
    );
    assert_eq!(demux.streams()[0].params.codec_id.as_str(), "vorbis");

    // Drain every packet. The second link's BOS will trigger registration
    // of the second logical stream, and its data packets must be delivered
    // exactly like the first link's.
    let mut by_serial: std::collections::HashMap<u32, Vec<oxideav_core::Packet>> =
        std::collections::HashMap::new();
    while let Ok(p) = demux.next_packet() {
        by_serial.entry(p.stream_index).or_default().push(p);
    }

    // Both streams should now be registered.
    let streams = demux.streams();
    assert_eq!(
        streams.len(),
        2,
        "both chained links registered after drain (got {})",
        streams.len()
    );
    for s in streams {
        assert_eq!(s.params.codec_id.as_str(), "vorbis");
        assert_eq!(s.params.channels, Some(2));
        assert_eq!(s.params.sample_rate, Some(48_000));
    }

    // Each link contributed its data pages as packets (header packets are
    // absorbed and not delivered).
    let mut counts: Vec<usize> = by_serial.values().map(|v| v.len()).collect();
    counts.sort_unstable();
    assert_eq!(
        counts,
        vec![3, 4],
        "expected 3 data packets from link A and 4 from link B"
    );

    // Sanity-check payloads: each link's data packets begin with its
    // marker byte (0xAA or 0xBB).
    for packets in by_serial.values() {
        let marker = packets[0].data[0];
        assert!(
            marker == 0xAA || marker == 0xBB,
            "unexpected payload marker {marker:#04x}"
        );
        for p in packets {
            assert_eq!(p.data[0], marker, "payload marker mixed across streams");
        }
    }

    // Both markers must have appeared — i.e. data from BOTH links was
    // returned, not just the first one.
    let markers: std::collections::HashSet<u8> =
        by_serial.values().map(|pkts| pkts[0].data[0]).collect();
    assert!(
        markers.contains(&0xAA) && markers.contains(&0xBB),
        "both link payloads (0xAA + 0xBB) must be delivered"
    );
}

#[test]
fn chained_second_link_extradata_rebuilt() {
    // After both links' headers are absorbed, each stream's extradata
    // should be the Xiph-laced 3-packet blob a Vorbis decoder expects —
    // not just the bare identification packet that `register_stream`
    // initially stamps in.
    let bytes = build_two_link_chain();
    let reader: Box<dyn ReadSeek> = Box::new(Cursor::new(bytes));
    let mut demux = oxideav_ogg::demux::open(reader, &oxideav_core::NullCodecResolver)
        .expect("demux chained ogg");

    while let Ok(_p) = demux.next_packet() {}

    let streams = demux.streams();
    assert_eq!(streams.len(), 2);
    for s in streams {
        let extra = &s.params.extradata;
        // Xiph-laced 3-packet blob always begins with a 0x02 count byte
        // (n_packets - 1 = 2).
        assert_eq!(
            extra.first().copied(),
            Some(0x02),
            "stream {} extradata is not Xiph-laced (first byte = {:?})",
            s.index,
            extra.first()
        );
        // Length must exceed the bare identification packet (30 bytes).
        assert!(
            extra.len() > 30,
            "stream {} extradata length {} ≤ ID-packet size; setup packet missing?",
            s.index,
            extra.len()
        );
    }
}
