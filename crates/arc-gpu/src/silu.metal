#include <metal_stdlib>
using namespace metal;

kernel void silu_mul(
    device int* gate [[buffer(0)]],
    device const int* up [[buffer(1)]],
    uint tid [[thread_position_in_grid]]
) {
    const int g = gate[tid];
    const int silu_g = (g > 0) ? g : (g >> 2);
    gate[tid] = (silu_g * up[tid]) >> 16;
}
