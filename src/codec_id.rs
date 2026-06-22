//! Sniff the codec carried by a logical Ogg bitstream from its first packet.
//!
//! Ogg is codec-agnostic: the first packet of every logical stream is a
//! codec-specific identification header that begins with a recognisable
//! signature. We use that to set [`CodecId`] in the demuxer's
//! [`StreamInfo`] without depending on any per-codec crate.

use oxideav_core::CodecId;

/// Identify the codec of a logical Ogg bitstream from its first packet.
pub fn detect(first_packet: &[u8]) -> CodecId {
    // Vorbis I, RFC 5215 §2.1: packet type 0x01, then "vorbis".
    if first_packet.len() >= 7 && first_packet[0] == 0x01 && &first_packet[1..7] == b"vorbis" {
        return CodecId::new("vorbis");
    }
    // Opus, RFC 7845 §5.1: "OpusHead".
    if first_packet.len() >= 8 && &first_packet[0..8] == b"OpusHead" {
        return CodecId::new("opus");
    }
    // FLAC-in-Ogg, https://xiph.org/flac/ogg_mapping.html: 0x7F + "FLAC".
    if first_packet.len() >= 5 && first_packet[0] == 0x7F && &first_packet[1..5] == b"FLAC" {
        return CodecId::new("flac");
    }
    // Theora: 0x80 + "theora".
    if first_packet.len() >= 7 && first_packet[0] == 0x80 && &first_packet[1..7] == b"theora" {
        return CodecId::new("theora");
    }
    // Speex: "Speex   " (8 bytes including trailing spaces).
    if first_packet.len() >= 8 && &first_packet[0..8] == b"Speex   " {
        return CodecId::new("speex");
    }
    CodecId::new("unknown")
}

/// Number of header packets a codec expects before audio/video data.
///
/// Ogg streams typically begin with one or more setup packets that don't carry
/// timestamps. The demuxer skips past them when reporting packet PTS.
///
/// For FLAC this is a *conservative* default of 1 because the precise count
/// is carried in the mapping header, not derivable from the codec id alone —
/// prefer [`header_packet_count_from_first`] when the BOS packet is in hand.
pub fn header_packet_count(id: &CodecId) -> usize {
    match id.as_str() {
        // Vorbis: identification, comment, setup.
        "vorbis" => 3,
        // Opus: head, tags.
        "opus" => 2,
        // FLAC-in-Ogg: 1 mapping packet + every metadata block (≥1 STREAMINFO).
        // We treat the mapping packet as the only "header" packet — STREAMINFO
        // and other metadata are also packets but each carries its own framing.
        // Conservative default: 1.
        "flac" => 1,
        // Theora and Speex have 3 and 2 header packets respectively.
        "theora" => 3,
        "speex" => 2,
        _ => 0,
    }
}

/// Number of header packets, derived from the logical bitstream's *first*
/// (BOS) packet when the codec encodes the count there.
///
/// FLAC-in-Ogg is the one mapping that declares its header-packet count
/// in-band. Per RFC 9639 §10.1 (`docs/audio/flac/rfc9639-flac.pdf`) the
/// first packet's bytes 7..9 hold the "Number of header packets (excluding
/// the first header packet) as an unsigned number coded big-endian"; the
/// total number of header packets is therefore `1 + that count`. A declared
/// count of `0` is the spec's explicit "unknown" marker ("The number of
/// header packets MAY be 0, which means the number of packets that follow is
/// unknown"), in which case we fall back to the conservative `1` (just the
/// mapping packet) and let per-packet metadata framing carry the rest. A
/// packet too short to reach the field also falls back to `1`.
///
/// Every other codec ignores `first` and defers to [`header_packet_count`].
pub fn header_packet_count_from_first(id: &CodecId, first: &[u8]) -> usize {
    if id.as_str() == "flac" {
        // 0x7F "FLAC" (5) + 2-byte mapping version + 2-byte header-packet
        // count (big-endian) at bytes 7..9.
        if first.len() >= 9 {
            let declared = u16::from_be_bytes([first[7], first[8]]) as usize;
            if declared > 0 {
                return 1 + declared;
            }
        }
        return 1;
    }
    header_packet_count(id)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn flac_header_count_reads_mapping_field() {
        // 0x7F "FLAC" + version 01 00 + BE count 0x0002 + "fLaC" + …
        let mut first = vec![0x7F];
        first.extend_from_slice(b"FLAC");
        first.extend_from_slice(&[0x01, 0x00]);
        first.extend_from_slice(&2u16.to_be_bytes());
        first.extend_from_slice(b"fLaC");
        let id = CodecId::new("flac");
        // 1 mapping packet + 2 declared = 3.
        assert_eq!(header_packet_count_from_first(&id, &first), 3);
    }

    #[test]
    fn flac_unknown_count_falls_back_to_one() {
        let mut first = vec![0x7F];
        first.extend_from_slice(b"FLAC");
        first.extend_from_slice(&[0x01, 0x00]);
        first.extend_from_slice(&0u16.to_be_bytes()); // 0 = unknown
        let id = CodecId::new("flac");
        assert_eq!(header_packet_count_from_first(&id, &first), 1);
    }

    #[test]
    fn flac_short_packet_falls_back_to_one() {
        let id = CodecId::new("flac");
        assert_eq!(header_packet_count_from_first(&id, b"\x7FFLAC"), 1);
    }

    #[test]
    fn non_flac_ignores_first_packet() {
        let id = CodecId::new("vorbis");
        assert_eq!(header_packet_count_from_first(&id, &[]), 3);
        let id = CodecId::new("opus");
        assert_eq!(header_packet_count_from_first(&id, &[]), 2);
    }
}
