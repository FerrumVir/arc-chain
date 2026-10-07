//! Backend selection for a worker that serves the dyadic profile: the GPU is
//! used only after it reproduces the CPU golden bit for bit, on this adapter
//! and driver, in this process. Otherwise the worker stays on the CPU and
//! says why.
//!
//! The gate ([`select`]), off unless a worker opts in:
//!
//! 1. The CPU runs the self-test workload (a deterministic model built in
//!    process, fixed prompts, both selection rules) and its digest must equal
//!    the pinned [`SELF_TEST_GOLDEN`]. A CPU build that drifted cannot vouch
//!    for a GPU.
//! 2. The selected adapter runs the operator known-answer test ([`kat`]):
//!    every kernel against the CPU operator, refusals included.
//! 3. The adapter runs the same self-test workload through the engine options
//!    the worker serves with. Its digest must equal the CPU golden exactly.
//!
//! Any failure, error or difference selects the CPU, with the reason. A model
//! the worker then serves ([`ModernBackend::for_model`]) is uploaded to the
//! same adapter and spot-checked against the CPU forward pass before the
//! first request (large matrices are chunked to the adapter's binding limit,
//! which the small self-test model never reaches). While serving, a GPU
//! execution failure, or a GPU refusal of an input the CPU accepts, moves the
//! worker to the CPU for good and the request is answered by the CPU.
//! Nothing here changes a digest: the GPU's results are the CPU's results.

use std::time::Instant;

use arc_gpu::modern::{AdapterReport, EngineOptions, GpuEngine, GpuModernError, OpLab};
use serde_json::{Value, json};

use super::{adapter_json, engine_for, generate_with, gpu_error, kat};
use crate::modern::arith::{self, DyadicMatrix, Selection};
use crate::modern::model::{
    GenerationOutput, GenerationRequest, ModernConfig, ModernLayer, ModernModel,
};
use crate::modern::tables::rope_tables;
use crate::modern::{ModernError, hex_lower};

/// The Proof Kit's `runs[].backend` (`arc.proof-result.v1`) for a GPU run.
pub const PROOF_BACKEND: &str = "gpu-wgpu";
/// The backend name of a CPU decision.
pub const CPU_BACKEND: &str = "cpu";
/// Schema of [`BackendDecision::to_json`].
pub const DECISION_SCHEMA: &str = "arc.inference-backend.v1";
/// Domain separator of the self-test digest.
const SELF_TEST_DOMAIN: &[u8] = b"arc.gpu-self-test.v1";
/// The CPU golden of the self-test workload: BLAKE3 over every case's
/// output hash and logits digest. Pinned, and re-derived on the CPU at every
/// selection; CI checks the pin on Linux, Windows and macOS.
pub const SELF_TEST_GOLDEN: &str =
    "c6349c34493398680a1bd452b1e2e0c848e9246021d9ed7c411fc8f1b0c64193";
/// Seed of the startup operator known-answer test (distinct from CI's).
pub const SELF_TEST_KAT_SEED: u64 = 0x00A2_5E1F;
/// Default operator known-answer rounds at startup (the Proof Kit's default).
pub const DEFAULT_SELF_TEST_ROUNDS: usize = 8;
/// Default tokens per GPU forward pass for prompts.
pub const DEFAULT_BATCH: usize = 8;
/// Forward passes of a served model compared with the CPU after upload.
pub const SPOT_CHECK_FORWARDS: usize = 2;

/// The worker's switch. `enabled` is false by default: the CPU serves.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct GpuBackendConfig {
    pub enabled: bool,
    /// Adapter index or name substring (`arc-modern gpu-info` lists them);
    /// default: the best hardware GPU, software rasterizers last.
    pub adapter: Option<String>,
    /// Operator known-answer rounds before the golden workload.
    pub self_test_rounds: usize,
    /// Tokens per GPU forward pass for prompts (results do not depend on it).
    pub batch: usize,
}

impl Default for GpuBackendConfig {
    fn default() -> Self {
        Self {
            enabled: false,
            adapter: None,
            self_test_rounds: DEFAULT_SELF_TEST_ROUNDS,
            batch: DEFAULT_BATCH,
        }
    }
}

