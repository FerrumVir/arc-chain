#include <metal_stdlib>
using namespace metal;

/// Native Metal i8 matmul — NO u32 packing overhead.
/// Uses char/char4 for direct i8 memory access.
/// Each thread: one output row. Input in threadgroup shared memory.

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
    uint gid [[thread_position_in_grid]],
    uint lid [[thread_position_in_threadgroup]],
    uint tg_size [[threads_per_threadgroup]]
) {
    // Load input into shared memory (max 11008 bytes)
    threadgroup char shared_input[11008];
    for (uint i = lid; i < params.in_size; i += tg_size) {
        shared_input[i] = input[i];
    }
    threadgroup_barrier(mem_flags::mem_threadgroup);

    if (gid >= params.out_size) return;

    uint row_off = gid * params.in_size;
    int acc0 = 0, acc1 = 0, acc2 = 0, acc3 = 0;

    // Process 16 chars per iteration using char4 vector loads
    uint vec_len = params.in_size / 16 * 16;
    for (uint j = 0; j < vec_len; j += 16) {
        char4 w0 = *reinterpret_cast<device const char4*>(weights + row_off + j);
        char4 w1 = *reinterpret_cast<device const char4*>(weights + row_off + j + 4);
        char4 w2 = *reinterpret_cast<device const char4*>(weights + row_off + j + 8);
        char4 w3 = *reinterpret_cast<device const char4*>(weights + row_off + j + 12);

        char4 i0 = *reinterpret_cast<threadgroup const char4*>(shared_input + j);
        char4 i1 = *reinterpret_cast<threadgroup const char4*>(shared_input + j + 4);
        char4 i2 = *reinterpret_cast<threadgroup const char4*>(shared_input + j + 8);
        char4 i3 = *reinterpret_cast<threadgroup const char4*>(shared_input + j + 12);

        acc0 += int(w0.x)*int(i0.x) + int(w0.y)*int(i0.y) + int(w0.z)*int(i0.z) + int(w0.w)*int(i0.w);
        acc1 += int(w1.x)*int(i1.x) + int(w1.y)*int(i1.y) + int(w1.z)*int(i1.z) + int(w1.w)*int(i1.w);
        acc2 += int(w2.x)*int(i2.x) + int(w2.y)*int(i2.y) + int(w2.z)*int(i2.z) + int(w2.w)*int(i2.w);
        acc3 += int(w3.x)*int(i3.x) + int(w3.y)*int(i3.y) + int(w3.z)*int(i3.z) + int(w3.w)*int(i3.w);
    }

    int acc = acc0 + acc1 + acc2 + acc3;

    // Scalar remainder
    for (uint j = vec_len; j < params.in_size; j++) {
        acc += int(weights[row_off + j]) * int(shared_input[j]);
    }

    int scale = scales[params.scale_offset + gid];
    output[gid] = acc * (scale >> 8);
}
