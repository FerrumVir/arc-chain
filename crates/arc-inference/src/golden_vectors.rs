//! Cross-platform known-answer vectors for the production integer engine.
//!
//! These tests deliberately do not calculate their expectations from a second
//! run. The model recipe, input tokens, output tokens, and BLAKE3 digests are
//! committed in `tests/fixtures/integer_inference_kat.json`. A platform whose
//! SIMD, Rayon, attention, or shard path changes even one output bit fails
//! against the same reviewed constants.

use crate::cached_integer_model::{
    ArithmeticProfile, CachedIntegerModel, CachedLayer, I8Weights, KVCache, ModelConfig,
    ShardInput, ShardOutput,
};
use crate::integer_lut::ONE;
use arc_crypto::hash_bytes;
use rayon::ThreadPoolBuilder;
use serde::Deserialize;

// Every execution mode (one pass, chunked, restored prefix, speculative,
// stage splits, thread counts) held to the token-by-token run on this model.
mod execution_modes;

// Exact speculative decoding with an external drafter: whatever the drafter
// proposes, the output equals plain decoding.
mod draft_verify;

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
            arithmetic_profile: ArithmeticProfile::LegacySplitHalfV0,
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
    let expected = expected_sequence(&fixture);

    // Exercise the exact production compute primitive under the widths users
    // can select with --threads / POST /node/threads. A scheduler or reduction
    // change must never alter tokens, logits, or KV-cache bytes.
    for threads in [1, 2, 4] {
        let pool = ThreadPoolBuilder::new()
            .num_threads(threads)
            .build()
            .expect("determinism test pool");

        let (i8, promoted_i16) = pool.install(|| {
            let model = build_fixture_model(&fixture);
            assert_eq!(
                hex::encode(model.weight_hash().0),
                fixture.expected.model_weight_hash
            );
            let i8 = run_whole_model(&model, &fixture.sequence_tokens);

            let mut model = build_fixture_model(&fixture);
            model.enable_i16();
            assert_eq!(
                model.effective_precision_label(),
                "INT16 integer (per-row, cross-platform deterministic)"
            );
            let promoted_i16 = run_whole_model(&model, &fixture.sequence_tokens);
            (i8, promoted_i16)
        });

        assert_eq!(
            i8, expected,
            "{threads}-thread I8 path drifted from the KAT"
        );
        assert_eq!(
            promoted_i16, expected,
            "{threads}-thread promoted-I16 path drifted from the KAT"
        );
    }
}

#[test]
fn golden_cached_integer_three_way_shards_match_whole_model() {
    let fixture = fixture();
    let expected = expected_sequence(&fixture);

    for threads in [1, 2, 4] {
        let pool = ThreadPoolBuilder::new()
            .num_threads(threads)
            .build()
            .expect("determinism test pool");

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

        assert_eq!(
            full_shard, expected,
            "{threads}-thread whole-model shard drifted from the KAT"
        );
        assert_eq!(
            split_shards.sequence, expected,
            "{threads}-thread three-way shard pipeline drifted from the KAT"
        );
        assert_eq!(
            split_shards.hidden_hashes, fixture.expected.shard_hidden_hashes,
            "{threads}-thread shard-boundary hidden state drifted from the KAT"
        );
    }
}

#[test]
fn golden_cached_integer_autoregressive_output_matches_known_answer() {
    let fixture = fixture();

    for threads in [1, 2, 4] {
        let pool = ThreadPoolBuilder::new()
            .num_threads(threads)
            .build()
            .expect("determinism test pool");
        let (tokens, output_hash) = pool.install(|| {
            let mut model = build_fixture_model(&fixture);
            model.enable_i16();
            model.generate(
                &fixture.generation_prompt,
                fixture.generation_max_tokens,
                &[],
            )
        });

        assert_eq!(
            tokens, fixture.expected.generated_tokens,
            "{threads}-thread generation changed tokens"
        );
        assert_eq!(
            hex::encode(output_hash.0),
            fixture.expected.generated_output_hash,
            "{threads}-thread generation changed the output hash"
        );
    }
}

// ── Operator vectors from the independent reference ─────────────────────────
//
// `integer_operator_kat.json` is produced by `scripts/arc_conformance`, a
// Python executor written from docs/protocol/integer-profile-contract-v1.md.
// These tests hold the engine to it, so agreement is between two separately
// written implementations rather than a run compared with its own output.

const OPERATOR_JSON: &str = include_str!("../tests/fixtures/integer_operator_kat.json");

