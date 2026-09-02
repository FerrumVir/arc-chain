# ARC community node: edited 2–3 minute walkthrough

This is the short recording script for the repaired headless/community flow.
The finished video is edited to 2–3 minutes; it is not a promise that install,
model loading, assignment, block inclusion, or the two-receipt earnings proof completes
in three minutes of wall-clock time. Record the verified waits, then cut them.
At the 2026-08-31 source-freeze review cutoff, the v0.8.0 recovery candidate
was not published or deployed; that is a tag-stable historical statement, not
a live probe. Use this script only when the complete v0.8.0 CLI release is
visible on GitHub and signed evidence proves the seed rollout checklist below
has been completed. Do not record against a mixed v0.7.2/v0.7.9 fleet as
though these branch fixes are already live.

## Before recording

- Require the JSON tool used by every fail-closed check in this script:

  ```bash
  command -v jq >/dev/null || { echo "jq is required" >&2; exit 1; }
  ```

- Use a clean SSH-only x86_64 Ubuntu 22.04, 24.04, or 26.04 machine, or an
  ARM64 Ubuntu 24.04 or 26.04 machine; those are the release-gated Linux
  environments.
- Use a fresh data directory. v0.8.0 writes `genesis.network-hash` and fails
  closed when an existing WAL has no marker or a different genesis hash. Back
  up old v2 identity/data for forensics, but do not reuse or copy the v2 WAL.
  Validators need the approved canonical checkpoint migration instead.
