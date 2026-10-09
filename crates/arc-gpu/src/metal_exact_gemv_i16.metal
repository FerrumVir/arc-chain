// Exact INT16 GEMV for the MLA + MoE profile's INT16 projection (decode).
//
//   d[i] = sum_j w[i][j] * x[j]
//
// w is INT16 (little endian, every value in [-32,767, 32,767]) and x is the
// i64 Q16 activation. The kernel writes the exact row dot d[i]. The profile's
// epilogue floor(d * mu / 2^k) needs a 95-bit product and Metal has no
// 128-bit integer type, so the host applies it on the CPU with the i128
// function the CPU projection itself uses (arith::dyadic_epilogue). Every dot
// is byte-identical to the CPU kernel (precision::project_i16 in
// crates/arc-inference/src/modern/mla/precision.rs).
//
// The only types used are char, uchar, short, int, uint, long and ulong.
// There is no floating point, no division, no modulo and no signed overflow
// anywhere in this file. Before every dispatch the host (metal_exact_i16.rs)
// has:
//
//  * admitted the weights once, at upload: an odd byte count or any -32,768
//    is refused, so |w| <= 32,767 (the CPU's I16Weights rule);
//  * refused any input with sum_j |x[j]| >= floor(2^63 / 32,767) =
//    281,483,566,907,400 (the CPU's accumulator guard, the same expression),
//    so the true dot satisfies |d| <= 32,767 * 281,483,566,907,399 =
//    2^63 - 32,775;
//  * written every activation as seven balanced base-256 digits,
//    x[j] = sum_{p<7} c[p][j] * 256^p with c in [-128, 127], checking the
//    reconstruction of each element. Seven digits cover
//    [-36,170,086,419,038,336, 35,887,507,618,889,599], which holds every
//    activation the guard admits, so the split never refuses a guarded input.
//
// Why the result is exact:
//
//  1. Block sums. A 16-byte block holds 8 weights of a row. Its sum against
//     one digit plane, sum_{8 columns} w * c, is formed in int: |w * c| <=
//     32,767 * 128 = 4,194,176, so every partial sum of a block is at most
//     33,553,408 < 2^25 in magnitude. The 16-bit variant (MUL16 = 1) writes
//     w = 256 h + l, with h the signed high byte and l the unsigned low byte
//     of w's two's-complement bits: |h * c| <= 16,384 and l * c lies in
//     [-32,640, 32,385], so both 16-bit products are exact, and
//     256 * sum(h * c) + sum(l * c) is the same block sum.
//  2. Lane sums. A lane adds at most I16_FLUSH = 64 block sums per plane in
//     int before it flushes them: 64 * 33,553,408 = 2,147,418,112 <=
//     2^31 - 1, so no order of addition can overflow.
//  3. Recombination. At each flush the lane adds part[p] * 256^p, for every
//     plane p, to a ulong total, where wrapping is defined. Since x[j] is
//     exactly sum_p c[p][j] * 256^p, the lane's total is its share of
//     sum_j w[j] * x[j], modulo 2^64.
//  4. Reduction. Each lane's total is cut into four 16-bit pieces in
//     [0, 65,535]. simd_sum adds one piece over the simdgroup's lanes (the
//     host requires at most 1,024), so every sum it forms is below 2^27 and
//     exact, and the pieces are put back together modulo 2^64. The result is
//     d modulo 2^64.
//  5. Range. |d| < 2^63 by the guard, so d modulo 2^64 read as a long is d.
//
// Rows are padded to whole blocks with zero weights and the digit planes with
// zero digits, which add nothing. Unused high digit planes are all zero; the
// host picks the PLANES variant that covers the highest non-zero digit, which
// changes the work, never the value. Integer addition is associative, so tile
// shapes, lane counts and simd_sum order cannot change a value either.

#include <metal_stdlib>
using namespace metal;

struct ExactI16Params {
    uint rows;        // output rows in this dispatch
    uint row_offset;  // matrix row of output row 0
    uint blocks;      // 16-byte weight blocks per row; each digit plane has as many 8-byte blocks
    uint reserved;    // zero
};

