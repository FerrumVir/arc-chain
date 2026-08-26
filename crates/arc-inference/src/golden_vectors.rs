//! Cross-platform known-answer vectors for the production integer engine.
//!
//! These tests deliberately do not calculate their expectations from a second
//! run. The model recipe, input tokens, output tokens, and BLAKE3 digests are
//! committed in `tests/fixtures/integer_inference_kat.json`. A platform whose
//! SIMD, Rayon, attention, or shard path changes even one output bit fails
//! against the same reviewed constants.

use crate::cached_integer_model::{
    CachedIntegerModel, CachedLayer, I8Weights, KVCache, ModelConfig, ShardInput, ShardOutput,
};
use crate::integer_lut::ONE;
use arc_crypto::hash_bytes;
use rayon::ThreadPoolBuilder;
use serde::Deserialize;

const FIXTURE_JSON: &str = include_str!("../tests/fixtures/integer_inference_kat.json");

#[derive(Clone, Debug, Deserialize)]
struct GoldenFixture {
    schema: u32,
    name: String,
    model_seed: String,
    vocab_size: usize,
    d_model: usize,
    n_heads: usize,
    n_kv_heads: usize,
    d_ff: usize,
    n_layers: usize,
    max_seq: usize,
    sequence_tokens: Vec<u32>,
    generation_prompt: Vec<u32>,
    generation_max_tokens: u32,
    shard_boundaries: Vec<usize>,
    expected: GoldenExpected,
}

#[derive(Clone, Debug, Deserialize)]
struct GoldenExpected {
    model_weight_hash: String,
    next_tokens: Vec<u32>,
    logits_hashes: Vec<String>,
    kv_cache_hash: String,
    shard_hidden_hashes: Vec<String>,
    generated_tokens: Vec<u32>,
    generated_output_hash: String,
}

#[derive(Debug, PartialEq, Eq)]
struct SequenceResult {
    next_tokens: Vec<u32>,
    logits_hashes: Vec<String>,
    kv_cache_hash: String,
}

#[derive(Debug, PartialEq, Eq)]
struct ShardSequenceResult {
    sequence: SequenceResult,
    hidden_hashes: Vec<String>,
}

/// Fixed integer generator used only to materialize the synthetic weights.
/// Wrapping arithmetic and byte extraction have identical semantics on every
/// Rust target; no host float, RNG implementation, or endianness is involved.
struct FixtureRng(u64);

impl FixtureRng {
    fn next_u64(&mut self) -> u64 {
        self.0 = self
            .0
            .wrapping_mul(6_364_136_223_846_793_005)
            .wrapping_add(1_442_695_040_888_963_407);
        self.0
    }

    fn weights(&mut self, rows: usize, cols: usize) -> I8Weights {
        let data = (0..rows * cols)
            .map(|_| {
                let value = ((self.next_u64() >> 56) as i16) - 128;
                value.max(-127) as i8
            })
            .collect();
        let scales = (0..rows)
            .map(|_| 64 + ((self.next_u64() >> 32) % 449) as i64)
            .collect();
        I8Weights {
            data,
            scales,
            n_rows: rows,
            n_cols: cols,
        }
    }

    fn norm(&mut self, len: usize) -> Vec<i64> {
        (0..len)
            .map(|_| ONE + (self.next_u64() % 8_193) as i64 - 4_096)
            .collect()
    }
}

fn fixture() -> GoldenFixture {
    let fixture: GoldenFixture =
        serde_json::from_str(FIXTURE_JSON).expect("golden fixture must be valid JSON");
    assert_eq!(fixture.schema, 1, "unsupported golden fixture schema");
    fixture
}

fn parse_seed(seed: &str) -> u64 {
    let hex = seed.strip_prefix("0x").unwrap_or(seed);
    u64::from_str_radix(hex, 16).expect("model_seed must be a hexadecimal u64")
}

