# How ARC's pipeline-parallel sharded inference works

> **Architecture history, not a current production guarantee.** The public-v2
> model ID binds a shape label rather than exact weight bytes, and a per-hop
> BLAKE3 digest detects changed bytes but does not prove an honest forward pass.
> The unreleased v0.8.0 candidate adds exact-artifact binding and authenticated
> independent recomputation; its blocking determinism evidence is currently a
> synthetic CPU ARM/x86 known-answer test, not every model or backend. The Aug
> 26 public fleet is forked/version-skewed. See
> [`PRODUCTION-RECOVERY-AUDIT-2026-08-26.md`](PRODUCTION-RECOVERY-AUDIT-2026-08-26.md).

A walkthrough of the architecture, the wire format, and the code paths for anyone who wants to understand or reproduce sharded LLM inference on ARC. Skip to the code reference at the bottom if you just want to read the actual files.

---

## The problem

Large language models don't fit on cheap machines. Llama-2-7B Q4_K_M is ~4 GB. Llama-2-13B Q4_K_M is ~7.4 GB. Llama-2-70B Q4_K_M is ~40 GB. A typical $5/month VPS has 8 GB of RAM. Anything bigger than 7B is out of reach for most operators - and even 7B is tight once you account for the dequantization buffers used during loading.

Centralized inference services solve this by running one big GPU with enough memory. Anyone who wants to use the model has to trust that GPU. There's no way to verify that the answer you got is the answer the model would have produced - you can't even tell if the operator silently swapped in a different model.

ARC solves both at once: split the model across N independent machines, hash every step, and arrive at an answer that any third party can re-derive bit-for-bit.

---

## The architecture

A transformer model consists of:

1. A **token embedding** table that converts a token id (an integer) into a hidden state vector (a few thousand floats)
2. **N transformer blocks** (Llama-7B has 32, Llama-13B has 40, Llama-70B has 80) - each block reads a hidden state and writes a new hidden state
3. A **final layer norm + LM head** that converts the final hidden state into a probability distribution over the vocab, then takes the argmax (or samples) to produce the next token id

ARC's sharding splits the N transformer blocks across multiple nodes:

```
                  Llama-2-7B - 32 transformer layers, 6-seed pipeline
                  with 3× replication per layer range

   token id           hidden state         hidden state         token id
      ↓                    ↓                    ↓                    ↑
  ┌───────┐  ─────►  ┌───────┐  ─────►   ┌───────┐  ─────►   ┌───────┐
  │ NYC   │          │ LAX   │           │ AMS   │           │ SGP   │
  │ [0,6) │  hidden  │[6,12) │  hidden   │[12,17)│   hidden  │[27,32)│
  │ +EMBED│  +BLAKE3 │       │  +BLAKE3  │       │  +BLAKE3  │+LM HD │
  └───────┘          └───────┘           └───────┘           └───────┘
```

- **Shard 0 (the first shard)** holds the token embedding table AND the first K transformer blocks. When a token id arrives, shard 0 looks up the embedding, runs blocks 0 through K-1, and forwards the resulting hidden state to shard 1.
- **Middle shards** hold a contiguous range of blocks. Each receives a hidden state from the previous shard, runs its blocks, and forwards to the next shard.
- **Shard N-1 (the last shard)** holds the final blocks AND the final layer norm + LM head. It receives a hidden state, runs its blocks, applies the LM head, takes the argmax, and returns the next token id to the coordinator.

The coordinator (any node that received the original `/inference/run_sharded` request) collects the token id and either repeats the loop (next token) or returns the result (max_tokens reached or EOS).

---

## What each shard actually does

Each shard runs `forward_shard_token` from `crates/arc-inference/src/cached_integer_model.rs`. This function takes either a raw token id (first shard only) or a hidden state, runs `[start_layer, end_layer)` of transformer blocks, and returns either a hidden state (intermediate shard) or a token id (last shard).

Inside the loop, for each transformer block:

