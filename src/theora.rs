//! Theora-in-Ogg **container mapping** helpers.
//!
//! The Theora specification's Ogg encapsulation appendix
//! (`docs/video/theora/Theora.pdf`, Appendix A) makes the container
//! layer depend on a handful of identification-header fields — most
//! importantly **KFGSHIFT**, which splits every data page's granule
//! position into a keyframe half and an offset-since-keyframe half,
//! and **FRN/FRD**, the frame rate that turns granule values into
//! time. This module parses (and builds) exactly the byte-aligned
//! identification-header layout of spec §6.2 plus the granule-position
//! arithmetic of spec §A.2.3. It performs **no** Theora bitstream
//! decoding — comment and setup packets are opaque here; frame packets
//! are never inspected.
//!
//! # Granule-position packing (spec §A.2.3)
//!
//! Data packets are marked by a granule position "derived from the
//! count of decodable frames after that packet is processed". The
//! field splits at bit `KFGSHIFT`:
//!
//! * the upper `64 − KFGSHIFT` bits carry the frame count at the last
//!   keyframe, and
//! * the lower `KFGSHIFT` bits carry the number of frames since that
//!   keyframe.
//!
//! "Thus a stream would begin with a split granulepos of 1|0 (a
//! keyframe), followed by 1|1, 1|2, 1|3, etc."
//!
//! Streams older than bitstream version 3.2.1 (VREV = 0) instead mark
//! packets "by a granulepos derived from the *index* of the frame
//! being decoded, rather than the count" — i.e. the upper half starts
//! from zero. Per the spec they "can be interpreted according to the
//! description above by adding 1 to the more significant field ... when
//! VREV is less than 1"; [`TheoraGranule`] folds that difference into
//! a single `count_from_one` flag so all callers see 0-based absolute
//! frame indices regardless of stream vintage.
//!
//! The spec's mid-stream worked example ("1234|37, 1271|0 (for the
//! keyframe)") is arithmetically inconsistent with its own stream-start
//! example (1|0, 1|1, …): under the frame-count origin, the keyframe
//! following the frame marked 1234|37 (count 1271) must carry 1272|0.
//! The staged reference bitstreams in `docs/video/theora/fixtures/`
//! settle it empirically — a version 3.2.1 stream whose every frame is
//! a keyframe carries granules 1|0, 2|0, 3|0, 4|0 on consecutive
//! pages, confirming the stream-start arithmetic implemented here.

use oxideav_core::{Error, Result};

/// Byte length of the identification-header packet: 7 bytes of common
/// header (`0x80` + `"theora"`) plus the fixed field layout of spec
/// §6.2 (Figure 6.2 ends after QUAL/KFGSHIFT/PF/reserved at byte 42).
pub const ID_HEADER_LEN: usize = 42;

/// The parsed **identification header** (spec §6.2) — only fields the
/// container mapping and stream description need. All multi-byte
/// fields are stored MSB-first on the wire.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct TheoraIdHeader {
    /// Major version number (`VMAJ`, must be 3).
    pub vmaj: u8,
    /// Minor version number (`VMIN`, must be 2).
    pub vmin: u8,
    /// Version revision (`VREV`). Revision 0 streams use the older
    /// frame-*index* granule marking; ≥ 1 use the frame-*count*
    /// marking (spec §A.2.3).
    pub vrev: u8,
    /// Frame width in macro blocks (`FMBW`, > 0); pixels = `FMBW * 16`.
    pub fmbw: u16,
    /// Frame height in macro blocks (`FMBH`, > 0); pixels = `FMBH * 16`.
    pub fmbh: u16,
    /// Picture-region width in pixels (`PICW`, ≤ `FMBW * 16`).
    pub picw: u32,
    /// Picture-region height in pixels (`PICH`, ≤ `FMBH * 16`).
    pub pich: u32,
    /// X offset of the picture region (`PICX`).
    pub picx: u8,
    /// Y offset of the picture region (`PICY`).
    pub picy: u8,
    /// Frame-rate numerator (`FRN`, > 0).
    pub frn: u32,
    /// Frame-rate denominator (`FRD`, > 0). Frames are sampled at the
    /// constant rate of `FRN / FRD` frames per second; the first
    /// frame's presentation time is zero seconds.
    pub frd: u32,
    /// Pixel aspect-ratio numerator (`PARN`; 0 = unspecified).
    pub parn: u32,
    /// Pixel aspect-ratio denominator (`PARD`; 0 = unspecified).
    pub pard: u32,
    /// Color space (`CS`, spec Table 6.3).
    pub cs: u8,
    /// Nominal bitrate in bits per second (`NOMBR`; 0 = unspecified).
    pub nombr: u32,
    /// Quality hint (`QUAL`, 6 bits).
    pub qual: u8,
    /// Granule shift (`KFGSHIFT`, 5 bits): the width of the
    /// offset-since-keyframe half of every data granule position.
    pub kfgshift: u8,
    /// Pixel format (`PF`, 2 bits, spec Table 6.4): 0 = 4:2:0,
    /// 2 = 4:2:2, 3 = 4:4:4 (1 is reserved).
    pub pf: u8,
}

