// RMS normalisation with an exact square root (spec §5.4), one workgroup per
// token:
//   S = sum x_i^2 (exact, 160 bits), v = floor(S / n) + eps,
//   r = isqrt(floor(2^92 / v)), y_i = (x_i * r * g_i) >> 46.
// Refusals mirror the CPU: S beyond 2^127 (checked i128 sum), v beyond 2^92,
// x*r*g beyond i128, |y| beyond 2^62.

struct NormParams {
    n: u32,
    eps_lo: u32,
    eps_hi: u32,
    pad0: u32,
    pad1: u32,
    pad2: u32,
    pad3: u32,
    pad4: u32,
}

@group(0) @binding(1) var<uniform> params: NormParams;
@group(0) @binding(2) var<storage, read> x: array<vec2<u32>>;
@group(0) @binding(3) var<storage, read> gain: array<vec2<u32>>;
@group(0) @binding(4) var<storage, read_write> normed: array<vec2<u32>>;

const NORM_WG: u32 = 256u;

var<workgroup> sq_lo: array<vec4<u32>, 256>;
var<workgroup> sq_hi: array<u32, 256>;
var<workgroup> inv_rms: vec2<u32>;

// r = isqrt(floor(2^92 / (floor(S / n) + eps))), or 0 after a refusal.
fn inverse_rms(s: vec4<u32>, s_hi: u32) -> vec2<u32> {
    // The CPU sums squares with checked i128 additions of non-negative terms,
    // so it refuses exactly when S >= 2^127.
    if (s_hi != 0u || (s.w >> 31u) != 0u) {
        flag(ST_RMS_SUM);
        return vec2<u32>(0u, 0u);
    }
    let eps = vec4<u32>(params.eps_lo, params.eps_hi, 0u, 0u);
    let mean = add128(div128_u32(s, params.n), eps);
    let limit = vec4<u32>(0u, 0u, 0x10000000u, 0u); // 2^92
    if (u128_lt(limit, mean)) {
        flag(ST_RMS_MEAN);
        return vec2<u32>(0u, 0u);
    }
    let r = isqrt_le_2pow92(div_2pow92(mean)); // <= 2^46
    return vec2<u32>(r.x, r.y);
}

fn normalize_one(v: vec2<u32>, r: vec2<u32>, g: vec2<u32>) -> vec2<u32> {
    let vm = i64_abs(v);
    let m1 = mul128_u64_wide(vec4<u32>(vm.x, vm.y, 0u, 0u), r); // |x| * r < 2^108
    let m2 = mul128_u64_wide(m1.lo, i64_abs(g));
    let neg = i64_is_neg(v) != i64_is_neg(g);
    if (!mag_fits_i128(m2.lo, m2.hi.x == 0u && m2.hi.y == 0u, neg)) {
        flag(ST_RMS_PRODUCT);
        return vec2<u32>(0u, 0u);
    }
    let out = sar128(signed_from_mag(m2.lo, neg), 46u);
    if (!act_ok128(out)) {
        flag(ST_RMS_OUTPUT);
    }
    return vec2<u32>(out.x, out.y);
}

@compute @workgroup_size(256)
fn rms_norm(
    @builtin(local_invocation_index) lid: u32,
    @builtin(workgroup_id) wid: vec3<u32>
) {
    let base = wid.x * params.n;
    var lo = vec4<u32>(0u);
    var hi = 0u;
    for (var i = lid; i < params.n; i = i + NORM_WG) {
        let m = i64_abs(x[base + i]);
        let square = mul128_u64_wide(vec4<u32>(m.x, m.y, 0u, 0u), m).lo; // < 2^128
        let sum = add128(lo, square);
        hi = hi + select(0u, 1u, u128_lt(sum, lo));
        lo = sum;
    }
    sq_lo[lid] = lo;
    sq_hi[lid] = hi;
    workgroupBarrier();
    for (var stride = NORM_WG / 2u; stride > 0u; stride = stride >> 1u) {
        if (lid < stride) {
            let a = sq_lo[lid];
            let sum = add128(a, sq_lo[lid + stride]);
            sq_hi[lid] = sq_hi[lid] + sq_hi[lid + stride] + select(0u, 1u, u128_lt(sum, a));
            sq_lo[lid] = sum;
        }
        workgroupBarrier();
    }
    if (lid == 0u) {
        inv_rms = inverse_rms(sq_lo[0], sq_hi[0]);
    }
    workgroupBarrier();
    let r = inv_rms;
    for (var i = lid; i < params.n; i = i + NORM_WG) {
        normed[base + i] = normalize_one(x[base + i], r, gain[i]);
    }
}
