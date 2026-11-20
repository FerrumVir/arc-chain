#include <metal_stdlib>
using namespace metal;

struct MatmulParams {
    uint in_size;
    uint out_size;
    uint scale_offset;
    uint _pad;
};

kernel void matmul_i8(
    device const char* weights [[buffer(0)]],
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
    device const char* w = weights + row * in_sz;

    const uint chunk = (in_sz + 31u) / 32u;
    const uint start = tiisg * chunk;
    const uint end = min(start + chunk, in_sz);

    int acc = 0;

    uint j = start;
    for (; j < end && (j & 3u) != 0u; j++) {
        acc += int(w[j]) * int(input[j]);
    }
    for (; j + 4u <= end; j += 4u) {
        char4 wv = *reinterpret_cast<device const char4*>(w + j);
        char4 iv = *reinterpret_cast<device const char4*>(input + j);
        acc += int(wv.x)*int(iv.x) + int(wv.y)*int(iv.y)
             + int(wv.z)*int(iv.z) + int(wv.w)*int(iv.w);
    }
    for (; j < end; j++) {
        acc += int(w[j]) * int(input[j]);
    }

    int total = simd_sum(acc);

    if (tiisg == 0) {
        int scale = scales[params.scale_offset + row];
        output[row] = total * (scale >> 8);
    }
}
