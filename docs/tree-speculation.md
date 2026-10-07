# Exact tree speculation (ENG-8)

This layer verifies candidate token trees in one target call and preserves
plain greedy output byte for byte. It is intended for models split across
network stages. The implementation and tests cover dense models, a portable
fallback, and an in-process two-stage pipeline; no live network or Kimi-class
throughput has been measured.

Code: `crates/arc-inference/src/modern/serving/tree.rs` (trees, drafters,
verification), `dense.rs` (shared-node tree forward), `mod.rs`
(`BatchModel::forward_tree`, `SeqKv::keep_path`). Measurement:
`examples/tree_public.rs`, run by `.github/workflows/tree-speculation.yml`
on pinned SmolLM3-3B weights.

## How a pass works

1. The drafter proposes a `DraftTree`: node 0 is the last emitted token, every
   other node names an earlier parent, siblings have distinct tokens.
2. `BatchModel::forward_tree` computes logits for every node in one call. Node
   `i` sits at position `committed + depth(i)` and attends to the committed
   cache followed by its own ancestors and itself.
3. Starting at the root, the target's selection rule (argmax or RP64) picks
   the next token from the node's logits. Verification moves to the child with
   that token if one exists, and stops at a mismatch, at EOS or at the output
   limit.
4. Only the visited path's KV is committed (`TreeOutput::commit`). Everything
   else is dropped. On any error the cache is left exactly as it was
   (`TreeOutput::discard`).

Each pass emits the accepted drafts plus one token chosen by the target, so a
pass never yields fewer tokens than plain decoding.

## Why the output cannot change

Induction over the visited path. The root's row is exactly the forward pass
plain decoding would run next. A child is visited only when its token equals
the token the target just selected, so its row has exactly the greedy prefix
and computes the greedy forward pass. Tokens, every logits hash and the
committed KV therefore equal plain decoding's for any drafter, including a
hostile one. A failure is raised only when it is on the visited path, where
greedy decoding would fail too, and with the same error. Failures of rejected
siblings never surface.

## Shared-node verification

`DenseModel::forward_tree` computes each node once. Its KV is appended in node
order, and per-node attention covers `0..committed` plus the node's ancestors
(`forward_tree_layers`, `attention_head_tree`). `attention_head_tree` is the
profile's two-pass integer attention, operation for operation; only the
visited positions differ. A test pins it to `arith::attention_head` on the
same keys laid out contiguously, including the wide `i128` branch. After
verification, `SeqKv::keep_path` moves the accepted nodes' KV into place.
Keys are rotated at `committed + depth`, which is where they land.

Any `BatchModel` that does not override `forward_tree` uses
`forward_tree_by_paths`: one sequence per root-to-leaf path in one
`forward_rows` call. That fallback duplicates shared ancestors and the prefix
cache. Pipeline stages call `forward_tree_layers` for their own layer range;
the two-stage pipeline fixture in the tests does exactly that.

Shared-node verification reduces computed and transferred rows when branches
share ancestors. Savings depend on tree shape and drafter. Historical
`422e1c88` CI found about 2.5–2.8× fewer rows for recycle/hybrid, but only about
1.05× for lookup. These are row counts, not measured network speedups.

## Drafters

| Drafter | Source of candidates | Needs |
|---|---|---|
| `LookupTree` | n-gram matches in the prompt and output (prompt lookup), several matches as branches | nothing |
| `RecycleTree` | token recycling: the target's own top-k candidates after each token, from every prompt row and every verified node, accepted or not, grown best-first | nothing; the last stage returns `top_k` ids per row |
| `RecycleTree` with `lookup` ("hybrid") | lookup branches with n ≥ 2 first, then recycling | nothing |
| `head_tree` | Cartesian tree from ranked per-depth candidate lists | candidate lists; no model or trained heads included |
| `LocalModelTree` | a small local model's greedy rollout with top-k per depth | a draft package with the target's tokenizer (not included) |

`RecycleTree` follows Token Recycling (Luo et al., arXiv 2408.08696). Its
candidates are by-products of rows the target computed anyway, so it adds no
target work and no model. Proposals are deterministic: ties break by path
cost, then depth, then insertion order.

## Proofs (tests)

All comparisons are byte for byte against `ModernModel::forward` /
`ModernModel::generate` (tokens, every logits hash, KV digest), scalar and
SIMD kernels:

- `every_tree_node_equals_its_path_alone_and_any_path_commits_exactly`: random
  trees up to 40 nodes over random prompts. Every node's logits from the
  shared-node forward and from path lowering equal the reference fed that
  node's path. Committing any node's path leaves the reference cache.
  Discarding restores the cache.
- `tree_attention_equals_attention_on_the_gathered_positions`: 200 random
  cases, including the `i128` branch, values and errors identical.
- `exact_tokens_logits_and_committed_kv_across_trees_kernels_and_stops`:
  lookup, recycle, hybrid and oracle-fed head trees, RP64 and argmax, EOS and
  output limits, depths 0–12.
- `in_place_tree_failures_stay_with_their_node`: a rejected sibling outside the
  domain is harmless. A node greedy would forward fails with `forward`'s exact
  error and leaves the cache untouched.
- `tree_generation_survives_pipeline_partition_and_microbatch_boundaries`: a
  two-stage pipeline doing shared-node trees equals the whole model, with each
  node computed once.
