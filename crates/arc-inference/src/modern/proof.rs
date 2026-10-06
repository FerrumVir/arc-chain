//! ARC Proof Kit: the published golden digest, the anti-faking challenge
//! (`arc.proof-challenge.v1`) and the volunteer result (`arc.proof-result.v1`).
//!
//! The rules are written in `docs/proof-kit.md`. The independent Python
//! reference is `scripts/arc_conformance/proof_kit_reference.py`; CI checks
//! that both derive the same challenge prompts and accept the same results.
//! Everything here is pure. The `arc-modern proof` command does the
//! downloads, the runs and (only with consent) the submission.

use serde_json::{Map, Value, json};
use sha2::{Digest, Sha256};

use super::{ModernError, arith, hex_lower};
use crate::model_package::canonical_json;

/// Schema of one volunteer result.
pub const RESULT_SCHEMA: &str = "arc.proof-result.v1";
/// Challenge derivation scheme; also the hash domain (followed by one zero byte).
pub const CHALLENGE_SCHEME: &str = "arc.proof-challenge.v1";
/// How prefill and decode speeds are measured (docs/proof-kit.md).
pub const SPEED_METHOD: &str = "arc.proof-speed.v1";
/// Version of the Proof Kit itself.
pub const KIT_VERSION: &str = "0.1.0";
/// ARC version this binary was built from.
pub const ARC_VERSION: &str = env!("CARGO_PKG_VERSION");
/// Largest result body the Hash Wall accepts.
pub const MAX_RESULT_BYTES: usize = 8192;
/// Golden digest every matching computer reproduces (PR #137, 28 of 28 CI runs).
pub const PUBLISHED_GOLDEN_DIGEST: &str =
    "3e43f342c00cf3e3be3072e654e9d1c43f547a73b8a4fc6e906b7119e5cb49f2";

/// The pinned model, as recorded in the result.
pub const MODEL_REPO: &str = "HuggingFaceTB/SmolLM3-3B";
pub const MODEL_REVISION: &str = "a07cc9a04f16550a088caea529712d1d335b0ac1";
pub const PACKAGE_SHA256: &str = "19c67496ee23fe5da0e12eb1f22cb17f6386c560071587b5b8dfa68731c0aa91";
pub const MANIFEST_BLAKE3: &str =
    "af388d01c3578c5f97238fd74aaa3d0d8194d29fc6fbd99f3aae226064659fa2";

/// Pinned inputs, embedded so that a release binary needs no other file.
pub const SOURCE_MANIFEST_JSON: &str =
    include_str!("../../../../docs/protocol/packages/smollm3-3b.source.json");
pub const PINNED_MANIFEST_JSON: &str =
    include_str!("../../../../docs/protocol/packages/smollm3-3b.integer-package.json");
pub const GOLDEN_CASES_JSON: &str =
    include_str!("../../../../scripts/arc_modern/smollm3_cases.json");
pub const GOLDEN_REFERENCE_JSON: &str =
    include_str!("../../../../docs/protocol/proof-kit/smollm3-golden-reference.json");
pub const CHALLENGE_WORDS_TEXT: &str =
    include_str!("../../../../docs/protocol/proof-kit/challenge-words-v1.txt");
/// SHA-256 of the 256 words, each followed by one LF.
pub const CHALLENGE_WORDS_SHA256: &str =
    "4eda9812f9ae77ed55b5e73f10506e8f3cf3f294dc1f086c77884be95917ad60";

/// Golden case ids in order, with each case's token budget.
pub const GOLDEN_CASES: [(&str, usize); 5] = [
    ("capital", 24),
    ("haiku", 24),
    ("integers", 24),
    ("primes", 24),
    ("product-greedy", 16),
];
/// SmolLM3-3B vocabulary size.
pub const VOCAB_SIZE: u64 = 128_256;

/// The challenge prompt is rendered like the golden prompts.
pub const CHALLENGE_TODAY: &str = "06 October 2026";
pub const CHALLENGE_MAX_TOKENS: usize = 24;
pub const CHALLENGE_EOS: u32 = 128_012;

/// The fixed test challenge (`--dry-run` and local-only runs). A Hash Wall
/// never accepts it: its signature is not one the server made.
pub const TEST_CHALLENGE_ID: &str = "test-challenge-v1";
pub const TEST_CHALLENGE_SEED: &str =
    "000102030405060708090a0b0c0d0e0f101112131415161718191a1b1c1d1e1f";
pub const TEST_CHALLENGE_EXPIRES_AT: &str = "2099-12-31T23:59:59Z";
pub const TEST_CHALLENGE_SIGNATURE: &str = "unsigned-test-challenge";
/// The test challenge's digest, as the kit's own CI dry run produced it on
/// both CPU kernels (Proof Kit CI run 37507415675, ubuntu-latest). Every
/// computer should reproduce it; local and dry runs print the comparison.
pub const TEST_CHALLENGE_DIGEST: &str =
    "fad7f4483e2092bd21f669fad7bc70e6f203792a81ddce440026917ffaba4f6c";

/// Run backends. GPU runs (`gpu-wgpu`) join when the portable GPU engine lands.
pub const BACKENDS: [&str; 3] = ["cpu-scalar", "cpu-simd", "gpu-wgpu"];
/// Vector instruction sets of the `cpu-simd` backend.
pub const ISAS: [&str; 2] = ["avx2", "neon-dotprod"];
/// Graphics APIs a GPU adapter can report.
pub const GPU_APIS: [&str; 4] = ["vulkan", "metal", "dx12", "gl"];
/// Where a mismatch can start: the tokenizer, one operator of a layer, the
/// final norm and LM head, or token selection.
pub const OPS: [&str; 20] = [
    "tokenizer",
    "embed",
    "attn_norm",
    "wq",
    "wk",
    "wv",
    "rope_q",
    "rope_k",
    "attention",
    "wo",
    "attn_residual",
    "ffn_norm",
    "w_gate",
    "w_up",
    "silu",
    "w_down",
    "ffn_residual",
    "final_norm",
    "lm_head",
    "select",
];
/// CPU features a result may report.
pub const CPU_FEATURES: [&str; 8] = [
    "avx2", "fma", "avx512f", "avx512bw", "dotprod", "i8mm", "sve", "sve2",
];
/// Coarse classes (largest class not above the measured value).
pub const MEMORY_CLASSES_GB: [u32; 17] = [
    1, 2, 4, 8, 16, 24, 32, 48, 64, 96, 128, 192, 256, 384, 512, 768, 1024,
];
pub const VRAM_CLASSES_GB: [u32; 18] = [
    1, 2, 4, 6, 8, 10, 12, 16, 20, 24, 32, 40, 48, 64, 80, 96, 128, 192,
];
pub const NETWORK_CLASSES_MBPS: [u32; 11] = [1, 10, 25, 50, 100, 250, 500, 1000, 2500, 5000, 10000];

const LABEL_MAX: usize = 64;
const LABEL_PUNCTUATION: &str = " ()@.,+/_-";
const GIB: f64 = 1_073_741_824.0;

fn invalid(message: impl Into<String>) -> ModernError {
    ModernError::Invalid(message.into())
}

// ---------------------------------------------------------------------------
// The challenge prompt

/// The 256 challenge words, checked against [`CHALLENGE_WORDS_SHA256`].
pub fn challenge_words() -> Result<Vec<&'static str>, ModernError> {
    let words: Vec<&'static str> = CHALLENGE_WORDS_TEXT.lines().collect();
    let mut sorted = words.clone();
    sorted.sort_unstable();
    sorted.dedup();
    if words.len() != 256 || sorted.len() != 256 {
        return Err(invalid(
            "the challenge word list must hold 256 distinct words",
        ));
    }
    if words
        .iter()
        .any(|w| w.is_empty() || !w.bytes().all(|b| b.is_ascii_lowercase()))
    {
        return Err(invalid("challenge words must be lowercase ASCII letters"));
    }
    let mut hasher = Sha256::new();
    for word in &words {
        hasher.update(word.as_bytes());
        hasher.update(b"\n");
    }
    let digest = hex_lower(&hasher.finalize());
    if digest != CHALLENGE_WORDS_SHA256 {
        return Err(invalid(format!(
            "challenge word list SHA-256 is {digest}, expected {CHALLENGE_WORDS_SHA256}"
        )));
    }
    Ok(words)
}

/// A seed is exactly 64 lowercase hex characters (32 bytes).
pub fn parse_seed(seed: &str) -> Result<[u8; 32], ModernError> {
    if !is_hex(seed, 64) {
        return Err(invalid("a challenge seed is 64 lowercase hex characters"));
    }
    let bytes = hex::decode(seed).map_err(|e| invalid(format!("challenge seed: {e}")))?;
    let mut out = [0u8; 32];
    out.copy_from_slice(&bytes);
    Ok(out)
}

