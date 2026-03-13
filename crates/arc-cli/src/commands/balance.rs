//! `arc balance <address>` — query an account's balance and nonce.

use anyhow::Result;
use crate::rpc::RpcClient;

pub async fn run(rpc: &RpcClient, address: &str) -> Result<()> {
    let data = rpc.get_account(address).await?;

    let balance = data.get("balance")
        .and_then(|v| v.as_u64())
        .unwrap_or(0);
    let nonce = data.get("nonce")
        .and_then(|v| v.as_u64())
        .unwrap_or(0);

    let short_addr = if address.len() > 16 {
        format!("{}...{}", &address[..8], &address[address.len()-8..])
    } else {
        address.to_string()
    };

    println!("Account: {}", short_addr);
    println!("Balance: {} ARC", format_amount(balance));
    println!("Nonce:   {}", nonce);

    Ok(())
}

/// Format a raw balance with thousand separators.
fn format_amount(amount: u64) -> String {
    let s = amount.to_string();
    let mut result = String::new();
    for (i, c) in s.chars().rev().enumerate() {
        if i > 0 && i % 3 == 0 {
            result.push(',');
        }
        result.push(c);
    }
    result.chars().rev().collect()
}
