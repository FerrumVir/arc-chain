# Two-machine distributed forward proof (2026-09-19)

The bounded physical query produced exact logits for 3/3 tested forward inputs across the local oracle and partitioned path:

| Input index | Input token | Exact logits | Local oracle | Partitioned |
|---:|---:|---|---:|---:|
| 0 | 1 | yes | 1,239 ms | 55,018 ms |
| 1 | 6,324 | yes | 1,719 ms | 51,673 ms |
| 2 | 29,892 | yes | 1,657 ms | 50,482 ms |

Evidence: `/Users/excaulibur/work/outputs/arc-chain-readiness-20260919/tensor-two-machine-query.jsonl`. The final aggregate reports TTFT 106,692 ms, decode interval 50,482 ms, output 2 tokens, input IDs 2, and speedup 0.029371926 (34.05× slower). Each forward made 225 remote projection calls, for 675 calls across the three forwards. Amsterdam computed the first 173,984 of 1,391,872 projection rows (one eighth), while the Mac computed the other seven eighths; attention, normalization, and token selection remained on the Mac. This is a correctness proof with a measured performance failure, not a speedup result. Three samples do not establish throughput distributions, and arbitrary WAN conditions cannot guarantee speedup. The five other gateway hosts remain inaccessible through configured SSH identities, so this is not a six-node or production-fleet proof.

For context only, [Fireworks serverless pricing](https://docs.fireworks.ai/serverless/pricing) publishes $0.20 per 1M input+output tokens for its 4B–16B “Other base models” category. At this request pattern (2 input IDs + 2 output IDs), the hypothetical token charge at that published rate would be $0.0000008 per request; the measured partitioned interval implies a request-pattern break-even ceiling of $0.0000183235/hour before extra costs. This is not a like-for-like model, hardware, latency, or commercial comparison, and does not assert Llama-2-7B availability in that category. ARC total cost remains unknown. The raw `Hi` prompt produced IDs `[29892, 306]`, which is diagnostic token evidence, not chat-quality evidence; input IDs include BOS as a diagnostic accounting convention.

Resource evidence is bounded and host-specific: the Linux sidecar exited 0 in 158.97 s with 811,008 KiB maximum RSS and no swap; its service was inactive and not found after the test. The Mac process wall time was 180.89 s with 6,846,021,632 bytes maximum RSS, 8,161,679,696 bytes peak footprint, and no swap. The 225 ARC row files total 827,306,469 bytes identically in both modes. The separate `export_bytes` value of 827,326,198 includes a 19,729-byte manifest and is directory-accounting overhead, not frame bytes.

Post-test health was HTTP 200, version 0.8.0, height 709305, peers 5, validators 6, and `chain_advancing=true` with zero-second block age.

No model-quality or API-parity claim follows from these logits. The physical result closes only this bounded exact-forward subgate; worker inference, broader quality, resilience, and release milestones remain open.

The tested code was commit `4c305f7`; 122 library tests passed and 1 was ignored. This was an isolated AMS sidecar test, with no validator restart or production rollout.
