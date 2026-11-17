# How work and sharding flow

Checked against 8949910b (v0.8.20) on 17 Nov 2026. Notes marked **v0.8.19**
say where a0265630 differs. Line numbers are at 8949910b unless a note says
otherwise.

The previous version of this document (last changed 2 Sep 2026) is in git
history at
[8949910b](https://github.com/FerrumVir/arc-chain/blob/8949910bdfd079bdef18d6a41b62f6838acf60b7/docs/HOW-SHARDING-WORKS.md),
for history only. For the repo layout, checks and invariants, start with
[AGENTS.md](../AGENTS.md).

This document describes code. It does not say what any network runs.

Terms:

- **Coordinator:** a node that receives `POST /inference/run` and hands out
  the job.
- **Community worker:** a stake-0 node whose loaded model has every layer in
  the canonical INT8 profile (`main.rs:9189-9206`). `--full-integer-worker`
  is the explicit way to start one, and the desktop app uses it when sharing
  is on.
- **Validator:** a staked node that holds model layer ranges and recomputes
  results to verify them.

Paths are under `crates/arc-node/src/` unless they start with another
top-level folder.

## The short answer

- A community job is never split. One worker runs every layer for the whole
  job (`main.rs:435-446`, `8461-8467`, `9189-9206`). The job carries the
  whole prompt and
  no layer range (`rpc.rs:14626-14664`).
- Twin execution, when an operator turns it on, runs the same job on two
  workers and compares them. It duplicates a job; it does not divide it.
- Layer sharding happens only among validators. They use it to verify
  community results, to serve `/inference/run` on a coordinator that lacks
  the full model, and for the sharded endpoints listed under
  [Layer ranges and the pipeline](#layer-ranges-and-the-pipeline).
- Each coordinator runs at most 2 public jobs at once (`rpc.rs:179`). More
  workers raise throughput only up to that limit and do not shorten one job.
- `--auto-shard-join` is off by default and only plans a range.
- Desktop share-by-default, new in v0.8.20, adds whole-job workers. It does
  not change how a job is split.

## One job's path

1. **Entry.** `inference_run` handles `POST /inference/run` (`rpc.rs:2763`,
   `8212-9004`). A caller can mark a request `public_demo`
   (`rpc.rs:8259-8262`, `8458-8486`). The demand pump also makes jobs (see
   [The demand pump](#the-demand-pump)).
2. **Readiness.** The coordinator uses community workers only when its
   readiness snapshot allows it (`rpc.rs:8051`, `8494-8503`). If results
   cannot be verified right now, it serves the job without them.
3. **Twin first.** A public demo (which needs `--community-twin-execution`
   and 2 ready workers, or it returns 503; `rpc.rs:8467-8483`), or any job
   while twin execution is on and at least 2 workers are live, goes to
   `dispatch_twin` (`rpc.rs:8508-8572`; `rpc/twin_dispatch.rs:780-782`). If
   the pair cannot be queued, a normal job falls back to one worker and a
   public demo returns 503 (`rpc.rs:8551-8562`). A twin failure after
   queueing returns 504 and does not fall through (`rpc.rs:8563-8571`).
4. **One worker.** Otherwise `dispatch_to_community_worker_with_probe`
   queues the job for one worker (`rpc.rs:7473`, `8574`). If it is refused
   before queueing, declined or withdrawn, the coordinator goes on to step 5.
   A failure after queueing returns 504 and does not fall through
   (`rpc.rs:8649-8690`, `8674-8686`).
5. **Coordinator.** A public demo never runs on the coordinator
   (`rpc.rs:8699-8706`). A coordinator that holds only some layers sends the
   job through the validator pipeline (`rpc.rs:8759-8780`). One with the full
   model runs it itself (`rpc.rs:8783-8790`).

### Choosing the worker for a single job

**v0.8.19** added this ranking. `rpc/community_routing.rs` does not exist at
v0.8.18 (f9a29727).

- Workers long-poll `POST /community/claim_work` for up to 30 seconds
  (`rpc.rs:14587`, `17457`). A worker that already holds a job or an open
  claim on that coordinator gets `worker_busy` (`rpc.rs:17522-17533`).
- The coordinator trusts no worker-reported timing
  (`rpc/community_routing.rs:1`). It ranks workers by its own measured
  seconds per position, an EWMA with weight 0.2
  (`rpc/community_routing.rs:53-60`; `rpc.rs:1209`). Workers it has never
  assigned come next, then the rest (`rpc/community_routing.rs:186-205`).
- It holds a new job for up to 1.5 seconds for the chosen worker
  (`rpc/community_routing.rs:6`, `280`). Every 4th decision tries a worker
  it has never assigned, if there is one (`rpc/community_routing.rs:201-203`,
  `239`).
- Slow results and timeouts count against a worker
  (`rpc/community_routing.rs:62-87`).
- Twin legs and targeted probes skip the ranking
  (`rpc/community_routing.rs:231-234`).

### Limits

- **2 public jobs per coordinator** (`rpc.rs:179`, `1916-1918`). Single-worker
  dispatch, twin dispatch, local runs and both validator pipeline entry
  points each take one permit (`rpc.rs:7533-7541`, `8783-8790`, `12786`,
  `13353`; `rpc/twin_dispatch.rs:979-987`). With none free, the caller is
  told to retry after 2 seconds.
- **1 job per worker.** A worker polls every coordinator it knows in
  parallel, keeps the first job and declines the others
  (`main.rs:9540-9545`, `9554-9639`).
- **90-second registry entries.** Workers send a heartbeat every 15 seconds
  and register again every fourth round (`main.rs:5641-5642`). An entry
  expires 90 seconds after the last heartbeat or claim (`rpc.rs:1014`,
  `17517-17520`).

### Verification

- Validators first check the claimed tokens teacher-forced, all positions
  through the layer pipeline at once, with the same 2-signature quorum per
  range. If that cannot settle the claim, they recompute the output
  sequentially through the pipeline (`rpc.rs:15316-15319`, `15471-15589`;
  `rpc/teacher_forced.rs:1-2`, `254-303`).
  **v0.8.19** added the teacher-forced path; `rpc/teacher_forced.rs` does not
  exist at v0.8.18.
- Each range is checked on up to 3 replicas and needs 2 distinct validator
  signatures that agree. A range with fewer than 2 distinct replicas fails
  the plan (`rpc.rs:6870-6876`, `15353-15375`).
- Each validator runs one reward-approval recomputation at a time and admits
  at most 6, the running one included; a 7th gets 429 (`rpc.rs:201-202`,
  `15946-15966`).

## Twin checks

Twin execution v0 is set per coordinator, is off by default and changes no
consensus rule (`rpc/twin_dispatch.rs:1-8`; `twin.rs:149-176`).

- **Two legs, one job.** `dispatch_twin` queues 2 legs of the same job, or 1
  leg to replay a verified reference (`rpc/twin_dispatch.rs:970-1176`,
  `1054`). The legs run in parallel and get half the dispatch budget
  (`rpc/twin_dispatch.rs:1046-1049`).
- **Pairing.** The two legs need distinct node keys (`twin.rs:353-370`). A
  leg may wait up to 3 seconds for an idle worker that differs more in
  region and platform, and, for generated demand only, for one served less
  recently (`twin.rs:61-66`, `398-399`, `404-426`, `435-482`). Operator
  and network-group checks exist, but those facts are not collected yet
  (`twin.rs:330-341`).
- **Comparison.** Legs must match on output hash, token count and text
  digest (`twin.rs:520-544`, `579-611`). After the first leg finishes, its
  sibling has 30 seconds to be claimed (`rpc/twin_dispatch.rs:18-20`).
- **When validators recompute.** On a mismatch, a reward or a spot check. If
  a leg is missing, a caller job falls back to single-worker verification and
  generated demand is dropped (`twin.rs:620-649`). Spot checks select 50 per
  mille of matched groups without a reward by default, keyed by a coordinator
  secret, at most 6 an hour (`twin.rs:48`, `651-666`;
  `rpc/twin_dispatch.rs:32-33`). One twin recomputation runs at a time per
  coordinator, and others wait up to 600 seconds
  (`rpc/twin_dispatch.rs:31`, `272`). A spot check never waits: it is
  skipped when a recomputation is already running
  (`rpc/twin_dispatch.rs:1599-1608`).
- **Public demo.** Requests marked `public_demo` are limited to 60 an hour,
  burst 6, and 64 output tokens (`rpc/twin_dispatch.rs:34-35`; `twin.rs:58`).

### The demand pump

`--community-demand-pump` (off by default) makes jobs for idle workers
(`main.rs:536-539`; `rpc/twin_dispatch.rs:1295-1326`).

- It ticks every `--community-demand-interval-secs` (default 120, minimum
  30), with 20% jitter either way (`twin.rs:50-52`;
  `rpc/twin_dispatch.rs:2077-2082`).
- A tick runs only while no caller job holds a permit, and only after the
  previous pump job has finished (`rpc/twin_dispatch.rs:2101-2112`). The pump
  is capped at 30 groups an hour, burst 2 (`rpc/twin_dispatch.rs:36-37`,
  `269`).
- Each job is a twin demo of one of 32 fixed public prompts, which needs
  twin execution on and 2 idle workers, or a replay of a verified reference.
  A replay runs on 250 per mille of ticks (a constant), or whenever a
  reference exists and a twin demo is not possible. References come only
  from verified pump demos, so the pump dispatches nothing while twin
  execution is off. Pump jobs generate at most 32 tokens (`twin.rs:54-56`,
  `114-147`, `894-912`; `rpc/twin_dispatch.rs:703-709`, `1839-1843`,
  `2131-2153`).
- `--community-demand-dry-run` logs what each tick would do and dispatches
  nothing (`rpc/twin_dispatch.rs:2116-2129`).

## Layer ranges and the pipeline

Validators hold layer ranges. A coordinator builds a pipeline from fresh,
signed shard announcements and walks it hop by hop.

- **Ranges.** A node loads each `--shard-range START:END` it is given and
  announces each one separately (`main.rs:465-472`, `8342-8351`). The planner
  uses six balanced ranges; for 32 layers they are [0,6) [6,12) [12,17)
  [17,22) [22,27) [27,32) (`rpc.rs:19356-19368`). In production mode the
  recovery tooling requires exactly 3 holders of every layer and 15-17 layers
  per validator (`scripts/recovery/recovery_rollout.py:143-145`,
  `2138-2142`, `2157-2168`).
- **Announcements.** Every 15 seconds a node announces each held range to
  every configured coordinator origin, from `--community-rpc-url` and
  `--shard-hosts` (`main.rs:5251`, `7308-7325`, `9091-9117`). Each
  `POST /shards/announce` is signed for its destination (`main.rs:9091-9094`)
  and authenticated on arrival (`rpc.rs:13838`). A
  coordinator drops an announcement after 60 seconds (`rpc.rs:1159`).
- **Assembly.** `assemble_pipeline` groups announcements by range, keeps one
  per validator identity, drops unroutable addresses, orders replicas by
  latency and covers the model greedily from layer 0 (`rpc.rs:1595-1610`,
  `1617-1725`). The latency EWMA has weight 0.2 and goes stale after 300
  seconds (`rpc.rs:1209`, `1222`, `1288-1318`).
- **Walk.** `run_pipeline` streams prompt positions through the hops, so
  during prefill each hop works on a different position. Decode is
  sequential: each new token passes every hop once (`rpc.rs:11989-11998`,
  `12041`, `12214-12282`).
- **Wire format.** Each hop is a `POST /inference/forward_shard` carrying a
  `ForwardShardRequest`: request id, the model commitment and execution
  profile, a token
  or a hidden state with its BLAKE3 hash, the position and the layer range
  (`rpc.rs:117`, `9882-9917`). Validators wrap it in
  `ValidatorForwardShardRequest` (`rpc.rs:9919-9924`). Unknown fields are
  rejected, and a request may be at most 4 MiB (`rpc.rs:9854`).
- **Per-validator capacity.** 3 shard computations at once, and at most 36
  admitted in total, the 3 running included. A waiting request gives up
  after 120 seconds (`rpc.rs:185`, `194-195`, `10575-10600`).

| Entry point | What it does | Code |
|---|---|---|
| Community verification | Needs 2 agreeing signatures per range, from up to 3 replicas | `rpc.rs:15471-15589` |
| `POST /inference/run_sharded` | One pass. `redundancy` 1-5 (default 1) fans out but needs only 1 answer | `rpc.rs:12771-13246`, `12839-12843`, `13009-13016` |
| `POST /inference/run_consensus` | `k` replicas per hop (default 3), majority `k/2 + 1`, clamped to the replicas it can reach | `rpc.rs:13338`, `13403`, `13456-13464` |
| `POST /inference/auto` | Prefers the pipeline when one covers the model | `rpc.rs:19481-19612`, `19519`, `19535` |

## Row cohorts

Row cohorts split the matrix rows of one native request across machines that
the node's own operator controls (`row_cohort.rs:1-16`;
`crates/arc-inference/src/tensor_parallel.rs:1-7`). They are set with
`--native-row-workers` (`main.rs:283-290`).

- Every row a vote depends on comes from this node or from one of the
  operator's listed machines over a pinned-host-key transport. A failed or
  faulty slice is recomputed locally, so the output equals local execution
  unless an operator machine returns wrong rows that no check sampled
  (`row_cohort.rs:5-11`).
- They are private by design: no public listener and no validator capability
  (`crates/arc-inference/src/tensor_parallel.rs:3-7`;
  `crates/arc-inference/src/row_service.rs:1-5`). They apply to native
  requests, not to community jobs.
- The code marks its multi-machine comparison as not yet passing
  (`row_cohort.rs:15-16`).

## Auto-shard-join and its flag

`--auto-shard-join` asks a coordinator to plan a layer range for this node at
boot (`main.rs:591-602`).

- **Off by default.** It runs only when the flag is set, the node is staked,
  a model is loaded and no range is given (`main.rs:7552-7558`). It posts to
  the first coordinator origin, from `--community-rpc-url` (or
  `ARC_COMMUNITY_RPC_URLS` or the config's `[community] rpc_urls`) and then
  `--shard-hosts`. With none it does not join, because P2P peers are not RPC
  origins (`main.rs:7279-7325`, `7564-7567`, `6185-6190`).
  `--full-integer-worker` refuses it
  (`main.rs:6017-6023`).
- **It only plans.** `POST /shards/join` is an unauthenticated planner that
  answers `"registered": false` (`rpc.rs:19388-19468`, `19425-19430`,
  `19463`). The node then loads the range and announces it like any other
  (`main.rs:7583-7591`). With no assignment it loads the full model
  (`main.rs:7592-7597`).
- **v0.8.20 planner** (`plan_shard_join`, `rpc.rs:19328-19374`). It fills the
  largest gap and ends the gap at the next occupied boundary, so it never
  partly overlaps a holder. When every layer is covered, it replicates the
  fresh on-grid range with the fewest holders, lowest start first. If no
  fresh on-grid range exists, it returns 409 (`rpc.rs:19417-19423`).
- **v0.8.19 planner** (inline in `shard_join`, a0265630
  `rpc.rs:19360-19428`). It took the longest uncovered run. When everything
  was covered, it assigned `total_layers / 4` layers starting at the
  least-covered layer. For 32 layers that is an 8-layer range that need not
  match the grid.

## What share-by-default changed

Desktop paths here are under `desktop/src-tauri/src/`; `shared/` is at the
repo root.

**v0.8.19.** The desktop shared compute only after an explicit yes, or for an
older install already in worker mode with a model (a0265630
`commands.rs:958-965`).

**v0.8.20:**

- At startup, an install with an identity, no recorded answer and no
  pending v0.7 question, on an eligible machine, is turned on
  (`commands.rs:978-1012`; `lib.rs:635-643`). An install not yet sharing gets
  consent, a core cap of a quarter of the logical cores (at least one) unless
  a count is already stored, and a one-time notice (`commands.rs:1008-1011`,
  `1024-1028`, `1336-1403`, `1425-1430`). An install already in worker mode
  with a model only has consent recorded: it keeps its cores and gets no
  notice (`commands.rs:1003-1007`).
- A recorded refusal never changes (`commands.rs:978-980`, `1000-1002`).
  Machines below the 16 GB nominal worker floor stay observers
  (`commands.rs:993-998`, `4968-4973`; `hardware.rs:8`).
- A model download started by the default is skipped on battery or on a
  network reported as metered; an unknown state proceeds. Downloads the user
  starts are not gated
  (`commands.rs:1320-1333`; `background_promotion.rs:215-231`).
- A new opt-in setting, `keep_awake_while_sharing` (`commands.rs:1405-1417`;
  `lib.rs:645-647`), holds off idle sleep while a worker runs on mains power
  (`sharing_awake.rs:21`). On Linux it blocks idle only, while the per-job
  hold blocks idle and sleep (`shared/keep_awake.rs:41-51`).

**Unchanged.** A sharing desktop still starts the node with
`--community-mode --full-integer-worker` and every production RPC origin
(`node_manager.rs:3550-3556`; a0265630 `node_manager.rs:2353-2355`). Each
desktop is one more whole-job worker. More desktops do not split jobs.

## What gets faster with more nodes, and what does not

| More of | Effect | Mechanism |
|---|---|---|
| Community workers | More jobs at once, up to 2 per coordinator | Each public job holds one of the coordinator's 2 permits and uses one worker (`rpc.rs:179`) |
| A faster worker | Lower latency for the jobs routed to it | Routing prefers the lowest measured seconds per position (`rpc/community_routing.rs:186-205`) |
| Workers, for one job | No speed-up | One worker runs every layer (`main.rs:8461-8467`); a twin runs the same job twice (`rpc/twin_dispatch.rs:1054`) |
| Workers, for generated demand | No more jobs | One pump job at a time, only while no caller job runs, at most 30 an hour (`rpc/twin_dispatch.rs:36-37`, `2101-2112`) |
| Coordinators | More jobs at once in total | Each has its own 2 permits; a desktop worker registers with every production origin (`desktop/src-tauri/src/node_manager.rs:3553-3555`) |
| Validators or replicas | More verification capacity and failover, not faster jobs | Up to 3 replicas per range are asked (`rpc.rs:6873`, `15353-15375`); one approval recomputation at a time per validator (`rpc.rs:201`) |
| Layer ranges (hops) | Prefill overlaps; each decode token crosses more hops | `rpc.rs:11989-11998`, `12214-12282` |
| `redundancy` on `run_sharded` | Redundancy, not speed | Fans out but needs 1 answer (`rpc.rs:13009-13016`) |
| Row cohort machines | Splits one native request, on the operator's own machines only | `row_cohort.rs:1-16`; `main.rs:283-290` |

These limits are code constants or design choices, not network settings:

1. 2 public jobs per coordinator (`rpc.rs:179`).
2. 1 job per worker (`rpc.rs:17522-17531`; `main.rs:9540-9545`).
3. Whole-model jobs with no layer range (`rpc.rs:14626-14664`).
4. Sequential decode across hops (`rpc.rs:11997-11998`).
5. 1 pump job at a time, 30 an hour (`rpc/twin_dispatch.rs:36-37`,
   `2110-2112`).
6. 1 approval recomputation per validator, at most 6 admitted
   (`rpc.rs:201-202`).
7. 1 twin recomputation per coordinator (`rpc/twin_dispatch.rs:272`).

### Worker counters and the scoreboard

- `GET /workers/scoreboard` scores a worker as success rate times 1000 minus
  its average milliseconds per job, or 0 with no successes
  (`rpc.rs:9345-9358`, `9411-9425`). It skips expired workers
  (`rpc.rs:9402-9404`).
- The scoreboard does not drive routing; routing keeps its own measurements
  (`rpc/community_routing.rs:53-60`). The comment at `rpc.rs:9354-9357`
  predates that.
- Counters live in memory and reset when the node restarts. A twin leg
  updates a worker's counters only while the worker has a registry entry
  (`rpc/twin_dispatch.rs:41`, `1807-1818`).

## Flags and defaults

Every node flag here has the same default at v0.8.19; only the
`--auto-shard-join` help text changed (`main.rs:593-601`). The two desktop
settings marked new did not exist at v0.8.19.

| Flag or setting | Default | Code | Notes |
|---|---|---|---|
| `--stake` | 0 | `main.rs:75-84` | 0 means an observer or community node |
| `--full-integer-worker` | off | `main.rs:435-446` | Needs stake 0, a model and a community RPC URL; no ranges (`main.rs:5989-6025`) |
| `--shard-range START:END` | none | `main.rs:465-472` | Repeatable. `--shard-start` and `--shard-end` are deprecated (`main.rs:448-463`) |
| `--community-mode` | off; implied by stake 0 | `main.rs:474-482`, `9139` | |
| `--no-community` | off | `main.rs:568-580` | No registration, heartbeat or claims |
| `--community` | off | `main.rs:557-566`, `7048-7072` | Stake 0, community mode, model discovery or download |
| `--community-rpc-url` | none | `main.rs:128-139` | Also `ARC_COMMUNITY_RPC_URLS`; never taken from P2P peers (`main.rs:7276-7283`) |
| `--community-twin-execution` | off | `main.rs:524-529` | |
| `--community-twin-spot-check-per-mille` | 50 | `main.rs:531-534`; `twin.rs:48` | At most 6 an hour |
| `--community-demand-pump` | off | `main.rs:536-539` | |
| `--community-demand-dry-run` | off | `main.rs:541-544` | |
| `--community-demand-interval-secs` | 120 | `main.rs:546-548`; `twin.rs:50-52` | Minimum 30 |
| `--community-region-probe` | off | `main.rs:550-555` | Signed round trips; only a coarse region is published |
| `--community-release-on-verification` | off | `main.rs:497-506` | |
| `--community-verify-profiles` | `legacy-split-half` | `main.rs:508-522` | Also `COMMUNITY_VERIFY_PROFILES` |
| `--auto-shard-join` | off | `main.rs:591-602` | See [Auto-shard-join](#auto-shard-join-and-its-flag) |
| `--prevent-sleep-during-jobs` | off | `main.rs:582-589` | The desktop passes it only when "Keep awake while a job runs" is on (`desktop/src-tauri/src/node_manager.rs:3564-3568`) |
| `--threads` | 0 (rayon default) | `main.rs:604-613` | The desktop passes its core cap (`desktop/src-tauri/src/node_manager.rs:2287-2290`) |
| `--native-row-workers` | none | `main.rs:283-290` | Row cohorts |
| `ARC_BATCHED_PREFILL` | on for stake-0 workers unless `0` | `main.rs:5116-5120`, `9460-9465` | Off by default in the library (`crates/arc-inference/src/canonical_prefill.rs:5`) |
| Desktop sharing default (new) | on for eligible installs with no answer | `desktop/src-tauri/src/commands.rs:978-1012` | Quarter of the cores for a new sharer with no stored count; existing workers keep theirs (`desktop/src-tauri/src/commands.rs:1003-1028`) |
| Desktop `keep_awake_while_sharing` (new) | off | `desktop/src-tauri/src/commands.rs:1405-1417` | Mains power only (`desktop/src-tauri/src/sharing_awake.rs:21`) |

## Where in the code

| Topic | Code |
|---|---|
| `/inference/run` routing | `rpc.rs:8212-9004` |
| Single-worker dispatch and speed routing | `rpc.rs:7473`; `rpc/community_routing.rs` |
| Claim and submit | `rpc.rs:17457`, `17758` |
| Community registry | `rpc.rs:13962` (section), `community_list` at `14520` |
| Verification | `rpc.rs:15302-15589`; `rpc/teacher_forced.rs:67`, `231` |
| Twin rules and orchestration | `twin.rs`; `rpc/twin_dispatch.rs` |
| Pipeline assembly and walk | `rpc.rs:1617-1725`, `12003` |
| Sharded entry points | `rpc.rs:12771`, `13338`, `19481` |
| Shard announce and join | `rpc.rs:13833`, `19333`, `19388` |
| Worker role checks and claim loop | `main.rs:5989-6025`, `9554-9639` |
| Auto-shard-join client | `main.rs:6177-6234`, `7549-7599` |
| Row cohorts | `row_cohort.rs`; `crates/arc-inference/src/tensor_parallel.rs` |
| Desktop sharing | `desktop/src-tauri/src/commands.rs:966-1035`, `1336-1445`; `desktop/src-tauri/src/node_manager.rs:3550-3568` |
| Scoreboard | `rpc.rs:9358` |
