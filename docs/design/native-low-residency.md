# Bounded native coordinator (candidate; not production qualification)

`--native-low-residency` explicitly selects a coordinator that retains norms,
RoPE tables (at most 64 MiB combined prepared state) and request KV/activations,
without embedding or projection matrices. The prepared-state bound is checked
before allocating RoPE tables.
It requires the real native runtime, a reference-qualification record, package
manifest and private row-worker configuration. Existing activation, qualification,
artifact/profile/generation/package, tokenizer, KV budget and durable-vote gates
remain in force. The flag does not activate the contract or qualify this route.

The GGUF reader computes the whole artifact BLAKE3 and a 64 KiB chunk hash index
in one streaming pass. All subsequent reads verify complete source chunks before
exposing bytes. The open descriptor keeps a path replacement from redirecting
reads; changed uncached bytes fail their digest. Previously cached bytes remain
the verified originals. A source row is dequantized using Candle's existing GGUF
reader, then prepared with the existing per-row I8 quantizer and Q/K permutation.
Embedding and norm conversions use the existing full loader's respective rules.
The resident and bounded coordinators share the forward and generation routines.

The existing private cohort still authenticates SSH hosts, measures links,
challenges workers, checks exclusions, reserves capacity, creates assignment
certificates, and performs seeded duplicate and local numerical spot checks.
Low-residency placement excludes the coordinator. Before the first forward, the
strict backend requires an open, eligible remote owner for every row of every
projection, including the output head, and a numerical spot check per stage.
Transport, identity, shape, coverage or numerical failure refuses the request;
no local generation or replacement primary slice is available. Local source rows
are used only for declared numerical checks/challenges. A refusal follows the
existing native failure/refund path and cannot produce a successful result vote.

`resident_output: true` explicitly adds output-head eligibility to a ranged private
worker. It does not add embedding residency or any transformer layer. Ranged
workers without it retain their old behavior. Public capability leases are not
extended by this change. The public serving context states whether complete row
coverage is required and whether local fallback is enabled; the loopback cohort
view exposes the same residency policy and actual measured worker state. Neither
configuration declarations nor an open connection establish performance readiness.

## Preparation and capacity

Build `tensor_row_low_residency_export` with the `candle` feature. It accepts
`--model`, `--artifact`, `--worker-id`, `--layers`, `--output-dir` and optional
`--include-output`, and produces the existing ARCROW01 files for
`tensor_row_stdio_worker`. Select at least one transformer layer per bundle so the
existing challenge/probe can validate that worker. Copy the manifest's
`resident_layers` and `resident_output` declarations into the private cohort
configuration. `max_workers` must permit the complete set required for coverage.
No default value guarantees that an arbitrary collection of bundles is coverable.

Preparation reads only the selected projection rows, one matrix at a time; it
never loads a full model and then drops unused weights. The aggregate exported
row files must fit the stdio worker's existing 1 GiB limit. The exporter records
conservative payload bounds for preparation (two largest-matrix copies) and
worker startup (total row bytes plus one largest-file conversion buffer). These
are calculated payload bounds, **not measured process RSS or host capacity**.
Worker serving retains its bounded rows and does not load GGUF embeddings/norms.
Do not use the full-model `tensor_row_model_worker` or original full-model bundle
exporter to infer suitability for an 8 GiB machine.

Before activation, measure simultaneous coordinator startup, every sidecar's cold
startup, node/legacy shard residency, allocator/process overhead, OS/page-cache
pressure, maximum admitted KV allocation, and failure/reconnect behavior on the
actual 8 GiB hosts. Each persistent SSH connection starts its own worker process:
count every validator cohort connection, not just distinct bundle paths. Six
coordinators pointing at one bundle through the old stdio command can create six
resident copies; that worker does not share a heap across processes. Use the
shared daemon/relay mode below so all those connections borrow one heap. Include duplicate-check and
cold-disk latency. Verify every
required layer and output-head row is held, worker limits leave host headroom,
measured placement succeeds, and a lost sidecar refuses rather than falls back.
The existing S10 real-machine comparison and model-quality/paid-path gates remain
unpassed by source changes or tiny fixture tests.

