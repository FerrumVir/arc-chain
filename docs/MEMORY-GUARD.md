# Community worker memory guard (ARC-410 L7, section A)

The memory guard is **off by default**. When it is off or not configured, it **protects nothing**: the worker claims and computes exactly as before. No launcher turns it on. The desktop app, the legacy bridge and `install.sh` all start `arc-node` without it. Turning it on for any of them is a decision that hasn't been made yet (see the end of this document).

It also does **not** cover loading the model. Running out of memory during the load is unaddressed until the launch gate ("G1") exists, and that gate still needs load-peak and residency measurements.

## What it does when on

Everything below happens after the model is loaded, inside `arc-node`. It applies the same way whichever launcher started the node.

| Step | Where | What |
|---|---|---|
| Floor pause | before each claim round (`main.rs`, claim loop) | If available memory is below `headroom`, the worker doesn't claim. It reports `paused_memory_guard` and checks again after `recheck-secs`. It resumes only after `resume-reads` consecutive reads at or above `headroom + resume-hysteresis`. |
| Admission | after the prompt is tokenized and the context preflight passes, before compute | The worker needs `KV + working memory + headroom ≤ available`. Otherwise it submits a **declined** result (no output, no attestation) and doesn't compute. The worker never retries that job. |
| KV reservation | `CachedIntegerModel::try_generate_with_reserved_kv` | With the guard on, the job's KV cache is reserved for exactly the preflighted positions, so it never grows past what admission charged. Tokens and hash are identical to `try_generate`. |
| Decline back-off (optional) | after N consecutive memory declines | Claims also pause for `decline-backoff-secs`. A job that fits resets the count. |

**How memory is read** (`memory_guard.rs`, `OsMemoryProbe`). Every decision reads fresh, with no caching and no `sysinfo`. A failed call is reported as a failure, never as a low reading:

| OS | Source | Counts as failure |
|---|---|---|
| macOS | `host_statistics64(HOST_VM_INFO64)` plus `sysctl hw.memsize`. Available is `free + inactive + purgeable − compressor` pages. Under heavy compression this saturates to 0, which is a real low reading. | not `KERN_SUCCESS`; sysctl failure |
| Windows | `GlobalMemoryStatusEx` | `FALSE`; total of 0 |
| Linux | `/proc/meminfo` `MemAvailable`, or the pre-3.14 estimate when it's absent; the source is recorded. With a memory cgroup, the figure used is the minimum of host available memory and the headroom (`limit − usage`) of the process's cgroup **and every ancestor up to the cgroup namespace root**. An ancestor's usage counts all its descendants. For v1 the leaf's `hierarchical_memory_limit` also applies. A level without a limit file, or with `max`, sets no limit there. | unreadable or unparsable files, including an ancestor's: its limit is never inferred from host memory. A cgroup path outside this namespace (`/..`). `"max"` means no limit, never `u64::MAX`. |

**What a job is charged.** All arithmetic is checked. A requirement that doesn't fit in a `u64` is declined as "job memory requirement not representable"; it never saturates.
- **KV:** `required_positions × 2 × n_layers × d_kv × 8`, from the loaded model's config. For the Llama-2-7B shape that is 2 MiB per position.
- **Working memory** of the community worker's token-at-a-time path, from `forward_one_token_observed`'s own buffers: `(6·d_model + 2·d_kv + 2·d_ff + 3·d_model + d_model + vocab)` i64, plus `2·d_model + d_ff` bytes of quantized inputs, plus `max_tokens × 8`. This deliberately overestimates a little: the attention and FFN phases don't overlap.
- **Not included.** The community worker never runs batched prefill: it calls `try_generate`, not `_v2`. Resident weights are not charged here, because the reading is taken after load.

## Configuration

Five values are required when `--memory-guard` is enabled; the back-off pair is optional and must be supplied together or omitted together. **None has a default**, because no production value has been decided or measured. A `--memory-guard-*` value given without `--memory-guard` is refused rather than ignored, and so is a partial configuration.

```sh
arc-node <the launcher's usual arguments> \
  --memory-guard \
  --memory-guard-headroom-bytes 2147483648 \
  --memory-guard-resume-hysteresis-bytes 536870912 \
  --memory-guard-resume-reads 2 \
  --memory-guard-recheck-secs 30 \
  --memory-guard-on-probe-failure pause
# optional back-off (both or neither):
#   --memory-guard-decline-backoff-after 3 --memory-guard-decline-backoff-secs 120
```

The numbers above are **examples for testing the mechanism**. They are not recommendations or measured values; see the table below.

`GET /community/worker/status` gains a `memory_guard` object, but only when the guard is on. When it's absent, there is no memory protection.

