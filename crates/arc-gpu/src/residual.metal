// UNTESTED — requires hardware validation on Apple Silicon
//
// Element-wise residual addition: hidden[i] += projected[i]
// Ported from transformer.wgsl residual_add kernel — must produce identical output.
//
// Dispatch: ceil(d_model / 256) threadgroups, 256 threads each.

#include <metal_stdlib>
using namespace metal;

kernel void residual_add(
    device int* hidden [[buffer(0)]],
    device const int* projected [[buffer(1)]],
    uint tid [[thread_position_in_grid]]
) {
    hidden[tid] += projected[tid];
}
