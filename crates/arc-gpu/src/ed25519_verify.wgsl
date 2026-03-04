// Ed25519 signature verification compute shader for ARC Chain.
//
// Each GPU thread verifies one Ed25519 signature.
// CPU pre-computes k = SHA-512(R || A || M) mod l (avoids SHA-512 in WGSL).
// GPU does: decompress R, decompress A, compute [S]B and R + [k]A, compare.
//
// Field element representation: 10 limbs of ~25.5 bits (radix 2^25.5).
//   Limbs alternate between 26-bit and 25-bit capacity.
//   This keeps intermediate products within u32 range when multiplying
//   individual limbs (max 26-bit * 26-bit < 2^52, accumulated via carry).
//
// Edwards curve: -x^2 + y^2 = 1 + d*x^2*y^2  where d = -121665/121666 mod p
// p = 2^255 - 19

// ── Bindings ─────────────────────────────────────────────────────────────────

// Input: packed 32 u32s (128 bytes) per signature
//   [0..8]   R (32 bytes, compressed point)
//   [8..16]  S (32 bytes, scalar)
//   [16..24] k (32 bytes, pre-computed scalar mod l)
//   [24..32] A (32 bytes, compressed public key)
@group(0) @binding(0) var<storage, read> sig_data: array<u32>;

// Output: 1 u32 per signature (1 = valid, 0 = invalid)
@group(0) @binding(1) var<storage, read_write> results: array<u32>;

// Params
struct Params { num_items: u32 }
@group(0) @binding(2) var<uniform> params: Params;

// Base point table: 16 entries × 30 u32s each (x[10], y[10], t[10])
// B_TABLE[i] = i * B in affine extended coordinates (z=1)
@group(0) @binding(3) var<storage, read> base_table: array<u32>;

// ── Constants ────────────────────────────────────────────────────────────────

// p = 2^255 - 19 in 10-limb form (alternating 26/25 bits)
// Limb widths: 26,25,26,25,26,25,26,25,26,25

// d = -121665/121666 mod p (twisted Edwards parameter)
// d = 37095705934669439343138083508754565189542113879843219016388785533085940283555

// 2*d
// These are stored as 10-limb field elements

// Ed25519 base point B (x, y) in compressed form:
// y = 4/5 mod p = 46316835694926478169428394003475163141307993866256225615783033890098355573289
// x (positive) = 15112221349535807912866137220509078750507884956996801397042474682556759602985

// ── Wide multiply helper ─────────────────────────────────────────────────────

// Multiply two u32s, return (lo, hi) of the 64-bit result
fn mul_wide(a: u32, b: u32) -> vec2<u32> {
    let a0 = a & 0xFFFFu;
    let a1 = a >> 16u;
    let b0 = b & 0xFFFFu;
    let b1 = b >> 16u;

    let p00 = a0 * b0;
    let p01 = a0 * b1;
    let p10 = a1 * b0;
    let p11 = a1 * b1;

    // Accumulate middle terms
    let mid = p01 + (p00 >> 16u);
    let mid_hi = mid >> 16u;
    let mid_lo = mid & 0xFFFFu;

    let mid2 = (mid_lo + p10);
    let mid2_carry = mid2 >> 16u;

    let lo = (p00 & 0xFFFFu) | (mid2 << 16u);
    let hi = p11 + mid_hi + mid2_carry;

    return vec2<u32>(lo, hi);
}

// Add two 64-bit values represented as vec2<u32>
fn add64(a: vec2<u32>, b: vec2<u32>) -> vec2<u32> {
    let lo = a.x + b.x;
    let carry = select(0u, 1u, lo < a.x);
    let hi = a.y + b.y + carry;
    return vec2<u32>(lo, hi);
}

// Shift right a 64-bit value by n bits (n < 32)
fn shr64(v: vec2<u32>, n: u32) -> vec2<u32> {
    if (n == 0u) { return v; }
    let lo = (v.x >> n) | (v.y << (32u - n));
    let hi = v.y >> n;
    return vec2<u32>(lo, hi);
}

// ── Field Element (mod p = 2^255 - 19) ──────────────────────────────────────
// 10 limbs stored in workgroup-private array.
// Limb sizes: 26, 25, 26, 25, 26, 25, 26, 25, 26, 25 bits
// Total = 26+25+26+25+26+25+26+25+26+25 = 255 bits

