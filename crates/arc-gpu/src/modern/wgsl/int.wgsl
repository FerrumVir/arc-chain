// Exact integer arithmetic for the dyadic profile
// `arc.hf-llama.i8-dyadic-row.q16.v1` (docs/protocol/integer-profile-hf-llama-dyadic-v1.md).
//
// WGSL has no 64-bit integers. A wide value is a little-endian vector of u32
// limbs in two's complement: an i64 is vec2<u32> (x = low word), an i128 is
// vec4<u32>. The rules that make this portable and exact:
//
// * Wide arithmetic uses only u32 operations. Unsigned overflow wraps on every
//   backend; signed overflow is undefined behaviour in MSL, so it is never
//   relied on. i32 is used only where a bound proves there is no overflow.
// * Every shift amount is a constant or is proven to lie in [0, 31]. Shifts by
//   32 or more are not portable across shading languages.
// * There is no floating point anywhere.
// * Reductions only add or compare exact integers, so their order (thread
//   count, workgroup size, scheduling) cannot change a value.
//
// Every kernel module is this library followed by one kernel. Binding 0 of
// group 0 is the status word: kernels set a bit when an intermediate leaves the
// profile's domain (spec §9), exactly where the CPU engine refuses.

@group(0) @binding(0) var<storage, read_write> status: array<atomic<u32>>;

const ST_PROJ_INPUT: u32 = 1u;      // 127 * sum |x| >= 2^63
const ST_PROJ_OUTPUT: u32 = 2u;     // projection output beyond 2^62
const ST_RMS_SUM: u32 = 4u;         // rms_norm sum of squares beyond 2^127
const ST_RMS_MEAN: u32 = 8u;        // rms_norm mean square beyond 2^92
const ST_RMS_PRODUCT: u32 = 16u;    // rms_norm product beyond 2^127
const ST_RMS_OUTPUT: u32 = 32u;     // rms_norm output beyond 2^62
const ST_ROPE_OUTPUT: u32 = 64u;    // rope output beyond 2^62
const ST_KV_RANGE: u32 = 128u;      // KV value outside i32
const ST_ATTN_PRODUCT: u32 = 256u;  // attention score product beyond 2^127
const ST_ATTN_SCORE: u32 = 512u;    // attention score beyond 2^62
const ST_SILU_PRODUCT: u32 = 1024u; // gated SiLU product beyond 2^127
const ST_SILU_OUTPUT: u32 = 2048u;  // gated SiLU output beyond 2^62
const ST_RESIDUAL: u32 = 4096u;     // residual beyond 2^62
const ST_EMBED_TOKEN: u32 = 8192u;  // token outside the bound embedding chunk

fn flag(bit: u32) {
    _ = atomicOr(&status[0], bit);
}

// ---------------------------------------------------------------- u32 ----

// 1 if `sum = a + b` wrapped, else 0.
fn carry(sum: u32, a: u32) -> u32 {
    return select(0u, 1u, sum < a);
}

// Full 32 x 32 -> 64-bit unsigned product, from four exact 16 x 16 products.
fn mul32(a: u32, b: u32) -> vec2<u32> {
    let a0 = a & 0xFFFFu;
    let a1 = a >> 16u;
    let b0 = b & 0xFFFFu;
    let b1 = b >> 16u;
    let p00 = a0 * b0;
    let p01 = a0 * b1;
    let p10 = a1 * b0;
    let p11 = a1 * b1;
    let mid = p01 + p10;
    let mid_carry = select(0u, 0x10000u, mid < p01);
    let lo = p00 + (mid << 16u);
    let lo_carry = select(0u, 1u, lo < p00);
    let hi = p11 + (mid >> 16u) + mid_carry + lo_carry;
    return vec2<u32>(lo, hi);
}

// |v| as u32; exact for i32 minimum (2^31).
fn abs_i32_u32(v: i32) -> u32 {
    let u = bitcast<u32>(v);
    return select(u, ~u + 1u, v < 0);
}

// Sign-extend the low byte of `b` (a two's complement i8) to i32.
fn sx8(b: u32) -> i32 {
    let v = b & 0xFFu;
    return i32(v) - i32(v & 0x80u) * 2;
}

// ---------------------------------------------------------------- i64 ----

fn i64_is_neg(v: vec2<u32>) -> bool {
    return (v.y >> 31u) != 0u;
}

fn i64_neg(v: vec2<u32>) -> vec2<u32> {
    let lo = ~v.x + 1u;
    let hi = ~v.y + select(0u, 1u, lo == 0u);
    return vec2<u32>(lo, hi);
}

