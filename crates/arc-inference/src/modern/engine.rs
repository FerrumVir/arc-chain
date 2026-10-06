//! The fast forward pass of the dyadic profile, and the engine/kernel specs
//! that select a forward pass on the command line and in CI.
//!
//! [`Engine`] computes exactly the function of [`ModernModel::forward`]
//! (spec §5.8), one token at a time, with less work per token:
//!
//! * each projection input is prepared once: the RMS norm is written straight
//!   into a [`PreparedInput`] and split into digit planes, and Q, K and V read
//!   it in one parallel region;
//! * the gated SiLU runs in the task that computed the gate and up rows, so
//!   the feed-forward block takes two parallel regions instead of three;
//! * every projection, and attention over the i32 KV cache, uses the exact
//!   SIMD kernels of [`super::kernels`];
//! * buffers are allocated when the engine is built and the KV cache is
//!   reserved for the whole sequence, so a token allocates nothing.
//!
//! Every check of the reference runs on the same values, so the engine
//! refuses exactly when the reference does; only the order of the checks
//! inside one fused step can differ, and any failure fails the token. CI runs
//! each kernel against the independent Python executor's golden digests.

use std::time::{Duration, Instant};

use rayon::prelude::*;

use super::ModernError;
use super::arith::{self, HeadCache, add_residual, exp_q16, rope_split_half, to_activation};
use super::kernels::{self, Isa, Kernel, MatrixRows, PreparedInput};
use super::model::{KvCache, ModernModel, ReferenceRun, TokenForward};
use super::tables::attention_lambda;
use crate::canonical_simd::{self, ProjectionCensus};

/// Phases of [`Engine::forward`] timed when profiling is on.
#[derive(Debug, Clone, Copy)]
enum Phase {
    Embed,
    Norm,
    Qkv,
    RopeKv,
    Attention,
    Wo,
    GateUpSilu,
    Down,
    Residual,
    LmHead,
}

const PHASES: usize = 10;

/// Names of the profiled phases, in [`Engine::phase_seconds`] order.
pub const PHASE_NAMES: [&str; PHASES] = [
    "embed",
    "rms_norm",
    "qkv_projection",
    "rope_and_kv_append",
    "attention",
    "o_projection",
    "gate_up_silu",
    "down_projection",
    "residual",
    "lm_head",
];

fn mark(profile: &mut Option<[Duration; PHASES]>, clock: &mut Option<Instant>, phase: Phase) {
    if let (Some(times), Some(start)) = (profile.as_mut(), clock.as_mut()) {
        let now = Instant::now();
        times[phase as usize] += now - *start;
        *start = now;
    }
}

/// The fast forward pass for one sequence.
pub struct Engine<'m> {
    model: &'m ModernModel,
    isa: Isa,
    cache: KvCache,
    hidden: Vec<i64>,
    qkv: Vec<i64>,
    attended: Vec<i64>,
    projected: Vec<i64>,
    act: Vec<i64>,
    logits: Vec<i64>,
    input: PreparedInput,
    /// Per-head scratch for attention: queries narrowed to i32, scores, and
    /// the weighted value sums.
    q32: Vec<i32>,
    scores: Vec<i64>,
    score_stride: usize,
    weighted: Vec<i64>,
    lambda: i64,
    profile: Option<[Duration; PHASES]>,
}

impl<'m> Engine<'m> {
    /// An engine for `model` whose projections and attention use `kernel`
    /// (the scalar kernel if this CPU does not support it).
    pub fn new(model: &'m ModernModel, kernel: Kernel) -> Self {
        let c = &model.config;
        Self {
            model,
            isa: Isa::new(kernel),
            cache: model.new_cache(),
            hidden: vec![0; c.d_model],
            qkv: vec![0; c.d_q() + 2 * c.d_kv()],
            attended: vec![0; c.d_q()],
            projected: vec![0; c.d_model],
            act: vec![0; c.d_ff],
            logits: vec![0; c.vocab_size],
            input: PreparedInput::new(),
            q32: vec![0; c.d_q()],
            scores: Vec::new(),
            score_stride: 0,
            weighted: vec![0; c.d_q()],
            lambda: attention_lambda(c.d_head),
            profile: None,
        }
    }

    /// The kernel in use.
    pub fn kernel(&self) -> Kernel {
        self.isa.kernel()
    }