- Pre-stage the supported GGUF model at an absolute path using the exact
  byte-count and SHA-256 procedure in
  [HEADLESS_INSTALL.md](HEADLESS_INSTALL.md#download-and-verify-the-exact-worker-model).
  Confirm that it loads every layer and that its streamed BLAKE3 artifact ID
  exactly matches the request/coordinator model ID. A multi-gigabyte download
  does not fit a three-minute recording.
- Pick one approved HTTPS seed origin that is sealing blocks and use that same
  origin for the worker, inference request, earnings query, receipt, and
  explorer. Mixing origins can mix chain histories during recovery.
- Confirm the seed is on the approved v3 genesis/checkpoint and the operator
  has completed [VALIDATOR-FLEET-ROLLOUT.md](VALIDATOR-FLEET-ROLLOUT.md).
- Confirm the recovered frontend checkpoint names the sealed
  `legacyPublicMaxHeight` and every one of its six v3 replicas reports a
  `last_block_height` strictly above it. If the app or explorer still shows
  maintenance, do not bypass that gate for the recording.
- Confirm the canonical activation height has been reached, the local
  issuance switch is open, and independent validator approval collection is
  ready. The stable reward-policy endpoint binds readiness to the recovery
  epoch, validator set, exact transaction domain, and all six configured RPC
  origins:

  ```bash
  # Choose exactly one of the six reviewed HTTPS origins. Raw remote HTTP and
  # :9090 are not production v3 configuration.
  export ARC_RPC=https://149.28.32.76
  curl -fsS "$ARC_RPC/community/reward_policy" \
    | jq '{schema,tx_type,protocol_active,issuance_ready,
           readiness_unavailable_reason,active_validator_count,
           configured_community_rpc_origins,validator_set_size_required,
           validator_approvals_required,recovery_epoch,validator_set_id,
           validator_set_commitment,transaction_domain,stake_zero_eligible,
           worker_min_stake_base,reward_base,reward_arc,issuance_policy,
           issuance_policy_hash,prospective_budget,
           treasury_rewards_remaining,reward_program,
           reward_is_customer_demand,earnings_evidence}'
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
curl -fsSLO --proto '=https' --proto-redir '=https' --tlsv1.2 https://raw.githubusercontent.com/FerrumVir/arc-chain/v0.8.0/install.sh
ARC_INSTALL_SHA256=4480a627e5f50f61a22b6a3b97ab4a8f102400c03f03a1c73d7d8abe79601151
if command -v sha256sum >/dev/null 2>&1; then
  printf '%s  %s\n' "$ARC_INSTALL_SHA256" install.sh | sha256sum -c -
else
  printf '%s  %s\n' "$ARC_INSTALL_SHA256" install.sh | shasum -a 256 -c -
fi
bash install.sh --version 0.8.0 --model /absolute/path/to/model.gguf
```

The pinned installer SHA-256 is
`4480a627e5f50f61a22b6a3b97ab4a8f102400c03f03a1c73d7d8abe79601151`.

The protected source-tag installer resolves one exact semantic-versioned
release and verifies the owner signature on `SHA256SUMS` before trusting any
download hash. It creates a stake-zero community service and
preserves its identity across updates. It never substitutes a desktop package
for `arc-node`, and its RPC listener stays on `127.0.0.1` by default. A
model-backed install uses the full deterministic integer-worker role without
announcing the home machine as a validator shard.

For a recognized v0.7.x node at the exact default `~/.arc`, the same command
preserves its identity, active model, custom ports, old data, and archived
configuration while selecting fresh `data-v0.8/` state. It verifies and stops
the real Linux global units, macOS agents, or detached PID before replacement.
On the common Linux layout, root keeps ownership only of the global unit files;
the node and signed updater both run as the original community user, and the
old unsigned updater cannot relaunch. Arbitrary custom or lookalike directories
are not adopted, a failed migration restores the prior binary/service state,
and a pending adoption cannot authorize purge.

## 0:35–1:00 — prove the process is running and inspect health

```bash
"$HOME/.arc/bin/arc-node" --version
curl -fsS http://127.0.0.1:9944/health | jq
ARC_SERVICE_SCOPE=$(sed -n 's/^service_scope=//p' "$HOME/.arc/install.conf")
case "$ARC_SERVICE_SCOPE" in
  user) systemctl --user --no-pager status arc-node ;;
  system-user) sudo systemctl --no-pager status arc-node ;;
  launchd) launchctl print "user/$(id -u)/network.arc.node" \
    || launchctl print "gui/$(id -u)/network.arc.node" ;;
  none) printf '%s\n' 'install-only mode: no service manager was configured' ;;
  *) printf 'unknown service_scope: %s\n' "$ARC_SERVICE_SCOPE" >&2; exit 1 ;;
esac
```

Say the returned status out loud. A `degraded` response proves that the
headless process and RPC started, not that the chain is synchronized or ready
to pay inference rewards.

The v0.7 Linux adoption records `service_scope=system-user`: its service is
global even though its node and updater run as the original community user, so
`systemctl --user` is wrong for that migrated topology. For a fresh system-wide
install rooted at `/var/lib/arc-chain`, use `sudo systemctl status arc-node`.

## 1:00–1:25 — show this node's registration and real capacity

```bash
export ARC_WORKER=$(curl -fsS http://127.0.0.1:9944/node/info | jq -er '.validator')
curl -fsS "$ARC_RPC/workers/scoreboard?limit=50" \
  | jq --arg worker "$ARC_WORKER" \
      '{count_visible,eligible_inference_workers,coordinator_model,
        this_node:[.workers[]|select(.worker_id==$worker)
          |{worker_id,name,platform,model,model_id,execution_profile,
            work_completed,success_count,failure_count,success_rate,
            avg_ms_per_job}]}'
test "$(curl -fsS "$ARC_RPC/workers/scoreboard?limit=50" \
  | jq --arg worker "$ARC_WORKER" '[.workers[]|select(.worker_id==$worker)]|length')" -eq 1
curl -fsS "$ARC_RPC/community/reward_policy" \
  | jq '{protocol_active,issuance_ready,readiness_unavailable_reason,
         stake_zero_eligible,reward_arc,validator_approvals_required,
         treasury_rewards_remaining,reward_program,earnings_evidence}'
```

Point to the actual platform, exact model identity, server-authoritative success
and failure counts, and the separate reward-readiness response. A heartbeat
cannot reset or inflate these counts. The read-only scoreboard must be queried
instead of legacy `/community/list`, whose GET handler prunes stale registry
entries as a side effect. `ARC_WORKER` comes from this machine's authenticated
`/node/info` identity; never select a convenient worker from the global list
and present it as the local node.

## 1:25–2:05 — route two one-token inferences to the worker

Use `/inference/run`, which is the community-worker router. The replicated
seed-only demo endpoint `/inference/run_consensus` does not assign community
work.

```bash
set -o pipefail
for ARC_PROBE in 1 2; do
  curl -fsS -X POST "$ARC_RPC/inference/run" \
    -H 'content-type: application/json' \
    -d '{"input":"The largest planet is","max_tokens":1}' \
    | tee "/tmp/arc-inference-$ARC_PROBE.json" \
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

  test "$(jq -er '.routed_via' "/tmp/arc-inference-$ARC_PROBE.json")" \
    = "community:$ARC_WORKER"
  ARC_REWARD_TX=$(jq -er '.settlement.tx_hash' "/tmp/arc-inference-$ARC_PROBE.json")
  ARC_DEADLINE=$((SECONDS + 180))
  while :; do
    curl -fsS "$ARC_RPC/community/reward_receipt/$ARC_REWARD_TX" \
      > "/tmp/arc-receipt-$ARC_PROBE.json"
    ARC_STATUS=$(jq -er '.status' "/tmp/arc-receipt-$ARC_PROBE.json")
    [ "$ARC_STATUS" = mined_success ] && break
    case "$ARC_STATUS" in
      pending_mined_receipt) ;;
      mined_failed|receipt_unavailable)
        echo "reward ended in $ARC_STATUS; no ARC is confirmed" >&2
        exit 1
        ;;
      *) echo "unknown reward receipt state: $ARC_STATUS" >&2; exit 1 ;;
    esac
    [ "$SECONDS" -ge "$ARC_DEADLINE" ] && {
      echo "receipt did not mine inside the 180-second recording bound" >&2
      exit 1
    }
    sleep 3
  done
done
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

The two probes are sequential on purpose: they prove two distinct successful
jobs mined in two distinct block heights and exactly 5 ARC gross. They do not
prove a daily rate. A rate and projection require at least three successful
mined receipts spanning at least 24 hours, so both fields must remain null with
the canonical `collecting data` reason here. Do not start probe 2 until probe 1
has reached `mined_success`, as the loop above enforces.

## 2:05–2:40 — prove two payments, not just submission

```bash
for ARC_PROBE in 1 2; do
  jq -e '
    .status=="mined_success" and .tx_type=="0x25" and
    .submitted==true and .included==true and
    .confirmed==true and .success==true and
    .receipt_url==( "/community/reward_receipt/" + .tx_hash ) and
    .reward_base==2500000000 and .reward_arc==2.5
  ' "/tmp/arc-receipt-$ARC_PROBE.json" >/dev/null
  jq '{status,tx_type,tx_hash,job_id,worker,model_id,input_hash,output_hash,
       recovery_epoch,validator_set_id,transaction_domain,
       validator_approvals,submitted,included,confirmed,success,
       block_height,block_hash,index,receipt_url,
       reward_base,reward_arc,evidence_source}' "/tmp/arc-receipt-$ARC_PROBE.json"
