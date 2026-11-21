//! CRC-32 used by the Ogg page checksum field.
//!
//! Polynomial 0x04C11DB7, non-reflected, initial value 0, no final XOR — the
//! checksum is computed over the page bytes with the checksum field itself
//! zeroed. Per RFC 3533 §6 field 7:
//!
//! > CRC_checksum: a 4 Byte field containing a 32 bit CRC checksum of the page
//! > (including header with zero CRC field and page content). The generator
//! > polynomial is 0x04c11db7.
//!
//! The CRC field itself lives at bytes 22..26 of an Ogg page header (RFC 3533
//! §6 byte diagram).
//!
//! The hot loop consumes **sixteen input bytes per step**. A byte that
//! sits `k` positions before the end of a block still has `k` further
//! byte-shifts ahead of it, so its contribution is the single-byte table
//! advanced through `k` zero bytes; sixteen such tables (`RANKS[0]` …
//! `RANKS[15]`, each derived from the generator polynomial at compile
//! time by the recurrence in [`build_slice_table`]) turn the block into
//! sixteen independent lookups XORed together. The lookups do not depend
//! on one another, so the processor overlaps them; the serial dependency
//! is one XOR tree per sixteen bytes instead of one table step per byte.
//! A four-byte step and a scalar tail finish the remainder.
//! `compute_page_checksum` splits its input into three straight-line
//! segments (`[..22]`, four zero bytes for the CRC field, `[26..]`) so
//! no per-byte "is this index in the CRC field" test is needed.

/// CRC-32/Ogg lookup table T0 (the standard single-byte advancement
/// table), generated at compile time.
///
/// Indexed by the high byte of the running CRC XOR the input byte; the
/// returned value is XORed into the CRC after a left-shift by 8. This is
/// the textbook non-reflected MSB-first CRC table.
const T0: [u32; 256] = {
    let mut table = [0u32; 256];
    let mut i = 0;
    while i < 256 {
        let mut r = (i as u32) << 24;
        let mut j = 0;
        while j < 8 {
            r = if r & 0x8000_0000 != 0 {
                (r << 1) ^ 0x04C1_1DB7
            } else {
                r << 1
            };
            j += 1;
        }
        table[i] = r;
        i += 1;
    }
    table
};

/// `RANKS[k]` is the single-byte table advanced through `k` zero bytes
/// (`RANKS[0]` is [`T0`] itself). Sixteen ranks serve the sixteen-byte
/// step; the first four also serve the four-byte step.
static RANKS: [[u32; 256]; 16] = {
    let mut ranks = [T0; 16];
    let mut k = 1;
    while k < 16 {
        ranks[k] = build_slice_table(ranks[k - 1]);
        k += 1;
    }
    ranks
};

/// Rank-1 table (T0 advanced by one zero byte); kept by name for the
/// recurrence tests.
#[cfg(test)]
const T1: [u32; 256] = build_slice_table(T0);
/// Rank-2 table.
#[cfg(test)]
const T2: [u32; 256] = build_slice_table(T1);
/// Rank-3 table.
#[cfg(test)]
const T3: [u32; 256] = build_slice_table(T2);

/// Given a single-byte advancement table `prev`, return the table that
/// is one extra zero byte deep. The recurrence is the same one the
/// byte-at-a-time loop uses internally on a zero-byte input:
/// `crc' = (crc << 8) ^ T0[(crc >> 24) as u8]`. Applied to each entry
/// of `prev` it yields the next slice-by-N rank.
const fn build_slice_table(prev: [u32; 256]) -> [u32; 256] {
    let mut out = [0u32; 256];
    let mut i = 0;
    while i < 256 {
        let v = prev[i];
        out[i] = (v << 8) ^ T0[(v >> 24) as usize];
        i += 1;
    }
    out
}

