# Serving statistics v2 contract

This contract accompanies `serving-stats-v2.json`, consumed by both
`rpc/serving_stats.rs` and `desktop/unit/serving-stats.spec.ts`. The independent
site consumer can use `desktop/src/lib/network-stats/serving-v2.ts` as the
reference aggregation implementation. This document describes measurements,
not a throughput claim.

## Sources and compatibility

GET `/community/model_stats/v2` and `/inference/model_stats/v2` are aliases of
one coordinator store. Their schema is `arc.community.model-stats.v2` and scope
is `completed_public_answers_on_this_coordinator`. Read exactly one alias per
coordinator. All routes are GET-only (POST returns 405); reads neither expire nor
modify state and do no network IO.

The unversioned aliases retain `arc.community.model-stats.v1` and its fields.
They return **503**, not 200 with an empty model list, when the window is incomplete
or the model cap has rejected answers. Existing v1 consumers map this to an error,
not measured zero. v2 returns 200 with `available:false` and a root `reason` in
these states. Ignore its measurements when unavailable. The shared
`serving-stats-v1-unavailable.json` vectors pin both failure cases.

## Time, completeness and counts

The coordinator captures a UTC origin at process startup, then advances it using
monotonic elapsed time. For observation time `t` in milliseconds:

- `end = floor(t / 60000) * 60000`.
- Rate window is `[end - 60000, end)`; `window_secs` is 60.
- Day window is `[end - 86400000, end)`; it is a rolling 24 hours, not a calendar day.
- An answer belongs to the window containing its completion time, even when its
  execution began earlier. Start is inclusive; end is exclusive.

Root fields `window_start_unix_ms`, `window_end_unix_ms`,
`day_window_start_unix_ms`, `day_window_end_unix_ms` identify these intervals.
`since_unix_ms` is the startup origin; `as_of_unix_ms` is the observation time
(second precision), which can differ between reads of the same completed minute.
Start fields are null until the entire corresponding interval is after startup.
Thus a startup at 00:00:00.500 first has minute coverage at 00:02:00, and day
coverage at the next day's 00:01:00. End fields remain present during startup.
`available` requires full minute coverage and no rejected model answers;
`day_available` additionally requires full day coverage. `reason` explains root
unavailability. Complete empty intervals are measured zero, not null.

Counts and samples from the current incomplete minute never enter the completed
minute. Writes retain the previous complete minute's samples and enough day
buckets to cover the anchored day through the next boundary. No projection from
one minute to a day is permitted.

## Model rows

`models` has at most 32 entries. `model_id` is a lowercase `0x`-prefixed 32-byte
hash; `model_name` is null. Rows may persist after becoming idle. Per-model fields:

| Fields | Meaning / null rules |
| --- | --- |
| `answers` | Successful completed public answers in the minute; null before full minute coverage. |
| `served_tokens`, `output_tokens` | Identical minute output counts, including cache hits; null before coverage. |
| `verified_tokens` | Subset of served output tokens from verified answers, never greater than served. |
| `served_tokens_per_second`, `output_tokens_per_second` | Output count divided by 60, not per-answer decode speed. |
| `input_tokens`, `input_tokens_per_second` | Measured tokenized input count (template included, synthetic BOS excluded), and count/60. Null if minute incomplete or any input count unknown. |
| `input_tokens_last_day`, `output_tokens_last_day` | Actual anchored day totals; null before day coverage. Input also null if any day input count is unknown. |
| `day_window_secs` | 86400. |
| `day_reason`, `input_reason` | Non-null when day coverage is incomplete or any retained minute/day input count is unknown, respectively. |
| `cached_answers` | Cached successful answers; have neither fresh decode nor TTFT measurement. |
| `answer_tokens_per_second` | Newest-first retained decode samples. For >=2 accepted output tokens: `(output - 1) / (last_token_elapsed - first_token_elapsed)` in seconds, requiring positive elapsed time. |
| `ttft_samples_ms` | Newest-first retained handler-entry-to-first-accepted-output durations. One-token answers qualify; zero-token answers do not. Response transport is excluded. |
| `answer_speed`, `time_to_first_token_ms` | `{samples,p5,p50,p95,reason}` over the respective raw arrays. |
| `sample_limit`, `sampled_answers` | 1000 and number of retained answers, including answers without usable timing. |
| `hop_samples`, `omitted_hop_samples` | Numeric hop totals and number of omitted hop records; see below. |
| `hop_latency_definition`, `ttft_definition` | Fixed explanatory text, never worker-supplied strings. |
| `reason` | Explains incomplete minute coverage; otherwise null. Partial timing has its own structured reasons below. |

Raw sample and hop arrays are empty before minute coverage; timing counters and
`timing_coverage` are null. `sampled_answers` / `omitted_hop_samples` may describe
retained data even when the source is unavailable and must not be treated as
measured coverage then. When a complete window has no eligible samples, percentile
values are null and its `reason` is non-null. Percentiles use sorted linear
interpolation at rank `(n - 1) * p` for p=.05/.50/.95; never average medians or
percentiles from different coordinators.

## Timing coverage and exclusions

These full-minute answer counts are computed **before** sample truncation and are
null before coverage. `cached_answers` excludes those answers from every other
eligibility/exclusion counter here.

