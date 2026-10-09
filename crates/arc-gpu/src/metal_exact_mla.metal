// Exact MLA attention for the MLA + MoE profile's INT16 layers (decode): the
// work between a layer's two per-head projections, so that every head of a
// layer runs in one command buffer (metal_exact_mla.rs):
//
//   exact_gemv_i16_heads  every head's wk_b dots      (metal_exact_gemv_i16.metal)
//   mla_epilogue          qa = floor(dot * mu / 2^k)  arith::dyadic_epilogue
//   mla_scores            s_i = floor(dot_i * lambda / 2^46)   ops::mla_attend
//   mla_softmax           w_i = exp_q16(s_i - max), total = sum_i w_i
//   mla_weighted          u = trunc(sum_i w_i * latent_i / total), and u's
//                         digit planes for the next projection
//   exact_gemv_i16_heads  every head's wv_b dots
//   mla_epilogue          out = floor(dot * mu / 2^k)
//
// Every kernel reproduces the CPU function named beside it (in
// crates/arc-inference/src/modern: arith.rs and mla/ops.rs) value for value.
// The CPU computes in i64 and i128. Metal has no 128-bit integer type, and
// this file uses no native integer division (#150 found a miscompiled one on
// an M2 Ultra), so:
//
//  * 128-bit values are (low, high) ulong pairs in two's complement. Products
//    are formed from 32-bit halves (mul_wide) and sums carry explicitly;
//  * a floor shift of a signed value is a shift of its magnitude, rounded up
//    when the value is negative and bits are lost (or a logical shift with an
//    explicit sign fill, sar);
//  * the one division, sum / total, is a shift-subtract loop on the
//    magnitude (udiv128), truncated toward zero as Rust's `/` is;
//  * there is no floating point, no division and no modulo in this file.
//
// Where the CPU would return an error, a kernel sets a bit in the status
// word and writes 0. The host then discards every head's result, and the
// CPU loop computes the heads, so a layer refuses exactly when the CPU
// refuses, with the CPU's own error.
//
// Bounds. Before every dispatch the host (metal_exact_mla.rs) has checked:
// 0 < lambda < 2^31; rank <= 2^16 and rope_dim <= 2^12; every row scale has
// mu = 0 or 2^30 <= mu < 2^31, and 16 <= k <= 62; 1 <= positions < 2^31;
// and the cached latents and RoPE keys are i32, so |c| <= 2^31. Every query
// value is an activation, |q| <= 2^62: qa leaves mla_epilogue only within
// 2^62, and qp is the CPU's own RoPE output.
//
//  1. Epilogue. |dot| <= 2^63 and mu < 2^31, so |dot| * mu < 2^94 is exact in
//     mul_wide's 128 bits, and k <= 62 keeps every shift below 64.
//  2. Mass. The CPU attends in i64 when sum |q| < 2^32 over qa and qp
//     ("narrow"), else in i128. A lane counts the values with |q| >= 2^32 and
//     adds the others: at most 2^17 values below 2^32 in all, so every sum is
//     below 2^49 and exact.
//  3. Narrow dot. Every |q| <= sum |q| < 2^32, so every product is below 2^63
//     and every partial sum, in any order, is at most sum |q| * 2^31 < 2^63:
//     the ulong sum modulo 2^64, read as a long, is the dot.
//  4. Wide dot. |q * c| <= 2^62 * 2^31 = 2^93, with at most 2^16 + 2^12
//     terms: every partial sum is below 2^110 in magnitude, so the sum modulo
//     2^128 is the dot.
//  5. Score. |dot| * lambda < 2^110 * 2^31 is formed exactly in 192 bits. The
//     CPU refuses when the i128 product overflows or when |floor(product /
//     2^46)| > 2^62; both happen exactly when the magnitude is too large for
//     the floor to stay within 2^62, which is the only case refused here.
//  6. Weights. Scores lie in [-2^62, 2^62], so s - max lies in [-2^63, 0]
//     and never wraps; exp_q16 gives a value in [0, 2^16]; total <= positions
//     * 2^16 < 2^47, and total >= 2^16 because the maximum contributes
//     exp_q16(0) = 2^16.
//  7. Weighted sum. |w * c| <= 2^16 * 2^31 = 2^47. A long adds at most
//     MLA_FLUSH = 2^15 of them (|sum| <= 2^62) before it is added to a
//     128-bit accumulator, which then holds at most positions * 2^47 < 2^78.
//  8. Output. |acc| <= sum_i w_i * |c_i| <= total * 2^31, so |u| <= 2^31.
//     That is within the CPU's 2^62 output check, within five balanced
//     base-256 digits, and makes sum |u| <= 2^16 * 2^31 = 2^47, below the
//     INT16 accumulator guard floor(2^63 / 32767) that the next projection's
//     kernel and the CPU projection require.
//
// Integer addition is associative, so tile shapes, lane counts and reduction
// order cannot change a value: every reduction below is exact.