// |v| as an unsigned 64-bit magnitude.
fn i64_abs(v: vec2<u32>) -> vec2<u32> {
    return select(v, i64_neg(v), i64_is_neg(v));
}

fn add64(a: vec2<u32>, b: vec2<u32>) -> vec2<u32> {
    let lo = a.x + b.x;
    return vec2<u32>(lo, a.y + b.y + carry(lo, a.x));
}

fn sub64(a: vec2<u32>, b: vec2<u32>) -> vec2<u32> {
    let borrow = select(0u, 1u, a.x < b.x);
    return vec2<u32>(a.x - b.x, a.y - b.y - borrow);
}

// Signed a < b.
fn i64_lt(a: vec2<u32>, b: vec2<u32>) -> bool {
    let ah = bitcast<i32>(a.y);
    let bh = bitcast<i32>(b.y);
    return ah < bh || (ah == bh && a.x < b.x);
}

// Whether an i64 lies in [-2^31, 2^31 - 1].
fn fits_i32(v: vec2<u32>) -> bool {
    return v.y == select(0u, 0xFFFFFFFFu, (v.x >> 31u) != 0u);
}

// (i64) s << sh for sh in [0, 63], modulo 2^64.
fn shl64_i32(s: i32, sh: u32) -> vec2<u32> {
    let lo = bitcast<u32>(s);
    let hi = select(0u, 0xFFFFFFFFu, s < 0);
    if (sh == 0u) {
        return vec2<u32>(lo, hi);
    }
    if (sh >= 32u) {
        return vec2<u32>(0u, lo << (sh - 32u));
    }
    return vec2<u32>(lo << sh, (hi << sh) | (lo >> (32u - sh)));
}

// w * v for w in [0, 2^16] and any i32 v (|product| <= 2^47), as i64.
fn mul_u32_i32(w: u32, v: i32) -> vec2<u32> {
    let m = mul32(w, abs_i32_u32(v));
    return select(m, i64_neg(m), v < 0);
}

// floor(n / d) for an unsigned 64-bit n and 0 < d <= 2^31. The remainder stays
// below d, so doubling it never leaves u32.
fn div64_u32(n: vec2<u32>, d: u32) -> vec2<u32> {
    var q = vec2<u32>(0u, 0u);
    var rem = 0u;
    for (var i = 0u; i < 64u; i = i + 1u) {
        let bi = 63u - i;
        let limb = bi >> 5u;
        let bit = (n[limb] >> (bi & 31u)) & 1u;
        rem = (rem << 1u) | bit;
        if (rem >= d) {
            rem = rem - d;
            q[limb] = q[limb] | (1u << (bi & 31u));
        }
    }
    return q;
}

// Truncating division of an i64 by 0 < d <= 2^31 (Rust `/`).
fn tdiv64(n: vec2<u32>, d: u32) -> vec2<u32> {
    let q = div64_u32(i64_abs(n), d);
    return select(q, i64_neg(q), i64_is_neg(n));
}

// ---------------------------------------------------------------- 96 -----

fn add96(m: vec3<u32>, a: vec2<u32>) -> vec3<u32> {
    let x = m.x + a.x;
    let c0 = carry(x, m.x);
    let y0 = m.y + a.y;
    let y = y0 + c0;
    let c1 = carry(y0, m.y) + carry(y, y0);
    return vec3<u32>(x, y, m.z + c1);
}

fn add96v(a: vec3<u32>, b: vec3<u32>) -> vec3<u32> {
    let s = add96(a, b.xy);
    return vec3<u32>(s.x, s.y, s.z + b.z);
}

// ---------------------------------------------------------------- i128 ---

fn i128_from_i64(v: vec2<u32>) -> vec4<u32> {
    let fill = select(0u, 0xFFFFFFFFu, i64_is_neg(v));
    return vec4<u32>(v.x, v.y, fill, fill);
}

fn i128_from_i32(v: i32) -> vec4<u32> {
    let fill = select(0u, 0xFFFFFFFFu, v < 0);
    return vec4<u32>(bitcast<u32>(v), fill, fill, fill);
}

fn is_neg128(a: vec4<u32>) -> bool {
    return (a.w >> 31u) != 0u;
}

fn add128(a: vec4<u32>, b: vec4<u32>) -> vec4<u32> {
    let x = a.x + b.x;
    let c0 = carry(x, a.x);
    let y0 = a.y + b.y;
    let y = y0 + c0;
    let c1 = carry(y0, a.y) + carry(y, y0);
    let z0 = a.z + b.z;
    let z = z0 + c1;
    let c2 = carry(z0, a.z) + carry(z, z0);
    return vec4<u32>(x, y, z, a.w + b.w + c2);
}

