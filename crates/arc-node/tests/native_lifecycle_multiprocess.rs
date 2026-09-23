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
                 validator_set_complete = false\n\
                 instance_id = \"{instance}\"\n\n\
                 [[accounts]]\naddress = \"{v}\"\nbalance = 1_000_000_000_000\n\n\
                 [[accounts]]\naddress = \"{r}\"\nbalance = 1_000_000_000_000\n\n\
                 [[validators]]\naddress = \"{v}\"\nstake = 6666667\n",
                instance = chain_instance_id(),
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
            // Retain the node's own log next to its data dir. A discarded
            // log is why an earlier staged run could report "height stayed 0"
            // without being able to say why; the consensus loop's own
            // proposal/drain/execution lines are the evidence that decides it.
            .env(
                "RUST_LOG",
                "arc_node=info,arc_node::consensus=debug,arc_consensus=debug",
            )
            .stdout(std::process::Stdio::from(
                std::fs::File::create(data_dir.join("node.log")).expect("node log"),
            ))
            .stderr(std::process::Stdio::from(
                std::fs::File::options()
                    .append(true)
                    .open(data_dir.join("node.log"))
                    .expect("node log"),
            ))
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

/// Track each stage separately so a failure names the stage that did not
/// happen, instead of timing out on the last one and implying the worker broke.
/// Pull the consensus-relevant lines out of a retained node log. Used to turn
/// "height stayed 0" into a statement about WHY, from the node's own record.
fn consensus_trace(data_dir: &Path) -> String {
    let Ok(text) = std::fs::read_to_string(data_dir.join("node.log")) else {
        return "<no node.log retained>".to_string();
    };
    let keys = [
        "Consensus loop started",
        "Drained",
        "Mempool reported entries but drain returned none",
        "Block produced",
        "Canonical single-validator block execution failed",
        "Cannot advance round",
        "Advanced to new round",
        "Block committed",
        "DAG block committed",
        "observing only",
        "Propose-Verify",
        // A consensus loop that RETURNS stops the chain silently. Every one of
        // its exit paths logs at error level, so keep those too.
        "ERROR",
        "WARN",
        "Fatal",
        "preimage",
        "lifecycle barrier",
        "panicked",
    ];
    let mut kept: Vec<&str> = text
        .lines()
        .filter(|line| keys.iter().any(|k| line.contains(k)))
        .collect();
    // Repeated identical debug lines add nothing; keep first + last occurrences.
    if kept.len() > 40 {
        let tail = kept.split_off(kept.len() - 20);
        kept.truncate(20);
        kept.push("    ... (middle elided) ...");
        kept.extend(tail);
    }
    if kept.is_empty() {
        format!(
            "<node.log had {} lines, none consensus-relevant>",
            text.lines().count()
        )
    } else {
        kept.join("\n")
    }
}

#[derive(Debug, Default)]
struct StageReport {
    baseline_height: u64,
    admitted: Option<u32>,
    height_after_submit: u64,
    receipt_seen: Option<String>,
    finalized: bool,
    /// A health probe that fails is not a height of zero. Defaulting the two to
    /// the same value is what made an earlier run of this test report "the
    /// chain never committed a block" about a node that had produced one and
    /// then shut itself down.
    health_probe_failures: u32,
    node_alive_at_end: bool,
    /// Settlement evidence read back from the receipt, so a "Finalized" label
    /// is not accepted on its own.
    settlement_credits: Vec<(String, u64)>,
    reserved_max_payment: Option<u64>,
    execution_price: Option<u64>,
    terminal_block_height: Option<u64>,
    output_hash: Option<String>,
    certificate_votes: Option<u64>,
}

