//! `arc block <height>` — display block details.

use anyhow::Result;
use crate::rpc::RpcClient;

pub async fn run(rpc: &RpcClient, height: u64) -> Result<()> {
    let data = rpc.get_block(height).await?;

    let block_height = data.get("height")
        .and_then(|v| v.as_u64())
        .unwrap_or(height);
    let hash = data.get("hash")
        .and_then(|v| v.as_str())
        .unwrap_or("unknown");
    let parent = data.get("parent_hash")
        .or_else(|| data.get("parent"))
        .and_then(|v| v.as_str())
        .unwrap_or("unknown");

    let tx_count = if let Some(n) = data.get("tx_count").and_then(|v| v.as_u64()) {
        n
    } else if let Some(arr) = data.get("transactions").and_then(|v| v.as_array()) {
        arr.len() as u64
    } else {
        0
    };

    let timestamp = data.get("timestamp")
        .and_then(|v| v.as_u64())
        .unwrap_or(0);
    let producer = data.get("producer")
        .or_else(|| data.get("proposer"))
        .and_then(|v| v.as_str())
        .unwrap_or("unknown");

    println!("Block #{}", block_height);
    println!("  Hash:       {}", truncate_hash(hash));
    println!("  Parent:     {}", truncate_hash(parent));
    println!("  Tx Count:   {}", tx_count);
    println!("  Timestamp:  {}", timestamp);
    println!("  Producer:   {}", truncate_hash(producer));

    Ok(())
}

fn truncate_hash(h: &str) -> String {
    if h.len() > 16 {
        format!("{}...{}", &h[..10], &h[h.len()-6..])
    } else {
        h.to_string()
    }
}
