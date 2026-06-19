//! Integration tests for the **mixed grouping + chaining** Ogg topology
//! that RFC 3533 §4 spells out as the most general legal physical bitstream:
//!
//! > It is possible to consecutively chain groups of concurrently
//! > multiplexed bitstreams. The groups, when unchained, MUST stand on
//! > their own as a valid concurrently multiplexed bitstream.
//!
//! The §4 schematic worked example is:
//!
//! ```text
//! |*A*|*B*|*C*|A|A|C|B|A|B|#A#|C|...|B|C|#B#|#C#|*D*|D|...|#D#|
//!  bos bos bos             eos           eos eos bos       eos
//! ```
//!
//! i.e. link 0 is a *grouped* (concurrently multiplexed) set of three
//! logical bitstreams A, B, C whose BOS pages all precede any data page,
//! whose data pages interleave in no particular order, and whose EOS pages
//! need not be contiguous; link 1 is a single bitstream D chained after the
//! whole group ends.
//!
//! The other chained tests (`chained_streams.rs`, `chained_duration.rs`)
//! only exercise **one stream per link**. These tests assert the demuxer
//! gets the *grouped-and-chained* shape right end to end: every grouped
//! stream registers under the same `link_index`, the chained link's
//! stream(s) register under the next `link_index`, interleaved data pages
//! reassemble per-serial without cross-contamination, EOS pages that are
//! not contiguous do not confuse link-boundary detection, and the per-link
//! / per-stream diagnostics (`link_count`, `stream_link_index`,
//! `stream_serial`, chained duration sum) all report the §4 topology
//! correctly.

use std::collections::{HashMap, HashSet};
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

/// A logical Vorbis stream being assembled into a grouped link: tracks its
/// own per-stream page sequence number (RFC 3533 §6 field 6: the sequence
/// number "is increasing on each logical bitstream separately") and the
/// marker byte its data packets carry.
struct StreamBuilder {
    serial: u32,
    marker: u8,
    seq: u32,
    sample_rate: u32,
}

impl StreamBuilder {
    fn new(serial: u32, marker: u8, sample_rate: u32) -> Self {
        StreamBuilder {
            serial,
            marker,
            seq: 0,
            sample_rate,
        }
    }

    /// The BOS identification page for this stream.
    fn bos_page(&mut self) -> Vec<u8> {
        let p = build_page(
            flags::FIRST_PAGE,
            0,
            self.serial,
            self.seq,
            &vorbis_id_packet(2, self.sample_rate),
        );
        self.seq += 1;
        p
    }

    /// The two remaining header pages (comment + setup), each on its own page.
    fn header_pages(&mut self) -> Vec<u8> {
        let mut out = Vec::new();
        out.extend(build_page(
            0,
            0,
            self.serial,
            self.seq,
            &vorbis_comment_packet(),
        ));
        self.seq += 1;
        out.extend(build_page(
            0,
            0,
            self.serial,
            self.seq,
            &vorbis_setup_packet(),
        ));
        self.seq += 1;
        out
    }

    /// One data page carrying a 2-byte `[marker, ordinal]` packet, with the
    /// given absolute granule. `eos` sets the last-page flag.
    fn data_page(&mut self, ordinal: u8, granule: i64, eos: bool) -> Vec<u8> {
        let flag = if eos { flags::LAST_PAGE } else { 0 };
        let payload = vec![self.marker, ordinal];
        let p = build_page(flag, granule, self.serial, self.seq, &payload);
        self.seq += 1;
        p
    }
}

