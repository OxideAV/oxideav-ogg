#![no_main]

//! Mux → demux round-trip invariant harness.
//!
//! The other targets hammer the demuxer with hostile bytes; this one
//! checks the crate against ITSELF: any packet sequence the muxer
//! accepts must come back from the demuxer byte-identical, in order,
//! on the right stream — and the demuxer's damage counters must all
//! read zero on our own output (a non-zero hole / framing-error /
//! resync / duplicate-serial count on muxer-produced bytes means one
//! side of the crate disagrees with the other about RFC 3533).
//!
//! Fuzz-driven shape:
//!
//! * 1–2 streams in the first link, per-stream codec choice
//!   (Vorbis 3-header / Opus 2-header mappings — the two extradata
//!   reconstruction paths `extract_codec_headers` implements);
//! * packet sizes from a class table straddling every lacing edge,
//!   including > 255×255 page-spanning packets (bounded to two per
//!   iteration);
//! * fuzz-chosen `unit_boundary` flags and pts deltas (non-negative,
//!   so granules stay monotone per the audio-mapping convention);
//! * optional soft page-size target (`set_page_target_bytes`);
//! * optional chained second link via `begin_new_link` (RFC 3533 §4
//!   sequential multiplex), whose packets must surface as a separate
//!   public stream with its own link index.
//!
//! HARD invariants checked after the drain:
//! * per-public-stream payload sequences equal what was written;
//! * `streams().len()` equals the total stream count across links;
//! * `hole_count == framing_error_count == resync_count ==
//!   duplicate_serial_count == 0`;
//! * `link_count()` equals the number of links written.

use std::io::Cursor;

use libfuzzer_sys::fuzz_target;
use oxideav_core::{
    CodecId, CodecParameters, Demuxer as _, Error, Muxer as _, NullCodecResolver, Packet, ReadSeek,
    StreamInfo, TimeBase, WriteSeek,
};
use oxideav_ogg::{demux, mux};

use oxideav_ogg_fuzz::{
    opus_head_packet, theora_comment_packet, theora_setup_packet, theora_valid_id_packet,
    vorbis_comment_packet, vorbis_id_packet, vorbis_setup_packet,
};

const SIZE_CLASSES: [usize; 8] = [0, 1, 254, 255, 256, 510, 4096, 66_000];
const MAX_PACKETS: usize = 48;
const MAX_HUGE: usize = 2;

/// Shared in-memory sink (the muxer takes the sink by value; the
/// harness needs the bytes back afterwards).
#[derive(Clone, Default)]
struct SharedBuf(std::sync::Arc<std::sync::Mutex<Cursor<Vec<u8>>>>);

impl std::io::Write for SharedBuf {
    fn write(&mut self, b: &[u8]) -> std::io::Result<usize> {
        self.0.lock().unwrap().write(b)
    }
    fn flush(&mut self) -> std::io::Result<()> {
        self.0.lock().unwrap().flush()
    }
}

impl std::io::Seek for SharedBuf {
    fn seek(&mut self, p: std::io::SeekFrom) -> std::io::Result<u64> {
        self.0.lock().unwrap().seek(p)
    }
}

#[derive(Clone, Copy, PartialEq)]
enum Codec {
    Vorbis,
    Opus,
    /// Theora (valid 3.2.1 ID header, KFGSHIFT 6, 25 fps): exercises
    /// the muxer's split granule-position packer and the demuxer's
    /// frame-index pts path.
    Theora,
}

fn make_stream(index: u32, codec: Codec) -> StreamInfo {
    if codec == Codec::Theora {
        let mut params = CodecParameters::video(CodecId::new("theora"));
        params.extradata = mux::xiph_lace(&[
            &theora_valid_id_packet(6),
            &theora_comment_packet(),
            &theora_setup_packet(),
        ])
        .expect("three packets lace");
        return StreamInfo {
            index,
            time_base: TimeBase::new(1, 25),
            duration: None,
            start_time: Some(0),
            params,
        };
    }
    let opus = codec == Codec::Opus;
    let mut params = CodecParameters::audio(CodecId::new(if opus { "opus" } else { "vorbis" }));
    params.channels = Some(2);
    params.sample_rate = Some(48_000);
    params.extradata = if opus {
        opus_head_packet(312)
    } else {
        mux::xiph_lace(&[
            &vorbis_id_packet(),
            &vorbis_comment_packet(),
            &vorbis_setup_packet(),
        ])
        .expect("three packets lace")
    };
    StreamInfo {
        index,
        time_base: TimeBase::new(1, 48_000),
        duration: None,
        start_time: Some(0),
        params,
    }
}