fn operators() -> serde_json::Value {
    let document: serde_json::Value =
        serde_json::from_str(OPERATOR_JSON).expect("operator vectors must be valid JSON");
    assert_eq!(document["schema"], 1, "unsupported operator vector schema");
    document
}

fn ints(value: &serde_json::Value) -> Vec<i64> {
    value
        .as_array()
        .expect("vector field must be an array")
        .iter()
        .map(|v| v.as_i64().expect("vector entries must be i64"))
        .collect()
}

fn token_ids(value: &serde_json::Value) -> Vec<u32> {
    ints(value)
        .into_iter()
        .map(|v| u32::try_from(v).expect("token ids are u32"))
        .collect()
}

fn text(value: &serde_json::Value) -> &str {
    value.as_str().expect("vector field must be a string")
}

#[test]
fn integer_operators_match_the_independent_reference() {
    use crate::cached_integer_model::{
        apply_rope, apply_rope_interleaved, flash_attention_i64, layernorm, matmul_fast,
        select_next_token_with_repetition_penalty, silu_i64,
    };
    use crate::integer_lut::{EXP_LUT, argmax_i64, integer_exp, integer_isqrt};

    let doc = operators();
    assert_eq!(hash_i64(&EXP_LUT[..]), text(&doc["exp_lut_blake3"]));
    for pair in doc["integer_exp"].as_array().unwrap() {
        let pair = ints(pair);
        assert_eq!(integer_exp(pair[0]), pair[1], "exp({})", pair[0]);
    }
    for pair in doc["integer_isqrt"].as_array().unwrap() {
        let pair = ints(pair);
        assert_eq!(integer_isqrt(pair[0]), pair[1], "isqrt({})", pair[0]);
    }
    for (index, case) in doc["rms_norm"].as_array().unwrap().iter().enumerate() {
        let output = layernorm(&ints(&case["input"]), &ints(&case["gamma"]));
        assert_eq!(output, ints(&case["output"]), "rms_norm case {index}");
    }
    for pair in doc["silu"].as_array().unwrap() {
        let pair = ints(pair);
        assert_eq!(silu_i64(pair[0]), pair[1], "silu({})", pair[0]);
    }
    for (index, case) in doc["matmul_rows"].as_array().unwrap().iter().enumerate() {
        let rows = case["rows"].as_u64().unwrap() as usize;
        let cols = case["cols"].as_u64().unwrap() as usize;
        let weights = I8Weights {
            data: ints(&case["weights"])
                .into_iter()
                .map(|w| w as i8)
                .collect(),
            scales: ints(&case["scales"]),
            n_rows: rows,
            n_cols: cols,
        };
        let output = matmul_fast(&weights, &ints(&case["input"]), cols, rows);
        assert_eq!(output, ints(&case["output"]), "projection case {index}");
    }
    let rope = &doc["rope"];
    let d_head = rope["d_head"].as_u64().unwrap() as usize;
    let (cos, sin) = (ints(&rope["cos"]), ints(&rope["sin"]));
    for (index, case) in rope["cases"].as_array().unwrap().iter().enumerate() {
        let mut values = ints(&case["input"]);
        let pos = case["pos"].as_u64().unwrap() as usize;
        match text(&case["layout"]) {
            "split_half" => apply_rope(&mut values, pos, d_head, &cos, &sin),
            "interleaved" => apply_rope_interleaved(&mut values, pos, d_head, &cos, &sin),
            other => panic!("unknown RoPE layout {other}"),
        }
        assert_eq!(values, ints(&case["output"]), "RoPE case {index}");
    }
    for case in doc["attention_head"].as_array().unwrap() {
        let q = ints(&case["q"]);
        let keys: Vec<i64> = case["keys"]
            .as_array()
            .unwrap()
            .iter()
            .flat_map(ints)
            .collect();
        let values: Vec<i64> = case["values"]
            .as_array()
            .unwrap()
            .iter()
            .flat_map(ints)
            .collect();
        let positions = keys.len() / q.len();
        let output = flash_attention_i64(
            &q,
            &keys,
            &values,
            q.len(),
            0,
            q.len(),
            positions,
            case["attn_scale"].as_i64().unwrap(),
        );
        assert_eq!(
            output,
            ints(&case["output"]),
            "attention: {}",
            text(&case["note"])
        );
    }
    for case in doc["repetition_penalty"].as_array().unwrap() {
        let mut logits = ints(&case["logits"]);
        let token =
            select_next_token_with_repetition_penalty(&mut logits, &token_ids(&case["generated"]));
        let note = text(&case["note"]);
        assert_eq!(logits, ints(&case["penalized"]), "penalty logits: {note}");
        assert_eq!(
            i64::from(token),
            case["token"].as_i64().unwrap(),
            "penalty token: {note}"
        );
    }
    for case in doc["argmax"].as_array().unwrap() {
        let index = argmax_i64(&ints(&case["values"]));
        assert_eq!(
            index as i64,
            case["index"].as_i64().unwrap(),
            "argmax {:?}",
            case["values"]
        );
    }
}

