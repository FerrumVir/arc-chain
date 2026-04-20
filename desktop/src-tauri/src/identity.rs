// ARC identity = BIP-39 phrase → ed25519 keypair → BLAKE3(pk) address.
//
// This MUST match how `arc-node --validator-seed <string>` derives its
// keypair, otherwise the seed phrase shown to a user in this app will
// not restore the same address when plugged into arc-node or arc-cli.
//
// Chain derivation (crates/arc-node/src/main.rs:310–311,
// crates/arc-crypto/src/signature.rs:404,521–522):
//
//     seed_bytes = blake3::derive_key(DOMAIN_TAG, seed_str.as_bytes())
//     signing_key = ed25519_dalek::SigningKey::from_bytes(&seed_bytes)
//     public_key  = signing_key.verifying_key().as_bytes()
//     address     = blake3::hash(public_key).as_bytes()  // 32 bytes
//
// The DOMAIN_TAG below matches the chain's constant verbatim. Do not change.

use crate::types::Identity;
use bip39::{Language, Mnemonic};
use ed25519_dalek::SigningKey;
use rand::rngs::OsRng;
use rand::RngCore;

const DOMAIN_TAG: &str = "ARC-chain-validator-keypair-v1";

/// Generate a fresh 12-word BIP-39 mnemonic and derive the ARC identity.
pub fn generate() -> Identity {
    let mut entropy = [0u8; 16]; // 128 bits → 12 words
    OsRng.fill_bytes(&mut entropy);
    let mnemonic = Mnemonic::from_entropy_in(Language::English, &entropy)
        .expect("valid entropy always yields a mnemonic");
    derive(&mnemonic.to_string())
        .expect("freshly-generated mnemonic derives successfully")
}

/// Derive an ARC identity from any seed string (typically a BIP-39 phrase).
/// The same input always produces the same (sk, pk, address) triple, on
/// every machine, forever.
pub fn derive(phrase: &str) -> Result<Identity, String> {
    // Validate BIP-39 format + checksum if it parses as one. Accept non-BIP-39
    // seeds too (matches arc-node's more permissive --validator-seed).
    let is_bip39 =
        Mnemonic::parse_in(Language::English, phrase.trim()).is_ok();
    let normalized = phrase.trim().to_string();

    let seed_bytes = blake3::derive_key(DOMAIN_TAG, normalized.as_bytes());
    let signing_key = SigningKey::from_bytes(&seed_bytes);
    let pk_bytes = signing_key.verifying_key().to_bytes();
    let address_bytes = blake3::hash(&pk_bytes);

    Ok(Identity {
        address: hex::encode(address_bytes.as_bytes()),
        public_key: format!("0x{}", hex::encode(pk_bytes)),
        seed_phrase: normalized,
        created_at: chrono::Utc::now().timestamp_millis(),
    })
    .map(|id| {
        // Tag logging only — identity still returned unchanged.
        if !is_bip39 {
            tracing::warn!(
                "imported identity is not a BIP-39 phrase — derivation still deterministic but no checksum protection"
            );
        }
        id
    })
}

/// Attempt to parse an input as a BIP-39 mnemonic, surfacing a clear error
/// to the user if checksum or word list validation fails.
pub fn validate_bip39(phrase: &str) -> Result<(), String> {
    Mnemonic::parse_in(Language::English, phrase.trim())
        .map(|_| ())
        .map_err(|e| format!("invalid seed phrase: {e}"))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn derivation_is_deterministic() {
        let phrase = "abandon abandon abandon abandon abandon abandon abandon abandon abandon abandon abandon about";
        let a = derive(phrase).unwrap();
        let b = derive(phrase).unwrap();
        assert_eq!(a.address, b.address);
        assert_eq!(a.public_key, b.public_key);
    }

    #[test]
    fn different_phrases_produce_different_addresses() {
        let a = derive("abandon abandon abandon abandon abandon abandon abandon abandon abandon abandon abandon about").unwrap();
        let b = derive("legal winner thank year wave sausage worth useful legal winner thank yellow").unwrap();
        assert_ne!(a.address, b.address);
    }

    #[test]
    fn address_is_64_hex_chars() {
        let id = generate();
        assert_eq!(id.address.len(), 64);
        assert!(id.address.chars().all(|c| c.is_ascii_hexdigit()));
    }

    #[test]
    fn generate_produces_valid_bip39() {
        let id = generate();
        assert!(validate_bip39(&id.seed_phrase).is_ok());
        let words: Vec<&str> = id.seed_phrase.split_whitespace().collect();
        assert_eq!(words.len(), 12);
    }

    #[test]
    fn bip39_with_typo_fails_validation() {
        // Swap a valid word for an invalid one
        let bad = "abandon abandon abandon abandon abandon abandon abandon abandon abandon abandon abandon notaword";
        assert!(validate_bip39(bad).is_err());
    }

    #[test]
    fn generated_phrase_restores_to_same_address() {
        let id1 = generate();
        let id2 = derive(&id1.seed_phrase).unwrap();
        assert_eq!(id1.address, id2.address);
        assert_eq!(id1.public_key, id2.public_key);
    }
}