fn neg128(a: vec4<u32>) -> vec4<u32> {
    return add128(~a, vec4<u32>(1u, 0u, 0u, 0u));
}

fn sub128(a: vec4<u32>, b: vec4<u32>) -> vec4<u32> {
    return add128(a, neg128(b));
}

// Unsigned a < b.
fn u128_lt(a: vec4<u32>, b: vec4<u32>) -> bool {
    if (a.w != b.w) {
        return a.w < b.w;
    }
    if (a.z != b.z) {
        return a.z < b.z;
    }
    if (a.y != b.y) {
        return a.y < b.y;
    }
    return a.x < b.x;
}

fn shl128_1(a: vec4<u32>) -> vec4<u32> {
    return vec4<u32>(
        a.x << 1u,
        (a.y << 1u) | (a.x >> 31u),
        (a.z << 1u) | (a.y >> 31u),
        (a.w << 1u) | (a.z >> 31u)
    );
}

fn shr128_1(a: vec4<u32>) -> vec4<u32> {
    return vec4<u32>(
        (a.x >> 1u) | (a.y << 31u),
        (a.y >> 1u) | (a.z << 31u),
        (a.z >> 1u) | (a.w << 31u),
        a.w >> 1u
    );
}

fn shr128_2(a: vec4<u32>) -> vec4<u32> {
    return vec4<u32>(
        (a.x >> 2u) | (a.y << 30u),
        (a.y >> 2u) | (a.z << 30u),
        (a.z >> 2u) | (a.w << 30u),
        a.w >> 2u
    );
}

// a * b modulo 2^128 for an unsigned b; the two's complement product is exact
// whenever the true product of a signed `a` and `b` fits in i128.
fn mul128_u32(a: vec4<u32>, b: u32) -> vec4<u32> {
    let p0 = mul32(a.x, b);
    let p1 = mul32(a.y, b);
    let p2 = mul32(a.z, b);
    let p3 = mul32(a.w, b);
    let r1 = p0.y + p1.x;
    let c1 = carry(r1, p0.y);
    let t2 = p1.y + p2.x;
    let r2 = t2 + c1;
    let c2 = carry(t2, p1.y) + carry(r2, t2);
    let r3 = p2.y + p3.x + c2;
    return vec4<u32>(p0.x, r1, r2, r3);
}

// a * b for a signed i128 `a` and i32 `b`, exact when |a * b| < 2^127.
fn mul128_i32(a: vec4<u32>, b: i32) -> vec4<u32> {
    let m = mul128_u32(a, abs_i32_u32(b));
    return select(m, neg128(m), b < 0);
}

// Floor division by 2^s (arithmetic shift right) for s in [0, 127].
fn sar128(a: vec4<u32>, s: u32) -> vec4<u32> {
    let fill = select(0u, 0xFFFFFFFFu, is_neg128(a));
    var v = a;
    var n = s;
    for (var i = 0u; i < 3u; i = i + 1u) {
        if (n >= 32u) {
            v = vec4<u32>(v.y, v.z, v.w, fill);
            n = n - 32u;
        }
    }
    if (n >= 32u) {
        return vec4<u32>(fill, fill, fill, fill);
    }
    if (n == 0u) {
        return v;
    }
    let r = 32u - n;
    return vec4<u32>(
        (v.x >> n) | (v.y << r),
        (v.y >> n) | (v.z << r),
        (v.z >> n) | (v.w << r),
        (v.w >> n) | (fill << r)
    );
}

// Whether a signed i128 satisfies |a| <= 2^62 (a storable activation, spec §9).
fn act_ok128(a: vec4<u32>) -> bool {
    if (is_neg128(a)) {
        return a.w == 0xFFFFFFFFu && a.z == 0xFFFFFFFFu && a.y >= 0xC0000000u;
    }
    return a.w == 0u && a.z == 0u && (a.y < 0x40000000u || (a.y == 0x40000000u && a.x == 0u));
}

// ------------------------------------------------- unsigned wide products --

struct U160 {
    lo: vec4<u32>,
    hi: u32,
}

struct U192 {
    lo: vec4<u32>,
    hi: vec2<u32>,
}

