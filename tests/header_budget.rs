//! `open()`-time header-collection budget.
//!
//! A `fishead\0` BOS obliges the demuxer to keep reading until the
//! Skeleton EOS page closes the control section (so `skeleton()` is
//! fully populated right after `open`). A hostile file that never
//! sends that EOS page previously made `open()` walk — and buffer the
//! packets of — the ENTIRE file before returning: memory proportional
//! to the input size before the caller had read a single packet. The
//! wait is now bounded by a page budget; past it, collection stops
//! best-effort exactly like the EOF path, and `next_packet` resumes
//! from wherever the cursor stopped.

use std::io::Cursor;

use oxideav_core::{Demuxer, Error, ReadSeek};
use oxideav_ogg::demux;
use oxideav_ogg::page::{flags, lace, Page};
use oxideav_ogg::skeleton::{FisHead, Version};

const SKELETON_SERIAL: u32 = 0x5C5C_5C5C;
const VORBIS_SERIAL: u32 = 0x0A0A_0A0A;

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
    p
}

fn page(flags_byte: u8, granule: i64, serial: u32, seq: u32, packet: &[u8]) -> Vec<u8> {
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

#[test]
fn missing_skeleton_eos_does_not_buffer_the_whole_file_at_open() {
    // Header section: Skeleton fishead BOS (whose EOS never arrives) +
    // a complete Vorbis header set.
    let mut buf = Vec::new();
    buf.extend(page(
        flags::FIRST_PAGE,
        0,
        SKELETON_SERIAL,
        0,
        &FisHead::new(Version::V4_0).to_bytes(),
    ));
    buf.extend(page(
        flags::FIRST_PAGE,
        0,
        VORBIS_SERIAL,
        0,
        &vorbis_id_packet(),
    ));
    let comment = {
        let mut p = vec![0x03];
        p.extend_from_slice(b"vorbis");
        p.extend_from_slice(&[0, 0, 0, 0, 0, 0, 0, 0, 1]);
        p
    };
    let setup = {
        let mut p = vec![0x05];
        p.extend_from_slice(b"vorbis");
        p.extend_from_slice(&[0u8; 16]);
        p
    };
    buf.extend(page(0, 0, VORBIS_SERIAL, 1, &comment));
    buf.extend(page(0, 0, VORBIS_SERIAL, 2, &setup));

    // Way more data pages than the header budget. The Skeleton EOS the
    // demuxer is waiting for never arrives.
    const N_DATA: usize = 9_500;
    for i in 0..N_DATA {
        let flags_byte = if i + 1 == N_DATA { flags::LAST_PAGE } else { 0 };
        buf.extend(page(
            flags_byte,
            (i as i64 + 1) * 128,
            VORBIS_SERIAL,
            3 + i as u32,
            &[(i & 0x7F) as u8; 24],
        ));
    }
    let file_len = buf.len() as u64;

    let reader: Box<dyn ReadSeek> = Box::new(Cursor::new(buf));
    let mut dmx = demux::open_concrete(reader, &oxideav_core::NullCodecResolver)
        .expect("hostile no-EOS file still opens best-effort");

    // The budget must have stopped the open()-time walk well short of
    // EOF — this is the observable form of "open() no longer buffers
    // the whole file".
    let pos_after_open = dmx.input_position().unwrap();
    assert!(
        pos_after_open < file_len,
        "open() walked to EOF ({pos_after_open} of {file_len} bytes) — header budget not applied"
    );

    // Skeleton state is still what the header section provided.
    assert!(dmx.skeleton().is_some());
    assert_eq!(dmx.streams().len(), 1);

    // Every data packet is still delivered — the budget defers reading,
    // it must not lose anything.
    let mut count = 0usize;
    loop {
        match dmx.next_packet() {
            Ok(pkt) => {
                assert_eq!(pkt.stream_index, 0);
                assert_eq!(pkt.data.len(), 24);
                count += 1;
            }
            Err(Error::Eof) => break,
            Err(e) => panic!("unexpected demux error: {e:?}"),
        }
    }
    assert_eq!(count, N_DATA, "no packet may be lost to the budget");
}
