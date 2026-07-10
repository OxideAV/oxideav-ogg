#![no_main]

//! Buffer-level framing-layer harness: `framing::PageWriter` /
//! `framing::PacketAssembler` / `framing::parse_pages` /
//! `framing::pages_to_packets` (the no-I/O packet ⇄ page layer added
//! alongside the trait-level mux/demux).
//!
//! Two modes, selected by the first input byte:
//!
//! * **Write mode** — fuzz-derived packet sizes / granule deltas /
//!   flush ops / page-target changes drive a [`PageWriter`], and the
//!   emitted bytes are checked against HARD round-trip invariants:
//!   `parse_pages` must accept every emitted page, the first page is
//!   BOS, the last is EOS, sequence numbers are contiguous from 0,
//!   every page has ≤ 255 lacing segments, and `pages_to_packets`
//!   returns exactly the pushed packet sequence byte-for-byte. Any
//!   violation is a bug in the writer or the assembler — the writer
//!   is this crate's own producer, so strict equality is a valid
//!   oracle under the clean-room wall.
//!
//! * **Read mode** — the raw fuzz bytes are fed to `parse_pages` /
//!   `pages_to_packets` (panic-freedom only), and every page that DOES
//!   parse is re-serialized via `Page::try_to_bytes` and re-parsed:
//!   parse → serialize → parse must be a fixpoint (flags, granule,
//!   serial, seq_no, lacing, data all preserved). A hostile buffer
//!   that parses into a page that does not round-trip would mean the
//!   parser and serializer disagree about the RFC 3533 §6 layout.
//!
//! Per-iteration allocations are bounded: packet sizes come from a
//! fixed class table whose largest entry (a 255-segment-spanning
//! 66 KB packet) is limited to two per iteration.

use libfuzzer_sys::fuzz_target;
use oxideav_ogg::framing::{pages_to_packets, parse_pages, PacketAssembler, PageWriter};
use oxideav_ogg::page::Page;

/// Packet-size classes. Chosen to straddle every lacing edge: empty
/// packet (single 0 lacing), 1, 254 (max single-segment), 255 (needs
/// zero terminator), 256, 510 (double 255 + 0), 8 KB (multi-segment),
/// and 66 000 (> 255 × 255, must span pages).
const SIZE_CLASSES: [usize; 8] = [0, 1, 254, 255, 256, 510, 8192, 66_000];

/// Cap on pushed packets per iteration.
const MAX_PACKETS: usize = 48;

/// Cap on 66 KB spanning packets per iteration (allocation bound).
const MAX_HUGE: usize = 2;

fuzz_target!(|data: &[u8]| {
    let Some((&mode, rest)) = data.split_first() else {
        return;
    };

    if mode & 1 == 0 {
        write_mode(rest);
    } else {
        read_mode(rest);
    }
});

/// Structure-aware write mode: drive PageWriter ops from descriptors,
/// then assert the full round-trip invariants on the emitted bytes.
fn write_mode(data: &[u8]) {
    let mut w = PageWriter::new(0x0BAD_CAFE);
    let mut expected: Vec<Vec<u8>> = Vec::new();
    let mut granule: i64 = 0;
    let mut huge_used = 0usize;

    // Each op consumes 3 descriptor bytes: [op/size selector, fill
    // byte, granule delta].
    for desc in data.chunks_exact(3).take(MAX_PACKETS) {
        let sel = desc[0];
        match sel >> 5 {
            // Ops 0..=5: push a packet of a fuzz-chosen size class.
            0..=5 => {
                let mut class = (sel & 0x07) as usize;
                if SIZE_CLASSES[class] == 66_000 {
                    if huge_used >= MAX_HUGE {
                        class = 6; // downgrade to the 8 KB class
                    } else {
                        huge_used += 1;
                    }
                }
                let len = SIZE_CLASSES[class];
                let pkt = vec![desc[1]; len];
                granule = granule.wrapping_add((desc[2] as i8) as i64);
                w.push_packet(&pkt, granule);
                expected.push(pkt);
            }
            // Op 6: force a page boundary.
            6 => w.flush_page(),
            // Op 7: change the soft page-size target (including
            // clearing it), so target-boundary interactions with
            // spanning packets get exercised.
            _ => {
                let target = match desc[1] & 0x03 {
                    0 => None,
                    1 => Some(1),
                    2 => Some(4096),
                    _ => Some(usize::from(desc[2]) * 64 + 1),
                };
                w.set_page_target(target);
            }
        }
    }

    let bytes = w.finish();
    if expected.is_empty() {
        assert!(
            bytes.is_empty(),
            "PageWriter with no packets must emit no pages"
        );
        return;
    }

    // HARD invariant: our own writer's output must parse cleanly.
    let pages = parse_pages(&bytes).expect("PageWriter output must parse");
    assert!(!pages.is_empty());
    assert!(pages[0].is_first(), "first emitted page must be BOS");
    assert!(
        pages.last().expect("non-empty").is_last(),
        "final emitted page must be EOS"
    );
    for (i, page) in pages.iter().enumerate() {
        assert_eq!(page.seq_no, i as u32, "page sequence must be contiguous");
        assert_eq!(page.serial, 0x0BAD_CAFE);
        assert!(page.lacing.len() <= 255);
        if i > 0 {
            // A page is continued iff the previous page ended on a
            // 255-valued lacing segment (RFC 3533 §6 field 3).
            let prev_open = pages[i - 1].lacing.last().copied() == Some(255);
            assert_eq!(
                page.is_continued(),
                prev_open,
                "continued flag must match the previous page's open packet"
            );
        }
    }

    // HARD invariant: whole-buffer packet reassembly returns exactly
    // what was pushed.
    let got = pages_to_packets(&bytes).expect("own output must reassemble");
    assert_eq!(got, expected, "packet round-trip through PageWriter");

    // Same result via the incremental assembler.
    let mut asm = PacketAssembler::new();
    let mut got2 = Vec::new();
    for page in &pages {
        got2.extend(asm.push_page(page).expect("own pages must assemble"));
    }
    assert_eq!(got2, expected);
    assert!(!asm.mid_packet(), "EOS page must not leave an open packet");
}

/// Hostile read mode: arbitrary bytes through the strict layer, plus
/// a parse → serialize → parse fixpoint check on every accepted page.
fn read_mode(data: &[u8]) {
    // Panic-freedom on the conveniences.
    let _ = pages_to_packets(data);

    let Ok(pages) = parse_pages(data) else {
        return;
    };
    for page in &pages {
        // Every parsed page must re-serialize (its lacing invariants
        // were just validated by the parser)…
        let bytes = page
            .try_to_bytes()
            .expect("parsed page must satisfy the lacing invariants");
        // …and re-parse to an identical page (fixpoint).
        let (again, used) = Page::parse(&bytes).expect("serialized page must re-parse");
        assert_eq!(used, bytes.len());
        assert_eq!(again.flags, page.flags);
        assert_eq!(again.granule_position, page.granule_position);
        assert_eq!(again.serial, page.serial);
        assert_eq!(again.seq_no, page.seq_no);
        assert_eq!(again.lacing, page.lacing);
        assert_eq!(again.data, page.data);
    }

    // Feed the parsed pages to a fresh assembler; errors are expected
    // on hostile framing, panics are not.
    let mut asm = PacketAssembler::new();
    for page in &pages {
        if asm.push_page(page).is_err() {
            asm.reset();
        }
    }
    let _ = asm.serial();
    let _ = asm.mid_packet();
}
