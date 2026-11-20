use anyhow::{Context, Result, bail};
use reqwest::Client;
use serde_json::Value;

pub struct RpcClient {
    client: Client,
    base_url: String,
}

impl RpcClient {
    pub fn new(base_url: &str) -> Self {
        Self {
            client: Client::new(),
            base_url: base_url.trim_end_matches('/').to_string(),
        }
    }

    pub async fn get_info(&self) -> Result<Value> {
        let url = format!("{}/info", self.base_url);
        let resp = self
            .client
            .get(&url)
            .send()
            .await
            .with_context(|| format!("failed to connect to {}", url))?;
        self.handle_response(resp).await
    }

    pub async fn get_health(&self) -> Result<Value> {
        let url = format!("{}/health", self.base_url);
        let resp = self
            .client
            .get(&url)
            .send()
            .await
            .with_context(|| format!("failed to connect to {}", url))?;
        self.handle_response(resp).await
    }

    pub async fn get_account(&self, addr: &str) -> Result<Value> {
        let url = format!("{}/account/{}", self.base_url, addr);
        let resp = self
            .client
            .get(&url)
            .send()
            .await
            .with_context(|| format!("failed to connect to {}", url))?;
        self.handle_response(resp).await
    }

    pub async fn get_block(&self, height: u64) -> Result<Value> {
        let url = format!("{}/block/{}", self.base_url, height);
        let resp = self
            .client
            .get(&url)
            .send()
            .await
            .with_context(|| format!("failed to connect to {}", url))?;
        self.handle_response(resp).await
    }

    pub async fn get_tx(&self, hash: &str) -> Result<Value> {
        let url = format!("{}/tx/{}/full", self.base_url, hash);
        let resp = self
            .client
            .get(&url)
            .send()
            .await
            .with_context(|| format!("failed to connect to {}", url))?;
        self.handle_response(resp).await
    }

    pub async fn submit_tx(&self, tx_json: Value) -> Result<Value> {
        let url = format!("{}/tx/submit", self.base_url);
        let resp = self
            .client
            .post(&url)
            .json(&tx_json)
            .send()
            .await
            .with_context(|| format!("failed to connect to {}", url))?;
        self.handle_response(resp).await
    }

    pub async fn claim_faucet(&self, faucet_url: &str, addr: &str) -> Result<Value> {
        let url = format!("{}/claim", faucet_url.trim_end_matches('/'));
        let body = serde_json::json!({ "address": addr });
        let resp = self
            .client
            .post(&url)
            .json(&body)
            .send()
            .await
            .with_context(|| format!("failed to connect to faucet at {}", url))?;
        self.handle_response(resp).await
    }

    async fn handle_response(&self, resp: reqwest::Response) -> Result<Value> {
        let status = resp.status();
        let body = resp.text().await.context("failed to read response body")?;

        if !status.is_success() {
            bail!("RPC error (HTTP {}): {}", status.as_u16(), body);
        }

        serde_json::from_str(&body).with_context(|| {
            format!(
                "failed to parse JSON response: {}",
                &body[..body.len().min(200)]
            )
        })
    }
}
