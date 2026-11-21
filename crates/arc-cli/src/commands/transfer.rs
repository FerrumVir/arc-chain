use crate::keygen;
use crate::rpc::RpcClient;
use anyhow::{Context, Result};

pub async fn run(rpc: &RpcClient, from_keyfile: &str, to: &str, amount: u64) -> Result<()> {
    if let Err(e) = super::validate_address(to) {
        anyhow::bail!("{}", e);
    }

    let keypair = keygen::load_keyfile(from_keyfile)
        .with_context(|| format!("failed to load keyfile '{}'", from_keyfile))?;
    let sender_addr = keypair.address().to_hex();

    let nonce = match rpc.get_account(&sender_addr).await {
        Ok(data) => data.get("nonce").and_then(|v| v.as_u64()).unwrap_or(0),
        Err(e) => {
            let msg = format!("{:#}", e);
            if msg.contains("404") {
                0
            } else {
                return Err(e).context("failed to fetch sender account");
            }
        }
    };

    let tx_json = serde_json::json!({
        "from": sender_addr,
        "to": to,
        "amount": amount,
        "nonce": nonce,
    });

    println!("Sending {} ARC", amount);
    println!(
        "  From:  {}...{}",
        &sender_addr[..8],
        &sender_addr[sender_addr.len() - 8..]
    );
    println!(
        "  To:    {}...{}",
        &to[..to.len().min(8)],
        &to[to.len().saturating_sub(8)..]
    );
    println!("  Nonce: {}", nonce);
    println!();

    let result = rpc
        .submit_tx(tx_json)
        .await
        .context("failed to submit transfer")?;

    if let Some(tx_hash) = result
        .get("hash")
        .and_then(|v| v.as_str())
        .or_else(|| result.get("tx_hash").and_then(|v| v.as_str()))
    {
        println!("Transaction submitted: {}", tx_hash);
    } else {
        println!("Response: {}", serde_json::to_string_pretty(&result)?);
    }

    Ok(())
}
