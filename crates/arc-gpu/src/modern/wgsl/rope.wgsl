// Rotary position embedding, split-half pairing (spec §5.5), in place:
//   u_i     = (a c - b s) >> 16,  u_{i+h} = (a s + b c) >> 16
// with one floor shift per output and the CPU's refusal of |u| > 2^62.
// |a|, |b| <= 2^62 and |c|, |s| <= 65537, so every product fits in i128.

struct RopeParams {
    heads: u32,
    d_head: u32,
    half: u32,
    width: u32,
    pad0: u32,
    pad1: u32,
    pad2: u32,
    pad3: u32,
}

struct Cursor {
    pos0: u32,
    count: u32,
    pad0: u32,
    pad1: u32,
    tokens: array<vec4<u32>, 16>,
}

@group(0) @binding(1) var<uniform> params: RopeParams;
@group(0) @binding(2) var<uniform> cursor: Cursor;
@group(0) @binding(3) var<storage, read_write> data: array<vec2<u32>>;
@group(0) @binding(4) var<storage, read> cos_table: array<i32>;
@group(0) @binding(5) var<storage, read> sin_table: array<i32>;

@compute @workgroup_size(64)
fn rope(@builtin(global_invocation_id) gid: vec3<u32>) {
    let idx = gid.x;
    let t = gid.y;
    if (idx >= params.heads * params.half || t >= cursor.count) {
        return;
    }
    let head = idx / params.half;
    let i = idx % params.half;
    let ia = t * params.width + head * params.d_head + i;
    let ib = ia + params.half;
    let at = (cursor.pos0 + t) * params.half + i;
    let c = cos_table[at];
    let s = sin_table[at];
    let a = i128_from_i64(data[ia]);
    let b = i128_from_i64(data[ib]);
    let ra = sar128(sub128(mul128_i32(a, c), mul128_i32(b, s)), 16u);
    let rb = sar128(add128(mul128_i32(a, s), mul128_i32(b, c)), 16u);
    if (!act_ok128(ra) || !act_ok128(rb)) {
        flag(ST_ROPE_OUTPUT);
    }
    data[ia] = vec2<u32>(ra.x, ra.y);
    data[ib] = vec2<u32>(rb.x, rb.y);
}
