//! Ogg Skeleton metadata bitstream — versions 3.0 and 4.0.
//!
//! Skeleton is a logical bitstream that describes the *other* logical
//! bitstreams in an Ogg physical stream. Its packets all live in the
//! header pages: a `fishead\0` BOS ident packet, one `fisbone\0`
//! secondary header per content track, optional 4.0 `index\0` keyframe
//! index packets, and an empty EOS packet that closes the control
//! section before any content pages appear.
//!
//! All on-wire integers are little-endian. Rational numbers (presentation
//! time, basetime, granule rate, timestamps) are split into 64-bit
//! numerator / denominator pairs.
//!
//! Reference: `docs/container/ogg/ogg-skeleton-3.0.md`,
//! `docs/container/ogg/ogg-skeleton-4.0.md`,
//! `docs/container/ogg/ogg-skeleton-message-headers.wiki`.

use oxideav_core::{Error, Result};

/// `fishead\0` magic at the start of every Skeleton BOS ident packet.
pub const FISHEAD_MAGIC: &[u8; 8] = b"fishead\0";

/// `fisbone\0` magic at the start of every Skeleton secondary header packet.
pub const FISBONE_MAGIC: &[u8; 8] = b"fisbone\0";

/// `index\0` magic at the start of every Skeleton 4.0 keyframe index packet.
/// The on-wire identifier is 6 bytes (the trailing NUL counts).
pub const INDEX_MAGIC: &[u8; 6] = b"index\0";

/// Size of the 3.0 `fishead` packet (bytes 0..64).
pub const FISHEAD_LEN_3_0: usize = 64;

/// Size of the 4.0 `fishead` packet (bytes 0..80). 4.0 adds the
/// *Segment length in bytes* (64..72) and *Content byte offset* (72..80)
/// fields on top of the 3.0 layout.
pub const FISHEAD_LEN_4_0: usize = 80;

/// Fixed prefix size of a `fisbone` packet (bytes 0..52) before the
/// trailing HTTP-style message header fields.
pub const FISBONE_FIXED_LEN: usize = 52;

/// Standard offset from the start of a `fisbone` packet to the first
/// HTTP-style message header byte. The packet stores this value at
/// bytes 8..12 (little-endian u32) for forward compatibility; encoders
/// emitting 4.0 fisbones write it verbatim.
pub const FISBONE_MSG_HEADER_OFFSET: u32 = (FISBONE_FIXED_LEN - 8) as u32;

/// Skeleton version on the wire. The two version fields in a `fishead`
/// header are a `(major, minor)` pair of 16-bit little-endian integers
/// at bytes 8..12.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Version {
    pub major: u16,
    pub minor: u16,
}

impl Version {
    pub const V3_0: Version = Version { major: 3, minor: 0 };
    pub const V4_0: Version = Version { major: 4, minor: 0 };

    /// True if this version is at least `other`. Used to gate
    /// 4.0-specific fields (segment length, content byte offset,
    /// keyframe index packets) when reading 3.0 streams.
    pub fn at_least(self, other: Version) -> bool {
        (self.major, self.minor) >= (other.major, other.minor)
    }
}

/// A rational number `(numerator, denominator)` stored on the wire as
/// two consecutive 64-bit little-endian integers. The Skeleton spec
/// uses these for presentation time, basetime, granule rate, and
/// per-keypoint timestamps.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Default)]
pub struct Rational {
    pub numerator: i64,
    pub denominator: i64,
}

impl Rational {
    pub const fn new(numerator: i64, denominator: i64) -> Self {
        Self {
            numerator,
            denominator,
        }
    }

    /// Convert to a floating-point seconds value. Returns 0.0 if the
    /// denominator is zero (Skeleton 4.0 §"Keyframe index packets":
    /// "If the denominator is 0 for the first-sample-time or the
    /// last-sample-time, then that value was unable to be determined
    /// at indexing time, and is unknown.").
    pub fn to_seconds(self) -> f64 {
        if self.denominator == 0 {
            0.0
        } else {
            self.numerator as f64 / self.denominator as f64
        }
    }
}

