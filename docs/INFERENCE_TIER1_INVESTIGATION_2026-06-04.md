# Inference Investigation — Tier-1 On-Chain Inference Does Not Finalize on the Live Seeds

> **Dated incident record.** Statements below about worker count, working
> community routing, versions, balances, and latency are observations from June
> 4, not current status. The 2026-08-26 read-only snapshot found a mixed
> v0.7.2/v0.7.9 fork and community `total_work_completed: 0`; v0.7.12 remains
> unpublished and undeployed. See
> [`PRODUCTION-RECOVERY-AUDIT-2026-08-26.md`](PRODUCTION-RECOVERY-AUDIT-2026-08-26.md).

**Date:** 2026-06-04
**Tested against:** the 5 public seeds — LAX `140.82.16.112`, AMS `136.244.109.1`, LHR `104.238.171.11`, NRT `202.182.107.41`, SGP `149.28.153.31` — all on **v0.7.9** (branch `fix/v078-attestation-wire-compat`, HEAD `631e5b0`).
**Audience:** TJ (and anyone working on the inference / consensus path).
**One-line conclusion:** A caller-signed `InferenceRequest` is *accepted* by `/inference/onchain/submit` (HTTP 200) but **never lands in a block on any seed** — so escrow never opens, the committee never votes, and inference never finalizes. The failure is at **transaction inclusion**, not signing or the endpoint. The deeper cause is that the 5 seeds run as **independent chains**, not one replicated network.

---

## 1. What works

| Path | Endpoint | Result |
|---|---|---|
| Faucet | `POST /faucet/claim` | ✅ address credited 10000 ARC (visible on LAX) |
| Community inference | `POST /inference/run` | ✅ returns real deterministic output, routed to a community GPU worker (11 live workers), INT8, ~27s |
| Submit accepts caller-signed tx | `POST /inference/onchain/submit` (`signed_tx` field) | ✅ HTTP 200, `signed_by: "caller"` |
| Read endpoints | `/health`, `/validators`, `/models`, `/inference/attestations` | ✅ all HTTP 200 |

So: **community (off-chain, worker-routed) inference works**. The wallet, faucet, and read paths work. The desktop (v0.7.7) functions against the v0.7.9 seeds.

## 2. What is broken — tier-1 on-chain inference

The `InferenceRequest` transaction **never applies on any seed**. Reproduced with TJ's own smoke example:

```
cargo run --release --example v079_signed_inference -p arc-node -- http://140.82.16.112:9090
```

Observed:
- faucet ok (10000 ARC), nonce read = 0
- submit → `HTTP 200 {"signed_by":"caller","request_id":"0x55c8…","tx_hash":"0xb046…","committee_size":15,...}`
- poll `/inference/onchain/result/0x55c8…` → `"no such request"` for the **full 180s**
- exit `FAIL: never reached Finalized`

Cross-seed verification (same request, all 5 seeds):

| Check | LAX | AMS | LHR | NRT | SGP |
|---|---|---|---|---|---|
| `tx_hash` via `/tx/0x b046…` | not found | not found | not found | not found | not found |
| signer balance | 10000 | — | — | — | — |
| signer nonce (was 0 at submit) | **0** | — | — | — | — |
| `request_id` result | `no such request` | error | error | error | error |

The signer **nonce never moves** (0 → 0 for the caller path; 30 → 30 for the self-sign path), which means the `InferenceRequest` tx is **never included in any committed block**. The escrow open that `InferenceRequest` performs never happens, so there is nothing for the committee to vote on.

