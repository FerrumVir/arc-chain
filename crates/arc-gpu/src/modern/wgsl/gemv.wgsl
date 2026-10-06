// Exact dyadic projection (spec §5.2): y_i = (sum_j q_ij * x_j * mu_i) >> k_i.
//
// With x_j = sum_d c_dj * 256^d (split.wgsl), sum_j q_ij x_j =
// sum_d 256^d * S_d with S_d = sum_j q_ij c_dj, an i8 x i8 dot product. Each
// S_d is accumulated in i32: |q c| <= 127 * 128, so a row of K <= 132,104
// inputs cannot overflow (the host refuses larger K), and every partial sum of
// any subset of products obeys the same bound, so the reduction order is free.
// The digit sums are then combined in 64-bit two's complement (exact, since the
// precondition gives |acc| < 2^63) and the dyadic epilogue runs in i128.
//
// Layout: 256 threads = 8 rows x 32 lanes. Lanes stride along the row in u32
// words (coalesced), each weight word is read once and applied to every digit
// plane in use. Workgroup index = x + y * nx (2-D dispatch for > 65,535
// groups); z is the token in the batch.

struct GemvParams {
    rows: u32,
    words: u32,
    out_offset: u32,
    out_stride: u32,
    pad0: u32,
    pad1: u32,
    pad2: u32,
    pad3: u32,
}

@group(0) @binding(1) var<uniform> params: GemvParams;
@group(0) @binding(2) var<storage, read> weights: array<u32>;
@group(0) @binding(3) var<storage, read> scales: array<vec2<u32>>;
@group(0) @binding(4) var<storage, read> digits: array<u32>;
@group(0) @binding(5) var<storage, read> ctrl: array<u32>;
@group(0) @binding(6) var<storage, read_write> out: array<vec2<u32>>;

const GEMV_WG: u32 = 256u;
const LANES: u32 = 32u;
const ROWS_PER_GROUP: u32 = 8u;

var<workgroup> part: array<i32, 2048>; // GEMV_WG * 8 digit sums

// (acc * mu) >> k with the CPU's refusal of |y| > 2^62.
fn dyadic_epilogue(acc: vec2<u32>, sc: vec2<u32>) -> vec2<u32> {
    // |acc| < 2^63 and mu < 2^31, so |acc * mu| < 2^94: exact in i128.
    let y = sar128(mul128_u32(i128_from_i64(acc), sc.x), sc.y);
    if (!act_ok128(y)) {
        flag(ST_PROJ_OUTPUT);
    }
    return vec2<u32>(y.x, y.y);
}

@compute @workgroup_size(256)
fn gemv(
    @builtin(local_invocation_index) lid: u32,
    @builtin(workgroup_id) wid: vec3<u32>,
    @builtin(num_workgroups) nwg: vec3<u32>
) {
    let group_index = wid.x + wid.y * nwg.x;
    let t = wid.z;
    let lane = lid % LANES;
    let row = group_index * ROWS_PER_GROUP + lid / LANES;
    let live = row < params.rows;
    let words = params.words;
    let dbase = t * 8u * words;
    let nd = ctrl[t];
    var a0 = 0;
    var a1 = 0;
    var a2 = 0;
    var a3 = 0;
    var a4 = 0;
    var a5 = 0;
    var a6 = 0;
    var a7 = 0;
    if (live) {
        let wbase = row * words;
        for (var w = lane; w < words; w = w + LANES) {
            let q = weights[wbase + w];
            let c = dbase + w;
            a0 = a0 + dot4_i8(q, digits[c]);
            if (nd > 1u) {
                a1 = a1 + dot4_i8(q, digits[c + words]);
            }
            if (nd > 2u) {
                a2 = a2 + dot4_i8(q, digits[c + 2u * words]);
            }
            if (nd > 3u) {
                a3 = a3 + dot4_i8(q, digits[c + 3u * words]);
            }
            if (nd > 4u) {
                a4 = a4 + dot4_i8(q, digits[c + 4u * words]);
            }
            if (nd > 5u) {
                a5 = a5 + dot4_i8(q, digits[c + 5u * words]);
            }
            if (nd > 6u) {
                a6 = a6 + dot4_i8(q, digits[c + 6u * words]);
            }
            if (nd > 7u) {
                a7 = a7 + dot4_i8(q, digits[c + 7u * words]);
            }
        }
    }
    let b = lid * 8u;
    part[b] = a0;
    part[b + 1u] = a1;
    part[b + 2u] = a2;
    part[b + 3u] = a3;
    part[b + 4u] = a4;
    part[b + 5u] = a5;
    part[b + 6u] = a6;
    part[b + 7u] = a7;
    workgroupBarrier();
    for (var stride = LANES / 2u; stride > 0u; stride = stride >> 1u) {
        if (lane < stride) {
            let o = b + stride * 8u;
            for (var d = 0u; d < 8u; d = d + 1u) {
                part[b + d] = part[b + d] + part[o + d];
            }
        }
        workgroupBarrier();
    }
    if (live && lane == 0u) {
        var acc = vec2<u32>(0u, 0u);
        for (var d = 0u; d < 8u; d = d + 1u) {
            if (d < nd) {
                acc = add64(acc, shl64_i32(part[b + d], 8u * d));
            }
        }
        out[t * params.out_stride + params.out_offset + row] = dyadic_epilogue(acc, scales[row]);
    }
}
