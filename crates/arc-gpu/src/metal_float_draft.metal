// NON-CANONICAL: ARC's per-row INT8 projection with the dot product formed in
// f32, for a speculative-decoding drafter. A drafter only proposes tokens;
// ARC's exact engine verifies every one, so nothing computed here is ever an
// ARC output. The exact kernels are in metal_exact_gemv.metal and
// metal_exact_decoder.metal; this file is compiled as its own library.
//
//   y[i] = (round(dot_f32(w[i], x)) * s[i]) >> 16
//
// dot_f32 is the order of the CPU study (`dot_f32` in
// crates/arc-inference/src/float_accumulation_study.rs), reproduced operation
// for operation, so a projection here equals the study's bit for bit:
//
//  1. Operands: every INT8 weight (exact in f32) and every activation, the
//     i64 Q16 value rounded to f32 to nearest, ties to even (Rust's `as f32`).
//  2. Sixteen chains. Chain l accumulates columns l, l + 16, l + 32, ... of
//     the whole 16-column blocks, in increasing order, starting from +0, one
//     fused multiply-add (a single rounding) per column.
//  3. acc = +0, then acc += chain 0, chain 1, ..., chain 15, in that order.
//  4. Columns after the last whole block, in order: one fused multiply-add
//     each into acc.
//  5. round(acc), halfway cases away from zero; to i64 as Rust's saturating
//     `as i64`; then the exact engine's requantisation: the product with the
//     row's scale, wrapping in 64 bits, and an arithmetic shift right by 16.
//
// Each step is one IEEE-754 single-precision operation rounded to nearest
// even (fma, +) or is exact (the INT8 conversion, round, the integer
// epilogue), so the same inputs give the same bits on any device that
// implements them as specified. The library is compiled with fast math off:
// no reassociation, no contraction, no approximate functions. The i64 <-> f32
// conversions are written with integer operations rather than left to the
// compiler. There is no half-precision type in this file.
//
// A row's sixteen chains run on 1, 4 or 16 threads (consecutive lanes of one
// simdgroup). That changes which thread runs a chain, never the order of its
// operations, so the three kernels give the same bits. As in the exact
// kernels, there is no integer division or modulo (#150 found a miscompiled
// one on an M2 Ultra): indices use shifts and masks.

#include <metal_stdlib>
using namespace metal;

struct DraftGemvParams {
    uint rows;    // matrix rows
    uint cols;    // columns per row, without the padding
    uint blocks;  // the row stride in 16-byte blocks
    uint mode;    // 0: (round(dot) * s) >> 16; 1: round(dot) itself
};

struct DraftConvertParams {
    uint n;
    uint pad0;
    uint pad1;
    uint pad2;
};

// Rust's `v as f32` for an i64: round to nearest, ties to even.
static inline float f32_from_i64(long v) {
    const ulong bits = as_type<ulong>(v);
    const bool negative = (bits >> 63) != 0UL;
    // |v|; i64::MIN gives 2^63, which ulong holds.
    const ulong magnitude = negative ? (0UL - bits) : bits;
    if (magnitude == 0UL) {
        return 0.0f;
    }
    const uint high = uint(magnitude >> 32);
    const uint low = uint(magnitude & 0xFFFFFFFFUL);
    const uint leading = high != 0u ? clz(high) : 32u + clz(low);
    const uint top = 63u - leading;  // the leading one's bit
    uint exponent = top + 127u;
    uint significand;  // 24 bits, the leading one at bit 23
    if (top <= 23u) {
        significand = uint(magnitude) << (23u - top);
    } else {
        const uint shift = top - 23u;  // 1 to 40
        ulong kept = magnitude >> shift;
        const ulong rest = magnitude & ((1UL << shift) - 1UL);
        const ulong half_way = 1UL << (shift - 1u);
        if (rest > half_way || (rest == half_way && (kept & 1UL) != 0UL)) {
            kept += 1UL;
            if (kept == (1UL << 24)) {
                kept >>= 1;
                exponent += 1u;
            }
        }
        significand = uint(kept);
    }
    const uint sign = negative ? 0x80000000u : 0u;
    return as_type<float>(sign | (exponent << 23) | (significand & 0x007FFFFFu));
}

