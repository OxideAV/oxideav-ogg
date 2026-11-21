//! Remux of real encoder output, judged by independent black-box
//! readers.
//!
//! A black-box encoder (`ffmpeg`, used purely as a CLI) produces real
//! Ogg Opus, Ogg Vorbis and Ogg FLAC files. Each is demuxed with this
//! crate and muxed again three ways — keeping the source page
//! boundaries, repaginating to the default size / duration targets, and
//! repaginating with a 200 ms duration target — and every result must
//!
//! * pass `oggz-validate` (page / granule conformance),
//! * be probed by `ffprobe` without a single warning,
//! * be described by `opusinfo` / `ogginfo` without a warning, and
//! * decode (`ffmpeg` to raw PCM) to exactly the samples the original
//!   decodes to — which pins granule positions, pre-skip, end trimming
//!   and the header packets end to end.
//!
//! Every tool is optional and checked at runtime; a missing tool skips
//! its check (noted on stderr). No tool's source is consulted.

use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};

use oxideav_core::{Error, Muxer, NullCodecResolver, Packet, ReadSeek, WriteSeek};
use oxideav_ogg::mux::OggMuxer;

fn have(cmd: &str, probe_arg: &str) -> bool {
    Command::new(cmd)
        .arg(probe_arg)
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .status()
        .is_ok()
}

fn scratch(name: &str) -> PathBuf {
    let dir = std::env::temp_dir().join(format!("oxideav-ogg-remux-{}", std::process::id()));
    std::fs::create_dir_all(&dir).expect("scratch dir");
    dir.join(name)
}

/// Ask the black-box encoder for 7.3 s of a two-tone signal in the
/// given codec. `None` when the encoder is unavailable.
fn encode(name: &str, codec_args: &[&str]) -> Option<PathBuf> {
    let path = scratch(name);
    let status = Command::new("ffmpeg")
        .args(["-hide_banner", "-loglevel", "error", "-y", "-f", "lavfi"])
        .args([
            "-i",
            "sine=frequency=440:duration=7.3:sample_rate=48000",
            "-ac",
            "2",
        ])
        .args(codec_args)
        .args(["-f", "ogg"])
        .arg(&path)
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .status()
        .ok()?;
    (status.success() && path.exists()).then_some(path)
}

/// Decode a file to raw 16-bit PCM with the black-box decoder.
fn decode(path: &Path) -> Option<Vec<u8>> {
    let out = Command::new("ffmpeg")
        .args(["-hide_banner", "-loglevel", "error", "-i"])
        .arg(path)
        .args(["-f", "s16le", "-"])
        .stdin(Stdio::null())
        .output()
        .ok()?;
    assert!(
        out.stderr.is_empty(),
        "decoder complained about {}: {}",
        path.display(),
        String::from_utf8_lossy(&out.stderr)
    );
    out.status.success().then_some(out.stdout)
}

fn remux(source: &Path, configure: impl Fn(&mut OggMuxer)) -> Vec<u8> {
    let input: Box<dyn ReadSeek> = Box::new(std::fs::File::open(source).unwrap());
    let mut demux = oxideav_ogg::demux::open(input, &NullCodecResolver).expect("demux open");
    let streams = demux.streams().to_vec();
    assert_eq!(streams.len(), 1);

    let dest = scratch("remux.tmp");
    let out: Box<dyn WriteSeek> = Box::new(std::fs::File::create(&dest).unwrap());
    let mut mux = oxideav_ogg::mux::open_concrete(out, &streams).expect("mux open");
    configure(&mut mux);
    mux.write_header().unwrap();
    let mut packets = 0;
    loop {
        match demux.next_packet() {
            Ok(p) => {
                let p: Packet = p;
                mux.write_packet(&p).unwrap();
                packets += 1;
            }
            Err(Error::Eof) => break,
            Err(e) => panic!("demux: {e}"),
        }
    }
    assert!(packets > 100, "{packets} packets");
    mux.write_trailer().unwrap();
    drop(mux);
    std::fs::read(&dest).unwrap()
}

fn tool_output(cmd: &str, args: &[&str], path: &Path) -> Option<(bool, String)> {
    let out = Command::new(cmd)
        .args(args)
        .arg(path)
        .stdin(Stdio::null())
        .output()
        .ok()?;
    Some((
        out.status.success(),
        format!(
            "{}{}",
            String::from_utf8_lossy(&out.stdout),
            String::from_utf8_lossy(&out.stderr)
        ),
    ))
}