// Load 32 bytes from sig_data into a 10-limb field element
fn fe_frombytes(base: u32) -> array<u32, 10> {
    // Read 32 bytes as 8 little-endian u32s
    var b: array<u32, 8>;
    for (var i = 0u; i < 8u; i++) {
        b[i] = sig_data[base + i];
    }

    // Pack into 10 limbs (26,25,26,25,26,25,26,25,26,25 bits)
    var f: array<u32, 10>;
    // Byte 0..3 → bits 0..31, limb0 = bits 0..25 (26 bits)
    f[0] = b[0] & 0x3FFFFFFu;                          // 26 bits
    f[1] = ((b[0] >> 26u) | (b[1] << 6u)) & 0x1FFFFFFu; // 25 bits
    f[2] = ((b[1] >> 19u) | (b[2] << 13u)) & 0x3FFFFFFu; // 26 bits
    f[3] = ((b[2] >> 13u) | (b[3] << 19u)) & 0x1FFFFFFu; // 25 bits
    f[4] = (b[3] >> 6u) & 0x3FFFFFFu;                   // 26 bits
    f[5] = b[4] & 0x1FFFFFFu;                           // 25 bits
    f[6] = ((b[4] >> 25u) | (b[5] << 7u)) & 0x3FFFFFFu; // 26 bits
    f[7] = ((b[5] >> 19u) | (b[6] << 13u)) & 0x1FFFFFFu; // 25 bits
    f[8] = ((b[6] >> 12u) | (b[7] << 20u)) & 0x3FFFFFFu; // 26 bits
    f[9] = (b[7] >> 6u) & 0x1FFFFFFu;                    // 25 bits, top bit cleared

    return f;
}

// Convert 10-limb field element back to 32 bytes (8 u32s written to results)
fn fe_tobytes(f_in: array<u32, 10>) -> array<u32, 8> {
    // Fully reduce to canonical form
    var f = fe_freeze(f_in);

    var out: array<u32, 8>;
    out[0] = f[0] | (f[1] << 26u);
    out[1] = (f[1] >> 6u) | (f[2] << 19u);
    out[2] = (f[2] >> 13u) | (f[3] << 13u);
    out[3] = (f[3] >> 19u) | (f[4] << 6u);
    out[4] = f[5] | (f[6] << 25u);
    out[5] = (f[6] >> 7u) | (f[7] << 19u);
    out[6] = (f[7] >> 13u) | (f[8] << 12u);
    out[7] = (f[8] >> 20u) | (f[9] << 6u);

    return out;
}

fn fe_zero() -> array<u32, 10> {
    return array<u32, 10>(0u,0u,0u,0u,0u,0u,0u,0u,0u,0u);
}

fn fe_one() -> array<u32, 10> {
    return array<u32, 10>(1u,0u,0u,0u,0u,0u,0u,0u,0u,0u);
}

fn fe_add(a: array<u32, 10>, b: array<u32, 10>) -> array<u32, 10> {
    var r: array<u32, 10>;
    for (var i = 0u; i < 10u; i++) {
        r[i] = a[i] + b[i];
    }
    return r;
}

fn fe_sub(a: array<u32, 10>, b: array<u32, 10>) -> array<u32, 10> {
    // Add multiples of p to avoid underflow
    // p limbs: each limb can hold ~25-26 bits, so adding 2*p ensures positive
    // 2p in limb form (double each limb of p representation):
    var r: array<u32, 10>;
    let bias = array<u32, 10>(
        0x7FFFFDAu, 0x3FFFFFEu, 0x7FFFFFEu, 0x3FFFFFEu, 0x7FFFFFEu,
        0x3FFFFFEu, 0x7FFFFFEu, 0x3FFFFFEu, 0x7FFFFFEu, 0x3FFFFFEu
    );
    for (var i = 0u; i < 10u; i++) {
        r[i] = (a[i] + bias[i]) - b[i];
    }
    return fe_reduce(r);
}

fn fe_neg(a: array<u32, 10>) -> array<u32, 10> {
    return fe_sub(fe_zero(), a);
}

