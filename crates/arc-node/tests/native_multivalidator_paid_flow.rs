//! P5: the paid native-inference flow on ONE chain with FOUR validators.
//!
//! Every earlier paid-flow test ran a single validator, which uses a different
//! block-production path from a committee. Reading the committee path for the
//! soak's workload found two defects this test now pins:
//!
//! * nothing in the consensus loop knew about protocol 4: a DAG block could
//!   carry two native requests, or an ordinary transfer, and block execution
//!   refused it as a fatal error - on every node, at the same block;
//! * each validator's vote stayed inside its own process, while a finalize
//!   transaction needs a strict supermajority of stake - so on any committee
//!   larger than one, no request could ever finalize.
//!
//! Four real `arc-node` processes, driven over HTTP, with the DETERMINISTIC
//! TEST EXECUTOR: protocol integration coverage that loads no model and
//! qualifies nothing about inference quality.
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

const NODES: usize = 4;
const STAKE: u64 = 6_666_667;

fn validator_keypair(seed: &str) -> KeyPair {
    let bytes = blake3::derive_key("ARC-chain-validator-keypair-v1", seed.as_bytes());
    KeyPair::Ed25519(ed25519_dalek::SigningKey::from_bytes(&bytes))
}

fn requester_keypair(seed: &str) -> KeyPair {
    let bytes = blake3::derive_key("ARC-native-multivalidator-test-requester-v1", seed.as_bytes());
    KeyPair::Ed25519(ed25519_dalek::SigningKey::from_bytes(&bytes))
}

fn curl(args: &[&str]) -> (u32, String) {
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
    let (code, body) = curl(&[&format!("http://127.0.0.1:{port}{path}")]);
    (code == 200).then(|| serde_json::from_str(&body).ok()).flatten()
}

fn submit(port: u16, tx: &Transaction) -> (u32, String) {
    let payload = serde_json::to_string(tx).unwrap();
    curl(&[
        "-X",
        "POST",
        "-H",
        "Content-Type: application/json",
        "-d",
        &payload,
        &format!("http://127.0.0.1:{port}/tx/submit_signed"),
    ])
}

