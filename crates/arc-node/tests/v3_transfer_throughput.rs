//! Relative protocol-v3 transfer throughput before and after block-level fee
//! settlement, on the six-process recovered-v3 fixture.
//!
//! Phase A runs the launch rule (no schedule): every transfer credits the
//! shared fee treasury itself, so a canonical block carries one transfer.
//! Phase B activates block-level settlement through the harness-only override
//! (`--test-v3-fee-settlement-activation-height`), so disjoint transfers
//! share a block and the treasury is credited once per block. Both phases
//! start six fresh `arc-node` processes from the same signed checkpoint, with
//! the same native migration record the live chain carries (which keeps
//! blocks on the sequential executor), and drive the same signed v3 transfer
//! load through every validator's `/tx/submit_batch`.
//!
//! Measured per phase: transactions per second of canonical block time,
//! `tx_count` per block, `commit_execute_us` per committed block, and where a
//! consensus round's time goes (every `/consensus/diagnostics` phase timer,
//! per validator and per round) next to each process's CPU time. Two extra
//! settlement phases separate harness effects from per-round cost: one fans
//! every submission out to all six validators, one offers far less load.
//! The JSON report is written to `ARC_V3_THROUGHPUT_REPORT`; node logs are
//! copied to `ARC_V3_THROUGHPUT_LOG_DIR`. Runner numbers are only meaningful
//! relative to each other.
//!
//! Tunables: `ARC_V3_THROUGHPUT_SENDERS` (default 800),
//! `ARC_V3_THROUGHPUT_LOW_LOAD_SENDERS` (100),
//! `ARC_V3_THROUGHPUT_WARMUP_SECONDS` (15) and
//! `ARC_V3_THROUGHPUT_MEASURE_SECONDS` (60).
#![cfg(all(unix, feature = "v3-fee-settlement-test-override"))]

use arc_crypto::signature::KeyPair;
use arc_crypto::{Hash256, hash_bytes};
use arc_types::transaction::{Transaction, TxBody};
use serde_json::{Value, json};
use std::collections::HashMap;
use std::io::Write;
use std::path::{Path, PathBuf};
use std::process::{Child, Command, Stdio};
use std::time::{Duration, Instant};

const VALIDATORS: usize = 6;
const STAKES: [u64; VALIDATORS] = [
    6_666_667, 6_666_667, 6_666_667, 6_666_667, 6_666_666, 6_666_666,
];
const SENDER_BALANCE: u64 = 1_000_000_000_000;
const TRANSFER_AMOUNT: u64 = 1;
const TRANSFER_FEE: u64 = 1;
/// `MAX_TX_SUBMIT_BATCH_SIZE` in the RPC server.
const SUBMIT_BATCH: usize = 64;
/// At least the RPC's 100 ms per-sender submission interval.
const WAVE_INTERVAL: Duration = Duration::from_millis(250);
/// Resubmit an envelope that has not reached a canonical block by then.
const RESUBMIT_AFTER: Duration = Duration::from_secs(10);
/// Retry sooner when no validator has accepted the envelope yet (for example
/// its target had not executed the sender's previous transfer).
const RETRY_UNACCEPTED_AFTER: Duration = Duration::from_secs(1);
/// `/proc/<pid>/stat` CPU times are in USER_HZ ticks, 100 per second on Linux.
const USER_HZ: f64 = 100.0;
/// Every phase timer `/consensus/diagnostics` exports, in microseconds.
const TIMER_KEYS: &[&str] = &[
    "loop_busy_us",
    "phase_inbound_us",
    "live_block_availability_us",
    "live_block_validate_us",
    "gossip_admit_us",
    "phase_certificates_us",
    "phase_history_us",
    "phase_checkpoint_us",
    "phase_absence_us",
    "phase_propose_us",
    "proposal_selection_us",
    "phase_commit_us",
    "commit_selection_us",
    "commit_execute_us",
    "dag_block_persist_us",
    "signing_record_persist_us",
    "state_snapshot_publish_us",
];
/// Event counters reported next to the timers.
const COUNT_KEYS: &[&str] = &[
    "rounds_advanced",
    "canonical_blocks_produced",
    "live_blocks_received",
    "live_blocks_accepted",
    "live_blocks_rejected_duplicate",
    "gossip_transactions_received",
    "gossip_transactions_already_pending",
    "stale_transactions_dropped",
    "loop_slow_iterations",
    "outbound_dropped",
    "history_requests_sent",
    "pending_blocks_held",
];
/// Native migration activates this many blocks after the recovery transition.
const NATIVE_ACTIVATION_OFFSET: u64 = 8;
/// Phase B activates block-level fee settlement this many blocks after it.
const FEE_SETTLEMENT_OFFSET: u64 = 12;
/// Measurement starts no earlier than this many blocks after it.
const WARM_HEIGHT_OFFSET: u64 = 16;

fn env_number(name: &str, default: u64) -> u64 {
    std::env::var(name)
        .ok()
        .and_then(|value| value.parse().ok())
        .unwrap_or(default)
}

fn derived_keypair(context: &str, seed: &str) -> KeyPair {
    let bytes = blake3::derive_key(context, seed.as_bytes());
    KeyPair::Ed25519(ed25519_dalek::SigningKey::from_bytes(&bytes))
}