/// `fishead` ident header packet (Skeleton 3.0 + 4.0 layout).
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct FisHead {
    /// Skeleton version. 3.0 omits the segment-length / content-byte-offset
    /// fields; 4.0 carries them.
    pub version: Version,
    /// Presentation time. The cut-in time all logical bitstreams are meant
    /// to start presenting from. Stored as a rational in seconds.
    pub presentation_time: Rational,
    /// Basetime. Maps granule position 0 to a playback time (e.g.
    /// professional-video content that starts at 01:00:00).
    pub basetime: Rational,
    /// UTC. Bytes 44..63 are a 20-byte ASCII timestamp slot that maps
    /// granule position 0 to a wall-clock time. The convention in
    /// existing Skeleton 3.0 files is `YYYYMMDDTHHMMSS.sssZ`
    /// (ISO 8601 basic format) but the spec does not mandate it, so we
    /// surface the raw 20-byte slot for callers that need a verbatim
    /// passthrough. Trailing NULs are stripped on parse.
    pub utc: [u8; 20],
    /// Segment length in bytes (4.0 only). Length of the indexed Ogg
    /// segment, used to validate that the index is still in sync with
    /// the file. `None` for 3.0 headers.
    pub segment_length: Option<u64>,
    /// Content byte offset (4.0 only). Offset of the first non-header
    /// page in the Ogg segment. `None` for 3.0 headers.
    pub content_byte_offset: Option<u64>,
}

impl FisHead {
    /// Build a minimal 4.0 fishead with the supplied version + presentation
    /// time, leaving every other field at its empty / zero default.
    pub fn new(version: Version) -> Self {
        let (segment_length, content_byte_offset) = if version.at_least(Version::V4_0) {
            (Some(0), Some(0))
        } else {
            (None, None)
        };
        Self {
            version,
            presentation_time: Rational::default(),
            basetime: Rational::default(),
            utc: [0u8; 20],
            segment_length,
            content_byte_offset,
        }
    }

    /// Parse a `fishead` packet. The packet is the full Skeleton BOS
    /// payload (`fishead\0` magic + header body). Accepts both 3.0
    /// (64-byte) and 4.0 (80-byte) layouts.
    pub fn parse(packet: &[u8]) -> Result<Self> {
        if packet.len() < FISHEAD_LEN_3_0 {
            return Err(Error::invalid(format!(
                "Skeleton fishead too short: {} bytes (need at least {})",
                packet.len(),
                FISHEAD_LEN_3_0
            )));
        }
        if &packet[0..8] != FISHEAD_MAGIC {
            return Err(Error::invalid(
                "Skeleton fishead missing 'fishead\\0' magic",
            ));
        }
        let major = u16::from_le_bytes([packet[8], packet[9]]);
        let minor = u16::from_le_bytes([packet[10], packet[11]]);
        let version = Version { major, minor };

        let pt_num = i64::from_le_bytes(packet[12..20].try_into().expect("8 bytes"));
        let pt_den = i64::from_le_bytes(packet[20..28].try_into().expect("8 bytes"));
        let bt_num = i64::from_le_bytes(packet[28..36].try_into().expect("8 bytes"));
        let bt_den = i64::from_le_bytes(packet[36..44].try_into().expect("8 bytes"));
        let mut utc = [0u8; 20];
        utc.copy_from_slice(&packet[44..64]);

        let (segment_length, content_byte_offset) = if version.at_least(Version::V4_0) {
            if packet.len() < FISHEAD_LEN_4_0 {
                return Err(Error::invalid(format!(
                    "Skeleton 4.0 fishead too short: {} bytes (need {})",
                    packet.len(),
                    FISHEAD_LEN_4_0
                )));
            }
            let seg = u64::from_le_bytes(packet[64..72].try_into().expect("8 bytes"));
            let off = u64::from_le_bytes(packet[72..80].try_into().expect("8 bytes"));
            (Some(seg), Some(off))
        } else {
            (None, None)
        };

        Ok(Self {
            version,
            presentation_time: Rational::new(pt_num, pt_den),
            basetime: Rational::new(bt_num, bt_den),
            utc,
            segment_length,
            content_byte_offset,
        })
    }

    /// Serialize this `fishead` to a packet ready to wrap in an Ogg
    /// BOS page. Emits the 64-byte 3.0 layout or the 80-byte 4.0 layout
    /// based on `self.version`.
    pub fn to_bytes(&self) -> Vec<u8> {
        let len = if self.version.at_least(Version::V4_0) {
            FISHEAD_LEN_4_0
        } else {
            FISHEAD_LEN_3_0
        };
        let mut out = Vec::with_capacity(len);
        out.extend_from_slice(FISHEAD_MAGIC);
        out.extend_from_slice(&self.version.major.to_le_bytes());
        out.extend_from_slice(&self.version.minor.to_le_bytes());
        out.extend_from_slice(&self.presentation_time.numerator.to_le_bytes());
        out.extend_from_slice(&self.presentation_time.denominator.to_le_bytes());
        out.extend_from_slice(&self.basetime.numerator.to_le_bytes());
        out.extend_from_slice(&self.basetime.denominator.to_le_bytes());
        out.extend_from_slice(&self.utc);
        if self.version.at_least(Version::V4_0) {
            let seg = self.segment_length.unwrap_or(0);
            let off = self.content_byte_offset.unwrap_or(0);
            out.extend_from_slice(&seg.to_le_bytes());
            out.extend_from_slice(&off.to_le_bytes());
        }
        debug_assert_eq!(out.len(), len);
        out
    }
}

