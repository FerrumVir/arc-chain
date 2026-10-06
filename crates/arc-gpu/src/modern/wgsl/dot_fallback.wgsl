// Exact 4-way i8 x i8 dot product without the packed_4x8_integer_dot_product
// language extension: each byte is sign-extended with u32 arithmetic and the
// four products are summed in i32. |sum| <= 4 * 128 * 128 = 2^16, so nothing
// can overflow.
fn unpack_i8x4(w: u32) -> vec4<i32> {
    return vec4<i32>(sx8(w), sx8(w >> 8u), sx8(w >> 16u), sx8(w >> 24u));
}

fn dot4_i8(a: u32, b: u32) -> i32 {
    return dot(unpack_i8x4(a), unpack_i8x4(b));
}