#include <metal_stdlib>
using namespace metal;

// 2^62, the CPU's activation limit (arith::ACTIVATION_LIMIT).
constant ulong ACTIVATION_LIMIT = 4611686018427387904UL;
// 2^32, the CPU's narrow-attention limit on sum |q|.
constant ulong NARROW_LIMIT = 4294967296UL;
// 2^31: every attention output is within it (bound 8).
constant ulong OUTPUT_LIMIT = 2147483648UL;
// 2^46 - 1: the bits a score's floor shift drops.
constant ulong SCORE_FRACTION = 70368744177663UL;
constant uint SCORE_SHIFT = 46u;
constant long ONE_Q16 = 65536;
// exp_q16's table has EXP_STEPS + 1 entries (tables::EXP_TABLE).
constant uint EXP_STEPS = 4096u;
// Positions a long adds before flushing into the 128-bit sum (bound 7).
constant uint MLA_FLUSH = 32768u;
// Digit planes per head in the next projection's digit buffer, as in
// metal_exact_gemv_i16.metal's I16_HEAD_PLANES.
constant uint HEAD_PLANES = 7u;
// The most threads mla_softmax runs per threadgroup (an array bound, so a
// macro).
#define SOFTMAX_THREADS 256u
constant long LOWEST = -9223372036854775807L - 1L;

constant uint ST_KEY = 1u;
constant uint ST_SCORE = 2u;
constant uint ST_OUTPUT = 4u;
constant uint ST_VALUE = 8u;

// ---- 64- and 128-bit helpers ---------------------------------------------

static inline long wadd(long a, long b) {
    return as_type<long>(as_type<ulong>(a) + as_type<ulong>(b));
}

static inline long wsub(long a, long b) {
    return as_type<long>(as_type<ulong>(a) - as_type<ulong>(b));
}

// Arithmetic shift right by s in [0, 63]: floor(v * 2^-s).
static inline long sar(long v, uint s) {
    ulong u = as_type<ulong>(v) >> s;
    if (v < 0) {
        u |= ~(0xFFFFFFFFFFFFFFFFUL >> s);
    }
    return as_type<long>(u);
}

// |v| as an unsigned magnitude, including 2^63 for i64::MIN.
static inline ulong magnitude(long v) {
    return (v < 0) ? (0UL - as_type<ulong>(v)) : as_type<ulong>(v);
}

// Full 64 x 64 -> 128-bit product as (low, high).
static inline ulong2 mul_wide(ulong a, ulong b) {
    const ulong a_lo = a & 0xFFFFFFFFUL;
    const ulong a_hi = a >> 32;
    const ulong b_lo = b & 0xFFFFFFFFUL;
    const ulong b_hi = b >> 32;
    const ulong ll = a_lo * b_lo;
    const ulong lh = a_lo * b_hi;
    const ulong hl = a_hi * b_lo;
    const ulong hh = a_hi * b_hi;
    const ulong mid = (ll >> 32) + (lh & 0xFFFFFFFFUL) + (hl & 0xFFFFFFFFUL);
    return ulong2((ll & 0xFFFFFFFFUL) | (mid << 32),
                  hh + (lh >> 32) + (hl >> 32) + (mid >> 32));
}

// Sum modulo 2^128.
static inline ulong2 add128(ulong2 a, ulong2 b) {
    const ulong lo = a.x + b.x;
    return ulong2(lo, a.y + b.y + ((lo < a.x) ? 1UL : 0UL));
}

