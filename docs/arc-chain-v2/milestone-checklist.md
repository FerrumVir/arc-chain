# ARC Chain V2 milestone checklist

- [ ] **1. Real INT8 correctness, resources, and quality — IN PROGRESS**
  - Acceptance requires a same-model Fireworks benchmark covering quality, reliability, speed, and total cost; deterministic token/hash output alone is insufficient.
  - [x] Artifact identity and canonical harness checks passed.
  - [x] The first 8-token real generation was deterministic with matching token-byte/output hashes, but echoed prompt-like text.
  - [ ] Quality gate: subsequent 3 checks × 6 generations × 32 tokens failed Paris, arithmetic, and greeting coherence checks. Source Llama2 Q4_K_M→INT8 is diagnostic evidence, not a paid API benchmark.
  - [ ] Interleaved GGUF RoPE versus split-half kernel mismatch is the current suspected cause; a new profile and reference implementation are pending. No fix is claimed.
  - [x] One corrected ARC minimal-Paris 8-token run matched the same-GGUF llama.cpp reference's exact eight completion IDs (including BOS); this is a single-prompt correctness subgate only.
  - [x] Reference llama.cpp tokenization matches ARC IDs for three vectors; this is limited tokenization proof only.

- [ ] **2. Paid canonical integration — IN PROGRESS**
  - [x] Candidate state is private genesis-only protocol 4; protocol 3/default admission rejects native families.
  - [x] 371 state tests and 16 receipt-link tests passed; full-block fsync precedes financial publication; bounded signed transitions and restart checks passed.
  - [x] Concrete node adapters and the bounded native flow test passed: one persistent StateDB, six keys, five workers/signatures, synthetic executor, mempool→request→votes→finalize→receipt links→reopen→refund/replay; one existing generic signed-ingress test also passed.
  - [x] New RPC activation/receipt test passed 1/1; native worker tests passed 6/6 and the existing generic signed-ingress test passed.
  - [ ] Native production startup remains unwired; no live activation is claimed.

- [ ] **3. Distributed work sharing — IN PROGRESS**
  - [x] Six live gateway probes returned HTTP 200 and collectively advertised layer ranges 0–32.
  - [ ] All six reported the same holder `arc-68b19960` and `0.0.0.0:9944`; `profile_bound=false`, `fully_covered=false`, and `dispatch=false`. Authenticated distinct holders, network-parallel querying, and speedup remain unproven.

- [ ] **4. Resilience and recovery — TODO**
  - [ ] Failure, retry, replay, checkpoint, finality, snapshot, and live-P2P acceptance remain pending; evidence is private-WAL-only.

- [ ] **5. Real product acceptance — TODO**
  - [ ] Explorer and node journeys, observability, and user-facing error handling remain pending.

- [ ] **6. Release and soak — TODO**
  - [ ] Signed release builds, long-duration soak, resource limits, upgrade/restart drills, and release review remain pending.

Account-wide usage was last observed at 84% used / 16% remaining; this is not attributable per task. No competitor benchmark has run.
