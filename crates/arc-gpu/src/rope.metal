// UNTESTED — requires hardware validation on Apple Silicon
//
// RoPE (Rotary Position Embedding) for Q and K vectors.
// Ported from transformer.wgsl rope kernel — must produce identical output.
//
// Layout: data contains [n_heads x d_head] i32 values.
// Each thread handles one (i, i+half) pair across all heads.
// cos/sin tables are precomputed Q16 fixed-point, indexed by [pos * half + i].

#include <metal_stdlib>
using namespace metal;

struct RopeParams {
    uint pos;
    uint d_head;
    uint n_heads;
    uint _pad;
};

kernel void rope_apply(
    device int* data [[buffer(0)]],
    device const int* cos_table [[buffer(1)]],
    device const int* sin_table [[buffer(2)]],
    constant RopeParams& params [[buffer(3)]],
    uint tid [[thread_position_in_grid]]
) {
    const uint half_d = params.d_head / 2u;
    const uint total = params.n_heads * half_d;
    if (tid >= total) return;

    const uint head = tid / half_d;
    const uint i = tid % half_d;
    const uint base = head * params.d_head;

    const int cos_val = cos_table[params.pos * half_d + i];
    const int sin_val = sin_table[params.pos * half_d + i];

    const int x0 = data[base + i];
    const int x1 = data[base + i + half_d];

    // Fixed-point Q16 rotation — matches WGSL: separate >> 16 per term
    data[base + i]          = ((x0 * cos_val) >> 16) - ((x1 * sin_val) >> 16);
    data[base + i + half_d] = ((x0 * sin_val) >> 16) + ((x1 * cos_val) >> 16);
}
