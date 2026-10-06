// Gated SiLU (spec §5.7), in place over the gate vector:
//   sigma(g) = floor(2^32 / (2^16 + exp(-g)))        if g >= 0
//            = floor(exp(g) * 2^16 / (2^16 + exp(g))) if g < 0
//   a = (g * sigma(g) * u) >> 32
// with the CPU's checked i128 product and refusal of |a| > 2^62.
// Grid: x = element within a token (n per token), y = token in the batch.

struct SiluParams {
    n: u32,
    pad0: u32,
    pad1: u32,
    pad2: u32,
    pad3: u32,
    pad4: u32,
    pad5: u32,
    pad6: u32,
}

struct Cursor {
    pos0: u32,
    count: u32,
    pad0: u32,
    pad1: u32,
    tokens: array<vec4<u32>, 16>,
}

@group(0) @binding(1) var<uniform> params: SiluParams;
@group(0) @binding(4) var<uniform> cursor: Cursor;
@group(0) @binding(2) var<storage, read_write> gate: array<vec2<u32>>;
@group(0) @binding(3) var<storage, read> up: array<vec2<u32>>;

// floor(2^32 / d) for d >= 2, from floor((2^32 - 1) / d).
fn div_2pow32(d: u32) -> u32 {
    let q = 0xFFFFFFFFu / d;
    let r = 0xFFFFFFFFu - q * d;
    return select(q, q + 1u, r + 1u == d);
}

fn sigmoid_q16(g: vec2<u32>) -> u32 {
    if (!i64_is_neg(g)) {
        return div_2pow32(65536u + exp_q16(i64_neg(g)));
    }
    let e = exp_q16(g); // <= 65535 for g < 0, so e << 16 < 2^32
    return (e << 16u) / (65536u + e);
}

@compute @workgroup_size(64)
fn gated_silu(@builtin(global_invocation_id) gid: vec3<u32>) {
    if (gid.x >= params.n || gid.y >= cursor.count) {
        return;
    }
    let i = gid.y * params.n + gid.x;
    let g = gate[i];
    let u = up[i];
    let gm = i64_abs(g);
    let m1 = mul128_u32_wide(vec4<u32>(gm.x, gm.y, 0u, 0u), sigmoid_q16(g)); // < 2^79
    let m2 = mul128_u64_wide(m1.lo, i64_abs(u));
    let neg = i64_is_neg(g) != i64_is_neg(u);
    if (!mag_fits_i128(m2.lo, m2.hi.x == 0u && m2.hi.y == 0u, neg)) {
        flag(ST_SILU_PRODUCT);
        gate[i] = vec2<u32>(0u, 0u);
        return;
    }
    let a = sar128(signed_from_mag(m2.lo, neg), 32u);
    if (!act_ok128(a)) {
        flag(ST_SILU_OUTPUT);
    }
    gate[i] = vec2<u32>(a.x, a.y);
}
