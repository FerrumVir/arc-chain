// Projection input preparation (spec §5.2), one workgroup per token.
//
// 1. The projection precondition 127 * sum |x_j| < 2^63, checked exactly.
// 2. Each activation x_j (|x_j| <= 2^62) is written as eight balanced base-256
//    digits c_d in [-128, 127] with x_j = sum_d c_d * 256^d, the same
//    decomposition as the CPU limb kernel (canonical_simd::split_limbs) with
//    eight digits instead of four, so it covers every storable activation.
//    Plane d holds digit d of four consecutive activations per u32.
// 3. ctrl[t] = the number of digit planes in use (the highest non-zero digit
//    over the vector, at least 1); the GEMV skips the all-zero planes above it.

struct SplitParams {
    n: u32,
    words: u32,
    pad0: u32,
    pad1: u32,
    pad2: u32,
    pad3: u32,
    pad4: u32,
    pad5: u32,
}

@group(0) @binding(1) var<uniform> params: SplitParams;
@group(0) @binding(2) var<storage, read> x: array<vec2<u32>>;
@group(0) @binding(3) var<storage, read_write> digits: array<u32>;
@group(0) @binding(4) var<storage, read_write> ctrl: array<u32>;

const SPLIT_WG: u32 = 256u;
const MAX_DIGITS: u32 = 8u;

var<workgroup> mass: array<vec3<u32>, 256>;
var<workgroup> used: array<u32, 256>;

fn load_or_zero(index: u32, end: u32) -> vec2<u32> {
    if (index < end) {
        return x[index];
    }
    return vec2<u32>(0u, 0u);
}

// The value left after removing the balanced digit whose byte is `low`:
// (u - digit) >> 8 with digit = low read as a signed byte, in two's
// complement u64 with a logical shift, exactly as the CPU does.
fn after_digit(u: vec2<u32>, low: u32) -> vec2<u32> {
    let v = select(sub64(u, vec2<u32>(low, 0u)), add64(u, vec2<u32>(256u - low, 0u)), low > 127u);
    return vec2<u32>((v.x >> 8u) | (v.y << 24u), v.y >> 8u);
}

@compute @workgroup_size(256)
fn split(
    @builtin(local_invocation_index) lid: u32,
    @builtin(workgroup_id) wid: vec3<u32>
) {
    let t = wid.x;
    let xbase = t * params.n;
    let xend = xbase + params.n;
    let dbase = t * MAX_DIGITS * params.words;
    var m = vec3<u32>(0u);
    var top = 1u;
    for (var w = lid; w < params.words; w = w + SPLIT_WG) {
        // Four activations walk their digits in lockstep; plane d's word holds
        // digit d of each, low byte first. Only scalars and storage are
        // written (no dynamically indexed local arrays).
        let j = xbase + w * 4u;
        var u0 = load_or_zero(j, xend);
        var u1 = load_or_zero(j + 1u, xend);
        var u2 = load_or_zero(j + 2u, xend);
        var u3 = load_or_zero(j + 3u, xend);
        m = add96(add96(add96(add96(m, i64_abs(u0)), i64_abs(u1)), i64_abs(u2)), i64_abs(u3));
        for (var d = 0u; d < MAX_DIGITS; d = d + 1u) {
            let l0 = u0.x & 255u;
            let l1 = u1.x & 255u;
            let l2 = u2.x & 255u;
            let l3 = u3.x & 255u;
            if ((l0 | l1 | l2 | l3) != 0u) {
                top = max(top, d + 1u);
            }
            digits[dbase + d * params.words + w] = l0 | (l1 << 8u) | (l2 << 16u) | (l3 << 24u);
            u0 = after_digit(u0, l0);
            u1 = after_digit(u1, l1);
            u2 = after_digit(u2, l2);
            u3 = after_digit(u3, l3);
        }
    }
    mass[lid] = m;
    used[lid] = top;
    workgroupBarrier();
    for (var stride = SPLIT_WG / 2u; stride > 0u; stride = stride >> 1u) {
        if (lid < stride) {
            mass[lid] = add96v(mass[lid], mass[lid + stride]);
            used[lid] = max(used[lid], used[lid + stride]);
        }
        workgroupBarrier();
    }
    if (lid == 0u) {
        // 127 * S < 2^63  <=>  S <= (2^63 - 1) / 127 = 0x0102040810204081.
        let s = mass[0];
        let ok = s.z == 0u && (s.y < 0x01020408u || (s.y == 0x01020408u && s.x <= 0x10204081u));
        if (!ok) {
            flag(ST_PROJ_INPUT);
        }
        ctrl[t] = used[0];
    }
}
