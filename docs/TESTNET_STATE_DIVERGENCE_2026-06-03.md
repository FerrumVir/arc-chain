# Testnet Investigation — Seeds Are Independent Chains, Not One Network

**Date:** 2026-06-03
**Investigator:** live RPC probing of the 5 public seeds + 1 retired NYC seed
**Binary version under test:** v0.7.7 (uniform across all live seeds)
**Bottom line:** The 5 "testnet seeds" are **5 independent blockchains** running the
same binary, not a single replicated consensus network. This is the real reason
end-to-end paid inference does not work across the network — and it is **not**
fixed by upgrading binaries to 0.7.7.

---

## 1. What prompted this

The working assumption (recorded in prior session notes) was:

> "The 5 seeds joined into one consensus network (27 validators, shared
> `dag_round`), so block production / faucet / inference / explorer all work
> end-to-end on testnet."

A live attempt to run paid inference end-to-end (`live_paid_inference` against
LAX) failed at the escrow-open step, which triggered this investigation.

## 2. Evidence

### 2.1 Versions are uniform (this part was fine)

All 5 live seeds report `version: 0.7.7` via `/health`. The desktop workspace,
`tauri.conf.json`, and desktop crate are all `0.7.7` as well. **There is no
desktop-vs-seed version mismatch.** "Update all seeds to 0.7.7" was completed and
is not the problem.

### 2.2 Block heights diverge by thousands and the offset is stable

Two snapshots, minutes apart, of `/health` `height`:

| Seed | IP                | height (t0) | height (t1) | Δ over interval | payer balance |
|------|-------------------|-------------|-------------|-----------------|---------------|
| LAX  | 140.82.16.112     | 46506       | 46552       | +46             | **10000**     |
| AMS  | 136.244.109.1     | 43832       | 43879       | +47             | 0             |
| LHR  | 104.238.171.11    | 45250       | 45292       | +42             | 0             |
| NRT  | 202.182.107.41    | 47504       | 47551       | +47             | 0             |
| SGP  | 149.28.153.31     | 48613       | 48634       | +21             | **10000**     |
| NYC  | 149.28.32.76      | —           | no response | —               | down          |

All seeds advance at ~the same rate, but the gaps between them (~4,700 blocks
between AMS and SGP) stay constant. They are not "one chain where some nodes
lag" — they are parallel tracks.

### 2.3 The same block height has a different hash on every seed (definitive)

`GET /block/43000` (a height every seed has passed):

| Seed | block #43000 hash | timestamp      | tx_count |
|------|-------------------|----------------|----------|
| LAX  | `91f780e2…`       | 1780349179939  | 1        |
| AMS  | `607183677…`      | 1780434189979  | 1        |
| LHR  | `71c674c2…`       | 1780372979323  | 1        |
| NRT  | `00378f72…`       | 1779699227650  | 1        |
| SGP  | `1e1b2215…`       | 1779457813294  | **2**    |

The same height yields a different hash on every node, the timestamps span
**~11 days** (1779457… vs 1780434…), and SGP even has a different transaction
count. A single replicated chain cannot produce this. These are **5 separate
chains.**

### 2.4 Faucet credit does not replicate

A faucet claim submitted to LAX credited the payer 10000 ARC. Minutes later the
balance was present on **LAX and SGP only**, never on AMS/LHR/NRT. On a single
replicated state machine, a DAG-committed faucet tx would apply identically on
every node. It does not.

### 2.5 `dag_round` is shared-looking but meaningless for state

All seeds report nearly identical `dag_round` (~4,951,3xx). This is the only
"shared" signal, and it is misleading: it does **not** drive shared block
production or shared state. Block contents (2.3) and account state (2.4) are
fully independent.

## 3. Conclusions

1. **There is no unified testnet.** There are 5 solo chains running the same
   0.7.7 binary, plus a dead NYC seed.
2. **"Inference works on testnet" is not true as a network property.** An
   inference request submitted to one seed can only ever be voted/finalized by
   that seed's own committee. Cross-seed consensus inference does not happen
   because the seeds do not share state.
3. **On-chain inference attestations are real but single-chain.** The
   `success:true` attestations observed at LAX block ~45433 are genuine — but
   they live only on LAX's chain, not on a shared ledger.
4. **The desktop masked this.** Pinning every wallet read to `WALLET_HOSTS[0]`
   (LAX) means the UI only ever reads one chain, so state looks self-consistent.
5. **NYC being down is irrelevant.** It is not in the desktop's 5-seed routing,
   consensus on each chain advances without it, and the failed inference run had
   already skipped it (LAX was the coordinator). Skipping NYC does not fix
   anything.

## 4. What this means for the original question

> "Update all seeds to 0.7.7 so inference runs — correct?"

- Upgrading all seeds to 0.7.7: **done, uniform, correct.** ✅
- "So inference runs (as a network)": **not achieved** — because the seeds are
  not a network. The blocker is architectural (independent chains), not a
  version or a single TxType bug.

## 5. Open questions / next steps

- **Why are the seeds independent?** Determine whether DAG consensus is actually
  wired to drive a single shared chain, or whether each `arc-node` builds its own
  local chain from its own mempool while only exchanging `dag_round` counters.
- **Decide the target topology.** Either (a) make the 5 seeds a genuinely
  replicated chain (shared state, identical block hashes per height), or (b)
  formally treat each seed as an independent solo demo chain and stop describing
  them as "one 27-validator network."
- **Re-test the tier-1 `InferenceRequest` path** once a single canonical chain
  exists. The previously "proven" submit path was verified against LAX alone, so
  it proves single-chain behavior, not network behavior.
- **NYC seed:** either revive `149.28.32.76` or retire it everywhere (examples
  now default to LAX; see code changes in this branch).

## 6. Code changes in this branch

- All `arc-node` example coordinators (`DEFAULT_COORD` / `COORD`) repointed from
  the dead NYC node (`149.28.32.76`) to LAX (`140.82.16.112`). Running any
  example without an explicit coordinator argument no longer fails immediately
  against a dead host.
- Removed the dead NYC entry from the `SEEDS` list in `live_paid_inference.rs`.
