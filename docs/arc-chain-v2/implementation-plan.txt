# ARC Chain V2 — completion implementation plan

Draft for TJ — September 19, 2026. Planning only; no production changes are authorized by this document itself.

## Baseline and completion scope

Use the inspected source commit `f616705d6058b75543f6a894dfb7254410055453`, the seven-file local patch in `readiness-fixes.patch`, and the four component evidence reports in this directory. `planning-baseline.json` covers that pre-documentation seven-file patch with the exact source SHA, working diff hash and changed-file hashes; the added planning documents do not invalidate those runtime patch hashes. Re-read only files needed for the next change. At execution start, verify that manifest, fetch GitHub once and inspect the delta if main has advanced; do not silently restart the audit or overwrite newer work. Commit the reconciled, tested baseline on an isolated implementation branch before concurrent work begins.

Completion means the recovered public network, block explorer, desktop/headless node product, and native on-chain inference work together under real load and recover from tested failures. This includes correct request authorization, model execution, settlement/refunds, receipt-backed earnings, signed distribution, and update/restart behavior. Source compiling or a web bundle loading does not close a native-product gate.

The initial supported execution profile must be explicit and reproducible. Use the existing reviewed CPU integer path and pinned production model artifact as the starting candidate, subject to correctness and quality acceptance. Do not switch to an easier toy model to declare the product complete. Other model families, GPU backends, bridges, contracts and dormant L1 transaction families need a supported-feature inventory; unfinished features must not be advertised as working. This plan does not assume every dormant transaction family should be re-enabled with inference.

The last observation showed six advancing nodes agreeing at a sampled block, inference readiness false on every node, an absent v0.8.0 release, and native Tier-1 transactions deliberately disabled. These are dated observations, not permanent status claims.

### Clarified requirement: automatic assignment across heterogeneous resources

TJ requires hardware-, model- and network-independent participation and automatic assignment of contributed compute. Implement an extensible protocol/runtime with capability-aware scheduling: a common interface cannot erase device memory limits, unsupported model operators, arithmetic differences, unavailable weights, or network delay. Support and deterministic conformance must be demonstrated per model/backend/profile combination before assignment.

Required components:

- **Model adapter/package contract:** immutable weights, architecture/operator graph, shapes, tokenizer/config, arithmetic profile, partitionable operations and required collectives, memory/KV estimates and permitted output semantics. New architectures extend adapters; an unrecognized model fails explicitly instead of being forced through the Llama path.
- **Worker capability admission:** authenticated identity, backend/driver/kernel versions, actual available RAM/VRAM, supported operators/profiles, benchmarked compute/memory throughput, concurrency limits, supported artifact partitions, availability lease and resource reservations. Client-advertised capacity needs measured validation and periodic refresh.
- **Network-aware placement:** measure relevant worker-to-worker latency, bandwidth, jitter and failure rates, including relay costs and reachability. Group tightly coupled partitions where communication is cheap; allocate bigger independent work units where it is expensive. Scheduler policy remains portable across transports but uses observed transport performance.
- **Automatic assignment:** choose eligible workers and unequal partition sizes according to measured speed/memory, current load, warm model residency, transfer cost and deadline. Bind the resulting model/profile/partition membership into the authorized job. More workers are used only when projected benefit outweighs additional costs or they are necessary to fit the model; equal one-eighth slices are not assumed for unequal devices.
- **Failure/reassignment:** use leases, reservations, bounded queues, checkpoint/replay rules, cancellation and idempotent settlement. Worker churn changes assignments through the defined authorization protocol; it must not silently change the quorum or mix incompatible KV/cache state.
- **Admission and cost feedback:** compare predicted and observed performance and refine scheduling without mutating an active job's deterministic semantics. Expose unsupported, unavailable, queued and degraded states honestly. Hardware-independent participation does not guarantee every device a role in every query.

**Integer-model portability requirement:** use a versioned common integer-operator contract, model import/adaptation layer, portable reference executor and validated device backends. Bind operator behavior, tensor shapes, scale/zero-point rules, rounding, accumulator width/overflow behavior and supported sampling to the execution profile. An INT-weight file alone does not prove the whole model executes with integer arithmetic or that two backends agree. Cover new architectures through adapters/operators without redesigning job assignment or settlement. Device acceleration is eligible only after conformance; use a supported reference fallback where the host can run it.

Add integration cases for mixed device speeds/memory, incompatible operators/profiles, cold versus warm weights, LAN/WAN placement, slow/failed links, NAT/relay paths, resource contention, dishonest capacity claims and worker dropout. Automatic assignment must place different useful pieces of one query where supported and show the complete accounting for duplicated verification work.

