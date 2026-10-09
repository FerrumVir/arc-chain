// Exact canonical per-row INT8 GEMV for ARC's integer engine (decode).
//
//   y[i] = ((sum_j w[i][j] * x[j]) * s[i]) >> 16
//
// w is INT8, x is the full-precision i64 Q16 activation, s is the i64 per-row
// Q16 scale and >> is the arithmetic (floor) shift of the integer profile
// contract (docs/protocol/integer-profile-contract-v1.md, sections 2 and 3.4).
// Every output is byte-identical to the CPU scalar kernel (dot_i8_i64 and
// matmul_i8_view_into in crates/arc-inference/src/cached_integer_model.rs).
//
// The only types used are char, short, int, uint, long and ulong. There is no
// floating point, no division, no modulo and no signed overflow anywhere in
// this file. The host (metal_exact.rs) runs before every dispatch and has:
//
//  * refused any row length above 131,071 columns;
//  * refused any activation outside [-2,155,905,152, 2,139,062,143] and
//    written every other one as four balanced base-256 digits,
//    x[j] = sum_{d<4} c[d][j] * 256^d with c in [-128, 127], checking the
//    reconstruction of each element (the decomposition of
//    canonical_simd::split_limbs, digit for digit);
//  * refused the canonical epilogue unless 128 * sum_j |x[j]| * |s[i]| fits in
//    i64 for every row.
//
// Why the result is exact:
//
//  1. Digit sums. S[d][i] = sum_j w[i][j] * c[d][j] is accumulated in int.
//     |w * c| <= 128 * 128 = 16,384 and a row has at most 131,071 non-zero
//     terms (zero padding contributes 0), so the sum of ANY subset of the
//     terms is at most 16,384 * 131,071 = 2,147,467,264 < 2^31 - 1. Every
//     partial sum a lane, a simd_sum tree or a tile shape can form is such a
//     subset sum, so no order of addition can overflow or change S.
//  2. Recombination. acc[i] = sum_d S[d][i] * 256^d is formed in ulong, where
//     wrapping is defined. |acc| <= 2^31 * (1 + 2^8 + 2^16 + 2^24) < 2^56, so
//     the wrapped value is the exact value.
//  3. Scale. acc * s[i] is formed in ulong. The host bound makes the true
//     product fit in i64, so the wrapped product read as long is exact.
//  4. Floor shift. A logical shift by 16 followed by filling the top 16 bits
//     with the sign is exactly the arithmetic shift i64 >> 16 the CPU uses.
//
// Unused high digit planes are all zero; the host picks the PLANES variant
// that covers the highest non-zero digit, which changes the work, never the
// value.

#include <metal_stdlib>
using namespace metal;

struct ExactGemvParams {
    uint rows;        // output rows in this dispatch
    uint row_offset;  // matrix row of output row 0
    uint blocks;      // 16-byte column blocks per row, i.e. the row stride in uint4
    uint mode;        // 0: canonical (acc * s) >> 16; 1: the exact dot acc itself
};

// Four signed bytes times four signed bytes. Each product is at most 2^14 in
// magnitude and the sum at most 2^16.
static inline int dot4_i32(uint a, uint b) {
    const int4 x = int4(as_type<char4>(a));
    const int4 y = int4(as_type<char4>(b));
    const int4 p = x * y;
    return p.x + p.y + p.z + p.w;
}

// The same value with 16-bit products: |p| <= 16,384 <= 32,767, so every
// short product is exact. Vector operands are not promoted, so the multiply
// is a genuine 16-bit one; the sum is formed in int.
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

