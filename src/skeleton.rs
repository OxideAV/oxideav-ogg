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

/// Documented value of the `Role` message-header field on a `fisbone`.
///
/// The Skeleton-4 message-headers wiki
/// (`docs/container/ogg/ogg-skeleton-message-headers.wiki` §Role)
/// enumerates the roles for text / video / audio tracks. Every variant
/// here mirrors a single bullet from that list; the wiki notes "Other
/// roles are possible, too", so callers will see [`RoleKind::Other`]
/// for forward-compatible / vendor-defined values.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum RoleKind {
    /// `text/caption` — transcription of all sounds, including speech,
    /// for the hard-of-hearing.
    TextCaption,
    /// `text/subtitle` — translation of speech, typically into a
    /// different language.
    TextSubtitle,
    /// `text/textaudiodesc` — description/transcription of everything
    /// happening in the video, for screen-reader / braille use.
    TextTextAudioDesc,
    /// `text/karaoke` — music lyrics delivered in chunks for sing-along.
    TextKaraoke,
    /// `text/chapters` — DVD-style chapter section titles.
    TextChapters,
    /// `text/tickertext` — text to scroll at the bottom of the display.
    TextTickerText,
    /// `text/lyrics` — transcript of song lyrics.
    TextLyrics,
    /// `text/metadata` — name/value pairs associated with sections of
    /// the media.
    TextMetadata,
    /// `text/annotation` — free text associated with sections of the
    /// media.
    TextAnnotation,
    /// `text/linguistic` — linguistic markup of spoken words.
    TextLinguistic,
    /// `video/main` — the main video track.
    VideoMain,
    /// `video/alternate` — an alternative video track (e.g. different
    /// camera angle).
    VideoAlternate,
    /// `video/sign` — a sign-language video track.
    VideoSign,
    /// `video/captioned` — the main video with burnt-in captions.
    VideoCaptioned,
    /// `video/subtitled` — the main video with burnt-in subtitles.
    VideoSubtitled,
    /// `audio/main` — the main audio track.
    AudioMain,
    /// `audio/alternate` — an alternative audio track.
    AudioAlternate,
    /// `audio/dub` — the audio track dubbed into another language.
    AudioDub,
    /// `audio/audiodesc` — an audio description for the vision-impaired.
    AudioAudioDesc,
    /// `audio/described` — the main audio mixed with audio descriptions.
    AudioDescribed,
    /// `audio/music` — a music-only track.
    AudioMusic,
    /// `audio/speech` — a speech-only track.
    AudioSpeech,
    /// `audio/sfx` — a sound-effects-only track.
    AudioSfx,
    /// `audio/commentary` — commentary on the main audio or video.
    AudioCommentary,
    /// A role that does not match any of the wiki-enumerated values.
    /// The original case-preserved kind string is retained verbatim so
    /// callers can surface it without losing information.
    Other(String),
}

impl RoleKind {
    /// Wire-format representation of this role (everything up to the
    /// first `;`). For [`RoleKind::Other`], the inner string is returned.
    pub fn as_wire(&self) -> &str {
        match self {
            RoleKind::TextCaption => "text/caption",
            RoleKind::TextSubtitle => "text/subtitle",
            RoleKind::TextTextAudioDesc => "text/textaudiodesc",
            RoleKind::TextKaraoke => "text/karaoke",
            RoleKind::TextChapters => "text/chapters",
            RoleKind::TextTickerText => "text/tickertext",
            RoleKind::TextLyrics => "text/lyrics",
            RoleKind::TextMetadata => "text/metadata",
            RoleKind::TextAnnotation => "text/annotation",
            RoleKind::TextLinguistic => "text/linguistic",
            RoleKind::VideoMain => "video/main",
            RoleKind::VideoAlternate => "video/alternate",
            RoleKind::VideoSign => "video/sign",
            RoleKind::VideoCaptioned => "video/captioned",
            RoleKind::VideoSubtitled => "video/subtitled",
            RoleKind::AudioMain => "audio/main",
            RoleKind::AudioAlternate => "audio/alternate",
            RoleKind::AudioDub => "audio/dub",
            RoleKind::AudioAudioDesc => "audio/audiodesc",
            RoleKind::AudioDescribed => "audio/described",
            RoleKind::AudioMusic => "audio/music",
            RoleKind::AudioSpeech => "audio/speech",
            RoleKind::AudioSfx => "audio/sfx",
            RoleKind::AudioCommentary => "audio/commentary",
            RoleKind::Other(s) => s.as_str(),
        }
    }

    /// True for `text/*` roles.
    pub fn is_text(&self) -> bool {
        matches!(
            self,
            RoleKind::TextCaption
                | RoleKind::TextSubtitle
                | RoleKind::TextTextAudioDesc
                | RoleKind::TextKaraoke
                | RoleKind::TextChapters
                | RoleKind::TextTickerText
                | RoleKind::TextLyrics
                | RoleKind::TextMetadata
                | RoleKind::TextAnnotation
                | RoleKind::TextLinguistic
        )
    }

    /// True for `video/*` roles.
    pub fn is_video(&self) -> bool {
        matches!(
            self,
            RoleKind::VideoMain
                | RoleKind::VideoAlternate
                | RoleKind::VideoSign
                | RoleKind::VideoCaptioned
                | RoleKind::VideoSubtitled
        )
    }

    /// True for `audio/*` roles.
    pub fn is_audio(&self) -> bool {
        matches!(
            self,
            RoleKind::AudioMain
                | RoleKind::AudioAlternate
                | RoleKind::AudioDub
                | RoleKind::AudioAudioDesc
                | RoleKind::AudioDescribed
                | RoleKind::AudioMusic
                | RoleKind::AudioSpeech
                | RoleKind::AudioSfx
                | RoleKind::AudioCommentary
        )
    }
}

/// Parsed value of the `Role` message-header field.
///
/// The wiki documents two shapes:
///
/// * a bare role tag, e.g. `audio/main`;
/// * a role tag followed by `;key=value` parameters, e.g.
///   `video/alternate;angle=nw`.
///
/// [`Role::kind`] holds the tag; [`Role::parameters`] holds the
/// (case-preserved, trimmed) key/value pairs in source order. Lookup
/// helpers are case-insensitive on the key per the HTTP-style framing
/// the rest of the Skeleton message-header block uses.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Role {
    /// Semantic role kind (one of the enumerated wiki values, or
    /// [`RoleKind::Other`]).
    pub kind: RoleKind,
    /// `;key=value` parameters appended after the role tag, in
    /// declaration order. The key is preserved as written; case-
    /// insensitive lookup is provided by [`Role::parameter`].
    pub parameters: Vec<(String, String)>,
}

impl Role {
    /// Parse a `Role` header value into a [`Role`].
    ///
    /// The role tag is matched case-insensitively against the
    /// wiki-enumerated values; anything else maps to
    /// [`RoleKind::Other`] retaining the as-written tag. Parameters
    /// are split on `;`; each parameter is split on the first `=`
    /// (no `=` → an empty value), and surrounding whitespace is
    /// trimmed on every token.
    pub fn parse(raw: &str) -> Role {
        let mut parts = raw.split(';');
        let head = parts.next().unwrap_or("").trim();
        let kind = match head.to_ascii_lowercase().as_str() {
            "text/caption" => RoleKind::TextCaption,
            "text/subtitle" => RoleKind::TextSubtitle,
            "text/textaudiodesc" => RoleKind::TextTextAudioDesc,
            "text/karaoke" => RoleKind::TextKaraoke,
            "text/chapters" => RoleKind::TextChapters,
            "text/tickertext" => RoleKind::TextTickerText,
            "text/lyrics" => RoleKind::TextLyrics,
            "text/metadata" => RoleKind::TextMetadata,
            "text/annotation" => RoleKind::TextAnnotation,
            "text/linguistic" => RoleKind::TextLinguistic,
            "video/main" => RoleKind::VideoMain,
            "video/alternate" => RoleKind::VideoAlternate,
            "video/sign" => RoleKind::VideoSign,
            "video/captioned" => RoleKind::VideoCaptioned,
            "video/subtitled" => RoleKind::VideoSubtitled,
            "audio/main" => RoleKind::AudioMain,
            "audio/alternate" => RoleKind::AudioAlternate,
            "audio/dub" => RoleKind::AudioDub,
            "audio/audiodesc" => RoleKind::AudioAudioDesc,
            "audio/described" => RoleKind::AudioDescribed,
            "audio/music" => RoleKind::AudioMusic,
            "audio/speech" => RoleKind::AudioSpeech,
            "audio/sfx" => RoleKind::AudioSfx,
            "audio/commentary" => RoleKind::AudioCommentary,
            _ => RoleKind::Other(head.to_string()),
        };
        let mut parameters = Vec::new();
        for p in parts {
            let p = p.trim();
            if p.is_empty() {
                continue;
            }
            if let Some(eq) = p.find('=') {
                let (k, v) = p.split_at(eq);
                parameters.push((k.trim().to_string(), v[1..].trim().to_string()));
            } else {
                parameters.push((p.to_string(), String::new()));
            }
        }
        Role { kind, parameters }
    }

    /// Look up a parameter value by case-insensitive name.
    pub fn parameter(&self, name: &str) -> Option<&str> {
        self.parameters
            .iter()
            .find(|(k, _)| k.eq_ignore_ascii_case(name))
            .map(|(_, v)| v.as_str())
    }
}

/// One numeric argument inside a `Display-hint` parametric form.
///
/// The Skeleton-4 message-headers wiki
/// (`docs/container/ogg/ogg-skeleton-message-headers.wiki` §Display-hint)
/// documents `pip(x,y,w,h)` and `mask(img,x,y,w,h)` arguments as
/// "x, y, w, and h can be specified in percentage, thus allowing
/// persistent placement independent of the scaling of the video display"
/// — the worked examples are `pip(40,40,690,60)` (raw pixel integers) and
/// `pip(20%,20%)` (percent values with a trailing `%`).
///
/// [`DisplayCoord::Percent`] carries the percent value (so `20%` →
/// `Percent(20.0)`); [`DisplayCoord::Pixels`] carries the raw integer
/// coordinate. Percent is `f32` because the wiki gives whole-number
/// examples but does not forbid fractional percents (e.g. `12.5%`); pixel
/// values are `i32` because the wiki gives only positive integer
/// examples but does not forbid negative offsets for off-screen anchors.
#[derive(Clone, Copy, Debug, PartialEq)]
pub enum DisplayCoord {
    /// Raw pixel offset / extent (the wiki's `pip(40,40,690,60)` form).
    Pixels(i32),
    /// Percentage value (the wiki's `pip(20%,20%)` / `transparent(7%)`
    /// form). The trailing `%` is stripped at parse time and the value
    /// stored as the bare number.
    Percent(f32),
}

impl DisplayCoord {
    /// Parse a single coordinate token (the wiki spells these as raw
    /// integers like `40` or percentages like `20%`). Trims surrounding
    /// whitespace.
    fn parse(raw: &str) -> Result<DisplayCoord> {
        let trimmed = raw.trim();
        if let Some(pct) = trimmed.strip_suffix('%') {
            let pct = pct.trim();
            pct.parse::<f32>().map(DisplayCoord::Percent).map_err(|e| {
                Error::invalid(format!(
                    "Skeleton Display-hint: malformed percent coordinate {trimmed:?}: {e}"
                ))
            })
        } else {
            trimmed
                .parse::<i32>()
                .map(DisplayCoord::Pixels)
                .map_err(|e| {
                    Error::invalid(format!(
                        "Skeleton Display-hint: malformed pixel coordinate {trimmed:?}: {e}"
                    ))
                })
        }
    }
}

/// Parsed value of the `Display-hint` message-header field.
///
/// The Skeleton-4 message-headers wiki
/// (`docs/container/ogg/ogg-skeleton-message-headers.wiki` §Display-hint)
/// enumerates three forms for the rendering hint:
///
/// * `pip(x,y,w,h)` — picture-in-picture placement. `x`/`y` are the
///   top-left origin; `w`/`h` are the width/height (optional per the
///   wiki: "w,h the width and height in pixels which are optional").
/// * `mask(img,x,y,w,h)` — black-on-white image mask. `img` is the URL
///   (mandatory); the four placement coordinates are all optional.
/// * `transparent(p%)` — uniform transparency `0..=100`.
///
/// The wiki explicitly warns "A media player can of course decide to
/// ignore these hints", and notes "Currently proposed hints are:" — so
/// vendor / forward-compatible hint kinds are surfaced as
/// [`DisplayHint::Other`] with the original tag plus the raw
/// comma-separated argument list, instead of failing to parse.
#[derive(Clone, Debug, PartialEq)]
pub enum DisplayHint {
    /// `pip(x,y[,w,h])` picture-in-picture hint. `width` / `height` are
    /// `None` when the wiki's 2-arg shorthand (`pip(20%,20%)`) is used.
    Pip {
        x: DisplayCoord,
        y: DisplayCoord,
        width: Option<DisplayCoord>,
        height: Option<DisplayCoord>,
    },
    /// `mask(img[,x,y[,w,h]])` video-mask hint. `image` is the mask URL.
    /// The four placement coordinates are all optional per the wiki's
    /// progressively-shorter examples (`mask(url)`, `mask(url,30%,25%)`,
    /// `mask(url,20,20,400,320)`).
    Mask {
        image: String,
        x: Option<DisplayCoord>,
        y: Option<DisplayCoord>,
        width: Option<DisplayCoord>,
        height: Option<DisplayCoord>,
    },
    /// `transparent(p%)` uniform transparency hint. The wiki specifies
    /// "int value between 0 and 100"; the parser stores it in a `u8` and
    /// rejects values outside `0..=100` as `Err(_)` at the
    /// `FisBone::display_hint()` outer layer.
    Transparent {
        /// Percent transparency, `0..=100`.
        percent: u8,
    },
    /// A hint with a tag not listed in the wiki's "Currently proposed
    /// hints are:" enumeration. The original tag is preserved verbatim
    /// alongside the trimmed comma-separated argument list so callers
    /// can route forward-compatible hints without losing information.
    Other {
        /// Hint name (everything before the opening `(`).
        tag: String,
        /// Raw, trimmed argument tokens in source order.
        arguments: Vec<String>,
    },
}

impl DisplayHint {
    /// Parse a `Display-hint` header value into a [`DisplayHint`].
    ///
    /// The value must take the wiki's `tag(args...)` shape — a bare
    /// `tag` with no parenthesised body is rejected as malformed because
    /// every documented form carries arguments. Surrounding whitespace
    /// on the value and on individual argument tokens is trimmed.
    ///
    /// Unknown hint tags map to [`DisplayHint::Other`] retaining the
    /// as-written tag, per the wiki's "Currently proposed hints are:"
    /// soft-enumeration wording.
    pub fn parse(raw: &str) -> Result<DisplayHint> {
        let value = raw.trim();
        let open = value.find('(').ok_or_else(|| {
            Error::invalid(format!(
                "Skeleton Display-hint: missing '(' in value {value:?}"
            ))
        })?;
        if !value.ends_with(')') {
            return Err(Error::invalid(format!(
                "Skeleton Display-hint: missing trailing ')' in value {value:?}"
            )));
        }
        let tag = value[..open].trim();
        if tag.is_empty() {
            return Err(Error::invalid(format!(
                "Skeleton Display-hint: empty tag in value {value:?}"
            )));
        }
        let body = &value[open + 1..value.len() - 1];
        let args: Vec<&str> = if body.trim().is_empty() {
            Vec::new()
        } else {
            body.split(',').map(str::trim).collect()
        };

        match tag.to_ascii_lowercase().as_str() {
            "pip" => {
                if args.len() != 2 && args.len() != 4 {
                    return Err(Error::invalid(format!(
                        "Skeleton Display-hint: pip(...) expects 2 or 4 arguments, got {} in {value:?}",
                        args.len()
                    )));
                }
                let x = DisplayCoord::parse(args[0])?;
                let y = DisplayCoord::parse(args[1])?;
                let (width, height) = if args.len() == 4 {
                    (
                        Some(DisplayCoord::parse(args[2])?),
                        Some(DisplayCoord::parse(args[3])?),
                    )
                } else {
                    (None, None)
                };
                Ok(DisplayHint::Pip {
                    x,
                    y,
                    width,
                    height,
                })
            }
            "mask" => {
                if args.is_empty() {
                    return Err(Error::invalid(format!(
                        "Skeleton Display-hint: mask(...) requires at least the image URL in {value:?}"
                    )));
                }
                // The wiki enumerates mask(img), mask(img,x,y),
                // mask(img,x,y,w,h) — i.e. 1, 3 or 5 arguments. Anything
                // else is rejected as malformed.
                if !matches!(args.len(), 1 | 3 | 5) {
                    return Err(Error::invalid(format!(
                        "Skeleton Display-hint: mask(...) expects 1, 3 or 5 arguments, got {} in {value:?}",
                        args.len()
                    )));
                }
                let image = args[0].to_string();
                let (x, y, width, height) = match args.len() {
                    1 => (None, None, None, None),
                    3 => (
                        Some(DisplayCoord::parse(args[1])?),
                        Some(DisplayCoord::parse(args[2])?),
                        None,
                        None,
                    ),
                    5 => (
                        Some(DisplayCoord::parse(args[1])?),
                        Some(DisplayCoord::parse(args[2])?),
                        Some(DisplayCoord::parse(args[3])?),
                        Some(DisplayCoord::parse(args[4])?),
                    ),
                    _ => unreachable!(),
                };
                Ok(DisplayHint::Mask {
                    image,
                    x,
                    y,
                    width,
                    height,
                })
            }
            "transparent" => {
                if args.len() != 1 {
                    return Err(Error::invalid(format!(
                        "Skeleton Display-hint: transparent(...) expects 1 argument, got {} in {value:?}",
                        args.len()
                    )));
                }
                let arg = args[0];
                let stripped = arg.strip_suffix('%').unwrap_or(arg).trim();
                let pct: u32 = stripped.parse().map_err(|e| {
                    Error::invalid(format!(
                        "Skeleton Display-hint: malformed transparent({arg:?}) value: {e}"
                    ))
                })?;
                if pct > 100 {
                    return Err(Error::invalid(format!(
                        "Skeleton Display-hint: transparent({arg:?}) value {pct} exceeds 100"
                    )));
                }
                Ok(DisplayHint::Transparent { percent: pct as u8 })
            }
            _ => Ok(DisplayHint::Other {
                tag: tag.to_string(),
                arguments: args.into_iter().map(|s| s.to_string()).collect(),
            }),
        }
    }
}

