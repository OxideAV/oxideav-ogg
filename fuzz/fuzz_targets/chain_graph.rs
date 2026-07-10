#![no_main]

//! Chained + grouped stream-graph harness.
//!
//! `continued_edge` exercises single-stream packet reassembly; this
//! target constructs whole PHYSICAL-stream graphs (RFC 3533 §4): up to
//! three chained links, each grouping one or two logical bitstreams,
//! with fuzz-chosen serials drawn from a four-entry pool so grouped
//! and chained unique-serial violations occur constantly, plus an
//! optional Skeleton control section (fishead BOS first, fisbones +
//! optional 4.0 index in the secondary-header section, empty-packet
//! EOS page) so the Skeleton routing path sees hostile graphs too.
//!
//! Per-page hostility: fuzz-driven header flags (a data page carrying
//! a spurious BOS bit is a mid-link duplicate-BOS restart; missing EOS
//! bits leave links unterminated), sequence-number jumps, hostile
//! granules (non-monotonic deltas and the `-1` sentinel), and an
//! optional single-byte global corruption plus tail truncation to
//! drive CRC-resync across link boundaries.
//!
//! Surfaces exercised: `open_concrete` BOS-section walk on grouped
//! streams, `next_packet` across link boundaries, chained-link
//! discovery both incremental and via `build_seek_index`'s full-file
//! scan, `restart_serial_on_duplicate_bos`, the link/serial/track
//! accessors, and `seek_to` on whatever codecs the graph declared.
//!
//! HARD invariants:
//! * a delivered packet's `stream_index` is always in range;
//! * `stream_link_index` / `stream_serial` / `stream_granuleshift`
//!   are `Some` for every in-range index and `None` past the end;
//! * the Skeleton "Track order" mapping is a bijection:
//!   `track_order_index(track_order_serial(t)) == t` for every
//!   `t < track_order_len()`.

use std::io::Cursor;

use libfuzzer_sys::fuzz_target;
use oxideav_core::{Demuxer as _, Error, NullCodecResolver, ReadSeek};
use oxideav_ogg::demux;
use oxideav_ogg::page::{flags, lace, Page};
use oxideav_ogg::skeleton::{FisBone, FisHead, Rational, SkelIndex, Version};

use oxideav_ogg_fuzz::{
    opus_head_packet, opus_tags_packet, theora_comment_packet, theora_id_packet,
    theora_setup_packet, vorbis_comment_packet, vorbis_id_packet, vorbis_setup_packet,
};

/// Content-serial pool. Four entries + fuzz picks make grouped and
/// chained serial collisions common instead of vanishingly rare.
const SERIALS: [u32; 4] = [0xA1A1_0001, 0xB2B2_0002, 0xC3C3_0003, 0xD4D4_0004];
const SKELETON_SERIAL: u32 = 0x5E5E_5E5E;

const MAX_LINKS: usize = 3;
const MAX_DATA_PAGES_PER_LINK: usize = 8;
const MAX_PACKETS: usize = 4096;
const MAX_SEEKS: usize = 6;

/// Byte-at-a-time descriptor reader.
struct Fz<'a> {
    data: &'a [u8],
    pos: usize,
}

impl<'a> Fz<'a> {
    fn new(data: &'a [u8]) -> Self {
        Self { data, pos: 0 }
    }
    fn u8(&mut self) -> u8 {
        let b = self.data.get(self.pos).copied().unwrap_or(0);
        self.pos += 1;
        b
    }
    fn done(&self) -> bool {
        self.pos >= self.data.len()
    }
}

/// (id, remaining header packets) for a fuzz-chosen codec shape.
fn codec_packets(sel: u8) -> (Vec<u8>, Vec<Vec<u8>>) {
    match sel % 5 {
        0 => (
            vorbis_id_packet(),
            vec![vorbis_comment_packet(), vorbis_setup_packet()],
        ),
        1 => (opus_head_packet(312), vec![opus_tags_packet()]),
        2 => (
            theora_id_packet(),
            vec![theora_comment_packet(), theora_setup_packet()],
        ),
        // Garbage identification packet — codec "unknown", zero
        // headers, every packet delivered as content.
        3 => (vec![sel; 12], Vec::new()),
        // Truncated Vorbis id (signature matches, body too short for
        // parse_vorbis_id) — drives the register_stream error path.
        _ => {
            let mut p = vorbis_id_packet();
            p.truncate(10);
            (p, Vec::new())
        }
    }
}