// Loop iterations of a lane between two flushes of its int partial sums.
constant uint I16_FLUSH = 64;

// Eight weights (one 16-byte block) times their eight digits (one 8-byte
// block of a digit plane), with 32-bit products. Little-endian layout: w.xy
// holds columns 0..3 and c.x their digits; w.zw holds columns 4..7 and c.y.
static inline int dot8_m32(uint4 w, uint2 c) {
    const int4 p = int4(as_type<short4>(w.xy)) * int4(as_type<char4>(c.x))
                 + int4(as_type<short4>(w.zw)) * int4(as_type<char4>(c.y));
    return p.x + p.y + p.z + p.w;
}

// Four weights (two uints) times four digits (one uint) with 16-bit products.
// Each uint holds two weights as bytes low0 high0 low1 high1.
static inline int dot4_m16(uint2 w, uint c) {
    const char4 s0 = as_type<char4>(w.x);
    const char4 s1 = as_type<char4>(w.y);
    const uchar4 u0 = as_type<uchar4>(w.x);
    const uchar4 u1 = as_type<uchar4>(w.y);
    const short4 digit = short4(as_type<char4>(c));
    // Vector operands are not promoted, so these are genuine 16-bit products.
    const short4 high = short4(short(s0.y), short(s0.w), short(s1.y), short(s1.w)) * digit;
    const short4 low = short4(short(u0.x), short(u0.z), short(u1.x), short(u1.z)) * digit;
    return 256 * (int(high.x) + int(high.y) + int(high.z) + int(high.w))
         + (int(low.x) + int(low.y) + int(low.z) + int(low.w));
}

template <uint MUL16>
static inline int dot8(uint4 w, uint2 c) {
    if (MUL16 != 0) {
        return dot4_m16(w.xy, c.x) + dot4_m16(w.zw, c.y);
    }
    return dot8_m32(w, c);
}

// One simdgroup computes ROWS consecutive output rows. Its lanes stride along
// the row in 16-byte blocks (coalesced loads); every weight block a lane
// loads is applied to all PLANES digit planes before the next load, and the
// digit blocks are loaded once per block and reused by all ROWS rows.
template <uint PLANES, uint ROWS, uint MUL16>
kernel void exact_gemv_i16(
    device const uint4 *weights [[buffer(0)]],
    device const uint2 *digits [[buffer(1)]],
    device long *out [[buffer(2)]],
    constant ExactI16Params &p [[buffer(3)]],
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

    ulong total[ROWS];
    for (uint r = 0; r < ROWS; ++r) {
        total[r] = 0;
    }

    // The host bounds blocks so that neither the plane offsets below nor
    // start + span can leave uint.
    const uint span = I16_FLUSH * sg_width;
    for (uint start = lane; start < p.blocks; start += span) {
        // The loop below runs at most I16_FLUSH times.
        const uint stop = start + min(p.blocks - start, span);
        int part[ROWS][PLANES];
        for (uint r = 0; r < ROWS; ++r) {
            for (uint d = 0; d < PLANES; ++d) {
                part[r][d] = 0;
            }
        }
        for (uint b = start; b < stop; b += sg_width) {
            uint2 c[PLANES];
            for (uint d = 0; d < PLANES; ++d) {
                c[d] = digits[d * p.blocks + b];
            }
            for (uint r = 0; r < ROWS; ++r) {
                const uint4 w = row_ptr[r][b];
                for (uint d = 0; d < PLANES; ++d) {
                    part[r][d] += dot8<MUL16>(w, c[d]);
                }
            }
        }
        for (uint r = 0; r < ROWS; ++r) {
            for (uint d = 0; d < PLANES; ++d) {
                total[r] += as_type<ulong>(long(part[r][d])) << (8u * d);
            }
        }
    }

    // Sum total[r] over the lanes modulo 2^64, in four exact 16-bit pieces.
    for (uint r = 0; r < ROWS; ++r) {
        ulong sum = 0;
        for (uint piece = 0; piece < 4u; ++piece) {
            const int bits = int((total[r] >> (16u * piece)) & 0xFFFFUL);
            sum += ulong(uint(simd_sum(bits))) << (16u * piece);
        }
        total[r] = sum;
    }

    for (uint r = 0; r < ROWS; ++r) {
        if (lane == r && first + r < p.rows) {
            out[first + r] = as_type<long>(total[r]);
        }
    }
}