/// Top-level "type" component of a `Content-Type` MIME value.
///
/// `docs/container/ogg/ogg-skeleton-message-headers.wiki` §Content-type names
/// `Content-Type` as the only **mandatory** Skeleton 4 per-track message
/// header; the value is "the mime type of the track" (e.g. `audio/vorbis`,
/// `video/theora`). The wiki and the 3.0/4.0 `fisbone\0` documentation
/// (`docs/container/ogg/ogg-skeleton-{3,4}.0.md`) both leave the
/// MIME-string grammar to the external HTTP / RFC 2045 specification, and
/// observe that the carried tracks fall almost entirely into three
/// well-known top-level types — `text/*`, `audio/*`, `video/*` —
/// matching the §Role enumeration shape.
///
/// `ContentTypeKind` enumerates those three plus an `Other(String)` escape
/// for forward-compatible / vendor types like `application/kate` (called
/// out verbatim in §Role: "mime types don't always provide the right main
/// content type (e.g. application/kate is semantically a text format)"),
/// `image/*` for cover-art tracks, etc. The original token is preserved
/// inside `Other` so callers can route on a string match.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum ContentTypeKind {
    /// `audio/*` — Vorbis, Opus, Speex, FLAC, …
    Audio,
    /// `video/*` — Theora, Daala (when carried), …
    Video,
    /// `text/*` — caption / subtitle / chapter / metadata tracks.
    Text,
    /// `image/*` — cover art and similar still-image tracks.
    Image,
    /// `application/*` — Kate (per the §Role note), Skeleton itself
    /// (`application/x-ogg-skeleton`), and other application-typed payloads.
    Application,
    /// Any top-level type not in the four well-known buckets above. The
    /// original token is preserved verbatim (case as written) so the
    /// caller can match on it without losing information.
    Other(String),
}

impl ContentTypeKind {
    /// Match a top-level MIME type string case-insensitively against the
    /// well-known buckets. Anything else maps to [`Self::Other`].
    fn from_token(token: &str) -> Self {
        match token.to_ascii_lowercase().as_str() {
            "audio" => ContentTypeKind::Audio,
            "video" => ContentTypeKind::Video,
            "text" => ContentTypeKind::Text,
            "image" => ContentTypeKind::Image,
            "application" => ContentTypeKind::Application,
            _ => ContentTypeKind::Other(token.to_string()),
        }
    }

    /// `true` for `audio/*` tracks.
    pub fn is_audio(&self) -> bool {
        matches!(self, ContentTypeKind::Audio)
    }

    /// `true` for `video/*` tracks.
    pub fn is_video(&self) -> bool {
        matches!(self, ContentTypeKind::Video)
    }

    /// `true` for `text/*` tracks.
    pub fn is_text(&self) -> bool {
        matches!(self, ContentTypeKind::Text)
    }

    /// `true` for `image/*` tracks.
    pub fn is_image(&self) -> bool {
        matches!(self, ContentTypeKind::Image)
    }

    /// `true` for `application/*` tracks (incl. `application/kate` per
    /// the §Role wiki note and `application/x-ogg-skeleton` for the
    /// metadata bitstream itself).
    pub fn is_application(&self) -> bool {
        matches!(self, ContentTypeKind::Application)
    }

    /// Lower-case wire token for the kind. `ContentTypeKind::Other(t)`
    /// returns `t` as-written so a round-trip preserves the original
    /// casing.
    pub fn as_wire(&self) -> &str {
        match self {
            ContentTypeKind::Audio => "audio",
            ContentTypeKind::Video => "video",
            ContentTypeKind::Text => "text",
            ContentTypeKind::Image => "image",
            ContentTypeKind::Application => "application",
            ContentTypeKind::Other(s) => s.as_str(),
        }
    }
}

/// Parsed value of the `Content-Type` Skeleton message-header field.
///
/// `docs/container/ogg/ogg-skeleton-message-headers.wiki` §Content-type
/// designates `Content-Type` as Skeleton 4's only **mandatory** message
/// header, with the value being a MIME type. The 4.0 `fisbone\0`
/// documentation (`docs/container/ogg/ogg-skeleton-4.0.md` §3 and
/// the matching 3.0 doc) gives the worked example
/// `"Content-Type: audio/vorbis"` and notes "Message header fields are
/// terminated/delimited by `\r\n`". Beyond the well-known
/// `<type>/<subtype>` shape, the wiki also points at the wider XiphWiki
/// `MIME_Types_and_File_Extensions` registry for the full
/// codec → mime-type map.
///
/// MIME types per RFC 2045 § 5.1 (Content-Type Header Field) allow
/// `;name=value` parameters appended after the bare type — e.g.
/// `audio/ogg;codecs=opus`, which encoders / containers occasionally
/// emit even on a Skeleton fisbone. This typed accessor splits the
/// MIME tail into a structured (`type_`, `subtype`, `parameters`)
/// triple so callers can route on either the `subtype` string or the
/// `kind` predicate without re-doing the parse themselves.
///
/// All three components are case-insensitive on lookup: MIME `type` and
/// `subtype` per RFC 2045 § 5.1 ("the type, subtype, and parameter
/// names are not case sensitive"), and parameter names follow the same
/// rule. Surrounding whitespace on the value and on every parameter
/// token is trimmed — the same HTTP-style framing tolerance the other
/// typed accessors apply.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ContentType {
    /// Top-level MIME type bucketed by [`ContentTypeKind`]. The original
    /// token survives inside [`ContentTypeKind::Other`] for forward-
    /// compatible / vendor types.
    pub kind: ContentTypeKind,
    /// MIME subtype as written (e.g. `vorbis`, `theora`, `x-ogg-skeleton`).
    /// Lookup via [`Self::subtype_eq`] is case-insensitive.
    pub subtype: String,
    /// `;key=value` parameters appended after the MIME tail, in
    /// declaration order. The key is preserved as written; case-
    /// insensitive lookup is provided by [`Self::parameter`]. Empty
    /// parameter tokens are dropped (`audio/ogg;` is a bare type).
    pub parameters: Vec<(String, String)>,
}

impl ContentType {
    /// Parse a `Content-Type` header value into a [`ContentType`].
    ///
    /// The grammar follows RFC 2045 § 5.1: a `type/subtype` pair followed
    /// by zero or more `;key=value` parameters. Surrounding whitespace
    /// is tolerated on the value, the `type`, the `subtype`, and every
    /// parameter token. The MIME `type` is matched case-insensitively
    /// against the well-known [`ContentTypeKind`] buckets; anything else
    /// (and the original casing) is preserved verbatim inside
    /// [`ContentTypeKind::Other`].
    ///
    /// A bare value with no `/` is rejected as malformed — every
    /// MIME-type spec requires the subtype, and accepting a bare
    /// `audio` would silently collapse `audio/vorbis` and `audio/opus`
    /// onto the same kind.
    pub fn parse(raw: &str) -> Result<ContentType> {
        let value = raw.trim();
        let mut parts = value.split(';');
        let head = parts.next().unwrap_or("").trim();
        if head.is_empty() {
            return Err(Error::invalid("Skeleton Content-Type: empty MIME value"));
        }
        let slash = head.find('/').ok_or_else(|| {
            Error::invalid(format!(
                "Skeleton Content-Type: missing '/' in MIME value {head:?}"
            ))
        })?;
        let type_token = head[..slash].trim();
        let subtype = head[slash + 1..].trim();
        if type_token.is_empty() {
            return Err(Error::invalid(format!(
                "Skeleton Content-Type: empty top-level type in {head:?}"
            )));
        }
        if subtype.is_empty() {
            return Err(Error::invalid(format!(
                "Skeleton Content-Type: empty subtype in {head:?}"
            )));
        }
        let kind = ContentTypeKind::from_token(type_token);
        let mut parameters = Vec::new();
        for p in parts {
            let p = p.trim();
            if p.is_empty() {
                continue;
            }
            if let Some(eq) = p.find('=') {
                let (k, v) = p.split_at(eq);
                parameters.push((k.trim().to_string(), v[1..].trim().to_string()));
            } else {
                parameters.push((p.to_string(), String::new()));
            }
        }
        Ok(ContentType {
            kind,
            subtype: subtype.to_string(),
            parameters,
        })
    }

    /// Look up a parameter value by case-insensitive name (RFC 2045
    /// § 5.1: "parameter names are not case sensitive").
    pub fn parameter(&self, name: &str) -> Option<&str> {
        self.parameters
            .iter()
            .find(|(k, _)| k.eq_ignore_ascii_case(name))
            .map(|(_, v)| v.as_str())
    }

    /// Case-insensitive subtype compare (RFC 2045 § 5.1: "the type,
    /// subtype, and parameter names are not case sensitive").
    pub fn subtype_eq(&self, expected: &str) -> bool {
        self.subtype.eq_ignore_ascii_case(expected)
    }
}

/// Parsed value of the `Title` Skeleton message-header field.
///
/// `docs/container/ogg/ogg-skeleton-message-headers.wiki` §Title designates
/// `Title` as "A free text field to provide a description of the track
/// content." with the worked example
/// `Title: "the French audio track for the movie"` — the value is shown
/// wrapped in literal double-quote characters. The wiki does not specify
/// whether those quotes are part of the on-wire value or a typographic
/// convention of the wiki rendering itself; surrounding `"…"` quotes are
/// neither mandated nor forbidden by any other §Title rule. To preserve
/// both readings without losing information:
///
/// * [`Title::raw`] returns the value with surrounding whitespace trimmed
///   (matching the HTTP-style framing tolerance the other typed accessors
///   apply) and any surrounding quote characters left intact, so callers
///   that round-trip the value back into a fisbone get the exact same
///   on-wire shape they started with;
/// * [`Title::display`] strips a single balanced pair of surrounding
///   `"…"` quotes when present, so callers that follow the wiki's
///   worked-example reading get a quote-free string.
///
/// Both views are computed lazily — the `Title` struct stores the trimmed
/// raw bytes once at parse time and projects either view on demand.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Title {
    raw: String,
}

impl Title {
    /// Parse a `Title` header value into a [`Title`]. Surrounding
    /// whitespace on the value is trimmed — same HTTP-style framing
    /// tolerance as `role()`, `languages()`, `altitude()`, `display_hint()`,
    /// and `content_type()`.
    pub fn parse(raw: &str) -> Title {
        Title {
            raw: raw.trim().to_string(),
        }
    }

    /// Trimmed value exactly as it appears on the wire (after dropping
    /// HTTP-style surrounding whitespace). Surrounding quote characters,
    /// if any, are retained so a round-trip back through `set_header`
    /// preserves the original shape byte-for-byte.
    pub fn raw(&self) -> &str {
        &self.raw
    }

    /// Display-oriented value with a single balanced pair of surrounding
    /// `"…"` quotes stripped when present. The wiki's worked example
    /// (`Title: "the French audio track for the movie"`) is shown with
    /// literal quotes; this view yields `the French audio track for the
    /// movie` matching what a media player would render.
    ///
    /// Only an outermost pair of straight double-quote characters is
    /// removed; inner quotes are kept verbatim, and a value that opens
    /// but doesn't close with a quote (or vice versa) is returned as-is.
    /// An empty value (`""`) collapses to an empty string after the
    /// strip — the caller can distinguish the two cases by checking
    /// `raw()` directly if needed.
    pub fn display(&self) -> &str {
        let s = self.raw.as_str();
        if s.len() >= 2 && s.starts_with('"') && s.ends_with('"') {
            &s[1..s.len() - 1]
        } else {
            s
        }
    }

    /// True if [`Self::raw`] is empty after trimming.
    pub fn is_empty(&self) -> bool {
        self.raw.is_empty()
    }
}

/// Parsed value of the `Name` Skeleton message-header field.
///
/// `docs/container/ogg/ogg-skeleton-message-headers.wiki` §Name designates
/// `Name` as a stable per-track identifier used "to allow direct addressing
/// of the track through its name", with the worked example
/// `track[name="Madonna_singing"]` showing how a media player can locate
/// the track by its declared name.
///
/// The wiki specifies the allowed character set verbatim — it is the
/// XML 1.0 `NCName` production:
///
/// * The first character must be one of `[A-Z] | "_" | [a-z] |
///   [#xC0-#xD6] | [#xD8-#xF6] | [#xF8-#x2FF] | [#x370-#x37D] |
///   [#x37F-#x1FFF] | [#x200C-#x200D] | [#x2070-#x218F] |
///   [#x2C00-#x2FEF] | [#x3001-#xD7FF] | [#xF900-#xFDCF] |
///   [#xFDF0-#xFFFD] | [#x10000-#xEFFFF]`.
/// * Any following character may additionally be one of `"-" | "." |
///   [0-9] | #xB7 | [#x0300-#x036F] | [#x203F-#x2040]`.
///
/// The wiki also states "The name needs to be unique between all the
/// track names, otherwise it is undefined which of the tracks is
/// retrieved when addressing by name." That uniqueness check is a
/// file-level invariant (it requires looking at every `fisbone\0`
/// in the same Skeleton stream) and lives outside this per-value
/// parser — callers walk `Skeleton::bone_for_serial` to enforce it.
///
/// This struct stores the trimmed raw bytes once at parse time and
/// surfaces both the original on-wire string ([`Name::raw`]) and a
/// boolean grammar check ([`Name::is_well_formed`]) so the caller can
/// decide whether to surface the value to a `track[name=…]` resolver
/// or reject the field.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Name {
    raw: String,
}

impl Name {
    /// Parse a `Name` header value into a [`Name`]. Surrounding
    /// whitespace on the value is trimmed — same HTTP-style framing
    /// tolerance as `role()`, `languages()`, `altitude()`,
    /// `display_hint()`, `content_type()`, and `title()`.
    pub fn parse(raw: &str) -> Name {
        Name {
            raw: raw.trim().to_string(),
        }
    }

    /// Trimmed value exactly as it appears on the wire (after dropping
    /// HTTP-style surrounding whitespace).
    pub fn raw(&self) -> &str {
        &self.raw
    }

    /// True iff [`Self::raw`] matches the XML 1.0 `NCName`-shaped grammar
    /// the wiki specifies for the `Name` field.
    ///
    /// The grammar is checked verbatim against the two §Name allow-lists
    /// (first-character set and following-character set). An empty value,
    /// or one whose first code point is outside the first-character set,
    /// or one whose remaining code points step outside the following-
    /// character set, returns `false`.
    ///
    /// The wiki places no length cap on `Name`, so this predicate is
    /// purely a character-class check; encoders that emit, say,
    /// `Name: Madonna_singing` get `true` while encoders that emit
    /// `Name: 9-track` (digit prefix) get `false`. Callers that want
    /// to surface the value to a `track[name=…]` resolver should gate
    /// on `is_well_formed` before publishing the name.
    pub fn is_well_formed(&self) -> bool {
        let mut chars = self.raw.chars();
        let Some(first) = chars.next() else {
            return false;
        };
        if !is_xml_name_start_char(first) {
            return false;
        }
        chars.all(is_xml_name_char)
    }

    /// True if [`Self::raw`] is empty after trimming.
    pub fn is_empty(&self) -> bool {
        self.raw.is_empty()
    }
}

/// Member of the §Name first-character allow-list per
/// `docs/container/ogg/ogg-skeleton-message-headers.wiki` §Name.
///
/// Mirrors the XML 1.0 `NameStartChar` production verbatim. Kept inline
/// (no external dep) because the allow-list is closed and machine-
/// readable from the wiki.
fn is_xml_name_start_char(c: char) -> bool {
    matches!(c,
        'A'..='Z'
        | '_'
        | 'a'..='z'
        | '\u{C0}'..='\u{D6}'
        | '\u{D8}'..='\u{F6}'
        | '\u{F8}'..='\u{2FF}'
        | '\u{370}'..='\u{37D}'
        | '\u{37F}'..='\u{1FFF}'
        | '\u{200C}'..='\u{200D}'
        | '\u{2070}'..='\u{218F}'
        | '\u{2C00}'..='\u{2FEF}'
        | '\u{3001}'..='\u{D7FF}'
        | '\u{F900}'..='\u{FDCF}'
        | '\u{FDF0}'..='\u{FFFD}'
        | '\u{10000}'..='\u{EFFFF}')
}

