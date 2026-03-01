// BLAKE3 compute shader for ARC Chain GPU-accelerated transaction hashing.
// Each invocation hashes one transaction payload (up to 256 bytes) → 32-byte hash.
//
// BLAKE3 operates on 32-bit words with 7 rounds per compression.
// Domain separation: uses key derived from "ARC-chain-tx-v1".

// Input: padded transaction payloads (PAYLOAD_PAD bytes each)
@group(0) @binding(0) var<storage, read> input_data: array<u32>;
// Output: 32-byte hashes (8 x u32 each)
@group(0) @binding(1) var<storage, read_write> output_hashes: array<u32>;
// Uniforms: number of items, padded payload stride in u32s
@group(0) @binding(2) var<uniform> params: Params;
// Per-item actual byte lengths
@group(0) @binding(3) var<storage, read> lengths: array<u32>;

struct Params {
    num_items: u32,
    stride_u32s: u32,  // padded stride in u32 units (e.g., 64 for 256 bytes)
}

// BLAKE3 IV (same as BLAKE2s IV, derived from fractional parts of sqrt(2..9))
const IV0: u32 = 0x6A09E667u;
const IV1: u32 = 0xBB67AE85u;
const IV2: u32 = 0x3C6EF372u;
const IV3: u32 = 0xA54FF53Au;
const IV4: u32 = 0x510E527Fu;
const IV5: u32 = 0x9B05688Cu;
const IV6: u32 = 0x1F83D9ABu;
const IV7: u32 = 0x5BE0CD19u;

// Pre-computed key words from BLAKE3 derive_key("ARC-chain-tx-v1")
// Phase 1: compress(IV, context_block, 0, 15, CHUNK_START|CHUNK_END|ROOT|DERIVE_KEY_CONTEXT)
// Verified against blake3 crate — all test vectors match.
const KEY0: u32 = 0x57D5A959u;
const KEY1: u32 = 0xD9D66D61u;
const KEY2: u32 = 0xD1F8CF60u;
const KEY3: u32 = 0x4BA02D53u;
const KEY4: u32 = 0xB59ABDB6u;
const KEY5: u32 = 0x28492BB0u;
const KEY6: u32 = 0x408B10D9u;
const KEY7: u32 = 0xEBAFD313u;

// BLAKE3 flags
const CHUNK_START: u32 = 1u;
const CHUNK_END: u32 = 2u;
const ROOT: u32 = 8u;
const DERIVE_KEY_MATERIAL: u32 = 64u;

// BLAKE3 message schedule (sigma permutation for 7 rounds)
const MSG_SCHEDULE: array<array<u32, 16>, 7> = array<array<u32, 16>, 7>(
    array<u32, 16>(0, 1, 2, 3, 4, 5, 6, 7, 8, 9, 10, 11, 12, 13, 14, 15),
    array<u32, 16>(2, 6, 3, 10, 7, 0, 4, 13, 1, 11, 12, 5, 9, 14, 15, 8),
    array<u32, 16>(3, 4, 10, 12, 13, 2, 7, 14, 6, 5, 9, 0, 11, 15, 8, 1),
    array<u32, 16>(10, 7, 12, 9, 14, 3, 13, 15, 4, 0, 11, 2, 5, 8, 1, 6),
    array<u32, 16>(12, 13, 9, 11, 15, 10, 14, 8, 7, 2, 5, 3, 0, 1, 6, 4),
    array<u32, 16>(9, 14, 11, 5, 8, 12, 15, 1, 13, 3, 0, 10, 2, 6, 4, 7),
    array<u32, 16>(11, 15, 5, 0, 1, 9, 8, 6, 14, 10, 2, 12, 3, 4, 7, 13)
);

// Rotate right 32-bit
fn rotr(x: u32, n: u32) -> u32 {
    return (x >> n) | (x << (32u - n));
}

// BLAKE3 G mixing function
fn g(state: ptr<function, array<u32, 16>>, a: u32, b: u32, c: u32, d: u32, mx: u32, my: u32) {
    (*state)[a] = (*state)[a] + (*state)[b] + mx;
    (*state)[d] = rotr((*state)[d] ^ (*state)[a], 16u);
    (*state)[c] = (*state)[c] + (*state)[d];
    (*state)[b] = rotr((*state)[b] ^ (*state)[c], 12u);
    (*state)[a] = (*state)[a] + (*state)[b] + my;
    (*state)[d] = rotr((*state)[d] ^ (*state)[a], 8u);
    (*state)[c] = (*state)[c] + (*state)[d];
    (*state)[b] = rotr((*state)[b] ^ (*state)[c], 7u);
}

// One full round of BLAKE3 compression
fn round(state: ptr<function, array<u32, 16>>, msg: ptr<function, array<u32, 16>>, schedule: u32) {
    let s = MSG_SCHEDULE[schedule];

    // Column step
    g(state, 0u, 4u, 8u,  12u, (*msg)[s[0]],  (*msg)[s[1]]);
    g(state, 1u, 5u, 9u,  13u, (*msg)[s[2]],  (*msg)[s[3]]);
    g(state, 2u, 6u, 10u, 14u, (*msg)[s[4]],  (*msg)[s[5]]);
    g(state, 3u, 7u, 11u, 15u, (*msg)[s[6]],  (*msg)[s[7]]);

    // Diagonal step
    g(state, 0u, 5u, 10u, 15u, (*msg)[s[8]],  (*msg)[s[9]]);
    g(state, 1u, 6u, 11u, 12u, (*msg)[s[10]], (*msg)[s[11]]);
    g(state, 2u, 7u, 8u,  13u, (*msg)[s[12]], (*msg)[s[13]]);
    g(state, 3u, 4u, 9u,  14u, (*msg)[s[14]], (*msg)[s[15]]);
}