fn curl(args: &[&str]) -> (u32, String) {
    let out = Command::new("curl")
        .args(["-s", "-w", "\n%{http_code}", "--max-time", "20"])
        .args(args)
        .output()
        .expect("curl must be available");
    let body = String::from_utf8_lossy(&out.stdout).to_string();
    let (body, code) = body.rsplit_once('\n').unwrap_or((body.as_str(), "0"));
    (code.trim().parse().unwrap_or(0), body.to_string())
}

fn get_json(port: u16, path: &str) -> Option<Value> {
    let (code, body) = curl(&[&format!("http://127.0.0.1:{port}{path}")]);
    (code == 200)
        .then(|| serde_json::from_str(&body).ok())
        .flatten()
}

/// POST a JSON body through curl's stdin so large batches stay off argv.
fn post_json(port: u16, path: &str, body: &str) -> (u32, String) {
    let mut child = Command::new("curl")
        .args([
            "-s",
            "-w",
            "\n%{http_code}",
            "--max-time",
            "20",
            "-X",
            "POST",
            "-H",
            "Content-Type: application/json",
            "--data-binary",
            "@-",
            &format!("http://127.0.0.1:{port}{path}"),
        ])
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .spawn()
        .expect("curl must be available");
    child
        .stdin
        .take()
        .expect("curl stdin")
        .write_all(body.as_bytes())
        .expect("curl accepts the request body");
    let out = child.wait_with_output().expect("curl finishes");
    let text = String::from_utf8_lossy(&out.stdout).to_string();
    let (text, code) = text.rsplit_once('\n').unwrap_or((text.as_str(), "0"));
    (code.trim().parse().unwrap_or(0), text.to_string())
}

fn wait_for<F: FnMut() -> bool>(timeout: Duration, label: &str, mut condition: F) {
    let start = Instant::now();
    while start.elapsed() < timeout {
        if condition() {
            return;
        }
        std::thread::sleep(Duration::from_millis(500));
    }
    panic!("timed out after {timeout:?} waiting for: {label}");
}

fn height(port: u16) -> Option<u64> {
    get_json(port, "/health").and_then(|health| health["height"].as_u64())
}

/// The recovered network both phases start from: one genesis, one signed
/// checkpoint, one native migration record and every key, all deterministic.
struct Network {
    dir: tempfile::TempDir,
    genesis: PathBuf,
    activation: PathBuf,
    checkpoint: PathBuf,
    manifest: Hash256,
    transaction_domain: Hash256,
    transition_height: u64,
    senders: Vec<KeyPair>,
    sender_addresses: Vec<Hash256>,
}

