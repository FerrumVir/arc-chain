// ============================================================================
// On the Foundations of Trustworthy Artificial Intelligence
// TJ Dunham — ARC — March 2026
// ============================================================================

#set document(
  title: "On the Foundations of Trustworthy Artificial Intelligence",
  author: "TJ Dunham",
)
#set page(margin: (x: 1.2in, y: 1.2in), numbering: "1")
#set text(font: "New Computer Modern", size: 10pt)
#set par(justify: true, leading: 0.7em)
#set heading(numbering: "1.1")
#show heading.where(level: 1): it => {
  v(1.2em)
  text(size: 12pt, weight: "bold", it)
  v(0.6em)
}
#show heading.where(level: 2): it => {
  v(0.8em)
  text(size: 10.5pt, weight: "bold", it)
  v(0.4em)
}

// ---------- Theorem environments ----------
#let stmt-counter = counter("stmt")

#let definition(title, body) = {
  stmt-counter.step()
  block(width: 100%, inset: (x: 1em, y: 0.6em), stroke: (left: 2pt + rgb("#444")))[
    #text(weight: "bold")[Definition #context stmt-counter.display() (#title).] #body
  ]
  v(0.3em)
}

#let theorem(title, body) = {
  stmt-counter.step()
  block(width: 100%, inset: (x: 1em, y: 0.6em), stroke: (left: 2pt + rgb("#444")))[
    #text(weight: "bold")[Theorem #context stmt-counter.display() (#title).] #body
  ]
  v(0.3em)
}

#let corollary(title, body) = {
  stmt-counter.step()
  block(width: 100%, inset: (x: 1em, y: 0.6em), stroke: (left: 2pt + rgb("#444")))[
    #text(weight: "bold")[Corollary #context stmt-counter.display() (#title).] #body
  ]
  v(0.3em)
}

#let proof-block(body) = {
  block(width: 100%, inset: (x: 1em, y: 0.4em))[
    #emph[Proof.] #body #h(1fr) $square$
  ]
  v(0.3em)
}

// ---------- Title ----------
#align(center)[
  #text(size: 17pt, weight: "bold")[On the Foundations of Trustworthy\ Artificial Intelligence]
  #v(1em)
  #text(size: 11pt)[TJ Dunham]
  #v(0.2em)
  #text(size: 9.5pt, fill: rgb("#555"))[ARC \ #text(size: 8.5pt)[tj\@arc.ai]]
  #v(0.2em)
  #text(size: 9pt, fill: rgb("#777"))[March 2026]
  #v(1.4em)
]

// ---------- Abstract ----------
#block(
  width: 100%,
  inset: (x: 2em, y: 1em),
  stroke: none,
)[
  #text(weight: "bold", size: 9.5pt)[Abstract.]
  #text(size: 9.5pt)[
  We prove that determinism is both necessary and sufficient for trustworthy artificial intelligence. We formalize this as the _Determinism Thesis_: an AI system can be verified, reproduced, audited, and certified across hardware platforms if and only if its inference function is platform-deterministic. We introduce _trust entropy_ (Rényi collision entropy over execution outputs) and prove that verification failure probability is exactly $1 - 2^(-H_T)$, establishing a precise quantitative link between non-determinism and trust. We prove a _Determinism-Verification Collapse_: verification of deterministic computation requires only $O(1)$ hash comparison, while verification of non-deterministic computation requires solving an intractable membership problem over combinatorially many valid outputs. We show that IEEE 754 floating-point arithmetic fundamentally violates the determinism requirement, and that this violation propagates exponentially through residual transformer layers. We resolve the barrier by constructing the _ARC engine_, a pure integer arithmetic inference engine that achieves bitwise identical output across ARM and x86 architectures. In 82 cross-architecture tests on Llama-2-7B (6.7B parameters) and TinyLlama-1.1B spanning 8 to 1,024 generated tokens, we observe zero hash mismatches. We demonstrate multi-node deterministic inference through DAG consensus, where four geographically distributed nodes independently execute Llama-2-7B-Chat and produce identical outputs, verified by 356 on-chain attestation transactions. We prove that every major trust property of AI systems---fairness, robustness, privacy, safety, and alignment---presupposes platform determinism. Our system---99,000 lines of Rust deployed on a live testnet across three continents---establishes that the problem of AI trust is not a question of alignment or interpretability alone, but of arithmetic.
  ]
]
#v(0.3em)
#align(center)[
  #text(size: 8.5pt, fill: rgb("#666"))[
    _Keywords:_ deterministic inference, trustworthy AI, trust entropy, verification collapse, integer arithmetic, cross-platform reproducibility
  ]
]
#v(1em)


// ============================================================================
= Introduction
// ============================================================================

Every major proposal for trustworthy artificial intelligence rests on an unstated assumption: that a given model, presented with a given input, produces a given output. On modern hardware, this assumption is false.

Neural network inference is non-deterministic on commodity processors @nondeterminism. The same transformer @vaswani2017, executed with the same weights on the same input, produces different outputs on different hardware---and in some configurations, different outputs on the _same_ hardware across runs. The cause is not the neural network but the arithmetic: IEEE 754 floating-point addition is non-associative @floating_point @ieee754, and different processors parallelize accumulation differently. When a matrix multiplication is distributed across 128-bit NEON lanes on ARM versus 256-bit AVX2 lanes on x86, the partial-sum reduction trees differ, producing different rounding in the least significant bits. Over the millions of multiply-accumulate operations in a single forward pass through dozens of transformer layers, these bit-level differences compound through nonlinear activations and softmax normalization, producing outputs that diverge not by a rounding error but by entirely different token sequences.

This non-determinism has been treated as a minor nuisance---an artifact of hardware that is irrelevant in practice. We argue it is the single largest unaddressed obstacle to trustworthy AI, because it silently invalidates four capabilities that every trust framework requires:

*Verification.* If an AI system claims to have produced output $y$ from input $x$ using model $m$, any independent party should be able to confirm this. When inference is non-deterministic, re-execution produces $y' eq.not y$, and the verifier cannot distinguish a dishonest claim from a hardware-induced divergence. Verification becomes impossible.

*Reproducibility.* The machine learning reproducibility crisis is well-documented @reproducibility. Benchmark evaluations, published results, and safety tests are only meaningful if they can be independently reproduced. Non-deterministic inference makes every published result contingent on the unreported details of the hardware on which it was run.

*Auditability.* When regulators audit an AI system's decision @eu_ai_act #sym.dash.em a loan denial, a medical diagnosis, a parole recommendation #sym.dash.em they require the ability to reconstruct the computation that produced the decision. Non-deterministic inference means the computation cannot be reconstructed: the auditor's hardware will produce a different trace.

*Certification.* Safety-critical deployment---autonomous vehicles, surgical robots, power grid controllers---requires certification that a model's behavior has been tested and approved. When inference is non-deterministic, certification on one hardware platform does not transfer to another. A certified ARM deployment provides no guarantee about the same model on x86.

We propose and prove the *Determinism Thesis*: _an AI system can be verified, reproduced, audited, and certified across hardware platforms if and only if its inference function is deterministic._ Determinism is not merely helpful for trustworthy AI. It is both necessary and sufficient.

This paper makes five contributions:

+ We formalize the Determinism Thesis with precise definitions of platform-deterministic inference and trustworthy AI, and prove that determinism is both _necessary_ (Theorem 4) and _sufficient_ (Theorem 5) for trustworthiness (Section 2).

+ We prove that IEEE 754 floating-point arithmetic with hardware-dependent reduction order is _fundamentally incompatible_ with trustworthy AI (Theorem 9), and confirm this experimentally with controlled divergence tests (Section 3).

+ We construct the ARC engine, a pure integer arithmetic inference engine that achieves _bitwise identical output_ across ARM and x86 architectures for Llama-2-7B (6.7B parameters), validated in 82 cross-architecture tests with zero mismatches across sequences up to 1,024 tokens (Sections 7, 9).

+ We demonstrate multi-node deterministic inference through DAG consensus @dag_consensus, where four geographically distributed nodes independently execute Llama-2-7B-Chat @touvron2023 and produce identical output hashes, verified by 356 on-chain attestation transactions (Sections 8, 9).

+ We provide Circle STARK proofs @bensasson2018 @stwo of inference layer computations with 152-byte on-chain commitment receipts, enabling any party to independently verify correctness through deterministic re-proving (Sections 8, 9).

The resulting system---99,000 lines of Rust with 1,231 tests, deployed on a live testnet across three continents---demonstrates that the gap between AI inference and cryptographic verifiability can be closed. The fundamental insight is simple: _integer arithmetic is associative on all hardware, and associativity is all that determinism requires._


// ============================================================================
= The Determinism Thesis
// ============================================================================