fn push_page(buf: &mut Vec<u8>, page: Page) {
    // All construction sites satisfy the lacing invariants; use the
    // fallible path anyway so a drifted helper cannot panic the harness.
    if let Ok(bytes) = page.try_to_bytes() {
        buf.extend_from_slice(&bytes);
    }
}

fuzz_target!(|data: &[u8]| {
    if data.len() < 4 {
        return;
    }
    let mut fz = Fz::new(data);
    let ctrl = fz.u8();
    let with_skeleton = ctrl & 0x01 != 0;
    let n_links = 1 + (ctrl as usize >> 1) % MAX_LINKS;
    let corrupt_byte = ctrl & 0x08 != 0;
    let truncate_tail = ctrl & 0x10 != 0;

    let mut buf: Vec<u8> = Vec::with_capacity(4096);
    let mut skel_seq: u32 = 0;

    // ---- Skeleton fishead BOS (very first BOS when present) ----
    if with_skeleton {
        let sel = fz.u8();
        let mut head = FisHead::new(if sel & 0x01 != 0 {
            Version::V3_0
        } else {
            Version::V4_0
        });
        if sel & 0x02 != 0 {
            // Hostile segment-length declarations: too long, or a
            // small value unlikely to land on a page boundary.
            head.segment_length = Some(if sel & 0x04 != 0 {
                u64::MAX / 2
            } else {
                u64::from(fz.u8())
            });
        }
        head.basetime = Rational::new(i64::from(fz.u8() as i8), i64::from(fz.u8() as i8));
        push_page(
            &mut buf,
            Page {
                flags: flags::FIRST_PAGE,
                granule_position: 0,
                serial: SKELETON_SERIAL,
                seq_no: skel_seq,
                lacing: lace(head.to_bytes().len()),
                data: head.to_bytes(),
            },
        );
        skel_seq += 1;
    }

    // ---- Links ----
    let mut first_link_serials: Vec<u32> = Vec::new();
    for link in 0..n_links {
        let link_ctrl = fz.u8();
        let n_streams = 1 + (link_ctrl as usize & 1);
        let mut link_streams: Vec<(u32, Vec<Vec<u8>>)> = Vec::new();

        // BOS pages (grouped: all BOS pages of the link first).
        for s in 0..n_streams {
            let pick = fz.u8();
            // Serial from the pool; one fuzz bit swaps in the Skeleton
            // serial itself (a content BOS reusing the metadata
            // stream's serial — routed to the Skeleton path).
            let serial = if with_skeleton && pick & 0x80 != 0 {
                SKELETON_SERIAL
            } else {
                SERIALS[(pick as usize + s) % SERIALS.len()]
            };
            let (id, rest) = codec_packets(pick >> 3);
            push_page(
                &mut buf,
                Page {
                    flags: flags::FIRST_PAGE,
                    granule_position: 0,
                    serial,
                    seq_no: 0,
                    lacing: lace(id.len()),
                    data: id,
                },
            );
            link_streams.push((serial, rest));
            if link == 0 {
                first_link_serials.push(serial);
            }
        }

        // Skeleton secondary headers ride inside the first link's
        // header section: one fisbone per first-link stream, an
        // optional hostile 4.0 index, then the empty EOS packet.
        if with_skeleton && link == 0 {
            for &(serial, _) in &link_streams {
                let sel = fz.u8();
                let mut bone = FisBone::new(
                    serial,
                    Rational::new(i64::from(sel as i8), i64::from(fz.u8() as i8)),
                );
                bone.granuleshift = fz.u8(); // hostile: may exceed 63
                bone.preroll = u32::from(fz.u8() & 0x07);
                bone.num_headers = u32::from(fz.u8() & 0x07);
                let bytes = bone.to_bytes();
                push_page(
                    &mut buf,
                    Page {
                        flags: 0,
                        granule_position: 0,
                        serial: SKELETON_SERIAL,
                        seq_no: skel_seq,
                        lacing: lace(bytes.len()),
                        data: bytes,
                    },
                );
                skel_seq += 1;
            }
            if fz.u8() & 0x01 != 0 {
                // Hostile keyframe index: offsets/timestamps point
                // anywhere; the seek fast-path must reject and fall
                // back, never panic.
                let mut idx = SkelIndex::new(
                    link_streams[0].0,
                    i64::from(fz.u8() as i8), // may be 0 or negative
                );
                let n_kp = (fz.u8() & 0x07) as usize;
                let mut off: u64 = 0;
                let mut ts: i64 = 0;
                for _ in 0..n_kp {
                    off = off.wrapping_add(u64::from(fz.u8()) * 37);
                    ts = ts.wrapping_add(i64::from(fz.u8() as i8) * 100);
                    idx.keypoints.push(oxideav_ogg::skeleton::KeyPoint {
                        offset: off,
                        timestamp: ts,
                    });
                }
                let bytes = idx.to_bytes();
                push_page(
                    &mut buf,
                    Page {
                        flags: 0,
                        granule_position: 0,
                        serial: SKELETON_SERIAL,
                        seq_no: skel_seq,
                        lacing: lace(bytes.len()),
                        data: bytes,
                    },
                );
                skel_seq += 1;
            }
            // Skeleton EOS: empty packet on its own page.
            push_page(
                &mut buf,
                Page {
                    flags: flags::LAST_PAGE,
                    granule_position: 0,
                    serial: SKELETON_SERIAL,
                    seq_no: skel_seq,
                    lacing: lace(0),
                    data: Vec::new(),
                },
            );
            skel_seq += 1;
        }

        // Remaining codec header pages, one packet per page.
        let mut seqs: Vec<u32> = vec![1; link_streams.len()];
        for (s, (serial, rest)) in link_streams.iter().enumerate() {
            for hp in rest {
                push_page(
                    &mut buf,
                    Page {
                        flags: 0,
                        granule_position: 0,
                        serial: *serial,
                        seq_no: seqs[s],
                        lacing: lace(hp.len()),
                        data: hp.clone(),
                    },
                );
                seqs[s] += 1;
            }
        }

        // Data pages with fuzz-driven flags / lacing / seq / granule.
        let mut granules: Vec<i64> = vec![0; link_streams.len()];
        let n_data = 1 + (fz.u8() as usize % MAX_DATA_PAGES_PER_LINK);
        for d in 0..n_data {
            if fz.done() {
                break;
            }
            let s = fz.u8() as usize % link_streams.len();
            let flag_sel = fz.u8();
            // Low three RFC 3533 §6 flag bits fuzz-driven: a spurious
            // FIRST bit mid-link is a duplicate-BOS restart; CONTINUED
            // with no open packet is a framing error; LAST may close
            // the stream early (or never arrive).
            let mut page_flags = flag_sel & 0x07;
            if d == n_data - 1 && flag_sel & 0x08 != 0 {
                page_flags |= flags::LAST_PAGE;
            }
            let fill = fz.u8();
            let (lacing, payload) = match flag_sel >> 5 {
                0 => (vec![fill.min(200)], vec![fill; usize::from(fill.min(200))]),
                1 => (vec![255u8], vec![fill; 255]),
                2 => (vec![255u8, 40], vec![fill; 295]),
                3 => (vec![10u8, 10], vec![fill; 20]),
                4 => (vec![255u8, 255, 0], vec![fill; 510]),
                _ => (vec![0u8], Vec::new()),
            };
            let seq_delta = fz.u8() & 0x03; // 0 = duplicate seq (hole)
            seqs[s] = seqs[s].wrapping_add(u32::from(seq_delta));
            let gb = fz.u8();
            granules[s] = if gb == 0xFF {
                -1
            } else {
                // Signed, scaled delta — non-monotonic on purpose.
                granules[s].wrapping_add(i64::from(gb as i8) * 4096)
            };
            push_page(
                &mut buf,
                Page {
                    flags: page_flags,
                    granule_position: granules[s],
                    serial: link_streams[s].0,
                    seq_no: seqs[s],
                    lacing,
                    data: payload,
                },
            );
        }
    }

    // ---- Global hostility: single-byte corruption + tail truncation ----
    if corrupt_byte && !buf.is_empty() {
        let off = (usize::from(fz.u8()) * 251 + usize::from(fz.u8())) % buf.len();
        buf[off] ^= fz.u8() | 1;
    }
    if truncate_tail && buf.len() > 8 {
        let cut = 1 + usize::from(fz.u8()) % (buf.len() / 2);
        buf.truncate(buf.len() - cut);
    }

    // ---- Drive the demuxer ----
    let reader: Box<dyn ReadSeek> = Box::new(Cursor::new(buf));
    let mut dmx = match demux::open_concrete(reader, &NullCodecResolver) {
        Ok(d) => d,
        Err(_) => return,
    };

    for _ in 0..MAX_PACKETS {
        match dmx.next_packet() {
            Ok(pkt) => {
                assert!(
                    (pkt.stream_index as usize) < dmx.streams().len(),
                    "packet stream index {} out of range {}",
                    pkt.stream_index,
                    dmx.streams().len()
                );
            }
            Err(Error::Eof) => break,
            Err(_) => break,
        }
    }

    check_accessors(&mut dmx);

    // Full-file scan re-discovers the same graph; accessors must stay
    // coherent (values may legitimately change — the scan sees pages
    // the incremental drain skipped — but never panic or break the
    // Some/None range contract or the track-order bijection).
    let _ = dmx.build_seek_index();
    let _ = dmx.seek_index_len();
    check_accessors(&mut dmx);

    // Seek storm on every stream (errors fine, panics not).
    let n = dmx.streams().len();
    if n > 0 {
        for i in 0..MAX_SEEKS {
            let target = i64::from(fz.u8()) * 8191 - 4096;
            let _ = dmx.seek_to((i % n) as u32, target);
        }
        // Post-seek drain must stay in range.
        for _ in 0..64 {
            match dmx.next_packet() {
                Ok(pkt) => {
                    assert!((pkt.stream_index as usize) < dmx.streams().len());
                }
                Err(_) => break,
            }
        }
    }
    let _ = dmx.duration_micros();
});

