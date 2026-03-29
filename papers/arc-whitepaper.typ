// ARC: A Deterministic Blockchain for Verifiable AI Inference
// TJ Dunham
// March 2026

#set document(
  title: "ARC: A Deterministic Blockchain for Verifiable AI Inference",
  author: "TJ Dunham",
)

#set page(margin: (x: 1.2in, y: 1.2in))
#set text(font: "New Computer Modern", size: 11pt)
#set par(justify: true, leading: 0.65em)
#set heading(numbering: "1.")

#align(center)[
  #text(size: 20pt, weight: "bold")[ARC: A Deterministic Blockchain for\ Verifiable AI Inference]

  #v(0.5em)

  #text(size: 12pt)[TJ Dunham]

  #text(size: 10pt, style: "italic")[tj\@arc.ai]

  #v(0.3em)

  #text(size: 10pt)[March 28, 2026]

  #v(1em)
]

#text(weight: "bold")[Abstract.]
We present ARC, a Layer 1 blockchain purpose-built for verifiable AI inference. ARC solves the fundamental problem that prevents AI computation from being trustlessly verified: the non-determinism of IEEE 754 floating-point arithmetic across hardware platforms. By constructing a pure integer inference engine that eliminates all floating-point operations from the neural network forward pass, ARC achieves bitwise identical inference output across ARM, x86, and GPU architectures. This platform determinism enables: (1) cryptographic verification of inference via hash comparison at O(1) cost, (2) distributed inference across untrusted heterogeneous devices without inter-node validation, (3) expert-parallel sharding of Mixture-of-Experts models across community GPU nodes at 15x lower cost than centralized providers, and (4) STARK proofs of neural network computation for cryptographic certainty. The system processes 33,230 transactions per second on a two-node network with DAG consensus, runs 7-billion parameter models at 76ms per token, and operates across 8 nodes on 6 continents producing identical output hashes. ARC introduces three inference verification tiers (all-execute, optimistic with fraud proofs, and STARK-proven), a VRF-based committee selection mechanism for large model verification, zero-fee AI agent settlements as first-class transaction types, and an EIP-1559-style inference gas lane. The system is implemented in 99,000 lines of Rust across 14 crates with 1,209 tests. A formal treatment of the underlying determinism requirement is given in our companion paper _On the Foundations of Trustworthy Artificial Intelligence_ (arXiv:2603.24904).

= Introduction

Artificial intelligence systems increasingly make decisions that affect human welfare — from medical diagnoses to financial assessments to legal recommendations. Yet no existing infrastructure provides mathematical proof that a specific AI model produced a specific output on a specific input. Users must trust the compute provider, creating a fundamental accountability gap.

This gap persists because of a single technical obstacle: floating-point non-determinism. IEEE 754 arithmetic produces different results on different hardware when parallel reduction trees differ in structure. A matrix multiplication performed on ARM NEON (128-bit SIMD) produces a different result than the same operation on x86 AVX2 (256-bit SIMD) or a GPU's warp-level reduction. Over billions of multiply-accumulate operations through dozens of transformer layers, these bit-level differences compound into entirely different output token sequences.

Without determinism, verification requires re-executing the full inference — doubling the cost. With determinism, verification requires comparing a single hash — O(1). This asymmetry is the foundation of ARC.

We present a system that:

+ Eliminates all floating-point arithmetic from neural network inference, achieving bitwise identical output across all platforms
+ Constructs a Layer 1 blockchain where AI inference is a native operation verified by consensus
+ Distributes Mixture-of-Experts models across community GPU nodes via expert-parallel sharding
+ Achieves 15x lower inference cost than centralized providers through deterministic sharding, speculative decoding, and consumer hardware economics
+ Provides three tiers of verification — all-execute, optimistic, and STARK-proven — selectable per inference request

= The Determinism Thesis

We proved in our companion paper (Dunham, 2026) that platform-deterministic inference is both necessary and sufficient for trustworthy AI. The key results:

*Theorem (Determinism-Verification Collapse).* Under deterministic inference, verification of an inference claim reduces to O(1) hash comparison. Under non-deterministic inference, the verifier faces an intractable membership problem.

*Definition (Trust Entropy).* For an inference system with non-determinism parameter $delta$, the trust entropy $H_T = -log_2(1 - delta)$ quantifies the information-theoretic cost of trusting the system. Verification failure probability equals $1 - 2^(-H_T)$ exactly.

These results establish that any system claiming to verify AI computation must first solve the determinism problem. ARC solves it through pure integer arithmetic.

= Integer Inference Engine

== Architecture

The ARC inference engine replaces every floating-point operation in the transformer forward pass with integer equivalents:

- *Weights*: Stored as INT8 with per-row scale factors in Q16 fixed-point (one unit = $2^(-16)$)
- *Activations*: Maintained in Q16 fixed-point throughout the forward pass
- *Accumulation*: 64-bit integer arithmetic with deterministic left-to-right reduction order
- *Normalization*: Integer-only RMSNorm using Newton-Raphson integer square root
- *Attention*: Fixed-point Q·K^T with integer softmax via lookup table
- *Activation functions*: SiLU/GELU via 256-entry lookup tables indexed by quantized input

