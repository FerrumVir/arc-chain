// Exact 4-way i8 x i8 dot product with the WGSL built-in. dot4I8Packed is
// defined as the exact sum of four signed 8 x 8 products (no saturation), so it
// computes the same i32 as the fallback. Selected only when the WGSL compiler
// reports the packed_4x8_integer_dot_product language extension; the module
// then starts with the matching `requires` directive.
fn dot4_i8(a: u32, b: u32) -> i32 {
    return dot4I8Packed(a, b);
}
