//! `arc-modern proof`: the ARC Proof Kit (docs/proof-kit.md).
//!
//! On a volunteer's own computer: download the pinned public SmolLM3-3B
//! weights (resumable, SHA-256 checked), convert them here with ARC's
//! deterministic converter, check the package against the pinned manifest,
//! run the five golden prompts and one challenge prompt on every CPU kernel,
//! measure speed, and print MATCH or MISMATCH against the published digest.
//!
//! Nothing leaves the computer unless `--submit` is given and the person
//! types `yes` at the terminal after seeing the exact JSON. `--dry-run`
//! prints that JSON and sends nothing.

mod host;
mod net;
mod trace;

use std::io::{BufRead, BufReader, Write};
use std::path::{Path, PathBuf};
use std::process::ExitCode;
use std::time::{Instant, SystemTime, UNIX_EPOCH};

use arc_inference::canonical_simd::{self, ProjectionCensus};
use arc_inference::modern::ModernError;
use arc_inference::modern::arith::Selection;
use arc_inference::modern::bpe::ByteLevelBpe;
use arc_inference::modern::chat::{ChatPrompt, render};
use arc_inference::modern::convert::{self, SourceFile, SourceManifest};
use arc_inference::modern::hex_lower;
use arc_inference::modern::model::{GenerationRequest, ModernModel};
use arc_inference::modern::package;
use arc_inference::modern::proof::{
    self, CaseDigest, CaseTrace, Challenge, DeviceProfile, Divergence, IslandProfile, RunSummary,
};
use serde_json::{Value, json};

use super::Args;

const PACKAGE_FILE: &str = "smollm3-3b.arcipkg";
/// Free memory the run needs (the loaded model peaks near 3.2 GB).
const MIN_FREE_RAM: u64 = 4_000_000_000;
/// Free disk for a fresh run: 6.2 GB of BF16 weights plus the 3.1 GB package.
const FRESH_DISK: u64 = 10_000_000_000;
/// Free disk when the converted package is already here.
const CACHED_DISK: u64 = 500_000_000;
/// Refresh a challenge that expires within this many seconds.
const CHALLENGE_MARGIN_SECONDS: i64 = 120;

const VALUE_OPTIONS: [&str; 5] = ["--dir", "--out", "--endpoint", "--backends", "--threads"];
const FLAG_OPTIONS: [&str; 7] = [
    "--dry-run",
    "--submit",
    "--gpu",
    "--keep-source",
    "--force",
    "--no-island",
    "--help",
];

fn invalid(message: impl Into<String>) -> ModernError {
    ModernError::Invalid(message.into())
}

fn io_error(path: &Path, error: std::io::Error) -> ModernError {
    ModernError::Io(format!("{}: {error}", path.display()))
}

fn gb(bytes: u64) -> String {
    format!("{:.1} GB", bytes as f64 / 1e9)
}

fn now_unix() -> i64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| i64::try_from(d.as_secs()).unwrap_or(i64::MAX))
        .unwrap_or(0)
}

fn show<T: std::fmt::Display>(value: Option<T>) -> String {
    value.map_or_else(|| "unknown".to_string(), |v| v.to_string())
}

fn heading(text: &str) {
    eprintln!();
    eprintln!("==> {text}");
}

// ---------------------------------------------------------------------------
// Backends

/// A way of computing the model. The CPU paths always run.
///
/// TODO(EX13, branch `gpu-portable-bitexact`): add a `GpuWgpu` variant when
/// the portable GPU engine exposes its entry point. It needs to (1) build
/// from the loaded `ModernModel` or the package path, (2) return a
/// `GenerationOutput` with exactly the semantics of `ModernModel::generate`,
/// and (3) report its adapter as `{vendor, device, backend, driver}` for
/// `runs[].adapter`. `--gpu` then appends it after the CPU backends;
/// `proof::BACKENDS` and the validator already accept `gpu-wgpu`, and the
/// layer/operator localisation in trace.rs needs a per-operator hook from it.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Backend {
    CpuScalar,
    CpuSimd,
}

impl Backend {
    fn name(self) -> &'static str {
        match self {
            Backend::CpuScalar => "cpu-scalar",
            Backend::CpuSimd => "cpu-simd",
        }
    }

    fn parse(name: &str) -> Result<Self, ModernError> {
        match name {
            "cpu-scalar" => Ok(Backend::CpuScalar),
            "cpu-simd" => Ok(Backend::CpuSimd),
            "gpu-wgpu" => Err(invalid(
                "gpu-wgpu is not in this build yet; the portable GPU engine lands separately",
            )),
            other => Err(invalid(format!(
                "unknown backend {other} (use cpu-scalar, cpu-simd)"
            ))),
        }
    }

    /// The vector instruction set `cpu-simd` uses on this computer.
    fn isa(self) -> Option<&'static str> {
        match self {
            Backend::CpuScalar => None,
            Backend::CpuSimd if cfg!(target_arch = "aarch64") => Some("neon-dotprod"),
            Backend::CpuSimd => Some("avx2"),
        }
    }

    fn describe(self) -> String {
        match self.isa() {
            Some(isa) => format!("{} ({isa})", self.name()),
            None => self.name().to_string(),
        }
    }

    /// Every CPU backend this computer can run.
    fn available() -> Vec<Backend> {
        let mut found = vec![Backend::CpuScalar];
        if canonical_simd::dotprod_available() {
            found.push(Backend::CpuSimd);
        }
        found
    }

    /// Select this backend's projection kernel for everything that follows.
    fn activate(self) -> Result<(), ModernError> {
        // Read the ARC_FAST_CANONICAL_KERNEL default first, so the explicit
        // choice below is the one that stays in force.
        let _ = canonical_simd::fast_canonical_kernel_enabled();
        let simd = self == Backend::CpuSimd;
        if simd && !canonical_simd::dotprod_available() {
            return Err(invalid(
                "cpu-simd needs NEON dotprod (arm64) or AVX2 (x86-64)",
            ));
        }
        canonical_simd::set_fast_canonical_kernel(simd);
        canonical_simd::set_projection_census_enabled(true);
        canonical_simd::reset_projection_census();
        Ok(())
    }
}

