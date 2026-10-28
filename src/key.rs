//! Master-key derivation.
//!
//! The master key is never stored. It's derived from a passphrase with Argon2id
//! (memory-hard, the current standard for password hashing) plus a random,
//! non-secret salt kept in the store's `meta.json`. Same passphrase + same salt
//! => same key; a wrong passphrase simply yields a different key, so the ends
//! decrypt to garbage and files won't open. There is intentionally no
//! "password correct" check — nothing in the store confirms the key, which keeps
//! an attacker from testing guesses offline against a verifier.

use crate::crypto::{Key, KEY_LEN};
use argon2::Argon2;
use rand::RngCore;

pub const SALT_LEN: usize = 16;
pub type Salt = [u8; SALT_LEN];

/// A fresh random salt for a brand-new store.
pub fn random_salt() -> Salt {
    let mut salt = [0u8; SALT_LEN];
    rand::thread_rng().fill_bytes(&mut salt);
    salt
}

/// Derive the 32-byte master key from a passphrase and the store's salt.
///
/// Uses Argon2id with the crate's default parameters. A real product would pin
/// explicit, tuned parameters (memory/time cost) and record them in `meta.json`
/// so future opens stay reproducible even if the defaults change.
pub fn derive_key(passphrase: &str, salt: &Salt) -> Key {
    let mut key = [0u8; KEY_LEN];
    Argon2::default()
        .hash_password_into(passphrase.as_bytes(), salt, &mut key)
        .expect("argon2 derivation into a 32-byte buffer cannot fail");
    key
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn same_inputs_same_key() {
        let salt = [5u8; SALT_LEN];
        assert_eq!(derive_key("hunter2", &salt), derive_key("hunter2", &salt));
    }

    #[test]
    fn different_passphrase_differs() {
        let salt = [5u8; SALT_LEN];
        assert_ne!(derive_key("hunter2", &salt), derive_key("hunter3", &salt));
    }

    #[test]
    fn different_salt_differs() {
        assert_ne!(
            derive_key("hunter2", &[1u8; SALT_LEN]),
            derive_key("hunter2", &[2u8; SALT_LEN])
        );
    }
}
