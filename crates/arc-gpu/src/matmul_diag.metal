#include <metal_stdlib>
using namespace metal;

// Diagnostic shader: write constant to output to verify buffer mapping.
// If output[0] == 12345 after dispatch, buffer(0) maps to binding(0).
kernel void diag_write(
    device int* output [[buffer(0)]],
    uint gid [[thread_position_in_grid]]
) {
    if (gid == 0) {
        output[0] = 12345;
        output[1] = 67890;
    }
}

// Diagnostic: 2 buffers, write input[0]+1 to output[0].
// Tests buffer(0)→binding(0) and buffer(1)→binding(1) mapping.
kernel void diag_copy(
    device const int* input [[buffer(0)]],
    device int* output [[buffer(1)]],
    uint gid [[thread_position_in_grid]]
) {
    if (gid == 0) {
        output[0] = input[0] + 1;
    }
}

// Diagnostic: 5 buffers matching matmul layout.
// Write buffer index markers to output to verify all mappings.
struct DiagParams {
    uint marker;
    uint _p1;
    uint _p2;
    uint _p3;
};

kernel void diag_5buf(
    device const int* buf0 [[buffer(0)]],
    device const int* buf1 [[buffer(1)]],
    device int* buf2 [[buffer(2)]],
    constant DiagParams& buf3 [[buffer(3)]],
    device const int* buf4 [[buffer(4)]],
    uint gid [[thread_position_in_grid]]
) {
    if (gid == 0) {
        buf2[0] = buf0[0];         // should be weights[0]
        buf2[1] = buf1[0];         // should be input[0]
        buf2[2] = int(buf3.marker); // should be params marker
        buf2[3] = buf4[0];         // should be scales[0]
        buf2[4] = 77777;           // sentinel
    }
}