impl GpuBackendConfig {
    fn engine_options(&self) -> EngineOptions {
        EngineOptions {
            adapter: self.adapter.clone(),
            max_positions: None,
            batch: Some(self.batch),
        }
    }
}

/// Deterministic pseudo-random dyadic matrix (an LCG, identical everywhere).
fn lcg_matrix(seed: &mut u64, rows: usize, cols: usize) -> DyadicMatrix {
    let mut next = || {
        *seed = seed
            .wrapping_mul(6_364_136_223_846_793_005)
            .wrapping_add(1_442_695_040_888_963_407);
        *seed >> 33
    };
    let q = (0..rows * cols)
        .map(|_| ((next() % 255) as i64 - 127) as i8)
        .collect();
    let mu = (0..rows)
        .map(|_| ((1u64 << 30) + next() % (1 << 30)) as i32)
        .collect();
    let k = (0..rows).map(|_| 40 + (next() % 3) as u8).collect();
    DyadicMatrix {
        rows,
        cols,
        q,
        mu,
        k,
    }
}

/// The self-test model: three layers (RoPE, NoPE, RoPE), grouped-query
/// attention, a tied embedding. Built in process so the gate needs no file.
pub fn self_test_model() -> ModernModel {
    let config = ModernConfig {
        architecture: "smollm3".into(),
        n_layers: 3,
        d_model: 32,
        n_heads: 4,
        n_kv_heads: 2,
        d_head: 8,
        d_ff: 48,
        vocab_size: 64,
        max_seq: 64,
        rms_eps_q32: 4295,
        rope_theta: 5_000_000,
        rope_layers: vec![true, false, true],
    };
    let (rope_cos, rope_sin) = rope_tables(config.rope_theta, config.d_head, config.max_seq)
        .expect("the self-test model's RoPE parameters are in range");
    let (d, dkv, ff) = (config.d_model, config.d_kv(), config.d_ff);
    let mut seed = 0x00A2_5E1F_u64;
    let layers = (0..config.n_layers)
        .map(|l| ModernLayer {
            attn_norm: vec![arith::ONE + 500 * l as i64; d],
            wq: lcg_matrix(&mut seed, d, d),
            wk: lcg_matrix(&mut seed, dkv, d),
            wv: lcg_matrix(&mut seed, dkv, d),
            wo: lcg_matrix(&mut seed, d, d),
            ffn_norm: vec![arith::ONE + 1000; d],
            w_gate: lcg_matrix(&mut seed, ff, d),
            w_up: lcg_matrix(&mut seed, ff, d),
            w_down: lcg_matrix(&mut seed, d, ff),
        })
        .collect();
    let model = ModernModel {
        embed: lcg_matrix(&mut seed, config.vocab_size, d),
        final_norm: vec![arith::ONE; d],
        rope_cos,
        rope_sin,
        layers,
        config,
    };
    model
        .validate()
        .expect("the self-test model is a valid profile model");
    model
}

/// The self-test prompts: `(prompt, max_tokens, selection)`. A single token,
/// short and batch-straddling prompts, and one that fills most of the context.
fn self_test_cases() -> Vec<(Vec<u32>, usize, Selection)> {
    let long: Vec<u32> = (0..21u32).map(|i| (i * 37 + 11) % 64).collect();
    vec![
        (vec![1], 12, Selection::Rp64Argmax),
        (vec![1, 2, 3], 12, Selection::Argmax),
        (
            vec![5, 9, 2, 33, 7, 0, 12, 4, 63],
            16,
            Selection::Rp64Argmax,
        ),
        (long, 24, Selection::Rp64Argmax),
    ]
}

