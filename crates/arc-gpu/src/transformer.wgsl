// ─── GPU-Resident Integer Transformer ──────────────────────────────────────────
//
// Full forward pass on GPU: embed → (layernorm → QKV → RoPE → attention →
// output proj → residual → layernorm → FFN → residual) × N_layers → LM head.
//
// All intermediate data stays on GPU. Single command buffer per token.
// Activations: i32 (sufficient range for Q16 values in typical LLM range).
// Weights: packed i8 in u32 arrays. KV cache: packed i8 in u32 arrays.
//
// Dispatched as multiple @compute kernels chained in one command buffer.

// ─── Shared Types ─────────────────────────────────────────────────────────────

struct MatmulParams {
    in_size: u32,
    out_size: u32,
    scale_offset: u32, // offset into scales buffer for this matrix's per-row scales
    _pad: u32,
}

struct LayerNormParams {
    size: u32,   // d_model
    _p1: u32,
    _p2: u32,
    _p3: u32,
}

struct RopeParams {
    pos: u32,      // current sequence position
    d_head: u32,
    n_heads: u32,
    _pad: u32,
}

struct AttnParams {
    d_head: u32,
    n_heads: u32,
    n_kv_heads: u32,
    seq_len: u32,   // full sequence length (pos + 1)
    d_kv: u32,
    attn_scale: i32,  // 1/sqrt(d_head) in Q16
    _p1: u32,
    _p2: u32,
}

// ─── Extract signed i8 from packed u32 ────────────────────────────────────────

fn ext_i8(packed: u32, idx: u32) -> i32 {
    let byte = (packed >> (idx * 8u)) & 0xFFu;
    return i32(byte) - i32((byte >> 7u) * 256u);
}

// ─── Kernel: i8×i8→i32 Matmul ────────────────────────────────────────────────
// Each thread computes one output row.
// weights: packed i8, input: packed i8, output: i32 accumulators.
// Per-row scale applied after accumulation.

@group(0) @binding(0) var<storage, read> mm_weights: array<u32>;
@group(0) @binding(1) var<storage, read> mm_input: array<u32>;
@group(0) @binding(2) var<storage, read_write> mm_output: array<i32>;
@group(0) @binding(3) var<uniform> mm_params: MatmulParams;
@group(0) @binding(4) var<storage, read> mm_scales: array<i32>;

@compute @workgroup_size(256)
fn matmul(@builtin(global_invocation_id) gid: vec3<u32>) {
    let row = gid.x;
    if (row >= mm_params.out_size) { return; }

    let packed_cols = mm_params.in_size / 4u;
    let row_off = row * packed_cols;
    var acc0: i32 = 0;
    var acc1: i32 = 0;
    var acc2: i32 = 0;
    var acc3: i32 = 0;

    // 4-way ILP: process 4 packed u32s (16 i8 values) per iteration
    let quads = packed_cols / 4u;
    for (var j: u32 = 0u; j < quads; j = j + 1u) {
        let j4 = j * 4u;
        let w0 = mm_weights[row_off + j4];
        let w1 = mm_weights[row_off + j4 + 1u];
        let w2 = mm_weights[row_off + j4 + 2u];
        let w3 = mm_weights[row_off + j4 + 3u];
        let i0 = mm_input[j4];
        let i1 = mm_input[j4 + 1u];
        let i2 = mm_input[j4 + 2u];
        let i3 = mm_input[j4 + 3u];
        acc0 += ext_i8(w0, 0u) * ext_i8(i0, 0u) + ext_i8(w0, 1u) * ext_i8(i0, 1u)
              + ext_i8(w0, 2u) * ext_i8(i0, 2u) + ext_i8(w0, 3u) * ext_i8(i0, 3u);
        acc1 += ext_i8(w1, 0u) * ext_i8(i1, 0u) + ext_i8(w1, 1u) * ext_i8(i1, 1u)
              + ext_i8(w1, 2u) * ext_i8(i1, 2u) + ext_i8(w1, 3u) * ext_i8(i1, 3u);
        acc2 += ext_i8(w2, 0u) * ext_i8(i2, 0u) + ext_i8(w2, 1u) * ext_i8(i2, 1u)
              + ext_i8(w2, 2u) * ext_i8(i2, 2u) + ext_i8(w2, 3u) * ext_i8(i2, 3u);
        acc3 += ext_i8(w3, 0u) * ext_i8(i3, 0u) + ext_i8(w3, 1u) * ext_i8(i3, 1u)
              + ext_i8(w3, 2u) * ext_i8(i3, 2u) + ext_i8(w3, 3u) * ext_i8(i3, 3u);
    }
    var acc = acc0 + acc1 + acc2 + acc3;

    // Remainder
    for (var j = quads * 4u; j < packed_cols; j = j + 1u) {
        let w = mm_weights[row_off + j];
        let i = mm_input[j];
        acc += ext_i8(w, 0u) * ext_i8(i, 0u) + ext_i8(w, 1u) * ext_i8(i, 1u)
             + ext_i8(w, 2u) * ext_i8(i, 2u) + ext_i8(w, 3u) * ext_i8(i, 3u);
    }

    let scale = mm_scales[mm_params.scale_offset + row];
    mm_output[row] = acc * (scale >> 8);
}

