# Exact tree speculation (ENG-8)

Kimi-class models are served on ARC's network split across many nodes, so every
forward pass crosses the network. Per-answer speed is then set by how many
tokens each traversal yields, not by how fast one node computes. This layer
lets a drafter propose a tree of candidate continuations and has the sharded
target verify the whole tree in one pass, while the output stays byte-identical
to plain greedy decoding.

Code: `crates/arc-inference/src/modern/serving/tree.rs` (trees, drafters,
verification), `dense.rs` (shared-node tree forward), `mod.rs`
(`BatchModel::forward_tree`, `SeqKv::keep_path`). Measurement:
`examples/tree_speculation.rs`, run by `.github/workflows/tree-speculation.yml`
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

On the network the difference matters: every row crosses every hop as a hidden
state. On the measured SmolLM3 trees the shared form sends about 2.7× fewer
rows than path lowering (30 vs 83 rows per pass, Studio lab table below).

## Drafters

| Drafter | Source of candidates | Needs |
|---|---|---|
| `LookupTree` | n-gram matches in the prompt and output (prompt lookup), several matches as branches | nothing |
| `RecycleTree` | token recycling: the target's own top-k candidates after each token, from every prompt row and every verified node, accepted or not, grown best-first | nothing; the last stage returns `top_k` ids per row |
| `RecycleTree` with `lookup` ("hybrid") | lookup branches with n ≥ 2 first, then recycling | nothing |
| `head_tree` | ranked per-depth candidate heads (Medusa/EAGLE layout) | trained heads (not included) |
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

## Measured acceptance

The real-model job (`tree-speculation.yml`, Ubuntu x86-64, 4 threads) runs nine
authored fixtures: three coding, three agent transcripts, three chat. It
checks every output against plain greedy generation, then records tokens per
verification pass. The CI numbers are in the PR and in the job's
`tree-speculation-evidence` artifact.

Studio lab (Apple M2 Ultra, 16 threads), same package
(`sha256 19c67496…aa91`), defaults (depth 8, lookup 32 nodes, recycle 48
nodes, top-k 8, hybrid lookup n ≥ 2). Drafter settings were chosen on the
first two cases of each category. The `-holdout` cases were added afterwards.

| Traffic | Drafter | Decode tokens | Passes | Tokens/pass | Rows/pass (shared) | Rows/pass (path-lowered) |
|---|---|---:|---:|---:|---:|---:|
| coding | lookup | 123 | 65 | 1.89 | 6.5 | 6.8 |
| coding | recycle | 123 | 52 | 2.37 | 38.1 | 97.8 |
| coding | hybrid | 123 | 48 | 2.56 | 30.4 | 86.2 |
| agent | lookup | 124 | 82 | 1.51 | 6.8 | 7.2 |
| agent | recycle | 124 | 68 | 1.82 | 32.5 | 80.6 |
| agent | hybrid | 124 | 62 | 2.00 | 31.6 | 83.4 |
| chat | lookup | 140 | 134 | 1.04 | 5.2 | 5.6 |
| chat | recycle | 140 | 86 | 1.63 | 28.4 | 73.0 |
| chat | hybrid | 140 | 86 | 1.63 | 27.4 | 71.0 |

All 27 outputs were byte-identical to plain greedy decoding.

Tree size vs tokens per pass (hybrid, Studio lab, all nine cases):

| Recycle nodes | Coding tokens/pass | Agent | Chat | Rows/pass (coding / agent / chat) |
|---:|---:|---:|---:|---|
| 8 | 1.98 | 1.51 | 1.14 | 5.0 / 5.0 / 3.4 |
| 16 | 2.16 | 1.70 | 1.30 | 10.5 / 9.3 / 7.5 |
| 32 | 2.41 | 1.91 | 1.56 | 20.5 / 21.4 / 18.8 |
| 48 (default) | 2.56 | 2.00 | 1.63 | 30.4 / 31.6 / 27.4 |
| 64 | 2.67 | 2.07 | 1.61 | 43.5 / 39.9 / 34.5 |

Returns flatten past about 32 nodes. On slow links, fewer rows per pass can
beat more tokens per pass; the harness's bandwidth table shows the tradeoff.

## Per-answer speed on the network (projection)

`tok/s = tokens per pass ÷ (network traversal + compute)`, with one sequential
traversal of all hops per pass. Tokens per pass come from SmolLM3 and are
measured. Everything else is assumed, and Kimi's own acceptance is
unmeasured. The harness prints the full grid of 8–60 hops at 10–60 ms per hop,
with 0 or 50 ms compute. It also adds the time to serialize each pass's rows on
every hop as raw Kimi `i64` hidden states (7,168 × 8 bytes per row) at 1 and
10 Gbit/s.

The arithmetic sets the target. At 8 hops × 10 ms with 50 ms compute, plain
decoding gives 7.7 tok/s, and 59 tok/s needs 7.7 tokens per pass. At 16 hops ×
30 ms it needs 31. Training-free drafters reach 1.6–2.6 tokens per pass on
SmolLM3. Closing the gap needs short, fast-linked islands (few hops) and
trained drafters (EAGLE-3-style heads). The tree interface and verification
here are already exact for those.

## Not done

- No trained EAGLE/Medusa heads and no small draft model with SmolLM3's or
  Kimi's tokenizer. Training them would be new spend.
- No Kimi weights through this path (ENG-5 plugs in via `BatchModel`).
- No live network measurement; hop latency and bandwidth are assumptions.
- The fixtures are authored, not sampled production traffic.