    /// Start a new sequence with room for `positions` positions.
    pub fn begin_sequence(&mut self, positions: usize) {
        let model = self.model;
        self.cache = model.new_cache();
        self.cache.reserve(positions, model.config.d_kv());
        self.ensure_scores(positions.min(model.config.max_seq));
    }

    /// Time every phase of [`Engine::forward`] from now on (or stop).
    pub fn set_profiling(&mut self, on: bool) {
        self.profile = on.then_some([Duration::ZERO; PHASES]);
    }

    /// Seconds spent in each phase since profiling was switched on.
    pub fn phase_seconds(&self) -> Vec<(&'static str, f64)> {
        match &self.profile {
            Some(times) => PHASE_NAMES
                .iter()
                .zip(times)
                .map(|(&name, time)| (name, time.as_secs_f64()))
                .collect(),
            None => Vec::new(),
        }
    }

    fn ensure_scores(&mut self, positions: usize) {
        if self.score_stride < positions {
            let c = &self.model.config;
            self.score_stride = positions
                .max(2 * self.score_stride)
                .min(c.max_seq.max(positions));
            self.scores.resize(c.n_heads * self.score_stride, 0);
        }
    }

    /// One token at the cache's next position; the same logits as
    /// [`ModernModel::forward`]. On error the sequence must be restarted.
    pub fn forward(&mut self, token: u32) -> Result<&[i64], ModernError> {
        let model = self.model;
        let c = &model.config;
        let position = self.cache.positions();
        if position >= c.max_seq {
            return Err(ModernError::Domain(format!(
                "position {position} is outside the {}-position context",
                c.max_seq
            )));
        }
        if !self.cache.has_layers(c.n_layers) {
            return Err(ModernError::Invalid(
                "KV cache belongs to another model".into(),
            ));
        }
        let kernel = self.isa.kernel();
        let mut clock = self.profile.as_ref().map(|_| Instant::now());
        arith::embed_row_into(&model.embed, token as usize, &mut self.hidden)?;
        mark(&mut self.profile, &mut clock, Phase::Embed);
        let half = c.d_head / 2;
        let cos = &model.rope_cos[position * half..(position + 1) * half];
        let sin = &model.rope_sin[position * half..(position + 1) * half];
        let (dq, dkv) = (c.d_q(), c.d_kv());
        self.ensure_scores(position + 1);
        for (l, layer) in model.layers.iter().enumerate() {
            self.input
                .prepare_rms_norm(&self.hidden, &layer.attn_norm, c.rms_eps_q32, kernel)?;
            mark(&mut self.profile, &mut clock, Phase::Norm);
            kernels::project_many(
                &[
                    (MatrixRows::of(&layer.wq), &self.input),
                    (MatrixRows::of(&layer.wk), &self.input),
                    (MatrixRows::of(&layer.wv), &self.input),
                ],
                &mut self.qkv,
            )?;
            mark(&mut self.profile, &mut clock, Phase::Qkv);
            {
                let (q, kv) = self.qkv.split_at_mut(dq);
                let (k, v) = kv.split_at_mut(dkv);
                if c.rope_layers[l] {
                    for head in q.chunks_exact_mut(c.d_head) {
                        rope_split_half(head, cos, sin)?;
                    }
                    for head in k.chunks_exact_mut(c.d_head) {
                        rope_split_half(head, cos, sin)?;
                    }
                }
                self.cache.push(l, k, v)?;
            }
            mark(&mut self.profile, &mut clock, Phase::RopeKv);
            self.attend(l, position + 1)?;
            mark(&mut self.profile, &mut clock, Phase::Attention);
            self.input.prepare(&self.attended, kernel)?;
            kernels::project(MatrixRows::of(&layer.wo), &self.input, &mut self.projected)?;
            mark(&mut self.profile, &mut clock, Phase::Wo);
            add_residual(&mut self.hidden, &self.projected)?;
            mark(&mut self.profile, &mut clock, Phase::Residual);
            self.input
                .prepare_rms_norm(&self.hidden, &layer.ffn_norm, c.rms_eps_q32, kernel)?;
            mark(&mut self.profile, &mut clock, Phase::Norm);
            kernels::project_swiglu(
                MatrixRows::of(&layer.w_gate),
                MatrixRows::of(&layer.w_up),
                &self.input,
                &mut self.act,
            )?;
            mark(&mut self.profile, &mut clock, Phase::GateUpSilu);
            self.input.prepare(&self.act, kernel)?;
            kernels::project(
                MatrixRows::of(&layer.w_down),
                &self.input,
                &mut self.projected,
            )?;
            mark(&mut self.profile, &mut clock, Phase::Down);
            add_residual(&mut self.hidden, &self.projected)?;
            mark(&mut self.profile, &mut clock, Phase::Residual);
        }
        self.cache.advance();
        self.input
            .prepare_rms_norm(&self.hidden, &model.final_norm, c.rms_eps_q32, kernel)?;
        mark(&mut self.profile, &mut clock, Phase::Norm);
        kernels::project(MatrixRows::of(&model.embed), &self.input, &mut self.logits)?;
        mark(&mut self.profile, &mut clock, Phase::LmHead);
        Ok(&self.logits)
    }