| Counter | Definition for noncached answers |
| --- | --- |
| `short_answers` | Fewer than two output tokens, including zero. |
| `zero_output_answers` | Zero output tokens (subset of short answers). |
| `decode_missing_timestamps_answers` | >=2 tokens but missing first or last timestamp. |
| `ttft_missing_timestamps_answers` | >=1 token but missing first timestamp. |
| `untimed_answers_no_worker_timestamps` | Union of the preceding two missing-timestamp sets. Counts once even if both are missing; overlaps short answers when an untimed answer has one token. The name covers absent accepted-token observations, including worker protocols that provide none; it does not infer where an answer ran. |
| `invalid_decode_timing_answers` | >=2 tokens with both timestamps but zero or backwards decode interval. |
| `speed_eligible_answers` | Has a usable decode rate before sampling. |
| `ttft_eligible_answers` | Has a first accepted output timestamp and >=1 output token before sampling. |

Decode partitions all answers into cached + short + missing decode timestamps +
invalid interval + speed eligible. TTFT independently partitions them into cached
+ zero output + missing first timestamp + TTFT eligible. Do not sum the union
counter and short count as disjoint reasons.

`timing_coverage.speed` and `.ttft` each expose `total_answers`, `eligible_answers`,
`sampled_answers` (usable retained measurements, not all retained answers),
`partial` (sample count < total answers), and `reasons`:

- Speed: `cached_answers`, `short_answers`, `missing_token_timestamps`,
  `invalid_decode_interval`, when their corresponding count is positive.
- TTFT: `cached_answers`, `zero_output_answers`, `missing_first_token_timestamp`.
- Either: `sample_limit` when retained usable samples < eligible answers.

An empty complete population has no partial-timing flag; its percentiles remain
null. Timing may be valid but unrepresentative of the untimed population. For
example, four timed local answers among 103 completed answers are reported as
4/103 coverage, not a speed measurement for all 103.

Samples retain the newest 1000 answers **per model per UTC minute**, with separate
caps for the current and previous minute (at most 2000 stored). Counts are not
sampled. This is bounded recent-answer sampling, not random sampling.

## Aggregation (`arc.network-stats.v2`)

`extendNetworkStatsV2` takes an already privacy-filtered v1 document, replaces its
schema and model section, and preserves every non-model section. It performs no
IO. The caller must provide every intended coordinator reading, including a null
body for a failed read. Omitting a source cannot be detected by the adapter.

Reject the entire model aggregate (`available:false`, empty `per_model`, null
window/source/as-of, explanatory `reason`) for missing/invalid/unavailable sources,
duplicate coordinator identities, malformed counts/samples, or differing minute
start/end values. Never silently align or combine different intervals. Reads one
or two seconds apart normally share the same completed minute. At a boundary,
readings can legitimately straddle two minutes; retry a consistent set rather
than rounding or shifting the measurements.

The reader allows at most 2000 ms of future clock skew and at most 62000 ms of age
from the minute end (one minute plus 2 s grace). End must be divisible by 60000;
day end must equal minute end, and an available day start must be end-86400000.
This tolerates small skew but does not repair incorrect clocks. The collector is
responsible for clock synchronization and polling; no retry IO occurs here.

Group by model hash, sum counts and the timing exclusion/eligibility counts, and
pool raw samples before calculating percentiles. Recompute output/input rates as
sum/60 and verified share as verified/served (null for zero served). Never add
the two aliases or count replicas as coordinators. Missing model rows on an
otherwise complete source mean zero for that model. Day totals require every
source to have `day_available:true`, even one without that model row. Any null
input component makes its aggregate null. Unsafe integer sums become null;
timing coverage adds `count_overflow` if its eligible count overflows.

The aggregate model section reports identical window boundaries, day boundaries,
and `as_of_unix_ms` at the common minute end. Each per-model row exposes the
summed counters, `sampled_answers`, `timing_coverage`,
`answer_tokens_per_second_percentiles`, `time_to_first_token_ms`, legacy median
and sample count, and `coordinators_reporting`. Its `reason` is non-null for
partial timing; `day_reason` and `input_reason` explain incomplete daily/input
coverage. Source-provided display names and explanatory text are not copied.
The shared two-coordinator vector pools [10,11,12] with [50] into median 11.5
and p95 44.3, rather than averaging coordinator medians.

## Hop scope, privacy and excluded work

Each hop has only `hop`, `start_layer`, `end_layer` (half-open layer range),
`positions`, `wall_ms`, `compute_ms`. Totals combine the same numeric hop/layer
range within sampled answers; positions include prefill and decode positions.
`wall_ms` measures the selected successful replica's coordinator round trip,
**including compute**, not pure network RTT. Failed attempts, unselected replicas
and waiting for the quorum tail are excluded. This is not whole-request latency.

At most 128 hop records per retained answer and 128 distinct hop/range keys per
model snapshot are included; each omitted record increments `omitted_hop_samples`.
Answer sample truncation is disclosed separately by timing/sample coverage.
The aggregate preserves hop/range totals per coordinator as `hop_latency` with
anonymous `coordinator_index`; means are total time/positions (null at zero).
`hop_samples_truncated` signals omitted hop records, not omitted answer samples.

Only completed user-facing `/inference/run`, `/inference/run_sharded`, and
`/inference/run_consensus` answers enter the store (also listed in root `routes`).
No demand-pump work, recovery probes, verifier recomputations, failed requests,
replica copies or direct worker-only executions are counted as extra answers.
Successful cache hits count served tokens but have no fresh timing or verification
attributed by this cache path. Untimed verified worker/twin answers still count.
No request IDs, prompts, worker identities, sockets, IPs, hostnames or free-text
model names are published. Model capacity rejection increments root
`rejected_model_answers` and makes the entire source unavailable until restart.
This revision does not change that policy or optimize per-second day-bucket scans.