impl Network {
    fn build(sender_count: usize) -> Self {
        use arc_state::recovery::{ArcCheckpoint, RecoveryExportSpec, RecoveryValidator};
        use arc_types::inference_contract::{
            InferenceDomain, ValidatorMember, validator_set_commitment,
        };

        let dir = tempfile::tempdir().unwrap();
        let validators: Vec<KeyPair> = (0..VALIDATORS)
            .map(|index| {
                derived_keypair(
                    "ARC-chain-validator-keypair-v1",
                    &format!("v3-throughput-validator-{index}"),
                )
            })
            .collect();
        let senders: Vec<KeyPair> = (0..sender_count)
            .map(|index| {
                derived_keypair(
                    "ARC-v3-transfer-throughput-sender-v1",
                    &format!("sender-{index}"),
                )
            })
            .collect();
        let sender_addresses: Vec<Hash256> = senders.iter().map(KeyPair::address).collect();

        let instance = format!(
            "{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        );
        let mut genesis = format!(
            "[chain]\nname = \"arc-v3-transfer-throughput\"\nchain_id = \"0x415243\"\n\
             validator_set_complete = true\ninstance_id = \"{instance}\"\n\n"
        );
        for address in &sender_addresses {
            genesis.push_str(&format!(
                "[[accounts]]\naddress = \"{}\"\nbalance = {SENDER_BALANCE}\n\n",
                address.to_hex()
            ));
        }
        for validator in &validators {
            genesis.push_str(&format!(
                "[[accounts]]\naddress = \"{}\"\nbalance = 1_000_000_000_000\n\n",
                validator.address().to_hex()
            ));
        }
        for (validator, stake) in validators.iter().zip(STAKES) {
            genesis.push_str(&format!(
                "[[validators]]\naddress = \"{}\"\nstake = {stake}\n\n",
                validator.address().to_hex()
            ));
        }
        let genesis_path = dir.path().join("genesis.toml");
        std::fs::write(&genesis_path, genesis).unwrap();
        let config = arc_node::config::load_genesis(genesis_path.to_str().unwrap()).unwrap();
        let genesis_hash = config.network_hash(false).unwrap();

        for (index, key) in validators.iter().enumerate() {
            let KeyPair::Ed25519(secret) = key else {
                panic!("the harness uses Ed25519 validators");
            };
            let mut file = arc_crypto::secret_file::create_new_private(
                &dir.path().join(format!("validator-{index}.key.json")),
            )
            .unwrap();
            file.write_all(
                json!({
                    "scheme": "ed25519",
                    "secret_key": hex::encode(secret.to_bytes()),
                    "public_key": hex::encode(key.public_key_bytes()),
                    "address": key.address().to_hex(),
                })
                .to_string()
                .as_bytes(),
            )
            .unwrap();
            file.sync_all().unwrap();
        }

        let mut funds = config.validated_accounts().unwrap();
        funds.push((
            arc_state::recovery::recovery_stake_reserve_address(),
            40_000_000,
        ));
        let source = arc_state::StateDB::with_genesis(&funds);
        let joins: Vec<Transaction> = validators
            .iter()
            .zip(STAKES)
            .map(|(key, stake)| {
                let mut tx = Transaction::new_transfer(key.address(), key.address(), 0, 0);
                tx.body = TxBody::JoinValidator(arc_types::transaction::JoinValidatorBody {
                    pubkey: key.public_key_bytes().try_into().unwrap(),
                    initial_stake: stake,
                });
                tx.tx_type = tx.body.tx_type();
                tx.sign(key).unwrap();
                tx
            })
            .collect();
        let (_, receipts) = source
            .execute_block(&joins, validators[0].address())
            .unwrap();
        assert!(receipts.iter().all(|receipt| receipt.success));
        for key in &validators {
            let mut account = source.get_account(&key.address()).unwrap();
            account.staked_balance = 0;
            source.update_account(&key.address(), account);
        }
        source.execute_block(&[], validators[0].address()).unwrap();
        let recovery_validators = validators
            .iter()
            .zip(STAKES)
            .map(|(key, stake)| RecoveryValidator {
                address: key.address(),
                public_key: key.public_key_bytes().try_into().unwrap(),
                stake,
            })
            .collect();
        let mut checkpoint = ArcCheckpoint::export_unsigned(
            &source,
            RecoveryExportSpec {
                chain_id: config.chain.chain_id.clone(),
                genesis_hash,
                source_consensus_round: 64,
                recovery_epoch: 1,
                validator_set_id: 1,
                validators: recovery_validators,
                community_rewards_v1_activation_height: None,
                created_at_unix_ms: 1,
            },
        )
        .unwrap();
        for key in validators.iter().take(5) {
            checkpoint.add_signature(key).unwrap();
        }
        let checkpoint_path = dir.path().join("approved.arcchkpt");
        checkpoint.write_to(&checkpoint_path).unwrap();
        let transition_height = checkpoint.manifest.source_height + 1;

        // The live chain is migrated: its native binding keeps every block on
        // the sequential executor. Mirror that, with native ingress closed.
        let mut members: Vec<ValidatorMember> = validators
            .iter()
            .zip(STAKES)
            .map(|(key, stake)| ValidatorMember::new(key.address(), stake))
            .collect();
        members.sort_by_key(|member| member.address.0);
        let tuple = hash_bytes(b"v3-transfer-throughput-unused-model");
        let context = arc_state::InferenceAdmissionContext {
            domain: InferenceDomain {
                chain_genesis: genesis_hash,
                recovery_epoch: 1,
                validator_set_hash: validator_set_commitment(&members).unwrap(),
            },
            members,
            allowed_executions: vec![arc_state::AllowedExecution {
                model_hash: tuple,
                profile_hash: tuple,
                generation_hash: tuple,
                assignment_hash: tuple,
            }],
            selection_rule: arc_state::NativeSelectionRule::SkipUsedNoncesV2,
        };
        let activation_path = dir.path().join("activation.json");
        std::fs::write(
            &activation_path,
            json!({
                "recovery_epoch": 1,
                "allowed_executions": context.allowed_executions,
                "selection_rule": context.selection_rule,
                "migration": arc_state::NativeMigrationRecord {
                    chain_genesis: genesis_hash,
                    recovery_epoch: 1,
                    validator_set_id: 1,
                    activation_height: transition_height + NATIVE_ACTIVATION_OFFSET,
                    context_commitment: context.commitment().unwrap(),
                },
            })
            .to_string(),
        )
        .unwrap();

        Self {
            genesis: genesis_path,
            activation: activation_path,
            checkpoint: checkpoint_path,
            manifest: checkpoint.manifest_hash(),
            transaction_domain: checkpoint.manifest.recovery_context().domain_hash(),
            transition_height,
            senders,
            sender_addresses,
            dir,
        }
    }
}

struct NodeProcess {
    child: Child,
    port: u16,
    log: PathBuf,
    keep_as: Option<PathBuf>,
}

impl Drop for NodeProcess {
    fn drop(&mut self) {
        let _ = self.child.kill();
        let _ = self.child.wait();
        if let Some(keep) = &self.keep_as {
            let _ = std::fs::copy(&self.log, keep);
        }
    }
}

/// Where the driver submits each signed transfer.
#[derive(Clone, Copy)]
enum Submission {
    /// To one validator per sender (`sender % 6`), as a wallet would.
    OneValidator,
    /// To all six validators, so every mempool holds every envelope.
    AllValidators,
}

impl Submission {
    fn describe(self) -> &'static str {
        match self {
            Submission::OneValidator => "one validator per sender",
            Submission::AllValidators => "all six validators",
        }
    }
}

struct Phase {
    name: &'static str,
    rule: &'static str,
    /// Blocks after the recovery transition at which settlement activates.
    fee_settlement_offset: Option<u64>,
    submission: Submission,
    /// How many funded senders keep a transfer in flight.
    senders: usize,
    rpc_base: u16,
    p2p_base: u16,
}

