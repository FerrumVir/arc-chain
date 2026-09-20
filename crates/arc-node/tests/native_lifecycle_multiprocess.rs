//! Paid native-inference lifecycle against a REAL node process.
//!
//! Every existing native-inference test constructs the worker in-process and
//! calls `run_one` directly, and every "restart" is a drop-and-reopen of a Rust
//! struct against the same directory. None of them start `arc-node`, none go
//! through RPC, and none prove that a node process actually executes an
//! admitted request or produces the finalization settlement needs.
//!
//! This spawns the binary, drives the flow over HTTP, and asserts on receipts.
//!
//! **This is protocol integration coverage.** It runs with the deterministic
//! test executor (`native-test-executor`), which loads no model. It qualifies
//! no model and says nothing about production inference quality — that stays
//! behind `CanonicalI8NativeExecutor::load_qualified`, which independently
//! enforces the artifact hash, the exact versioned profile/generation
//! commitments and an explicit reference-qualification decision.
#![cfg(feature = "native-test-executor")]

use arc_crypto::signature::KeyPair;
use arc_crypto::{Hash256, hash_bytes};
use arc_types::inference_contract::{
    INFERENCE_CONTRACT_VERSION, InferenceDomain, InferenceJob, InferenceRequest,
};
use arc_types::transaction::{NativeInferenceRequestBody, Transaction, TxBody};
use std::path::{Path, PathBuf};
use std::process::{Child, Command};
use std::time::{Duration, Instant};

fn validator_keypair(seed: &str) -> KeyPair {
    let bytes = blake3::derive_key("ARC-chain-validator-keypair-v1", seed.as_bytes());
    KeyPair::Ed25519(ed25519_dalek::SigningKey::from_bytes(&bytes))
}

/// Requester identity. Domain-separated from the validator derivation so a
/// requester key can never collide with a validator key.
fn requester_keypair(seed: &str) -> KeyPair {
    let bytes = blake3::derive_key("ARC-native-lifecycle-test-requester-v1", seed.as_bytes());
    KeyPair::Ed25519(ed25519_dalek::SigningKey::from_bytes(&bytes))
}

fn curl(args: &[&str]) -> (u32, String) {
    // curl rather than an HTTP crate: this test must not add a dependency to
    // exercise a binary it is already shelling out to.
    let out = Command::new("curl")
        .args(["-s", "-w", "\n%{http_code}", "--max-time", "10"])
        .args(args)
        .output()
        .expect("curl must be available");
    let body = String::from_utf8_lossy(&out.stdout).to_string();
    let (body, code) = body.rsplit_once('\n').unwrap_or((body.as_str(), "0"));
    (code.trim().parse().unwrap_or(0), body.to_string())
}

fn get_json(port: u16, path: &str) -> Option<serde_json::Value> {
    let url = format!("http://127.0.0.1:{port}{path}");
    let (code, body) = curl(&[&url]);
    if code != 200 {
        return None;
    }
    serde_json::from_str(&body).ok()
}

fn wait_for<F: Fn() -> bool>(timeout: Duration, label: &str, f: F) {
    let start = Instant::now();
    while start.elapsed() < timeout {
        if f() {
            return;
        }
        std::thread::sleep(Duration::from_millis(500));
    }
    panic!("timed out after {timeout:?} waiting for: {label}");
}

struct NodeProcess {
    child: Child,
    port: u16,
    data_dir: PathBuf,
}

impl NodeProcess {
    fn kill(&mut self) {
        let _ = self.child.kill();
        let _ = self.child.wait();
    }
}

impl Drop for NodeProcess {
    fn drop(&mut self) {
        self.kill();
    }
}

struct Fixture {
    dir: tempfile::TempDir,
    genesis: PathBuf,
    activation: PathBuf,
    validator: KeyPair,
    requester: KeyPair,
    allowed: [Hash256; 4],
}

