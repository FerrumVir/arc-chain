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

@compute @workgroup_size(256)
fn split(
    @builtin(local_invocation_index) lid: u32,
    @builtin(workgroup_id) wid: vec3<u32>
) {
    let t = wid.x;
    let xbase = t * params.n;
    let dbase = t * MAX_DIGITS * params.words;
    var m = vec3<u32>(0u);
    var top = 1u;
    for (var w = lid; w < params.words; w = w + SPLIT_WG) {
        // Explicit initializer: naga re-runs it on every iteration (a bare
        // `var` would be hoisted and keep the previous word's bytes).
        var planes = array<u32, 8>(0u, 0u, 0u, 0u, 0u, 0u, 0u, 0u);
        for (var b = 0u; b < 4u; b = b + 1u) {
            let j = w * 4u + b;
            var v = vec2<u32>(0u, 0u);
            if (j < params.n) {
                v = x[xbase + j];
            }
            m = add96(m, i64_abs(v));
            // Two's complement walk: digit = low byte read as signed, then
            // u = (u - digit) >> 8 (logical), exactly as the CPU does in u64.
            var u = v;
            for (var d = 0u; d < MAX_DIGITS; d = d + 1u) {
                let low = u.x & 255u;
                planes[d] = planes[d] | (low << (b * 8u));
                if (low != 0u) {
                    top = max(top, d + 1u);
                }
                if (low > 127u) {
                    u = add64(u, vec2<u32>(256u - low, 0u));
                } else {
                    u = sub64(u, vec2<u32>(low, 0u));
                }
                u = vec2<u32>((u.x >> 8u) | (u.y << 24u), u.y >> 8u);
            }
        }
        for (var d = 0u; d < MAX_DIGITS; d = d + 1u) {
            digits[dbase + d * params.words + w] = planes[d];
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
