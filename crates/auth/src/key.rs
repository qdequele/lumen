//! Virtual-key generation and hashing.
//!
//! A virtual key is `fg-` + 32 cryptographically random bytes (hex). Only its
//! BLAKE3 hash is ever stored: the keys are 256-bit uniform random, so a fast
//! cryptographic hash gives full preimage resistance - a slow password KDF
//! (argon2) exists for *low-entropy* secrets and would burn ~100 ms of CPU on
//! every authenticated request for nothing (pillar 1).

use rand::RngCore;
use std::fmt;

/// A freshly generated virtual key in the clear.
///
/// This value exists only between generation and the single admin response
/// that reveals it. Its `Debug` output is redacted so it can never leak
/// through logs or error chains.
pub struct PlaintextKey(String);

impl PlaintextKey {
    /// The clear-text key. Call sites must never log the returned value.
    #[must_use]
    pub fn reveal(&self) -> &str {
        &self.0
    }
}

impl fmt::Debug for PlaintextKey {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str("PlaintextKey(REDACTED)")
    }
}

/// Generate a new virtual key: `fg-` + 64 hex chars (32 random bytes).
#[must_use]
pub fn generate() -> PlaintextKey {
    let mut bytes = [0_u8; 32];
    rand::rng().fill_bytes(&mut bytes);
    let mut key = String::with_capacity(3 + 64);
    key.push_str("fg-");
    push_hex(&mut key, &bytes);
    PlaintextKey(key)
}

/// The stored form of a key: lowercase hex of its BLAKE3 hash.
#[must_use]
pub fn hash_key(plaintext: &str) -> String {
    blake3::hash(plaintext.as_bytes()).to_hex().to_string()
}

/// Generate a random identifier (16 bytes, hex) for DB primary keys.
#[must_use]
pub(crate) fn random_id() -> String {
    let mut bytes = [0_u8; 16];
    rand::rng().fill_bytes(&mut bytes);
    let mut id = String::with_capacity(32);
    push_hex(&mut id, &bytes);
    id
}

/// Append lowercase hex of `bytes` to `out` (no intermediate allocations).
fn push_hex(out: &mut String, bytes: &[u8]) {
    const HEX: &[u8; 16] = b"0123456789abcdef";
    for b in bytes {
        out.push(HEX[usize::from(b >> 4)] as char);
        out.push(HEX[usize::from(b & 0x0f)] as char);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn generated_keys_have_the_documented_shape() {
        let key = generate();
        let s = key.reveal();
        assert!(s.starts_with("fg-"));
        assert_eq!(s.len(), 67);
        assert!(s[3..].bytes().all(|b| b.is_ascii_hexdigit()));
    }

    #[test]
    fn generated_keys_are_unique() {
        assert_ne!(generate().reveal(), generate().reveal());
    }

    #[test]
    fn hash_is_deterministic_and_not_the_plaintext() {
        let key = generate();
        let h1 = hash_key(key.reveal());
        let h2 = hash_key(key.reveal());
        assert_eq!(h1, h2);
        assert_ne!(h1, key.reveal());
        assert_eq!(h1.len(), 64); // blake3 = 32 bytes hex
    }

    #[test]
    fn debug_never_shows_the_key() {
        let key = generate();
        let dbg = format!("{key:?}");
        assert!(!dbg.contains(key.reveal()));
        assert!(dbg.contains("REDACTED"));
    }
}