    /// Attention of every query head at layer `layer` over `positions`
    /// cached positions (spec §5.6), heads in parallel.
    fn attend(&mut self, layer: usize, positions: usize) -> Result<(), ModernError> {
        let model = self.model;
        let c = &model.config;
        let (keys, values) = self.cache.layer(layer);
        let group = c.n_heads / c.n_kv_heads;
        let (d_head, d_kv) = (c.d_head, c.d_kv());
        let (isa, lambda) = (self.isa, self.lambda);
        let q = &self.qkv[..c.d_q()];
        self.attended
            .par_chunks_mut(d_head)
            .zip(q.par_chunks(d_head))
            .zip(self.q32.par_chunks_mut(d_head))
            .zip(self.scores.par_chunks_mut(self.score_stride))
            .zip(self.weighted.par_chunks_mut(d_head))
            .enumerate()
            .try_for_each(|(head, ((((out, q_head), q32), scores), weighted))| {
                let view = HeadCache {
                    keys,
                    values,
                    positions,
                    stride: d_kv,
                    offset: (head / group) * d_head,
                };
                attend_head(
                    isa,
                    q_head,
                    view,
                    lambda,
                    out,
                    q32,
                    &mut scores[..positions],
                    weighted,
                )
            })
    }
}

/// One query head (spec §5.6), the same integers as
/// [`arith::attention_head`]: exact dot products, the exact maximum, exact
/// sums and one truncating division. The SIMD loops need every `|q_t| < 2^31`
/// and `Σ|q_t| < 2^32` (the reference's i64 case); anything else, and the
/// scalar kernel, runs the reference function itself.
#[allow(clippy::too_many_arguments)]
fn attend_head(
    isa: Isa,
    q: &[i64],
    cache: HeadCache<'_>,
    lambda: i64,
    out: &mut [i64],
    q32: &mut [i32],
    scores: &mut [i64],
    weighted: &mut [i64],
) -> Result<(), ModernError> {
    let width = q.len();
    let q_mass: u128 = q.iter().map(|v| u128::from(v.unsigned_abs())).sum();
    let narrow = q_mass < (1u128 << 32) && q.iter().all(|&v| i32::try_from(v).is_ok());
    if !narrow || isa.kernel() == Kernel::Scalar {
        return arith::attention_head(q, cache, lambda, out);
    }
    let last = cache
        .positions
        .checked_sub(1)
        .and_then(|p| p.checked_mul(cache.stride))
        .and_then(|base| base.checked_add(cache.offset + width));
    if cache.positions == 0
        || out.len() != width
        || q32.len() != width
        || weighted.len() != width
        || scores.len() != cache.positions
        || last.is_none_or(|end| end > cache.keys.len() || end > cache.values.len())
    {
        return Err(ModernError::Invalid("attention cache shape".into()));
    }
    for (narrowed, &v) in q32.iter_mut().zip(q) {
        *narrowed = v as i32;
    }
    for (position, score) in scores.iter_mut().enumerate() {
        let base = position * cache.stride + cache.offset;
        // |k| < 2^31 and Σ|q| < 2^32 bound every partial sum below 2^63.
        let dot = isa.dot_i32(q32, &cache.keys[base..base + width]);
        let scaled = i128::from(dot)
            .checked_mul(i128::from(lambda))
            .ok_or_else(|| ModernError::Domain("attention score product beyond 2^127".into()))?;
        *score = to_activation(scaled >> 46, "attention score beyond 2^62")?;
    }
    let max_score = scores.iter().copied().max().unwrap_or(0);
    weighted.fill(0);
    let mut total: i64 = 0;
    for (position, &score) in scores.iter().enumerate() {
        let weight = exp_q16(score - max_score);
        if weight == 0 {
            continue;
        }
        total += weight;
        let base = position * cache.stride + cache.offset;
        // 0 < weight <= 2^16, so it fits i32.
        isa.axpy_i32(weighted, weight as i32, &cache.values[base..base + width]);
    }
    for (slot, &acc) in out.iter_mut().zip(weighted.iter()) {
        *slot = acc / total;
    }
    Ok(())
}

