//! End-to-end: demux a Theora-in-Ogg file, then remux its packets through the
//! auto-index muxer. The Skeleton 4.0 keyframe index the muxer builds keys off
//! `PacketFlags::keyframe`, which the demuxer derives from the granuleshift
//! packing (`docs/container/ogg/ogg-skeleton-4.0.md`). Before the demuxer
//! distinguished Theora keyframes from inter-frames, every packet was flagged
//! a keyframe, so a remux would record one keypoint per *frame* instead of one
//! per *keyframe*. This test pins the corrected flow: only true keyframes
//! (granule offset-since-keyframe == 0) become index keypoints.
//!
//! Spec: `docs/container/ogg/rfc3533-ogg.txt`,
//! `docs/container/ogg/ogg-skeleton-4.0.md`.

use std::io::Cursor;

use oxideav_core::{CodecId, CodecParameters, Packet, ReadSeek, StreamInfo, TimeBase, WriteSeek};
use oxideav_ogg::page::{flags, lace, Page};
use oxideav_ogg::skeleton::{FisBone, FisHead, Rational, Skeleton, Version};

const SKEL_SERIAL: u32 = 0x5BE1E70F;
const THEORA_SERIAL: u32 = 0x71EB1A11;

fn single_packet_page(
    packet: &[u8],
    flags_byte: u8,
    serial: u32,
    seq: u32,
    granule: i64,
) -> Vec<u8> {
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

fn theora_id_packet() -> Vec<u8> {
    let mut p = vec![0x80];
    p.extend_from_slice(b"theora");
    p.extend_from_slice(&[0u8; 35]);
    p
}

fn theora_comment_packet() -> Vec<u8> {
    let mut p = vec![0x81];
    p.extend_from_slice(b"theora");
    p.extend_from_slice(&0u32.to_le_bytes());
    p.extend_from_slice(&0u32.to_le_bytes());
    p
}

fn theora_setup_packet() -> Vec<u8> {
    let mut p = vec![0x82];
    p.extend_from_slice(b"theora");
    p.extend_from_slice(&[0u8; 24]);
    p
}

/// Granules describing frames under shift = 6:
///   30   = (0<<6)|30   -> inter
///   64   = (1<<6)|0    -> KEYFRAME (frame 64)
///   100  = (1<<6)|36   -> inter
///   8192 = (128<<6)|0  -> KEYFRAME (frame 128)
/// i.e. exactly 2 of the 4 data pages are keyframes.
const GRANULES: [i64; 4] = [30, 64, 100, 8192];
const GRANULESHIFT: u8 = 6;

fn build_theora_file() -> Vec<u8> {
    let mut head = FisHead::new(Version::V4_0);
    head.presentation_time = Rational::new(0, 1);
    head.basetime = Rational::new(0, 1);
    head.segment_length = Some(0);
    head.content_byte_offset = Some(0);

    let mut bone = FisBone::new(THEORA_SERIAL, Rational::new(30, 1));
    bone.num_headers = 3;
    bone.granuleshift = GRANULESHIFT;
    bone.set_header("Content-Type", "video/theora");

    let mut out = Vec::new();
    out.extend(single_packet_page(
        &head.to_bytes(),
        flags::FIRST_PAGE,
        SKEL_SERIAL,
        0,
        0,
    ));
    out.extend(single_packet_page(
        &theora_id_packet(),
        flags::FIRST_PAGE,
        THEORA_SERIAL,
        0,
        0,
    ));
    out.extend(single_packet_page(
        &theora_comment_packet(),
        0,
        THEORA_SERIAL,
        1,
        0,
    ));
    out.extend(single_packet_page(&bone.to_bytes(), 0, SKEL_SERIAL, 1, 0));
    out.extend(single_packet_page(
        &theora_setup_packet(),
        0,
        THEORA_SERIAL,
        2,
        0,
    ));
    out.extend(single_packet_page(&[], flags::LAST_PAGE, SKEL_SERIAL, 2, 0));

    for (i, gr) in GRANULES.iter().enumerate() {
        let last = i + 1 == GRANULES.len();
        let flag = if last { flags::LAST_PAGE } else { 0 };
        out.extend(single_packet_page(
            &[0xAB; 16],
            flag,
            THEORA_SERIAL,
            3 + i as u32,
            *gr,
        ));
    }
    out
}

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

#[test]
fn theora_demux_remux_index_records_only_true_keyframes() {
    // --- 1. Demux the source Theora file, collecting (pts, keyframe). ---
    let src = build_theora_file();
    let reader: Box<dyn ReadSeek> = Box::new(Cursor::new(src));
    let mut dmx = oxideav_ogg::demux::open(reader, &oxideav_core::NullCodecResolver).expect("open");
    assert_eq!(dmx.streams()[0].params.codec_id.as_str(), "theora");

    // Reuse the demuxer's reconstructed extradata (the 3-packet Xiph-laced
    // Theora header blob) so the remux can write a valid Theora BOS.
    let theora_extradata = dmx.streams()[0].params.extradata.clone();
    assert!(
        !theora_extradata.is_empty(),
        "demuxer must reconstruct Theora extradata"
    );

    let mut demuxed: Vec<(Option<i64>, bool)> = Vec::new();
    while let Ok(pkt) = dmx.next_packet() {
        demuxed.push((pkt.pts, pkt.flags.keyframe));
    }
    assert_eq!(demuxed.len(), 4, "4 Theora data packets");

    // The demuxer flags a keyframe iff the granule's low 6 bits are zero.
    let kf_flags: Vec<bool> = demuxed.iter().map(|(_, kf)| *kf).collect();
    assert_eq!(
        kf_flags,
        vec![false, true, false, true],
        "only granules 64 and 8192 (offset 0) are keyframes"
    );
    // Each page carried a granule, so every packet has a pts.
    let pts: Vec<i64> = demuxed.iter().map(|(p, _)| p.unwrap()).collect();
    assert_eq!(pts, vec![30, 64, 100, 8192]);

    // --- 2. Remux those packets through the auto-index muxer. The index keys
    //        off PacketFlags::keyframe, so it must record exactly 2 keypoints
    //        (the true keyframes), not 4. ---
    let mut out_params = CodecParameters::video(CodecId::new("theora"));
    out_params.extradata = theora_extradata;
    let out_stream = StreamInfo {
        index: 0,
        time_base: TimeBase::new(1, 30), // 30 fps; pts are frame numbers
        duration: None,
        start_time: Some(0),
        params: out_params,
    };

    let mut skel = Skeleton::new();
    skel.set_head(FisHead::new(Version::V4_0));
    let mut out_bone = FisBone::new(0, Rational::new(30, 1));
    out_bone.set_header("Content-Type", "video/theora");
    skel.push_bone(out_bone);

    let cfg = oxideav_ogg::mux::AutoIndexConfig {
        max_keypoints: 16,
        min_keypoint_byte_gap: 0,
        min_keypoint_time_gap_ms: 0,
    };

    let shared = SharedBuf::default();
    let writer: Box<dyn WriteSeek> = Box::new(shared.clone());
    let mut mux = oxideav_ogg::mux::open_with_skeleton_indexed(
        writer,
        std::slice::from_ref(&out_stream),
        skel,
        cfg,
    )
    .expect("open_with_skeleton_indexed");
    mux.write_header().unwrap();
    for (pts, keyframe) in &demuxed {
        let mut pkt = Packet::new(0, out_stream.time_base, vec![0xAB; 16]);
        pkt.pts = *pts;
        pkt.dts = *pts;
        pkt.flags.keyframe = *keyframe;
        pkt.flags.unit_boundary = true;
        mux.write_packet(&pkt).unwrap();
    }
    mux.write_trailer().unwrap();
    drop(mux);
    let remuxed = shared.0.lock().unwrap().get_ref().clone();

    // --- 3. Demux the remuxed output; the recovered index must carry exactly
    //        the 2 true keyframes at pts 64 and 8192. ---
    let reader: Box<dyn ReadSeek> = Box::new(Cursor::new(remuxed));
    let dmx2 = oxideav_ogg::demux::open_concrete(reader, &oxideav_core::NullCodecResolver)
        .expect("demux remuxed");
    let sk = dmx2.skeleton().expect("Skeleton recovered");
    assert_eq!(sk.indexes.len(), 1, "one index for the content stream");
    let idx = &sk.indexes[0];
    assert_eq!(
        idx.keypoints.len(),
        2,
        "index records 2 keyframes, not 4 frames — the M1 keyframe fix flowing \
         through demux -> auto-index remux"
    );
    let kp_ts: Vec<i64> = idx.keypoints.iter().map(|kp| kp.timestamp).collect();
    assert_eq!(kp_ts, vec![64, 8192], "keypoints at the true keyframe pts");
}
