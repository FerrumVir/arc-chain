# Read-only BF16 row census

`scripts/arc_mla/bf16_row_census.py` scans retained, hash-pinned local BF16
sources. Engine admission reference remains `616ba16a60f43b5e70666ca24f5d7f9ce99ab932`
(restacked unchanged into #168). It never fetches, converts or modifies weights,
selects a flush/fallback policy or approves admission. This delivery uses only
synthetic fixtures; it is not the real K2.6 census.

## Invocation and trust

Requires existing `numpy` and `blake3` dependencies. The caller must authenticate
the pinned `arc.hf-source.v1` manifest; its hash alone is not an external trust
anchor. Keep its config, index and retained shards together. Do not regenerate
pins to bless changed real inputs.

```sh
python scripts/arc_mla/bf16_row_census.py \
  --source-dir retained-source --source-manifest pinned.source.json \
  --precision complete-policy.json --out census.json
```

The policy requires integer `version:1` and exactly `attention`, `dense`,
`shared`, `embedding`, `head`, each `int8` or `int16`. Both complete mixed
policies from #156 and #168 remain supported and explicitly recorded.

Present pinned files are length/SHA256-verified before and after scanning using
the same read-only descriptor. Hashing buffers are at most 1 MiB. Absent shards
and missing language tensors are listed, never fetched. Config/index must be
present. Names, prefix, dtype, shape, header extents, duplicates and overlaps
retain their validation. Unlisted local files are not inputs. Report output
must be a fresh path outside the source directory; links/existing paths reject.
Avoid concurrent source writers: verification does not lock paths after scanning.

Exit 0 means a report was written, **including if rows are unsupported**. Exit 2
means invalid inputs/hash/metadata/output location. A partial or empty scan is
not whole-model support. `admission_approved` is always false. Inspect inventory,
missing scope and both native/selectable admission flags before further work.

## Version 3 report (bounded rejection details)

The schema is `arc.bf16-row-census.v3`: `tensors`, `classes`, top-level `rows`
and `counts` now include BF16 **norms and routers**, in addition to the five
selectable classes. `native_or_ignored_tensors` retains shape-only experts,
correction biases and ignored vision/rotary tensors; they are not value-scanned.
Native INT4 payloads/scales are never quantized by this tool. Header extents,
expected shapes/dtypes and whole-file hashes still cover them.

Each tensor/component record carries source file/hash, original tensor name,
source dtype/shape, class, actual precision, layout and semantic shape. Fields:

- `counts`: actual selected-matrix or native converter admission, exhaustive
  zero/below/in/at-or-above/nonfinite categories.
- `int16_window_counts`: the **fixed diagnostic** matrix window, independent
  of selected policy or native representation: zero, `(0,2^-17)`,
  `[2^-17,2^30)`, `[2^30,infinity)` finite, and nonfinite separately.
- `int8_accepted_int16_rejected`: exact intersection, equivalently finite row
  maxima in `[127*2^-32,2^-17)`; zero is accepted by both. It is not computed
  from the selected policy. INT8's exact upper bound is `127*2^15`, below the
  INT16 upper bound, so there is no upper-tail contribution to this intersection.
- `row_max_abs`: finite-row count, nonfinite-row count and numeric min/median/max
  of absolute row maxima (weight units, not q values). Nonfinite rows are
  excluded, not treated as zeros or finite maxima. Empty finite populations
  have null statistics. Finite signed-zero rows contribute zero. Numeric values
  are binary64 representations; `median_bf16_pair` and `median_exact` decimal
  numerator/denominator strings preserve the exact even median independently
  of display rounding. Even medians are the mean of the two middle observations.
- `rejected_rows`: a bounded prefix of rows rejected by actual admission or fixed INT16
  diagnostics, with zero-based semantic row, semantic coordinates, maximum
  absolute BF16 encoding, separate actual/diagnostic rejection categories and
  intersection flag. Tensor name/component live in the enclosing record;
  identity is `(tensor, layout, semantic_row)`. A shared `--max-rejected-rows` budget (default 1000, allowed 0..100000)
  bounds identity storage across the entire report, including native diagnostics
  and both KV-B components. Counts, distributions and statistics remain complete.
  Each component declares `rejected_row_count`, `rejected_rows_omitted` and
  `rejected_rows_truncated`. The root adds the configured limit and total retained
  identities. Truncation is explicit, never silent; use limit 0 for counts only.
  Retention follows deterministic shard/tensor scan order, with key then value
  within each KV-B head; it is not a random or representative sample. Nonfinite
  rows have their own rejection category.

`tensor_totals` also pools both KV-B components into one tensor summary (ordinary
tensors have one component). Tensor, class and global statistics use pooled histogram counts, **not averages or
medians of tensor statistics**. All counts and statistics are repeated at these
aggregate levels; rejection identities appear once under tensor components.
`scanned_selectable_rows_supported` checks actual selected policy only;
`scanned_native_rows_supported` checks scanned norms/router only. Neither flag
certifies diagnostic INT16 support, unscanned bias/expert values, whole inventory,
YaRN preparation, resources or quality.

## Converter geometry and distinct domains

| Class/component | Semantic rows | Actual admission |
|---|---|---|
| Attention, dense, shared, embedding, head | Source output rows; vocabulary rows for embed/head | Selected dyadic INT8/INT16, zero exception. INT16 maxima `[2^-17,2^30)`; INT8 `[127*2^-32,127*2^15)`. |
| KV-B key | Source `[H*(N+V),R]` → key `[H*R,N]`; row `h*R+r` gathers source `[h*(N+V)+n,r]`; coordinates `[h,r]` | Selected attention policy, evaluated **after transpose**. |
| KV-B value | `[H*V,R]`; row `h*V+v` is source `[h*(N+V)+N+v,:]`; coordinates `[h,v]` | Selected attention policy. |
| Norms | `norm_scalar_q16`: one source scalar per semantic row, `[elements,1]`; coordinates `[element]` | I64 Q16, finite `abs(w)<2^46`; small values round as specified by the native converter, not a new matrix flush. |
| Router | `router_expert_row`: source `[experts,hidden]`, coordinates `[expert]` | Existing INT16 power-of-two scale, nonzero maxima `[2^-48,2^15)`, zero exception. |

Prefix handling remains driven by the wrapped source config. Nonfinite entries
win over finite maxima in every row, regardless of sign. A diagnostic matrix
rejection on a norm/router row is **not** a native conversion rejection. Bias
is deliberately separate from router weight distributions.

## Bounds, tests and handoff

Payload reads are bounded by `--chunk-elements` (default 32768, max 524288).
Rows span chunks; KV-B uses rank-column tiles, never a whole-tensor transpose.
Per-file JSON metadata is capped at 16 MiB. Histograms have 32768 unsigned-64
bins per active component/class/global accumulator, independent of tensor size.
Metadata/result memory scales with tensor inventory and the explicitly bounded
rejection-detail budget. JSON is written incrementally with `json.dump`, without
materializing a second full serialized report. Full counts continue after the
budget is exhausted. This is not a constant-memory whole-model claim: metadata,
model inventory and fixed histograms still occupy memory.

`check_census_memory.py OUT` uses the pinned real configuration's 5,891,392
semantic rows and actual row widths/KV-B geometry, a virtual all-NaN BF16 source,
and the native inventory. Every row is flagged; only 1000 identities are retained.
It records process peak RSS through report serialization, exact semantic component
dimensions, output bytes and elapsed time. It fetches no weights and does not
simulate safetensors headers or hashing I/O. The measured report
belongs in delivery evidence; it is not a real-data RSS guarantee.

```sh
python scripts/arc_mla/check_bf16_row_census.py \
  target/release/arc-mla fresh-synthetic-evidence
```

The harness retains the original 146 converter cases and 16 malformed-input
controls. It adds 10 native norm/router converter cases, independently recomputes
all statistics and rejection identities from tiny fixture arrays in converter
order, checks aggregation and policy independence, tests exact even medians and
empty/all-nonfinite distributions, and checks admission against #156's Fraction
oracle corpus. Read-tile invariance includes nonconstant KV-B; row 1 in a key
transpose and row 33 in a value component are explicit converter-error controls.
Source snapshots, deterministic CLI outputs, hashes and ignored/expert shape-only
handling are retained. The existing Linux x86/ARM and Windows slice CI runs it.
Cap controls include zero and the exact flagged-count boundary. Injected output
creation and mid-serialization disk-full failures must return exit 2, emit no
success summary and preserve the sources. A failed write can leave a partial
report: consume output only after exit 0; existing output paths are never overwritten.

Next is a **separately dispatched** admitted Studio census, not a download by
this tool-completion pass. Operator conditions: fresh Studio disk/RAM/heavy-job
report; at least 140 GB free; at most 60 GB cumulative anonymous downloads and
40 GB resident; exact official revision and LFS SHA256 verification before use;
retain the verified layer-0 shard. The gaming PC gates the later pipeline, not
this census. The independent full-forward oracle is not required for this read-only census.
It **is required before real-weight conversion and quality work**, as the stricter
admission documents specify. Census authorization does not authorize conversion
or quality measurements. No
implicit fallback, owner precision decision or Kimi performance claim follows.
