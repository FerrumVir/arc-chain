// Ed25519 signature verification — Native Metal compute shader for ARC Chain.
//
// Each GPU thread verifies one Ed25519 signature using NATIVE 64-bit arithmetic.
// CPU pre-computes k = SHA-512(R || A || M) mod l.
// GPU does: decompress R, decompress A, compute [S]B + [k](-A), compare with R.
//
// Key advantage over WGSL: native ulong (uint64_t) for field multiplication.
// Apple Silicon supports hardware 32x32→64 multiply at ~half throughput,
// eliminating the mul_wide/add64/shr64 emulation overhead (~6x fewer ops).
//
// Field element: 10 limbs of radix-2^25.5 (alternating 26/25 bit widths)
// Curve: -x^2 + y^2 = 1 + d*x^2*y^2  (Ed25519 twisted Edwards)
// p = 2^255 - 19

#include <metal_stdlib>
using namespace metal;

// ── Types ──────────────────────────────────────────────────────────────────────

struct Params {
    uint num_items;
};

struct Fe {
    uint v[10];
};

struct GePoint {
    Fe x, y, z, t;
};

// ── Constants ──────────────────────────────────────────────────────────────────

// d = -121665/121666 mod p in 10-limb form
constant uint D_VALS[10] = {
    0x035978A3, 0x00D37284, 0x03156EBD, 0x006A0A0E, 0x0001C029,
    0x0179E898, 0x03A03CBB, 0x01CE7198, 0x02E2B6FF, 0x01480DB3
};

// 2*d in 10-limb form
constant uint D2_VALS[10] = {
    0x02B2F159, 0x01A6E509, 0x022ADD7A, 0x00D4141D, 0x00038052,
    0x00F3D130, 0x03407977, 0x019CE331, 0x01C56DFF, 0x00901B67
};

// sqrt(-1) mod p in 10-limb form
constant uint SQRTM1_VALS[10] = {
    0x020EA0B0, 0x0186C9D2, 0x008F189D, 0x0035697F, 0x00BD0C60,
    0x01FBD7A7, 0x02804C9E, 0x01E16569, 0x0004FC1D, 0x00AE0C92
};

// 2*p bias for subtraction (prevents underflow)
constant uint FE_SUB_BIAS[10] = {
    0x7FFFFDA, 0x3FFFFFE, 0x7FFFFFE, 0x3FFFFFE, 0x7FFFFFE,
    0x3FFFFFE, 0x7FFFFFE, 0x3FFFFFE, 0x7FFFFFE, 0x3FFFFFE
};

// ── Fe Helper Constructors ─────────────────────────────────────────────────────

Fe fe_from_const(constant uint* vals) {
    Fe r;
    for (uint i = 0; i < 10; i++) r.v[i] = vals[i];
    return r;
}

Fe fe_zero() {
    Fe r;
    for (uint i = 0; i < 10; i++) r.v[i] = 0;
    return r;
}

Fe fe_one() {
    Fe r;
    r.v[0] = 1;
    for (uint i = 1; i < 10; i++) r.v[i] = 0;
    return r;
}

Fe fe_d()      { return fe_from_const(D_VALS); }
Fe fe_2d()     { return fe_from_const(D2_VALS); }
Fe fe_sqrtm1() { return fe_from_const(SQRTM1_VALS); }

// ── Field Arithmetic ───────────────────────────────────────────────────────────

Fe fe_add(Fe a, Fe b) {
    Fe r;
    for (uint i = 0; i < 10; i++) r.v[i] = a.v[i] + b.v[i];
    return r;
}

// Carry-propagate to normalize limbs
Fe fe_reduce(Fe f) {
    Fe r = f;
    for (uint round = 0; round < 2; round++) {
        for (uint i = 0; i < 9; i++) {
            if (i % 2 == 0) {
                uint carry = r.v[i] >> 26;
                r.v[i] &= 0x3FFFFFF;
                r.v[i + 1] += carry;
            } else {
                uint carry = r.v[i] >> 25;
                r.v[i] &= 0x1FFFFFF;
                r.v[i + 1] += carry;
            }
        }
        uint carry9 = r.v[9] >> 25;
        r.v[9] &= 0x1FFFFFF;
        r.v[0] += carry9 * 19;
    }
    return r;
}

