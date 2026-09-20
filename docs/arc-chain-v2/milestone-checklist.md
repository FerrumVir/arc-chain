# ARC Chain V2 milestone checklist

- [ ] **1. Real INT8 correctness, resources, and quality — IN PROGRESS**
  - Acceptance requires comparable quality, reliability, latency, and total-cost evidence; deterministic token/hash output alone is insufficient. Published provider figures are projection context, not a like-for-like paid benchmark.
  - [x] Artifact identity and canonical harness checks passed.
  - [x] The first 8-token real generation was deterministic with matching token-byte/output hashes, but echoed prompt-like text.
  - [ ] Historical quality run: 3 checks × 6 generations × 32 tokens failed Paris, arithmetic, and greeting coherence; this is superseded by the current bounded 6/6 content checks and 5/6 direct sampled-sequence matches. Source Llama2 Q4_K_M→INT8 remains diagnostic evidence, not a paid API benchmark.
  - [x] The versioned interleaved-RoPE profile and reference implementation are present; broad model qualification remains open.
  - [ ] Direct same-GGUF traces match 5/6 sampled greedy sequences. Paris matches the first eight content IDs, then llama.cpp emits EOG while ARC continues to its bound; this is not an exact sampled-ID match.
  - [x] Reference llama.cpp tokenization matches ARC IDs for 11/11 vectors; this is profile evidence, not full tokenizer qualification.

- [ ] **2. Paid canonical integration — IN PROGRESS**
  - [x] Private activation is now wired into node startup behind a default-off
        `--native-inference-activation <PATH>` flag. Verified end-to-end against a
        real `arc-node` process: without the flag `GET /native-inference/context`
        returns HTTP 404; with it the node logs the activation and the endpoint
        returns HTTP 200 carrying the commitment, genesis binding, members and
        validator-set hash. Six unit tests cover the operator format, pin
        mismatches, an empty allowlist, a committee-less observer and a chain
        past height 0. This activates the CONTRACT; production model execution
        remains behind `CanonicalI8NativeExecutor::load_qualified`.
  - [x] Candidate state is private genesis-only protocol 4; protocol 3/default admission rejects native families.
  - [x] 371 state tests and 16 receipt-link tests passed; full-block fsync precedes financial publication; bounded signed transitions and restart checks passed.
  - [x] Concrete node adapters and the bounded native flow test passed: one persistent StateDB, six keys, five workers/signatures, synthetic executor, mempool→request→votes→finalize→receipt links→reopen→refund/replay; one existing generic signed-ingress test also passed.
  - [x] New RPC activation/receipt test passed 1/1; native worker tests passed 6/6 and the existing generic signed-ingress test passed.
  - [ ] Native production startup remains unwired; no live activation is claimed.

- [ ] **3. Distributed work sharing — IN PROGRESS**
  - [x] Six live gateway probes returned HTTP 200 and collectively advertised layer ranges 0–32.
  - [x] Bounded two-machine forward query matched exact logits for 3/3 inputs.
  - [ ] Performance/fleet gate: partitioned execution was 34.05× slower than local; five other gateway hosts remain inaccessible by configured SSH identities. Authenticated distinct holders, arbitrary-WAN speedup, and production worker inference remain unproven.

- [ ] **4. Resilience and recovery — IN PROGRESS**
  - [x] A separate-process, separate-store multi-node fixture exists
        (`scripts/arc-multinode-fixture.sh`): it derives each validator address
        from a deterministic dev seed, writes a shared disposable genesis, starts
        N independent OS processes with their own data dirs, requires a real
        quorum, compares state roots at a COMMON height, and SIGKILLs a node and
        restarts it as a new process.
  - [ ] **Peering does not currently work between separate processes.** All
        nodes accept the shared genesis and report `validators=N`, the QUIC
        listeners bind ("P2P transport listening on 127.0.0.1:<port>"), and dials
        are issued - but every connection times out and `peers` stays 0, so
        consensus reports "Round stalled, but no authenticated quorum
        view-change certificate is available" and height never leaves 0. This
        reproduces independently of genesis (the soak harness hit the same wall).
        Replica agreement therefore remains UNPROVEN, and this is now a concrete
        transport defect to chase rather than a missing harness.
  - [ ] Failure, retry, replay, checkpoint, finality and snapshot acceptance
        remain pending; `Consensus::generate_finality_proof` is still a
        fail-closed stub returning `None`.

- [ ] **5. Real product acceptance — IN PROGRESS**
  - [x] Product suites re-run with evidence that proves execution rather than
        implying it: six suites with captured exit codes, and a Playwright JSON
        report showing 214 expected / 0 unexpected / 0 flaky / 4 skipped. The
        runner now aggregates failures and exits nonzero, refuses to reuse a run
        tag, and fails on a missing, unparseable or zero-test report.
  - [x] Wallet error handling: two swallowed `catch(e) {}` blocks on the balance
        refresh and the post-selection `/health` read now report instead of
        leaving a stale balance or a permanently "Connecting..." header.
  - [ ] The 4 skipped Playwright specs are exactly the live-backend ones; they
        need `ARC_LIVE_PORT` and a real node. **No live-backend journey is
        covered**, so mock-suite success must not be read as product acceptance.

- [ ] **6. Release and soak — IN PROGRESS / EXTERNALLY BLOCKED**
  - [x] `scripts/soak_test.sh` rewritten to be able to fail. It previously passed
        three flags that do not exist, passed `--benchmark` to a binary built
        without the feature that defines it, never checked startup, and reported
        a verdict its own exit status contradicted (`rc` mutated inside a
        subshell). Eight self-test cases now cover status propagation,
        divergence vs lag, missing samples and resource bounds.
  - [x] Bounded local soak demonstrated: 2 nodes, 150 s, Arc-tier stake,
        `Verdict: PASS` with process exit 0 and `exitcode.txt` 0, both nodes
        alive, 0 errors, heights 133/124, peak RSS 1222/1411 MB inside bound.
        A controlled failure case agrees in the other direction (exit 1).
  - [ ] This is **not** the required long-duration soak: minutes against a
        24-hour requirement, on a debug binary, three nodes on one host.
  - [ ] Signing remains **externally blocked**: `tauri.conf.json` uses ad-hoc
        macOS signing and a null Windows thumbprint; no Developer ID or
        Authenticode certificate exists in the repo.

Account-wide usage was last observed at 90% used / 10% remaining; this is not attributable per task. No competitor benchmark has run.