/// Compute the CRC-32 over `bytes` using the Ogg parameters.
///
/// Starts from CRC `0` (the per-page initial value RFC 3533 §6 field 7
/// specifies). Sixteen bytes per step, then four, then a scalar tail.
pub fn checksum(bytes: &[u8]) -> u32 {
    continue_checksum(0, bytes)
}

/// Continue a CRC computation from a known `state` over `bytes`.
///
/// This is the same one-shot CRC the [`checksum`] function computes, but
/// taking an initial state so callers can feed the input in chunks (e.g.
/// a header buffer + a payload buffer) without materialising one large
/// concatenated slice. `continue_checksum(0, bytes) == checksum(bytes)`.
///
/// Used internally by [`compute_page_checksum`] to feed the page's
/// header prefix, then the four zero bytes the spec substitutes for the
/// CRC field, then the rest of the page — without the per-byte range
/// check the original implementation paid for every input byte.
pub fn continue_checksum(state: u32, bytes: &[u8]) -> u32 {
    let mut crc = state;
    // Sixteen bytes per step. The running state is XORed into the first
    // four bytes of the block (the MSB-first recurrence pulls input
    // through the state's high byte, so block byte 0 lines up with the
    // state's top byte); after that every byte of the block is an
    // independent lookup in the rank matching its distance from the
    // block's end.
    let (blocks, rest) = bytes.as_chunks::<16>();
    for block in blocks {
        let head = crc ^ u32::from_be_bytes([block[0], block[1], block[2], block[3]]);
        crc = RANKS[15][(head >> 24) as usize]
            ^ RANKS[14][((head >> 16) & 0xFF) as usize]
            ^ RANKS[13][((head >> 8) & 0xFF) as usize]
            ^ RANKS[12][(head & 0xFF) as usize]
            ^ RANKS[11][block[4] as usize]
            ^ RANKS[10][block[5] as usize]
            ^ RANKS[9][block[6] as usize]
            ^ RANKS[8][block[7] as usize]
            ^ RANKS[7][block[8] as usize]
            ^ RANKS[6][block[9] as usize]
            ^ RANKS[5][block[10] as usize]
            ^ RANKS[4][block[11] as usize]
            ^ RANKS[3][block[12] as usize]
            ^ RANKS[2][block[13] as usize]
            ^ RANKS[1][block[14] as usize]
            ^ RANKS[0][block[15] as usize];
    }
    // Four bytes per step for what is left of a partial block.
    let (words, tail) = rest.as_chunks::<4>();
    for word in words {
        let w = crc ^ u32::from_be_bytes(*word);
        crc = RANKS[3][(w >> 24) as usize]
            ^ RANKS[2][((w >> 16) & 0xFF) as usize]
            ^ RANKS[1][((w >> 8) & 0xFF) as usize]
            ^ RANKS[0][(w & 0xFF) as usize];
    }
    // Scalar tail.
    for &b in tail {
        crc = (crc << 8) ^ T0[((crc >> 24) as u8 ^ b) as usize];
    }
    crc
}

/// Byte offset of the CRC field within an Ogg page header (RFC 3533 §6).
pub const CRC_FIELD_OFFSET: usize = 22;

/// Byte length of the CRC field (32 bits).
pub const CRC_FIELD_LEN: usize = 4;

/// Advance a CRC state by four zero bytes — the same operation
/// [`compute_page_checksum`] performs to substitute the CRC field with
/// zeros without splitting the slice.
///
/// The recurrence is the textbook scalar step with `b = 0`, unrolled
/// four times. The compiler turns it into four table loads + four
/// shift/xor ops with no loop overhead.
fn advance_four_zero_bytes(state: u32) -> u32 {
    let mut crc = state;
    crc = (crc << 8) ^ T0[(crc >> 24) as usize];
    crc = (crc << 8) ^ T0[(crc >> 24) as usize];
    crc = (crc << 8) ^ T0[(crc >> 24) as usize];
    crc = (crc << 8) ^ T0[(crc >> 24) as usize];
    crc
}

