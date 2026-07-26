//! Registry-backed codec identification for logical Ogg bitstreams.
//!
//! Core's codec registry lets codec crates declare their BOS-packet
//! magic prefixes at registration (`CodecInfo::payload_magic`), and the
//! demuxer resolves each logical stream's first packet through
//! `CodecResolver::resolve_payload_magic` (longest declared prefix
//! wins). These tests pin the demuxer's documented resolution order:
//!
//!   1. the resolver borrowed for the duration of `open()`;
//!   2. the demuxer-held shared resolver (`open_shared` /
//!      `open_concrete_shared`), which outlives `open` and therefore
//!      also covers chained links discovered mid-file;
//!   3. the built-in `codec_id::detect` table, for magics no registered
//!      codec claims.
//!
//! The registries are constructed locally (tag-only registrations, no
//! factories) because sibling codec crates may not have declared their
//! magics yet — and cross-crate dev-deps are barred anyway.

use std::io::Cursor;
use std::sync::Arc;

use oxideav_core::{
    CodecId, CodecInfo, CodecRegistry, Demuxer, MediaType, NullCodecResolver, ReadSeek,
};
use oxideav_ogg::page::{flags, lace, Page};
use oxideav_ogg::skeleton::{FisBone, FisHead, Rational, Version};

// ───────────────────────── fixtures ─────────────────────────