// ─── Kernel: LayerNorm ────────────────────────────────────────────────────────
// Reduction over d_model elements using workgroup shared memory.
// input → output (normalized), gamma applied element-wise.

@group(0) @binding(0) var<storage, read> ln_input: array<i32>;
@group(0) @binding(1) var<storage, read_write> ln_output: array<i32>;
@group(0) @binding(2) var<storage, read> ln_gamma: array<i32>;
@group(0) @binding(3) var<uniform> ln_params: LayerNormParams;

var<workgroup> ln_shared: array<i32, 256>;

@compute @workgroup_size(256)
fn layernorm(
    @builtin(local_invocation_id) lid: vec3<u32>,
    @builtin(workgroup_id) wid: vec3<u32>
) {
    let size = ln_params.size;
    let tid = lid.x;

    // Step 1: compute partial sums for mean
    var local_sum: i32 = 0;
    for (var i = tid; i < size; i = i + 256u) {
        local_sum += ln_input[i];
    }
    ln_shared[tid] = local_sum;
    workgroupBarrier();

    // Reduce to get total sum
    for (var stride = 128u; stride > 0u; stride = stride >> 1u) {
        if (tid < stride) {
            ln_shared[tid] += ln_shared[tid + stride];
        }
        workgroupBarrier();
    }
    let mean = ln_shared[0] / i32(size);
    workgroupBarrier();

    // Step 2: compute variance
    var local_var: i32 = 0;
    for (var i = tid; i < size; i = i + 256u) {
        let d = ln_input[i] - mean;
        local_var += (d * d) >> 16; // Q16 shift
    }
    ln_shared[tid] = local_var;
    workgroupBarrier();

    for (var stride = 128u; stride > 0u; stride = stride >> 1u) {
        if (tid < stride) {
            ln_shared[tid] += ln_shared[tid + stride];
        }
        workgroupBarrier();
    }
    let variance = ln_shared[0] / i32(size);

    // isqrt approximation (Newton-Raphson would be complex in WGSL)
    // Use simple bit-shifting approximation
    let inv_std = 65536 / max(1, i32(sqrt(f32(max(1, variance + 1)))));
    workgroupBarrier();

    // Step 3: normalize and scale by gamma
    for (var i = tid; i < size; i = i + 256u) {
        let norm = ((ln_input[i] - mean) * inv_std) >> 16;
        ln_output[i] = (norm * ln_gamma[i]) >> 16;
    }
}

// ─── Kernel: Quantize i32 → packed i8 (for matmul input) ─────────────────────

@group(0) @binding(0) var<storage, read> q_input: array<i32>;
@group(0) @binding(1) var<storage, read_write> q_output: array<u32>;
@group(0) @binding(2) var<storage, read_write> q_scale: array<i32>; // single element: scale factor

var<workgroup> q_shared_max: array<i32, 256>;

