//! Wallet primitives that must stay on the Rust side of the Tauri boundary.
//!
//! The WebView sends only a destination and a decimal ARC amount. Recovery
//! material is read from the private Rust store and used here; it is never an
//! IPC argument or result.

use crate::types::Identity;
use arc_crypto::{Hash256, KeyPair};
use arc_types::Transaction;
use ed25519_dalek::SigningKey;
use std::net::IpAddr;

const KEY_DERIVATION_DOMAIN: &str = "ARC-chain-validator-keypair-v1";
pub const ARC_BASE_UNITS: u64 = arc_types::economics::ARC_BASE_UNITS;
/// Protocol-v3 transfer floor. Kept explicit here because the desktop depends
/// only on the wire/type crate; v3 recovery-domain signing is the signal that
/// the state admission rule applies.
pub const V3_MIN_TRANSFER_FEE_BASE: u64 = 1;

pub fn transfer_fee_base(transaction_domain: Option<Hash256>) -> u64 {
    if transaction_domain.is_some() {
        V3_MIN_TRANSFER_FEE_BASE
    } else {
        0
    }
}

/// Accept production HTTPS origins and loopback-only HTTP for local dev.
/// Wallet writes never follow redirects (configured in `lib.rs`).
pub fn validate_rpc_origin(value: &str) -> Result<String, String> {
    let parsed = reqwest::Url::parse(value.trim())
        .map_err(|_| "wallet RPC must be a valid origin URL".to_string())?;
    if !parsed.username().is_empty()
        || parsed.password().is_some()
        || parsed.query().is_some()
        || parsed.fragment().is_some()
        || parsed.path() != "/"
    {
        return Err(
            "wallet RPC must be an origin with no credentials, path, query, or fragment"
                .to_string(),
        );
    }
    if parsed.port() == Some(0) {
        return Err("wallet RPC port must be non-zero".to_string());
    }
    let host = parsed
        .host_str()
        .ok_or_else(|| "wallet RPC origin is missing a host".to_string())?;
    let loopback = host.eq_ignore_ascii_case("localhost")
        || host
            .parse::<IpAddr>()
            .map(|ip| ip.is_loopback())
            .unwrap_or(false);
    match parsed.scheme() {
        "https" => {}
        "http" if loopback => {}
        "http" => {
            return Err(
                "remote wallet RPC must use HTTPS; plaintext HTTP is local-dev only".to_string(),
            );
        }
        _ => {
            return Err(
                "wallet RPC must use HTTPS (or HTTP on loopback for local dev)".to_string(),
            );
        }
    }
    Ok(value.trim().trim_end_matches('/').to_string())
}

/// Parse a plain decimal ARC amount into exact base units.
///
/// Scientific notation, signs, separators, and more than nine fractional
/// digits are rejected. Nothing is rounded and no floating point is used.
pub fn parse_arc_amount(input: &str) -> Result<u64, String> {
    let value = input.trim();
    if value.is_empty() {
        return Err("enter an ARC amount".to_string());
    }
    if value.starts_with(['+', '-']) || value.contains(['e', 'E', ',']) {
        return Err(
            "amount must be a plain positive decimal with at most 9 decimal places".to_string(),
        );
    }

    let mut parts = value.split('.');
    let whole_text = parts.next().unwrap_or_default();
    let fraction_text = parts.next();
    if parts.next().is_some() {
        return Err("amount contains more than one decimal point".to_string());
    }
    if whole_text.is_empty() && fraction_text.is_none() {
        return Err("enter an ARC amount".to_string());
    }
    if !whole_text.is_empty() && !whole_text.bytes().all(|b| b.is_ascii_digit()) {
        return Err("amount must contain digits and one optional decimal point".to_string());
    }

    let fraction_text = fraction_text.unwrap_or("");
    if value.contains('.') && fraction_text.is_empty() {
        return Err("enter digits after the decimal point".to_string());
    }
    if fraction_text.len() > arc_types::economics::DECIMALS as usize {
        return Err(
            "ARC supports at most 9 decimal places; the amount was not rounded".to_string(),
        );
    }
    if !fraction_text.bytes().all(|b| b.is_ascii_digit()) {
        return Err("amount must contain digits and one optional decimal point".to_string());
    }

    let whole = if whole_text.is_empty() {
        0
    } else {
        whole_text
            .parse::<u64>()
            .map_err(|_| "amount is larger than ARC's base-unit limit".to_string())?
    };
    let whole_base = whole
        .checked_mul(ARC_BASE_UNITS)
        .ok_or_else(|| "amount is larger than ARC's base-unit limit".to_string())?;

    let mut fraction_base = if fraction_text.is_empty() {
        0
    } else {
        fraction_text
            .parse::<u64>()
            .map_err(|_| "invalid ARC fraction".to_string())?
    };
    for _ in fraction_text.len()..arc_types::economics::DECIMALS as usize {
        fraction_base = fraction_base
            .checked_mul(10)
            .ok_or_else(|| "amount is larger than ARC's base-unit limit".to_string())?;
    }

    whole_base
        .checked_add(fraction_base)
        .ok_or_else(|| "amount is larger than ARC's base-unit limit".to_string())
}

/// Format exact base units as ARC without rounding or scientific notation.
pub fn format_arc_amount(base_units: u64) -> String {
    let whole = base_units / ARC_BASE_UNITS;
    let fraction = base_units % ARC_BASE_UNITS;
    if fraction == 0 {
        return whole.to_string();
    }
    let fraction = format!("{fraction:09}");
    format!("{whole}.{}", fraction.trim_end_matches('0'))
}

