//! Offered native-inference load for the recorded soak (checklist R8).
//!
//!     soak_native_load --rpc 9960,9961,9962,9963 --rate 0.2 --duration 600 \
//!                      --out workload.jsonl --requester-seed soak --tuple m,p,g,a
//!
//! Offers signed native-inference requests at a fixed rate, round-robin across
//! the given nodes, and follows each one to a terminal receipt on ANY replica.
//! Every offered item is written to `--out` as one JSON line the soak analyzer
//! accounts for: finalized / refunded / rejected-with-reason / submit_error /
//! still pending at the drained end, with latency, attempts and the node that
//! accepted it.
//!
//! Ingress admits exactly the nonce that is valid at the next height, so a
//! requester cannot queue a second request behind a first that is not yet
//! included. The driver therefore runs `--requesters` independent requester
//! accounts, each with at most one request in flight (submitted until it
//! reaches a terminal receipt). An offer that finds every requester busy is
//! counted as backpressure, not silently dropped. A request that no replica
//! has included after `--lost-after` seconds, while its sender's nonce has
//! not moved on any node, is presumed lost: its requester resynchronises its
//! nonce from the chain and carries on, and the item stays on a watch list
//! so a late inclusion is still accounted for at the end.
//!
//! Requests are built exactly as the protocol tests build them, from the same
//! types - a Python reimplementation of the signed, bincode-framed envelope
//! would be a second definition of the wire format to keep in step.
//!
//! This drives the protocol and settlement path. With the deterministic test
//! executor it qualifies nothing about inference quality; that is stated in
//! every record's `executor` field so the evidence cannot be mistaken for a
//! real-model result.

use arc_crypto::signature::KeyPair;
use arc_crypto::{Hash256, hash_bytes};
use arc_types::inference_contract::{
    INFERENCE_CONTRACT_VERSION, InferenceDomain, InferenceJob, InferenceRequest,
};
use arc_types::transaction::{NativeInferenceRequestBody, Transaction, TxBody};
use std::io::Write;
use std::process::Command;
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

fn arg(name: &str) -> Option<String> {
    let mut args = std::env::args();
    while let Some(a) = args.next() {
        if a == name {
            return args.next();
        }
    }
    None
}

fn now_s() -> f64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap()
        .as_secs_f64()
}