/// Range + bijection contracts on the link/serial/track accessors.
fn check_accessors(dmx: &mut demux::OggDemuxer) {
    let n = dmx.streams().len() as u32;
    for i in 0..n {
        assert!(
            dmx.stream_link_index(i).is_some(),
            "stream {i} must have a link index"
        );
        assert!(
            dmx.stream_serial(i).is_some(),
            "stream {i} must have a serial"
        );
        assert!(
            dmx.stream_granuleshift(i).is_some(),
            "stream {i} must have a granuleshift"
        );
    }
    assert!(dmx.stream_link_index(n).is_none());
    assert!(dmx.stream_serial(n).is_none());
    assert!(dmx.stream_granuleshift(n).is_none());
    if n > 0 {
        assert!(dmx.link_count() >= 1, "registered streams imply a link");
    }

    // Track-order bijection (Skeleton message-headers wiki §"Track
    // order"): every track index maps to a serial and back.
    let tracks = dmx.track_order_len();
    for t in 0..tracks {
        let serial = dmx
            .track_order_serial(t)
            .unwrap_or_else(|| panic!("track {t} of {tracks} must resolve to a serial"));
        assert_eq!(
            dmx.track_order_index(serial),
            Some(t),
            "track order must round-trip for track {t}"
        );
    }
    assert!(dmx.track_order_serial(tracks).is_none());

    let _ = dmx.hole_count();
    let _ = dmx.framing_error_count();
    let _ = dmx.resync_count();
    let _ = dmx.duplicate_serial_count();
    let _ = dmx.skeleton();
    let _ = dmx.metadata();
}