impl Fixture {
    fn new(tag: &str) -> Self {
        let dir = tempfile::tempdir().unwrap();
        let validator = validator_keypair(tag);
        let requester = requester_keypair(tag);
        // One tuple, reused by the activation allowlist and by every job below.
        // A job whose tuple is not on the allowlist is supposed to be refused,
        // which a later case exercises deliberately.
        let m = hash_bytes(format!("{tag}-model").as_bytes());
        let p = hash_bytes(format!("{tag}-profile").as_bytes());
        let g = hash_bytes(format!("{tag}-generation").as_bytes());
        let a = hash_bytes(format!("{tag}-assignment").as_bytes());

        let genesis = dir.path().join("genesis.toml");
        std::fs::write(
            &genesis,
            format!(
                "[chain]\n\
                 name = \"arc-native-lifecycle\"\n\
                 chain_id = \"0x415243\"\n\
                 validator_set_complete = false\n\n\
                 [[accounts]]\naddress = \"{v}\"\nbalance = 1_000_000_000_000\n\n\
                 [[accounts]]\naddress = \"{r}\"\nbalance = 1_000_000_000_000\n\n\
                 [[validators]]\naddress = \"{v}\"\nstake = 6666667\n",
                v = validator.address().to_hex(),
                r = requester.address().to_hex(),
            ),
        )
        .unwrap();

        let activation = dir.path().join("activation.json");
        std::fs::write(
            &activation,
            serde_json::json!({
                "recovery_epoch": 0,
                "allowed_executions": [{
                    "model_hash": m.to_hex(),
                    "profile_hash": p.to_hex(),
                    "generation_hash": g.to_hex(),
                    "assignment_hash": a.to_hex(),
                }]
            })
            .to_string(),
        )
        .unwrap();

        Self {
            dir,
            genesis,
            activation,
            validator,
            requester,
            allowed: [m, p, g, a],
        }
    }

    fn spawn(&self, tag: &str, port: u16, p2p: u16, data_dir: &Path) -> NodeProcess {
        std::fs::create_dir_all(data_dir).unwrap();
        let child = Command::new(env!("CARGO_BIN_EXE_arc-node"))
            .args([
                "--rpc",
                &format!("127.0.0.1:{port}"),
                "--p2p-port",
                &p2p.to_string(),
                "--data-dir",
                data_dir.to_str().unwrap(),
                "--genesis",
                self.genesis.to_str().unwrap(),
                "--insecure-dev-validator-seed",
                "--validator-seed",
                tag,
                "--stake",
                "6666667",
                "--native-inference-activation",
                self.activation.to_str().unwrap(),
                "--native-inference-runtime",
                "--native-inference-test-executor",
            ])
            .stdout(std::process::Stdio::null())
            .stderr(std::process::Stdio::null())
            .spawn()
            .expect("arc-node must start");
        NodeProcess {
            child,
            port,
            data_dir: data_dir.to_path_buf(),
        }
    }

    /// A signed request bound to the node's activated domain.
    fn signed_request(
        &self,
        domain: InferenceDomain,
        nonce: u64,
        expires_at: u64,
        tuple: [Hash256; 4],
    ) -> (Transaction, Hash256) {
        let input = format!("native lifecycle input {nonce}").into_bytes();
        let job = InferenceJob {
            version: INFERENCE_CONTRACT_VERSION,
            domain,
            requester: self.requester.address(),
            nonce,
            model_hash: tuple[0],
            profile_hash: tuple[1],
            input_hash: hash_bytes(&input),
            generation_hash: tuple[2],
            assignment_hash: tuple[3],
            max_tokens: 8,
            max_output_bytes: 128,
            execution_price: 10,
            reserved_max_payment: 100,
            expires_at,
        };
        let request_id = job.request_id();
        // InferenceRequest::sign owns the job and produces the bound signature;
        // the requester signature is over the job, not over the envelope.
        let request = InferenceRequest::sign(job, &self.requester)
            .expect("the requester must be able to sign its own job");
        let body = TxBody::NativeInferenceRequest(NativeInferenceRequestBody {
            request,
            input_blob: input,
        });
        let mut tx = Transaction {
            tx_type: body.tx_type(),
            from: self.requester.address(),
            nonce,
            body,
            fee: 0,
            gas_limit: 5_000_000,
            hash: Hash256::ZERO,
            signature: arc_crypto::signature::Signature::null(),
            sig_verified: false,
        };
        tx.hash = tx.compute_hash();
        tx.signature = self
            .requester
            .sign(&tx.hash)
            .expect("signing the transaction envelope must succeed");
        (tx, request_id)
    }
}

fn submit(port: u16, tx: &Transaction) -> (u32, String) {
    let payload = serde_json::to_string(tx).unwrap();
    let url = format!("http://127.0.0.1:{port}/tx/submit_signed");
    curl(&[
        "-X",
        "POST",
        "-H",
        "Content-Type: application/json",
        "-d",
        &payload,
        &url,
    ])
}