/// Indices of the four words a seed selects: the first four bytes of
/// `SHA-256("arc.proof-challenge.v1" || 0x00 || seed)`.
pub fn challenge_word_indices(seed: &str) -> Result<[usize; 4], ModernError> {
    let seed = parse_seed(seed)?;
    let mut hasher = Sha256::new();
    hasher.update(CHALLENGE_SCHEME.as_bytes());
    hasher.update([0u8]);
    hasher.update(seed);
    let digest = hasher.finalize();
    Ok([
        usize::from(digest[0]),
        usize::from(digest[1]),
        usize::from(digest[2]),
        usize::from(digest[3]),
    ])
}

/// The challenge prompt's user message (docs/proof-kit.md).
pub fn challenge_prompt(seed: &str) -> Result<String, ModernError> {
    let words = challenge_words()?;
    let [a, b, c, d] = challenge_word_indices(seed)?;
    Ok(format!(
        "Write one short sentence that mentions {}, {}, {} and {}.",
        words[a], words[b], words[c], words[d]
    ))
}

/// The challenge as an `arc.modern-cases.v1` case, rendered like the golden prompts.
pub fn challenge_case(prompt: &str) -> Value {
    json!({
        "id": "challenge",
        "user": prompt,
        "today": CHALLENGE_TODAY,
        "thinking": false,
        "max_tokens": CHALLENGE_MAX_TOKENS,
        "eos": [CHALLENGE_EOS],
        "selection": "rp64-argmax",
    })
}

/// Lowercase hex SHA-256 of a text.
pub fn sha256_hex(text: &str) -> String {
    hex_lower(&Sha256::digest(text.as_bytes()))
}

/// A challenge handed out by a Hash Wall (`GET <endpoint>/challenge`).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Challenge {
    pub challenge_id: String,
    pub seed: String,
    pub expires_at: String,
    pub signature: String,
}

impl Challenge {
    /// The fixed test challenge used by `--dry-run` and local-only runs.
    pub fn test() -> Self {
        Self {
            challenge_id: TEST_CHALLENGE_ID.into(),
            seed: TEST_CHALLENGE_SEED.into(),
            expires_at: TEST_CHALLENGE_EXPIRES_AT.into(),
            signature: TEST_CHALLENGE_SIGNATURE.into(),
        }
    }

    pub fn is_test(&self) -> bool {
        self.challenge_id == TEST_CHALLENGE_ID
    }

    /// Read a challenge response. Unknown fields are ignored (and never echoed).
    pub fn from_json(value: &Value) -> Result<Self, ModernError> {
        let field = |key: &str, ok: fn(&str) -> bool| -> Result<String, ModernError> {
            value
                .get(key)
                .and_then(Value::as_str)
                .filter(|text| ok(text))
                .map(str::to_string)
                .ok_or_else(|| invalid(format!("the challenge's {key} is missing or malformed")))
        };
        let challenge = Self {
            challenge_id: field("challenge_id", is_challenge_id)?,
            seed: field("seed", is_hex64)?,
            expires_at: field("expires_at", is_timestamp)?,
            signature: field("signature", is_signature)?,
        };
        if challenge.is_test() {
            return Err(invalid(
                "the server returned the reserved test challenge id",
            ));
        }
        Ok(challenge)
    }

    /// `expires_at` as Unix seconds.
    pub fn expires_unix(&self) -> Option<i64> {
        unix_seconds(&self.expires_at)
    }

    pub fn prompt(&self) -> Result<String, ModernError> {
        challenge_prompt(&self.seed)
    }
}

/// Unix seconds of a `YYYY-MM-DDTHH:MM:SSZ` timestamp (years 2000-9999).
pub fn unix_seconds(timestamp: &str) -> Option<i64> {
    if !is_timestamp(timestamp) {
        return None;
    }
    let number = |range: std::ops::Range<usize>| timestamp[range].parse::<i64>().ok();
    let (year, month, day) = (number(0..4)?, number(5..7)?, number(8..10)?);
    let (hour, minute, second) = (number(11..13)?, number(14..16)?, number(17..19)?);
    let days_in_month = match month {
        1 | 3 | 5 | 7 | 8 | 10 | 12 => 31,
        4 | 6 | 9 | 11 => 30,
        2 if year % 4 == 0 && (year % 100 != 0 || year % 400 == 0) => 29,
        2 => 28,
        _ => return None,
    };
    if !(2000..=9999).contains(&year)
        || !(1..=days_in_month).contains(&day)
        || hour > 23
        || minute > 59
        || second > 59
    {
        return None;
    }
    // Days from 1970-01-01 (proleptic Gregorian, March-based years).
    let y = if month <= 2 { year - 1 } else { year };
    let era = y / 400;
    let year_of_era = y - era * 400;
    let day_of_year = (153 * ((month + 9) % 12) + 2) / 5 + day - 1;
    let day_of_era = year_of_era * 365 + year_of_era / 4 - year_of_era / 100 + day_of_year;
    let days = era * 146_097 + day_of_era - 719_468;
    Some(days * 86_400 + hour * 3_600 + minute * 60 + second)
}

// ---------------------------------------------------------------------------
// Digests

/// One case's digest entry: the generated tokens and the hashes over them.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CaseDigest {
    pub id: String,
    pub tokens: Vec<u32>,
    pub output_hash: String,
    pub logits_digest: String,
}

impl CaseDigest {
    /// The entry `arc-modern golden` hashes (and the result carries).
    pub fn entry(&self) -> Value {
        json!({
            "id": self.id,
            "tokens": self.tokens,
            "output_hash": self.output_hash,
            "logits_digest": self.logits_digest,
        })
    }
}

/// BLAKE3 (hex) of the canonical JSON list of case entries: the golden digest
/// for the five golden cases, the challenge digest for the challenge case.
pub fn matrix_digest(cases: &[CaseDigest]) -> Result<String, ModernError> {
    let entries: Vec<Value> = cases.iter().map(CaseDigest::entry).collect();
    let text = canonical_json(&Value::Array(entries))
        .map_err(|e| invalid(format!("canonical JSON: {e}")))?;
    Ok(blake3::hash(text.as_bytes()).to_hex().to_string())
}

/// One case's full trace: prompt ids, generated ids and every logits hash.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CaseTrace {
    pub id: String,
    pub prompt_tokens: Vec<u32>,
    pub tokens: Vec<u32>,
    pub logits_hashes: Vec<String>,
}

impl CaseTrace {
    /// Token fed to the forward pass at `position` (teacher forcing).
    pub fn input_token(&self, position: usize) -> Option<u32> {
        let prompt = self.prompt_tokens.len();
        if position < prompt {
            self.prompt_tokens.get(position).copied()
        } else {
            self.tokens.get(position - prompt).copied()
        }
    }
}

fn u32_list(value: Option<&Value>, what: &str) -> Result<Vec<u32>, ModernError> {
    value
        .and_then(Value::as_array)
        .ok_or_else(|| invalid(format!("{what} is not a list")))?
        .iter()
        .map(|v| {
            v.as_u64()
                .and_then(|n| u32::try_from(n).ok())
                .ok_or_else(|| invalid(format!("{what} holds a value that is not a u32")))
        })
        .collect()
}

/// The published per-position reference of the golden prompts.
pub fn golden_reference() -> Result<Vec<CaseTrace>, ModernError> {
    let value: Value = serde_json::from_str(GOLDEN_REFERENCE_JSON)
        .map_err(|e| invalid(format!("golden reference JSON: {e}")))?;
    let cases = value
        .get("cases")
        .and_then(Value::as_array)
        .ok_or_else(|| invalid("golden reference has no cases"))?;
    cases
        .iter()
        .map(|case| -> Result<CaseTrace, ModernError> {
            let id = case
                .get("id")
                .and_then(Value::as_str)
                .ok_or_else(|| invalid("golden reference case without an id"))?;
            let hashes = case
                .get("logits_hashes")
                .and_then(Value::as_array)
                .ok_or_else(|| invalid("golden reference case without logits hashes"))?
                .iter()
                .map(|h| {
                    h.as_str()
                        .filter(|text| is_hex64(text))
                        .map(str::to_string)
                        .ok_or_else(|| invalid("golden reference hash is malformed"))
                })
                .collect::<Result<Vec<String>, ModernError>>()?;
            Ok(CaseTrace {
                id: id.to_string(),
                prompt_tokens: u32_list(case.get("prompt_tokens"), "prompt_tokens")?,
                tokens: u32_list(case.get("tokens"), "tokens")?,
                logits_hashes: hashes,
            })
        })
        .collect()
}