Fe fe_sub(Fe a, Fe b) {
    Fe r;
    for (uint i = 0; i < 10; i++) {
        r.v[i] = (a.v[i] + FE_SUB_BIAS[i]) - b.v[i];
    }
    return fe_reduce(r);
}

Fe fe_neg(Fe a) {
    return fe_sub(fe_zero(), a);
}

// Field multiplication with NATIVE 64-bit arithmetic.
// Fully unrolled schoolbook multiply: 100 products, zero branches.
//
// The 10-limb radix-2^25.5 representation has alternating widths (26,25,...).
// When both indices i,j are odd, the product must be DOUBLED because
// pos[i]+pos[j] = pos[i+j] + 1. For wrapped terms (i+j >= 10), factor = 19.
// When both odd AND wrapping, factor = 2*19 = 38.
//
// Precomputed coefficients:
//   da[i] = 2 * a[i]  (for odd i, used in both-odd non-wrapping terms)
//   b19[j] = 19 * b[j]  (for wrapping terms)
// Both-odd wrapping: da[i] * b19[j] = 2*a[i]*19*b[j] = 38*a[i]*b[j]
Fe fe_mul(Fe a, Fe b) {
    ulong a0 = a.v[0], a1 = a.v[1], a2 = a.v[2], a3 = a.v[3], a4 = a.v[4];
    ulong a5 = a.v[5], a6 = a.v[6], a7 = a.v[7], a8 = a.v[8], a9 = a.v[9];
    ulong b0 = b.v[0], b1 = b.v[1], b2 = b.v[2], b3 = b.v[3], b4 = b.v[4];
    ulong b5 = b.v[5], b6 = b.v[6], b7 = b.v[7], b8 = b.v[8], b9 = b.v[9];

    // Doubled odd-index a values (for both-odd terms)
    ulong da1 = 2 * a1, da3 = 2 * a3, da5 = 2 * a5, da7 = 2 * a7, da9 = 2 * a9;

    // 19 * b[j] for wrapped terms
    ulong b19_1 = 19 * b1, b19_2 = 19 * b2, b19_3 = 19 * b3, b19_4 = 19 * b4;
    ulong b19_5 = 19 * b5, b19_6 = 19 * b6, b19_7 = 19 * b7, b19_8 = 19 * b8, b19_9 = 19 * b9;

    // 100 products, fully unrolled, zero branches
    ulong h0 = a0*b0     + da1*b19_9 + a2*b19_8  + da3*b19_7 + a4*b19_6
             + da5*b19_5  + a6*b19_4  + da7*b19_3 + a8*b19_2  + da9*b19_1;
    ulong h1 = a0*b1     + a1*b0     + a2*b19_9  + a3*b19_8  + a4*b19_7
             + a5*b19_6   + a6*b19_5  + a7*b19_4  + a8*b19_3  + a9*b19_2;
    ulong h2 = a0*b2     + da1*b1    + a2*b0     + da3*b19_9 + a4*b19_8
             + da5*b19_7  + a6*b19_6  + da7*b19_5 + a8*b19_4  + da9*b19_3;
    ulong h3 = a0*b3     + a1*b2     + a2*b1     + a3*b0     + a4*b19_9
             + a5*b19_8   + a6*b19_7  + a7*b19_6  + a8*b19_5  + a9*b19_4;
    ulong h4 = a0*b4     + da1*b3    + a2*b2     + da3*b1    + a4*b0
             + da5*b19_9  + a6*b19_8  + da7*b19_7 + a8*b19_6  + da9*b19_5;
    ulong h5 = a0*b5     + a1*b4     + a2*b3     + a3*b2     + a4*b1
             + a5*b0      + a6*b19_9  + a7*b19_8  + a8*b19_7  + a9*b19_6;
    ulong h6 = a0*b6     + da1*b5    + a2*b4     + da3*b3    + a4*b2
             + da5*b1     + a6*b0     + da7*b19_9 + a8*b19_8  + da9*b19_7;
    ulong h7 = a0*b7     + a1*b6     + a2*b5     + a3*b4     + a4*b3
             + a5*b2      + a6*b1     + a7*b0     + a8*b19_9  + a9*b19_8;
    ulong h8 = a0*b8     + da1*b7    + a2*b6     + da3*b5    + a4*b4
             + da5*b3     + a6*b2     + da7*b1    + a8*b0     + da9*b19_9;
    ulong h9 = a0*b9     + a1*b8     + a2*b7     + a3*b6     + a4*b5
             + a5*b4      + a6*b3     + a7*b2     + a8*b1     + a9*b0;

    // Carry propagation (single pass + wrap)
    ulong carry;
    carry = h0 >> 26; h0 &= 0x3FFFFFF; h1 += carry;
    carry = h1 >> 25; h1 &= 0x1FFFFFF; h2 += carry;
    carry = h2 >> 26; h2 &= 0x3FFFFFF; h3 += carry;
    carry = h3 >> 25; h3 &= 0x1FFFFFF; h4 += carry;
    carry = h4 >> 26; h4 &= 0x3FFFFFF; h5 += carry;
    carry = h5 >> 25; h5 &= 0x1FFFFFF; h6 += carry;
    carry = h6 >> 26; h6 &= 0x3FFFFFF; h7 += carry;
    carry = h7 >> 25; h7 &= 0x1FFFFFF; h8 += carry;
    carry = h8 >> 26; h8 &= 0x3FFFFFF; h9 += carry;
    carry = h9 >> 25; h9 &= 0x1FFFFFF;
    // Final carry wraps with factor 19 — native ulong, no emulation needed
    h0 += carry * 19;
    carry = h0 >> 26; h0 &= 0x3FFFFFF;
    h1 += carry;

    Fe r;
    r.v[0] = (uint)h0; r.v[1] = (uint)h1; r.v[2] = (uint)h2; r.v[3] = (uint)h3; r.v[4] = (uint)h4;
    r.v[5] = (uint)h5; r.v[6] = (uint)h6; r.v[7] = (uint)h7; r.v[8] = (uint)h8; r.v[9] = (uint)h9;
    return r;
}