No floating-point operation appears anywhere in the forward pass. The output is a sequence of integer token IDs that are bitwise identical on ARM Cortex-A, x86-64 (with AVX2 or AVX-512), Apple Silicon (NEON), and GPU (via WGSL compute shaders).

== Performance

Benchmarked on Apple M2 Ultra (24 cores, 192 GB unified memory):

#table(
  columns: (auto, auto, auto, auto),
  [*Backend*], [*Speed*], [*Deterministic*], [*Verified*],
  [ARC Integer (GPU)], [76 ms/token], [Yes], [Hash + STARK],
  [ARC Integer (CPU)], [139 ms/token], [Yes], [Hash + STARK],
  [Float baseline (candle Q4)], [175 ms/token], [No], [No],
)

The deterministic engine is 2.3x faster than the floating-point baseline on GPU. Integer operations avoid denormalization handling, NaN propagation, and reduction ordering overhead.

== Cross-Platform Verification

In 82 cross-architecture tests on models up to 6.7 billion parameters, we observed zero hash mismatches between ARM and x86 execution. Results were verified across 8 nodes on 6 continents via on-chain attestation transactions.

= Distributed Inference via Expert-Parallel Sharding

== The Cost Problem

Centralized inference providers (Together.ai, Groq, Fireworks, DeepInfra) serve open-source models at \$0.36–\$0.90 per million output tokens for 70B-class models and \$2.19+ for frontier reasoning models (DeepSeek R1). These costs reflect:

+ Datacenter H100 GPU rental (\$3.90/hr per GPU)
+ 3x redundancy for fault tolerance
+ Datacenter overhead (30%+ for power, cooling, networking)
+ Corporate margin (40-60%)

== The ARC Approach

ARC eliminates redundancy through determinism and distributes computation across community-owned GPUs:

*Zero Redundancy (3x savings).* Centralized providers run three copies of every model for fault tolerance. ARC runs one copy. If the output is questioned, any node can re-execute and produce the identical hash. Verification is a hash comparison, not a re-computation. Freivalds' probabilistic verification provides 358x cheaper validation than full recomputation when needed.

*Expert-Parallel Sharding.* Modern frontier models (DeepSeek R1, Llama 4 Maverick, Qwen3.5, Mistral Large 3) use Mixture-of-Experts architectures with 8-256 experts per layer. Each expert is a small, independent neural network (22 MB in INT4 for R1). ARC distributes experts across community nodes — each node loads a subset into GPU VRAM. For each token, the router selects active experts and dispatches computation to the nodes holding them. All expert computations are embarrassingly parallel and, under deterministic inference, require zero inter-node validation.

*Consumer Hardware Economics.* An RTX 3090 provides 936 GB/s memory bandwidth at a purchase cost of \$500. An H100 provides 3,350 GB/s at \$30,000. Per dollar of memory bandwidth, the RTX 3090 is 17x more cost-efficient. A fleet of 100 community RTX 3090s provides more aggregate bandwidth than 28 H100s at 1/60th the hardware cost.

*Speculative Decoding.* A small draft model proposes candidate tokens at high speed. The full model verifies the entire batch in a single forward pass. Under deterministic inference, speculation has a 0% conflict rate (inference is a pure function). Adaptive draft length achieves 2-4x throughput improvement on 70B-class models.

*Combined Economics.* 3x (no redundancy) × 2x (speculative decoding) × consumer hardware cost efficiency = 10-15x lower cost per verified output token compared to centralized providers. At scale:

#table(
  columns: (auto, auto, auto, auto),
  [*Model*], [*Centralized Cost*], [*ARC Cost*], [*Savings*],
  [Llama 70B], [\$0.36–0.90/M], [\$0.06/M], [6-15x],
  [DeepSeek R1], [\$2.19/M], [\$0.15/M], [15x],
  [Qwen3.5 397B], [\$2.34/M], [\$0.06/M], [39x],
  [Mistral Large 3], [\$3.00/M], [\$0.10/M], [30x],
)

== Network Capacity

A 100-node community network (RTX 3090s) provides approximately 7,000 tokens per second aggregate throughput on a mixed workload, serving 603 million tokens per day. This scales linearly: 1,000 nodes serve 6 billion tokens per day, comparable to major centralized providers.

= Blockchain Architecture

== DAG Consensus

ARC uses a sender-sharded Directed Acyclic Graph (DAG) consensus protocol with two-round finality achieving approximately 200ms commit latency. Each validator maintains its own chain of blocks (vertices in the DAG), references other validators' blocks, and commits when a vertex is referenced by a supermajority. This achieves 33,230 transactions per second on a two-node real network over QUIC transport, with 183,000 single-node peak throughput.

== Transaction Types

ARC defines 24 native transaction types including three specific to AI inference:

- *InferenceAttestation (0x16)*: A validator attests to an inference result by posting the model hash, input hash, output hash, and an economic bond
- *InferenceChallenge (0x17)*: Any party can challenge an attestation; the challenger posts a bond and the network re-executes to determine correctness
- *InferenceRegister (0x18)*: Validators register their hardware capabilities and available models