#[test]
fn native_request_lifecycle_stages_against_a_real_node_process() {
    // The previous version of this test waited for a committed block BEFORE
    // submitting anything, so a failure there never reached the paid workload
    // and could not say whether the chain needs WORK in order to produce
    // blocks. That is the causality this measures rather than assumes: submit
    // valid work as soon as the node is ready, then watch admission, inclusion,
    // execution and finalization as separate stages.
    let tag = "native-lifecycle-a";
    let fx = Fixture::new(tag);
    let data = fx.dir.path().join("node");
    let mut node = fx.spawn(tag, 9961, 9161, &data);
    let mut report = StageReport::default();

    wait_for(Duration::from_secs(90), "node health", || {
        get_json(node.port, "/health").is_some()
    });
    let ctx = get_json(node.port, "/native-inference/context")
        .expect("the contract must be activated by the startup flag");
    assert_eq!(ctx["candidate_protocol"], 4);
    let domain = InferenceDomain {
        chain_genesis: Hash256::from_hex(ctx["chain_genesis"].as_str().unwrap()).unwrap(),
        recovery_epoch: ctx["recovery_epoch"].as_u64().unwrap(),
        validator_set_hash: Hash256::from_hex(ctx["validator_set_hash"].as_str().unwrap()).unwrap(),
    };

    // Some(h) = the node answered; None = it did not answer at all.
    let height =
        |port: u16| -> Option<u64> { get_json(port, "/health").and_then(|h| h["height"].as_u64()) };
    report.baseline_height = height(node.port).expect("node must answer /health at baseline");

    // STAGE 1 - admission. Submit immediately after readiness; do not wait for
    // a block first.
    let (tx, request_id) = fx.signed_request(domain, 0, u64::MAX / 2, fx.allowed);
    let (code, body) = submit(node.port, &tx);
    report.admitted = Some(code);
    assert_eq!(code, 200, "signed native request must be admitted: {body}");

    // STAGE 2 - inclusion. Does submitting work make the chain commit?
    let mut included = false;
    for _ in 0..30 {
        match height(node.port) {
            Some(h) => {
                report.height_after_submit = h;
                if h > report.baseline_height {
                    included = true;
                    break;
                }
            }
            None => report.health_probe_failures += 1,
        }
        std::thread::sleep(Duration::from_secs(2));
    }
    if let Some(h) = height(node.port) {
        report.height_after_submit = h;
    }
    report.node_alive_at_end = height(node.port).is_some();
    assert!(
        report.health_probe_failures == 0,
        "the node stopped answering /health {} times while waiting for inclusion. It did not \
         merely fail to commit - it went away. Treating that as height 0 is what hid a \
         consensus-loop shutdown. Observed: {report:#?}\nNode's own trace:\n{}",
        report.health_probe_failures,
        consensus_trace(&data)
    );

    // STAGE 3/4 - execution and finalization, observed through the receipt.
    // The receipt reports `observed_status` (Pending | Finalized | Refunded).
    // It deliberately does NOT assert consensus finality - it carries
    // "consensus_finality": "not asserted by milestone-2 receipt" - so this
    // stage is terminal SETTLEMENT, not finality.
    let receipt_path = format!("/native-inference/receipt/{}", request_id.to_hex());
    for _ in 0..40 {
        if let Some(r) = get_json(node.port, &receipt_path) {
            let status = r["observed_status"].as_str().unwrap_or("").to_string();
            report.receipt_seen = Some(status.clone());
            report.reserved_max_payment = r["reserved_max_payment"].as_u64();
            report.execution_price = r["execution_price"].as_u64();
            report.output_hash = r["output_hash"].as_str().map(str::to_string);
            report.certificate_votes = r["certificate_votes"].as_u64();
            report.terminal_block_height = r["terminal_transaction"]["block_height"].as_u64();
            report.settlement_credits = r["settlement_credits"]
                .as_array()
                .map(|credits| {
                    credits
                        .iter()
                        .map(|credit| {
                            (
                                credit["payee"].as_str().unwrap_or("").to_string(),
                                credit["amount"].as_u64().unwrap_or(0),
                            )
                        })
                        .collect()
                })
                .unwrap_or_default();
            if status == "Finalized" {
                report.finalized = true;
                break;
            }
        }
        std::thread::sleep(Duration::from_secs(2));
    }

    node.kill();

    // Keep the whole log outside the TempDir, which is deleted on drop. An
    // elided trace was enough to disprove the previous conclusion but not
    // enough to explain it.
    let retained = std::env::temp_dir().join("arc-native-lifecycle-node.log");
    let _ = std::fs::copy(data.join("node.log"), &retained);
    eprintln!("full node log retained at {}", retained.display());
    let trace = consensus_trace(&data);
    eprintln!("stage report: {report:#?}");
    eprintln!("--- node consensus trace ---\n{trace}\n--- end trace ---");
    assert_eq!(report.admitted, Some(200), "admission");
    assert!(
        included,
        "INCLUSION did not happen: the request was admitted (HTTP 200) but the chain never \
         committed a block. Height stayed at {} after submitting valid work, so idle blocks are \
         not the missing ingredient - this single-validator configuration does not commit even \
         WITH work pending. Execution and finalization were never reachable. \
         Observed: {report:#?}\nNode's own consensus trace:\n{trace}",
        report.baseline_height
    );
    assert!(
        report.finalized,
        "the request was admitted and the chain advanced, but no Finalized receipt appeared. \
         Last observed receipt status: {:?}. Observed: {report:#?}",
        report.receipt_seen
    );

    // STAGE 4 evidence. A status string alone would pass while paying nobody.
    assert!(
        report.terminal_block_height.is_some(),
        "a Finalized receipt must name the block its terminal transaction landed in: {report:#?}"
    );
    assert!(
        report.output_hash.is_some(),
        "a Finalized receipt must carry the committed output hash: {report:#?}"
    );
    assert!(
        report.certificate_votes.unwrap_or(0) >= 1,
        "a Finalized receipt must carry at least one certificate vote: {report:#?}"
    );
    assert!(
        !report.settlement_credits.is_empty(),
        "settlement produced no credits, so nothing was actually paid: {report:#?}"
    );
    let paid: u64 = report
        .settlement_credits
        .iter()
        .map(|(_, amount)| amount)
        .sum();
    let reserved = report
        .reserved_max_payment
        .expect("the receipt must report what the requester reserved");
    let price = report
        .execution_price
        .expect("the receipt must report the execution price");
    // Conservation, checked from the receipt rather than trusted: a finalized
    // settlement drains the escrow exactly, paying the price and refunding the
    // unused reserve. inference_contract_state.rs rejects any other total, so
    // an inequality here means the receipt and the ledger disagree.
    assert_eq!(
        paid, reserved,
        "a finalized settlement must drain the escrow exactly: {report:#?}"
    );
    assert!(
        paid > 0,
        "settlement credited zero, which is not a paid flow: {report:#?}"
    );
    let amounts: Vec<u64> = report.settlement_credits.iter().map(|(_, a)| *a).collect();
    assert!(
        amounts.contains(&price),
        "no credit equals the execution price {price}: {report:#?}"
    );
    assert!(
        amounts.contains(&(reserved - price)),
        "no credit equals the refunded remainder {}: {report:#?}",
        reserved - price
    );
    let payees: std::collections::HashSet<&String> =
        report.settlement_credits.iter().map(|(p, _)| p).collect();
    assert_eq!(
        payees.len(),
        report.settlement_credits.len(),
        "a payee appears twice in one settlement: {report:#?}"
    );
    eprintln!(
        "settlement: {paid} credited across {} payee(s), reserved max {reserved}, price {:?}",
        report.settlement_credits.len(),
        report.execution_price
    );
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

/// One request's settlement facts, as the node reports them.
#[derive(Debug, Clone, PartialEq, Eq)]
struct Settlement {
    status: String,
    credits: Vec<(String, u64)>,
    terminal_tx: Option<String>,
    terminal_height: Option<u64>,
    output_hash: Option<String>,
}

fn settlement(port: u16, request_id: Hash256) -> Option<Settlement> {
    let r = get_json(
        port,
        &format!("/native-inference/receipt/{}", request_id.to_hex()),
    )?;
    let mut credits: Vec<(String, u64)> = r["settlement_credits"]
        .as_array()?
        .iter()
        .map(|c| {
            (
                c["payee"].as_str().unwrap_or("").to_string(),
                c["amount"].as_u64().unwrap_or(0),
            )
        })
        .collect();
    // Order is not part of the claim; the multiset is.
    credits.sort();
    Some(Settlement {
        status: r["observed_status"].as_str().unwrap_or("").to_string(),
        credits,
        terminal_tx: r["terminal_transaction"]["tx_hash"]
            .as_str()
            .map(str::to_string),
        terminal_height: r["terminal_transaction"]["block_height"].as_u64(),
        output_hash: r["output_hash"].as_str().map(str::to_string),
    })
}

fn domain_of(port: u16) -> InferenceDomain {
    let ctx = get_json(port, "/native-inference/context").expect("contract must be activated");
    assert_eq!(ctx["candidate_protocol"], 4);
    InferenceDomain {
        chain_genesis: Hash256::from_hex(ctx["chain_genesis"].as_str().unwrap()).unwrap(),
        recovery_epoch: ctx["recovery_epoch"].as_u64().unwrap(),
        validator_set_hash: Hash256::from_hex(ctx["validator_set_hash"].as_str().unwrap()).unwrap(),
    }
}

/// Round-3 item 4: the paid flow across concurrent, INDEPENDENT stores.
///
/// Two node processes, two data directories, two genesis documents, two
/// activation documents, two requester identities, running at the same time.
/// They are not peers and share nothing, which is the point: a receipt, a
/// settlement and a replay refusal must each belong to exactly one store.
///
/// Covers, in one run: payment accounting, receipt identity, exactly-once
/// settlement, replay refusal, and restart persistence. Deterministic test
/// executor only - this validates the protocol, never model quality.
#[test]
fn paid_flow_across_concurrent_independent_stores() {
    let fx_a = Fixture::new("store-a");
    let fx_b = Fixture::new("store-b");
    let data_a = fx_a.dir.path().join("node");
    let data_b = fx_b.dir.path().join("node");

    // Started back to back so both chains are live at the same time.
    let mut node_a = fx_a.spawn("store-a", 9963, 9163, &data_a);
    let mut node_b = fx_b.spawn("store-b", 9964, 9164, &data_b);
    wait_for(Duration::from_secs(90), "store A health", || {
        get_json(node_a.port, "/health").is_some()
    });
    wait_for(Duration::from_secs(90), "store B health", || {
        get_json(node_b.port, "/health").is_some()
    });

    let (tx_a, id_a) = fx_a.signed_request(domain_of(node_a.port), 0, u64::MAX / 2, fx_a.allowed);
    let (tx_b, id_b) = fx_b.signed_request(domain_of(node_b.port), 0, u64::MAX / 2, fx_b.allowed);
    assert_ne!(
        id_a, id_b,
        "the two stores must not produce the same request id"
    );

    assert_eq!(
        submit(node_a.port, &tx_a).0,
        200,
        "store A must admit its own request"
    );
    assert_eq!(
        submit(node_b.port, &tx_b).0,
        200,
        "store B must admit its own request"
    );

    let finalized = |port: u16, id: Hash256| -> Settlement {
        let mut last = None;
        for _ in 0..60 {
            if let Some(s) = settlement(port, id) {
                if s.status == "Finalized" {
                    return s;
                }
                last = Some(s);
            }
            std::thread::sleep(Duration::from_millis(500));
        }
        panic!(
            "no Finalized receipt on port {port} for {}: {last:?}",
            id.to_hex()
        );
    };
    let settled_a = finalized(node_a.port, id_a);
    let settled_b = finalized(node_b.port, id_b);

    // ── payment accounting ───────────────────────────────────────────────────
    for (label, s) in [("A", &settled_a), ("B", &settled_b)] {
        let total: u64 = s.credits.iter().map(|(_, amount)| amount).sum();
        assert!(total > 0, "store {label} settled nothing: {s:?}");
        assert!(
            s.terminal_tx.is_some(),
            "store {label} has no terminal transaction: {s:?}"
        );
        assert!(
            s.output_hash.is_some(),
            "store {label} has no output hash: {s:?}"
        );
        let payees: std::collections::HashSet<_> = s.credits.iter().map(|(p, _)| p).collect();
        assert_eq!(
            payees.len(),
            s.credits.len(),
            "store {label} credits a payee twice: {s:?}"
        );
    }

    // ── receipt identity ─────────────────────────────────────────────────────
    // Each store must know its own request and not the other's. A store that
    // answered for a request it never admitted would make every other
    // assertion here meaningless.
    assert!(
        settlement(node_a.port, id_b).is_none(),
        "store A returned a receipt for store B's request id"
    );
    assert!(
        settlement(node_b.port, id_a).is_none(),
        "store B returned a receipt for store A's request id"
    );
    assert_ne!(
        settled_a.terminal_tx, settled_b.terminal_tx,
        "two independent stores produced the same terminal transaction"
    );

    // ── replay refusal + exactly-once settlement ─────────────────────────────
    let (replay_code, replay_body) = submit(node_a.port, &tx_a);
    assert_ne!(
        replay_code, 200,
        "replaying an identical signed request must be refused, got 200: {replay_body}"
    );
    // Cross-store replay: A's signed request is bound to A's domain and must
    // not be admissible against B.
    let (cross_code, cross_body) = submit(node_b.port, &tx_a);
    assert_ne!(
        cross_code, 200,
        "store B admitted a request signed against store A's domain: {cross_body}"
    );
    std::thread::sleep(Duration::from_secs(3));
    assert_eq!(
        settlement(node_a.port, id_a).expect("store A receipt must persist"),
        settled_a,
        "settlement changed after a refused replay, so it is not exactly-once"
    );
    assert!(
        settlement(node_b.port, id_a).is_none(),
        "store B now holds a receipt for store A's request after a cross-store replay"
    );

    // ── restart persistence ──────────────────────────────────────────────────
    // Store A restarts onto the same data directory. Store B keeps running, so
    // this also shows the two stores are genuinely independent.
    node_a.kill();
    std::thread::sleep(Duration::from_secs(1));
    let mut node_a = fx_a.spawn("store-a", 9963, 9163, &data_a);
    wait_for(
        Duration::from_secs(90),
        "store A health after restart",
        || get_json(node_a.port, "/health").is_some(),
    );
    let after_restart =
        settlement(node_a.port, id_a).expect("store A must still hold its receipt after a restart");
    assert_eq!(
        after_restart, settled_a,
        "the receipt, its settlement credits and its terminal transaction must survive a restart \
         byte for byte"
    );
    assert_eq!(
        settlement(node_b.port, id_b).expect("store B receipt must survive A's restart"),
        settled_b,
        "restarting store A disturbed store B"
    );

    // A replay after the restart must still be refused: restart recovery must
    // not reopen a settled request.
    let (post_restart_replay, body) = submit(node_a.port, &tx_a);
    assert_ne!(
        post_restart_replay, 200,
        "a settled request became replayable after a restart: {body}"
    );

    eprintln!("store A settlement: {settled_a:?}");
    eprintln!("store B settlement: {settled_b:?}");
    node_a.kill();
    node_b.kill();
}

/// A fresh chain-run identity for every execution of this test. The genesis
/// is otherwise byte-identical run to run, so without it the certificates of
/// one run would verify against the next one's committee.
fn chain_instance_id() -> String {
    format!(
        "{}-{}",
        std::process::id(),
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_nanos()
    )
}
