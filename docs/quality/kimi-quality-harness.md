# Quality harness: ARC's integer engine against the reference model (ENG-12)

**Question.** Does ARC's integer engine answer as well as the reference
model? The reference is the published model run in its native precision
(BF16 for SmolLM3-3B; the official INT4 weights or the vendor API for Kimi).
"Same quality" here means a measured paired difference on fixed benchmark
items that stays inside a tolerance fixed *before* the run.

**Status**
- The harness is runnable today on SmolLM3-3B (CI, label `quality-proof`).
- Full Kimi evaluation remains dependent on engine integration, admitted real
  weights and a matching reference. The offline one-layer synthetic prerequisite
  now executes pinned ARC and official PyTorch code without model downloads or
  API calls; see [kimi-layer-reference.md](kimi-layer-reference.md). It does not
  establish real Kimi quality. No paid reference calls were made.
- The tolerance policy is a **proposal awaiting TJ's approval**
  (`scripts/arc_quality/policy.json`, `"status": "PROPOSED"`).

## How it works

```
fetch-data ─► prepare ─► items.jsonl ─┬─► ARC engine  (arc-modern / arc-mla golden) ─► run ─► score ─┐
                          cases.json  └─► reference   (hf | openai)                 ─► run ─► score ─┴─► compare ─► report
```

1. **Items.** `prepare` draws a fixed subset from pinned public files
   (`scripts/arc_quality/data/datasets.json`: Hugging Face revision, bytes,
   SHA-256). The draw is the first N items ordered by
   `SHA-256("arc-quality-v1:<benchmark>:<id>")`, so a larger run always
   contains the smaller one.
2. **Engines.** ARC reads the cases file (`arc.modern-cases.v1`, the existing
   golden-run input) and writes its usual run file. The reference reads the
   **prompt token ids from the ARC run** (`--prompt-ids-from`), so both
   engines see identical inputs. Both decode greedily (ARC `argmax`
   selection; transformers `do_sample=False`), with the same stop token and
   the same token cap.
