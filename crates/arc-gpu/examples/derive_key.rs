/// Extract the correct BLAKE3 derived key for "ARC-chain-tx-v1" and verify
/// against the blake3 crate's output.
///
/// BLAKE3 `new_derive_key(context)` works in two phases:
/// 1. Hash the context string with DERIVE_KEY_CONTEXT flag → 32-byte key
/// 2. Hash data using that key as initial CV with DERIVE_KEY_MATERIAL flag
///
/// We need the key from phase 1 to bake into the GPU shader.

const IV: [u32; 8] = [
    0x6A09E667, 0xBB67AE85, 0x3C6EF372, 0xA54FF53A,
    0x510E527F, 0x9B05688C, 0x1F83D9AB, 0x5BE0CD19,
];

const MSG_SCHEDULE: [[usize; 16]; 7] = [
    [0, 1, 2, 3, 4, 5, 6, 7, 8, 9, 10, 11, 12, 13, 14, 15],
    [2, 6, 3, 10, 7, 0, 4, 13, 1, 11, 12, 5, 9, 14, 15, 8],
    [3, 4, 10, 12, 13, 2, 7, 14, 6, 5, 9, 0, 11, 15, 8, 1],
    [10, 7, 12, 9, 14, 3, 13, 15, 4, 0, 11, 2, 5, 8, 1, 6],
    [12, 13, 9, 11, 15, 10, 14, 8, 7, 2, 5, 3, 0, 1, 6, 4],
    [9, 14, 11, 5, 8, 12, 15, 1, 13, 3, 0, 10, 2, 6, 4, 7],
    [11, 15, 5, 0, 1, 9, 8, 6, 14, 10, 2, 12, 3, 4, 7, 13],
];

// BLAKE3 flags
const CHUNK_START: u32 = 1;
const CHUNK_END: u32 = 2;
const ROOT: u32 = 8;
const DERIVE_KEY_CONTEXT: u32 = 32;
const DERIVE_KEY_MATERIAL: u32 = 64;

fn rotr(x: u32, n: u32) -> u32 {
    x.rotate_right(n)
}

fn g(state: &mut [u32; 16], a: usize, b: usize, c: usize, d: usize, mx: u32, my: u32) {
    state[a] = state[a].wrapping_add(state[b]).wrapping_add(mx);
    state[d] = rotr(state[d] ^ state[a], 16);
    state[c] = state[c].wrapping_add(state[d]);
    state[b] = rotr(state[b] ^ state[c], 12);
    state[a] = state[a].wrapping_add(state[b]).wrapping_add(my);
    state[d] = rotr(state[d] ^ state[a], 8);
    state[c] = state[c].wrapping_add(state[d]);
    state[b] = rotr(state[b] ^ state[c], 7);
}

fn compress(cv: &[u32; 8], block: &[u32; 16], counter: u64, block_len: u32, flags: u32) -> [u32; 16] {
    let mut state: [u32; 16] = [
        cv[0], cv[1], cv[2], cv[3],
        cv[4], cv[5], cv[6], cv[7],
        IV[0], IV[1], IV[2], IV[3],
        counter as u32, (counter >> 32) as u32, block_len, flags,
    ];

    for round in 0..7 {
        let s = &MSG_SCHEDULE[round];
        // Column step
        g(&mut state, 0, 4, 8,  12, block[s[0]],  block[s[1]]);
        g(&mut state, 1, 5, 9,  13, block[s[2]],  block[s[3]]);
        g(&mut state, 2, 6, 10, 14, block[s[4]],  block[s[5]]);
        g(&mut state, 3, 7, 11, 15, block[s[6]],  block[s[7]]);
        // Diagonal step
        g(&mut state, 0, 5, 10, 15, block[s[8]],  block[s[9]]);
        g(&mut state, 1, 6, 11, 12, block[s[10]], block[s[11]]);
        g(&mut state, 2, 7, 8,  13, block[s[12]], block[s[13]]);
        g(&mut state, 3, 4, 9,  14, block[s[14]], block[s[15]]);
    }

    // Finalization XOR
    for i in 0..8 {
        state[i] ^= state[i + 8];
        state[i + 8] ^= cv[i];
    }

    state
}

fn bytes_to_block(data: &[u8]) -> [u32; 16] {
    let mut block = [0u32; 16];
    for (i, chunk) in data.chunks(4).enumerate() {
        if i >= 16 { break; }
        let mut word = [0u8; 4];
        word[..chunk.len()].copy_from_slice(chunk);
        block[i] = u32::from_le_bytes(word);
    }
    block
}