// Carry-propagate to normalize limbs
fn fe_reduce(f: array<u32, 10>) -> array<u32, 10> {
    var r = f;

    // Propagate carries: even limbs → 26 bits, odd limbs → 25 bits
    for (var round = 0u; round < 2u; round++) {
        for (var i = 0u; i < 9u; i++) {
            if (i % 2u == 0u) {
                // 26-bit limb
                let carry = r[i] >> 26u;
                r[i] = r[i] & 0x3FFFFFFu;
                r[i + 1u] = r[i + 1u] + carry;
            } else {
                // 25-bit limb
                let carry = r[i] >> 25u;
                r[i] = r[i] & 0x1FFFFFFu;
                r[i + 1u] = r[i + 1u] + carry;
            }
        }
        // Last limb (25-bit): carry wraps with factor 19
        let carry9 = r[9] >> 25u;
        r[9] = r[9] & 0x1FFFFFFu;
        r[0] = r[0] + carry9 * 19u;
    }

    return r;
}

// Field multiplication using schoolbook with 64-bit intermediates.
//
// IMPORTANT: The 10-limb radix-2^25.5 representation has alternating widths
// (26,25,26,25,...). When both indices i and j are odd, the positional values
// satisfy pos[i]+pos[j] = pos[i+j] + 1, so the product must be DOUBLED.
// For wrapped terms (i+j >= 10), use factor 38 instead of 19 when both odd.
fn fe_mul(a: array<u32, 10>, b: array<u32, 10>) -> array<u32, 10> {
    // Precompute 19*b[j] for wrap-around reduction
    var b19: array<u32, 10>;
    for (var i = 0u; i < 10u; i++) {
        b19[i] = b[i] * 19u;
    }

    // Pre-doubled a[i] for odd indices (used when both i,j odd)
    var a2: array<u32, 10>;
    for (var i = 0u; i < 10u; i++) {
        a2[i] = a[i] * 2u;
    }

    // Accumulate 10 output limbs using 64-bit intermediates
    var h: array<vec2<u32>, 10>;
    for (var i = 0u; i < 10u; i++) {
        h[i] = vec2<u32>(0u, 0u);
    }

    // Schoolbook multiply with reduction and odd-odd doubling
    for (var i = 0u; i < 10u; i++) {
        let i_odd = (i & 1u) == 1u;
        for (var j = 0u; j < 10u; j++) {
            let k = i + j;
            let both_odd = i_odd && ((j & 1u) == 1u);
            // When both indices are odd, use doubled a[i] to account for
            // the extra factor of 2 in positional value
            let ai = select(a[i], a2[i], both_odd);
            let bj = select(b[j], b19[j], k >= 10u);
            let idx = k % 10u;
            let prod = mul_wide(ai, bj);
            h[idx] = add64(h[idx], prod);
        }
    }

    // Extract low bits and propagate carries
    var r: array<u32, 10>;
    var carry = vec2<u32>(0u, 0u);
    for (var i = 0u; i < 10u; i++) {
        let sum = add64(h[i], carry);
        if (i % 2u == 0u) {
            r[i] = sum.x & 0x3FFFFFFu; // 26 bits
            carry = shr64(sum, 26u);
        } else {
            r[i] = sum.x & 0x1FFFFFFu; // 25 bits
            carry = shr64(sum, 25u);
        }
    }
    // Final carry wraps with factor 19.
    // IMPORTANT: carry can be larger than 32 bits (up to ~37 bits), so we must
    // use the full 64-bit carry * 19, not just carry.x * 19.
    let carry19 = mul_wide(carry.x, 19u);
    // Also handle carry.y (high 32 bits of carry) * 19
    let carry19_hi = carry.y * 19u; // small value, fits in u32
    // carry * 19 = carry19 + (carry19_hi << 32) = carry19 + vec2(0, carry19_hi)
    let carry19_full = add64(carry19, vec2<u32>(0u, carry19_hi));
    // Add to r[0] and propagate
    var r0_wide = add64(vec2<u32>(r[0], 0u), carry19_full);
    r[0] = r0_wide.x & 0x3FFFFFFu;
    let extra_carry = shr64(r0_wide, 26u);
    r[1] = r[1] + extra_carry.x;

    return fe_reduce(r);
}

