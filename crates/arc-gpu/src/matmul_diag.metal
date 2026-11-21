#include <metal_stdlib>
using namespace metal;

kernel void diag_write(
    device int* output [[buffer(0)]],
    uint gid [[thread_position_in_grid]]
) {
    if (gid == 0) {
        output[0] = 12345;
        output[1] = 67890;
    }
}

kernel void diag_copy(
    device const int* input [[buffer(0)]],
    device int* output [[buffer(1)]],
    uint gid [[thread_position_in_grid]]
) {
    if (gid == 0) {
        output[0] = input[0] + 1;
    }
}

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
        buf2[0] = buf0[0];
        buf2[1] = buf1[0];
        buf2[2] = int(buf3.marker);
        buf2[3] = buf4[0];
        buf2[4] = 77777;
    }
}
