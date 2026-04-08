# ARC Chain — Factual Benchmark Report

Runs 10 factual prompts through the sharded inference pipeline and checks
each output for the expected keyword. Reproducible: anyone with curl + python3
can run `scripts/arc-bench.sh` against the live testnet and get the same
hashes (deterministic inference).

- **Coordinator**: `http://149.28.32.76:9090`
- **Date**: 2026-04-08 11:25:47 UTC
- **Max tokens per response**: 12

| # | Prompt | Expected | Output | Pass | ms/tok | tx_hash |
|---|--------|----------|--------|------|--------|---------|
| 1 | The capital of France is | paris | Paris.<0x0A><0x0A><0x0A> Unterscheidung between  Paris.<0x0A><0x0A> | ✓ | 16633 | `0x2336b4b04418…` |
| 2 | The largest planet is | jupiter | Jupiter, which is more than 1,31 | ✓ | 9206 | `0x1780cfc08bc5…` |
| 3 | The fastest land animal is | cheetah | (request failed) | ✗ | — | — |
| 4 | The longest river is | nile | (request failed) | ✗ | — | — |
| 5 | The tallest mountain is | everest | (request failed) | ✗ | — | — |

## Summary

- **Pass rate**: 2 / 5 (40%)
- **Average ms/token**: 5167
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