Fe fe_sq(Fe a) { return fe_mul(a, a); }

Fe fe_sq_n(Fe a, uint n) {
    Fe r = a;
    for (uint i = 0; i < n; i++) r = fe_sq(r);
    return r;
}

// Fully reduce to canonical form [0, p)
Fe fe_freeze(Fe f_in) {
    Fe h = fe_reduce(f_in);

    uint q = (h.v[0] + 19) >> 26;
    q = (h.v[1] + q) >> 25;
    q = (h.v[2] + q) >> 26;
    q = (h.v[3] + q) >> 25;
    q = (h.v[4] + q) >> 26;
    q = (h.v[5] + q) >> 25;
    q = (h.v[6] + q) >> 26;
    q = (h.v[7] + q) >> 25;
    q = (h.v[8] + q) >> 26;
    q = (h.v[9] + q) >> 25;

    h.v[0] += 19 * q;

    uint c;
    c = h.v[0] >> 26; h.v[0] &= 0x3FFFFFF; h.v[1] += c;
    c = h.v[1] >> 25; h.v[1] &= 0x1FFFFFF; h.v[2] += c;
    c = h.v[2] >> 26; h.v[2] &= 0x3FFFFFF; h.v[3] += c;
    c = h.v[3] >> 25; h.v[3] &= 0x1FFFFFF; h.v[4] += c;
    c = h.v[4] >> 26; h.v[4] &= 0x3FFFFFF; h.v[5] += c;
    c = h.v[5] >> 25; h.v[5] &= 0x1FFFFFF; h.v[6] += c;
    c = h.v[6] >> 26; h.v[6] &= 0x3FFFFFF; h.v[7] += c;
    c = h.v[7] >> 25; h.v[7] &= 0x1FFFFFF; h.v[8] += c;
    c = h.v[8] >> 26; h.v[8] &= 0x3FFFFFF; h.v[9] += c;
    h.v[9] &= 0x1FFFFFF;

    return h;
}