/// Where a run first differs from its reference.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Divergence {
    /// Golden case id, or `challenge`; `None` when unknown.
    pub case: Option<String>,
    /// Forward position (index into the case's logits hashes).
    pub position: Option<usize>,
    /// Layer of the first differing operator, when a trace located it.
    pub layer: Option<usize>,
    /// One of [`OPS`], when known.
    pub op: Option<&'static str>,
}

impl Divergence {
    /// A mismatch nothing could locate.
    pub fn unlocated() -> Self {
        Self {
            case: None,
            position: None,
            layer: None,
            op: None,
        }
    }

    pub fn to_json(&self) -> Value {
        json!({
            "case": self.case,
            "position": self.position,
            "layer": self.layer,
            "op": self.op,
        })
    }
}

fn first_difference<T: PartialEq>(a: &[T], b: &[T]) -> Option<usize> {
    if a == b {
        return None;
    }
    Some(
        a.iter()
            .zip(b)
            .position(|(x, y)| x != y)
            .unwrap_or(a.len().min(b.len())),
    )
}

/// The first place `local` differs from `reference`, case by case: the
/// prompt ids (tokenizer), then the logits hashes, then token selection.
pub fn first_divergence(reference: &[CaseTrace], local: &[CaseTrace]) -> Option<Divergence> {
    for (r, l) in reference.iter().zip(local) {
        let case = Some(r.id.clone());
        if let Some(index) = first_difference(&r.prompt_tokens, &l.prompt_tokens) {
            return Some(Divergence {
                case,
                position: Some(index),
                layer: None,
                op: Some("tokenizer"),
            });
        }
        if let Some(position) = first_difference(&r.logits_hashes, &l.logits_hashes) {
            return Some(Divergence {
                case,
                position: Some(position),
                layer: None,
                op: None,
            });
        }
        if let Some(index) = first_difference(&r.tokens, &l.tokens) {
            // Token i is selected from the logits at position P - 1 + i.
            return Some(Divergence {
                case,
                position: Some((r.prompt_tokens.len() + index).saturating_sub(1)),
                layer: None,
                op: Some("select"),
            });
        }
    }
    (reference.len() != local.len()).then(Divergence::unlocated)
}

// ---------------------------------------------------------------------------
// Building a result

/// One backend's run, as the result reports it.
#[derive(Debug, Clone)]
pub struct RunSummary {
    /// One of [`BACKENDS`].
    pub backend: &'static str,
    /// One of [`ISAS`] for `cpu-simd`, otherwise `None`.
    pub isa: Option<&'static str>,
    pub golden: Vec<CaseDigest>,
    pub golden_digest: String,
    pub challenge: CaseDigest,
    pub challenge_digest: String,
    pub prefill_tok_s: f64,
    pub decode_tok_s: f64,
    pub threads: Option<usize>,
    /// `(attempted, accepted)` vector-kernel projections.
    pub vector_projections: Option<(u64, u64)>,
    /// GPU adapter `{vendor, device, backend, driver}`; `None` on CPU.
    pub adapter: Option<Value>,
    /// Where the run first differs (computed by the caller on a mismatch).
    pub divergence: Option<Divergence>,
}

/// The coarse device profile (no hostname, user name, address or serial).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DeviceProfile {
    pub os: String,
    pub os_version: Option<String>,
    pub arch: String,
    pub cpu_model: Option<String>,
    pub logical_cpus: usize,
    pub cpu_features: Vec<&'static str>,
    pub gpu_model: Option<String>,
}

/// Coarse hardware facts for planning model islands.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct IslandProfile {
    pub memory_class_gb: Option<u32>,
    pub unified_memory: bool,
    pub gpu_vram_class_gb: Option<u32>,
    pub thunderbolt5: Option<bool>,
    pub download_mbps_class: Option<u32>,
}

/// Everything a result is made of.
#[derive(Debug, Clone)]
pub struct ResultInputs<'a> {
    pub nonce: &'a str,
    pub challenge: &'a Challenge,
    pub runs: &'a [RunSummary],
    pub device: &'a DeviceProfile,
    pub island: Option<&'a IslandProfile>,
}

/// A run matches when its golden digest is the published one and its
/// challenge digest equals the first run's.
pub fn run_matches(run: &RunSummary, first: &RunSummary) -> bool {
    run.golden_digest == PUBLISHED_GOLDEN_DIGEST && run.challenge_digest == first.challenge_digest
}

fn speed(value: f64) -> f64 {
    if value.is_finite() {
        ((value * 1000.0).round() / 1000.0).clamp(0.0, 100_000.0)
    } else {
        0.0
    }
}

/// A new submission nonce: 16 random bytes from the operating system, as hex.
pub fn new_nonce() -> Result<String, ModernError> {
    use rand::RngCore;
    let mut bytes = [0u8; 16];
    rand::rngs::OsRng
        .try_fill_bytes(&mut bytes)
        .map_err(|e| ModernError::Io(format!("operating-system randomness: {e}")))?;
    Ok(hex_lower(&bytes))
}

/// Assemble an `arc.proof-result.v1` result. The golden cases and the
/// challenge case come from the first run.
pub fn build_result(inputs: &ResultInputs<'_>) -> Result<Value, ModernError> {
    let first = inputs
        .runs
        .first()
        .ok_or_else(|| invalid("a result needs at least one run"))?;
    let prompt_text = inputs.challenge.prompt()?;
    let mut all_match = true;
    let runs: Vec<Value> = inputs
        .runs
        .iter()
        .map(|run| {
            let matches = run_matches(run, first);
            all_match &= matches;
            let divergence = if matches {
                None
            } else {
                Some(run.divergence.clone().unwrap_or_else(Divergence::unlocated))
            };
            json!({
                "backend": run.backend,
                "isa": run.isa,
                "verdict": if matches { "MATCH" } else { "MISMATCH" },
                "golden_digest": run.golden_digest,
                "challenge_digest": run.challenge_digest,
                "prefill_tok_s": speed(run.prefill_tok_s),
                "decode_tok_s": speed(run.decode_tok_s),
                "threads": run.threads,
                "vector_projections": run
                    .vector_projections
                    .map(|(attempted, accepted)| json!({"attempted": attempted, "accepted": accepted})),
                "adapter": run.adapter,
                "divergence": divergence.map(|d| d.to_json()),
            })
        })
        .collect();
    let golden: Vec<Value> = first.golden.iter().map(CaseDigest::entry).collect();
    let device = inputs.device;
    let island = inputs.island.map(|i| {
        json!({
            "memory_class_gb": i.memory_class_gb,
            "unified_memory": i.unified_memory,
            "gpu_vram_class_gb": i.gpu_vram_class_gb,
            "thunderbolt5": i.thunderbolt5,
            "download_mbps_class": i.download_mbps_class,
        })
    });
    Ok(json!({
        "schema": RESULT_SCHEMA,
        "kit_version": KIT_VERSION,
        "arc_version": ARC_VERSION,
        "nonce": inputs.nonce,
        "model": {
            "repo": MODEL_REPO,
            "revision": MODEL_REVISION,
            "profile": super::PROFILE,
            "package_sha256": PACKAGE_SHA256,
            "manifest_blake3": MANIFEST_BLAKE3,
        },
        "verdict": if all_match { "MATCH" } else { "MISMATCH" },
        "golden": {
            "published_digest": PUBLISHED_GOLDEN_DIGEST,
            "digest": first.golden_digest,
            "cases": golden,
        },
        "challenge": {
            "challenge_id": inputs.challenge.challenge_id,
            "seed": inputs.challenge.seed,
            "expires_at": inputs.challenge.expires_at,
            "signature": inputs.challenge.signature,
            "prompt_sha256": sha256_hex(&prompt_text),
            "digest": first.challenge_digest,
            "case": first.challenge.entry(),
        },
        "speed_method": SPEED_METHOD,
        "runs": runs,
        "device": {
            "os": device.os,
            "os_version": device.os_version,
            "arch": device.arch,
            "cpu_model": device.cpu_model,
            "logical_cpus": device.logical_cpus,
            "cpu_features": device.cpu_features,
            "gpu_model": device.gpu_model,
        },
        "island": island,
    }))
}

/// The exact bytes the kit shows and sends: two-space indentation, one field
/// per line, arrays of plain values on one line, and a final newline.
pub fn wire_json(value: &Value) -> String {
    let mut out = String::new();
    write_wire(value, 0, &mut out);
    out.push('\n');
    out
}

fn push_indent(out: &mut String, level: usize) {
    out.push_str(&"  ".repeat(level));
}

