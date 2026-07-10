#![no_main]

//! Hostile-page-graph SEEK harness.
//!
//! `granule_walk` bisects over raw fuzz bytes, which rarely produce a
//! parseable multi-page file; this target CONSTRUCTS a valid-CRC page
//! graph whose *seek-relevant* structure is hostile — non-monotonic
//! granule positions, `-1` sentinels, extreme positive/negative
//! granules, duplicate serials, sequence jumps, an optional truncated
//! final page — and optionally attaches a Skeleton whose fisbone
//! (granuleshift up to 255, zero/negative granule rates, fuzz preroll
//! and header counts) and 4.0 `index\0` packet (keypoint offsets and
//! timestamps pointing anywhere, zero/negative timestamp
//! denominators) actively lie about the file. Every seek entry point
//! is then stormed:
//!
//! * [`oxideav_core::Demuxer::seek_to`] — Skeleton-index fast path,
//!   segment-length check, per-keypoint landing validation, page-index
//!   floor, bisection, and the byte-scanner fallback;
//! * [`oxideav_ogg::demux::OggDemuxer::seek_to_with_preroll`] — the
//!   backward page walk driven by a hostile fisbone preroll;
//! * [`oxideav_ogg::demux::OggDemuxer::seek_to_keyframe`] — the
//!   granuleshift unpack + re-seek path (Theora-shaped stream);
//! * [`oxideav_ogg::demux::OggDemuxer::build_seek_index`] before or
//!   between storms (dense-index fast return vs bisection tightening).
//!
//! Oracles: panic-freedom on every call; a delivered packet's
//! `stream_index` stays in range after every landing;
//! `input_position` stays callable; the Skeleton diagnostic counters
//! (`skeleton_index_seek_count` / `skeleton_index_invalid_count` /
//! `preroll_seek_count`) never panic. Landed granules are NOT
//! compared to ground truth — with a lying index and non-monotonic
//! granules there is none; the demuxer's contract on such files is
//! "return something or an error, never crash, never emit an
//! out-of-range packet".

use std::io::Cursor;

use libfuzzer_sys::fuzz_target;
use oxideav_core::{Demuxer as _, NullCodecResolver, ReadSeek};
use oxideav_ogg::demux;
use oxideav_ogg::page::{flags, lace, Page};
use oxideav_ogg::skeleton::{FisBone, FisHead, KeyPoint, Rational, SkelIndex, Version};

use oxideav_ogg_fuzz::{
    theora_comment_packet, theora_id_packet, theora_setup_packet, vorbis_comment_packet,
    vorbis_id_packet, vorbis_setup_packet,
};

const PRIMARY_SERIAL: u32 = 0xAB01_CD02;
const SECOND_SERIAL: u32 = 0xEF03_1234;
const SKELETON_SERIAL: u32 = 0x0102_0304;

const MAX_DATA_PAGES: usize = 14;
const MAX_SEEK_OPS: usize = 10;
const POST_SEEK_DRAIN: usize = 4;

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
    fn i64x(&mut self) -> i64 {
        // Wide-dynamic-range signed value from 3 bytes: covers small
        // targets, huge magnitudes, and negatives.
        let a = self.u8() as i8 as i64;
        let b = self.u8() as i64;
        let c = self.u8() as i64;
        a.wrapping_mul(1 << (c & 0x3F)).wrapping_add(b)
    }
}

fn push_page(buf: &mut Vec<u8>, page: Page) {
    if let Ok(bytes) = page.try_to_bytes() {
        buf.extend_from_slice(&bytes);
    }
}

fn skeleton_page(buf: &mut Vec<u8>, seq: &mut u32, page_flags: u8, packet: Vec<u8>) {
    push_page(
        buf,
        Page {
            flags: page_flags,
            granule_position: 0,
            serial: SKELETON_SERIAL,
            seq_no: *seq,
            lacing: lace(packet.len()),
            data: packet,
        },
    );
    *seq += 1;
}