- `deep_branch_verification_is_one_batch_and_rejected_invalid_sibling_is_harmless`:
  one target call per tree. The fallback and the shared form agree.

## Target hidden-feature hook

`StepOutput::features` and `TreeOutput::features` expose `TargetFeatures`:
last-layer residuals **before** final RMSNorm/LM head, signed Q16 i64 values,
with source token and absolute position. Dense shared-node and path fallback
passes expose the same values. Empty features mean the model does not implement
the capability; failed rows expose none. This hook currently copies residuals
for successful rows requesting logits; it adds memory/copy overhead, included
in measured timings. Pipeline adapters can use `dense::target_features` on their
final stage's residual after row/ancestor error filtering.

`TreeDrafter::propose_with_features` receives the last **committed** row's
features and the full current token context including the pending root. The
first call receives the last prompt row. Later calls receive the last visited
node of the preceding successful verification. Rejected-node features are not
handed to the proposal. Existing drafters delegate to `propose` by default;
recycling still observes successful logits from all verified nodes, separately.

The untrained deterministic stub test consumes residual values plus the pending
token, independently checks each supplied feature against serial accepted-prefix
forward rows, and verifies tokens/logits/KV identity with wrong proposals and
head failure. Random-tree tests compare every node's features to its isolated
path under scalar/SIMD kernels. This is a target-feature integration hook, **not
trained EAGLE/Medusa**, nor an implementation of their architectures (including
EAGLE multi-layer feature selection). No training or head-performance claim.

## Public sample and measurement

`tests/fixtures/tree_public/cases.json` pins 60 public cases, 20/class:

- Coding: SWE-bench Verified issues from Django/SymPy, full issue text plus all
  old code hunks localized using the reference patch. No added gold lines or
  tests. Oracle localization and the two-project restriction limit generality.
- Agent: actual model/tool rollouts on SWE-smith synthetic repair tasks. Every
  original message/field through the first tool response is retained, including
  calls, arguments, IDs and complete observations. The source provides no tool
  schema. One shard and one stable trajectory per instance define the population.
- Chat: human English OpenAssistant conversations, complete root-to-final-user
  prefixes of at least three turns, one per conversation tree.

The pinned source revisions, SHA-256s, licensing notices and base-commit source
licenses are bundled alongside the fixture. `scripts/tree_speculation/sample.py`
reproduces selection: SHA256(`ARC-70-public-v1:` + source ID), ascending, first
20 eligible/class. Complete content must be 256–1,536 pinned-tokenizer tokens;
no messages are truncated. The harness wraps each context in a fixed-date,
no-thinking SmolLM3 user turn (transcripts serialized as JSON); it does not
claim native tool execution or evaluate task success. 128 output tokens are
allowed and EOS is honored. These are public benchmark samples, not production
logs, and the sample does not represent all traffic distributions.

All 60 cases run full target greedy generation. Only **lookup** acceptance is
replayed from prompt + already-emitted tokens; each proposed tree is fixed
before later greedy tokens score its path. Greedy traces cannot replay recycling,
which observes rejected-node logits, or feature heads. The first two sampled
cases/class form the predeclared full-tree subset: shared-node lookup, recycle,
and hybrid outputs must match all greedy tokens, every logits hash and committed
KV; replay counts must match full lookup. The same six greedy outputs are checked
against independent serial `ModernModel::generate`. The other 54 cases have
lookup replay evidence, not full-tree identity evidence. Recycling/hybrid results
are exploratory n=2/class, never reported as a 20/class sample.

The CI artifact contains flushed per-case `records.jsonl`, consolidated raw
`results.json`, and `summary.md` with per-case timings and pooled plus median,
p10/p90 acceptance. Quantiles use linear interpolation (R type 7); immediate
EOS has no decode pass and is excluded from quantiles with counts reported.
No model weights, outputs or successful tool commands are inputs to sampling.

## Timing and projections

Baseline depth-zero greedy and full trees now share **batched prefill**.
Prefill includes cache/row setup, prompt forward, feature extraction, hashes,
observer updates and first selection. Decode includes drafting, verification,
feature extraction, hashes, observer updates, commit and bookkeeping. Draft and
verify subtimes are within decode. Package loading, tokenization, serial identity
checks, text decoding and evidence writing are outside. Lookup replay time is
CPU scoring only, not target inference. Each case has one ordered trial; timings
are descriptive, with no repetition/confidence interval or throughput claim.
Historical greedy/tree totals at `422e1c88` used serial/batched prefill and are
not comparable. Studio tables were removed because raw evidence was unavailable.

Projection only: tokens/s = tokens/pass / ((hops × latency + assumed compute +
forward payload transfer) / 1000). The grid uses 8/16/32/60 hops, 10/30/60 ms,
0/50 ms compute and 1/10 Gbit/s. Forward payload counts 7,168 × 8 bytes of i64
hidden state per row per hop. Return logits/tokens, serialization and drafting
must fit inside the compute assumption; 50 ms is illustrative, does not scale
with tree rows and is not a measured compute estimate. Kimi acceptance and
lossless-codec ratios are unmeasured. Drafter-specific row reductions and these
assumptions determine projected gains; neither faster islands nor trained heads
are demonstrated necessary or sufficient to reach 59 tokens/s.

No training, new draft package, live node access, deployment, or Kimi/network
performance measurement is included.