bool fe_is_zero(Fe a) {
    Fe r = fe_freeze(a);
    uint acc = 0;
    for (uint i = 0; i < 10; i++) acc |= r.v[i];
    return acc == 0;
}

bool fe_is_negative(Fe a) {
    Fe r = fe_freeze(a);
    return (r.v[0] & 1) == 1;
}

bool fe_eq(Fe a, Fe b) {
    return fe_is_zero(fe_sub(a, b));
}

// a^((p-5)/8) = a^(2^252 - 3) for square root
Fe fe_pow2523(Fe z) {
    Fe z2 = fe_sq(z);
    Fe z9 = fe_mul(fe_sq_n(z2, 2), z);
    Fe z11 = fe_mul(z9, z2);
    Fe z_5_0 = fe_mul(fe_sq(z11), z9);
    Fe z_10_0 = fe_mul(fe_sq_n(z_5_0, 5), z_5_0);
    Fe z_20_0 = fe_mul(fe_sq_n(z_10_0, 10), z_10_0);
    Fe z_40_0 = fe_mul(fe_sq_n(z_20_0, 20), z_20_0);
    Fe z_50_0 = fe_mul(fe_sq_n(z_40_0, 10), z_10_0);
    Fe z_100_0 = fe_mul(fe_sq_n(z_50_0, 50), z_50_0);
    Fe z_200_0 = fe_mul(fe_sq_n(z_100_0, 100), z_100_0);
    Fe z_250_0 = fe_mul(fe_sq_n(z_200_0, 50), z_50_0);
    return fe_mul(fe_sq_n(z_250_0, 2), z);
}

// ── Edwards Curve Point (Extended Coordinates) ─────────────────────────────────
// Point = (X, Y, Z, T) where x = X/Z, y = Y/Z, x*y = T/Z

GePoint ge_zero() {
    return GePoint{fe_zero(), fe_one(), fe_one(), fe_zero()};
}

// Extended point addition (HWCD unified formula)
GePoint ge_add(GePoint p, GePoint q) {
    Fe a = fe_mul(fe_sub(p.y, p.x), fe_sub(q.y, q.x));
    Fe b = fe_mul(fe_add(p.y, p.x), fe_add(q.y, q.x));
    Fe c = fe_mul(fe_mul(p.t, q.t), fe_2d());
    Fe dd = fe_mul(p.z, fe_add(q.z, q.z));

    Fe e = fe_sub(b, a);
    Fe f = fe_sub(dd, c);
    Fe g = fe_add(dd, c);
    Fe h = fe_add(b, a);

    return GePoint{fe_mul(e, f), fe_mul(g, h), fe_mul(f, g), fe_mul(e, h)};
}

// Point doubling
GePoint ge_double(GePoint p) {
    Fe a = fe_sq(p.x);
    Fe b = fe_sq(p.y);
    Fe c = fe_add(fe_sq(p.z), fe_sq(p.z));
    Fe dd = fe_neg(a);

    Fe e = fe_sub(fe_sq(fe_add(p.x, p.y)), fe_add(a, b));
    Fe g = fe_add(dd, b);
    Fe f = fe_sub(g, c);
    Fe h = fe_sub(dd, b);

    return GePoint{fe_mul(e, f), fe_mul(g, h), fe_mul(f, g), fe_mul(e, h)};
}

// Compare two extended-coordinate points for equality
// P1 == P2 iff X1*Z2 == X2*Z1 AND Y1*Z2 == Y2*Z1
bool ge_eq(GePoint a, GePoint b) {
    Fe lhs_x = fe_mul(a.x, b.z);
    Fe rhs_x = fe_mul(b.x, a.z);
    Fe lhs_y = fe_mul(a.y, b.z);
    Fe rhs_y = fe_mul(b.y, a.z);
    return fe_eq(lhs_x, rhs_x) && fe_eq(lhs_y, rhs_y);
}