fn fe_sq(a: array<u32, 10>) -> array<u32, 10> {
    return fe_mul(a, a);
}

// Compute a^(2^n) by repeated squaring
fn fe_sq_n(a: array<u32, 10>, n: u32) -> array<u32, 10> {
    var r = a;
    for (var i = 0u; i < n; i++) {
        r = fe_sq(r);
    }
    return r;
}

// Field inversion: a^(p-2) mod p using addition chain
fn fe_invert(z: array<u32, 10>) -> array<u32, 10> {
    let z2 = fe_sq(z);
    let z9 = fe_mul(fe_sq_n(z2, 2u), z);  // z^9
    let z11 = fe_mul(z9, z2);             // z^11
    let z_5_0 = fe_mul(fe_sq(z11), z9);   // z^(2^5 - 1)
    let z_10_0 = fe_mul(fe_sq_n(z_5_0, 5u), z_5_0);
    let z_20_0 = fe_mul(fe_sq_n(z_10_0, 10u), z_10_0);
    let z_40_0 = fe_mul(fe_sq_n(z_20_0, 20u), z_20_0);
    let z_50_0 = fe_mul(fe_sq_n(z_40_0, 10u), z_10_0);
    let z_100_0 = fe_mul(fe_sq_n(z_50_0, 50u), z_50_0);
    let z_200_0 = fe_mul(fe_sq_n(z_100_0, 100u), z_100_0);
    let z_250_0 = fe_mul(fe_sq_n(z_200_0, 50u), z_50_0);
    // z^(p-2) = z^(2^255 - 21)
    return fe_mul(fe_sq_n(z_250_0, 5u), z11);
}

// Compute a^((p-5)/8) = a^(2^252 - 3) for square root
fn fe_pow2523(z: array<u32, 10>) -> array<u32, 10> {
    let z2 = fe_sq(z);
    let z9 = fe_mul(fe_sq_n(z2, 2u), z);
    let z11 = fe_mul(z9, z2);
    let z_5_0 = fe_mul(fe_sq(z11), z9);
    let z_10_0 = fe_mul(fe_sq_n(z_5_0, 5u), z_5_0);
    let z_20_0 = fe_mul(fe_sq_n(z_10_0, 10u), z_10_0);
    let z_40_0 = fe_mul(fe_sq_n(z_20_0, 20u), z_20_0);
    let z_50_0 = fe_mul(fe_sq_n(z_40_0, 10u), z_10_0);
    let z_100_0 = fe_mul(fe_sq_n(z_50_0, 50u), z_50_0);
    let z_200_0 = fe_mul(fe_sq_n(z_100_0, 100u), z_100_0);
    let z_250_0 = fe_mul(fe_sq_n(z_200_0, 50u), z_50_0);
    return fe_mul(fe_sq_n(z_250_0, 2u), z);
}

// Fully reduce a field element to canonical form [0, p).
// After fe_reduce, the value may still equal p (represented as non-zero limbs).
// fe_freeze detects this and subtracts p if needed.
fn fe_freeze(f_in: array<u32, 10>) -> array<u32, 10> {
    var h = fe_reduce(f_in);

    // Compute q: add 19 and propagate carries through all limbs.
    // If the result overflows 2^255, then h >= p, so q = 1.
    var q = (h[0] + 19u) >> 26u;
    q = (h[1] + q) >> 25u;
    q = (h[2] + q) >> 26u;
    q = (h[3] + q) >> 25u;
    q = (h[4] + q) >> 26u;
    q = (h[5] + q) >> 25u;
    q = (h[6] + q) >> 26u;
    q = (h[7] + q) >> 25u;
    q = (h[8] + q) >> 26u;
    q = (h[9] + q) >> 25u;
    // q is 0 or 1

    // If h >= p, subtract p by adding 19 (since p = 2^255 - 19, adding 19
    // and letting the carry overflow 2^255 effectively subtracts p)
    h[0] += 19u * q;

    // Carry propagation to normalize
    var carry: u32;
    carry = h[0] >> 26u; h[0] &= 0x3FFFFFFu; h[1] += carry;
    carry = h[1] >> 25u; h[1] &= 0x1FFFFFFu; h[2] += carry;
    carry = h[2] >> 26u; h[2] &= 0x3FFFFFFu; h[3] += carry;
    carry = h[3] >> 25u; h[3] &= 0x1FFFFFFu; h[4] += carry;
    carry = h[4] >> 26u; h[4] &= 0x3FFFFFFu; h[5] += carry;
    carry = h[5] >> 25u; h[5] &= 0x1FFFFFFu; h[6] += carry;
    carry = h[6] >> 26u; h[6] &= 0x3FFFFFFu; h[7] += carry;
    carry = h[7] >> 25u; h[7] &= 0x1FFFFFFu; h[8] += carry;
    carry = h[8] >> 26u; h[8] &= 0x3FFFFFFu; h[9] += carry;
    h[9] &= 0x1FFFFFFu;

    return h;
}