/// Compute the page checksum the way RFC 3533 §6 field 7 specifies:
/// the bytes 22..26 (CRC field) are treated as zero, then [`checksum`]
/// runs over the full page (header + lacing table + segment data).
///
/// `page_bytes` must be one complete page — at minimum 27 bytes (the
/// fixed header) and otherwise as long as `27 + n_segs + sum(lacing)`.
/// Returns `None` if the slice is too short to contain even the CRC
/// field (i.e. fewer than 26 bytes).
///
/// Implementation note: split into three straight-line segments
/// (`[..22]`, four-zero CRC-field substitute, `[26..]`) so the per-byte
/// "is this index in the CRC field?" range check the original
/// implementation performed is gone. For a max-size 65 KiB page this
/// removes 65 535 range checks from the hot path.
pub fn compute_page_checksum(page_bytes: &[u8]) -> Option<u32> {
    if page_bytes.len() < CRC_FIELD_OFFSET + CRC_FIELD_LEN {
        return None;
    }
    let prefix = continue_checksum(0, &page_bytes[..CRC_FIELD_OFFSET]);
    // Substitute the CRC field with four zero bytes.
    let after_crc_field = advance_four_zero_bytes(prefix);
    Some(continue_checksum(
        after_crc_field,
        &page_bytes[CRC_FIELD_OFFSET + CRC_FIELD_LEN..],
    ))
}

/// Read the CRC field stored in an Ogg page byte slice (little-endian,
/// per RFC 3533 §6: "Fields with more than one byte length are encoded
/// LSB first").
pub fn read_page_checksum(page_bytes: &[u8]) -> Option<u32> {
    if page_bytes.len() < CRC_FIELD_OFFSET + CRC_FIELD_LEN {
        return None;
    }
    Some(u32::from_le_bytes([
        page_bytes[CRC_FIELD_OFFSET],
        page_bytes[CRC_FIELD_OFFSET + 1],
        page_bytes[CRC_FIELD_OFFSET + 2],
        page_bytes[CRC_FIELD_OFFSET + 3],
    ]))
}