fn spawn_node(
    network: &Network,
    phase: &Phase,
    index: usize,
    activation: Option<u64>,
    log_dir: Option<&Path>,
) -> NodeProcess {
    let port = phase.rpc_base + index as u16;
    let data_dir = network
        .dir
        .path()
        .join(phase.name)
        .join(format!("node-{index}"));
    std::fs::create_dir_all(&data_dir).unwrap();
    let peers: Vec<String> = (0..VALIDATORS)
        .filter(|peer| *peer != index)
        .map(|peer| format!("127.0.0.1:{}", phase.p2p_base + peer as u16))
        .collect();
    let log_path = data_dir.join("node.log");
    let log = std::fs::File::create(&log_path).unwrap();
    let mut command = Command::new(env!("CARGO_BIN_EXE_arc-node"));
    command
        .args([
            "--rpc",
            &format!("127.0.0.1:{port}"),
            "--p2p-port",
            &(phase.p2p_base + index as u16).to_string(),
            "--data-dir",
            data_dir.to_str().unwrap(),
            "--genesis",
            network.genesis.to_str().unwrap(),
            "--peers",
            &peers.join(","),
            "--stake",
            &STAKES[index].to_string(),
            "--native-inference-activation",
            network.activation.to_str().unwrap(),
        ])
        .arg("--validator-key-file")
        .arg(
            network
                .dir
                .path()
                .join(format!("validator-{index}.key.json")),
        )
        .arg("--recovery-checkpoint")
        .arg(&network.checkpoint)
        .arg("--approved-recovery-manifest-hash")
        .arg(network.manifest.to_hex());
    if let Some(height) = activation {
        command.args([
            "--test-v3-fee-settlement-activation-height",
            &height.to_string(),
        ]);
    }
    let child = command
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
        log: log_path,
        keep_as: log_dir.map(|dir| dir.join(format!("{}-node-{index}.log", phase.name))),
    }
}

/// One signed transfer envelope waiting for a canonical block.
struct InFlight {
    sender: usize,
    request: Value,
    last_submitted: Instant,
    first_submitted: Instant,
    accepted_once: bool,
}

/// Keeps exactly one transfer in flight per active sender (ingress accepts
/// only the next nonce), with a fresh recipient for every transfer, posted to
/// the validators' batch endpoints as the phase's `Submission` says.
struct LoadDriver<'a> {
    network: &'a Network,
    ports: Vec<u16>,
    submission: Submission,
    active_senders: usize,
    public_keys: Vec<String>,
    next_nonce: Vec<u64>,
    busy: Vec<bool>,
    in_flight: HashMap<Hash256, InFlight>,
    scanned_height: u64,
    blocks: Vec<(u64, u32, u64)>,
    submitted: u64,
    resubmitted: u64,
    accepted: u64,
    confirmed: u64,
    confirmation_ms: Vec<u64>,
}

impl<'a> LoadDriver<'a> {
    fn new(
        network: &'a Network,
        ports: Vec<u16>,
        scanned_height: u64,
        submission: Submission,
        active_senders: usize,
    ) -> Self {
        let senders = network.senders.len();
        Self {
            network,
            ports,
            submission,
            active_senders: active_senders.min(senders),
            public_keys: network
                .senders
                .iter()
                .map(|key| hex::encode(key.public_key_bytes()))
                .collect(),
            next_nonce: vec![0; senders],
            busy: vec![false; senders],
            in_flight: HashMap::new(),
            scanned_height,
            blocks: Vec::new(),
            submitted: 0,
            resubmitted: 0,
            accepted: 0,
            confirmed: 0,
            confirmation_ms: Vec::new(),
        }
    }

    fn signed_request(&self, sender: usize) -> (Hash256, Value) {
        let nonce = self.next_nonce[sender];
        let recipient = hash_bytes(format!("v3-throughput-recipient-{sender}-{nonce}").as_bytes());
        let mut tx = Transaction::new_transfer(
            self.network.sender_addresses[sender],
            recipient,
            TRANSFER_AMOUNT,
            nonce,
        );
        tx.fee = TRANSFER_FEE;
        tx.sign_in_domain(
            &self.network.senders[sender],
            &self.network.transaction_domain,
        )
        .unwrap();
        let arc_crypto::Signature::Ed25519 { signature, .. } = &tx.signature else {
            panic!("Ed25519 senders sign with Ed25519");
        };
        let request = json!({
            "from": self.network.sender_addresses[sender].to_hex(),
            "to": recipient.to_hex(),
            "amount": TRANSFER_AMOUNT,
            "nonce": nonce,
            "fee": TRANSFER_FEE,
            "signature": hex::encode(signature),
            "public_key": self.public_keys[sender],
        });
        (tx.hash, request)
    }

    /// The validators a sender's envelopes go to.
    fn targets(&self, sender: usize) -> Vec<usize> {
        match self.submission {
            Submission::OneValidator => vec![sender % self.ports.len()],
            Submission::AllValidators => (0..self.ports.len()).collect(),
        }
    }