// BLAKE3 compression function
fn compress(
    cv: ptr<function, array<u32, 8>>,
    block: ptr<function, array<u32, 16>>,
    counter: u32,
    block_len: u32,
    flags: u32
) -> array<u32, 16> {
    var state: array<u32, 16> = array<u32, 16>(
        (*cv)[0], (*cv)[1], (*cv)[2], (*cv)[3],
        (*cv)[4], (*cv)[5], (*cv)[6], (*cv)[7],
        IV0, IV1, IV2, IV3,
        counter, 0u, block_len, flags
    );

    // 7 rounds
    for (var i: u32 = 0u; i < 7u; i = i + 1u) {
        round(&state, block, i);
    }

    // XOR upper and lower halves
    state[0] = state[0] ^ state[8];
    state[1] = state[1] ^ state[9];
    state[2] = state[2] ^ state[10];
    state[3] = state[3] ^ state[11];
    state[4] = state[4] ^ state[12];
    state[5] = state[5] ^ state[13];
    state[6] = state[6] ^ state[14];
    state[7] = state[7] ^ state[15];
    state[8]  = state[8]  ^ (*cv)[0];
    state[9]  = state[9]  ^ (*cv)[1];
    state[10] = state[10] ^ (*cv)[2];
    state[11] = state[11] ^ (*cv)[3];
    state[12] = state[12] ^ (*cv)[4];
    state[13] = state[13] ^ (*cv)[5];
    state[14] = state[14] ^ (*cv)[6];
    state[15] = state[15] ^ (*cv)[7];

    return state;
}

@compute @workgroup_size(256)
fn main(@builtin(global_invocation_id) gid: vec3<u32>) {
    let idx = gid.x;
    if (idx >= params.num_items) {
        return;
    }

    let input_offset = idx * params.stride_u32s;
    let output_offset = idx * 8u;  // 8 u32s = 32 bytes per hash

    // Actual byte length for this item
    let actual_bytes = lengths[idx];
    let actual_u32s = (actual_bytes + 3u) / 4u;

    // Initialize chaining value from pre-derived key
    var cv: array<u32, 8> = array<u32, 8>(
        KEY0, KEY1, KEY2, KEY3, KEY4, KEY5, KEY6, KEY7
    );

    // Process payload in 64-byte (16 u32) blocks
    let full_blocks = actual_u32s / 16u;
    let remaining_u32s = actual_u32s % 16u;
    let total_blocks = full_blocks + select(0u, 1u, remaining_u32s > 0u);

    // Handle empty input (0 bytes) — still need one compression
    let effective_blocks = max(total_blocks, 1u);

    for (var block_idx: u32 = 0u; block_idx < effective_blocks; block_idx = block_idx + 1u) {
        var msg: array<u32, 16>;

        let block_offset = input_offset + block_idx * 16u;
        let is_last = (block_idx == effective_blocks - 1u);

        // Determine how many u32s are valid in this block
        var block_u32s: u32;
        if (is_last && remaining_u32s > 0u) {
            block_u32s = remaining_u32s;
        } else if (actual_bytes == 0u) {
            block_u32s = 0u;
        } else {
            block_u32s = 16u;
        }

        // Load message words
        for (var i: u32 = 0u; i < 16u; i = i + 1u) {
            if (i < block_u32s) {
                msg[i] = input_data[block_offset + i];
            } else {
                msg[i] = 0u;
            }
        }

        // Compute block flags
        var flags: u32 = DERIVE_KEY_MATERIAL;
        if (block_idx == 0u) {
            flags = flags | CHUNK_START;
        }
        if (is_last) {
            flags = flags | CHUNK_END | ROOT;
        }

        // Block length in bytes
        var block_bytes: u32;
        if (is_last) {
            let remaining_bytes = actual_bytes - block_idx * 64u;
            block_bytes = min(remaining_bytes, 64u);
        } else {
            block_bytes = 64u;
        }
        // For empty input, block_bytes = 0
        if (actual_bytes == 0u) {
            block_bytes = 0u;
        }

        let result = compress(&cv, &msg, 0u, block_bytes, flags);

        // Update chaining value from first 8 words
        cv[0] = result[0];
        cv[1] = result[1];
        cv[2] = result[2];
        cv[3] = result[3];
        cv[4] = result[4];
        cv[5] = result[5];
        cv[6] = result[6];
        cv[7] = result[7];
    }

    // Write output hash (8 u32s = 32 bytes)
    output_hashes[output_offset + 0u] = cv[0];
    output_hashes[output_offset + 1u] = cv[1];
    output_hashes[output_offset + 2u] = cv[2];
    output_hashes[output_offset + 3u] = cv[3];
    output_hashes[output_offset + 4u] = cv[4];
    output_hashes[output_offset + 5u] = cv[5];
    output_hashes[output_offset + 6u] = cv[6];
    output_hashes[output_offset + 7u] = cv[7];
}
