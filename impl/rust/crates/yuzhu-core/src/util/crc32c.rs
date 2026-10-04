//! CRC32C (Castagnoli), hand-written. Used by the control file and, in M3,
//! the WAL (`m2.md` §3.2).
//!
//! Reflected polynomial `0x82F63B78`, initial value `0xFFFF_FFFF`, final bit
//! inversion. `crc32c(b"123456789") == 0xE306_9283`.

const POLY: u32 = 0x82F6_3B78;

const fn make_table() -> [u32; 256] {
    let mut table = [0u32; 256];
    let mut i = 0;
    while i < 256 {
        #[allow(clippy::cast_possible_truncation)]
        let mut c = i as u32;
        let mut k = 0;
        while k < 8 {
            c = if c & 1 != 0 { (c >> 1) ^ POLY } else { c >> 1 };
            k += 1;
        }
        table[i] = c;
        i += 1;
    }
    table
}

static TABLE: [u32; 256] = make_table();

/// CRC32C of `data`.
pub fn crc32c(data: &[u8]) -> u32 {
    crc32c_append(0, data)
}

/// Continues a finished CRC: `crc32c_append(crc32c(a), b) == crc32c(a ++ b)`.
pub fn crc32c_append(crc: u32, data: &[u8]) -> u32 {
    let mut state = !crc;
    for &b in data {
        state = TABLE[usize::from(state.to_le_bytes()[0] ^ b)] ^ (state >> 8);
    }
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
            assert_eq!(crc32c_append(crc32c(a), b), crc32c(&data));
        }
    }
}
