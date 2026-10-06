// Append post-RoPE keys and raw values to one layer's i32 KV cache (spec §5.8),
// refusing values outside i32 exactly like the CPU's KvCache::push.

struct KvParams {
    d_kv: u32,
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

@group(0) @binding(1) var<uniform> params: KvParams;
@group(0) @binding(2) var<uniform> cursor: Cursor;
@group(0) @binding(3) var<storage, read> k_in: array<vec2<u32>>;
@group(0) @binding(4) var<storage, read> v_in: array<vec2<u32>>;
@group(0) @binding(5) var<storage, read_write> k_cache: array<i32>;
@group(0) @binding(6) var<storage, read_write> v_cache: array<i32>;

@compute @workgroup_size(64)
fn kv_store(@builtin(global_invocation_id) gid: vec3<u32>) {
    let e = gid.x;
    let t = gid.y;
    if (e >= params.d_kv || t >= cursor.count) {
        return;
    }
    let src = t * params.d_kv + e;
    let kv = k_in[src];
    let vv = v_in[src];
    if (!fits_i32(kv) || !fits_i32(vv)) {
        flag(ST_KV_RANGE);
    }
    let dst = (cursor.pos0 + t) * params.d_kv + e;
    k_cache[dst] = bitcast<i32>(kv.x);
    v_cache[dst] = bitcast<i32>(vv.x);
}