/// Member of the §Name following-character allow-list — the
/// first-character allow-list plus the extra `"-" | "." | [0-9] | #xB7
/// | [#x0300-#x036F] | [#x203F-#x2040]` code points per
/// `docs/container/ogg/ogg-skeleton-message-headers.wiki` §Name.
///
/// Mirrors the XML 1.0 `NameChar` production verbatim.
fn is_xml_name_char(c: char) -> bool {
    if is_xml_name_start_char(c) {
        return true;
    }
    matches!(c,
        '-'
        | '.'
        | '0'..='9'
        | '\u{B7}'
        | '\u{0300}'..='\u{036F}'
        | '\u{203F}'..='\u{2040}')
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

    /// Typed `Role` accessor.
    ///
    /// Parses the `Role` message header per
    /// `docs/container/ogg/ogg-skeleton-message-headers.wiki` §Role into a
    /// [`Role`] value (kind enum + optional `;key=value` parameters).
    /// Returns `None` if no `Role` header is present.
    ///
    /// Unknown role strings round-trip as [`RoleKind::Other`] so callers
    /// can surface forward-compatible role tags without a parse error.
    pub fn role(&self) -> Option<Role> {
        self.header("Role").map(Role::parse)
    }

    /// Parse the `Language` message header into a list of BCP-47-shaped
    /// language tags per
    /// `docs/container/ogg/ogg-skeleton-message-headers.wiki` §Language.
    ///
    /// The wiki specifies comma-separated tags with the dominating
    /// language first; this method splits on `,`, trims each entry, and
    /// drops empty fragments. No BCP-47 syntax validation is performed
    /// (the wiki references BCP 47 / W3C LTLI but does not enumerate the
    /// grammar inside the Skeleton spec itself).
    ///
    /// Returns `None` if no `Language` header is present.
    pub fn languages(&self) -> Option<Vec<&str>> {
        self.header("Language").map(|raw| {
            raw.split(',')
                .map(str::trim)
                .filter(|s| !s.is_empty())
                .collect()
        })
    }

    /// Typed `Altitude` accessor.
    ///
    /// Parses the `Altitude` message header per
    /// `docs/container/ogg/ogg-skeleton-message-headers.wiki` §Altitude
    /// into a signed integer stack-order value: "The Altitude field
    /// takes the same numerical values as the z-index in CSS, unlimited
    /// negative and positive numbers. ... An element with greater stack
    /// order is always in front of an element with a lower stack order."
    /// The wiki gives `Altitude: -150` as the worked example.
    ///
    /// The outer `Option` distinguishes "header absent" from "header
    /// present"; the inner [`Result`] surfaces a parse error (malformed
    /// non-integer value, or one that overflows `i64`) so the caller
    /// can decide whether to skip the field or reject the packet. The
    /// value is trimmed of surrounding whitespace before parsing — the
    /// rest of the Skeleton message-header block uses HTTP-style framing
    /// that may inject a leading space after the colon.
    ///
    /// The wiki notes "unlimited" stack-order magnitudes; this accessor
    /// caps at `i64` for the same reason CSS implementations cap at
    /// 32/64-bit signed: a real `Altitude` value sits comfortably in
    /// the small-integer range, and the spec phrasing stops at
    /// "z-index in CSS" for the precision requirement. A value outside
    /// `i64` range surfaces as `Some(Err(_))` rather than silently
    /// clamping.
    pub fn altitude(&self) -> Option<Result<i64>> {
        self.header("Altitude").map(|raw| {
            let trimmed = raw.trim();
            trimmed.parse::<i64>().map_err(|e| {
                Error::invalid(format!(
                    "Skeleton fisbone: malformed Altitude value {trimmed:?}: {e}"
                ))
            })
        })
    }

    /// Typed `Display-hint` accessor.
    ///
    /// Parses the `Display-hint` message header per
    /// `docs/container/ogg/ogg-skeleton-message-headers.wiki` §Display-hint
    /// into a [`DisplayHint`] discriminating between the three documented
    /// rendering hints — `pip(x,y[,w,h])`, `mask(img[,x,y[,w,h]])`,
    /// `transparent(p%)` — and a fall-through [`DisplayHint::Other`] for
    /// forward-compatible / vendor-defined hint tags (the wiki phrasing
    /// "Currently proposed hints are:" leaves room for new ones).
    ///
    /// The outer `Option` distinguishes "header absent" from "header
    /// present"; the inner [`Result`] surfaces a parse error (missing
    /// parentheses, wrong number of arguments for a documented tag,
    /// non-numeric coordinate, out-of-range `transparent` percent, …)
    /// so the caller can decide whether to skip the field or reject the
    /// packet. The header value is trimmed and so is every individual
    /// argument token — the same HTTP-style framing tolerance as
    /// `role()`, `languages()` and `altitude()`.
    ///
    /// The wiki notes "A media player can of course decide to ignore
    /// these hints" — this accessor surfaces the structured form so
    /// callers can route decisions on it rather than re-doing the string
    /// match themselves.
    pub fn display_hint(&self) -> Option<Result<DisplayHint>> {
        self.header("Display-hint").map(DisplayHint::parse)
    }

    /// Typed `Content-Type` accessor.
    ///
    /// Parses the `Content-Type` message header per
    /// `docs/container/ogg/ogg-skeleton-message-headers.wiki` §Content-type
    /// (and the matching example in `docs/container/ogg/ogg-skeleton-4.0.md`
    /// §3 `"Content-Type: audio/vorbis"`) into a [`ContentType`] triple of
    /// (`kind`, `subtype`, `parameters`). The MIME `type` is bucketed by
    /// the [`ContentTypeKind`] enum (`audio` / `video` / `text` / `image`
    /// / `application`); unknown top-level types round-trip as
    /// [`ContentTypeKind::Other`] so the wiki's "mime types don't always
    /// provide the right main content type (e.g. application/kate is
    /// semantically a text format)" pattern survives intact.
    ///
    /// `Content-Type` is the only **mandatory** Skeleton 4 message header
    /// per the wiki ("Right now, there is one mandatory message header
    /// field for all of the logical bitstreams: the `Content-type`
    /// header field"); the outer `Option` distinguishes "header absent"
    /// (a non-conforming fisbone) from "header present", and the inner
    /// [`Result`] surfaces a parse error (empty value, missing `/`,
    /// empty `type` or `subtype`) so the caller can decide whether to
    /// skip the field or reject the packet. Surrounding whitespace on
    /// the value and on every parameter token is trimmed — the same
    /// HTTP-style framing tolerance as `role()`, `languages()`,
    /// `altitude()`, and `display_hint()`. Header-name lookup is
    /// case-insensitive via the underlying `FisBone::header` path.
    pub fn content_type(&self) -> Option<Result<ContentType>> {
        self.header("Content-Type").map(ContentType::parse)
    }

    /// Typed `Title` accessor.
    ///
    /// Parses the `Title` message header per
    /// `docs/container/ogg/ogg-skeleton-message-headers.wiki` §Title:
    /// "A free text field to provide a description of the track content."
    /// The wiki gives the worked example
    /// `Title: "the French audio track for the movie"` — the value is
    /// shown wrapped in literal double-quote characters, which the wiki
    /// neither requires nor forbids elsewhere in the message-header block.
    /// The accessor surfaces both shapes through a dedicated [`Title`]
    /// type: [`Title::raw`] retains the value exactly as the
    /// header carries it (whitespace trimmed only — same HTTP-style
    /// framing tolerance as the other typed accessors) and
    /// [`Title::display`] strips a single balanced pair of surrounding
    /// `"…"` quotes so callers that want the wiki-example reading get a
    /// quote-free string without losing the original.
    ///
    /// Returns `None` if no `Title` header is present. The field is
    /// optional per the wiki (only `Content-Type` is mandatory, per
    /// §Content-type "Right now, there is one mandatory message header
    /// field for all of the logical bitstreams").
    pub fn title(&self) -> Option<Title> {
        self.header("Title").map(Title::parse)
    }

    /// Typed `Name` accessor.
    ///
    /// Parses the `Name` message header per
    /// `docs/container/ogg/ogg-skeleton-message-headers.wiki` §Name:
    /// "This field provides the opportunity to associate a free text
    /// string with the track to allow direct addressing of the track
    /// through its name." The wiki's worked example
    /// `track[name="Madonna_singing"]` shows how a media player resolves
    /// a track by its declared name.
    ///
    /// The wiki specifies an XML 1.0 `NCName`-shaped grammar verbatim
    /// for the allowed character set ("the first character has to be
    /// one of … any following characters can be one of …"). The accessor
    /// surfaces the value through a dedicated [`Name`] struct: [`Name::raw`]
    /// retains the value exactly as the header carries it (whitespace
    /// trimmed only — same HTTP-style framing tolerance as the other typed
    /// accessors) and [`Name::is_well_formed`] returns the grammar check.
    /// Callers that want to surface the value to a `track[name=…]`
    /// resolver gate on `is_well_formed` before publishing the name.
    ///
    /// Returns `None` if no `Name` header is present. The field is
    /// optional per the wiki (only `Content-Type` is mandatory, per
    /// §Content-type "Right now, there is one mandatory message header
    /// field for all of the logical bitstreams"). The wiki additionally
    /// states "The name needs to be unique between all the track
    /// names, otherwise it is undefined which of the tracks is retrieved
    /// when addressing by name" — that uniqueness invariant is a
    /// file-level check across every fisbone in the same Skeleton stream
    /// and is enforced by callers via [`crate::skeleton::Skeleton::bone_for_serial`], not
    /// inside this per-value parser.
    pub fn name(&self) -> Option<Name> {
        self.header("Name").map(Name::parse)
    }

    /// Extract the *granule value* from a content page's raw `granulepos`
    /// field by undoing this track's granuleshift packing.
    ///
    /// Per `docs/container/ogg/ogg-skeleton-4.0.md` §"What decoding-related
    /// information is needed?", the granuleshift is "the number of lower
    /// bits from the granulepos field that are used to provide position
    /// information for sub-seekable units (like the keyframe shift in
    /// theora)", and the spec notes that "the granulepos of a data page
    /// must first be parsed to extract a granule value using the method
    /// described in *GranulePosAndSeeking*. This value can then be mapped
    /// to time by calculating `granules / granulerate`."
    ///
    /// For a track whose `granuleshift` is `0` (Vorbis, Opus, FLAC, Speex
    /// — every audio mapping), the granulepos *is* the granule value and
    /// this returns it unchanged. For Theora-style packed granulepos the
    /// high bits hold the index of the last keyframe and the low
    /// `granuleshift` bits hold the offset since that keyframe; the
    /// absolute granule value is the sum of the two halves
    /// (`(g >> shift) + (g & ((1 << shift) - 1))`), exactly the formula the
    /// demuxer's Theora seek path uses to map a packed granulepos to an
    /// absolute frame number.
    ///
    /// A negative `granulepos` (`-1` means "no packet finishes on this
    /// page" per RFC 3533 §6) is returned verbatim so callers can treat it
    /// as "no timing information on this page" rather than mis-splitting a
    /// sentinel. A `granuleshift >= 63` (degenerate / attacker-edited:
    /// every bit would be "offset" with no room for a keyframe index)
    /// yields `0` rather than overflowing the `1 << shift` mask.
    pub fn extract_granules(&self, granulepos: i64) -> i64 {
        let shift = self.granuleshift as u32;
        if granulepos < 0 {
            return granulepos;
        }
        if shift == 0 {
            return granulepos;
        }
        if shift >= 63 {
            return 0;
        }
        let g = granulepos as u64;
        let kf = (g >> shift) as i64;
        let off = (g & ((1u64 << shift) - 1)) as i64;
        kf.saturating_add(off)
    }

    /// Map a content page's raw `granulepos` to a playback time in
    /// seconds, relative to this track's granule position 0.
    ///
    /// This is the two-step mapping `docs/container/ogg/ogg-skeleton-4.0.md`
    /// §"What decoding-related information is needed?" spells out: first
    /// [`Self::extract_granules`] undoes any granuleshift packing, then the
    /// granule value is divided by the granule rate ("This value can then
    /// be mapped to time by calculating `granules / granulerate`"). The
    /// granule rate is the per-track rational the fisbone carries
    /// (`granule_rate`, in Hz for audio / fps for video).
    ///
    /// Returns `None` when timing cannot be determined:
    /// - the `granulepos` is negative (the RFC 3533 §6 `-1` "no packets
    ///   finish on this page" sentinel carries no position), or
    /// - the granule rate is unusable — a non-positive numerator or
    ///   denominator. The Skeleton rationals are signed 64-bit pairs and a
    ///   zero denominator is the spec's "unknown" marker; a real granule
    ///   rate is a strictly positive samples-per-second / frames-per-second
    ///   value, so anything else is reported as "unknown" rather than
    ///   producing a NaN or a negative time.
    ///
    /// The returned value is **relative to granule 0** and does *not*
    /// include the fishead's basetime offset; for the absolute playback
    /// time (granule-0 mapped to the basetime) use
    /// [`Skeleton::granule_to_seconds`], which adds the fishead basetime on
    /// top of this per-track value.
    pub fn granule_to_seconds(&self, granulepos: i64) -> Option<f64> {
        if granulepos < 0 {
            return None;
        }
        let rate = self.granule_rate;
        if rate.numerator <= 0 || rate.denominator <= 0 {
            return None;
        }
        let granules = self.extract_granules(granulepos);
        Some(granules as f64 * rate.denominator as f64 / rate.numerator as f64)
    }

    /// The playback time, in seconds, at which this logical bitstream's
    /// own data begins in a (possibly remuxed) Ogg segment — i.e. the
    /// time that corresponds to this track's *basegranule*.
    ///
    /// `docs/container/ogg/ogg-skeleton-4.0.md` §"How to allow the
    /// creation of substreams from an Ogg physical bitstream?" defines
    /// the basegranule as "the granule number with which this logical
    /// bitstream starts in the remuxed stream", and says it "provides for
    /// each logical bitstream the accurate start time of its data stream".
    /// For a freshly-muxed (un-cut) stream the basegranule is `0` and the
    /// data start time is therefore `0.0`; for a substream cut out of a
    /// larger file it carries the original-timeline granule the kept data
    /// began at, so the start time is non-zero.
    ///
    /// Computed exactly as the granule-to-time mapping of §"What
    /// decoding-related information is needed?" applied to the basegranule:
    /// `basegranule / granulerate`. The basegranule is a plain granule
    /// value (it is *not* granuleshift-packed — it names a granule number,
    /// not an on-wire `granulepos`), so no [`Self::extract_granules`] step
    /// is applied. The value is relative to granule 0 and does *not*
    /// include the fishead basetime; for the file-absolute data-start time
    /// (basetime added on top) use [`Skeleton::stream_start_seconds`].
    ///
    /// Returns `None` when the granule rate is unusable (non-positive
    /// numerator or denominator — the spec's zero-denominator "unknown"
    /// marker), matching [`Self::granule_to_seconds`]. A negative
    /// basegranule is preserved with its sign so a stream whose kept data
    /// begins before the original granule 0 maps to a negative start time
    /// rather than being silently clamped.
    pub fn start_seconds(&self) -> Option<f64> {
        let rate = self.granule_rate;
        if rate.numerator <= 0 || rate.denominator <= 0 {
            return None;
        }
        Some(self.basegranule as f64 * rate.denominator as f64 / rate.numerator as f64)
    }

    /// Map a content page's raw `granulepos` to a playback time in seconds
    /// measured *relative to where this track's data starts* in a remuxed
    /// segment, rather than relative to the track's granule 0.
    ///
    /// `docs/container/ogg/ogg-skeleton-4.0.md` §"How to allow the
    /// creation of substreams from an Ogg physical bitstream?" keeps the
    /// content pages — "including the framing and granule positions" —
    /// byte-for-byte intact when cutting a subpart out of a larger file,
    /// and records the basegranule so a decoder can recover "the accurate
    /// start time of its data stream". The elapsed time of a page within
    /// the kept segment is therefore the granule value minus the
    /// basegranule, divided by the granule rate:
    /// `(extract_granules(granulepos) - basegranule) / granulerate`. For an
    /// un-cut stream (basegranule `0`) this equals
    /// [`Self::granule_to_seconds`].
    ///
    /// Returns `None` on the same conditions as [`Self::granule_to_seconds`]
    /// — a negative `granulepos` (the RFC 3533 §6 `-1` sentinel) or an
    /// unusable granule rate. The result may be negative for a page whose
    /// granule precedes the basegranule (e.g. a preroll page that survived
    /// the cut), since that page presents before the cut-in point.
    pub fn granule_to_seconds_since_start(&self, granulepos: i64) -> Option<f64> {
        if granulepos < 0 {
            return None;
        }
        let rate = self.granule_rate;
        if rate.numerator <= 0 || rate.denominator <= 0 {
            return None;
        }
        let granules = self.extract_granules(granulepos);
        let elapsed = granules.saturating_sub(self.basegranule);
        Some(elapsed as f64 * rate.denominator as f64 / rate.numerator as f64)
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

        // Cap the up-front allocation by the bytes actually remaining in
        // the packet. Each keypoint is (offset-delta vbi, timestamp-delta
        // vbi), and the variable-byte integer encoder always emits at
        // least one byte per integer, so the absolute upper bound on
        // representable keypoints is `(packet.len() - PREFIX) / 2`. An
        // attacker-controlled `n_keypoints = u64::MAX` declaring billions
        // of keypoints in a 42-byte packet would otherwise pre-allocate
        // tens of gigabytes before the read loop ever discovered the
        // truncation. The cap is purely a starting capacity; the loop
        // below still grows the vector if `n_keypoints` is genuinely
        // achievable.
        let payload_remaining = packet.len() - PREFIX;
        let cap_by_bytes = payload_remaining / 2;
        let init_cap = (n_keypoints as usize).min(cap_by_bytes);
        let mut keypoints = Vec::with_capacity(init_cap);
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

    // ----------------------------------------------------------------
    // Time-domain typed accessors.
    //
    // Skeleton 4.0 §"Keyframe index packets": every per-keypoint
    // timestamp, plus the first-sample-time and last-sample-time
    // numerators, share the single `timestamp_denominator` stored in
    // the index packet header. The on-wire integers are deltas from
    // the previous keypoint (already resolved into absolute values
    // by `parse`); divide each absolute numerator by the shared
    // denominator to recover seconds. The spec marks `denominator = 0`
    // as "unable to be determined at indexing time, and is unknown"
    // for first/last sample times — the typed accessors below surface
    // that as `Option::None`.
    // ----------------------------------------------------------------

    /// Convert keypoint `index`'s timestamp to seconds using this
    /// index packet's shared `timestamp_denominator`. Returns `None`
    /// when `index` is out of range, or when the denominator is zero
    /// (Skeleton 4.0 §"Keyframe index packets": a zero denominator
    /// means the time value is unknown). Spec field-4 also notes
    /// "This must not be 0" for the denominator on writes; readers
    /// guard against malformed inputs that violate that constraint.
    pub fn keypoint_seconds(&self, index: usize) -> Option<f64> {
        if self.timestamp_denominator == 0 {
            return None;
        }
        let kp = self.keypoints.get(index)?;
        Some(kp.seconds(self.timestamp_denominator))
    }

    /// Presentation time in seconds of the first sample in this
    /// stream. `None` when the on-wire `timestamp_denominator` is 0
    /// (per Skeleton 4.0: "If the denominator is 0 for the
    /// first-sample-time or the last-sample-time, then that value
    /// was unable to be determined at indexing time, and is unknown.").
    pub fn first_sample_seconds(&self) -> Option<f64> {
        if self.timestamp_denominator == 0 {
            None
        } else {
            Some(self.first_sample_time as f64 / self.timestamp_denominator as f64)
        }
    }

    /// End time in seconds of the last sample in this stream. `None`
    /// when the shared denominator is 0 — see [`Self::first_sample_seconds`].
    pub fn last_sample_seconds(&self) -> Option<f64> {
        if self.timestamp_denominator == 0 {
            None
        } else {
            Some(self.last_sample_time as f64 / self.timestamp_denominator as f64)
        }
    }

    /// Duration of the indexed stream in seconds — the difference
    /// between the last- and first-sample times when both are known.
    /// Skeleton 4.0 §"Keyframe indexes for faster seeking" calls this
    /// out explicitly: "you can calculate the duration as the end time
    /// of the last active stream minus the start time of first active
    /// stream."
    pub fn duration_seconds(&self) -> Option<f64> {
        let first = self.first_sample_seconds()?;
        let last = self.last_sample_seconds()?;
        Some(last - first)
    }

    /// True iff keypoints are stored in non-decreasing-offset order.
    /// Skeleton 4.0 §"Keyframe index packets" mandates: "The key
    /// points are stored in increasing order by offset (and thus by
    /// presentation time as well)." A keypoint vector that violates
    /// this on a parsed index marks the input as malformed; the
    /// helper lets callers cheaply validate the invariant before
    /// trusting the binary search in [`Self::keypoint_for_time`].
    pub fn is_sorted_by_offset(&self) -> bool {
        self.keypoints
            .windows(2)
            .all(|w| w[0].offset <= w[1].offset && w[0].timestamp <= w[1].timestamp)
    }

    /// Locate the keypoint to start decoding from for a target time
    /// in seconds. Returns the *index* of the last keypoint whose
    /// presentation time is less than or equal to `target_seconds`, or
    /// `None` when:
    ///
    /// * the shared `timestamp_denominator` is 0 (timestamps unknown),
    /// * the keypoint vector is empty,
    /// * `target_seconds` is NaN or precedes every keypoint's time.
    ///
    /// Per Skeleton 4.0 §"Keyframe indexes for faster seeking": "first
    /// construct the set which contains every active streams' last
    /// keypoint which has time less than or equal to the seek target
    /// time." This helper computes that per-stream "last keypoint at
    /// or before t" answer. The caller is then expected to take the
    /// minimum byte-offset across all per-stream answers and seek
    /// there.
    ///
    /// Uses binary search; runs in `O(log n)` over the keypoint table.
    /// Relies on the spec invariant that keypoints are sorted by
    /// increasing offset (and therefore by increasing timestamp);
    /// callers can pre-flight with [`Self::is_sorted_by_offset`].
    pub fn keypoint_for_time(&self, target_seconds: f64) -> Option<usize> {
        if self.timestamp_denominator == 0 || self.keypoints.is_empty() {
            return None;
        }
        // Special floating-point inputs: NaN is rejected (no ordering
        // exists); +inf maps to "past every keypoint" → last index;
        // -inf maps to "before every keypoint" → None.
        if target_seconds.is_nan() {
            return None;
        }
        if target_seconds == f64::INFINITY {
            return Some(self.keypoints.len() - 1);
        }
        if target_seconds == f64::NEG_INFINITY {
            return None;
        }
        // Convert the target back to numerator-space so the search is
        // pure-integer comparison and immune to floating-point rounding
        // around the boundary. `target_num` is the largest integer N
        // such that N / timestamp_denominator <= target_seconds.
        //
        // Skeleton 4.0 stores timestamp_denominator as a signed i64
        // whose spec permits any non-zero value (negative denominators
        // are unusual but not forbidden by the wire format). Work in
        // signed f64 throughout the conversion and floor toward
        // negative infinity to honor the "less than or equal" wording
        // in the spec.
        let denom = self.timestamp_denominator as f64;
        let scaled = target_seconds * denom;
        // Reject overflow into the i64 range — a target so large it
        // exceeds i64::MAX/i64::MIN in numerator-space cannot match
        // any in-range keypoint, so behave like "after the last
        // keypoint" (return the last index) for positive overflow and
        // "before the first keypoint" (None) for negative overflow.
        let max = i64::MAX as f64;
        let min = i64::MIN as f64;
        let target_num: i64 = if scaled >= max {
            // Target time is greater than every representable timestamp
            // → answer is the last keypoint.
            return Some(self.keypoints.len() - 1);
        } else if scaled <= min {
            // Target time is before any representable timestamp → no
            // valid "at or before" keypoint exists.
            return None;
        } else {
            scaled.floor() as i64
        };

        // partition_point returns the count of leading elements whose
        // timestamp is <= target_num. The keypoint we want is the one
        // immediately preceding that boundary.
        let strictly_after = self
            .keypoints
            .partition_point(|kp| kp.timestamp <= target_num);
        if strictly_after == 0 {
            None
        } else {
            Some(strictly_after - 1)
        }
    }
}

impl KeyPoint {
    /// Presentation time of this keypoint in seconds, given the shared
    /// `timestamp_denominator` from the enclosing [`SkelIndex`]. Returns
    /// 0.0 when the denominator is zero — that matches [`Rational::to_seconds`]
    /// and lets keypoint scans through degenerate indexes proceed without
    /// branching at every step. Use [`SkelIndex::keypoint_seconds`] for the
    /// `Option`-returning variant that distinguishes "unknown" from
    /// "zero seconds".
    pub fn seconds(&self, timestamp_denominator: i64) -> f64 {
        if timestamp_denominator == 0 {
            0.0
        } else {
            self.timestamp as f64 / timestamp_denominator as f64
        }
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

    /// Map a content page's raw `granulepos` for the track identified by
    /// `serial` to an **absolute** playback time in seconds.
    ///
    /// This is the full Skeleton time mapping per
    /// `docs/container/ogg/ogg-skeleton-4.0.md` §"What decoding-related
    /// information is needed?": the per-track value
    /// ([`FisBone::granule_to_seconds`], i.e. granuleshift extraction then
    /// `granules / granulerate`) plus the fishead's **basetime**, which
    /// "provides a mapping for granule position 0 (for all logical
    /// bitstreams) to a playback time; an example use: most content in
    /// professional analog video creation actually starts at a time of
    /// 1 hour and thus adding this additional field allows them to retain
    /// this mapping on digitizing their content". The basetime is a
    /// per-file rational shared by all logical bitstreams (carried on the
    /// fishead, not the fisbone), so it is added once on top of the
    /// per-track granule-to-time result.
    ///
    /// Returns `None` when the absolute time cannot be determined:
    /// - no fisbone describes `serial` (the track is not in this
    ///   Skeleton), or
    /// - the per-track mapping is `None` (negative `granulepos` sentinel,
    ///   or an unusable granule rate — see [`FisBone::granule_to_seconds`]).
    ///
    /// When no fishead has been recorded, or the fishead basetime
    /// denominator is `0` (the spec's "unknown" marker), the basetime
    /// offset is treated as `0.0` and the result is the per-track value
    /// alone — basetime is an optional offset, not a precondition for a
    /// valid granule-to-time mapping.
    pub fn granule_to_seconds(&self, serial: u32, granulepos: i64) -> Option<f64> {
        let bone = self.bone_for_serial(serial)?;
        let track_seconds = bone.granule_to_seconds(granulepos)?;
        let basetime = self
            .head
            .as_ref()
            .map(|h| h.basetime.to_seconds())
            .unwrap_or(0.0);
        Some(track_seconds + basetime)
    }

    /// The Skeleton's **presentation time** (cut-in time) in seconds, the
    /// time from which all logical bitstreams are meant to start presenting.
    ///
    /// `docs/container/ogg/ogg-skeleton-4.0.md` §"How to allow the creation
    /// of substreams from an Ogg physical bitstream?" defines the
    /// presentation time as "the actual cut-in time and all logical
    /// bitstreams are meant to start presenting from this time onwards, not
    /// from the time their data starts, which may be some time before that
    /// (because this time may have mapped right into the middle of a packet,
    /// or because the logical bitstream has a preroll or a keyframe shift)".
    /// The motivating example (§intro) is the `?t=7-59` Web cut: a segment
    /// carved between the 7th and 59th second "would be nice to continue to
    /// start … with a playback time of 7 seconds and not of 0".
    ///
    /// Carried once per file on the fishead (not per fisbone), so this is a
    /// `Skeleton`-level accessor. Returns `None` when no fishead has been
    /// recorded yet; a fishead whose presentation-time denominator is `0`
    /// (the spec's "unknown" marker) yields `Some(0.0)` via
    /// [`Rational::to_seconds`], matching the un-cut default where content
    /// presents from time 0.
    pub fn presentation_seconds(&self) -> Option<f64> {
        self.head.as_ref().map(|h| h.presentation_time.to_seconds())
    }

    /// The **file-absolute** playback time, in seconds, at which the track
    /// identified by `serial` begins its data in a (possibly remuxed) Ogg
    /// segment: the per-track [`FisBone::start_seconds`] (basegranule /
    /// granulerate) plus the fishead **basetime**.
    ///
    /// Per `docs/container/ogg/ogg-skeleton-4.0.md`, the basegranule
    /// "provides for each logical bitstream the accurate start time of its
    /// data stream", and the basetime "provides a mapping for granule
    /// position 0 (for all logical bitstreams) to a playback time". The data
    /// of a track therefore starts, on the file's absolute timeline, at
    /// `basetime + basegranule / granulerate`. For an un-cut stream
    /// (basegranule `0`, basetime `0`) this is `0.0`.
    ///
    /// Returns `None` when no fisbone describes `serial`, or when the
    /// track's [`FisBone::start_seconds`] is `None` (unusable granule rate).
    /// An absent or zero-denominator basetime contributes a `0.0` offset
    /// rather than blocking the mapping, consistent with
    /// [`Self::granule_to_seconds`].
    pub fn stream_start_seconds(&self, serial: u32) -> Option<f64> {
        let bone = self.bone_for_serial(serial)?;
        let start = bone.start_seconds()?;
        let basetime = self
            .head
            .as_ref()
            .map(|h| h.basetime.to_seconds())
            .unwrap_or(0.0);
        Some(start + basetime)
    }

    /// Map a content page's raw `granulepos` for the track identified by
    /// `serial` to its **substream presentation time** in seconds: the time
    /// on the cut segment's playback timeline at which the page presents.
    ///
    /// This is the substream mapping of
    /// `docs/container/ogg/ogg-skeleton-4.0.md` §"How to allow the creation
    /// of substreams from an Ogg physical bitstream?". When a subpart is cut
    /// out of a larger file, the kept content pages retain their original
    /// granule positions, the fisbone records the **basegranule** (the
    /// granule the kept data starts at), and the fishead records the
    /// **presentation time** (the cut-in time all bitstreams present from,
    /// e.g. 7 s for a `?t=7-59` cut). A page's time on that timeline is the
    /// elapsed time since the stream's own data start
    /// ([`FisBone::granule_to_seconds_since_start`]) added to the shared
    /// cut-in presentation time:
    /// `presentation_time + (extract_granules(granulepos) - basegranule)
    /// / granulerate`.
    ///
    /// For an un-cut stream (basegranule `0`, presentation time `0`) this
    /// equals [`Self::granule_to_seconds`] without the basetime term —
    /// substream timing is expressed on the cut-in timeline (which begins at
    /// the presentation time), distinct from the basetime/granule-0 mapping
    /// that [`Self::granule_to_seconds`] returns. Choose
    /// [`Self::granule_to_seconds`] for "what wall-/base-time does granule 0
    /// correspond to"; choose this for "where on the cut segment's own
    /// playback bar does this page land".
    ///
    /// Returns `None` when no fisbone describes `serial`, when the per-track
    /// elapsed mapping is `None` (negative `granulepos` sentinel or unusable
    /// granule rate), or when no fishead has been recorded (the presentation
    /// time is then unknown). A fishead present but with a zero-denominator
    /// presentation time contributes a `0.0` cut-in offset.
    pub fn substream_granule_to_seconds(&self, serial: u32, granulepos: i64) -> Option<f64> {
        let bone = self.bone_for_serial(serial)?;
        let elapsed = bone.granule_to_seconds_since_start(granulepos)?;
        let presentation = self.presentation_seconds()?;
        Some(presentation + elapsed)
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
    fn index_capacity_bounded_by_remaining_payload() {
        // A 42-byte `index\0` packet whose on-wire `n_keypoints` field
        // declares u64::MAX must NOT pre-allocate ~96 GB. The parser
        // bounds the up-front allocation by the remaining payload
        // length (every delta-encoded keypoint is at least 2 VBI bytes
        // = 2 bytes total, so a 0-byte remaining payload yields a
        // 0-capacity vector). The parse itself fails with `Invalid`
        // because no keypoint bytes are present, but it must fail
        // *fast* — without an allocation step that the OS rejects.
        let mut bytes = Vec::with_capacity(42);
        bytes.extend_from_slice(INDEX_MAGIC);
        bytes.extend_from_slice(&7u32.to_le_bytes()); // serial
        bytes.extend_from_slice(&u64::MAX.to_le_bytes()); // attacker n_keypoints
        bytes.extend_from_slice(&1_000_000i64.to_le_bytes()); // ts denom
        bytes.extend_from_slice(&0i64.to_le_bytes()); // first sample
        bytes.extend_from_slice(&0i64.to_le_bytes()); // last sample
        assert_eq!(bytes.len(), 42);
        // Must return an error (truncated body), not OOM-abort.
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

    // -------------------------------------------------------------
    // Time-domain typed accessors for SkelIndex.
    //
    // Skeleton 4.0 §"Keyframe index packets" defines the shared
    // `timestamp_denominator` for every per-keypoint timestamp and
    // for the first/last sample times. The accessors below convert
    // numerator-space integers into seconds and provide the binary
    // search keyed on the spec's "last keypoint with time <= target"
    // rule (§"Keyframe indexes for faster seeking").
    // -------------------------------------------------------------

    fn build_demo_index() -> SkelIndex {
        // Per-stream index with the 7843-byte spec-worked-example
        // offset delta and a 1 MHz denominator so per-keypoint times
        // land at exact 1.0 s / 2.5 s boundaries.
        let mut idx = SkelIndex::new(0x2A, 1_000_000);
        idx.first_sample_time = 0;
        idx.last_sample_time = 60_000_000;
        idx.push(4096, 0);
        idx.push(4096 + 7843, 1_000_000);
        idx.push(4096 + 7843 + 65_536, 2_500_000);
        idx
    }

    #[test]
    fn skel_index_keypoint_seconds_typed() {
        let idx = build_demo_index();
        assert_eq!(idx.keypoint_seconds(0), Some(0.0));
        assert_eq!(idx.keypoint_seconds(1), Some(1.0));
        assert_eq!(idx.keypoint_seconds(2), Some(2.5));
        assert_eq!(idx.keypoint_seconds(3), None); // out of range
    }

    #[test]
    fn skel_index_keypoint_seconds_returns_none_on_unknown_denominator() {
        // Manually craft an index with denominator 0 (forbidden on
        // writes per §"Keyframe index packets" point 4 but accepted
        // by `parse` to surface malformed inputs as unknown rather
        // than NaN-yielding nonsense).
        let mut idx = SkelIndex::new(1, 0);
        idx.push(100, 42);
        assert_eq!(idx.keypoint_seconds(0), None);
    }

    #[test]
    fn skel_index_first_last_sample_seconds() {
        let idx = build_demo_index();
        assert_eq!(idx.first_sample_seconds(), Some(0.0));
        assert_eq!(idx.last_sample_seconds(), Some(60.0));
        assert_eq!(idx.duration_seconds(), Some(60.0));
    }

    #[test]
    fn skel_index_first_last_seconds_unknown_when_denom_zero() {
        let mut idx = SkelIndex::new(5, 0);
        idx.first_sample_time = 12345;
        idx.last_sample_time = 67890;
        // §"Keyframe index packets": denom 0 → unknown.
        assert_eq!(idx.first_sample_seconds(), None);
        assert_eq!(idx.last_sample_seconds(), None);
        assert_eq!(idx.duration_seconds(), None);
    }

    #[test]
    fn skel_index_is_sorted_by_offset_invariant() {
        let idx = build_demo_index();
        assert!(idx.is_sorted_by_offset());

        // A vector that violates the §"Keyframe index packets"
        // increasing-offset invariant must report false.
        let mut bad = SkelIndex::new(0xCAFE, 1_000_000);
        bad.push(2_000, 100);
        bad.push(1_000, 50);
        assert!(!bad.is_sorted_by_offset());
    }

    #[test]
    fn skel_index_keypoint_for_time_exact_boundaries() {
        let idx = build_demo_index();
        // At each keypoint's exact time, the per-spec answer is THAT
        // keypoint (since the rule is "<= target").
        assert_eq!(idx.keypoint_for_time(0.0), Some(0));
        assert_eq!(idx.keypoint_for_time(1.0), Some(1));
        assert_eq!(idx.keypoint_for_time(2.5), Some(2));
    }

    #[test]
    fn skel_index_keypoint_for_time_between_keypoints() {
        let idx = build_demo_index();
        // Between keypoint 0 (t=0) and 1 (t=1) → answer is 0.
        assert_eq!(idx.keypoint_for_time(0.5), Some(0));
        // Between keypoint 1 (t=1) and 2 (t=2.5) → answer is 1.
        assert_eq!(idx.keypoint_for_time(1.5), Some(1));
        assert_eq!(idx.keypoint_for_time(2.4999), Some(1));
        // Past the last keypoint → answer is the last keypoint
        // (the spec's "last keypoint at or before t" rule degrades to
        // "last keypoint" when t is past every entry).
        assert_eq!(idx.keypoint_for_time(100.0), Some(2));
    }

    #[test]
    fn skel_index_keypoint_for_time_before_first_is_none() {
        let idx = build_demo_index();
        // Negative target precedes every keypoint → no per-spec
        // "at or before" answer exists.
        assert_eq!(idx.keypoint_for_time(-1.0), None);
    }

    #[test]
    fn skel_index_keypoint_for_time_rejects_nan_and_inf() {
        let idx = build_demo_index();
        assert_eq!(idx.keypoint_for_time(f64::NAN), None);
        // +inf → spec answer is the last keypoint (target is past
        // every entry). The implementation returns the last index
        // via the overflow-clamp branch.
        assert_eq!(idx.keypoint_for_time(f64::INFINITY), Some(2));
        // -inf → no "at or before" answer exists.
        assert_eq!(idx.keypoint_for_time(f64::NEG_INFINITY), None);
    }

    #[test]
    fn skel_index_keypoint_for_time_empty_or_unknown_denominator() {
        // Empty index → None.
        let empty = SkelIndex::new(1, 48_000);
        assert_eq!(empty.keypoint_for_time(1.0), None);
        // Denominator 0 → None.
        let mut unknown_denom = SkelIndex::new(1, 0);
        unknown_denom.push(0, 0);
        assert_eq!(unknown_denom.keypoint_for_time(1.0), None);
    }

    #[test]
    fn skel_index_keypoint_seconds_with_negative_timestamps() {
        // §"Keyframe index packets" timestamps are i64 numerators,
        // signed: a stream whose `presentation_time` precedes
        // granule 0 yields negative keypoint timestamps. The typed
        // accessors must hand them through with sign preserved.
        let mut idx = SkelIndex::new(11, 1_000);
        idx.first_sample_time = -3_000;
        idx.last_sample_time = 5_000;
        idx.push(0, -2_500);
        idx.push(8192, 0);
        idx.push(16384, 4_000);
        assert_eq!(idx.first_sample_seconds(), Some(-3.0));
        assert_eq!(idx.last_sample_seconds(), Some(5.0));
        assert_eq!(idx.duration_seconds(), Some(8.0));
        assert_eq!(idx.keypoint_seconds(0), Some(-2.5));
        assert_eq!(idx.keypoint_seconds(2), Some(4.0));
        // Searching at -1.0 finds keypoint 0 (the only one at-or-before).
        assert_eq!(idx.keypoint_for_time(-1.0), Some(0));
        // Searching at -2.5 (exact match on keypoint 0) returns 0.
        assert_eq!(idx.keypoint_for_time(-2.5), Some(0));
        // Searching at -10.0 precedes every keypoint.
        assert_eq!(idx.keypoint_for_time(-10.0), None);
    }

    #[test]
    fn skel_index_keypoint_seconds_one_keypoint_at_zero() {
        // Single keypoint at t=0 — the binary search must still
        // honor the spec's "<=" boundary at the exact match.
        let mut idx = SkelIndex::new(99, 48_000);
        idx.push(0, 0);
        assert_eq!(idx.keypoint_for_time(0.0), Some(0));
        assert_eq!(idx.keypoint_for_time(0.5), Some(0));
        // Below 0 → no at-or-before answer.
        assert_eq!(idx.keypoint_for_time(-1e-6), None);
    }

    #[test]
    fn keypoint_seconds_method_handles_zero_denominator() {
        let kp = KeyPoint {
            offset: 1024,
            timestamp: 1_000_000,
        };
        // Non-zero denominator → exact division.
        assert_eq!(kp.seconds(1_000_000), 1.0);
        // Zero denominator → 0.0 (matches `Rational::to_seconds`
        // behaviour; the `Option`-returning typed accessor is
        // `SkelIndex::keypoint_seconds`).
        assert_eq!(kp.seconds(0), 0.0);
    }

    // -------------------------------------------------------------
    // Role / Language typed message-header accessors
    // (docs/container/ogg/ogg-skeleton-message-headers.wiki).
    // -------------------------------------------------------------

    fn bone_with(name: &str, value: &str) -> FisBone {
        let mut b = FisBone::new(1, Rational::new(48_000, 1));
        b.set_header(name, value);
        b
    }

    #[test]
    fn role_returns_none_when_header_absent() {
        let b = FisBone::new(1, Rational::new(48_000, 1));
        assert!(b.role().is_none());
        assert!(b.languages().is_none());
    }

    #[test]
    fn role_parses_each_documented_text_role() {
        // Every text/* bullet from the wiki §Role list.
        let cases = [
            ("text/caption", RoleKind::TextCaption),
            ("text/subtitle", RoleKind::TextSubtitle),
            ("text/textaudiodesc", RoleKind::TextTextAudioDesc),
            ("text/karaoke", RoleKind::TextKaraoke),
            ("text/chapters", RoleKind::TextChapters),
            ("text/tickertext", RoleKind::TextTickerText),
            ("text/lyrics", RoleKind::TextLyrics),
            ("text/metadata", RoleKind::TextMetadata),
            ("text/annotation", RoleKind::TextAnnotation),
            ("text/linguistic", RoleKind::TextLinguistic),
        ];
        for (wire, expected) in cases {
            let r = Role::parse(wire);
            assert_eq!(r.kind, expected, "{wire}");
            assert!(r.parameters.is_empty(), "{wire}");
            assert!(r.kind.is_text(), "{wire}");
            assert!(!r.kind.is_video(), "{wire}");
            assert!(!r.kind.is_audio(), "{wire}");
            assert_eq!(r.kind.as_wire(), wire);
        }
    }

    #[test]
    fn role_parses_each_documented_video_role() {
        // Every video/* bullet from the wiki §Role list.
        let cases = [
            ("video/main", RoleKind::VideoMain),
            ("video/alternate", RoleKind::VideoAlternate),
            ("video/sign", RoleKind::VideoSign),
            ("video/captioned", RoleKind::VideoCaptioned),
            ("video/subtitled", RoleKind::VideoSubtitled),
        ];
        for (wire, expected) in cases {
            let r = Role::parse(wire);
            assert_eq!(r.kind, expected, "{wire}");
            assert!(r.kind.is_video(), "{wire}");
            assert!(!r.kind.is_text(), "{wire}");
            assert!(!r.kind.is_audio(), "{wire}");
            assert_eq!(r.kind.as_wire(), wire);
        }
    }

    #[test]
    fn role_parses_each_documented_audio_role() {
        // Every audio/* bullet from the wiki §Role list.
        let cases = [
            ("audio/main", RoleKind::AudioMain),
            ("audio/alternate", RoleKind::AudioAlternate),
            ("audio/dub", RoleKind::AudioDub),
            ("audio/audiodesc", RoleKind::AudioAudioDesc),
            ("audio/described", RoleKind::AudioDescribed),
            ("audio/music", RoleKind::AudioMusic),
            ("audio/speech", RoleKind::AudioSpeech),
            ("audio/sfx", RoleKind::AudioSfx),
            ("audio/commentary", RoleKind::AudioCommentary),
        ];
        for (wire, expected) in cases {
            let r = Role::parse(wire);
            assert_eq!(r.kind, expected, "{wire}");
            assert!(r.kind.is_audio(), "{wire}");
            assert!(!r.kind.is_text(), "{wire}");
            assert!(!r.kind.is_video(), "{wire}");
            assert_eq!(r.kind.as_wire(), wire);
        }
    }

    #[test]
    fn role_parses_with_wiki_example_parameter() {
        // Wiki §Role: "video/alternate;angle=nw" is shown verbatim as
        // the example of a parameterised role tag.
        let r = Role::parse("video/alternate;angle=nw");
        assert_eq!(r.kind, RoleKind::VideoAlternate);
        assert_eq!(r.parameters.len(), 1);
        assert_eq!(r.parameter("angle"), Some("nw"));
        // Case-insensitive parameter lookup.
        assert_eq!(r.parameter("Angle"), Some("nw"));
        assert_eq!(r.parameter("ANGLE"), Some("nw"));
        // Unknown parameter → None.
        assert_eq!(r.parameter("missing"), None);
    }

    #[test]
    fn role_case_insensitive_on_tag_and_whitespace_tolerant() {
        // Spec-style HTTP framing: capitalisation of the role tag is
        // not mandated by the wiki, and the trailing/leading whitespace
        // that the parser already trims at the header level may also
        // appear inside the value.
        let r = Role::parse("  Video/Alternate ; ANGLE = nw ");
        assert_eq!(r.kind, RoleKind::VideoAlternate);
        assert_eq!(r.parameter("angle"), Some("nw"));
    }

    #[test]
    fn role_unknown_tag_maps_to_other_preserving_case() {
        // "Other roles are possible, too" per the wiki — the parser
        // must round-trip an unknown tag without losing it.
        let r = Role::parse("application/vendor-x;profile=2");
        assert_eq!(r.kind, RoleKind::Other("application/vendor-x".to_string()));
        assert_eq!(r.kind.as_wire(), "application/vendor-x");
        assert_eq!(r.parameter("profile"), Some("2"));
        assert!(!r.kind.is_text() && !r.kind.is_video() && !r.kind.is_audio());
    }

    #[test]
    fn role_parameter_without_equals_yields_empty_value() {
        let r = Role::parse("audio/main;flag");
        assert_eq!(r.kind, RoleKind::AudioMain);
        assert_eq!(r.parameters, vec![("flag".to_string(), String::new())]);
        assert_eq!(r.parameter("flag"), Some(""));
    }

    #[test]
    fn role_multiple_parameters_preserve_order_and_lookup() {
        let r = Role::parse("video/alternate;angle=nw;quality=high");
        assert_eq!(
            r.parameters,
            vec![
                ("angle".to_string(), "nw".to_string()),
                ("quality".to_string(), "high".to_string()),
            ]
        );
        assert_eq!(r.parameter("Quality"), Some("high"));
    }

    #[test]
    fn role_through_fisbone_accessor_round_trips_via_set_header() {
        let b = bone_with("Role", "video/alternate;angle=nw");
        let r = b.role().expect("role present");
        assert_eq!(r.kind, RoleKind::VideoAlternate);
        assert_eq!(r.parameter("angle"), Some("nw"));
    }

    #[test]
    fn role_lookup_is_case_insensitive_on_header_name() {
        // `FisBone::header` already lower-cases; make sure the typed
        // accessor inherits that.
        let mut b = FisBone::new(1, Rational::new(48_000, 1));
        b.set_header("role", "audio/main");
        assert_eq!(b.role().map(|r| r.kind), Some(RoleKind::AudioMain));
    }

    #[test]
    fn languages_parses_wiki_example() {
        // Wiki §Language: "Language: en-US, fr" — dominating language
        // first, comma-separated list after.
        let b = bone_with("Language", "en-US, fr");
        assert_eq!(b.languages(), Some(vec!["en-US", "fr"]));
    }

    #[test]
    fn languages_handles_single_tag() {
        let b = bone_with("Language", "de-DE");
        assert_eq!(b.languages(), Some(vec!["de-DE"]));
    }

    #[test]
    fn languages_drops_empty_fragments() {
        // Trailing commas / double commas surface in real-world files
        // even when not spec-conformant. The parser is liberal in what
        // it accepts and just drops the empties.
        let b = bone_with("Language", "en-US,, fr,");
        assert_eq!(b.languages(), Some(vec!["en-US", "fr"]));
    }

    #[test]
    fn languages_trims_surrounding_whitespace_on_each_tag() {
        let b = bone_with("Language", "   en-US ,   fr   ");
        assert_eq!(b.languages(), Some(vec!["en-US", "fr"]));
    }

    #[test]
    fn languages_returns_empty_vec_when_value_is_blank() {
        // Blank value → header is present but expands to zero tags
        // after splitting. Distinguishable from "header absent" via
        // the outer `Option` wrapper.
        let b = bone_with("Language", "");
        assert_eq!(b.languages(), Some(vec![]));
    }

    // -------------------------------------------------------------
    // Typed `Altitude` accessor
    // (docs/container/ogg/ogg-skeleton-message-headers.wiki §Altitude).
    // -------------------------------------------------------------

    #[test]
    fn altitude_returns_none_when_header_absent() {
        let b = FisBone::new(1, Rational::new(48_000, 1));
        assert!(b.altitude().is_none());
    }

    #[test]
    fn altitude_parses_wiki_example_value() {
        // Wiki §Altitude worked example: "Altitude: -150" — a CSS
        // z-index-style negative integer.
        let b = bone_with("Altitude", "-150");
        assert_eq!(b.altitude().expect("present").expect("valid"), -150);
    }

    #[test]
    fn altitude_parses_positive_integer() {
        let b = bone_with("Altitude", "42");
        assert_eq!(b.altitude().expect("present").expect("valid"), 42);
    }

    #[test]
    fn altitude_parses_zero() {
        let b = bone_with("Altitude", "0");
        assert_eq!(b.altitude().expect("present").expect("valid"), 0);
    }

    #[test]
    fn altitude_trims_surrounding_whitespace() {
        // The Skeleton message-header block uses HTTP-style framing;
        // surrounding whitespace on the value may appear after the
        // leading-space strip done at parse time. Tolerate it on the
        // typed accessor too.
        let b = bone_with("Altitude", "   -150   ");
        assert_eq!(b.altitude().expect("present").expect("valid"), -150);
    }

    #[test]
    fn altitude_at_i64_bounds_round_trips() {
        // Wiki §Altitude: "unlimited negative and positive numbers".
        // The typed accessor caps at i64 — values at the boundary
        // still parse successfully.
        let b_max = bone_with("Altitude", "9223372036854775807"); // i64::MAX
        assert_eq!(b_max.altitude().expect("present").expect("valid"), i64::MAX);
        let b_min = bone_with("Altitude", "-9223372036854775808"); // i64::MIN
        assert_eq!(b_min.altitude().expect("present").expect("valid"), i64::MIN);
    }

    #[test]
    fn altitude_value_above_i64_max_yields_inner_err() {
        // Past i64::MAX → inner Err so the caller can decide. Stays
        // Some(...) so it's distinguishable from "header absent".
        let b = bone_with("Altitude", "9223372036854775808");
        let parsed = b.altitude().expect("header present");
        assert!(parsed.is_err());
    }

    #[test]
    fn altitude_non_integer_value_yields_inner_err() {
        let b = bone_with("Altitude", "top");
        let parsed = b.altitude().expect("header present");
        assert!(parsed.is_err());
    }

    #[test]
    fn altitude_blank_value_yields_inner_err() {
        // Empty/blank value is "header present but unparseable", not
        // "header absent" — the outer Option still distinguishes them.
        let b = bone_with("Altitude", "");
        let parsed = b.altitude().expect("header present");
        assert!(parsed.is_err());
    }

    #[test]
    fn altitude_lookup_is_case_insensitive_on_header_name() {
        let mut b = FisBone::new(1, Rational::new(48_000, 1));
        b.set_header("altitude", "-1");
        assert_eq!(b.altitude().expect("present").expect("valid"), -1);
        // Mixed case round-trips via the case-insensitive header
        // lookup the underlying FisBone::header provides.
        let mut b2 = FisBone::new(1, Rational::new(48_000, 1));
        b2.set_header("Altitude", "7");
        assert_eq!(b2.altitude().expect("present").expect("valid"), 7);
    }

    #[test]
    fn altitude_decimal_value_yields_inner_err() {
        // CSS z-index is integer-only; a decimal value violates the
        // wiki's "z-index in CSS" specification and must surface as
        // a parse error rather than silently truncating.
        let b = bone_with("Altitude", "1.5");
        let parsed = b.altitude().expect("header present");
        assert!(parsed.is_err());
    }

    #[test]
    fn altitude_through_set_header_replace() {
        // The typed accessor reflects the most-recent set_header value
        // (case-insensitive replacement on the underlying storage).
        let mut b = FisBone::new(1, Rational::new(48_000, 1));
        b.set_header("Altitude", "10");
        b.set_header("ALTITUDE", "-3");
        assert_eq!(b.altitude().expect("present").expect("valid"), -3);
    }

    // ---------------- Display-hint typed accessor ----------------
    //
    // Wiki reference: docs/container/ogg/ogg-skeleton-message-headers.wiki
    // §Display-hint. Three documented hint forms — pip(...), mask(...),
    // transparent(...) — plus a forward-compatible Other fall-through.

    #[test]
    fn display_hint_returns_none_when_header_absent() {
        let b = FisBone::new(1, Rational::new(48_000, 1));
        assert!(b.display_hint().is_none());
    }

    #[test]
    fn display_hint_parses_wiki_pip_two_arg_percent_example() {
        // Wiki example: "Display-hint: pip(20%,20%)".
        let b = bone_with("Display-hint", "pip(20%,20%)");
        let parsed = b.display_hint().expect("present").expect("valid");
        assert_eq!(
            parsed,
            DisplayHint::Pip {
                x: DisplayCoord::Percent(20.0),
                y: DisplayCoord::Percent(20.0),
                width: None,
                height: None,
            }
        );
    }

    #[test]
    fn display_hint_parses_wiki_pip_four_arg_pixel_example() {
        // Wiki example: "Display-hint: pip(40,40,690,60)".
        let b = bone_with("Display-hint", "pip(40,40,690,60)");
        let parsed = b.display_hint().expect("present").expect("valid");
        assert_eq!(
            parsed,
            DisplayHint::Pip {
                x: DisplayCoord::Pixels(40),
                y: DisplayCoord::Pixels(40),
                width: Some(DisplayCoord::Pixels(690)),
                height: Some(DisplayCoord::Pixels(60)),
            }
        );
    }

    #[test]
    fn display_hint_pip_rejects_three_arguments() {
        // The wiki enumerates 2- and 4-arg pip forms; 3 args is malformed.
        let b = bone_with("Display-hint", "pip(10,20,30)");
        let parsed = b.display_hint().expect("present");
        assert!(parsed.is_err());
    }

    #[test]
    fn display_hint_pip_mixed_pixel_and_percent() {
        // The wiki documents both pixel and percent shapes; mixing them is
        // not explicitly forbidden ("x, y, w, and h can be specified in
        // percentage") so we parse each coordinate independently.
        let b = bone_with("Display-hint", "pip(50%,30,75%,20)");
        let parsed = b.display_hint().expect("present").expect("valid");
        assert_eq!(
            parsed,
            DisplayHint::Pip {
                x: DisplayCoord::Percent(50.0),
                y: DisplayCoord::Pixels(30),
                width: Some(DisplayCoord::Percent(75.0)),
                height: Some(DisplayCoord::Pixels(20)),
            }
        );
    }

    #[test]
    fn display_hint_parses_wiki_mask_one_arg_example() {
        // Wiki example: "Display-hint: mask(http://www.example.com/image.png)".
        let b = bone_with("Display-hint", "mask(http://www.example.com/image.png)");
        let parsed = b.display_hint().expect("present").expect("valid");
        assert_eq!(
            parsed,
            DisplayHint::Mask {
                image: "http://www.example.com/image.png".to_string(),
                x: None,
                y: None,
                width: None,
                height: None,
            }
        );
    }

    #[test]
    fn display_hint_parses_wiki_mask_three_arg_example() {
        // Wiki example: "Display-hint: mask(http://.../image.png,30%,25%)".
        let b = bone_with(
            "Display-hint",
            "mask(http://www.example.com/image.png,30%,25%)",
        );
        let parsed = b.display_hint().expect("present").expect("valid");
        assert_eq!(
            parsed,
            DisplayHint::Mask {
                image: "http://www.example.com/image.png".to_string(),
                x: Some(DisplayCoord::Percent(30.0)),
                y: Some(DisplayCoord::Percent(25.0)),
                width: None,
                height: None,
            }
        );
    }

    #[test]
    fn display_hint_parses_wiki_mask_five_arg_example() {
        // Wiki example: "Display-hint: mask(http://.../image.png,20,20,400,320)".
        let b = bone_with(
            "Display-hint",
            "mask(http://www.example.com/image.png,20,20,400,320)",
        );
        let parsed = b.display_hint().expect("present").expect("valid");
        assert_eq!(
            parsed,
            DisplayHint::Mask {
                image: "http://www.example.com/image.png".to_string(),
                x: Some(DisplayCoord::Pixels(20)),
                y: Some(DisplayCoord::Pixels(20)),
                width: Some(DisplayCoord::Pixels(400)),
                height: Some(DisplayCoord::Pixels(320)),
            }
        );
    }

    #[test]
    fn display_hint_mask_rejects_two_arguments() {
        // The wiki enumerates 1-, 3- and 5-arg mask forms; 2-arg is
        // malformed.
        let b = bone_with("Display-hint", "mask(url,10%)");
        let parsed = b.display_hint().expect("present");
        assert!(parsed.is_err());
    }

    #[test]
    fn display_hint_parses_wiki_transparent_examples() {
        // Wiki examples: "Display-hint: transparent(25%)" and
        // "Display-hint: transparent(7%)".
        for (raw, expect) in [("transparent(25%)", 25u8), ("transparent(7%)", 7u8)] {
            let b = bone_with("Display-hint", raw);
            let parsed = b.display_hint().expect("present").expect("valid");
            assert_eq!(parsed, DisplayHint::Transparent { percent: expect });
        }
    }

    #[test]
    fn display_hint_transparent_accepts_zero_and_hundred_bounds() {
        for v in [0u8, 100u8] {
            let raw = format!("transparent({v}%)");
            let b = bone_with("Display-hint", &raw);
            let parsed = b.display_hint().expect("present").expect("valid");
            assert_eq!(parsed, DisplayHint::Transparent { percent: v });
        }
    }

    #[test]
    fn display_hint_transparent_rejects_value_above_100() {
        // Wiki spec: "int value between 0 and 100".
        let b = bone_with("Display-hint", "transparent(150%)");
        let parsed = b.display_hint().expect("present");
        assert!(parsed.is_err());
    }

    #[test]
    fn display_hint_transparent_rejects_non_integer() {
        // The wiki spells "int value" — fractional percent is malformed.
        let b = bone_with("Display-hint", "transparent(25.5%)");
        let parsed = b.display_hint().expect("present");
        assert!(parsed.is_err());
    }

    #[test]
    fn display_hint_unknown_tag_yields_other() {
        // The wiki phrasing "Currently proposed hints are:" leaves room
        // for vendor / forward-compatible hint tags. Anything not in the
        // documented enumeration is surfaced as Other.
        let b = bone_with("Display-hint", "vendor-zoom(2.0)");
        let parsed = b.display_hint().expect("present").expect("valid");
        assert_eq!(
            parsed,
            DisplayHint::Other {
                tag: "vendor-zoom".to_string(),
                arguments: vec!["2.0".to_string()],
            }
        );
    }

    #[test]
    fn display_hint_unknown_tag_preserves_multiple_arguments() {
        let b = bone_with("Display-hint", "fancy(a,b,c,d)");
        let parsed = b.display_hint().expect("present").expect("valid");
        assert_eq!(
            parsed,
            DisplayHint::Other {
                tag: "fancy".to_string(),
                arguments: vec![
                    "a".to_string(),
                    "b".to_string(),
                    "c".to_string(),
                    "d".to_string(),
                ],
            }
        );
    }

    #[test]
    fn display_hint_rejects_missing_open_paren() {
        let b = bone_with("Display-hint", "pip 20%,20%");
        let parsed = b.display_hint().expect("present");
        assert!(parsed.is_err());
    }

    #[test]
    fn display_hint_rejects_missing_close_paren() {
        let b = bone_with("Display-hint", "pip(20%,20%");
        let parsed = b.display_hint().expect("present");
        assert!(parsed.is_err());
    }

    #[test]
    fn display_hint_rejects_empty_tag() {
        // A value of "(20%,20%)" has no tag — every documented form
        // names its hint, so reject the empty-tag shape.
        let b = bone_with("Display-hint", "(20%,20%)");
        let parsed = b.display_hint().expect("present");
        assert!(parsed.is_err());
    }

    #[test]
    fn display_hint_trims_surrounding_whitespace_on_value_and_args() {
        // The HTTP-style framing in the rest of the message-header block
        // may inject a leading space after the colon; argument tokens
        // commonly carry stray whitespace too.
        let b = bone_with("Display-hint", "  pip( 10 , 20 , 30 , 40 )  ");
        let parsed = b.display_hint().expect("present").expect("valid");
        assert_eq!(
            parsed,
            DisplayHint::Pip {
                x: DisplayCoord::Pixels(10),
                y: DisplayCoord::Pixels(20),
                width: Some(DisplayCoord::Pixels(30)),
                height: Some(DisplayCoord::Pixels(40)),
            }
        );
    }

    #[test]
    fn display_hint_pip_lowercase_tag_match_is_case_insensitive() {
        // The wiki spells tags lower-case; tolerate upper-case spellings
        // the way the rest of the message-header block tolerates
        // case-mismatched header names.
        let b = bone_with("Display-hint", "PIP(10%,10%)");
        let parsed = b.display_hint().expect("present").expect("valid");
        assert_eq!(
            parsed,
            DisplayHint::Pip {
                x: DisplayCoord::Percent(10.0),
                y: DisplayCoord::Percent(10.0),
                width: None,
                height: None,
            }
        );
    }

    #[test]
    fn display_hint_lookup_is_case_insensitive_on_header_name() {
        // The accessor delegates to FisBone::header, which the rest of
        // the typed accessors already verify is case-insensitive.
        let mut b = FisBone::new(1, Rational::new(48_000, 1));
        b.set_header("display-hint", "transparent(40%)");
        let parsed = b.display_hint().expect("present").expect("valid");
        assert_eq!(parsed, DisplayHint::Transparent { percent: 40 });

        let mut b2 = FisBone::new(1, Rational::new(48_000, 1));
        b2.set_header("DISPLAY-HINT", "transparent(60%)");
        let parsed2 = b2.display_hint().expect("present").expect("valid");
        assert_eq!(parsed2, DisplayHint::Transparent { percent: 60 });
    }

    #[test]
    fn display_hint_through_set_header_replace() {
        // Most-recent set_header value wins, via case-insensitive
        // replacement on the underlying storage.
        let mut b = FisBone::new(1, Rational::new(48_000, 1));
        b.set_header("Display-hint", "transparent(10%)");
        b.set_header("DISPLAY-HINT", "transparent(80%)");
        let parsed = b.display_hint().expect("present").expect("valid");
        assert_eq!(parsed, DisplayHint::Transparent { percent: 80 });
    }

    #[test]
    fn display_hint_pip_rejects_non_numeric_coordinate() {
        let b = bone_with("Display-hint", "pip(abc,10%)");
        let parsed = b.display_hint().expect("present");
        assert!(parsed.is_err());
    }

    #[test]
    fn display_hint_mask_preserves_image_url_as_first_arg_verbatim() {
        // The mask image arg is documented as a URL (e.g. http://...);
        // the parser must NOT try to coerce it into a numeric coordinate.
        let b = bone_with("Display-hint", "mask(file:///opt/share/masks/circle.png)");
        let parsed = b.display_hint().expect("present").expect("valid");
        assert_eq!(
            parsed,
            DisplayHint::Mask {
                image: "file:///opt/share/masks/circle.png".to_string(),
                x: None,
                y: None,
                width: None,
                height: None,
            }
        );
    }

    // -------------------------------------------------------------
    // Typed `Content-Type` accessor
    // (docs/container/ogg/ogg-skeleton-message-headers.wiki §Content-type,
    //  docs/container/ogg/ogg-skeleton-{3,4}.0.md §fisbone "Content-Type:
    //  audio/vorbis" worked example).
    // -------------------------------------------------------------

    #[test]
    fn content_type_returns_none_when_header_absent() {
        let b = FisBone::new(1, Rational::new(48_000, 1));
        assert!(b.content_type().is_none());
    }

    #[test]
    fn content_type_parses_wiki_audio_vorbis_example() {
        // docs/container/ogg/ogg-skeleton-4.0.md §3 worked example:
        // "Content-Type: audio/vorbis".
        let b = bone_with("Content-Type", "audio/vorbis");
        let ct = b.content_type().expect("present").expect("valid");
        assert_eq!(ct.kind, ContentTypeKind::Audio);
        assert!(ct.kind.is_audio());
        assert!(!ct.kind.is_video());
        assert!(!ct.kind.is_text());
        assert_eq!(ct.subtype, "vorbis");
        assert!(ct.subtype_eq("Vorbis"));
        assert!(ct.parameters.is_empty());
        assert_eq!(ct.kind.as_wire(), "audio");
    }

    #[test]
    fn content_type_parses_wiki_video_theora_example() {
        // docs/container/ogg/ogg-skeleton-4.0.md §3 worked example
        // explicitly names "video/theora" alongside "audio/vorbis" as
        // the canonical content types Skeleton's Content-Type field
        // carries.
        let b = bone_with("Content-Type", "video/theora");
        let ct = b.content_type().expect("present").expect("valid");
        assert_eq!(ct.kind, ContentTypeKind::Video);
        assert!(ct.kind.is_video());
        assert_eq!(ct.subtype, "theora");
        assert_eq!(ct.kind.as_wire(), "video");
    }

    #[test]
    fn content_type_each_well_known_top_level_kind() {
        let cases = [
            ("audio/opus", ContentTypeKind::Audio, "opus"),
            ("video/theora", ContentTypeKind::Video, "theora"),
            ("text/caption", ContentTypeKind::Text, "caption"),
            ("image/png", ContentTypeKind::Image, "png"),
            ("application/kate", ContentTypeKind::Application, "kate"),
        ];
        for (wire, expected_kind, expected_subtype) in cases {
            let b = bone_with("Content-Type", wire);
            let ct = b.content_type().expect("present").expect("valid");
            assert_eq!(ct.kind, expected_kind, "{wire}");
            assert_eq!(ct.subtype, expected_subtype, "{wire}");
        }
    }

    #[test]
    fn content_type_unknown_top_level_maps_to_other_preserving_case() {
        // Wiki §Role notes "mime types don't always provide the right
        // main content type (e.g. application/kate is semantically a
        // text format)" — same principle applies on the Content-Type
        // side: an unrecognised top-level type still round-trips
        // verbatim into ContentTypeKind::Other.
        let b = bone_with("Content-Type", "Multipart/Mixed");
        let ct = b.content_type().expect("present").expect("valid");
        // Top-level type is case-insensitive on the bucket match, but
        // the as-written value survives inside Other for unknown types
        // — and Mixed lives in `subtype` verbatim.
        match ct.kind {
            ContentTypeKind::Other(s) => assert_eq!(s, "Multipart"),
            other => panic!("expected Other, got {:?}", other),
        }
        assert_eq!(ct.subtype, "Mixed");
    }

    #[test]
    fn content_type_top_level_match_is_case_insensitive() {
        // RFC 2045 § 5.1: "the type, subtype, and parameter names are
        // not case sensitive". The bucket match folds case before the
        // ContentTypeKind lookup, so AUDIO/Vorbis still classifies as
        // ContentTypeKind::Audio.
        let b = bone_with("Content-Type", "AUDIO/Vorbis");
        let ct = b.content_type().expect("present").expect("valid");
        assert_eq!(ct.kind, ContentTypeKind::Audio);
        // Subtype casing is preserved as-written; subtype_eq folds it.
        assert_eq!(ct.subtype, "Vorbis");
        assert!(ct.subtype_eq("vorbis"));
        assert!(ct.subtype_eq("VORBIS"));
    }

    #[test]
    fn content_type_parses_with_rfc2045_parameter() {
        // RFC 2045 § 5.1 allows MIME parameters; encoders sometimes
        // emit them on Skeleton fisbones (e.g. "audio/ogg;codecs=opus").
        let b = bone_with("Content-Type", "audio/ogg;codecs=opus");
        let ct = b.content_type().expect("present").expect("valid");
        assert_eq!(ct.kind, ContentTypeKind::Audio);
        assert_eq!(ct.subtype, "ogg");
        assert_eq!(ct.parameters.len(), 1);
        assert_eq!(ct.parameter("codecs"), Some("opus"));
    }

    #[test]
    fn content_type_parameter_lookup_is_case_insensitive() {
        // RFC 2045 § 5.1: "parameter names are not case sensitive".
        let b = bone_with("Content-Type", "audio/ogg;Codecs=opus");
        let ct = b.content_type().expect("present").expect("valid");
        assert_eq!(ct.parameter("codecs"), Some("opus"));
        assert_eq!(ct.parameter("CODECS"), Some("opus"));
        assert_eq!(ct.parameter("Codecs"), Some("opus"));
        assert_eq!(ct.parameter("missing"), None);
    }

    #[test]
    fn content_type_multiple_parameters_preserve_order_and_lookup() {
        let b = bone_with("Content-Type", "video/mp4;codecs=avc1.42E01E;profiles=mp42");
        let ct = b.content_type().expect("present").expect("valid");
        assert_eq!(ct.kind, ContentTypeKind::Video);
        assert_eq!(ct.subtype, "mp4");
        assert_eq!(ct.parameters.len(), 2);
        // Order preserved as written.
        assert_eq!(ct.parameters[0].0, "codecs");
        assert_eq!(ct.parameters[0].1, "avc1.42E01E");
        assert_eq!(ct.parameters[1].0, "profiles");
        assert_eq!(ct.parameters[1].1, "mp42");
    }

    #[test]
    fn content_type_parameter_without_equals_yields_empty_value() {
        // Mirrors `Role::parse` parameter handling: parameter token
        // without an `=` becomes (key, "").
        let b = bone_with("Content-Type", "audio/ogg;flag");
        let ct = b.content_type().expect("present").expect("valid");
        assert_eq!(ct.parameters.len(), 1);
        assert_eq!(ct.parameter("flag"), Some(""));
    }

    #[test]
    fn content_type_empty_parameter_segments_are_dropped() {
        // Trailing `;` or doubled `;;` should not become empty
        // parameter entries — the parser tolerates loose serialisation.
        let b = bone_with("Content-Type", "audio/ogg;;codecs=opus;");
        let ct = b.content_type().expect("present").expect("valid");
        assert_eq!(ct.parameters.len(), 1);
        assert_eq!(ct.parameter("codecs"), Some("opus"));
    }

    #[test]
    fn content_type_trims_surrounding_whitespace_on_value_and_params() {
        // HTTP-style framing tolerance — same as the other typed accessors.
        let b = bone_with("Content-Type", "   audio/vorbis ;  codecs = ogg  ");
        let ct = b.content_type().expect("present").expect("valid");
        assert_eq!(ct.kind, ContentTypeKind::Audio);
        assert_eq!(ct.subtype, "vorbis");
        assert_eq!(ct.parameter("codecs"), Some("ogg"));
    }

    #[test]
    fn content_type_skeleton_self_application_subtype() {
        // Skeleton's own bitstream is sometimes carried with
        // "application/x-ogg-skeleton" or similar vendor subtypes.
        // Verify the application/* bucket plus x- subtype prefix.
        let b = bone_with("Content-Type", "application/x-ogg-skeleton");
        let ct = b.content_type().expect("present").expect("valid");
        assert!(ct.kind.is_application());
        assert_eq!(ct.subtype, "x-ogg-skeleton");
    }

    #[test]
    fn content_type_missing_slash_yields_inner_err() {
        // "audio" alone is not a valid MIME type; without a subtype
        // there's no way to tell vorbis apart from opus, so the parser
        // refuses to guess.
        let b = bone_with("Content-Type", "audio");
        let parsed = b.content_type().expect("present");
        assert!(parsed.is_err());
    }

    #[test]
    fn content_type_empty_value_yields_inner_err() {
        let b = bone_with("Content-Type", "");
        let parsed = b.content_type().expect("present");
        assert!(parsed.is_err());
    }

    #[test]
    fn content_type_empty_subtype_yields_inner_err() {
        let b = bone_with("Content-Type", "audio/");
        let parsed = b.content_type().expect("present");
        assert!(parsed.is_err());
    }

    #[test]
    fn content_type_empty_top_level_yields_inner_err() {
        let b = bone_with("Content-Type", "/vorbis");
        let parsed = b.content_type().expect("present");
        assert!(parsed.is_err());
    }

    #[test]
    fn content_type_blank_value_with_whitespace_yields_inner_err() {
        // Surrounding whitespace is trimmed, so "   " collapses to ""
        // and triggers the empty-value rejection.
        let b = bone_with("Content-Type", "   ");
        let parsed = b.content_type().expect("present");
        assert!(parsed.is_err());
    }

    #[test]
    fn content_type_lookup_is_case_insensitive_on_header_name() {
        // FisBone::header is case-insensitive; verify the typed accessor
        // inherits that behaviour.
        let mut b = FisBone::new(1, Rational::new(48_000, 1));
        b.set_header("content-type", "audio/opus");
        let ct = b.content_type().expect("present").expect("valid");
        assert_eq!(ct.kind, ContentTypeKind::Audio);
        assert_eq!(ct.subtype, "opus");

        let mut b2 = FisBone::new(1, Rational::new(48_000, 1));
        b2.set_header("CONTENT-TYPE", "video/theora");
        let ct2 = b2.content_type().expect("present").expect("valid");
        assert_eq!(ct2.kind, ContentTypeKind::Video);
        assert_eq!(ct2.subtype, "theora");
    }

    #[test]
    fn content_type_through_set_header_replace() {
        // Most-recent set_header value wins, via case-insensitive
        // replacement on the underlying storage.
        let mut b = FisBone::new(1, Rational::new(48_000, 1));
        b.set_header("Content-Type", "audio/vorbis");
        b.set_header("content-type", "audio/opus");
        let ct = b.content_type().expect("present").expect("valid");
        assert_eq!(ct.kind, ContentTypeKind::Audio);
        assert_eq!(ct.subtype, "opus");
        assert_eq!(b.headers.len(), 1, "set_header should replace, not append");
    }

    #[test]
    fn content_type_kind_predicates_are_exclusive() {
        // Sanity: exactly one of is_{audio,video,text,image,application}
        // is true for the well-known kinds.
        let cases = [
            ContentTypeKind::Audio,
            ContentTypeKind::Video,
            ContentTypeKind::Text,
            ContentTypeKind::Image,
            ContentTypeKind::Application,
        ];
        for k in cases {
            let votes = [
                k.is_audio(),
                k.is_video(),
                k.is_text(),
                k.is_image(),
                k.is_application(),
            ]
            .iter()
            .filter(|b| **b)
            .count();
            assert_eq!(votes, 1, "{:?} matched multiple predicates", k);
        }
        // ContentTypeKind::Other returns false for every predicate.
        let other = ContentTypeKind::Other("multipart".to_string());
        assert!(!other.is_audio());
        assert!(!other.is_video());
        assert!(!other.is_text());
        assert!(!other.is_image());
        assert!(!other.is_application());
    }

    // -------- Title typed accessor --------------------------------------
    //
    // Worked-example coverage for the wiki §Title section
    // (`docs/container/ogg/ogg-skeleton-message-headers.wiki`): the
    // sole on-record example is `Title: "the French audio track for
    // the movie"` and the field is a free-text track-content
    // description. The tests below pin every shape the wiki leaves
    // open: with and without surrounding quotes, with and without
    // surrounding whitespace, the empty `""` collapse case, an inner
    // quote that must survive verbatim, an unbalanced quote that must
    // *not* be stripped, header-absent vs. header-present, and the
    // round-trip behaviour through `set_header` / `to_bytes` / `parse`.

    #[test]
    fn title_wiki_worked_example_strips_outer_quotes() {
        let mut b = FisBone::new(1, Rational::new(48_000, 1));
        b.set_header("Title", "\"the French audio track for the movie\"");
        let t = b.title().expect("title header present");
        assert_eq!(t.raw(), "\"the French audio track for the movie\"");
        assert_eq!(t.display(), "the French audio track for the movie");
        assert!(!t.is_empty());
    }

    #[test]
    fn title_unquoted_value_round_trips_through_both_views() {
        let mut b = FisBone::new(1, Rational::new(48_000, 1));
        b.set_header("Title", "track 3 — Verbier 2019");
        let t = b.title().expect("title header present");
        assert_eq!(t.raw(), "track 3 — Verbier 2019");
        // No surrounding quotes → display() === raw().
        assert_eq!(t.display(), "track 3 — Verbier 2019");
    }

    #[test]
    fn title_trims_surrounding_whitespace_on_value() {
        let mut b = FisBone::new(1, Rational::new(48_000, 1));
        // Pretend an upstream encoder padded the value with stray spaces
        // (the HTTP-style framing allows a leading space after the
        // colon already; this exercises the extra trim on either side).
        b.set_header("Title", "   padded title   ");
        let t = b.title().expect("title header present");
        assert_eq!(t.raw(), "padded title");
    }

    #[test]
    fn title_empty_quoted_value_collapses_to_empty_display() {
        let mut b = FisBone::new(1, Rational::new(48_000, 1));
        b.set_header("Title", "\"\"");
        let t = b.title().expect("title header present");
        assert_eq!(t.raw(), "\"\"");
        assert_eq!(t.display(), "");
    }

    #[test]
    fn title_inner_quote_is_preserved_through_display() {
        // Wiki gives no quoting / escaping rule; an inner quote must
        // round-trip verbatim regardless of whether the outer pair is
        // present. The accessor strips at most one outermost pair.
        let mut b = FisBone::new(1, Rational::new(48_000, 1));
        b.set_header("Title", "\"the \"main\" audio\"");
        let t = b.title().expect("title header present");
        assert_eq!(t.raw(), "\"the \"main\" audio\"");
        // Outer pair stripped; inner pair retained in the middle.
        assert_eq!(t.display(), "the \"main\" audio");
    }

    #[test]
    fn title_unbalanced_quote_is_not_stripped() {
        let mut b = FisBone::new(1, Rational::new(48_000, 1));
        b.set_header("Title", "\"only opens");
        let t = b.title().expect("title header present");
        assert_eq!(t.raw(), "\"only opens");
        assert_eq!(t.display(), "\"only opens");

        b.set_header("Title", "only closes\"");
        let t = b.title().expect("title header present");
        assert_eq!(t.display(), "only closes\"");
    }

    #[test]
    fn title_single_quote_character_is_not_a_balanced_pair() {
        // A lone `"` is one byte: less than 2, so the strip path is
        // never reached. The value must round-trip verbatim.
        let mut b = FisBone::new(1, Rational::new(48_000, 1));
        b.set_header("Title", "\"");
        let t = b.title().expect("title header present");
        assert_eq!(t.raw(), "\"");
        assert_eq!(t.display(), "\"");
    }

    #[test]
    fn title_returns_none_when_header_absent() {
        // No Title header at all → None (vs. None inside Some(_) for
        // an empty header value).
        let b = FisBone::new(1, Rational::new(48_000, 1));
        assert!(b.title().is_none());
    }

    #[test]
    fn title_lookup_is_case_insensitive_on_header_name() {
        // FisBone::header is case-insensitive; Title accessor inherits
        // that. Encoders that emit `title:` or `TITLE:` must still
        // resolve through `title()`.
        let mut b = FisBone::new(1, Rational::new(48_000, 1));
        b.set_header("TITLE", "Lower vs Upper");
        let t = b.title().expect("uppercase header still resolves");
        assert_eq!(t.raw(), "Lower vs Upper");
    }

    #[test]
    fn title_round_trips_through_fisbone_serialization() {
        // FisBone::to_bytes emits CRLF-delimited headers; FisBone::parse
        // re-reads them. The Title accessor must give the same value
        // across the round trip — verifies the typed accessor sits on
        // top of the existing message-header serializer correctly.
        let mut bone = FisBone::new(0xCAFE, Rational::new(48_000, 1));
        bone.num_headers = 3;
        bone.set_header("Content-Type", "audio/vorbis");
        bone.set_header("Title", "\"the French audio track for the movie\"");
        let bytes = bone.to_bytes();
        let back = FisBone::parse(&bytes).expect("fisbone round-trips");
        let t = back.title().expect("Title survives round trip");
        assert_eq!(t.raw(), "\"the French audio track for the movie\"");
        assert_eq!(t.display(), "the French audio track for the movie");
    }

    #[test]
    fn title_set_header_replace_semantics_update_the_typed_view() {
        // set_header with a case-insensitive match replaces the
        // existing value; title() must reflect the new value.
        let mut b = FisBone::new(1, Rational::new(48_000, 1));
        b.set_header("Title", "old value");
        b.set_header("title", "new value");
        let t = b.title().expect("present");
        assert_eq!(t.raw(), "new value");
        // No duplicate header was appended.
        assert_eq!(
            b.headers.len(),
            1,
            "case-insensitive replace must keep the header count at 1"
        );
    }

    #[test]
    fn title_blank_value_yields_empty_raw_and_empty_display() {
        // The wiki places no restriction on blank Title values; a
        // header line `Title:   ` (only whitespace) must surface as
        // an empty raw / display through the trim path.
        let mut b = FisBone::new(1, Rational::new(48_000, 1));
        b.set_header("Title", "   ");
        let t = b.title().expect("present-but-blank");
        assert_eq!(t.raw(), "");
        assert_eq!(t.display(), "");
        assert!(t.is_empty());
    }

    #[test]
    fn name_wiki_worked_example_round_trips_and_is_well_formed() {
        // Wiki §Name worked example: `track[name="Madonna_singing"]` —
        // the on-wire Name value is `Madonna_singing`, which matches
        // the first-character rule ([a-z]) and the following-character
        // rule (letters + `_`).
        let mut b = FisBone::new(1, Rational::new(48_000, 1));
        b.set_header("Name", "Madonna_singing");
        let n = b.name().expect("name header present");
        assert_eq!(n.raw(), "Madonna_singing");
        assert!(n.is_well_formed());
        assert!(!n.is_empty());
    }

    #[test]
    fn name_trims_surrounding_whitespace_on_value() {
        // HTTP-style framing tolerance — same as the other typed
        // accessors. Stray surrounding spaces on the value get dropped
        // before the grammar check runs (otherwise a leading space
        // would always fail the first-character rule).
        let mut b = FisBone::new(1, Rational::new(48_000, 1));
        b.set_header("Name", "   track_a   ");
        let n = b.name().expect("present");
        assert_eq!(n.raw(), "track_a");
        assert!(n.is_well_formed());
    }

    #[test]
    fn name_rejects_digit_prefix() {
        // The wiki's first-character allow-list does NOT include digits;
        // they only appear in the following-character allow-list. A
        // value like `9-track` must therefore fail the grammar check
        // while still round-tripping through raw() so the caller can
        // surface the rejection reason intact.
        let mut b = FisBone::new(1, Rational::new(48_000, 1));
        b.set_header("Name", "9-track");
        let n = b.name().expect("present");
        assert_eq!(n.raw(), "9-track");
        assert!(!n.is_well_formed());
    }

    #[test]
    fn name_rejects_hyphen_prefix() {
        // `-` only appears in the following-character allow-list. A
        // value starting with `-` therefore fails the grammar check.
        let mut b = FisBone::new(1, Rational::new(48_000, 1));
        b.set_header("Name", "-track");
        let n = b.name().expect("present");
        assert!(!n.is_well_formed());
    }

    #[test]
    fn name_rejects_dot_prefix() {
        // `.` only appears in the following-character allow-list.
        let mut b = FisBone::new(1, Rational::new(48_000, 1));
        b.set_header("Name", ".hidden");
        let n = b.name().expect("present");
        assert!(!n.is_well_formed());
    }

    #[test]
    fn name_accepts_underscore_prefix() {
        // `_` IS in the first-character allow-list per the wiki.
        let mut b = FisBone::new(1, Rational::new(48_000, 1));
        b.set_header("Name", "_internal");
        let n = b.name().expect("present");
        assert!(n.is_well_formed());
    }

    #[test]
    fn name_accepts_following_chars_after_letter_start() {
        // Letters + digits + `-` + `.` are all valid as following chars
        // when preceded by a letter / underscore start.
        let mut b = FisBone::new(1, Rational::new(48_000, 1));
        b.set_header("Name", "track-2.audio_main");
        let n = b.name().expect("present");
        assert!(n.is_well_formed());
    }

    #[test]
    fn name_rejects_internal_space() {
        // Space is in neither allow-list. An internal whitespace
        // character is rejected by the grammar check (Name is supposed
        // to be addressable as a stable identifier).
        let mut b = FisBone::new(1, Rational::new(48_000, 1));
        b.set_header("Name", "two words");
        let n = b.name().expect("present");
        assert_eq!(n.raw(), "two words");
        assert!(!n.is_well_formed());
    }

    #[test]
    fn name_rejects_special_punctuation() {
        // `@`, `:`, `/`, `(`, `)`, `,`, `=`, `"` are all outside both
        // allow-lists.
        for raw in ["a@b", "a:b", "a/b", "a(b)", "a,b", "a=b", "a\"b"] {
            let mut b = FisBone::new(1, Rational::new(48_000, 1));
            b.set_header("Name", raw);
            let n = b.name().expect("present");
            assert!(!n.is_well_formed(), "{raw:?} must be rejected");
        }
    }

    #[test]
    fn name_rejects_empty_value_after_trim() {
        // An empty trimmed value has no first character → fails the
        // first-character rule.
        let mut b = FisBone::new(1, Rational::new(48_000, 1));
        b.set_header("Name", "   ");
        let n = b.name().expect("present-but-blank");
        assert_eq!(n.raw(), "");
        assert!(n.is_empty());
        assert!(!n.is_well_formed());
    }

    #[test]
    fn name_accepts_unicode_letter_start() {
        // Wiki's first-character allow-list includes broad Unicode
        // ranges. é (U+00E9) sits inside [#xD8-#xF6] and is therefore
        // a valid first character per the spec.
        let mut b = FisBone::new(1, Rational::new(48_000, 1));
        b.set_header("Name", "épisode");
        let n = b.name().expect("present");
        assert_eq!(n.raw(), "épisode");
        assert!(n.is_well_formed());
    }

    #[test]
    fn name_accepts_middle_dot_as_following_char() {
        // U+00B7 MIDDLE DOT is explicitly listed in the following-
        // character allow-list (the §Name `#xB7` reference, used in
        // Catalan orthography like `Bel·la`).
        let mut b = FisBone::new(1, Rational::new(48_000, 1));
        b.set_header("Name", "Bel\u{B7}la");
        let n = b.name().expect("present");
        assert!(n.is_well_formed());
    }

    #[test]
    fn name_rejects_middle_dot_prefix() {
        // U+00B7 is only in the following-character allow-list, NOT
        // the first-character one. A value starting with `·` fails.
        let mut b = FisBone::new(1, Rational::new(48_000, 1));
        b.set_header("Name", "\u{B7}leading");
        let n = b.name().expect("present");
        assert!(!n.is_well_formed());
    }

    #[test]
    fn name_returns_none_when_header_absent() {
        // No Name header at all → None.
        let b = FisBone::new(1, Rational::new(48_000, 1));
        assert!(b.name().is_none());
    }

    #[test]
    fn name_lookup_is_case_insensitive_on_header_name() {
        // FisBone::header is case-insensitive; Name accessor inherits
        // that. Encoders that emit `name:` or `NAME:` must still
        // resolve through `name()`.
        let mut b = FisBone::new(1, Rational::new(48_000, 1));
        b.set_header("NAME", "track_a");
        let n = b.name().expect("uppercase header still resolves");
        assert_eq!(n.raw(), "track_a");
        assert!(n.is_well_formed());
    }

    #[test]
    fn name_round_trips_through_fisbone_serialization() {
        // FisBone::to_bytes emits CRLF-delimited headers; FisBone::parse
        // re-reads them. The Name accessor must give the same value
        // across the round trip — verifies the typed accessor sits on
        // top of the existing message-header serializer correctly.
        let mut bone = FisBone::new(0xCAFE, Rational::new(48_000, 1));
        bone.num_headers = 3;
        bone.set_header("Content-Type", "audio/vorbis");
        bone.set_header("Name", "Madonna_singing");
        let bytes = bone.to_bytes();
        let back = FisBone::parse(&bytes).expect("fisbone round-trips");
        let n = back.name().expect("Name survives round trip");
        assert_eq!(n.raw(), "Madonna_singing");
        assert!(n.is_well_formed());
    }

    #[test]
    fn name_set_header_replace_semantics_update_the_typed_view() {
        // set_header with a case-insensitive match replaces the
        // existing value; name() must reflect the new value.
        let mut b = FisBone::new(1, Rational::new(48_000, 1));
        b.set_header("Name", "old_name");
        b.set_header("name", "new_name");
        let n = b.name().expect("present");
        assert_eq!(n.raw(), "new_name");
        // No duplicate header was appended.
        assert_eq!(
            b.headers.len(),
            1,
            "case-insensitive replace must keep the header count at 1"
        );
    }

    // ---- granulepos -> time mapping (Skeleton 4.0 §"What decoding-
    //      related information is needed?") ----

    #[test]
    fn extract_granules_unshifted_passthrough() {
        // granuleshift 0 (every audio mapping): granulepos IS the granule.
        let bone = FisBone::new(1, Rational::new(48_000, 1));
        assert_eq!(bone.granuleshift, 0);
        assert_eq!(bone.extract_granules(0), 0);
        assert_eq!(bone.extract_granules(48_000), 48_000);
        assert_eq!(bone.extract_granules(123_456_789), 123_456_789);
    }

    #[test]
    fn extract_granules_theora_shift_sums_halves() {
        // Theora-style packing: high bits = keyframe index, low `shift`
        // bits = offset since the keyframe; the absolute granule is the
        // sum of the two halves (spec GranulePosAndSeeking method).
        let mut bone = FisBone::new(1, Rational::new(30, 1));
        bone.granuleshift = 6;
        // keyframe index 10, offset 5 -> granulepos (10 << 6) | 5 = 645.
        let granulepos = (10i64 << 6) | 5;
        assert_eq!(bone.extract_granules(granulepos), 15);
        // exactly on a keyframe (offset 0).
        let on_key = 7i64 << 6;
        assert_eq!(bone.extract_granules(on_key), 7);
    }

    #[test]
    fn extract_granules_negative_sentinel_passthrough() {
        // RFC 3533 §6 -1 "no packets finish on this page" is returned
        // verbatim, never split.
        let mut bone = FisBone::new(1, Rational::new(48_000, 1));
        bone.granuleshift = 6;
        assert_eq!(bone.extract_granules(-1), -1);
    }

    #[test]
    fn extract_granules_degenerate_shift_clamps_to_zero() {
        let mut bone = FisBone::new(1, Rational::new(48_000, 1));
        bone.granuleshift = 63;
        // shift >= 63 is degenerate; yields 0 rather than overflowing.
        assert_eq!(bone.extract_granules(i64::MAX), 0);
    }

    #[test]
    fn granule_to_seconds_audio_rate() {
        // 48 kHz audio: granulepos 48_000 -> exactly 1.0 s.
        let bone = FisBone::new(1, Rational::new(48_000, 1));
        let s = bone.granule_to_seconds(48_000).expect("rate is usable");
        assert!((s - 1.0).abs() < 1e-9, "got {s}");
        let half = bone.granule_to_seconds(24_000).expect("usable");
        assert!((half - 0.5).abs() < 1e-9, "got {half}");
        assert_eq!(bone.granule_to_seconds(0), Some(0.0));
    }

    #[test]
    fn granule_to_seconds_video_fps_with_shift() {
        // 30 fps Theora: packed granulepos at frame 15 -> 0.5 s.
        let mut bone = FisBone::new(1, Rational::new(30, 1));
        bone.granuleshift = 6;
        let granulepos = (10i64 << 6) | 5; // frame 15
        let s = bone.granule_to_seconds(granulepos).expect("usable");
        assert!((s - 0.5).abs() < 1e-9, "got {s}");
    }

    #[test]
    fn granule_to_seconds_rational_rate_non_integer_fps() {
        // 30000/1001 fps (NTSC): frame 30000 -> 1001 s exactly.
        let bone = FisBone::new(1, Rational::new(30_000, 1001));
        let s = bone.granule_to_seconds(30_000).expect("usable");
        assert!((s - 1001.0).abs() < 1e-6, "got {s}");
    }

    #[test]
    fn granule_to_seconds_none_on_negative_or_unusable_rate() {
        let bone = FisBone::new(1, Rational::new(48_000, 1));
        // -1 sentinel: no timing.
        assert_eq!(bone.granule_to_seconds(-1), None);
        // zero / negative numerator or denominator -> "unknown".
        let zero_den = FisBone::new(1, Rational::new(48_000, 0));
        assert_eq!(zero_den.granule_to_seconds(48_000), None);
        let zero_num = FisBone::new(1, Rational::new(0, 1));
        assert_eq!(zero_num.granule_to_seconds(48_000), None);
        let neg = FisBone::new(1, Rational::new(-48_000, 1));
        assert_eq!(neg.granule_to_seconds(48_000), None);
    }

    #[test]
    fn skeleton_granule_to_seconds_adds_basetime() {
        // basetime maps granule 0 to 3600 s (the pro-video "starts at
        // 01:00:00" case the spec calls out); per-track time is added.
        let mut head = FisHead::new(Version::V4_0);
        head.basetime = Rational::new(3600, 1);
        let mut sk = Skeleton::new();
        sk.set_head(head);
        sk.push_bone(FisBone::new(0x1234, Rational::new(48_000, 1)));
        // granule 48_000 -> 1.0 s track time + 3600 s basetime.
        let s = sk.granule_to_seconds(0x1234, 48_000).expect("usable");
        assert!((s - 3601.0).abs() < 1e-6, "got {s}");
    }

    #[test]
    fn skeleton_granule_to_seconds_unknown_basetime_is_zero_offset() {
        // basetime denominator 0 ("unknown") contributes 0.0, not NaN.
        let mut head = FisHead::new(Version::V4_0);
        head.basetime = Rational::new(0, 0);
        let mut sk = Skeleton::new();
        sk.set_head(head);
        sk.push_bone(FisBone::new(0x1234, Rational::new(48_000, 1)));
        let s = sk.granule_to_seconds(0x1234, 48_000).expect("usable");
        assert!((s - 1.0).abs() < 1e-9, "got {s}");
    }

    #[test]
    fn skeleton_granule_to_seconds_no_fishead_is_zero_offset() {
        // No fishead recorded: basetime offset defaults to 0.0.
        let mut sk = Skeleton::new();
        sk.push_bone(FisBone::new(0x1234, Rational::new(48_000, 1)));
        let s = sk.granule_to_seconds(0x1234, 96_000).expect("usable");
        assert!((s - 2.0).abs() < 1e-9, "got {s}");
    }

    #[test]
    fn skeleton_granule_to_seconds_unknown_serial_is_none() {
        let mut sk = Skeleton::new();
        sk.push_bone(FisBone::new(0x1234, Rational::new(48_000, 1)));
        assert_eq!(sk.granule_to_seconds(0xBEEF, 48_000), None);
    }

    #[test]
    fn fisbone_start_seconds_basegranule_over_rate() {
        // basegranule 336_000 at 48 kHz -> the kept data started at 7 s
        // on the original timeline (the spec's ?t=7-59 cut example).
        let mut bone = FisBone::new(1, Rational::new(48_000, 1));
        bone.basegranule = 336_000;
        let s = bone.start_seconds().expect("usable rate");
        assert!((s - 7.0).abs() < 1e-9, "got {s}");
        // Un-cut stream: basegranule 0 -> starts at 0.0.
        let uncut = FisBone::new(1, Rational::new(48_000, 1));
        assert_eq!(uncut.start_seconds(), Some(0.0));
    }

    #[test]
    fn fisbone_start_seconds_negative_basegranule_preserves_sign() {
        // A preroll page kept across the cut can sit before granule 0.
        let mut bone = FisBone::new(1, Rational::new(48_000, 1));
        bone.basegranule = -48_000;
        let s = bone.start_seconds().expect("usable");
        assert!((s + 1.0).abs() < 1e-9, "got {s}");
    }

    #[test]
    fn fisbone_start_seconds_unusable_rate_is_none() {
        let zero_den = FisBone::new(1, Rational::new(48_000, 0));
        assert_eq!(zero_den.start_seconds(), None);
        let zero_num = FisBone::new(1, Rational::new(0, 1));
        assert_eq!(zero_num.start_seconds(), None);
    }

    #[test]
    fn fisbone_granule_since_start_subtracts_basegranule() {
        // Data starts at granule 336_000 (7 s); a page at granule 384_000
        // is 1 s into the kept segment ((384_000 - 336_000) / 48_000).
        let mut bone = FisBone::new(1, Rational::new(48_000, 1));
        bone.basegranule = 336_000;
        let s = bone
            .granule_to_seconds_since_start(384_000)
            .expect("usable");
        assert!((s - 1.0).abs() < 1e-9, "got {s}");
        // The basegranule page itself is at elapsed 0.0.
        assert_eq!(bone.granule_to_seconds_since_start(336_000), Some(0.0));
        // For an un-cut stream this equals granule_to_seconds.
        let uncut = FisBone::new(1, Rational::new(48_000, 1));
        assert_eq!(
            uncut.granule_to_seconds_since_start(96_000),
            uncut.granule_to_seconds(96_000)
        );
    }

    #[test]
    fn fisbone_granule_since_start_negative_for_preroll_page() {
        // A page whose granule precedes the basegranule (a surviving
        // preroll page) presents before the cut-in -> negative elapsed.
        let mut bone = FisBone::new(1, Rational::new(48_000, 1));
        bone.basegranule = 96_000;
        let s = bone.granule_to_seconds_since_start(48_000).expect("usable");
        assert!((s + 1.0).abs() < 1e-9, "got {s}");
    }

    #[test]
    fn fisbone_granule_since_start_none_on_sentinel_or_unusable() {
        let mut bone = FisBone::new(1, Rational::new(48_000, 1));
        bone.basegranule = 336_000;
        assert_eq!(bone.granule_to_seconds_since_start(-1), None);
        let zero = FisBone::new(1, Rational::new(48_000, 0));
        assert_eq!(zero.granule_to_seconds_since_start(384_000), None);
    }

    #[test]
    fn skeleton_presentation_seconds_reads_fishead() {
        let mut head = FisHead::new(Version::V4_0);
        head.presentation_time = Rational::new(7, 1);
        let mut sk = Skeleton::new();
        sk.set_head(head);
        let s = sk.presentation_seconds().expect("fishead present");
        assert!((s - 7.0).abs() < 1e-9, "got {s}");
        // Zero-denominator presentation time is the "unknown" / un-cut
        // default of 0.0, not None.
        let mut head0 = FisHead::new(Version::V4_0);
        head0.presentation_time = Rational::new(0, 0);
        let mut sk0 = Skeleton::new();
        sk0.set_head(head0);
        assert_eq!(sk0.presentation_seconds(), Some(0.0));
        // No fishead at all -> None (presentation time is unknown).
        assert_eq!(Skeleton::new().presentation_seconds(), None);
    }

    #[test]
    fn skeleton_stream_start_seconds_adds_basetime() {
        // basegranule 336_000 @ 48 kHz = 7 s of data start, plus a
        // basetime of 3600 s -> file-absolute data start 3607 s.
        let mut head = FisHead::new(Version::V4_0);
        head.basetime = Rational::new(3600, 1);
        let mut bone = FisBone::new(0x1234, Rational::new(48_000, 1));
        bone.basegranule = 336_000;
        let mut sk = Skeleton::new();
        sk.set_head(head);
        sk.push_bone(bone);
        let s = sk.stream_start_seconds(0x1234).expect("usable");
        assert!((s - 3607.0).abs() < 1e-6, "got {s}");
        // Unknown serial -> None.
        assert_eq!(sk.stream_start_seconds(0xBEEF), None);
    }

    #[test]
    fn skeleton_substream_granule_to_seconds_cut_in_timeline() {
        // The ?t=7-59 cut: presentation_time 7 s, basegranule 336_000
        // (the kept data's start granule at 48 kHz). A page 1 s into the
        // kept segment (granule 384_000) presents at 7 + 1 = 8 s.
        let mut head = FisHead::new(Version::V4_0);
        head.presentation_time = Rational::new(7, 1);
        // basetime must NOT leak into the substream timeline.
        head.basetime = Rational::new(3600, 1);
        let mut bone = FisBone::new(0x1234, Rational::new(48_000, 1));
        bone.basegranule = 336_000;
        let mut sk = Skeleton::new();
        sk.set_head(head);
        sk.push_bone(bone);
        let s = sk
            .substream_granule_to_seconds(0x1234, 384_000)
            .expect("usable");
        assert!((s - 8.0).abs() < 1e-6, "got {s}");
        // The cut-in page itself presents exactly at the presentation time.
        let at_cut = sk
            .substream_granule_to_seconds(0x1234, 336_000)
            .expect("usable");
        assert!((at_cut - 7.0).abs() < 1e-6, "got {at_cut}");
    }

    #[test]
    fn skeleton_substream_granule_none_without_fishead_or_serial() {
        // No fishead -> presentation time unknown -> None.
        let mut sk = Skeleton::new();
        sk.push_bone(FisBone::new(0x1234, Rational::new(48_000, 1)));
        assert_eq!(sk.substream_granule_to_seconds(0x1234, 48_000), None);
        // Fishead present, unknown serial -> None.
        let mut sk2 = Skeleton::new();
        sk2.set_head(FisHead::new(Version::V4_0));
        sk2.push_bone(FisBone::new(0x1234, Rational::new(48_000, 1)));
        assert_eq!(sk2.substream_granule_to_seconds(0xBEEF, 48_000), None);
        // Fishead present, -1 sentinel granulepos -> None.
        assert_eq!(sk2.substream_granule_to_seconds(0x1234, -1), None);
    }

    #[test]
    fn skeleton_substream_uncut_equals_presentation_plus_track() {
        // For an un-cut stream (basegranule 0, presentation_time 0) the
        // substream time is just the per-track elapsed time.
        let mut sk = Skeleton::new();
        sk.set_head(FisHead::new(Version::V4_0));
        sk.push_bone(FisBone::new(0x1234, Rational::new(48_000, 1)));
        let s = sk
            .substream_granule_to_seconds(0x1234, 96_000)
            .expect("usable");
        assert!((s - 2.0).abs() < 1e-9, "got {s}");
    }
}