// One simdgroup computes ROWS consecutive output rows. Its lanes stride along
// the row in 16-byte blocks (coalesced loads), and every weight block that a
// lane loads is applied to all PLANES digit planes before the next load. The
// digit blocks are loaded once per block and reused by all ROWS rows.
template <uint PLANES, uint ROWS, uint MUL16>
kernel void exact_gemv(
    device const uint4 *weights [[buffer(0)]],
    device const uint4 *digits [[buffer(1)]],
    device const long *scales [[buffer(2)]],
    device long *out [[buffer(3)]],
    constant ExactGemvParams &p [[buffer(4)]],
    uint tg_index [[threadgroup_position_in_grid]],
    uint sg_index [[simdgroup_index_in_threadgroup]],
    uint sg_count [[simdgroups_per_threadgroup]],
    uint lane [[thread_index_in_simdgroup]],
    uint sg_width [[threads_per_simdgroup]])
{
    const uint first = (tg_index * sg_count + sg_index) * ROWS;
    // Uniform across the simdgroup, so simd_sum below always sees full groups.
    if (first >= p.rows) {
        return;
    }

    // Rows past the end of the dispatch load the last valid row (in bounds)
    // and never store.
    device const uint4 *row_ptr[ROWS];
    for (uint r = 0; r < ROWS; ++r) {
        const uint local = min(first + r, p.rows - 1u);
        row_ptr[r] = weights + ulong(p.row_offset + local) * ulong(p.blocks);
    }

    int acc[ROWS][PLANES];
    for (uint r = 0; r < ROWS; ++r) {
        for (uint d = 0; d < PLANES; ++d) {
            acc[r][d] = 0;
        }
    }

    for (uint b = lane; b < p.blocks; b += sg_width) {
        uint4 c[PLANES];
        for (uint d = 0; d < PLANES; ++d) {
            c[d] = digits[d * p.blocks + b];
        }
        for (uint r = 0; r < ROWS; ++r) {
            const uint4 w = row_ptr[r][b];
            for (uint d = 0; d < PLANES; ++d) {
                acc[r][d] += dot16<MUL16>(w, c[d]);
            }
        }
    }

    // Integer simd_sum is exact: every partial sum it forms is a subset sum.
    for (uint r = 0; r < ROWS; ++r) {
        for (uint d = 0; d < PLANES; ++d) {
            acc[r][d] = simd_sum(acc[r][d]);
        }
    }

    for (uint r = 0; r < ROWS; ++r) {
        if (lane == r && first + r < p.rows) {
            ulong total = 0;
            for (uint d = 0; d < PLANES; ++d) {
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

// Explicit instantiations, one pipeline each (the pattern ggml-metal uses).
typedef decltype(exact_gemv<1, 1, 0>) exact_gemv_t;

#define EXACT_GEMV(P, R, M)                                                     \
    template [[host_name("exact_gemv_p" #P "_r" #R "_m" #M)]]                  \
    kernel exact_gemv_t exact_gemv<P, R, M>;

#define EXACT_GEMV_ROWS(P, M)                                                   \
    EXACT_GEMV(P, 1, M)                                                         \
    EXACT_GEMV(P, 2, M)                                                         \
    EXACT_GEMV(P, 4, M)                                                         \
    EXACT_GEMV(P, 8, M)

#define EXACT_GEMV_PLANES(M)                                                    \
    EXACT_GEMV_ROWS(1, M)                                                       \
    EXACT_GEMV_ROWS(2, M)                                                       \
    EXACT_GEMV_ROWS(3, M)                                                       \
    EXACT_GEMV_ROWS(4, M)

EXACT_GEMV_PLANES(0)
EXACT_GEMV_PLANES(1)

// Read-bandwidth probe for the benchmark: every thread XOR-folds a strided
// share of the buffer, so every byte is loaded exactly once. Four independent
// loads per iteration keep enough requests in flight to approach the
// device's read bandwidth.
kernel void read_bandwidth(
    device const uint4 *src [[buffer(0)]],
    device uint *sink [[buffer(1)]],
    constant uint &count [[buffer(2)]],
    uint gid [[thread_position_in_grid]],
    uint threads [[threads_per_grid]])
{
    uint4 f0 = uint4(0u);
    uint4 f1 = uint4(0u);
    uint4 f2 = uint4(0u);
    uint4 f3 = uint4(0u);
    uint i = gid;
    for (; i + 3u * threads < count; i += 4u * threads) {
        f0 ^= src[i];
        f1 ^= src[i + threads];
        f2 ^= src[i + 2u * threads];
        f3 ^= src[i + 3u * threads];
    }
    for (; i < count; i += threads) {
        f0 ^= src[i];
    }
    const uint4 fold = f0 ^ f1 ^ f2 ^ f3;
    sink[gid] = fold.x ^ fold.y ^ fold.z ^ fold.w;
}
