// Two-pass attention for one query head (spec §5.6), one workgroup per
// (head, token):
//   dot_j = sum_t q_t k_jt (exact, i128)     score_j = (dot_j * lambda) >> 46
//   M = max_j score_j    w_j = exp(score_j - M)    Z = sum_j w_j
//   out_t = tdiv(sum_j w_j v_jt, Z)
// Every sum is exact and the maximum is exact, so splitting positions across
// threads cannot change a bit. Bounds (host-checked): positions <= 2^15, so
// Z <= 2^31 fits u32 and |sum_j w_j v_jt| < 2^63 fits i64.

struct AttnParams {
    d_head: u32,
    kv_group: u32,
    stride: u32,
    lambda: u32,
    max_pos: u32,
    n_heads: u32,
    pad0: u32,
    pad1: u32,
}

struct Cursor {
    pos0: u32,
    count: u32,
    pad0: u32,
    pad1: u32,
    tokens: array<vec4<u32>, 16>,
}

@group(0) @binding(1) var<uniform> params: AttnParams;
@group(0) @binding(2) var<uniform> cursor: Cursor;
@group(0) @binding(3) var<storage, read> q: array<vec2<u32>>;
@group(0) @binding(4) var<storage, read> k_cache: array<i32>;
@group(0) @binding(5) var<storage, read> v_cache: array<i32>;
@group(0) @binding(6) var<storage, read_write> scratch: array<vec2<u32>>;
@group(0) @binding(7) var<storage, read_write> attended: array<vec2<u32>>;

const ATT_WG: u32 = 128u;

var<workgroup> red_max: array<vec2<u32>, 128>;
var<workgroup> red_z: array<u32, 128>;

// (dot * lambda) >> 46 with the CPU's checked i128 product and |score| <= 2^62.
fn score_of(dotp: vec4<u32>) -> vec2<u32> {
    let neg = is_neg128(dotp);
    let mag = select(dotp, neg128(dotp), neg);
    let m = mul128_u32_wide(mag, params.lambda);
    if (!mag_fits_i128(m.lo, m.hi == 0u, neg)) {
        flag(ST_ATTN_PRODUCT);
        return vec2<u32>(0u, 0u);
    }
    let s = sar128(signed_from_mag(m.lo, neg), 46u);
    if (!act_ok128(s)) {
        flag(ST_ATTN_SCORE);
    }
    return vec2<u32>(s.x, s.y);
}

@compute @workgroup_size(128)
fn attention(
    @builtin(local_invocation_index) lid: u32,
    @builtin(workgroup_id) wid: vec3<u32>
) {
    let head = wid.x;
    let t = wid.y;
    let npos = cursor.pos0 + t + 1u;
    let qbase = (t * params.n_heads + head) * params.d_head;
    let sbase = (t * params.n_heads + head) * params.max_pos;
    let kvoff = (head / params.kv_group) * params.d_head;

    // Pass 1: exact scores and the per-thread maximum.
    var best = vec2<u32>(0u, 0x80000000u); // i64 minimum
    for (var j = lid; j < npos; j = j + ATT_WG) {
        let kb = j * params.stride + kvoff;
        var dotp = vec4<u32>(0u);
        for (var e = 0u; e < params.d_head; e = e + 1u) {
            let prod = mul128_i32(i128_from_i64(q[qbase + e]), k_cache[kb + e]);
            dotp = add128(dotp, prod);
        }
        let sc = score_of(dotp);
        scratch[sbase + j] = sc;
        if (i64_lt(best, sc)) {
            best = sc;
        }
    }
    red_max[lid] = best;
    workgroupBarrier();
    for (var stride = ATT_WG / 2u; stride > 0u; stride = stride >> 1u) {
        if (lid < stride) {
            let other = red_max[lid + stride];
            if (i64_lt(red_max[lid], other)) {
                red_max[lid] = other;
            }
        }
        workgroupBarrier();
    }
    let mx = red_max[0];

    // Pass 2: weights w_j = exp(score_j - M) (overwriting the scores this
    // thread wrote) and their exact sum.
    var z = 0u;
    for (var j = lid; j < npos; j = j + ATT_WG) {
        let w = exp_q16(sub64(scratch[sbase + j], mx));
        scratch[sbase + j] = vec2<u32>(w, 0u);
        z = z + w;
    }
    red_z[lid] = z;
    storageBarrier();
    workgroupBarrier();
    for (var stride = ATT_WG / 2u; stride > 0u; stride = stride >> 1u) {
        if (lid < stride) {
            red_z[lid] = red_z[lid] + red_z[lid + stride];
        }
        workgroupBarrier();
    }
    let total = red_z[0];

    // Pass 3: exact weighted sums, one truncating division per output.
    for (var e = lid; e < params.d_head; e = e + ATT_WG) {
        var acc = vec2<u32>(0u, 0u);
        for (var j = 0u; j < npos; j = j + 1u) {
            let w = scratch[sbase + j].x;
            if (w != 0u) {
                acc = add64(acc, mul_u32_i32(w, v_cache[j * params.stride + kvoff + e]));
            }
        }
        attended[qbase + e] = tdiv64(acc, total);
    }
}
