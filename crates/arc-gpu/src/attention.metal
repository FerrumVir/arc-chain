#include <metal_stdlib>
using namespace metal;

struct AttnParams {
    uint d_head;
    uint n_heads;
    uint n_kv_heads;
    uint seq_len;
    uint d_kv;
    int attn_scale;
    uint _p1;
    uint _p2;
};

inline int ext_i8(uint packed, uint idx) {
    uint byte = (packed >> (idx * 8u)) & 0xFFu;
    return int(byte) - int((byte >> 7u) * 256u);
}

kernel void attention(
    device const int* q [[buffer(0)]],
    device const uint* k_cache [[buffer(1)]],
    device const uint* v_cache [[buffer(2)]],
    device const int* k_scales [[buffer(3)]],
    device const int* v_scales [[buffer(4)]],
    device int* output [[buffer(5)]],
    constant AttnParams& params [[buffer(6)]],
    uint3 wid [[threadgroup_position_in_grid]],
    uint tid [[thread_index_in_threadgroup]]
) {
    const uint head = wid.x;
    const uint dh = params.d_head;
    const uint seq = params.seq_len;
    const uint kv_h = head * params.n_kv_heads / params.n_heads;

    threadgroup int attn_scores[2048];
    threadgroup int smx_shared[32];

    for (uint j = tid; j < seq; j += 32u) {
        int dot = 0;
        const uint q_off = head * dh;
        const uint k_off = j * params.d_kv + kv_h * dh;
        const int k_scale = k_scales[j];
        const uint packed_dh = dh / 4u;

        for (uint d = 0u; d < packed_dh; d++) {
            uint kp = k_cache[k_off / 4u + d];
            for (uint k = 0u; k < 4u; k++) {
                dot += q[q_off + d * 4u + k] * ext_i8(kp, k);
            }
        }
        attn_scores[j] = ((dot >> 16) * k_scale >> 16) * params.attn_scale >> 16;
    }
    threadgroup_barrier(mem_flags::mem_threadgroup);

    int local_max = -999999;
    for (uint j = tid; j < seq; j += 32u) {
        local_max = max(local_max, attn_scores[j]);
    }
    smx_shared[tid] = local_max;
    threadgroup_barrier(mem_flags::mem_threadgroup);

    for (uint stride = 16u; stride > 0u; stride >>= 1u) {
        if (tid < stride) {
            smx_shared[tid] = max(smx_shared[tid], smx_shared[tid + stride]);
        }
        threadgroup_barrier(mem_flags::mem_threadgroup);
    }
    const int max_val = smx_shared[0];
    threadgroup_barrier(mem_flags::mem_threadgroup);

    int local_sum = 0;
    for (uint j = tid; j < seq; j += 32u) {
        const int x = attn_scores[j] - max_val;
        const int e = (x > -65536 * 8) ? max(0, 65536 + x) : 0;
        attn_scores[j] = e;
        local_sum += e;
    }
    smx_shared[tid] = local_sum;
    threadgroup_barrier(mem_flags::mem_threadgroup);

    for (uint stride = 16u; stride > 0u; stride >>= 1u) {
        if (tid < stride) {
            smx_shared[tid] += smx_shared[tid + stride];
        }
        threadgroup_barrier(mem_flags::mem_threadgroup);
    }
    const int sum_exp = smx_shared[0];
    threadgroup_barrier(mem_flags::mem_threadgroup);

    if (sum_exp > 0) {
        for (uint j = tid; j < seq; j += 32u) {
            attn_scores[j] = (attn_scores[j] * 65536) / sum_exp;
        }
    }
    threadgroup_barrier(mem_flags::mem_threadgroup);

    const uint dims_per_thread = max(1u, dh / 32u);
    const uint d_start = tid * dims_per_thread;
    const uint d_end = min(dh, d_start + dims_per_thread);

    for (uint d = d_start; d < d_end; d++) {
        int acc = 0;
        for (uint j = 0u; j < seq; j++) {
            const uint v_off = j * params.d_kv + kv_h * dh;
            const int v_scale = v_scales[j];
            const uint v_packed_idx = v_off / 4u + d / 4u;
            const int v_val = ext_i8(v_cache[v_packed_idx], d % 4u) * v_scale;
            acc += (attn_scores[j] * v_val) >> 16;
        }
        output[head * dh + d] = acc;
    }
}
