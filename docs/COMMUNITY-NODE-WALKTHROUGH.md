# ARC community node: 2–3 minute walkthrough

This is the short recording script for the repaired headless/community flow.
The v0.7.12 recovery candidate is not published or deployed. Use this script
only after the complete v0.7.12 CLI release is visible on GitHub and the seed
rollout checklist below has been completed. Do not record against the current
mixed v0.7.2/v0.7.9 fleet as though these branch fixes are already live.

## Before recording

- Use a clean SSH-only Ubuntu 24.04 or 26.04 machine.
- Use a fresh data directory. v0.7.12 writes `genesis.network-hash` and fails
  closed when an existing WAL has no marker or a different genesis hash. Back
  up old v2 identity/data for forensics, but do not reuse or copy the v2 WAL.
  Validators need the approved canonical checkpoint migration instead.
- Pre-stage the supported GGUF model at an absolute path. Confirm that it loads
  every layer and that its streamed BLAKE3 artifact ID exactly matches the
  request/coordinator model ID. A multi-gigabyte download does not fit a
  three-minute recording.
- Pick one seed that is sealing blocks and use that same seed for the worker,
  inference request, earnings query, and explorer. Mixing seeds can mix chain
  histories.
- Confirm the seed is on the approved v3 genesis/checkpoint and the operator
  has completed [VALIDATOR-FLEET-ROLLOUT.md](VALIDATOR-FLEET-ROLLOUT.md).
- Confirm the canonical activation height has been reached, the local
  issuance switch is open, and independent validator approval collection is
  ready. The combined `community_rewards_v1_enabled` field is true only when
  all three conditions hold. In the current recovery candidate, approval
  collection intentionally remains unavailable and issuance fails closed:

  ```bash
  # Public seed RPC is normally 9090. Port 9944 is the separate local-node
  # default and should be used only when you intentionally target that node.
  export ARC_RPC=http://SEED_HOST:9090
  curl -fsS "$ARC_RPC/economics/rewards" \
    | jq '{community_rewards_v1_enabled,
           community_rewards_v1_protocol_active,
           community_rewards_v1_approval_collection_ready,
           community_rewards_v1_activation_height,
           community_rewards_v1_issuance_enabled,
           community_rewards_v1_note,
           reward_per_attestation_arc,treasury_balance_arc}'
  ```

If the activation height is null, the issuance switch is false, approval
collection is false, or the effective enabled field is false, demonstrate
verified inference only. Do not say the worker earned a reward.
An absent genesis activation fails closed. Missing validator approval quorum
also fails closed; the CLI flag alone cannot turn issuance on.

## 0:00–0:35 — install without a screen

Narration: “This is a plain server over SSH—no desktop and no GUI.”

```bash
curl -fsSLO https://github.com/FerrumVir/arc-chain/releases/download/v0.7.12/install.sh
bash install.sh --version 0.7.12 --model /absolute/path/to/model.gguf
```

The installer resolves one exact semantic-versioned release, verifies every
download against `SHA256SUMS`, creates a stake-zero community service, and
preserves its identity across updates. It never substitutes a desktop package
for `arc-node`, and its RPC listener stays on `127.0.0.1` by default.

## 0:35–1:00 — prove the process is running and inspect health

```bash
"$HOME/.arc/bin/arc-node" --version
curl -fsS http://127.0.0.1:9944/health | jq
systemctl --user --no-pager status arc-node | sed -n '1,12p'
```

Say the returned status out loud. A `degraded` response proves that the
headless process and RPC started, not that the chain is synchronized or ready
to pay inference rewards.

For a system-wide install, use `sudo systemctl status arc-node`. On macOS use
the `launchctl` commands in [HEADLESS_INSTALL.md](HEADLESS_INSTALL.md).

## 1:00–1:25 — show registration and real capacity

```bash
curl -fsS "$ARC_RPC/community/list" \
  | jq '{count,total_work_completed,community_rewards_v1_enabled,
         community_rewards_v1_approval_collection_ready,community_rewards_v1_note,
         workers:[.workers[]|{worker_id,name,platform,model,work_completed}]}'
```

