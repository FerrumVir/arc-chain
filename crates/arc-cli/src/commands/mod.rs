pub mod balance;
pub mod block;
pub mod faucet;
pub mod info;
pub mod transfer;
pub mod tx;

/// Validate that an address is 64 hex characters (no 0x prefix).
pub fn validate_address(addr: &str) -> Result<(), String> {
    let addr = addr.strip_prefix("0x").unwrap_or(addr);
    if addr.len() != 64 {
        return Err(format!(
            "Invalid address: expected 64 hex characters, got {}. Do not include 0x prefix.",
            addr.len()
        ));
    }
    if !addr.chars().all(|c| c.is_ascii_hexdigit()) {
        return Err("Invalid address: contains non-hex characters.".into());
    }
    Ok(())
}