fn build_page(flags_byte: u8, granule: i64, serial: u32, seq: u32, packet: &[u8]) -> Vec<u8> {
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

/// Minimal valid Vorbis identification packet (30 bytes).
fn vorbis_id_packet(channels: u8, sample_rate: u32) -> Vec<u8> {
    let mut p = Vec::with_capacity(30);
    p.push(0x01);
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
    p.extend_from_slice(&0u32.to_le_bytes()); // user comment count
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

/// One complete Vorbis-in-Ogg link: BOS + comment + setup + `data_pages`
/// single-packet data pages, the last EOS-flagged.
fn build_vorbis_link(serial: u32, payload_byte: u8, data_pages: usize) -> Vec<u8> {
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
        let flag = if i + 1 == data_pages {
            flags::LAST_PAGE
        } else {
            0
        };
        out.extend(build_page(
            flag,
            960 * (i as i64 + 1),
            serial,
            seq,
            &[payload_byte, i as u8],
        ));
        seq += 1;
    }
    out
}

/// An Ogg/Speex identification header (Speex manual table 7.1): a fixed
/// 80-byte little-endian struct.
fn speex_header(rate: u32, channels: u32) -> Vec<u8> {
    let mut p = Vec::with_capacity(80);
    p.extend_from_slice(b"Speex   "); // speex_string (8)
    p.extend_from_slice(&[0u8; 20]); // speex_version (20)
    p.extend_from_slice(&1u32.to_le_bytes()); // speex_version_id
    p.extend_from_slice(&80u32.to_le_bytes()); // header_size
    p.extend_from_slice(&rate.to_le_bytes()); // rate
    p.extend_from_slice(&0u32.to_le_bytes()); // mode
    p.extend_from_slice(&4u32.to_le_bytes()); // mode_bitstream_version
    p.extend_from_slice(&channels.to_le_bytes()); // nb_channels
    p.extend_from_slice(&0u32.to_le_bytes()); // bitrate
    p.extend_from_slice(&160u32.to_le_bytes()); // frame_size
    p.extend_from_slice(&0u32.to_le_bytes()); // vbr
    p.extend_from_slice(&1u32.to_le_bytes()); // frames_per_packet
    p.extend_from_slice(&0u32.to_le_bytes()); // extra_headers
    p.extend_from_slice(&0u32.to_le_bytes()); // reserved1
    p.extend_from_slice(&0u32.to_le_bytes()); // reserved2
    debug_assert_eq!(p.len(), 80);
    p
}

/// A Speex comment packet — bare vorbis_comment layout, empty.
fn speex_comment_packet() -> Vec<u8> {
    let mut p = Vec::new();
    p.extend_from_slice(&0u32.to_le_bytes()); // vendor len
    p.extend_from_slice(&0u32.to_le_bytes()); // user comment count
    p
}

/// One complete Speex-in-Ogg link: BOS + comment + `data_pages`
/// single-packet data pages, the last EOS-flagged.
fn build_speex_link(serial: u32, payload_byte: u8, data_pages: usize) -> Vec<u8> {
    let mut out = Vec::new();
    let mut seq = 0u32;
    out.extend(build_page(
        flags::FIRST_PAGE,
        0,
        serial,
        seq,
        &speex_header(16_000, 1),
    ));
    seq += 1;
    out.extend(build_page(0, 0, serial, seq, &speex_comment_packet()));
    seq += 1;
    for i in 0..data_pages {
        let flag = if i + 1 == data_pages {
            flags::LAST_PAGE
        } else {
            0
        };
        out.extend(build_page(
            flag,
            160 * (i as i64 + 1),
            serial,
            seq,
            &[payload_byte, i as u8],
        ));
        seq += 1;
    }
    out
}

/// An `OpusHead` identification packet (RFC 7845 §5.1, 19 bytes for
/// channel mapping family 0).
fn opus_head_packet(channels: u8, pre_skip: u16, input_rate: u32) -> Vec<u8> {
    let mut p = Vec::with_capacity(19);
    p.extend_from_slice(b"OpusHead");
    p.push(1); // version
    p.push(channels);
    p.extend_from_slice(&pre_skip.to_le_bytes());
    p.extend_from_slice(&input_rate.to_le_bytes());
    p.extend_from_slice(&0i16.to_le_bytes()); // output gain
    p.push(0); // channel mapping family 0
    assert_eq!(p.len(), 19);
    p
}

fn opus_tags_packet() -> Vec<u8> {
    let mut p = Vec::new();
    p.extend_from_slice(b"OpusTags");
    p.extend_from_slice(&0u32.to_le_bytes()); // vendor len
    p.extend_from_slice(&0u32.to_le_bytes()); // user comment count
    p
}

/// One complete Opus-in-Ogg link: BOS + tags + one EOS data page.
fn build_opus_stream(serial: u32) -> Vec<u8> {
    let mut out = Vec::new();
    out.extend(build_page(
        flags::FIRST_PAGE,
        0,
        serial,
        0,
        &opus_head_packet(2, 312, 48_000),
    ));
    out.extend(build_page(0, 0, serial, 1, &opus_tags_packet()));
    out.extend(build_page(
        flags::LAST_PAGE,
        960 + 312,
        serial,
        2,
        &[0xA0, 0x01],
    ));
    out
}

/// A FLAC-in-Ogg mapping packet with an embedded STREAMINFO (RFC 9639
/// §10.1 / §8.2), declaring 0 extra header packets.
fn flac_mapping_packet(sample_rate: u32, channels: u32) -> Vec<u8> {
    let mut p = Vec::new();
    p.push(0x7F);
    p.extend_from_slice(b"FLAC");
    p.extend_from_slice(&[0x01, 0x00]); // mapping version 1.0
    p.extend_from_slice(&0u16.to_be_bytes()); // 0 extra header packets
    p.extend_from_slice(b"fLaC");
    p.extend_from_slice(&[0x00, 0x00, 0x00, 34]); // STREAMINFO header
    let mut si = vec![0u8; 34];
    let packed: u32 = ((sample_rate & 0xF_FFFF) << 12) | (((channels - 1) & 0x7) << 9) | (15 << 4);
    si[10..14].copy_from_slice(&packed.to_be_bytes());
    p.extend_from_slice(&si);
    p
}

fn build_flac_stream(serial: u32) -> Vec<u8> {
    let mut out = Vec::new();
    out.extend(build_page(
        flags::FIRST_PAGE,
        0,
        serial,
        0,
        &flac_mapping_packet(44_100, 2),
    ));
    out.extend(build_page(flags::LAST_PAGE, 4096, serial, 1, &[0xF8, 0x01]));
    out
}

/// A Theora identification header signature followed by an opaque body.
/// `TheoraIdHeader::parse` failing on the body is fine — identification
/// is what's under test and the demuxer degrades gracefully.
fn theora_id_signature_packet() -> Vec<u8> {
    let mut p = vec![0x80];
    p.extend_from_slice(b"theora");
    p.extend_from_slice(&[0u8; 36]);
    p
}

fn build_theora_bos_only(serial: u32) -> Vec<u8> {
    let mut out = Vec::new();
    out.extend(build_page(
        flags::FIRST_PAGE,
        0,
        serial,
        0,
        &theora_id_signature_packet(),
    ));
    out.extend(build_page(flags::LAST_PAGE, 1, serial, 1, &[0x00, 0x42]));
    out
}

// ───────────────────────── registries ─────────────────────────

/// A locally-constructed registry carrying the given payload-magic
/// claims (tag-only registrations — no factories needed to resolve).
fn registry(claims: &[(&[u8], &str)]) -> CodecRegistry {
    let mut reg = CodecRegistry::new();
    for &(magic, id) in claims {
        reg.register(CodecInfo::new(CodecId::new(id)).payload_magic(magic));
    }
    reg
}

/// The Ogg family's canonical claims, as the codec crates will declare
/// them once they register payload magics.
fn family_registry() -> CodecRegistry {
    registry(&[
        (b"\x01vorbis", "vorbis"),
        (b"OpusHead", "opus"),
        (b"\x80theora", "theora"),
        (b"Speex   ", "speex"),
        (b"\x7FFLAC", "flac"),
    ])
}

fn open_with(bytes: Vec<u8>, reg: &CodecRegistry) -> Box<dyn Demuxer> {
    let input: Box<dyn ReadSeek> = Box::new(Cursor::new(bytes));
    oxideav_ogg::demux::open(input, reg).expect("open")
}

fn drain(demux: &mut dyn Demuxer) -> Vec<oxideav_core::Packet> {
    let mut out = Vec::new();
    while let Ok(p) = demux.next_packet() {
        out.push(p);
    }
    out
}

// ───────────────────────── tests ─────────────────────────

#[test]
fn family_magics_resolve_via_registry() {
    let reg = family_registry();
    let cases: Vec<(Vec<u8>, &str)> = vec![
        (build_vorbis_link(0x0001, 0xAA, 2), "vorbis"),
        (build_opus_stream(0x0002), "opus"),
        (build_speex_link(0x0003, 0x5E, 2), "speex"),
        (build_flac_stream(0x0004), "flac"),
        (build_theora_bos_only(0x0005), "theora"),
    ];
    for (bytes, want) in cases {
        let demux = open_with(bytes, &reg);
        assert_eq!(
            demux.streams()[0].params.codec_id.as_str(),
            want,
            "registry-backed identification for {want}"
        );
    }
}

#[test]
fn registry_resolved_canonical_ids_keep_mapping_intelligence() {
    // A registry answer of "vorbis" must engage exactly the same
    // downstream mapping logic as the built-in table: 3 header packets
    // absorbed into extradata, 1/sample_rate time base, data packets
    // delivered as content.
    let reg = family_registry();
    let mut demux = open_with(build_vorbis_link(0x1001, 0xAA, 3), &reg);
    let s = &demux.streams()[0];
    assert_eq!(s.params.codec_id.as_str(), "vorbis");
    assert_eq!(s.params.sample_rate, Some(48_000));
    assert_eq!(s.time_base.0.den, 48_000);
    let packets = drain(demux.as_mut());
    assert_eq!(packets.len(), 3, "3 data packets; headers absorbed");
    assert!(packets.iter().all(|p| p.data[0] == 0xAA));
    let extra = &demux.streams()[0].params.extradata;
    assert_eq!(
        extra.first().copied(),
        Some(0x02),
        "extradata is the Xiph-laced 3-packet header set"
    );
}

#[test]
fn registry_claim_wins_over_builtin_table() {
    // Resolver-first doctrine: when a registered codec claims the
    // magic, its answer is used even though the built-in table also
    // knows this prefix. A non-canonical id opts the stream out of the
    // vorbis mapping specifics: no header budget (all 6 packets are
    // delivered) and the placeholder time base.
    let reg = registry(&[(b"\x01vorbis", "vorbis-r430")]);
    let mut demux = open_with(build_vorbis_link(0x2001, 0xAB, 3), &reg);
    let s = &demux.streams()[0];
    assert_eq!(s.params.codec_id.as_str(), "vorbis-r430");
    assert_eq!(s.params.media_type, MediaType::Unknown);
    assert_eq!(s.time_base.0.den, 1_000_000, "placeholder time base");
    let packets = drain(demux.as_mut());
    assert_eq!(
        packets.len(),
        6,
        "unknown mapping: id + comment + setup + 3 data packets all delivered"
    );
}

#[test]
fn unclaimed_magic_falls_back_to_builtin_table() {
    // The resolver is consulted but claims only Opus; a Vorbis stream
    // must still be identified by the built-in table, with the full
    // mapping intelligence engaged.
    let reg = registry(&[(b"OpusHead", "opus")]);
    let mut demux = open_with(build_vorbis_link(0x3001, 0xAC, 2), &reg);
    let s = &demux.streams()[0];
    assert_eq!(s.params.codec_id.as_str(), "vorbis");
    assert_eq!(s.time_base.0.den, 48_000);
    assert_eq!(drain(demux.as_mut()).len(), 2);
}

#[test]
fn null_resolver_keeps_builtin_path() {
    // `NullCodecResolver` resolves nothing (the trait's default
    // `resolve_payload_magic` returns `None`), so the historical
    // behaviour is unchanged for resolver-free callers.
    let input: Box<dyn ReadSeek> = Box::new(Cursor::new(build_vorbis_link(0x4001, 0xAD, 2)));
    let demux = oxideav_ogg::demux::open(input, &NullCodecResolver).expect("open");
    assert_eq!(demux.streams()[0].params.codec_id.as_str(), "vorbis");
}

#[test]
fn locally_unknown_codec_resolves_via_registry() {
    // A codec the built-in table has never heard of, identified purely
    // by its registry claim — the point of registry-first resolution:
    // new codec crates extend identification without touching this
    // crate.
    let mut bos = b"Xr430Magic\x01\x02".to_vec();
    bos.extend_from_slice(&[0u8; 8]);
    let mut bytes = Vec::new();
    bytes.extend(build_page(flags::FIRST_PAGE, 0, 0x5001, 0, &bos));
    bytes.extend(build_page(flags::LAST_PAGE, 100, 0x5001, 1, &[0xEE, 0x01]));

    let reg = registry(&[(b"Xr430Magic", "xcodec-r430")]);
    let mut demux = open_with(bytes, &reg);
    let s = &demux.streams()[0];
    assert_eq!(s.params.codec_id.as_str(), "xcodec-r430");
    assert_eq!(s.params.media_type, MediaType::Unknown);
    // No header budget is known for an unmapped codec: both packets
    // (the identification packet and the data packet) are delivered.
    assert_eq!(drain(demux.as_mut()).len(), 2);
}

#[test]
fn longest_declared_prefix_wins() {
    // Two claims share a prefix; the demuxer hands the whole first
    // packet to the resolver and the longest (most specific) declared
    // magic must win regardless of registration order.
    let reg = registry(&[(b"Opus", "opus-generic"), (b"OpusHead", "opus")]);
    let demux = open_with(build_opus_stream(0x6001), &reg);
    let s = &demux.streams()[0];
    assert_eq!(s.params.codec_id.as_str(), "opus");
    assert_eq!(s.time_base.0.den, 48_000, "Opus 48 kHz time base engaged");
}

#[test]
fn grouped_streams_all_resolve_at_open() {
    // Two grouped logical bitstreams (both BOS pages in the initial
    // section) must both resolve through the borrowed resolver during
    // `open()`. Custom ids prove the registry answered for each.
    let v_serial = 0x7001u32;
    let s_serial = 0x7002u32;
    let mut bytes = Vec::new();
    bytes.extend(build_page(
        flags::FIRST_PAGE,
        0,
        v_serial,
        0,
        &vorbis_id_packet(2, 48_000),
    ));
    bytes.extend(build_page(
        flags::FIRST_PAGE,
        0,
        s_serial,
        0,
        &speex_header(16_000, 1),
    ));
    bytes.extend(build_page(flags::LAST_PAGE, 960, v_serial, 1, &[0xAA, 1]));
    bytes.extend(build_page(flags::LAST_PAGE, 160, s_serial, 1, &[0xBB, 1]));

    let reg = registry(&[(b"\x01vorbis", "vorbis-r430"), (b"Speex   ", "speex-r430")]);
    let demux = open_with(bytes, &reg);
    let ids: Vec<&str> = demux
        .streams()
        .iter()
        .map(|s| s.params.codec_id.as_str())
        .collect();
    assert_eq!(ids, vec!["vorbis-r430", "speex-r430"]);
}

#[test]
fn chained_link_resolves_via_shared_resolver() {
    // A chained physical bitstream: the second link's BOS is only met
    // during `next_packet`, after `open` returned. The shared-resolver
    // entry point keeps the registry handle alive, so the late link
    // still resolves registry-first (custom id proves the path).
    let mut bytes = build_vorbis_link(0xAAAA_0001, 0xAA, 2);
    bytes.extend(build_speex_link(0xBBBB_0002, 0xBB, 3));

    let reg = registry(&[(b"\x01vorbis", "vorbis"), (b"Speex   ", "speex-r430")]);
    let input: Box<dyn ReadSeek> = Box::new(Cursor::new(bytes));
    let mut demux =
        oxideav_ogg::demux::open_concrete_shared(input, Arc::new(reg)).expect("open_shared");
    assert_eq!(demux.streams().len(), 1, "second link not yet visible");
    assert_eq!(demux.streams()[0].params.codec_id.as_str(), "vorbis");

    while let Ok(_p) = demux.next_packet() {}
    let ids: Vec<&str> = demux
        .streams()
        .iter()
        .map(|s| s.params.codec_id.as_str())
        .collect();
    assert_eq!(
        ids,
        vec!["vorbis", "speex-r430"],
        "chained link identified through the shared resolver"
    );
}

#[test]
fn chained_link_falls_back_without_shared_resolver() {
    // Documented fallback order for the borrowed-resolver entry point:
    // the registry answers for streams identified during `open()` (the
    // custom first-link id proves it), while a chained link discovered
    // after `open` returns is identified by the built-in table (the
    // canonical "speex", NOT the registry's custom id).
    let mut bytes = build_vorbis_link(0xAAAA_0011, 0xAA, 2);
    bytes.extend(build_speex_link(0xBBBB_0012, 0xBB, 2));

    let reg = registry(&[(b"\x01vorbis", "vorbis-r430"), (b"Speex   ", "speex-r430")]);
    let mut demux = open_with(bytes, &reg);
    assert_eq!(demux.streams()[0].params.codec_id.as_str(), "vorbis-r430");

    let _ = drain(demux.as_mut());
    let ids: Vec<&str> = demux
        .streams()
        .iter()
        .map(|s| s.params.codec_id.as_str())
        .collect();
    assert_eq!(
        ids,
        vec!["vorbis-r430", "speex"],
        "post-open chained link uses the built-in fallback table"
    );
}

#[test]
fn build_seek_index_registers_chained_links_via_shared_resolver() {
    // `build_seek_index`'s full-file scan pre-registers every link's
    // BOS. Run before any packet is drained, it must identify the
    // chained link through the demuxer-held shared resolver too.
    let mut bytes = build_vorbis_link(0xAAAA_0021, 0xAA, 2);
    bytes.extend(build_speex_link(0xBBBB_0022, 0xBB, 2));

    let reg = registry(&[(b"\x01vorbis", "vorbis"), (b"Speex   ", "speex-r430")]);
    let input: Box<dyn ReadSeek> = Box::new(Cursor::new(bytes));
    let mut demux =
        oxideav_ogg::demux::open_concrete_shared(input, Arc::new(reg)).expect("open_shared");
    demux.build_seek_index().expect("index build");
    let ids: Vec<&str> = demux
        .streams()
        .iter()
        .map(|s| s.params.codec_id.as_str())
        .collect();
    assert_eq!(ids, vec!["vorbis", "speex-r430"]);
}

#[test]
fn open_indexed_identifies_chained_links_registry_first() {
    // `open_indexed` runs the full-file scan while the borrowed
    // resolver is still in scope, so every chained link's BOS is
    // pre-registered registry-first — no shared handle needed.
    let mut bytes = build_vorbis_link(0xAAAA_0031, 0xAA, 2);
    bytes.extend(build_speex_link(0xBBBB_0032, 0xBB, 2));

    let reg = registry(&[(b"\x01vorbis", "vorbis"), (b"Speex   ", "speex-r430")]);
    let input: Box<dyn ReadSeek> = Box::new(Cursor::new(bytes));
    let demux = oxideav_ogg::demux::open_indexed(input, &reg).expect("open_indexed");
    let ids: Vec<&str> = demux
        .streams()
        .iter()
        .map(|s| s.params.codec_id.as_str())
        .collect();
    assert_eq!(ids, vec!["vorbis", "speex-r430"]);
}

#[test]
fn skeleton_fast_path_unaffected_by_registry_claims() {
    // The Skeleton bitstream is container-level metadata, identified
    // before codec resolution runs. A hostile registry claim on the
    // `fishead\0` magic must neither register a phantom content stream
    // nor disturb Skeleton parsing.
    let skel_serial = 0xCAFE_0001u32;
    let v_serial = 0x9001u32;

    let mut head = FisHead::new(Version::V4_0);
    head.presentation_time = Rational::new(0, 1000);
    head.basetime = Rational::new(0, 1000);
    head.segment_length = Some(0);
    head.content_byte_offset = Some(0);
    let mut bone = FisBone::new(v_serial, Rational::new(48_000, 1));
    bone.num_headers = 3;
    bone.set_header("Content-Type", "audio/vorbis");

    let mut bytes = Vec::new();
    bytes.extend(build_page(
        flags::FIRST_PAGE,
        0,
        skel_serial,
        0,
        &head.to_bytes(),
    ));
    bytes.extend(build_page(
        flags::FIRST_PAGE,
        0,
        v_serial,
        0,
        &vorbis_id_packet(2, 48_000),
    ));
    bytes.extend(build_page(0, 0, v_serial, 1, &vorbis_comment_packet()));
    bytes.extend(build_page(0, 0, skel_serial, 1, &bone.to_bytes()));
    bytes.extend(build_page(0, 0, v_serial, 2, &vorbis_setup_packet()));
    bytes.extend(build_page(flags::LAST_PAGE, 0, skel_serial, 2, &[]));
    bytes.extend(build_page(flags::LAST_PAGE, 960, v_serial, 3, &[0xAA, 1]));

    let reg = registry(&[(b"fishead\x00", "evil-r430"), (b"\x01vorbis", "vorbis")]);
    let input: Box<dyn ReadSeek> = Box::new(Cursor::new(bytes));
    let demux = oxideav_ogg::demux::open_concrete(input, &reg).expect("open");

    assert_eq!(demux.streams().len(), 1, "Skeleton is not a content stream");
    assert_eq!(demux.streams()[0].params.codec_id.as_str(), "vorbis");
    let sk = demux.skeleton().expect("skeleton parsed");
    assert_eq!(sk.bones.len(), 1, "fisbone captured");
}