/// Build the RFC 3533 §4 worked-example topology: a first link that groups
/// three Vorbis streams A/B/C (all BOS pages first, then interleaved data
/// pages, with non-contiguous EOS), chained to a second link of a single
/// Vorbis stream D.
///
/// Returns `(bytes, [serial_a, serial_b, serial_c, serial_d])`.
fn build_grouped_then_chained() -> (Vec<u8>, [u32; 4]) {
    let mut a = StreamBuilder::new(0x0A0A_0A0A, 0xAA, 48_000);
    let mut b = StreamBuilder::new(0x0B0B_0B0B, 0xBB, 44_100);
    let mut c = StreamBuilder::new(0x0C0C_0C0C, 0xCC, 48_000);

    let mut out = Vec::new();

    // --- Link 0: grouped A, B, C ---
    // RFC 3533 §4: "all bos pages of all logical bitstreams MUST appear
    // together at the beginning" — emit all three BOS first.
    out.extend(a.bos_page());
    out.extend(b.bos_page());
    out.extend(c.bos_page());
    // Each stream's secondary header pages still precede its data.
    out.extend(a.header_pages());
    out.extend(b.header_pages());
    out.extend(c.header_pages());

    // Interleaved data pages "in no particular order", with A ending first
    // (#A#) well before B and C — exercising the §4 "a grouped bitstream
    // can end long before the other bitstreams in the group end" and "eos
    // pages ... need not all occur contiguously" clauses.
    // A: 2 data pages, granule 960, 1920 (ends here).
    // B: 4 data pages, granule 882, 1764, 2646, 3528.
    // C: 3 data pages, granule 960, 1920, 2880.
    out.extend(a.data_page(0, 960, false));
    out.extend(b.data_page(0, 882, false));
    out.extend(c.data_page(0, 960, false));
    out.extend(a.data_page(1, 1920, true)); // #A# — A ends first
    out.extend(b.data_page(1, 1764, false));
    out.extend(c.data_page(1, 1920, false));
    out.extend(b.data_page(2, 2646, false));
    out.extend(c.data_page(2, 2880, true)); // #C#
    out.extend(b.data_page(3, 3528, true)); // #B# — last EOS of the group

    // --- Link 1: chained single stream D ---
    let mut d = StreamBuilder::new(0x0D0D_0D0D, 0xDD, 48_000);
    out.extend(d.bos_page());
    out.extend(d.header_pages());
    out.extend(d.data_page(0, 960, false));
    out.extend(d.data_page(1, 1920, true)); // #D#

    (out, [a.serial, b.serial, c.serial, d.serial])
}

#[test]
fn grouped_then_chained_registers_all_four_streams() {
    let (bytes, serials) = build_grouped_then_chained();
    let reader: Box<dyn ReadSeek> = Box::new(Cursor::new(bytes));
    let mut demux = oxideav_ogg::demux::open(reader, &oxideav_core::NullCodecResolver)
        .expect("demux grouped+chained ogg");

    // After open() the three grouped BOS pages (all before any data page)
    // are visible; link 1's D BOS is mid-file and not yet seen.
    assert_eq!(
        demux.streams().len(),
        3,
        "all three grouped streams register at open() (their BOS pages precede any data page)"
    );

    // Drain every packet, bucketed by stream_index.
    let mut by_index: HashMap<u32, Vec<oxideav_core::Packet>> = HashMap::new();
    while let Ok(p) = demux.next_packet() {
        by_index.entry(p.stream_index).or_default().push(p);
    }

    // All four streams (A,B,C grouped + D chained) are now registered.
    assert_eq!(
        demux.streams().len(),
        4,
        "fourth stream (chained link D) registers once its mid-file BOS is read"
    );

    // A=2, B=4, C=3, D=2 data packets — headers absorbed, not delivered.
    let mut counts: Vec<usize> = by_index.values().map(|v| v.len()).collect();
    counts.sort_unstable();
    assert_eq!(
        counts,
        vec![2, 2, 3, 4],
        "per-stream data-packet counts (A=2, D=2, C=3, B=4)"
    );

    // No cross-serial contamination: every packet of a stream carries the
    // same marker byte, and all four markers appear.
    let mut markers = HashSet::new();
    for packets in by_index.values() {
        let marker = packets[0].data[0];
        markers.insert(marker);
        for p in packets {
            assert_eq!(
                p.data[0], marker,
                "interleaved grouped pages mixed payloads across serials"
            );
        }
    }
    assert_eq!(
        markers,
        HashSet::from([0xAA, 0xBB, 0xCC, 0xDD]),
        "all four logical streams delivered data"
    );

    // Public indices are dense 0..4, one per delivered stream.
    let public_indices: HashSet<u32> = demux.streams().iter().map(|s| s.index).collect();
    assert_eq!(public_indices, HashSet::from([0, 1, 2, 3]));
    assert_eq!(
        by_index.keys().copied().collect::<HashSet<u32>>(),
        HashSet::from([0, 1, 2, 3]),
        "packets delivered for every public stream index"
    );
    let _ = serials;
}