fn write_wire(value: &Value, level: usize, out: &mut String) {
    match value {
        Value::Object(map) if !map.is_empty() => {
            out.push_str("{\n");
            for (index, (key, item)) in map.iter().enumerate() {
                push_indent(out, level + 1);
                out.push_str(&Value::String(key.clone()).to_string());
                out.push_str(": ");
                write_wire(item, level + 1, out);
                if index + 1 < map.len() {
                    out.push(',');
                }
                out.push('\n');
            }
            push_indent(out, level);
            out.push('}');
        }
        Value::Array(items) if items.iter().any(|v| v.is_object() || v.is_array()) => {
            out.push_str("[\n");
            for (index, item) in items.iter().enumerate() {
                push_indent(out, level + 1);
                write_wire(item, level + 1, out);
                if index + 1 < items.len() {
                    out.push(',');
                }
                out.push('\n');
            }
            push_indent(out, level);
            out.push(']');
        }
        Value::Array(items) => {
            let parts: Vec<String> = items.iter().map(Value::to_string).collect();
            out.push('[');
            out.push_str(&parts.join(", "));
            out.push(']');
        }
        other => out.push_str(&other.to_string()),
    }
}

// ---------------------------------------------------------------------------
// Coarse classes and labels

fn floor_class(classes: &[u32], value: f64) -> Option<u32> {
    classes
        .iter()
        .rev()
        .copied()
        .find(|&c| f64::from(c) <= value)
}

/// Memory class in GB. Allows 15% for memory the firmware or an integrated
/// GPU reserves, so a 16 GB machine reporting 14 GiB is still class 16.
pub fn memory_class_gb(total_bytes: u64) -> Option<u32> {
    floor_class(&MEMORY_CLASSES_GB, total_bytes as f64 / GIB * 1.15)
}

/// GPU memory class in GB (5% allowance).
pub fn vram_class_gb(bytes: u64) -> Option<u32> {
    floor_class(&VRAM_CLASSES_GB, bytes as f64 / GIB * 1.05)
}

/// Download-throughput class in Mb/s.
pub fn network_class_mbps(bits_per_second: f64) -> Option<u32> {
    if bits_per_second.is_finite() {
        floor_class(&NETWORK_CLASSES_MBPS, bits_per_second / 1e6)
    } else {
        None
    }
}

/// A hardware name reduced to the label alphabet: letters, digits, spaces and
/// `()@.,+/_-`; anything else becomes a space; at most 64 characters.
pub fn label(raw: &str) -> Option<String> {
    let mapped: String = raw
        .chars()
        .map(|c| {
            if c.is_ascii_alphanumeric() || LABEL_PUNCTUATION.contains(c) {
                c
            } else {
                ' '
            }
        })
        .collect();
    let joined = mapped.split_whitespace().collect::<Vec<_>>().join(" ");
    let start = joined.find(|c: char| c.is_ascii_alphanumeric())?;
    let mut out = joined[start..].to_string();
    out.truncate(LABEL_MAX);
    let out = out.trim_end().to_string();
    (!out.is_empty()).then_some(out)
}

// ---------------------------------------------------------------------------
// Formats

fn is_hex(text: &str, len: usize) -> bool {
    text.len() == len
        && text
            .bytes()
            .all(|b| b.is_ascii_digit() || (b'a'..=b'f').contains(&b))
}

fn is_hex64(text: &str) -> bool {
    is_hex(text, 64)
}

fn is_nonce(text: &str) -> bool {
    is_hex(text, 32)
}

fn is_version(text: &str) -> bool {
    let parts: Vec<&str> = text.split('.').collect();
    parts.len() == 3
        && parts.iter().zip([4usize, 4, 6]).all(|(p, max)| {
            !p.is_empty() && p.len() <= max && p.bytes().all(|b| b.is_ascii_digit())
        })
}

fn is_challenge_id(text: &str) -> bool {
    (1..=128).contains(&text.len())
        && text
            .bytes()
            .all(|b| b.is_ascii_alphanumeric() || b"._-".contains(&b))
}

fn is_signature(text: &str) -> bool {
    (1..=512).contains(&text.len())
        && text
            .bytes()
            .all(|b| b.is_ascii_alphanumeric() || b"._~+/=-".contains(&b))
}

fn is_timestamp(text: &str) -> bool {
    let b = text.as_bytes();
    b.len() == 20
        && b.iter().enumerate().all(|(i, &c)| match i {
            4 | 7 => c == b'-',
            10 => c == b'T',
            13 | 16 => c == b':',
            19 => c == b'Z',
            _ => c.is_ascii_digit(),
        })
}

fn is_label(text: &str) -> bool {
    let b = text.as_bytes();
    (1..=LABEL_MAX).contains(&b.len())
        && b[0].is_ascii_alphanumeric()
        && b.iter()
            .all(|&c| c.is_ascii_alphanumeric() || LABEL_PUNCTUATION.as_bytes().contains(&c))
}

fn is_os_name(text: &str) -> bool {
    let b = text.as_bytes();
    (2..=16).contains(&b.len())
        && b[0].is_ascii_lowercase()
        && b.iter()
            .all(|c| c.is_ascii_lowercase() || c.is_ascii_digit())
}

fn is_arch(text: &str) -> bool {
    let b = text.as_bytes();
    (2..=16).contains(&b.len())
        && b[0].is_ascii_lowercase()
        && b.iter()
            .all(|&c| c.is_ascii_lowercase() || c.is_ascii_digit() || c == b'_')
}

// ---------------------------------------------------------------------------
// Validation (docs/proof-kit.md, "Validation rules")

struct Checker {
    errors: Vec<String>,
}

impl Checker {
    fn fail(&mut self, at: &str, message: &str) {
        self.errors.push(format!("{at}: {message}"));
    }

    fn object<'v>(
        &mut self,
        at: &str,
        value: &'v Value,
        keys: &[&str],
    ) -> Option<&'v Map<String, Value>> {
        let Some(map) = value.as_object() else {
            self.fail(at, "must be an object");
            return None;
        };
        let missing: Vec<&str> = keys
            .iter()
            .copied()
            .filter(|k| !map.contains_key(*k))
            .collect();
        let mut extra: Vec<&str> = map
            .keys()
            .map(String::as_str)
            .filter(|k| !keys.contains(k))
            .collect();
        extra.sort_unstable();
        if !missing.is_empty() {
            self.fail(at, &format!("missing fields {missing:?}"));
        }
        if !extra.is_empty() {
            self.fail(at, &format!("unknown fields {extra:?}"));
        }
        (missing.is_empty() && extra.is_empty()).then_some(map)
    }

    fn text(&mut self, at: &str, value: &Value, ok: fn(&str) -> bool, nullable: bool) {
        let fine = match value {
            Value::Null => nullable,
            Value::String(text) => ok(text),
            _ => false,
        };
        if !fine {
            self.fail(at, "has the wrong format");
        }
    }

    fn one_of(&mut self, at: &str, value: &Value, allowed: &[&str], nullable: bool) {
        let fine = match value {
            Value::Null => nullable,
            Value::String(text) => allowed.contains(&text.as_str()),
            _ => false,
        };
        if !fine {
            self.fail(at, &format!("must be one of {allowed:?}"));
        }
    }

    fn integer(&mut self, at: &str, value: &Value, low: u64, high: u64, nullable: bool) {
        let fine = match value {
            Value::Null => nullable,
            other => other.as_u64().is_some_and(|n| (low..=high).contains(&n)),
        };
        if !fine {
            self.fail(at, &format!("must be an integer in [{low}, {high}]"));
        }
    }

    fn class(&mut self, at: &str, value: &Value, classes: &[u32]) {
        let fine = match value {
            Value::Null => true,
            other => other
                .as_u64()
                .and_then(|n| u32::try_from(n).ok())
                .is_some_and(|n| classes.contains(&n)),
        };
        if !fine {
            self.fail(at, &format!("must be null or one of {classes:?}"));
        }
    }

    fn equals(&mut self, at: &str, value: &Value, expected: &str) {
        if value.as_str() != Some(expected) {
            self.fail(at, &format!("must be {expected:?}"));
        }
    }

    fn case(&mut self, at: &str, value: &Value, id: &str, budget: usize) -> Option<CaseDigest> {
        let map = self.object(at, value, &["id", "tokens", "output_hash", "logits_digest"])?;
        self.equals(&format!("{at}.id"), &map["id"], id);
        let tokens: Option<Vec<u32>> = map["tokens"].as_array().and_then(|list| {
            list.iter()
                .map(|t| {
                    t.as_u64()
                        .filter(|&n| n < VOCAB_SIZE)
                        .and_then(|n| u32::try_from(n).ok())
                })
                .collect()
        });
        let tokens = tokens.filter(|t| (1..=budget).contains(&t.len()));
        if tokens.is_none() {
            self.fail(
                &format!("{at}.tokens"),
                &format!("must be 1..{budget} token ids below {VOCAB_SIZE}"),
            );
        }
        self.text(
            &format!("{at}.output_hash"),
            &map["output_hash"],
            is_hex64,
            false,
        );
        self.text(
            &format!("{at}.logits_digest"),
            &map["logits_digest"],
            is_hex64,
            false,
        );
        Some(CaseDigest {
            id: id.to_string(),
            tokens: tokens?,
            output_hash: map["output_hash"].as_str()?.to_string(),
            logits_digest: map["logits_digest"].as_str()?.to_string(),
        })
    }
}