// Negation modulo 2^128.
static inline ulong2 neg128(ulong2 v) {
    return ulong2(0UL - v.x, ~v.y + ((v.x == 0UL) ? 1UL : 0UL));
}

static inline ulong2 widen(long v) {
    return ulong2(as_type<ulong>(v), (v < 0) ? 0xFFFFFFFFFFFFFFFFUL : 0UL);
}

// The exact product of a long and an int, as a 128-bit value.
static inline ulong2 mul_signed(long a, int c) {
    const ulong2 m = mul_wide(magnitude(a), magnitude(long(c)));
    return ((a < 0) != (c < 0)) ? neg128(m) : m;
}

// n / d by shift and subtract, for 0 < d < 2^63 (so r << 1 cannot overflow).
static inline ulong2 udiv128(ulong2 n, ulong d) {
    ulong q_lo = 0UL;
    ulong q_hi = 0UL;
    ulong r = 0UL;
    for (int i = 127; i >= 0; --i) {
        const ulong bit = (i >= 64) ? ((n.y >> uint(i - 64)) & 1UL) : ((n.x >> uint(i)) & 1UL);
        r = (r << 1) | bit;
        if (r >= d) {
            r -= d;
            if (i >= 64) {
                q_hi |= (1UL << uint(i - 64));
            } else {
                q_lo |= (1UL << uint(i));
            }
        }
    }
    return ulong2(q_lo, q_hi);
}

// The sum of v over the simdgroup modulo 2^64, in four 16-bit pieces: each
// piece's simd_sum is at most 1,024 * 65,535 < 2^27, exact in int (the
// reduction metal_exact_gemv_i16.metal proves and uses).
static inline ulong simd_sum_mod64(ulong v) {
    ulong sum = 0UL;
    for (uint piece = 0u; piece < 4u; ++piece) {
        const int bits = int((v >> (16u * piece)) & 0xFFFFUL);
        sum += ulong(uint(simd_sum(bits))) << (16u * piece);
    }
    return sum;
}

// The same for a 128-bit value, modulo 2^128: the low word's pieces carry
// into each other and then into the high word.
static inline ulong2 simd_sum_mod128(ulong2 v) {
    ulong lo = 0UL;
    ulong carry = 0UL;
    for (uint piece = 0u; piece < 4u; ++piece) {
        const int bits = int((v.x >> (16u * piece)) & 0xFFFFUL);
        const ulong t = ulong(uint(simd_sum(bits))) + carry;
        lo |= (t & 0xFFFFUL) << (16u * piece);
        carry = t >> 16;
    }
    return ulong2(lo, simd_sum_mod64(v.y) + carry);
}

// ---- arith::dyadic_epilogue ----------------------------------------------

// floor(acc * mu / 2^k) for 0 <= mu < 2^31 and 16 <= k <= 62, or false
// where the CPU refuses an output beyond 2^62 (bound 1).
static inline bool dyadic_epilogue(long acc, int mu, uint k, thread long &result) {
    const ulong2 p = mul_wide(magnitude(acc), ulong(uint(mu)));
    const ulong q_lo = (p.x >> k) | (p.y << (64u - k));
    const ulong q_hi = p.y >> k;
    if (q_hi != 0UL || q_lo > ACTIVATION_LIMIT) {
        return false;
    }
    if (acc >= 0) {
        result = as_type<long>(q_lo);
        return true;
    }
    // A negative value's floor rounds its magnitude up.
    const ulong q = q_lo + (((p.x & ((1UL << k) - 1UL)) != 0UL) ? 1UL : 0UL);
    if (q > ACTIVATION_LIMIT) {
        return false;
    }
    result = as_type<long>(0UL - q);
    return true;
}

struct EpilogueParams {
    uint count;       // rows: heads * rows per head
    uint status_bit;  // ST_KEY or ST_VALUE
    uint reserved0;
    uint reserved1;
};

