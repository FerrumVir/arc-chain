// Exact residual addition h += delta, refusing |h| > 2^62 (spec §5.8).
// Grid: x = element within a token (n per token), y = token in the batch.

struct ResidualParams {
    n: u32,
    pad0: u32,
    pad1: u32,
    pad2: u32,
    pad3: u32,
    pad4: u32,
    pad5: u32,
    pad6: u32,
}

struct Cursor {
    pos0: u32,
    count: u32,
    pad0: u32,
    pad1: u32,
    tokens: array<vec4<u32>, 16>,
}

@group(0) @binding(1) var<uniform> params: ResidualParams;
@group(0) @binding(4) var<uniform> cursor: Cursor;
@group(0) @binding(2) var<storage, read_write> hidden: array<vec2<u32>>;
@group(0) @binding(3) var<storage, read> delta: array<vec2<u32>>;

@compute @workgroup_size(64)
fn residual(@builtin(global_invocation_id) gid: vec3<u32>) {
    if (gid.x >= params.n || gid.y >= cursor.count) {
        return;
    }
    let i = gid.y * params.n + gid.x;
    let s = add128(i128_from_i64(hidden[i]), i128_from_i64(delta[i]));
    if (!act_ok128(s)) {
        flag(ST_RESIDUAL);
    }
    hidden[i] = vec2<u32>(s.x, s.y);
}
