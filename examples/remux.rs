//! Remux an Ogg file through the demuxer and muxer:
//!
//! ```sh
//! cargo run --example remux -- input.ogv output.ogv
//! ```
//!
//! Every content packet is read via [`oxideav_ogg::demux`] and written
//! back via [`oxideav_ogg::mux`] using the demuxer-reconstructed
//! `StreamInfo` (codec parameters + extradata). The output is a fresh,
//! spec-conformant layout — page boundaries are not preserved, but
//! packet bytes, timing (pts / granule semantics), and keyframe
//! structure are. Useful for validating the mux path against external
//! tools: the remuxed file must describe and decode identically to the
//! source.

use oxideav_core::{Error, NullCodecResolver, ReadSeek, WriteSeek};

fn main() {
    let mut args = std::env::args().skip(1);
    let (Some(input), Some(output)) = (args.next(), args.next()) else {
        eprintln!("usage: remux <input.ogg> <output.ogg>");
        std::process::exit(2);
    };

    let reader: Box<dyn ReadSeek> = Box::new(std::fs::File::open(&input).expect("open input file"));
    let mut dmx = oxideav_ogg::demux::open(reader, &NullCodecResolver).expect("demux open");

    let streams = dmx.streams().to_vec();
    for s in &streams {
        eprintln!(
            "stream {}: {} {}x{} tb={:?}",
            s.index,
            s.params.codec_id.as_str(),
            s.params.width.unwrap_or(0),
            s.params.height.unwrap_or(0),
            s.time_base
        );
    }

    let writer: Box<dyn WriteSeek> =
        Box::new(std::fs::File::create(&output).expect("create output file"));
    let mut mux = oxideav_ogg::mux::open(writer, &streams).expect("mux open");
    mux.write_header().expect("write_header");

    let mut n = 0u64;
    loop {
        match dmx.next_packet() {
            Ok(mut pkt) => {
                // Flush a page whenever the just-written packet carries a
                // pts: RFC 3533 §6 wants every page on which a packet
                // finishes to carry a granule position, and pts is where
                // the muxer gets one (Theora packets all carry a pts; an
                // audio stream's mid-page packets may not — those stay
                // buffered until the next granule-bearing packet).
                pkt.flags.unit_boundary = pkt.pts.is_some();
                mux.write_packet(&pkt).expect("write_packet");
                n += 1;
            }
            Err(Error::Eof) => break,
            Err(e) => panic!("demux error after {n} packets: {e}"),
        }
    }
    mux.write_trailer().expect("write_trailer");
    eprintln!("remuxed {n} packets");
}
