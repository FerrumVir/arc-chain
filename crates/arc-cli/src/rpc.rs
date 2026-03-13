//! HTTP client wrapper for the ARC Chain RPC node.

use anyhow::{Result, Context, bail};
use reqwest::Client;
use serde_json::Value;

/// HTTP client for communicating with an ARC Chain RPC node.
pub struct RpcClient {
    client: Client,
    base_url: String,
}

impl RpcClient {
    /// Create a new RPC client targeting the given base URL.
    pub fn new(base_url: &str) -> Self {
        Self {
            client: Client::new(),
            base_url: base_url.trim_end_matches('/').to_string(),
        }
    }

    /// `GET /info` — chain metadata (version, height, account count, mempool size).
    pub async fn get_info(&self) -> Result<Value> {
        let url = format!("{}/info", self.base_url);
        let resp = self.client.get(&url).send().await
            .with_context(|| format!("failed to connect to {}", url))?;
        self.handle_response(resp).await
    }

    /// `GET /account/{addr}` — account balance and nonce.
    pub async fn get_account(&self, addr: &str) -> Result<Value> {
        let url = format!("{}/account/{}", self.base_url, addr);
        let resp = self.client.get(&url).send().await
            .with_context(|| format!("failed to connect to {}", url))?;
        self.handle_response(resp).await
    }

    /// `GET /block/{height}` — block header and transaction list.
    pub async fn get_block(&self, height: u64) -> Result<Value> {
        let url = format!("{}/block/{}", self.base_url, height);
        let resp = self.client.get(&url).send().await
            .with_context(|| format!("failed to connect to {}", url))?;
        self.handle_response(resp).await
    }

    /// `GET /tx/{hash}/full` — full transaction with receipt.
    pub async fn get_tx(&self, hash: &str) -> Result<Value> {
        let url = format!("{}/tx/{}/full", self.base_url, hash);
        let resp = self.client.get(&url).send().await
            .with_context(|| format!("failed to connect to {}", url))?;
        self.handle_response(resp).await
    }

    /// `POST /tx/submit` — submit a transaction (JSON body).
    pub async fn submit_tx(&self, tx_json: Value) -> Result<Value> {
        let url = format!("{}/tx/submit", self.base_url);
        let resp = self.client.post(&url).json(&tx_json).send().await
            .with_context(|| format!("failed to connect to {}", url))?;
        self.handle_response(resp).await
    }

    /// `POST {faucet_url}/claim` — request testnet tokens from the faucet.
    pub async fn claim_faucet(&self, faucet_url: &str, addr: &str) -> Result<Value> {
        let url = format!("{}/claim", faucet_url.trim_end_matches('/'));
        let body = serde_json::json!({ "address": addr });
        let resp = self.client.post(&url).json(&body).send().await
            .with_context(|| format!("failed to connect to faucet at {}", url))?;
        self.handle_response(resp).await
    }

    /// Process an HTTP response: check status, parse JSON body.
    async fn handle_response(&self, resp: reqwest::Response) -> Result<Value> {
        let status = resp.status();
        let body = resp.text().await
            .context("failed to read response body")?;

        if !status.is_success() {
            bail!("RPC error (HTTP {}): {}", status.as_u16(), body);
        }

        serde_json::from_str(&body)
            .with_context(|| format!("failed to parse JSON response: {}", &body[..body.len().min(200)]))
    }
}