impl TheoraIdHeader {
    /// Parse an identification-header packet (the Theora logical
    /// stream's BOS packet).
    ///
    /// Validates only what the container mapping relies on: the common
    /// header magic, the packet length, `VMAJ`/`VMIN` = 3.2 (spec §6.2
    /// steps 2–3 — the granule semantics of §A.2.3 are defined for this
    /// version), and the MUST-be-positive `FMBW`/`FMBH`/`FRN`/`FRD`
    /// fields. Codec-level constraints with no container impact
    /// (reserved pixel format, picture-region bounds, reserved bits)
    /// are left to the codec layer.
    pub fn parse(packet: &[u8]) -> Result<Self> {
        if packet.len() < ID_HEADER_LEN {
            return Err(Error::invalid(
                "Theora identification header shorter than 42 bytes",
            ));
        }
        if packet[0] != 0x80 || &packet[1..7] != b"theora" {
            return Err(Error::invalid(
                "Theora identification header magic mismatch",
            ));
        }
        let vmaj = packet[7];
        let vmin = packet[8];
        let vrev = packet[9];
        if vmaj != 3 || vmin != 2 {
            return Err(Error::unsupported(format!(
                "Theora bitstream version {vmaj}.{vmin}.{vrev} (container mapping defined for 3.2.x)"
            )));
        }
        let fmbw = u16::from_be_bytes([packet[10], packet[11]]);
        let fmbh = u16::from_be_bytes([packet[12], packet[13]]);
        let picw = u32::from_be_bytes([0, packet[14], packet[15], packet[16]]);
        let pich = u32::from_be_bytes([0, packet[17], packet[18], packet[19]]);
        let picx = packet[20];
        let picy = packet[21];
        let frn = u32::from_be_bytes([packet[22], packet[23], packet[24], packet[25]]);
        let frd = u32::from_be_bytes([packet[26], packet[27], packet[28], packet[29]]);
        let parn = u32::from_be_bytes([0, packet[30], packet[31], packet[32]]);
        let pard = u32::from_be_bytes([0, packet[33], packet[34], packet[35]]);
        let cs = packet[36];
        let nombr = u32::from_be_bytes([0, packet[37], packet[38], packet[39]]);
        // Final 16 bits, MSB-first: QUAL (6) | KFGSHIFT (5) | PF (2) |
        // reserved (3).
        let tail = u16::from_be_bytes([packet[40], packet[41]]);
        let qual = (tail >> 10) as u8;
        let kfgshift = ((tail >> 5) & 0x1F) as u8;
        let pf = ((tail >> 3) & 0x03) as u8;
        if fmbw == 0 || fmbh == 0 {
            return Err(Error::invalid(
                "Theora identification header: FMBW/FMBH must be greater than zero",
            ));
        }
        if frn == 0 || frd == 0 {
            return Err(Error::invalid(
                "Theora identification header: FRN/FRD must be greater than zero",
            ));
        }
        Ok(Self {
            vmaj,
            vmin,
            vrev,
            fmbw,
            fmbh,
            picw,
            pich,
            picx,
            picy,
            frn,
            frd,
            parn,
            pard,
            cs,
            nombr,
            qual,
            kfgshift,
            pf,
        })
    }