impl TokenForward for Engine<'_> {
    fn begin(&mut self, positions: usize) {
        self.begin_sequence(positions);
    }

    fn forward_token(&mut self, token: u32) -> Result<&[i64], ModernError> {
        self.forward(token)
    }

    fn kv_cache(&self) -> &KvCache {
        &self.cache
    }

    fn set_kv_cache(&mut self, cache: KvCache) {
        self.cache = cache;
    }
}

// ------------------------------------------------------------------ specs --

/// The kernel of a reference-path spec.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ReferenceKernel {
    /// The opt-in limb kernel of [`crate::canonical_simd`] (the `simd` path
    /// before this engine existed); the scalar kernel where it refuses.
    Legacy,
    /// An exact kernel of [`super::kernels`].
    Exact(Kernel),
}

/// A forward pass and the kernel its dot products use, as named on the
/// command line, in run files and in CI.
///
/// | name | forward pass | kernel |
/// |---|---|---|
/// | `scalar`, `ref:scalar` | [`ModernModel::forward`] | scalar |
/// | `legacy`, `ref:legacy` | [`ModernModel::forward`] | legacy limb kernel |
/// | `ref:avx2`, `ref:neon`, `ref:auto` | [`ModernModel::forward`] | that kernel |
/// | `simd`, `fast`, `fast:auto` | [`Engine`] | fastest SIMD kernel here |
/// | `fast:scalar`, `fast:avx2`, `fast:neon` | [`Engine`] | that kernel |
///
/// Every spec computes the same logits; CI checks each against the golden
/// digests. Naming a kernel this CPU lacks is an error, never a silent
/// fallback.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Spec {
    Reference(ReferenceKernel),
    Fast(Kernel),
}

impl Spec {
    /// Parse a spec name (see the table on [`Spec`]).
    pub fn parse(name: &str) -> Result<Self, ModernError> {
        let spec = match name {
            "scalar" => Spec::Reference(ReferenceKernel::Exact(Kernel::Scalar)),
            "legacy" | "ref:legacy" => Spec::Reference(ReferenceKernel::Legacy),
            "simd" | "fast" | "fast:auto" => Spec::Fast(simd_kernel()?),
            _ => match name.split_once(':') {
                Some(("ref", kernel)) => {
                    Spec::Reference(ReferenceKernel::Exact(Kernel::parse(kernel)?))
                }
                Some(("fast", kernel)) => Spec::Fast(Kernel::parse(kernel)?),
                _ => {
                    return Err(ModernError::Invalid(format!(
                        "unknown kernel spec {name} (scalar, simd, legacy, ref:<kernel>, \
                         fast:<kernel>; kernels: scalar, avx2, neon, auto)"
                    )));
                }
            },
        };
        spec.check_available()?;
        Ok(spec)
    }

    /// Canonical name: `ref:scalar`, `ref:legacy`, `fast:avx2`, ...
    pub fn name(self) -> String {
        format!("{}:{}", self.engine_name(), self.kernel_name())
    }

