// Exact decoder kernels for ARC's canonical per-row INT8 profile: everything a
// Llama decoder token needs between its projections, so that a whole token
// runs in one command buffer (metal_decoder.rs).
//
// Every kernel reproduces the CPU engine operation by operation
// (crates/arc-inference/src/cached_integer_model.rs: layernorm, apply_rope,
// flash_attention_i64, silu_i64; integer_lut.rs: integer_exp,
// integer_isqrt), with the semantics of the release build the engine ships:
// i64 products, sums and differences wrap modulo 2^64, `>>` is an arithmetic
// (floor) shift, and `/` truncates toward zero. MSL leaves signed overflow
// undefined, so:
//
//  * products, sums and differences are formed in ulong and reinterpreted
//    (wmul, wadd, wsub): two's complement modulo 2^64, as on the CPU;
//  * arithmetic shifts are logical shifts plus an explicit sign fill (sar);
//  * every division is a shift-subtract loop on unsigned magnitudes (udiv64,
//    udiv128) or a biased shift for a power of two (div_pow2_trunc). There is
//    no native integer division (#150 found a miscompiled one on an M2 Ultra);
//  * there is no floating point anywhere in this file.
//
// Where the CPU would compute something these kernels do not reproduce, a
// kernel sets a bit in the status word instead, and the host discards the
// token's GPU result and runs the CPU engine for that token:
//
//  * ST_NORM_DOMAIN: an RMSNorm input of magnitude 2^56 or more. Below that
//    every square is under 2^112 and a sum of at most 2^15 squares stays
//    under 2^127, so the CPU's i128 sum is exact and non-negative;
//  * ST_SPLIT_DOMAIN: a projection input outside the four-digit domain
//    [-2,155,905,152, 2,139,062,143], or a digit reconstruction mismatch;
//  * ST_SCALE_BOUND: 128 * sum|x| * max|s| above i64::MAX for the matrices
//    that read the vector (the same rule as the single-projection kernel).

#include <metal_stdlib>
using namespace metal;

constant long ONE_Q16 = 65536;
constant long PLANE_MAX_V = 2139062143;
constant long PLANE_MIN_V = -2155905152;
constant ulong NORM_LIMIT = 72057594037927936UL; // 2^56
constant ulong I64_MAX_U = 9223372036854775807UL;
constant uint ST_NORM_DOMAIN = 1u;
constant uint ST_SPLIT_DOMAIN = 2u;
constant uint ST_SCALE_BOUND = 4u;
// Dimensions per lane in attention: d_head <= HEAD_SLOTS * 32 = 256.
#define HEAD_SLOTS 8u

// ---- 64-bit two's-complement helpers -------------------------------------

static inline long wadd(long a, long b) {
    return as_type<long>(as_type<ulong>(a) + as_type<ulong>(b));
}

static inline long wsub(long a, long b) {
    return as_type<long>(as_type<ulong>(a) - as_type<ulong>(b));
}