    /// Sign a transfer for every idle active sender, resubmit stale
    /// envelopes, and post each validator its share in `SUBMIT_BATCH`-sized
    /// batches, all validators at once.
    fn submit_wave(&mut self) {
        let now = Instant::now();
        let mut outgoing: Vec<(Hash256, usize, Value)> = Vec::new();
        for sender in 0..self.active_senders {
            if self.busy[sender] {
                continue;
            }
            let (hash, request) = self.signed_request(sender);
            outgoing.push((hash, sender, request.clone()));
            self.busy[sender] = true;
            self.submitted += 1;
            self.in_flight.insert(
                hash,
                InFlight {
                    sender,
                    request,
                    last_submitted: now,
                    first_submitted: now,
                    accepted_once: false,
                },
            );
        }
        for (hash, entry) in self.in_flight.iter_mut() {
            if now.duration_since(entry.last_submitted) >= RESUBMIT_AFTER {
                outgoing.push((*hash, entry.sender, entry.request.clone()));
                entry.last_submitted = now;
                self.resubmitted += 1;
            }
        }
        if outgoing.is_empty() {
            return;
        }
        let mut per_validator: Vec<Vec<Value>> = vec![Vec::new(); self.ports.len()];
        for (_, sender, request) in &outgoing {
            for target in self.targets(*sender) {
                per_validator[target].push(request.clone());
            }
        }
        let bodies: Vec<Vec<String>> = per_validator
            .iter()
            .map(|requests| {
                requests
                    .chunks(SUBMIT_BATCH)
                    .map(|chunk| json!({ "transactions": chunk }).to_string())
                    .collect()
            })
            .collect();
        let accepted_hashes: Vec<String> = std::thread::scope(|scope| {
            let handles: Vec<_> = self
                .ports
                .iter()
                .zip(&bodies)
                .map(|(port, bodies)| {
                    let port = *port;
                    scope.spawn(move || {
                        let mut accepted = Vec::new();
                        for body in bodies {
                            let (code, response) = post_json(port, "/tx/submit_batch", body);
                            if code != 200 {
                                continue;
                            }
                            if let Some(hashes) = serde_json::from_str::<Value>(&response)
                                .ok()
                                .and_then(|value| value["tx_hashes"].as_array().cloned())
                            {
                                accepted.extend(
                                    hashes
                                        .iter()
                                        .filter_map(|hash| hash.as_str().map(str::to_owned)),
                                );
                            }
                        }
                        accepted
                    })
                })
                .collect();
            handles
                .into_iter()
                .flat_map(|handle| handle.join().unwrap_or_default())
                .collect()
        });
        self.accepted += accepted_hashes.len() as u64;
        let accepted: std::collections::HashSet<String> = accepted_hashes.into_iter().collect();
        for (hash, _, _) in &outgoing {
            if let Some(entry) = self.in_flight.get_mut(hash) {
                if accepted.contains(&hash.to_hex()) {
                    entry.accepted_once = true;
                } else if !entry.accepted_once {
                    // Nobody took it yet: try again shortly, not after the
                    // full resubmission interval.
                    entry.last_submitted = now
                        .checked_sub(RESUBMIT_AFTER - RETRY_UNACCEPTED_AFTER)
                        .unwrap_or(now);
                }
            }
        }
    }

    /// Read every new canonical block from the first validator and release
    /// the senders whose transfer it carries.
    fn scan_blocks(&mut self) {
        let Some(tip) = height(self.ports[0]) else {
            return;
        };
        while self.scanned_height < tip {
            let next = self.scanned_height + 1;
            let Some(value) = get_json(self.ports[0], &format!("/block/{next}")) else {
                return;
            };
            let block: arc_types::Block =
                serde_json::from_value(value).expect("block JSON decodes");
            let now = Instant::now();
            for hash in &block.tx_hashes {
                if let Some(entry) = self.in_flight.remove(hash) {
                    self.busy[entry.sender] = false;
                    self.next_nonce[entry.sender] += 1;
                    self.confirmed += 1;
                    self.confirmation_ms
                        .push(now.duration_since(entry.first_submitted).as_millis() as u64);
                }
            }
            self.blocks.push((
                block.header.height,
                block.header.tx_count,
                block.header.timestamp,
            ));
            self.scanned_height = next;
        }
    }

    fn run_until(&mut self, deadline: Instant) {
        while Instant::now() < deadline {
            let wave_start = Instant::now();
            self.scan_blocks();
            self.submit_wave();
            self.scan_blocks();
            let elapsed = wave_start.elapsed();
            if elapsed < WAVE_INTERVAL {
                std::thread::sleep(WAVE_INTERVAL - elapsed);
            }
        }
    }
}

fn diagnostics_snapshot(ports: &[u16]) -> Vec<Value> {
    ports
        .iter()
        .map(|port| get_json(*port, "/consensus/diagnostics").unwrap_or(Value::Null))
        .collect()
}

fn counter(snapshot: &Value, key: &str) -> u64 {
    snapshot[key].as_u64().unwrap_or(0)
}

/// Per validator and on average: how much of each round every phase timer
/// took, next to the event counts, over one measurement window.
fn round_breakdown(start: &[Value], end: &[Value], wall_seconds: f64) -> Value {
    let mut per_validator = Vec::new();
    for (start, end) in start.iter().zip(end) {
        let rounds =
            counter(end, "rounds_advanced").saturating_sub(counter(start, "rounds_advanced"));
        let mut timers = serde_json::Map::new();
        for key in TIMER_KEYS {
            let micros = counter(end, key).saturating_sub(counter(start, key)) as f64;
            timers.insert(
                (*key).to_string(),
                json!({
                    "total_ms": micros / 1_000.0,
                    "per_round_ms": if rounds == 0 { 0.0 } else { micros / 1_000.0 / rounds as f64 },
                    "share_of_wall": micros / 1_000_000.0 / wall_seconds,
                }),
            );
        }
        let mut counts = serde_json::Map::new();
        for key in COUNT_KEYS {
            counts.insert(
                (*key).to_string(),
                json!(counter(end, key).saturating_sub(counter(start, key))),
            );
        }
        per_validator.push(json!({ "rounds": rounds, "timers": timers, "counts": counts }));
    }
    let validators = per_validator.len().max(1) as f64;
    let mut mean_per_round_ms = serde_json::Map::new();
    for key in TIMER_KEYS {
        let total: f64 = per_validator
            .iter()
            .map(|validator| {
                validator["timers"][*key]["per_round_ms"]
                    .as_f64()
                    .unwrap_or(0.0)
            })
            .sum();
        mean_per_round_ms.insert((*key).to_string(), json!(total / validators));
    }
    let mean_rounds: f64 = per_validator
        .iter()
        .map(|validator| validator["rounds"].as_u64().unwrap_or(0) as f64)
        .sum::<f64>()
        / validators;
    json!({
        "rounds_per_second": mean_rounds / wall_seconds,
        "mean_per_round_ms": mean_per_round_ms,
        "per_validator": per_validator,
    })
}