/// One HTTP-style message header field carried inside a `fisbone` packet.
///
/// The Skeleton spec defines three compulsory headers in 4.0: `Content-Type`
/// (the codec MIME type), `Role` (the function of the track, e.g.
/// `video/main` or `audio/main`), and `Name` (a free-text identifier).
/// Many others are defined in
/// `docs/container/ogg/ogg-skeleton-message-headers.wiki`.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct MessageHeader {
    /// Field name, e.g. `Content-Type`. Stored as-written; lookup is
    /// case-insensitive per HTTP convention.
    pub name: String,
    /// Field value, e.g. `audio/vorbis`.
    pub value: String,
}

impl MessageHeader {
    pub fn new<N: Into<String>, V: Into<String>>(name: N, value: V) -> Self {
        Self {
            name: name.into(),
            value: value.into(),
        }
    }
}

/// `fisbone` secondary header packet describing one logical bitstream.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct FisBone {
    /// `bitstream_serial_number` of the content track this fisbone
    /// describes (RFC 3533 §6 field 5). Matches the BOS page of the
    /// referenced Vorbis/Theora/etc. stream.
    pub serial: u32,
    /// Number of header packets the referenced codec emits before any
    /// content packets (Vorbis 3, Opus 2, …). Surfaced verbatim from
    /// `fisbone` bytes 16..20.
    pub num_headers: u32,
    /// Granule rate, expressed as a rational in Hz (samples per second
    /// for audio; frames per second for video).
    pub granule_rate: Rational,
    /// Basegranule. Granule number this logical bitstream starts at in
    /// the (possibly remuxed) stream — provides the accurate start time
    /// of the first data packet.
    pub basegranule: i64,
    /// Preroll. Number of past content packets a decoder must consume
    /// before delivering output for seeking purposes (Vorbis 2, Speex
    /// 3, …).
    pub preroll: u32,
    /// Granuleshift. Number of low bits of `granulepos` reserved for
    /// sub-seekable units (e.g. Theora's keyframe shift).
    pub granuleshift: u8,
    /// HTTP-style message header fields. Compulsory ones in 4.0 are
    /// `Content-Type`, `Role`, `Name`.
    pub headers: Vec<MessageHeader>,
}

impl FisBone {
    /// Build a fisbone with the minimum required state. Callers append
    /// `Content-Type` / `Role` / `Name` via [`Self::set_header`].
    pub fn new(serial: u32, granule_rate: Rational) -> Self {
        Self {
            serial,
            num_headers: 0,
            granule_rate,
            basegranule: 0,
            preroll: 0,
            granuleshift: 0,
            headers: Vec::new(),
        }
    }

    /// Replace the value of an existing case-insensitively-matched
    /// header, or append a new one.
    pub fn set_header<N: Into<String>, V: Into<String>>(&mut self, name: N, value: V) {
        let name = name.into();
        let value = value.into();
        if let Some(h) = self
            .headers
            .iter_mut()
            .find(|h| h.name.eq_ignore_ascii_case(&name))
        {
            h.value = value;
        } else {
            self.headers.push(MessageHeader::new(name, value));
        }
    }

    /// Look up a header value by case-insensitive name.
    pub fn header(&self, name: &str) -> Option<&str> {
        self.headers
            .iter()
            .find(|h| h.name.eq_ignore_ascii_case(name))
            .map(|h| h.value.as_str())
    }