// ---------------------------------------------------------------------------
// Options

#[derive(Debug, Clone, PartialEq, Eq)]
enum Mode {
    /// Run and keep everything on this computer (the default).
    Local,
    /// Print the exact result JSON; contact nothing but Hugging Face.
    DryRun,
    /// Fetch a challenge and, after confirmation, send the result here.
    Submit(String),
}

#[derive(Debug)]
struct Options {
    dir: PathBuf,
    out: PathBuf,
    mode: Mode,
    backends: Vec<Backend>,
    keep_source: bool,
    force: bool,
    island: bool,
    gpu: bool,
}

fn default_dir() -> PathBuf {
    if let Some(dir) = std::env::var_os("ARC_PROOF_KIT_DIR") {
        return PathBuf::from(dir);
    }
    let env = |key: &str| std::env::var_os(key).map(PathBuf::from);
    let base = if cfg!(windows) {
        env("LOCALAPPDATA")
    } else if cfg!(target_os = "macos") {
        env("HOME").map(|home| home.join("Library").join("Caches"))
    } else {
        env("XDG_CACHE_HOME").or_else(|| env("HOME").map(|home| home.join(".cache")))
    };
    base.unwrap_or_else(|| PathBuf::from("."))
        .join("arc-proof-kit")
}

fn parse_options(args: &Args) -> Result<Options, ModernError> {
    let mut index = 1;
    while let Some(item) = args.items.get(index) {
        if VALUE_OPTIONS.contains(&item.as_str()) {
            if args.items.get(index + 1).is_none() {
                return Err(invalid(format!("{item} needs a value")));
            }
            index += 2;
        } else if FLAG_OPTIONS.contains(&item.as_str()) {
            index += 1;
        } else {
            return Err(invalid(format!(
                "unknown option {item}\n\n{}",
                super::USAGE
            )));
        }
    }
    let mode = match (args.flag("--dry-run"), args.flag("--submit")) {
        (true, true) => return Err(invalid("--dry-run and --submit cannot be combined")),
        (true, false) => Mode::DryRun,
        (false, true) => {
            let raw = args
                .value("--endpoint")
                .or_else(|| std::env::var("ARC_PROOF_ENDPOINT").ok())
                .ok_or_else(|| {
                    invalid(
                        "--submit needs --endpoint URL (or ARC_PROOF_ENDPOINT); \
                         docs/proof-kit.md says where the Hash Wall is",
                    )
                })?;
            Mode::Submit(net::endpoint(&raw)?)
        }
        (false, false) => Mode::Local,
    };
    let available = Backend::available();
    let backends = match args.value("--backends") {
        None => available,
        Some(list) => {
            let mut chosen = Vec::new();
            for name in list.split(',').map(str::trim).filter(|s| !s.is_empty()) {
                let backend = Backend::parse(name)?;
                if !available.contains(&backend) {
                    return Err(invalid(format!("{name} is not available on this computer")));
                }
                if !chosen.contains(&backend) {
                    chosen.push(backend);
                }
            }
            if chosen.is_empty() {
                return Err(invalid("--backends lists no backend"));
            }
            chosen
        }
    };
    let dir = args
        .value("--dir")
        .map(PathBuf::from)
        .unwrap_or_else(default_dir);
    let out = args
        .value("--out")
        .map(PathBuf::from)
        .unwrap_or_else(|| dir.join("results"));
    Ok(Options {
        dir,
        out,
        mode,
        backends,
        keep_source: args.flag("--keep-source"),
        force: args.flag("--force"),
        island: !args.flag("--no-island"),
        gpu: args.flag("--gpu"),
    })
}

// ---------------------------------------------------------------------------
// One case, one backend

#[derive(Debug, Clone)]
struct CaseRun {
    id: String,
    prompt_tokens: Vec<u32>,
    tokens: Vec<u32>,
    output_hash: String,
    logits_digest: String,
    logits_hashes: Vec<String>,
    text: String,
    prefill_seconds: f64,
    decode_seconds: f64,
    decode_forwards: usize,
}

