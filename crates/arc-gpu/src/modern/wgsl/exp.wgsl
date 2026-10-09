// The correctly rounded exp table of spec §5.1: T[i] = rha(e^(-(4096 - i)/256) * 2^16),
// 4,097 entries in [0, 65536], uploaded from the CPU engine's own table (its
// BLAKE3 is checked on the host before upload).
@group(0) @binding(8) var<storage, read> exp_table: array<u32>;

// exp(x) in Q16 for an i64 x (spec §5.1): 65536 for x >= 0, 0 for
// x <= -16 * 2^16, otherwise linear interpolation with a floor shift.
fn exp_q16(x: vec2<u32>) -> u32 {
    if (!i64_is_neg(x)) {
        return 65536u;
    }
    // Here x < 0. x > -2^20 exactly when the high word is all ones and the low
    // word exceeds 2^32 - 2^20.
    if (x.y != 0xFFFFFFFFu || x.x <= 0xFFF00000u) {
        return 0u;
    }
    let offset = x.x - 0xFFF00000u; // x + 16 * 2^16, in [1, 2^20 - 1]
    let index = offset >> 8u;       // <= 4095
    let fraction = offset & 255u;
    let low = exp_table[index];
    let high = exp_table[index + 1u];
    return low + (((high - low) * fraction) >> 8u);
}