    /// Parse a `fisbone` packet (the full Skeleton secondary header
    /// payload, starting with `fisbone\0`).
    pub fn parse(packet: &[u8]) -> Result<Self> {
        if packet.len() < FISBONE_FIXED_LEN {
            return Err(Error::invalid(format!(
                "Skeleton fisbone too short: {} bytes (need at least {})",
                packet.len(),
                FISBONE_FIXED_LEN
            )));
        }
        if &packet[0..8] != FISBONE_MAGIC {
            return Err(Error::invalid(
                "Skeleton fisbone missing 'fisbone\\0' magic",
            ));
        }
        let msg_off_field = u32::from_le_bytes(packet[8..12].try_into().expect("4 bytes"));
        // The on-wire field is measured from byte 8 (the field's own
        // location) onwards: the standard value of `FISBONE_MSG_HEADER_OFFSET`
        // (= 44) lines up the first message-header byte with byte 52 of
        // the packet. We allow any value as long as the resulting offset
        // sits past the fixed prefix and inside the packet.
        let msg_start = 8usize.saturating_add(msg_off_field as usize);
        if msg_start < FISBONE_FIXED_LEN || msg_start > packet.len() {
            return Err(Error::invalid(format!(
                "Skeleton fisbone: message-header offset {msg_off_field} out of range"
            )));
        }
        let serial = u32::from_le_bytes(packet[12..16].try_into().expect("4 bytes"));
        let num_headers = u32::from_le_bytes(packet[16..20].try_into().expect("4 bytes"));
        let gr_num = i64::from_le_bytes(packet[20..28].try_into().expect("8 bytes"));
        let gr_den = i64::from_le_bytes(packet[28..36].try_into().expect("8 bytes"));
        let basegranule = i64::from_le_bytes(packet[36..44].try_into().expect("8 bytes"));
        let preroll = u32::from_le_bytes(packet[44..48].try_into().expect("4 bytes"));
        let granuleshift = packet[48];
        // Bytes 49..52 are documented as "padding/future use" and ignored.

        let headers = parse_message_headers(&packet[msg_start..]);

        Ok(Self {
            serial,
            num_headers,
            granule_rate: Rational::new(gr_num, gr_den),
            basegranule,
            preroll,
            granuleshift,
            headers,
        })
    }

    /// Serialize this `fisbone` to a packet. The message-header offset
    /// is fixed at the standard 44 (placing the first header byte at
    /// packet offset 52) and headers are emitted in registration order,
    /// each terminated by CRLF (`\r\n`) per the 4.0 spec.
    pub fn to_bytes(&self) -> Vec<u8> {
        let mut out = Vec::with_capacity(FISBONE_FIXED_LEN + 64);
        out.extend_from_slice(FISBONE_MAGIC);
        out.extend_from_slice(&FISBONE_MSG_HEADER_OFFSET.to_le_bytes());
        out.extend_from_slice(&self.serial.to_le_bytes());
        out.extend_from_slice(&self.num_headers.to_le_bytes());
        out.extend_from_slice(&self.granule_rate.numerator.to_le_bytes());
        out.extend_from_slice(&self.granule_rate.denominator.to_le_bytes());
        out.extend_from_slice(&self.basegranule.to_le_bytes());
        out.extend_from_slice(&self.preroll.to_le_bytes());
        out.push(self.granuleshift);
        out.extend_from_slice(&[0u8; 3]); // padding / future use
        debug_assert_eq!(out.len(), FISBONE_FIXED_LEN);
        for h in &self.headers {
            out.extend_from_slice(h.name.as_bytes());
            out.extend_from_slice(b": ");
            out.extend_from_slice(h.value.as_bytes());
            out.extend_from_slice(b"\r\n");
        }
        out
    }
}

/// Parse a CRLF-delimited block of HTTP-style message headers into a
/// `Vec<MessageHeader>`. Lines without a `:` separator are silently
/// skipped — they cannot be reconstructed as `(name, value)` pairs.
/// Surrounding whitespace on values is trimmed; names are left intact
/// (case-insensitive lookup happens at [`FisBone::header`] time).
fn parse_message_headers(buf: &[u8]) -> Vec<MessageHeader> {
    let mut out = Vec::new();
    for line in buf.split(|&b| b == b'\n') {
        let line = if line.last() == Some(&b'\r') {
            &line[..line.len() - 1]
        } else {
            line
        };
        if line.is_empty() {
            continue;
        }
        let Some(colon) = line.iter().position(|&b| b == b':') else {
            continue;
        };
        let name = String::from_utf8_lossy(&line[..colon]).trim().to_string();
        let value_bytes = &line[colon + 1..];
        // Strip a single leading space after the colon if present —
        // standard HTTP framing.
        let value_bytes = if value_bytes.first() == Some(&b' ') {
            &value_bytes[1..]
        } else {
            value_bytes
        };
        let value = String::from_utf8_lossy(value_bytes).trim().to_string();
        if name.is_empty() {
            continue;
        }
        out.push(MessageHeader::new(name, value));
    }
    out
}

/// One entry in a Skeleton 4.0 keyframe index.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct KeyPoint {
    /// Absolute byte offset of the indexed page from the start of the
    /// Ogg segment. The on-wire encoding stores this as a delta from
    /// the previous keypoint; [`SkelIndex::parse`] reconstructs the
    /// absolute value, and [`SkelIndex::to_bytes`] reverses it.
    pub offset: u64,
    /// Presentation-time numerator, also reconstructed from the
    /// on-wire delta encoding. Divide by [`SkelIndex::timestamp_denominator`]
    /// to recover seconds.
    pub timestamp: i64,
}