kernel void mla_epilogue(
    device const long *dots [[buffer(0)]],
    device const int *mu [[buffer(1)]],
    device const uchar *shift [[buffer(2)]],
    device long *out [[buffer(3)]],
    device atomic_uint *status [[buffer(4)]],
    constant EpilogueParams &p [[buffer(5)]],
    uint i [[thread_position_in_grid]])
{
    if (i >= p.count) {
        return;
    }
    long value = 0;
    if (!dyadic_epilogue(dots[i], mu[i], uint(shift[i]), value)) {
        atomic_fetch_or_explicit(status, p.status_bit, memory_order_relaxed);
        value = 0;
    }
    out[i] = value;
}

// ---- ops::mla_attend: scores ---------------------------------------------

// floor(dot * lambda / 2^46) for a 128-bit dot below 2^110 in magnitude and
// 0 < lambda < 2^31, or false where the CPU refuses (bound 5).
static inline bool attention_score(ulong2 dot, long lambda, thread long &score) {
    const bool negative = as_type<long>(dot.y) < 0;
    const ulong2 m = negative ? neg128(dot) : dot;
    const ulong l = as_type<ulong>(lambda);
    const ulong2 low = mul_wide(m.x, l);
    const ulong2 high = mul_wide(m.y, l);
    // |dot| * lambda = v0 + v1 * 2^64 + v2 * 2^128.
    const ulong v0 = low.x;
    const ulong v1 = low.y + high.x;
    const ulong v2 = high.y + ((v1 < low.y) ? 1UL : 0UL);
    if (v2 != 0UL) {
        return false;
    }
    const ulong q_lo = (v0 >> SCORE_SHIFT) | (v1 << (64u - SCORE_SHIFT));
    const ulong q_hi = v1 >> SCORE_SHIFT;
    if (q_hi != 0UL || q_lo > ACTIVATION_LIMIT) {
        return false;
    }
    if (!negative) {
        score = as_type<long>(q_lo);
        return true;
    }
    const ulong q = q_lo + (((v0 & SCORE_FRACTION) != 0UL) ? 1UL : 0UL);
    if (q > ACTIVATION_LIMIT) {
        return false;
    }
    score = as_type<long>(0UL - q);
    return true;
}

struct ScoreParams {
    uint heads;
    uint rank;
    uint rope_dim;
    uint positions;
    uint stripe;     // positions per simdgroup
    uint reserved;
    long lambda;     // in (0, 2^31)
};

// One simdgroup per head and stripe of positions; its lanes stride along the
// latent and RoPE dimensions (coalesced loads). The simdgroups of a
// threadgroup are consecutive heads over the same positions, so they read the
// same cache rows.
kernel void mla_scores(
    device const long *qa [[buffer(0)]],
    device const long *qp [[buffer(1)]],
    device const int *latent [[buffer(2)]],
    device const int *rope_keys [[buffer(3)]],
    device long *scores [[buffer(4)]],
    device atomic_uint *status [[buffer(5)]],
    constant ScoreParams &p [[buffer(6)]],
    uint2 tg [[threadgroup_position_in_grid]],
    uint sg [[simdgroup_index_in_threadgroup]],
    uint sg_count [[simdgroups_per_threadgroup]],
    uint lane [[thread_index_in_simdgroup]],
    uint width [[threads_per_simdgroup]])
{
    const uint head = tg.y * sg_count + sg;
    const uint first = tg.x * p.stripe;
    // Uniform across the simdgroup, so every simd_sum below sees all lanes.
    if (head >= p.heads || first >= p.positions) {
        return;
    }
    const uint stop = first + min(p.positions - first, p.stripe);
    device const long *a = qa + ulong(head) * ulong(p.rank);
    device const long *b = qp + ulong(head) * ulong(p.rope_dim);

    // The CPU's rule (bound 2): narrow iff sum |q| < 2^32. Both reductions
    // run on every lane, so no simd function sits in a branch.
    int beyond = 0;
    ulong mass = 0UL;
    for (uint r = lane; r < p.rank; r += width) {
        const ulong m = magnitude(a[r]);
        if (m >= NARROW_LIMIT) {
            beyond = 1;
        } else {
            mass += m;
        }
    }
    for (uint t = lane; t < p.rope_dim; t += width) {
        const ulong m = magnitude(b[t]);
        if (m >= NARROW_LIMIT) {
            beyond = 1;
        } else {
            mass += m;
        }
    }
    const int beyond_lanes = simd_sum(beyond);
    const ulong total_mass = simd_sum_mod64(mass);
    const bool narrow = (beyond_lanes == 0) && (total_mass < NARROW_LIMIT);

    for (uint i = first; i < stop; ++i) {
        device const int *row = latent + ulong(i) * ulong(p.rank);
        device const int *key = rope_keys + ulong(i) * ulong(p.rope_dim);
        ulong2 dot;
        if (narrow) {
            // Bound 3: products and sums modulo 2^64 are exact.
            ulong part = 0UL;
            for (uint r = lane; r < p.rank; r += width) {
                part += as_type<ulong>(a[r]) * as_type<ulong>(long(row[r]));
            }
            for (uint t = lane; t < p.rope_dim; t += width) {
                part += as_type<ulong>(b[t]) * as_type<ulong>(long(key[t]));
            }
            dot = widen(as_type<long>(simd_sum_mod64(part)));
        } else {
            // Bound 4: exact 128-bit products and sums.
            ulong2 part = ulong2(0UL, 0UL);
            for (uint r = lane; r < p.rank; r += width) {
                part = add128(part, mul_signed(a[r], row[r]));
            }
            for (uint t = lane; t < p.rope_dim; t += width) {
                part = add128(part, mul_signed(b[t], key[t]));
            }
            dot = simd_sum_mod128(part);
        }
        long score = 0;
        const bool ok = attention_score(dot, p.lambda, score);
        if (lane == 0u) {
            if (!ok) {
                atomic_fetch_or_explicit(status, ST_SCORE, memory_order_relaxed);
                score = 0;
            }
            scores[ulong(head) * ulong(p.positions) + ulong(i)] = score;
        }
    }
}