@compute @workgroup_size(256)
fn quantize_i32_to_i8(
    @builtin(local_invocation_id) lid: vec3<u32>,
    @builtin(num_workgroups) nwg: vec3<u32>
) {
    let tid = lid.x;
    let size = nwg.x * 256u; // approximate

    // Find abs max using workgroup reduction
    var local_max: i32 = 0;
    for (var i = tid; i < 16384u; i = i + 256u) { // max d_ff
        let v = q_input[i];
        let av = select(-v, v, v >= 0);
        local_max = max(local_max, av);
    }
    q_shared_max[tid] = local_max;
    workgroupBarrier();

    for (var stride = 128u; stride > 0u; stride = stride >> 1u) {
        if (tid < stride) {
            q_shared_max[tid] = max(q_shared_max[tid], q_shared_max[tid + stride]);
        }
        workgroupBarrier();
    }
    let abs_max = max(1, q_shared_max[0]);
    let scale_factor = max(1, abs_max / 127);

    if (tid == 0u) {
        q_scale[0] = scale_factor;
    }
    workgroupBarrier();

    // Quantize and pack into u32
    for (var i = tid; i < 4096u; i = i + 256u) { // packed count
        let base = i * 4u;
        var packed: u32 = 0u;
        for (var k = 0u; k < 4u; k = k + 1u) {
            let v = clamp(q_input[base + k] / scale_factor, -127, 127);
            packed |= (u32(v) & 0xFFu) << (k * 8u);
        }
        q_output[i] = packed;
    }
}

// ─── Kernel: RoPE ─────────────────────────────────────────────────────────────

@group(0) @binding(0) var<storage, read_write> rope_data: array<i32>; // Q/K vectors
@group(0) @binding(1) var<storage, read> rope_cos: array<i32>;
@group(0) @binding(2) var<storage, read> rope_sin: array<i32>;
@group(0) @binding(3) var<uniform> rope_params: RopeParams;

@compute @workgroup_size(256)
fn rope(@builtin(global_invocation_id) gid: vec3<u32>) {
    let idx = gid.x; // index into d_head/2 elements across all heads
    let half = rope_params.d_head / 2u;
    let total = rope_params.n_heads * half;
    if (idx >= total) { return; }

    let head = idx / half;
    let i = idx % half;
    let pos = rope_params.pos;

    let base = head * rope_params.d_head;
    let cos_val = rope_cos[pos * half + i];
    let sin_val = rope_sin[pos * half + i];

    let x0 = rope_data[base + i];
    let x1 = rope_data[base + i + half];

    rope_data[base + i] = ((x0 * cos_val) >> 16) - ((x1 * sin_val) >> 16);
    rope_data[base + i + half] = ((x0 * sin_val) >> 16) + ((x1 * cos_val) >> 16);
}

// ─── Kernel: Attention (scores + softmax + weighted V) ────────────────────────
// One workgroup per head. Computes full attention for that head.

@group(0) @binding(0) var<storage, read> attn_q: array<i32>;        // [n_heads × d_head]
@group(0) @binding(1) var<storage, read> attn_k_cache: array<u32>;  // packed i8 KV cache
@group(0) @binding(2) var<storage, read> attn_v_cache: array<u32>;
@group(0) @binding(3) var<storage, read> attn_k_scales: array<i32>;
@group(0) @binding(4) var<storage, read> attn_v_scales: array<i32>;
@group(0) @binding(5) var<storage, read_write> attn_output: array<i32>; // [n_heads × d_head]
@group(0) @binding(6) var<uniform> attn_params: AttnParams;

var<workgroup> attn_scores: array<i32, 2048>; // max seq len