impl CaseRun {
    fn digest(&self) -> CaseDigest {
        CaseDigest {
            id: self.id.clone(),
            tokens: self.tokens.clone(),
            output_hash: self.output_hash.clone(),
            logits_digest: self.logits_digest.clone(),
        }
    }

    fn trace(&self) -> CaseTrace {
        CaseTrace {
            id: self.id.clone(),
            prompt_tokens: self.prompt_tokens.clone(),
            tokens: self.tokens.clone(),
            logits_hashes: self.logits_hashes.clone(),
        }
    }

    fn record(&self) -> Value {
        json!({
            "id": self.id,
            "prompt_tokens": self.prompt_tokens,
            "tokens": self.tokens,
            "text": self.text,
            "output_hash": self.output_hash,
            "logits_digest": self.logits_digest,
            "logits_hashes": self.logits_hashes,
            "prefill_seconds": self.prefill_seconds,
            "decode_seconds": self.decode_seconds,
            "decode_forwards": self.decode_forwards,
        })
    }
}

/// Render, tokenise and generate one text case exactly as `arc-modern golden` does.
fn run_case(
    model: &ModernModel,
    tokenizer: &ByteLevelBpe,
    case: &Value,
) -> Result<CaseRun, ModernError> {
    let id = case
        .get("id")
        .and_then(Value::as_str)
        .ok_or_else(|| invalid("every case needs an id"))?;
    let user = case
        .get("user")
        .and_then(Value::as_str)
        .ok_or_else(|| invalid(format!("case {id} has no user text")))?;
    let text = render(&ChatPrompt {
        system: case.get("system").and_then(Value::as_str),
        user,
        thinking: case
            .get("thinking")
            .and_then(Value::as_bool)
            .unwrap_or(false),
        today: case
            .get("today")
            .and_then(Value::as_str)
            .unwrap_or(proof::CHALLENGE_TODAY),
    });
    let prompt = tokenizer.encode(&text)?;
    let max_tokens = case
        .get("max_tokens")
        .and_then(Value::as_u64)
        .and_then(|v| usize::try_from(v).ok())
        .ok_or_else(|| invalid(format!("case {id} needs max_tokens")))?;
    let eos: Vec<u32> = case
        .get("eos")
        .and_then(Value::as_array)
        .map(|list| {
            list.iter()
                .filter_map(|v| v.as_u64().and_then(|n| u32::try_from(n).ok()))
                .collect()
        })
        .unwrap_or_default();
    let selection = Selection::parse(
        case.get("selection")
            .and_then(Value::as_str)
            .unwrap_or("rp64-argmax"),
    )?;
    let out = model.generate(&GenerationRequest {
        prompt: &prompt,
        max_tokens,
        eos: &eos,
        selection,
    })?;
    Ok(CaseRun {
        id: id.to_string(),
        text: tokenizer.decode(&out.tokens, true),
        output_hash: hex_lower(&out.output_hash),
        logits_digest: hex_lower(&out.logits_digest),
        logits_hashes: out.logits_hashes.iter().map(|h| hex_lower(h)).collect(),
        prompt_tokens: prompt,
        tokens: out.tokens,
        prefill_seconds: out.prefill_seconds,
        decode_seconds: out.decode_seconds,
        decode_forwards: out.decode_forwards,
    })
}

/// Everything one backend produced.
#[derive(Debug, Clone)]
struct BackendRun {
    backend: Backend,
    golden: Vec<CaseRun>,
    golden_digest: String,
    /// Vector-kernel census over the golden prompts.
    census: ProjectionCensus,
    challenge: Option<CaseRun>,
    challenge_digest: String,
}

impl BackendRun {
    /// `arc.proof-speed.v1`: prompt tokens per prefill second and forward
    /// passes per decode second, summed over the five golden prompts.
    fn speeds(&self) -> (f64, f64) {
        let rate = |count: usize, seconds: f64| {
            if seconds > 0.0 {
                count as f64 / seconds
            } else {
                0.0
            }
        };
        let prompt: usize = self.golden.iter().map(|c| c.prompt_tokens.len()).sum();
        let prefill: f64 = self.golden.iter().map(|c| c.prefill_seconds).sum();
        let forwards: usize = self.golden.iter().map(|c| c.decode_forwards).sum();
        let decode: f64 = self.golden.iter().map(|c| c.decode_seconds).sum();
        (rate(prompt, prefill), rate(forwards, decode))
    }

    fn details(&self) -> Value {
        let (prefill, decode) = self.speeds();
        json!({
            "backend": self.backend.name(),
            "isa": self.backend.isa(),
            "golden_digest": self.golden_digest,
            "challenge_digest": self.challenge_digest,
            "prefill_tok_s": prefill,
            "decode_tok_s": decode,
            "census": self.census,
            "cases": self.golden.iter().map(CaseRun::record).collect::<Vec<_>>(),
            "challenge": self.challenge.as_ref().map(CaseRun::record),
        })
    }
}

