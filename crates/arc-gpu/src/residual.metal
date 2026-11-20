#include <metal_stdlib>
using namespace metal;

kernel void residual_add(
    device int* hidden [[buffer(0)]],
    device const int* projected [[buffer(1)]],
    uint tid [[thread_position_in_grid]]
) {
    hidden[tid] += projected[tid];
}
