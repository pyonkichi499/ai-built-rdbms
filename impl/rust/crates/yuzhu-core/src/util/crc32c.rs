//! CRC32C (Castagnoli), hand-written, slice-by-8. Used by the control file and
//! the WAL (`m2.md` §3.2, `m3.md` §6.1).
//!
//! Reflected polynomial `0x82F63B78`, initial value `0xFFFF_FFFF`, final bit
//! inversion. `crc32c(b"123456789") == 0xE306_9283`.

const POLY: u32 = 0x82F6_3B78;

const fn make_tables() -> [[u32; 256]; 8] {
    let mut t = [[0u32; 256]; 8];
    let mut i = 0;
    while i < 256 {
        #[allow(clippy::cast_possible_truncation)]
        let mut c = i as u32;
        let mut k = 0;
        while k < 8 {
            c = if c & 1 != 0 { (c >> 1) ^ POLY } else { c >> 1 };
            k += 1;
        }
        t[0][i] = c;
        i += 1;
    }
    let mut i = 0;
    while i < 256 {
        let mut c = t[0][i];
        let mut s = 1;
        while s < 8 {
            c = t[0][(c & 0xFF) as usize] ^ (c >> 8);
            t[s][i] = c;
            s += 1;
        }
        i += 1;
    }
    t
}

static TABLES: [[u32; 256]; 8] = make_tables();

/// Initial (not yet inverted) state for [`crc32c_append`].
pub const CRC32C_INIT: u32 = 0xFFFF_FFFF;

/// CRC32C of `data`.
pub fn crc32c(data: &[u8]) -> u32 {
    crc32c_finish(crc32c_append(CRC32C_INIT, data))
}

/// Continues an unfinished state (slice-by-8):
/// `crc32c_finish(crc32c_append(crc32c_append(CRC32C_INIT, a), b)) == crc32c(a ++ b)`.
pub fn crc32c_append(state: u32, data: &[u8]) -> u32 {
    let mut crc = state;
    #[allow(clippy::chunks_exact_to_as_chunks)]
    let mut chunks = data.chunks_exact(8);
    for c in &mut chunks {
        let lo = u32::from_le_bytes([c[0], c[1], c[2], c[3]]) ^ crc;
        let hi = u32::from_le_bytes([c[4], c[5], c[6], c[7]]);
        crc = TABLES[7][(lo & 0xFF) as usize]
            ^ TABLES[6][((lo >> 8) & 0xFF) as usize]
            ^ TABLES[5][((lo >> 16) & 0xFF) as usize]
            ^ TABLES[4][(lo >> 24) as usize]
            ^ TABLES[3][(hi & 0xFF) as usize]
            ^ TABLES[2][((hi >> 8) & 0xFF) as usize]
            ^ TABLES[1][((hi >> 16) & 0xFF) as usize]
            ^ TABLES[0][(hi >> 24) as usize];
    }
    for &b in chunks.remainder() {
        crc = TABLES[0][((crc ^ u32::from(b)) & 0xFF) as usize] ^ (crc >> 8);
    }
    crc
}

/// Final bit inversion.
pub fn crc32c_finish(state: u32) -> u32 {
    !state
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn known_vectors() {
        assert_eq!(crc32c(b"123456789"), 0xE306_9283);
        assert_eq!(crc32c(b""), 0);
        // RFC 3720 (iSCSI) test vectors.
        assert_eq!(crc32c(&[0u8; 32]), 0x8A91_36AA);
        assert_eq!(crc32c(&[0xFFu8; 32]), 0x62A8_AB43);
        let inc: Vec<u8> = (0..32).collect();
        assert_eq!(crc32c(&inc), 0x46DD_794E);
        let dec: Vec<u8> = (0..32).rev().collect();
        assert_eq!(crc32c(&dec), 0x113F_DB5C);
    }

    #[test]
    fn append_is_concatenation() {
        let data: Vec<u8> = (0..=255).collect();
        for split in [0, 1, 100, 255, 256] {
            let (a, b) = data.split_at(split);
            let st = crc32c_append(crc32c_append(CRC32C_INIT, a), b);
            assert_eq!(crc32c_finish(st), crc32c(&data));
        }
    }

    fn naive(data: &[u8]) -> u32 {
        let mut c = !0u32;
        for &b in data {
            c ^= u32::from(b);
            for _ in 0..8 {
                c = if c & 1 != 0 { (c >> 1) ^ POLY } else { c >> 1 };
            }
        }
        !c
    }

    proptest::proptest! {
        #[test]
        fn matches_naive(data in proptest::collection::vec(proptest::prelude::any::<u8>(), 0..300)) {
            proptest::prop_assert_eq!(crc32c(&data), naive(&data));
        }

        #[test]
        fn split_append_matches(
            data in proptest::collection::vec(proptest::prelude::any::<u8>(), 0..200),
            split in 0usize..200,
        ) {
            let split = split.min(data.len());
            let (a, b) = data.split_at(split);
            let st = crc32c_append(crc32c_append(CRC32C_INIT, a), b);
            proptest::prop_assert_eq!(crc32c_finish(st), crc32c(&data));
        }
    }
}