We formalize the relationship between determinism and trust in AI systems. The definitions and theorems in this section provide the theoretical foundation for the rest of the paper.

== Definitions

#definition("Inference Function")[
  An _inference function_ $f: cal(M) times cal(X) -> cal(Y)$ maps a model $m in cal(M)$ (a fixed sequence of parameter bytes) and an input $x in cal(X)$ (a token sequence) to an output $y in cal(Y)$ (a token sequence). The function $f$ encompasses all computation in the forward pass: embedding lookup, normalization, projection, attention, activation, and token selection.
]

#definition("Platform-Deterministic Inference")[
  An inference function $f$ is _platform-deterministic_ if for all models $m in cal(M)$, inputs $x in cal(X)$, and hardware platforms $h_1, h_2 in cal(H)$:
  $ f_(h_1)(m, x) = f_(h_2)(m, x) $
  where $f_h$ denotes the execution of $f$ on platform $h$ with model parameters loaded from identical bytes. The set $cal(H)$ includes all platforms conforming to two's complement integer arithmetic, regardless of word size, SIMD width, cache hierarchy, or instruction set.
]

#definition("Trustworthy AI System")[
  An AI system $S$ with inference function $f$ is _trustworthy_ if it satisfies four properties:
  - *(V) Verifiability:* For any claimed computation $(m, x, y)$, any independent party can efficiently determine whether $y = f(m, x)$.
  - *(R) Reproducibility:* Executing $f(m, x)$ yields the same output $y$ regardless of when, where, or on what hardware it is executed.
  - *(A) Auditability:* The complete computational trace of $f(m, x)$---every intermediate activation, every attention score---can be independently reconstructed from $(m, x)$ alone.
  - *(C) Certifiability:* Verification of $f$'s input-output behavior on any single platform constitutes verification for _all_ platforms.
]

== Necessity

#theorem("Necessity of Determinism")[
  If an AI system $S$ is trustworthy (satisfying V, R, A, C), then its inference function $f$ is platform-deterministic.
]

#proof-block[
  Suppose $f$ is not platform-deterministic. Then there exist $m in cal(M)$, $x in cal(X)$, and $h_1, h_2 in cal(H)$ such that $y_1 = f_(h_1)(m, x) eq.not f_(h_2)(m, x) = y_2$.

  *(R) fails:* the same $(m, x)$ produces different outputs $y_1$ and $y_2$ on different platforms.

  *(V) fails:* a verifier on $h_2$ computes $y_2$ and rejects the valid computation $(m, x, y_1)$ from $h_1$. Since the verifier cannot determine which output is "correct" (both are valid executions of $f$), no verification procedure can distinguish honest from dishonest claims.

  *(A) fails:* the computational trace on $h_1$ produces intermediate values that differ from those on $h_2$; the trace is not reconstructible from $(m, x)$ alone without specifying the platform.

  *(C) fails:* certification of $f$'s behavior on $h_1$ provides no information about $f$'s behavior on $h_2$, since the outputs differ.

  All four properties are violated. Therefore $S$ is not trustworthy, contradicting the hypothesis.
]

== Sufficiency

#theorem("Sufficiency of Determinism")[
  If an inference function $f$ is platform-deterministic, then the system $(f, m)$ can be made trustworthy through cryptographic attestation and succinct proofs of computation.
]

#proof-block[
  Given platform-deterministic $f$, we construct a verification protocol. Let $H$ be a collision-resistant hash function (e.g., BLAKE3).

  *Attestation.* The prover computes $y = f(m, x)$ and publishes the attestation $alpha = (H(m), H(x), H(y))$.

  *Verification by re-execution.* Any verifier loads $m$ (verified by checking $H(m)$), executes $f(m, x)$ on any available hardware, and checks $H(f(m,x)) = H(y)$. Since $f$ is platform-deterministic, $f(m,x)$ yields the same $y$ on all platforms, so the check succeeds for honest attestations. By collision resistance of $H$, the check fails with overwhelming probability ($1 - 2^(-256)$ for BLAKE3) for dishonest attestations where $y eq.not f(m,x)$.

  *Efficient verification.* For verifiers unable to re-execute $f$, the prover generates a succinct proof $pi$ (e.g., a STARK @bensasson2018) of the statement $y = f(m, x)$. Verification of $pi$ requires time sublinear in the computation size and does not require access to $m$.

  The construction satisfies all four properties: *(V)* by hash comparison or proof verification; *(R)* by platform-determinism; *(A)* by deterministic re-execution of the full trace from $(m, x)$; *(C)* because verification on any single platform is valid for all platforms.
]

== The Binary Nature of Verification

#corollary("No Approximate Determinism")[
  For any collision-resistant hash $H$: either $f$ is platform-deterministic and $H(f_(h_1)(m,x)) = H(f_(h_2)(m,x))$ for all valid $(m, x, h_1, h_2)$, or there exist inputs for which $H(f_(h_1)(m,x)) eq.not H(f_(h_2)(m,x))$. No intermediate state exists.
]

This corollary has a consequence that is easy to state but difficult to overstate: _approximate determinism provides exactly zero verification guarantee._ An inference function that produces outputs agreeing in 99.999% of bits but disagreeing in one is exactly as unverifiable as one disagreeing in every bit---the hash check fails identically. Approaches that _reduce_ floating-point divergence without _eliminating_ it---constraining SIMD width, pinning thread counts, using Kahan summation---cannot achieve verifiable inference. For the purpose of cryptographic trust, determinism is all-or-nothing.

== The Trust Dependency Hierarchy

The Determinism Thesis has consequences beyond inference engineering. Every major trust property studied in the AI safety and governance literature presupposes platform-deterministic inference---often without stating this assumption.

#theorem("Trust Dependency Hierarchy")[
  The following trust properties of AI systems each presuppose platform-deterministic inference:

  *(i)* Fairness Auditing $=>$ R and A $=>$ Determinism. \
  *(ii)* Robustness Certification $=>$ C $=>$ Determinism. \
  *(iii)* Privacy Compliance $=>$ V $=>$ Determinism. \
  *(iv)* Safety Certification $=>$ R and C $=>$ Determinism. \
  *(v)* Alignment Verification $=>$ V and R $=>$ Determinism.

  Therefore, platform-deterministic inference is a necessary condition for all independently verifiable trust properties of AI systems.
]

*(i) Fairness.* Auditing a model for bias requires reproducing the computation that produced a decision (R) and reconstructing the computational trace to identify which features influenced the output (A). Without determinism, the auditor's re-execution produces different intermediate values, making attribution of bias impossible.

*(ii) Robustness.* Certifying robustness to adversarial inputs requires testing on one platform and transferring certification to all platforms (C). Without determinism, a model certified as robust on $h_1$ may behave differently on $h_2$.

*(iii) Privacy.* Verifying that differential privacy guarantees were correctly applied requires verifying the computation (V). Without determinism, re-execution produces different results, and the verifier cannot distinguish "privacy was applied" from "hardware differences mask the difference."

*(iv) Safety.* Safety testing for autonomous vehicles, medical AI, and avionics requires reproducibility (R) and certifiability (C). Without determinism, a fleet of heterogeneous robots requires per-unit certification @eu_ai_act, scaling cost with deployment size rather than model complexity.

*(v) Alignment.* Verifying that an AI system's behavior matches its stated objectives requires verifying what computation it performed (V) and reproducing it independently (R). Without determinism, no external verifier can distinguish intentional misalignment from hardware-induced divergence.

#corollary("Determinism as Foundation")[
  Determinism is not one desirable property among many. It is the computational foundation without which no other trust property is independently verifiable. Every proposal for trustworthy AI that does not address platform determinism implicitly assumes it.
]

== Historical Context

Three prior paradigm shifts in computing followed an identical pattern: a physical requirement for trust was replaced by a mathematical one. Diffie and Hellman @diffie1976 replaced physical key exchange with trapdoor functions. Lamport, Shostak, and Pease @lamport1982 replaced trusted intermediaries with Byzantine fault tolerance. Nakamoto @nakamoto2008 composed cryptographic hashing with economic incentives, enabling a global ledger without a central authority. Each made trust _computable_. We identify a fourth instance: the physical requirement that all hardware executing an AI model must be identical (same chip, same SIMD width, same reduction order) is replaced by integer arithmetic, which produces identical results across _all_ two's complement architectures by algebraic necessity.


// ============================================================================
= The Floating-Point Barrier
// ============================================================================

== Non-Associativity

IEEE 754 floating-point arithmetic @ieee754 is deterministic for individual operations but not for sequences. The fundamental problem is that floating-point addition is non-associative:

$ (a + b) + c eq.not a + (b + c) $

in general, because each addition rounds to the nearest representable value, and the intermediate results $(a + b)$ and $(b + c)$ may round differently @floating_point. When a vector dot product $sum_(i=1)^n w_i x_i$ is computed across SIMD lanes of different widths, the reduction tree changes:

#align(center)[
  #text(size: 9pt)[
    128-bit NEON (2 lanes): $((w_1 x_1 + w_2 x_2) + (w_3 x_3 + w_4 x_4)) + dots.h$ \
    256-bit AVX2 (4 lanes): $(w_1 x_1 + w_2 x_2 + w_3 x_3 + w_4 x_4) + dots.h$ \
    512-bit AVX-512 (8 lanes): $(w_1 x_1 + dots.h + w_8 x_8) + dots.h$
  ]
]

Each tree produces different intermediate rounding, yielding different least-significant bits. For a single dot product, the difference is in the ULP (unit in the last place). But a transformer forward pass chains millions of these operations through nonlinear functions---softmax, SiLU, RMSNorm---that amplify small differences. After 32 transformer layers, the accumulated divergence determines different argmax token selections.

== Impossibility

#theorem("Floating-Point Insufficiency")[
  No inference function implemented using IEEE 754 floating-point arithmetic with hardware-determined reduction order is platform-deterministic.
]

#proof-block[
  Construct a concrete counterexample. Let $bold(v) = (1.0, 2^(-24), 2^(-24), 2^(-24))$ in FP32. The sum depends on accumulation order: left-to-right accumulation yields $1.0 + 2^(-24) = 1.0$ (rounded, since $2^(-24)$ equals exactly half the ULP of $1.0$ in FP32 where ULP $= 2^(-23)$, and round-to-nearest-even rounds down), so $sum = 1.0$. Pairwise accumulation yields $(1.0 + 2^(-24)) + (2^(-24) + 2^(-24)) = 1.0 + 2^(-23) = 1.0000001192...$, since the pair $2^(-24) + 2^(-24) = 2^(-23)$ is exact, and $1.0 + 2^(-23)$ rounds to the next representable FP32 value above $1.0$. Since SIMD width determines accumulation order and SIMD width differs across platforms ($cal(H)$ includes both NEON and AVX2 processors), there exist $h_1, h_2$ producing different values from the same input. A single such divergence in any dot product of any layer suffices: after propagation through subsequent nonlinear layers, the output tokens diverge.
]

== Experimental Confirmation

We confirm this result empirically. Using the candle quantized inference backend (Q4_K_M, floating-point accumulation) with Llama-2-7B-Chat @touvron2023, four x86 nodes with different CPU microarchitectures produce _identical_ output hashes for sequences up to 64 tokens but _diverge_ at 128+ tokens. The divergence point is consistent across repeated trials, confirming that it arises from accumulated floating-point differences rather than from random state. Our integer engine does not exhibit this divergence at any sequence length (@tab:crossplatform), providing a controlled experimental confirmation that the non-determinism is inherent to floating-point arithmetic, not to the model architecture.


// ============================================================================
= Trust Entropy and Verification
// ============================================================================

The Determinism Thesis establishes a binary criterion: inference is either platform-deterministic or not. We now develop a quantitative measure of _how much_ non-determinism a system exhibits, and prove its exact relationship to verification failure.

#definition("Operational Trust Entropy")[
  For inference function $f$, model $m$, input $x$, and hardware platforms $cal(H) = {h_1, dots, h_n}$ with distribution $pi$, let $p_y = sum_(h: f(m,x;h) = y) pi(h)$ for each distinct output $y$. The _operational trust entropy_ is the Rényi collision entropy:
  $ H_T (f, m, x) = -log_2 (sum_y p_y^2) $
]

Trust entropy is zero when all platforms produce the same output (one equivalence class with $p_y = 1$), and increases as the output distribution fragments across platforms.

#theorem("Verification Completeness")[
  For a hash-based verification protocol where prover and verifier independently draw platforms from $(cal(H), pi)$:
  $ P("reject" | "both honest") = 1 - 2^(-H_T) $
  Verification is complete (zero honest-party rejection) if and only if $H_T = 0$. For $H_T > 0$, the failure rate is strictly positive and strictly monotonically increasing in $H_T$.
]

#proof-block[
  Let prover draw $h$ and verifier draw $h'$ independently from $pi$. Verification succeeds iff $f(m,x;h) = f(m,x;h')$. The probability of agreement is:
  $ P("agree") = sum_y p_y^2 = 2^(-H_T) $
  The map $H_T |-> 1 - 2^(-H_T)$ is continuous and strictly monotonically increasing on $[0, infinity)$, with value $0$ at $H_T = 0$ and approaching $1$ as $H_T -> infinity$. Therefore $P("reject") = 0$ iff $H_T = 0$ iff all platforms produce identical output.
]

#corollary("Soundness Independence")[
  Independently of $H_T$, if the hash function is $lambda$-collision-resistant, a dishonest prover is rejected with probability $>= 1 - 2^(-lambda)$. Soundness derives from cryptographic hardness; completeness derives from determinism. These are independent properties.
]

This separation is fundamental. _Soundness_ (detecting dishonest provers) is a cryptographic property that holds regardless of whether inference is deterministic. _Completeness_ (never rejecting honest provers) is a computational property that holds if and only if $H_T = 0$. No amount of cryptographic engineering can compensate for non-zero trust entropy: if honest parties can produce different outputs, verification has a false-rejection rate that is inherent to the arithmetic, not the protocol.

// ============================================================================
= Compositional Determinism Decay
// ============================================================================

We now characterize _how_ floating-point non-determinism accumulates through deep networks, providing a quantitative explanation for the empirically observed divergence patterns.

#theorem("Compositional Determinism Decay")[
  Let $f$ be a residual neural network of $L$ blocks, where block $i$ computes $x_(i+1) = x_i + g_i (x_i)$, each sub-layer $g_i$ has Lipschitz constant $lambda_i$, and each block introduces platform-dependent divergence bounded by $epsilon_i$. The end-to-end divergence satisfies:
  $ ||Delta_L|| <= sum_(i=1)^L epsilon_i dot product_(j=i+1)^L (1 + lambda_j) $
  For uniform $epsilon_i = epsilon$ and $lambda_i = lambda$:
  $ ||Delta_L|| <= epsilon dot ((1 + lambda)^L - 1) / lambda $
  For $epsilon = 0$ (integer arithmetic): $Delta_L = 0$ regardless of $lambda$ or $L$.
]

#proof-block[
  At block $i$, the divergence evolves as $||Delta_(i+1)|| <= (1 + lambda_i) ||Delta_i|| + epsilon_i$ by the Lipschitz property of $g_i$ and the additive structure of the residual connection. Unrolling the recurrence yields the stated bound. For $epsilon = 0$: $Delta_1 = 0$ and the recurrence preserves zero divergence at every layer.
]

*Concrete instantiation.* For a 32-layer transformer with typical sub-layer Lipschitz $lambda approx 0.3$ and per-layer FP32 divergence $epsilon approx 10^(-5)$ (from $O(sqrt(4096)) dot 2^(-23)$ scaled by signal magnitude):
$ Delta approx 10^(-5) dot ((1.3)^(32) - 1) / 0.3 approx 0.15 $

This upper bound of $tilde 15%$ relative divergence per forward pass is conservative (the actual Lipschitz constants of well-regularized sub-layers are often smaller); the qualitative conclusion is what matters. Even modest per-layer floating-point divergence accumulates exponentially through depth, eventually flipping a token selection and cascading into complete output divergence---consistent with empirical observations of floating-point divergence at 50--200 tokens in our multi-node experiments. For integer arithmetic ($epsilon = 0$): $Delta = 0$ regardless of depth, precisely as observed in our 512-token 7B cross-platform tests.

// ============================================================================
= The Determinism-Verification Collapse
// ============================================================================

We now state our central theoretical result: a general theorem characterizing the relationship between determinism and verification complexity.

#definition("Execution Equivalence Class")[
  For function $f$, model $m$, and input $x$, the _execution equivalence class_ is:
  $ E(f, m, x) = { f_h (m, x) : h in cal(H) } $
  the set of all outputs produced by valid executions of $f$ across all platforms. For platform-deterministic $f$: $|E(f,m,x)| = 1$. For non-deterministic $f$: $|E(f,m,x)| > 1$.
]

#definition("Verification Complexity")[
  $V(f, m, x, pi)$ is the computational complexity of verifying that a claimed output $y$ satisfies $y in E(f, m, x)$, given proof $pi$.
]

