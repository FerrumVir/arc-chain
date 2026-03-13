//! `arc faucet <address>` — request testnet tokens from the faucet.

use anyhow::Result;
use crate::rpc::RpcClient;

pub async fn run(rpc: &RpcClient, address: &str, faucet_url: &str) -> Result<()> {
    println!("Requesting tokens from faucet for {}...", &address[..address.len().min(16)]);

    let data = rpc.claim_faucet(faucet_url, address).await?;

    if let Some(amount) = data.get("amount").and_then(|v| v.as_u64()) {
        println!("Received {} ARC", amount);
    } else if let Some(msg) = data.get("message").and_then(|v| v.as_str()) {
        println!("{}", msg);
    } else if let Some(tx_hash) = data.get("tx_hash").and_then(|v| v.as_str()) {
        println!("Faucet tx: {}", tx_hash);
    } else {
        println!("Faucet response: {}", serde_json::to_string_pretty(&data)?);
    }

    Ok(())
}
