# `arc.network-stats.v1`: ARC's live network numbers

One definition of the live numbers ARC shows in public: the ARC Node desktop
app's "Network, live" panel and the arc.ai live counter. Both read the same
public validator endpoints, use the same windows and apply the same rules, so
they show the same numbers.

The reference implementation is plain TypeScript with no browser or Tauri
dependency, so the website can reuse it as is:

| File | What it does |
|---|---|
| `desktop/src/lib/network-stats/contract.ts` | The document's types and constants |
| `desktop/src/lib/network-stats/window.ts` | The 60 s block window: parsing, hash linking, counting |
| `desktop/src/lib/network-stats/aggregate.ts` | Validators, chain height, community nodes, twin counters |
| `desktop/src/lib/network-stats/poller.ts` | Cadence, backoff, failover |
| `desktop/src/lib/network-stats/format.ts` | Display rules |
| `docs/network-stats-vectors.json` | Window test vectors; any implementation must reproduce them |

## Rules that override everything else

1. **Measured, or absent.** Every number is computed from validator answers
   read for this document. A number that could not be measured is `null`, with
   a reason next to it. Never estimate, extrapolate, interpolate between reads,
   scale a partial window up, carry an old reading forward as if current, or
   fill a gap with zero.
2. **State the window.** A rate is a count over a stated 60-second window of
   finalized blocks, divided by 60. Show the window next to the rate.
3. **Read-only GETs only.** Use only the five reads below. Never call
   `GET /community/list`: its handler prunes the worker registry as a side
   effect. Never POST.
4. **Live and measured are different things.** One-time lab or CI
   measurements belong in the separate measured-records list (below), never
   next to a live figure without a "not live" label.

## Sources

The six public validators, one replicated chain (block 6,105,000 has the same
hash and state root on all six, read 2026-10-06):

| Label | Origin |
|---|---|
| NYC | `https://149.28.32.76` |
| LAX | `https://140.82.16.112` |
| AMS | `https://136.244.109.1` |
| LHR | `https://104.238.171.11` |
| NRT | `https://202.182.107.41` |
| SGP | `https://149.28.153.31` |

