use anyhow::{Context, Result, bail};
use std::net::IpAddr;

pub fn require_loopback_rpc(raw: &str) -> Result<String> {
    let url = reqwest::Url::parse(raw).context("RPC URL is invalid")?;
    if !matches!(url.scheme(), "http" | "https") {
        bail!("RPC URL must use http or https");
    }
    if !url.username().is_empty() || url.password().is_some() {
        bail!("RPC URL must not contain credentials");
    }
    if url.query().is_some() || url.fragment().is_some() {
        bail!("RPC URL must not contain a query or fragment");
    }
    if url.path() != "/" {
        bail!("RPC URL must be an origin without a path");
    }

    let host = url.host_str().context("RPC URL is missing a host")?;
    let numeric_host = host
        .strip_prefix('[')
        .and_then(|value| value.strip_suffix(']'))
        .unwrap_or(host);
    let is_loopback = numeric_host
        .parse::<IpAddr>()
        .map(|address| address.is_loopback())
        .unwrap_or(false);
    if !is_loopback {
        bail!("RPC URL must target a numeric loopback address");
    }

    Ok(url.as_str().trim_end_matches('/').to_string())
}