### What must happen now and what can be staged later

Define model/operator/backend capability contracts, assignment identity and settlement interfaces now so the first working path does not hard-code away the universal integer-model goal. Keep the small communication/partition feasibility experiment early and bounded. The full heterogeneous tensor scheduler and broad adapter/backend coverage can follow the first complete request-to-receipt path on a controlled supported profile. Explorer, native app, signing/release preparation and restart/finality work can proceed independently while this scheduler matures. A staged release must state its actual supported models/hardware; the complete heterogeneous work-sharing objective remains open until it passes its own acceptance gates.

The targeted current-source review is [hardware-assignment-audit.md](hardware-assignment-audit.md). It finds a deterministic planner with coarse fixed buckets/declared capacity, advisory auto-plan endpoints, whole-prompt community dispatch, authenticated shard announcements and local latency routing, but no wired global placement optimizer, tensor collectives or safe midstream KV reassignment. Startup's 32-layer/canonical-INT8 assumptions must become model/profile inputs. Replace name-based replica deduplication with authenticated participant identity. These are substantive executor/scheduler deliverables, not merely deployment switches.

## What parallel inference means

There are two separate schedules: engineering tasks can run concurrently, and the production inference executor can run concurrent work.

**TJ's clarified primary performance objective:** divide the useful computation of each individual query among multiple nodes, each executing a different share. Concurrent copies of the whole model and higher throughput across unrelated queries do not satisfy this objective. Eight workers each doing roughly one eighth of the useful arithmetic is a target decomposition; eightfold end-to-end speedup is an unproven ideal, not an acceptance claim. Verification duplicates and communication must be accounted for separately.

For a standard transformer, this calls for evaluating tensor/intra-layer parallelism, potentially combined with the existing layer pipeline. Each worker computes a different slice of a layer's matrix operations; collective communication combines needed partial results before dependent operations continue. The model generally requires these exchanges throughout execution, not only one final assembly. Sequential layer or token dependencies do not rule out accelerating each layer's arithmetic across workers.

For execution, use a bounded asynchronous scheduler with isolated request state:

Reuse existing concurrency: `crates/arc-node/src/rpc.rs:9327` implements concurrent replica fanout, and `:9692` implements prompt-prefill stages connected by bounded channels. Prompt positions can already overlap across stages while each stage preserves its own position/KV order. The existing decode path remains ordered. These are source capabilities, not evidence that the current incomplete live topology is ready. The dormant validator executor also has a one-compute permit; raising concurrency requires measured memory/CPU headroom rather than removing that protection.

1. Independent user requests run concurrently within CPU/RAM/token limits; queue and reject excess work explicitly rather than allowing unbounded tasks.
2. Independent authenticated replicas run concurrently. For a model stage requiring agreement, dispatch to the eligible replicas together, validate each signed response, and advance only after the defined matching quorum. Request IDs, model/profile identity, stage/range, token position and input commitments bind responses to the work being verified.
3. Consecutive transformer stages for one token retain their causal order. Reuse the existing prefill overlap across known prompt positions, then add overlap across independent requests or safe microbatches: while a later stage handles request A, an earlier stage can handle request B. One request's next autoregressive token still depends on its prior token. Preserve per-request KV-cache isolation and stage-local position ordering across batching, cancellation, retries and restart.
4. Full-model committee members can execute independently in parallel if capacity permits. Sharded execution is a supported alternative only after complete authenticated topology, exact execution profiles and end-to-end equivalence are demonstrated. Do not assume shard-only validators can also run the full-model committee path.
5. Tensor/intra-layer parallelism is now a primary feasibility gate for the clarified single-query speed goal. Profile communication and synchronization on the actual intended hosts before committing to a large implementation. Compare isolated/local compute with the real network placement; WAN transfers may eliminate the compute savings. The existing layer pipeline alone is insufficient evidence of single-query speedup.

Verification replication and model partitioning solve different problems. Report useful output tokens/second, completed requests/minute, p50/p95 latency, memory and CPU use, network bytes, and resource cost per successful verified request. Three replicas repeating a computation are not three times the useful throughput.

The native on-chain flow is: signed request committed -> consensus-bound executor/committee authorization and model profile -> parallel worker execution -> authenticated votes -> deterministic on-chain finalization or timeout/refund -> durable receipt and balance reconciliation. Heavy model computation must not block consensus, RPC or block application. An HTTP answer alone does not prove this lifecycle completed.

## Execution sequence and parallel tracks