/// Check one result against every rule of docs/proof-kit.md ("Validation
/// rules"), including the internal consistency of every digest. `raw_len` is
/// the size of the body as sent. Returns the problems found.
pub fn validate_result(value: &Value, raw_len: Option<usize>) -> Vec<String> {
    let mut c = Checker { errors: Vec::new() };
    if let Some(len) = raw_len.filter(|&len| len > MAX_RESULT_BYTES) {
        c.fail(
            "body",
            &format!("{len} bytes is over the {MAX_RESULT_BYTES}-byte limit"),
        );
    }
    let top = [
        "schema",
        "kit_version",
        "arc_version",
        "nonce",
        "model",
        "verdict",
        "golden",
        "challenge",
        "speed_method",
        "runs",
        "device",
        "island",
    ];
    let Some(result) = c.object("result", value, &top) else {
        return c.errors;
    };
    c.equals("schema", &result["schema"], RESULT_SCHEMA);
    c.text("kit_version", &result["kit_version"], is_version, false);
    c.text("arc_version", &result["arc_version"], is_version, false);
    c.text("nonce", &result["nonce"], is_nonce, false);
    let pinned = [
        ("repo", MODEL_REPO),
        ("revision", MODEL_REVISION),
        ("profile", super::PROFILE),
        ("package_sha256", PACKAGE_SHA256),
        ("manifest_blake3", MANIFEST_BLAKE3),
    ];
    let keys: Vec<&str> = pinned.iter().map(|(k, _)| *k).collect();
    if let Some(model) = c.object("model", &result["model"], &keys) {
        for (key, expected) in pinned {
            c.equals(&format!("model.{key}"), &model[key], expected);
        }
    }
    c.one_of("verdict", &result["verdict"], &["MATCH", "MISMATCH"], false);
    c.equals("speed_method", &result["speed_method"], SPEED_METHOD);

    let mut golden_cases: Option<Vec<CaseDigest>> = None;
    let mut golden_digest: Option<&str> = None;
    if let Some(golden) = c.object(
        "golden",
        &result["golden"],
        &["published_digest", "digest", "cases"],
    ) {
        c.equals(
            "golden.published_digest",
            &golden["published_digest"],
            PUBLISHED_GOLDEN_DIGEST,
        );
        c.text("golden.digest", &golden["digest"], is_hex64, false);
        golden_digest = golden["digest"].as_str();
        match golden["cases"].as_array() {
            Some(list) if list.len() == GOLDEN_CASES.len() => {
                // Check every case (so every problem is reported), then keep
                // the digests only if all of them parsed.
                let checked: Vec<Option<CaseDigest>> = list
                    .iter()
                    .zip(GOLDEN_CASES)
                    .enumerate()
                    .map(|(i, (case, (id, budget)))| {
                        c.case(&format!("golden.cases[{i}]"), case, id, budget)
                    })
                    .collect();
                golden_cases = checked.into_iter().collect();
            }
            _ => c.fail("golden.cases", "must list the 5 golden cases in order"),
        }
    }

    let mut challenge_case: Option<CaseDigest> = None;
    let mut challenge_digest: Option<&str> = None;
    let challenge_keys = [
        "challenge_id",
        "seed",
        "expires_at",
        "signature",
        "prompt_sha256",
        "digest",
        "case",
    ];
    if let Some(ch) = c.object("challenge", &result["challenge"], &challenge_keys) {
        c.text(
            "challenge.challenge_id",
            &ch["challenge_id"],
            is_challenge_id,
            false,
        );
        c.text("challenge.seed", &ch["seed"], is_hex64, false);
        c.text(
            "challenge.expires_at",
            &ch["expires_at"],
            is_timestamp,
            false,
        );
        c.text("challenge.signature", &ch["signature"], is_signature, false);
        c.text(
            "challenge.prompt_sha256",
            &ch["prompt_sha256"],
            is_hex64,
            false,
        );
        c.text("challenge.digest", &ch["digest"], is_hex64, false);
        challenge_digest = ch["digest"].as_str();
        challenge_case = c.case(
            "challenge.case",
            &ch["case"],
            "challenge",
            CHALLENGE_MAX_TOKENS,
        );
        if let Some(seed) = ch["seed"].as_str().filter(|s| is_hex64(s)) {
            let derived = challenge_prompt(seed).map(|p| sha256_hex(&p)).ok();
            if derived.as_deref() != ch["prompt_sha256"].as_str() {
                c.fail(
                    "challenge.prompt_sha256",
                    "is not the SHA-256 of the prompt the seed derives",
                );
            }
        }
    }

    let run_keys = [
        "backend",
        "isa",
        "verdict",
        "golden_digest",
        "challenge_digest",
        "prefill_tok_s",
        "decode_tok_s",
        "threads",
        "vector_projections",
        "adapter",
        "divergence",
    ];
    let mut runs_ok = false;
    match result["runs"].as_array() {
        Some(runs) if (1..=4).contains(&runs.len()) => {
            runs_ok = true;
            let backends: Vec<&str> = runs
                .iter()
                .filter_map(|r| r.get("backend").and_then(Value::as_str))
                .collect();
            let mut distinct = backends.clone();
            distinct.sort_unstable();
            distinct.dedup();
            if distinct.len() != runs.len() {
                c.fail("runs", "backends must be distinct");
            }
            if !backends.iter().any(|b| b.starts_with("cpu-")) {
                c.fail("runs", "must include a CPU backend");
            }
            for (i, run) in runs.iter().enumerate() {
                let at = format!("runs[{i}]");
                let Some(run) = c.object(&at, run, &run_keys) else {
                    runs_ok = false;
                    continue;
                };
                c.one_of(&format!("{at}.backend"), &run["backend"], &BACKENDS, false);
                c.one_of(&format!("{at}.isa"), &run["isa"], &ISAS, true);
                if run["backend"] == "cpu-scalar" && !run["isa"].is_null() {
                    c.fail(&format!("{at}.isa"), "must be null for cpu-scalar");
                }
                c.one_of(
                    &format!("{at}.verdict"),
                    &run["verdict"],
                    &["MATCH", "MISMATCH"],
                    false,
                );
                c.text(
                    &format!("{at}.golden_digest"),
                    &run["golden_digest"],
                    is_hex64,
                    false,
                );
                c.text(
                    &format!("{at}.challenge_digest"),
                    &run["challenge_digest"],
                    is_hex64,
                    false,
                );
                for key in ["prefill_tok_s", "decode_tok_s"] {
                    let fine = run[key]
                        .as_f64()
                        .is_some_and(|x| x.is_finite() && (0.0..=100_000.0).contains(&x));
                    if !fine {
                        c.fail(&format!("{at}.{key}"), "must be a number in [0, 100000]");
                    }
                }
                c.integer(&format!("{at}.threads"), &run["threads"], 1, 4096, true);
                let vp = &run["vector_projections"];
                if !vp.is_null()
                    && let Some(vp) = c.object(
                        &format!("{at}.vector_projections"),
                        vp,
                        &["attempted", "accepted"],
                    )
                {
                    let limit = 1u64 << 53;
                    c.integer(
                        &format!("{at}.vector_projections.attempted"),
                        &vp["attempted"],
                        0,
                        limit,
                        false,
                    );
                    c.integer(
                        &format!("{at}.vector_projections.accepted"),
                        &vp["accepted"],
                        0,
                        limit,
                        false,
                    );
                    if let (Some(a), Some(b)) = (vp["attempted"].as_u64(), vp["accepted"].as_u64())
                        && b > a
                    {
                        c.fail(
                            &format!("{at}.vector_projections"),
                            "accepted exceeds attempted",
                        );
                    }
                }
                let adapter = &run["adapter"];
                let cpu = run["backend"]
                    .as_str()
                    .is_some_and(|b| b.starts_with("cpu-"));
                if cpu && !adapter.is_null() {
                    c.fail(&format!("{at}.adapter"), "must be null for a CPU backend");
                } else if !adapter.is_null()
                    && let Some(adapter) = c.object(
                        &format!("{at}.adapter"),
                        adapter,
                        &["vendor", "device", "backend", "driver"],
                    )
                {
                    c.text(
                        &format!("{at}.adapter.vendor"),
                        &adapter["vendor"],
                        is_label,
                        true,
                    );
                    c.text(
                        &format!("{at}.adapter.device"),
                        &adapter["device"],
                        is_label,
                        true,
                    );
                    c.one_of(
                        &format!("{at}.adapter.backend"),
                        &adapter["backend"],
                        &GPU_APIS,
                        true,
                    );
                    c.text(
                        &format!("{at}.adapter.driver"),
                        &adapter["driver"],
                        is_label,
                        true,
                    );
                }
                let div = &run["divergence"];
                let verdict = run["verdict"].as_str();
                if verdict == Some("MATCH") && !div.is_null() {
                    c.fail(
                        &format!("{at}.divergence"),
                        "must be null when the run matches",
                    );
                }
                if verdict == Some("MISMATCH") && div.is_null() {
                    c.fail(
                        &format!("{at}.divergence"),
                        "must be an object when the run does not match",
                    );
                }
                if !div.is_null()
                    && let Some(div) = c.object(
                        &format!("{at}.divergence"),
                        div,
                        &["case", "position", "layer", "op"],
                    )
                {
                    let mut cases: Vec<&str> = GOLDEN_CASES.iter().map(|(id, _)| *id).collect();
                    cases.push("challenge");
                    c.one_of(&format!("{at}.divergence.case"), &div["case"], &cases, true);
                    c.integer(
                        &format!("{at}.divergence.position"),
                        &div["position"],
                        0,
                        65_535,
                        true,
                    );
                    c.integer(
                        &format!("{at}.divergence.layer"),
                        &div["layer"],
                        0,
                        1023,
                        true,
                    );
                    c.one_of(&format!("{at}.divergence.op"), &div["op"], &OPS, true);
                }
            }
        }
        _ => c.fail("runs", "must list 1 to 4 runs"),
    }

    let device_keys = [
        "os",
        "os_version",
        "arch",
        "cpu_model",
        "logical_cpus",
        "cpu_features",
        "gpu_model",
    ];
    if let Some(device) = c.object("device", &result["device"], &device_keys) {
        c.text("device.os", &device["os"], is_os_name, false);
        c.text("device.os_version", &device["os_version"], is_label, true);
        c.text("device.arch", &device["arch"], is_arch, false);
        c.text("device.cpu_model", &device["cpu_model"], is_label, true);
        c.integer(
            "device.logical_cpus",
            &device["logical_cpus"],
            1,
            4096,
            false,
        );
        let features: Option<Vec<&str>> = device["cpu_features"]
            .as_array()
            .and_then(|list| list.iter().map(Value::as_str).collect());
        let fine = features.is_some_and(|list| {
            let mut distinct = list.clone();
            distinct.sort_unstable();
            distinct.dedup();
            distinct.len() == list.len() && list.iter().all(|f| CPU_FEATURES.contains(f))
        });
        if !fine {
            c.fail(
                "device.cpu_features",
                &format!("must be distinct values from {CPU_FEATURES:?}"),
            );
        }
        c.text("device.gpu_model", &device["gpu_model"], is_label, true);
    }

    let island_keys = [
        "memory_class_gb",
        "unified_memory",
        "gpu_vram_class_gb",
        "thunderbolt5",
        "download_mbps_class",
    ];
    if !result["island"].is_null()
        && let Some(island) = c.object("island", &result["island"], &island_keys)
    {
        c.class(
            "island.memory_class_gb",
            &island["memory_class_gb"],
            &MEMORY_CLASSES_GB,
        );
        if !island["unified_memory"].is_boolean() {
            c.fail("island.unified_memory", "must be true or false");
        }
        c.class(
            "island.gpu_vram_class_gb",
            &island["gpu_vram_class_gb"],
            &VRAM_CLASSES_GB,
        );
        if !island["thunderbolt5"].is_null() && !island["thunderbolt5"].is_boolean() {
            c.fail("island.thunderbolt5", "must be true, false or null");
        }
        c.class(
            "island.download_mbps_class",
            &island["download_mbps_class"],
            &NETWORK_CLASSES_MBPS,
        );
    }

    // The verdicts follow from the digests alone; every digest must be the
    // hash of what the result carries.
    if !c.errors.is_empty() || !runs_ok {
        return c.errors;
    }
    let (Some(golden_cases), Some(golden_digest), Some(challenge_case), Some(challenge_digest)) = (
        golden_cases,
        golden_digest,
        challenge_case,
        challenge_digest,
    ) else {
        return c.errors;
    };
    let runs = result["runs"]
        .as_array()
        .map(Vec::as_slice)
        .unwrap_or_default();
    let mut all_match = true;
    for (i, run) in runs.iter().enumerate() {
        let matches = run["golden_digest"] == PUBLISHED_GOLDEN_DIGEST
            && run["challenge_digest"] == challenge_digest;
        all_match &= matches;
        let expected = if matches { "MATCH" } else { "MISMATCH" };
        if run["verdict"] != expected {
            c.fail(
                &format!("runs[{i}].verdict"),
                &format!("must be {expected} for these digests"),
            );
        }
    }
    let expected = if all_match { "MATCH" } else { "MISMATCH" };
    if result["verdict"] != expected {
        c.fail("verdict", "must be MATCH exactly when every run matches");
    }
    if runs[0]["golden_digest"] != golden_digest {
        c.fail(
            "runs[0].golden_digest",
            "must equal golden.digest (the cases come from runs[0])",
        );
    }
    if runs[0]["challenge_digest"] != challenge_digest {
        c.fail("runs[0].challenge_digest", "must equal challenge.digest");
    }
    for (i, case) in golden_cases.iter().enumerate() {
        if hex_lower(&arith::tokens_hash(&case.tokens)) != case.output_hash {
            c.fail(
                &format!("golden.cases[{i}].output_hash"),
                "is not the hash of its tokens",
            );
        }
    }
    if matrix_digest(&golden_cases).ok().as_deref() != Some(golden_digest) {
        c.fail("golden.digest", "is not the digest of golden.cases");
    }
    if hex_lower(&arith::tokens_hash(&challenge_case.tokens)) != challenge_case.output_hash {
        c.fail(
            "challenge.case.output_hash",
            "is not the hash of its tokens",
        );
    }
    if matrix_digest(std::slice::from_ref(&challenge_case))
        .ok()
        .as_deref()
        != Some(challenge_digest)
    {
        c.fail("challenge.digest", "is not the digest of challenge.case");
    }
    c.errors
}