fuzz_target!(|data: &[u8]| {
    if data.len() < 6 {
        return;
    }
    let mut fz = Fz::new(data);
    let ctrl = fz.u8();
    let with_skeleton = ctrl & 0x01 != 0;
    let with_index = ctrl & 0x02 != 0;
    let with_second = ctrl & 0x04 != 0;
    let with_dup_bos = ctrl & 0x08 != 0;
    let truncate_tail = ctrl & 0x10 != 0;
    let index_first = ctrl & 0x20 != 0;
    let primary_theora = ctrl & 0x40 != 0;

    let mut buf: Vec<u8> = Vec::with_capacity(4096);
    let mut skel_seq: u32 = 0;

    // ---- Skeleton fishead BOS ----
    if with_skeleton {
        let mut head = FisHead::new(Version::V4_0);
        // Hostile segment-length: 0 (opt-out), a tiny value (page
        // boundary check), or an impossible huge one.
        head.segment_length = match fz.u8() & 0x03 {
            0 => Some(0),
            1 => Some(u64::from(fz.u8())),
            2 => Some(u64::MAX / 3),
            _ => None,
        };
        skeleton_page(&mut buf, &mut skel_seq, flags::FIRST_PAGE, head.to_bytes());
    }

    // ---- Content BOS pages ----
    let (primary_id, primary_rest) = if primary_theora {
        (
            theora_id_packet(),
            vec![theora_comment_packet(), theora_setup_packet()],
        )
    } else {
        (
            vorbis_id_packet(),
            vec![vorbis_comment_packet(), vorbis_setup_packet()],
        )
    };
    push_page(
        &mut buf,
        Page {
            flags: flags::FIRST_PAGE,
            granule_position: 0,
            serial: PRIMARY_SERIAL,
            seq_no: 0,
            lacing: lace(primary_id.len()),
            data: primary_id.clone(),
        },
    );
    if with_second {
        let id = vorbis_id_packet();
        push_page(
            &mut buf,
            Page {
                flags: flags::FIRST_PAGE,
                granule_position: 0,
                serial: SECOND_SERIAL,
                seq_no: 0,
                lacing: lace(id.len()),
                data: id,
            },
        );
    }

    // ---- Skeleton secondary headers: fisbone (+ hostile index) + EOS ----
    if with_skeleton {
        let mut bone = FisBone::new(
            PRIMARY_SERIAL,
            Rational::new(i64::from(fz.u8() as i8), i64::from(fz.u8() as i8)),
        );
        bone.granuleshift = fz.u8(); // 0..=255, hostile past 63
        bone.preroll = u32::from(fz.u8() & 0x07);
        bone.num_headers = u32::from(fz.u8() & 0x07);
        skeleton_page(&mut buf, &mut skel_seq, 0, bone.to_bytes());

        if with_index {
            let mut idx = SkelIndex::new(PRIMARY_SERIAL, i64::from(fz.u8() as i8));
            idx.first_sample_time = i64::from(fz.u8() as i8);
            idx.last_sample_time = i64::from(fz.u8() as i8) * 48_000;
            let n_kp = (fz.u8() & 0x07) as usize;
            let mut off: u64 = 0;
            let mut ts: i64 = 0;
            for _ in 0..n_kp {
                // Offsets stride across (and past) the eventual file;
                // timestamps may run backwards.
                off = off.wrapping_add(u64::from(fz.u8()) * 53);
                ts = ts.wrapping_add(i64::from(fz.u8() as i8) * 1000);
                idx.keypoints.push(KeyPoint {
                    offset: off,
                    timestamp: ts,
                });
            }
            skeleton_page(&mut buf, &mut skel_seq, 0, idx.to_bytes());
        }
        skeleton_page(&mut buf, &mut skel_seq, flags::LAST_PAGE, Vec::new());
    }

    // ---- Remaining codec header pages ----
    let mut primary_seq: u32 = 1;
    for hp in &primary_rest {
        push_page(
            &mut buf,
            Page {
                flags: 0,
                granule_position: 0,
                serial: PRIMARY_SERIAL,
                seq_no: primary_seq,
                lacing: lace(hp.len()),
                data: hp.clone(),
            },
        );
        primary_seq += 1;
    }
    let mut second_seq: u32 = 1;
    if with_second {
        for hp in [vorbis_comment_packet(), vorbis_setup_packet()] {
            push_page(
                &mut buf,
                Page {
                    flags: 0,
                    granule_position: 0,
                    serial: SECOND_SERIAL,
                    seq_no: second_seq,
                    lacing: lace(hp.len()),
                    data: hp,
                },
            );
            second_seq += 1;
        }
    }

    // ---- Hostile data pages ----
    let n_pages = 2 + (fz.u8() as usize % MAX_DATA_PAGES);
    let mut primary_granule: i64 = 0;
    let mut second_granule: i64 = 0;
    for d in 0..n_pages {
        let on_second = with_second && fz.u8() & 0x01 != 0;
        let fill = fz.u8();
        let body = usize::from(fill) % 180 + 1;
        let gsel = fz.u8();
        let granule = if gsel == 0xFF {
            -1
        } else {
            // Non-monotonic by construction: signed delta scaled by a
            // fuzz-chosen power of two (up to 2^57 → near-overflow
            // granules that stress the shift/offset unpack paths).
            let g = if on_second {
                &mut second_granule
            } else {
                &mut primary_granule
            };
            *g = g.wrapping_add(i64::from(gsel as i8) << (fz.u8() & 0x39));
            *g
        };
        let (serial, seq) = if on_second {
            second_seq = second_seq.wrapping_add(u32::from(fz.u8() & 0x03));
            (SECOND_SERIAL, second_seq)
        } else {
            primary_seq = primary_seq.wrapping_add(u32::from(fz.u8() & 0x03));
            (PRIMARY_SERIAL, primary_seq)
        };
        let mut page_flags = 0u8;
        if d == n_pages - 1 && fz.u8() & 0x01 != 0 {
            page_flags |= flags::LAST_PAGE;
        }
        // Mid-graph duplicate-BOS injection (unique-serial violation
        // + seek across the restart boundary).
        if with_dup_bos && d == n_pages / 2 {
            push_page(
                &mut buf,
                Page {
                    flags: flags::FIRST_PAGE,
                    granule_position: 0,
                    serial: PRIMARY_SERIAL,
                    seq_no: 0,
                    lacing: lace(primary_id.len()),
                    data: primary_id.clone(),
                },
            );
        }
        push_page(
            &mut buf,
            Page {
                flags: page_flags,
                granule_position: granule,
                serial,
                seq_no: seq,
                lacing: lace(body),
                data: vec![fill; body],
            },
        );
    }

    if truncate_tail && buf.len() > 40 {
        let cut = 1 + usize::from(fz.u8()) % 39;
        buf.truncate(buf.len() - cut);
    }

    // ---- Open + seek storm ----
    let reader: Box<dyn ReadSeek> = Box::new(Cursor::new(buf));
    let mut dmx = match demux::open_concrete(reader, &NullCodecResolver) {
        Ok(d) => d,
        Err(_) => return,
    };

    if index_first {
        let _ = dmx.build_seek_index();
    }

    let n_streams = dmx.streams().len();
    if n_streams == 0 {
        return;
    }

    for op in 0..MAX_SEEK_OPS {
        if fz.done() {
            break;
        }
        let sel = fz.u8();
        let stream = (sel as usize >> 4) as u32 % n_streams as u32;
        let target = fz.i64x();
        match sel & 0x03 {
            0 | 3 => {
                let _ = dmx.seek_to(stream, target);
            }
            1 => {
                let _ = dmx.seek_to_with_preroll(stream, target);
            }
            _ => {
                let _ = dmx.seek_to_keyframe(stream, target);
            }
        }
        let _ = dmx.input_position();
        // Every landing must resume cleanly: a short drain may error
        // (hostile bytes) but never panics and never emits an
        // out-of-range stream index.
        for _ in 0..POST_SEEK_DRAIN {
            match dmx.next_packet() {
                Ok(pkt) => {
                    assert!(
                        (pkt.stream_index as usize) < dmx.streams().len(),
                        "post-seek packet stream index out of range"
                    );
                }
                Err(_) => break,
            }
        }
        // Mid-storm dense-index build: later seeks take the
        // dense-index fast return instead of bisection.
        if op == MAX_SEEK_OPS / 2 && !index_first {
            let _ = dmx.build_seek_index();
        }
    }

    // Diagnostics must be callable after the storm.
    let _ = dmx.skeleton_index_seek_count();
    let _ = dmx.skeleton_index_invalid_count();
    let _ = dmx.preroll_seek_count();
    let _ = dmx.hole_count();
    let _ = dmx.framing_error_count();
    let _ = dmx.resync_count();
    let _ = dmx.duplicate_serial_count();
    let _ = dmx.duration_micros();
});
