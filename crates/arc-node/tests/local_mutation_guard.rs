#[path = "../examples/support/local_rpc.rs"]
mod local_rpc;

use local_rpc::require_loopback_rpc;

#[test]
fn accepts_only_loopback_rpc_origins() {
    assert_eq!(
        require_loopback_rpc("http://127.0.0.1:9090").unwrap(),
        "http://127.0.0.1:9090"
    );
    assert_eq!(
        require_loopback_rpc("https://127.42.0.9:9443/").unwrap(),
        "https://127.42.0.9:9443"
    );
    assert_eq!(
        require_loopback_rpc("http://[::1]:9090").unwrap(),
        "http://[::1]:9090"
    );
}

#[test]
fn rejects_public_wildcard_and_credentialed_rpc_origins() {
    for value in [
        "http://140.82.16.112:9090",
        "https://example.com",
        "http://localhost:9090",
        "http://127.0.0.1.example.com:9090",
        "http://0.0.0.0:9090",
        "http://user:pass@127.0.0.1:9090",
        "ftp://127.0.0.1:9090",
        "http://127.0.0.1:9090/rpc",
        "http://127.0.0.1:9090?redirect=https://example.com",
    ] {
        assert!(
            require_loopback_rpc(value).is_err(),
            "unsafe RPC origin was accepted: {value}"
        );
    }
}
