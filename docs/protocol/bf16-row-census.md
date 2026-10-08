# Read-only BF16 row census

`scripts/arc_mla/bf16_row_census.py` inventories retained local sources against
reviewed engine `616ba16a60f43b5e70666ca24f5d7f9ce99ab932`. It does not fetch
weights or change source files. A report is **not admission approval** and does
not implement a flush, clamp, fallback, conversion or forward-execution oracle.
No real-weight census has been performed for this change.

## Usage

Requires the existing Python conformance dependencies (`numpy`, `blake3`).
Use a trusted pinned `arc.hf-source.v1` manifest, its local `config.json`, any
pinned index, and retained safetensors shards. The manifest itself is the trust
anchor: the report includes its SHA-256, but the caller must authenticate that
pin independently. Do not regenerate the manifest to bless changed weights.

```sh
python scripts/arc_mla/bf16_row_census.py \
  --source-dir retained-source \
  --source-manifest pinned.source.json \
  --precision policy.json \
  --out census.json
```

A complete policy is mandatory, including for all-INT8:

```json
{"version":1,"attention":"int16","dense":"int16","shared":"int8","embedding":"int8","head":"int16"}
```

This is #168's mixed policy. #156's mixed policy instead uses attention/shared/
embedding INT16 and dense/head INT8. The report records all five selections,
including classes absent from a retained subset. Missing/unknown fields,
unknown selections, duplicate JSON keys and versions other than integer 1 fail.

The output must be a new file outside the source directory. Existing paths,
including links to inputs, are refused. Exit 0 means a valid report was written;
unsupported rows **still produce a report and exit 0**. Exit 2 means invalid
inputs, hashes or output location; no census report is written for those errors.
Before considering conversion, inspect `scanned_selectable_rows_supported`,
`complete_language_tensor_inventory`, `missing_language_tensors` and
`absent_shards`, and match the scanned scope to the separately authorized scope.
An empty or partial scan cannot establish whole-model support.

Every present pinned source is checked for exact length and SHA-256 before and
after scanning, using the same read-only file descriptor. Pinned config/index
metadata must be present; absent shards are explicitly recorded. Unlisted local
files are not inputs. Safetensors metadata, extents, duplicates and expected
language tensor shapes/dtypes are validated. The scanner never rewrites a pin,
fetches a missing shard, or modifies/deletes a source. Avoid concurrent source
writers: checks describe the bytes read, not a lock on paths after the scan.

## Counts and row layout

Counts are exhaustive and disjoint, with nonfinite taking precedence over any
finite value in a row, then the all-zero exception (including negative zero):

| Category | Meaning |
|---|---|
| `zero` | All entries are positive or negative zero; supported separately |
| `below_range` | Nonzero finite maximum yields scale shift k > 62 |
| `in_range` | Nonzero finite maximum yields 16 <= k <= 62 |
| `at_or_above_range` | Nonzero finite maximum yields k < 16 |
| `nonfinite` | At least one BF16 infinity or NaN |

The classifier derives k using integer mantissa/exponent arithmetic and the
converter's scale normalization/rounding. Exhaustively over finite positive
BF16 encodings, INT16 accepts maxima `0x3700..0x4e7f` (2^-17 inclusive to 2^30
exclusive); INT8 accepts `0x32fe..0x4a7d`. Signs do not affect admission. A small
entry in a row with an admissible maximum does not make that row below-range.

`language_model.` is stripped according to the wrapped source config before
class selection. Normal matrices use source row order. KV-B `[H*(N+V),R]`
is reported as two semantic matrices, in the converter's order:

- key `[H*R,N]`: row `(head,rank)` gathers the N key positions;
- value `[H*V,R]`: row `(head,value)` reads the contiguous R columns after
  the key block of that head.

Each record includes the original tensor and shard name/hash, semantic shape,
layout, class, selected precision, row total and five counts. Class aggregates
and global totals count **semantic conversion rows**, so KV-B contributes
`H*R + H*V`, not its source row count. Native routed experts (including BF16
scales), routers, correction biases and norms are listed separately; their
admission rules are not inferred from selectable-matrix counts. Ignored vision,
projector and rotary-frequency tensors are also identified separately. The
scanner does not validate native tensor values or approve YaRN preparation.

## Resource bounds and synthetic validation

Payloads are streamed, with `--chunk-elements` (default 32768, maximum 524288)
limiting each BF16 read. Even a row larger than that limit is chunked. KV-B keys
are accumulated in rank-column tiles; the complete tensor is never transposed
or loaded. Hashing reads at most 1 MiB at a time. JSON metadata is limited to
16 MiB per file/header. Memory also includes parsed model/tensor metadata,
per-tensor result records and a bounded category cache; it scales with inventory
size, not weight payload size. This is not a measured real-model RAM budget.

```sh
python scripts/arc_mla/check_bf16_row_census.py \
  target/release/arc-mla fresh-synthetic-evidence
```

The harness checks 146 actual-converter cases with independent expected counts:
all five classes, both mixed policies, boundary neighbors, zeros, infinities,
NaNs, and non-constant KV-B key/value patterns. It compares small and large read
tiles, exhaustively checks finite maximum classifications, refuses bad hashes,
policies and malformed tensor metadata, and verifies source bytes unchanged.
It retains raw converter stdout/stderr, counts, policies, hashes, deterministic
CLI outputs and a reproducible synthetic sample source/report. Fixture setup
mutates only generated synthetic inputs between cases; the census does not.
The existing three-host slice CI runs these checks without a real-weight job.

Stop on unsupported/nonfinite rows before any later authorized conversion.
Owner acceptance, resource admission and real-weight authorization remain
separate gates. LOW-1 (independent INT16/YaRN forward execution) and LOW-5
(`provisional/unreviewed` label pending owner acceptance) remain open. Preserve
the recorded `INT16-CONTRACT.md` merge-order follow-up: whichever of #156/#168
lands second must reconcile the file while retaining both complete policies.
