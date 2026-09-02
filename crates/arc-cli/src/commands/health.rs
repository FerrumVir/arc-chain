//! `arc health` - validate and print the node readiness status.

use crate::rpc::RpcClient;
use anyhow::{Result, bail};
use serde_json::Value;

fn ready_status(data: &Value) -> Result<&str> {
    let Some(status) = data.get("status").and_then(Value::as_str) else {
        bail!("health response has no string status field");
    };
    match status {
        "ok" | "degraded" => Ok(status),
        other => bail!("node is not ready (status={other})"),
    }
}

pub async fn run(rpc: &RpcClient) -> Result<()> {
    let data = rpc.get_health().await?;
    println!("{}", ready_status(&data)?);
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn accepts_only_explicit_ready_statuses() {
        assert_eq!(ready_status(&json!({"status": "ok"})).unwrap(), "ok");
        assert_eq!(
            ready_status(&json!({"status": "degraded"})).unwrap(),
            "degraded"
        );
        assert!(ready_status(&json!({"status": "starting"})).is_err());
        assert!(ready_status(&json!({"status": true})).is_err());
        assert!(ready_status(&json!({"message": "status=ok"})).is_err());
    }
}