/// Skeleton 4.0 keyframe index packet (`index\0`).
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct SkelIndex {
    /// `bitstream_serial_number` of the content stream this index applies to.
    pub serial: u32,
    /// Denominator shared by every timestamp (first sample, last sample,
    /// and per-keypoint values) in this index. Must be non-zero.
    pub timestamp_denominator: i64,
    /// Presentation-time numerator of the first sample in the indexed
    /// stream. Combined with `timestamp_denominator` to recover seconds.
    pub first_sample_time: i64,
    /// End-time numerator of the last sample in the indexed stream.
    /// Combined with `timestamp_denominator` to recover seconds.
    pub last_sample_time: i64,
    /// Key points in increasing-offset order. Increasing-offset implies
    /// increasing-timestamp per the spec ("The key points are stored in
    /// increasing order by offset (and thus by presentation time as
    /// well).").
    pub keypoints: Vec<KeyPoint>,
}

impl SkelIndex {
    /// Build an empty index for the given serial and timestamp denominator.
    pub fn new(serial: u32, timestamp_denominator: i64) -> Self {
        Self {
            serial,
            timestamp_denominator,
            first_sample_time: 0,
            last_sample_time: 0,
            keypoints: Vec::new(),
        }
    }

    /// Parse an `index\0` packet. The packet layout per the 4.0 spec is:
    ///
    /// * bytes 0..6: `"index\0"` identifier;
    /// * bytes 6..10: serial number (u32 LE);
    /// * bytes 10..18: number of keypoints (u64 LE);
    /// * bytes 18..26: timestamp denominator (i64 LE);
    /// * bytes 26..34: first-sample-time numerator (i64 LE);
    /// * bytes 34..42: last-sample-time numerator (i64 LE);
    /// * bytes 42..: keypoints, each = (offset-delta vbi, timestamp-delta vbi).
    pub fn parse(packet: &[u8]) -> Result<Self> {
        const PREFIX: usize = 42;
        if packet.len() < PREFIX {
            return Err(Error::invalid(format!(
                "Skeleton index packet too short: {} bytes (need at least {})",
                packet.len(),
                PREFIX
            )));
        }
        if &packet[0..6] != INDEX_MAGIC {
            return Err(Error::invalid(
                "Skeleton index packet missing 'index\\0' magic",
            ));
        }
        let serial = u32::from_le_bytes(packet[6..10].try_into().expect("4 bytes"));
        let n_keypoints = u64::from_le_bytes(packet[10..18].try_into().expect("8 bytes"));
        let timestamp_denominator = i64::from_le_bytes(packet[18..26].try_into().expect("8 bytes"));
        let first_sample_time = i64::from_le_bytes(packet[26..34].try_into().expect("8 bytes"));
        let last_sample_time = i64::from_le_bytes(packet[34..42].try_into().expect("8 bytes"));

        let mut keypoints = Vec::with_capacity(n_keypoints.min(u32::MAX as u64) as usize);
        let mut cursor = PREFIX;
        let mut abs_offset: u64 = 0;
        let mut abs_timestamp: i64 = 0;
        for _ in 0..n_keypoints {
            let (off_delta, n1) = read_vbi_u64(&packet[cursor..])
                .ok_or_else(|| Error::invalid("Skeleton index: truncated keypoint offset-delta"))?;
            cursor += n1;
            let (ts_delta, n2) = read_vbi_u64(&packet[cursor..]).ok_or_else(|| {
                Error::invalid("Skeleton index: truncated keypoint timestamp-delta")
            })?;
            cursor += n2;
            abs_offset = abs_offset
                .checked_add(off_delta)
                .ok_or_else(|| Error::invalid("Skeleton index: keypoint offset overflowed u64"))?;
            // Timestamp deltas are unsigned-encoded but the running
            // total is i64 (timestamps may be negative for streams
            // whose `presentation_time` predates granule 0). Accumulate
            // modulo 2^64 via wrapping_add against the signed running
            // value's bit pattern.
            abs_timestamp = abs_timestamp.wrapping_add(ts_delta as i64);
            keypoints.push(KeyPoint {
                offset: abs_offset,
                timestamp: abs_timestamp,
            });
        }

        Ok(Self {
            serial,
            timestamp_denominator,
            first_sample_time,
            last_sample_time,
            keypoints,
        })
    }