#[cfg(test)]
mod tests {
    use super::*;

    fn reference_digests() -> Vec<CaseDigest> {
        golden_reference()
            .unwrap()
            .into_iter()
            .map(|case| {
                let raw: Vec<[u8; 32]> = case
                    .logits_hashes
                    .iter()
                    .map(|h| {
                        let mut out = [0u8; 32];
                        out.copy_from_slice(&hex::decode(h).unwrap());
                        out
                    })
                    .collect();
                CaseDigest {
                    id: case.id.clone(),
                    output_hash: hex_lower(&arith::tokens_hash(&case.tokens)),
                    logits_digest: hex_lower(&arith::logits_digest(&raw)),
                    tokens: case.tokens,
                }
            })
            .collect()
    }

    fn sample_run(backend: &'static str, isa: Option<&'static str>) -> RunSummary {
        let golden = reference_digests();
        let golden_digest = matrix_digest(&golden).unwrap();
        let tokens = vec![791u32, 6864, 315, 128_012];
        let challenge = CaseDigest {
            id: "challenge".into(),
            output_hash: hex_lower(&arith::tokens_hash(&tokens)),
            logits_digest: "ab".repeat(32),
            tokens,
        };
        let challenge_digest = matrix_digest(std::slice::from_ref(&challenge)).unwrap();
        RunSummary {
            backend,
            isa,
            golden,
            golden_digest,
            challenge,
            challenge_digest,
            prefill_tok_s: 3.476_110_978,
            decode_tok_s: 3.437_051_902,
            threads: Some(4),
            vector_projections: Some((121_187, 121_187)),
            adapter: None,
            divergence: None,
        }
    }

    fn sample_device() -> DeviceProfile {
        DeviceProfile {
            os: "linux".into(),
            os_version: Some("ubuntu 24.04".into()),
            arch: "x86_64".into(),
            cpu_model: label("AMD EPYC 7763 64-Core Processor"),
            logical_cpus: 4,
            cpu_features: vec!["avx2", "fma"],
            gpu_model: None,
        }
    }

    fn sample_island() -> IslandProfile {
        IslandProfile {
            memory_class_gb: Some(16),
            unified_memory: false,
            gpu_vram_class_gb: None,
            thunderbolt5: None,
            download_mbps_class: Some(1000),
        }
    }