/// User plus system CPU seconds a process has used, from `/proc` (Linux).
fn cpu_seconds(pid: u32) -> Option<f64> {
    let stat = std::fs::read_to_string(format!("/proc/{pid}/stat")).ok()?;
    // After the parenthesised command name: state is field 3, utime 14 and
    // stime 15 (1-based), so utime and stime are the 12th and 13th here.
    let fields: Vec<&str> = stat.rsplit_once(')')?.1.split_whitespace().collect();
    let utime: f64 = fields.get(11)?.parse().ok()?;
    let stime: f64 = fields.get(12)?.parse().ok()?;
    Some((utime + stime) / USER_HZ)
}

fn percentile(sorted: &[u64], fraction: f64) -> u64 {
    if sorted.is_empty() {
        return 0;
    }
    let rank = ((sorted.len() - 1) as f64 * fraction).round() as usize;
    sorted[rank.min(sorted.len() - 1)]
}

fn distribution(values: &[u64]) -> Value {
    let mut sorted = values.to_vec();
    sorted.sort_unstable();
    let total: u64 = sorted.iter().sum();
    json!({
        "count": sorted.len(),
        "min": sorted.first().copied().unwrap_or(0),
        "max": sorted.last().copied().unwrap_or(0),
        "mean": if sorted.is_empty() { 0.0 } else { total as f64 / sorted.len() as f64 },
        "p50": percentile(&sorted, 0.50),
        "p90": percentile(&sorted, 0.90),
        "p99": percentile(&sorted, 0.99),
    })
}