/// Presentation-time (seconds) of a first-link page granule, for the
/// cross-stream ordering invariant. Serial == stream index for the
/// first link (`derive_serial`).
fn page_secs(codecs: &[Codec], serial: u32, granule: i64) -> Option<f64> {
    let codec = *codecs.get(serial as usize)?;
    if codec == Codec::Theora {
        // KFGSHIFT 6, frame-count origin, 25 fps.
        let count = (granule >> 6) + (granule & 63);
        Some(count as f64 / 25.0)
    } else {
        Some(granule as f64 / 48_000.0)
    }
}

fuzz_target!(|data: &[u8]| {
    if data.len() < 2 {
        return;
    }
    let ctrl = data[0];
    let n_streams = 1 + (ctrl & 1) as u32; // 1 or 2 first-link streams
    let use_target = ctrl & 0x02 != 0;
    let want_chain = ctrl & 0x04 != 0;
    let theora_first = ctrl & 0x08 != 0; // stream 0 becomes Theora
    let opus_bits = ctrl >> 4;

    let codecs: Vec<Codec> = (0..n_streams)
        .map(|i| {
            if i == 0 && theora_first {
                Codec::Theora
            } else if opus_bits & (1 << i) != 0 {
                Codec::Opus
            } else {
                Codec::Vorbis
            }
        })
        .collect();
    let streams: Vec<StreamInfo> = (0..n_streams)
        .map(|i| make_stream(i, codecs[i as usize]))
        .collect();

    let shared = SharedBuf::default();
    let out: Box<dyn WriteSeek> = Box::new(shared.clone());
    let mut mx = match mux::open_concrete(out, &streams) {
        Ok(m) => m,
        Err(_) => return,
    };
    if use_target {
        mx.set_page_target_bytes(Some(4096));
    }
    if mx.write_header().is_err() {
        return;
    }

    // Expected payloads per PUBLIC stream index: link 0 streams occupy
    // 0..n_streams; a chained link's single stream lands at n_streams.
    let total_slots = n_streams as usize + usize::from(want_chain);
    let mut expected: Vec<Vec<Vec<u8>>> = vec![Vec::new(); total_slots];

    let mut pts: i64 = 1;
    let mut theora_frame: i64 = 0;
    let mut huge_used = 0usize;
    let mut wrote_any = false;
    let mut in_second_link = false;
    let mut expected_links = 1u32;

    let descs: Vec<&[u8]> = data[1..].chunks_exact(3).take(MAX_PACKETS).collect();
    let switch_at = descs.len() / 2;

    for (i, desc) in descs.iter().enumerate() {
        // Halfway through, optionally open the chained second link
        // (requires at least one content packet in the current link).
        if want_chain && !in_second_link && i == switch_at && wrote_any {
            let link2 = vec![make_stream(
                0,
                if desc[0] & 0x40 != 0 {
                    Codec::Opus
                } else {
                    Codec::Vorbis
                },
            )];
            if mx.begin_new_link(&link2).is_err() {
                return;
            }
            in_second_link = true;
            expected_links = 2;
        }

        let (local_index, slot) = if in_second_link {
            (0u32, n_streams as usize)
        } else {
            let s = (desc[0] as u32) % n_streams;
            (s, s as usize)
        };
        let mut class = (desc[1] & 0x07) as usize;
        if SIZE_CLASSES[class] == 66_000 {
            if huge_used >= MAX_HUGE {
                class = 6;
            } else {
                huge_used += 1;
            }
        }
        let payload = vec![desc[1]; SIZE_CLASSES[class]];
        let is_theora = !in_second_link && codecs[local_index as usize] == Codec::Theora;

        let mut pkt = if is_theora {
            // Theora packets are frames: pts is the 0-based frame
            // index (one per packet), the keyframe flag is fuzz-chosen,
            // and the muxer packs the split granule. An unflagged run
            // longer than 2^KFGSHIFT-1 frames is a legitimate caller
            // error the muxer rejects — bail like any other Err below.
            let mut pkt = Packet::new(local_index, TimeBase::new(1, 25), payload.clone());
            pkt.pts = Some(theora_frame);
            pkt.flags.keyframe = desc[2] & 0x40 != 0 || theora_frame == 0;
            theora_frame += 1;
            pkt
        } else {
            pts += (desc[2] & 0x7F) as i64 * 31;
            let mut pkt = Packet::new(local_index, TimeBase::new(1, 48_000), payload.clone());
            pkt.pts = Some(pts);
            pkt.flags.keyframe = true;
            pkt
        };
        pkt.flags.unit_boundary = desc[0] & 0x80 != 0;
        if mx.write_packet(&pkt).is_err() {
            return;
        }
        expected[slot].push(payload);
        wrote_any = true;
    }
    if !wrote_any {
        return;
    }
    if mx.write_trailer().is_err() {
        return;
    }
    drop(mx);

    let bytes = {
        let guard = shared.0.lock().unwrap();
        guard.get_ref().clone()
    };

    // ---- demux side ----
    let reader: Box<dyn ReadSeek> = Box::new(Cursor::new(bytes));
    let mut dmx =
        demux::open_concrete(reader, &NullCodecResolver).expect("muxer output must open cleanly");

    // Slots the demuxer may legitimately never materialise: a chained
    // link that was requested but never opened (no content before the
    // switch point, or too few descriptors).
    let materialised_slots = n_streams as usize + usize::from(in_second_link);

    let mut got: Vec<Vec<Vec<u8>>> = vec![Vec::new(); materialised_slots];
    loop {
        match dmx.next_packet() {
            Ok(pkt) => {
                let idx = pkt.stream_index as usize;
                assert!(
                    idx < materialised_slots,
                    "demuxer emitted stream index {idx} beyond the {materialised_slots} written streams"
                );
                got[idx].push(pkt.data);
            }
            Err(Error::Eof) => break,
            Err(e) => panic!("demux of muxer output errored: {e:?}"),
        }
    }

    assert_eq!(
        dmx.streams().len(),
        materialised_slots,
        "public stream count must equal the written stream count"
    );
    for slot in 0..materialised_slots {
        assert_eq!(
            got[slot], expected[slot],
            "stream {slot} payload sequence must round-trip"
        );
    }

    // Cross-stream page-order invariant (Theora spec §A.3.2 / the
    // RFC 3533 interleave design): a single-link multiplex's data
    // pages appear in non-decreasing granule-time order. (Chained
    // files restart the clock per link, so the walk is skipped there.)
    if !in_second_link {
        let bytes = {
            let guard = shared.0.lock().unwrap();
            guard.get_ref().clone()
        };
        let mut off = 0usize;
        let mut last_t = 0.0f64;
        while off < bytes.len() {
            let (page, used) =
                oxideav_ogg::page::Page::parse(&bytes[off..]).expect("muxer output pages parse");
            off += used;
            if page.granule_position <= 0 {
                continue; // header pages and no-packet-finishes pages
            }
            if let Some(t) = page_secs(&codecs, page.serial, page.granule_position) {
                assert!(
                    t >= last_t,
                    "page (serial {} granule {}) at {t}s written after {last_t}s",
                    page.serial,
                    page.granule_position
                );
                last_t = t;
            }
        }
    }

    // Damage counters on our own output must all be zero.
    assert_eq!(dmx.hole_count(), 0, "muxer output must have no holes");
    assert_eq!(
        dmx.framing_error_count(),
        0,
        "muxer output must have no framing errors"
    );
    assert_eq!(dmx.resync_count(), 0, "muxer output must need no resyncs");
    assert_eq!(
        dmx.duplicate_serial_count(),
        0,
        "muxer output must have unique serials"
    );
    assert_eq!(
        dmx.link_count(),
        expected_links,
        "link count must match what was written"
    );
    let _ = dmx.duration_micros();
});
