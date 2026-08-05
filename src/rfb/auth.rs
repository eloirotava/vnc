//! VNC authentication (RFB security type 2): DES with a bit-reversed key.

use cipher::generic_array::GenericArray;
use cipher::{BlockEncrypt, KeyInit};
use des::Des;

/// Encrypt a 16-byte challenge the way RFB expects: the password is truncated
/// or zero-padded to 8 bytes, every key byte has its bits reversed, and the
/// challenge is encrypted as two ECB blocks.
pub fn encrypt_challenge(password: &str, challenge: &[u8; 16]) -> [u8; 16] {
    let mut key = [0u8; 8];
    for (dst, src) in key.iter_mut().zip(password.as_bytes()) {
        *dst = src.reverse_bits();
    }
    let cipher = Des::new(GenericArray::from_slice(&key));
    let mut out = [0u8; 16];
    for (i, chunk) in challenge.chunks(8).enumerate() {
        let mut block = GenericArray::clone_from_slice(chunk);
        cipher.encrypt_block(&mut block);
        out[i * 8..i * 8 + 8].copy_from_slice(&block);
    }
    out
}

/// 16 random bytes for the auth challenge, from the OS.
pub fn challenge() -> std::io::Result<[u8; 16]> {
    use std::io::Read;
    let mut buf = [0u8; 16];
    std::fs::File::open("/dev/urandom")?.read_exact(&mut buf)?;
    Ok(buf)
}

/// Constant-time comparison so a wrong response cannot be probed byte by byte.
pub fn constant_time_eq(a: &[u8], b: &[u8]) -> bool {
    if a.len() != b.len() {
        return false;
    }
    let mut diff = 0u8;
    for (x, y) in a.iter().zip(b) {
        diff |= x ^ y;
    }
    diff == 0
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Vectors generated with noVNC's own DES implementation
    /// (`web/novnc/core/crypto/des.js`), so a mismatch here means real
    /// clients would fail to authenticate.
    const CHALLENGE: [u8; 16] = [
        3, 20, 37, 54, 71, 88, 105, 122, 139, 156, 173, 190, 207, 224, 241, 2,
    ];

    #[test]
    fn matches_novnc_for_a_short_password() {
        assert_eq!(
            encrypt_challenge("test", &CHALLENGE),
            [60, 203, 75, 9, 225, 251, 179, 159, 199, 30, 6, 123, 208, 115, 70, 148]
        );
    }

    #[test]
    fn matches_novnc_on_an_all_zero_challenge() {
        let out = encrypt_challenge("test", &[0u8; 16]);
        assert_eq!(out[..8], [119, 223, 168, 28, 159, 215, 180, 7]);
        // ECB: identical plaintext blocks encrypt identically.
        assert_eq!(out[..8], out[8..]);
    }

    #[test]
    fn password_is_truncated_to_eight_bytes() {
        let expected = [84, 12, 169, 124, 69, 230, 73, 113, 32, 242, 22, 101, 226, 27, 25, 207];
        assert_eq!(encrypt_challenge("12345678", &CHALLENGE), expected);
        assert_eq!(encrypt_challenge("123456789abc", &CHALLENGE), expected);
    }

    #[test]
    fn eq_is_length_sensitive() {
        assert!(constant_time_eq(b"abc", b"abc"));
        assert!(!constant_time_eq(b"abc", b"abd"));
        assert!(!constant_time_eq(b"abc", b"ab"));
    }

    #[test]
    fn challenges_differ() {
        assert_ne!(challenge().unwrap(), challenge().unwrap());
    }
}