fn main() {
    let context = b"ARC-chain-tx-v1";

    // Phase 1: Derive key from context string
    // Context is 15 bytes — fits in one block (64 bytes)
    let context_block = bytes_to_block(context);
    let flags = CHUNK_START | CHUNK_END | ROOT | DERIVE_KEY_CONTEXT;
    let context_result = compress(&IV, &context_block, 0, context.len() as u32, flags);

    // The key is the first 8 words of the compression output
    let key: [u32; 8] = [
        context_result[0], context_result[1], context_result[2], context_result[3],
        context_result[4], context_result[5], context_result[6], context_result[7],
    ];

    println!("// Correct BLAKE3 derived key for \"ARC-chain-tx-v1\"");
    println!("// Phase 1: compress(IV, context_block, 0, 15, CHUNK_START|CHUNK_END|ROOT|DERIVE_KEY_CONTEXT)");
    for (i, word) in key.iter().enumerate() {
        println!("const KEY{i}: u32 = 0x{word:08X}u;");
    }

    // Phase 2: Verify by hashing a known input
    // Hash 4 bytes [0x00, 0x00, 0x00, 0x00] using the derived key + DERIVE_KEY_MATERIAL
    let test_data = [0u8; 4];
    let data_block = bytes_to_block(&test_data);
    let data_flags = CHUNK_START | CHUNK_END | ROOT | DERIVE_KEY_MATERIAL;
    let data_result = compress(&key, &data_block, 0, test_data.len() as u32, data_flags);

    // Convert first 8 words to bytes
    let mut manual_hash = [0u8; 32];
    for i in 0..8 {
        manual_hash[i * 4..(i + 1) * 4].copy_from_slice(&data_result[i].to_le_bytes());
    }

    // Compare with blake3 crate
    let mut hasher = blake3::Hasher::new_derive_key("ARC-chain-tx-v1");
    hasher.update(&test_data);
    let crate_hash = hasher.finalize();

    println!("\n// Verification: hash of [0,0,0,0] (4 bytes)");
    println!("// Manual:  {}", hex::encode(manual_hash));
    println!("// Crate:   {crate_hash}");
    println!("// Match:   {}", hex::encode(manual_hash) == crate_hash.to_string());

    // Also verify with 128-byte payload
    let test_128 = [0u8; 128];
    // 128 bytes = 2 blocks of 64 bytes
    let block0 = bytes_to_block(&test_128[0..64]);
    let block1 = bytes_to_block(&test_128[64..128]);

    // Block 0: first block in chunk
    let r0 = compress(&key, &block0, 0, 64, CHUNK_START | DERIVE_KEY_MATERIAL);
    let cv1: [u32; 8] = [r0[0], r0[1], r0[2], r0[3], r0[4], r0[5], r0[6], r0[7]];

    // Block 1: last block in chunk + root
    let r1 = compress(&cv1, &block1, 0, 64, CHUNK_END | ROOT | DERIVE_KEY_MATERIAL);
    let mut manual_128 = [0u8; 32];
    for i in 0..8 {
        manual_128[i * 4..(i + 1) * 4].copy_from_slice(&r1[i].to_le_bytes());
    }

    let mut hasher2 = blake3::Hasher::new_derive_key("ARC-chain-tx-v1");
    hasher2.update(&test_128);
    let crate_128 = hasher2.finalize();

    println!("\n// Verification: hash of 128 zero bytes");
    println!("// Manual:  {}", hex::encode(manual_128));
    println!("// Crate:   {crate_128}");
    println!("// Match:   {}", hex::encode(manual_128) == crate_128.to_string());

    // 256-byte payload (4 blocks)
    let test_256 = [0u8; 256];
    let mut cv = key;
    for block_idx in 0..4u32 {
        let offset = (block_idx as usize) * 64;
        let blk = bytes_to_block(&test_256[offset..offset + 64]);
        let mut flags = DERIVE_KEY_MATERIAL;
        if block_idx == 0 { flags |= CHUNK_START; }
        if block_idx == 3 { flags |= CHUNK_END | ROOT; }
        let r = compress(&cv, &blk, 0, 64, flags);
        cv = [r[0], r[1], r[2], r[3], r[4], r[5], r[6], r[7]];
    }
    let mut manual_256 = [0u8; 32];
    for i in 0..8 {
        manual_256[i * 4..(i + 1) * 4].copy_from_slice(&cv[i].to_le_bytes());
    }

    let mut hasher3 = blake3::Hasher::new_derive_key("ARC-chain-tx-v1");
    hasher3.update(&test_256);
    let crate_256 = hasher3.finalize();

    println!("\n// Verification: hash of 256 zero bytes");
    println!("// Manual:  {}", hex::encode(manual_256));
    println!("// Crate:   {crate_256}");
    println!("// Match:   {}", hex::encode(manual_256) == crate_256.to_string());
}