// Rust's `f as i64` for an integral f: saturating, NaN to 0.
static inline long i64_from_f32(float f) {
    const uint bits = as_type<uint>(f);
    const uint biased = (bits >> 23) & 0xFFu;
    const uint fraction = bits & 0x007FFFFFu;
    const bool negative = (bits >> 31) != 0u;
    if (biased == 0xFFu && fraction != 0u) {
        return 0L;  // NaN
    }
    if (biased >= 127u + 63u) {  // |f| >= 2^63, or infinite
        return negative ? as_type<long>(0x8000000000000000UL)
                        : as_type<long>(0x7FFFFFFFFFFFFFFFUL);
    }
    if (biased < 127u) {
        return 0L;  // |f| < 1
    }
    const uint e = biased - 127u;  // 0 to 62
    const ulong significand = ulong(fraction | 0x00800000u);
    const ulong magnitude = e >= 23u ? (significand << (e - 23u)) : (significand >> (23u - e));
    return negative ? as_type<long>(0UL - magnitude) : as_type<long>(magnitude);
}

// The exact engine's epilogue: (dot * s) wrapping in 64 bits, then the
// arithmetic shift right by 16 (a logical shift with the sign filled in).
static inline long requantize(long dot, long scale) {
    const ulong product = as_type<ulong>(dot) * as_type<ulong>(scale);
    ulong shifted = product >> 16;
    if (as_type<long>(product) < 0L) {
        shifted |= 0xFFFF000000000000UL;
    }
    return as_type<long>(shifted);
}

// Steps 4 and 5 for row r, after the ordered sum of its chains.
static inline void finish_row(
    device const uchar *weights,
    device const float *x,
    device const long *scales,
    device long *out,
    constant DraftGemvParams &p,
    uint r,
    float acc)
{
    device const char *w = (device const char *)(weights + ulong(r) * ulong(p.blocks) * 16UL);
    for (uint j = (p.cols >> 4) << 4; j < p.cols; ++j) {
        acc = fma(float(w[j]), x[j], acc);
    }
    const long dot = i64_from_f32(round(acc));
    out[r] = p.mode == 0u ? requantize(dot, scales[r]) : dot;
}

// Step 1 for one activation vector.
kernel void draft_to_f32(
    device const long *x [[buffer(0)]],
    device float *y [[buffer(1)]],
    constant DraftConvertParams &p [[buffer(2)]],
    uint gid [[thread_position_in_grid]])
{
    if (gid < p.n) {
        y[gid] = f32_from_i64(x[gid]);
    }
}

// One thread per row: all sixteen chains in registers, 16-byte loads.
kernel void draft_gemv_t1(
    device const uchar *weights [[buffer(0)]],
    device const float *x [[buffer(1)]],
    device const long *scales [[buffer(2)]],
    device long *out [[buffer(3)]],
    constant DraftGemvParams &p [[buffer(4)]],
    uint gid [[thread_position_in_grid]])
{
    if (gid >= p.rows) {
        return;
    }
    const uint r = gid;
    device const uint4 *w = (device const uint4 *)(weights) + ulong(r) * ulong(p.blocks);
    device const float4 *x4 = (device const float4 *)(x);
    const uint whole = p.cols >> 4;
    // Chains 0-3, 4-7, 8-11 and 12-15.
    float4 c0 = float4(0.0f);
    float4 c1 = float4(0.0f);
    float4 c2 = float4(0.0f);
    float4 c3 = float4(0.0f);
    uint b = 0u;
    for (; b + 2u <= whole; b += 2u) {
        const uint4 v = w[b];
        const uint4 u = w[b + 1u];
        const float4 a0 = x4[4u * b];
        const float4 a1 = x4[4u * b + 1u];
        const float4 a2 = x4[4u * b + 2u];
        const float4 a3 = x4[4u * b + 3u];
        const float4 e0 = x4[4u * b + 4u];
        const float4 e1 = x4[4u * b + 5u];
        const float4 e2 = x4[4u * b + 6u];
        const float4 e3 = x4[4u * b + 7u];
        c0 = fma(float4(as_type<char4>(v.x)), a0, c0);
        c1 = fma(float4(as_type<char4>(v.y)), a1, c1);
        c2 = fma(float4(as_type<char4>(v.z)), a2, c2);
        c3 = fma(float4(as_type<char4>(v.w)), a3, c3);
        c0 = fma(float4(as_type<char4>(u.x)), e0, c0);
        c1 = fma(float4(as_type<char4>(u.y)), e1, c1);
        c2 = fma(float4(as_type<char4>(u.z)), e2, c2);
        c3 = fma(float4(as_type<char4>(u.w)), e3, c3);
    }
    for (; b < whole; ++b) {
        const uint4 v = w[b];
        c0 = fma(float4(as_type<char4>(v.x)), x4[4u * b], c0);
        c1 = fma(float4(as_type<char4>(v.y)), x4[4u * b + 1u], c1);
        c2 = fma(float4(as_type<char4>(v.z)), x4[4u * b + 2u], c2);
        c3 = fma(float4(as_type<char4>(v.w)), x4[4u * b + 3u], c3);
    }
    float acc = 0.0f;
    acc += c0.x;
    acc += c0.y;
    acc += c0.z;
    acc += c0.w;
    acc += c1.x;
    acc += c1.y;
    acc += c1.z;
    acc += c1.w;
    acc += c2.x;
    acc += c2.y;
    acc += c2.z;
    acc += c2.w;
    acc += c3.x;
    acc += c3.y;
    acc += c3.z;
    acc += c3.w;
    finish_row(weights, x, scales, out, p, r, acc);
}

