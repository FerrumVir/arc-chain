//! `arc block <height>` — display block details.

use anyhow::Result;
use crate::rpc::RpcClient;

pub async fn run(rpc: &RpcClient, height: u64) -> Result<()> {
    let data = rpc.get_block(height).await?;

    // Fields may be at top level or nested under "header" depending on the
    // serialization format. Check both locations.
    let header = data.get("header");

    let block_height = data.get("height")
        .or_else(|| header.and_then(|h| h.get("height")))
        .and_then(|v| v.as_u64())
        .unwrap_or(height);
    let hash = data.get("hash")
        .and_then(|v| v.as_str())
        .unwrap_or("unknown");
    let parent = data.get("parent_hash")
        .or_else(|| header.and_then(|h| h.get("parent_hash")))
        .or_else(|| data.get("parent"))
        .and_then(|v| v.as_str())
        .unwrap_or("unknown");

    let tx_count = data.get("tx_count")
        .or_else(|| header.and_then(|h| h.get("tx_count")))
        .and_then(|v| v.as_u64())
        .or_else(|| data.get("tx_hashes").and_then(|v| v.as_array()).map(|a| a.len() as u64))
        .unwrap_or(0);

    let timestamp = data.get("timestamp")
        .or_else(|| header.and_then(|h| h.get("timestamp")))
        .and_then(|v| v.as_u64())
        .unwrap_or(0);
    let producer = data.get("producer")
        .or_else(|| header.and_then(|h| h.get("producer")))
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