| Read | Path | Used for |
|---|---|---|
| health | `GET /health` | validators online, version, chain height |
| finality | `GET /finality/latest` | `finalized_height`, where the window ends |
| blocks | `GET /blocks?from=F&to=T&limit=N` (N ≤ 100) | block header `timestamp` and `tx_count` |
| scoreboard | `GET /workers/scoreboard?limit=0` | `eligible_inference_workers` (and #139's `twin` summary) |
| twin stats | `GET /community/twin_stats` | twin execution counters (v0.8.11, PR #139) |

`/blocks` returns the same header fields as `GET /block/{h}` (`height`,
`hash`, `parent_hash`, `timestamp`, `tx_count`) for up to 100 blocks per
request. At about 4.4 blocks per second a 60 s window is about 265 blocks:
three range reads instead of 265 single-block reads, then one range read per
poll. `limit=0` on the scoreboard returns no worker rows, so no worker names
are fetched.

## Polling etiquette

The desktop's cadence, per validator:

| Read | Every | Notes |
|---|---|---|
| `/health` | 10 s | odd validators offset by 5 s, so a fresh height arrives every 5 s |
| `/finality/latest` + `/blocks` | 15 s | one validator per poll, rotating; fail over to the next on error |
| `/workers/scoreboard?limit=0` | 30 s | first read 2 s after start |
| `/community/twin_stats` | 60 s | 10 min while it answers 404; first read 4 s after start |

- A failed read waits `base × 2^failures`, capped at 5 minutes, and resets on
  success.
- Nothing is read while no panel is on screen or the window is hidden.
- The desktop's native layer adds a read budget (a burst of 30 reads, refilled
  at 2 per second across all validators), a 512 KB body cap, a 3 s timeout, no
  redirects and no automatic retries.
- A public website should poll from one server, cache the document, and serve
  every visitor from that cache. It must not poll from each visitor's browser.

## The document

Keys are snake_case. Times are unix milliseconds. Rates are per second.

```jsonc
{
  "schema": "arc.network-stats.v1",
  "as_of_unix_ms": 1791302043708,   // reader's clock at the newest successful read in this document
  "validators": {
    "total": 6,                     // public validators polled
    "online": 6,                    // latest GET /health answered HTTP 200 with status "ok"
    "checked": 6,                   // validators checked at least once
    "versions": [{ "version": "0.8.10", "count": 6 }],   // among online validators, most common first
    "per_validator": [
      { "validator": "NYC", "origin": "https://149.28.32.76", "online": true,
        "version": "0.8.10", "height": 6105063, "checked_at_unix_ms": 1791302040000, "reason": null }
    ],
    "source": "GET /health on each public validator"
  },
  "chain": {
    "height": 6105644,              // highest height among online validators' latest /health
    "height_validator": "LAX",
    "height_as_of_unix_ms": 1791302041000,
    "source": "GET /health (height)"
  },
  "window": {
    "length_ms": 60000,
    "status": "live",               // waiting | measuring | live | stalled | unavailable
    "end_height": 6105642,          // newest finalized block read; the window ends here
    "end_timestamp_ms": 1791302043000,           // its header timestamp (validator clock)
    "start_exclusive_timestamp_ms": 1791301983000,
    "blocks": 264,                  // finalized blocks in the window       (null unless "live")
    "finalized_tx": 0,              // sum of header tx_count over them    (null unless "live")
    "blocks_per_second": 4.4,       // blocks / 60                         (null unless "live")
    "tps": 0,                       // finalized_tx / 60                   (null unless "live")
    "finalized_height": 6105642,    // from the latest GET /finality/latest
    "advancing": true,              // finalized height grew since the previous window read (null on the first)
    "read_from": ["LAX"],
    "as_of_unix_ms": 1791302043708,
    "reason": null,                 // why the rates are null
    "source": "GET /finality/latest, then GET /blocks?from&to (block header timestamp and tx_count)"
  },
  "community": {
    "ready_workers": 3,             // highest eligible_inference_workers among validators that answered
    "validators_reporting": 6,
    "per_validator": [{ "validator": "NYC", "eligible_inference_workers": 3,
                        "checked_at_unix_ms": 1791302030000, "reason": null }],
    "as_of_unix_ms": 1791302030000,
    "reason": null,
    "source": "GET /workers/scoreboard?limit=0 (eligible_inference_workers)"
  },
  "twin": {                         // all null and available: false until v0.8.11 (PR #139)
    "available": false,
    "verified_tokens_per_second": null,
    "verified_tokens_last_hour": null,
    "match_rate": null,
    "groups_matched": null,
    "groups_compared": null,
    "coordinators_reporting": 0,
    "since_unix_ms": null,
    "source": null,
    "as_of_unix_ms": null,
    "reason": "No validator serves /community/twin_stats yet; it arrives with v0.8.11 (PR #139)."
  },
  "models": {                       // per-model live stats: empty until a validator reports them
    "available": false,
    "window_ms": null,
    "per_model": [],                // see "Per-model live stats" for each entry's fields
    "source": null,
    "as_of_unix_ms": null,
    "reason": "No validator reports per-model serving stats yet."
  }
}
```

## Definitions

### Validators online

A validator is online when its latest `GET /health` answered HTTP 200 with
`status: "ok"`. Anything else (unreachable, timeout, another status code, a
different `status`) is offline, with the observed reason. `versions` counts
`version` among online validators only.

### Chain height

The highest `height` among online validators' latest `/health` answers,
named with the validator that reported it. Heights are never interpolated
between reads.

### The 60-second window

1. Read `finalized_height` F from `GET /finality/latest`. If a validator's
   finalized height is not above the newest block already held, ask the next
   validator (up to three in all), so one lagging validator cannot look like a
   stalled chain.
2. Hold the finalized blocks up to F: read forward from the newest block held,
   100 at a time.
3. Every block must name the block before it as its parent
   (`parent_hash == hash` of height − 1). A mismatch discards the window; a
   missing block leaves it incomplete.
4. Let E be the newest block held (normally F) and T its header timestamp.
   Walking back from E, a block is in the window while its timestamp is
   greater than T − 60 000. The first block at or before T − 60 000, the
   boundary block, ends the walk and is not counted. Read further back until
   the boundary block is held.
5. With the boundary block held, the window is complete:
   - `blocks` = number of blocks in the window;
   - `finalized_tx` = sum of their header `tx_count`;
   - `blocks_per_second` = `blocks / 60`;
   - `tps` = `finalized_tx / 60`.

   Without it, the window is incomplete: status `measuring`, all four null. A
   partial window is never scaled up.

The window is anchored to the chain's own clock (the newest finalized block's
timestamp), not the reader's clock, so two readers holding the same blocks
compute the same numbers. `docs/network-stats-vectors.json` pins the edge
cases: the exact boundary, a block 1 ms inside it, incomplete coverage, a gap,
and out-of-order timestamps.

