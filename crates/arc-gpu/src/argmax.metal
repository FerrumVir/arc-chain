// UNTESTED — requires hardware validation on Apple Silicon
//
// Argmax over i32 logits array — finds index of maximum value.
// Ported from transformer.wgsl argmax kernel — must produce identical output.
//
// 256 threads, 1 threadgroup. Each thread strides over vocab (hardcoded 32000).
// Threadgroup reduction finds global max index. Result written to result[0].

#include <metal_stdlib>
using namespace metal;

struct ArgmaxParams {
    uint vocab_size;
    uint _p1;
    uint _p2;
    uint _p3;
};

kernel void argmax_i32(
    device const int* logits [[buffer(0)]],
    device uint* result [[buffer(1)]],
    constant ArgmaxParams& params [[buffer(2)]],
    uint tid [[thread_index_in_threadgroup]]
) {
    const uint vocab = params.vocab_size;

    threadgroup uint shared_idx[256];
    threadgroup int shared_val[256];

    uint best_idx = tid;
    int best_val = -2147483647;

    for (uint i = tid; i < vocab; i += 256u) {
        const int v = logits[i];
        if (v > best_val) {
            best_val = v;
            best_idx = i;
        }
    }

    shared_idx[tid] = best_idx;
    shared_val[tid] = best_val;
    threadgroup_barrier(mem_flags::mem_threadgroup);

    for (uint stride = 128u; stride > 0u; stride >>= 1u) {
        if (tid < stride) {
            if (shared_val[tid + stride] > shared_val[tid]) {
                shared_val[tid] = shared_val[tid + stride];
                shared_idx[tid] = shared_idx[tid + stride];
            }
        }
        threadgroup_barrier(mem_flags::mem_threadgroup);
    }

    if (tid == 0u) {
        result[0] = shared_idx[0];
    }
}