/// The self-test digest of `model` under a generation function (the CPU
/// engine or a GPU engine).
fn self_test_digest<G>(model: &ModernModel, mut generate: G) -> Result<String, ModernError>
where
    G: FnMut(&GenerationRequest<'_>) -> Result<GenerationOutput, ModernError>,
{
    let mut hasher = blake3::Hasher::new();
    hasher.update(SELF_TEST_DOMAIN);
    for (prompt, max_tokens, selection) in self_test_cases() {
        let out = generate(&GenerationRequest {
            prompt: &prompt,
            max_tokens: max_tokens.min(model.config.max_seq - prompt.len()),
            eos: &[],
            selection,
        })?;
        hasher.update(&out.output_hash);
        hasher.update(&out.logits_digest);
    }
    Ok(hasher.finalize().to_hex().to_string())
}

/// The self-test digest on the CPU engine (must equal [`SELF_TEST_GOLDEN`]).
pub fn cpu_self_test_digest() -> Result<String, ModernError> {
    let model = self_test_model();
    self_test_digest(&model, |request| model.generate(request))
}

/// What the adapter produced in the self-test.
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize)]
pub struct GpuSelfTest {
    pub adapter: AdapterReport,
    /// Operator known-answer test on the same adapter.
    pub kat: kat::KatReport,
    pub kat_rounds: usize,
    /// The self-test digest the adapter computed.
    pub digest: String,
}

/// Run the self-test of `model` on the adapter `config` selects.
fn gpu_self_test_of(
    model: &ModernModel,
    config: &GpuBackendConfig,
) -> Result<GpuSelfTest, ModernError> {
    let lab = OpLab::new(config.adapter.as_deref()).map_err(gpu_error)?;
    let kat = kat::run(&lab, SELF_TEST_KAT_SEED, config.self_test_rounds);
    let kat_adapter = lab.report().clone();
    drop(lab);
    let mut engine = engine_for(model, &config.engine_options())?;
    if !same_adapter(engine.report(), &kat_adapter) {
        return Err(ModernError::Invalid(format!(
            "the operator test ran on {} and the engine on {}",
            kat_adapter.name,
            engine.report().name
        )));
    }
    let digest = self_test_digest(model, |request| {
        super::generate_gpu(&mut engine, &model.config, request)
    })?;
    Ok(GpuSelfTest {
        adapter: engine.report().clone(),
        kat,
        kat_rounds: config.self_test_rounds,
        digest,
    })
}

/// The self-test on the adapter `config` selects.
pub fn gpu_self_test(config: &GpuBackendConfig) -> Result<GpuSelfTest, ModernError> {
    gpu_self_test_of(&self_test_model(), config)
}

fn same_adapter(a: &AdapterReport, b: &AdapterReport) -> bool {
    a.index == b.index && a.name == b.name && a.backend == b.backend && a.driver == b.driver
}

/// Which backend serves, and why.
#[derive(Debug, Clone, PartialEq, serde::Serialize)]
pub struct BackendDecision {
    /// [`PROOF_BACKEND`] (`gpu-wgpu`) or [`CPU_BACKEND`] (`cpu`).
    pub backend: &'static str,
    /// One sentence: why this backend.
    pub reason: String,
    /// The pinned CPU golden of the self-test.
    pub expected_digest: String,
    /// The self-test digest the CPU computed in this process.
    pub cpu_digest: Option<String>,
    /// What the adapter did, when it ran.
    pub gpu: Option<GpuSelfTest>,
    /// Seconds the gate took.
    pub seconds: f64,
}

impl BackendDecision {
    pub fn uses_gpu(&self) -> bool {
        self.backend == PROOF_BACKEND
    }

    fn cpu(reason: impl Into<String>) -> Self {
        Self {
            backend: CPU_BACKEND,
            reason: reason.into(),
            expected_digest: SELF_TEST_GOLDEN.to_string(),
            cpu_digest: None,
            gpu: None,
            seconds: 0.0,
        }
    }

    /// The adapter that serves, when the GPU serves.
    pub fn adapter(&self) -> Option<&AdapterReport> {
        self.gpu
            .as_ref()
            .filter(|_| self.uses_gpu())
            .map(|g| &g.adapter)
    }

    /// The decision as JSON: the backend, the reason, the self-test digests
    /// and the adapter twice, as the Proof Kit's `runs[].adapter`
    /// (`proof_adapter`) and as wgpu reported it (`adapter`).
    pub fn to_json(&self) -> Value {
        let tested = self.gpu.as_ref();
        json!({
            "schema": DECISION_SCHEMA,
            "backend": self.backend,
            "reason": self.reason,
            "self_test": {
                "expected_digest": self.expected_digest,
                "cpu_digest": self.cpu_digest,
                "gpu_digest": tested.map(|g| g.digest.clone()),
                "kat_rounds": tested.map(|g| g.kat_rounds),
                "kat_cases": tested.map(|g| g.kat.cases),
                "kat_mismatches": tested.map(|g| g.kat.mismatch_count),
                "seconds": self.seconds,
            },
            "adapter": tested.map(|g| &g.adapter),
            "proof_adapter": tested.map(|g| adapter_json(&g.adapter)),
        })
    }
}