// ── Buffer Access Functions ────────────────────────────────────────────────────

// Load 32 bytes from buffer into a 10-limb field element
Fe fe_frombytes(device const uint* buf, uint base) {
    uint b0 = buf[base], b1 = buf[base+1], b2 = buf[base+2], b3 = buf[base+3];
    uint b4 = buf[base+4], b5 = buf[base+5], b6 = buf[base+6], b7 = buf[base+7];

    Fe f;
    f.v[0] = b0 & 0x3FFFFFF;
    f.v[1] = ((b0 >> 26) | (b1 << 6)) & 0x1FFFFFF;
    f.v[2] = ((b1 >> 19) | (b2 << 13)) & 0x3FFFFFF;
    f.v[3] = ((b2 >> 13) | (b3 << 19)) & 0x1FFFFFF;
    f.v[4] = (b3 >> 6) & 0x3FFFFFF;
    f.v[5] = b4 & 0x1FFFFFF;
    f.v[6] = ((b4 >> 25) | (b5 << 7)) & 0x3FFFFFF;
    f.v[7] = ((b5 >> 19) | (b6 << 13)) & 0x1FFFFFF;
    f.v[8] = ((b6 >> 12) | (b7 << 20)) & 0x3FFFFFF;
    f.v[9] = (b7 >> 6) & 0x1FFFFFF;
    return f;
}

// Read a field element from base point table buffer
Fe bt_read_fe(device const uint* base_table, uint offset) {
    Fe r;
    for (uint i = 0; i < 10; i++) r.v[i] = base_table[offset + i];
    return r;
}

// Lookup from base point table by index (0..15)
GePoint b_table_lookup(device const uint* base_table, uint idx) {
    uint base = idx * 30;
    return GePoint{
        bt_read_fe(base_table, base),
        bt_read_fe(base_table, base + 10),
        fe_one(),
        bt_read_fe(base_table, base + 20)
    };
}

// Decompress a 32-byte compressed Edwards point
GePoint ge_frombytes(device const uint* sig_data, uint base) {
    uint yb0 = sig_data[base], yb1 = sig_data[base+1], yb2 = sig_data[base+2], yb3 = sig_data[base+3];
    uint yb4 = sig_data[base+4], yb5 = sig_data[base+5], yb6 = sig_data[base+6], yb7 = sig_data[base+7];

    uint x_sign = (yb7 >> 31) & 1;
    yb7 &= 0x7FFFFFFF;

    Fe y;
    y.v[0] = yb0 & 0x3FFFFFF;
    y.v[1] = ((yb0 >> 26) | (yb1 << 6)) & 0x1FFFFFF;
    y.v[2] = ((yb1 >> 19) | (yb2 << 13)) & 0x3FFFFFF;
    y.v[3] = ((yb2 >> 13) | (yb3 << 19)) & 0x1FFFFFF;
    y.v[4] = (yb3 >> 6) & 0x3FFFFFF;
    y.v[5] = yb4 & 0x1FFFFFF;
    y.v[6] = ((yb4 >> 25) | (yb5 << 7)) & 0x3FFFFFF;
    y.v[7] = ((yb5 >> 19) | (yb6 << 13)) & 0x1FFFFFF;
    y.v[8] = ((yb6 >> 12) | (yb7 << 20)) & 0x3FFFFFF;
    y.v[9] = (yb7 >> 6) & 0x1FFFFFF;
    y = fe_reduce(y);

    // Recover x from y: x^2 = (y^2 - 1) / (d * y^2 + 1)
    Fe y2 = fe_sq(y);
    Fe u = fe_sub(y2, fe_one());
    Fe v = fe_add(fe_mul(fe_d(), y2), fe_one());

    // x = u * v^3 * (u * v^7)^((p-5)/8)
    Fe v3 = fe_mul(fe_sq(v), v);
    Fe v7 = fe_mul(fe_sq(v3), v);
    Fe uv7 = fe_mul(u, v7);
    Fe x = fe_mul(fe_mul(u, v3), fe_pow2523(uv7));

    // Check: v * x^2 == u?
    Fe vx2 = fe_mul(v, fe_sq(x));
    if (!fe_eq(vx2, u)) {
        // Try x * sqrt(-1)
        x = fe_mul(x, fe_sqrtm1());
        Fe vx2_neg = fe_mul(v, fe_sq(x));
        if (!fe_eq(vx2_neg, u)) {
            return ge_zero();
        }
    }

    // Adjust sign
    if (fe_is_negative(x) != (x_sign == 1)) {
        x = fe_neg(x);
    }

    Fe t = fe_mul(x, y);
    return GePoint{x, y, fe_one(), t};
}