fn curl(args: &[&str]) -> (u32, String) {
    let out = match Command::new("curl")
        .args(["-s", "-w", "\n%{http_code}", "--max-time", "5"])
        .args(args)
        .output()
    {
        Ok(out) => out,
        Err(_) => return (0, String::new()),
    };
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

fn requester_keypair(seed: &str, slot: usize) -> KeyPair {
    // Slot 0 keeps the single-requester derivation, so older runs' funded
    // address is still slot 0's.
    let material = if slot == 0 {
        seed.to_string()
    } else {
        format!("{seed}/{slot}")
    };
    let bytes = blake3::derive_key("ARC-soak-native-load-requester-v1", material.as_bytes());
    KeyPair::Ed25519(ed25519_dalek::SigningKey::from_bytes(&bytes))
}

/// One offered request, from first offer to its terminal record.
struct Item {
    record: serde_json::Map<String, serde_json::Value>,
    request_id: Hash256,
    nonce: u64,
    accepted_t: f64,
}

enum Slot {
    Idle,
    InFlight(Item),
}

struct Requester {
    key: KeyPair,
    next_nonce: u64,
    slot: Slot,
}

struct Driver {
    ports: Vec<u16>,
    domain: InferenceDomain,
    tuple: Vec<Hash256>,
    executor: String,
    out: std::fs::File,
    lost_after: f64,
    requesters: Vec<Requester>,
    /// Presumed lost; still polled so a late inclusion is accounted for.
    watch: Vec<Item>,
    turn: usize,
    offered: u64,
    backpressured: u64,
}

impl Driver {
    fn write(&mut self, record: &serde_json::Map<String, serde_json::Value>) {
        let _ = writeln!(self.out, "{}", serde_json::Value::Object(record.clone()));
        let _ = self.out.flush();
    }

    /// The highest nonce any replica reports for `address` (a lagging node's
    /// view is behind, never ahead).
    fn chain_nonce(&self, address: &Hash256) -> Option<u64> {
        self.ports
            .iter()
            .filter_map(|p| get_json(*p, &format!("/account/{}", address.to_hex())))
            .filter_map(|a| a["nonce"].as_u64())
            .max()
    }

    fn receipt_status(&self, request_id: &Hash256) -> Option<String> {
        let path = format!("/native-inference/receipt/{}", request_id.to_hex());
        let mut seen: Option<String> = None;
        for port in &self.ports {
            if let Some(status) =
                get_json(*port, &path).and_then(|r| r["observed_status"].as_str().map(str::to_string))
            {
                // A terminal status on any replica wins over "Pending".
                if status != "Pending" {
                    return Some(status);
                }
                seen = Some(status);
            }
        }
        seen
    }

    fn build(&self, requester: &KeyPair, nonce: u64, height: u64) -> (Transaction, Hash256) {
        let input = format!("soak native input {}/{nonce}", requester.address().to_hex()).into_bytes();
        let job = InferenceJob {
            version: INFERENCE_CONTRACT_VERSION,
            domain: self.domain,
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
            expires_at: height + 2_000,
        };
        let request_id = job.request_id();
        let request = InferenceRequest::sign(job, requester).expect("sign job");
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
        tx.signature = requester.sign(&tx.hash).expect("sign tx");
        (tx, request_id)
    }

    /// Offer one request from an idle requester. Every node is tried once,
    /// starting from the round-robin turn: a node that is down, not ready, or
    /// has not yet applied the block that made this nonce current is a reason
    /// to try the next node, not a verdict on the request.
    fn offer(&mut self) {
        self.offered += 1;
        let Some(slot) = (0..self.requesters.len())
            .map(|i| (self.turn + i) % self.requesters.len())
            .find(|i| matches!(self.requesters[*i].slot, Slot::Idle))
        else {
            self.backpressured += 1;
            return;
        };
        let first_node = self.turn % self.ports.len();
        self.turn += 1;
        let key = self.requesters[slot].key.clone();
        let mut nonce = self.requesters[slot].next_nonce;
        let mut record = serde_json::Map::new();
        record.insert("id".into(), format!("native-{}-{slot}-{nonce}", self.offered).into());
        record.insert("kind".into(), "native_inference".into());
        record.insert("executor".into(), self.executor.clone().into());
        record.insert("requester_slot".into(), (slot as u64).into());
        record.insert("submitted_t".into(), now_s().into());
        record.insert("submitted_to".into(), (first_node as u64).into());
        let mut reasons: Vec<String> = Vec::new();
        let mut resynced = false;
        let mut attempts = 0u64;
        let mut answered = false;
        let mut i = 0usize;
        while i < self.ports.len() {
            let node = (first_node + i) % self.ports.len();
            let port = self.ports[node];
            let height = get_json(port, "/health")
                .and_then(|h| h["height"].as_u64())
                .unwrap_or(0);
            let (tx, request_id) = self.build(&key, nonce, height);
            let payload = serde_json::to_string(&tx).unwrap();
            attempts += 1;
            let (code, body) = curl(&[
                "-X",
                "POST",
                "-H",
                "Content-Type: application/json",
                "-d",
                &payload,
                &format!("http://127.0.0.1:{port}/tx/submit_signed"),
            ]);
            if code != 0 {
                answered = true;
            }
            if code == 200 {
                let accepted_t = now_s();
                record.insert("request_id".into(), request_id.to_hex().into());
                record.insert("nonce".into(), nonce.into());
                record.insert("accepted_by".into(), (node as u64).into());
                record.insert("accepted_t".into(), accepted_t.into());
                record.insert("attempts".into(), attempts.into());
                self.requesters[slot].slot = Slot::InFlight(Item {
                    record,
                    request_id,
                    nonce,
                    accepted_t,
                });
                return;
            }
            let reason = if code == 0 {
                format!("node {node}: no HTTP response")
            } else {
                format!("node {node}: HTTP {code}: {body:.160}")
            };
            // A nonce refusal on the first node that answers may mean this
            // driver's view is stale (a request it presumed lost was included
            // after all): resynchronise once from the chain and retry here.
            if code == 400 && body.contains("nonce") && !resynced {
                resynced = true;
                if let Some(chain) = self.chain_nonce(&key.address())
                    && chain != nonce
                {
                    reasons.push(format!("{reason} (resynced nonce {nonce} -> {chain})"));
                    nonce = chain;
                    self.requesters[slot].next_nonce = chain;
                    continue;
                }
            }
            reasons.push(reason);
            i += 1;
        }
        record.insert("attempts".into(), attempts.into());
        record.insert("reason".into(), reasons.join("; ").into());
        if answered {
            record.insert("final_status".into(), "rejected".into());
        } else {
            // No node answered at all: nothing was accepted and the nonce was
            // not consumed.
            record.insert("final_status".into(), "submit_error".into());
        }
        self.write(&record);
    }

    fn settle(&mut self, mut item: Item, status: &str) {
        let settled = now_s();
        item.record.insert("final_status".into(), status.into());
        item.record.insert("settled_t".into(), settled.into());
        let submitted = item.record["submitted_t"].as_f64().unwrap_or(settled);
        item.record.insert("latency_s".into(), (settled - submitted).into());
        self.write(&item.record);
    }

    /// Follow every in-flight request; free its requester when it settles.
    fn poll(&mut self) {
        for slot in 0..self.requesters.len() {
            let Slot::InFlight(item) = &self.requesters[slot].slot else {
                continue;
            };
            let request_id = item.request_id;
            let nonce = item.nonce;
            let age = now_s() - item.accepted_t;
            let status = self.receipt_status(&request_id);
            let terminal = match status.as_deref() {
                Some("Finalized") => Some("finalized"),
                Some("Refunded") => Some("refunded"),
                _ => None,
            };
            if let Some(final_status) = terminal {
                let Slot::InFlight(item) = std::mem::replace(&mut self.requesters[slot].slot, Slot::Idle)
                else {
                    unreachable!()
                };
                self.requesters[slot].next_nonce = nonce + 1;
                self.settle(item, final_status);
                continue;
            }
            if status.is_some() || age < self.lost_after {
                // Included (Pending) or still young: keep waiting.
                continue;
            }
            // Old, and no replica has even included it. If the sender's nonce
            // has not moved anywhere, the request is gone (typically held only
            // by a node that was killed before gossiping it).
            let address = self.requesters[slot].key.address();
            match self.chain_nonce(&address) {
                Some(chain) if chain <= nonce => {
                    let Slot::InFlight(mut item) =
                        std::mem::replace(&mut self.requesters[slot].slot, Slot::Idle)
                    else {
                        unreachable!()
                    };
                    item.record.insert("presumed_lost_t".into(), now_s().into());
                    self.requesters[slot].next_nonce = chain;
                    self.watch.push(item);
                }
                // Included after all (the receipt is just slow to appear), or
                // no node answered: keep waiting.
                _ => {}
            }
        }
        // A presumed-lost request that turns up after all is accounted for.
        let mut still = Vec::new();
        for item in std::mem::take(&mut self.watch) {
            match self.receipt_status(&item.request_id).as_deref() {
                Some("Finalized") => self.settle(item, "finalized"),
                Some("Refunded") => self.settle(item, "refunded"),
                _ => still.push(item),
            }
        }
        self.watch = still;
    }

    fn busy(&self) -> bool {
        self.requesters
            .iter()
            .any(|r| matches!(r.slot, Slot::InFlight(_)))
    }

    fn finish(&mut self) {
        let mut left: Vec<Item> = std::mem::take(&mut self.watch);
        for requester in &mut self.requesters {
            if let Slot::InFlight(item) = std::mem::replace(&mut requester.slot, Slot::Idle) {
                left.push(item);
            }
        }
        for mut item in left {
            item.record.insert("final_status".into(), "pending".into());
            self.write(&item.record);
        }
        let summary = serde_json::json!({
            "offered": self.offered,
            "backpressured": self.backpressured,
            "requesters": self.requesters.len(),
        });
        eprintln!("load driver summary: {summary}");
    }
}

fn main() {
    let seed = arg("--requester-seed").unwrap_or_else(|| "soak".into());
    let count: usize = arg("--requesters")
        .map(|n| n.parse().expect("--requesters N"))
        .unwrap_or(4)
        .max(1);
    // Before anything else: the orchestrator asks for the requesters'
    // addresses to fund them in genesis, before any node exists to answer
    // --rpc. One address per line, slot order.
    if std::env::args().any(|a| a == "--print-requester") {
        for slot in 0..count {
            println!("{}", requester_keypair(&seed, slot).address().to_hex());
        }
        return;
    }
    let ports: Vec<u16> = arg("--rpc")
        .expect("--rpc PORT[,PORT...]")
        .split(',')
        .map(|p| p.parse().expect("port"))
        .collect();
    let rate: f64 = arg("--rate").map(|r| r.parse().unwrap()).unwrap_or(0.2);
    let duration = Duration::from_secs(
        arg("--duration")
            .map(|d| d.parse().unwrap())
            .unwrap_or(600),
    );
    let lost_after: f64 = arg("--lost-after")
        .map(|s| s.parse().unwrap())
        .unwrap_or(300.0);
    let out_path = arg("--out").expect("--out FILE");
    let tuple: Vec<Hash256> = arg("--tuple")
        .expect("--tuple model,profile,generation,assignment (hex)")
        .split(',')
        .map(|h| Hash256::from_hex(h).expect("tuple hash"))
        .collect();
    assert_eq!(tuple.len(), 4, "--tuple needs four hashes");
    let executor = arg("--executor-label").unwrap_or_else(|| "deterministic-test-executor".into());
    let out = std::fs::OpenOptions::new()
        .create(true)
        .append(true)
        .open(&out_path)
        .expect("open --out");

    // The domain every request binds to, from the first node that answers.
    let ctx = loop {
        if let Some(ctx) = ports
            .iter()
            .find_map(|p| get_json(*p, "/native-inference/context"))
        {
            break ctx;
        }
        std::thread::sleep(Duration::from_secs(1));
    };
    let domain = InferenceDomain {
        chain_genesis: Hash256::from_hex(ctx["chain_genesis"].as_str().unwrap()).unwrap(),
        recovery_epoch: ctx["recovery_epoch"].as_u64().unwrap(),
        validator_set_hash: Hash256::from_hex(ctx["validator_set_hash"].as_str().unwrap()).unwrap(),
    };
    let mut driver = Driver {
        ports,
        domain,
        tuple,
        executor,
        out,
        lost_after,
        requesters: Vec::new(),
        watch: Vec::new(),
        turn: 0,
        offered: 0,
        backpressured: 0,
    };
    for slot in 0..count {
        let key = requester_keypair(&seed, slot);
        let next_nonce = driver.chain_nonce(&key.address()).unwrap_or(0);
        driver.requesters.push(Requester {
            key,
            next_nonce,
            slot: Slot::Idle,
        });
    }

    let interval = Duration::from_secs_f64(1.0 / rate.max(1e-6));
    let started = Instant::now();
    let mut next = Instant::now();
    while started.elapsed() < duration {
        if Instant::now() >= next {
            next += interval;
            driver.offer();
        }
        driver.poll();
        std::thread::sleep(Duration::from_millis(250));
    }
    // Drain: a bounded chance to settle, then whatever is left is pending.
    let drain_until = Instant::now() + Duration::from_secs(90);
    while (driver.busy() || !driver.watch.is_empty()) && Instant::now() < drain_until {
        driver.poll();
        std::thread::sleep(Duration::from_secs(1));
    }
    driver.finish();
}