    /// Serialize back into the 42-byte identification-header packet
    /// (spec §6.2 Figure 6.2). Inverse of [`parse`](Self::parse); the
    /// 3 reserved trailing bits are written as zero.
    ///
    /// Values wider than their wire field (`PICW`/`PICH`/`PARN`/
    /// `PARD`/`NOMBR` > 24 bits, `QUAL` > 6 bits, `KFGSHIFT` > 5 bits,
    /// `PF` > 2 bits) are truncated to the field width.
    #[must_use]
    pub fn to_bytes(&self) -> Vec<u8> {
        let mut out = Vec::with_capacity(ID_HEADER_LEN);
        out.push(0x80);
        out.extend_from_slice(b"theora");
        out.push(self.vmaj);
        out.push(self.vmin);
        out.push(self.vrev);
        out.extend_from_slice(&self.fmbw.to_be_bytes());
        out.extend_from_slice(&self.fmbh.to_be_bytes());
        out.extend_from_slice(&self.picw.to_be_bytes()[1..4]);
        out.extend_from_slice(&self.pich.to_be_bytes()[1..4]);
        out.push(self.picx);
        out.push(self.picy);
        out.extend_from_slice(&self.frn.to_be_bytes());
        out.extend_from_slice(&self.frd.to_be_bytes());
        out.extend_from_slice(&self.parn.to_be_bytes()[1..4]);
        out.extend_from_slice(&self.pard.to_be_bytes()[1..4]);
        out.push(self.cs);
        out.extend_from_slice(&self.nombr.to_be_bytes()[1..4]);
        let tail: u16 = ((self.qual as u16 & 0x3F) << 10)
            | ((self.kfgshift as u16 & 0x1F) << 5)
            | ((self.pf as u16 & 0x03) << 3);
        out.extend_from_slice(&tail.to_be_bytes());
        out
    }

    /// The granule-position codec for this stream: `KFGSHIFT` plus the
    /// version-dependent counting origin (spec §A.2.3 — VREV ≥ 1
    /// streams count frames from 1, VREV 0 streams index from 0).
    #[must_use]
    pub fn granule(&self) -> TheoraGranule {
        TheoraGranule {
            shift: self.kfgshift as u32,
            count_from_one: self.vrev >= 1,
        }
    }
}

/// Granule-position arithmetic for one Theora logical stream (spec
/// §A.2.3).
///
/// All public methods speak **0-based absolute frame indices** — frame
/// 0 is the stream's first frame, presented at 0 seconds — folding the
/// VREV-dependent counting origin away.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct TheoraGranule {
    /// `KFGSHIFT`: bit width of the offset-since-keyframe half.
    pub shift: u32,
    /// `true` for bitstream 3.2.1+ (VREV ≥ 1): the upper granule half
    /// holds the keyframe's frame *count* (first frame = 1). `false`
    /// for VREV 0 streams whose upper half holds the frame *index*.
    pub count_from_one: bool,
}

impl TheoraGranule {
    /// Largest representable offset-since-keyframe: `2^shift − 1`.
    #[must_use]
    pub fn max_offset(&self) -> i64 {
        if self.shift == 0 {
            0
        } else if self.shift >= 63 {
            i64::MAX
        } else {
            (1i64 << self.shift) - 1
        }
    }

    /// Split a raw granule into `(upper, offset)` halves. `None` for
    /// negative granules (the RFC 3533 §6 "no packet finishes on this
    /// page" sentinel) and for a degenerate `shift ≥ 63`.
    fn split(&self, granule: i64) -> Option<(i64, i64)> {
        if granule < 0 || self.shift >= 63 {
            return None;
        }
        if self.shift == 0 {
            return Some((granule, 0));
        }
        let g = granule as u64;
        Some((
            (g >> self.shift) as i64,
            (g & ((1u64 << self.shift) - 1)) as i64,
        ))
    }

    /// The 0-based absolute frame index of the last frame that
    /// finishes on a page carrying `granule`.
    ///
    /// `None` for negative granules, for a degenerate `shift ≥ 63`,
    /// and for a granule below the stream's counting origin (e.g. the
    /// header pages' granule 0 on a VREV ≥ 1 stream, whose first data
    /// granule is `1|0`).
    #[must_use]
    pub fn frame_index(&self, granule: i64) -> Option<i64> {
        let (upper, offset) = self.split(granule)?;
        let count_bias = i64::from(self.count_from_one);
        upper
            .checked_add(offset)?
            .checked_sub(count_bias)
            .filter(|&idx| idx >= 0)
    }

    /// The number of frames whose display interval is complete once
    /// the frame carrying `granule` has been presented — the stream
    /// duration in frames when applied to the final page's granule.
    #[must_use]
    pub fn frame_count(&self, granule: i64) -> Option<i64> {
        self.frame_index(granule)?.checked_add(1)
    }

    /// The 0-based absolute frame index of the **keyframe** the
    /// granule counts from (the seek target for the frame it marks).
    #[must_use]
    pub fn keyframe_index(&self, granule: i64) -> Option<i64> {
        let (upper, _) = self.split(granule)?;
        let count_bias = i64::from(self.count_from_one);
        upper.checked_sub(count_bias).filter(|&idx| idx >= 0)
    }

    /// Whether the frame marked by `granule` is itself a keyframe
    /// (offset-since-keyframe = 0).
    #[must_use]
    pub fn is_keyframe(&self, granule: i64) -> bool {
        matches!(self.split(granule), Some((_, 0)))
    }