// Explicit instantiations, one pipeline each (the pattern ggml-metal uses).
typedef decltype(exact_gemv_i16<1, 1, 0>) exact_gemv_i16_t;

#define EXACT_GEMV_I16(P, R, M)                                                 \
    template [[host_name("exact_gemv_i16_p" #P "_r" #R "_m" #M)]]              \
    kernel exact_gemv_i16_t exact_gemv_i16<P, R, M>;

#define EXACT_GEMV_I16_ROWS(P, M)                                               \
    EXACT_GEMV_I16(P, 1, M)                                                     \
    EXACT_GEMV_I16(P, 2, M)                                                     \
    EXACT_GEMV_I16(P, 4, M)                                                     \
    EXACT_GEMV_I16(P, 8, M)

#define EXACT_GEMV_I16_PLANES(M)                                                \
    EXACT_GEMV_I16_ROWS(1, M)                                                   \
    EXACT_GEMV_I16_ROWS(2, M)                                                   \
    EXACT_GEMV_I16_ROWS(3, M)                                                   \
    EXACT_GEMV_I16_ROWS(4, M)                                                   \
    EXACT_GEMV_I16_ROWS(5, M)                                                   \
    EXACT_GEMV_I16_ROWS(6, M)                                                   \
    EXACT_GEMV_I16_ROWS(7, M)

EXACT_GEMV_I16_PLANES(0)
EXACT_GEMV_I16_PLANES(1)

// ---------------------------------------------------------------------------
// Per-head projections in one dispatch (MLA's per-head key and value stacks).
//
// A stack of `heads` matrices of `rows` x `cols`, stored one after another
// (head h's row i is matrix row row_offset + h * rows + i), each projected
// with its own activation vector. The grid's y coordinate is the head. Every
// output row is computed by exactly the steps of exact_gemv_i16 above, so
// the proof at the top of this file applies to it row by row: the same
// weights, the same per-row loop, the same int block and lane sums, the same
// ulong recombination and 16-bit reduction. Only two things depend on the
// head: which digit planes the row reads (its own head's, written by the
// host from that head's vector, which passed the guard on its own) and which
// output slot it writes. The host refuses a stack whose rows do not fit the
// matrix, and the offsets below are formed in ulong.

struct ExactI16HeadsParams {
    uint rows;        // output rows of each head
    uint row_offset;  // matrix row of head 0's first row
    uint blocks;      // 16-byte weight blocks per row; 8-byte digit blocks per plane
    uint heads;       // heads in this dispatch (the grid's y size)
};

// Digit planes per head in the digit buffer: every head's vector is written
// as all seven planes, so head h's planes start at h * 7 * blocks blocks.
constant uint I16_HEAD_PLANES = 7;

template <uint PLANES, uint ROWS, uint MUL16>
kernel void exact_gemv_i16_heads(
    device const uint4 *weights [[buffer(0)]],
    device const uint2 *digits [[buffer(1)]],
    device long *out [[buffer(2)]],
    constant ExactI16HeadsParams &p [[buffer(3)]],
    uint2 tg_index [[threadgroup_position_in_grid]],
    uint sg_index [[simdgroup_index_in_threadgroup]],
    uint sg_count [[simdgroups_per_threadgroup]],
    uint lane [[thread_index_in_simdgroup]],
    uint sg_width [[threads_per_simdgroup]])
{
    const uint head = tg_index.y;
    const uint first = (tg_index.x * sg_count + sg_index) * ROWS;
    // Uniform across the simdgroup, so simd_sum below always sees full groups.
    if (head >= p.heads || first >= p.rows) {
        return;
    }
    device const uint2 *head_digits =
        digits + ulong(head) * ulong(I16_HEAD_PLANES) * ulong(p.blocks);
    const ulong head_row = ulong(p.row_offset) + ulong(head) * ulong(p.rows);

    // Rows past the end of the head load the head's last row (in bounds)
    // and never store.
    device const uint4 *row_ptr[ROWS];
    for (uint r = 0; r < ROWS; ++r) {
        const uint local = min(first + r, p.rows - 1u);
        row_ptr[r] = weights + (head_row + ulong(local)) * ulong(p.blocks);
    }

    ulong total[ROWS];
    for (uint r = 0; r < ROWS; ++r) {
        total[r] = 0;
    }

    const uint span = I16_FLUSH * sg_width;
    for (uint start = lane; start < p.blocks; start += span) {
        // The loop below runs at most I16_FLUSH times.
        const uint stop = start + min(p.blocks - start, span);
        int part[ROWS][PLANES];
        for (uint r = 0; r < ROWS; ++r) {
            for (uint d = 0; d < PLANES; ++d) {
                part[r][d] = 0;
            }
        }
        for (uint b = start; b < stop; b += sg_width) {
            uint2 c[PLANES];
            for (uint d = 0; d < PLANES; ++d) {
                c[d] = head_digits[d * p.blocks + b];
            }
            for (uint r = 0; r < ROWS; ++r) {
                const uint4 w = row_ptr[r][b];
                for (uint d = 0; d < PLANES; ++d) {
                    part[r][d] += dot8<MUL16>(w, c[d]);
                }
            }
        }
        for (uint r = 0; r < ROWS; ++r) {
            for (uint d = 0; d < PLANES; ++d) {
                total[r] += as_type<ulong>(long(part[r][d])) << (8u * d);
            }
        }
    }

    // Sum total[r] over the lanes modulo 2^64, in four exact 16-bit pieces.
    for (uint r = 0; r < ROWS; ++r) {
        ulong sum = 0;
        for (uint piece = 0; piece < 4u; ++piece) {
            const int bits = int((total[r] >> (16u * piece)) & 0xFFFFUL);
            sum += ulong(uint(simd_sum(bits))) << (16u * piece);
        }
        total[r] = sum;
    }

    for (uint r = 0; r < ROWS; ++r) {
        if (lane == r && first + r < p.rows) {
            out[ulong(head) * ulong(p.rows) + ulong(first + r)] = as_type<long>(total[r]);
        }
    }
}

typedef decltype(exact_gemv_i16_heads<1, 1, 0>) exact_gemv_i16_heads_t;

#define EXACT_GEMV_I16_HEADS(P, R, M)                                           \
    template [[host_name("exact_gemv_i16_heads_p" #P "_r" #R "_m" #M)]]        \
    kernel exact_gemv_i16_heads_t exact_gemv_i16_heads<P, R, M>;

#define EXACT_GEMV_I16_HEADS_ROWS(P, M)                                         \
    EXACT_GEMV_I16_HEADS(P, 1, M)                                               \
    EXACT_GEMV_I16_HEADS(P, 2, M)                                               \
    EXACT_GEMV_I16_HEADS(P, 4, M)                                               \
    EXACT_GEMV_I16_HEADS(P, 8, M)

#define EXACT_GEMV_I16_HEADS_PLANES(M)                                          \
    EXACT_GEMV_I16_HEADS_ROWS(1, M)                                             \
    EXACT_GEMV_I16_HEADS_ROWS(2, M)                                             \
    EXACT_GEMV_I16_HEADS_ROWS(3, M)                                             \
    EXACT_GEMV_I16_HEADS_ROWS(4, M)                                             \
    EXACT_GEMV_I16_HEADS_ROWS(5, M)                                             \
    EXACT_GEMV_I16_HEADS_ROWS(6, M)                                             \
    EXACT_GEMV_I16_HEADS_ROWS(7, M)

EXACT_GEMV_I16_HEADS_PLANES(0)
EXACT_GEMV_I16_HEADS_PLANES(1)