1. **LayerNorm** the hidden state
2. **Q, K, V projections** - three INT16 matmuls of `(d_model)` against `(d_model × d_model)` weights
3. **RoPE** rotary positional encoding on Q and K
4. **Push K and V into the per-request KV cache** (this is why each shard has to remember per-request state across the multi-token generation loop)
5. **Multi-head attention** against the cumulative KV cache for this layer - Q dot K → softmax → weighted sum of V
6. **Wo projection + residual** - INT16 matmul to project the attention output back to `d_model`, add to the running hidden state
7. **FFN gate, up, down** - three INT16 matmuls with SiLU activation: `down(silu(gate(x)) * up(x))`
8. **Add to hidden state**

After all blocks in this shard's range have run, if it's the **last shard** the function also runs:

9. **Final LayerNorm**
10. **LM head matmul** (INT16, `d_model × vocab_size`)
11. **Argmax** of the logits → next token id
12. **BLAKE3 hash of the logits** for verification

The **per-row INT16 quantization** is important. Each weight row has its own scale factor. Weights are stored as `i16`, accumulators are `i64`, the matmul does pure integer arithmetic. There's no floating-point anywhere in the forward pass - that's how you get bit-identical output across ARM, x86, and (eventually) RISC-V. For the math, see `matmul_i16_into` in the same file.

---

## The wire format

Activations between shards are sent as `Vec<i64>` serialized to little-endian bytes with a BLAKE3 integrity hash. The HTTP body is JSON for now (small payloads, easy to debug):

```json
POST /inference/forward_shard
{
  "request_id": "0xaa2b6de5...",       // unique per inference request
  "hidden": [12345, -67890, ...],       // i64 hidden state, d_model entries
  "hidden_hash": "0xabc123...",         // BLAKE3 of the hidden state bytes
  "position": 3,                         // token position in the sequence
  "start_layer": 5,                      // shard's layer range (sanity check)
  "end_layer": 10
}
```

The receiving shard:
1. Verifies it actually holds `[start_layer, end_layer)` (rejects if not - protects against pipeline confusion)
2. Decodes the hidden state and verifies the BLAKE3 hash matches what it received (protects against in-flight corruption)
3. Looks up the per-request KV cache (creates a new one if this is the first call for `request_id`)
4. Runs `forward_shard_token` for its layer range
5. Returns either a hidden state + new BLAKE3 (intermediate) or a token id + logits hash (last shard)

For the first shard, the request body has `"token": <u32>` instead of `"hidden"`, and the shard does the embedding lookup before running its layers.

---

## The coordinator

`POST /inference/run_sharded` is implemented in `crates/arc-node/src/rpc.rs`. The handler:

1. Looks up the local `ShardRegistry` (populated via `/shards/announce` gossip from the rest of the network)
2. Verifies the pipeline is **fully covered** - every layer from 0 to `n_layers` is held by some shard, with no gaps
3. Tokenizes the user's input and prepends the BOS token
4. For `position` in `[0, prompt_len + max_tokens)`:
   - Picks the input token (from prompt or from last generated token)
   - Walks the pipeline shard-by-shard via HTTP `forward_shard` calls, threading the hidden state through
   - The last shard returns a token id, which the coordinator appends to `generated`
5. After `max_tokens` (or EOS), sends a cleanup request to each shard to evict the per-request KV cache
6. Returns the full output text + per-hop trace + total bytes transferred

The per-hop trace is what the dashboard renders as the live pipeline diagram. Each entry in the trace is `{hop, node, layers, compute_ms, wall_ms, payload_bytes, is_terminal}`.

---

## Sharded loading

Each node loads ONLY its slice of the model. The full GGUF file is mmap'd, but only the layer ranges that this shard holds are extracted, dequantized to f32, then re-quantized to per-row INT16 (and INT8 as a fallback) and stored in the model struct. Layers outside the held range get `CachedLayer::placeholder()` - empty `I8Weights` / `I16Weights` structs that take ~144 bytes each (essentially zero memory).

