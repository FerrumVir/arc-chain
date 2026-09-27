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
//! The recovered-v3 regression additionally imports a signed checkpoint into
//! six processes and crosses activation/restart with default-closed admission.
//!
//! Real `arc-node` processes, driven over HTTP, with the DETERMINISTIC
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
    let bytes = blake3::derive_key(
        "ARC-native-multivalidator-test-requester-v1",
        seed.as_bytes(),
    );
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
    (code == 200)
        .then(|| serde_json::from_str(&body).ok())
        .flatten()
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
    log_already_preserved: bool,
    port: u16,
    data_dir: PathBuf,
}

impl Drop for NodeProcess {
    fn drop(&mut self) {
        let _ = self.child.kill();
        let _ = self.child.wait();
        // Keep each node's log for diagnosis; the temp dir is deleted.
        let keep = std::env::temp_dir().join(format!("arc-p5-node-{}.log", self.port));
        if !self.log_already_preserved {
            let _ = std::fs::copy(self.data_dir.join("node.log"), keep);
        }
    }
}

struct RecoveryFixture {
    checkpoint: PathBuf,
    manifest: Hash256,
    transaction_domain: Hash256,
    activation_height: u64,
}

struct Fixture {
    stakes: Vec<u64>,
    recovery: Option<RecoveryFixture>,
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
        Self::with_stakes(base_rpc, base_p2p, node_args, vec![STAKE; NODES])
    }

    fn with_stakes(base_rpc: u16, base_p2p: u16, node_args: &[&str], stakes: Vec<u64>) -> Self {
        let dir = tempfile::tempdir().unwrap();
        let requesters = [requester_keypair("p5"), requester_keypair("p5-second")];
        let tuple = [
            hash_bytes(b"p5-model"),
            hash_bytes(b"p5-profile"),
            hash_bytes(b"p5-generation"),
            hash_bytes(b"p5-assignment"),
        ];
        let mut genesis = format!(
            "[chain]\nname = \"arc-p5-paid-flow\"\nchain_id = \"0x415243\"\n\
             validator_set_complete = false\ninstance_id = \"{}\"\n\n",
            chain_instance_id()
        );
        for requester in &requesters {
            genesis.push_str(&format!(
                "[[accounts]]\naddress = \"{}\"\nbalance = 1_000_000_000_000\n\n",
                requester.address().to_hex()
            ));
        }
        for i in 0..stakes.len() {
            let v = validator_keypair(&format!("p5-validator-{i}"))
                .address()
                .to_hex();
            // Validators need canonical accounts: any of them may submit the
            // finalize transaction once it holds a supermajority certificate.
            genesis.push_str(&format!(
                "[[accounts]]\naddress = \"{v}\"\nbalance = 1_000_000_000_000\n\n"
            ));
        }
        for i in 0..stakes.len() {
            let v = validator_keypair(&format!("p5-validator-{i}"))
                .address()
                .to_hex();
            genesis.push_str(&format!(
                "[[validators]]\naddress = \"{v}\"\nstake = {}\n\n",
                stakes[i]
            ));
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
            stakes,
            recovery: None,
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

    /// Same process harness, with a complete six-member production-shaped
    /// recovery identity. All secrets and checkpoint signatures are generated
    /// for this disposable local test; no external model/files are consulted.
    fn recovered() -> Self {
        use arc_state::recovery::{ArcCheckpoint, RecoveryExportSpec, RecoveryValidator};
        use arc_types::inference_contract::{ValidatorMember, validator_set_commitment};
        use std::io::Write;
        let mut fx = Self::with_stakes(
            9980,
            9180,
            &["--snapshot-every-blocks", "50"],
            vec![
                6_666_667, 6_666_667, 6_666_667, 6_666_667, 6_666_666, 6_666_666,
            ],
        );
        let genesis_text = std::fs::read_to_string(&fx.genesis).unwrap().replace(
            "validator_set_complete = false",
            "validator_set_complete = true",
        );
        std::fs::write(&fx.genesis, genesis_text).unwrap();
        let genesis = arc_node::config::load_genesis(fx.genesis.to_str().unwrap()).unwrap();
        let genesis_hash = genesis.network_hash(false).unwrap();
        let keys: Vec<_> = (0..fx.stakes.len())
            .map(|i| validator_keypair(&format!("p5-validator-{i}")))
            .collect();
        for (i, key) in keys.iter().enumerate() {
            let KeyPair::Ed25519(secret) = key else {
                panic!("test needs Ed25519");
            };
            let mut file = arc_crypto::secret_file::create_new_private(
                &fx.dir.path().join(format!("validator-{i}.key.json")),
            )
            .unwrap();
            file.write_all(serde_json::json!({ "scheme": "ed25519", "secret_key": hex::encode(secret.to_bytes()),
                "public_key": hex::encode(key.public_key_bytes()), "address": key.address().to_hex() })
                .to_string().as_bytes()).unwrap();
            file.sync_all().unwrap();
        }
        let mut funds = genesis.validated_accounts().unwrap();
        funds.push((
            arc_state::recovery::recovery_stake_reserve_address(),
            40_000_000,
        ));
        let source = arc_state::StateDB::with_genesis(&funds);
        let joins: Vec<_> = keys
            .iter()
            .enumerate()
            .map(|(i, key)| {
                let mut tx = Transaction::new_transfer(key.address(), key.address(), 0, 0);
                tx.body = TxBody::JoinValidator(arc_types::transaction::JoinValidatorBody {
                    pubkey: key.public_key_bytes().try_into().unwrap(),
                    initial_stake: fx.stakes[i],
                });
                tx.tx_type = tx.body.tx_type();
                tx.sign(key).unwrap();
                tx
            })
            .collect();
        let (_, receipts) = source.execute_block(&joins, keys[0].address()).unwrap();
        assert!(receipts.iter().all(|r| r.success));
        for key in &keys {
            let mut account = source.get_account(&key.address()).unwrap();
            account.staked_balance = 0;
            source.update_account(&key.address(), account);
        }
        source.execute_block(&[], keys[0].address()).unwrap();
        let validators = keys
            .iter()
            .enumerate()
            .map(|(i, key)| RecoveryValidator {
                address: key.address(),
                public_key: key.public_key_bytes().try_into().unwrap(),
                stake: fx.stakes[i],
            })
            .collect();
        let mut checkpoint = ArcCheckpoint::export_unsigned(
            &source,
            RecoveryExportSpec {
                chain_id: genesis.chain.chain_id.clone(),
                genesis_hash,
                source_consensus_round: 64,
                recovery_epoch: 1,
                validator_set_id: 1,
                validators,
                community_rewards_v1_activation_height: None,
                created_at_unix_ms: 1,
            },
        )
        .unwrap();
        for key in keys.iter().take(5) {
            checkpoint.add_signature(key).unwrap();
        }
        let checkpoint_path = fx.dir.path().join("approved.arcchkpt");
        checkpoint.write_to(&checkpoint_path).unwrap();
        let mut members: Vec<_> = keys
            .iter()
            .enumerate()
            .map(|(i, key)| ValidatorMember::new(key.address(), fx.stakes[i]))
            .collect();
        members.sort_by_key(|m| m.address.0);
        let context = arc_state::InferenceAdmissionContext {
            domain: InferenceDomain {
                chain_genesis: genesis_hash,
                recovery_epoch: 1,
                validator_set_hash: validator_set_commitment(&members).unwrap(),
            },
            members,
            allowed_executions: vec![arc_state::AllowedExecution {
                model_hash: fx.tuple[0],
                profile_hash: fx.tuple[1],
                generation_hash: fx.tuple[2],
                assignment_hash: fx.tuple[3],
            }],
            selection_rule: arc_state::NativeSelectionRule::SkipUsedNoncesV2,
        };
        // Plenty of actual DAG rounds for the ordinary transfer before H.
        let activation_height = checkpoint.manifest.source_height + 1 + 256;
        std::fs::write(&fx.activation, serde_json::json!({
            "recovery_epoch": 1, "allowed_executions": context.allowed_executions,
            "selection_rule": context.selection_rule,
            "migration": arc_state::NativeMigrationRecord { chain_genesis: genesis_hash, recovery_epoch: 1,
                validator_set_id: 1, activation_height, context_commitment: context.commitment().unwrap() },
        }).to_string()).unwrap();
        fx.recovery = Some(RecoveryFixture {
            checkpoint: checkpoint_path,
            manifest: checkpoint.manifest_hash(),
            transaction_domain: checkpoint.manifest.recovery_context().domain_hash(),
            activation_height,
        });
        fx
    }

    /// Start node `index`, or restart it on the data directory it had.
    fn spawn(&self, index: usize) -> NodeProcess {
        self.spawn_mode(index, true, true)
    }

    fn spawn_mode(&self, index: usize, runtime: bool, admit_requests: bool) -> NodeProcess {
        let port = self.base_rpc + index as u16;
        let data_dir = self.dir.path().join(format!("node-{index}"));
        std::fs::create_dir_all(&data_dir).unwrap();
        let peers: Vec<String> = (0..self.stakes.len())
            .filter(|j| *j != index)
            .map(|j| format!("127.0.0.1:{}", self.base_p2p + j as u16))
            .collect();
        // Appended, so a restarted node's log follows its first life's.
        let log = std::fs::OpenOptions::new()
            .create(true)
            .append(true)
            .open(data_dir.join("node.log"))
            .unwrap();
        // Preserve recovery diagnostics even if CI's hard timeout kills the
        // test runner before Drop runs. The link survives TempDir cleanup.
        if self.recovery.is_some() && !data_dir.join("recovery.active").exists() {
            let keep = std::env::temp_dir().join(format!("arc-p5-node-{port}.log"));
            match std::fs::remove_file(&keep) {
                Ok(()) => {}
                Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
                Err(error) => panic!("cannot replace prior test log: {error}"),
            }
            std::fs::hard_link(data_dir.join("node.log"), keep).unwrap();
        }
        let mut command = Command::new(env!("CARGO_BIN_EXE_arc-node"));
        command.args([
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
            "--stake",
            &self.stakes[index].to_string(),
            "--native-inference-activation",
            self.activation.to_str().unwrap(),
        ]);
        if let Some(recovery) = &self.recovery {
            command
                .arg("--validator-key-file")
                .arg(self.dir.path().join(format!("validator-{index}.key.json")));
            if !data_dir.join("recovery.active").exists() {
                command
                    .arg("--recovery-checkpoint")
                    .arg(&recovery.checkpoint)
                    .arg("--approved-recovery-manifest-hash")
                    .arg(recovery.manifest.to_hex());
            }
        } else {
            command.args([
                "--insecure-dev-validator-seed",
                "--validator-seed",
                &format!("p5-validator-{index}"),
            ]);
        }
        if runtime {
            command.args([
                "--native-inference-runtime",
                "--native-inference-test-executor",
            ]);
        }
        if admit_requests {
            command.arg("--enable-native-inference-requests");
        }
        let child = command
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
            log_already_preserved: self.recovery.is_some(),
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
        self.request_using(who, domain, nonce, expires_at, self.tuple)
    }

    /// The same, against a stated execution tuple - so a test can submit work
    /// for a model the chain only allows after a binding update.
    fn request_using(
        &self,
        who: usize,
        domain: InferenceDomain,
        nonce: u64,
        expires_at: u64,
        tuple: [Hash256; 4],
    ) -> (Transaction, Hash256) {
        let requester = &self.requesters[who];
        let input = format!("p5 input {who}/{nonce}").into_bytes();
        let job = InferenceJob {
            version: INFERENCE_CONTRACT_VERSION,
            domain,
            requester: requester.address(),
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
        if let Some(recovery) = &self.recovery {
            tx.sign_in_domain(requester, &recovery.transaction_domain)
                .unwrap();
        } else {
            tx.sign(requester).unwrap();
        }
        (tx, request_id)
    }
}

/// The receipt fields that must be identical on every replica.
fn settlement(port: u16, request_id: Hash256) -> Option<(String, u64, String, Vec<(String, u64)>)> {
    let r = get_json(
        port,
        &format!("/native-inference/receipt/{}", request_id.to_hex()),
    )?;
    let status = r["observed_status"].as_str()?.to_string();
    let height = r["terminal_transaction"]["block_height"]
        .as_u64()
        .unwrap_or(0);
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

/// Reap every process through its normal durability barrier before restarting.
/// Send all TERM signals before waiting: a stopped validator may be the next
/// required leader, so serial stop-and-wait must not masquerade as progress.
#[cfg(unix)]
fn stop_recovered_nodes(nodes: &mut [NodeProcess]) {
    for node in nodes.iter() {
        assert!(
            Command::new("kill")
                .args(["-TERM", &node.child.id().to_string()])
                .status()
                .unwrap()
                .success()
        );
    }
    let deadline = Instant::now() + Duration::from_secs(30);
    for node in nodes.iter_mut() {
        loop {
            if let Some(status) = node.child.try_wait().unwrap() {
                assert!(
                    status.success(),
                    "validator {} failed normal shutdown: {status}",
                    node.port
                );
                let retained_log =
                    std::env::temp_dir().join(format!("arc-p5-node-{}.log", node.port));
                assert!(
                    std::fs::metadata(retained_log).unwrap().len() > 0,
                    "every recovered process must leave nonempty CI diagnostics"
                );
                break;
            }
            assert!(
                Instant::now() < deadline,
                "validator {} did not stop durably",
                node.port
            );
            std::thread::sleep(Duration::from_millis(50));
        }
    }
}

fn recovered_account(port: u16, who: Hash256) -> arc_types::Account {
    serde_json::from_value(
        get_json(port, &format!("/account/{}", who.to_hex())).expect("account RPC"),
    )
    .unwrap()
}

/// No StateDB execution in this helper: the only writer is the real node's
/// DAG loop. Every replica must publish the same successful canonical receipt
/// and v3 header, with an actual nonzero DAG decision commitment.
fn recovered_commit(nodes: &[NodeProcess], tx: Hash256, native: bool, phase: &str) -> u64 {
    let label = format!(
        "{phase}: transaction {} committed on all recovered validators",
        tx.to_hex()
    );
    eprintln!("Waiting for {label}");
    wait_for(Duration::from_secs(90), &label, || {
        nodes.iter().all(|n| {
            get_json(n.port, &format!("/tx/{}", tx.to_hex())).is_some_and(|r| r["success"] == true)
        })
    });
    let reference = get_json(nodes[0].port, &format!("/tx/{}", tx.to_hex())).unwrap();
    let height = reference["block_height"].as_u64().unwrap();
    let reference_block = get_json(nodes[0].port, &format!("/block/{height}")).unwrap();
    let block: arc_types::Block = serde_json::from_value(reference_block.clone()).unwrap();
    assert_eq!(block.header.protocol_version.major, 3);
    assert_ne!(
        block.header.proof_hash,
        Hash256::ZERO,
        "must commit through actual recovered DAG"
    );
    assert!(block.tx_hashes.contains(&tx));
    if native {
        assert_eq!(
            block.header.tx_count, 1,
            "native transition gets an isolated state block"
        );
    }
    for node in nodes {
        assert_eq!(
            get_json(node.port, &format!("/tx/{}", tx.to_hex())).unwrap(),
            reference
        );
        assert_eq!(
            get_json(node.port, &format!("/block/{height}")).unwrap(),
            reference_block,
            "recovered replicas disagree on header/root/body"
        );
    }
    eprintln!(
        "Completed {phase}: transaction {} at height {height}",
        tx.to_hex()
    );
    height
}

fn wait_recovered_peers(nodes: &[NodeProcess], phase: &str) {
    wait_for(
        Duration::from_secs(60),
        &format!("six recovered validators fully connected: {phase}"),
        || {
            nodes
                .iter()
                .all(|n| get_json(n.port, "/health").and_then(|h| h["peers"].as_u64()) == Some(5))
        },
    );
}

/// Covers the actual six-process recovered-v3 proposal/commit path, not a
/// private v4 shortcut or direct state publisher. The synthetic executor is
/// protocol evidence only; independent real-model qualification is separate.
#[cfg(unix)]
#[test]
fn recovered_v3_dag_migration_paid_lifecycle_survives_restart() {
    let fx = Fixture::recovered();
    let recovery = fx.recovery.as_ref().unwrap();
    let mut nodes: Vec<_> = (0..6).map(|i| fx.spawn_mode(i, false, false)).collect();
    wait_recovered_peers(&nodes, "initial signed-checkpoint import before activation");
    let initial = recovered_account(nodes[0].port, fx.requesters[0].address());
    assert_eq!(initial.nonce, 0);
    let mut before =
        Transaction::new_transfer(fx.requesters[0].address(), fx.requesters[1].address(), 1, 0);
    before.fee = 1;
    before
        .sign_in_domain(&fx.requesters[0], &recovery.transaction_domain)
        .unwrap();
    let (code, reason) = submit(nodes[0].port, &before);
    assert_eq!(code, 200, "ordinary recovered transfer refused: {reason}");
    assert!(
        recovered_commit(
            &nodes,
            before.hash,
            false,
            "ordinary transfer before activation"
        ) < recovery.activation_height,
        "fixture must actually exercise ordinary traffic before native activation"
    );
    wait_for(
        Duration::from_secs(180),
        "DAG reaches coordinated native activation with admission closed",
        || {
            nodes.iter().all(|n| {
                get_json(n.port, "/native-inference/context").is_some_and(|ctx| {
                    ctx["chain_protocol"] == 3
                        && ctx["native_only_chain"] == false
                        && ctx["request_admission_open"] == false
                        && ctx["request_admission"]["operator_enabled"] == false
                        && ctx["request_admission"]["runtime_ready"] == false
                })
            })
        },
    );
    let ctx = get_json(nodes[0].port, "/native-inference/context").unwrap();
    let domain = InferenceDomain {
        chain_genesis: Hash256::from_hex(ctx["chain_genesis"].as_str().unwrap()).unwrap(),
        recovery_epoch: ctx["recovery_epoch"].as_u64().unwrap(),
        validator_set_hash: Hash256::from_hex(ctx["validator_set_hash"].as_str().unwrap()).unwrap(),
    };
    let (unadmitted, _) = fx.request(0, domain, 1, 1_000_000);
    for node in &nodes {
        assert_eq!(
            submit(node.port, &unadmitted).0,
            503,
            "activation alone must never open paid ingress"
        );
        assert_eq!(
            recovered_account(node.port, fx.requesters[0].address()).balance,
            initial.balance - 2
        );
    }
    let activation_block = get_json(
        nodes[0].port,
        &format!("/block/{}", recovery.activation_height),
    )
    .unwrap();
    assert_eq!(activation_block["header"]["protocol_version"]["major"], 3);
    for node in &nodes {
        assert_eq!(
            get_json(node.port, &format!("/block/{}", recovery.activation_height)).unwrap(),
            activation_block
        );
    }
    stop_recovered_nodes(&mut nodes);
    drop(nodes);

    // Only one node accepts NEW requests. The other five must still retain
    // committed preimages, execute jobs, gossip votes and admit finalizations.
    let mut nodes: Vec<_> = (0..6).map(|i| fx.spawn_mode(i, true, i == 0)).collect();
    wait_recovered_peers(
        &nodes,
        "first graceful restart with native runtimes enabled",
    );
    wait_for(
        Duration::from_secs(60),
        "every native runtime live, exactly one ingress enabled",
        || {
            nodes.iter().enumerate().all(|(i, n)| {
                get_json(n.port, "/native-inference/context").is_some_and(|ctx| {
                    ctx["request_admission"]["runtime_ready"] == true
                        && ctx["request_admission_open"] == (i == 0)
                        && ctx["chain_protocol"] == 3
                        && ctx["native_only_chain"] == false
                })
            })
        },
    );
    let (paid, paid_id) = fx.request(0, domain, 1, 1_000_000);
    assert_eq!(submit(nodes[1].port, &paid).0, 503);
    let (code, reason) = submit(nodes[0].port, &paid);
    assert_eq!(
        code, 200,
        "ready opted-in recovered node refused request: {reason}"
    );
    recovered_commit(&nodes, paid.hash, true, "paid request after first restart");
    wait_finalized(
        &nodes.iter().collect::<Vec<_>>(),
        paid_id,
        Duration::from_secs(120),
        "native quorum finalizes on recovered DAG",
    );
    let paid_path = format!("/native-inference/receipt/{}", paid_id.to_hex());
    let paid_receipt = get_json(nodes[0].port, &paid_path).unwrap();
    assert!(
        paid_receipt["certificate_votes"].as_u64().unwrap() >= 4,
        "the uneven six-member committee cannot certify with one runtime"
    );
    let finalized_tx = Hash256::from_hex(
        paid_receipt["terminal_transaction"]["tx_hash"]
            .as_str()
            .unwrap(),
    )
    .unwrap();
    recovered_commit(&nodes, finalized_tx, true, "paid request finalization");
    let expected_settlement = settlement(nodes[0].port, paid_id).unwrap();
    assert_eq!(
        expected_settlement
            .3
            .iter()
            .map(|(_, amount)| amount)
            .sum::<u64>(),
        100
    );
    assert_eq!(
        expected_settlement
            .3
            .iter()
            .find(|(payee, _)| *payee == fx.requesters[0].address().to_hex())
            .unwrap()
            .1,
        90
    );
    for node in &nodes {
        assert_eq!(settlement(node.port, paid_id).unwrap(), expected_settlement);
        assert_eq!(
            recovered_account(node.port, fx.requesters[0].address()).balance,
            initial.balance - 12
        );
    }
    let mut after =
        Transaction::new_transfer(fx.requesters[0].address(), fx.requesters[1].address(), 1, 2);
    after.fee = 1;
    after
        .sign_in_domain(&fx.requesters[0], &recovery.transaction_domain)
        .unwrap();
    assert_eq!(
        submit(nodes[1].port, &after).0,
        200,
        "ordinary v3 traffic must pass a closed native gate"
    );
    recovered_commit(
        &nodes,
        after.hash,
        false,
        "ordinary transfer after native finalization",
    );

    // A valid one-token job is deliberately smaller than the synthetic
    // executor's fixed two-token answer. Each real worker must refuse output
    // bounds, never invent a shorter answer/certificate. It stays refundable.
    let expires = heights(&nodes).into_iter().flatten().max().unwrap() + 40;
    let (mut bounded, _) = fx.request(1, domain, 0, expires);
    let TxBody::NativeInferenceRequest(body) = &mut bounded.body else {
        unreachable!()
    };
    let mut job = body.request.job.clone();
    job.max_tokens = 1;
    job.max_output_bytes = 4;
    body.request = InferenceRequest::sign(job, &fx.requesters[1]).unwrap();
    let refund_id = body.request.job.request_id();
    bounded
        .sign_in_domain(&fx.requesters[1], &recovery.transaction_domain)
        .unwrap();
    let (code, reason) = submit(nodes[0].port, &bounded);
    assert_eq!(
        code, 200,
        "bounded valid request refused before execution: {reason}"
    );
    recovered_commit(
        &nodes,
        bounded.hash,
        true,
        "bounded request before refund restart",
    );
    for node in &nodes {
        assert_eq!(settlement(node.port, refund_id).unwrap().0, "Pending");
    }
    stop_recovered_nodes(&mut nodes);
    drop(nodes);

    // Restart every process on exactly its durable data/signing state with
    // NEW ingress closed. No checkpoint reimport, state edits or fake tips.
    let mut nodes: Vec<_> = (0..6).map(|i| fx.spawn_mode(i, true, false)).collect();
    wait_recovered_peers(
        &nodes,
        "second graceful restart after paid settlement with refund pending",
    );
    wait_for(
        Duration::from_secs(120),
        "reopened DAG advances to refund height with ingress closed",
        || {
            heights(&nodes)
                .iter()
                .all(|h| h.is_some_and(|h| h + 1 >= expires))
        },
    );
    for node in &nodes {
        assert_eq!(
            get_json(node.port, "/native-inference/context").unwrap()["request_admission_open"],
            false
        );
        assert_eq!(settlement(node.port, paid_id).unwrap(), expected_settlement);
        assert_eq!(settlement(node.port, refund_id).unwrap().0, "Pending");
    }
    let body = TxBody::NativeInferenceRefund(arc_types::transaction::NativeInferenceRefundBody {
        request_id: refund_id.0,
    });
    let mut refund =
        Transaction::new_transfer(fx.requesters[1].address(), fx.requesters[1].address(), 0, 1);
    refund.tx_type = body.tx_type();
    refund.body = body;
    refund.gas_limit = arc_types::transaction::gas_costs::NATIVE_INFERENCE_REFUND;
    refund
        .sign_in_domain(&fx.requesters[1], &recovery.transaction_domain)
        .unwrap();
    let (code, reason) = submit(nodes[2].port, &refund);
    assert_eq!(
        code, 200,
        "closed local ingress stranded an eligible refund: {reason}"
    );
    recovered_commit(
        &nodes,
        refund.hash,
        true,
        "expired request refund after second restart",
    );
    for node in &nodes {
        let receipt = settlement(node.port, refund_id).unwrap();
        assert_eq!(receipt.0, "Refunded");
        assert_eq!(receipt.3, vec![(fx.requesters[1].address().to_hex(), 100)]);
        assert_eq!(
            recovered_account(node.port, fx.requesters[0].address()).balance,
            initial.balance - 14
        );
        assert_eq!(
            recovered_account(node.port, fx.requesters[1].address()).balance,
            initial.balance + 2
        );
        assert_eq!(
            get_json(node.port, "/native-inference/context").unwrap()["chain_protocol"],
            3
        );
    }
    stop_recovered_nodes(&mut nodes);
    // Reopen the real process-written recovery stores and independently
    // verify the stored native certificate against the frozen binding. Four
    // high-stake validators can meet this fleet's strict stake quorum; a
    // fixed five-vote assertion would incorrectly change the actual rule.
    let genesis = arc_node::config::load_genesis(fx.genesis.to_str().unwrap()).unwrap();
    let policy = arc_state::recovery::RecoveryNetworkPolicy {
        chain_id: genesis.chain.chain_id.clone(),
        genesis_hash: genesis.network_hash(false).unwrap(),
        recovery_epoch: 1,
        validator_set_id: 1,
        validators: genesis.validated_validator_set(false).unwrap(),
        community_rewards_v1_activation_height: None,
    };
    for node in &nodes {
        let state = arc_state::StateDB::with_genesis_persistent_recovery(
            &[],
            &node.data_dir,
            policy.clone(),
            None,
        )
        .unwrap();
        let context = state.native_inference_context().unwrap();
        let stored = state
            .native_inference_receipt(paid_id, context.commitment().unwrap())
            .unwrap()
            .unwrap();
        let pending = stored
            .metadata
            .request
            .validate(&context.domain, &context.members, stored.admission_height)
            .unwrap();
        let terminal_height = stored.terminal_transaction.unwrap().block_height;
        let plan = arc_types::inference_contract::plan_finalize(
            &pending,
            stored.metadata.certificate.as_ref().unwrap(),
            &context.members,
            &context.domain,
            terminal_height,
        )
        .unwrap();
        let arc_types::inference_contract::SettlementPlan::Finalize {
            credits,
            output_hash,
            ..
        } = plan
        else {
            unreachable!()
        };
        assert_eq!(credits, stored.metadata.credits);
        assert_eq!(Some(output_hash), stored.metadata.output_hash);
        assert!(
            state
                .native_inference_pending_requests(context.commitment().unwrap())
                .unwrap()
                .is_empty()
        );
        assert_eq!(
            state
                .get_block(state.height())
                .unwrap()
                .header
                .protocol_version
                .major,
            3
        );
    }
}

#[test]
fn a_paid_request_settles_identically_on_every_validator_of_one_chain() {
    let fx = Fixture::new(9940, 9140, &[]);
    let nodes: Vec<NodeProcess> = (0..NODES).map(|i| fx.spawn(i)).collect();

    // All four start at the same instant. Before the transport served inbound
    // handshakes while its own bootstrap dials were in flight (D1), this took
    // at least 45 s of dial timeouts plus the 30 s reconnect timer; 40 s is a
    // regression bound with slack for a loaded debug build.
    wait_for(
        Duration::from_secs(40),
        "every node healthy and fully peered",
        || {
            nodes.iter().all(|n| {
                get_json(n.port, "/health")
                    .and_then(|h| h["peers"].as_u64())
                    .is_some_and(|p| p >= (NODES - 1) as u64)
            })
        },
    );
    wait_for(
        Duration::from_secs(120),
        "the committee to commit blocks",
        || heights(&nodes).iter().all(|h| h.unwrap_or(0) >= 3),
    );

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
    assert_eq!(
        code, 200,
        "a signed native request must be admitted: {body}"
    );

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
    assert_eq!(
        paid, 100,
        "settlement must drain the reserved escrow exactly: {credits:?}"
    );
    // The reserve (100) minus the execution price (10) goes back to the
    // requester, once.
    let requester_hex = fx.requesters[0].address().to_hex();
    let refunds: Vec<u64> = credits
        .iter()
        .filter(|(payee, _)| *payee == requester_hex)
        .map(|(_, amount)| *amount)
        .collect();
    assert_eq!(
        refunds,
        vec![90],
        "the unused reserve is refunded exactly once: {credits:?}"
    );
    // The price is split pro rata by stake across the signers of the matching
    // output (plan_finalize: floor shares, remainder units one each to the
    // first). Equal stakes here, so every share is floor or floor + 1.
    let committee: Vec<String> = (0..NODES)
        .map(|i| {
            validator_keypair(&format!("p5-validator-{i}"))
                .address()
                .to_hex()
        })
        .collect();
    let shares: Vec<u64> = credits
        .iter()
        .filter(|(payee, _)| *payee != requester_hex)
        .map(|(payee, amount)| {
            assert!(
                committee.contains(payee),
                "{payee} is paid but is not a committee member"
            );
            *amount
        })
        .collect();
    assert!(
        shares.len() * 3 > NODES * 2,
        "only a strict supermajority certificate can finalize: {} signers paid",
        shares.len()
    );
    assert_eq!(
        shares.iter().sum::<u64>(),
        10,
        "exactly the price reaches the signers: {credits:?}"
    );
    let floor = 10 / shares.len() as u64;
    assert!(
        shares
            .iter()
            .all(|share| *share == floor || *share == floor + 1),
        "equal stakes split the price evenly: {credits:?}"
    );

    // ── ingress admits exactly the nonce valid at the next height ──────────
    // A future nonce is refused rather than parked: the preflight is the
    // executor's own admission check, and a native request is not admissible
    // until its predecessor has been included.
    let (future, _) = fx.request(0, domain, 2, expiry);
    assert_eq!(
        submit(nodes[1].port, &future).0,
        400,
        "a future nonce is not admissible yet"
    );

    // ── two requesters' requests into one node back-to-back ────────────────
    // Before the fix both could land in one DAG block, which block execution
    // refuses as fatal - on every node at once. Selection at commit now
    // executes one and carries the other to a later block.
    let (tx1, second) = fx.request(0, domain, 1, expiry);
    let (tx2, third) = fx.request(1, domain, 0, expiry);
    for tx in [&tx1, &tx2] {
        let (code, body) = submit(nodes[1].port, tx);
        assert_eq!(
            code, 200,
            "back-to-back request from {} refused: {body}",
            tx.from
        );
    }
    wait_for(
        Duration::from_secs(240),
        "both back-to-back requests to be Finalized everywhere",
        || {
            [second, third].iter().all(|id| {
                nodes.iter().all(|n| {
                    settlement(n.port, *id).is_some_and(|(status, ..)| status == "Finalized")
                })
            })
        },
    );
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
    let mut transfer =
        Transaction::new_transfer(fx.requesters[0].address(), hash_bytes(b"somebody"), 1, 3);
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
        &format!(
            "{{\"address\":\"{}\"}}",
            hash_bytes(b"faucet-target").to_hex()
        ),
        &format!("http://127.0.0.1:{}/faucet/claim", nodes[2].port),
    ]);
    assert_eq!(
        code, 503,
        "the faucet cannot be offered on a protocol-4 chain"
    );

    // Nobody's consensus loop exited along the way.
    for (i, n) in nodes.iter().enumerate() {
        assert!(
            get_json(n.port, "/health").is_some(),
            "node {i} stopped answering"
        );
        let diag = get_json(n.port, "/consensus/diagnostics").unwrap();
        eprintln!(
            "node {i}: votes gossiped={} accepted={} refused={} protocol4_omitted={}",
            diag["native_votes_gossiped"],
            diag["native_votes_accepted"],
            diag["native_votes_refused"],
            diag["protocol4_omitted_transactions"]
        );
    }
    let _: &Path = fx.dir.path();
}

/// Wait until `id` is Finalized on every listed node.
/// A configuration change on a running committee, with real processes.
///
/// NOTE on the restart shape: this rolls one validator at a time on purpose.
/// Stopping the whole committee and starting it again does not resume block
/// production - every node then sits requesting bounded DAG history from the
/// round it stopped at, and none of them advances. That is a property of the
/// consensus restart path, not of anything here, and it is the reason a
/// rollout is done one node at a time.
///
/// Publishing a binding writes state, so every validator has to publish it in
/// the SAME block or they derive different roots from there on. This drives
/// that through the operator seam - a config file naming the binding being
/// replaced and the coordinated height - across four `arc-node` processes,
/// and checks what the chain does either side of it: same binding everywhere,
/// still agreeing block for block, work before and after the change settling
/// with the escrow drained exactly.
#[test]
fn a_binding_update_publishes_on_every_validator_at_one_coordinated_height() {
    let fx = Fixture::new(9960, 9160, &[]);
    let nodes: Vec<NodeProcess> = (0..NODES).map(|i| fx.spawn(i)).collect();
    wait_for(
        Duration::from_secs(40),
        "every node healthy and fully peered",
        || {
            nodes.iter().all(|n| {
                get_json(n.port, "/health")
                    .and_then(|h| h["peers"].as_u64())
                    .is_some_and(|p| p >= (NODES - 1) as u64)
            })
        },
    );
    wait_for(
        Duration::from_secs(120),
        "the committee to commit blocks",
        || heights(&nodes).iter().all(|h| h.unwrap_or(0) >= 3),
    );

    let ctx = get_json(nodes[0].port, "/native-inference/context").expect("activated");
    let v1 = ctx["context_commitment"].as_str().unwrap().to_string();
    let domain = InferenceDomain {
        chain_genesis: Hash256::from_hex(ctx["chain_genesis"].as_str().unwrap()).unwrap(),
        recovery_epoch: ctx["recovery_epoch"].as_u64().unwrap(),
        validator_set_hash: Hash256::from_hex(ctx["validator_set_hash"].as_str().unwrap()).unwrap(),
    };
    for n in &nodes[1..] {
        let other = get_json(n.port, "/native-inference/context").expect("activated");
        assert_eq!(
            other["context_commitment"].as_str().unwrap(),
            v1,
            "every validator starts on the same binding"
        );
        assert_eq!(other["allowed_execution_count"], 1);
    }

    // Work admitted under the binding in force before the change.
    let expiry = heights(&nodes)[0].unwrap() + 5_000;
    let (tx, before) = fx.request(0, domain, 0, expiry);
    let (code, body) = submit(nodes[0].port, &tx);
    assert_eq!(code, 200, "a request under the first binding: {body}");
    wait_finalized(
        &nodes.iter().collect::<Vec<_>>(),
        before,
        Duration::from_secs(180),
        "the request submitted before the update",
    );

    // A rolling restart, which is what a real rollout is: one validator at a
    // time, the chain still producing on the others. Stopping the whole
    // committee at once does NOT work - see the note below - so the
    // coordinated height has to be far enough ahead that every node is back
    // before the chain reaches it.
    let running_at = heights(&nodes)
        .iter()
        .filter_map(|h| *h)
        .max()
        .expect("a committed height");
    // Deliberately close. The change is applied inside the block that reaches
    // this height, on every block-application path, so a validator still
    // catching up when the chain gets here applies it as it executes that
    // block - or inherits it with whatever state it took. An earlier version
    // applied it from a hook on one path only, and this margin was the
    // difference between the committee agreeing and one node silently holding
    // a different binding.
    let at_height = running_at + 15;

    let upgraded = [
        hash_bytes(b"p5-model-2"),
        hash_bytes(b"p5-profile-2"),
        hash_bytes(b"p5-generation-2"),
        hash_bytes(b"p5-assignment-2"),
    ];
    let execution = |t: &[Hash256; 4]| {
        serde_json::json!({
            "model_hash": t[0].to_hex(),
            "profile_hash": t[1].to_hex(),
            "generation_hash": t[2].to_hex(),
            "assignment_hash": t[3].to_hex(),
        })
    };
    std::fs::write(
        &fx.activation,
        serde_json::json!({
            "recovery_epoch": 0,
            "allowed_executions": [execution(&fx.tuple), execution(&upgraded)],
            "update_from": v1,
            "update_at_height": at_height,
        })
        .to_string(),
    )
    .unwrap();

    // One at a time. Each node reads the new config on its own restart,
    // authorises the update, and keeps serving under the binding still in
    // force until the chain reaches the coordinated height.
    let mut slots: Vec<Option<NodeProcess>> = nodes.into_iter().map(Some).collect();
    for index in 0..NODES {
        slots[index] = None;
        slots[index] = Some(fx.spawn(index));
        let port = slots[index].as_ref().unwrap().port;
        wait_for(
            Duration::from_secs(90),
            "the restarted validator to rejoin",
            || {
                get_json(port, "/health")
                    .and_then(|h| h["peers"].as_u64())
                    .is_some_and(|peers| peers >= 1)
            },
        );
    }
    let nodes: Vec<NodeProcess> = slots.into_iter().map(|slot| slot.unwrap()).collect();
    wait_for(
        Duration::from_secs(120),
        "the committee fully peered after the rolling restart",
        || {
            nodes.iter().all(|n| {
                get_json(n.port, "/health")
                    .and_then(|h| h["peers"].as_u64())
                    .is_some_and(|p| p >= (NODES - 1) as u64)
            })
        },
    );
    wait_for(
        Duration::from_secs(420),
        "every validator to publish the new binding",
        || {
            nodes.iter().all(|n| {
                get_json(n.port, "/native-inference/context")
                    .and_then(|c| c["allowed_execution_count"].as_u64())
                    .is_some_and(|count| count == 2)
            })
        },
    );

    // The same new binding on every one of them, and not the old one.
    let updated = get_json(nodes[0].port, "/native-inference/context").expect("activated");
    let v2 = updated["context_commitment"].as_str().unwrap().to_string();
    assert_ne!(v2, v1, "a new configuration is a new binding");
    for n in &nodes[1..] {
        let other = get_json(n.port, "/native-inference/context").expect("activated");
        assert_eq!(
            other["context_commitment"].as_str().unwrap(),
            v2,
            "every validator published the SAME binding"
        );
    }

    // Still one chain: same block hash at a height past the change.
    wait_for(
        Duration::from_secs(180),
        "blocks past the coordinated height",
        || {
            heights(&nodes)
                .iter()
                .all(|h| h.unwrap_or(0) >= at_height + 2)
        },
    );
    let agreed = at_height + 1;
    let reference = block_hash(nodes[0].port, agreed).expect("a block past the update");
    for (i, n) in nodes.iter().enumerate().skip(1) {
        assert_eq!(
            block_hash(n.port, agreed).as_deref(),
            Some(reference.as_str()),
            "validator {i} disagrees about the block after the binding update"
        );
    }

    // The new configuration governs new work: a request for the model the
    // chain only allows after the update.
    let domain2 = InferenceDomain {
        chain_genesis: Hash256::from_hex(updated["chain_genesis"].as_str().unwrap()).unwrap(),
        recovery_epoch: updated["recovery_epoch"].as_u64().unwrap(),
        validator_set_hash: Hash256::from_hex(updated["validator_set_hash"].as_str().unwrap())
            .unwrap(),
    };
    let expiry = heights(&nodes)[0].unwrap() + 5_000;
    let (tx, _upgraded_request) = fx.request_using(1, domain2, 0, expiry, upgraded);
    let (code, body) = submit(nodes[0].port, &tx);
    assert_eq!(
        code, 200,
        "the upgraded model is admissible only because the update added it: {body}"
    );
    // It is admitted, not finalized: a worker refuses any job whose model is
    // not the one it is qualified for, so adding a model to the chain's
    // allowlist does not provision it. That is the correct boundary - the
    // chain's configuration and the operators' loaded models are separate
    // things, and an update to one is not an update to the other.

    // And the chain still settles the work it can execute, after the update.
    // Requester 0, whose earlier request finalized, so its next nonce is 1.
    // Requester 1's nonce is still 0: its upgraded-model request was admitted
    // and is waiting for a worker that will never take it.
    let (tx, after) = fx.request_using(0, domain2, 1, expiry, fx.tuple);
    let (code, body) = submit(nodes[0].port, &tx);
    assert_eq!(code, 200, "a request for the served model: {body}");
    wait_finalized(
        &nodes.iter().collect::<Vec<_>>(),
        after,
        Duration::from_secs(180),
        "a request submitted and settled under the new binding",
    );

    // Payment conservation, on every replica, for both requests.
    for id in [before, after] {
        let settled: Vec<_> = nodes.iter().map(|n| settlement(n.port, id)).collect();
        let first = settled[0].clone().expect("a settlement");
        for (i, s) in settled.iter().enumerate() {
            assert_eq!(
                s.as_ref().expect("a settlement"),
                &first,
                "replica {i} disagrees about the settlement of {}",
                id.to_hex()
            );
        }
        let paid: u64 = first.3.iter().map(|(_, amount)| amount).sum();
        assert_eq!(
            paid, 100,
            "settlement drains the reserved escrow exactly, either side of a binding update: {:?}",
            first.3
        );
    }
}

fn wait_finalized(nodes: &[&NodeProcess], id: Hash256, timeout: Duration, label: &str) {
    wait_for(timeout, label, || {
        nodes
            .iter()
            .all(|n| settlement(n.port, id).is_some_and(|(status, ..)| status == "Finalized"))
    });
}

fn block_hash(port: u16, height: u64) -> Option<String> {
    get_json(port, &format!("/block/{height}")).and_then(|b| b["hash"].as_str().map(str::to_string))
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
        &[
            "--dag-retained-rounds",
            "100",
            "--snapshot-every-blocks",
            "50",
        ],
    );
    let mut nodes: Vec<NodeProcess> = (0..NODES).map(|i| fx.spawn(i)).collect();
    wait_for(
        Duration::from_secs(40),
        "every node healthy and fully peered",
        || {
            nodes.iter().all(|n| {
                get_json(n.port, "/health")
                    .and_then(|h| h["peers"].as_u64())
                    .is_some_and(|p| p >= (NODES - 1) as u64)
            })
        },
    );
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
    wait_for(
        Duration::from_secs(300),
        "the chain to pass 3x the retention window",
        || round(nodes[0].port) >= 300,
    );

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
    wait_for(
        Duration::from_secs(5),
        "the three to keep committing",
        || {
            up.iter().all(|n| {
                get_json(n.port, "/health")
                    .and_then(|h| h["height"].as_u64())
                    .is_some_and(|h| h > killed_at)
            })
        },
    );

    // Restart it on the same data directory.
    let old = std::mem::replace(&mut nodes[victim], fx.spawn(victim));
    std::mem::forget(old); // already reaped above; its Drop would kill nothing useful
    let victim_port = nodes[victim].port;

    // It must catch up past the height it was killed at and agree with its
    // peers block for block at a common height.
    wait_for(
        Duration::from_secs(180),
        "the restarted validator to catch up",
        || {
            let mine = get_json(victim_port, "/health").and_then(|h| h["height"].as_u64());
            let theirs = get_json(nodes[0].port, "/health").and_then(|h| h["height"].as_u64());
            matches!((mine, theirs), (Some(m), Some(t)) if m >= killed_at + 20 && m + 10 >= t)
        },
    );
    let common = get_json(victim_port, "/health").unwrap()["height"]
        .as_u64()
        .unwrap()
        - 5;
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
    wait_finalized(
        &all,
        while_down,
        Duration::from_secs(120),
        "work from the downtime settles everywhere",
    );
    assert_eq!(
        settlement(victim_port, while_down),
        settlement(nodes[0].port, while_down)
    );

    // New work submitted THROUGH the restarted node settles everywhere.
    let (tx, after) = fx.request(1, domain, 1, expiry);
    wait_for(
        Duration::from_secs(60),
        "the restarted node to accept new work",
        || submit(victim_port, &tx).0 == 200,
    );
    wait_finalized(
        &all,
        after,
        Duration::from_secs(120),
        "new work through the restarted node",
    );

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

/// A validator down LONGER than its peers' DAG retention rejoins by adopting
/// an authenticated checkpoint.
///
/// History cannot help it: its commit cursor round is pruned everywhere. It
/// used to verify a checkpoint and then refuse to adopt it because it had
/// history of its own ("rebasing a durable store onto a checkpoint is not
/// implemented"), and stay down. Now it adopts one durably (`WalOp::Rebase`,
/// with the resume round proven from the certified tip header), rebuilds the
/// DAG from the round after the checkpoint's anchor, and carries on.
#[test]
fn a_validator_down_longer_than_retention_rejoins_by_checkpoint() {
    let fx = Fixture::new(
        9930,
        9130,
        &[
            "--dag-retained-rounds",
            "100",
            "--snapshot-every-blocks",
            "50",
        ],
    );
    let mut nodes: Vec<NodeProcess> = (0..NODES).map(|i| fx.spawn(i)).collect();
    wait_for(
        Duration::from_secs(40),
        "every node healthy and fully peered",
        || {
            nodes.iter().all(|n| {
                get_json(n.port, "/health")
                    .and_then(|h| h["peers"].as_u64())
                    .is_some_and(|p| p >= (NODES - 1) as u64)
            })
        },
    );
    let ctx = get_json(nodes[0].port, "/native-inference/context").expect("activated");
    let domain = InferenceDomain {
        chain_genesis: Hash256::from_hex(ctx["chain_genesis"].as_str().unwrap()).unwrap(),
        recovery_epoch: ctx["recovery_epoch"].as_u64().unwrap(),
        validator_set_hash: Hash256::from_hex(ctx["validator_set_hash"].as_str().unwrap()).unwrap(),
    };
    let round = |port: u16| {
        get_json(port, "/health")
            .and_then(|h| h["dag_round"].as_u64())
            .unwrap_or(0)
    };
    wait_for(
        Duration::from_secs(300),
        "the chain to pass 2x the retention window",
        || round(nodes[0].port) >= 200,
    );

    let victim = 3;
    let killed_round = round(nodes[victim].port);
    {
        let process = &mut nodes[victim];
        process.child.kill().unwrap();
        process.child.wait().unwrap();
    }
    // Work while it is down, then let the others run far past its cursor:
    // more than their retention, so no history can reach it.
    let (tx, while_down) = fx.request(1, domain, 0, 1_000_000);
    assert_eq!(submit(nodes[0].port, &tx).0, 200);
    wait_for(
        Duration::from_secs(300),
        "the others to run past the victim's retention",
        || round(nodes[0].port) >= killed_round + 250,
    );

    let old = std::mem::replace(&mut nodes[victim], fx.spawn(victim));
    std::mem::forget(old);
    let victim_port = nodes[victim].port;
    wait_for(
        Duration::from_secs(240),
        "the restarted validator to catch up",
        || {
            let mine = get_json(victim_port, "/health").and_then(|h| h["height"].as_u64());
            let theirs = get_json(nodes[0].port, "/health").and_then(|h| h["height"].as_u64());
            matches!((mine, theirs), (Some(m), Some(t)) if m + 10 >= t && m > 0)
        },
    );
    let log = std::fs::read_to_string(nodes[victim].data_dir.join("node.log")).unwrap();
    assert!(
        log.contains("Adopted an authenticated checkpoint"),
        "the restarted validator did not rejoin through a checkpoint"
    );
    let common = get_json(victim_port, "/health").unwrap()["height"]
        .as_u64()
        .unwrap()
        - 5;
    let reference = block_hash(nodes[0].port, common).expect("peer block");
    for n in &nodes {
        assert_eq!(
            block_hash(n.port, common).as_deref(),
            Some(reference.as_str())
        );
    }
    let all: Vec<&NodeProcess> = nodes.iter().collect();
    wait_finalized(
        &all,
        while_down,
        Duration::from_secs(120),
        "work from the downtime settles everywhere",
    );
    let (tx, after) = fx.request(1, domain, 1, 1_000_000);
    wait_for(
        Duration::from_secs(60),
        "the rejoined node to accept new work",
        || submit(victim_port, &tx).0 == 200,
    );
    wait_finalized(
        &all,
        after,
        Duration::from_secs(120),
        "new work through the rejoined node",
    );

    // And a plain restart afterwards recovers from its own rebased store.
    {
        let process = &mut nodes[victim];
        process.child.kill().unwrap();
        process.child.wait().unwrap();
    }
    std::thread::sleep(Duration::from_secs(3));
    let old = std::mem::replace(&mut nodes[victim], fx.spawn(victim));
    std::mem::forget(old);
    wait_for(
        Duration::from_secs(120),
        "the rebased node to restart and catch up",
        || {
            let mine = get_json(victim_port, "/health").and_then(|h| h["height"].as_u64());
            let theirs = get_json(nodes[0].port, "/health").and_then(|h| h["height"].as_u64());
            matches!((mine, theirs), (Some(m), Some(t)) if m + 10 >= t && m > common)
        },
    );
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
