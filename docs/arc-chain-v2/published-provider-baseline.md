# Published provider baseline

This is a contextual comparison for ARC measurements. It is not a live API test,
and published provider figures are not an ARC SLA or a same-model proof.

| Source | Published figure and conditions | Limit |
|---|---|---|
| [Fireworks Llama 4 Maverick](https://fireworks.ai/blog/llama4-maverick) (accessed 2026-09-19) | 145 output tokens/s streaming, independently tested by Artificial Analysis (2025-04-27), Llama 4 Maverick on H200 | Model/GPU/config specific; no universal TTFT or API guarantee |
| [FireAttention V4](https://fireworks.ai/blog/fireattention-v4-fp4-b200) (accessed 2026-09-19) | >250 tokens/s, DeepSeek V3 FP4 on NVIDIA B200; speculation/MTP disabled | Different model from Maverick; vendor benchmark, not TTFT |
| [Fireworks pricing](https://fireworks.ai/pricing) (accessed 2026-09-19) | On-demand from 2026-09-01: H100/H200 $8/GPU-hour, B200 $13, B300 $15, GB300 $20; region restricted 1.5x | GPU-hour price, not a model throughput promise; serverless model prices vary |
| [Fireworks serverless pricing](https://docs.fireworks.ai/serverless/pricing) (accessed 2026-09-19) | “Other base models,” 4B–16B: $0.20 per 1M tokens, uniform input + output | Pricing category is not proof that Llama-2-7B is currently available under it |

ARC's calculator (`scripts/benchmarks/project_inference_cost.py`) takes total
output tokens and benchmark duration, then reports measured tokens/sec and
`benchmark_cost / output_tokens * 1e6`. Benchmark cost is worker hourly USD ×
duration/3600, plus measured egress bytes × USD/GB, plus verification worker
seconds × hourly rate. Concurrency is recorded for context and never multiplied
into aggregate measured throughput. Missing or partial cost inputs return
`unknown` or fail validation; verification already included in worker time must
be marked `--verification-included` to avoid double counting. The script rejects
non-finite or non-positive units and includes egress, verification, concurrency,
missing-input, and invalid-unit self-tests (`--self-test`).
Comparisons must state model, hardware, precision, context, concurrency, TTFT,
and output rate; the 145 and >250 figures are not directly comparable to each
other or to ARC until those conditions are measured.

For a later distributed query, report effective end-to-end output rate as
`output_tokens / (prefill_seconds + decode_seconds)` and separately report
inter-token decode rate as `(N_output_tokens - 1) / (time_last_token -
time_first_token)`. A two-token proof therefore reports one decode interval,
not two tokens per interval; one interval is neither a sustained throughput
benchmark nor a p50/p95 distribution. It should not be compared directly with
a provider's single-interval headline. For the published $0.20/1M-token category,
the request-pattern break-even ceiling is
`(input_ids + output_ids) × 0.20 / 1e6 × 3600 / partitioned_seconds` USD/hour.
This is a request-pattern ceiling, not a claimed worker price or provider
availability statement.