#[test]
fn interleaved_profile_and_generation_v2_match_the_independent_reference() {
    let doc = operators();
    let section = &doc["interleaved_generation_v2"];
    let fixture = fixture();
    for threads in [1, 4] {
        let pool = ThreadPoolBuilder::new()
            .num_threads(threads)
            .build()
            .expect("determinism test pool");
        pool.install(|| {
            let mut model = build_fixture_model(&fixture);
            assert_eq!(
                hex::encode(model.weight_hash().0),
                text(&section["model_weight_hash_before_row_rewrite"])
            );
            model
                .canonicalize_gguf_interleaved_rope_rows()
                .expect("the fixture is a complete canonical I8 model");
            assert_eq!(model.arithmetic_profile(), text(&section["profile"]));

            let sequence = run_whole_model(&model, &token_ids(&section["sequence_tokens"]));
            assert_eq!(sequence.next_tokens, token_ids(&section["next_tokens"]));
            let expected_logits: Vec<String> = section["logits_hashes"]
                .as_array()
                .unwrap()
                .iter()
                .map(|v| text(v).to_owned())
                .collect();
            assert_eq!(sequence.logits_hashes, expected_logits, "{threads} threads");
            assert_eq!(
                sequence.kv_cache_hash,
                text(&section["kv_cache_hash_split_half_layout"])
            );

            for run in section["generation_v2"].as_array().unwrap() {
                let prompt = token_ids(&run["prompt"]);
                let max_tokens = run["max_tokens"].as_u64().unwrap() as u32;
                let eos = token_ids(&run["eos_tokens"]);
                let (tokens, hash) = if run["repetition_penalty"].as_bool().unwrap() {
                    model.try_generate_v2(&prompt, max_tokens, &eos)
                } else {
                    model.try_generate_v2_greedy(&prompt, max_tokens, &eos)
                }
                .expect("the vectors fit the fixture context window");
                assert_eq!(tokens, token_ids(&run["tokens"]), "{threads} threads");
                assert_eq!(hex::encode(hash.0), text(&run["output_hash"]));
            }
        });
    }
}

#[cfg(feature = "candle")]
mod gguf_preparation {
    //! A tiny Llama GGUF, written with candle and loaded by the production
    //! canonical loader, must prepare to the state the contract's §8 rules
    //! produce in the independent reference, and then run identically.

    use super::*;
    use crate::cached_integer_model::{
        GGUF_INTERLEAVED_ROPE_I8_INFERENCE_PROFILE, load_cached_model_canonical_i8_interleaved_rope,
    };
    use candle_core::quantized::{GgmlDType, QTensor, gguf_file};
    use candle_core::{Device, Tensor};

    struct RecipeRng(u64);

    impl RecipeRng {
        fn next_u64(&mut self) -> u64 {
            self.0 = self
                .0
                .wrapping_mul(6_364_136_223_846_793_005)
                .wrapping_add(1_442_695_040_888_963_407);
            self.0
        }

        // k / 2^22 with |k| < 2^23: exact in f32.
        fn weight(&mut self) -> f32 {
            ((self.next_u64() >> 40) as i64 - (1 << 23)) as f32 / (1u32 << 22) as f32
        }

        fn norm(&mut self) -> f32 {
            ((self.next_u64() >> 43) as i64 - (1 << 20) + (1 << 22)) as f32 / (1u32 << 22) as f32
        }
    }

    struct Recipe {
        shape: serde_json::Value,
        tensors: Vec<RecipeTensor>,
    }

    type RecipeTensor = (String, Vec<usize>, Vec<f32>);

    fn matrix(rng: &mut RecipeRng, name: String, rows: usize, cols: usize) -> RecipeTensor {
        let values = (0..rows * cols).map(|_| rng.weight()).collect();
        (name, vec![rows, cols], values)
    }

    fn norm(rng: &mut RecipeRng, name: String, size: usize) -> RecipeTensor {
        (name, vec![size], (0..size).map(|_| rng.norm()).collect())
    }