done

jq -s -e '
  (map(.tx_hash)|unique|length)==2 and
  (map(.job_id)|unique|length)==2 and
  (map(.block_height)|unique|length)==2 and
  (map(.block_hash)|unique|length)==2 and
  (map(.worker)|unique)==[env.ARC_WORKER]
' /tmp/arc-receipt-1.json /tmp/arc-receipt-2.json >/dev/null

curl -fsS "$ARC_RPC/worker/earnings/$ARC_WORKER" \
  | tee /tmp/arc-earnings.json \
  | jq '{address,onchain_balance_arc,confirmed_receipt_count,
         confirmed_gross_earnings_base,confirmed_gross_earnings_arc,
         confirmed_receipts,attestations_per_day_observed,
         attestations_per_day_unavailable_reason,projected_daily_arc,
         projected_daily_unavailable_reason,projection_policy,
         reward_issuance_policy_hash,reward_budget,
         issuance_ready_for_worker,reward_program,
         reward_is_customer_demand,
         reward_per_attestation_base,reward_per_attestation_arc,
         recovery_epoch,validator_set_id,stake_zero_eligible,
         archive_mode,history_complete_since_recovery,history_scope,history_domain,
         last_reward_block,last_reward_tx_hash}'

jq -e '
  (.address|ascii_downcase|ltrimstr("0x"))==(env.ARC_WORKER|ascii_downcase|ltrimstr("0x")) and
  .archive_mode==true and .history_complete_since_recovery==true and
  .history_scope=="complete canonical reward history since the v3 recovery boundary" and
  .history_domain=="all canonical 0x25 reward domains since the v3 recovery boundary; historical rows retain their own recovery_epoch, validator_set_id, and transaction_domain" and
  .confirmed_receipt_count==2 and
  .confirmed_gross_earnings_base==5000000000 and
  .confirmed_gross_earnings_arc==5 and
  .attestations_per_day_observed==null and
  .projected_daily_arc==null and
  .attestations_per_day_unavailable_reason==
    "collecting data: a projection needs at least 3 successful mined reward receipts spanning at least 24 hours, not the initial one or two rollout canaries" and
  .projected_daily_unavailable_reason==
    "collecting data: a projection needs at least 3 successful mined reward receipts spanning at least 24 hours, not the initial one or two rollout canaries"
