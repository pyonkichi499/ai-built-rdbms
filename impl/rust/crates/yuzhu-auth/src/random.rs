//! OS random numbers (CSPRNG).

use std::io;

/// Fill `buf` with random bytes from the OS.
pub fn fill(buf: &mut [u8]) -> io::Result<()> {
    getrandom::fill(buf).map_err(|e| io::Error::other(e.to_string()))
}

/// Return `N` random bytes.
pub fn bytes<const N: usize>() -> io::Result<[u8; N]> {
    let mut b = [0u8; N];
    fill(&mut b)?;
    Ok(b)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn fill_is_not_all_zero() {
        let mut b = [0u8; 32];
        fill(&mut b).unwrap();
        assert!(b.iter().any(|&x| x != 0));
    }

    #[test]
    fn bytes_const_len_and_distinct() {
        let a = bytes::<16>().unwrap();
        let b = bytes::<16>().unwrap();
        assert_ne!(a, b);
        assert!(bytes::<0>().unwrap().is_empty());
    }
}