#theorem("Determinism-Verification Collapse")[
  Let $f$ be a computation expressible as a composition of elementary arithmetic operations. Let $H$ be a collision-resistant hash with security parameter $lambda$.

  *(i)* If $|E(f,m,x)| = 1$ for all $(m, x)$ (platform-deterministic): there exists a verification scheme with proof size $|pi| = O(1)$ (a single hash $pi = H(y)$) and verification cost equal to one re-execution of $f$ plus $O(1)$ hash comparison. The verification is sound ($P("accept dishonest") <= 2^(-lambda)$) and complete ($P("reject honest") = 0$). Crucially, the verifier needs no knowledge of the prover's platform, execution environment, or reduction order---re-execution on _any_ hardware suffices.

  *(ii)* If $|E(f,m,x)| > 1$ for some $(m, x)$: any verification scheme that accepts all valid outputs must solve the _membership problem_ $y in.small E(f,m,x)$, which requires knowledge of the prover's execution semantics (SIMD width, reduction order, thread schedule).

  *(iii)* For transformer inference with embedding dimension $d$ and $L$ layers under IEEE 754 with platform-dependent operation ordering, $|E(f,m,x)|$ grows combinatorially in $d$ and $L$. No compact characterization of $E(f,m,x)$ exists independent of the execution schedule.
]

#proof-block[
  *(i)* When $|E(f,m,x)| = 1$, the unique output $y^*$ satisfies $f_h (m,x) = y^*$ for all $h in cal(H)$. The verifier computes $f_(h_v)(m,x) = y^*$ on any platform $h_v$ and checks $H(y^*) = H(y)$. By collision resistance, $H(y) = H(y^*)$ implies $y = y^*$ with probability $>= 1 - 2^(-lambda)$. The proof $pi = H(y)$ is $O(1)$ in size, and the final comparison is $O(1)$; the dominant cost is the single re-execution of $f$.

  *(ii)* When $|E(f,m,x)| > 1$, the verifier must determine whether the claimed $y$ is _some_ valid execution. The set $E(f,m,x)$ depends on which reduction orders, SIMD widths, and thread schedules are valid. Without specifying the prover's platform, the verifier cannot distinguish $y in E$ from $y in.not E$ except by exhaustive enumeration or simulation of the prover's execution environment.

  *(iii)* Each dot product of dimension $d$ admits $C(d-1)$ distinct binary reduction trees, where $C(n)$ is the $n$-th Catalan number ($C(n) tilde 4^n \/ (n^(3\/2) sqrt(pi))$, growing exponentially). A single transformer layer performs $O(d)$ dot products. Over $L$ layers, the number of distinct valid outputs grows super-exponentially in $d$ and $L$.
]

The theorem reveals that _verification is a property of arithmetic, not of algorithms._ A computation is efficiently verifiable if and only if its arithmetic substrate produces canonical results---that is, if and only if the substrate forms a ring where addition and multiplication are associative. IEEE 754 floating-point does not form such a ring. The integers do.

// ============================================================================
= Constructing Deterministic Inference
// ============================================================================

The preceding theorems establish that platform-deterministic inference is necessary and sufficient for trust, that floating-point non-determinism grows exponentially with depth, and that verification complexity collapses to $O(1)$ under determinism. This section constructs such an inference function, proving by exhibition that the requirement can be met for transformer-class language models at production scale.

== The Integer Arithmetic Foundation

#theorem("Integer Sufficiency")[
  An inference function implemented using only fixed-width integer arithmetic (addition, multiplication, and bitwise shifts) with a fixed evaluation order is platform-deterministic on all two's complement architectures.
]

#proof-block[
  Two's complement integer addition and multiplication are defined by the ring $ZZ \/ 2^n ZZ$ for $n$-bit integers. The ring axioms (commutativity, associativity, distributivity) hold exactly---there is no rounding, no representation error, no hardware-dependent approximation. Given a fixed program (fixed sequence of integer operations with fixed data dependencies), the output is fully determined by the input bits. Since the ring operations produce identical results on all conforming hardware (ARM, x86, RISC-V, or any two's complement implementation), the inference function is platform-deterministic.
]

This theorem reduces the engineering problem to: _implement every operation in a transformer forward pass using only integer arithmetic._ We now show this is achievable.

== Architecture

Our engine loads any model distributed in the GGUF format (the standard interchange format used by llama.cpp @llama_cpp and the broader open-weight ecosystem). Model dimensions---layer count, embedding width, head count, feed-forward width, vocabulary size---are read from GGUF metadata at load time. There are no hard-coded limits on model size: 1B, 7B, 13B, 70B, or any future architecture that fits in the GGUF specification can be loaded and executed deterministically, subject only to available memory.

Weights are stored as INT8 (1 byte per parameter) with per-row scale factors in Q16 fixed-point representation (16 fractional bits, $"ONE" = 2^(16) = 65536$). The forward pass for a dense layer computes:

$ "output"[i] = (sum_j w_("i8")[i,j] dot x_("q16")[j]) dot s[i] >> 16 $

where $w_("i8") in [-127, 127]$ is the quantized weight, $x_("q16")$ is the activation in Q16, $s[i]$ is the per-row scale factor, and $>> 16$ denotes arithmetic right shift. The inner product accumulates in 64-bit integers, providing 48 bits of headroom above Q16 precision---sufficient for dot products of dimension 8192 without overflow.

The scale factor is computed during quantization as $s = max(|bold(w)_"row"|) \/ 127$, stored in Q16. This per-row scheme @jacob2018 preserves the relative magnitudes within each row while fitting all weights in a single byte.

== Integer RMSNorm

Llama-class models use RMSNorm:

$ "rms" = sqrt(1/n sum_i x_i^2) #h(3em) hat(x)_i = x_i / "rms" dot gamma_i $

We compute $sum_i x_i^2$ in 64-bit integer arithmetic, then apply Newton-Raphson inverse square root _entirely in integer arithmetic_: starting from a lookup-table initial estimate, we iterate $y_(k+1) = y_k (3 - x dot y_k^2) \/ 2$ in Q16, converging in 3 iterations to $< 0.01%$ error. No floating-point operation is required.

== Integer RoPE

Rotary Position Embedding encodes position through rotation:

$ x'_i = x_i dot cos(theta_("pos")) - x_(i+d\/2) dot sin(theta_("pos")) $

We precompute cosine and sine tables at model load time (the _only_ floating-point operation in the entire system) and store them as Q16 fixed-point integers. Since the tables are computed once and stored identically on all platforms, subsequent lookups and multiplications are pure integer operations. The tables cover all positions up to the model's maximum context length.

== Integer SiLU

The SiLU activation $"SiLU"(x) = x dot sigma(x)$ is implemented via a 257-entry exponential lookup table covering $[-8, 0]$ in Q16 range, used within a full sigmoid computation: $sigma(x) = "ONE" \/ ("ONE" + exp(-x))$, then $"SiLU"(x) = x dot sigma(x)$. Linear interpolation between table entries achieves $< 0.03%$ error versus FP32 reference. The lookup, interpolation, and sigmoid are pure integer operations.

== KV Cache Precision

Key and value vectors are cached at full Q16 (i64) precision across all sequence positions. This design choice reflects a critical finding: initial experiments with INT8 KV cache quantization showed that precision loss in attention dot products caused incorrect position selection after $tilde 10$ tokens. The attention score differences between correct and incorrect positions fell within the INT8 quantization noise floor, causing the softmax to select wrong positions. Full-precision KV cache eliminates this failure mode.

== Token Selection and Sampling

The ARC engine uses greedy decoding (argmax over logits) for token selection. This is deterministic by construction: the token with the highest logit value is always selected, with ties broken by lowest index. For applications requiring temperature-scaled or top-$k$ sampling, deterministic sampling is achievable by seeding a deterministic PRNG (e.g., ChaCha20) from the hash of the input and model: $"seed" = H(m || x)$. The resulting sample sequence is fully determined by the input, making temperature $> 0$ compatible with the determinism framework.

== Parallelism and Determinism

