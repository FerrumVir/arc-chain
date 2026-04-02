// UNTESTED — requires hardware validation on Apple Silicon
//
// SiLU gate activation: gate[i] = silu(gate[i]) * up[i]
// Ported from transformer.wgsl silu_mul kernel — must produce identical output.
//
// Integer SiLU approximation (matches WGSL exactly):
//   silu(g) = g > 0 ? g : g >> 2   (piecewise linear)
//   result  = (silu(g) * up[i]) >> 16

#include <metal_stdlib>
using namespace metal;

kernel void silu_mul(
    device int* gate [[buffer(0)]],
    device const int* up [[buffer(1)]],
    uint tid [[thread_position_in_grid]]
) {
    const int g = gate[tid];
    // Match WGSL: select(g >> 2, g, g > 0) — pass-through if positive, quarter if negative
    const int silu_g = (g > 0) ? g : (g >> 2);
    gate[tid] = (silu_g * up[tid]) >> 16;
}
