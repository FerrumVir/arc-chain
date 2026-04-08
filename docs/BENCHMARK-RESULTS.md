# ARC Chain — Factual Benchmark Report

Runs 10 factual prompts through the sharded inference pipeline and checks
each output for the expected keyword. Reproducible: anyone with curl + python3
can run `scripts/arc-bench.sh` against the live testnet and get the same
hashes (deterministic inference).

- **Coordinator**: `http://149.28.32.76:9090`
- **Date**: 2026-04-08 11:06:03 UTC
- **Max tokens per response**: 12

| # | Prompt | Expected | Output | Pass | ms/tok | tx_hash |
|---|--------|----------|--------|------|--------|---------|
| 1 | The capital of France is | paris | Paris.<0x0A><0x0A><0x0A> Unterscheidung between  Paris.<0x0A><0x0A> | ✓ | 14809 | `0x6a97e945fcaa…` |
| 2 | The largest planet is | jupiter | Jupiter, which is more than 1,31 | ✓ | 12811 | `0x1f3934939e75…` |
| 3 | The sky is | blue | (request failed) | ✗ | — | — |
| 4 | The fastest land animal is | cheetah | (request failed) | ✗ | — | — |
| 5 | The deepest ocean is | challenger | (request failed) | ✗ | — | — |
| 6 | The tallest mountain is | everest | (request failed) | ✗ | — | — |
| 7 | The currency of Japan is | yen | (request failed) | ✗ | — | — |
| 8 | The longest river is | nile | the Nile River, which flows through 110 | ✓ | 13368 | `0x660262b2fff8…` |
| 9 | The hottest planet is | venus | (request failed) | ✗ | — | — |
| 10 | The speed of light in a vacuum is | 299 | (request failed) | ✗ | — | — |

## Summary

- **Pass rate**: 3 / 10 (30%)
- **Average ms/token**: 4098
- **Pipeline length**: 7 shards (Llama-2-7B-Chat Q4_K_M, 32 layers split across 7 nodes in 7 cities)
- **All output_hashes** are deterministic — re-running this benchmark on any node will produce the same hashes

To reproduce:

```bash
bash scripts/arc-bench.sh
```

To verify any individual run, take its tx_hash from the table above and run:

```bash
bash scripts/arc-verify.sh <tx_hash>
```