AI agent transactions are first-class citizens:
- *RegisterAgent (0x07)*: Register an AI agent identity on-chain
- *Settle (0x06)*: Zero-fee settlement between agents

== Inference Verification Tiers

Three tiers provide different trust-cost tradeoffs:

*Tier 1 — All-Execute.* For models up to 20B parameters, every validator runs the inference. Consensus is reached when a supermajority produces the same output hash. Provides maximum trust at highest cost.

*Tier 2 — Optimistic.* A VRF-selected committee of 7 validators executes the inference. Results are accepted after a challenge period. Economic bonds ensure honest behavior through slashing. Suitable for 20-100B parameter models.

*Tier 3 — STARK-Proven.* A single validator executes the inference and produces a Circle STARK proof (via the Stwo prover) of the computation. The proof is verified on-chain at a fraction of the execution cost. Provides cryptographic certainty for models of any size.

VRF-based committee selection ensures unpredictable, unbiasable assignment of validators to inference tasks, preventing collusion.

== Dual Virtual Machine

ARC supports both EVM (via revm) and WebAssembly (via Wasmer) smart contracts natively. Eleven precompiles provide access to BLAKE3 hashing, Ed25519/BLS/Falcon-512 signatures, VRF evaluation, Merkle proofs, and AI inference (precompile 0x0A).

== Post-Quantum Cryptography

Five signature algorithms are supported in production: Ed25519, secp256k1 (Ethereum compatibility), BLS12-381 (aggregate signatures), Falcon-512 (NIST post-quantum standard), and ML-DSA (NIST Dilithium). Validators may use any algorithm. The BLS threshold scheme provides an encrypted mempool for MEV protection.

= Token Economics

The $ARC token has a fixed supply of 1.03 billion tokens with no inflation and no burn mechanism. Network fees are distributed:

- 40% to block proposers
- 25% to inference verifiers
- 15% to network observers
- 20% to protocol treasury

An EIP-1559-style inference gas lane with a separate base fee prevents inference requests from competing with financial transactions for block space.

= Private Network Deployment

The ARC software can be deployed as a private network on enterprise infrastructure. The same deterministic inference, verification tiers, and consensus operate behind a corporate firewall. This enables:

- Healthcare organizations to verify AI decisions on patient data without exposing data to a public network
- Financial institutions to audit AI trading decisions with mathematical certainty
- Government agencies to run classified AI workloads with verifiable output
- Any organization requiring EU AI Act compliance with continuous, automated verification

Private network licensing provides enterprise revenue for the ARC protocol independent of token economics.

= Regulatory Alignment

The EU AI Act (effective August 2, 2026) requires that high-risk AI systems be auditable, explainable, and reproducible. ARC provides this at infrastructure level:

- *Auditability*: Every inference produces an on-chain attestation with model hash, input hash, and output hash
- *Reproducibility*: Deterministic inference means any party can independently verify any result at any time
- *Explainability*: The full inference trace is available for inspection through the STARK proof system

Current compliance approaches (third-party audits at \$5,000–\$50,000 per cycle, or audit-as-a-service at \$56,000/month) are replaced by continuous, automatic verification at marginal cost approaching zero.

= Implementation

ARC is implemented in 99,000 lines of Rust across 14 crates:

#table(
  columns: (auto, auto, auto),
  [*Crate*], [*LOC*], [*Function*],
  [arc-types], [14,490], [24 transaction types, blocks, accounts, governance],
  [arc-state], [13,203], [DashMap state, Jellyfish Merkle Tree, BlockSTM],
  [arc-crypto], [11,680], [Ed25519, BLS, BLAKE3, Falcon-512, STARK prover],
  [arc-vm], [8,439], [WASM + EVM, 11 precompiles, inference oracle],
  [arc-node], [8,424], [Block production, 34 RPC endpoints, consensus],
  [arc-consensus], [7,971], [DAG consensus, VRF, slashing, epochs],
  [arc-gpu], [5,250], [Metal/WGSL GPU verification],
  [arc-inference], [620+], [INT4 runtime, VRF committees, gas lane],
  [+ 6 more], [...], [Networking, mempool, channels, CLI, benchmarks],
)

The system passes 1,209 tests and operates on a live testnet across 8 nodes on 6 continents.

= Conclusion

ARC demonstrates that the determinism problem — the single obstacle preventing trustless AI verification — is solvable through pure integer arithmetic, and that the solution enables a fundamentally new economic model for AI inference: distributed, verified, and 15x cheaper than centralized alternatives.

The combination of platform determinism, expert-parallel sharding, and consumer hardware economics creates a network where any GPU owner can contribute to AI inference and earn proportional rewards, while any AI consumer receives mathematically verified outputs at a fraction of current market costs.

As AI systems assume greater responsibility in human decision-making, the ability to verify their computations transitions from a technical novelty to a societal necessity. ARC provides that infrastructure.

The code is open source. The testnet is live. The paper is published.

#bibliography("references.bib")