    fn recipe(section: &serde_json::Value) -> Recipe {
        let shape = section["recipe"]["shape"].clone();
        let dim = |key: &str| shape[key].as_u64().unwrap() as usize;
        let (d, vocab, d_ff) = (dim("d_model"), dim("vocab_size"), dim("d_ff"));
        let d_kv = d / dim("n_heads") * dim("n_kv_heads");
        let mut rng = RecipeRng(section["recipe"]["seed"].as_u64().unwrap());
        let mut tensors = vec![
            matrix(&mut rng, "token_embd.weight".into(), vocab, d),
            matrix(&mut rng, "output.weight".into(), vocab, d),
            norm(&mut rng, "output_norm.weight".into(), d),
        ];
        for layer in 0..dim("n_layers") {
            let p = format!("blk.{layer}");
            tensors.push(matrix(&mut rng, format!("{p}.attn_q.weight"), d, d));
            tensors.push(matrix(&mut rng, format!("{p}.attn_k.weight"), d_kv, d));
            tensors.push(matrix(&mut rng, format!("{p}.attn_v.weight"), d_kv, d));
            tensors.push(matrix(&mut rng, format!("{p}.attn_output.weight"), d, d));
            tensors.push(matrix(&mut rng, format!("{p}.ffn_gate.weight"), d_ff, d));
            tensors.push(matrix(&mut rng, format!("{p}.ffn_up.weight"), d_ff, d));
            tensors.push(matrix(&mut rng, format!("{p}.ffn_down.weight"), d, d_ff));
            tensors.push(norm(&mut rng, format!("{p}.attn_norm.weight"), d));
            tensors.push(norm(&mut rng, format!("{p}.ffn_norm.weight"), d));
        }
        // Token 0's embedding row is all zero, as in the reference recipe.
        tensors[0].2[..d].fill(0.0);
        let order: Vec<&str> = tensors.iter().map(|(name, _, _)| name.as_str()).collect();
        let expected: Vec<&str> = section["recipe"]["tensor_order"]
            .as_array()
            .unwrap()
            .iter()
            .map(text)
            .collect();
        assert_eq!(
            order, expected,
            "recipe draw order must match the reference"
        );
        Recipe { shape, tensors }
    }

    fn write_gguf(recipe: &Recipe, omit: Option<&str>) -> std::path::PathBuf {
        let dim = |key: &str| recipe.shape[key].as_u64().unwrap() as u32;
        let vocab: Vec<gguf_file::Value> = (0..dim("vocab_size"))
            .map(|token| gguf_file::Value::String(format!("kat_{token}")))
            .collect();
        let metadata = [
            (
                "general.architecture",
                gguf_file::Value::String("llama".into()),
            ),
            ("llama.block_count", gguf_file::Value::U32(dim("n_layers"))),
            (
                "llama.embedding_length",
                gguf_file::Value::U32(dim("d_model")),
            ),
            (
                "llama.attention.head_count",
                gguf_file::Value::U32(dim("n_heads")),
            ),
            (
                "llama.attention.head_count_kv",
                gguf_file::Value::U32(dim("n_kv_heads")),
            ),
            (
                "llama.feed_forward_length",
                gguf_file::Value::U32(dim("d_ff")),
            ),
            ("tokenizer.ggml.bos_token_id", gguf_file::Value::U32(1)),
            ("tokenizer.ggml.eos_token_id", gguf_file::Value::U32(2)),
            ("tokenizer.ggml.tokens", gguf_file::Value::Array(vocab)),
        ];
        let tensors: Vec<(String, QTensor)> = recipe
            .tensors
            .iter()
            .filter(|(name, _, _)| Some(name.as_str()) != omit)
            .map(|(name, shape, values)| {
                let tensor = Tensor::from_vec(values.clone(), shape.as_slice(), &Device::Cpu)
                    .expect("recipe tensor");
                let tensor = QTensor::quantize(&tensor, GgmlDType::F32).expect("f32 GGUF tensor");
                (name.clone(), tensor)
            })
            .collect();
        let path = std::env::temp_dir().join(format!(
            "arc-prep-{}-{}.gguf",
            std::process::id(),
            omit.unwrap_or("complete").replace('.', "_")
        ));
        let mut file = std::fs::File::create(&path).expect("create test GGUF");
        let metadata: Vec<(&str, &gguf_file::Value)> =
            metadata.iter().map(|(key, value)| (*key, value)).collect();
        let tensors: Vec<(&str, &QTensor)> = tensors
            .iter()
            .map(|(name, tensor)| (name.as_str(), tensor))
            .collect();
        gguf_file::write(&mut file, &metadata, &tensors).expect("write test GGUF");
        path
    }