/// Where each run first differs from its reference (`None` when it matches).
fn locate(
    model: &ModernModel,
    runs: &[BackendRun],
    reference: &[CaseTrace],
) -> Result<Vec<Option<Divergence>>, ModernError> {
    let Some(first) = runs.first() else {
        return Ok(Vec::new());
    };
    let published = |run: &BackendRun| run.golden_digest == proof::PUBLISHED_GOLDEN_DIGEST;
    let matching = runs.iter().find(|&r| published(r)).map(|r| r.backend);
    let mut found = Vec::with_capacity(runs.len());
    for run in runs {
        let golden_ok = published(run);
        if golden_ok && run.challenge_digest == first.challenge_digest {
            found.push(None);
            continue;
        }
        // Golden cases compare with the published reference; the challenge
        // (which has none) compares with the first backend's run.
        let (expected, local, good): (Vec<CaseTrace>, Vec<CaseTrace>, Option<Backend>) =
            if golden_ok {
                (
                    first.challenge.iter().map(CaseRun::trace).collect(),
                    run.challenge.iter().map(CaseRun::trace).collect(),
                    published(first).then_some(first.backend),
                )
            } else {
                (
                    reference.to_vec(),
                    run.golden.iter().map(CaseRun::trace).collect(),
                    matching,
                )
            };
        let mut divergence =
            proof::first_divergence(&expected, &local).unwrap_or_else(Divergence::unlocated);
        if divergence.op.is_none()
            && let (Some(good), Some(case_id), Some(position)) =
                (good, divergence.case.clone(), divergence.position)
            && good != run.backend
            && let Some(case) = expected.iter().find(|c| c.id == case_id)
        {
            heading(&format!(
                "Locating where {} first differs ({case_id}, position {position}): \
                 up to {} forward passes on {} and on {}",
                run.backend.name(),
                position + 1,
                good.name(),
                run.backend.name()
            ));
            if let Some(hit) = trace::locate(model, good, run.backend, case, position)? {
                divergence.position = Some(hit.position);
                divergence.layer = hit.layer;
                divergence.op = Some(hit.op);
            }
        }
        found.push(Some(divergence));
    }
    Ok(found)
}

// ---------------------------------------------------------------------------
// The kit

/// Totals of the large transfers, for the coarse network class.
#[derive(Debug, Default, Clone, Copy)]
struct DownloadStats {
    bytes: u64,
    seconds: f64,
}

impl DownloadStats {
    fn bits_per_second(self) -> Option<f64> {
        if self.bytes >= 100_000_000 && self.seconds >= 1.0 {
            Some(self.bytes as f64 * 8.0 / self.seconds)
        } else {
            None
        }
    }
}

struct Prepared {
    result: Value,
    wire: String,
    all_match: bool,
    result_path: PathBuf,
}

struct Kit<'a> {
    options: &'a Options,
    model: ModernModel,
    tokenizer: ByteLevelBpe,
    reference: Vec<CaseTrace>,
    device: DeviceProfile,
    island: Option<IslandProfile>,
    threads: usize,
    nonce: String,
    preparation: Value,
}