// Check if field element is zero (canonical reduction first)
fn fe_is_zero(a: array<u32, 10>) -> bool {
    let r = fe_freeze(a);
    var acc = 0u;
    for (var i = 0u; i < 10u; i++) {
        acc = acc | r[i];
    }
    return acc == 0u;
}

// Check if field element is negative (lowest bit, canonical)
fn fe_is_negative(a: array<u32, 10>) -> bool {
    let r = fe_freeze(a);
    return (r[0] & 1u) == 1u;
}

// Check equality of two field elements
fn fe_eq(a: array<u32, 10>, b: array<u32, 10>) -> bool {
    return fe_is_zero(fe_sub(a, b));
}

// ── Edwards Curve Point (Extended Coordinates) ──────────────────────────────
// Point = (X, Y, Z, T) where x = X/Z, y = Y/Z, x*y = T/Z

struct GePoint {
    x: array<u32, 10>,
    y: array<u32, 10>,
    z: array<u32, 10>,
    t: array<u32, 10>,
}

fn ge_zero() -> GePoint {
    return GePoint(
        fe_zero(),
        fe_one(),
        fe_one(),
        fe_zero()
    );
}

// d constant: -121665/121666 mod p
// In 10-limb form (precomputed)
fn fe_d() -> array<u32, 10> {
    return array<u32, 10>(
        0x35978A3u, 0x0D37284u, 0x3156EBDu, 0x06A0A0Eu, 0x001C029u,
        0x179E898u, 0x3A03CBBu, 0x1CE7198u, 0x2E2B6FFu, 0x1480DB3u
    );
}

// 2*d constant
fn fe_2d() -> array<u32, 10> {
    return array<u32, 10>(
        0x2B2F159u, 0x1A6E509u, 0x22ADD7Au, 0x0D4141Du, 0x0038052u,
        0x0F3D130u, 0x3407977u, 0x19CE331u, 0x1C56DFFu, 0x0901B67u
    );
}

// sqrt(-1) mod p = 2^((p-1)/4) mod p
fn fe_sqrtm1() -> array<u32, 10> {
    return array<u32, 10>(
        0x20EA0B0u, 0x186C9D2u, 0x08F189Du, 0x035697Fu, 0x0BD0C60u,
        0x1FBD7A7u, 0x2804C9Eu, 0x1E16569u, 0x004FC1Du, 0x0AE0C92u
    );
}

// ── Base Point Table (from storage buffer) ──────────────────────────────────
// The base_table buffer contains 16 entries × 30 u32s each:
//   entry[i] = { x[0..9], y[0..9], t[0..9] } for point i*B
// Layout: base_table[i*30 + 0..9] = x, [i*30 + 10..19] = y, [i*30 + 20..29] = t

// Read a field element from the base point table buffer.
fn bt_read_fe(offset: u32) -> array<u32, 10> {
    return array<u32, 10>(
        base_table[offset], base_table[offset + 1u], base_table[offset + 2u],
        base_table[offset + 3u], base_table[offset + 4u], base_table[offset + 5u],
        base_table[offset + 6u], base_table[offset + 7u], base_table[offset + 8u],
        base_table[offset + 9u]
    );
}

// Lookup from base point table by index (0..15).
fn b_table_lookup(idx: u32) -> GePoint {
    let base = idx * 30u;
    return GePoint(
        bt_read_fe(base),        // x
        bt_read_fe(base + 10u),  // y
        fe_one(),                // z = 1 (affine)
        bt_read_fe(base + 20u)   // t
    );
}

