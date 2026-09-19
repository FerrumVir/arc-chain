# ARC inference/network readiness audit

Audit date: 2026-09-19. Checkout: `f616705` (`main`). Read-only live probes only; no transaction or production mutation was attempted.

## Current verdict

The recovered v3 network is presently live and converged for empty-block chain progress, but inference is deliberately dark and the public model topology is not usable. There is no native on-chain inference request/result/reward path available today. The working public behavior is a read-only chain plus disabled or non-rewarding HTTP inference surfaces.

## Live evidence

- All six HTTPS gateways in `shared/frontend/arc-network.json` returned `/health` 200, `version=0.8.0`, five peers, six validators, `chain_advancing=true`, and fresh blocks. `/network/info` returned protocol `3.0.0`, `recovery_active=true`, recovery epoch/set `1/1`, active stake `40,000,000`, and `is_block_producing=true`.
- At common height `631000`, all six returned the same block hash `356eeef04911d88524afd8d3cac4fb4b03698bb016221143e5309e178cbaf093`, timestamp, parent, empty transaction root, and recovery checkpoint state root `d103671a855f22414f4c9546eb329d6e1d1c5c1676781583900fe28622061c33`. This proves current convergence at that sampled height, not an independently consumable finality certificate.
- `/sync/status` was available on all six. `/inference/attestations` and `/inference/results` were empty on all six; `paid_success_count=0`.
- Every node's `/inference/readiness` returned `safe_to_dispatch=false`, `community_dispatch_ready=false`, `local_model_ready=false`, `sharded_pipeline_ready=false`, and `live_community_workers=0`.
- Every node's `/models` reported one model (`0x934efc12…`), 6176 MB nominal full size, only 15–17 of 32 layers covered, `fully_covered=false`, `profile_bound=false`, and INT8 profile. `/shards` exposed only three local ranges per gateway, with `socket_addr=http://0.0.0.0:9944` and the same `node_name=arc-68b19960` in each observed response; no complete routable multi-node pipeline was visible.
- `/provenance` and `/version` returned 404 on live v3 gateways. `/maintenance/status` identifies the retired legacy HTTP origins as unreachable and carries `source_main_commit=047e625…`, not this checkout's `f616705`; binary/source provenance is therefore unproven through the public API.

## Native on-chain inference status

1. The only Tier-1 submit endpoint is fail-closed: `crates/arc-node/src/rpc.rs:6162-6178` returns `503 paid_inference_unavailable` before reading state, signing, or touching the mempool. The same error says authenticated replica payouts and Tier-1 committee/VRF authorization are not production-ready (`rpc.rs:2399-2405`).
2. Generic signed transaction ingress rejects `InferenceEscrowOpen`, `InferenceRequest`, `InferenceVote`, and `InferenceFinalize` (`rpc.rs:2886-2920`).
3. Protocol-v3 state admission allows only transfer, faucet claim, and community reward; all Tier-1 request/vote/finalize families are explicitly denied (`crates/arc-state/src/lib.rs:4597-4644`, with the v3 envelope rejection at `1914-1926`).
4. The validator task exists and can run Candle locally, but boot intentionally disables it for protocol v3 (`crates/arc-node/src/main.rs:5474-5484`). The spawn branch logs that Tier-1 is disabled while paid inference is dark (`main.rs:7629-7667`). Thus the substantial request/vote/finalize implementation is dormant on the recovered chain.
5. The HTTP coordinator path is not native on-chain inference. `/inference/run`, `/inference/run_sharded`, and `/inference/run_consensus` are worker/HTTP pipeline calls; any attestation or commitment they produce is not an on-chain finalized request/result. The source explicitly distinguishes raw attestations from mined `CommunityInferenceReward` receipts (`inference_validator.rs:386-390`).

## Consensus/finality and explorer implications

- The chain currently advances and six nodes agree at the sampled height, but `ConsensusEngine::generate_finality_proof` always returns `None` until a canonical domain-separated finality transcript exists (`crates/arc-consensus/src/lib.rs:2711-2733`). Commit support is logged as internal only because D-block signatures do not sign the committed B-block transcript (`lib.rs:1972-1983`). Explorer data can show canonical node block responses, but there is no exportable light-client finality proof to verify independently.
- The recovery record itself documents the direct checkpoint import and deferred release/provenance (`docs/recovery/v3-live-recovery.md:16-20,51-60`). `shared/frontend/production-status.json` still says public release, recovered fleet, and post-release acceptance are not all proven.
- The documented restart limitation remains material: full WAL replay with no persisted state snapshot can pause production while a validator is down (`docs/recovery/v3-live-recovery.md:51-56`). Consensus also requires full recovered-domain round participation before advancing; do not relax that without a protocol design review.

## Cheap model-efficiency readiness

- The cheap memory-saving shard strategy is not currently usable: live coverage is incomplete, no profile is bound, and all observed shard sockets are stubs. The source requires fresh profile-bound contiguous coverage before readiness (`crates/arc-node/src/rpc.rs:11343-11399`). Local registry seeding/refresh is process-local (`rpc.rs:1865-1898`), so it cannot substitute for six authenticated, routable announcements.
- The boot path intentionally skips Candle on shard holders to save roughly 4 GB (`crates/arc-node/src/main.rs:7001-7011`), while full integer/reward workers must load every layer and canonical INT8 (`main.rs:7205-7222`). A production Tier-1 committee therefore needs an exact artifact, complete model load, and enough RAM/CPU on selected validators; current shard-only deployment cannot satisfy it.
- Determinism evidence is bounded to synthetic CPU I8/I16 KATs. It does not cover a production 7B GGUF, Q4, GPU, or every CPU architecture (`INFERENCE_DETERMINISM.md:36-49`). Quality requires a fresh pinned GGUF benchmark and threshold (`INFERENCE_DETERMINISM.md:51-63`). Do not advertise cheap INT8 commitments as accurate model inference until those gates pass.

## Closure criteria / next work

1. Publish and verify an immutable v0.8.0 binary/genesis/provenance bundle; expose a signed `/provenance` (or equivalent receipt) that binds running binary digest, source commit, genesis/checkpoint, protocol version, and validator identity. Re-run the six-node provenance and checkpoint gate.
2. Preserve current strict v3 transaction policy. Before enabling Tier-1, finish a reviewed v3 admission design for request/vote/finalize, authenticated committee membership/VRF, exact model artifact binding, deterministic result storage, timeout/refund, and validator-authorized reward settlement. Add multi-node integration evidence on the recovered domain.
3. Deploy complete exact-model artifacts to the intended committee (or a reviewed canonical economical model), then prove full model coverage and a canonical execution profile on all selected nodes. Replace stub shard origins with unique authenticated routable origins and demonstrate 32/32 contiguous coverage with the required replica/quorum policy.
4. Add/export canonical finality proofs bound to the committed block transcript, and make explorer acceptance require those proofs (or explicitly label blocks as node-observed commits).
5. Add persisted state snapshots plus bounded WAL-tail replay, then fault-test one-validator restarts and catch-up without stopping production. Keep full-round participation until a certified view-change/skip protocol exists.
6. Run the exact CPU KAT matrix plus pinned production-GGUF cross-platform vectors and a quality/perplexity threshold. Record inference latency, RAM, and cost per token for the chosen full-model and shard/replica plans.

## Test attempt

`cargo test --locked -p arc-node node_runtime_roles --lib` could not start because Cargo wanted to update the lockfile. The offline retry began a broad dependency compilation in the fresh checkout and was stopped before completion to respect the light-test/no-heavy-build bound. No targeted test result is claimed.
