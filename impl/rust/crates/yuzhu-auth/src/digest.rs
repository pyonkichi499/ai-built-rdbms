//! Thin public digests for other crates (SQL `md5()` / `sha*()` functions),
//! so they need no direct dependency on the crypto crates.

use sha2::{Digest, Sha224, Sha256, Sha384, Sha512};

/// SHA-224.
pub fn sha224(data: &[u8]) -> [u8; 28] {
    Sha224::digest(data).into()
}
/// SHA-256.
pub fn sha256(data: &[u8]) -> [u8; 32] {
    Sha256::digest(data).into()
}
/// SHA-384.
pub fn sha384(data: &[u8]) -> [u8; 48] {
    Sha384::digest(data).into()
}
/// SHA-512.
pub fn sha512(data: &[u8]) -> [u8; 64] {
    Sha512::digest(data).into()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn hex(b: &[u8]) -> String {
        b.iter().fold(String::new(), |s, x| s + &format!("{x:02x}"))
    }

    #[test]
    fn known_vectors() {
        assert_eq!(
            hex(&sha256(b"abc")),
            "ba7816bf8f01cfea414140de5dae2223b00361a396177a9cb410ff61f20015ad"
        );
        assert_eq!(
            hex(&sha224(b"abc")),
            "23097d223405d8228642a477bda255b32aadbce4bda0b3f7e36c9da7"
        );
        assert_eq!(sha384(b"").len(), 48);
        assert_eq!(&hex(&sha512(b"abc"))[..16], "ddaf35a193617aba");
    }
}