impl Kit<'_> {
    fn run_golden(&self, backend: Backend, cases: &[Value]) -> Result<BackendRun, ModernError> {
        backend.activate()?;
        let mut golden = Vec::with_capacity(cases.len());
        for case in cases {
            let run = run_case(&self.model, &self.tokenizer, case)?;
            eprintln!(
                "  {:<15} {:>2} tokens  output {}",
                run.id,
                run.tokens.len(),
                &run.output_hash[..16]
            );
            golden.push(run);
        }
        let census = canonical_simd::projection_census();
        let digests: Vec<CaseDigest> = golden.iter().map(CaseRun::digest).collect();
        let golden_digest = proof::matrix_digest(&digests)?;
        eprintln!("  golden digest {golden_digest}");
        Ok(BackendRun {
            backend,
            golden,
            golden_digest,
            census,
            challenge: None,
            challenge_digest: String::new(),
        })
    }

    /// Run the challenge prompt on every backend; returns the prompt text.
    fn run_challenge(
        &self,
        runs: &mut [BackendRun],
        challenge: &Challenge,
    ) -> Result<String, ModernError> {
        let prompt = challenge.prompt()?;
        let kind = if challenge.is_test() {
            "the fixed test challenge"
        } else {
            "this run's challenge"
        };
        heading(&format!(
            "Challenge prompt ({kind} {})",
            challenge.challenge_id
        ));
        eprintln!("  \"{prompt}\"");
        let case = proof::challenge_case(&prompt);
        for run in runs.iter_mut() {
            run.backend.activate()?;
            let result = run_case(&self.model, &self.tokenizer, &case)?;
            run.challenge_digest = proof::matrix_digest(std::slice::from_ref(&result.digest()))?;
            eprintln!(
                "  {:<24} challenge digest {}",
                run.backend.describe(),
                run.challenge_digest
            );
            run.challenge = Some(result);
        }
        Ok(prompt)
    }

    /// Build, self-check and save the result.
    fn prepare(
        &self,
        runs: &[BackendRun],
        challenge: &Challenge,
        prompt: &str,
    ) -> Result<Prepared, ModernError> {
        let divergences = locate(&self.model, runs, &self.reference)?;
        let mut summaries = Vec::with_capacity(runs.len());
        for (run, divergence) in runs.iter().zip(divergences) {
            let (prefill, decode) = run.speeds();
            let challenge_case = run
                .challenge
                .as_ref()
                .ok_or_else(|| invalid("the challenge prompt has not run"))?;
            summaries.push(RunSummary {
                backend: run.backend.name(),
                isa: run.backend.isa(),
                golden: run.golden.iter().map(CaseRun::digest).collect(),
                golden_digest: run.golden_digest.clone(),
                challenge: challenge_case.digest(),
                challenge_digest: run.challenge_digest.clone(),
                prefill_tok_s: prefill,
                decode_tok_s: decode,
                threads: Some(self.threads),
                vector_projections: Some((run.census.attempted, run.census.accepted)),
                adapter: None,
                divergence,
            });
        }
        let result = proof::build_result(&proof::ResultInputs {
            nonce: &self.nonce,
            challenge,
            runs: &summaries,
            device: &self.device,
            island: self.island.as_ref(),
        })?;
        let problems = proof::validate_result(&result, None);
        if !problems.is_empty() {
            return Err(invalid(format!(
                "the kit built a result that fails its own checks (please report this): {}",
                problems.join("; ")
            )));
        }
        let wire = proof::wire_json(&result);
        if wire.len() > proof::MAX_RESULT_BYTES {
            return Err(invalid(format!(
                "the result is {} bytes, over the {}-byte limit (please report this)",
                wire.len(),
                proof::MAX_RESULT_BYTES
            )));
        }
        let out = &self.options.out;
        std::fs::create_dir_all(out).map_err(|e| io_error(out, e))?;
        let result_path = out.join("proof-result.json");
        std::fs::write(&result_path, wire.as_bytes()).map_err(|e| io_error(&result_path, e))?;
        let details = json!({
            "schema": "arc.proof-run.v1",
            "note": "Local details of one Proof Kit run. Never sent anywhere by the kit.",
            "result": result,
            "challenge_prompt": prompt,
            "runs": runs.iter().map(BackendRun::details).collect::<Vec<_>>(),
            "preparation": self.preparation,
        });
        let details_path = out.join("proof-run.json");
        let text =
            serde_json::to_string_pretty(&details).map_err(|e| invalid(format!("JSON: {e}")))?;
        std::fs::write(&details_path, text + "\n").map_err(|e| io_error(&details_path, e))?;
        Ok(Prepared {
            all_match: result["verdict"] == "MATCH",
            result,
            wire,
            result_path,
        })
    }
}

fn golden_cases() -> Result<Vec<Value>, ModernError> {
    let cases: Value = serde_json::from_str(proof::GOLDEN_CASES_JSON)
        .map_err(|e| invalid(format!("golden cases: {e}")))?;
    cases
        .get("cases")
        .and_then(Value::as_array)
        .cloned()
        .ok_or_else(|| invalid("the golden cases file has no cases"))
}

fn check_prerequisites(
    options: &Options,
    facts: &host::Facts,
    cached: bool,
) -> Result<(), ModernError> {
    let mut problems = Vec::new();
    match facts.available_ram {
        Some(bytes) if bytes < MIN_FREE_RAM => problems.push(format!(
            "about {} of memory is free; the run needs {} (close other apps first)",
            gb(bytes),
            gb(MIN_FREE_RAM)
        )),
        Some(bytes) => eprintln!("  memory: {} free (needs {})", gb(bytes), gb(MIN_FREE_RAM)),
        None => eprintln!("  memory: could not read free memory; continuing"),
    }
    let need = if cached { CACHED_DISK } else { FRESH_DISK };
    match host::free_disk(&options.dir) {
        Some(bytes) if bytes < need => problems.push(format!(
            "{} of disk is free where the kit keeps its files; it needs {}",
            gb(bytes),
            gb(need)
        )),
        Some(bytes) => eprintln!("  disk: {} free (needs {})", gb(bytes), gb(need)),
        None => eprintln!("  disk: could not read free space; continuing"),
    }
    let needs_network = !cached || matches!(options.mode, Mode::Submit(_));
    if needs_network && !net::curl_available() {
        return Err(invalid(
            "curl was not found. It ships with macOS and with Windows 10 and later; \
             on Linux install it with your package manager",
        ));
    }
    if problems.is_empty() {
        return Ok(());
    }
    for problem in &problems {
        eprintln!("  problem: {problem}");
    }
    if options.force {
        eprintln!("  --force: continuing anyway (expect heavy swapping or a full disk)");
        Ok(())
    } else {
        Err(invalid(
            "this computer does not meet the requirements above (--force tries anyway)",
        ))
    }
}