// ---- ops::mla_attend: weights --------------------------------------------

// arith::exp_q16: exp(x) in Q16 for x <= 0, by linear interpolation in the
// table (bound 6). The table never decreases, but the shift is a floor for
// either sign, as the CPU's `>>` is.
static inline long exp_q16(long x, device const long *table) {
    if (x >= 0) {
        return ONE_Q16;
    }
    if (x <= -16 * ONE_Q16) {
        return 0;
    }
    const long offset = x + 16 * ONE_Q16;
    const uint index = uint(offset >> 8);
    const long fraction = offset & 255;
    if (index >= EXP_STEPS) {
        return table[EXP_STEPS];
    }
    const long low = table[index];
    const long high = table[index + 1u];
    return low + sar((high - low) * fraction, 8u);
}

struct SoftmaxParams {
    uint heads;
    uint positions;
    uint reserved0;
    uint reserved1;
};

// One threadgroup per head, of a power-of-two number of threads (at most
// SOFTMAX_THREADS): the maximum score, then every weight and their exact sum.
kernel void mla_softmax(
    device const long *scores [[buffer(0)]],
    device long *weights [[buffer(1)]],
    device long *totals [[buffer(2)]],
    device const long *table [[buffer(3)]],
    constant SoftmaxParams &p [[buffer(4)]],
    uint head [[threadgroup_position_in_grid]],
    uint tid [[thread_index_in_threadgroup]],
    uint threads [[threads_per_threadgroup]])
{
    threadgroup long shared[SOFTMAX_THREADS];
    if (head >= p.heads) {
        return;
    }
    device const long *s = scores + ulong(head) * ulong(p.positions);
    device long *w = weights + ulong(head) * ulong(p.positions);

    long best = LOWEST;
    for (uint i = tid; i < p.positions; i += threads) {
        const long v = s[i];
        best = (v > best) ? v : best;
    }
    shared[tid] = best;
    threadgroup_barrier(mem_flags::mem_threadgroup);
    for (uint step = threads >> 1; step > 0u; step >>= 1) {
        if (tid < step) {
            const long other = shared[tid + step];
            if (other > shared[tid]) {
                shared[tid] = other;
            }
        }
        threadgroup_barrier(mem_flags::mem_threadgroup);
    }
    const long top = shared[0];
    threadgroup_barrier(mem_flags::mem_threadgroup);

    long sum = 0;
    for (uint i = tid; i < p.positions; i += threads) {
        const long e = exp_q16(wsub(s[i], top), table);
        w[i] = e;
        sum = wadd(sum, e);
    }
    shared[tid] = sum;
    threadgroup_barrier(mem_flags::mem_threadgroup);
    for (uint step = threads >> 1; step > 0u; step >>= 1) {
        if (tid < step) {
            shared[tid] = wadd(shared[tid], shared[tid + step]);
        }
        threadgroup_barrier(mem_flags::mem_threadgroup);
    }
    if (tid == 0u) {
        totals[head] = shared[0];
    }
}