## Shared row service

Use `tensor_row_shared_worker serve --rows-dir /bundle/rows --artifact HASH
--socket /private-runtime-directory/rows.sock` once per bundle on its host.
Configure every coordinator's existing pinned-SSH transport to run
`tensor_row_shared_worker relay --socket /private-runtime-directory/rows.sock`.
Relays carry the unchanged row frames and hold no row weights. The daemon loads
one immutable bundle before serving, then all connections borrow those same
weights. An exclusive, nonblocking directory lock prevents a second service
from preparing another heap for the same bundle directory, even at another
socket path. Separate copies of a bundle in separate directories still count as
separate resident copies and must not be used to evade the capacity accounting.

The socket's parent must be owned by the service uid and deny group/other access;
the socket is mode 0600. Both endpoints check Unix peer uid, in addition to the
existing SSH host-key pin. Run the SSH relay as the service user. All files in a
bundle must carry the same exported worker ID, and requests must use that ID.
Give every validator cohort the same ID for this physical bundle; the node's
existing machine-address derivation already scopes it by validator. A second
alias for the same bundle is refused rather than counted as an independent
participant. Numerical checks, challenges, artifact/profile/call/input identity
and exact coverage are still enforced by the coordinator.

Limits are explicit: default 8 clients (maximum 32), one decoded request per
client, one compute slot, a 4 MiB frame bound, a total frame deadline, a call
budget including queue wait/compute/response writes, and an idle timeout. Compute
checks cancellation/deadline between at most 32 output rows. A malformed, timed
out or disconnected client closes its own connection. The daemon continues
serving other clients. Startup/stop stderr JSON records bundle copies (one),
logical resident row bytes, serialized bytes, startup payload bound and counters.
The worker's ordinary canonical kernel honors the existing SIMD feature setting;
this change introduces neither a result cache nor a claimed speedup.

Use a dedicated systemd `RuntimeDirectory` with `RuntimeDirectoryMode=0700`,
`RuntimeDirectoryPreserve=no`, matching service/SSH uid, and `KillMode=control-group`.
The deployment unit must let the old service cgroup finish before recreating its
runtime directory on restart. Normal SIGTERM/SIGINT drains bounded client work
and removes the socket it owns. SIGKILL can leave a stale socket: startup
intentionally refuses to unlink an existing path. For manual recovery, first
stop the owning unit, verify its process/cgroup is gone, then remove only that
unit's known socket from its private runtime directory and start the unit. Never
unlink a possibly live socket or launch another daemon to work around its lock.
Measure the entire coordinator/node/legacy/shared-daemon/relay cgroup set under
simultaneous cold startup and maximal admitted KV, including failure recovery.

## Verification scope

Tiny tests compare F32, Q4_0, Q4K, Q6K and Q8_0 source-row preparation against the
full canonical loader, including grouped-query Q/K permutation, embeddings,
logits, exact KV contents, context boundaries, tokens/hash, tied output weights
and byte-identical row exports. Additional tests reject missing tensors/norms,
changed source chunks, incomplete output coverage, malformed response identity,
transport loss and failed numerical checks. Shared-service tests cover two
independent clients, one retained bundle, rejection of alias worker IDs, duplicate
instance locks, malformed/disconnected clients and queue/frame/client bounds.
Run existing independent golden
vectors as well, because a shared forward routine alone cannot establish an
independent numerical oracle. None of these tests establishes real-model quality,
8 GiB RSS, network performance or production readiness.

## Offline real-model comparison

Build the candle example `low_residency_conformance` alongside
`tensor_row_low_residency_export` and `tensor_row_shared_worker`. On an isolated
Linux runner with the already downloaded, pinned artifact, run:

```sh
python3 scripts/qualification/run_low_residency_conformance.py \
  --binaries-dir /absolute/path/to/examples \
  --model /absolute/path/to/llama2-7b.gguf \
  --output-dir /absolute/path/to/new-result-directory
```