Point to the actual platform, loaded model, and server-authoritative completed
work count. A heartbeat cannot reset or inflate this count.

Copy the worker address from `worker_id`:

```bash
export ARC_WORKER=0xPASTE_WORKER_ID
```

## 1:25–2:05 — route one inference to the worker

Use `/inference/run`, which is the community-worker router. The replicated
seed-only demo endpoint `/inference/run_consensus` does not assign community
work.

```bash
curl -fsS -X POST "$ARC_RPC/inference/run" \
  -H 'content-type: application/json' \
  -d '{"input":"The largest planet is","max_tokens":8}' \
  | tee /tmp/arc-inference.json \
  | jq '{success,routed_via,inference:{output:.inference.output,
        output_hash:.inference.output_hash,tokens_generated:.inference.tokens_generated,
        inference_ms:.inference.inference_ms,engine:.inference.engine},
        verification:{method:.verification.method,output_hash:.verification.output_hash,
        tokens_generated:.verification.tokens_generated,ranges:.verification.ranges,
        range_position_quorums:.verification.range_position_quorums,
        signatures_required_per_quorum:.verification.signatures_required_per_quorum,
        replicas_contacted_per_quorum:.verification.replicas_contacted_per_quorum},
        settlement:{status:.settlement.status,included:.settlement.included,
        reward_tx_hash:.settlement.reward_tx_hash,reason:.settlement.reason,
        error:.settlement.error}}'
```

`routed_via` must begin with `community:`. If it says `local`, the seed fell
back safely and this run did not demonstrate community assignment. A successful
community response is accepted only after the coordinator recomputes the
output and obtains two distinct active-validator signatures for every model
range and token position. Show the coordinator quorum summary and confirm its
`output_hash` exactly equals `inference.output_hash`; the current summary does
not expose a client-verifiable raw signature bundle, so do not describe it as
one. Also show settlement status—submission to the mempool is not mined income.
The off-chain two-signature range quorum is not the on-chain payment threshold.
The `0x25` reward certificate separately needs unique active-validator
approvals covering strict greater-than-two-thirds of both identities and active
stake; with six equally staked validators, five approvals are required.

## 2:05–2:40 — prove payment, not just submission

```bash
curl -fsS "$ARC_RPC/worker/earnings/$ARC_WORKER" \
  | jq '{onchain_balance_arc,total_rewards,estimated_total_arc,
         reward_per_attestation_arc,last_reward_block,last_reward_tx_hash,
         attestations_per_day_observed,attestations_per_day_unavailable_reason,
         community_rewards_v1_enabled,
         community_rewards_v1_approval_collection_ready,
         community_rewards_v1_note}'
```

Narration:

- `onchain_balance_arc` is the address’s actual current chain balance.
- `total_rewards` counts only successful, mined
  `CommunityInferenceReward` receipts visible in this node’s retained index.
- `estimated_total_arc` is gross retained-window reward history, not the
  wallet balance and not a promise of future work.
- An observed rate appears only when real timestamps provide a measurement. An
  unavailable value stays null rather than being invented.

Open `last_reward_tx_hash` in the static explorer, select the same seed as the
data source, prove the transaction type is `CommunityInferenceReward` (`0x25`),
and show the successful receipt. A mined raw `InferenceAttestation` (`0x16`)
pays nothing. If the hash is null, pruned, a different type, or unsuccessful,
the reward is not proven—wait or say so plainly.

## Recording stop conditions

Stop instead of improvising if any of these is true:

- the selected seed’s latest block is stale;
- seeds disagree on a common-height block hash or state root;
- the worker is absent, has not fully loaded its model, or advertises an
  artifact ID different from the request;
- `routed_via` is not a community worker;
- reward issuance is not fully ready, including validator approval collection;
- independent verifier evidence is absent or incomplete;
- the reward transaction is pending, failed, pruned, or absent.

Those are diagnostic outcomes, not footage to market as a completed flow.