This is the key to fitting the model on small machines. A 7B Q4 split 7 ways uses:
- Disk: full 4 GB GGUF on each node (mmap'd, can be paged out)
- RAM: ~1 GB of layer weights + dequant buffer per shard

A 70B Q4 split 14 ways would use ~3 GB of RAM per shard, which fits on 8 GB VPS. (We haven't run that yet - there's a known loader OOM issue with 13B+ on tight RAM that we're working around by using bigger boxes. The shard math still works.)

The first shard (`start_layer == 0`) additionally loads the token embedding table. The last shard (`end_layer == n_layers`) additionally loads the final layer norm + LM head. Other shards skip both. See `load_cached_model_shard` in `crates/arc-inference/src/cached_integer_model.rs`.

---

## Determinism guarantees

ARC's sharded inference produces bit-identical output regardless of:

1. **Which node is shard 0 vs shard 1 vs shard 6** - as long as each holds the right layer range and the same model weights, the result is identical
2. **CPU architecture** - ARM, x86, RISC-V all produce the same bytes because the math is integer-only
3. **OS / OS version** - there are no system call dependencies in the forward pass
4. **Number of CPU cores** - the matmul uses rayon for parallelism but the fold order is deterministic
5. **Time of day, network jitter, packet ordering** - none of these affect the computation

The proof: every wire-format hidden state is hashed with BLAKE3. The receiving shard verifies the hash before running its computation. The final logits get hashed too. Run the same prompt twice, you get the same `output_hash`. Run the same prompt on a completely different network with the same model file and the same shard layout, you get the same `output_hash`.

This is what makes the model verifiable. Anyone can re-derive the answer and check.

---

## Why per-request KV caches

A transformer's self-attention reads ALL previous K and V values for the current layer when computing the next token. That's the "context window" - every position attends to every prior position.

In a non-sharded forward pass, the KV cache grows on each token: at position `t`, layer `l` has accumulated K and V vectors for positions `[0, t]`.

In sharded inference, EACH shard owns its own KV cache for its layer range. When the coordinator runs a multi-token generation, the same `request_id` is used for every token; each shard looks up its own cache for that `request_id`, appends the new K/V from this position, and runs attention against the full cache.

When two different requests arrive concurrently, they have different `request_id`s and therefore different KV cache slots. The shard's `DashMap<request_id, Mutex<KVCache>>` keeps them isolated. When the request completes (max_tokens or EOS), the coordinator sends a cleanup signal and the cache entry is evicted.

This is what makes the 10-concurrent stress test possible: 10 different prompts produce 10 different output hashes because their KV caches don't bleed into each other.

---

## Code reference

- `crates/arc-inference/src/cached_integer_model.rs` - `load_cached_model_shard`, `forward_shard_token`, `ShardInput`, `ShardOutput`, `KVCache`
- `crates/arc-inference/src/distributed.rs` - `ShardRegistry`, `ShardAssignment`, `compute_shard_plan`, `serialize_activations`
- `crates/arc-node/src/rpc.rs` - `inference_run_sharded` (coordinator), `inference_forward_shard` (per-shard handler), `get_shards`, `announce_shard`
- `crates/arc-node/src/main.rs` - `--shard-start` / `--shard-end` CLI flags, ShardInfo broadcaster + puller
- `dashboard/index.html` - `runShardedInference()`, `refreshShardRegistry()`, the visual pipeline replay

---

## What's next

The current implementation is the simplest correct version of pipeline-parallel sharded inference. There are obvious improvements:

- **Pipeline overlap**: start the next token's forward through shard 0 while the previous token is still flowing through shard N-1. Should ~Nx the throughput for long generations.
- **Smaller wire format**: hidden states are currently sent as i64 JSON arrays (~25 KB per hop for d_model=4096). Switching to msgpack or raw little-endian binary would cut that to ~32 KB → ~33 KB total per token. Still small but free latency win.
- **Speculative shards**: a follower shard could speculatively run on a guessed previous-shard output and either commit or discard based on the actual hash. Only useful for very high-latency links.
- **Heterogeneous shards**: a beefy node holds multiple layer ranges, a small node holds one. The shard plan auto-balances by available RAM.
- **GPU shards**: drop the integer engine on the hot path, use Metal/CUDA when available, while still hashing the inputs and outputs for verification. Would let you mix GPU farms and cheap VPS in the same pipeline.

None of these are blockers for the demo. They're just future work.
