<stdin>:15: DeprecationWarning: datetime.datetime.utcnow() is deprecated and scheduled for removal in a future version. Use timezone-aware objects to represent datetimes in UTC: datetime.datetime.now(datetime.UTC).
# ARC Chain — Factual Benchmark Report

3 factual prompts run sequentially through the live 7-shard pipeline,
with 60s pause between requests to let the coordinator drain.

- **Coordinator**: `http://149.28.32.76:9090`
- **Date**: 2026-04-08 13:05:41 UTC
- **Pipeline**: 7 shards · Llama-2-7B-Chat Q4_K_M · 32 layers split across NYC→LAX→AMS→LHR→NRT→SGP→JNB

| # | Prompt | Expected | Output | Pass | ms/tok | tx_hash |
|---|--------|----------|--------|------|--------|---------|
| 1 | The largest planet is | jupiter |  Jupiter, which is more than 1,31 | ✓ | 14014 | `0xce11b0abe4ad…` |
| 2 | The capital of France is | paris |  Paris.<0x0A><0x0A><0x0A> Unterscheidung between  Paris.<0x0A><0x0A> | ✓ | 10173 | `0x018135bb1dbe…` |
| 3 | The longest river is | nile |  the Nile River, which flows through 110 | ✓ | 9377 | `0x660262b2fff8…` |

## Summary

- **Pass rate**: 3 / 3 (100%)
- **Unique output_hashes**: 3 / 3

Each tx_hash above can be independently verified by anyone with:

```bash
bash scripts/arc-verify.sh <tx_hash>
```

Or to verify the most recent run on the network:

```bash
bash scripts/arc-verify.sh --latest
```
