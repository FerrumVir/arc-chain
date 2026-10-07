// Gated SiLU (spec §5.7), in place over the gate vector:
//   sigma(g) = floor(2^32 / (2^16 + exp(-g)))        if g >= 0
//            = floor(exp(g) * 2^16 / (2^16 + exp(g))) if g < 0
//   a = (g * sigma(g) * u) >> 32
// with the CPU's checked i128 product and refusal of |a| > 2^62.
// Grid: x = element within a token (n per token), y = token in the batch.
//
// sigma uses the shift-subtract divider of int.wgsl (div_word), never the
// native `/`. The previous version divided with `/` (the only two native
// integer divisions in these kernels, both with a per-thread divisor). On an
// Apple M2 Ultra (Metal, wgpu 25, macOS 14.6.1) at commit b2f90797b it
// returned products multiplied by exactly 2^15 + 1 for positive gates of 55 to
// 59 bits, while lavapipe and WARP agreed with the CPU on the same inputs
// (docs/gpu-portable-kernels.md §5). sigma is also branch-free now.

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

// sigma(g) in Q16, in [0, 65536], as one long division:
//   e = exp(-|g|) in Q16 (0 for |g| >= 2^20, 65536 for g = 0), d = 2^16 + e;
//   the numerator is 2^32 (the 64-bit pair (0, 1)) for g >= 0 and e * 2^16
//   (< 2^32, since e <= 65535 for g < 0) for g < 0.
// d lies in [2^16, 2^17], within div_word's bound of 2^31, and the quotient is
// at most 2^16, so its high word is zero.
fn sigmoid_q16(g: vec2<u32>) -> u32 {
    let e = exp_q16(i64_neg(i64_abs(g)));
    let d = 65536u + e;
    let n = select(vec2<u32>(0u, 1u), vec2<u32>(e << 16u, 0u), i64_is_neg(g));
    return div64_u32(n, d).x;
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