fn run_phase(
    network: &Network,
    phase: &Phase,
    warmup: Duration,
    measure: Duration,
    log_dir: Option<&Path>,
) -> Value {
    let activation = phase
        .fee_settlement_offset
        .map(|offset| network.transition_height + offset);
    let nodes: Vec<NodeProcess> = (0..VALIDATORS)
        .map(|index| spawn_node(network, phase, index, activation, log_dir))
        .collect();
    let ports: Vec<u16> = nodes.iter().map(|node| node.port).collect();
    let pids: Vec<u32> = nodes.iter().map(|node| node.child.id()).collect();
    eprintln!("[{}] waiting for six connected validators", phase.name);
    wait_for(
        Duration::from_secs(120),
        "six recovered validators fully connected",
        || {
            ports.iter().all(|port| {
                get_json(*port, "/health").and_then(|health| health["peers"].as_u64()) == Some(5)
            })
        },
    );
    for port in &ports {
        let info = get_json(*port, "/network/info").expect("network info");
        assert_eq!(
            info["v3_fee_settlement_activation_height"].as_u64(),
            activation,
            "validator {port} reports the phase's activation"
        );
    }
    let warm_height = network.transition_height + WARM_HEIGHT_OFFSET;
    eprintln!("[{}] waiting for height {warm_height}", phase.name);
    wait_for(
        Duration::from_secs(180),
        "every validator past native and fee-settlement activation",
        || {
            ports
                .iter()
                .all(|port| height(*port).is_some_and(|tip| tip >= warm_height))
        },
    );

    let scan_from = height(ports[0]).expect("first validator height");
    let mut driver = LoadDriver::new(
        network,
        ports.clone(),
        scan_from,
        phase.submission,
        phase.senders,
    );
    eprintln!(
        "[{}] warm-up load for {warmup:?}: {} senders, submitted to {}",
        phase.name,
        driver.active_senders,
        phase.submission.describe()
    );
    driver.run_until(Instant::now() + warmup);
    let start_height = driver.scanned_height;
    let start_diagnostics = diagnostics_snapshot(&ports);
    let start_cpu: Vec<Option<f64>> = pids.iter().map(|pid| cpu_seconds(*pid)).collect();
    let measure_start = Instant::now();
    eprintln!(
        "[{}] measuring for {measure:?} from height {start_height}",
        phase.name
    );
    driver.run_until(measure_start + measure);
    let wall_seconds = measure_start.elapsed().as_secs_f64();
    let end_cpu: Vec<Option<f64>> = pids.iter().map(|pid| cpu_seconds(*pid)).collect();
    let end_diagnostics = diagnostics_snapshot(&ports);
    let end_height = driver.scanned_height;

    // Every replica must agree on the measured history.
    wait_for(
        Duration::from_secs(60),
        "every validator executed the measured blocks",
        || {
            ports
                .iter()
                .all(|port| height(*port).is_some_and(|tip| tip >= end_height))
        },
    );
    let reference = get_json(ports[0], &format!("/block/{end_height}")).expect("last block");
    for port in &ports[1..] {
        assert_eq!(
            get_json(*port, &format!("/block/{end_height}")).expect("last block"),
            reference,
            "validators disagree on block {end_height}"
        );
    }
    // No new load from here on. Once the envelopes still in flight have
    // landed, the fee treasury must hold exactly the fees of every committed
    // transfer, under either rule.
    let treasury_path = format!("/account/{}", arc_state::v3_fee_treasury_address().to_hex());
    let mut treasury: u64 = 0;
    let mut committed_transfers: u64 = 0;
    wait_for(
        Duration::from_secs(90),
        "the fee treasury equals the fees of every committed transfer",
        || {
            driver.scan_blocks();
            committed_transfers = driver
                .blocks
                .iter()
                .map(|(_, tx_count, _)| u64::from(*tx_count))
                .sum::<u64>();
            treasury = get_json(ports[0], &treasury_path)
                .and_then(|account| account["balance"].as_u64())
                .unwrap_or(0);
            treasury == committed_transfers * TRANSFER_FEE
        },
    );

    let measured: Vec<(u64, u32, u64)> = driver
        .blocks
        .iter()
        .copied()
        .filter(|(block_height, _, _)| *block_height > start_height && *block_height <= end_height)
        .collect();
    let start_timestamp = driver
        .blocks
        .iter()
        .find(|(block_height, _, _)| *block_height == start_height)
        .map(|(_, _, timestamp)| *timestamp);
    let end_timestamp = measured.last().map(|(_, _, timestamp)| *timestamp);
    let block_seconds = match (start_timestamp, end_timestamp) {
        (Some(start), Some(end)) if end > start => (end - start) as f64 / 1_000.0,
        _ => wall_seconds,
    };
    let transactions: u64 = measured
        .iter()
        .map(|(_, tx_count, _)| u64::from(*tx_count))
        .sum();
    let tx_counts: Vec<u64> = measured
        .iter()
        .map(|(_, tx_count, _)| u64::from(*tx_count))
        .collect();
    let per_node_execute_us: Vec<f64> = start_diagnostics
        .iter()
        .zip(&end_diagnostics)
        .map(|(start, end)| {
            let blocks = counter(end, "canonical_blocks_produced")
                .saturating_sub(counter(start, "canonical_blocks_produced"));
            let micros = counter(end, "commit_execute_us")
                .saturating_sub(counter(start, "commit_execute_us"));
            if blocks == 0 {
                0.0
            } else {
                micros as f64 / blocks as f64
            }
        })
        .collect();
    let mean_execute_us =
        per_node_execute_us.iter().sum::<f64>() / per_node_execute_us.len().max(1) as f64;
    let breakdown = round_breakdown(&start_diagnostics, &end_diagnostics, wall_seconds);
    let rounds_per_second = breakdown["rounds_per_second"].as_f64().unwrap_or(0.0);
    let cpu_per_validator: Vec<Option<f64>> = start_cpu
        .iter()
        .zip(&end_cpu)
        .map(|(start, end)| start.zip(*end).map(|(start, end)| end - start))
        .collect();
    let cpu_total: f64 = cpu_per_validator.iter().flatten().sum();
    let mean_rounds = rounds_per_second * wall_seconds;
    let cpu_ms_per_round_per_validator = if mean_rounds > 0.0 {
        cpu_total / cpu_per_validator.len().max(1) as f64 / mean_rounds * 1_000.0
    } else {
        0.0
    };
    let max_tx_count = tx_counts.iter().copied().max().unwrap_or(0);
    let tps = transactions as f64 / block_seconds;
    eprintln!(
        "[{}] {} blocks, {transactions} transfers in {block_seconds:.1} s of block time: \
         {tps:.1} TPS, max {max_tx_count} per block, {rounds_per_second:.2} rounds/s, \
         {mean_execute_us:.0} us execute per block, {:.2} CPU-s per wall-s across validators",
        phase.name,
        measured.len(),
        cpu_total / wall_seconds,
    );
    drop(nodes);

    json!({
        "rule": phase.rule,
        "submission": phase.submission.describe(),
        "active_senders": driver.active_senders,
        "fee_settlement_activation_height": activation,
        "measured_heights": [start_height, end_height],
        "blocks": measured.len(),
        "transactions": transactions,
        "block_time_seconds": block_seconds,
        "wall_seconds": wall_seconds,
        "tps": tps,
        "tps_wall_clock": transactions as f64 / wall_seconds,
        "blocks_per_second": measured.len() as f64 / block_seconds,
        "rounds_per_second": rounds_per_second,
        "tx_count_per_block": distribution(&tx_counts),
        "tx_count_series": measured
            .iter()
            .map(|(block_height, tx_count, _)| json!([block_height, tx_count]))
            .collect::<Vec<_>>(),
        "commit_execute_us_per_block": {
            "mean_over_validators": mean_execute_us,
            "per_validator": per_node_execute_us,
        },
        "round_breakdown": breakdown,
        "cpu": {
            "per_validator_seconds": cpu_per_validator,
            "cpu_seconds_per_wall_second": cpu_total / wall_seconds,
            "ms_per_round_per_validator": cpu_ms_per_round_per_validator,
            "available_parallelism": std::thread::available_parallelism()
                .map(|cpus| cpus.get())
                .unwrap_or(0),
        },
        "driver": {
            "signed": driver.submitted,
            "resubmitted": driver.resubmitted,
            "accepted_by_validators": driver.accepted,
            "confirmed": driver.confirmed,
            "confirmation_ms": distribution(&driver.confirmation_ms),
        },
        "fee_treasury_balance": treasury,
        "committed_transfers_total": committed_transfers,
    })
}

