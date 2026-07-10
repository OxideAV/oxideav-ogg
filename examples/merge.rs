//! Merge the content streams of several Ogg files into one multiplexed
//! (grouped) Ogg physical stream:
//!
//! ```sh
//! cargo run --example merge -- output.ogv video.ogv audio.oga [...]
//! ```
//!
//! Every input's streams are re-declared in the output (the muxer's
//! header layout puts a Theora stream's BOS page first, per the Theora
//! spec's multiplexed-stream mapping) and packets are written in
//! increasing presentation-time order across inputs — the RFC 3533 /
//! Theora §A.3.2 page-ordering rule ("data pages … should be placed in
//! the stream in increasing order by the time equivalents of their
//! granule position fields") then holds page-wise because each packet
//! is flushed onto its own page.

use oxideav_core::{Demuxer, Error, NullCodecResolver, Packet, ReadSeek, WriteSeek};

/// Pull packets from one demuxer, remembering the last pts seen so
/// pts-less packets still merge in stream order.
struct Feed {
    dmx: Box<dyn Demuxer>,
    /// Output stream index for each of this input's stream indices.
    index_map: Vec<u32>,
    /// Time (in seconds) of the packet currently waiting to be merged.
    next: Option<(f64, Packet)>,
    last_secs: f64,
}

impl Feed {
    fn advance(&mut self) {
        self.next = None;
        match self.dmx.next_packet() {
            Ok(mut pkt) => {
                let secs = match pkt.pts {
                    Some(pts) => pkt.time_base.seconds_of(pts),
                    None => self.last_secs,
                };
                self.last_secs = secs;
                pkt.stream_index = self.index_map[pkt.stream_index as usize];
                self.next = Some((secs, pkt));
            }
            Err(Error::Eof) => {}
            Err(e) => panic!("demux error: {e}"),
        }
    }
}

fn main() {
    let mut args = std::env::args().skip(1);
    let Some(output) = args.next() else {
        eprintln!("usage: merge <output.ogg> <input.ogg> [<input.ogg> ...]");
        std::process::exit(2);
    };
    let inputs: Vec<String> = args.collect();
    assert!(!inputs.is_empty(), "at least one input required");

    let mut feeds = Vec::new();
    let mut out_streams = Vec::new();
    for path in &inputs {
        let reader: Box<dyn ReadSeek> =
            Box::new(std::fs::File::open(path).expect("open input file"));
        let dmx = oxideav_ogg::demux::open(reader, &NullCodecResolver).expect("demux open");
        let mut index_map = Vec::new();
        for s in dmx.streams() {
            let mut s = s.clone();
            index_map.push(out_streams.len() as u32);
            s.index = out_streams.len() as u32;
            eprintln!(
                "{path}: stream {} -> {} ({})",
                index_map.len() - 1,
                s.index,
                s.params.codec_id.as_str()
            );
            out_streams.push(s);
        }
        feeds.push(Feed {
            dmx,
            index_map,
            next: None,
            last_secs: 0.0,
        });
    }

    let writer: Box<dyn WriteSeek> =
        Box::new(std::fs::File::create(&output).expect("create output file"));
    let mut mux = oxideav_ogg::mux::open(writer, &out_streams).expect("mux open");
    mux.write_header().expect("write_header");

    for feed in &mut feeds {
        feed.advance();
    }
    let mut n = 0u64;
    // Merge: earliest presentation time across inputs goes next.
    let earliest = |feeds: &[Feed]| {
        feeds
            .iter()
            .enumerate()
            .filter(|(_, f)| f.next.is_some())
            .min_by(|(_, a), (_, b)| {
                let (ta, _) = a.next.as_ref().unwrap();
                let (tb, _) = b.next.as_ref().unwrap();
                ta.total_cmp(tb)
            })
            .map(|(i, _)| i)
    };
    while let Some(i) = earliest(&feeds) {
        let (_, mut pkt) = feeds[i].next.take().expect("selected feed has a packet");
        // Flush a page on every granule-bearing packet: RFC 3533 §6 wants
        // a granule position on each page where a packet finishes, and a
        // pts-less packet (an audio stream's mid-page packet) can't
        // provide one — it stays buffered until the next pts.
        pkt.flags.unit_boundary = pkt.pts.is_some();
        mux.write_packet(&pkt).expect("write_packet");
        n += 1;
        feeds[i].advance();
    }
    mux.write_trailer().expect("write_trailer");
    eprintln!("merged {n} packets from {} inputs", inputs.len());
}
