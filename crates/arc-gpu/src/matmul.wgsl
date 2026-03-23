// Integer matmul compute shader for on-chain inference.
// Processes i8 weights packed as u32 (4 values per u32).
// Each thread computes one output row's dot product.
//
// Weights: [out_size × in_size/4] u32 (packed i8×4)
// Input: [in_size/4] u32 (packed i8×4)
// Output: [out_size] i32 (raw accumulator, scaled on CPU)

struct Params {
    in_size: u32,     // actual number of i8 elements
    out_size: u32,
}

@group(0) @binding(0) var<storage, read> weights: array<u32>;
@group(0) @binding(1) var<storage, read> input_data: array<u32>;
@group(0) @binding(2) var<storage, read_write> output_data: array<i32>;
@group(0) @binding(3) var<uniform> params: Params;

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
    var acc: i32 = 0;

    // Process 4 i8 values per iteration (one u32)
    for (var j: u32 = 0u; j < packed_cols; j = j + 1u) {
        let w = weights[row_off + j];
        let inp = input_data[j];

        // Extract 4 i8 pairs and multiply
        acc += extract_i8(w, 0u) * extract_i8(inp, 0u);
        acc += extract_i8(w, 1u) * extract_i8(inp, 1u);
        acc += extract_i8(w, 2u) * extract_i8(inp, 2u);
        acc += extract_i8(w, 3u) * extract_i8(inp, 3u);
    }

    output_data[row] = acc;
}