/// The gate's rule, given the CPU's self-test digest and what the adapter
/// produced. The GPU serves only when the CPU reproduces the pinned golden,
/// the adapter's operator test has no mismatch, and the adapter's self-test
/// digest equals the golden exactly.
pub fn decide(
    expected: &str,
    cpu_digest: Result<String, ModernError>,
    gpu: Result<GpuSelfTest, ModernError>,
) -> BackendDecision {
    let mut decision = BackendDecision {
        expected_digest: expected.to_string(),
        ..BackendDecision::cpu("")
    };
    let cpu_digest = match cpu_digest {
        Ok(digest) => digest,
        Err(error) => {
            decision.reason = format!("the CPU self-test failed ({error}); serving on the CPU");
            return decision;
        }
    };
    decision.cpu_digest = Some(cpu_digest.clone());
    if cpu_digest != expected {
        decision.reason = format!(
            "the CPU self-test digest {cpu_digest} differs from the pinned golden {expected}, \
             so it cannot vouch for a GPU; serving on the CPU"
        );
        return decision;
    }
    let gpu = match gpu {
        Ok(gpu) => gpu,
        Err(error) => {
            decision.reason = format!("no GPU passed the self-test ({error}); serving on the CPU");
            return decision;
        }
    };
    let name = gpu.adapter.name.clone();
    let reason = if !gpu.kat.passed() {
        Some(format!(
            "{name}: the operator self-test found {} mismatches in {} cases (first: {}); \
             serving on the CPU",
            gpu.kat.mismatch_count,
            gpu.kat.cases,
            gpu.kat.mismatches.first().map_or("-", String::as_str)
        ))
    } else if gpu.digest != expected {
        Some(format!(
            "{name}: the GPU self-test digest {} differs from the CPU golden {expected}; \
             serving on the CPU",
            gpu.digest
        ))
    } else {
        None
    };
    match reason {
        Some(reason) => decision.reason = reason,
        None => {
            decision.backend = PROOF_BACKEND;
            decision.reason = format!(
                "{name} ({}) reproduced the CPU golden {expected} bit for bit and passed the \
                 operator self-test ({} cases); serving on the GPU",
                gpu.adapter.backend, gpu.kat.cases
            );
        }
    }
    decision.gpu = Some(gpu);
    decision
}

/// The gate. `config.enabled == false` (the default) selects the CPU without
/// touching a GPU.
pub fn select(config: &GpuBackendConfig) -> BackendDecision {
    if !config.enabled {
        return BackendDecision::cpu("the GPU backend is off (the default); serving on the CPU");
    }
    let start = Instant::now();
    let mut decision = decide(
        SELF_TEST_GOLDEN,
        cpu_self_test_digest(),
        gpu_self_test(config),
    );
    decision.seconds = start.elapsed().as_secs_f64();
    decision
}

/// A served model and the backend that runs it.
pub struct ModernBackend {
    decision: BackendDecision,
    engine: Option<GpuEngine>,
}

impl ModernBackend {
    /// Gate the GPU ([`select`]), then upload `model` to the adapter that
    /// passed and compare its first forward passes with the CPU's. Any
    /// failure leaves the CPU serving, with the reason.
    pub fn for_model(model: &ModernModel, config: &GpuBackendConfig) -> Self {
        let decision = select(config);
        Self::with_decision(model, config, decision)
    }