// ── Scalar Operations ──────────────────────────────────────────────────────────

// Load 8 u32s from buffer
void load_scalar(device const uint* sig_data, uint base, thread uint* s) {
    for (uint i = 0; i < 8; i++) s[i] = sig_data[base + i];
}

// Get bit i of a 256-bit scalar (8 little-endian u32s)
uint scalar_bit(thread uint* s, uint bit) {
    return (s[bit / 32] >> (bit % 32)) & 1;
}

// Branchless double scalar multiplication: [a]B + [b]P using Shamir's trick.
//
// Fully uniform execution: every thread runs identical instructions (no divergence).
// Uses a 4-entry lookup table indexed by (a_bit, b_bit):
//   0 = identity, 1 = B, 2 = P, 3 = B+P
// Adding identity when both bits are 0 is a mathematical no-op but keeps SIMD uniform.
// The 4-entry LUT costs only 640 bytes (vs 2560 for 16-entry windowed table).
GePoint ge_double_scalarmult(thread uint* a, thread uint* b,
                              GePoint p, device const uint* base_table) {
    // Build 4-entry LUT: indexed by a_bit + 2 * b_bit
    GePoint lut[4];
    lut[0] = ge_zero();                              // (0,0): identity
    lut[1] = b_table_lookup(base_table, 1);          // (1,0): B
    lut[2] = p;                                      // (0,1): P
    lut[3] = ge_add(lut[1], p);                      // (1,1): B+P

    GePoint result = ge_zero();

    for (uint i_rev = 0; i_rev < 256; i_rev++) {
        uint i = 255 - i_rev;
        result = ge_double(result);
        uint idx = scalar_bit(a, i) + 2u * scalar_bit(b, i);
        result = ge_add(result, lut[idx]);
    }
    return result;
}

// ── Kernel Entry Point ─────────────────────────────────────────────────────────

kernel void ed25519_verify_main(
    device const uint* sig_data   [[buffer(0)]],
    device uint*       results    [[buffer(1)]],
    constant Params&   params     [[buffer(2)]],
    device const uint* base_table [[buffer(3)]],
    uint gid [[thread_position_in_grid]])
{
    if (gid >= params.num_items) {
        return;
    }

    uint base = gid * 32;
    GePoint point_R = ge_frombytes(sig_data, base);
    GePoint point_A = ge_frombytes(sig_data, base + 24);

    // Reject identity points
    bool r_is_id = fe_is_zero(point_R.x) && fe_eq(point_R.y, fe_one());
    bool a_is_id = fe_is_zero(point_A.x) && fe_eq(point_A.y, fe_one());
    if (r_is_id || a_is_id) {
        results[gid] = 0;
        return;
    }

    uint scalar_S[8], scalar_k[8];
    load_scalar(sig_data, base + 8, scalar_S);
    load_scalar(sig_data, base + 16, scalar_k);

    // Negate A: (-x, y, z, -t)
    GePoint neg_A = GePoint{fe_neg(point_A.x), point_A.y, point_A.z, fe_neg(point_A.t)};

    // Compute [S]B + [k](-A) via Shamir's trick
    GePoint check = ge_double_scalarmult(scalar_S, scalar_k, neg_A, base_table);

    // Verify: [S]B + [k](-A) == R
    bool valid = ge_eq(check, point_R);
    results[gid] = valid ? 1u : 0u;
}