Both submit paths fail the same way:
- **self-sign** (`signed_by: validator_self`, the path the desktop's dead-code `tier1_submit` uses): tx not found, validator nonce frozen.
- **caller-signed** (`signed_by: caller`, the `signed_tx` field TJ added in `34e1fd0`): tx not found, signer nonce frozen.

The submit response even anticipates this — it returns the note: *"If status stays 'no such request' >60s, retry via the `signed_tx` field…"* — but the `signed_tx` path also fails to land.

## 3. Root cause — the seeds are independent chains, and InferenceRequest txs are never included

Two layers of failure compound here.

### 3a. The seeds do not share state (independent chains)
- `GET /block/43000` returns a **different block hash on every seed** (LAX `91f780e2…`, AMS `607183677…`, LHR `71c674c2…`, NRT `00378f72…`, SGP `1e1b2215…`); block timestamps span ~11 days; SGP even has a different `tx_count` at the same height.
- `/health` heights diverge ~4,700 blocks with a **stable** offset (parallel tracks, not lag).
- A faucet credit on LAX replicates to LAX (+SGP) only — never AMS/LHR/NRT.
- Shared-looking `dag_round` does **not** drive shared state.

(Full evidence: `docs/TESTNET_STATE_DIVERGENCE_2026-06-03.md`.)

### 3b. The InferenceRequest tx is not included even on the seed it was submitted to
The faucet credit shows up on LAX (likely pre-applied by the faucet handler directly into state), but a *signed user transaction* (the `InferenceRequest`) submitted to LAX never makes it into a LAX block (nonce frozen). So the problem is specifically **transaction inclusion for the InferenceRequest TxType**, on top of the no-shared-state problem.

## 4. Where to look (file:line)

1. **Submit handler, `signed_tx` branch** — confirm the caller-signed tx is actually `mempool.insert`-ed *and* gossiped to peers (not just decoded and acked):
   `crates/arc-node/src/rpc.rs:3391+` (the `signed_tx` decode/validate block of `inference_onchain_submit`).
2. **Block commit inclusion** — on DAG commit, the committed block only includes txs found in the **local** `pending_txs`; any hash not present is **silently skipped** (no peer-fetch, no error):
   `crates/arc-node/src/consensus.rs:964-974`.
3. **InferenceRequest apply** — verify it is ever reached (it is not, since nonce never moves):
   `crates/arc-state/src/lib.rs:4607-4747` (`TxBody::InferenceRequest`).
4. **Non-deterministic block sealing** — blocks are sealed with `SystemTime::now()` instead of the committed `DagBlock.timestamp`, so two nodes can never produce the same block hash even with identical txs:
   `crates/arc-node/src/consensus.rs:800`, `crates/arc-state/src/lib.rs:1397`.
5. **Committee model match** — `canonical_testnet_model_id() = BLAKE3("arc-32L-test")` (`crates/arc-node/src/inference_validator.rs:53`). Note this differs from the registry id `BLAKE3("arc-32L-4096d-32h-32000v")` shown by `/models`. The smoke example uses the canonical id, so this is internally consistent — but worth confirming the committee validators actually have a model loaded and run the `InferenceValidatorTask`.

## 5. Output-quality note (separate from the inclusion bug)
Community inference returns coherent-mechanism but garbage-text output (e.g. `"<0x0A>[Based]</s>"`). Three determinants:
- **Precision:** the worker used INT8; INT16 is the production-quality precision (per repo notes INT8 ≈ PPL 144, INT16 targets ≈ FP16 5.47).
- **Coverage:** `/models` reports `fully_covered: false` (`shard_count: 22`) — an incomplete forward pass yields garbage.
- **Weights:** `full_model_mb: 6176` (~6 GB) suggests real 7B-class weights are present, so the prime suspects are precision + coverage, not missing weights.
For a useful demo: serve a real base GGUF (Llama-2-7B = the `32L-4096d-32h-32000v` shape) fully loaded on one node at INT16, not sharded/INT8.

## 6. What would fix tier-1 (and where help is most useful)
The minimum to make tier-1 finalize is **reliable transaction inclusion on a single canonical chain**:
1. Ensure caller-signed `InferenceRequest` txs reach the mempool and a committed block on the submit node (fix 4.1 / 4.2).
2. Make block production deterministic + replicated so the committee's `InferenceVote` txs (themselves transactions) propagate and aggregate — this is the Model-1 work.

A full, phased fix plan is on branch `investigation/testnet-state-divergence`:
- `docs/superpowers/plans/2026-06-04-replicated-chain-model-1.md`
- Acceptance tool (the objective pass/fail): `cargo run --release --example cross_seed_block_check -p arc-node -- <height>` → must print **AGREE** across all seeds (currently **DIVERGE**).

## 7. Reproduction commands
```bash
# Tier-1 caller-signed submit + poll (TJ's example) — currently FAILS to finalize:
cargo run --release --example v079_signed_inference -p arc-node -- http://140.82.16.112:9090

# Community inference — currently WORKS (garbled text, INT8):
curl -s -X POST http://140.82.16.112:9090/inference/run \
  -H 'Content-Type: application/json' \
  -d '{"input":"[INST] What is the capital of France? [/INST]","max_tokens":8}'

# Cross-seed chain divergence (the systemic root cause):
for ip in 140.82.16.112 136.244.109.1 104.238.171.11 202.182.107.41 149.28.153.31; do
  echo -n "$ip block#43000: "; curl -s http://$ip:9090/block/43000 | grep -oE '"hash":"[0-9a-f]+"' | head -1
done
```
