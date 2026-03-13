//! `arc tx <hash>` — display transaction details.

use anyhow::Result;
use crate::rpc::RpcClient;

pub async fn run(rpc: &RpcClient, hash: &str) -> Result<()> {
    let data = rpc.get_tx(hash).await?;

    let tx_type = data.get("tx_type")
        .and_then(|v| v.as_str())
        .unwrap_or("unknown");
    let from = data.get("from")
        .and_then(|v| v.as_str())
        .unwrap_or("unknown");
    let nonce = data.get("nonce")
        .and_then(|v| v.as_u64())
        .unwrap_or(0);
    let fee = data.get("fee")
        .and_then(|v| v.as_u64())
        .unwrap_or(0);

    let short_hash = if hash.len() > 16 {
        format!("{}...{}", &hash[..10], &hash[hash.len()-6..])
    } else {
        hash.to_string()
    };

    println!("Transaction {}", short_hash);
    println!("  Type:   {}", tx_type);
    println!("  From:   {}", truncate_addr(from));
    println!("  Nonce:  {}", nonce);
    println!("  Fee:    {} ARC", fee);

    // Print body details based on type
    if let Some(body) = data.get("body") {
        print_body(tx_type, body);
    }

    // Print receipt if present
    if let Some(receipt) = data.get("receipt") {
        let success = receipt.get("success")
            .and_then(|v| v.as_bool())
            .unwrap_or(false);
        let gas_used = receipt.get("gas_used")
            .and_then(|v| v.as_u64())
            .unwrap_or(0);
        println!("  Status: {}", if success { "Success" } else { "Failed" });
        println!("  Gas:    {}", gas_used);
    }

    Ok(())
}

fn print_body(tx_type: &str, body: &serde_json::Value) {
    match tx_type {
        "Transfer" => {
            if let Some(to) = body.get("to").and_then(|v| v.as_str()).or_else(|| {
                body.get("Transfer").and_then(|t| t.get("to")).and_then(|v| v.as_str())
            }) {
                println!("  To:     {}", truncate_addr(to));
            }
            if let Some(amount) = body.get("amount").and_then(|v| v.as_u64()).or_else(|| {
                body.get("Transfer").and_then(|t| t.get("amount")).and_then(|v| v.as_u64())
            }) {
                println!("  Amount: {} ARC", amount);
            }
        }
        "Settle" => {
            if let Some(amount) = body.get("amount").and_then(|v| v.as_u64()).or_else(|| {
                body.get("Settle").and_then(|t| t.get("amount")).and_then(|v| v.as_u64())
            }) {
                println!("  Amount: {} ARC", amount);
            }
        }
        _ => {
            // For other types, print the raw body compactly
            if let Ok(s) = serde_json::to_string(body) {
                if s.len() <= 200 {
                    println!("  Body:   {}", s);
                } else {
                    println!("  Body:   {}...", &s[..200]);
                }
            }
        }
    }
}

fn truncate_addr(addr: &str) -> String {
    if addr.len() > 16 {
        format!("{}...{}", &addr[..8], &addr[addr.len()-8..])
    } else {
        addr.to_string()
    }
}