#[test]
fn v3_transfer_throughput_launch_rule_vs_block_settlement() {
    let senders = env_number("ARC_V3_THROUGHPUT_SENDERS", 800) as usize;
    let low_load_senders = env_number("ARC_V3_THROUGHPUT_LOW_LOAD_SENDERS", 100) as usize;
    let warmup = Duration::from_secs(env_number("ARC_V3_THROUGHPUT_WARMUP_SECONDS", 15));
    let measure = Duration::from_secs(env_number("ARC_V3_THROUGHPUT_MEASURE_SECONDS", 60));
    let log_dir = std::env::var_os("ARC_V3_THROUGHPUT_LOG_DIR").map(PathBuf::from);
    if let Some(dir) = &log_dir {
        std::fs::create_dir_all(dir).unwrap();
    }
    let network = Network::build(senders.max(low_load_senders));
    let settlement = "block-level settlement via the harness override";
    let phases = [
        Phase {
            name: "phase-a",
            rule: "launch rule: every transfer credits the fee treasury itself",
            fee_settlement_offset: None,
            submission: Submission::OneValidator,
            senders,
            rpc_base: 9701,
            p2p_base: 9601,
        },
        Phase {
            name: "phase-b",
            rule: settlement,
            fee_settlement_offset: Some(FEE_SETTLEMENT_OFFSET),
            submission: Submission::OneValidator,
            senders,
            rpc_base: 9721,
            p2p_base: 9621,
        },
        Phase {
            name: "phase-b-fanout",
            rule: settlement,
            fee_settlement_offset: Some(FEE_SETTLEMENT_OFFSET),
            submission: Submission::AllValidators,
            senders,
            rpc_base: 9741,
            p2p_base: 9641,
        },
        Phase {
            name: "phase-b-low-load",
            rule: settlement,
            fee_settlement_offset: Some(FEE_SETTLEMENT_OFFSET),
            submission: Submission::OneValidator,
            senders: low_load_senders,
            rpc_base: 9761,
            p2p_base: 9661,
        },
    ];
    let results: Vec<Value> = phases
        .iter()
        .map(|phase| run_phase(&network, phase, warmup, measure, log_dir.as_deref()))
        .collect();
    let (a, b, fanout, low_load) = (&results[0], &results[1], &results[2], &results[3]);
    let ratio = |numerator: &Value, denominator: &Value, key: &str| {
        numerator[key].as_f64().unwrap_or(0.0)
            / denominator[key]
                .as_f64()
                .unwrap_or(0.0)
                .max(f64::MIN_POSITIVE)
    };

    let report = json!({
        "schema": "arc.v3-transfer-throughput.v2",
        "commit": std::env::var("GITHUB_SHA").ok(),
        "run_id": std::env::var("GITHUB_RUN_ID").ok(),
        "available_parallelism": std::thread::available_parallelism()
            .map(|cpus| cpus.get())
            .unwrap_or(0),
        "config": {
            "validators": VALIDATORS,
            "senders": senders,
            "low_load_senders": low_load_senders,
            "warmup_seconds": warmup.as_secs(),
            "measure_seconds": measure.as_secs(),
            "transfer_amount": TRANSFER_AMOUNT,
            "transfer_fee": TRANSFER_FEE,
            "submission": "one in-flight transfer per active sender, fresh recipient per \
                           transfer, /tx/submit_batch (64 per request)",
            "execution": "migrated recovered-v3 chain (native binding active, ingress closed): \
                          sequential executor",
        },
        "phase_a": a,
        "phase_b": b,
        "variants": {
            "phase_b_fanout": fanout,
            "phase_b_low_load": low_load,
        },
        "comparison": {
            "tps_ratio_b_over_a": ratio(b, a, "tps"),
            "rounds_per_second_ratio_b_over_a": ratio(b, a, "rounds_per_second"),
            "max_tx_count_a": a["tx_count_per_block"]["max"],
            "max_tx_count_b": b["tx_count_per_block"]["max"],
        },
        "note": "GitHub runner numbers are only meaningful relative to each other; all six \
                 validators share one runner's CPUs.",
    });
    let rendered = serde_json::to_string_pretty(&report).unwrap();
    println!("{rendered}");
    if let Some(path) = std::env::var_os("ARC_V3_THROUGHPUT_REPORT") {
        let path = PathBuf::from(path);
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent).unwrap();
        }
        std::fs::write(&path, &rendered).unwrap();
    }

    // Functional expectations, independent of runner speed.
    for (label, phase) in [
        ("phase A", a),
        ("phase B", b),
        ("phase B fan-out", fanout),
        ("phase B low load", low_load),
    ] {
        assert!(
            phase["transactions"].as_u64().unwrap_or(0) > 0,
            "{label} committed transfers"
        );
    }
    assert!(
        a["tx_count_per_block"]["max"].as_u64().unwrap_or(u64::MAX) <= 1,
        "the launch rule keeps one transfer per canonical block"
    );
    assert!(
        b["tx_count_per_block"]["max"].as_u64().unwrap_or(0) > 1,
        "block-level settlement packs disjoint transfers into one block"
    );
}
