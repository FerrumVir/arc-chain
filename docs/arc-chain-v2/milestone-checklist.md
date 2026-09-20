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
  - [x] Candidate state is private genesis-only protocol 4; protocol 3/default admission rejects native families.
  - [x] 371 state tests and 16 receipt-link tests passed; full-block fsync precedes financial publication; bounded signed transitions and restart checks passed.
  - [x] Concrete node adapters and the bounded native flow test passed: one persistent StateDB, six keys, five workers/signatures, synthetic executor, mempool→request→votes→finalize→receipt links→reopen→refund/replay; one existing generic signed-ingress test also passed.
  - [x] New RPC activation/receipt test passed 1/1; native worker tests passed 6/6 and the existing generic signed-ingress test passed.
  - [ ] Native production startup remains unwired; no live activation is claimed.

- [ ] **3. Distributed work sharing — IN PROGRESS**
  - [x] Six live gateway probes returned HTTP 200 and collectively advertised layer ranges 0–32.
  - [x] Bounded two-machine forward query matched exact logits for 3/3 inputs.
  - [ ] Performance/fleet gate: partitioned execution was 34.05× slower than local; five other gateway hosts remain inaccessible by configured SSH identities. Authenticated distinct holders, arbitrary-WAN speedup, and production worker inference remain unproven.

- [ ] **4. Resilience and recovery — TODO**
  - [ ] Failure, retry, replay, checkpoint, finality, snapshot, and live-P2P acceptance remain pending; evidence is private-WAL-only.

- [ ] **5. Real product acceptance — TODO**
  - [ ] Explorer and node journeys, observability, and user-facing error handling remain pending.

- [ ] **6. Release and soak — TODO**
  - [ ] Signed release builds, long-duration soak, resource limits, upgrade/restart drills, and release review remain pending.

Account-wide usage was last observed at 90% used / 10% remaining; this is not attributable per task. No competitor benchmark has run.