3. **Grading.** One decoder (the model's `tokenizer.json`) turns both token
   streams into text; the same extractor grades both.
4. **Comparison.** Per item, each engine is right or wrong. The paired 2×2
   table gives the accuracy difference (ARC − reference), its 95% interval
   (Newcombe's hybrid score method for paired proportions) and an exact
   McNemar test. Answer agreement and token-level identity are reported
   alongside.

The prompts are zero-shot and short, so absolute scores are **not**
comparable with vendor-reported numbers. Only the paired difference matters.

### Benchmarks

| group | benchmark | what is graded | items available |
|---|---|---|---|
| mmlu_pro | MMLU-Pro (TIGER-Lab, MIT), 10 options | the letter after "Answer:" | 12,032 |
| gsm8k | GSM8K test (OpenAI, MIT), step-by-step | the number after "Answer:" equals the reference answer | 1,319 |
| code | HumanEval (OpenAI, MIT) + MBPP sanitized test (Google, CC-BY-4.0) | pass@1: the program passes the dataset's tests | 164 + 257 |
| toolcall | ARC-authored tool-call set v1 (`data/toolcall_v1.jsonl`) | the JSON call names the right tool with the right arguments | 16 |

- Code is executed only with `--allow-exec`, in a child process with a
  timeout, an empty environment and resource limits. That is not a security
  boundary, so it runs on disposable CI runners only.
- **The tool-call set is a smoke test, not a standard benchmark.** Kimi's
  agentic quality needs a public set before any claim (see "Not done").

### The logit track

This is the method PR #137 used for SmolLM3:
- the same 1,024 token ids go through both engines;
- the report gives the perplexity difference and the top-1 agreement.

PR #137 measured +0.072% to +0.332% (Federal Register text, 3 BF16 jobs on CI
runners). The `logit` job reruns it, and the report folds it in. It needs
full logits from the reference: transformers locally, or a self-hosted server
with prompt log-probabilities. Chat APIs do not provide them.

## Report format

`compare` writes `arc.quality-report.v1` (JSON) and a Markdown summary.

- `overall`: `verdict` (PASS, FAIL, INCONCLUSIVE or NO DATA), `certifying`
  (true only with all certification sizes and validated evidence), `scope`,
  and `blockers` explaining missing evidence. An incomplete run cannot return
  overall PASS; known prompt mismatches return FAIL.
- `groups.<group>`:
  - `n`, `reference_accuracy`, `arc_accuracy`;
  - `delta` and `delta_ci` (fractions, ARC − reference);
  - `both`, `only_a` (reference only), `only_b` (ARC only), `neither`;
  - `discordant_rate`, `mcnemar_p` and `answer_agreement`;
  - `prompt_alignment`: matched, mismatched and unavailable input-ID counts,
    including text-only API runs that have no generated token IDs;
  - `token_level`: same prompt ids, identical generations, and the median
    first divergence;
  - `policy`: verdict, margin, certification size, and the items needed at
    the observed discordance.
- `benchmarks.<benchmark>`: the same block for each benchmark.
- `pooled`: all items together, judged on the point estimate.
- `logit_track[]`: reference and ARC perplexity, delta %, top-1 agreement and
  the ARC logits digest.
- `noise_floor`: present when a second, independent reference run is given.
- `engines`: run metadata for each side (package hash, kernel, platform,
  timing and API usage).

Every number carries a label for where it was measured, for example "CI
runner, ubuntu-latest". Per-item results stay in the `*-scored.json` files
(text, extracted answer, tokens, output hash and logits digest), so any
disagreement can be traced.

### Evidence and provenance required for certification

The full requested item manifest must be covered once on both sides. Duplicate
item IDs are rejected; missing/extra results, missing policy groups or member
benchmarks, changed item contents, or unavailable input IDs block certification.
A supplied second reference must cover and align with the same inputs too.
Prompt-ID mismatches are detected even if generated token IDs are unavailable.

`score` records a SHA-256 of each source run file and a canonical JSON SHA-256
of each item (including its prompt and grading target). For certification, pass
`score --provenance MANIFEST.json`, an audited producer manifest containing:

```json
{
  "model_id": "organization/model",
  "model_revision": "immutable weight revision",
  "weights_sha256": "<64 lowercase hex: source weight manifest digest>",
  "tokenizer_sha256": "<64 lowercase hex: tokenizer.json digest>",
  "run_sha256": ["<64 lowercase hex: actual source run file digest>"]
}
```

`weights_sha256` identifies the pinned source-weight manifest shared by the
integer conversion and native reference, not their different runtime tensor
formats. The producer must audit that lineage. `score` checks that the declared
run hashes match the files being graded and, when decoding, checks the tokenizer
file hash. `compare` checks the item fingerprints and consistent model, source
weights, tokenizer and run identities across every shard and reference. A label,
API model alias or local directory name alone does not establish provenance.
These checks verify consistency of supplied evidence, not producer authenticity.

The logit track is required by this policy. Its report must have positive finite
PPL values, positive scored-token counts, and a delta consistent with the two
PPL values. For certification, it must also include `provenance` with the same
four model identity fields and `tokens_sha256` (canonical JSON hash of the input
token list). Both `bf16_reference` and `integer_engine` must record that token
hash and matching `scored_tokens`; the ARC logits digest must be present.
Missing identities or inconsistent coverage block certification. Invalid
numeric evidence is rejected, rather than silently treated as a passing gate.

The old CI scored/PPL artifacts lack this complete provenance contract. They
remain readable as smoke evidence and are explicitly non-certifying. The current
smoke workflow does not invent or backfill producer attestations. Certification
runs need audited producer manifests and enriched PPL evidence; API token-ID
alignment remains unavailable. The policy remains unapproved by TJ. Even a
complete synthetic test fixture's PASS only exercises the proposed policy.

## Tolerance policy (PROPOSED for TJ's approval)

**Rule.** The difference d is ARC accuracy minus reference accuracy, in
points, with a 95% interval [L, U]. For each group:

- **PASS** when L ≥ −margin: ARC is no worse than the margin, with 95%
  confidence;
- **FAIL** when U < −margin: ARC is worse by more than the margin, with 95%
  confidence;
- **INCONCLUSIVE** otherwise: there are too few items to decide.

| group | margin | certification size |
|---|---|---|
| mmlu_pro | 1.5 points | 2,000 items (fixed subset) |
| gsm8k | 2.0 points | 1,319 (the full test set) |
| code (HumanEval + MBPP) | 3.0 points | 421 (both sets in full) |
| toolcall | 3.0 points | 400 (needs a public agentic set; the ARC v1 set has 16) |

The overall verdict needs **all** of these:
- every group PASSes at its certification size;
- the pooled point estimate is ≥ −0.5 points;
- the logit track's perplexity delta is ≤ +1.0%;
- the noise-floor condition holds, when a second reference run exists. ARC
  may disagree with the reference no more than 1.5× as often as the reference
  disagrees with itself, plus 1 point.

A run below the certification sizes reports the same verdicts but is labelled
"smoke". It can show a FAIL, but overall PASS requires complete evidence.
Statistical group/pooled PASS values alone never establish certification.

**Why these numbers**
- Published INT8 quantisation results report 98.7–100.3% "recovery" of the
  BF16 score (Red Hat W8A8, third-party; see research-8 §4.3). A 1.5–3-point
  margin is about that band at these accuracies.
- The margins can only be verified with enough items. What matters is how
  often the two engines disagree item by item (the discordant rate r): the
  95% half-width is about 1.96·√(r/n). Items needed, at a delta of 0:

  | discordant rate r | margin 1.0 | margin 1.5 | margin 2.0 | margin 3.0 |
  |---|---|---|---|---|
  | 2% | 769 | 342 | 193 | 86 |
  | 5% | 1,921 | 854 | 481 | 214 |
  | 10% | 3,842 | 1,708 | 961 | 427 |
  | 15% | 5,763 | 2,561 | 1,441 | 641 |

  Greedy generations diverge after the first differing token, so r is larger
  on long answers (GSM8K, code) than on one-letter answers (MMLU-Pro). That is
  why the long-answer groups have wider margins. The report prints the items
  needed at the observed r for each group.
- **A tighter claim is possible** if TJ wants one (for example, 1.0 point
  everywhere). It costs items: about 2,000–4,000 per group at r = 5–10%.

**Decisions for TJ**
1. Approve or change the margins and certification sizes above.
2. Choose the Kimi reference (below) and set its budget.
3. Choose the public agentic set that replaces the 16-item smoke set.

## Running it

From `scripts/`:

```bash
python -m arc_quality fetch-data --dir DATA
python -m arc_quality prepare --profile arc_quality/models/smollm3-3b.json \
  --benchmarks "mmlu_pro=24,gsm8k=16,humaneval=10,mbpp=10,toolcall=all" \
  --data-dir DATA --out-items items.jsonl --out-cases cases.json [--shard i/N]
../target/release/arc-modern golden --package smollm3-3b.arcipkg --tokenizer tokenizer.json \
  --cases cases.json --kernel simd --out arc-run.json
python -m arc_quality reference hf --model-dir MODEL --items items.jsonl \
  --prompt-ids-from arc-run.json --eos 128012 --out ref-run.json
python -m arc_quality score --items items.jsonl --run arc-run.json --tokenizer tokenizer.json \
  --allow-exec --label ARC --out arc-scored.json          # disposable machine only
python -m arc_quality score --items items.jsonl --run ref-run.json --tokenizer tokenizer.json \
  --allow-exec --label reference --out ref-scored.json
python -m arc_quality compare --items items.jsonl --arc arc-scored.json --reference ref-scored.json \
  --out report.json --summary-md report.md
```

**In CI**, `.github/workflows/quality-harness.yml` does the same:
- it runs on every PR that touches the harness: unit tests, the pinned-data
  check and the subset draw;
- the label `quality-proof` adds the SmolLM3-3B run (4 shards on
  `ubuntu-latest`, 76 items), the logit track and the report.

## Kimi

**When ENG-5/ENG-10 land**
1. Fill in `models/kimi-k2.6.json`: the stop token, the chat template and the
   pinned weight revision. Its values are placeholders until then.
2. Prepare the certification set: `--benchmarks
   "mmlu_pro=2000,gsm8k=all,humaneval=all,mbpp=all,toolcall=all"`. That is
   3,756 items today; it rises once a public agentic set replaces the 16
   tool-call items.
3. Run ARC's Kimi engine (`arc-mla golden`, PR #156) on the cases. The
   harness needs only its run file: `cases[]` with `id`, `prompt_tokens` and
   `tokens`.
4. Run the reference with `reference openai` against the chosen endpoint.
   - With a self-hosted server, give it the same prompt ids. vLLM accepts
     token-id prompts on the `/completions` endpoint; that adapter is a small
     addition once the server exists.
   - With a vendor API, the comparison is text-level. The vendor renders its
     own chat template. The report explicitly marks prompt alignment unavailable
     and cannot certify it under this policy.
5. Run the reference **twice**, on different days or servers, to measure the
   noise floor. API "temperature 0" is not deterministic.

**Volume of one reference pass over the 3,756 items** (estimated at 3
characters per token; `reference openai --dry-run`):
- about 0.81M input tokens;
- at most 0.69M output tokens (every reply at its cap).

**Reference options. None is free, so none was run:**

| reference | what it is | cost |
|---|---|---|
| Moonshot API or another provider serving Kimi K2.6 | vendor-served model, text-level comparison, not bit-reproducible | per-token price × 2 passes (noise floor); the price was not checked for this document |
| self-hosted vLLM/SGLang on the official `moonshotai/Kimi-K2.6` weights (native INT4 experts) | the exact published weights, token-id inputs, full logits for the logit track | GPU hours on a node class that holds ~600 GB of weights |

The `openai` adapter requires finite nonnegative prices and budget (default
0). Character/token estimates are advisory, including during `--dry-run`.
Before sending, `--context-tokens` must specify the endpoint's **enforced**
maximum input/context token count. Do not substitute an estimated prompt
length: providers render their own templates. The guard reserves that entire
input bound plus the item's positive integer output cap, with `n=1`, at the
supplied prices. Output caps above the context bound are rejected.

Every request and retry must fit the remaining budget **before** transport.
`--max-attempts` defaults to 1 (maximum 6). Each attempt reserves its full
bound; failed, timed-out and missing-usage responses never refund it. The
single-attempt transport does not retry internally. Usage exceeding a bound
or multiple returned choices stops the run before another request. Reserves
can exhaust the budget before all items complete. This guard is per
invocation, not an account spending limit. Its billing guarantee depends on
correct configured prices and an endpoint that honors its declared token
caps; an endpoint violation cannot be undone after a response.

All nonempty `--extra-body` objects are rejected, including vendor extensions:
unknown fields can affect billing, multiplicity or input alignment. Model,
messages, generation parameters and token limits cannot be overridden.
No endpoint was called to test these guards; regression tests mock transport.

## Not done, and why

- **No Kimi measurement.** The Kimi engine (ENG-5) and the weights (ENG-10)
  have not landed, and every reference option needs a budget nobody has
  approved.
- **No public agentic benchmark.** The ARC tool-call set is a 16-item smoke
  test. Before a Kimi claim it should be replaced or joined by a public set,
  for example the BFCL single-turn categories (Apache-2.0), with pinned
  files. Multi-turn agentic suites (τ-bench, SWE-bench Verified) need an
  environment runner. That is a separate piece of work.
- **The token-id prompt path for a self-hosted Kimi server** (vLLM
  `/completions` with `prompt_token_ids`) is described above but not built.
  No server exists to test it against.
- **Few-shot or chain-of-thought MMLU-Pro** (the vendor protocol) was not
  used. Zero-shot letter answers keep each item to a few hundred tokens,
  which a CPU CI runner can afford.

## Offline original-layer prerequisite

The executable synthetic one-layer comparison and retained official reference
are documented in [kimi-layer-reference.md](kimi-layer-reference.md). It exports
actual activations/logits from pinned ARC and PyTorch implementations, remains
non-certifying, and requires no model download or API call. Real-weight execution
still requires disk/RAM admission and retained original source shards.
