//! Black-box cross-checks of muxer output against independent
//! validator binaries, invoked as opaque CLIs: `oggz-validate` (page
//! and granule-level conformance) and `ffprobe` (container + stream
//! probing). Neither tool's source is consulted — pass/fail exit
//! status and printed stream attributes only.
//!
//! The tools are optional: when a binary is not installed the check is
//! skipped at runtime (with a note on stderr), so CI environments
//! without them still run the rest of the suite. The in-tree
//! whole-file validator (`oxideav_ogg::validate`) provides the
//! always-on equivalent gate in `tests/conformance_validator.rs`.

use std::io::Cursor;
use std::process::Command;

use oxideav_core::{CodecId, CodecParameters, Muxer, Packet, StreamInfo, TimeBase, WriteSeek};
use oxideav_ogg::mux;
use oxideav_ogg::skeleton::{FisBone, FisHead, Rational, Skeleton, Version};

// ─────────────────────────── plumbing ───────────────────────────

#[derive(Clone, Default)]
struct SharedBuf(std::sync::Arc<std::sync::Mutex<Cursor<Vec<u8>>>>);

impl SharedBuf {
    fn bytes(&self) -> Vec<u8> {
        self.0.lock().unwrap().get_ref().clone()
    }
}

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

/// True when `cmd` can be spawned (present on PATH).
fn have(cmd: &str, probe_arg: &str) -> bool {
    Command::new(cmd)
        .arg(probe_arg)
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null())
        .status()
        .is_ok()
}

/// Write `bytes` to a unique temp file; caller removes it.
fn temp_ogg(tag: &str, bytes: &[u8]) -> std::path::PathBuf {
    let path = std::env::temp_dir().join(format!(
        "oxideav-ogg-blackbox-{tag}-{}.ogg",
        std::process::id()
    ));
    std::fs::write(&path, bytes).expect("write temp ogg");
    path
}

/// Run `oggz-validate` over the file; returns `None` when the tool is
/// absent, `Some((success, combined_output))` otherwise.
fn oggz_validate(path: &std::path::Path) -> Option<(bool, String)> {
    if !have("oggz-validate", "--version") {
        eprintln!("oggz-validate not installed; skipping black-box check");
        return None;
    }
    let out = Command::new("oggz-validate").arg(path).output().ok()?;
    let text = format!(
        "{}{}",
        String::from_utf8_lossy(&out.stdout),
        String::from_utf8_lossy(&out.stderr)
    );
    Some((out.status.success(), text))
}

/// Run `ffprobe -show_entries stream=codec_name` over the file;
/// returns `None` when the tool is absent, else `(stderr, stdout)`.
fn ffprobe_streams(path: &std::path::Path) -> Option<(String, String)> {
    if !have("ffprobe", "-version") {
        eprintln!("ffprobe not installed; skipping black-box check");
        return None;
    }
    let out = Command::new("ffprobe")
        .args([
            "-v",
            "error",
            "-show_entries",
            "stream=codec_name,sample_rate,channels",
            "-of",
            "default=noprint_wrappers=1",
        ])
        .arg(path)
        .output()
        .ok()?;
    Some((
        String::from_utf8_lossy(&out.stderr).into_owned(),
        String::from_utf8_lossy(&out.stdout).into_owned(),
    ))
}

// ─────────────────────────── stream builders ───────────────────────────

fn opus_stream(index: u32) -> StreamInfo {
    let mut head = Vec::with_capacity(19);
    head.extend_from_slice(b"OpusHead");
    head.push(1); // version
    head.push(2); // channels
    head.extend_from_slice(&312u16.to_le_bytes()); // pre-skip
    head.extend_from_slice(&48_000u32.to_le_bytes()); // input rate
    head.extend_from_slice(&0i16.to_le_bytes()); // output gain
    head.push(0); // mapping family
    let mut params = CodecParameters::audio(CodecId::new("opus"));
    params.channels = Some(2);
    params.sample_rate = Some(48_000);
    params.extradata = head;
    StreamInfo {
        index,
        time_base: TimeBase::new(1, 48_000),
        duration: None,
        start_time: Some(0),
        params,
    }
}

