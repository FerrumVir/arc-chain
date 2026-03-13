//! `arc transfer` — send ARC tokens to another address.

use anyhow::{Result, Context};
use crate::keygen;
use crate::rpc::RpcClient;

pub async fn run(rpc: &RpcClient, from_keyfile: &str, to: &str, amount: u64) -> Result<()> {
    // 1. Load sender keypair
    let keypair = keygen::load_keyfile(from_keyfile)
        .with_context(|| format!("failed to load keyfile '{}'", from_keyfile))?;
    let sender_addr = keypair.address().to_hex();

    // 2. Get sender's current nonce
    let account_data = rpc.get_account(&sender_addr).await
        .with_context(|| "failed to fetch sender account")?;
    let nonce = account_data.get("nonce")
        .and_then(|v| v.as_u64())
        .unwrap_or(0);

    // 3. Build transfer request
    //    For the initial version, we submit to /tx/submit which handles
    //    unsigned transfers server-side. A future version will build and
    //    sign locally once /tx/submit_signed exists.
    let tx_json = serde_json::json!({
        "from": sender_addr,
        "to": to,
        "amount": amount,
        "nonce": nonce,
    });

    // 4. Submit
    println!("Sending {} ARC", amount);
    println!("  From:  {}...{}", &sender_addr[..8], &sender_addr[sender_addr.len()-8..]);
    println!("  To:    {}...{}", &to[..to.len().min(8)], &to[to.len().saturating_sub(8)..]);
    println!("  Nonce: {}", nonce);
    println!();

    let result = rpc.submit_tx(tx_json).await
        .context("failed to submit transfer")?;

    if let Some(tx_hash) = result.get("hash").and_then(|v| v.as_str())
        .or_else(|| result.get("tx_hash").and_then(|v| v.as_str()))
    {
        println!("Transaction submitted: {}", tx_hash);
    } else {
        println!("Response: {}", serde_json::to_string_pretty(&result)?);
    }

    Ok(())
}