    /// [`Self::for_model`] with the gate's decision already taken.
    pub fn with_decision(
        model: &ModernModel,
        config: &GpuBackendConfig,
        mut decision: BackendDecision,
    ) -> Self {
        let Some(tested) = decision.adapter().cloned() else {
            return Self {
                decision,
                engine: None,
            };
        };
        let engine = engine_for(model, &config.engine_options()).and_then(|mut engine| {
            if !same_adapter(engine.report(), &tested) {
                return Err(ModernError::Invalid(format!(
                    "the model went to {} but {} passed the self-test",
                    engine.report().name,
                    tested.name
                )));
            }
            spot_check(model, &mut engine)?;
            Ok(engine)
        });
        match engine {
            Ok(engine) => Self {
                decision,
                engine: Some(engine),
            },
            Err(error) => {
                decision.backend = CPU_BACKEND;
                decision.reason = format!(
                    "{}: the served model failed its GPU check ({error}); serving on the CPU",
                    tested.name
                );
                Self {
                    decision,
                    engine: None,
                }
            }
        }
    }

    pub fn decision(&self) -> &BackendDecision {
        &self.decision
    }

    fn fall_back(&mut self, reason: String) {
        self.engine = None;
        self.decision.backend = CPU_BACKEND;
        self.decision.reason = reason;
    }

    /// Generate on the backend in use. The output is the CPU engine's output
    /// for every request; see the module documentation for the fallbacks.
    pub fn generate(
        &mut self,
        model: &ModernModel,
        request: &GenerationRequest<'_>,
    ) -> Result<GenerationOutput, ModernError> {
        let Some(engine) = self.engine.as_mut() else {
            return model.generate(request);
        };
        if request.prompt.len() + request.max_tokens > engine.capacity() {
            // Beyond the GPU's KV capacity: the CPU answers (or refuses).
            return model.generate(request);
        }
        engine.reset();
        let batch = engine.batch();
        let mut gpu_failure: Option<GpuModernError> = None;
        let result = generate_with(&model.config, request, batch, |tokens| {
            engine.forward_batch(tokens).map_err(|error| match error {
                GpuModernError::Domain(message) => ModernError::Domain(message),
                other => {
                    let message = other.to_string();
                    gpu_failure = Some(other);
                    ModernError::Invalid(message)
                }
            })
        });
        if let Some(error) = gpu_failure {
            let name = engine.report().name.clone();
            self.fall_back(format!(
                "{name}: GPU execution failed while serving ({error}); serving on the CPU"
            ));
            return model.generate(request);
        }
        match result {
            Err(ModernError::Domain(gpu_refusal)) => {
                // The GPU refused. The CPU decides: the same refusal, or an
                // answer, which means the GPU disagreed and must stop serving.
                let cpu = model.generate(request);
                if cpu.is_ok() {
                    let name = engine.report().name.clone();
                    self.fall_back(format!(
                        "{name}: the GPU refused an input the CPU accepts ({gpu_refusal}); \
                         serving on the CPU"
                    ));
                }
                cpu
            }
            other => other,
        }
    }
}