@compute @workgroup_size(32) // one thread per sequence position (up to 32)
fn attention(
    @builtin(workgroup_id) wid: vec3<u32>,
    @builtin(local_invocation_id) lid: vec3<u32>
) {
    let head = wid.x;
    let tid = lid.x;
    let dh = attn_params.d_head;
    let seq = attn_params.seq_len;
    let kv_h = head * attn_params.n_kv_heads / attn_params.n_heads;

    // Each thread computes one attention score Q·K[j]
    if (tid < seq) {
        let j = tid;
        var dot: i32 = 0;
        let q_off = head * dh;
        let k_off = j * attn_params.d_kv + kv_h * dh;
        let k_scale = attn_k_scales[j];

        let packed_dh = dh / 4u;
        for (var d = 0u; d < packed_dh; d = d + 1u) {
            let kp = attn_k_cache[k_off / 4u + d];
            // Q is i32, K is packed i8 — do i32 × i8
            for (var k = 0u; k < 4u; k = k + 1u) {
                dot += attn_q[q_off + d * 4u + k] * ext_i8(kp, k);
            }
        }
        attn_scores[j] = ((dot >> 16) * k_scale >> 16) * attn_params.attn_scale >> 16;
    }
    workgroupBarrier();

    // Softmax (thread 0 computes for the whole head)
    if (tid == 0u) {
        // Find max
        var max_val: i32 = -999999;
        for (var j = 0u; j < seq; j = j + 1u) {
            max_val = max(max_val, attn_scores[j]);
        }

        // exp and sum (approximation: exp(x) ≈ max(0, 1 + x/65536) for small x)
        var sum_exp: i32 = 0;
        for (var j = 0u; j < seq; j = j + 1u) {
            let x = attn_scores[j] - max_val;
            // Simple exp approximation: piecewise linear
            let e = select(0, 65536 + x, x > -65536 * 8);
            attn_scores[j] = max(0, e);
            sum_exp += max(0, e);
        }

        // Normalize
        if (sum_exp > 0) {
            for (var j = 0u; j < seq; j = j + 1u) {
                attn_scores[j] = (attn_scores[j] * 65536) / sum_exp;
            }
        }
    }
    workgroupBarrier();

    // Weighted V sum (each thread handles d_head/32 dimensions)
    let dims_per_thread = max(1u, dh / 32u);
    let d_start = tid * dims_per_thread;
    let d_end = min(dh, d_start + dims_per_thread);

    for (var d = d_start; d < d_end; d = d + 1u) {
        var acc: i32 = 0;
        for (var j = 0u; j < seq; j = j + 1u) {
            let v_off = j * attn_params.d_kv + kv_h * dh;
            let v_scale = attn_v_scales[j];
            let v_packed_idx = v_off / 4u + d / 4u;
            let v_val = ext_i8(attn_v_cache[v_packed_idx], d % 4u) * v_scale;
            acc += (attn_scores[j] * v_val) >> 16;
        }
        attn_output[head * dh + d] = acc;
    }
}

// ─── Kernel: SiLU gate * up ───────────────────────────────────────────────────

@group(0) @binding(0) var<storage, read_write> silu_gate: array<i32>;
@group(0) @binding(1) var<storage, read> silu_up: array<i32>;

@compute @workgroup_size(256)
fn silu_mul(@builtin(global_invocation_id) gid: vec3<u32>) {
    let i = gid.x;
    let g = silu_gate[i];
    let silu_g = select(g >> 2, g, g > 0); // SiLU approx
    silu_gate[i] = (silu_g * silu_up[i]) >> 16;
}

// ─── Kernel: Residual Add ─────────────────────────────────────────────────────

@group(0) @binding(0) var<storage, read_write> res_hidden: array<i32>;
@group(0) @binding(1) var<storage, read> res_add: array<i32>;

@compute @workgroup_size(256)
fn residual_add(@builtin(global_invocation_id) gid: vec3<u32>) {
    res_hidden[gid.x] += res_add[gid.x];
}

// ─── Kernel: Argmax ───────────────────────────────────────────────────────────

@group(0) @binding(0) var<storage, read> argmax_input: array<i32>;
@group(0) @binding(1) var<storage, read_write> argmax_result: array<u32>; // [best_idx, best_val]

var<workgroup> argmax_shared_idx: array<u32, 256>;
var<workgroup> argmax_shared_val: array<i32, 256>;

@compute @workgroup_size(256)
fn argmax(
    @builtin(local_invocation_id) lid: vec3<u32>,
    @builtin(num_workgroups) nwg: vec3<u32>
) {
    let tid = lid.x;
    let size = nwg.x * 256u;

    var best_idx: u32 = tid;
    var best_val: i32 = -2147483647;

    for (var i = tid; i < 32000u; i = i + 256u) {
        let v = argmax_input[i];
        if (v > best_val) {
            best_val = v;
            best_idx = i;
        }
    }

    argmax_shared_idx[tid] = best_idx;
    argmax_shared_val[tid] = best_val;
    workgroupBarrier();

    for (var stride = 128u; stride > 0u; stride = stride >> 1u) {
        if (tid < stride) {
            if (argmax_shared_val[tid + stride] > argmax_shared_val[tid]) {
                argmax_shared_val[tid] = argmax_shared_val[tid + stride];
                argmax_shared_idx[tid] = argmax_shared_idx[tid + stride];
            }
        }
        workgroupBarrier();
    }

    if (tid == 0u) {
        argmax_result[0] = argmax_shared_idx[0];
    }
}