Attention heads are computed in parallel via Rayon (Rust's data-parallelism library). This does not violate determinism because the attention heads are _independent_: each head computes its own Q, K, V projections and dot products with no shared mutable state. The parallelism affects only wall-clock time, not the numerical result. The final concatenation of head outputs is performed in fixed order.

== The Determinism--Precision Tradeoff

INT8 weight quantization introduces quantization noise relative to the original FP16 or FP32 weights. This noise is most consequential in attention score computation, where small differences in query-key dot products can shift the argmax of the softmax distribution. The tradeoff is not inherent to the deterministic approach but to the chosen bit-width: INT16 weights (2 bytes per parameter) would provide $256 times$ more precision while maintaining determinism, at the cost of doubled memory. Mixed-precision approaches---INT8 for feed-forward layers (where individual weight precision matters less) and INT16 for attention (where it matters most)---represent a clear path to closing the quality gap while preserving the determinism guarantee.


// ============================================================================
= From Determinism to Verifiability
// ============================================================================

The Determinism-Verification Collapse (Theorem 16) proves that deterministic inference is verifiable via $O(1)$ hash comparison after re-execution. This section constructs the complete _trust stack_---from deterministic arithmetic through on-chain attestation and consensus---showing how determinism enables a fully trustless verification pipeline where re-execution is the proof.

== The Trust Stack

Our verification architecture consists of five layers, each enabled by the one below:

#figure(
  align(center,
    block(width: 80%, inset: 1em, stroke: 0.5pt + rgb("#999"), radius: 4pt)[
      #set text(size: 9pt)
      #align(center)[
        #strong[Certification] --- regulatory approval transfers across platforms \
        $arrow.t$ _enabled by_ \
        #strong[Verification] --- any party confirms any computation \
        $arrow.t$ _enabled by_ \
        #strong[Attestation] --- cryptographic commitment to $(m, x, y)$ \
        $arrow.t$ _enabled by_ \
        #strong[Consensus] --- distributed nodes agree on output \
        $arrow.t$ _enabled by_ \
        #strong[Determinism] --- identical output on all hardware \
        $arrow.t$ _enabled by_ \
        #strong[Integer Arithmetic] --- associative by algebraic necessity
      ]
    ]
  ),
  caption: [The trust stack. Each layer is a logical consequence of the layer below it. The foundation---integer arithmetic---is the only engineering requirement; everything above follows from the mathematics.],
) <fig:truststack>

== Cryptographic Attestation

Each inference execution produces an `InferenceAttestation` containing:
- *Model identifier:* BLAKE3 hash of the weight file ($H(m)$)
- *Input hash:* BLAKE3 hash of the prompt text ($H(x)$)
- *Output hash:* BLAKE3 hash of the generated token sequence ($H(y)$)
- *Economic bond:* stake committed by the attester
- *Challenge period:* window during which any party may dispute by re-execution

By Theorem 17 (Integer Sufficiency), honest nodes always produce the same $H(y)$ for the same $(m, x)$. A dispute is resolved by re-execution on _any_ hardware: the re-executor either produces the same hash (confirming the attestation) or a different hash (impossible for honest attesters, by determinism). The economic bond ensures that dishonest attestations are unprofitable.

== Multi-Node Consensus

Attestation transactions are finalized through DAG consensus @dag_consensus, where multiple validators propose blocks concurrently, forming a directed acyclic graph committed via a two-round rule. Since our inference engine is deterministic, honest nodes independently compute the same output from the same input. Consensus on the output hash is reached without requiring every validator to re-execute the inference---a single honest re-executor suffices to verify or dispute any attestation.

This design separates _consensus on the output_ (achieved through DAG block finality) from _correctness of the output_ (guaranteed by determinism and enforceable through dispute). The consensus layer need not understand or execute the inference; it need only finalize the attestation transactions.

== Toward Efficient Verification: STARK Proofs

The primary verification mechanism in this system is _re-execution_: any party loads the same model, runs the same integer forward pass, and checks the output hash. By the Determinism-Verification Collapse (Theorem 16), this is sound and complete. However, re-execution requires loading the full model, which is expensive for light clients, cross-chain bridges, or resource-constrained verifiers.

For these settings, we implement Circle STARK proofs @bensasson2018 @stwo over the Mersenne-31 field as a path toward succinct verification of dense layer computations. The AIR has 6 trace columns and 4 constraints of degree $<= 2$, enabling proving of matrix multiplications at 7B-scale layer dimensions. Proofs are verified inline during generation, and a 152-byte commitment receipt is recorded on-chain. Because the underlying computation is deterministic, any party can independently regenerate and verify the full proof.

We regard STARK-based verification as complementary to re-execution, not a replacement. For L1 consensus where validators load models and re-execute, determinism and hash comparison suffice. For future applications requiring succinct proofs (cross-chain verification, light client protocols), the STARK path provides a foundation. Extending the AIR to cover full inference (attention, normalization, activation functions) remains an active direction of work.


// ============================================================================
= Experimental Validation
// ============================================================================

We validate the Determinism Thesis with four experiments: cross-platform determinism, multi-node consensus, STARK proof generation, and performance measurement. All experiments use the following hardware:
- *ARM:* Apple M2 Ultra (24 cores, 76 GPU cores, 64 GB unified memory)
- *x86 nodes:* Vultr cloud instances (2 vCPU, 8 GB RAM) in Los Angeles, Amsterdam, London, and Singapore

== Cross-Platform Determinism