// Exact a * b for unsigned 128-bit a and 32-bit b.
fn mul128_u32_wide(a: vec4<u32>, b: u32) -> U160 {
    let p0 = mul32(a.x, b);
    let p1 = mul32(a.y, b);
    let p2 = mul32(a.z, b);
    let p3 = mul32(a.w, b);
    let r1 = p0.y + p1.x;
    let c1 = carry(r1, p0.y);
    let t2 = p1.y + p2.x;
    let r2 = t2 + c1;
    let c2 = carry(t2, p1.y) + carry(r2, t2);
    let t3 = p2.y + p3.x;
    let r3 = t3 + c2;
    let c3 = carry(t3, p2.y) + carry(r3, t3);
    return U160(vec4<u32>(p0.x, r1, r2, r3), p3.y + c3);
}

// Exact a * b for unsigned 128-bit a and 64-bit b.
fn mul128_u64_wide(a: vec4<u32>, b: vec2<u32>) -> U192 {
    let m0 = mul128_u32_wide(a, b.x);
    let m1 = mul128_u32_wide(a, b.y);
    // m0 + (m1 << 32)
    let l1 = m0.lo.y + m1.lo.x;
    var c = carry(l1, m0.lo.y);
    let t2 = m0.lo.z + m1.lo.y;
    let l2 = t2 + c;
    c = carry(t2, m0.lo.z) + carry(l2, t2);
    let t3 = m0.lo.w + m1.lo.z;
    let l3 = t3 + c;
    c = carry(t3, m0.lo.w) + carry(l3, t3);
    let t4 = m0.hi + m1.lo.w;
    let l4 = t4 + c;
    c = carry(t4, m0.hi) + carry(l4, t4);
    return U192(vec4<u32>(m0.lo.x, l1, l2, l3), vec2<u32>(l4, m1.hi + c));
}

// Whether a product of magnitude `lo` (with all higher limbs zero when
// `upper_zero`) and sign `neg` lies in [-2^127, 2^127 - 1], i.e. whether the
// CPU's checked i128 multiply succeeds.
fn mag_fits_i128(lo: vec4<u32>, upper_zero: bool, neg: bool) -> bool {
    if (!upper_zero) {
        return false;
    }
    if (lo.w < 0x80000000u) {
        return true;
    }
    return neg && lo.w == 0x80000000u && lo.z == 0u && lo.y == 0u && lo.x == 0u;
}

// The i128 with magnitude `lo` and sign `neg`.
fn signed_from_mag(lo: vec4<u32>, neg: bool) -> vec4<u32> {
    return select(lo, neg128(lo), neg);
}

// ----------------------------------------------------- unsigned division --

// floor(n / d) for unsigned 128-bit n and 0 < d <= 2^31.
fn div128_u32(n: vec4<u32>, d: u32) -> vec4<u32> {
    var q = vec4<u32>(0u);
    var rem = 0u;
    for (var i = 0u; i < 128u; i = i + 1u) {
        let bi = 127u - i;
        let limb = bi >> 5u;
        let bit = (n[limb] >> (bi & 31u)) & 1u;
        rem = (rem << 1u) | bit;
        if (rem >= d) {
            rem = rem - d;
            q[limb] = q[limb] | (1u << (bi & 31u));
        }
    }
    return q;
}

// floor(2^92 / d) for 1 <= d <= 2^92 (restoring division; the remainder stays
// below d, so doubling it stays below 2^93).
fn div_2pow92(d: vec4<u32>) -> vec4<u32> {
    var q = vec4<u32>(0u);
    var rem = vec4<u32>(0u);
    for (var i = 0u; i < 93u; i = i + 1u) {
        let bi = 92u - i;
        rem = shl128_1(rem);
        if (i == 0u) {
            rem.x = rem.x | 1u;
        }
        if (!u128_lt(rem, d)) {
            rem = sub128(rem, d);
            q[bi >> 5u] = q[bi >> 5u] | (1u << (bi & 31u));
        }
    }
    return q;
}

// floor(sqrt(n)) for n <= 2^92, digit by digit (exact; any exact method gives
// the same integer as the CPU's Newton iteration).
fn isqrt_le_2pow92(n_in: vec4<u32>) -> vec4<u32> {
    var n = n_in;
    var res = vec4<u32>(0u);
    var bit = vec4<u32>(0u, 0u, 0x10000000u, 0u); // 2^92, the largest power of four needed
    for (var i = 0u; i < 47u; i = i + 1u) {
        let t = add128(res, bit);
        if (!u128_lt(n, t)) {
            n = sub128(n, t);
            res = add128(shr128_1(res), bit);
        } else {
            res = shr128_1(res);
        }
        bit = shr128_2(bit);
    }
    return res;
}
