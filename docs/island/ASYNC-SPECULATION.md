# Asynchronous pipelined speculation on the regional ring (prototype)

Status: draft prototype, stacked on ENG-6 (#167). Code:
`crates/arc-inference/src/modern/mla/island/speculative.rs` (coordinator and
drafters), `worker.rs` (rollback, early cancellation, clone-free draft trees),
`model.rs` (`StageModel::truncate_cache`). CLI `arc-island spec-bench`. CI
`.github/workflows/async-speculation.yml`. Nothing here changes the profile's
arithmetic, the canonical model, consensus or rewards, and nothing talks to a
node.

## Why

On a regional ring every token of plain decoding pays every hop: one position
of an answer is on the ring at a time. The planner (`arc-swarm-planner`
REPORT-v2.1) and the literature review (PipeInfer, SpecPipe/PipeDec, and the
2026 bit-exact consumer-laptop pipeline that measured 3.1x at about 16 ms per
hop) point to pipelined speculation as the main lever when hops dominate.
Before this change `Coordinator::verify_tree` needed an idle ring, and each
stage cloned the prefix cache for every draft-tree node.

## Design

**Passes in flight.** `Coordinator::run_speculative(requests, config,
drafter)` keeps up to `depth` (D) passes of one answer on the ring, each of
`rows` (R) positions. Pass `i + 1` carries drafted tokens that continue pass
`i`'s drafts and leaves before pass `i` returns. One code path covers three
regimes: D = 1, R = 1 is plain decoding (the drafter is never called);
D = 1, R = k + 1 is synchronous chain speculation (k drafts verified per round
trip); D > 1 is asynchronous pipelined speculation. The first pass is the
prompt (one item, exactly as `Coordinator::run` sends it); every later
position travels as its own item, so the last stage commits a selection at
every position, as it does when it decodes that position alone.

**Shared-prefix KV branches, no copies.** All passes of an answer extend the
sequence's one live cache on every stage. The speculative branch is the
suffix beyond the verified length. `Frame::Rollback { seq, keep }` makes every
stage keep the first `keep` positions of the sequence's cache, forwarded
tokens and activation log, in place (`StageModel::truncate_cache` truncates
each layer's latent and RoPE-key vectors; entries are only ever appended, so
the kept prefix is byte-identical). Rollbacks are written to the activation
log and replayed on restart. Draft trees (`Frame::Tree`) now use the same
primitive: a stage evaluates the nodes depth first on the prefix's own cache,
truncating back to each node's parent, and restores the prefix exactly
afterwards, even on failure. No stage clones a cache any more.

**Cancellation.** When the target's selection at position `p` differs from
the draft at `p + 1`, every position from `p + 1` on is dead. The coordinator
marks every later pass cancelled (their results are ignored when they
return), sends `Rollback { keep: p + 1 }`, and continues at once from the
target's token. Links and stages are first-in first-out, so every stage
applies the rollback after the dead passes and before the replacement. A
stage reads every frame that has already arrived before computing the next
one: if a rollback is queued behind a dead item, the item is marked
`superseded` and passed on without being computed (PipeInfer's early
cancellation; `worker::skip_superseded`). The answer's end works the same
way: positions sent past an EOS are rolled back before the sequence is
closed, so stage logs hold exactly the plain-decoding positions.

**Drafters.** `speculative::Drafter` is a pluggable trait:
`draft(context, prompt_len, max)` returns up to `max` tokens to follow the
prompt, the verified output and the drafts already in flight. Drafts are
untrusted: the coordinator truncates over-long drafts and stops at the first
out-of-vocabulary token. Two implementations:

* `NgramDrafter`: prompt-lookup / suffix drafting with no weights. It finds the
  most recent earlier occurrence of the context's last n tokens (longest n
  first, 4 down to 1) and proposes what followed, continuing through its own
  proposals when the match runs into the end of the context (a repeating
  cycle extends itself).
* `ScriptedDrafter`: a test drafter with controllable acceptance. It holds the
  plain-decoding output of each request and proposes the target's token where
  its `Script` says so (`Rate { rate, seed }`: an independent, reproducible
  hash per output index; `Pattern`: a cyclic accept/reject pattern) and a
  different token elsewhere. Every mode of a benchmark sees the same
  acceptance pattern.

## Correctness argument

The engine is integer-only, so a position's per-layer activation hashes,
logits hash and selection are a function of the weights, the input token and
the cache contents at that position. The coordinator records a result into
the answer's ledger only at a position `q` whose input token is verified (it
was confirmed by the target's selection at `q - 1`, or it is the target's own
token) and whose cache holds only verified positions: any position computed
after a rejected draft is cancelled, and the stages truncate it before the
replacement arrives. Every recorded position therefore carries exactly the
commitment plain decoding produces there, the last stage's repetition-penalty
history is truncated with the tokens, and the output is the target's tokens
in order. This holds for any drafter, acceptance pattern, depth, pass width
and stage split; drafts change speed, never bytes.

Tests (CI, never run locally):

* `island::tests::speculation::speculation_matches_plain_decoding_for_any_drafter_depth_and_split`:
  threads, every contiguous split of the 4-layer model (all 8 for one format,
  3 for the other two formats), shapes (D, R) in {(1,1), (1,2), (1,4), (2,1),
  (4,1), (3,2), (6,3)}, eight drafters (always right, always wrong,
  alternating, two-in-three, rate 0.5, n-gram, garbage with over-long and
  out-of-vocabulary drafts, silent), requests with EOS ids and both selection
  rules. Tokens, every logits hash and every boundary digest equal the single
  process's `generate`; the whole ledger (every stage's hashes, logits hash
  and selected token at every position) equals plain decoding's on the same
  ring.
