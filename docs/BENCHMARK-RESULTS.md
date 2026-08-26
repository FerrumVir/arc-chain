# ARC Chain - Factual Benchmark Report

> **Historical benchmark:** this run was captured against the April 7-shard,
> 1×-replica topology. At the later 2026-04-22 snapshot, the cluster reported 6
> layer ranges with 3× replication each. Neither topology is current production
> evidence: the 2026-08-26 fleet is forked/version-skewed, and v0.7.12 is not
> deployed. Per-hop numbers and receipt links below are point-in-time evidence,
> not a spec; re-run them before quoting them.

3 factual prompts run sequentially through the then-live 7-shard pipeline,
with 60s pause between requests to let the coordinator drain.

- **Coordinator**: `http://149.28.32.76:9090`
- **Date**: 2026-04-08 13:05:41 UTC
- **Pipeline (historical)**: 7 shards · Llama-2-7B-Chat Q4_K_M · 32 layers split across NYC→LAX→AMS→LHR→NRT→SGP→JNB

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
