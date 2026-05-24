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

/// CRC-32/Ogg lookup table, generated at compile time.
const TABLE: [u32; 256] = {
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

/// Compute the CRC-32 over `bytes` using the Ogg parameters.
pub fn checksum(bytes: &[u8]) -> u32 {
    let mut crc: u32 = 0;
    for &b in bytes {
        crc = (crc << 8) ^ TABLE[((crc >> 24) as u8 ^ b) as usize];
    }
    crc
}

/// Byte offset of the CRC field within an Ogg page header (RFC 3533 §6).
pub const CRC_FIELD_OFFSET: usize = 22;

/// Byte length of the CRC field (32 bits).
pub const CRC_FIELD_LEN: usize = 4;

/// Compute the page checksum the way RFC 3533 §6 field 7 specifies:
/// the bytes 22..26 (CRC field) are treated as zero, then [`checksum`]
/// runs over the full page (header + lacing table + segment data).
///
/// `page_bytes` must be one complete page — at minimum 27 bytes (the
/// fixed header) and otherwise as long as `27 + n_segs + sum(lacing)`.
/// Returns `None` if the slice is too short to contain even the CRC
/// field (i.e. fewer than 26 bytes).
pub fn compute_page_checksum(page_bytes: &[u8]) -> Option<u32> {
    if page_bytes.len() < CRC_FIELD_OFFSET + CRC_FIELD_LEN {
        return None;
    }
    let mut crc: u32 = 0;
    for (i, &b) in page_bytes.iter().enumerate() {
        // Treat the CRC field itself as zero, per RFC 3533 §6 field 7.
        let b = if (CRC_FIELD_OFFSET..CRC_FIELD_OFFSET + CRC_FIELD_LEN).contains(&i) {
            0
        } else {
            b
        };
        crc = (crc << 8) ^ TABLE[((crc >> 24) as u8 ^ b) as usize];
    }
    Some(crc)
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
}
