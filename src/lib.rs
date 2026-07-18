//! Pure-Rust Ogg container (RFC 3533).
//!
//! Implements the page layer (capture pattern, segment table, CRC32) and a
//! packet-reassembly demuxer / packet-splitting muxer. Codec-specific parsing
//! lives in dedicated crates (`oxideav-vorbis`, future `oxideav-opus`, …);
//! this crate only sniffs the first packet of each logical bitstream to set
//! `CodecParameters::codec_id` correctly so the registry can dispatch.
//!
//! # Example: write an `.ogg` file, then read it back
//!
//! Ogg is codec-agnostic — the packet payloads below (the three Vorbis
//! header packets and the audio packets) normally come from a codec
//! crate such as `oxideav-vorbis`; minimal hand-built stand-ins keep
//! the example self-contained. The mux side writes packets into a
//! fresh `.ogg` via [`mux::open`]; the demux side reopens it with
//! [`demux::open`] and iterates the packets.
//!
//! ```rust
//! use oxideav_core::{CodecId, CodecParameters, NullCodecResolver, Packet, StreamInfo, TimeBase};
//! use oxideav_core::{ReadSeek, WriteSeek};
//!
//! fn main() -> Result<(), Box<dyn std::error::Error>> {
//!     // Codec packets normally come from a codec crate (e.g.
//!     // oxideav-vorbis's encoder); these stand-ins are the minimal
//!     // valid Vorbis header set.
//!     let mut id_header = vec![0x01];
//!     id_header.extend_from_slice(b"vorbis");
//!     id_header.extend_from_slice(&0u32.to_le_bytes()); // vorbis_version
//!     id_header.push(2); // audio_channels
//!     id_header.extend_from_slice(&48_000u32.to_le_bytes()); // sample rate
//!     id_header.extend_from_slice(&[0; 12]); // bitrate max/nominal/min
//!     id_header.extend_from_slice(&[0xB8, 0x01]); // blocksizes + framing bit
//!     let mut comment_header = vec![0x03];
//!     comment_header.extend_from_slice(b"vorbis");
//!     comment_header.extend_from_slice(&[0, 0, 0, 0, 0, 0, 0, 0, 1]); // empty comment + framing
//!     let mut setup_header = vec![0x05];
//!     setup_header.extend_from_slice(b"vorbis");
//!     setup_header.extend_from_slice(&[0; 16]); // (real codebooks go here)
//!
//!     // ---- mux side: write packets into a fresh .ogg ----
//!     let mut params = CodecParameters::audio(CodecId::new("vorbis"));
//!     params.channels = Some(2);
//!     params.sample_rate = Some(48_000);
//!     // Vorbis/Theora carry their 3 header packets Xiph-laced in extradata.
//!     params.extradata =
//!         oxideav_ogg::mux::xiph_lace(&[&id_header, &comment_header, &setup_header]).unwrap();
//!     let streams = vec![StreamInfo {
//!         index: 0,
//!         time_base: TimeBase::new(1, 48_000),
//!         duration: None,
//!         start_time: Some(0),
//!         params,
//!     }];
//!
//!     let path = std::env::temp_dir().join("oxideav-ogg-roundtrip.ogg");
//!     let out: Box<dyn WriteSeek> = Box::new(std::fs::File::create(&path)?);
//!     let mut mux = oxideav_ogg::mux::open(out, &streams)?;
//!     mux.write_header()?; // BOS + header pages, rebuilt from extradata
//!     for i in 1..=3i64 {
//!         // Payload bytes come from your codec's encoder.
//!         let mut pkt = Packet::new(0, TimeBase::new(1, 48_000), vec![0xAB; 64]);
//!         pkt.pts = Some(960 * i); // granule position (Vorbis: end PCM sample)
//!         pkt.flags.unit_boundary = true; // flush the page after this packet
//!         mux.write_packet(&pkt)?;
//!     }
//!     mux.write_trailer()?; // flushes + marks the final page EOS
//!
//!     // ---- demux side: open the .ogg and iterate packets ----
//!     let input: Box<dyn ReadSeek> = Box::new(std::fs::File::open(&path)?);
//!     let mut dmx = oxideav_ogg::demux::open(input, &NullCodecResolver)?;
//!     assert_eq!(dmx.streams()[0].params.codec_id.as_str(), "vorbis"); // sniffed from BOS
//!     assert_eq!(dmx.streams()[0].params.sample_rate, Some(48_000));
//!     let mut count = 0;
//!     loop {
//!         match dmx.next_packet() {
//!             // pkt.data is one codec packet — hand it to your decoder.
//!             // (Header packets were consumed into extradata during open.)
//!             Ok(pkt) => {
//!                 assert_eq!(pkt.data, vec![0xAB; 64]);
//!                 count += 1;
//!             }
//!             Err(oxideav_core::Error::Eof) => break,
//!             Err(e) => return Err(e.into()),
//!         }
//!     }
//!     assert_eq!(count, 3);
//!     std::fs::remove_file(&path)?;
//!     Ok(())
//! }
//! ```
//!
//! For a single logical bitstream without `StreamInfo`/I-O plumbing
//! (e.g. inside a codec crate), the [`framing`] module offers the same
//! round-trip at the buffer level: [`framing::PageWriter`] turns
//! packets + granule positions into page bytes, and
//! [`framing::PacketAssembler`] / [`framing::pages_to_packets`] invert
//! it.

pub mod codec_id;
pub mod crc;
pub mod demux;
pub mod framing;
pub mod mux;
pub mod page;
pub mod skeleton;
pub mod theora;
pub mod validate;

use oxideav_core::ContainerRegistry;

/// Register the Ogg demuxer/muxer with a [`ContainerRegistry`].
pub fn register_containers(reg: &mut ContainerRegistry) {
    reg.register_demuxer("ogg", demux::open);
    reg.register_muxer("ogg", mux::open);
    reg.register_extension("ogg", "ogg");
    reg.register_extension("oga", "ogg");
    reg.register_extension("ogv", "ogg");
    reg.register_extension("opus", "ogg");
    reg.register_probe("ogg", probe);
}

/// Install the Ogg container into a [`oxideav_core::RuntimeContext`].
///
/// Convenience wrapper around [`register_containers`] that matches the
/// uniform `register(&mut RuntimeContext)` entry point every sibling
/// crate exposes.
///
/// Also wired into [`oxideav_meta::register_all`] via the
/// [`oxideav_core::register!`] macro below.
pub fn register(ctx: &mut oxideav_core::RuntimeContext) {
    register_containers(&mut ctx.containers);
}

oxideav_core::register!("ogg", register);

/// `OggS` capture pattern (RFC 3533 §6) at offset 0.
fn probe(p: &oxideav_core::ProbeData) -> u8 {
    if p.buf.len() >= 4 && &p.buf[0..4] == b"OggS" {
        100
    } else {
        0
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn register_via_runtime_context_installs_container() {
        let mut ctx = oxideav_core::RuntimeContext::new();
        register(&mut ctx);
        assert_eq!(ctx.containers.container_for_extension("ogg"), Some("ogg"));
        assert_eq!(ctx.containers.container_for_extension("opus"), Some("ogg"));
    }
}
