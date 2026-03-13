//! `arc info` — display chain metadata.

use anyhow::Result;
use crate::rpc::RpcClient;

pub async fn run(rpc: &RpcClient) -> Result<()> {
    let data = rpc.get_info().await?;

    let version = data.get("version")
        .and_then(|v| v.as_str())
        .unwrap_or("unknown");
    let height = data.get("block_height")
        .or_else(|| data.get("height"))
        .and_then(|v| v.as_u64())
        .unwrap_or(0);
    let accounts = data.get("accounts")
        .or_else(|| data.get("account_count"))
        .and_then(|v| v.as_u64())
        .unwrap_or(0);
    let mempool = data.get("mempool")
        .or_else(|| data.get("mempool_size"))
        .and_then(|v| v.as_u64())
        .unwrap_or(0);

    println!("ARC Chain");
    println!("  Version:      {}", version);
    println!("  Block Height: {}", height);
    println!("  Accounts:     {}", accounts);
    println!("  Mempool:      {}", mempool);

    Ok(())
}