### Window status

| Status | Meaning | Rates |
|---|---|---|
| `waiting` | no window read has finished | null |
| `measuring` | blocks read, but not yet a full window | null |
| `live` | complete, and the finalized height moved since the previous read (on the first read: the newest finalized block is under 120 s old by the reader's clock) | shown |
| `stalled` | complete, but the finalized height did not move between two reads 15 s apart (or the first read found the newest finalized block over 120 s old) | null |
| `unavailable` | the latest read failed on every validator tried | null |

### Community nodes ready

`eligible_inference_workers` from `GET /workers/scoreboard?limit=0`: workers
seen in the last 90 s that are idle (not assigned a job) and run the
coordinator's exact model and execution profile. Each validator counts the
workers registered with it, and every worker registers with every validator,
so `ready_workers` is the **highest** count among validators that answered,
never a sum. It undercounts a busy network (a node mid-job is not counted); it
never overcounts.

### Twin execution (v0.8.11, PR #139)

From `GET /community/twin_stats` (`schema: "arc.community.twin-stats.v1"`),
per coordinator:

- `counters.groups_matched`, `counters.groups_mismatched`;
- `throughput_last_hour.verified_tokens` over `throughput_last_hour.window_secs` (3600).

Where a validator answers 404 there but its `/workers/scoreboard` carries
#139's `twin` summary (`groups_matched`, `groups_mismatched`,
`verified_tokens_last_hour`), that summary is used instead and named in
`source`.

Each twin group is recorded only by the coordinator that ran it, so counters
are **summed** across coordinators:

- `verified_tokens_per_second` = Σ (verified tokens in each coordinator's last hour / its window seconds);
- `match_rate` = Σ `groups_matched` / Σ (`groups_matched` + `groups_mismatched`); null when nothing was compared.

The counters live in each coordinator's memory and restart from zero when it
restarts; `since_unix_ms` is the earliest start among those reporting. Until a
validator serves either source, `available` is false and every figure is
null. The desktop hides these figures entirely in that state.

### Per-model live stats (defined now, reported later)

These figures are defined now so that the desktop, the website and the
validators agree on them before any validator reports them. Until then
`models.available` is false, `per_model` is empty, and the desktop shows
nothing for them. The fields are listed in this order:

| Field | Definition |
|---|---|
| `model_id`, `model_name` | the exact model identity (0x-prefixed hash) and its name, as the source reports them |
| `served_tokens` | output tokens of answers completed in the window, summed over coordinators |
| `served_tokens_per_second` | `served_tokens` ÷ window seconds (a trailing average: label it with the window, e.g. "average over the last hour") |
| `verified_tokens`, `verified_share` | output tokens of those answers that passed verification (twin match or validator recompute), and `verified_tokens ÷ served_tokens` (0..1; displayed like the match rate, truncated, never rounded up to 100%) |
| `answers` | answers completed in the window |
| `median_answer_tokens_per_second` | the median, over the **pooled** answers of every coordinator, of each answer's decode rate `(output tokens − 1) ÷ (last token time − first token time)`; answers with fewer than 2 output tokens have no rate |
| `answer_samples` | how many per-answer rates were pooled (a source may report only its most recent answers) |
| `coordinators_reporting`, `reason` | how many coordinators contributed; why a figure is null |

Rules:

- A median is never combined from medians. Averaging two coordinators'
  medians of 11 and 50 tok/s would claim 30.5. The median of their pooled
  answers (10, 11, 12, 50) is 11.5. `summarizeModels()` in `aggregate.ts`
  implements this and a unit test pins it.
- Counts over different windows are never combined.
- The per-answer rate is a decode rate. It leaves out time to first token,
  which gets its own figure once it is measured. Compare it only with the
  same quantity for the same model, for example a hosted API's per-answer
  output speed. Show such a comparison only as a dated, sourced reference,
  never inside a live figure.

**Proposed source (not built yet; the desktop does not request it).**
`GET /community/model_stats`, schema `arc.community.model-stats.v1`, returned
per coordinator:
- `window_secs`;
- per model: `model_id`, `model_name`, `answers`, `served_tokens`,
  `verified_tokens` (≤ `served_tokens`), and `answer_tokens_per_second`, one
  rate per answer, newest first, capped (for example at 1,000).

`parseModelStats()` already reads this shape. When a validator serves it,
three things are needed:
1. add a sixth typed read to `src-tauri/src/network_live.rs` and to the poller;
2. extend the gateway allowlist;
3. show the section in the panel.

The fields above stay the same.

## Display rules

| Figure | Label | Format |
|---|---|---|
| validators | "Validators online" | `6 of 6`, then "v0.8.10 on all 6" or "v0.8.10 on 5 · v0.8.11 on 1" |
| chain.height | "Chain height" | whole number with separators |
| window.blocks_per_second | "Blocks per second" | 2 decimals under 10, 1 under 100, whole above; note the block count "in the last 60 s" |
| window.tps | "Transactions per second" | same; note `finalized_tx` "finalized in the last 60 s" |
| community.ready_workers | "Community nodes ready" | whole number; "online, idle and running the network model" |
| twin.verified_tokens_per_second | "Verified tokens per second" | rate format; "average over the last hour" |
| twin.match_rate | "Twin match rate" | percentage with one decimal, **truncated, never rounded up**: 1,999 of 2,000 is 99.9%, not 100.0%; 100% only when nothing mismatched |
| models[].served_tokens_per_second | "Served tokens per second" (per model) | rate format, with the window: "average over the last hour" |
| models[].verified_share | "Verified" (per model) | like the match rate: truncated, never rounded up |
| models[].median_answer_tokens_per_second | "Median tokens per second per answer" | rate format, with `answer_samples`: "median of 412 answers" |

- A null figure is a dash with its reason, never 0.
- Counters move only when a value changes, and not at all when the viewer
  asks for reduced motion (`prefers-reduced-motion: reduce`).
- Never call a fixture or a preview "live". The desktop's browser preview says
  "Synthetic preview" in place of "Live".

## Measured records: `arc.measured-records.v1`

A separate, static list (`desktop/src/lib/network-stats/measured-records.json`)
of one-time measurements, shown apart from the live numbers and labelled "not
live". Each record carries:

| Field | Meaning |
|---|---|
| `id` | stable identifier |
| `headline` | the figure or claim in plain words |
| `detail` | what exactly was measured, where, and what it does not cover |
| `setting` | `lab` (a staged test, such as several validators on one machine), `ci` (public CI runners), or `testnet` (measured once on the public testnet, on the stated date). A projection is never a record |
| `measured_on` | `YYYY-MM-DD` |
| `prs` | the pull requests it came from |
| `receipts` | one or more `{ label, url }` links: a public CI run in this repository (optionally one job), or an evidence file pinned to a commit (`…/blob/<40-hex commit>/…`) |
| `model` (optional) | the exact model, for model records |
| `hardware` (optional) | the machines it ran on |
| `metric`, `value`, `unit` (optional, all three together) | the measured figure: `metric` is one of `transfers_per_second`, `answer_tokens_per_second`, `served_tokens_per_second`, `time_to_first_token_seconds` |

Add a record only with its receipt. `measuredRecordsProblems()` in
`records.ts` checks the list, and a unit test runs it on every change.

**Adding a model-speed record (for example, a Kimi-class model at X tok/s).**
- Name the exact model and revision in `model`, for example the Hugging Face
  repository and commit. "Kimi-class" may appear in the headline only next to
  that exact name.
- Describe the hardware in `hardware`: machine count, type, memory and
  interconnect. A model-speed record without `model` and `hardware` is refused.
- Use `answer_tokens_per_second` for a per-answer decode rate, or
  `served_tokens_per_second` for aggregate throughput. The `value` must be the
  number in the receipt, not an estimate or a projection.
- Set `setting` to where it was measured, and set the date.
- Link a receipt that contains the raw measurements.

## Versioning

v1 may gain fields; existing fields keep their meaning and units. A change of
meaning, unit or window is a new schema (`arc.network-stats.v2`).