/// Download one pinned source file unless a verified copy is already here.
fn fetch(
    source: &SourceManifest,
    file: &SourceFile,
    dir: &Path,
    stats: &mut DownloadStats,
) -> Result<(), ModernError> {
    let dest = dir.join(&file.name);
    if dest.is_file() {
        if convert::verify_source_file(dir, file).is_ok() {
            eprintln!("  {}: already here, SHA-256 verified", file.name);
            return Ok(());
        }
        eprintln!(
            "  {}: the copy here is incomplete or wrong; downloading it again",
            file.name
        );
        std::fs::remove_file(&dest).map_err(|e| io_error(&dest, e))?;
    }
    let url = format!(
        "https://huggingface.co/{}/resolve/{}/{}",
        source.repo, source.revision, file.name
    );
    eprintln!(
        "  {}: downloading {} from Hugging Face",
        file.name,
        gb(file.bytes)
    );
    let transfer = net::download(&url, &dest, file.bytes, &file.sha256)?;
    eprintln!("  {}: SHA-256 verified", file.name);
    if file.bytes >= 100_000_000 {
        stats.bytes += transfer.bytes;
        stats.seconds += transfer.seconds;
    }
    Ok(())
}

/// Ask at the terminal, after showing the exact bytes. No terminal, no consent.
fn confirm_on_terminal(wire: &str, endpoint: &str) -> bool {
    let (input, output) = if cfg!(windows) {
        ("CONIN$", "CONOUT$")
    } else {
        ("/dev/tty", "/dev/tty")
    };
    let Ok(mut terminal) = std::fs::OpenOptions::new().write(true).open(output) else {
        eprintln!("  no terminal is attached, so nothing can be confirmed; nothing was sent");
        return false;
    };
    let Ok(keyboard) = std::fs::File::open(input) else {
        eprintln!("  no terminal is attached, so nothing can be confirmed; nothing was sent");
        return false;
    };
    let rule = "=".repeat(72);
    let text = format!(
        "\n{rule}\nThe kit is ready to send the JSON below to:\n  {endpoint}\n\
         It holds the digests, speeds and coarse device facts shown, nothing else:\n\
         no hostname, user name, IP address field or serial number. The server sees\n\
         your IP address, as with any web request.\n{rule}\n{wire}{rule}\n\
         Send exactly this? Type yes to send: "
    );
    if terminal
        .write_all(text.as_bytes())
        .and_then(|()| terminal.flush())
        .is_err()
    {
        return false;
    }
    let mut answer = String::new();
    if BufReader::new(keyboard).read_line(&mut answer).is_err() {
        return false;
    }
    answer.trim().eq_ignore_ascii_case("yes")
}

fn expiring(challenge: &Challenge) -> bool {
    challenge
        .expires_unix()
        .is_none_or(|at| at - now_unix() < CHALLENGE_MARGIN_SECONDS)
}

fn print_summary(runs: &[BackendRun], prepared: &Prepared) {
    let result = &prepared.result;
    eprintln!();
    if prepared.all_match {
        eprintln!("RESULT: MATCH");
        eprintln!(
            "This computer produced the published answer, bit for bit, on every kernel it ran."
        );
    } else {
        eprintln!("RESULT: MISMATCH");
        eprintln!(
            "This computer's answer differs from the published one. Please keep proof-run.json:"
        );
        eprintln!("it records where the difference starts.");
    }
    eprintln!();
    let pad = " ".repeat(24);
    eprintln!(
        "  published golden digest  {}",
        proof::PUBLISHED_GOLDEN_DIGEST
    );
    let reported = result["runs"]
        .as_array()
        .map(Vec::as_slice)
        .unwrap_or_default();
    for (run, summary) in runs.iter().zip(reported) {
        let (prefill, decode) = run.speeds();
        eprintln!(
            "  {:<24} {}  {}",
            run.backend.describe(),
            run.golden_digest,
            summary["verdict"].as_str().unwrap_or("?")
        );
        eprintln!("  {pad} prefill {prefill:.2} tok/s, decode {decode:.2} tok/s");
        let divergence = &summary["divergence"];
        if !divergence.is_null() {
            eprintln!(
                "  {pad} first difference: case {}, position {}, layer {}, op {}",
                divergence["case"], divergence["position"], divergence["layer"], divergence["op"]
            );
        }
    }
    eprintln!(
        "  challenge digest         {}  ({})",
        result["challenge"]["digest"].as_str().unwrap_or("?"),
        result["challenge"]["challenge_id"].as_str().unwrap_or("?")
    );
    eprintln!();
    eprintln!(
        "Speed ({}): the five golden prompts, one request at a time on all {} threads; prefill",
        proof::SPEED_METHOD,
        result["runs"][0]["threads"]
    );
    eprintln!(
        "is prompt tokens per second of prompt processing (one token per forward pass), decode"
    );
    eprintln!("is generated tokens per second after the first. CPU only; your own measurement.");
    eprintln!();
    eprintln!("Saved {}", prepared.result_path.display());
    eprintln!("  and proof-run.json next to it (full local details; never sent).");
}

struct Outcome {
    all_match: bool,
    submit_failed: bool,
}

/// Entry point of `arc-modern proof`.
pub(super) fn run(args: &Args) -> ExitCode {
    if args.flag("--help") {
        eprintln!("{}", super::USAGE);
        eprintln!("\nSee docs/proof-kit.md for what the kit does and what it sends.");
        return ExitCode::SUCCESS;
    }
    match proof(args) {
        Ok(Outcome {
            submit_failed: true,
            ..
        }) => ExitCode::from(4),
        Ok(Outcome {
            all_match: false, ..
        }) => ExitCode::from(3),
        Ok(_) => ExitCode::SUCCESS,
        Err(error) => {
            eprintln!("\narc-modern proof: {error}");
            ExitCode::FAILURE
        }
    }
}