| Batch | Work that can proceed together | Dependency / handoff |
|---|---|---|
| 0 — Freeze baseline | Preserve and review prepared security/explorer patches; capture supported-feature and release inventories; identify available test hardware | One exact source SHA, lockfiles and test commands |
| 0b — Prove useful single-query work sharing | Small deterministic partitioned-matmul and representative transformer-block experiment on 1/2/4/8 workers where hardware is available; profile actual interconnect | Different workers compute different pieces; identical output; report useful compute, synchronization, transfer and verification costs separately. Decide whether target hardware/topology can support useful latency gains before full executor work |
| 1 — Specify interfaces | Inference authorization/state design; consensus/finality design; snapshot design; product acceptance fixtures; build/signing preflight | Astra resolves protocol interfaces once; Luna implements harnesses and routine preparations |
| 2 — Implement | Inference state/worker pipeline; local persistence snapshots; consensus liveness/finality; explorer/app lifecycle work | Separate modules and branches; serialize changes to shared state/type/RPC files |
| 3 — Integrate privately | Isolated six-node network; real model/profile; multi-request parallel load; crashes, disagreement, refund and replay cases | Protocol versions, persisted data and product RPC contracts agree |
| 4 — Package and canary | Build supported platforms in parallel; signature verification; native app/headless canaries | Exact accepted binaries and compatibility/migration plan |
| 5 — Release and prove | Controlled coordinated deployment, artifact publication, product publication, live canaries and soak | Every gate below passes; deployment record binds actual source and artifacts |

Engineering parallelism does not remove the request -> execution -> settlement dependencies. Native packaging cannot certify functionality absent from the target network, and a production rollout cannot precede the isolated failure tests.

Batch 1 produces one versioned protocol/interface decision record and a compatibility matrix covering inference admission/committee, escrow/refund, model profile, finality transcript, snapshot format, activation and RPC lifecycle. Each decision includes an acceptance fixture. Test harness and packaging preparation can start independently; Tracks A/B/C must conform to this contract before their integration changes land. This is a technical consistency gate rather than a new team-wide approval workflow.

### Track A — Native inference protocol

Primary surfaces: `crates/arc-types`, `crates/arc-state`, `crates/arc-node/src/rpc.rs`, `inference_validator.rs`, and node runtime-role selection.

- Write one protocol decision record specifying signed request fields, chain/recovery domain, nonce/replay rules, exact model and execution profile, input/output commitment format, token/compute limits, expiry, deterministic committee selection and authenticated membership, and quorum/disagreement rules. Do not introduce an unproven VRF or let coordinators select arbitrary signers. Decide whether an existing reviewed selection mechanism can be retained or a new one is needed.
- Define atomic escrow/reservation, vote admission, finalization, fee distribution, treasury limits where applicable, exactly-once payment, and timeout/refund. Define whose work earns which payment; native paid requests and community incentives must reconcile without accidental double payment.
- Specify bounded result storage, output availability, certificate size and verification cost. Use canonical commitments and bounded evidence; do not place an unbounded per-token/per-range transaction stream into the chain. Model identity must include the source artifact and execution profile, with tokenizer/configuration and generation parameters bound to the request.
- Bind activation to an explicit protocol version/height and migration contract. Implement semantic validation consistently at every transaction ingress and state application. Re-enable the disabled Tier-1 family only after these invariants are tested.
- Restore a bounded background executor with durable request tracking, cancellation, backpressure and idempotent retry. Restart must preserve requests, balances, finalized outputs and refunds.
- Exercise actual signed requests through multiple validators using the real model path. Include unauthorized votes, duplicate signers, wrong profile/artifact, cross-domain replay, mismatched outputs, stalled committees and expired requests. No result or payment should depend on one coordinator's assertion.

Done: a real signed request completes execution, authenticated agreement and canonical settlement; explorer and app show the same receipt/balance. Negative and restart cases conserve funds and cannot finalize twice.

### Track B — Parallel model execution and topology

Primary surfaces: integer model loader/executor, shard registry and authenticated dispatch in node RPC, worker scheduling, and deployment topology.

