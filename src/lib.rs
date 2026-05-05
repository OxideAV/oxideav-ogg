//! Pure-Rust Ogg container (RFC 3533).
//!
//! Implements the page layer (capture pattern, segment table, CRC32) and a
//! packet-reassembly demuxer / packet-splitting muxer. Codec-specific parsing
//! lives in dedicated crates (`oxideav-vorbis`, future `oxideav-opus`, …);
//! this crate only sniffs the first packet of each logical bitstream to set
//! `CodecParameters::codec_id` correctly so the registry can dispatch.

pub mod codec_id;
pub mod crc;
pub mod demux;
pub mod mux;
pub mod page;

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
/// Also auto-registered into [`oxideav_core::REGISTRARS`] via the
/// [`oxideav_core::register!`] macro below so consumers calling
/// [`oxideav_core::RuntimeContext::with_all_features`] pick Ogg up
/// without any explicit umbrella plumbing.
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