// ── Point Decompression ─────────────────────────────────────────────────────

// Decompress a 32-byte compressed Edwards point.
// Returns (point, success). success=false if the point is invalid.
fn ge_frombytes(base: u32) -> GePoint {
    // Read the y coordinate (clear top bit = sign)
    var y_bytes: array<u32, 8>;
    for (var i = 0u; i < 8u; i++) {
        y_bytes[i] = sig_data[base + i];
    }
    let x_sign = (y_bytes[7] >> 31u) & 1u;
    y_bytes[7] = y_bytes[7] & 0x7FFFFFFFu; // clear sign bit

    // Convert y bytes to field element
    var y_raw: array<u32, 10>;
    y_raw[0] = y_bytes[0] & 0x3FFFFFFu;
    y_raw[1] = ((y_bytes[0] >> 26u) | (y_bytes[1] << 6u)) & 0x1FFFFFFu;
    y_raw[2] = ((y_bytes[1] >> 19u) | (y_bytes[2] << 13u)) & 0x3FFFFFFu;
    y_raw[3] = ((y_bytes[2] >> 13u) | (y_bytes[3] << 19u)) & 0x1FFFFFFu;
    y_raw[4] = (y_bytes[3] >> 6u) & 0x3FFFFFFu;
    y_raw[5] = y_bytes[4] & 0x1FFFFFFu;
    y_raw[6] = ((y_bytes[4] >> 25u) | (y_bytes[5] << 7u)) & 0x3FFFFFFu;
    y_raw[7] = ((y_bytes[5] >> 19u) | (y_bytes[6] << 13u)) & 0x1FFFFFFu;
    y_raw[8] = ((y_bytes[6] >> 12u) | (y_bytes[7] << 20u)) & 0x3FFFFFFu;
    y_raw[9] = (y_bytes[7] >> 6u) & 0x1FFFFFFu;

    let y = fe_reduce(y_raw);

    // Recover x from y: x^2 = (y^2 - 1) / (d * y^2 + 1)
    let y2 = fe_sq(y);
    let u = fe_sub(y2, fe_one());           // u = y^2 - 1
    let v = fe_add(fe_mul(fe_d(), y2), fe_one()); // v = d*y^2 + 1

    // x = u * v^3 * (u * v^7)^((p-5)/8)
    let v3 = fe_mul(fe_sq(v), v);
    let v7 = fe_mul(fe_sq(v3), v);
    let uv7 = fe_mul(u, v7);
    let x_candidate = fe_mul(fe_mul(u, v3), fe_pow2523(uv7));

    // Check: v * x^2 == u?
    let vx2 = fe_mul(v, fe_sq(x_candidate));
    var x = x_candidate;

    if (!fe_eq(vx2, u)) {
        // Try x * sqrt(-1)
        x = fe_mul(x, fe_sqrtm1());
        let vx2_neg = fe_mul(v, fe_sq(x));
        if (!fe_eq(vx2_neg, u)) {
            // Point not on curve — return identity (will fail verification)
            return ge_zero();
        }
    }

    // Adjust sign
    if (fe_is_negative(x) != (x_sign == 1u)) {
        x = fe_neg(x);
    }

    let t = fe_mul(x, y);

    return GePoint(x, y, fe_one(), t);
}

// Extended point addition (unified formula)
// Uses the formula from "Twisted Edwards Curves Revisited" (HWCD)
fn ge_add(p: GePoint, q: GePoint) -> GePoint {
    let a = fe_mul(fe_sub(p.y, p.x), fe_sub(q.y, q.x));
    let b = fe_mul(fe_add(p.y, p.x), fe_add(q.y, q.x));
    let c = fe_mul(fe_mul(p.t, q.t), fe_2d());
    let d = fe_mul(p.z, fe_add(q.z, q.z));

    let e = fe_sub(b, a);
    let f = fe_sub(d, c);
    let g = fe_add(d, c);
    let h = fe_add(b, a);

    return GePoint(
        fe_mul(e, f),
        fe_mul(g, h),
        fe_mul(f, g),
        fe_mul(e, h)
    );
}