- Inventory real CPU/RAM/disk capacity and the model artifact/profile from current deployment evidence. Select a production profile that fits with node/OS headroom and passes quality checks.
- Replace wildcard/stub shard origins with unique authenticated routable identities. Prove all model layers are covered by contiguous ranges with the required independent replica coverage. Readiness must reject stale, incomplete, wrong-profile or unreachable membership.
- Preserve simultaneous replica dispatch and add bounded cross-request concurrency, queue limits and fair scheduling. Specify whether batching changes token arithmetic; prove equivalence where the profile permits it. Isolate KV caches and make cancellation/retry cleanup reliable.
- The existing free `/inference/run_consensus` route enables `allow_degraded_quorum` (`rpc.rs:11108`). A paid path must use the fixed authenticated assignment/quorum, rejecting insufficient replicas or disagreement. A majority of whatever replicas happen to respond cannot silently become payment authorization. Cached/free/degraded responses retain their truthful status and cannot mint a new reward.
- Add a benchmark that compares sequential request serving with concurrent/pipelined execution on the same hosts, model, prompt set, token limits and verification policy. Measure completed valid outputs and resource use, including failed work and retries.
- Add the primary latency benchmark: one identical query on 1/2/4/8 participating workers where supported, with useful model operations partitioned across workers. Record worker traces proving different slices, exact output equivalence, time to first token, decode time/token and complete-query latency. Report both compute-only and fully verified latency, total resource use and the same security policy for fair comparisons. Throughput gains across independent requests do not close this gate. Set the attainable speedup target from the bounded hardware experiment rather than inventing an eightfold guarantee.
- Run cross-platform deterministic vectors on the pinned production GGUF plus semantic/quality evaluation against the defined reference. Synthetic KAT success alone does not close this gate.

Use a complete-model route as a correctness/performance reference and, if useful, an isolated settlement test harness. It is not the primary execution architecture for TJ's clarified goal. Prioritize the measured model-parallel route and its authenticated partition evidence. Benchmark verification as a separate cost: repeatedly recomputing the full query on every worker would defeat the intended useful-work partition. The security design must specify which computations are replicated, proven or independently checked and what trust assumptions remain; determinism alone is not a cheap proof of correctness.

Done: real requests overlap in traces, all required replicas authenticate and agree, the full model executes, and the recorded throughput/latency/resource comparison supports the published performance claims. Hardware capacity constraints produce a clear queue/unavailable state instead of false readiness.

### Track C — Consensus, finality and restart stability

Primary surfaces: consensus engine, signed protocol types, persistent state/WAL, node startup and recovery tooling.

- Design the signed view-change/leader-skip and committed-block finality transcripts together. Specify lock/commit rules, domain separation, quorum intersections, equivocation handling, replay and restart behavior, and protocol activation. The existing all-validator guard stays in force until its replacement has demonstrated safety and liveness.
- Implement deterministic multi-node simulation and failure injection before enabling the new path: one of six unavailable, delayed/reordered messages, duplicate/equivocating messages, partitions, leader changes and node rejoin. Do not infer Byzantine safety from only an all-honest run.
- Export verifiable finality certificates over the correct committed-block transcript; never relabel existing signatures over other DAG blocks as finality signatures. Make clients distinguish observed blocks from cryptographically verified finality.
- Add atomic local snapshots with network/recovery identity, state root and a durable WAL cursor. Bound tail replay, preserve receipts/escrows/indexes and anti-equivocation signing state, and define safe retention. Validate interrupted writes, corrupt snapshots, truncated tails, disk failure and restore/restart equivalence.
- Combine restart tests with continued chain progress and catch-up. Faster loading alone does not satisfy tolerance of an offline validator.

Done: isolated six-node tests maintain safety and authorized progress through the agreed fault cases, fresh nodes can verify finality, and restart recovers the same state without replaying unbounded history. Production receives the tested compatible protocol only through the controlled rollout.

### Track D — Explorer, desktop and headless products

- Land the prepared dependency and latest-block fixes after full applicable CI; retain checkpoint, common-height agreement and source identity checks. Investigate gateway/source mismatch and recurring replica drift instead of weakening evidence checks.
- Explorer: cover live block pagination/detail, a successful transaction, address history/balance, inference request progression, finalized/refunded result and payment receipt, deep links and reloads. Prove maintenance, stale source, fork/disagreement, malformed input and missing-data behavior. Historical data must respect recovery boundaries and available archive evidence.
- A real transfer/faucet receipt can close generic transaction/address navigation. Native inference settlement and mined `0x25` community-reward receipts close separate product gates; neither is assumed to produce the other automatically. Test fixtures cannot close a real economic acceptance gate.
- Desktop/headless: verify first install, persistent identity and private recovery, managed versus external process ownership, start/stop/restart, reconnect, model download verification/load, capacity/readiness, request submission and cancellation, earnings reconciliation and actionable failures.
- Wire frontend state machines to the finalized protocol/RPC contract. Test loading, queued, executing, voting, finalized, rejected and refunded states; a transaction hash or local result cannot imply payment.
- Prove update behavior using real native packages: signed metadata, correct platform/version, rejected bad signatures, interruption/rollback where supported, identity preservation and node compatibility. Test clean install and upgrade on every advertised platform; report unsupported targets explicitly.