fn fixed_rope_tables(d_head: usize, max_seq: usize) -> (Vec<i64>, Vec<i64>) {
    // Q16 samples of a unit circle at multiples of pi/8. Each RoPE pair uses a
    // different integer multiple of that angle, giving non-trivial positions
    // without computing sin/cos through platform libm during test setup.
    const COS: [i64; 16] = [
        65_536, 60_547, 46_341, 25_080, 0, -25_080, -46_341, -60_547, -65_536, -60_547, -46_341,
        -25_080, 0, 25_080, 46_341, 60_547,
    ];
    const SIN: [i64; 16] = [
        0, 25_080, 46_341, 60_547, 65_536, 60_547, 46_341, 25_080, 0, -25_080, -46_341, -60_547,
        -65_536, -60_547, -46_341, -25_080,
    ];

    let half = d_head / 2;
    let mut cos = Vec::with_capacity(max_seq * half);
    let mut sin = Vec::with_capacity(max_seq * half);
    for position in 0..max_seq {
        for pair in 0..half {
            let angle = (position * (pair + 1)) % COS.len();
            cos.push(COS[angle]);
            sin.push(SIN[angle]);
        }
    }
    (cos, sin)
}

fn build_fixture_model(fixture: &GoldenFixture) -> CachedIntegerModel {
    assert_eq!(fixture.d_model % fixture.n_heads, 0);
    let d_head = fixture.d_model / fixture.n_heads;
    let d_kv = d_head * fixture.n_kv_heads;
    let mut rng = FixtureRng(parse_seed(&fixture.model_seed));

    let embedding_i8 = rng.weights(fixture.vocab_size, fixture.d_model);
    let mut embedding_q16 = Vec::with_capacity(fixture.vocab_size * fixture.d_model);
    for row in 0..fixture.vocab_size {
        let scale = embedding_i8.scales[row];
        for col in 0..fixture.d_model {
            embedding_q16.push((embedding_i8.data[row * fixture.d_model + col] as i64) * scale);
        }
    }
    let output_weight = rng.weights(fixture.vocab_size, fixture.d_model);

    let mut layers = Vec::with_capacity(fixture.n_layers);
    for _ in 0..fixture.n_layers {
        layers.push(CachedLayer {
            wq: rng.weights(fixture.d_model, fixture.d_model),
            wk: rng.weights(d_kv, fixture.d_model),
            wv: rng.weights(d_kv, fixture.d_model),
            wo: rng.weights(fixture.d_model, fixture.d_model),
            w_gate: rng.weights(fixture.d_ff, fixture.d_model),
            w_up: rng.weights(fixture.d_ff, fixture.d_model),
            w_down: rng.weights(fixture.d_model, fixture.d_ff),
            attn_norm: rng.norm(fixture.d_model),
            ffn_norm: rng.norm(fixture.d_model),
        });
    }

    let final_norm = rng.norm(fixture.d_model);
    let (rope_cos, rope_sin) = fixed_rope_tables(d_head, fixture.max_seq);

    CachedIntegerModel {
        config: ModelConfig {
            n_layers: fixture.n_layers,
            d_model: fixture.d_model,
            n_heads: fixture.n_heads,
            n_kv_heads: fixture.n_kv_heads,
            d_ff: fixture.d_ff,
            d_head,
            d_kv,
            vocab_size: fixture.vocab_size,
            // round(2^16 / sqrt(8)); the fixture fixes d_head=8.
            attn_scale: 23_170,
            rope_cos,
            rope_sin,
            max_seq: fixture.max_seq,
            eos_tokens: Vec::new(),
            bos_token: 1,
            chat_template: String::new(),
        },
        embedding_q16,
        embedding_i8,
        layers,
        final_norm,
        output_weight,
        vocab: (0..fixture.vocab_size)
            .map(|token| format!("kat_{token}"))
            .collect(),
        q4_layers: None,
        q4_output: None,
        i16_layers: None,
        i16_output: None,
        block_i8_layers: None,
        block_i8_output: None,
        ternary_layers: None,
        ternary_output: None,
        ternary_hybrid_layers: None,
        ternary_hybrid_output: None,
    }
}