fn parse_address(value: &str, field: &str) -> Result<Hash256, String> {
    let clean = value
        .trim()
        .strip_prefix("0x")
        .or_else(|| value.trim().strip_prefix("0X"))
        .unwrap_or(value.trim());
    Hash256::from_hex(clean)
        .map_err(|_| format!("{field} must be exactly 32 bytes (64 hexadecimal characters)"))
}

/// Build and sign an ARC transfer from the Rust-only identity.
pub fn signed_transfer(
    identity: &Identity,
    recipient: &str,
    amount_base: u64,
    nonce: u64,
    transaction_domain: Option<Hash256>,
) -> Result<Transaction, String> {
    if amount_base == 0 {
        return Err("amount must be greater than zero".to_string());
    }

    let expected_from = parse_address(&identity.address, "stored wallet address")?;
    let to = parse_address(recipient, "recipient address")?;
    if expected_from == to {
        return Err("recipient must be different from this wallet".to_string());
    }

    let secret = blake3::derive_key(KEY_DERIVATION_DOMAIN, identity.seed_phrase.as_bytes());
    let keypair = KeyPair::Ed25519(SigningKey::from_bytes(&secret));
    if keypair.address() != expected_from {
        return Err("stored wallet identity does not match its recovery phrase".to_string());
    }

    let mut tx = Transaction::new_transfer(expected_from, to, amount_base, nonce);
    tx.fee = transfer_fee_base(transaction_domain);
    match transaction_domain {
        Some(domain) => tx
            .sign_in_domain(&keypair, &domain)
            .map_err(|e| format!("could not sign transfer in the active recovery domain: {e}"))?,
        None => tx
            .sign(&keypair)
            .map_err(|e| format!("could not sign transfer: {e}"))?,
    }
    Ok(tx)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::identity;

    const PHRASE: &str = "abandon abandon abandon abandon abandon abandon abandon abandon abandon abandon abandon about";

    #[test]
    fn arc_amounts_are_exact_to_nine_decimals() {
        assert_eq!(parse_arc_amount("1").unwrap(), 1_000_000_000);
        assert_eq!(parse_arc_amount("2.5").unwrap(), 2_500_000_000);
        assert_eq!(parse_arc_amount(".000000001").unwrap(), 1);
        assert_eq!(parse_arc_amount("0.00001").unwrap(), 10_000);
        assert_eq!(parse_arc_amount("18446744073.709551615").unwrap(), u64::MAX);
    }

    #[test]
    fn wallet_origins_reject_remote_plaintext_and_url_smuggling() {
        assert_eq!(
            validate_rpc_origin("https://149.28.32.76/").unwrap(),
            "https://149.28.32.76"
        );
        assert!(validate_rpc_origin("http://127.0.0.1:9090").is_ok());
        assert!(validate_rpc_origin("http://localhost:9090").is_ok());
        for invalid in [
            "http://149.28.32.76:9090",
            "https://user:pass@example.com",
            "https://example.com/rpc",
            "https://example.com?to=evil",
            "https://example.com#fragment",
            "file:///tmp/node",
        ] {
            assert!(validate_rpc_origin(invalid).is_err(), "accepted {invalid}");
        }
    }

    #[test]
    fn arc_amount_parser_never_rounds_or_accepts_ambiguous_syntax() {
        for invalid in [
            "",
            "-1",
            "+1",
            "1e3",
            "1,000",
            "1.",
            "1.0000000000",
            "18446744073.709551616",
            "18446744074",
            "1.2.3",
        ] {
            assert!(parse_arc_amount(invalid).is_err(), "accepted {invalid:?}");
        }
    }

    #[test]
    fn formatting_is_exact_and_trims_only_fractional_zeroes() {
        assert_eq!(format_arc_amount(0), "0");
        assert_eq!(format_arc_amount(10_000), "0.00001");
        assert_eq!(format_arc_amount(2_500_000_000), "2.5");
        assert_eq!(format_arc_amount(u64::MAX), "18446744073.709551615");
    }

    #[test]
    fn transfer_is_signed_by_the_stored_phrase_in_the_exact_domain() {
        let identity = identity::derive(PHRASE).unwrap();
        let recipient = "11".repeat(32);
        let domain = Hash256([7u8; 32]);
        let tx = signed_transfer(&identity, &recipient, 2_500_000_000, 4, Some(domain)).unwrap();

        assert_eq!(tx.from.to_hex(), identity.address);
        assert_eq!(tx.nonce, 4);
        assert_eq!(tx.fee, V3_MIN_TRANSFER_FEE_BASE);
        tx.verify_signature_in_domain(&domain).unwrap();
        assert!(
            tx.verify_signature().is_err(),
            "domain-bound tx verified as legacy"
        );
    }

    #[test]
    fn legacy_transfer_keeps_zero_fee_while_v3_fee_is_signed() {
        let identity = identity::derive(PHRASE).unwrap();
        let recipient = "11".repeat(32);
        let legacy = signed_transfer(&identity, &recipient, 1, 0, None).unwrap();
        assert_eq!(legacy.fee, 0);
        legacy.verify_signature().unwrap();

        let domain = Hash256([9u8; 32]);
        let v3 = signed_transfer(&identity, &recipient, 1, 0, Some(domain)).unwrap();
        assert_eq!(v3.fee, 1);
        v3.verify_signature_in_domain(&domain).unwrap();
    }

    #[test]
    fn transfer_rejects_a_tampered_stored_address() {
        let mut identity = identity::derive(PHRASE).unwrap();
        identity.address = "22".repeat(32);
        let err = signed_transfer(&identity, &"11".repeat(32), 1, 0, None).unwrap_err();
        assert!(err.contains("does not match"));
    }
}