fn judge(kind: &str, label: &str, bytes: &[u8], reference_pcm: &[u8]) {
    let path = scratch(&format!("{kind}-{label}.ogg"));
    std::fs::write(&path, bytes).unwrap();

    // In-tree validator: always on.
    let report = oxideav_ogg::validate::validate(bytes);
    assert!(report.is_clean(), "{kind}/{label}: {:?}", report.issues);

    if have("oggz-validate", "--version") {
        let (ok, text) = tool_output("oggz-validate", &[], &path).unwrap();
        assert!(
            ok,
            "{kind}/{label}: oggz-validate rejected the file:\n{text}"
        );
    } else {
        eprintln!("oggz-validate not installed; skipping");
    }

    if have("ffprobe", "-version") {
        let (ok, text) = tool_output(
            "ffprobe",
            &[
                "-hide_banner",
                "-v",
                "warning",
                "-show_entries",
                "format=duration",
                "-of",
                "csv=p=0",
            ],
            &path,
        )
        .unwrap();
        assert!(ok, "{kind}/{label}: ffprobe failed:\n{text}");
        let lines: Vec<&str> = text.lines().filter(|l| !l.trim().is_empty()).collect();
        assert_eq!(lines.len(), 1, "{kind}/{label}: ffprobe warned:\n{text}");
        let duration: f64 = lines[0].trim().parse().expect("duration");
        assert!(
            (duration - 7.3).abs() < 0.05,
            "{kind}/{label}: {duration} s"
        );
    } else {
        eprintln!("ffprobe not installed; skipping");
    }

    let info = if kind == "opus" {
        "opusinfo"
    } else {
        "ogginfo"
    };
    // `ogginfo` describes Vorbis streams; it has nothing to say about
    // the others.
    if kind != "flac" && have(info, "-h") {
        let (ok, text) = tool_output(info, &[], &path).unwrap();
        let lower = text.to_lowercase();
        assert!(
            ok && !lower.contains("warning") && !lower.contains("error"),
            "{kind}/{label}: {info} complained:\n{text}"
        );
    } else {
        eprintln!("{info} not used for {kind}; skipping");
    }

    let pcm = decode(&path).expect("decode remux");
    assert_eq!(
        pcm.len(),
        reference_pcm.len(),
        "{kind}/{label}: decoded length differs"
    );
    assert!(
        pcm == reference_pcm,
        "{kind}/{label}: decoded samples differ"
    );
    let _ = std::fs::remove_file(&path);
}

/// `encoders` lists alternative argument sets; the first one the
/// installed encoder build accepts is used.
fn remux_and_judge(kind: &str, encoders: &[&[&str]]) {
    if !have("ffmpeg", "-version") {
        eprintln!("ffmpeg not installed; skipping black-box remux ({kind})");
        return;
    }
    let Some(source) = encoders
        .iter()
        .find_map(|args| encode(&format!("source-{kind}.ogg"), args))
    else {
        eprintln!("black-box encoder cannot produce {kind}; skipping");
        return;
    };
    let reference = decode(&source).expect("decode source");
    assert!(reference.len() > 7 * 48_000 * 4);

    let kept = remux(&source, |_| {});
    judge(kind, "kept", &kept, &reference);

    let merged = remux(&source, |m| m.set_keep_source_page_boundaries(false));
    judge(kind, "merged", &merged, &reference);

    let short = remux(&source, |m| {
        m.set_keep_source_page_boundaries(false);
        m.set_page_target_duration(Some(std::time::Duration::from_millis(200)));
        m.set_page_target_bytes(Some(1024));
    });
    judge(kind, "short", &short, &reference);

    // Demuxing the remux yields the same packets as demuxing the source.
    let packets = |bytes: Vec<u8>| {
        let input: Box<dyn ReadSeek> = Box::new(std::io::Cursor::new(bytes));
        let mut d = oxideav_ogg::demux::open(input, &NullCodecResolver).unwrap();
        let mut out = Vec::new();
        while let Ok(p) = d.next_packet() {
            out.push(p.data);
        }
        out
    };
    let original = packets(std::fs::read(&source).unwrap());
    assert_eq!(packets(kept), original);
    assert_eq!(packets(merged), original);
    assert_eq!(packets(short), original);
    let _ = std::fs::remove_dir_all(source.parent().unwrap());
}

// One test: the three codecs share a scratch directory.
#[test]
fn remuxed_encoder_output_is_accepted_and_decodes_identically() {
    remux_and_judge("opus", &[&["-c:a", "libopus", "-b:a", "64k"]]);
    remux_and_judge(
        "vorbis",
        &[
            &["-c:a", "libvorbis", "-q:a", "3"],
            &["-c:a", "vorbis", "-strict", "experimental"],
        ],
    );
    remux_and_judge("flac", &[&["-c:a", "flac"]]);
}