    /// Serialize this index packet. Keypoint offsets and timestamps are
    /// re-deltified relative to the previous entry's running total.
    pub fn to_bytes(&self) -> Vec<u8> {
        let mut out = Vec::with_capacity(42 + self.keypoints.len() * 4);
        out.extend_from_slice(INDEX_MAGIC);
        out.extend_from_slice(&self.serial.to_le_bytes());
        out.extend_from_slice(&(self.keypoints.len() as u64).to_le_bytes());
        out.extend_from_slice(&self.timestamp_denominator.to_le_bytes());
        out.extend_from_slice(&self.first_sample_time.to_le_bytes());
        out.extend_from_slice(&self.last_sample_time.to_le_bytes());

        let mut prev_offset: u64 = 0;
        let mut prev_timestamp: i64 = 0;
        for kp in &self.keypoints {
            let off_delta = kp.offset.saturating_sub(prev_offset);
            let ts_delta = kp.timestamp.wrapping_sub(prev_timestamp) as u64;
            write_vbi_u64(&mut out, off_delta);
            write_vbi_u64(&mut out, ts_delta);
            prev_offset = kp.offset;
            prev_timestamp = kp.timestamp;
        }
        out
    }

    /// Insert a `(offset, timestamp)` keypoint, keeping the per-spec
    /// invariant that keypoints are sorted by increasing offset.
    pub fn push(&mut self, offset: u64, timestamp: i64) {
        self.keypoints.push(KeyPoint { offset, timestamp });
    }
}

/// Variable-byte integer encoder used by Skeleton 4.0 index keypoint
/// offsets and timestamps. Encodes `n` little-endian in 7-bit chunks;
/// the high bit is set on the *last* byte. Always emits at least one byte.
pub fn write_vbi_u64(out: &mut Vec<u8>, mut n: u64) {
    loop {
        let chunk = (n & 0x7F) as u8;
        n >>= 7;
        if n == 0 {
            out.push(chunk | 0x80);
            return;
        } else {
            out.push(chunk);
        }
    }
}

/// Decode a Skeleton 4.0 variable-byte integer from the start of `buf`.
/// Returns the decoded value and the number of bytes consumed, or
/// `None` if `buf` is empty or the terminator byte never arrives.
/// Bytes past the 10th are not accepted (a 7-bit-per-byte 64-bit
/// integer needs at most 10 bytes).
pub fn read_vbi_u64(buf: &[u8]) -> Option<(u64, usize)> {
    let mut value: u64 = 0;
    for (i, &b) in buf.iter().enumerate() {
        if i >= 10 {
            return None;
        }
        let chunk = (b & 0x7F) as u64;
        value |= chunk << (7 * i);
        if b & 0x80 != 0 {
            return Some((value, i + 1));
        }
    }
    None
}

/// Aggregate Skeleton state collected from an Ogg physical stream.
///
/// Built by the demuxer when it sees a `fishead\0` BOS page; populated
/// further as `fisbone\0` secondary headers and 4.0 `index\0` packets
/// arrive. The same struct is what an encoder hands to the muxer when
/// it wants Skeleton emitted alongside the content streams.
#[derive(Clone, Debug, Default)]
pub struct Skeleton {
    /// The Skeleton BOS ident packet. `None` until parsed.
    pub head: Option<FisHead>,
    /// All `fisbone` secondary header packets in their on-wire order.
    pub bones: Vec<FisBone>,
    /// All 4.0 `index` packets in their on-wire order. Empty for 3.0
    /// streams or 4.0 streams that omit the index.
    pub indexes: Vec<SkelIndex>,
    /// `bitstream_serial_number` of the Skeleton logical bitstream
    /// itself. Set when the demuxer parses the BOS page. `None` if
    /// Skeleton was constructed for encoding without a chosen serial
    /// yet — pass a serial at mux time in that case.
    pub serial: Option<u32>,
}

impl Skeleton {
    /// Construct an empty Skeleton ready to be populated.
    pub fn new() -> Self {
        Self::default()
    }

    /// True if a `fishead\0` ident packet has been recorded.
    pub fn is_parsed(&self) -> bool {
        self.head.is_some()
    }

    /// Skeleton version, defaulting to 4.0 if no fishead has been
    /// recorded yet (so encoders that haven't called `set_head` still
    /// emit a 4.0 BOS).
    pub fn version(&self) -> Version {
        self.head
            .as_ref()
            .map(|h| h.version)
            .unwrap_or(Version::V4_0)
    }

    /// Set the fishead ident packet on this Skeleton, replacing any
    /// previously-recorded one.
    pub fn set_head(&mut self, head: FisHead) {
        self.head = Some(head);
    }

    /// Append a fisbone secondary header.
    pub fn push_bone(&mut self, bone: FisBone) {
        self.bones.push(bone);
    }

    /// Append a 4.0 keyframe index packet.
    pub fn push_index(&mut self, index: SkelIndex) {
        self.indexes.push(index);
    }

    /// Look up the fisbone describing the content stream with the
    /// given serial.
    pub fn bone_for_serial(&self, serial: u32) -> Option<&FisBone> {
        self.bones.iter().find(|b| b.serial == serial)
    }

