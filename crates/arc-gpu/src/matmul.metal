#include <metal_stdlib>
using namespace metal;

// llama.cpp-style tiled matmul adapted for integer-only per-row INT8.
//
// Pattern from ggml-metal.metal (MIT license) with f16→int32 arithmetic:
// - 4 simdgroups per threadgroup (128 threads total)
// - Each simdgroup handles 1 output row
// - 32 threads split the inner dimension (simd_sum for reduction)
// - char4 vector loads (4 i8 per instruction, no u32 packing)
// - simd_sum hardware reduction (~2 cycles vs manual workgroup barrier)

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
    // Each simdgroup handles one row, 4 simdgroups per threadgroup = 4 rows
    const int row = tgpig.x * 4 + sgitg;
    if (row >= (int)params.out_size) return;

    const uint in_sz = params.in_size;
    device const char* w = weights + row * in_sz;

    // 32 threads split inner dimension: ceil division handles small sizes
    const uint chunk = (in_sz + 31u) / 32u;
    const uint start = tiisg * chunk;
    const uint end = min(start + chunk, in_sz);

    int acc = 0;

    // Walk through [start, end) with char4 vector loads where aligned
    uint j = start;
    // Scalar prefix: align to 4-byte boundary
    for (; j < end && (j & 3u) != 0u; j++) {
        acc += int(w[j]) * int(input[j]);
    }
    // char4 vector body: 4 i8 per load, no extraction overhead
    for (; j + 4u <= end; j += 4u) {
        char4 wv = *reinterpret_cast<device const char4*>(w + j);
        char4 iv = *reinterpret_cast<device const char4*>(input + j);
        acc += int(wv.x)*int(iv.x) + int(wv.y)*int(iv.y)
             + int(wv.z)*int(iv.z) + int(wv.w)*int(iv.w);
    }
    // Scalar suffix
    for (; j < end; j++) {
        acc += int(w[j]) * int(input[j]);
    }

    // simd_sum: hardware reduction across 32 threads in ~2 cycles
    int total = simd_sum(acc);

    // Thread 0 writes the scaled result
    if (tiisg == 0) {
        int scale = scales[params.scale_offset + row];
        output[row] = total * (scale >> 8);
    }
}
