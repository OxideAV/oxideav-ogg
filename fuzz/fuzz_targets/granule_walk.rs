#![no_main]

//! Seek-index + granule-walk panic-hardening harness for `oxideav-ogg`.
//!
//! Opens arbitrary fuzz-supplied bytes via
//! [`oxideav_ogg::demux::open_concrete`] (so the inherent
//! [`oxideav_ogg::demux::OggDemuxer::build_seek_index`] /
//! [`oxideav_ogg::demux::OggDemuxer::seek_index_len`] APIs are
//! reachable), runs the full-file page-header scan, then drives
//! [`oxideav_core::Demuxer::seek_to`] across every reported stream
//! at granule values derived from the input.
//!
//! Surfaces exercised:
//!
//! * `open_concrete` — same BOS-section walk + codec identification
//!   as `open`, but yielding the concrete type.
//! * `open_concrete_shared` — the shared-resolver entry point: on
//!   odd-first-byte inputs the demuxer holds a `CodecRegistry`
//!   carrying the Ogg family payload-magic claims *plus* hostile
//!   overlapping / lookalike claims (`OggS`, `fishead\0`, a
//!   single-byte prefix), so every BOS identification — including
//!   the chained-link registrations inside `build_seek_index` —
//!   routes attacker bytes through
//!   `CodecResolver::resolve_payload_magic` longest-prefix matching.
//! * `build_seek_index` — full-file `OggS` byte scanner +
//!   header-only page parsing with payload-skipping seeks, plus
//!   chained-link discovery (RFC 3533 §4) on mid-file BOS pages.
//! * `seek_index_len` — accessor for the index size, queried
//!   per stream.
//! * `seek_to` — index-floor lookup + bisection fallback,
//!   touching the granule-position translation (Vorbis / Opus /
//!   FLAC / Speex are accepted; Theora / unknown return
//!   `Error::Unsupported` per the README).
//! * `hole_count` / `framing_error_count` / `resync_count` —
//!   diagnostic accessors after the index walk and seeks.
//!
//! None of these calls may panic on attacker bytes. `build_seek_index`
//! intentionally swallows IO errors mid-scan (it documents that the
//! index may be left partial), so the fuzz harness mirrors that and
//! does not treat partial state as a failure.

use std::io::Cursor;
use std::sync::{Arc, OnceLock};

use libfuzzer_sys::fuzz_target;
use oxideav_core::{
    CodecId, CodecInfo, CodecRegistry, CodecResolver, Demuxer as _, NullCodecResolver, ReadSeek,
};
use oxideav_ogg::demux;

/// Cap per-input seek attempts so a pathological bisection that
/// loops the byte scanner stays inside the fuzz iteration budget.
const MAX_SEEKS_PER_STREAM: usize = 8;

/// Shared registry of payload-magic claims: the Ogg family's canonical
/// magics plus deliberately hostile ones — a claim on the page capture
/// pattern itself (`OggS`, which can only match if a BOS packet
/// *starts* with it), a claim on the Skeleton `fishead\0` ident (which
/// must never shadow the container-level Skeleton path), and a
/// single-byte prefix overlapping `OpusHead` (longest-prefix
/// tie-breaking under fire).
fn claiming_registry() -> Arc<dyn CodecResolver + Send + Sync> {
    static REG: OnceLock<Arc<CodecRegistry>> = OnceLock::new();
    REG.get_or_init(|| {
        let mut reg = CodecRegistry::new();
        let claims: [(&[u8], &str); 8] = [
            (b"\x01vorbis", "vorbis"),
            (b"OpusHead", "opus"),
            (b"\x80theora", "theora"),
            (b"Speex   ", "speex"),
            (b"\x7FFLAC", "flac"),
            (b"OggS", "oggs-lookalike"),
            (b"fishead\x00", "skeleton-imposter"),
            (b"O", "single-byte"),
        ];
        for (magic, id) in claims {
            reg.register(CodecInfo::new(CodecId::new(id)).payload_magic(magic));
        }
        Arc::new(reg)
    })
    .clone()
}

fuzz_target!(|data: &[u8]| {
    let reader: Box<dyn ReadSeek> = Box::new(Cursor::new(data.to_vec()));
    // Odd first byte → registry-first identification through the
    // shared resolver; even / empty → the historical built-in-table
    // path via NullCodecResolver. Both must be panic-free.
    let use_registry = data.first().is_some_and(|b| b & 1 == 1);
    let mut dmx = if use_registry {
        match demux::open_concrete_shared(reader, claiming_registry()) {
            Ok(d) => d,
            Err(_) => return,
        }
    } else {
        match demux::open_concrete(reader, &NullCodecResolver) {
            Ok(d) => d,
            Err(_) => return,
        }
    };

    // build_seek_index must return Ok or Err but not panic. A partial
    // index (per the API's own contract on IO errors) is fine. On the
    // shared-resolver path its chained-link BOS registrations resolve
    // through the registry.
    let _ = dmx.build_seek_index();

    // Snapshot the stream metadata before mutating the demuxer. The
    // accessor must work both before and after seeks.
    let stream_count = dmx.streams().len();
    let stream_indexes: Vec<u32> = dmx.streams().iter().map(|s| s.index).collect();

    // Total index length accessor — purely a read, must not panic.
    let _ = dmx.seek_index_len();

    // Drive seek_to at granule values derived from the input. Each
    // 8-byte chunk yields one signed-i64 target, cycling across the
    // available streams so multiplexed files get all streams probed.
    if stream_count == 0 {
        return;
    }
    for (probe, chunk) in data
        .chunks_exact(8)
        .take(MAX_SEEKS_PER_STREAM * stream_count)
        .enumerate()
    {
        let target = i64::from_le_bytes([
            chunk[0], chunk[1], chunk[2], chunk[3], chunk[4], chunk[5], chunk[6], chunk[7],
        ]);
        let stream_idx = stream_indexes[probe % stream_count];
        // Either Ok (a landing granule) or Err (unsupported codec /
        // out-of-range / IO error) — both must not panic.
        let _ = dmx.seek_to(stream_idx, target);
    }

    // Diagnostic accessors after the seek storm.
    let _ = dmx.hole_count();
    let _ = dmx.framing_error_count();
    let _ = dmx.resync_count();
});
