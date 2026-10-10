//! A drafter for exact speculative decoding: ARC's algorithm with f32
//! accumulation, one command buffer per token on the Apple GPU.
//!
//! NON-CANONICAL. [`MetalFloatDrafter`] implements #191's [`TokenDrafter`]:
//! it proposes tokens, and ARC's exact engine verifies every one of them
//! (`crate::draft_verify`), so its arithmetic changes only how many
//! proposals are accepted, never an output.
//!
//! It runs #178's decoder built with `MetalDecoder::new_float_draft`. Every
//! projection forms its dot product in f32, in the order of the CPU study
//! (`float_accumulation_study`, #191), and is then requantised exactly. Every
//! other step is the exact engine's integer arithmetic: RMSNorm, RoPE, the KV
//! cache, attention, SiLU and the residuals on the GPU, and the repetition
//! penalty and token selection on the host. There are no half-precision
//! values anywhere. The tests require its logits and KV rows to equal, bit
//! for bit, the CPU engine's with the study's f32 accumulation on (random
//! models, including Llama-2-7B widths). The study's measured agreement with
//! the exact engine on Llama-2-7B, teacher-forced on the exact engine's own
//! sequences, is therefore this drafter's per-token agreement on that model:
//! p = 0.99982 on the interleaved profile and p = 0.99860 on the live legacy
//! profile (#191, CI run 38024838399).
//!
//! The drafter keeps its own KV cache on the device. Each call reuses the
//! rows of the longest prefix it shares with the verifier's stream, forwards
//! the rest of the stream (normally one or two tokens), then drafts one token
//! per forward step.
//!
//! Opt-in: this module exists only with the `metal-exact` feature, and only
//! code that builds a [`MetalFloatDrafter`] uses it.

use std::time::Instant;

use arc_gpu::metal_decoder::{MetalDecoder, Submission};
use arc_gpu::metal_float_draft::RowThreads;

use crate::cached_integer_model::{CachedIntegerModel, select_next_token_with_repetition_penalty};
use crate::draft_verify::{DraftError, TokenDrafter};
use crate::metal_forward::{decoder_inputs, decoder_shape};

/// What a [`MetalFloatDrafter`] did, for reports.
#[derive(Debug, Clone, Copy, Default, serde::Serialize)]
pub struct FloatDraftStats {
    /// Calls of `propose`.
    pub proposals: u64,
    /// Tokens proposed.
    pub drafted: u64,
    /// Stream tokens forwarded to catch up with the verifier.
    pub catch_up_steps: u64,
    /// Drafted tokens forwarded to draft the next one.
    pub draft_steps: u64,
    /// Device KV rows dropped because the stream left them.
    pub rows_dropped: u64,
    /// Wall and GPU seconds of every forward step.
    pub step_seconds: f64,
    pub gpu_seconds: f64,
}

/// A NON-CANONICAL f32-accumulating drafter on the GPU. See the module.
pub struct MetalFloatDrafter<'a> {
    model: &'a CachedIntegerModel,
    decoder: MetalDecoder,
    /// The tokens whose KV rows the device cache holds, in order.
    consumed: Vec<u32>,
    stats: FloatDraftStats,
}

impl<'a> MetalFloatDrafter<'a> {
    /// Upload `model` (a device copy of its own) with a device KV cache of
    /// `kv_capacity` positions (at most the model's `max_seq`). Canonical
    /// per-row INT8 models only, as the exact GPU decoder.
    pub fn new(model: &'a CachedIntegerModel, kv_capacity: usize) -> Result<Self, String> {
        let (engine, shape, weights) = decoder_inputs(model, kv_capacity)?;
        let decoder = MetalDecoder::new_float_draft(engine, shape, weights)?;
        Self::with_decoder(model, decoder)
    }

    /// A drafter on a float-draft decoder the caller built for `model`, for
    /// example one that shares its resident weights with an exact decoder.
    pub fn with_decoder(
        model: &'a CachedIntegerModel,
        decoder: MetalDecoder,
    ) -> Result<Self, String> {
        if !decoder.is_float_draft() {
            return Err("the drafter needs a decoder built with new_float_draft".to_string());
        }
        let shape = *decoder.shape();
        if shape != decoder_shape(model, shape.kv_capacity) {
            return Err(format!(
                "the decoder's shape {shape:?} does not match the model"
            ));
        }
        Ok(Self {
            model,
            decoder,
            consumed: Vec::new(),
            stats: FloatDraftStats::default(),
        })
    }

    pub fn stats(&self) -> FloatDraftStats {
        self.stats
    }

    /// How many threads run each row of a projection. Changes speed only.
    pub fn set_row_threads(&mut self, threads: RowThreads) -> Result<(), String> {
        self.decoder.set_float_row_threads(threads)
    }

    /// Forget the consumed tokens: the next call starts from position 0.
    pub fn reset(&mut self) {
        self.stats.rows_dropped += self.consumed.len() as u64;
        self.consumed.clear();
    }