static inline long wmul(long a, long b) {
    return as_type<long>(as_type<ulong>(a) * as_type<ulong>(b));
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

// Rust's truncating n / 2^k for k in [1, 62]: bias negative values by 2^k - 1.
static inline long div_pow2_trunc(long n, uint k) {
    const long bias = (n < 0) ? as_type<long>((1UL << k) - 1UL) : 0;
    return sar(wadd(n, bias), k);
}

// n / d by shift and subtract, for 0 < d < 2^63 (so r << 1 cannot overflow).
static inline ulong udiv64(ulong n, ulong d) {
    ulong q = 0UL;
    ulong r = 0UL;
    for (int i = 63; i >= 0; --i) {
        r = (r << 1) | ((n >> uint(i)) & 1UL);
        if (r >= d) {
            r -= d;
            q |= (1UL << uint(i));
        }
    }
    return q;
}

// Rust's truncating i64 division n / d for 0 < d < 2^63.
static inline long div_trunc(long n, long d) {
    const ulong q = udiv64(magnitude(n), as_type<ulong>(d));
    return (n < 0) ? as_type<long>(0UL - q) : as_type<long>(q);
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
    return ulong2((ll & 0xFFFFFFFFUL) | (mid << 32), hh + (lh >> 32) + (hl >> 32) + (mid >> 32));
}

static inline ulong2 add128(ulong2 a, ulong2 b) {
    const ulong lo = a.x + b.x;
    return ulong2(lo, a.y + b.y + ((lo < a.x) ? 1UL : 0UL));
}

// 128-bit n / d by shift and subtract, for 0 < d < 2^63.
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

// ---- integer_lut.rs ------------------------------------------------------

// integer_exp: exp(x) in Q16 for x <= 0 by linear interpolation in the
// 4,097-entry table. offset is in (0, 16 * ONE), so offset / 256 and
// offset % 256 are a shift and a mask, and (hi - lo) * frac >= 0 because the
// table never decreases, so the final division is a shift as well.
static inline long iexp(long x, device const long *lut) {
    if (x >= 0) {
        return ONE_Q16;
    }
    if (x <= -16 * ONE_Q16) {
        return 0;
    }
    const long offset = x + 16 * ONE_Q16;
    const uint idx = uint(offset >> 8);
    const long frac = offset & 255;
    if (idx >= 4096u) {
        return ONE_Q16;
    }
    const long lo = lut[idx];
    const long hi = lut[idx + 1u];
    return lo + (((hi - lo) * frac) >> 8);
}

// integer_isqrt: exactly the CPU's five Newton steps from a bit-length
// estimate. ONE * 256 / 2^s is (2^24) >> s; y * (3 * ONE - x * y^2) / (2 *
// ONE) truncates toward zero.
static long isqrt_q16(long x) {
    if (x <= 0) {
        return ONE_Q16 * 100;
    }
    uint bits = 0u;
    for (ulong t = as_type<ulong>(x); t > 1UL; t >>= 1) {
        bits += 1u;
    }
    long y = as_type<long>((1UL << 24) >> ((bits + 1u) >> 1));
    if (y <= 0) {
        y = 1;
    }
    for (int i = 0; i < 5; ++i) {
        const long y2 = sar(wmul(y, y), 16);
        const long xy2 = sar(wmul(x, y2), 16);
        const long three_minus = wsub(3 * ONE_Q16, xy2);
        y = div_pow2_trunc(wmul(y, three_minus), 17);
        if (y <= 0) {
            y = 1;
        }
    }
    return y;
}

// ---- RMSNorm (layernorm) -------------------------------------------------

struct NormParams {
    uint n;
    uint pad0;
    uint pad1;
    uint pad2;
};

// One threadgroup of 256 threads (a power of two). gamma is padded with ONE
// up to n by the host, as the CPU reads ONE past gamma's end.
kernel void rms_norm(
    device const long *x [[buffer(0)]],
    device const long *gamma [[buffer(1)]],
    device long *y [[buffer(2)]],
    device atomic_uint *status [[buffer(3)]],
    constant NormParams &p [[buffer(4)]],
    uint tid [[thread_index_in_threadgroup]],
    uint threads [[threads_per_threadgroup]])
{
    threadgroup ulong part_lo[256];
    threadgroup ulong part_hi[256];
    threadgroup long inv_rms;
    ulong2 acc = ulong2(0UL, 0UL);
    bool big = false;
    for (uint j = tid; j < p.n; j += threads) {
        const ulong m = magnitude(x[j]);
        if (m >= NORM_LIMIT) {
            big = true;
        }
        acc = add128(acc, mul_wide(m, m));
    }
    if (big) {
        atomic_fetch_or_explicit(status, ST_NORM_DOMAIN, memory_order_relaxed);
    }
    part_lo[tid] = acc.x;
    part_hi[tid] = acc.y;
    threadgroup_barrier(mem_flags::mem_threadgroup);
    for (uint stride = threads >> 1; stride > 0u; stride >>= 1) {
        if (tid < stride) {
            const ulong2 s = add128(ulong2(part_lo[tid], part_hi[tid]),
                                    ulong2(part_lo[tid + stride], part_hi[tid + stride]));
            part_lo[tid] = s.x;
            part_hi[tid] = s.y;
        }
        threadgroup_barrier(mem_flags::mem_threadgroup);
    }
    if (tid == 0u) {
        // mean_sq_q32 = sum / n; mean_sq = (mean_sq_q32 >> 16) as i64, i.e.
        // the low 64 bits of the shifted 128-bit quotient.
        const ulong2 mean = udiv128(ulong2(part_lo[0], part_hi[0]), ulong(p.n));
        const ulong mean_sq = (mean.x >> 16) | (mean.y << 48);
        inv_rms = isqrt_q16(wadd(as_type<long>(mean_sq), 1));
    }
    threadgroup_barrier(mem_flags::mem_threadgroup);
    const long inv = inv_rms;
    for (uint j = tid; j < p.n; j += threads) {
        const long norm = sar(wmul(x[j], inv), 16);
        y[j] = sar(wmul(norm, gamma[j]), 16);
    }
}

// ---- projection input: four balanced base-256 digit planes ---------------

struct SplitParams {
    uint n;          // activation length
    uint stride;     // bytes per digit plane: n rounded up to 16
    ulong max_scale; // the largest |s_i| of every matrix that reads the planes
};

// One threadgroup of 256 threads. Writes plane d of x[j] at d * stride + j
// (zero padding up to stride), ctrl[0] = planes in use (the highest non-zero
// digit, at least 1), and flags the domain and scale bound.
kernel void split_planes(
    device const long *x [[buffer(0)]],
    device char *planes [[buffer(1)]],
    device uint *ctrl [[buffer(2)]],
    device atomic_uint *status [[buffer(3)]],
    constant SplitParams &p [[buffer(4)]],
    uint tid [[thread_index_in_threadgroup]],
    uint threads [[threads_per_threadgroup]])
{
    threadgroup ulong mass[256];
    threadgroup uint top[256];
    ulong sum_abs = 0UL;
    uint used = 1u;
    bool bad = false;
    for (uint j = tid; j < p.stride; j += threads) {
        if (j >= p.n) {
            for (uint d = 0u; d < 4u; ++d) {
                planes[d * p.stride + j] = char(0);
            }
            continue;
        }
        const long v = x[j];
        if (v < PLANE_MIN_V || v > PLANE_MAX_V) {
            bad = true;
        }
        ulong u = as_type<ulong>(v);
        ulong rebuilt = 0UL;
        for (uint d = 0u; d < 4u; ++d) {
            const int low = int(u & 255UL);
            const int c = (low > 127) ? (low - 256) : low;
            planes[d * p.stride + j] = char(c);
            rebuilt += as_type<ulong>(long(c)) << (8u * d);
            u = (u - as_type<ulong>(long(c))) >> 8;
            if (c != 0) {
                used = max(used, d + 1u);
            }
        }
        if (as_type<long>(rebuilt) != v) {
            bad = true;
        }
        sum_abs += magnitude(v);
    }
    if (bad) {
        atomic_fetch_or_explicit(status, ST_SPLIT_DOMAIN, memory_order_relaxed);
    }
    mass[tid] = sum_abs;
    top[tid] = used;
    threadgroup_barrier(mem_flags::mem_threadgroup);
    for (uint stride = threads >> 1; stride > 0u; stride >>= 1) {
        if (tid < stride) {
            mass[tid] += mass[tid + stride];
            top[tid] = max(top[tid], top[tid + stride]);
        }
        threadgroup_barrier(mem_flags::mem_threadgroup);
    }
    if (tid == 0u) {
        ctrl[0] = top[0];
        // In the domain, sum|x| <= 131,071 * 2^31 < 2^48, so 128 * sum|x|
        // fits and the 128-bit product decides the bound exactly.
        const ulong2 bound = mul_wide(mass[0] << 7, p.max_scale);
        if (bound.y != 0UL || bound.x > I64_MAX_U) {
            atomic_fetch_or_explicit(status, ST_SCALE_BOUND, memory_order_relaxed);
        }
    }
}

// ---- RoPE (split half) and the KV-cache append ---------------------------

struct RopeParams {
    uint n_heads;
    uint n_kv_heads;
    uint pairs; // d_head / 2
    uint d_head;
    uint pos;
    uint d_kv;
    uint pad0;
    uint pad1;
};

// Thread (i, head): rotates pair i of a query head in place, or of a key head
// into the key cache row `pos`, and copies the matching value pair into the
// value cache row `pos`.
kernel void rope_store(
    device long *q [[buffer(0)]],
    device const long *k [[buffer(1)]],
    device const long *v [[buffer(2)]],
    device long *k_cache [[buffer(3)]],
    device long *v_cache [[buffer(4)]],
    device const long *cos_table [[buffer(5)]],
    device const long *sin_table [[buffer(6)]],
    constant RopeParams &p [[buffer(7)]],
    uint2 gid [[thread_position_in_grid]])
{
    const uint i = gid.x;
    const uint head = gid.y;
    if (i >= p.pairs || head >= p.n_heads + p.n_kv_heads) {
        return;
    }
    const ulong t = ulong(p.pos) * ulong(p.pairs) + ulong(i);
    const long c = cos_table[t];
    const long s = sin_table[t];
    if (head < p.n_heads) {
        const ulong base = ulong(head) * ulong(p.d_head);
        const long x0 = q[base + i];
        const long x1 = q[base + i + p.pairs];
        q[base + i] = wsub(sar(wmul(x0, c), 16), sar(wmul(x1, s), 16));
        q[base + i + p.pairs] = wadd(sar(wmul(x0, s), 16), sar(wmul(x1, c), 16));
    } else {
        const ulong base = ulong(head - p.n_heads) * ulong(p.d_head);
        const ulong row = ulong(p.pos) * ulong(p.d_kv) + base;
        const long x0 = k[base + i];
        const long x1 = k[base + i + p.pairs];
        k_cache[row + i] = wsub(sar(wmul(x0, c), 16), sar(wmul(x1, s), 16));
        k_cache[row + i + p.pairs] = wadd(sar(wmul(x0, s), 16), sar(wmul(x1, c), 16));
        v_cache[row + i] = v[base + i];
        v_cache[row + i + p.pairs] = v[base + i + p.pairs];
    }
}

// ---- attention (flash_attention_i64) --------------------------------------

struct AttnParams {
    uint d_head;
    uint d_kv;
    uint positions; // pos + 1
    uint pad0;
    long attn_scale;
};

// Every lane's partial sum, added modulo 2^64 across the 32-lane simdgroup.
// Addition modulo 2^64 is associative, so the tree order cannot matter.
static inline ulong simd_sum_u64(ulong v) {
    for (ushort offset = 16; offset > 0; offset >>= 1) {
        const uint2 parts = as_type<uint2>(v);
        const uint2 other = uint2(simd_shuffle_xor(parts.x, offset),
                                  simd_shuffle_xor(parts.y, offset));
        v += as_type<ulong>(other);
    }
    return v;
}

// One 32-lane simdgroup per query head; lane l owns dimensions l, l + 32, ...
// Positions are visited in order, exactly as the CPU does: the online softmax
// truncates every time the running maximum rises, so order is part of the
// definition. Every lane computes the same scalar state (dot, score, max,
// sum); only the per-dimension accumulators differ.
kernel void attention(
    device const long *q [[buffer(0)]],
    device const long *k_cache [[buffer(1)]],
    device const long *v_cache [[buffer(2)]],
    device long *out [[buffer(3)]],
    device const long *lut [[buffer(4)]],
    device const uint *head_kv [[buffer(5)]],
    constant AttnParams &p [[buffer(6)]],
    uint head [[threadgroup_position_in_grid]],
    uint lane [[thread_index_in_simdgroup]])
{
    const uint dh = p.d_head;
    const ulong kv_base = ulong(head_kv[head]) * ulong(dh);
    const ulong q_base = ulong(head) * ulong(dh);
    long qv[HEAD_SLOTS];
    long acc[HEAD_SLOTS];
    for (uint s = 0u; s < HEAD_SLOTS; ++s) {
        const uint dd = lane + 32u * s;
        qv[s] = (dd < dh) ? q[q_base + dd] : 0;
        acc[s] = 0;
    }
    long running_max = -4611686018427387904L; // i64::MIN / 2
    long running_sum = 0;
    for (uint j = 0u; j < p.positions; ++j) {
        const ulong row = ulong(j) * ulong(p.d_kv) + kv_base;
        ulong partial = 0UL;
        for (uint s = 0u; s < HEAD_SLOTS; ++s) {
            const uint dd = lane + 32u * s;
            if (dd < dh) {
                partial += as_type<ulong>(qv[s]) * as_type<ulong>(k_cache[row + dd]);
            }
        }
        const long dot = as_type<long>(simd_sum_u64(partial));
        const long score = sar(wmul(sar(dot, 16), p.attn_scale), 16);
        if (score > running_max) {
            const long correction = iexp(wsub(running_max, score), lut);
            running_sum = sar(wmul(running_sum, correction), 16);
            for (uint s = 0u; s < HEAD_SLOTS; ++s) {
                acc[s] = sar(wmul(acc[s], correction), 16);
            }
            running_max = score;
        }
        const long w = iexp(wsub(score, running_max), lut);
        running_sum = wadd(running_sum, w);
        for (uint s = 0u; s < HEAD_SLOTS; ++s) {
            const uint dd = lane + 32u * s;
            if (dd < dh) {
                acc[s] = wadd(acc[s], sar(wmul(w, v_cache[row + dd]), 16));
            }
        }
    }
    // running_sum <= positions * ONE < 2^63, so div_trunc's bound holds.
    if (running_sum > 0) {
        for (uint s = 0u; s < HEAD_SLOTS; ++s) {
            acc[s] = div_trunc(wmul(acc[s], ONE_Q16), running_sum);
        }
    }
    for (uint s = 0u; s < HEAD_SLOTS; ++s) {
        const uint dd = lane + 32u * s;
        if (dd < dh) {
            out[q_base + dd] = acc[s];
        }
    }
}

// ---- projections: the exact GEMV with a GPU-side plane count -------------

// The same parameter block, helpers and proof as metal_exact_gemv.metal,
// repeated here so that this library compiles on its own: a fault in the
// decoder kernels can never take down the single-projection engine.
struct ExactGemvParams {
    uint rows;
    uint row_offset;
    uint blocks;
    uint mode;
};

static inline int dot4_i32(uint a, uint b) {
    const int4 x = int4(as_type<char4>(a));
    const int4 y = int4(as_type<char4>(b));
    const int4 p = x * y;
    return p.x + p.y + p.z + p.w;
}

static inline int dot4_i16(uint a, uint b) {
    const short4 p = short4(as_type<char4>(a)) * short4(as_type<char4>(b));
    return int(p.x) + int(p.y) + int(p.z) + int(p.w);
}

template <uint MUL16>
static inline int dot16(uint4 w, uint4 c) {
    if (MUL16 != 0) {
        return dot4_i16(w.x, c.x) + dot4_i16(w.y, c.y) + dot4_i16(w.z, c.z) + dot4_i16(w.w, c.w);
    }
    return dot4_i32(w.x, c.x) + dot4_i32(w.y, c.y) + dot4_i32(w.z, c.z) + dot4_i32(w.w, c.w);
}

// The exact GEMV of metal_exact_gemv.metal, except that the number of digit
// planes in use is read from ctrl[0], which split_planes wrote earlier in the
// same command buffer. Planes
// at or above it are all zero, so skipping them changes the work, never the
// value; every proof step above holds unchanged. The plane branches are
// uniform across the whole grid.
template <uint ROWS, uint MUL16>
kernel void exact_gemv_dyn(
    device const uint4 *weights [[buffer(0)]],
    device const uint4 *digits [[buffer(1)]],
    device const long *scales [[buffer(2)]],
    device long *out [[buffer(3)]],
    constant ExactGemvParams &p [[buffer(4)]],
    device const uint *ctrl [[buffer(5)]],
    uint tg_index [[threadgroup_position_in_grid]],
    uint sg_index [[simdgroup_index_in_threadgroup]],
    uint sg_count [[simdgroups_per_threadgroup]],
    uint lane [[thread_index_in_simdgroup]],
    uint sg_width [[threads_per_simdgroup]])
{
    const uint planes = min(ctrl[0], 4u);
    const uint first = (tg_index * sg_count + sg_index) * ROWS;
    if (first >= p.rows) {
        return;
    }

    device const uint4 *row_ptr[ROWS];
    for (uint r = 0; r < ROWS; ++r) {
        const uint local = min(first + r, p.rows - 1u);
        row_ptr[r] = weights + ulong(p.row_offset + local) * ulong(p.blocks);
    }

    int acc[ROWS][4];
    for (uint r = 0; r < ROWS; ++r) {
        for (uint d = 0; d < 4u; ++d) {
            acc[r][d] = 0;
        }
    }

    for (uint b = lane; b < p.blocks; b += sg_width) {
        uint4 c[4];
        for (uint d = 0; d < 4u; ++d) {
            c[d] = (d < planes) ? digits[d * p.blocks + b] : uint4(0u);
        }
        for (uint r = 0; r < ROWS; ++r) {
            const uint4 w = row_ptr[r][b];
            for (uint d = 0; d < 4u; ++d) {
                if (d < planes) {
                    acc[r][d] += dot16<MUL16>(w, c[d]);
                }
            }
        }
    }

    for (uint r = 0; r < ROWS; ++r) {
        for (uint d = 0; d < 4u; ++d) {
            acc[r][d] = simd_sum(acc[r][d]);
        }
    }

    for (uint r = 0; r < ROWS; ++r) {
        if (lane == r && first + r < p.rows) {
            ulong total = 0;
            for (uint d = 0; d < 4u; ++d) {
                total += as_type<ulong>(long(acc[r][d])) << (8u * d);
            }
            if (p.mode == 0u) {
                const ulong product = total * as_type<ulong>(scales[p.row_offset + first + r]);
                ulong shifted = product >> 16;
                if (as_type<long>(product) < 0) {
                    shifted |= 0xFFFF000000000000UL;
                }
                out[first + r] = as_type<long>(shifted);
            } else {
                out[first + r] = as_type<long>(total);
            }
        }
    }
}

typedef decltype(exact_gemv_dyn<1, 0>) exact_gemv_dyn_t;

#define EXACT_GEMV_DYN(R, M)                                                    \
    template [[host_name("exact_gemv_dyn_r" #R "_m" #M)]]                      \
    kernel exact_gemv_dyn_t exact_gemv_dyn<R, M>;

EXACT_GEMV_DYN(1, 0)
EXACT_GEMV_DYN(2, 0)
EXACT_GEMV_DYN(4, 0)
EXACT_GEMV_DYN(8, 0)
EXACT_GEMV_DYN(1, 1)
EXACT_GEMV_DYN(2, 1)
EXACT_GEMV_DYN(4, 1)
EXACT_GEMV_DYN(8, 1)

// ---- MLP activation and residual -----------------------------------------

struct VecParams {
    uint n;
    uint pad0;
    uint pad1;
    uint pad2;
};

// silu_i64: sigma is ONE^2 / (ONE + exp(-x)) or exp(x) * ONE / (ONE + exp(x)),
// both non-negative over a divisor in [ONE, 2 * ONE].
static inline long silu_q16(long x, device const long *lut) {
    long sigma;
    if (x >= 0) {
        const long e = iexp(wsub(0, x), lut);
        sigma = as_type<long>(udiv64(1UL << 32, as_type<ulong>(ONE_Q16 + e)));
    } else {
        const long e = iexp(x, lut);
        sigma = as_type<long>(udiv64(as_type<ulong>(e * ONE_Q16), as_type<ulong>(ONE_Q16 + e)));
    }
    return sar(wmul(x, sigma), 16);
}

// act[j] = (silu(gate[j]) * up[j]) >> 16
kernel void silu_mul(
    device const long *gate [[buffer(0)]],
    device const long *up [[buffer(1)]],
    device long *act [[buffer(2)]],
    device const long *lut [[buffer(3)]],
    constant VecParams &p [[buffer(4)]],
    uint gid [[thread_position_in_grid]])
{
    if (gid >= p.n) {
        return;
    }
    act[gid] = sar(wmul(silu_q16(gate[gid], lut), up[gid]), 16);
}

// hidden[j] += delta[j]
kernel void residual_add(
    device long *hidden [[buffer(0)]],
    device const long *delta [[buffer(1)]],
    constant VecParams &p [[buffer(2)]],
    uint gid [[thread_position_in_grid]])
{
    if (gid >= p.n) {
        return;
    }
    hidden[gid] = wadd(hidden[gid], delta[gid]);
}

// ---- multi-row passes: up to 8 consecutive tokens at once ----------------
//
// The kernels below run k = 1..8 consecutive tokens of one sequence in one
// pass (metal_decoder.rs, MetalDecoder::step_rows). Each is its single-row
// counterpart above with a row index r: row r reads and writes its own slice
// of every activation buffer (rows are contiguous, n values apart), and its
// token sits at position pos + r. The arithmetic of a row is exactly that of
// the single-row kernel, so a k-row pass equals k single-row passes value for
// value. Only the projections change shape: exact_gemm_dyn applies each
// weight it reads to all k rows.

// rms_norm, one threadgroup of 256 threads per row.
kernel void rms_norm_rows(
    device const long *x [[buffer(0)]],
    device const long *gamma [[buffer(1)]],
    device long *y [[buffer(2)]],
    device atomic_uint *status [[buffer(3)]],
    constant NormParams &p [[buffer(4)]],
    uint row [[threadgroup_position_in_grid]],
    uint tid [[thread_index_in_threadgroup]],
    uint threads [[threads_per_threadgroup]])
{
    threadgroup ulong part_lo[256];
    threadgroup ulong part_hi[256];
    threadgroup long inv_rms;
    device const long *xr = x + ulong(row) * ulong(p.n);
    device long *yr = y + ulong(row) * ulong(p.n);
    ulong2 acc = ulong2(0UL, 0UL);
    bool big = false;
    for (uint j = tid; j < p.n; j += threads) {
        const ulong m = magnitude(xr[j]);
        if (m >= NORM_LIMIT) {
            big = true;
        }
        acc = add128(acc, mul_wide(m, m));
    }
    if (big) {
        atomic_fetch_or_explicit(status, ST_NORM_DOMAIN, memory_order_relaxed);
    }
    part_lo[tid] = acc.x;
    part_hi[tid] = acc.y;
    threadgroup_barrier(mem_flags::mem_threadgroup);
    for (uint stride = threads >> 1; stride > 0u; stride >>= 1) {
        if (tid < stride) {
            const ulong2 s = add128(ulong2(part_lo[tid], part_hi[tid]),
                                    ulong2(part_lo[tid + stride], part_hi[tid + stride]));
            part_lo[tid] = s.x;
            part_hi[tid] = s.y;
        }
        threadgroup_barrier(mem_flags::mem_threadgroup);
    }
    if (tid == 0u) {
        const ulong2 mean = udiv128(ulong2(part_lo[0], part_hi[0]), ulong(p.n));
        const ulong mean_sq = (mean.x >> 16) | (mean.y << 48);
        inv_rms = isqrt_q16(wadd(as_type<long>(mean_sq), 1));
    }
    threadgroup_barrier(mem_flags::mem_threadgroup);
    const long inv = inv_rms;
    for (uint j = tid; j < p.n; j += threads) {
        const long norm = sar(wmul(xr[j], inv), 16);
        yr[j] = sar(wmul(norm, gamma[j]), 16);
    }
}

// split_planes, one threadgroup of 256 threads per row. Row r's planes start
// at r * 4 * stride, and its plane count goes to ctrl[r].
kernel void split_planes_rows(
    device const long *x [[buffer(0)]],
    device char *planes [[buffer(1)]],
    device uint *ctrl [[buffer(2)]],
    device atomic_uint *status [[buffer(3)]],
    constant SplitParams &p [[buffer(4)]],
    uint row [[threadgroup_position_in_grid]],
    uint tid [[thread_index_in_threadgroup]],
    uint threads [[threads_per_threadgroup]])
{
    threadgroup ulong mass[256];
    threadgroup uint top[256];
    device const long *xr = x + ulong(row) * ulong(p.n);
    device char *pr = planes + ulong(row) * 4UL * ulong(p.stride);
    ulong sum_abs = 0UL;
    uint used = 1u;
    bool bad = false;
    for (uint j = tid; j < p.stride; j += threads) {
        if (j >= p.n) {
            for (uint d = 0u; d < 4u; ++d) {
                pr[d * p.stride + j] = char(0);
            }
            continue;
        }
        const long v = xr[j];
        if (v < PLANE_MIN_V || v > PLANE_MAX_V) {
            bad = true;
        }
        ulong u = as_type<ulong>(v);
        ulong rebuilt = 0UL;
        for (uint d = 0u; d < 4u; ++d) {
            const int low = int(u & 255UL);
            const int c = (low > 127) ? (low - 256) : low;
            pr[d * p.stride + j] = char(c);
            rebuilt += as_type<ulong>(long(c)) << (8u * d);
            u = (u - as_type<ulong>(long(c))) >> 8;
            if (c != 0) {
                used = max(used, d + 1u);
            }
        }
        if (as_type<long>(rebuilt) != v) {
            bad = true;
        }
        sum_abs += magnitude(v);
    }
    if (bad) {
        atomic_fetch_or_explicit(status, ST_SPLIT_DOMAIN, memory_order_relaxed);
    }
    mass[tid] = sum_abs;
    top[tid] = used;
    threadgroup_barrier(mem_flags::mem_threadgroup);
    for (uint stride = threads >> 1; stride > 0u; stride >>= 1) {
        if (tid < stride) {
            mass[tid] += mass[tid + stride];
            top[tid] = max(top[tid], top[tid + stride]);
        }
        threadgroup_barrier(mem_flags::mem_threadgroup);
    }
    if (tid == 0u) {
        ctrl[row] = top[0];
        const ulong2 bound = mul_wide(mass[0] << 7, p.max_scale);
        if (bound.y != 0UL || bound.x > I64_MAX_U) {
            atomic_fetch_or_explicit(status, ST_SCALE_BOUND, memory_order_relaxed);
        }
    }
}

struct RopeRowsParams {
    uint n_heads;
    uint n_kv_heads;
    uint pairs; // d_head / 2
    uint d_head;
    uint pos;   // position of row 0
    uint d_kv;
    uint rows;
    uint pad0;
};

// rope_store, thread (i, head, row): row r is the token at pos + r. Its query
// is rotated in place in row r of q, and its key and value land in cache row
// pos + r.
kernel void rope_store_rows(
    device long *q [[buffer(0)]],
    device const long *k [[buffer(1)]],
    device const long *v [[buffer(2)]],
    device long *k_cache [[buffer(3)]],
    device long *v_cache [[buffer(4)]],
    device const long *cos_table [[buffer(5)]],
    device const long *sin_table [[buffer(6)]],
    constant RopeRowsParams &p [[buffer(7)]],
    uint3 gid [[thread_position_in_grid]])
{
    const uint i = gid.x;
    const uint head = gid.y;
    const uint row = gid.z;
    if (i >= p.pairs || head >= p.n_heads + p.n_kv_heads || row >= p.rows) {
        return;
    }
    const uint pos = p.pos + row;
    const ulong t = ulong(pos) * ulong(p.pairs) + ulong(i);
    const long c = cos_table[t];
    const long s = sin_table[t];
    if (head < p.n_heads) {
        const ulong base = (ulong(row) * ulong(p.n_heads) + ulong(head)) * ulong(p.d_head);
        const long x0 = q[base + i];
        const long x1 = q[base + i + p.pairs];
        q[base + i] = wsub(sar(wmul(x0, c), 16), sar(wmul(x1, s), 16));
        q[base + i + p.pairs] = wadd(sar(wmul(x0, s), 16), sar(wmul(x1, c), 16));
    } else {
        const ulong head_base = ulong(head - p.n_heads) * ulong(p.d_head);
        const ulong src = ulong(row) * ulong(p.d_kv) + head_base;
        const ulong dst = ulong(pos) * ulong(p.d_kv) + head_base;
        const long x0 = k[src + i];
        const long x1 = k[src + i + p.pairs];
        k_cache[dst + i] = wsub(sar(wmul(x0, c), 16), sar(wmul(x1, s), 16));
        k_cache[dst + i + p.pairs] = wadd(sar(wmul(x0, s), 16), sar(wmul(x1, c), 16));
        v_cache[dst + i] = v[src + i];
        v_cache[dst + i + p.pairs] = v[src + i + p.pairs];
    }
}

struct AttnRowsParams {
    uint d_head;
    uint d_kv;
    uint positions; // pos + 1: the positions row 0 attends to
    uint n_heads;
    long attn_scale;
};

// attention, one 32-lane simdgroup per (head, row). Row r attends to cache
// positions 0 .. pos + r, which hold the earlier rows of this pass (written
// by rope_store_rows before this kernel runs) and nothing after it.
kernel void attention_rows(
    device const long *q [[buffer(0)]],
    device const long *k_cache [[buffer(1)]],
    device const long *v_cache [[buffer(2)]],
    device long *out [[buffer(3)]],
    device const long *lut [[buffer(4)]],
    device const uint *head_kv [[buffer(5)]],
    constant AttnRowsParams &p [[buffer(6)]],
    uint2 tg [[threadgroup_position_in_grid]],
    uint lane [[thread_index_in_simdgroup]])
{
    const uint head = tg.x;
    const uint row = tg.y;
    const uint dh = p.d_head;
    const uint positions = p.positions + row;
    const ulong kv_base = ulong(head_kv[head]) * ulong(dh);
    const ulong q_base = (ulong(row) * ulong(p.n_heads) + ulong(head)) * ulong(dh);
    long qv[HEAD_SLOTS];
    long acc[HEAD_SLOTS];
    for (uint s = 0u; s < HEAD_SLOTS; ++s) {
        const uint dd = lane + 32u * s;
        qv[s] = (dd < dh) ? q[q_base + dd] : 0;
        acc[s] = 0;
    }
    long running_max = -4611686018427387904L; // i64::MIN / 2
    long running_sum = 0;
    for (uint j = 0u; j < positions; ++j) {
        const ulong cache_row = ulong(j) * ulong(p.d_kv) + kv_base;
        ulong partial = 0UL;
        for (uint s = 0u; s < HEAD_SLOTS; ++s) {
            const uint dd = lane + 32u * s;
            if (dd < dh) {
                partial += as_type<ulong>(qv[s]) * as_type<ulong>(k_cache[cache_row + dd]);
            }
        }
        const long dot = as_type<long>(simd_sum_u64(partial));
        const long score = sar(wmul(sar(dot, 16), p.attn_scale), 16);
        if (score > running_max) {
            const long correction = iexp(wsub(running_max, score), lut);
            running_sum = sar(wmul(running_sum, correction), 16);
            for (uint s = 0u; s < HEAD_SLOTS; ++s) {
                acc[s] = sar(wmul(acc[s], correction), 16);
            }
            running_max = score;
        }
        const long w = iexp(wsub(score, running_max), lut);
        running_sum = wadd(running_sum, w);
        for (uint s = 0u; s < HEAD_SLOTS; ++s) {
            const uint dd = lane + 32u * s;
            if (dd < dh) {
                acc[s] = wadd(acc[s], sar(wmul(w, v_cache[cache_row + dd]), 16));
            }
        }
    }
    if (running_sum > 0) {
        for (uint s = 0u; s < HEAD_SLOTS; ++s) {
            acc[s] = div_trunc(wmul(acc[s], ONE_Q16), running_sum);
        }
    }
    for (uint s = 0u; s < HEAD_SLOTS; ++s) {
        const uint dd = lane + 32u * s;
        if (dd < dh) {
            out[q_base + dd] = acc[s];
        }
    }
}

struct ExactGemmParams {
    uint rows;       // matrix rows to compute
    uint row_offset; // first matrix row
    uint blocks;     // 16-byte blocks per matrix row (and per digit plane)
    uint inputs;     // activation rows sharing each weight read, 1..8
};

// The exact projection of K activation rows (K = 1..8) with every weight
// block read once and applied to all K rows. Digits are row-major: plane d of
// activation row x is block row x * 4 + d; ctrl[x] holds row x's plane count
// and the kernel runs the largest (planes above a row's own count are zero, so
// they add nothing).
//
// Exactness. Each block product is the int dot16 of #176's kernel: at most 16
// terms of magnitude 16,384, so it is exact in 32 bits. It is scaled by 256^d
// and added into a 64-bit accumulator per (matrix row, activation row). Every
// partial sum is an integer of magnitude below 2^56 (the bound of #176's
// recombination), so the additions are exact in any order, across blocks,
// lanes and the simdgroup reduction alike; the total is the integer
// sum_d S_d * 256^d that the single-row kernel forms. The epilogue is the
// single-row one: (acc * s) >> 16, wrapping in 64 bits, with an arithmetic
// shift.
template <uint ROWS, uint MUL16, uint K>
kernel void exact_gemm_dyn(
    device const uint4 *weights [[buffer(0)]],
    device const uint4 *digits [[buffer(1)]],
    device const long *scales [[buffer(2)]],
    device long *out [[buffer(3)]],
    constant ExactGemmParams &p [[buffer(4)]],
    device const uint *ctrl [[buffer(5)]],
    uint tg_index [[threadgroup_position_in_grid]],
    uint sg_index [[simdgroup_index_in_threadgroup]],
    uint sg_count [[simdgroups_per_threadgroup]],
    uint lane [[thread_index_in_simdgroup]],
    uint sg_width [[threads_per_simdgroup]])
{
    uint planes = 1u;
    for (uint x = 0u; x < K; ++x) {
        planes = max(planes, min(ctrl[x], 4u));
    }
    const uint first = (tg_index * sg_count + sg_index) * ROWS;
    if (first >= p.rows) {
        return;
    }

    device const uint4 *row_ptr[ROWS];
    for (uint r = 0; r < ROWS; ++r) {
        const uint local = min(first + r, p.rows - 1u);
        row_ptr[r] = weights + ulong(p.row_offset + local) * ulong(p.blocks);
    }

    ulong acc[ROWS][K];
    for (uint r = 0; r < ROWS; ++r) {
        for (uint x = 0; x < K; ++x) {
            acc[r][x] = 0UL;
        }
    }

    for (uint b = lane; b < p.blocks; b += sg_width) {
        uint4 w[ROWS];
        for (uint r = 0; r < ROWS; ++r) {
            w[r] = row_ptr[r][b];
        }
        for (uint x = 0; x < K; ++x) {
            for (uint d = 0; d < 4u; ++d) {
                if (d < planes) {
                    const uint4 c = digits[(ulong(x) * 4UL + ulong(d)) * ulong(p.blocks) + b];
                    for (uint r = 0; r < ROWS; ++r) {
                        const long part = long(dot16<MUL16>(w[r], c));
                        acc[r][x] += as_type<ulong>(part) << (8u * d);
                    }
                }
            }
        }
    }

    for (uint r = 0; r < ROWS; ++r) {
        for (uint x = 0; x < K; ++x) {
            acc[r][x] = simd_sum_u64(acc[r][x]);
        }
    }

    for (uint r = 0; r < ROWS; ++r) {
        if (lane == r && first + r < p.rows) {
            const ulong scale = as_type<ulong>(scales[p.row_offset + first + r]);
            for (uint x = 0; x < K; ++x) {
                const ulong product = acc[r][x] * scale;
                ulong shifted = product >> 16;
                if (as_type<long>(product) < 0) {
                    shifted |= 0xFFFF000000000000UL;
                }
                out[ulong(x) * ulong(p.rows) + first + r] = as_type<long>(shifted);
            }
        }
    }
}

typedef decltype(exact_gemm_dyn<4, 0, 1>) exact_gemm_dyn_t;

#define EXACT_GEMM_DYN(M, K)                                                    \
    template [[host_name("exact_gemm_dyn_m" #M "_k" #K)]]                      \
    kernel exact_gemm_dyn_t exact_gemm_dyn<4, M, K>;

EXACT_GEMM_DYN(0, 1)
EXACT_GEMM_DYN(0, 2)
EXACT_GEMM_DYN(0, 3)
EXACT_GEMM_DYN(0, 4)
EXACT_GEMM_DYN(0, 5)
EXACT_GEMM_DYN(0, 6)
EXACT_GEMM_DYN(0, 7)
EXACT_GEMM_DYN(0, 8)
EXACT_GEMM_DYN(1, 1)
EXACT_GEMM_DYN(1, 2)
EXACT_GEMM_DYN(1, 3)
EXACT_GEMM_DYN(1, 4)
EXACT_GEMM_DYN(1, 5)
EXACT_GEMM_DYN(1, 6)
EXACT_GEMM_DYN(1, 7)
EXACT_GEMM_DYN(1, 8)