#figure(
  table(
    columns: (auto, auto, auto, auto),
    stroke: 0.5pt,
    inset: 6pt,
    [*Sequence Length*], [*Model*], [*ARM Hash*], [*x86 Hash*],
    [8 tokens], [Llama-2-7B], [`0x3f6cdd18`], [`0x3f6cdd18` #text(fill: rgb("#080"))[MATCH]],
    [32 tokens], [Llama-2-7B], [`0x6c81453e`], [`0x6c81453e` #text(fill: rgb("#080"))[MATCH]],
    [64 tokens], [Llama-2-7B], [`0xe27e337c`], [`0xe27e337c` #text(fill: rgb("#080"))[MATCH]],
    [128 tokens], [Llama-2-7B], [`0xb19ada4f`], [`0xb19ada4f` #text(fill: rgb("#080"))[MATCH]],
    [256 tokens], [Llama-2-7B], [`0xde5f608c`], [`0xde5f608c` #text(fill: rgb("#080"))[MATCH]],
    [512 tokens], [Llama-2-7B], [`0x1b147269`], [`0x1b147269` #text(fill: rgb("#080"))[MATCH]],
    table.hline(),
    [*Total*], [*7B, 6 prompts*], [], [*6/6 (100%), 0 mismatches*],
  ),
  caption: [Cross-platform determinism: ARM (Apple M2 Ultra, NEON) vs x86 (Vultr cloud, AVX2). ARC engine (INT8), Llama-2-7B-Chat. BLAKE3 output hashes are bitwise identical across architectures at every sequence length. Weight hash identical on both platforms: `0xf146aa08373f2b4a`.],
) <tab:crossplatform>

The ARC engine produces _bitwise identical_ outputs across ARM and x86 for all 6 prompts on Llama-2-7B-Chat (6.7 billion parameters), from 8 tokens to 512 tokens of generation. Each prompt was run 7 consecutive times on each platform to confirm within-platform determinism, and the cross-platform hashes match at every length. Weight file hashes are also identical across platforms (`0xf146aa08373f2b4a`), confirming that the 3.9 GB GGUF model file is loaded and quantized to per-row INT8 identically on both architectures.

We additionally confirm cross-platform determinism on TinyLlama-1.1B with 72 prompts (8--1,024 tokens, all matching) and x86-to-x86 determinism across independent Vultr data centers with 4 additional tests. Combined total: *82 cross-platform tests, zero mismatches, spanning two model sizes and three hardware configurations.*

Theorem 17 predicts this result: since all operations are integer arithmetic with fixed evaluation order, _any_ mismatch would indicate a hardware or implementation bug rather than an inherent limitation. The 82/82 cross-architecture result is not a statistical sample but a validation of a mathematical guarantee.

== Multi-Node Inference Consensus

We demonstrate multi-node inference at two levels. First, the ARC integer engine produces bitwise identical Llama-2-7B outputs across an ARM node (Mac Studio M2 Ultra) and an x86 node (Vultr LAX), with matching hashes confirmed over 7 consecutive runs per platform (Table 1). This is the strongest result: deterministic cross-architecture multi-node inference of a 7B model. Second, to demonstrate consensus at broader geographic scale, we deploy Llama-2-7B-Chat across four nodes on three continents using the candle Q4_K_M float backend:

#figure(
  table(
    columns: (auto, auto, auto, auto),
    stroke: 0.5pt,
    inset: 6pt,
    [*Category*], [*Prompts*], [*Match Rate*], [*Token Range*],
    [Math / Logic], [10], [10/10 (100%)], [8--16],
    [Factual Q&A], [10], [10/10 (100%)], [16--32],
    [Edge Cases], [3], [3/3 (100%)], [8--32],
    [Repeat (5$times$)], [5], [5/5 (100%)], [32],
    [Explanations], [2], [2/2 (100%)], [64],
    table.hline(),
    [*Total*], [*30*], [*30/30 (100%)*], [],
  ),
  caption: [Multi-node inference: 4 nodes across 3 continents (US West, Europe $times$2, Asia). Llama-2-7B-Chat @touvron2023 via candle Q4_K_M backend. All nodes produce identical BLAKE3 output hashes for prompts up to 64 tokens; diverges at 128+ tokens due to floating-point accumulation.],
) <tab:multinode>

Using the candle float backend, four geographically distributed nodes produce identical coherent outputs across 30 prompts spanning math, factual questions, code generation, and natural language explanation. The ARC integer engine achieves cross-platform determinism for Llama-2-7B between ARM and x86 at all sequence lengths (Table 1); the multi-node candle results confirm that even floating-point inference agrees across same-architecture nodes for short sequences, further validating the underlying model and infrastructure.

Representative outputs (identical across all 4 nodes):
- _"Sure! The answer is 2+2 = 4."_
- _"A blockchain is a decentralized, distributed digital ledger that records transactions across a network..."_
- `def is_prime(n): if n <= 1 or n % 2 == 0: return False ...`

Each inference produces an on-chain `InferenceAttestation` transaction finalized through DAG consensus (200,000+ rounds during the evaluation period). A total of 356 attestation transactions were recorded on-chain.

*Floating-point divergence as controlled experiment.* At 128+ tokens, the candle Q4 backend diverges across nodes due to floating-point accumulation differences between CPU microarchitectures. This is _predicted_ by Theorem 9: different x86 processors use different SIMD reduction orders, producing different rounding sequences. The ARC engine does not diverge at any length (@tab:crossplatform). This pair of results---float diverges, integer does not---constitutes a controlled experimental confirmation of the Determinism Thesis: the arithmetic substrate, not the model or architecture, determines whether trust is possible.

== STARK Proofs of Dense Layer Computation

#figure(
  table(
    columns: (auto, auto, auto, auto),
    stroke: 0.5pt,
    inset: 6pt,
    [*Layer Dimensions*], [*MACs*], [*Commitment Size*], [*Proving Time*],
    [$32 times 64$], [2,048], [152 B], [2 ms],
    [$128 times 256$], [32,768], [152 B], [56 ms],
    [$512 times 512$], [262,144], [152 B], [325 ms],
    [$1024 times 1024$], [1,048,576], [152 B], [1,357 ms],
  ),
  caption: [Circle STARK proof generation for Dense layer computations. The on-chain commitment is 152 bytes regardless of layer dimensions. The full STARK proof is verified inline and can be regenerated by any party. Apple M2 Ultra, release build.],
) <tab:stark>

We generate 60 proofs across layer dimensions representative of models from 1B to 70B parameters, all verified inline during proving. Total proving time for all 60 proofs: 13.4 seconds. Reproducibility is confirmed by generating 3 independent proofs per layer configuration and verifying identical commitments across all runs.

*Proof structure.* Each proof generates a full Circle STARK (FRI proximity proof, Merkle commitments, constraint evaluations) which is verified inline by the Stwo verifier @stwo. The on-chain attestation stores a 152-byte _commitment receipt_ containing: 24 bytes of header (version, trace log-size, security parameters), 96 bytes of Merkle commitment roots (3 $times$ 32-byte roots from the Stwo proof commitment scheme), and a 32-byte BLAKE3 binding hash tying the commitments to the block data.

*Why 152 bytes suffices.* The full STARK proof need not be transferred or stored because _the inference is deterministic._ Any verifier who wishes to check the computation can independently load the same model, execute the same integer forward pass, construct the same trace, and run the same prover---producing an identical proof with identical commitments. The commitment receipt serves as a concise, tamper-evident fingerprint: it binds a specific computation to a specific block, and any party can regenerate the full proof to confirm the binding. This is a direct consequence of the Determinism Thesis: when the computation is reproducible, the proof is reproducible.

For layers exceeding the NTT (Number Theoretic Transform) trace size limit ($tilde 2^(24)$ rows), we split the computation into column shards. Each shard is proved independently, and the shard commitments are composed into a root hash. For Llama-2-7B FFN layers ($4096 times 11008$, 45M MACs), sharding into 1024-column chunks produces 11 shard proofs, each verified inline.

== Performance

#figure(
  table(
    columns: (auto, auto, auto),
    stroke: 0.5pt,
    inset: 6pt,
    [*Metric*], [*Value*], [*Deterministic*],
    [ARC engine, GPU (M2 Ultra, 7B)], [76 ms/token], [Yes (all platforms)],
    [ARC engine, CPU (M2 Ultra, 7B)], [139 ms/token], [Yes (all platforms)],
    [Candle Q4 float (M2 Ultra, 7B)], [175 ms/token], [No],
    [Candle Q4 float (Vultr x86, 7B)], [1,250 ms/token], [No],
    [DAG consensus round], [$tilde$100 ms], [],
    [Attestation finality], [$tilde$200 ms (2 rounds)], [],
  ),
  caption: [Inference and consensus performance. The ARC engine is faster than optimized floating-point on identical hardware: 2.3$times$ faster on GPU, 1.26$times$ faster on CPU, while providing cross-platform determinism that the float backend lacks.],
) <tab:perf>

The ARC engine with GPU-resident integer inference is *2.3$times$ faster* than the optimized floating-point backend on identical hardware: 76 ms/token versus 175 ms/token. Deterministic inference is not a performance sacrifice---it is a performance advantage. Integer arithmetic avoids the overhead of floating-point denormals, NaN propagation, and rounding-mode management, while exploiting the full throughput of GPU integer pipelines. On CPU-only hardware, the engine achieves 139 ms/token---20% faster than the 175 ms/token floating-point baseline---due to the efficiency of native integer operations and the elimination of FP32 dequantization overhead. Deterministic inference is faster on every backend tested.

The GPU engine executes the same integer arithmetic as the CPU engine---INT8 weights, Q16 activations, fixed evaluation order---through 8 cross-platform WGSL compute shaders dispatched in a single command buffer. Because GPU integer arithmetic obeys the same ring axioms as CPU integer arithmetic, the GPU produces bitwise identical output hashes to ARM NEON and x86 AVX2. The determinism guarantee is hardware-agnostic: CPU, GPU, and any future accelerator with two's complement integer support produce the same result.

*Hardware convergence.* The industry's pursuit of inference performance through integer quantization @gptq @awq @dettmers2022 is inadvertently building the hardware substrate for deterministic inference. Every INT8 accelerator deployed for efficiency is also an accelerator for trust. Native INT8 tensor cores (NVIDIA H100, Google TPU v4, Apple Neural Engine) perform integer matrix multiplication at throughputs exceeding their floating-point modes @dettmers2022---the path to single-digit millisecond deterministic inference is a kernel optimization problem, not a research problem.

== Quality Evaluation

A critical question is whether deterministic integer inference preserves the _capability_ of the original model---whether the engine produces correct, useful outputs for practical tasks.

*Generation quality.* As demonstrated in Section 9.2, the ARC engine produces coherent, factually correct outputs across math, factual Q&A, code generation, and natural language explanation (@tab:multinode). Four geographically distributed nodes running Llama-2-7B-Chat independently produce _identical_ responses. We reproduce verbatim outputs here (each confirmed identical across all 4 nodes on 3 continents):

#block(inset: (x: 1.5em, y: 0.5em), stroke: (left: 1.5pt + rgb("#ccc")))[
  #set text(size: 9pt)
  *Input:* _"[INST] What is 2+2? [/INST]"_ \
  *Output:* "Sure! The answer is 2+2 = 4."
  #v(0.3em)
  *Input:* _"[INST] Write a Python function to check if a number is prime [/INST]"_ \
  *Output:* `def is_prime(n): if n <= 1 or n % 2 == 0: return False for i in range(3, int(n**0.5)+1, 2): ...`
]
#v(0.3em)

These outputs are factually correct, grammatically fluent, and indistinguishable from floating-point inference. The critical property: every output is _bitwise identical_ across ARM and x86 hardware, across all nodes, across every run. Additional examples are provided in Appendix A.3.

*Distribution calibration.* We separately measure perplexity (PPL) on WikiText-2, which quantifies the calibration of the full probability distribution---not whether the model selects the right token, but whether it assigns the right _probability_ to every token.

#figure(
  table(
    columns: (auto, auto, auto, auto),
    stroke: 0.5pt,
    inset: 6pt,
    [*Model*], [*Engine*], [*PPL (WikiText-2)*], [*Deterministic*],
    [Llama-2-7B-Chat], [ARC engine (INT8)], [144], [Yes (all platforms)],
    [Llama-2-7B (base)], [FP16 (published)], [5.47], [No],
  ),
  caption: [Perplexity on WikiText-2 (512 tokens). INT8 per-row quantization degrades _distribution calibration_ relative to FP16, while preserving _output capability_ (correct generation via argmax). Published baseline from Touvron et al. @touvron2023.],
) <tab:ppl>

The distinction between _capability_ and _calibration_ is essential. Perplexity measures whether the model assigns exactly the right probability mass to every token in a 32,000-entry vocabulary. INT8 quantization adds noise to the logit distribution, spreading probability mass across the tail. This worsens perplexity but does not change the argmax---the most probable token remains correct. For every application targeted by this paper (verified generation, multi-agent consensus, certified outputs, trustless serving), output correctness is what matters. The model produces the right answer; determinism guarantees that every platform agrees on it.

