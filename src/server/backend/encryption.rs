use aes_gcm::{
    aead::{Aead, KeyInit},
    Aes256Gcm, Key, Nonce,
};
use rand::Rng;

pub use crate::config::parse_hex_key;
use crate::server::error::{KeyzError, Result};

pub fn encrypt_partition(plaintext: &[u8], key: &[u8; 32]) -> Result<Vec<u8>> {
    let nonce_bytes: [u8; 12] = rand::thread_rng().gen();
    let cipher = Aes256Gcm::new(Key::<Aes256Gcm>::from_slice(key));
    let nonce = Nonce::from_slice(&nonce_bytes);

    let ciphertext = cipher
        .encrypt(nonce, plaintext)
        .map_err(|e| KeyzError::Encryption(e.to_string()))?;

    let mut result = Vec::with_capacity(12 + ciphertext.len());
    result.extend_from_slice(&nonce_bytes);
    result.extend(ciphertext);
    Ok(result)
}

pub fn decrypt_partition(data: &[u8], key: &[u8; 32]) -> Result<Vec<u8>> {
    if data.len() < 12 {
        return Err(KeyzError::Encryption("data too short for nonce".into()));
    }

    let (nonce_bytes, ciphertext) = data.split_at(12);
    let cipher = Aes256Gcm::new(Key::<Aes256Gcm>::from_slice(key));
    let nonce = Nonce::from_slice(nonce_bytes);

    cipher
        .decrypt(nonce, ciphertext)
        .map_err(|e| KeyzError::Encryption(e.to_string()))
}


#[cfg(test)]
mod tests {
    use super::*;

    fn test_key() -> [u8; 32] {
        let mut key = [0u8; 32];
        for (i, byte) in key.iter_mut().enumerate() {
            *byte = i as u8;
        }
        key
    }

    #[test]
    fn encrypt_decrypt_roundtrip() {
        let key = test_key();
        let plaintext = b"hello world, this is a secret message";

        let encrypted = encrypt_partition(plaintext, &key).unwrap();
        assert_ne!(&encrypted[12..], plaintext); // ciphertext differs

        let decrypted = decrypt_partition(&encrypted, &key).unwrap();
        assert_eq!(decrypted, plaintext);
    }

    #[test]
    fn decrypt_with_wrong_key_fails() {
        let key = test_key();
        let mut wrong_key = test_key();
        wrong_key[0] ^= 0xFF;

        let encrypted = encrypt_partition(b"secret", &key).unwrap();
        assert!(decrypt_partition(&encrypted, &wrong_key).is_err());
    }

    #[test]
    fn decrypt_tampered_ciphertext_fails() {
        let key = test_key();
        let mut encrypted = encrypt_partition(b"secret", &key).unwrap();
        let last = encrypted.len() - 1;
        encrypted[last] ^= 0xFF; // tamper
        assert!(decrypt_partition(&encrypted, &key).is_err());
    }

    #[test]
    fn decrypt_too_short_fails() {
        let key = test_key();
        assert!(decrypt_partition(&[0u8; 5], &key).is_err());
    }

    #[test]
    fn encrypt_empty_data() {
        let key = test_key();
        let encrypted = encrypt_partition(b"", &key).unwrap();
        let decrypted = decrypt_partition(&encrypted, &key).unwrap();
        assert!(decrypted.is_empty());
    }

    #[test]
    fn parse_hex_key_valid() {
        let hex = "000102030405060708090a0b0c0d0e0f101112131415161718191a1b1c1d1e1f";
        let key = parse_hex_key(hex).unwrap();
        for i in 0..32 {
            assert_eq!(key[i], i as u8);
        }
    }

    #[test]
    fn parse_hex_key_wrong_length() {
        assert!(parse_hex_key("abcd").is_err());
    }

    #[test]
    fn parse_hex_key_invalid_chars() {
        let hex = "zz0102030405060708090a0b0c0d0e0f101112131415161718191a1b1c1d1e1f";
        assert!(parse_hex_key(hex).is_err());
    }
}