    fn sample_result(runs: &[RunSummary]) -> Value {
        let challenge = Challenge::test();
        let device = sample_device();
        let island = sample_island();
        build_result(&ResultInputs {
            nonce: "0123456789abcdef0123456789abcdef",
            challenge: &challenge,
            runs,
            device: &device,
            island: Some(&island),
        })
        .unwrap()
    }

    #[test]
    fn the_word_list_is_the_published_one() {
        let words = challenge_words().unwrap();
        assert_eq!(words.len(), 256);
        assert_eq!(words[0], "acorns");
        assert_eq!(words[255], "zebras");
    }

    #[test]
    fn challenge_prompts_match_the_published_vectors() {
        let text = include_str!("../../../../docs/protocol/proof-kit/challenge-vectors-v1.json");
        let vectors: Value = serde_json::from_str(text).unwrap();
        assert_eq!(vectors["words_sha256"], CHALLENGE_WORDS_SHA256);
        assert_eq!(vectors["scheme"], CHALLENGE_SCHEME);
        let list = vectors["vectors"].as_array().unwrap();
        assert!(list.len() >= 10);
        for vector in list {
            let seed = vector["seed"].as_str().unwrap();
            let indices: Vec<u64> = challenge_word_indices(seed)
                .unwrap()
                .iter()
                .map(|&i| i as u64)
                .collect();
            let expected: Vec<u64> = vector["word_indices"]
                .as_array()
                .unwrap()
                .iter()
                .map(|v| v.as_u64().unwrap())
                .collect();
            assert_eq!(indices, expected, "seed {seed}");
            let prompt = challenge_prompt(seed).unwrap();
            assert_eq!(prompt, vector["prompt"].as_str().unwrap(), "seed {seed}");
            assert_eq!(
                sha256_hex(&prompt),
                vector["prompt_sha256"].as_str().unwrap()
            );
        }
    }

    #[test]
    fn the_test_challenge_derives_the_documented_prompt() {
        let challenge = Challenge::test();
        assert!(challenge.is_test());
        assert_eq!(
            challenge.prompt().unwrap(),
            "Write one short sentence that mentions frogs, kayaks, buttons and teapots."
        );
        let case = challenge_case(&challenge.prompt().unwrap());
        assert_eq!(case["id"], "challenge");
        assert_eq!(case["max_tokens"], 24);
        assert_eq!(case["eos"], json!([128_012]));
        assert_eq!(case["selection"], "rp64-argmax");
    }

    #[test]
    fn seeds_and_challenges_are_checked() {
        assert!(parse_seed(TEST_CHALLENGE_SEED).is_ok());
        let (short, upper, non_hex) = ("0".repeat(63), "A".repeat(64), "g".repeat(64));
        for bad in ["", "00", short.as_str(), upper.as_str(), non_hex.as_str()] {
            assert!(parse_seed(bad).is_err(), "{bad}");
        }
        let good = json!({
            "challenge_id": "c-2026-10-06.abc_DEF",
            "seed": "ff".repeat(32),
            "expires_at": "2026-10-06T18:00:00Z",
            "signature": "aGVsbG8=.x_y-z",
            "issued_at": "ignored",
        });
        let challenge = Challenge::from_json(&good).unwrap();
        assert_eq!(challenge.expires_unix(), Some(1_791_309_600));
        assert!(!challenge.is_test());
        for (key, bad) in [
            ("challenge_id", json!("has space")),
            ("challenge_id", json!(TEST_CHALLENGE_ID)),
            ("seed", json!("FF".repeat(32))),
            ("expires_at", json!("2026-10-06 18:00:00Z")),
            ("signature", json!("x".repeat(513))),
            ("signature", json!(7)),
        ] {
            let mut value = good.clone();
            value[key] = bad;
            assert!(Challenge::from_json(&value).is_err(), "{key}");
        }
        let mut missing = good.clone();
        let _ = missing.as_object_mut().unwrap().remove("seed");
        assert!(Challenge::from_json(&missing).is_err());
    }

    #[test]
    fn timestamps_convert_to_unix_seconds() {
        assert_eq!(unix_seconds("2099-12-31T23:59:59Z"), Some(4_102_444_799));
        assert_eq!(unix_seconds("2026-10-06T17:00:00Z"), Some(1_791_306_000));
        assert_eq!(unix_seconds("2000-02-29T12:00:00Z"), Some(951_825_600));
        for bad in [
            "2026-02-29T00:00:00Z",
            "2026-13-01T00:00:00Z",
            "2026-10-06T24:00:00Z",
            "1999-12-31T23:59:59Z",
            "2026-10-06T17:00:00",
            "2026-10-06T17:00:00.5Z",
        ] {
            assert_eq!(unix_seconds(bad), None, "{bad}");
        }
    }

    #[test]
    fn the_golden_reference_reproduces_the_published_digest() {
        let reference = golden_reference().unwrap();
        let ids: Vec<&str> = reference.iter().map(|c| c.id.as_str()).collect();
        let expected: Vec<&str> = GOLDEN_CASES.iter().map(|(id, _)| *id).collect();
        assert_eq!(ids, expected);
        for (case, (_, budget)) in reference.iter().zip(GOLDEN_CASES) {
            assert!((1..=budget).contains(&case.tokens.len()));
            assert_eq!(
                case.logits_hashes.len(),
                case.prompt_tokens.len() + case.tokens.len() - 1
            );
            assert_eq!(case.input_token(0), case.prompt_tokens.first().copied());
            assert_eq!(
                case.input_token(case.prompt_tokens.len()),
                case.tokens.first().copied()
            );
        }
        assert_eq!(
            matrix_digest(&reference_digests()).unwrap(),
            PUBLISHED_GOLDEN_DIGEST
        );
        // The digests recorded next to the hashes agree with them.
        let value: Value = serde_json::from_str(GOLDEN_REFERENCE_JSON).unwrap();
        for (case, digest) in value["cases"]
            .as_array()
            .unwrap()
            .iter()
            .zip(reference_digests())
        {
            assert_eq!(case["output_hash"], digest.output_hash.as_str());
            assert_eq!(case["logits_digest"], digest.logits_digest.as_str());
        }
        assert_eq!(value["golden_digest"], PUBLISHED_GOLDEN_DIGEST);
    }

    #[test]
    fn pinned_constants_match_the_embedded_files() {
        let manifest: Value = serde_json::from_str(PINNED_MANIFEST_JSON).unwrap();
        assert_eq!(manifest["package"]["sha256"], PACKAGE_SHA256);
        assert_eq!(manifest["manifest_blake3"], MANIFEST_BLAKE3);
        assert_eq!(manifest["source"]["repo"], MODEL_REPO);
        assert_eq!(manifest["source"]["revision"], MODEL_REVISION);
        assert_eq!(manifest["profile"], super::super::PROFILE);
        let cases: Value = serde_json::from_str(GOLDEN_CASES_JSON).unwrap();
        let listed: Vec<(String, u64)> = cases["cases"]
            .as_array()
            .unwrap()
            .iter()
            .map(|c| {
                (
                    c["id"].as_str().unwrap().to_string(),
                    c["max_tokens"].as_u64().unwrap(),
                )
            })
            .collect();
        let expected: Vec<(String, u64)> = GOLDEN_CASES
            .iter()
            .map(|(id, budget)| (id.to_string(), *budget as u64))
            .collect();
        assert_eq!(listed, expected);
        let source: Value = serde_json::from_str(SOURCE_MANIFEST_JSON).unwrap();
        assert_eq!(source["repo"], MODEL_REPO);
        assert_eq!(source["revision"], MODEL_REVISION);
    }

    #[test]
    fn divergence_is_located_at_the_first_differing_position() {
        let reference = golden_reference().unwrap();
        assert_eq!(first_divergence(&reference, &reference), None);

        let mut logits = reference.clone();
        logits[2].logits_hashes[10] = "00".repeat(32);
        logits[3].logits_hashes[0] = "00".repeat(32);
        let found = first_divergence(&reference, &logits).unwrap();
        assert_eq!(found.case.as_deref(), Some("integers"));
        assert_eq!(found.position, Some(10));
        assert_eq!(found.op, None);

        let mut prompt = reference.clone();
        prompt[1].prompt_tokens[5] += 1;
        let found = first_divergence(&reference, &prompt).unwrap();
        assert_eq!(
            (found.case.as_deref(), found.position, found.op),
            (Some("haiku"), Some(5), Some("tokenizer"))
        );

        let mut selected = reference.clone();
        selected[0].tokens[2] += 1;
        let found = first_divergence(&reference, &selected).unwrap();
        let expected_position = reference[0].prompt_tokens.len() + 1;
        assert_eq!(
            (found.position, found.op),
            (Some(expected_position), Some("select"))
        );
    }