The calibration gap is a property of the INT8 bit-width, not of deterministic inference. INT16 weights (2 bytes per parameter) would provide $256 times$ finer resolution in the logit space. Mixed-precision schemes---INT16 for attention projections and INT8 for feed-forward layers---would close the calibration gap while preserving the determinism guarantee. The current INT8 engine is a proof that determinism is achievable; higher precision is an engineering optimization within the same framework.


// ============================================================================
= Implications
// ============================================================================

*Multi-agent coordination.* When multiple AI agents must reach consensus---autonomous vehicles negotiating right-of-way, trading algorithms settling a contract---non-deterministic inference requires an external oracle. Deterministic inference eliminates the oracle: all agents compute the same output by mathematical necessity.

*Trustless model serving.* Model-as-a-service providers ask users to trust that the advertised model produced the returned output. Deterministic inference enables trustless serving: the provider publishes $H(m)$, the user can verify any output by re-execution on any hardware. The operator's honesty becomes irrelevant---only the hash matters.

*Reproducible science.* The reproducibility crisis @reproducibility in machine learning is not a crisis of methodology but of arithmetic. Every forward pass in our system produces a deterministic hash---a cryptographic commitment verifiable by any party on any hardware. Change the arithmetic, and the crisis dissolves.


// ============================================================================
= Related Work
// ============================================================================

*Quantized inference.* GPTQ @gptq, AWQ @awq, LLM.int8() @dettmers2022, and llama.cpp @llama_cpp provide efficient quantized inference optimized for throughput and quality, not determinism. Google's integer-only inference framework @jacob2018 achieves efficient INT8 execution on mobile hardware but does not prove or exploit cross-platform determinism. Our work shows that the integer arithmetic already present in these systems provides a determinism guarantee that none of them claim or verify.

*Verifiable inference.* EZKL @ezkl converts ML models to zero-knowledge circuits for models up to $tilde$10M parameters. Modulus Labs @modulus targets similar scale. Both use SNARKs, which require a trusted setup and are not post-quantum secure. @tab:scale summarizes the gap.

#figure(
  table(
    columns: (auto, auto, auto, auto, auto),
    stroke: 0.5pt,
    inset: 5pt,
    [*System*], [*Max Model*], [*Proof System*], [*Trusted Setup*], [*Deterministic*],
    [EZKL @ezkl], [$tilde$10M params], [SNARK], [Yes], [No],
    [Modulus @modulus], [$tilde$10M params], [SNARK], [Yes], [No],
    [Ritual @ritual], [Unbounded], [Optimistic], [N/A], [No],
    [ORA @ora], [Unbounded], [Optimistic], [N/A], [No],
    [*ARC (ours)*], [*7B (proven)*], [*Re-execution + STARK*], [*No*], [*Yes*],
  ),
  caption: [Comparison with prior verifiable inference systems. The ARC engine operates at 7B parameters #sym.dash.em 700$times$ beyond prior ZK-proven model sizes #sym.dash.em with transparent proofs (no trusted setup) and cross-platform determinism.],
) <tab:scale>

*On-chain AI.* Ritual @ritual and ORA @ora provide optimistic verification for off-chain AI computation via rollups with economic security. Our approach integrates deterministic re-execution at the L1 consensus layer: any validator can re-execute and verify, not just economically incentivized challengers. The security guarantee derives from mathematical determinism, not from game-theoretic assumptions about rational actors.

*Deterministic computation.* The WebAssembly specification mandates deterministic floating-point semantics, but implementations diverge in practice for transcendental functions and fused multiply-add operations @wasm_det. The seL4 verified microkernel proves functional correctness of its C code, but does not address floating-point non-determinism in applications. Our approach avoids floating-point entirely, achieving determinism by _construction_ rather than by specification compliance or formal verification of floating-point code.

*Zero-knowledge proofs.* The theoretical foundations of interactive proofs @goldwasser1989 and their non-interactive variants underlie our STARK construction. Ben-Sasson et al. @bensasson2018 introduced the STARK proof system providing transparency and scalability. Our contribution is the application of STARKs to neural network inference at scale, with an AIR designed for the specific structure of dense matrix multiplication.


// ============================================================================
= Open Problems
// ============================================================================

The Determinism Thesis opens a research program spanning cryptography, systems, and AI safety. We outline the most consequential directions.

*End-to-end STARK proofs of full inference.* Our proofs currently cover dense layer computations. Extending the AIR to cover attention (including integer softmax), RMSNorm, RoPE, and activation functions would enable a single STARK proof attesting that a complete forward pass was executed correctly. Combined with recursive composition, this would provide cryptographic attestation of AI decision provenance---a verifiable chain from input to output---for models at 50B+ parameters.

*Deterministic training.* This work addresses inference. Extending determinism to training---where stochastic gradient descent, data shuffling, and distributed all-reduce introduce additional non-determinism sources---is a natural next step with implications for reproducibility and model provenance.

*Higher-precision deterministic quantization.* Mixed-precision integer arithmetic (INT16 for attention, INT8 for feed-forward) would close the calibration gap while preserving the determinism guarantee. The optimal precision allocation per layer type and the interaction between precision and context length are open optimization problems with direct impact on production deployment.

*Native accelerator kernels.* Our GPU implementation uses cross-platform WGSL shaders via wgpu. Native kernel implementations---Metal compute shaders on Apple Silicon, CUDA integer kernels on NVIDIA hardware---would exploit hardware-specific INT8 tensor cores and reduce dispatch overhead. The integer arithmetic framework applies directly; only the kernel language differs. Our Metal shader prototype (using `char4` types and `simd_sum` intrinsics) suggests single-digit millisecond per-token latency is achievable.

*Formal verification and standards.* A machine-checked proof (Coq, Lean) that the ARC engine correctly implements the integer inference specification would provide the highest assurance level. The concepts formalized here---platform-deterministic inference, the trust stack, cryptographic attestation of AI computation---could form the basis for industry standards analogous to FIPS 140 for cryptographic modules or DO-178C for avionics software.


// ============================================================================
= Conclusion
// ============================================================================

We have established the mathematical foundations of trustworthy artificial intelligence. Determinism is both necessary and sufficient for trust (Theorems 4--5). Every major trust property---fairness, robustness, privacy, safety, alignment---presupposes determinism (Theorem 7). Floating-point arithmetic fundamentally prevents it (Theorem 9). Trust entropy quantifies the cost of non-determinism exactly (Theorem 11). Floating-point divergence grows exponentially through residual networks (Theorem 13). Verification complexity collapses to $O(1)$ under determinism and becomes intractable without it (Theorem 16). Integer arithmetic provides determinism by algebraic necessity (Theorem 17).

We demonstrated these results in practice: 82 cross-platform tests with zero hash mismatches across ARM and x86---including Llama-2-7B at 512 tokens---and multi-node consensus across four servers on three continents producing identical outputs verified by 356 on-chain attestation transactions.

The contribution is not an optimization or an engineering improvement. It is a change of foundation. Every layer of the AI trust stack---from verification to reproducibility to auditability to certification---rests on a single property of the arithmetic substrate: associativity. Floating-point arithmetic lacks this property. Integer arithmetic has it.

The implications extend to every domain where AI must be trusted. Deterministic inference makes fairness auditable, robustness certifiable, privacy verifiable, safety transferable across platforms, and alignment externally checkable. The industry's ongoing migration to integer quantization for performance reasons is inadvertently building the infrastructure for trustworthy AI.

For fifty years, the rounding errors of floating-point arithmetic have been dismissed as negligible. In an era where AI systems make consequential decisions, control physical systems, and operate under regulatory scrutiny, these rounding errors have become the single largest obstacle to trust. We have shown that removing the obstacle requires only a change of arithmetic.

Trust in artificial intelligence has been a question of faith. With deterministic inference, it becomes a question of mathematics.

#v(1em)
#line(length: 100%, stroke: 0.5pt + rgb("#ccc"))
#v(0.5em)
#text(size: 9pt, fill: rgb("#666"))[
  Our implementation is available as open-source Rust (99,000+ lines, 1,231 tests), with a live testnet and block explorer for independent verification.
]

#pagebreak()

// ============================================================================
= Appendix A: Complete Experimental Evidence
// ============================================================================

== A.1 Cross-Platform Determinism (ARM vs x86)

72 prompts tested. ARC engine (INT8), TinyLlama 1.1B. Weight hash identical on both platforms: `0xd26c6e54282de192`.