// Point doubling
fn ge_double(p: GePoint) -> GePoint {
    let a = fe_sq(p.x);
    let b = fe_sq(p.y);
    let c_inner = fe_sq(p.z);
    let c = fe_add(c_inner, c_inner);
    let d = fe_neg(a);  // -x^2 (for the -1 in -x^2 + y^2 = 1 + d*x^2*y^2)

    let e = fe_sub(fe_sq(fe_add(p.x, p.y)), fe_add(a, b));
    let g = fe_add(d, b);
    let f = fe_sub(g, c);
    let h = fe_sub(d, b);

    return GePoint(
        fe_mul(e, f),
        fe_mul(g, h),
        fe_mul(f, g),
        fe_mul(e, h)
    );
}

// Load a 32-byte scalar from sig_data as 32 bytes
fn load_scalar(base: u32) -> array<u32, 8> {
    var s: array<u32, 8>;
    for (var i = 0u; i < 8u; i++) {
        s[i] = sig_data[base + i];
    }
    return s;
}

// Get bit i of a 256-bit scalar (stored as 8 little-endian u32s)
fn scalar_bit(s: array<u32, 8>, bit: u32) -> u32 {
    let word = bit / 32u;
    let pos = bit % 32u;
    return (s[word] >> pos) & 1u;
}

// Double scalar multiplication: [a]B + [b]P using Shamir's trick.
// Processes both scalars simultaneously in a single 256-iteration loop.
// Precomputes B+P to handle all 4 cases per bit.
fn ge_double_scalarmult(a: array<u32, 8>, b: array<u32, 8>, p: GePoint) -> GePoint {
    let bp = b_table_lookup(1u);   // base point B
    let bpp = ge_add(bp, p);       // B + P (precomputed)

    var result = ge_zero();
    var found_one = false;
    for (var i_rev = 0u; i_rev < 256u; i_rev++) {
        let i = 255u - i_rev;
        if (found_one) {
            result = ge_double(result);
        }
        let ab = scalar_bit(a, i);
        let bb = scalar_bit(b, i);

        if (ab == 1u && bb == 1u) {
            if (found_one) { result = ge_add(result, bpp); }
            else { result = bpp; found_one = true; }
        } else if (ab == 1u) {
            if (found_one) { result = ge_add(result, bp); }
            else { result = bp; found_one = true; }
        } else if (bb == 1u) {
            if (found_one) { result = ge_add(result, p); }
            else { result = p; found_one = true; }
        }
    }
    return result;
}

// Compare two extended-coordinate points for equality
// In projective: P1 == P2 iff X1*Z2 == X2*Z1 AND Y1*Z2 == Y2*Z1
fn ge_eq(a: GePoint, b: GePoint) -> bool {
    let lhs_x = fe_mul(a.x, b.z);
    let rhs_x = fe_mul(b.x, a.z);
    let lhs_y = fe_mul(a.y, b.z);
    let rhs_y = fe_mul(b.y, a.z);
    return fe_eq(lhs_x, rhs_x) && fe_eq(lhs_y, rhs_y);
}

// ── Main Verification Kernel ─────────────────────────────────────────────────

@compute @workgroup_size(64)
fn main(@builtin(global_invocation_id) gid: vec3<u32>) {
    let idx = gid.x;
    if (idx >= params.num_items) {
        return;
    }

    let base = idx * 32u;
    let point_R = ge_frombytes(base + 0u);
    let point_A = ge_frombytes(base + 24u);

    let r_is_id = fe_is_zero(point_R.x) && fe_eq(point_R.y, fe_one());
    let a_is_id = fe_is_zero(point_A.x) && fe_eq(point_A.y, fe_one());
    if (r_is_id || a_is_id) {
        results[idx] = 0u;
        return;
    }

    let scalar_S = load_scalar(base + 8u);
    let scalar_k = load_scalar(base + 16u);

    let neg_A = GePoint(fe_neg(point_A.x), point_A.y, point_A.z, fe_neg(point_A.t));
    let check = ge_double_scalarmult(scalar_S, scalar_k, neg_A);

    let valid = ge_eq(check, point_R);
    results[idx] = select(0u, 1u, valid);
}