Done: real packaged products complete the user journeys against the accepted network, including successful receipts and failure recovery. Browser mocks, frontend builds and updater unit tests remain supporting evidence only.

### Track E — Release, deployment and operations

- Validate the repository's complete expected asset list against the release contract; the current record calls for 32 assets. Verify the exact list from the selected release source, signatures, checksums, SBOM/provenance, and updater migration rules.
- Bind running binaries and gateways to source SHA, binary digest, model/profile, genesis/checkpoint and protocol version through verifiable provenance. Reconcile direct checkpoint recovery with the new release receipts honestly.
- Prepare a concrete rollout/compatibility plan, backup/restore evidence, exact target artifacts and rollback limits. Consensus/state-format changes may require forward recovery or coordinated migration rather than blind binary downgrade.
- Run native and headless canaries, then release through the existing protected workflow and publish the accepted explorer/app configuration. Production mutations must follow the applicable reviewed recovery/deployment procedure and any required approval; no restart is performed as a diagnostic experiment.
- Use deterministic monitoring for block finality age, agreement, replica drift, peer loss, restart duration, memory/disk, inference queue/latency, success/refund rates and payment reconciliation. A new recurring automation template requires TJ's approval under the project rule before enabling it.
- Proposed stability acceptance: a 24-hour recorded soak with representative parallel inference traffic and controlled failure/rejoin tests first performed on the isolated network. If a failure changes code or configuration, rerun the affected acceptance window. Distinguish successful command execution from sustained behavior.

Done: exact signed artifacts are available, installed canaries match them, the public explorer/app and network pass acceptance together, and an operator can follow the tested runbook for failure recovery.

## Model and usage discipline

V2 is a planning and product label, not an automatic protocol-v2 migration. The current budget target is up to 5 additional account-wide weekly percentage points; that is a resource target and gives no completion guarantee. The observed account-wide meter was 65% used on September 19, versus 60% in the earlier planning observation; other active tasks mean no portion of that difference can be attributed to this work. Work should proceed in small bounded batches, with a fresh execution baseline and a meter check before each new batch. Review at approximately +2 points and pause new work before +5 points; the shared rounded meter cannot enforce a precise per-task hard cap.

Native supported-profile inference execution through authenticated verification, settlement and refund is already included in this scope. Prioritizing it now changes sequencing, not scope. Universal heterogeneous integer-model work sharing is a larger final V2 objective; its cost is uncalibrated until the bounded feasibility experiment, so no usage estimate or completion claim should be invented.

- Astra Ultra: coordination, protocol/interface decisions, difficult correctness issues and focused review of high-risk changes. Keep its inputs to a short contract, relevant diff and failing evidence.
- Luna: routine implementation, fixtures, build/test execution, platform/package work, documentation and targeted debugging. Escalate a specific blocker after a bounded attempt rather than silently expanding the audit.
- At most three worker lanes alongside the coordinator. No two workers edit the same shared protocol/state/RPC surface concurrently. Use one integrator for shared files; cross-track contracts make independent work possible.
- Keep one ledger of task ID, exact commit, changed files, test command/result and unresolved issue. Reuse build caches and existing test evidence until a changed dependency invalidates it. Run focused tests during development, the required full matrix at integration, and repeat only failures or affected checks.
- Keep a small-batch ledger of task ID, exact commit, changed files, test command/result and unresolved issue. Check the fresh shared meter before starting each batch and stop before the additional +5-point target is reached. If other tasks move the counter, attribute no unsupported per-task cost.
- No repeated archive/history searches. Any new search names the current blocker and ends when the required fact is found. Long builds run without model polling loops; retain logs and summarize their relevant result once.

## Release decision checklist

All required gates must pass: exact running provenance; healthy converged chain with tested fault recovery; native signed request/vote/finalization/refund; complete authenticated model topology/profile; production-model correctness and measured parallel performance; durable reconciled receipts; successful explorer/app/headless journeys; signed supported-platform artifacts and updates; and the recorded stability soak.

Open decisions are deliberately confined to Batch 1: secure committee selection/quorum, supported model/profile and hardware capacity, view-change/finality protocol, activation/migration, and supported platform/feature inventory. These require bounded source review and experiments; an honest plan cannot replace those proofs with a promise. Their outputs are written contracts that unlock the parallel implementation tracks.