    /// `reference` or `fast`.
    pub fn engine_name(self) -> &'static str {
        match self {
            Spec::Reference(_) => "ref",
            Spec::Fast(_) => "fast",
        }
    }

    /// The kernel's name (`legacy` for the legacy limb kernel).
    pub fn kernel_name(self) -> &'static str {
        match self {
            Spec::Reference(ReferenceKernel::Legacy) => "legacy",
            Spec::Reference(ReferenceKernel::Exact(kernel)) | Spec::Fast(kernel) => kernel.name(),
        }
    }

    fn check_available(self) -> Result<(), ModernError> {
        let available = match self {
            Spec::Reference(ReferenceKernel::Legacy) => canonical_simd::dotprod_available(),
            Spec::Reference(ReferenceKernel::Exact(kernel)) | Spec::Fast(kernel) => {
                kernel.available()
            }
        };
        if available {
            Ok(())
        } else {
            Err(ModernError::Invalid(format!(
                "kernel {} is not available on this CPU (available: {})",
                self.name(),
                Kernel::available_kernels()
                    .iter()
                    .map(|k| k.name())
                    .collect::<Vec<_>>()
                    .join(", ")
            )))
        }
    }

    /// Set the process-wide switches the reference path reads. Call before
    /// running a spec; fast specs need none and clear them.
    pub fn apply(self) {
        // Read the ARC_FAST_CANONICAL_KERNEL default first, so the explicit
        // choice below is the one that stays in force.
        let _ = canonical_simd::fast_canonical_kernel_enabled();
        match self {
            Spec::Reference(ReferenceKernel::Legacy) => {
                canonical_simd::set_fast_canonical_kernel(true);
                kernels::set_reference_kernel(Kernel::Scalar);
            }
            Spec::Reference(ReferenceKernel::Exact(kernel)) => {
                canonical_simd::set_fast_canonical_kernel(false);
                kernels::set_reference_kernel(kernel);
            }
            Spec::Fast(_) => {
                canonical_simd::set_fast_canonical_kernel(false);
                kernels::set_reference_kernel(Kernel::Scalar);
            }
        }
    }

    /// A forward pass for `model` (call [`Spec::apply`] first).
    pub fn runner<'m>(self, model: &'m ModernModel) -> Box<dyn TokenForward + 'm> {
        match self {
            Spec::Reference(_) => Box::new(ReferenceRun::new(model)),
            Spec::Fast(kernel) => Box::new(Engine::new(model, kernel)),
        }
    }

    /// Switch on and zero the census of every kernel.
    pub fn start_census() {
        canonical_simd::set_projection_census_enabled(true);
        canonical_simd::reset_projection_census();
        kernels::set_census_enabled(true);
        kernels::reset_census();
    }

    /// The census of the kernel this spec uses.
    pub fn census(self) -> ProjectionCensus {
        match self {
            Spec::Reference(ReferenceKernel::Legacy) => canonical_simd::projection_census(),
            _ => kernels::census(),
        }
    }
}