fn hash_i64(values: &[i64]) -> String {
    let bytes: Vec<u8> = values
        .iter()
        .flat_map(|value| value.to_le_bytes())
        .collect();
    hex::encode(hash_bytes(&bytes).0)
}

fn hash_cache(cache: &KVCache) -> String {
    let mut bytes = Vec::new();
    bytes.extend_from_slice(&(cache.seq_len as u64).to_le_bytes());
    for (keys, values) in cache.k_data.iter().zip(&cache.v_data) {
        bytes.extend_from_slice(&(keys.len() as u64).to_le_bytes());
        for value in keys {
            bytes.extend_from_slice(&value.to_le_bytes());
        }
        bytes.extend_from_slice(&(values.len() as u64).to_le_bytes());
        for value in values {
            bytes.extend_from_slice(&value.to_le_bytes());
        }
    }
    hex::encode(hash_bytes(&bytes).0)
}

fn run_whole_model(model: &CachedIntegerModel, tokens: &[u32]) -> SequenceResult {
    let mut cache = KVCache::new(model.config.n_layers);
    let mut next_tokens = Vec::with_capacity(tokens.len());
    let mut logits_hashes = Vec::with_capacity(tokens.len());
    for &token in tokens {
        let logits = model.forward_one_token(token, &mut cache);
        let next = crate::integer_lut::argmax_i64(&logits) as u32;
        next_tokens.push(next);
        logits_hashes.push(hash_i64(&logits));
    }
    SequenceResult {
        next_tokens,
        logits_hashes,
        kv_cache_hash: hash_cache(&cache),
    }
}

fn run_full_shard(model: &CachedIntegerModel, tokens: &[u32]) -> SequenceResult {
    let mut cache = KVCache::new(model.config.n_layers);
    let mut next_tokens = Vec::with_capacity(tokens.len());
    let mut logits_hashes = Vec::with_capacity(tokens.len());
    for (position, &token) in tokens.iter().enumerate() {
        match model
            .forward_shard_token(
                ShardInput::Token(token),
                &mut cache,
                0,
                model.config.n_layers,
                position,
            )
            .expect("whole-model shard sequence must be contiguous")
        {
            ShardOutput::Token { id, logits_hash } => {
                next_tokens.push(id);
                logits_hashes.push(hex::encode(logits_hash.0));
            }
            ShardOutput::Hidden(_) => panic!("the final shard must return a token"),
        }
    }
    SequenceResult {
        next_tokens,
        logits_hashes,
        kv_cache_hash: hash_cache(&cache),
    }
}

fn run_split_shards(
    model: &CachedIntegerModel,
    tokens: &[u32],
    boundaries: &[usize],
) -> ShardSequenceResult {
    assert_eq!(boundaries.len(), 2, "fixture uses a three-way split");
    let first_end = boundaries[0];
    let second_end = boundaries[1];
    assert!(0 < first_end && first_end < second_end);
    assert!(second_end < model.config.n_layers);

    let mut cache = KVCache::new(model.config.n_layers);
    let mut next_tokens = Vec::with_capacity(tokens.len());
    let mut logits_hashes = Vec::with_capacity(tokens.len());
    let mut hidden_hashes = Vec::with_capacity(tokens.len() * 2);

    for (position, &token) in tokens.iter().enumerate() {
        let first_hidden = match model
            .forward_shard_token(ShardInput::Token(token), &mut cache, 0, first_end, position)
            .expect("first shard sequence must be contiguous")
        {
            ShardOutput::Hidden(hidden) => hidden,
            ShardOutput::Token { .. } => panic!("first shard must return hidden state"),
        };
        hidden_hashes.push(hash_i64(&first_hidden));

        let second_hidden = match model
            .forward_shard_token(
                ShardInput::Hidden(first_hidden),
                &mut cache,
                first_end,
                second_end,
                position,
            )
            .expect("middle shard sequence must be contiguous")
        {
            ShardOutput::Hidden(hidden) => hidden,
            ShardOutput::Token { .. } => panic!("middle shard must return hidden state"),
        };
        hidden_hashes.push(hash_i64(&second_hidden));

        match model
            .forward_shard_token(
                ShardInput::Hidden(second_hidden),
                &mut cache,
                second_end,
                model.config.n_layers,
                position,
            )
            .expect("final shard sequence must be contiguous")
        {
            ShardOutput::Token { id, logits_hash } => {
                next_tokens.push(id);
                logits_hashes.push(hex::encode(logits_hash.0));
            }
            ShardOutput::Hidden(_) => panic!("final shard must return a token"),
        }
    }

    ShardSequenceResult {
        sequence: SequenceResult {
            next_tokens,
            logits_hashes,
            kv_cache_hash: hash_cache(&cache),
        },
        hidden_hashes,
    }
}