// Four threads per row: thread q of the row runs chains 4q to 4q + 3, with
// 4-byte loads. Rows past the end compute the last row and never store, so
// every lane of a simdgroup reaches the shuffles.
kernel void draft_gemv_t4(
    device const uchar *weights [[buffer(0)]],
    device const float *x [[buffer(1)]],
    device const long *scales [[buffer(2)]],
    device long *out [[buffer(3)]],
    constant DraftGemvParams &p [[buffer(4)]],
    uint gid [[thread_position_in_grid]],
    uint lane [[thread_index_in_simdgroup]])
{
    const uint row = gid >> 2;
    const uint part = gid & 3u;
    const uint r = min(row, p.rows - 1u);
    device const uint *w = (device const uint *)(weights) + ulong(r) * ulong(p.blocks) * 4UL;
    device const float4 *x4 = (device const float4 *)(x);
    const uint whole = p.cols >> 4;
    float4 c = float4(0.0f);
    uint b = 0u;
    for (; b + 4u <= whole; b += 4u) {
        const uint o = 4u * b + part;
        const uint v0 = w[o];
        const uint v1 = w[o + 4u];
        const uint v2 = w[o + 8u];
        const uint v3 = w[o + 12u];
        const float4 a0 = x4[o];
        const float4 a1 = x4[o + 4u];
        const float4 a2 = x4[o + 8u];
        const float4 a3 = x4[o + 12u];
        c = fma(float4(as_type<char4>(v0)), a0, c);
        c = fma(float4(as_type<char4>(v1)), a1, c);
        c = fma(float4(as_type<char4>(v2)), a2, c);
        c = fma(float4(as_type<char4>(v3)), a3, c);
    }
    for (; b < whole; ++b) {
        const uint o = 4u * b + part;
        c = fma(float4(as_type<char4>(w[o])), x4[o], c);
    }
    // The row's chains in order, on every lane; its first thread stores.
    const ushort lead = ushort(lane & ~3u);
    float acc = 0.0f;
    for (ushort q = 0; q < 4; ++q) {
        const ushort from = ushort(lead + q);
        acc += simd_shuffle(c.x, from);
        acc += simd_shuffle(c.y, from);
        acc += simd_shuffle(c.z, from);
        acc += simd_shuffle(c.w, from);
    }
    if (part == 0u && row < p.rows) {
        finish_row(weights, x, scales, out, p, r, acc);
    }
}

// Sixteen threads per row: thread l of the row runs chain l, with byte loads
// that the row's sixteen threads make contiguous.
kernel void draft_gemv_t16(
    device const uchar *weights [[buffer(0)]],
    device const float *x [[buffer(1)]],
    device const long *scales [[buffer(2)]],
    device long *out [[buffer(3)]],
    constant DraftGemvParams &p [[buffer(4)]],
    uint gid [[thread_position_in_grid]],
    uint lane [[thread_index_in_simdgroup]])
{
    const uint row = gid >> 4;
    const uint part = gid & 15u;
    const uint r = min(row, p.rows - 1u);
    device const char *w = (device const char *)(weights + ulong(r) * ulong(p.blocks) * 16UL);
    const uint whole = p.cols >> 4;
    float c = 0.0f;
    uint b = 0u;
    for (; b + 4u <= whole; b += 4u) {
        const uint o = 16u * b + part;
        const float w0 = float(w[o]);
        const float w1 = float(w[o + 16u]);
        const float w2 = float(w[o + 32u]);
        const float w3 = float(w[o + 48u]);
        const float a0 = x[o];
        const float a1 = x[o + 16u];
        const float a2 = x[o + 32u];
        const float a3 = x[o + 48u];
        c = fma(w0, a0, c);
        c = fma(w1, a1, c);
        c = fma(w2, a2, c);
        c = fma(w3, a3, c);
    }
    for (; b < whole; ++b) {
        const uint o = 16u * b + part;
        c = fma(float(w[o]), x[o], c);
    }
    const ushort lead = ushort(lane & ~15u);
    float acc = 0.0f;
    for (ushort l = 0; l < 16; ++l) {
        acc += simd_shuffle(c, ushort(lead + l));
    }
    if (part == 0u && row < p.rows) {
        finish_row(weights, x, scales, out, p, r, acc);
    }
}