fn simd_kernel() -> Result<Kernel, ModernError> {
    match Kernel::best() {
        Kernel::Scalar => Err(ModernError::Invalid(
            "--kernel simd needs NEON dotprod (arm64) or AVX2 (x86-64)".into(),
        )),
        kernel => Ok(kernel),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::modern::arith::Selection;
    use crate::modern::model::tests::{lcg_matrix, tiny_model};
    use crate::modern::model::{GenerationRequest, generate_with};

    const TOKENS: [u32; 9] = [3, 17, 5, 39, 0, 12, 7, 7, 30];

    /// Reference and engine logits for the same tokens, and both caches.
    type Trace = (Vec<Vec<i64>>, [u8; 32]);

    fn reference_trace(model: &ModernModel, tokens: &[u32]) -> Result<Trace, ModernError> {
        let mut cache = model.new_cache();
        let mut logits = Vec::new();
        for &t in tokens {
            logits.push(model.forward(t, &mut cache)?);
        }
        Ok((logits, cache.digest()))
    }

    fn engine_trace(
        model: &ModernModel,
        kernel: Kernel,
        tokens: &[u32],
    ) -> Result<Trace, ModernError> {
        let mut engine = Engine::new(model, kernel);
        engine.begin_sequence(tokens.len());
        let mut logits = Vec::new();
        for &t in tokens {
            logits.push(engine.forward(t)?.to_vec());
        }
        Ok((logits, engine.cache.digest()))
    }

    #[test]
    fn the_engine_matches_the_reference_for_every_kernel_and_thread_count() {
        let model = tiny_model();
        let expected = reference_trace(&model, &TOKENS).unwrap();
        for kernel in Kernel::available_kernels() {
            for threads in [1usize, 3] {
                let pool = rayon::ThreadPoolBuilder::new()
                    .num_threads(threads)
                    .build()
                    .unwrap();
                let got = pool.install(|| engine_trace(&model, kernel, &TOKENS).unwrap());
                assert_eq!(got, expected, "kernel {} threads {threads}", kernel.name());
            }
        }
    }

    #[test]
    fn large_activations_take_more_digits_or_the_scalar_path_and_stay_exact() {
        // Norm gains of 2^k push every projection input up by 2^k, through
        // every digit count and past the SIMD domains.
        for shift in [6u32, 14, 22, 30] {
            let mut model = tiny_model();
            for layer in &mut model.layers {
                for g in layer.attn_norm.iter_mut().chain(layer.ffn_norm.iter_mut()) {
                    *g <<= shift;
                }
            }
            let expected = reference_trace(&model, &TOKENS[..4]);
            for kernel in Kernel::available_kernels() {
                let got = engine_trace(&model, kernel, &TOKENS[..4]);
                match (&expected, &got) {
                    (Ok(e), Ok(g)) => assert_eq!(g, e, "{} shift {shift}", kernel.name()),
                    (Err(e), Err(g)) => assert_eq!(
                        std::mem::discriminant(e),
                        std::mem::discriminant(g),
                        "{} shift {shift}: {e} vs {g}",
                        kernel.name()
                    ),
                    _ => panic!(
                        "{} shift {shift}: reference {:?}, engine {:?}",
                        kernel.name(),
                        expected.as_ref().map(|_| ()),
                        got.as_ref().map(|_| ())
                    ),
                }
            }
        }
    }

    #[test]
    fn the_engine_refuses_when_the_reference_refuses() {
        // A query row whose scale shift is the minimum turns an ordinary
        // accumulator into an output beyond 2^62.
        let mut model = tiny_model();
        let mut seed = 99u64;
        model.layers[1].wq = lcg_matrix(&mut seed, 16, 16);
        model.layers[1].wq.k[3] = 16;
        model.layers[1].wq.mu[3] = i32::MAX;
        for g in &mut model.layers[1].attn_norm {
            *g <<= 24;
        }
        let expected = reference_trace(&model, &TOKENS[..3]);
        assert!(matches!(expected, Err(ModernError::Domain(_))));
        for kernel in Kernel::available_kernels() {
            let got = engine_trace(&model, kernel, &TOKENS[..3]);
            assert!(
                matches!(got, Err(ModernError::Domain(_))),
                "{}: {:?}",
                kernel.name(),
                got.map(|_| ())
            );
        }
    }

    #[test]
    fn generation_is_identical_with_every_spec() {
        let model = tiny_model();
        let prompt = [1u32, 2, 3, 9];
        let request = GenerationRequest {
            prompt: &prompt,
            max_tokens: 8,
            eos: &[],
            selection: Selection::Rp64Argmax,
        };
        let expected = model.generate(&request).unwrap();
        let _guard = canonical_simd::kernel_switch_guard();
        let mut names = vec!["scalar", "fast:scalar"];
        if Kernel::best() != Kernel::Scalar {
            names.extend(["simd", "legacy", "ref:auto", "fast:auto"]);
        }
        for name in names {
            let spec = Spec::parse(name).unwrap();
            spec.apply();
            let mut runner = spec.runner(&model);
            let got = generate_with(runner.as_mut(), &model.config, &request).unwrap();
            assert_eq!(got.tokens, expected.tokens, "{name}");
            assert_eq!(got.logits_hashes, expected.logits_hashes, "{name}");
            assert_eq!(got.logits_digest, expected.logits_digest, "{name}");
            assert_eq!(runner.kv_cache().positions(), 4 + 8 - 1, "{name}");
        }
        Spec::Reference(ReferenceKernel::Exact(Kernel::Scalar)).apply();
    }

    #[test]
    fn specs_parse_and_name_themselves() {
        assert_eq!(Spec::parse("scalar").unwrap().name(), "ref:scalar");
        assert_eq!(Spec::parse("fast:scalar").unwrap().name(), "fast:scalar");
        assert!(Spec::parse("turbo").is_err());
        assert!(Spec::parse("fast:avx512").is_err());
        for kernel in Kernel::ALL {
            let fast = format!("fast:{}", kernel.name());
            assert_eq!(Spec::parse(&fast).is_ok(), kernel.available(), "{fast}");
        }
        if Kernel::best() != Kernel::Scalar {
            assert_eq!(
                Spec::parse("simd").unwrap(),
                Spec::Fast(Kernel::best()),
                "simd is the fast engine with the best kernel"
            );
        }
    }

    #[test]
    fn profiling_accounts_for_the_forward_pass() {
        let model = tiny_model();
        let mut engine = Engine::new(&model, Kernel::best());
        engine.begin_sequence(4);
        engine.set_profiling(true);
        for &t in &TOKENS[..4] {
            engine.forward(t).unwrap();
        }
        let phases = engine.phase_seconds();
        assert_eq!(phases.len(), PHASE_NAMES.len());
        assert!(phases.iter().all(|&(_, s)| s >= 0.0));
        engine.set_profiling(false);
        assert!(engine.phase_seconds().is_empty());
    }
}