' /tmp/arc-earnings.json >/dev/null
```

The 180-second bound is a recording stop condition, not an expected settlement
SLA. Block cadence, assignment, and validator approval latency can make the
wall-clock run longer; retain honest timestamps and edit only after both
receipts exist. The earnings response must retain both 2.5 ARC receipts,
reconcile to exactly 5 ARC gross in this fresh canary window, and report null
rate/projection fields with the canonical collecting-data reason. Do not show
or narrate a numeric daily forecast from these two receipts.

Narration:

- `onchain_balance_arc` is the address’s actual current chain balance.
- `confirmed_receipt_count` counts only successful, mined
  `CommunityInferenceReward` receipts visible in this node’s retained index.
- `confirmed_receipts` provides the rows, including exact block and
  recovery-domain bindings, that sum to `confirmed_gross_earnings_base`.
  Archive lifetime history begins at the initial v3 recovery boundary and
  intentionally spans all later recovery epochs, so mixed per-row
  `recovery_epoch`, `validator_set_id`, and `transaction_domain` values are
  valid historical evidence rather than a mismatch.
- `projected_daily_arc` appears only when an explicit active policy, confirmed
  receipt rate, funded treasury, and remaining block/epoch/worker/coordinator
  budgets support it. Otherwise it stays null and
  `projected_daily_unavailable_reason` says why. It is a capped testnet
  promotional subsidy projection, not evidence of customer demand or revenue.

The receipt must say `status: mined_success`, `confirmed: true`, `success:
true`, and `tx_type: 0x25` before the amount is earned. Then open the same hash
in the production dashboard, paste `ARC_WORKER` into **Your ARC earnings**, and
show **5 ARC** under **Mined reward ARC** with **2 successful retained 0x25
receipts**. The rate and projected/day cards must remain unavailable with the
collecting-data reason. Paste either reward hash into **Inspect a receipt**,
then open the same hash in the composite explorer and show its matching block
on the same canonical source. This is the browser-friendly close of the
headless demo; it does not require copying the server's signing identity into a
desktop app.

Protocol v3 rejects standalone `InferenceAttestation` (`0x16`) submission; a
legacy historical `0x16` pays nothing. The worker certificate is embedded and
reverified inside `0x25`. If the hash is null, pending, pruned, a different
type, or unsuccessful, the reward is not proven—wait or say so plainly.

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
