#include <metal_stdlib>
using namespace metal;

/// Integer matmul for on-chain deterministic inference.
/// Computes output[row] = sum_j(weights[row*cols + j] * input[j])
/// Using native Metal char (int8_t) types — no packing overhead.
///
/// Each thread processes one output row. Input loaded into threadgroup
/// shared memory for reuse across all threads in the workgroup.
///
/// Weights: [out_size × in_size] as packed char (int8_t)
/// Input: [in_size] as packed char (int8_t)
/// Output: [out_size] as int (i32 accumulators, scaled on CPU)

struct MatmulParams {
    uint in_size;
    uint out_size;
};

kernel void matmul_i8(
    device const char* weights [[buffer(0)]],
    device const char* input [[buffer(1)]],
    device int* output [[buffer(2)]],
    constant MatmulParams& params [[buffer(3)]],
    uint gid [[thread_position_in_grid]],
    uint lid [[thread_position_in_threadgroup]],
    uint tg_size [[threads_per_threadgroup]]
) {
    // Load input into threadgroup shared memory (4KB for d=4096)
    threadgroup char shared_input[11008]; // max d_ff for 7B

    for (uint i = lid; i < params.in_size; i += tg_size) {
        shared_input[i] = input[i];
    }
    threadgroup_barrier(mem_flags::mem_threadgroup);

    if (gid >= params.out_size) return;

    // Compute dot product for this row using char4 SIMD loads
    uint row_off = gid * params.in_size;
    int acc = 0;

    // Process 16 elements per iteration using char4 (4 chars per vector)
    uint vec4_len = params.in_size / 16 * 16;
    for (uint j = 0; j < vec4_len; j += 16) {
        // Load 4 groups of 4 chars each (weights)
        char4 w0 = *reinterpret_cast<device const char4*>(weights + row_off + j);
        char4 w1 = *reinterpret_cast<device const char4*>(weights + row_off + j + 4);
        char4 w2 = *reinterpret_cast<device const char4*>(weights + row_off + j + 8);
        char4 w3 = *reinterpret_cast<device const char4*>(weights + row_off + j + 12);

        // Load 4 groups of 4 chars each (input from shared memory)
        char4 i0 = *reinterpret_cast<threadgroup const char4*>(shared_input + j);
        char4 i1 = *reinterpret_cast<threadgroup const char4*>(shared_input + j + 4);
        char4 i2 = *reinterpret_cast<threadgroup const char4*>(shared_input + j + 8);
        char4 i3 = *reinterpret_cast<threadgroup const char4*>(shared_input + j + 12);

        // Multiply and accumulate (16 MACs total)
        acc += int(w0.x) * int(i0.x) + int(w0.y) * int(i0.y) + int(w0.z) * int(i0.z) + int(w0.w) * int(i0.w);
        acc += int(w1.x) * int(i1.x) + int(w1.y) * int(i1.y) + int(w1.z) * int(i1.z) + int(w1.w) * int(i1.w);
        acc += int(w2.x) * int(i2.x) + int(w2.y) * int(i2.y) + int(w2.z) * int(i2.z) + int(w2.w) * int(i2.w);
        acc += int(w3.x) * int(i3.x) + int(w3.y) * int(i3.y) + int(w3.z) * int(i3.z) + int(w3.w) * int(i3.w);
    }

    // Scalar remainder
    for (uint j = vec4_len; j < params.in_size; j++) {
        acc += int(weights[row_off + j]) * int(shared_input[j]);
    }

    output[gid] = acc;
}