/// The first [`SPOT_CHECK_FORWARDS`] forward passes of a fixed prompt on the
/// GPU and the CPU must return identical logits.
fn spot_check(model: &ModernModel, engine: &mut GpuEngine) -> Result<(), ModernError> {
    let vocab = model.config.vocab_size as u32;
    let tokens: Vec<u32> = (0..SPOT_CHECK_FORWARDS as u32)
        .map(|i| (i * 7919 + 1) % vocab)
        .take(model.config.max_seq.min(engine.capacity()))
        .collect();
    engine.reset();
    let mut cache = model.new_cache();
    for (index, &token) in tokens.iter().enumerate() {
        let cpu = model.forward(token, &mut cache)?;
        let gpu = engine.forward(token).map_err(gpu_error)?;
        if cpu != gpu {
            engine.reset();
            return Err(ModernError::Invalid(format!(
                "forward {index}: GPU logits {} differ from CPU logits {}",
                hex_lower(&arith::logits_hash(&gpu)),
                hex_lower(&arith::logits_hash(&cpu))
            )));
        }
    }
    engine.reset();
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn fake_gpu(digest: &str, mismatches: usize) -> GpuSelfTest {
        GpuSelfTest {
            adapter: AdapterReport {
                index: 0,
                name: "Test GPU".into(),
                vendor_id: 0x10DE,
                vendor: "NVIDIA".into(),
                device_id: 1,
                device_type: "DiscreteGpu".into(),
                backend: "Vulkan".into(),
                driver: "test".into(),
                driver_info: "1.0".into(),
                software: false,
            },
            kat: kat::KatReport {
                cases: 100,
                refusals: 10,
                mismatches: (0..mismatches).map(|i| format!("case {i}")).collect(),
                mismatch_count: mismatches,
            },
            kat_rounds: 8,
            digest: digest.to_string(),
        }
    }

    const GOLDEN: &str = "aa";

    #[test]
    fn the_cpu_self_test_reproduces_the_pinned_golden() {
        let digest = cpu_self_test_digest().unwrap();
        assert_eq!(digest, SELF_TEST_GOLDEN, "re-pin SELF_TEST_GOLDEN");
        // Every case generates without a refusal, so the digest covers the
        // whole workload.
        let model = self_test_model();
        for (prompt, max_tokens, selection) in self_test_cases() {
            let out = model
                .generate(&GenerationRequest {
                    prompt: &prompt,
                    max_tokens,
                    eos: &[],
                    selection,
                })
                .unwrap();
            assert_eq!(out.tokens.len(), max_tokens);
        }
    }

    #[test]
    fn the_gpu_serves_only_on_an_exact_match() {
        let gpu = decide(GOLDEN, Ok(GOLDEN.into()), Ok(fake_gpu(GOLDEN, 0)));
        assert!(gpu.uses_gpu(), "{}", gpu.reason);
        assert_eq!(gpu.backend, PROOF_BACKEND);
        assert_eq!(gpu.adapter().unwrap().name, "Test GPU");
        assert!(gpu.reason.contains("bit for bit"));
    }

    #[test]
    fn a_digest_mismatch_falls_back_to_the_cpu() {
        let decision = decide(GOLDEN, Ok(GOLDEN.into()), Ok(fake_gpu("ab", 0)));
        assert!(!decision.uses_gpu());
        assert_eq!(decision.backend, CPU_BACKEND);
        assert!(decision.adapter().is_none());
        assert!(
            decision.reason.contains("GPU self-test digest ab differs"),
            "{}",
            decision.reason
        );
        // The evidence is kept for the log and the status page.
        assert_eq!(decision.gpu.as_ref().unwrap().digest, "ab");
        assert_eq!(decision.to_json()["self_test"]["gpu_digest"], "ab");
        assert_eq!(decision.to_json()["backend"], "cpu");
    }

    #[test]
    fn every_other_failure_falls_back_to_the_cpu() {
        let kat = decide(GOLDEN, Ok(GOLDEN.into()), Ok(fake_gpu(GOLDEN, 3)));
        assert!(!kat.uses_gpu());
        assert!(kat.reason.contains("3 mismatches"), "{}", kat.reason);
        let drifted_cpu = decide(GOLDEN, Ok("ab".into()), Ok(fake_gpu("ab", 0)));
        assert!(!drifted_cpu.uses_gpu());
        assert!(drifted_cpu.reason.contains("cannot vouch"));
        assert!(drifted_cpu.gpu.is_none());
        let cpu_error = decide(
            GOLDEN,
            Err(ModernError::Domain("x".into())),
            Ok(fake_gpu(GOLDEN, 0)),
        );
        assert!(!cpu_error.uses_gpu());
        let no_gpu = decide(
            GOLDEN,
            Ok(GOLDEN.into()),
            Err(gpu_error(GpuModernError::NoAdapter("none".into()))),
        );
        assert!(!no_gpu.uses_gpu());
        assert!(no_gpu.reason.contains("no usable GPU adapter"));
    }

    #[test]
    fn the_switch_is_off_by_default() {
        let config = GpuBackendConfig::default();
        assert!(!config.enabled);
        let decision = select(&config);
        assert!(!decision.uses_gpu());
        assert!(decision.reason.contains("off (the default)"));
        let mut backend = ModernBackend::with_decision(&self_test_model(), &config, decision);
        let model = self_test_model();
        let request = GenerationRequest {
            prompt: &[1, 2, 3],
            max_tokens: 5,
            eos: &[],
            selection: Selection::Argmax,
        };
        let served = backend.generate(&model, &request).unwrap();
        assert_eq!(served.tokens, model.generate(&request).unwrap().tokens);
    }

    /// The GPU config for tests: the CI adapter, or `None` (skipped) when
    /// this machine has no adapter and `ARC_GPU_REQUIRE` is not `1`.
    fn gpu_config_or_skip() -> Option<GpuBackendConfig> {
        let config = GpuBackendConfig {
            enabled: true,
            adapter: std::env::var("ARC_GPU_ADAPTER").ok(),
            self_test_rounds: 2,
            batch: 4,
        };
        match OpLab::new(config.adapter.as_deref()) {
            Ok(_) => Some(config),
            Err(error) => {
                let message = error.to_string();
                let required = std::env::var("ARC_GPU_REQUIRE").as_deref() == Ok("1");
                assert!(
                    !required && message.contains("no usable GPU adapter"),
                    "GPU unavailable: {message}"
                );
                eprintln!("SKIP (no GPU adapter): {message}");
                None
            }
        }
    }

    #[test]
    fn the_gate_selects_an_exact_gpu_and_serves_the_cpu_output() {
        let Some(config) = gpu_config_or_skip() else {
            return;
        };
        let decision = select(&config);
        eprintln!("decision: {}", decision.to_json());
        assert!(decision.uses_gpu(), "{}", decision.reason);
        assert_eq!(
            decision.gpu.as_ref().unwrap().digest,
            SELF_TEST_GOLDEN,
            "the GPU self-test digest is the CPU golden"
        );
        let model = self_test_model();
        let mut backend = ModernBackend::with_decision(&model, &config, decision);
        assert!(
            backend.decision().uses_gpu(),
            "{}",
            backend.decision().reason
        );
        for (prompt, max_tokens, selection) in self_test_cases() {
            let request = GenerationRequest {
                prompt: &prompt,
                max_tokens,
                eos: &[],
                selection,
            };
            let cpu = model.generate(&request).unwrap();
            let served = backend.generate(&model, &request).unwrap();
            assert_eq!(served.tokens, cpu.tokens);
            assert_eq!(served.logits_hashes, cpu.logits_hashes);
            assert_eq!(served.output_hash, cpu.output_hash);
        }
        assert!(backend.decision().uses_gpu());
        // A request beyond the context: the same refusal as the CPU, and the
        // GPU keeps serving.
        let too_long = GenerationRequest {
            prompt: &[1; 60],
            max_tokens: 10,
            eos: &[],
            selection: Selection::Argmax,
        };
        assert!(matches!(
            backend.generate(&model, &too_long),
            Err(ModernError::Domain(_))
        ));
        assert!(backend.decision().uses_gpu());
    }

    #[test]
    fn a_gpu_that_computes_a_different_digest_falls_back_to_the_cpu() {
        let Some(config) = gpu_config_or_skip() else {
            return;
        };
        // The adapter runs a model with one changed weight: its self-test
        // digest differs from the CPU golden, and the gate must refuse it.
        let mut altered = self_test_model();
        let weight = &mut altered.layers[1].w_up.q[0];
        *weight = if *weight > 0 { -127 } else { 127 };
        let gpu = gpu_self_test_of(&altered, &config).unwrap();
        assert!(gpu.kat.passed(), "{:#?}", gpu.kat.mismatches);
        assert_ne!(gpu.digest, SELF_TEST_GOLDEN);
        let decision = decide(SELF_TEST_GOLDEN, cpu_self_test_digest(), Ok(gpu));
        assert!(!decision.uses_gpu());
        assert!(
            decision.reason.contains("differs from the CPU golden"),
            "{}",
            decision.reason
        );
        // And a served model that the GPU computes differently from the CPU
        // fails its spot check: the CPU serves.
        let passed = decide(
            SELF_TEST_GOLDEN,
            cpu_self_test_digest(),
            gpu_self_test(&config),
        );
        assert!(passed.uses_gpu(), "{}", passed.reason);
        let model = self_test_model();
        let mut engine = engine_for(&altered, &config.engine_options()).unwrap();
        let error = spot_check(&model, &mut engine).unwrap_err();
        assert!(
            error.to_string().contains("differ from CPU logits"),
            "{error}"
        );
    }
}