    fn prepared_digest(model: &CachedIntegerModel) -> String {
        let cfg = &model.config;
        let mut bytes = Vec::new();
        for value in [
            cfg.n_layers,
            cfg.d_model,
            cfg.n_heads,
            cfg.n_kv_heads,
            cfg.d_ff,
            cfg.vocab_size,
            cfg.max_seq,
        ] {
            bytes.extend_from_slice(&(value as u64).to_le_bytes());
        }
        let push = |bytes: &mut Vec<u8>, values: &[i64]| {
            for value in values {
                bytes.extend_from_slice(&value.to_le_bytes());
            }
        };
        let matrix = |bytes: &mut Vec<u8>, weights: &I8Weights| {
            bytes.extend(weights.data.iter().map(|&w| w as u8));
            for scale in &weights.scales {
                bytes.extend_from_slice(&scale.to_le_bytes());
            }
        };
        push(&mut bytes, &[cfg.attn_scale]);
        push(&mut bytes, &cfg.rope_cos);
        push(&mut bytes, &cfg.rope_sin);
        push(&mut bytes, &model.embedding_q16);
        matrix(&mut bytes, &model.embedding_i8);
        matrix(&mut bytes, &model.output_weight);
        push(&mut bytes, &model.final_norm);
        for layer in &model.layers {
            for weights in [
                &layer.wq,
                &layer.wk,
                &layer.wv,
                &layer.wo,
                &layer.w_gate,
                &layer.w_up,
                &layer.w_down,
            ] {
                matrix(&mut bytes, weights);
            }
            push(&mut bytes, &layer.attn_norm);
            push(&mut bytes, &layer.ffn_norm);
        }
        hex::encode(hash_bytes(&bytes).0)
    }

    #[test]
    fn a_gguf_prepared_by_the_canonical_loader_matches_the_independent_reference() {
        let doc = operators();
        let section = &doc["gguf_preparation"];
        let recipe = recipe(section);
        assert_eq!(
            1e-10f32.to_bits() as u64,
            section["f32_1e_10_bits"].as_u64().unwrap(),
            "the reference must floor abs_max at Rust's own 1e-10 literal"
        );
        let path = write_gguf(&recipe, None);
        let model = load_cached_model_canonical_i8_interleaved_rope(path.to_str().unwrap())
            .expect("the canonical loader accepts the complete test GGUF");
        let _ = std::fs::remove_file(&path);

        assert_eq!(
            model.canonical_execution_profile(),
            Some(GGUF_INTERLEAVED_ROPE_I8_INFERENCE_PROFILE)
        );
        assert_eq!(
            model.embedding_i8.scales[0],
            section["embedding_row0_scale"].as_i64().unwrap()
        );
        assert_eq!(
            model.config.attn_scale,
            section["attn_scale"].as_i64().unwrap()
        );
        assert_eq!(
            prepared_digest(&model),
            text(&section["prepared_state_blake3"])
        );

        let sequence = run_whole_model(&model, &token_ids(&section["sequence_tokens"]));
        assert_eq!(sequence.next_tokens, token_ids(&section["next_tokens"]));
        let expected_logits: Vec<String> = section["logits_hashes"]
            .as_array()
            .unwrap()
            .iter()
            .map(|v| text(v).to_owned())
            .collect();
        assert_eq!(sequence.logits_hashes, expected_logits);

        let run = &section["generation_v2"];
        let (tokens, hash) = model
            .try_generate_v2(
                &token_ids(&run["prompt"]),
                run["max_tokens"].as_u64().unwrap() as u32,
                &token_ids(&run["eos_tokens"]),
            )
            .expect("fits the context window");
        assert_eq!(tokens, token_ids(&run["tokens"]));
        assert_eq!(hex::encode(hash.0), text(&run["output_hash"]));
    }

    #[test]
    fn a_gguf_missing_a_norm_weight_is_refused_by_the_canonical_loader() {
        let doc = operators();
        let recipe = recipe(&doc["gguf_preparation"]);
        let path = write_gguf(&recipe, Some("blk.0.ffn_norm.weight"));
        let result = load_cached_model_canonical_i8_interleaved_rope(path.to_str().unwrap());
        let _ = std::fs::remove_file(&path);
        match result {
            Err(error) => assert!(
                error.to_string().contains("blk.0.ffn_norm.weight"),
                "unexpected error: {error}"
            ),
            Ok(_) => panic!("a GGUF without blk.0.ffn_norm.weight must not load"),
        }
    }
}