// ---- ops::mla_attend: output, and the next projection's digits ------------

struct WeightedParams {
    uint heads;
    uint rank;
    uint positions;
    uint padded;     // digit columns per plane: rank rounded up to 8
};

// One simdgroup per head and 32 consecutive latent dimensions (one lane
// each); the simdgroups of a threadgroup are consecutive heads over the same
// dimensions, so they read the same cache rows. Lanes past the rank write the
// padding digits, which must be zero.
kernel void mla_weighted(
    device const long *weights [[buffer(0)]],
    device const long *totals [[buffer(1)]],
    device const int *latent [[buffer(2)]],
    device long *u [[buffer(3)]],
    device char *digits [[buffer(4)]],
    device atomic_uint *status [[buffer(5)]],
    constant WeightedParams &p [[buffer(6)]],
    uint2 tg [[threadgroup_position_in_grid]],
    uint sg [[simdgroup_index_in_threadgroup]],
    uint sg_count [[simdgroups_per_threadgroup]],
    uint lane [[thread_index_in_simdgroup]],
    uint width [[threads_per_simdgroup]])
{
    const uint head = tg.y * sg_count + sg;
    const uint column = tg.x * width + lane;
    if (head >= p.heads || column >= p.padded) {
        return;
    }
    device char *planes = digits + ulong(head) * ulong(HEAD_PLANES) * ulong(p.padded);
    if (column >= p.rank) {
        for (uint d = 0u; d < HEAD_PLANES; ++d) {
            planes[ulong(d) * ulong(p.padded) + ulong(column)] = 0;
        }
        return;
    }
    device const long *w = weights + ulong(head) * ulong(p.positions);

    // Bound 7: at most MLA_FLUSH products per long, then into 128 bits.
    ulong2 acc = ulong2(0UL, 0UL);
    for (uint start = 0u; start < p.positions; start += MLA_FLUSH) {
        const uint stop = start + min(p.positions - start, MLA_FLUSH);
        ulong part = 0UL;
        for (uint i = start; i < stop; ++i) {
            const long c = long(latent[ulong(i) * ulong(p.rank) + ulong(column)]);
            part += as_type<ulong>(w[i]) * as_type<ulong>(c);
        }
        acc = add128(acc, widen(as_type<long>(part)));
    }

    // u = trunc(acc / total) (bound 6: 2^16 <= total < 2^47).
    const bool negative = as_type<long>(acc.y) < 0;
    const ulong2 q = udiv128(negative ? neg128(acc) : acc, as_type<ulong>(totals[head]));
    long value = 0;
    if (q.y != 0UL || q.x > OUTPUT_LIMIT) {
        // Unreachable (bound 8); refused rather than written.
        atomic_fetch_or_explicit(status, ST_OUTPUT, memory_order_relaxed);
    } else {
        value = negative ? as_type<long>(0UL - q.x) : as_type<long>(q.x);
    }
    u[ulong(head) * ulong(p.rank) + ulong(column)] = value;

    // u as seven balanced base-256 digits c in [-128, 127], u = sum_d c_d *
    // 256^d, the decomposition of metal_exact_i16.rs's split_planes (it is
    // unique). |u| <= 2^31 needs five; the rest are zero.
    long rest = value;
    for (uint d = 0u; d < HEAD_PLANES; ++d) {
        const int low = int(as_type<ulong>(rest) & 255UL);
        const int c = (low > 127) ? (low - 256) : low;
        planes[ulong(d) * ulong(p.padded) + ulong(column)] = char(c);
        rest = sar(wsub(rest, long(c)), 8u);
    }
    if (rest != 0) {
        atomic_fetch_or_explicit(status, ST_OUTPUT, memory_order_relaxed);
    }
}