fn expected_sequence(fixture: &GoldenFixture) -> SequenceResult {
    SequenceResult {
        next_tokens: fixture.expected.next_tokens.clone(),
        logits_hashes: fixture.expected.logits_hashes.clone(),
        kv_cache_hash: fixture.expected.kv_cache_hash.clone(),
    }
}

#[test]
fn golden_cached_integer_whole_model_is_thread_count_independent() {
    let fixture = fixture();
    assert_eq!(fixture.name, "cached-integer-i8-i16-v1");

    let scalar_pool = ThreadPoolBuilder::new()
        .num_threads(1)
        .build()
        .expect("one-thread pool");
    let scalar = scalar_pool.install(|| {
        let model = build_fixture_model(&fixture);
        assert_eq!(
            hex::encode(model.weight_hash().0),
            fixture.expected.model_weight_hash
        );
        run_whole_model(&model, &fixture.sequence_tokens)
    });

    let parallel_pool = ThreadPoolBuilder::new()
        .num_threads(4)
        .build()
        .expect("four-thread pool");
    let parallel_i16 = parallel_pool.install(|| {
        let mut model = build_fixture_model(&fixture);
        model.enable_i16();
        assert_eq!(
            model.effective_precision_label(),
            "INT16 integer (per-row, cross-platform deterministic)"
        );
        run_whole_model(&model, &fixture.sequence_tokens)
    });

    let expected = expected_sequence(&fixture);
    assert_eq!(scalar, expected, "one-thread I8 path drifted from the KAT");
    assert_eq!(
        parallel_i16, expected,
        "four-thread promoted-I16 path drifted from the KAT"
    );
}

#[test]
fn golden_cached_integer_three_way_shards_match_whole_model() {
    let fixture = fixture();
    let pool = ThreadPoolBuilder::new()
        .num_threads(4)
        .build()
        .expect("four-thread pool");

    let (full_shard, split_shards) = pool.install(|| {
        let mut full_model = build_fixture_model(&fixture);
        full_model.enable_i16();
        let full = run_full_shard(&full_model, &fixture.sequence_tokens);

        let mut split_model = build_fixture_model(&fixture);
        split_model.enable_i16();
        let split = run_split_shards(
            &split_model,
            &fixture.sequence_tokens,
            &fixture.shard_boundaries,
        );
        (full, split)
    });

    let expected = expected_sequence(&fixture);
    assert_eq!(
        full_shard, expected,
        "whole-model shard drifted from the KAT"
    );
    assert_eq!(
        split_shards.sequence, expected,
        "three-way shard pipeline drifted from the KAT"
    );
    assert_eq!(
        split_shards.hidden_hashes, fixture.expected.shard_hidden_hashes,
        "a shard-boundary hidden state drifted from the KAT"
    );
}

#[test]
fn golden_cached_integer_autoregressive_output_matches_known_answer() {
    let fixture = fixture();
    let pool = ThreadPoolBuilder::new()
        .num_threads(4)
        .build()
        .expect("four-thread pool");
    let (tokens, output_hash) = pool.install(|| {
        let mut model = build_fixture_model(&fixture);
        model.enable_i16();
        model.generate(
            &fixture.generation_prompt,
            fixture.generation_max_tokens,
            &[],
        )
    });

    assert_eq!(tokens, fixture.expected.generated_tokens);
    assert_eq!(
        hex::encode(output_hash.0),
        fixture.expected.generated_output_hash
    );
}
