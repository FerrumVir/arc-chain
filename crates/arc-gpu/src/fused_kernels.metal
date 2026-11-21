#include <metal_stdlib>
using namespace metal;

struct LNQParams {
    uint size;
    uint _p1;
    uint _p2;
    uint _p3;
};

kernel void layernorm_quantize(
    device const int* input [[buffer(0)]],
    device uint* output [[buffer(1)]],
    device const int* gamma [[buffer(2)]],
    constant LNQParams& params [[buffer(3)]],
    device int* out_scale [[buffer(4)]],
    uint tid [[thread_index_in_threadgroup]]
) {
    const uint size = params.size;

    threadgroup int shared[256];
    threadgroup int norm_cache[8192];

    int local_sum = 0;
    for (uint i = tid; i < size; i += 256u) {
        local_sum += input[i];
    }
    shared[tid] = local_sum;
    threadgroup_barrier(mem_flags::mem_threadgroup);

    for (uint stride = 128u; stride > 0u; stride >>= 1u) {
        if (tid < stride) shared[tid] += shared[tid + stride];
        threadgroup_barrier(mem_flags::mem_threadgroup);
    }
    const int mean = shared[0] / int(size);
    threadgroup_barrier(mem_flags::mem_threadgroup);

    int local_var = 0;
    for (uint i = tid; i < size; i += 256u) {
        int d = input[i] - mean;
        local_var += (d * d) >> 16;
    }
    shared[tid] = local_var;
    threadgroup_barrier(mem_flags::mem_threadgroup);

    for (uint stride = 128u; stride > 0u; stride >>= 1u) {
        if (tid < stride) shared[tid] += shared[tid + stride];
        threadgroup_barrier(mem_flags::mem_threadgroup);
    }
    const int variance = shared[0] / int(size);
    const int inv_std = 65536 / max(1, int(sqrt(float(max(1, variance + 1)))));
    threadgroup_barrier(mem_flags::mem_threadgroup);

    int local_max = 0;
    for (uint i = tid; i < size; i += 256u) {
        int norm = ((input[i] - mean) * inv_std) >> 16;
        int val = (norm * gamma[i]) >> 16;
        norm_cache[i] = val;
        int av = val >= 0 ? val : -val;
        local_max = max(local_max, av);
    }
    shared[tid] = local_max;
    threadgroup_barrier(mem_flags::mem_threadgroup);

    for (uint stride = 128u; stride > 0u; stride >>= 1u) {
        if (tid < stride) shared[tid] = max(shared[tid], shared[tid + stride]);
        threadgroup_barrier(mem_flags::mem_threadgroup);
    }
    const int abs_max = max(1, shared[0]);
    const int scale_factor = max(1, abs_max / 127);

    if (tid == 0u) {
        out_scale[0] = scale_factor;
    }
    threadgroup_barrier(mem_flags::mem_threadgroup);

    const uint packed_size = (size + 3u) / 4u;
    for (uint i = tid; i < packed_size; i += 256u) {
        uint base = i * 4u;
        uint packed = 0u;
        for (uint k = 0u; k < 4u; k++) {
            if (base + k < size) {
                int q = clamp(norm_cache[base + k] / scale_factor, -127, 127);
                packed |= (uint(q) & 0xFFu) << (k * 8u);
            }
        }
        output[i] = packed;
    }
}

struct MatmulParams {
    uint in_size;
    uint out_size;
    uint scale_offset;
    uint _pad;
};

kernel void matmul_i4(
    device const uchar* weights [[buffer(0)]],
    device const char* input [[buffer(1)]],
    device int* output [[buffer(2)]],
    constant MatmulParams& params [[buffer(3)]],
    device const int* scales [[buffer(4)]],
    uint3 tgpig [[threadgroup_position_in_grid]],
    ushort tiisg [[thread_index_in_simdgroup]],
    ushort sgitg [[simdgroup_index_in_threadgroup]]
) {
    const int row = tgpig.x * 4 + sgitg;
    if (row >= (int)params.out_size) return;

    const uint in_sz = params.in_size;
    const uint byte_cols = in_sz / 2u;
    device const uchar* w = weights + row * byte_cols;

    const uint chunk = (byte_cols + 31u) / 32u;
    const uint start = tiisg * chunk;
    const uint end = min(start + chunk, byte_cols);

    int acc = 0;

    for (uint j = start; j < end; j++) {
        uchar packed = w[j];

        int w_lo = int(packed & 0xFu) - 8;
        int w_hi = int((packed >> 4u) & 0xFu) - 8;

        acc += w_lo * int(input[j * 2u]) + w_hi * int(input[j * 2u + 1u]);
    }

    int total = simd_sum(acc);

    if (tiisg == 0) {
        int scale = scales[params.scale_offset + row];
        output[row] = total * (scale >> 8);
    }
}
