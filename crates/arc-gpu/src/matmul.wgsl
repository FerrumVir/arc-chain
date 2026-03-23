// Integer matmul compute shader for on-chain inference.
// Processes i8 weights packed as u32 (4 values per u32).
// Each thread computes one output row's dot product.
//
// Weights: [out_size × in_size/4] u32 (packed i8×4)
// Input: [in_size/4] u32 (packed i8×4)
// Output: [out_size] i32 (scaled accumulator)
// Scales: per-row i32 scale factors

struct Params {
    in_size: u32,     // actual number of i8 elements
    out_size: u32,
    scale_offset: u32,
    _pad: u32,
}

@group(0) @binding(0) var<storage, read> weights: array<u32>;
@group(0) @binding(1) var<storage, read> input_data: array<u32>;
@group(0) @binding(2) var<storage, read_write> output_data: array<i32>;
@group(0) @binding(3) var<uniform> params: Params;
@group(0) @binding(4) var<storage, read> scales: array<i32>;

// Extract signed i8 from packed u32
fn extract_i8(packed: u32, idx: u32) -> i32 {
    let shift = idx * 8u;
    let byte = (packed >> shift) & 0xFFu;
    // Sign extend: if bit 7 is set, subtract 256
    return i32(byte) - i32((byte >> 7u) * 256u);
}

@compute @workgroup_size(256)
fn main(@builtin(global_invocation_id) gid: vec3<u32>) {
    let row = gid.x;
    if (row >= params.out_size) {
        return;
    }

    let packed_cols = params.in_size / 4u;
    let row_off = row * packed_cols;
    var acc0: i32 = 0;
    var acc1: i32 = 0;
    var acc2: i32 = 0;
    var acc3: i32 = 0;

    // 4-way ILP: process 4 packed u32s (16 i8 values) per iteration
    let quads = packed_cols / 4u;
    for (var j: u32 = 0u; j < quads; j = j + 1u) {
        let j4 = j * 4u;
        let w0 = weights[row_off + j4];
        let w1 = weights[row_off + j4 + 1u];
        let w2 = weights[row_off + j4 + 2u];
        let w3 = weights[row_off + j4 + 3u];
        let i0 = input_data[j4];
        let i1 = input_data[j4 + 1u];
        let i2 = input_data[j4 + 2u];
        let i3 = input_data[j4 + 3u];
        acc0 += extract_i8(w0, 0u) * extract_i8(i0, 0u) + extract_i8(w0, 1u) * extract_i8(i0, 1u)
              + extract_i8(w0, 2u) * extract_i8(i0, 2u) + extract_i8(w0, 3u) * extract_i8(i0, 3u);
        acc1 += extract_i8(w1, 0u) * extract_i8(i1, 0u) + extract_i8(w1, 1u) * extract_i8(i1, 1u)
              + extract_i8(w1, 2u) * extract_i8(i1, 2u) + extract_i8(w1, 3u) * extract_i8(i1, 3u);
        acc2 += extract_i8(w2, 0u) * extract_i8(i2, 0u) + extract_i8(w2, 1u) * extract_i8(i2, 1u)
              + extract_i8(w2, 2u) * extract_i8(i2, 2u) + extract_i8(w2, 3u) * extract_i8(i2, 3u);
        acc3 += extract_i8(w3, 0u) * extract_i8(i3, 0u) + extract_i8(w3, 1u) * extract_i8(i3, 1u)
              + extract_i8(w3, 2u) * extract_i8(i3, 2u) + extract_i8(w3, 3u) * extract_i8(i3, 3u);
    }
    var acc = acc0 + acc1 + acc2 + acc3;

    // Remainder
    for (var j = quads * 4u; j < packed_cols; j = j + 1u) {
        let w = weights[row_off + j];
        let i = input_data[j];
        acc += extract_i8(w, 0u) * extract_i8(i, 0u) + extract_i8(w, 1u) * extract_i8(i, 1u)
             + extract_i8(w, 2u) * extract_i8(i, 2u) + extract_i8(w, 3u) * extract_i8(i, 3u);
    }

    let scale = scales[params.scale_offset + row];
    output_data[row] = acc * (scale >> 8);
}
