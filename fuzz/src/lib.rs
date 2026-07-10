//! Support lib for the `oxideav-ogg` fuzz crate.
//!
//! Shared header-packet builders used by the structure-aware fuzz
//! targets (`mux_roundtrip`, `chain_graph`, `seek_hostile`). Every
//! byte layout here is hand-written from the staged specs (Vorbis I
//! §4.2.2, RFC 7845 §5, Speex manual §7.3, Theora ident conventions
//! as sniffed by `oxideav_ogg::codec_id`) — no external library code
//! consulted, per the clean-room wall.

/// Vorbis I identification packet (Vorbis I spec §4.2.2): type 0x01,
/// "vorbis", version 0, channels, sample rate, 3 bitrate fields,
/// packed blocksizes, framing bit. 30 bytes.
pub fn vorbis_id_packet() -> Vec<u8> {
    let mut p = Vec::with_capacity(30);
    p.push(0x01);
    p.extend_from_slice(b"vorbis");
    p.extend_from_slice(&0u32.to_le_bytes()); // vorbis_version
    p.push(2); // audio_channels
    p.extend_from_slice(&48_000u32.to_le_bytes()); // audio_sample_rate
    p.extend_from_slice(&0i32.to_le_bytes()); // bitrate_maximum
    p.extend_from_slice(&128_000i32.to_le_bytes()); // bitrate_nominal
    p.extend_from_slice(&0i32.to_le_bytes()); // bitrate_minimum
    p.push(0xB8); // blocksize_0 | blocksize_1 packed
    p.push(0x01); // framing bit
    p
}

/// Minimal Vorbis comment packet: type 0x03, "vorbis", empty vendor,
/// zero comments, framing bit.
pub fn vorbis_comment_packet() -> Vec<u8> {
    let mut p = Vec::with_capacity(16);
    p.push(0x03);
    p.extend_from_slice(b"vorbis");
    p.extend_from_slice(&0u32.to_le_bytes()); // vendor_length
    p.extend_from_slice(&0u32.to_le_bytes()); // user_comment_list_length
    p.push(0x01); // framing bit
    p
}

/// Minimal Vorbis setup packet placeholder: type 0x05, "vorbis",
/// 16 zero bytes. The demuxer only counts it, never decodes it.
pub fn vorbis_setup_packet() -> Vec<u8> {
    let mut p = Vec::with_capacity(23);
    p.push(0x05);
    p.extend_from_slice(b"vorbis");
    p.extend_from_slice(&[0u8; 16]);
    p
}

/// Minimal OpusHead identification packet (RFC 7845 §5.1): magic,
/// version 1, channel count, pre-skip, input sample rate, output
/// gain, channel mapping family 0. 19 bytes.
pub fn opus_head_packet(pre_skip: u16) -> Vec<u8> {
    let mut p = Vec::with_capacity(19);
    p.extend_from_slice(b"OpusHead");
    p.push(1); // version
    p.push(2); // channel count
    p.extend_from_slice(&pre_skip.to_le_bytes()); // pre-skip (48 kHz samples)
    p.extend_from_slice(&48_000u32.to_le_bytes()); // input sample rate
    p.extend_from_slice(&0i16.to_le_bytes()); // output gain
    p.push(0); // channel mapping family
    p
}

/// Minimal OpusTags packet (RFC 7845 §5.2): magic, empty vendor,
/// zero comments.
pub fn opus_tags_packet() -> Vec<u8> {
    let mut p = Vec::with_capacity(16);
    p.extend_from_slice(b"OpusTags");
    p.extend_from_slice(&0u32.to_le_bytes()); // vendor string length
    p.extend_from_slice(&0u32.to_le_bytes()); // user comment count
    p
}

/// Theora-shaped identification packet: the 0x80 + "theora" signature
/// `oxideav_ogg::codec_id::detect` sniffs, padded so downstream field
/// reads (if any) stay in-bounds. Only the signature is load-bearing
/// for the container layer.
pub fn theora_id_packet() -> Vec<u8> {
    let mut p = Vec::with_capacity(42);
    p.push(0x80);
    p.extend_from_slice(b"theora");
    p.extend_from_slice(&[0u8; 35]);
    p
}

/// Theora-shaped comment packet: 0x81 + "theora" + empty
/// vorbis-comment body.
pub fn theora_comment_packet() -> Vec<u8> {
    let mut p = Vec::with_capacity(16);
    p.push(0x81);
    p.extend_from_slice(b"theora");
    p.extend_from_slice(&0u32.to_le_bytes());
    p.extend_from_slice(&0u32.to_le_bytes());
    p
}

/// Theora-shaped setup packet: 0x82 + "theora" + padding.
pub fn theora_setup_packet() -> Vec<u8> {
    let mut p = Vec::with_capacity(15);
    p.push(0x82);
    p.extend_from_slice(b"theora");
    p.extend_from_slice(&[0u8; 8]);
    p
}
