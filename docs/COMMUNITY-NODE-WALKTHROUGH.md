# ARC community node: 2–3 minute walkthrough

This is the short recording script for the repaired headless/community flow.
The v0.8.0 recovery candidate is not published or deployed. Use this script
only after the complete v0.8.0 CLI release is visible on GitHub and the seed
rollout checklist below has been completed. Do not record against the current
mixed v0.7.2/v0.7.9 fleet as though these branch fixes are already live.

## Before recording

- Use a clean SSH-only Ubuntu 24.04 or 26.04 machine.
- Use a fresh data directory. v0.8.0 writes `genesis.network-hash` and fails
  closed when an existing WAL has no marker or a different genesis hash. Back
  up old v2 identity/data for forensics, but do not reuse or copy the v2 WAL.
  Validators need the approved canonical checkpoint migration instead.
- Pre-stage the supported GGUF model at an absolute path. Confirm that it loads
  every layer and that its streamed BLAKE3 artifact ID exactly matches the
  request/coordinator model ID. A multi-gigabyte download does not fit a
  three-minute recording.
- Pick one approved HTTPS seed origin that is sealing blocks and use that same
  origin for the worker, inference request, earnings query, receipt, and
  explorer. Mixing origins can mix chain histories during recovery.
- Confirm the seed is on the approved v3 genesis/checkpoint and the operator
  has completed [VALIDATOR-FLEET-ROLLOUT.md](VALIDATOR-FLEET-ROLLOUT.md).
- Confirm the canonical activation height has been reached, the local
  issuance switch is open, and independent validator approval collection is
  ready. The stable reward-policy endpoint binds readiness to the recovery
  epoch, validator set, exact transaction domain, and all six configured RPC
  origins:

  ```bash
  # Choose exactly one of the six reviewed HTTPS origins. Raw remote HTTP and
  # :9090 are not production v3 configuration.
  export ARC_RPC=https://149-28-32-76.nip.io
  curl -fsS "$ARC_RPC/community/reward_policy" \
    | jq '{schema,tx_type,protocol_active,issuance_ready,
           readiness_unavailable_reason,active_validator_count,
           configured_community_rpc_origins,validator_set_size_required,
           validator_approvals_required,recovery_epoch,validator_set_id,
           validator_set_commitment,transaction_domain,stake_zero_eligible,
           worker_min_stake_base,reward_base,reward_arc,earnings_evidence}'
  ```

For the six-validator recovery network, `configured_community_rpc_origins` is
the integer `6`, `validator_approvals_required` is `5`, and
`stake_zero_eligible` is true. If `issuance_ready` is false, any epoch/domain
field is absent, or the returned set does not match the locked rollout, show
verified inference only. Do not say the worker earned a reward.
An absent genesis activation fails closed. Missing validator approval quorum also fails
closed; the CLI flag alone cannot turn issuance on.

## 0:00–0:35 — install without a screen

Narration: “This is a plain server over SSH—no desktop and no GUI.”

```bash
curl -fsSLO https://github.com/FerrumVir/arc-chain/releases/download/v0.8.0/install.sh
bash install.sh --version 0.8.0 --model /absolute/path/to/model.gguf
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
        settlement:{status:.settlement.status,submitted:.settlement.submitted,
        included:.settlement.included,tx_hash:.settlement.tx_hash,
        job_id:.settlement.job_id,receipt_url:.settlement.receipt_url,
        recovery_epoch:.settlement.recovery_epoch,
        validator_set_id:.settlement.validator_set_id,
        transaction_domain:.settlement.transaction_domain,
        validator_approvals:.settlement.validator_approvals,
        required_validator_approvals:.settlement.required_validator_approvals,
        reason:.settlement.reason}}'
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
export ARC_REWARD_TX=$(jq -r '.settlement.tx_hash // empty' /tmp/arc-inference.json)
test -n "$ARC_REWARD_TX"
curl -fsS "$ARC_RPC/community/reward_receipt/$ARC_REWARD_TX" \
  | jq '{status,tx_type,tx_hash,job_id,worker,model_id,input_hash,output_hash,
         recovery_epoch,validator_set_id,transaction_domain,
         validator_approvals,included,confirmed,success,block_height,block_hash,
         reward_base,reward_arc,evidence_source}'

curl -fsS "$ARC_RPC/worker/earnings/$ARC_WORKER" \
  | jq '{onchain_balance_arc,confirmed_receipt_count,
         confirmed_gross_earnings_base,confirmed_gross_earnings_arc,
         confirmed_receipts,projected_daily_arc,
         projected_daily_unavailable_reason,projection_policy,
         reward_per_attestation_base,reward_per_attestation_arc,
         recovery_epoch,validator_set_id,stake_zero_eligible,
         last_reward_block,last_reward_tx_hash}'
```

Narration:

- `onchain_balance_arc` is the address’s actual current chain balance.
- `confirmed_receipt_count` counts only successful, mined
  `CommunityInferenceReward` receipts visible in this node’s retained index.
- `confirmed_receipts` provides the rows, including block and recovery-domain
  bindings, that sum to `confirmed_gross_earnings_base`.
- `projected_daily_arc` appears only when an explicit active policy, confirmed
  receipt rate, and funded treasury support it. Otherwise it stays null and
  `projected_daily_unavailable_reason` says why.

The receipt must say `status: mined_success`, `confirmed: true`, `success:
true`, and `tx_type: 0x25` before the amount is earned. Then open the same hash
in the static explorer, select the same origin as the data source, and show the
matching block. A mined raw `InferenceAttestation` (`0x16`) pays nothing. If the
hash is null, pending, pruned, a different type, or unsuccessful, the reward is
not proven—wait or say so plainly.

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