/// A minimal syntactically valid Opus packet: TOC byte (config 31,
/// stereo, code 0 = one frame) plus opaque frame bytes.
fn opus_packet(stream: &StreamInfo, i: i64) -> Packet {
    let mut data = vec![0xFC];
    data.extend_from_slice(&[0x42; 20]);
    let mut pkt = Packet::new(stream.index, stream.time_base, data);
    // RFC 7845 granule: PCM sample position at 48 kHz plus pre-skip.
    pkt.pts = Some(960 * i);
    pkt.flags.keyframe = true;
    pkt.flags.unit_boundary = true;
    pkt
}

fn mux_opus(packets: i64) -> Vec<u8> {
    let stream = opus_stream(0);
    let shared = SharedBuf::default();
    let out: Box<dyn WriteSeek> = Box::new(shared.clone());
    let mut muxer = mux::open(out, std::slice::from_ref(&stream)).unwrap();
    muxer.write_header().unwrap();
    for i in 1..=packets {
        muxer.write_packet(&opus_packet(&stream, i)).unwrap();
    }
    muxer.write_trailer().unwrap();
    drop(muxer);
    shared.bytes()
}

fn vorbis_stream(index: u32) -> StreamInfo {
    let mut id = vec![0x01];
    id.extend_from_slice(b"vorbis");
    id.extend_from_slice(&0u32.to_le_bytes());
    id.push(2);
    id.extend_from_slice(&48_000u32.to_le_bytes());
    id.extend_from_slice(&[0; 12]);
    id.extend_from_slice(&[0xB8, 0x01]);
    let mut com = vec![0x03];
    com.extend_from_slice(b"vorbis");
    com.extend_from_slice(&[0, 0, 0, 0, 0, 0, 0, 0, 1]);
    let mut setup = vec![0x05];
    setup.extend_from_slice(b"vorbis");
    setup.extend_from_slice(&[0; 32]);
    let mut params = CodecParameters::audio(CodecId::new("vorbis"));
    params.channels = Some(2);
    params.sample_rate = Some(48_000);
    params.extradata = mux::xiph_lace(&[&id, &com, &setup]).unwrap();
    StreamInfo {
        index,
        time_base: TimeBase::new(1, 48_000),
        duration: None,
        start_time: Some(0),
        params,
    }
}

fn mux_vorbis(packets: i64) -> Vec<u8> {
    let stream = vorbis_stream(0);
    let shared = SharedBuf::default();
    let out: Box<dyn WriteSeek> = Box::new(shared.clone());
    let mut muxer = mux::open(out, std::slice::from_ref(&stream)).unwrap();
    muxer.write_header().unwrap();
    for i in 1..=packets {
        let mut pkt = Packet::new(0, stream.time_base, vec![(i & 0x7f) as u8; 120]);
        pkt.pts = Some(960 * i);
        pkt.flags.unit_boundary = true;
        muxer.write_packet(&pkt).unwrap();
    }
    muxer.write_trailer().unwrap();
    drop(muxer);
    shared.bytes()
}

/// A generic audio packet for stream builders whose payload bytes are
/// opaque to probes (Vorbis stand-ins).
fn opaque_packet(stream: &StreamInfo, i: i64) -> Packet {
    let mut pkt = Packet::new(stream.index, stream.time_base, vec![(i & 0x7f) as u8; 80]);
    pkt.pts = Some(960 * i);
    pkt.flags.unit_boundary = true;
    pkt
}

/// Two sequential links (RFC 3533 §4 chaining) of the given stream
/// kind.
fn mux_chained(
    stream_of: fn(u32) -> StreamInfo,
    packet_of: fn(&StreamInfo, i64) -> Packet,
) -> Vec<u8> {
    let shared = SharedBuf::default();
    let out: Box<dyn WriteSeek> = Box::new(shared.clone());
    let link0 = stream_of(0);
    let mut muxer = mux::open_concrete(out, std::slice::from_ref(&link0)).unwrap();
    muxer.write_header().unwrap();
    for i in 1..=5i64 {
        muxer.write_packet(&packet_of(&link0, i)).unwrap();
    }
    let link1 = stream_of(0);
    muxer.begin_new_link(std::slice::from_ref(&link1)).unwrap();
    for i in 1..=5i64 {
        muxer.write_packet(&packet_of(&link1, i)).unwrap();
    }
    muxer.write_trailer().unwrap();
    drop(muxer);
    shared.bytes()
}