#[test]
fn grouped_then_chained_link_partitioning() {
    // The diagnostic accessors live on the concrete demuxer.
    let (bytes, serials) = build_grouped_then_chained();
    let reader: Box<dyn ReadSeek> = Box::new(Cursor::new(bytes));
    let mut demux = oxideav_ogg::demux::open_concrete(reader, &oxideav_core::NullCodecResolver)
        .expect("demux grouped+chained ogg");

    // Drain so the chained link's BOS is discovered.
    while demux.next_packet().is_ok() {}

    // Two chained links total: the grouped {A,B,C} link and the {D} link.
    assert_eq!(
        demux.link_count(),
        2,
        "grouped {{A,B,C}} = link 0, chained {{D}} = link 1"
    );

    // Map each public stream index back to its serial + link index.
    let mut serial_to_link: HashMap<u32, u32> = HashMap::new();
    for i in 0..demux.streams().len() as u32 {
        let serial = demux.stream_serial(i).expect("serial for public index");
        let link = demux.stream_link_index(i).expect("link for public index");
        serial_to_link.insert(serial, link);
    }

    let [sa, sb, sc, sd] = serials;
    // A, B, C are all in the grouped link 0 — sharing a link index is the
    // whole point of grouping (RFC 3533 §4: BOS pages before any data page
    // belong to the same concurrently-multiplexed group).
    assert_eq!(serial_to_link.get(&sa), Some(&0), "A in grouped link 0");
    assert_eq!(serial_to_link.get(&sb), Some(&0), "B in grouped link 0");
    assert_eq!(serial_to_link.get(&sc), Some(&0), "C in grouped link 0");
    // D is the chained link 1.
    assert_eq!(serial_to_link.get(&sd), Some(&1), "D in chained link 1");
}

#[test]
fn grouped_then_chained_duration_sums_link0_max_plus_link1() {
    // Chained duration is the SUM of per-link durations; within the grouped
    // link 0 the duration is the MAX over its concurrently-multiplexed
    // streams. So total = max(A,B,C) + D.
    //
    //   A: granule 1920 @ 48000 Hz = 40.000 ms
    //   B: granule 3528 @ 44100 Hz = 80.000 ms  <- link-0 max
    //   C: granule 2880 @ 48000 Hz = 60.000 ms
    //   D: granule 1920 @ 48000 Hz = 40.000 ms
    //   total = 80 + 40 = 120 ms
    let (bytes, _serials) = build_grouped_then_chained();
    let reader: Box<dyn ReadSeek> = Box::new(Cursor::new(bytes));
    let mut demux = oxideav_ogg::demux::open_concrete(reader, &oxideav_core::NullCodecResolver)
        .expect("demux grouped+chained ogg");

    demux.build_seek_index().expect("build seek index");

    let dur = demux
        .duration_micros()
        .expect("chained+grouped duration available");
    let expected = 120_000i64; // 120 ms
    assert!(
        (dur - expected).abs() <= 2,
        "grouped+chained duration should be max(A,B,C) + D = 80ms + 40ms = 120ms (±2µs), got {dur}µs"
    );
}

/// Build the fully-general §4 shape where **both** chained links are groups:
/// link 0 groups {A,B}, link 1 groups {C,D}. The §4 sentence "It is possible
/// to consecutively chain groups of concurrently multiplexed bitstreams"
/// permits each link to be a group, not only the first.
///
/// Returns `(bytes, [serial_a, serial_b, serial_c, serial_d])`.
fn build_grouped_then_grouped() -> (Vec<u8>, [u32; 4]) {
    let mut a = StreamBuilder::new(0x1A1A_1A1A, 0xAA, 48_000);
    let mut b = StreamBuilder::new(0x1B1B_1B1B, 0xBB, 48_000);

    let mut out = Vec::new();

    // --- Link 0: grouped A, B ---
    out.extend(a.bos_page());
    out.extend(b.bos_page());
    out.extend(a.header_pages());
    out.extend(b.header_pages());
    out.extend(a.data_page(0, 960, false));
    out.extend(b.data_page(0, 960, false));
    out.extend(a.data_page(1, 1920, true)); // #A#
    out.extend(b.data_page(1, 1920, true)); // #B# — whole group ends

    // --- Link 1: grouped C, D ---
    let mut c = StreamBuilder::new(0x1C1C_1C1C, 0xCC, 48_000);
    let mut d = StreamBuilder::new(0x1D1D_1D1D, 0xDD, 48_000);
    out.extend(c.bos_page());
    out.extend(d.bos_page());
    out.extend(c.header_pages());
    out.extend(d.header_pages());
    out.extend(c.data_page(0, 960, false));
    out.extend(d.data_page(0, 960, false));
    out.extend(c.data_page(1, 1920, false));
    out.extend(c.data_page(2, 2880, true)); // #C#
    out.extend(d.data_page(1, 1920, true)); // #D#

    (out, [a.serial, b.serial, c.serial, d.serial])
}

