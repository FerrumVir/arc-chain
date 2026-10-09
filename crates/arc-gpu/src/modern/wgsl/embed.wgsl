// Embedding lookup (spec §5.3): e_j = (q_tj * mu_t) >> (k_t - 16).
//
// The INT8 embedding is split into at most four row chunks so that no binding
// exceeds the adapter's storage-binding limit (128 MiB on the WebGPU minimum).
// Unused chunk slots are bound to chunk 0 and never read.

struct EmbedParams {
    d_model: u32,
    row_words: u32,
    chunk_rows: u32,
    vocab: u32,
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

@group(0) @binding(1) var<uniform> params: EmbedParams;
@group(0) @binding(2) var<uniform> cursor: Cursor;
@group(0) @binding(3) var<storage, read> chunk0: array<u32>;
@group(0) @binding(4) var<storage, read> chunk1: array<u32>;
@group(0) @binding(5) var<storage, read> chunk2: array<u32>;
@group(0) @binding(6) var<storage, read> chunk3: array<u32>;
@group(0) @binding(7) var<storage, read> scales: array<vec2<u32>>;
@group(0) @binding(9) var<storage, read_write> hidden: array<vec2<u32>>;

fn embed_word(chunk: u32, index: u32) -> u32 {
    if (chunk == 0u) {
        return chunk0[index];
    }
    if (chunk == 1u) {
        return chunk1[index];
    }
    if (chunk == 2u) {
        return chunk2[index];
    }
    return chunk3[index];
}

@compute @workgroup_size(64)
fn embed(@builtin(global_invocation_id) gid: vec3<u32>) {
    let j = gid.x;
    let t = gid.y;
    if (j >= params.d_model || t >= cursor.count) {
        return;
    }
    let token = cursor.tokens[t >> 2u][t & 3u];
    if (token >= params.vocab) {
        flag(ST_EMBED_TOKEN);
        return;
    }
    let chunk = token / params.chunk_rows;
    let local_row = token % params.chunk_rows;
    let word = embed_word(chunk, local_row * params.row_words + (j >> 2u));
    let q = sx8(word >> ((j & 3u) * 8u));
    let sc = scales[token];
    // |q * mu| < 2^38; k is in [16, 62], so the shift is in [0, 46].
    let e = sar128(mul128_u32(i128_from_i32(q), sc.x), sc.y - 16u);
    hidden[t * params.d_model + j] = vec2<u32>(e.x, e.y);
}
