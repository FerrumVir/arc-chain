# Performance comparison - what ARC trades for what

> **Historical comparison — do not quote as current capability or performance.**
> The tables below mix an April-era seven-node topology, external estimates,
> and design claims. They do not describe the forked/version-skewed public fleet
> observed on 2026-08-26 or the unreleased v0.7.12 candidate. Public-v2 hashes do
> not bind exact model bytes; transit hashes alone are not recomputation proof;
> and current evidence does not establish RISC-V, GPU, full-GGUF, permissionless
> work assignment, or one-command production participation. Re-benchmark the
> exact commit, model, backend, topology, and hardware before publishing a
> comparison. See
> [`PRODUCTION-RECOVERY-AUDIT-2026-08-26.md`](PRODUCTION-RECOVERY-AUDIT-2026-08-26.md).

If you're evaluating ARC's sharded inference against a centralized API or a single-machine local LLM, the latency difference is real and the answer is not "ARC is faster." Here is the honest comparison and what you actually get for the slower number.

## Latency

| Setup | Latency per token | What's running | What you trust |
|-------|------------------|----------------|----------------|
| OpenAI / Anthropic API | **~30-100 ms** | One operator's GPU farm | The operator |
| Single-machine `llama.cpp` (CPU) | **~50-200 ms** | A 7B model on your laptop | Your machine |
| Single-machine `llama.cpp` (GPU) | **~10-50 ms** | A 7B model on a $1500+ GPU | Your machine |
| **ARC sharded (current)** | **~12-15 sec** | A 7B model split across 7 cheap VPS over the public internet | Anyone with the model file can re-derive any answer bit-for-bit |

ARC is roughly **100-1000× slower per token** than centralized inference. That's because the request goes through 7 HTTP hops, each one parsing JSON, verifying a BLAKE3 hash, and running compute on a different machine in a different city. The bottleneck is not compute - it's network round trips.

We're not pretending otherwise.

## What you get for the slower latency

| Property | Centralized API | Single-machine local | ARC sharded |
|----------|----------------|---------------------|-------------|
| Verifiable output | ❌ No way to prove the answer came from the claimed model | ⚠️ Yes, but only on your one machine | ✅ Anyone can re-derive any past answer with the same hash |
| Cross-platform determinism | ❌ FP non-determinism, model swapped silently anytime | ❌ Different chips → different bytes | ✅ Pure i64 + INT16 → bit-identical on ARM, x86, RISC-V |
| Permissionless to participate | ❌ The operator decides who can run nodes | n/a | ✅ One curl command → you're a node |
| Permissionless to use | ⚠️ Account, credit card, rate limit, content policy | ✅ Local | ✅ HTTP POST, no account |
| Memory required per node | Whole model (40+ GB for 70B) | Whole model | **1/N of the model** (~1 GB per node for 7B sharded 7 ways) |
| Hardware required | $20K H100 for big models | $1500+ GPU for 7B | Cheap $5/month VPS - and it doesn't even need a GPU |
| Single point of failure | Yes - operator outage = no inference | n/a | No - any shard can be replaced by another holder of the same layer range |
| On-chain receipt | None | None | Every run produces an `InferenceAttestation` TX with model_id + input_hash + output_hash |

## When to use what

- **Use a centralized API** when you don't care who knows what you asked, you don't need the answer to be auditable, and 30 ms latency matters.
- **Use a local LLM** when you want privacy and you have a beefy machine.
- **Use ARC sharded** when:
  - You need to *prove* the answer came from a specific model with specific weights
  - You need the inference to be re-derivable months later by an auditor
  - You're running a model that's bigger than any single machine you can afford
  - You need permissionless participation (anyone can join, no operator gatekeeping)
  - You need bit-identical output across heterogeneous chips
  - The use case can tolerate seconds-per-token latency (governance, audit, consensus, scientific reproducibility, on-chain settlement, agent coordination)

## Where the latency goes

For a single token through the 6-shard pipeline with 3× replication per range (~10-13 sec):

| Step | Time | % |
|------|------|---|
| HTTP roundtrip NYC → LAX → AMS → LHR → NRT → SGP | ~7-8 sec | ~65% |
| Per-shard compute (5 transformer layers each, INT16 matmul on CPU) | ~1-2 sec | ~15% |
| BLAKE3 hash verification at each hop | ~50 ms | <1% |
| JSON encode/decode at each hop | ~100 ms | ~1% |
| Coordinator orchestration | ~100 ms | ~1% |
| Slow links (LAX has 6-12 sec wall on the first hop because of NYC → LAX RTT) | varies | up to 50% on cold runs |

## Where the latency could go

This is the simplest correct version. There are obvious wins waiting:

- **Pipeline overlap** - start the next token's forward through shard 0 while the previous token is still in transit. Should give roughly N× speedup for long generations. Currently O(N · pipeline_length); could be O(N + pipeline_length) per N tokens.
- **Smaller wire format** - hidden states are sent as i64 JSON arrays (~25 KB per hop). Switching to raw little-endian binary or msgpack would cut that to ~32 KB total per token across all hops.
- **GPU shards** - drop the integer engine on the hot path, use Metal/CUDA where available, while still hashing the inputs and outputs for verification. Mixes GPU farms and cheap VPS in the same pipeline.
- **Closer-together shards** - currently the 6 seeds span 3 continents (N. America · Europe · Asia). Latency is dominated by RTT. A pipeline of 6 nodes in the same datacenter would drop wall time by 5-10×.
- **Batched generation** - coordinator pre-encodes multiple input tokens, ships them in one HTTP body, last shard returns multiple output tokens. Cuts HTTP overhead per token.

None of these change the fundamental property: the output is still bit-identical and any third party can still re-derive it. The slowness is currently a network artifact, not an algorithm artifact.

## Bottom line

ARC sharded inference is the only way we know to run a real LLM across independent operators in a way that anyone can later prove a specific output came from a specific model with specific weights. The latency tradeoff is real and the optimization roadmap is concrete. For the use cases where verifiability matters more than latency, this is the only thing that exists.

For everyone else: use OpenAI and accept that you're trusting the operator.