    /// Pack an absolute frame index and its governing keyframe's index
    /// into a granule position.
    ///
    /// Errors when `keyframe_index > frame_index`, either index is
    /// negative, or the offset `frame_index − keyframe_index` exceeds
    /// [`max_offset`](Self::max_offset) (the encoder must emit a new
    /// keyframe before the offset half overflows).
    pub fn pack(&self, frame_index: i64, keyframe_index: i64) -> Result<i64> {
        if frame_index < 0 || keyframe_index < 0 || keyframe_index > frame_index {
            return Err(Error::invalid(format!(
                "Theora granule pack: invalid frame/keyframe pair ({frame_index}, {keyframe_index})"
            )));
        }
        let offset = frame_index - keyframe_index;
        if offset > self.max_offset() {
            return Err(Error::invalid(format!(
                "Theora granule pack: {offset} frames since keyframe exceeds the \
                 2^KFGSHIFT-1 = {} capacity of the offset field",
                self.max_offset()
            )));
        }
        let upper = keyframe_index + i64::from(self.count_from_one);
        if self.shift == 0 {
            // Degenerate shift: the whole field is the frame half. Only
            // self-keyed frames are representable (offset is always 0
            // here because max_offset() == 0 rejected everything else).
            return Ok(upper + offset);
        }
        if self.shift >= 63 {
            return Err(Error::invalid(
                "Theora granule pack: KFGSHIFT >= 63 leaves no room for the keyframe half",
            ));
        }
        upper
            .checked_shl(self.shift)
            .filter(|&v| v >= 0 && (v >> self.shift) == upper)
            .map(|v| v | offset)
            .ok_or_else(|| {
                Error::invalid("Theora granule pack: keyframe index overflows the granule field")
            })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn id_header(vrev: u8, kfgshift: u8, frn: u32, frd: u32) -> TheoraIdHeader {
        TheoraIdHeader {
            vmaj: 3,
            vmin: 2,
            vrev,
            fmbw: 20,
            fmbh: 15,
            picw: 320,
            pich: 240,
            picx: 0,
            picy: 0,
            frn,
            frd,
            parn: 0,
            pard: 0,
            cs: 0,
            nombr: 0,
            qual: 32,
            kfgshift,
            pf: 0,
        }
    }

    #[test]
    fn id_header_round_trips() {
        let h = TheoraIdHeader {
            vmaj: 3,
            vmin: 2,
            vrev: 1,
            fmbw: 120,
            fmbh: 68,
            picw: 1920,
            pich: 1080,
            picx: 0,
            picy: 8,
            frn: 30_000,
            frd: 1001,
            parn: 1,
            pard: 1,
            cs: 1,
            nombr: 2_000_000,
            qual: 63,
            kfgshift: 31,
            pf: 3,
        };
        let bytes = h.to_bytes();
        assert_eq!(bytes.len(), ID_HEADER_LEN);
        assert_eq!(TheoraIdHeader::parse(&bytes).unwrap(), h);
    }

    #[test]
    fn parse_rejects_bad_magic_version_and_zero_fields() {
        let good = id_header(1, 6, 30, 1).to_bytes();
        assert!(TheoraIdHeader::parse(&good).is_ok());
        // Truncated.
        assert!(TheoraIdHeader::parse(&good[..41]).is_err());
        // Magic.
        let mut bad = good.clone();
        bad[1] = b'T';
        assert!(TheoraIdHeader::parse(&bad).is_err());
        // VMAJ.
        let mut bad = good.clone();
        bad[7] = 2;
        assert!(TheoraIdHeader::parse(&bad).is_err());
        // VMIN.
        let mut bad = good.clone();
        bad[8] = 3;
        assert!(TheoraIdHeader::parse(&bad).is_err());
        // FMBW = 0.
        let mut bad = good.clone();
        bad[10] = 0;
        bad[11] = 0;
        assert!(TheoraIdHeader::parse(&bad).is_err());
        // FRD = 0.
        let mut bad = good.clone();
        bad[26..30].fill(0);
        assert!(TheoraIdHeader::parse(&bad).is_err());
    }

    #[test]
    fn spec_a23_stream_start_sequence_vrev1() {
        // §A.2.3: "a stream would begin with a split granulepos of 1|0
        // (a keyframe), followed by 1|1, 1|2, 1|3".
        let g = id_header(1, 6, 30, 1).granule();
        assert_eq!(g.pack(0, 0).unwrap(), 1 << 6); // 1|0
        assert_eq!(g.pack(1, 0).unwrap(), (1 << 6) | 1);
        assert_eq!(g.pack(2, 0).unwrap(), (1 << 6) | 2);
        assert_eq!(g.pack(3, 0).unwrap(), (1 << 6) | 3);
        // And back.
        assert_eq!(g.frame_index(1 << 6), Some(0)); // 1|0
        assert_eq!(g.frame_index((1 << 6) | 3), Some(3));
        assert!(g.is_keyframe(1 << 6)); // 1|0
        assert!(!g.is_keyframe((1 << 6) | 3));
        assert_eq!(g.keyframe_index((1 << 6) | 3), Some(0));
        // Header pages carry granule 0, which is below the VREV>=1
        // counting origin — not a frame.
        assert_eq!(g.frame_index(0), None);
        // The -1 sentinel is not a frame either.
        assert_eq!(g.frame_index(-1), None);
    }

    #[test]
    fn spec_a23_mid_stream_keyframe_vrev1() {
        // §A.2.3's mid-stream shape: ...1234|35, 1234|36, 1234|37, then
        // a keyframe restarts the offset at 0. Under the frame-count
        // origin, upper 1234 = keyframe index 1233; 1234|37 marks frame
        // index 1233 + 37 = 1270; the next frame (1271) is a keyframe
        // with granule 1272|0.
        let g = id_header(1, 6, 30, 1).granule();
        let inter = (1234 << 6) | 37;
        assert_eq!(g.frame_index(inter), Some(1270));
        assert_eq!(g.keyframe_index(inter), Some(1233));
        assert!(!g.is_keyframe(inter));
        let kf = g.pack(1271, 1271).unwrap();
        assert_eq!(kf, 1272 << 6);
        assert!(g.is_keyframe(kf));
        assert!(kf > inter, "granule positions increase monotonically");
        assert_eq!(g.frame_index(kf), Some(1271));
    }

    #[test]
    fn vrev0_streams_index_from_zero() {
        // §A.2.3: prior to 3.2.1 the upper half is the frame *index*;
        // "adding 1 to the more significant field ... when VREV is less
        // than 1" recovers the count interpretation. Our 0-based frame
        // index is therefore the raw sum for VREV 0.
        let g = id_header(0, 6, 30, 1).granule();
        assert_eq!(g.pack(0, 0).unwrap(), 0);
        assert_eq!(g.pack(5, 0).unwrap(), 5);
        assert_eq!(g.pack(64, 64).unwrap(), 64 << 6);
        assert_eq!(g.frame_index(0), Some(0));
        assert_eq!(g.frame_index((64 << 6) | 3), Some(67));
        assert_eq!(g.keyframe_index((64 << 6) | 3), Some(64));
        assert!(g.is_keyframe(64 << 6));
    }

    #[test]
    fn pack_rejects_offset_overflow_and_bad_pairs() {
        let g = id_header(1, 4, 30, 1).granule(); // max_offset = 15
        assert_eq!(g.max_offset(), 15);
        assert!(g.pack(15, 0).is_ok());
        assert!(g.pack(16, 0).is_err(), "offset 16 overflows 4 bits");
        assert!(g.pack(3, 5).is_err(), "keyframe after frame");
        assert!(g.pack(-1, -1).is_err());
    }

    #[test]
    fn pack_and_unpack_are_inverse_across_versions() {
        for vrev in [0u8, 1, 2] {
            for shift in [1u8, 6, 15, 31] {
                let g = id_header(vrev, shift, 25, 1).granule();
                for (n, k) in [(0i64, 0i64), (1, 0), (100, 99), (5000, 5000)] {
                    if n - k > g.max_offset() {
                        continue;
                    }
                    let gr = g.pack(n, k).unwrap();
                    assert_eq!(g.frame_index(gr), Some(n), "vrev={vrev} shift={shift}");
                    assert_eq!(g.keyframe_index(gr), Some(k));
                    assert_eq!(g.is_keyframe(gr), n == k);
                }
            }
        }
    }

    #[test]
    fn granule_values_monotonic_in_frame_index() {
        // Ogg requires monotonically increasing granule positions; the
        // packing preserves frame order for any keyframe placement.
        let g = id_header(1, 6, 30, 1).granule();
        let mut prev = -1i64;
        let mut kf = 0i64;
        for n in 0..200i64 {
            if n % 45 == 0 {
                kf = n;
            }
            let gr = g.pack(n, kf).unwrap();
            assert!(gr > prev, "frame {n}: granule {gr} <= previous {prev}");
            prev = gr;
        }
    }
}