    /// Look up the keyframe index for the content stream with the
    /// given serial.
    pub fn index_for_serial(&self, serial: u32) -> Option<&SkelIndex> {
        self.indexes.iter().find(|i| i.serial == serial)
    }
}

/// True if `packet` is a Skeleton BOS ident packet (starts with
/// `fishead\0`). Used by the demuxer to flag the Skeleton stream when
/// it walks BOS pages.
pub fn is_fishead(packet: &[u8]) -> bool {
    packet.len() >= 8 && &packet[0..8] == FISHEAD_MAGIC
}

/// True if `packet` is a Skeleton secondary header (starts with
/// `fisbone\0`).
pub fn is_fisbone(packet: &[u8]) -> bool {
    packet.len() >= 8 && &packet[0..8] == FISBONE_MAGIC
}

/// True if `packet` is a Skeleton 4.0 keyframe index packet (starts
/// with `index\0`).
pub fn is_index(packet: &[u8]) -> bool {
    packet.len() >= 6 && &packet[0..6] == INDEX_MAGIC
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn vbi_round_trip_small() {
        for n in [0u64, 1, 0x7F, 0x80, 0xFF, 0x3FFF, 0x4000, 7843, u64::MAX] {
            let mut buf = Vec::new();
            write_vbi_u64(&mut buf, n);
            let (decoded, used) = read_vbi_u64(&buf).expect("decode");
            assert_eq!(decoded, n);
            assert_eq!(used, buf.len());
        }
    }

    #[test]
    fn vbi_spec_example_7843() {
        // Skeleton 4.0 §"Keyframe index packets" — worked example:
        //   integer 7843 (0001 1110 1010 0011) encodes as 0x23, 0xBD
        //   (low 7 bits first, high bit set on the terminator byte).
        let mut buf = Vec::new();
        write_vbi_u64(&mut buf, 7843);
        assert_eq!(buf, vec![0x23, 0xBD]);
        let (v, n) = read_vbi_u64(&buf).unwrap();
        assert_eq!(v, 7843);
        assert_eq!(n, 2);
    }

    #[test]
    fn vbi_read_returns_none_on_empty_or_unterminated() {
        assert!(read_vbi_u64(&[]).is_none());
        // 0x00..0x7F all have the high bit clear, so a run of them with
        // no terminator must reject.
        let unterm = vec![0x01u8; 11];
        assert!(read_vbi_u64(&unterm).is_none());
    }

    #[test]
    fn fishead_3_0_round_trip() {
        let mut head = FisHead::new(Version::V3_0);
        head.presentation_time = Rational::new(7, 1000);
        head.basetime = Rational::new(0, 1);
        head.utc[..15].copy_from_slice(b"20260529T064100");
        assert_eq!(head.segment_length, None);
        let bytes = head.to_bytes();
        assert_eq!(bytes.len(), FISHEAD_LEN_3_0);
        let back = FisHead::parse(&bytes).unwrap();
        assert_eq!(back, head);
    }

    #[test]
    fn fishead_4_0_round_trip() {
        let mut head = FisHead::new(Version::V4_0);
        head.presentation_time = Rational::new(0, 1000);
        head.basetime = Rational::new(0, 1000);
        head.segment_length = Some(1_234_567);
        head.content_byte_offset = Some(4096);
        let bytes = head.to_bytes();
        assert_eq!(bytes.len(), FISHEAD_LEN_4_0);
        let back = FisHead::parse(&bytes).unwrap();
        assert_eq!(back, head);
    }

    #[test]
    fn fishead_rejects_short_packet() {
        assert!(FisHead::parse(b"fishead\0only_a_bit").is_err());
    }

    #[test]
    fn fishead_rejects_wrong_magic() {
        let mut bytes = FisHead::new(Version::V4_0).to_bytes();
        bytes[0] = b'X';
        assert!(FisHead::parse(&bytes).is_err());
    }

    #[test]
    fn fisbone_round_trip() {
        let mut bone = FisBone::new(0xdead_beef, Rational::new(48_000, 1));
        bone.num_headers = 3;
        bone.basegranule = 0;
        bone.preroll = 2;
        bone.granuleshift = 6;
        bone.set_header("Content-Type", "audio/vorbis");
        bone.set_header("Role", "audio/main");
        bone.set_header("Name", "english_main");
        let bytes = bone.to_bytes();
        let back = FisBone::parse(&bytes).unwrap();
        assert_eq!(back, bone);
        assert_eq!(back.header("content-type"), Some("audio/vorbis"));
        assert_eq!(back.header("role"), Some("audio/main"));
    }

    #[test]
    fn fisbone_set_header_replaces_case_insensitively() {
        let mut bone = FisBone::new(1, Rational::new(48_000, 1));
        bone.set_header("Role", "audio/main");
        bone.set_header("role", "audio/alternate");
        assert_eq!(bone.headers.len(), 1);
        assert_eq!(bone.header("Role"), Some("audio/alternate"));
    }

    #[test]
    fn fisbone_parse_tolerates_skipped_lines() {
        let mut bone = FisBone::new(7, Rational::new(25, 1));
        bone.set_header("Content-Type", "video/theora");
        let mut bytes = bone.to_bytes();
        // Append a malformed line that has no ':' separator — it should
        // be silently skipped without aborting the parse.
        bytes.extend_from_slice(b"not-a-header-line\r\n");
        let back = FisBone::parse(&bytes).unwrap();
        assert_eq!(back.headers.len(), 1);
        assert_eq!(back.header("Content-Type"), Some("video/theora"));
    }

    #[test]
    fn index_round_trip() {
        let mut idx = SkelIndex::new(0x2A, 1_000_000);
        idx.first_sample_time = 0;
        idx.last_sample_time = 60_000_000;
        // Three keypoints at increasing (offset, timestamp).
        idx.push(4096, 0);
        idx.push(4096 + 7843, 1_000_000);
        idx.push(4096 + 7843 + 65_536, 2_500_000);
        let bytes = idx.to_bytes();
        let back = SkelIndex::parse(&bytes).unwrap();
        assert_eq!(back, idx);
        assert_eq!(back.keypoints.len(), 3);
    }

    #[test]
    fn index_empty_round_trip() {
        let idx = SkelIndex::new(99, 48_000);
        let bytes = idx.to_bytes();
        let back = SkelIndex::parse(&bytes).unwrap();
        assert_eq!(back, idx);
        assert!(back.keypoints.is_empty());
    }

    #[test]
    fn index_rejects_truncated_keypoints() {
        // Pretend there's one keypoint but only emit the offset delta —
        // the timestamp delta should fail to decode.
        let mut bytes = Vec::new();
        bytes.extend_from_slice(INDEX_MAGIC);
        bytes.extend_from_slice(&1u32.to_le_bytes()); // serial
        bytes.extend_from_slice(&1u64.to_le_bytes()); // n_keypoints
        bytes.extend_from_slice(&1_000_000i64.to_le_bytes()); // ts denominator
        bytes.extend_from_slice(&0i64.to_le_bytes()); // first sample
        bytes.extend_from_slice(&0i64.to_le_bytes()); // last sample
        write_vbi_u64(&mut bytes, 4096); // offset delta only
        assert!(SkelIndex::parse(&bytes).is_err());
    }

    #[test]
    fn skeleton_lookups() {
        let mut sk = Skeleton::new();
        sk.set_head(FisHead::new(Version::V4_0));
        let mut bone_a = FisBone::new(10, Rational::new(48_000, 1));
        bone_a.set_header("Content-Type", "audio/vorbis");
        let mut bone_b = FisBone::new(20, Rational::new(25, 1));
        bone_b.set_header("Content-Type", "video/theora");
        sk.push_bone(bone_a);
        sk.push_bone(bone_b);
        sk.push_index(SkelIndex::new(10, 48_000));
        assert!(sk.is_parsed());
        assert_eq!(sk.version(), Version::V4_0);
        assert_eq!(
            sk.bone_for_serial(10).unwrap().header("Content-Type"),
            Some("audio/vorbis")
        );
        assert!(sk.bone_for_serial(999).is_none());
        assert!(sk.index_for_serial(10).is_some());
        assert!(sk.index_for_serial(20).is_none());
    }

    #[test]
    fn detector_helpers() {
        assert!(is_fishead(FISHEAD_MAGIC));
        assert!(is_fisbone(FISBONE_MAGIC));
        assert!(is_index(INDEX_MAGIC));
        assert!(!is_fishead(b"opus    "));
        assert!(!is_fisbone(b"vorbis "));
        assert!(!is_index(b"fishead"));
    }

    #[test]
    fn version_ordering() {
        assert!(Version::V4_0.at_least(Version::V3_0));
        assert!(Version::V4_0.at_least(Version::V4_0));
        assert!(!Version::V3_0.at_least(Version::V4_0));
    }

    #[test]
    fn rational_seconds() {
        assert_eq!(Rational::new(7, 1000).to_seconds(), 0.007);
        assert_eq!(Rational::new(60_000_000, 1_000_000).to_seconds(), 60.0);
        // Skeleton 4.0 §"Keyframe index packets": denominator 0 means
        // "unknown"; expose that as 0.0 rather than NaN.
        assert_eq!(Rational::new(123, 0).to_seconds(), 0.0);
    }
}