* `speculative_answers_pass_every_stage_audit`: after speculative answers that
  keep their logs, every stage's reveal re-executes to its commitments
  (`audit_all` gives `Valid` for every stage).
* `rollbacks_truncate_in_place_and_replay_from_the_log`: a rollback truncates
  every stage, is logged, and a restarted stage replays items and rollbacks to
  the same state; continuing gives the bytes of a ring that never saw the
  draft. Unknown sequences are ignored; growing or a closed cache is refused.
* `queued_rollbacks_mark_the_items_they_discard` and
  `a_stage_skips_work_a_queued_rollback_discards`: early cancellation marks
  exactly the dead items and the serve loop skips them while computing the
  replacement exactly.
* `model::tests::truncated_caches_continue_like_fresh_ones`: logits and cache
  bytes after truncate-and-extend equal a cache that never held the dropped
  positions.
* `speculative::tests`: the n-gram drafter continues the latest match and
  extends cycles; the scripted drafter follows its script on the target's
  path only, and its rate is close to nominal.
* `tests/island_processes.rs::speculative_stage_processes_match_plain_decoding_and_audit`:
  four stage processes over TCP with emulated 1 ms hops; synchronous and
  pipelined shapes with a scripted and the n-gram drafter match the single
  process and plain decoding's ledgers, and every stage audits clean.
* The existing draft-tree test now exercises the clone-free tree path.

## Measurement

`arc-island spec-bench` uses ENG-6's emulated-WAN harness: separate stage
processes on one host over loopback TCP, every stage's outgoing hop shaped by
`ShapedTransport` (one-way delay with 10% jitter, a bounded uplink, Kimi-width
activation padding of 28,672 bytes per position), fresh processes for every
run so stage counters cover exactly one run. For each island it measures
plain decoding (`Coordinator::run`, one answer at a time), synchronous
speculation (k = 3 and 7) and pipelined speculation (R x D = 1x8 and 2x8)
with scripted drafters at acceptance 0.5, 0.7 and 0.9 and the n-gram drafter.
Every answer is checked byte for byte against the single process, and every
ledger against plain decoding's; any difference fails the run.

```sh
arc-island synth --shape small --out small.arcspkg
arc-island spec-bench --package small.arcspkg --out spec.json --hop-ms 16 \
  --stages 4,8 --uplink-mbit 100 --kernel simd --threads 1 --stage-threads 1
python3 scripts/arc_island/spec_report.py spec.json --out spec.md
```

CI runs one bench job per hop delay (1, 5, 16 and 50 ms one-way per shaped
hop; 4 and 8 stages at 100 Mbit/s, 8 stages at 1 Gbit/s) plus a 40-stage
regional cell (RTT 5/10/20 ms), and the `report` job renders one table into
the run summary and the `spec-report` artifact. **These are emulated
measurements on CI runners**: a synthetic model, one host, shaped loopback
links. They are not real-WAN or Kimi speeds, and the measured tables live in
the pull request and the CI artifacts, not here.

## Limits

* **Test models.** The synthetic tiny MLA/MoE models run one position at a
  time on each stage (no batched kernels), so a pass of R rows costs R times a
  row and rows never share weight reads. The MoE expert-union effect of a real
  batched verifier (the union of the experts every row picks grows the bytes
  per pass) is modelled in the planner, not measured here.
* **One answer at a time.** `run_speculative` needs the ring to itself; mixing
  speculative answers with batched streams (`Coordinator::run`) is not built.
  Speculation buys per-answer latency with ring capacity: cancelled positions
  are computed (or skipped) work that batched serving would use.
* **Cancellation cannot overtake.** On a first-in first-out ring a rollback
  travels behind the dead passes; a stage skips dead work only when the
  rollback is already in its queue (when it is the bottleneck). A direct
  control channel from the coordinator to every stage would cancel earlier.
* **Drafter cost and placement.** Scripted and n-gram drafters cost almost
  nothing. A model drafter (the K2.6 EAGLE-3 head) adds its step time and needs
  hidden states from three stages; with passes in flight it drafts on
  features one round late.
* **Greedy only.** Sampling would need drafter-invariant coupling (shared
  randomness per position) to keep outputs independent of the drafter.
* **Recovery.** Rollbacks are logged and replayed, but in-flight recovery is
  still not built, and replica relays journal rollbacks like any other frame
  without a dedicated speculation test.
* **Trees stay synchronous.** `verify_tree` no longer clones caches but still
  needs an idle ring; pipelined passes are chains.

## Next steps

1. Measure the K2.6 EAGLE-3 head's acceptance on ARC traffic and its draft-step
   time on the coordinator's GPU, then plug it in as a `Drafter` (with feature
   taps from the stages holding layers 2, 30 and 58).
2. Batched verification kernels (ENG-1) so a pass's rows share weight reads,
   then measure the expert-union cost per pass on Moonlight and K2.6 routing.
3. A control channel for early cancellation, adaptive depth and pass width
   (fall back to plain decoding when measured acceptance or uplink capacity is
   low), and tree-shaped passes in flight.
4. Speculative answers alongside batched streams on one ring, with admission
   that budgets the cancelled work.
5. Real community nodes over real WAN links, where relay queueing matters
   (the S6 paper warns emulated numbers are a bound).