#[test]
fn native_request_is_executed_and_finalized_by_a_real_node_process() {
    let tag = "native-lifecycle-a";
    let fx = Fixture::new(tag);
    let data = fx.dir.path().join("node");
    let mut node = fx.spawn(tag, 9961, 9161, &data);

    wait_for(Duration::from_secs(90), "node health", || {
        get_json(node.port, "/health").is_some()
    });

    // Activation must actually be live, or nothing below means anything.
    let ctx = get_json(node.port, "/native-inference/context")
        .expect("the contract must be activated by the startup flag");
    assert_eq!(ctx["candidate_protocol"], 4);
    let domain = InferenceDomain {
        chain_genesis: Hash256::from_hex(ctx["chain_genesis"].as_str().unwrap()).unwrap(),
        recovery_epoch: ctx["recovery_epoch"].as_u64().unwrap(),
        validator_set_hash: Hash256::from_hex(ctx["validator_set_hash"].as_str().unwrap()).unwrap(),
    };

    let expires = u64::MAX / 2;
    let (tx, request_id) = fx.signed_request(domain, 0, expires, fx.allowed);
    let (code, body) = submit(node.port, &tx);
    assert_eq!(code, 200, "signed native request must be admitted: {body}");

    // The worker must execute it and the finalization must land, with no
    // further help from this test.
    wait_for(
        Duration::from_secs(120),
        "the request to reach a terminal receipt",
        || {
            get_json(node.port, &format!("/native-inference/receipt/{}", request_id.to_hex()))
                .and_then(|r| r["status"].as_str().map(|s| s.to_string()))
                .is_some_and(|s| s == "Finalized")
        },
    );

    let receipt =
        get_json(node.port, &format!("/native-inference/receipt/{}", request_id.to_hex())).unwrap();
    assert_eq!(receipt["status"], "Finalized");

    // Durable across a real process death: kill -9 equivalent, restart, and the
    // receipt must still be there without re-running the request.
    node.kill();
    let mut restarted = fx.spawn(tag, 9961, 9161, &data);
    wait_for(Duration::from_secs(90), "node health after restart", || {
        get_json(restarted.port, "/health").is_some()
    });
    let after = get_json(
        restarted.port,
        &format!("/native-inference/receipt/{}", request_id.to_hex()),
    )
    .expect("the receipt must survive a real process restart");
    assert_eq!(
        after["status"], "Finalized",
        "a settled request must still be settled after the node process is killed and restarted"
    );
    restarted.kill();
}

#[test]
fn duplicate_and_unlisted_native_requests_are_refused_by_a_real_node_process() {
    let tag = "native-lifecycle-b";
    let fx = Fixture::new(tag);
    let data = fx.dir.path().join("node");
    let mut node = fx.spawn(tag, 9962, 9162, &data);

    wait_for(Duration::from_secs(90), "node health", || {
        get_json(node.port, "/health").is_some()
    });
    let ctx = get_json(node.port, "/native-inference/context").expect("activated");
    let domain = InferenceDomain {
        chain_genesis: Hash256::from_hex(ctx["chain_genesis"].as_str().unwrap()).unwrap(),
        recovery_epoch: ctx["recovery_epoch"].as_u64().unwrap(),
        validator_set_hash: Hash256::from_hex(ctx["validator_set_hash"].as_str().unwrap()).unwrap(),
    };

    let (tx, _id) = fx.signed_request(domain, 0, u64::MAX / 2, fx.allowed);
    let (first, _) = submit(node.port, &tx);
    assert_eq!(first, 200, "the first submission must be admitted");

    // Replay of the identical signed transaction must not be admitted twice.
    let (second, body) = submit(node.port, &tx);
    assert_ne!(
        second, 200,
        "replaying an identical signed request must be refused, got 200: {body}"
    );

    // A tuple that is not on the activation allowlist must be refused, so the
    // allowlist is an admission control and not decoration.
    let bogus = hash_bytes(b"a model that was never allowed");
    let (unlisted, _) = fx.signed_request(domain, 1, u64::MAX / 2, [bogus, bogus, bogus, bogus]);
    let (code, body) = submit(node.port, &unlisted);
    assert_ne!(
        code, 200,
        "a request outside the activation allowlist must be refused, got 200: {body}"
    );
    node.kill();
}