    /// Forward `token` at the next position and return its logits.
    fn forward(&mut self, token: u32) -> Result<Vec<i64>, DraftError> {
        let cfg = &self.model.config;
        let d = cfg.d_model;
        let idx = (token as usize).min(cfg.vocab_size.saturating_sub(1));
        let hidden = self
            .model
            .embedding_q16
            .get(idx * d..(idx + 1) * d)
            .ok_or_else(|| DraftError(format!("token {token} has no embedding row")))?;
        let pos = self.consumed.len();
        let started = Instant::now();
        let step = self
            .decoder
            .step(
                hidden,
                pos,
                0..cfg.n_layers,
                true,
                Submission::OneCommandBuffer,
            )
            .map_err(|e| DraftError(format!("float drafter at position {pos}: {e}")))?;
        self.stats.step_seconds += started.elapsed().as_secs_f64();
        self.stats.gpu_seconds += step.gpu_seconds;
        self.consumed.push(token);
        step.logits
            .ok_or_else(|| DraftError("the decoder returned no logits".to_string()))
    }
}

impl TokenDrafter for MetalFloatDrafter<'_> {
    fn label(&self) -> &str {
        "metal-f32"
    }

    fn propose(
        &mut self,
        stream: &[u32],
        generated: &[u32],
        max_draft: usize,
    ) -> Result<Vec<u32>, DraftError> {
        self.stats.proposals += 1;
        if stream.is_empty() || max_draft == 0 {
            return Ok(Vec::new());
        }
        let capacity = self.decoder.shape().kv_capacity;
        if stream.len() > capacity {
            return Err(DraftError(format!(
                "the stream's {} tokens exceed the drafter's {capacity} cached positions",
                stream.len()
            )));
        }
        // Keep the rows of the longest common prefix, but forward the
        // stream's last token again: the first draft is chosen from its
        // logits.
        let common = self
            .consumed
            .iter()
            .zip(stream)
            .take_while(|(held, wanted)| held == wanted)
            .count();
        let keep = common.min(stream.len() - 1);
        self.stats.rows_dropped += (self.consumed.len() - keep) as u64;
        self.consumed.truncate(keep);
        let mut logits = Vec::new();
        for &token in &stream[keep..] {
            logits = self.forward(token)?;
            self.stats.catch_up_steps += 1;
        }
        // Each draft is chosen as the exact engine chooses its token, with the
        // repetition penalty over the output so far and the drafts before it.
        let mut history = generated.to_vec();
        let mut drafts = Vec::with_capacity(max_draft);
        loop {
            let next = select_next_token_with_repetition_penalty(&mut logits, &history);
            drafts.push(next);
            history.push(next);
            if drafts.len() == max_draft || self.consumed.len() >= capacity {
                break;
            }
            logits = self.forward(next)?;
            self.stats.draft_steps += 1;
        }
        self.stats.drafted += drafts.len() as u64;
        Ok(drafts)
    }
}