```json
"state": "paused_memory_guard",
"memory_guard": {
  "state": "paused_low_memory", "memory_check": "ok",
  "last_available_bytes": 1610612736, "last_total_bytes": 17179869184,
  "last_source": "host_statistics64", "last_cgroup_headroom_bytes": null,
  "headroom_bytes": 2147483648, "paused_rounds": 4,
  "declined_low_memory": 1, "declined_not_representable": 0, "declined_probe_failed": 0,
  "probe_failures": 0, "backoffs": 0
}
```

## Limitations (not resolved here)

- **Model loading is not covered.** This needs the launch gate (G1).
- **Allocation failure.** With the guard enabled, reserved KV uses checked size arithmetic and `try_reserve_exact` for both outer layer tables and every layer's K and V `i64` buffer (two table allocations plus two buffers per nonempty layer). Capacity overflow or allocator refusal drops all partially reserved buffers and returns `kv_reservation_failed` before token generation. The community worker submits a declined result with empty output/hash/engine, zero tokens/timings and no attestation, records a decline rather than a failed inference, and advances to another job. Existing result-delivery retries remain transport retries, not generation retries. Admission accounting/configuration are unchanged; allocator refusal does not add a probe reading or feed the optional low-memory admission backoff. A subsequent fitting job can run.
  This covers **only reserved KV allocations**. The guard-off growth path, model loading (G1 peaks), generation working buffers, token/output/decode storage, result/error construction and transport allocations remain outside this guarantee. Successful reservation is not physical-memory commitment: OS overcommit, later page faults, concurrent pressure and OOM termination can still kill the process. This is not general allocation-failure protection.
  Synthetic tests refuse each of six reservation allocations in a two-layer tiny model via a test-binary-only allocator, track partial-buffer deallocation, and exercise the model and worker result path with a fake submission callback. Capacity/addressing limits are tested without huge allocations. These tests do not load a real model, probe OS memory, contact a coordinator, or establish native deployment behavior.
- **Malformed cgroup membership.** An unparsable `/proc/self/cgroup` is read as "no cgroup", so host memory alone is used. It is not reported as `Failed`.
- **Limits above the cgroup namespace root are not visible** from inside the namespace, and they are not guessed. Host available memory still bounds the figure.
- **Conservative readings.** cgroup usage includes reclaimable page cache, so headroom reads low. The v1 "unlimited" sentinel reads as a very large limit, which is harmless.
- **Memory taken between admission and compute, or during a job,** is not prevented; it is seen at the next decision.
- **Desktop status.** The desktop shows unknown states as "Running", so `paused_memory_guard` would read as Running. A truthful status mapping is an **activation prerequisite**.

## Proposed policy for TJ (not decided; values unmeasured)

| Decision | Proposal | Rationale | Unmeasured assumptions |
|---|---|---|---|
| Failure behaviour (`on-probe-failure`) | `pause` | Reviewer's recommendation. A guard that can't read memory can't protect, and an explicit failure shows up as `memory_check: failed` plus a counter, rather than a silent stop. | How often each OS call fails in the field (unknown). |
| Operator default | Off everywhere until the values below are chosen. Later: on by default in `arc-node`, with an explicit opt-out announced as a behaviour change for headless and bridge operators (Reviewer's recommendation). | A guard turned on with guessed values could pause workers that are fine, or protect nobody. | — |
| `headroom-bytes` | A named, configurable value with no public claim. **Starting point for a trial only: 2 GiB.** | Room for the OS, the desktop app and chain state alongside a worker. | Not measured. It needs calibrating against the reported 16 GB slowdown (ARC-287 part 7). |
| `resume-hysteresis-bytes` / `resume-reads` | Trial: 512 MiB, 2 reads | Avoids flapping between paused and claiming when memory hovers at the line. | Not measured. |
| `recheck-secs` | 30 | Matches the claim long-poll (30 s) and `CLAIM_RETRY_CAP` (30 s) already in the worker. | — |
| Decline back-off | Trial: after 3 declines, 120 s | Stops claim-then-decline churn. 120 s is twice the dispatcher's 60 s fallback noted in the claim loop. | Not measured. |
| Status wording | The desktop shows unknown states as "Running". Proposed copy for `paused_memory_guard`: "Paused: not enough free memory for AI jobs". | Honest, and makes no performance claim. | Needs the approved-wording step. |
| macOS signal | sysinfo's "available" formula, as implemented | — | Whether memory pressure tracks the slowdown better (D7): not measured. |

## Remaining activation decisions

1. On probe failure: `pause` or `proceed`.
2. Whether, and when, the desktop, bridge and headless launchers pass `--memory-guard`. Each launcher then needs a small change to pass it, and none is made here.
3. The headroom, hysteresis, resume-reads and back-off values.
4. The desktop status copy for `paused_memory_guard`.
5. G1, the launch gate: load-peak and resident-weight measurements, and the recovery policy (D6).

Nothing here is a RAM requirement, a minimum, a performance guarantee or a measured footprint.