/// Verify that the CRC stored in `page_bytes` matches the CRC recomputed
/// over the same bytes with the CRC field zeroed (RFC 3533 §6 field 7).
///
/// Returns `Some(true)` on match, `Some(false)` on mismatch, and `None`
/// if the slice is too short to contain the CRC field (fewer than 26
/// bytes).
///
/// Unlike [`crate::page::Page::parse`], this helper does NOT decode the
/// segment table or copy any data — it is a pure byte-slice check, useful
/// for tools that want to scan a file's pages for integrity without
/// reassembling packets.
pub fn validate_page_crc(page_bytes: &[u8]) -> Option<bool> {
    let stored = read_page_checksum(page_bytes)?;
    let computed = compute_page_checksum(page_bytes)?;
    Some(stored == computed)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn empty_is_zero() {
        assert_eq!(checksum(&[]), 0);
    }

    #[test]
    fn deterministic() {
        // Same input must always produce the same checksum.
        assert_eq!(checksum(b"hello world"), checksum(b"hello world"));
        assert_ne!(checksum(b"hello world"), checksum(b"hello WORLD"));
    }

    #[test]
    fn validate_rejects_short_buffers() {
        // Anything shorter than the CRC field's 22+4 end-byte must
        // return `None` rather than silently passing.
        assert_eq!(validate_page_crc(&[]), None);
        assert_eq!(validate_page_crc(&[0u8; 25]), None);
        assert!(validate_page_crc(&[0u8; 26]).is_some()); // exactly CRC field's end.
    }

    #[test]
    fn validate_round_trips_a_built_page() {
        // Build a minimal valid page by hand: 27-byte header, n_segs=1,
        // one segment of two bytes, CRC computed in place.
        let mut page = vec![0u8; 27 + 1 + 2];
        page[0..4].copy_from_slice(b"OggS");
        page[4] = 0; // version
        page[5] = 0x02; // BOS flag
        // granule (bytes 6..14) left zero.
        // serial (bytes 14..18) — pick something non-zero.
        page[14..18].copy_from_slice(&0x1234_5678u32.to_le_bytes());
        // seq_no (bytes 18..22) left zero.
        // CRC field (bytes 22..26) left zero for the initial compute.
        page[26] = 1; // n_segs
        page[27] = 2; // lacing[0] = 2 bytes
        page[28] = 0xAA;
        page[29] = 0xBB;
        let crc = compute_page_checksum(&page).expect("len >= 26");
        page[22..26].copy_from_slice(&crc.to_le_bytes());

        // Now the stored CRC and the recomputed CRC must agree.
        assert_eq!(validate_page_crc(&page), Some(true));

        // Flip a single byte anywhere in the payload — the stored CRC
        // should no longer agree with the recomputed one.
        let mut tampered = page.clone();
        tampered[29] ^= 0x01;
        assert_eq!(validate_page_crc(&tampered), Some(false));

        // Flip a header byte (the serial number) — also a mismatch.
        let mut tampered2 = page.clone();
        tampered2[14] ^= 0x80;
        assert_eq!(validate_page_crc(&tampered2), Some(false));

        // Corrupt the stored CRC itself — also a mismatch.
        let mut tampered3 = page.clone();
        tampered3[22] ^= 0xFF;
        assert_eq!(validate_page_crc(&tampered3), Some(false));
    }

    #[test]
    fn compute_matches_zero_field_invariant() {
        // compute_page_checksum must give the same result regardless of
        // what's stored in the CRC field, because it treats those four
        // bytes as zero.
        let mut page = vec![0u8; 30];
        page[0..4].copy_from_slice(b"OggS");
        page[26] = 1;
        page[27] = 2;
        let with_zero_crc = compute_page_checksum(&page).expect("len >= 26");
        page[22..26].copy_from_slice(&0xDEAD_BEEFu32.to_le_bytes());
        let with_garbage_crc = compute_page_checksum(&page).expect("len >= 26");
        assert_eq!(with_zero_crc, with_garbage_crc);
    }

    /// Slice-by-4 must give the same answer as the one-byte-at-a-time
    /// loop did before r192. The scalar oracle below is a verbatim copy of
    /// the original loop — running it alongside `checksum` over a range of
    /// lengths catches any rank-table miscompute.
    #[test]
    fn slice_by_4_matches_scalar_oracle() {
        fn scalar_oracle(bytes: &[u8]) -> u32 {
            let mut crc: u32 = 0;
            for &b in bytes {
                crc = (crc << 8) ^ T0[((crc >> 24) as u8 ^ b) as usize];
            }
            crc
        }
        // Cover every length class up to 32 bytes (one block, partial
        // tail of 0/1/2/3, multi-block), plus three sizes that mirror
        // the framing-bench scenarios (short / mid / max-page).
        let mut rng_state: u32 = 0x1234_5678;
        let mut bytes = Vec::with_capacity(65_535);
        for _ in 0..65_535 {
            // Cheap LCG to fill with deterministic non-pattern bytes;
            // a real RNG isn't needed because the test only cares about
            // sample diversity, not statistical randomness.
            rng_state = rng_state.wrapping_mul(1_103_515_245).wrapping_add(12_345);
            bytes.push((rng_state >> 16) as u8);
        }
        for &len in &[
            0, 1, 2, 3, 4, 5, 6, 7, 8, 9, 11, 15, 16, 17, 23, 31, 32, 33, 63, 64, 65, 127, 255,
            256, 257, 1023, 1024, 1025, 65_534, 65_535,
        ] {
            let prefix = &bytes[..len];
            assert_eq!(
                checksum(prefix),
                scalar_oracle(prefix),
                "slice-by-4 mismatch at len {}",
                len
            );
        }
    }

    /// `continue_checksum` must be associative: feeding the same input
    /// to it in two halves matches feeding it whole. This is the contract
    /// `compute_page_checksum` relies on to splice in the zero CRC field
    /// without reassembling a buffer.
    #[test]
    fn continue_checksum_associative() {
        let payload: Vec<u8> = (0u8..200).collect();
        let one_shot = checksum(&payload);
        for split in 0..=payload.len() {
            let a = continue_checksum(0, &payload[..split]);
            let b = continue_checksum(a, &payload[split..]);
            assert_eq!(b, one_shot, "split at {}", split);
        }
    }

    /// `advance_four_zero_bytes(0)` equals the result of running the
    /// scalar recurrence four times from state 0. This pins the unrolled
    /// helper so a future refactor catches a typo immediately.
    #[test]
    fn advance_four_zero_bytes_matches_scalar_loop() {
        let mut expected: u32 = 0;
        for _ in 0..CRC_FIELD_LEN {
            expected = (expected << 8) ^ T0[(expected >> 24) as usize];
        }
        assert_eq!(advance_four_zero_bytes(0), expected);
    }

    /// The slice-by-4 advancement tables must match the recurrence the
    /// scalar loop produces on the same input — running T0 once on byte
    /// `i` must give the same final state as advancing the CRC over
    /// `[i, 0, 0, 0]` byte-by-byte. (T1/T2/T3 are derived by feeding
    /// zero bytes through T0, so by induction they each match the loop
    /// at one more zero-byte rank.)
    #[test]
    fn slice_tables_consistent_with_t0_recurrence() {
        for i in 0u32..256 {
            // Direct: T1 entry for byte i.
            let from_table = T1[i as usize];
            // Reference: take the running state `T0[i]` and feed a zero
            // byte through T0 once more.
            let mut crc = T0[i as usize];
            crc = (crc << 8) ^ T0[(crc >> 24) as usize];
            assert_eq!(
                from_table, crc,
                "T1 disagrees with T0+1 zero byte at i={}",
                i
            );
        }
        for i in 0u32..256 {
            let from_table = T2[i as usize];
            let mut crc = T0[i as usize];
            for _ in 0..2 {
                crc = (crc << 8) ^ T0[(crc >> 24) as usize];
            }
            assert_eq!(
                from_table, crc,
                "T2 disagrees with T0+2 zero bytes at i={}",
                i
            );
        }
        for i in 0u32..256 {
            let from_table = T3[i as usize];
            let mut crc = T0[i as usize];
            for _ in 0..3 {
                crc = (crc << 8) ^ T0[(crc >> 24) as usize];
            }
            assert_eq!(
                from_table, crc,
                "T3 disagrees with T0+3 zero bytes at i={}",
                i
            );
        }
    }

    /// The slice-by-4 main loop must produce the same answer as a
    /// straightforward scalar loop on every length class that can leave
    /// a 0-, 1-, 2-, or 3-byte tail. This is a tighter focus than the
    /// generic oracle test above — it specifically exercises the
    /// `chunks_exact(4).remainder()` boundary.
    #[test]
    fn tail_lengths_match_scalar() {
        fn scalar(bytes: &[u8]) -> u32 {
            let mut crc: u32 = 0;
            for &b in bytes {
                crc = (crc << 8) ^ T0[((crc >> 24) as u8 ^ b) as usize];
            }
            crc
        }
        // 80, 81, 82, 83 cover all four mod-4 classes within a
        // multi-block payload (so >1 slice-by-4 iteration runs).
        for &len in &[80usize, 81, 82, 83] {
            let buf: Vec<u8> = (0..len).map(|i| (i ^ 0xA5) as u8).collect();
            assert_eq!(checksum(&buf), scalar(&buf), "mismatch at len {}", len);
        }
    }
}
