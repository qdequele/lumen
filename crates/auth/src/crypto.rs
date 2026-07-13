//! AES-256-GCM sealing of provider keys at rest.
//!
//! The default mode for provider keys remains environment variables; storing
//! them in the database is opt-in and always encrypted with a master key from
//! `FERROGATE_MASTER_KEY` (64 hex chars = 32 bytes). The master key itself is
//! never persisted and never printed — `MasterKey`'s `Debug` is redacted.

use crate::AuthError;
use aes_gcm::aead::{Aead, KeyInit};
use aes_gcm::{Aes256Gcm, Key, Nonce};
use rand::RngCore;
use std::fmt;

/// AES-GCM nonce length in bytes.
const NONCE_LEN: usize = 12;

/// The 32-byte master key used to seal provider keys at rest.
pub struct MasterKey([u8; 32]);

impl fmt::Debug for MasterKey {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str("MasterKey(REDACTED)")
    }
}

impl MasterKey {
    /// Parse the `FERROGATE_MASTER_KEY` env value: exactly 64 hex characters.
    pub fn from_env_value(value: &str) -> Result<Self, AuthError> {
        let trimmed = value.trim();
        if !trimmed.is_ascii() || trimmed.len() != 64 {
            return Err(AuthError::InvalidMasterKey(
                "expected 64 hex characters (32 bytes)",
            ));
        }
        let mut bytes = [0_u8; 32];
        for (i, chunk) in trimmed.as_bytes().chunks_exact(2).enumerate() {
            let hi = hex_val(chunk[0]);
            let lo = hex_val(chunk[1]);
            match (hi, lo) {
                (Some(h), Some(l)) => bytes[i] = (h << 4) | l,
                _ => {
                    return Err(AuthError::InvalidMasterKey(
                        "expected 64 hex characters (32 bytes)",
                    ))
                }
            }
        }
        Ok(Self(bytes))
    }

    /// Encrypt `plaintext`; returns `nonce || ciphertext` (nonce is 12 random
    /// bytes, fresh per call).
    pub fn seal(&self, plaintext: &[u8]) -> Result<Vec<u8>, AuthError> {
        let cipher = Aes256Gcm::new(Key::<Aes256Gcm>::from_slice(&self.0));
        let mut nonce_bytes = [0_u8; NONCE_LEN];
        rand::rng().fill_bytes(&mut nonce_bytes);
        let nonce = Nonce::from_slice(&nonce_bytes);
        let ciphertext = cipher
            .encrypt(nonce, plaintext)
            .map_err(|_| AuthError::Decrypt)?;
        let mut sealed = Vec::with_capacity(NONCE_LEN + ciphertext.len());
        sealed.extend_from_slice(&nonce_bytes);
        sealed.extend_from_slice(&ciphertext);
        Ok(sealed)
    }

    /// Decrypt a `nonce || ciphertext` blob produced by [`seal`](Self::seal).
    ///
    /// # Errors
    ///
    /// [`AuthError::Decrypt`] on a wrong key or tampered data — deliberately
    /// detail-free.
    pub fn open(&self, sealed: &[u8]) -> Result<Vec<u8>, AuthError> {
        if sealed.len() <= NONCE_LEN {
            return Err(AuthError::Decrypt);
        }
        let (nonce_bytes, ciphertext) = sealed.split_at(NONCE_LEN);
        let cipher = Aes256Gcm::new(Key::<Aes256Gcm>::from_slice(&self.0));
        cipher
            .decrypt(Nonce::from_slice(nonce_bytes), ciphertext)
            .map_err(|_| AuthError::Decrypt)
    }
}

/// Decode one lowercase/uppercase hex digit.
fn hex_val(b: u8) -> Option<u8> {
    match b {
        b'0'..=b'9' => Some(b - b'0'),
        b'a'..=b'f' => Some(b - b'a' + 10),
        b'A'..=b'F' => Some(b - b'A' + 10),
        _ => None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn master(fill: char) -> MasterKey {
        MasterKey::from_env_value(&fill.to_string().repeat(64)).expect("valid key")
    }

    #[test]
    fn roundtrip_seals_and_opens() {
        let key = master('a');
        let sealed = key.seal(b"sk-secret").expect("seal");
        assert_eq!(key.open(&sealed).expect("open"), b"sk-secret");
    }

    #[test]
    fn ciphertext_never_contains_the_plaintext() {
        let key = master('a');
        let sealed = key.seal(b"sk-visible-secret").expect("seal");
        let window = b"sk-visible-secret";
        assert!(!sealed.windows(window.len()).any(|w| w == window));
    }

    #[test]
    fn nonces_are_fresh_per_call() {
        let key = master('a');
        let a = key.seal(b"same").expect("seal");
        let b = key.seal(b"same").expect("seal");
        assert_ne!(a, b, "same plaintext must never seal identically");
    }

    #[test]
    fn wrong_key_fails_closed() {
        let sealed = master('a').seal(b"data").expect("seal");
        assert!(matches!(master('b').open(&sealed), Err(AuthError::Decrypt)));
    }

    #[test]
    fn tampered_ciphertext_is_rejected() {
        let key = master('a');
        let mut sealed = key.seal(b"data").expect("seal");
        if let Some(last) = sealed.last_mut() {
            *last ^= 0x01;
        }
        assert!(matches!(key.open(&sealed), Err(AuthError::Decrypt)));
    }

    #[test]
    fn short_blob_is_rejected_without_panic() {
        assert!(matches!(
            master('a').open(&[0_u8; 5]),
            Err(AuthError::Decrypt)
        ));
    }

    #[test]
    fn malformed_env_values_are_rejected() {
        assert!(MasterKey::from_env_value("too-short").is_err());
        assert!(MasterKey::from_env_value(&"g".repeat(64)).is_err());
        assert!(MasterKey::from_env_value(&"é".repeat(32)).is_err());
    }

    #[test]
    fn debug_is_redacted() {
        let dbg = format!("{:?}", master('a'));
        assert!(!dbg.contains("aaaa"));
        assert!(dbg.contains("REDACTED"));
    }
}