/// The same weights for a second decoder: the matrices are shared, not
/// copied.
#[cfg(test)]
fn share_weights(
    weights: &arc_gpu::metal_decoder::DecoderWeights,
) -> arc_gpu::metal_decoder::DecoderWeights {
    use arc_gpu::metal_decoder::{DecoderLayerWeights, DecoderWeights};
    DecoderWeights {
        layers: weights
            .layers
            .iter()
            .map(|l| DecoderLayerWeights {
                wq: l.wq.clone(),
                wk: l.wk.clone(),
                wv: l.wv.clone(),
                wo: l.wo.clone(),
                w_gate: l.w_gate.clone(),
                w_up: l.w_up.clone(),
                w_down: l.w_down.clone(),
                attn_norm: l.attn_norm.clone(),
                ffn_norm: l.ffn_norm.clone(),
            })
            .collect(),
        final_norm: weights.final_norm.clone(),
        output: weights.output.clone(),
        rope_cos: weights.rope_cos.clone(),
        rope_sin: weights.rope_sin.clone(),
        exp_lut: weights.exp_lut.clone(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::draft_verify::{
        DraftPolicy, DraftVerifyConfig, ExactSemantics, generate_with_drafter,
    };
    use crate::metal_forward::{SyntheticShape, synthetic_model};
    use arc_gpu::metal_decoder::DecoderRefusal;

    /// Grouped-query attention, and a d_ff with a tail after its last
    /// 16-column block.
    fn small_shape() -> SyntheticShape {
        SyntheticShape {
            n_layers: 2,
            d_model: 64,
            n_heads: 4,
            n_kv_heads: 2,
            d_ff: 88,
            vocab: 300,
            max_seq: 64,
            embedding_rows: 300,
        }
    }

    /// The CPU engine with the study's f32 accumulation on this thread:
    /// logits per token, and the KV cache it leaves.
    #[cfg(feature = "float-accumulation-study")]
    fn study_run(
        model: &CachedIntegerModel,
        tokens: &[u32],
    ) -> (Vec<Vec<i64>>, crate::cached_integer_model::KVCache) {
        let _f32 = crate::float_accumulation_study::on_this_thread();
        let mut cache = crate::cached_integer_model::KVCache::new(model.config.n_layers);
        let logits = tokens
            .iter()
            .map(|&t| model.forward_one_token(t, &mut cache))
            .collect();
        (logits, cache)
    }

    /// Every logit and every KV row the drafter's decoder computes for
    /// `tokens` equals the CPU study engine's, with each row split.
    #[cfg(feature = "float-accumulation-study")]
    fn assert_equals_the_study(model: &CachedIntegerModel, tokens: &[u32], kv_capacity: usize) {
        let (want, cache) = study_run(model, tokens);
        let d_kv = model.config.d_kv;
        let mut drafter = MetalFloatDrafter::new(model, kv_capacity).expect("float drafter");
        for threads in RowThreads::ALL {
            drafter.set_row_threads(threads).expect("a float decoder");
            drafter.reset();
            for (index, &token) in tokens.iter().enumerate() {
                let got = drafter.forward(token).expect("an in-domain token");
                assert_eq!(got, want[index], "{threads:?}: logits of token {index}");
            }
            for (layer, (keys, values)) in cache.k_data.iter().zip(&cache.v_data).enumerate() {
                let rows = keys.chunks_exact(d_kv).zip(values.chunks_exact(d_kv));
                for (pos, (want_k, want_v)) in rows.enumerate().take(tokens.len()) {
                    let (k, v) = drafter.decoder.read_kv(layer, pos).expect("a cached row");
                    assert_eq!(k, want_k, "{threads:?}: K {layer}/{pos}");
                    assert_eq!(v, want_v, "{threads:?}: V {layer}/{pos}");
                }
            }
        }
    }

    #[cfg(feature = "float-accumulation-study")]
    #[test]
    fn float_draft_logits_and_kv_rows_equal_the_cpu_study_engine() {
        let tokens = [1u32, 7, 42, 3, 299, 150, 7, 9, 64, 128, 5, 5];
        let model = synthetic_model(0x0F1D_7AF7_0000_0001, small_shape());
        // The comparison is not vacuous: f32 accumulation changes logits.
        let (study, _) = study_run(&model, &tokens);
        let mut exact_cache = crate::cached_integer_model::KVCache::new(model.config.n_layers);
        let exact: Vec<Vec<i64>> = tokens
            .iter()
            .map(|&t| model.forward_one_token(t, &mut exact_cache))
            .collect();
        assert_ne!(exact, study, "f32 accumulation changed no logit");
        assert_equals_the_study(&model, &tokens, small_shape().max_seq);
    }

    /// Real Llama-2-7B widths (d_model 4096, d_ff 11008, 32 heads of 128),
    /// one layer and a 4,096-token head.
    #[cfg(feature = "float-accumulation-study")]
    #[test]
    fn llama_7b_width_float_draft_equals_the_cpu_study_engine() {
        let model = synthetic_model(
            0x0F1D_7AF7_0000_0002,
            SyntheticShape {
                n_layers: 1,
                d_model: 4096,
                n_heads: 32,
                n_kv_heads: 32,
                d_ff: 11008,
                vocab: 4096,
                max_seq: 16,
                embedding_rows: 8,
            },
        );
        assert_equals_the_study(&model, &[1, 5, 2], 16);
    }

    /// Reusing the device rows of a shared prefix, and dropping the rest,
    /// proposes exactly what a drafter starting from nothing would.
    #[test]
    fn proposals_do_not_depend_on_what_the_drafter_saw_before() {
        let model = synthetic_model(0x0F1D_7AF7_0000_0003, small_shape());
        let max_seq = small_shape().max_seq;
        let mut reused = MetalFloatDrafter::new(&model, max_seq).expect("float drafter");
        let mut fresh = MetalFloatDrafter::new(&model, max_seq).expect("float drafter");
        let cases: [(&[u32], &[u32], usize); 7] = [
            (&[1, 5, 9, 13], &[], 4),
            (&[1, 5, 9, 13, 17, 2], &[17, 2], 5),
            (&[1, 5, 8], &[], 3),
            (&[1, 5, 8], &[], 3),
            (&[1, 5, 8, 8, 8], &[8, 8], 7),
            (&[1], &[], 2),
            (&[1, 5, 9, 13, 17, 2, 40, 41], &[17, 2, 40], 1),
        ];
        for (index, (stream, generated, max_draft)) in cases.into_iter().enumerate() {
            let got = reused
                .propose(stream, generated, max_draft)
                .expect("drafts");
            fresh.reset();
            let want = fresh.propose(stream, generated, max_draft).expect("drafts");
            assert_eq!(got, want, "case {index}");
            assert_eq!(got.len(), max_draft, "case {index}");
        }
        let stats = reused.stats();
        assert!(
            stats.rows_dropped > 0 && stats.catch_up_steps > 0,
            "{stats:?}"
        );
    }

    /// With this drafter proposing, the output equals plain exact decoding,
    /// for both generation contracts, one stage and two; and it proposes
    /// tokens the verifier accepts.
    #[test]
    fn drafting_with_it_never_changes_the_output() {
        let shape = small_shape();
        let model = synthetic_model(0x0F1D_7AF7_0000_0004, shape);
        let mut drafter = MetalFloatDrafter::new(&model, shape.max_seq).expect("float drafter");
        let prompts: [&[u32]; 2] = [&[1, 7, 42, 3], &[9, 9, 150, 299, 12, 64, 5]];
        let max_tokens = 40;
        let mut accepted = 0;
        for semantics in [ExactSemantics::Worker, ExactSemantics::V2] {
            for stage_ends in [vec![shape.n_layers], vec![1, shape.n_layers]] {
                for prompt in prompts {
                    let (tokens, hash) = match semantics {
                        ExactSemantics::Worker => model.try_generate(prompt, max_tokens, &[]),
                        ExactSemantics::V2 => model.try_generate_v2(prompt, max_tokens, &[]),
                    }
                    .expect("the generation fits");
                    let config = DraftVerifyConfig {
                        semantics,
                        stage_ends: stage_ends.clone(),
                        policy: DraftPolicy::MEASURED,
                    };
                    drafter.reset();
                    let output = generate_with_drafter(
                        &model,
                        prompt,
                        max_tokens,
                        &[],
                        &mut drafter,
                        &config,
                    )
                    .expect("generation");
                    assert_eq!(output.tokens, tokens, "{semantics:?} {stage_ends:?}");
                    assert_eq!(output.output_hash, hash, "{semantics:?} {stage_ends:?}");
                    assert_eq!(output.stats.drafter_errors, 0);
                    accepted += output.stats.accepted;
                    eprintln!(
                        "{semantics:?} {stage_ends:?}: drafted {}, accepted {}, passes {}, plain steps {}",
                        output.stats.drafted,
                        output.stats.accepted,
                        output.stats.passes,
                        output.stats.plain_steps
                    );
                }
            }
        }
        assert!(accepted > 0, "the verifier accepted no draft");
        eprintln!("drafter: {:?}", drafter.stats());
    }

    #[test]
    fn a_float_decoder_refuses_multi_row_passes_and_an_exact_one_has_no_f32_kernels() {
        let shape = small_shape();
        let model = synthetic_model(0x0F1D_7AF7_0000_0005, shape);
        let (engine, decoder_shape, weights) =
            decoder_inputs(&model, shape.max_seq).expect("upload");
        let mut exact = MetalDecoder::new(engine.clone(), decoder_shape, share_weights(&weights))
            .expect("exact decoder");
        assert!(!exact.is_float_draft());
        assert!(exact.set_float_row_threads(RowThreads::One).is_err());
        assert!(MetalFloatDrafter::with_decoder(&model, exact).is_err());
        let mut float =
            MetalDecoder::new_float_draft(engine, decoder_shape, weights).expect("float decoder");
        let hidden = model.embedding_q16[..2 * shape.d_model].to_vec();
        assert!(matches!(
            float.step_rows(
                &hidden,
                0,
                0..shape.n_layers,
                true,
                Submission::OneCommandBuffer
            ),
            Err(DecoderRefusal::Input(_))
        ));
        assert!(
            float
                .step(
                    &hidden[..shape.d_model],
                    0,
                    0..shape.n_layers,
                    true,
                    Submission::OneCommandBuffer
                )
                .is_ok()
        );
    }
}

#[cfg(test)]
mod bench {
    use super::*;
    use crate::cached_integer_model::I8Weights;
    use crate::draft_verify::{
        DraftPolicy, DraftVerifyConfig, ExactSemantics, generate_with_drafter,
    };
    use crate::integer_lut::{EXP_LUT, ONE};
    use crate::metal_forward::{SynthRng, SyntheticShape, synthetic_model, synthetic_rope};
    use crate::metal_gemv::metal_engine;
    use arc_gpu::metal_decoder::{DecoderLayerWeights, DecoderShape, DecoderWeights, MAX_ROWS};
    use arc_gpu::metal_exact::{ResidentMatrix, Storage};
    use arc_gpu::metal_float_draft::FloatDraftKernels;
    use std::sync::Arc;

    const READ_PROBE_BYTES: usize = 256 << 20;
    const WARM_UP: usize = 2;
    const TOKENS: usize = 10;
    /// Depths of the real-width models with distinct weights in every layer.
    const DEPTHS: [usize; 4] = [1, 2, 4, 6];
    const EMBEDDING_ROWS: usize = 16;
    /// Positions of the benchmark decoders' device KV caches.
    const KV_CAPACITY: usize = 16;
    /// The 7B study's agreement of this arithmetic with the exact engine (#191,
    /// CI run 38024838399): (profile, p).
    const STUDY_P: [(&str, f64); 2] = [("interleaved", 0.99982), ("legacy (live)", 0.99860)];
    /// Draft lengths of a verify pass: #191's quad-aligned lengths that fit
    /// one #186 pass of at most MAX_ROWS rows.
    const DRAFT_LENGTHS: [usize; 2] = [3, 7];

    fn median(mut samples: Vec<f64>) -> f64 {
        samples.sort_by(f64::total_cmp);
        samples[samples.len() / 2]
    }

    /// Least-squares line through (layers, seconds): (fixed, per layer).
    fn fit(points: &[(usize, f64)]) -> (f64, f64) {
        let n = points.len() as f64;
        let mean_x = points.iter().map(|&(x, _)| x as f64).sum::<f64>() / n;
        let mean_y = points.iter().map(|&(_, y)| y).sum::<f64>() / n;
        let (mut sxx, mut sxy) = (0.0, 0.0);
        for &(x, y) in points {
            let dx = x as f64 - mean_x;
            sxx += dx * dx;
            sxy += dx * (y - mean_y);
        }
        let per_layer = sxy / sxx;
        (mean_y - per_layer * mean_x, per_layer)
    }

    fn at_32((fixed, per_layer): (f64, f64)) -> f64 {
        fixed + 32.0 * per_layer
    }

    /// Median wall and GPU seconds of one token through layers 0..depth and
    /// the head, after WARM_UP untimed tokens.
    fn token_times(decoder: &mut MetalDecoder, hidden: &[i64], depth: usize) -> (f64, f64) {
        let (mut walls, mut gpus) = (Vec::new(), Vec::new());
        for pos in 0..WARM_UP + TOKENS {
            let start = Instant::now();
            let step = decoder
                .step(hidden, pos, 0..depth, true, Submission::OneCommandBuffer)
                .expect("an in-domain token");
            if pos >= WARM_UP {
                walls.push(start.elapsed().as_secs_f64());
                gpus.push(step.gpu_seconds);
            }
        }
        (median(walls), median(gpus))
    }

    /// Median wall and GPU seconds of one exact `rows`-row pass at position 0.
    fn pass_times(decoder: &mut MetalDecoder, hidden: &[i64], rows: usize) -> (f64, f64) {
        let input: Vec<i64> = hidden
            .iter()
            .copied()
            .cycle()
            .take(rows * hidden.len())
            .collect();
        let n_layers = decoder.shape().n_layers;
        let (mut walls, mut gpus) = (Vec::new(), Vec::new());
        for step in 0..WARM_UP + TOKENS {
            let start = Instant::now();
            let pass = decoder
                .step_rows(&input, 0, 0..n_layers, true, Submission::OneCommandBuffer)
                .expect("an in-domain pass");
            if step >= WARM_UP {
                walls.push(start.elapsed().as_secs_f64());
                gpus.push(pass.gpu_seconds);
            }
        }
        (median(walls), median(gpus))
    }

    /// Committed tokens per verify pass of `k` drafts when each draft equals
    /// ARC's token with probability `p`: the accepted run plus ARC's own.
    fn tokens_per_pass(k: usize, p: f64) -> f64 {
        1.0 + (1..=k).map(|j| p.powi(j as i32)).sum::<f64>()
    }

    #[test]
    #[ignore = "benchmark: run explicitly in release with --ignored --nocapture"]
    fn float_drafter_against_the_exact_decoder() {
        let engine = metal_engine().expect("Metal device");
        let device = engine.report();
        let read = |storage| {
            let times = engine
                .measure_read_bandwidth(READ_PROBE_BYTES, 10, storage)
                .expect("read probe");
            READ_PROBE_BYTES as f64 / times[0] / 1e9
        };
        let read_gbps = read(Storage::Shared).max(read(Storage::Private));

        // ---- One 7B layer's matrices and the head, shared by 32 layers (as
        // #178's token benchmark: the VM cannot hold 6.6 GB of weights).
        let (d, d_ff, vocab, heads, d_head) =
            (4096usize, 11008usize, 32000usize, 32usize, 128usize);
        let mut rng = SynthRng(0x0F1D_7AF7_BE4C_0001);
        let up = |w: I8Weights| -> Arc<ResidentMatrix> {
            Arc::new(
                engine
                    .upload(&w.data, &w.scales, w.n_rows, w.n_cols, Storage::Shared)
                    .expect("upload"),
            )
        };
        let shared_layer = [
            up(rng.matrix(d, d)),
            up(rng.matrix(d, d)),
            up(rng.matrix(d, d)),
            up(rng.matrix(d, d)),
            up(rng.matrix(d_ff, d)),
            up(rng.matrix(d_ff, d)),
            up(rng.matrix(d, d_ff)),
        ];
        let output = up(rng.matrix(vocab, d));
        let gains: Vec<i64> = (0..d)
            .map(|_| ONE + (rng.next_u64() % 8193) as i64 - 4096)
            .collect();
        let (rope_cos, rope_sin) = synthetic_rope(d_head, 64);
        let weights = DecoderWeights {
            layers: (0..32)
                .map(|_| DecoderLayerWeights {
                    wq: shared_layer[0].clone(),
                    wk: shared_layer[1].clone(),
                    wv: shared_layer[2].clone(),
                    wo: shared_layer[3].clone(),
                    w_gate: shared_layer[4].clone(),
                    w_up: shared_layer[5].clone(),
                    w_down: shared_layer[6].clone(),
                    attn_norm: gains.clone(),
                    ffn_norm: gains.clone(),
                })
                .collect(),
            final_norm: gains.clone(),
            output: output.clone(),
            rope_cos,
            rope_sin,
            exp_lut: EXP_LUT.to_vec(),
        };
        let shape = DecoderShape {
            n_layers: 32,
            d_model: d,
            n_heads: heads,
            n_kv_heads: heads,
            d_head,
            d_kv: d,
            d_ff,
            vocab,
            attn_scale: 5_793,
            max_seq: 64,
            kv_capacity: KV_CAPACITY,
        };
        let mut exact = MetalDecoder::new(engine.clone(), shape, share_weights(&weights))
            .expect("exact decoder");
        let mut float =
            MetalDecoder::new_float_draft(engine.clone(), shape, weights).expect("float decoder");
        let token_bytes = exact.weight_bytes_per_token();
        let hidden: Vec<i64> = (0..d)
            .map(|_| ((rng.next_u64() % 255) as i64 - 127) * 64)
            .collect();

        // ---- Projections alone: GPU ms per pass over the 7B layer's seven
        // matrices and the head.
        let x_d: Vec<i64> = (0..d)
            .map(|_| ((rng.next_u64() % 255) as i64 - 127) * 64)
            .collect();
        let x_ff: Vec<i64> = (0..d_ff)
            .map(|_| ((rng.next_u64() % 255) as i64 - 127) * 64)
            .collect();
        let mut items: Vec<(&ResidentMatrix, &[i64])> = shared_layer
            .iter()
            .map(|m| {
                let input: &[i64] = if m.n_cols() == d { &x_d } else { &x_ff };
                (m.as_ref(), input)
            })
            .collect();
        items.push((output.as_ref(), &x_d));
        let projection_bytes: usize = items.iter().map(|(m, _)| m.weight_bytes()).sum();
        let exact_projections = engine
            .time_batch(&items, engine.tile(), 3)
            .expect("exact projections");
        let kernels = FloatDraftKernels::new(&engine).expect("float kernels");
        let float_projections: Vec<(RowThreads, f64)> = RowThreads::ALL
            .into_iter()
            .map(|threads| {
                let seconds = kernels
                    .time_batch(&engine, &items, threads, 3)
                    .expect("float projections");
                (threads, seconds)
            })
            .collect();

        // ---- Whole 32-layer tokens: exact, then the drafter with each split.
        let (exact_wall, exact_gpu) = token_times(&mut exact, &hidden, 32);
        let mut float_tokens: Vec<(RowThreads, (f64, f64))> = Vec::new();
        for threads in RowThreads::ALL {
            float.set_float_row_threads(threads).expect("float decoder");
            float_tokens.push((threads, token_times(&mut float, &hidden, 32)));
        }
        let &(best, (float_wall, _)) = float_tokens
            .iter()
            .min_by(|a, b| a.1.0.total_cmp(&b.1.0))
            .expect("three variants");

        // ---- #186's exact multi-row verify pass on the same decoder.
        let passes: Vec<(usize, (f64, f64))> = DRAFT_LENGTHS
            .iter()
            .map(|&k| (k + 1, pass_times(&mut exact, &hidden, k + 1)))
            .collect();
        assert!(passes.iter().all(|&(rows, _)| rows <= MAX_ROWS));
        drop(exact);
        drop(float);
        drop(items);
        drop(shared_layer);
        drop(output);

        // ---- Real-width models with distinct weights in every layer: a
        // line per path through 1 to 6 layers, extrapolated to 32 (DERIVED).
        let mut exact_points = Vec::new();
        let mut float_points = Vec::new();
        for depth in DEPTHS {
            let model = synthetic_model(
                0x0F1D_7AF7_BE4C_0010 + depth as u64,
                SyntheticShape {
                    n_layers: depth,
                    d_model: d,
                    n_heads: heads,
                    n_kv_heads: heads,
                    d_ff,
                    vocab,
                    max_seq: 64,
                    embedding_rows: EMBEDDING_ROWS,
                },
            );
            let (engine, depth_shape, weights) =
                decoder_inputs(&model, KV_CAPACITY).expect("upload");
            let mut exact = MetalDecoder::new(engine.clone(), depth_shape, share_weights(&weights))
                .expect("exact decoder");
            let mut float =
                MetalDecoder::new_float_draft(engine, depth_shape, weights).expect("float decoder");
            float.set_float_row_threads(best).expect("float decoder");
            let embedding = &model.embedding_q16[..d];
            exact_points.push((depth, token_times(&mut exact, embedding, depth).0));
            float_points.push((depth, token_times(&mut float, embedding, depth).0));
        }
        let exact_line = fit(&exact_points);
        let float_line = fit(&float_points);

        // ---- Agreement on a small random model (not the 7B): the drafter's
        // first proposal at every position of exact generations, teacher-forced,
        // and what the verifier accepts in generate_with_drafter.
        let agreement_shape = SyntheticShape {
            n_layers: 4,
            d_model: 512,
            n_heads: 8,
            n_kv_heads: 4,
            d_ff: 1376,
            vocab: 4096,
            max_seq: 192,
            embedding_rows: 4096,
        };
        let small = synthetic_model(0x0F1D_7AF7_BE4C_0020, agreement_shape);
        let mut drafter =
            MetalFloatDrafter::new(&small, agreement_shape.max_seq).expect("float drafter");
        drafter.set_row_threads(best).expect("float decoder");
        let max_tokens = 120u32;
        let (mut agree, mut positions) = (0usize, 0usize);
        let (mut drafted, mut accepted, mut passes_run) = (0usize, 0usize, 0usize);
        for seed in 0..4u64 {
            let mut prompt_rng = SynthRng(0x0F1D_7AF7_BE4C_0030 + seed);
            let prompt: Vec<u32> = (0..8)
                .map(|_| (prompt_rng.next_u64() % 4096) as u32)
                .collect();
            let (tokens, hash) = small
                .try_generate(&prompt, max_tokens, &[])
                .expect("exact generation");
            // Worker semantics: BOS, the prompt, the last prompt token again,
            // then the output.
            let mut stream = vec![small.config.bos_token];
            stream.extend_from_slice(&prompt);
            stream.push(*prompt.last().expect("a prompt"));
            drafter.reset();
            for (i, &token) in tokens.iter().enumerate() {
                let proposal = drafter.propose(&stream, &tokens[..i], 1).expect("a draft");
                agree += usize::from(proposal.first() == Some(&token));
                positions += 1;
                stream.push(token);
            }
            drafter.reset();
            let output = generate_with_drafter(
                &small,
                &prompt,
                max_tokens,
                &[],
                &mut drafter,
                &DraftVerifyConfig {
                    semantics: ExactSemantics::Worker,
                    stage_ends: vec![agreement_shape.n_layers],
                    policy: DraftPolicy::MEASURED,
                },
            )
            .expect("generation");
            assert_eq!(
                output.tokens, tokens,
                "the output must equal exact decoding"
            );
            assert_eq!(output.output_hash, hash);
            drafted += output.stats.drafted;
            accepted += output.stats.accepted;
            passes_run += output.stats.passes;
        }

        // ---- Report.
        let ms = |seconds: f64| seconds * 1e3;
        let gbps = |bytes: usize, seconds: f64| bytes as f64 / seconds / 1e9;
        let mut md = String::new();
        md.push_str("### f32 drafter against the exact decoder, Llama-2-7B shape\n\n");
        md.push_str(
            "Hosted-VM CI measurement: GitHub-hosted macOS VM (Apple M1, virtual), paravirtual \
             Metal GPU. Not Apple GPU hardware numbers. The drafter is NON-CANONICAL: it only \
             proposes tokens, which the exact engine verifies.\n\n",
        );
        md.push_str(&format!(
            "Device `{}`. Measured read bandwidth {read_gbps:.1} GB/s (256 MiB, best of 10, \
             best of shared and private). A token reads {token_bytes} weight bytes. Token times \
             are medians of {TOKENS} tokens after {WARM_UP} warm-up tokens.\n\n",
            device.name
        ));
        md.push_str(
            "**Projections alone** (the seven matrices of a 7B layer and the 32,000-row head, \
             GPU time per pass, mean of 3 passes in one command buffer):\n\n\
             | Kernel | GPU ms | GB/s | Share of read bandwidth |\n|---|---|---|---|\n",
        );
        md.push_str(&format!(
            "| Exact GEMV (#176, tile {:?}) | {:.2} | {:.1} | {:.0}% |\n",
            engine.tile(),
            ms(exact_projections),
            gbps(projection_bytes, exact_projections),
            100.0 * gbps(projection_bytes, exact_projections) / read_gbps
        ));
        for &(threads, seconds) in &float_projections {
            md.push_str(&format!(
                "| f32 drafter, {} thread(s) per row | {:.2} | {:.1} | {:.0}% |\n",
                threads.threads(),
                ms(seconds),
                gbps(projection_bytes, seconds),
                100.0 * gbps(projection_bytes, seconds) / read_gbps
            ));
        }
        md.push_str(
            "\n**A whole 32-layer token in one command buffer** (the 32 layers share one \
             layer's weights, which still stream from memory in every layer):\n\n\
             | Path | Wall ms per token | GPU ms per token | tok/s (wall) | Weights GB/s (GPU) |\n\
             |---|---|---|---|---|\n",
        );
        md.push_str(&format!(
            "| Exact decoder (#178) | {:.1} | {:.1} | {:.2} | {:.1} |\n",
            ms(exact_wall),
            ms(exact_gpu),
            1.0 / exact_wall,
            gbps(token_bytes, exact_gpu)
        ));
        for &(threads, (wall, gpu)) in &float_tokens {
            md.push_str(&format!(
                "| f32 drafter, {} thread(s) per row | {:.1} | {:.1} | {:.2} | {:.1} |\n",
                threads.threads(),
                ms(wall),
                ms(gpu),
                1.0 / wall,
                gbps(token_bytes, gpu)
            ));
        }
        md.push_str(&format!(
            "\nThe drafter ({} thread(s) per row, the fastest here) takes {:.2}x the exact \
             decoder's wall time per token: {:.2}x as fast.\n\n",
            best.threads(),
            float_wall / exact_wall,
            exact_wall / float_wall
        ));
        md.push_str(
            "**Real-width models with distinct weights in every layer** (wall ms per token; a \
             least-squares line per path, extrapolated to 32 layers, DERIVED):\n\n| Path |",
        );
        for depth in DEPTHS {
            md.push_str(&format!(
                " {depth} layer{} |",
                if depth == 1 { "" } else { "s" }
            ));
        }
        md.push_str(" ms per layer | fixed ms | DERIVED 32 layers, ms | DERIVED tok/s |\n|---|");
        md.push_str(&"---|".repeat(DEPTHS.len() + 4));
        md.push('\n');
        for (name, points, line) in [
            ("Exact decoder (#178)", &exact_points, exact_line),
            ("f32 drafter", &float_points, float_line),
        ] {
            md.push_str(&format!("| {name} |"));
            for &(_, seconds) in points.iter() {
                md.push_str(&format!(" {:.1} |", ms(seconds)));
            }
            md.push_str(&format!(
                " {:.2} | {:.1} | {:.0} | {:.2} |\n",
                ms(line.1),
                ms(line.0),
                ms(at_32(line)),
                1.0 / at_32(line)
            ));
        }
        md.push_str(
            "\n**Speculative decoding on this GPU, DERIVED** from the 32-layer token times \
             above and #186's exact multi-row verify pass measured on the same decoder. A pass \
             with k drafts costs k drafter steps and one (k + 1)-row verify pass, and commits \
             the accepted run plus ARC's own token: 1 + p + ... + p^k tokens. p is the 7B \
             study's measured agreement of this arithmetic (#191, CI run 38024838399), which \
             carries over because the drafter equals the study engine bit for bit.\n\n\
             | k | Verify pass, wall ms | p | Tokens per pass | ms per token | Against exact \
             one-row decoding |\n|---|---|---|---|---|---|\n",
        );
        let mut derived = Vec::new();
        for &(rows, (pass_wall, _)) in &passes {
            let k = rows - 1;
            for (profile, p) in STUDY_P {
                let tokens = tokens_per_pass(k, p);
                let per_token = (k as f64 * float_wall + pass_wall) / tokens;
                md.push_str(&format!(
                    "| {k} | {:.1} | {p} ({profile}) | {tokens:.3} | {:.1} | {:.2}x |\n",
                    ms(pass_wall),
                    ms(per_token),
                    exact_wall / per_token
                ));
                derived.push(serde_json::json!({
                    "k": k,
                    "profile": profile,
                    "p": p,
                    "verify_pass_wall_ms": ms(pass_wall),
                    "tokens_per_pass": tokens,
                    "ms_per_token": ms(per_token),
                    "speedup_over_exact_one_row": exact_wall / per_token,
                }));
            }
        }
        let (free_rows, (free_pass, _)) = passes[passes.len() - 1];
        md.push_str(&format!(
            "\nWith a drafter that cost nothing, the {free_rows}-row pass alone would give \
             {:.1} ms per token ({:.2}x the exact one-row decoder): the ceiling for any drafter \
             on this GPU.\n\n",
            ms(free_pass / free_rows as f64),
            exact_wall / (free_pass / free_rows as f64)
        ));
        md.push_str(&format!(
            "**Agreement on a small random model** (4 layers, d_model 512, 4,096 tokens; NOT the \
             7B): the drafter's first proposal equals the exact token at {agree} of {positions} \
             teacher-forced positions. With it drafting, `generate_with_drafter` reproduced the \
             exact output on all 4 prompts; {accepted} of {drafted} drafts were accepted in \
             {passes_run} verify passes.\n"
        ));
        println!("{md}");
        let json = serde_json::json!({
            "label": "hosted-VM CI measurement",
            "device": device,
            "read_gbps": read_gbps,
            "token_weight_bytes": token_bytes,
            "projections": {
                "bytes": projection_bytes,
                "exact_gpu_ms": ms(exact_projections),
                "float_gpu_ms": float_projections
                    .iter()
                    .map(|&(t, s)| serde_json::json!([t, ms(s)]))
                    .collect::<Vec<_>>(),
            },
            "token_32_layers_shared_weights": {
                "exact_wall_ms": ms(exact_wall),
                "exact_gpu_ms": ms(exact_gpu),
                "float": float_tokens
                    .iter()
                    .map(|&(t, (wall, gpu))| serde_json::json!({
                        "row_threads": t,
                        "wall_ms": ms(wall),
                        "gpu_ms": ms(gpu),
                    }))
                    .collect::<Vec<_>>(),
                "best_row_threads": best,
            },
            "verify_passes": passes
                .iter()
                .map(|&(rows, (wall, gpu))| serde_json::json!({
                    "rows": rows,
                    "wall_ms": ms(wall),
                    "gpu_ms": ms(gpu),
                }))
                .collect::<Vec<_>>(),
            "real_width": {
                "exact_ms_by_depth": exact_points.iter().map(|&(x, y)| serde_json::json!([x, ms(y)])).collect::<Vec<_>>(),
                "float_ms_by_depth": float_points.iter().map(|&(x, y)| serde_json::json!([x, ms(y)])).collect::<Vec<_>>(),
                "exact_derived_32_layers_ms": ms(at_32(exact_line)),
                "float_derived_32_layers_ms": ms(at_32(float_line)),
            },
            "speculative_derived": derived,
            "small_model_agreement": {
                "teacher_forced_agree": agree,
                "teacher_forced_positions": positions,
                "drafted": drafted,
                "accepted": accepted,
                "passes": passes_run,
            },
            "drafter_stats": drafter.stats(),
        });
        println!("METAL_DRAFT_BENCH {json}");
        if let Ok(path) = std::env::var("ARC_METAL_DRAFT_BENCH_MD") {
            std::fs::write(path, md).expect("write the benchmark summary");
        }
    }
}