#[test]
fn grouped_then_grouped_two_groups_two_links() {
    let (bytes, serials) = build_grouped_then_grouped();
    let reader: Box<dyn ReadSeek> = Box::new(Cursor::new(bytes));
    let mut demux = oxideav_ogg::demux::open_concrete(reader, &oxideav_core::NullCodecResolver)
        .expect("demux grouped+grouped ogg");

    // Link 0's two BOS pages precede any data page → both visible at open().
    assert_eq!(
        demux.streams().len(),
        2,
        "link 0's two grouped BOS pages register at open()"
    );

    let mut by_index: HashMap<u32, Vec<oxideav_core::Packet>> = HashMap::new();
    while let Ok(p) = demux.next_packet() {
        by_index.entry(p.stream_index).or_default().push(p);
    }

    // All four streams across both groups registered.
    assert_eq!(demux.streams().len(), 4);

    // A=2, B=2, C=3, D=2 data packets.
    let mut counts: Vec<usize> = by_index.values().map(|v| v.len()).collect();
    counts.sort_unstable();
    assert_eq!(counts, vec![2, 2, 2, 3]);

    // Two chained links, each grouping two streams.
    assert_eq!(demux.link_count(), 2);

    let mut serial_to_link: HashMap<u32, u32> = HashMap::new();
    for i in 0..demux.streams().len() as u32 {
        let serial = demux.stream_serial(i).unwrap();
        let link = demux.stream_link_index(i).unwrap();
        serial_to_link.insert(serial, link);
    }
    let [sa, sb, sc, sd] = serials;
    // A, B share grouped link 0; C, D share grouped link 1. The key §4
    // invariant: the *second* link's grouping (C,D both before any of its
    // data pages) does NOT split into two extra links — they share link 1.
    assert_eq!(serial_to_link.get(&sa), Some(&0), "A in group/link 0");
    assert_eq!(serial_to_link.get(&sb), Some(&0), "B in group/link 0");
    assert_eq!(serial_to_link.get(&sc), Some(&1), "C in group/link 1");
    assert_eq!(serial_to_link.get(&sd), Some(&1), "D in group/link 1");
}

#[test]
fn seek_into_grouped_stream_lands_on_floor_page() {
    // A seek against one stream of a grouped link must walk only that
    // stream's serial when bisecting — the interleaved pages of the other
    // grouped streams (different serials) must not perturb the landing.
    // Target stream B (serial 0x0B0B_0B0B, 44_100 Hz): its data-page
    // granules are 882, 1764, 2646, 3528. A pts that maps to granule 2000
    // should land on the page with granule 1764 (the greatest at-or-below).
    let (bytes, serials) = build_grouped_then_chained();
    let reader: Box<dyn ReadSeek> = Box::new(Cursor::new(bytes));
    let mut demux = oxideav_ogg::demux::open_concrete(reader, &oxideav_core::NullCodecResolver)
        .expect("demux grouped+chained ogg");
    demux.build_seek_index().expect("build seek index");

    // Find B's public stream index from its serial.
    let sb = serials[1];
    let b_index = (0..demux.streams().len() as u32)
        .find(|&i| demux.stream_serial(i) == Some(sb))
        .expect("stream B present");

    // B's time-base is 1/44100 (granule == sample count for Vorbis), so a
    // target pts in those units of 2000 floors to granule 1764.
    let landed = demux
        .seek_to(b_index, 2000)
        .expect("seek into grouped stream B");
    assert_eq!(
        landed, 1764,
        "seek floors to B's granule 1764 (the greatest ≤ 2000), unperturbed by interleaved A/C pages"
    );
}