fn wait_for<F: FnMut() -> bool>(timeout: Duration, label: &str, mut f: F) {
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

impl Drop for NodeProcess {
    fn drop(&mut self) {
        let _ = self.child.kill();
        let _ = self.child.wait();
        // Keep each node's log for diagnosis; the temp dir is deleted.
        let keep = std::env::temp_dir().join(format!("arc-p5-node-{}.log", self.port));
        let _ = std::fs::copy(self.data_dir.join("node.log"), keep);
    }
}

struct Fixture {
    base_rpc: u16,
    base_p2p: u16,
    /// Extra arguments every node is started with.
    node_args: Vec<String>,
    dir: tempfile::TempDir,
    genesis: PathBuf,
    activation: PathBuf,
    /// Two requesters: ingress admits only the nonce valid at the next
    /// height, so two native requests can share a DAG block only when they
    /// come from different accounts - which is the case that used to halt.
    requesters: [KeyPair; 2],
    tuple: [Hash256; 4],
}

impl Fixture {
    /// Tests in this file run concurrently, so each uses its own ports.
    fn new(base_rpc: u16, base_p2p: u16, node_args: &[&str]) -> Self {
        let dir = tempfile::tempdir().unwrap();
        let requesters = [requester_keypair("p5"), requester_keypair("p5-second")];
        let tuple = [
            hash_bytes(b"p5-model"),
            hash_bytes(b"p5-profile"),
            hash_bytes(b"p5-generation"),
            hash_bytes(b"p5-assignment"),
        ];
        let mut genesis = String::from(
            "[chain]\nname = \"arc-p5-paid-flow\"\nchain_id = \"0x415243\"\n\
             validator_set_complete = false\n\n",
        );
        for requester in &requesters {
            genesis.push_str(&format!(
                "[[accounts]]\naddress = \"{}\"\nbalance = 1_000_000_000_000\n\n",
                requester.address().to_hex()
            ));
        }
        for i in 0..NODES {
            let v = validator_keypair(&format!("p5-validator-{i}")).address().to_hex();
            // Validators need canonical accounts: any of them may submit the
            // finalize transaction once it holds a supermajority certificate.
            genesis.push_str(&format!(
                "[[accounts]]\naddress = \"{v}\"\nbalance = 1_000_000_000_000\n\n"
            ));
        }
        for i in 0..NODES {
            let v = validator_keypair(&format!("p5-validator-{i}")).address().to_hex();
            genesis.push_str(&format!("[[validators]]\naddress = \"{v}\"\nstake = {STAKE}\n\n"));
        }
        let genesis_path = dir.path().join("genesis.toml");
        std::fs::write(&genesis_path, genesis).unwrap();
        let activation = dir.path().join("activation.json");
        std::fs::write(
            &activation,
            serde_json::json!({
                "recovery_epoch": 0,
                "allowed_executions": [{
                    "model_hash": tuple[0].to_hex(),
                    "profile_hash": tuple[1].to_hex(),
                    "generation_hash": tuple[2].to_hex(),
                    "assignment_hash": tuple[3].to_hex(),
                }]
            })
            .to_string(),
        )
        .unwrap();
        Self {
            base_rpc,
            base_p2p,
            node_args: node_args.iter().map(|a| a.to_string()).collect(),
            dir,
            genesis: genesis_path,
            activation,
            requesters,
            tuple,
        }
    }

    /// Start node `index`, or restart it on the data directory it had.
    fn spawn(&self, index: usize) -> NodeProcess {
        let port = self.base_rpc + index as u16;
        let data_dir = self.dir.path().join(format!("node-{index}"));
        std::fs::create_dir_all(&data_dir).unwrap();
        let peers: Vec<String> = (0..NODES)
            .filter(|j| *j != index)
            .map(|j| format!("127.0.0.1:{}", self.base_p2p + j as u16))
            .collect();
        // Appended, so a restarted node's log follows its first life's.
        let log = std::fs::OpenOptions::new()
            .create(true)
            .append(true)
            .open(data_dir.join("node.log"))
            .unwrap();
        let child = Command::new(env!("CARGO_BIN_EXE_arc-node"))
            .args([
                "--rpc",
                &format!("127.0.0.1:{port}"),
                "--p2p-port",
                &(self.base_p2p + index as u16).to_string(),
                "--data-dir",
                data_dir.to_str().unwrap(),
                "--genesis",
                self.genesis.to_str().unwrap(),
                "--peers",
                &peers.join(","),
                "--insecure-dev-validator-seed",
                "--validator-seed",
                &format!("p5-validator-{index}"),
                "--stake",
                &STAKE.to_string(),
                "--native-inference-activation",
                self.activation.to_str().unwrap(),
                "--native-inference-runtime",
                "--native-inference-test-executor",
            ])
            .args(&self.node_args)
            // ARC_TEST_NODE_LOG raises the nodes' log level when diagnosing.
            .env(
                "RUST_LOG",
                std::env::var("ARC_TEST_NODE_LOG").unwrap_or_else(|_| "info".into()),
            )
            .stdout(log.try_clone().unwrap())
            .stderr(log)
            .spawn()
            .expect("arc-node must start");
        NodeProcess {
            child,
            port,
            data_dir,
        }
    }

    fn request(
        &self,
        who: usize,
        domain: InferenceDomain,
        nonce: u64,
        expires_at: u64,
    ) -> (Transaction, Hash256) {
        let requester = &self.requesters[who];
        let input = format!("p5 input {who}/{nonce}").into_bytes();
        let job = InferenceJob {
            version: INFERENCE_CONTRACT_VERSION,
            domain,
            requester: requester.address(),
            nonce,
            model_hash: self.tuple[0],
            profile_hash: self.tuple[1],
            input_hash: hash_bytes(&input),
            generation_hash: self.tuple[2],
            assignment_hash: self.tuple[3],
            max_tokens: 8,
            max_output_bytes: 128,
            execution_price: 10,
            reserved_max_payment: 100,
            expires_at,
        };
        let request_id = job.request_id();
        let request = InferenceRequest::sign(job, requester).unwrap();
        let body = TxBody::NativeInferenceRequest(NativeInferenceRequestBody {
            request,
            input_blob: input,
        });
        let mut tx = Transaction {
            tx_type: body.tx_type(),
            from: requester.address(),
            nonce,
            body,
            fee: 0,
            gas_limit: 5_000_000,
            hash: Hash256::ZERO,
            signature: arc_crypto::signature::Signature::null(),
            sig_verified: false,
        };
        tx.hash = tx.compute_hash();
        tx.signature = requester.sign(&tx.hash).unwrap();
        (tx, request_id)
    }
}

/// The receipt fields that must be identical on every replica.
fn settlement(port: u16, request_id: Hash256) -> Option<(String, u64, String, Vec<(String, u64)>)> {
    let r = get_json(port, &format!("/native-inference/receipt/{}", request_id.to_hex()))?;
    let status = r["observed_status"].as_str()?.to_string();
    let height = r["terminal_transaction"]["block_height"].as_u64().unwrap_or(0);
    let output = r["output_hash"].as_str().unwrap_or("").to_string();
    let mut credits: Vec<(String, u64)> = r["settlement_credits"]
        .as_array()
        .map(|c| {
            c.iter()
                .map(|x| {
                    (
                        x["payee"].as_str().unwrap_or("").to_string(),
                        x["amount"].as_u64().unwrap_or(0),
                    )
                })
                .collect()
        })
        .unwrap_or_default();
    credits.sort();
    Some((status, height, output, credits))
}

fn heights(nodes: &[NodeProcess]) -> Vec<Option<u64>> {
    nodes
        .iter()
        .map(|n| get_json(n.port, "/health").and_then(|h| h["height"].as_u64()))
        .collect()
}

#[test]
fn a_paid_request_settles_identically_on_every_validator_of_one_chain() {
    let fx = Fixture::new(9940, 9140, &[]);
    let nodes: Vec<NodeProcess> = (0..NODES).map(|i| fx.spawn(i)).collect();

    // All four start at the same instant. Before the transport served inbound
    // handshakes while its own bootstrap dials were in flight (D1), this took
    // at least 45 s of dial timeouts plus the 30 s reconnect timer; 40 s is a
    // regression bound with slack for a loaded debug build.
    wait_for(Duration::from_secs(40), "every node healthy and fully peered", || {
        nodes.iter().all(|n| {
            get_json(n.port, "/health")
                .and_then(|h| h["peers"].as_u64())
                .is_some_and(|p| p >= (NODES - 1) as u64)
        })
    });
    wait_for(Duration::from_secs(120), "the committee to commit blocks", || {
        heights(&nodes).iter().all(|h| h.unwrap_or(0) >= 3)
    });

    let ctx = get_json(nodes[0].port, "/native-inference/context").expect("activated");
    assert_eq!(ctx["candidate_protocol"], 4);
    let domain = InferenceDomain {
        chain_genesis: Hash256::from_hex(ctx["chain_genesis"].as_str().unwrap()).unwrap(),
        recovery_epoch: ctx["recovery_epoch"].as_u64().unwrap(),
        validator_set_hash: Hash256::from_hex(ctx["validator_set_hash"].as_str().unwrap()).unwrap(),
    };
    // Every replica must be bound to the SAME committee and chain.
    for n in &nodes[1..] {
        let other = get_json(n.port, "/native-inference/context").expect("activated");
        assert_eq!(other["validator_set_hash"], ctx["validator_set_hash"]);
        assert_eq!(other["chain_genesis"], ctx["chain_genesis"]);
    }
    let expiry = heights(&nodes)[0].unwrap() + 5_000;

    // ── one request, submitted to one node, settled everywhere ─────────────
    let (tx, first) = fx.request(0, domain, 0, expiry);
    let (code, body) = submit(nodes[0].port, &tx);
    assert_eq!(code, 200, "a signed native request must be admitted: {body}");

    let mut settled = Vec::new();
    let finalized = {
        let start = Instant::now();
        let mut ok = false;
        while start.elapsed() < Duration::from_secs(180) {
            settled = nodes.iter().map(|n| settlement(n.port, first)).collect();
            if settled
                .iter()
                .all(|s| s.as_ref().is_some_and(|(status, ..)| status == "Finalized"))
            {
                ok = true;
                break;
            }
            std::thread::sleep(Duration::from_millis(500));
        }
        ok
    };
    if !finalized {
        // Say WHY before failing: which stage of the vote exchange stopped.
        for (i, n) in nodes.iter().enumerate() {
            let d = get_json(n.port, "/consensus/diagnostics").unwrap_or_default();
            eprintln!(
                "node {i}: receipt={:?} votes gossiped={} accepted={} refused={} \
                 p4_omitted={} height={}",
                settled[i],
                d["native_votes_gossiped"],
                d["native_votes_accepted"],
                d["native_votes_refused"],
                d["protocol4_omitted_transactions"],
                d["height"]
            );
        }
        panic!("the request was not Finalized on every replica within 180 s");
    }
    let reference = settled[0].clone().unwrap();
    for (i, s) in settled.iter().enumerate() {
        assert_eq!(
            s.as_ref().unwrap(),
            &reference,
            "replica {i} disagrees about the settlement of the same request"
        );
    }
    let (_, terminal_height, output_hash, credits) = reference;
    assert!(terminal_height > 0 && !output_hash.is_empty());
    let paid: u64 = credits.iter().map(|(_, a)| a).sum();
    assert_eq!(paid, 100, "settlement must drain the reserved escrow exactly: {credits:?}");
    // The reserve (100) minus the execution price (10) goes back to the
    // requester, once.
    let requester_hex = fx.requesters[0].address().to_hex();
    let refunds: Vec<u64> = credits
        .iter()
        .filter(|(payee, _)| *payee == requester_hex)
        .map(|(_, amount)| *amount)
        .collect();
    assert_eq!(refunds, vec![90], "the unused reserve is refunded exactly once: {credits:?}");
    // The price is split pro rata by stake across the signers of the matching
    // output (plan_finalize: floor shares, remainder units one each to the
    // first). Equal stakes here, so every share is floor or floor + 1.
    let committee: Vec<String> = (0..NODES)
        .map(|i| validator_keypair(&format!("p5-validator-{i}")).address().to_hex())
        .collect();
    let shares: Vec<u64> = credits
        .iter()
        .filter(|(payee, _)| *payee != requester_hex)
        .map(|(payee, amount)| {
            assert!(committee.contains(payee), "{payee} is paid but is not a committee member");
            *amount
        })
        .collect();
    assert!(
        shares.len() * 3 > NODES * 2,
        "only a strict supermajority certificate can finalize: {} signers paid",
        shares.len()
    );
    assert_eq!(shares.iter().sum::<u64>(), 10, "exactly the price reaches the signers: {credits:?}");
    let floor = 10 / shares.len() as u64;
    assert!(
        shares.iter().all(|share| *share == floor || *share == floor + 1),
        "equal stakes split the price evenly: {credits:?}"
    );

    // ── ingress admits exactly the nonce valid at the next height ──────────
    // A future nonce is refused rather than parked: the preflight is the
    // executor's own admission check, and a native request is not admissible
    // until its predecessor has been included.
    let (future, _) = fx.request(0, domain, 2, expiry);
    assert_eq!(submit(nodes[1].port, &future).0, 400, "a future nonce is not admissible yet");

    // ── two requesters' requests into one node back-to-back ────────────────
    // Before the fix both could land in one DAG block, which block execution
    // refuses as fatal - on every node at once. Selection at commit now
    // executes one and carries the other to a later block.
    let (tx1, second) = fx.request(0, domain, 1, expiry);
    let (tx2, third) = fx.request(1, domain, 0, expiry);
    for tx in [&tx1, &tx2] {
        let (code, body) = submit(nodes[1].port, tx);
        assert_eq!(code, 200, "back-to-back request from {} refused: {body}", tx.from);
    }
    wait_for(Duration::from_secs(240), "both back-to-back requests to be Finalized everywhere", || {
        [second, third].iter().all(|id| {
            nodes.iter().all(|n| {
                settlement(n.port, *id).is_some_and(|(status, ..)| status == "Finalized")
            })
        })
    });
    let before = heights(&nodes);
    std::thread::sleep(Duration::from_secs(6));
    let after = heights(&nodes);
    for (i, (b, a)) in before.iter().zip(&after).enumerate() {
        assert!(
            a.unwrap_or(0) > b.unwrap_or(0),
            "node {i} stopped advancing after settling two requests ({b:?} -> {a:?})"
        );
    }

    // ── a non-native transaction is refused at ingress ─────────────────────
    let mut transfer = Transaction::new_transfer(
        fx.requesters[0].address(),
        hash_bytes(b"somebody"),
        1,
        3,
    );
    transfer.sign(&fx.requesters[0]).unwrap();
    let (code, _) = submit(nodes[2].port, &transfer);
    assert_eq!(
        code, 503,
        "a protocol-4 chain must refuse a transaction its blocks can never carry"
    );
    let (code, _) = curl(&[
        "-X",
        "POST",
        "-H",
        "Content-Type: application/json",
        "-d",
        &format!("{{\"address\":\"{}\"}}", hash_bytes(b"faucet-target").to_hex()),
        &format!("http://127.0.0.1:{}/faucet/claim", nodes[2].port),
    ]);
    assert_eq!(code, 503, "the faucet cannot be offered on a protocol-4 chain");

    // Nobody's consensus loop exited along the way.
    for (i, n) in nodes.iter().enumerate() {
        assert!(get_json(n.port, "/health").is_some(), "node {i} stopped answering");
        let diag = get_json(n.port, "/consensus/diagnostics").unwrap();
        eprintln!(
            "node {i}: votes gossiped={} accepted={} refused={} protocol4_omitted={}",
            diag["native_votes_gossiped"], diag["native_votes_accepted"],
            diag["native_votes_refused"], diag["protocol4_omitted_transactions"]
        );
    }
    let _: &Path = fx.dir.path();
}

/// Wait until `id` is Finalized on every listed node.
fn wait_finalized(nodes: &[&NodeProcess], id: Hash256, timeout: Duration, label: &str) {
    wait_for(timeout, label, || {
        nodes.iter().all(|n| {
            settlement(n.port, id).is_some_and(|(status, ..)| status == "Finalized")
        })
    });
}

fn block_hash(port: u16, height: u64) -> Option<String> {
    get_json(port, &format!("/block/{height}"))
        .and_then(|b| b["hash"].as_str().map(str::to_string))
}

/// A validator killed after the chain ran past its DAG retention window must
/// rejoin by history from its own durable commit cursor.
///
/// It used to ask peers for history from round 0 - the only round whose
/// blocks validate on an empty DAG. Past the retention window peers had pruned
/// it and served their oldest retained rounds instead, all below the node's
/// cursor, which it refused as adding nothing, forever. A 20-minute native
/// run hit exactly that on its first fault (killed at round 4291, 4096
/// retained). Here the window is the 100-round floor, so the chain passes it
/// in well under a minute.
#[test]
fn a_validator_restarted_past_the_retention_window_rejoins_and_settles_new_work() {
    let fx = Fixture::new(
        9950,
        9150,
        &["--dag-retained-rounds", "100", "--snapshot-every-blocks", "50"],
    );
    let mut nodes: Vec<NodeProcess> = (0..NODES).map(|i| fx.spawn(i)).collect();
    wait_for(Duration::from_secs(40), "every node healthy and fully peered", || {
        nodes.iter().all(|n| {
            get_json(n.port, "/health")
                .and_then(|h| h["peers"].as_u64())
                .is_some_and(|p| p >= (NODES - 1) as u64)
        })
    });
    let ctx = get_json(nodes[0].port, "/native-inference/context").expect("activated");
    let domain = InferenceDomain {
        chain_genesis: Hash256::from_hex(ctx["chain_genesis"].as_str().unwrap()).unwrap(),
        recovery_epoch: ctx["recovery_epoch"].as_u64().unwrap(),
        validator_set_hash: Hash256::from_hex(ctx["validator_set_hash"].as_str().unwrap()).unwrap(),
    };
    let expiry = 1_000_000;

    // Run well past the retention window, so round 0 is pruned everywhere.
    let round = |port: u16| {
        get_json(port, "/health")
            .and_then(|h| h["dag_round"].as_u64())
            .unwrap_or(0)
    };
    wait_for(Duration::from_secs(300), "the chain to pass 3x the retention window", || {
        round(nodes[0].port) >= 300
    });

    // Kill validator 3 (SIGKILL: a crash, not a shutdown).
    let victim = 3;
    {
        let process = &mut nodes[victim];
        process.child.kill().unwrap();
        process.child.wait().unwrap();
    }
    let killed_at = get_json(nodes[0].port, "/health").unwrap()["height"]
        .as_u64()
        .unwrap();

    // The other three keep working - exactly a quorum of stake. The request
    // is not awaited here: history can rejoin a node only while its peers
    // still hold its commit cursor round, so the downtime must stay well
    // inside the (deliberately tiny) retention window. Longer downtime needs
    // a checkpoint rebase, which is a different mechanism.
    let (tx, while_down) = fx.request(1, domain, 0, expiry);
    assert_eq!(submit(nodes[0].port, &tx).0, 200);
    std::thread::sleep(Duration::from_secs(5));
    let up: Vec<&NodeProcess> = nodes.iter().take(3).collect();
    wait_for(Duration::from_secs(5), "the three to keep committing", || {
        up.iter().all(|n| {
            get_json(n.port, "/health")
                .and_then(|h| h["height"].as_u64())
                .is_some_and(|h| h > killed_at)
        })
    });

    // Restart it on the same data directory.
    let old = std::mem::replace(&mut nodes[victim], fx.spawn(victim));
    std::mem::forget(old); // already reaped above; its Drop would kill nothing useful
    let victim_port = nodes[victim].port;

    // It must catch up past the height it was killed at and agree with its
    // peers block for block at a common height.
    wait_for(Duration::from_secs(180), "the restarted validator to catch up", || {
        let mine = get_json(victim_port, "/health").and_then(|h| h["height"].as_u64());
        let theirs = get_json(nodes[0].port, "/health").and_then(|h| h["height"].as_u64());
        matches!((mine, theirs), (Some(m), Some(t)) if m >= killed_at + 20 && m + 10 >= t)
    });
    let common = get_json(victim_port, "/health").unwrap()["height"].as_u64().unwrap() - 5;
    let reference = block_hash(nodes[0].port, common).expect("peer block");
    for n in &nodes {
        assert_eq!(
            block_hash(n.port, common).as_deref(),
            Some(reference.as_str()),
            "node on port {} disagrees at height {common}",
            n.port
        );
    }
    // The work submitted while it was down settles, identically, on it too.
    let all: Vec<&NodeProcess> = nodes.iter().collect();
    wait_finalized(&all, while_down, Duration::from_secs(120), "work from the downtime settles everywhere");
    assert_eq!(settlement(victim_port, while_down), settlement(nodes[0].port, while_down));

    // New work submitted THROUGH the restarted node settles everywhere.
    let (tx, after) = fx.request(1, domain, 1, expiry);
    wait_for(Duration::from_secs(60), "the restarted node to accept new work", || {
        submit(victim_port, &tx).0 == 200
    });
    wait_finalized(&all, after, Duration::from_secs(120), "new work through the restarted node");

    // And it got there by the new path, not by luck.
    let log = std::fs::read_to_string(nodes[victim].data_dir.join("node.log")).unwrap();
    assert!(
        log.contains("bootstrapping history from this node's durable commit cursor"),
        "the restarted node did not bootstrap from its commit cursor"
    );
    assert!(
        log.contains("DAG bootstrap complete"),
        "the restarted node never finished its bootstrap, so ingress stayed closed"
    );
}