The runner downloads nothing. It verifies the package's SHA-256/size and the
executor verifies its BLAKE3. Defaults are explicit prompt IDs `[6324]` (without
BOS), two generated tokens, no warmup, and the scalar kernel. Optional
`--prompt-ids`, `--max-tokens 1..4`, `--warmups 0..1`, and `--kernel scalar|fast`
are recorded in every result. Each run uses fresh processes and a fresh output
directory; scalar/fast timings must not be pooled. The caller must keep the GGUF
immutable throughout the run; the resident reference rechecks its digest after
execution as well as before loading.

The runner first checks actual Linux `MemAvailable`, physical RAM, cgroup limits
and free disk. An unreadable cgroup memory constraint refuses; the optional
`memory.peak` observation may be unavailable. The resident-reference preflight requires at least
12 GiB available (or the calculated model/KV payload plus 3 GiB, whichever is
larger). This margin is an admission check, not a measured startup bound. Lower
capacity refuses instead of running the reference amid memory pressure.
The reference process completes and exits before any row preparation starts.
The existing bounded exporter then creates complete layer/output bundles under
the 1 GiB limit, and separate shared-daemon processes load each bundle exactly
once. The low-residency coordinator uses the actual Unix row protocol, complete
remote primary coverage, a local duplicate row and three local numerical spot
rows per projection. It has no resident reference or full-model fallback.

The reference uses the existing resident `forward_one_token` path, independently
of the backend forward being tested. Both runs replay generation-v2 selection,
including BOS once, repetition penalty, EOS inclusion and the final non-EOS
forward. Each records every position's input token, logit count, BLAKE3 of the
little-endian i64 logits, complete KV digest, generated IDs and output hash.
The runner compares these exact commitments and identities, excluding timing.
Warmup runs, when requested, have their own traces and are compared separately.

`summary.json` reports success only after this comparison; phase JSON, worker
manifests, process arguments and stderr logs remain available on failure.
Termination reaps child process groups before removing the private socket
directory. Every child must exit zero for success, including daemon shutdown;
forced cleanup cannot pass. The default global deadline is 2700 seconds
(`--total-timeout-seconds`, 60..5400); cleanup and evidence writing follow it.
The CI job must reserve time afterward for evidence upload. Row exports remain under `bundles/`; do not upload those large files
as CI logs. The reported 100 ms aggregate RSS sampling can miss peaks, per-process
`getrusage` excludes other processes, and an existing cgroup peak can contain
earlier or unrelated work. These limitations are included in the result.
This exercise establishes neither the production SSH/cohort path nor quality,
paid execution, real-host capacity or distributed latency qualification.


## Recovered v3 routing and local request readiness

An authorized migration adds native inference to the recovered v3 chain. Native
blocks retain protocol 3 headers and the recovery root commits both native account
and storage updates. Ordinary v3 admission rules remain in force. A native block
contains at most one transition and cannot mix native and ordinary transactions.

New local paid requests require `--enable-native-inference-requests`, which
requires `--native-inference-runtime`. Leave the request flag absent through
activation and while starting the qualified runtimes on the whole committee.
Enable it only after the operator has verified the coordinated fleet's readiness.
The flag is local rollout policy, not an attestation of fleet readiness, reference
conformance or model quality.

Even with the flag, admission stays closed until the runtime polls successfully.
The low-residency executor must also have a current measured placement covering
all primary projections, including the output head, passing the same strict
coverage preflight as execution. Readiness expires after five seconds without a
successful poll and closes on dependency failure, worker stop, error-limit exit
or panic. Long execution can therefore temporarily close new admission. A new
process starts without inherited health and must pass these checks again.

`/native-inference/context` remains readable and preserves `candidate_protocol: 4`
for the existing contract API. `chain_protocol` reports the actual chain major,
`native_only_chain` distinguishes a fresh native chain from a migrated chain,
and `request_admission_open` reports this node's local gate. The detailed
`request_admission` object separates operator opt-in from runtime readiness.

The gate covers signed RPC submissions, transaction gossip, local proposals and
mempool requeues. It never changes consensus validity: a valid committed request
is retained and executed even on a locally closed node. Workers still process
canonical pending jobs while admission is closed, and finalization and refunds
remain available. Closing admission cannot undo an already committed reservation.