/// A Skeleton 4.0 control bitstream over one content stream of the
/// given kind.
fn mux_skeleton(
    stream_of: fn(u32) -> StreamInfo,
    packet_of: fn(&StreamInfo, i64) -> Packet,
) -> Vec<u8> {
    let stream = stream_of(0);
    let mut skel = Skeleton::new();
    skel.set_head(FisHead::new(Version::V4_0));
    skel.push_bone(FisBone::new(0, Rational::new(48_000, 1)));
    let shared = SharedBuf::default();
    let out: Box<dyn WriteSeek> = Box::new(shared.clone());
    let mut muxer =
        mux::open_with_skeleton(out, std::slice::from_ref(&stream), Some(skel)).unwrap();
    muxer.write_header().unwrap();
    for i in 1..=8i64 {
        muxer.write_packet(&packet_of(&stream, i)).unwrap();
    }
    muxer.write_trailer().unwrap();
    drop(muxer);
    shared.bytes()
}

// ─────────────────────────── tests ───────────────────────────

#[test]
fn oggz_validate_accepts_muxer_outputs() {
    let cases: Vec<(&str, Vec<u8>)> = vec![
        ("opus", mux_opus(10)),
        ("vorbis", mux_vorbis(10)),
        ("chained-vorbis", mux_chained(vorbis_stream, opaque_packet)),
        ("chained-opus", mux_chained(opus_stream, opus_packet)),
        (
            "skeleton-vorbis",
            mux_skeleton(vorbis_stream, opaque_packet),
        ),
        ("skeleton-opus", mux_skeleton(opus_stream, opus_packet)),
    ];
    for (tag, bytes) in cases {
        let path = temp_ogg(&format!("oggzv-{tag}"), &bytes);
        let result = oggz_validate(&path);
        std::fs::remove_file(&path).ok();
        let Some((ok, output)) = result else { return };
        assert!(
            ok,
            "oggz-validate rejected the {tag} muxer output:\n{output}"
        );
    }
}

#[test]
fn ffprobe_reports_the_muxed_opus_stream() {
    let bytes = mux_opus(10);
    let path = temp_ogg("ffprobe-opus", &bytes);
    let result = ffprobe_streams(&path);
    std::fs::remove_file(&path).ok();
    let Some((stderr, stdout)) = result else {
        return;
    };
    assert!(
        stderr.trim().is_empty(),
        "ffprobe reported errors on the Opus muxer output:\n{stderr}"
    );
    assert!(
        stdout.contains("codec_name=opus"),
        "ffprobe did not identify the Opus stream:\n{stdout}"
    );
    assert!(
        stdout.contains("sample_rate=48000"),
        "ffprobe did not report the 48 kHz input rate:\n{stdout}"
    );
    assert!(
        stdout.contains("channels=2"),
        "ffprobe did not report the channel count:\n{stdout}"
    );
}

#[test]
fn ffprobe_probes_chained_and_skeleton_opus() {
    // A fully probeable codec mapping (Opus needs only its OpusHead to
    // init) lets ffprobe cross-check the trickier layouts too: chained
    // links and a Skeleton 4.0 control bitstream. (The Vorbis stand-in
    // headers cannot serve here: a probe aborts the whole open when the
    // synthetic setup packet fails codec init — a codec-layer matter,
    // covered instead by `oggz-validate`, which validates the same
    // files at the container level and accepts them.)
    for (tag, bytes) in [
        ("chained", mux_chained(opus_stream, opus_packet)),
        ("skeleton", mux_skeleton(opus_stream, opus_packet)),
    ] {
        let path = temp_ogg(&format!("ffprobe-{tag}"), &bytes);
        let result = ffprobe_streams(&path);
        std::fs::remove_file(&path).ok();
        let Some((stderr, stdout)) = result else {
            return;
        };
        assert!(
            stderr.trim().is_empty(),
            "ffprobe reported errors on the {tag} Opus muxer output:\n{stderr}"
        );
        assert!(
            stdout.contains("codec_name=opus"),
            "ffprobe did not identify the Opus stream in the {tag} file:\n{stdout}"
        );
    }
}