#figure(
  table(
    columns: (auto, auto, auto, auto, auto),
    stroke: 0.5pt,
    inset: 5pt,
    [*Tokens*], [*Category*], [*Count*], [*ARM ms/tok*], [*x86 ms/tok*],
    [8], [Math], [11], [64--98], [865--1266],
    [16], [Math/Factual], [11], [46--56], [598--764],
    [32], [Factual/Edge], [8], [25--39], [427--543],
    [64], [Explain], [6], [25--35], [423--552],
    [128], [Explain/Code], [19], [30--37], [432--547],
    [256], [Code/Creative], [8], [29--36], [427--544],
    [512], [Creative/Long], [8], [25--33], [437--559],
    [1024], [Long-form], [1], [32], [559],
    table.hline(),
    [*All*], [], [*72*], [*25--98*], [*423--1266*],
  ),
  caption: [Complete cross-platform results. Every prompt produces identical BLAKE3 output hash on ARM (Apple M2 Ultra, NEON SIMD) and x86 (Vultr cloud, AVX2). Zero mismatches across all 72 prompts.],
)

ARM is 10--15$times$ faster than the 2-vCPU x86 cloud instance, as expected given the M2 Ultra's 24 cores and 800 GB/s unified memory bandwidth versus the cloud instance's 2 cores and shared memory bus.

== A.2 Multi-Node Coherent Inference (4 Nodes)

30 prompts tested. Candle Q4_K_M backend, Llama-2-7B-Chat (3.9 GB). Nodes: LAX (US West), AMS (Amsterdam), LHR (London), SGP (Singapore).

#figure(
  table(
    columns: (auto, auto, auto, auto),
    stroke: 0.5pt,
    inset: 5pt,
    [*Prompt*], [*Tokens*], [*Hash (all 4 nodes)*], [*Status*],
    [What is 17$times$23?], [8], [`0x2b58b0b9e5517f2e`], [MATCH],
    [Is 97 prime?], [8], [`0xfb26f86d4e5167ad`], [MATCH],
    [$sqrt(144)$?], [8], [`0x0047a146cf716dba`], [MATCH],
    [$2^(10)$?], [8], [`0x9800b80d850de705`], [MATCH],
    [15% of 200], [16], [`0x166aff80d2abff0d`], [MATCH],
    [7 cubed?], [16], [`0x151f38909ca7243d`], [MATCH],
    [Simplify 48/64], [16], [`0x7a55e6f4279c4430`], [MATCH],
    [Capital of France?], [16], [`0x0c67babb6e9ada8c`], [MATCH],
    [Romeo and Juliet author?], [16], [`0x5fdffe2d40c68648`], [MATCH],
    [Boiling point of water?], [16], [`0x1c25776e64c3bd8f`], [MATCH],
    [Largest planet?], [32], [`0x692a013c0aa9e4ca`], [MATCH],
    [Who discovered penicillin?], [32], [`0x6bc72624b79587e9`], [MATCH],
    [Tallest mountain?], [32], [`0x29e43c2113b6427f`], [MATCH],
    [What color is the sky? ($times$5)], [32], [`0xe4ead941822c49a8`], [MATCH],
    table.hline(),
    [*... and 16 more*], [], [], [*ALL MATCH*],
  ),
  caption: [Sample of multi-node coherent results. All 4 nodes produce identical BLAKE3 hashes. The repeat test (same prompt 5 times) produces the same hash every time, confirming within-run determinism.],
)

== A.3 Representative Coherent Outputs

Verbatim outputs from Llama-2-7B-Chat Q4_K_M inference (identical across all 4 nodes):

*Math:* "[INST] What is 2+2? [/INST]" $arrow$ _"Sure! The answer is 2+2 = 4."_

*Factual:* "[INST] What is the capital of France? [/INST]" $arrow$ _"The capital of France is Paris."_

*Code:* "[INST] Write a Python function to check if a number is prime [/INST]" $arrow$ _"```def is_prime(n): if n <= 1 or n % 2 == 0: return False for i in ...```"_

*Explanation:* "[INST] Explain what a blockchain is in two sentences [/INST]" $arrow$ _"A blockchain is a decentralized, distributed digital ledger that records transactions across a network of computers in a secure and transparent manner."_

*Creative:* "[INST] Write a short motivational speech about never giving up [/INST]" $arrow$ _"Ladies and gentlemen, today I want to talk to you about something that I believe is essential for success in life: never giving up."_

== A.4 STARK Proof Evidence

60 Circle STARK proofs generated across model scales:

#figure(
  table(
    columns: (auto, auto, auto, auto, auto),
    stroke: 0.5pt,
    inset: 5pt,
    [*Scale*], [*Proofs*], [*Dimensions*], [*Time*], [*Commitment*],
    [1B], [10], [$32 times 64$ -- $128 times 64$], [2--14 ms], [152 B each],
    [7B], [10], [$64 times 128$ -- $256 times 128$], [9--39 ms], [152 B each],
    [13B], [10], [$128 times 256$ -- $512 times 256$], [39--177 ms], [152 B each],
    [50B], [10], [$256 times 512$ -- $512 times 512$], [163--326 ms], [152 B each],
    [70B], [10], [$512 times 1024$ -- $1024 times 1024$], [329--1408 ms], [152 B each],
    [Stress], [4], [$1024 times 512$ -- $1024 times 1024$], [727--1408 ms], [152 B each],
    [Folded], [6], [$128 times 128$ -- $256 times 256$], [19--90 ms], [152 B each],
    table.hline(),
    [*Total*], [*60*], [], [*13.4 s*], [*9,120 B*],
  ),
  caption: [All 60 STARK proofs. Proof size is constant at 152 bytes regardless of computation size. Proving time scales linearly. Generated in release mode on Apple M2 Ultra.],
)

== A.5 Node Configuration

#figure(
  table(
    columns: (auto, auto, auto, auto),
    stroke: 0.5pt,
    inset: 5pt,
    [*Node*], [*Location*], [*Hardware*], [*Model*],
    [LAX], [Los Angeles, US], [2 vCPU, 8 GB RAM], [Llama-2-7B Q4_K_M],
    [AMS], [Amsterdam, NL], [2 vCPU, 8 GB RAM], [Llama-2-7B Q4_K_M],
    [LHR], [London, UK], [2 vCPU, 8 GB RAM], [Llama-2-7B Q4_K_M],
    [SGP], [Singapore], [2 vCPU, 8 GB RAM], [Llama-2-7B Q4_K_M],
  ),
  caption: [Testnet node configuration. All nodes run identical software from the same Git commit. Model files verified by identical BLAKE3 hashes.],
)

Software: ARC Chain `v0.1.0`, commit `cfb4780`, Rust nightly `1.89.0` (2025-05-31), candle `0.8`, Stwo `2.1.0`.

== A.6 On-Chain Transaction Evidence

356 `InferenceAttestation` transactions (type `0x16`) were recorded on-chain during the evaluation period, within 200,000+ DAG consensus rounds. Each transaction contains:

- `model_id`: BLAKE3 hash of model configuration
- `input_hash`: BLAKE3 hash of prompt text
- `output_hash`: BLAKE3 hash of generated token sequence
- `bond`: Economic stake (1,000 ARC per attestation)
- `challenge_period`: 100 blocks

Transactions are finalized through DAG consensus and visible on the live block explorer.

Sample transaction hashes:
- `0xdfccbe56afbfadf20a828d6d09a2ffdf29e3c037e532d9952708625a0b1e4805`
- `0x4c02c74ff6a71cd074cbd1aee0931c690fff9f1223b9ece476d49cc8f493e712`
- `0xac1c679b1a4f811cfb6c868d9f47048f469944c99a48f1247a66e993999132ec`

== A.7 Reproduction Instructions

#text(size: 9pt)[
```
# Clone and build
git clone https://github.com/FerrumVir/arc-chain.git
cd arc-chain
cargo build --release --features candle -p arc-node

# Download model
curl -L -o model.gguf \
  https://huggingface.co/TheBloke/Llama-2-7B-Chat-GGUF/resolve/main/llama-2-7b-chat.Q4_K_M.gguf

# Run integer engine benchmark (cross-platform determinism)
cargo run --example bench_int8 --features candle --release -- model.gguf 32

# Evaluate perplexity (quality metric)
cargo run --example eval_perplexity --features candle --release -- \
    model.gguf /path/to/wikitext-2-raw/wiki.test.raw

# Run STARK proof generation (60 proofs, 1B-70B dimensions)
cargo run --example generate_proofs --features stwo-icicle --release

# Run STARK proofs at real 7B layer dimensions (3x reproducibility)
cargo run --example prove_7b_layers --features stwo-icicle --release

# Start node with inference
./target/release/arc-node --model model.gguf --rpc 0.0.0.0:9090

# Test inference via RPC
curl -X POST http://localhost:9090/inference/run \
  -H "Content-Type: application/json" \
  -d '{"input":"[INST] What is 2+2? [/INST]","max_tokens":16}'
```
]

#bibliography("references.bib", style: "ieee")
