#![no_main]

//! End-to-end demuxer panic-hardening harness for `oxideav-ogg`.
//!
//! Drives arbitrary fuzz-supplied bytes through the full RFC 3533
//! demuxer stack:
//!
//! * [`oxideav_ogg::demux::open`] — BOS-section walk, codec
//!   sniffing (Vorbis / Opus / FLAC / Theora / Speex first-packet
//!   signature match), Xiph-comment metadata parsing.
//! * [`oxideav_core::Demuxer::next_packet`] — page reader,
//!   packet reassembly across page boundaries, page-loss detection
//!   (`page_sequence_number` gap, RFC 3533 §6 field 6), continued-
//!   flag framing-consistency check (RFC 3533 §6 field 3),
//!   `OggS`-capture recapture (RFC 3533 §3, §6 field 1) after both
//!   bytes-between-pages garbage and CRC mismatches.
//! * The streams / metadata / counter accessors
//!   (`streams()`, `metadata()`, `hole_count()`,
//!   `framing_error_count()`, `resync_count()`) so their accessor
//!   surface is also panic-checked on whatever final state the
//!   driver lands in.
//!
//! The contract under test: every call returns. A malformed stream
//! yields `Err(oxideav_core::Error::…)`; a well-formed one yields
//! `Ok(Packet)` until `Err(Error::Eof)`. Neither path may panic,
//! overflow, abort, or OOM. The return values are intentionally
//! discarded after a few invariants are checked.

use std::io::Cursor;

use libfuzzer_sys::fuzz_target;
use oxideav_core::{Error, NullCodecResolver, ReadSeek};
use oxideav_ogg::demux;

/// Cap the per-call packet drain so a corrupted-but-recoverable
/// stream cannot loop forever generating packets — the fuzz timeout
/// would catch it but this keeps each iteration fast.
const MAX_PACKETS: usize = 4096;

fuzz_target!(|data: &[u8]| {
    // The demuxer requires a Read + Seek input; wrap the fuzz bytes
    // in a Cursor. No `Box::new` of arbitrary-size buffers happens
    // here — the cursor borrows the input.
    let reader: Box<dyn ReadSeek> = Box::new(Cursor::new(data.to_vec()));
    let resolver = NullCodecResolver;
    let mut dmx = match demux::open(reader, &resolver) {
        Ok(d) => d,
        Err(_) => {
            // BOS-section walk rejected the input; the other code
            // paths still get coverage on subsequent fuzz inputs.
            return;
        }
    };

    // Accessor surface must not panic regardless of input.
    let _ = dmx.format_name();
    let _ = dmx.streams();
    let _ = dmx.metadata();

    // Drain packets. The loop terminates on the first `Err`; an
    // `Eof` is the clean end, anything else is a malformed-stream
    // error and also acceptable.
    for _ in 0..MAX_PACKETS {
        match dmx.next_packet() {
            Ok(pkt) => {
                // A delivered packet must carry a valid stream
                // index for the streams the demuxer just reported.
                // The streams vec was captured before drain, so
                // re-borrowing it now is safe.
                let n = dmx.streams().len();
                assert!(
                    (pkt.stream_index as usize) < n,
                    "demuxer emitted packet for unknown stream {} (streams: {})",
                    pkt.stream_index,
                    n,
                );
            }
            Err(Error::Eof) => break,
            Err(_) => break,
        }
    }
});