    #[test]
    fn a_complete_result_validates() {
        let runs = [
            sample_run("cpu-scalar", None),
            sample_run("cpu-simd", Some("avx2")),
        ];
        let result = sample_result(&runs);
        let wire = wire_json(&result);
        assert!(wire.len() <= MAX_RESULT_BYTES, "{} bytes", wire.len());
        assert_eq!(
            validate_result(&result, Some(wire.len())),
            Vec::<String>::new()
        );
        assert_eq!(result["verdict"], "MATCH");
        assert_eq!(result["challenge"]["challenge_id"], TEST_CHALLENGE_ID);
        assert_eq!(
            result["challenge"]["prompt_sha256"],
            sha256_hex(
                "Write one short sentence that mentions frogs, kayaks, buttons and teapots."
            )
        );
        assert!(result["runs"][1]["divergence"].is_null());
        assert_eq!(result["runs"][1]["isa"], "avx2");
        let prefill = result["runs"][0]["prefill_tok_s"].as_f64().unwrap();
        assert!((prefill - 3.476).abs() < 1e-9, "{prefill}");
        // The wire form parses back to the same value.
        let parsed: Value = serde_json::from_str(&wire).unwrap();
        assert_eq!(parsed, result);
        assert!(wire.contains("\"tokens\": [791, 6864, 315"));
    }

    #[test]
    fn a_mismatching_run_carries_its_divergence() {
        let mut bad = sample_run("cpu-simd", Some("neon-dotprod"));
        bad.golden_digest = "cd".repeat(32);
        bad.divergence = Some(Divergence {
            case: Some("haiku".into()),
            position: Some(81),
            layer: Some(7),
            op: Some("w_gate"),
        });
        let runs = [sample_run("cpu-scalar", None), bad];
        let result = sample_result(&runs);
        assert_eq!(validate_result(&result, None), Vec::<String>::new());
        assert_eq!(result["verdict"], "MISMATCH");
        assert_eq!(result["runs"][0]["verdict"], "MATCH");
        assert_eq!(result["runs"][1]["verdict"], "MISMATCH");
        assert_eq!(result["runs"][1]["divergence"]["op"], "w_gate");
        assert_eq!(result["runs"][1]["divergence"]["layer"], 7);

        // A mismatch with nothing located still reports an (empty) divergence.
        let mut unlocated = sample_run("cpu-simd", Some("avx2"));
        unlocated.challenge_digest = "ef".repeat(32);
        let runs = [sample_run("cpu-scalar", None), unlocated];
        let result = sample_result(&runs);
        assert_eq!(validate_result(&result, None), Vec::<String>::new());
        assert_eq!(
            result["runs"][1]["divergence"],
            Divergence::unlocated().to_json()
        );
    }

    #[test]
    fn tampered_results_are_rejected() {
        let runs = [sample_run("cpu-scalar", None)];
        let good = sample_result(&runs);
        type Mutation = Box<dyn Fn(&mut Value)>;
        let mutations: Vec<(&str, Mutation)> = vec![
            (
                "extra field",
                Box::new(|v: &mut Value| v["hostname"] = json!("alice-laptop")),
            ),
            (
                "extra device field",
                Box::new(|v: &mut Value| v["device"]["serial"] = json!("C02X")),
            ),
            ("nonce", Box::new(|v: &mut Value| v["nonce"] = json!("xyz"))),
            (
                "schema",
                Box::new(|v: &mut Value| v["schema"] = json!("arc.proof-result.v2")),
            ),
            (
                "model",
                Box::new(|v: &mut Value| v["model"]["package_sha256"] = json!("00".repeat(32))),
            ),
            (
                "claimed match",
                Box::new(|v: &mut Value| v["golden"]["digest"] = json!("11".repeat(32))),
            ),
            (
                "run verdict",
                Box::new(|v: &mut Value| v["runs"][0]["verdict"] = json!("MISMATCH")),
            ),
            (
                "top verdict",
                Box::new(|v: &mut Value| v["verdict"] = json!("MISMATCH")),
            ),
            (
                "tokens",
                Box::new(|v: &mut Value| v["golden"]["cases"][0]["tokens"][0] = json!(1)),
            ),
            (
                "case order",
                Box::new(|v: &mut Value| v["golden"]["cases"][0]["id"] = json!("haiku")),
            ),
            (
                "challenge prompt",
                Box::new(|v: &mut Value| v["challenge"]["prompt_sha256"] = json!("22".repeat(32))),
            ),
            (
                "challenge digest",
                Box::new(|v: &mut Value| v["challenge"]["digest"] = json!("33".repeat(32))),
            ),
            (
                "free text",
                Box::new(|v: &mut Value| v["device"]["cpu_model"] = json!("Apple M2\nhello")),
            ),
            (
                "feature",
                Box::new(|v: &mut Value| v["device"]["cpu_features"] = json!(["avx2", "avx2"])),
            ),
            (
                "class",
                Box::new(|v: &mut Value| v["island"]["memory_class_gb"] = json!(17)),
            ),
            (
                "adapter on cpu",
                Box::new(|v: &mut Value| {
                    v["runs"][0]["adapter"] =
                        json!({"vendor": "x", "device": "y", "backend": "gl", "driver": "1"})
                }),
            ),
            (
                "speed",
                Box::new(|v: &mut Value| v["runs"][0]["decode_tok_s"] = json!(-1)),
            ),
            (
                "threads",
                Box::new(|v: &mut Value| v["runs"][0]["threads"] = json!(0)),
            ),
            (
                "timestamp",
                Box::new(|v: &mut Value| v["challenge"]["expires_at"] = json!("tomorrow")),
            ),
        ];
        for (name, mutate) in mutations {
            let mut value = good.clone();
            mutate(&mut value);
            assert!(
                !validate_result(&value, None).is_empty(),
                "{name} was accepted"
            );
        }
        assert!(!validate_result(&good, Some(MAX_RESULT_BYTES + 1)).is_empty());
        let mut duplicate = good.clone();
        let first = duplicate["runs"][0].clone();
        duplicate["runs"].as_array_mut().unwrap().push(first);
        assert!(
            !validate_result(&duplicate, None).is_empty(),
            "duplicate backend accepted"
        );
    }

    #[test]
    fn a_gpu_run_slots_in() {
        let mut gpu = sample_run("gpu-wgpu", None);
        gpu.threads = None;
        gpu.vector_projections = None;
        gpu.adapter = Some(json!({
            "vendor": "nvidia",
            "device": "NVIDIA GeForce RTX 4070",
            "backend": "vulkan",
            "driver": "560.94",
        }));
        let runs = [sample_run("cpu-scalar", None), gpu];
        let result = sample_result(&runs);
        assert_eq!(validate_result(&result, None), Vec::<String>::new());
    }

    #[test]
    fn classes_floor_with_an_allowance() {
        let gib = |x: f64| (x * GIB) as u64;
        assert_eq!(memory_class_gb(gib(16.0)), Some(16));
        assert_eq!(memory_class_gb(gib(15.5)), Some(16));
        assert_eq!(memory_class_gb(gib(14.0)), Some(16));
        assert_eq!(memory_class_gb(gib(36.0)), Some(32));
        assert_eq!(memory_class_gb(gib(512.0)), Some(512));
        assert_eq!(memory_class_gb(gib(0.5)), None);
        assert_eq!(vram_class_gb(24_564 * 1_048_576), Some(24));
        assert_eq!(vram_class_gb(8_188 * 1_048_576), Some(8));
        assert_eq!(network_class_mbps(300e6), Some(250));
        assert_eq!(network_class_mbps(0.5e6), None);
        assert_eq!(network_class_mbps(f64::NAN), None);
    }

    #[test]
    fn labels_keep_only_the_label_alphabet() {
        assert_eq!(
            label("Intel(R) Core(TM) i7-8700B CPU @ 3.20GHz").as_deref(),
            Some("Intel(R) Core(TM) i7-8700B CPU @ 3.20GHz")
        );
        assert_eq!(label("  [AMD/ATI]  ").as_deref(), Some("AMD/ATI"));
        assert_eq!(label("Apple M2\tPro\n").as_deref(), Some("Apple M2 Pro"));
        assert_eq!(label("---"), None);
        assert_eq!(label(""), None);
        let long = label(&"x".repeat(100)).unwrap();
        assert_eq!(long.len(), 64);
        assert!(is_label(&long));
        assert!(is_label(
            &label("Snapdragon(R) X Elite - X1E78100 - Qualcomm(R) Oryon(TM) CPU").unwrap()
        ));
    }

    #[test]
    fn nonces_are_fresh_hex() {
        let a = new_nonce().unwrap();
        let b = new_nonce().unwrap();
        assert!(is_nonce(&a) && is_nonce(&b));
        assert_ne!(a, b);
    }
}