fn proof(args: &Args) -> Result<Outcome, ModernError> {
    let options = parse_options(args)?;
    let threads = super::configure_threads(args)?;
    eprintln!(
        "ARC Proof Kit {} (ARC {}): SmolLM3-3B, bit for bit, on this computer.",
        proof::KIT_VERSION,
        proof::ARC_VERSION
    );
    match &options.mode {
        Mode::Local => eprintln!(
            "Local run: nothing about this computer is sent anywhere (add --submit to share)."
        ),
        Mode::DryRun => {
            eprintln!("Dry run: prints the exact JSON a submission would send; sends nothing.")
        }
        Mode::Submit(endpoint) => {
            eprintln!("Submit: the result goes to {endpoint} only after you see it and type yes.")
        }
    }
    eprintln!("Files: {}", options.dir.display());

    heading("Checking this computer");
    let source_dir = options.dir.join("source");
    std::fs::create_dir_all(&source_dir).map_err(|e| io_error(&source_dir, e))?;
    let package_path = options.dir.join(PACKAGE_FILE);
    let facts = host::probe();
    check_prerequisites(&options, &facts, package_path.is_file())?;
    if options.gpu {
        eprintln!("  --gpu: the portable GPU path is not in this build yet; running the CPU paths");
    }
    let backend_names: Vec<String> = options.backends.iter().map(|b| b.describe()).collect();
    eprintln!("  kernels: {}", backend_names.join(", "));

    let mut challenge = match &options.mode {
        Mode::Submit(endpoint) => {
            heading("Fetching a challenge from the Hash Wall");
            let challenge = net::fetch_challenge(endpoint)?;
            eprintln!(
                "  challenge {} (expires {})",
                challenge.challenge_id, challenge.expires_at
            );
            challenge
        }
        Mode::Local | Mode::DryRun => Challenge::test(),
    };

    heading("Getting the model: HuggingFaceTB/SmolLM3-3B at a pinned revision");
    let source = SourceManifest::parse(proof::SOURCE_MANIFEST_JSON.as_bytes())?;
    let tokenizer_file = source
        .tokenizer
        .clone()
        .ok_or_else(|| invalid("the pinned source names no tokenizer"))?;
    let mut downloads = DownloadStats::default();
    fetch(&source, &tokenizer_file, &source_dir, &mut downloads)?;
    let pinned = proof::PINNED_MANIFEST_JSON.as_bytes();
    let mut verified = None;
    if package_path.is_file() {
        eprintln!("  found a converted package from an earlier run; checking it");
        match package::verify_package(&package_path, pinned) {
            Ok(digest) => verified = Some(digest),
            Err(error) => {
                eprintln!("  it does not match the pinned manifest ({error}); converting again");
                std::fs::remove_file(&package_path).map_err(|e| io_error(&package_path, e))?;
            }
        }
    }
    let mut convert_seconds = None;
    let digest = match verified {
        Some(digest) => digest,
        None => {
            for file in &source.files {
                fetch(&source, file, &source_dir, &mut downloads)?;
            }
            heading("Converting the BF16 weights on this computer (integer-only, deterministic)");
            let partial = options.dir.join(format!("{PACKAGE_FILE}.partial"));
            let report = convert::convert(&source_dir, &source, &partial)?;
            convert_seconds = Some(report.seconds);
            eprintln!("  converted in {:.1} s", report.seconds);
            heading("Checking the package against the pinned manifest");
            let digest = match package::verify_package(&partial, pinned) {
                Ok(digest) => digest,
                Err(error) => {
                    let kept = options.dir.join(format!("{PACKAGE_FILE}.mismatch"));
                    let _ = std::fs::rename(&partial, &kept);
                    eprintln!("\nRESULT: MISMATCH (conversion)");
                    eprintln!(
                        "The package converted on this computer differs from the pinned one:"
                    );
                    eprintln!("  {error}");
                    eprintln!(
                        "It is kept at {} for inspection. Please report it.",
                        kept.display()
                    );
                    return Ok(Outcome {
                        all_match: false,
                        submit_failed: false,
                    });
                }
            };
            std::fs::rename(&partial, &package_path).map_err(|e| io_error(&package_path, e))?;
            if !options.keep_source {
                for file in source
                    .files
                    .iter()
                    .filter(|f| f.name.ends_with(".safetensors"))
                {
                    let _ = std::fs::remove_file(source_dir.join(&file.name));
                }
                eprintln!("  removed the BF16 source files (--keep-source keeps them)");
            }
            digest
        }
    };
    eprintln!(
        "  package sha256 {} ({} bytes): matches the pinned manifest",
        digest.sha256, digest.bytes
    );

    heading("Loading the package (about 3 GB of memory)");
    let started = Instant::now();
    let model = package::load_package(&package_path)?;
    let tokenizer_path = source_dir.join(&tokenizer_file.name);
    let tokenizer_bytes =
        std::fs::read(&tokenizer_path).map_err(|e| io_error(&tokenizer_path, e))?;
    let tokenizer = ByteLevelBpe::from_json(&tokenizer_bytes)?;
    eprintln!("  loaded in {:.1} s", started.elapsed().as_secs_f64());

    let device = host::device_profile(&facts);
    let island = options
        .island
        .then(|| host::island_profile(&facts, downloads.bits_per_second()));
    let kit = Kit {
        options: &options,
        model,
        tokenizer,
        reference: proof::golden_reference()?,
        device,
        island,
        threads,
        nonce: proof::new_nonce()?,
        preparation: json!({
            "package_sha256": digest.sha256,
            "convert_seconds": convert_seconds,
            "downloaded_bytes": downloads.bytes,
            "download_seconds": downloads.seconds,
        }),
    };

    let cases = golden_cases()?;
    let mut runs = Vec::with_capacity(options.backends.len());
    for &backend in &options.backends {
        heading(&format!("Golden prompts on {}", backend.describe()));
        runs.push(kit.run_golden(backend, &cases)?);
    }
    let mut prompt = kit.run_challenge(&mut runs, &challenge)?;
    let mut prepared = kit.prepare(&runs, &challenge, &prompt)?;
    print_summary(&runs, &prepared);
    let island_text = match &kit.island {
        Some(island) => format!(
            "memory class {} GB, GPU memory class {} GB, Thunderbolt 5 {}, download class {} Mb/s",
            show(island.memory_class_gb),
            show(island.gpu_vram_class_gb),
            show(island.thunderbolt5),
            show(island.download_mbps_class)
        ),
        None => "not included (--no-island)".to_string(),
    };
    eprintln!(
        "Device as reported: {} {}, {}, {}, {} logical CPUs; island facts: {island_text}",
        kit.device.os,
        kit.device.os_version.as_deref().unwrap_or("?"),
        kit.device.arch,
        kit.device.cpu_model.as_deref().unwrap_or("unknown CPU"),
        kit.device.logical_cpus
    );

    let endpoint = match &options.mode {
        Mode::Local => {
            eprintln!("\nNothing was sent. To add this result to the Hash Wall, run again with");
            eprintln!(
                "--submit --endpoint URL; you will see the exact JSON before anything is sent."
            );
            return Ok(Outcome {
                all_match: prepared.all_match,
                submit_failed: false,
            });
        }
        Mode::DryRun => {
            eprintln!(
                "\nDRY RUN: this is the exact JSON a submission would send. Nothing was sent.\n"
            );
            print!("{}", prepared.wire);
            std::io::stdout()
                .flush()
                .map_err(|e| ModernError::Io(format!("stdout: {e}")))?;
            return Ok(Outcome {
                all_match: prepared.all_match,
                submit_failed: false,
            });
        }
        Mode::Submit(endpoint) => endpoint.clone(),
    };

    for _ in 0..3 {
        if expiring(&challenge) {
            heading("The challenge is about to expire: fetching a new one");
            challenge = net::fetch_challenge(&endpoint)?;
            prompt = kit.run_challenge(&mut runs, &challenge)?;
            prepared = kit.prepare(&runs, &challenge, &prompt)?;
        }
        if !confirm_on_terminal(&prepared.wire, &endpoint) {
            eprintln!("\nNot sent. Nothing left this computer. The result is saved locally.");
            return Ok(Outcome {
                all_match: prepared.all_match,
                submit_failed: false,
            });
        }
        if expiring(&challenge) {
            eprintln!("  the challenge expired while waiting; refreshing it and asking again");
            continue;
        }
        heading(&format!("Sending to {endpoint}"));
        let response = net::submit(&endpoint, &prepared.result_path)?;
        let accepted = (200..300).contains(&response.status);
        eprintln!("  HTTP {}: {}", response.status, response.body);
        if accepted {
            eprintln!("  Sent. Thank you for adding this computer to the Hash Wall.");
        } else {
            eprintln!("  The Hash Wall did not accept the result. It is saved locally.");
        }
        return Ok(Outcome {
            all_match: prepared.all_match,
            submit_failed: !accepted,
        });
    }
    Err(invalid(
        "the challenge kept expiring before the result was confirmed; nothing was sent",
    ))
}

/// `arc-modern challenge-prompt`: the prompt a seed derives (CI cross-check).
pub(super) fn cmd_challenge_prompt(args: &Args) -> Result<(), ModernError> {
    if let Some(path) = args.value("--seeds") {
        let text =
            std::fs::read_to_string(&path).map_err(|e| ModernError::Io(format!("{path}: {e}")))?;
        for seed in text.lines().map(str::trim).filter(|line| !line.is_empty()) {
            let prompt = proof::challenge_prompt(seed)?;
            let line = json!({
                "seed": seed,
                "prompt": prompt,
                "prompt_sha256": proof::sha256_hex(&prompt),
            });
            println!("{line}");
        }
        return Ok(());
    }
    let seed = args.required("--seed")?;
    println!("{}", proof::challenge_prompt(&seed)?);
    Ok(())
}
