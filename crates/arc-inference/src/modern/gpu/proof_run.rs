//! The Proof Kit's GPU run entry: one `runs[]` object of an
//! `arc.proof-result.v1` result (PR #149, `docs/proof-kit.md`, "Result
//! format"), built from a GPU run of the golden prompts and the challenge.
//!
//! `arc-modern gpu-check --proof-run-out` writes it. The Proof Kit itself
//! (#149) assembles the whole result and submits it; until #149 is on `main`
//! and its `--gpu` arm calls this module, no Proof Kit result carries a GPU
//! run (`docs/gpu-worker-backend.md`).
//!
//! The rules below restate #149's (`modern/proof.rs`, `build_result`,
//! `run_matches` and the `runs[]` part of `validate_result`, at 93e4e368).
//! [`check_run_entry`] enforces them on every entry this module emits.

use arc_gpu::modern::AdapterReport;
use serde_json::{Value, json};

use super::adapter_json;
use super::backend::PROOF_BACKEND;

/// The published SmolLM3-3B golden digest (#149 `PUBLISHED_GOLDEN_DIGEST`,
/// `scripts/arc_modern/golden/smollm3-3b.cpu-golden.json`).
pub const PUBLISHED_GOLDEN_DIGEST: &str =
    "3e43f342c00cf3e3be3072e654e9d1c43f547a73b8a4fc6e906b7119e5cb49f2";
/// The golden case ids, in result order (#149 `GOLDEN_CASES`).
pub const GOLDEN_CASE_IDS: [&str; 5] = ["capital", "haiku", "integers", "primes", "product-greedy"];
/// Where a mismatch can start (#149 `OPS`).
pub const PROOF_OPS: [&str; 20] = [
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
/// The keys of a run entry, in #149's order.
pub const RUN_KEYS: [&str; 11] = [
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

/// Where a GPU run first differs, in the Proof Kit's terms.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ProofDivergence {
    /// A golden case id or `challenge`; `None` when unknown.
    pub case: Option<String>,
    /// Forward position in that case.
    pub position: Option<usize>,
    pub layer: Option<usize>,
    /// One of [`PROOF_OPS`].
    pub op: Option<&'static str>,
}

impl ProofDivergence {
    /// A mismatch nothing located.
    pub fn unlocated() -> Self {
        Self {
            case: None,
            position: None,
            layer: None,
            op: None,
        }
    }

    /// From the GPU trace localisation (`super::Divergence`): `forward` is
    /// the position, and the traced operation name maps to a Proof Kit op.
    pub fn from_trace(case: &str, forward: usize, trace_op: &str) -> Self {
        let (layer, op) = proof_op(trace_op);
        Self {
            case: Some(case.to_string()),
            position: Some(forward),
            layer,
            op,
        }
    }

    fn to_json(&self) -> Value {
        json!({
            "case": self.case,
            "position": self.position,
            "layer": self.layer,
            "op": self.op,
        })
    }
}

/// A GPU trace operation name (`layer12.gate`, `logits`, ...) as the Proof
/// Kit's `(layer, op)`. Names it does not know (a GPU refusal, a trace
/// layout difference) give `(layer, None)`.
pub fn proof_op(trace_op: &str) -> (Option<usize>, Option<&'static str>) {
    let (layer, name) = match trace_op
        .strip_prefix("layer")
        .and_then(|rest| rest.split_once('.'))
    {
        Some((index, name)) => (index.parse::<usize>().ok(), name),
        None => (None, trace_op),
    };
    let op = match name {
        "embed" => "embed",
        "attn_norm" => "attn_norm",
        "q" => "wq",
        "k" => "wk",
        "v" => "wv",
        "q_rope" => "rope_q",
        "k_rope" => "rope_k",
        "attention" => "attention",
        "o_proj" => "wo",
        "attn_residual" => "attn_residual",
        "ffn_norm" => "ffn_norm",
        "gate" => "w_gate",
        "up" => "w_up",
        "silu" => "silu",
        "down" => "w_down",
        "ffn_residual" => "ffn_residual",
        "final_norm" => "final_norm",
        "logits" => "lm_head",
        _ => return (layer, None),
    };
    (layer, Some(op))
}

/// Everything a GPU run entry reports.
#[derive(Debug, Clone)]
pub struct GpuProofRun<'a> {
    pub adapter: &'a AdapterReport,
    /// Matrix digest of the five golden cases on the GPU.
    pub golden_digest: String,
    /// Matrix digest of the challenge case on the GPU.
    pub challenge_digest: String,
    /// The first (CPU) run's challenge digest, which the GPU must equal.
    pub reference_challenge_digest: String,
    /// `arc.proof-speed.v1` over the golden prompts.
    pub prefill_tok_s: f64,
    pub decode_tok_s: f64,
    /// Where the run first differs, when it does and a trace located it.
    pub divergence: Option<ProofDivergence>,
}

/// #149 `speed`: three decimals, clamped to [0, 100000].
fn speed(value: f64) -> f64 {
    if value.is_finite() {
        ((value * 1000.0).round() / 1000.0).clamp(0.0, 100_000.0)
    } else {
        0.0
    }
}

/// The run entry, as #149's `build_result` writes a run: `MATCH` exactly
/// when the golden digest is the published one and the challenge digest
/// equals the reference run's; a mismatch always carries a divergence
/// (unlocated when nothing located it).
pub fn gpu_run_entry(run: &GpuProofRun<'_>) -> Value {
    let matches = run.golden_digest == PUBLISHED_GOLDEN_DIGEST
        && run.challenge_digest == run.reference_challenge_digest;
    let divergence = (!matches).then(|| {
        run.divergence
            .clone()
            .unwrap_or_else(ProofDivergence::unlocated)
            .to_json()
    });
    json!({
        "backend": PROOF_BACKEND,
        "isa": Value::Null,
        "verdict": if matches { "MATCH" } else { "MISMATCH" },
        "golden_digest": run.golden_digest,
        "challenge_digest": run.challenge_digest,
        "prefill_tok_s": speed(run.prefill_tok_s),
        "decode_tok_s": speed(run.decode_tok_s),
        "threads": Value::Null,
        "vector_projections": Value::Null,
        "adapter": adapter_json(run.adapter),
        "divergence": divergence,
    })
}

fn is_hex64(text: &str) -> bool {
    text.len() == 64
        && text
            .bytes()
            .all(|b| b.is_ascii_digit() || (b'a'..=b'f').contains(&b))
}

fn is_label(text: &str) -> bool {
    let b = text.as_bytes();
    (1..=64).contains(&b.len())
        && b[0].is_ascii_alphanumeric()
        && b.iter()
            .all(|&c| c.is_ascii_alphanumeric() || b" ()@.,+/_-".contains(&c))
}

/// #149's `runs[]` rules for a GPU entry (`validate_result`): exactly the
/// [`RUN_KEYS`], `backend` `gpu-wgpu`, `isa`/`threads`/`vector_projections`
/// null, 64-hex digests, speeds in [0, 100000], the adapter's labels, and a
/// divergence exactly when the verdict is `MISMATCH`. Returns the problems.
pub fn check_run_entry(run: &Value) -> Vec<String> {
    let mut problems = Vec::new();
    let mut fail = |what: &str| problems.push(what.to_string());
    let Some(object) = run.as_object() else {
        return vec!["run is not an object".into()];
    };
    let mut keys: Vec<&str> = object.keys().map(String::as_str).collect();
    keys.sort_unstable();
    let mut want = RUN_KEYS.to_vec();
    want.sort_unstable();
    if keys != want {
        fail("run keys differ from arc.proof-result.v1");
    }
    if run["backend"] != PROOF_BACKEND {
        fail("backend is not gpu-wgpu");
    }
    for key in ["isa", "threads", "vector_projections"] {
        if !run[key].is_null() {
            fail(&format!("{key} must be null for a GPU run"));
        }
    }
    let verdict = run["verdict"].as_str();
    if !matches!(verdict, Some("MATCH" | "MISMATCH")) {
        fail("verdict is not MATCH or MISMATCH");
    }
    for key in ["golden_digest", "challenge_digest"] {
        if !run[key].as_str().is_some_and(is_hex64) {
            fail(&format!("{key} is not 64 lower-case hex"));
        }
    }
    for key in ["prefill_tok_s", "decode_tok_s"] {
        if !run[key]
            .as_f64()
            .is_some_and(|x| x.is_finite() && (0.0..=100_000.0).contains(&x))
        {
            fail(&format!("{key} is not a number in [0, 100000]"));
        }
    }
    let adapter = &run["adapter"];
    match adapter.as_object() {
        None => fail("adapter must be an object for a GPU run"),
        Some(fields) => {
            let mut names: Vec<&str> = fields.keys().map(String::as_str).collect();
            names.sort_unstable();
            if names != ["backend", "device", "driver", "vendor"] {
                fail("adapter keys differ from {vendor, device, backend, driver}");
            }
            for key in ["vendor", "device", "driver"] {
                if !(adapter[key].is_null() || adapter[key].as_str().is_some_and(is_label)) {
                    fail(&format!("adapter.{key} is not a label or null"));
                }
            }
            let backend = &adapter["backend"];
            if !(backend.is_null()
                || backend
                    .as_str()
                    .is_some_and(|b| super::PROOF_GPU_APIS.contains(&b)))
            {
                fail("adapter.backend is not vulkan, metal, dx12, gl or null");
            }
        }
    }
    let divergence = &run["divergence"];
    match (verdict, divergence.is_null()) {
        (Some("MATCH"), false) => fail("divergence must be null when the run matches"),
        (Some("MISMATCH"), true) => fail("divergence must be an object on a mismatch"),
        _ => {}
    }
    if let Some(d) = divergence.as_object() {
        let mut names: Vec<&str> = d.keys().map(String::as_str).collect();
        names.sort_unstable();
        if names != ["case", "layer", "op", "position"] {
            fail("divergence keys differ from {case, position, layer, op}");
        }
        let case_ok = divergence["case"].is_null()
            || divergence["case"]
                .as_str()
                .is_some_and(|c| c == "challenge" || GOLDEN_CASE_IDS.contains(&c));
        if !case_ok {
            fail("divergence.case is not a golden case id or challenge");
        }
        for (key, max) in [("position", 65_535u64), ("layer", 1023)] {
            let v = &divergence[key];
            if !(v.is_null() || v.as_u64().is_some_and(|n| n <= max)) {
                fail(&format!("divergence.{key} is out of range"));
            }
        }
        if !(divergence["op"].is_null()
            || divergence["op"]
                .as_str()
                .is_some_and(|op| PROOF_OPS.contains(&op)))
        {
            fail("divergence.op is not a Proof Kit op");
        }
    }
    problems
}

#[cfg(test)]
mod tests {
    use super::*;

    fn apple() -> AdapterReport {
        AdapterReport {
            index: 0,
            name: "Apple M2 Ultra".into(),
            vendor_id: 0,
            vendor: "Apple".into(),
            device_id: 0,
            device_type: "IntegratedGpu".into(),
            backend: "Metal".into(),
            driver: String::new(),
            driver_info: String::new(),
            software: false,
        }
    }

    const CHALLENGE: &str = "fad7f4483e2092bd21f669fad7bc70e6f203792a81ddce440026917ffaba4f6c";

    fn run(golden: &str, challenge: &str) -> Value {
        let adapter = apple();
        gpu_run_entry(&GpuProofRun {
            adapter: &adapter,
            golden_digest: golden.into(),
            challenge_digest: challenge.into(),
            reference_challenge_digest: CHALLENGE.into(),
            prefill_tok_s: 38.968_857_7,
            decode_tok_s: 22.346_495_9,
            divergence: None,
        })
    }

    #[test]
    fn a_matching_gpu_run_is_a_valid_match_entry() {
        let entry = run(PUBLISHED_GOLDEN_DIGEST, CHALLENGE);
        assert_eq!(check_run_entry(&entry), Vec::<String>::new());
        assert_eq!(
            entry,
            json!({
                "backend": "gpu-wgpu",
                "isa": null,
                "verdict": "MATCH",
                "golden_digest": PUBLISHED_GOLDEN_DIGEST,
                "challenge_digest": CHALLENGE,
                "prefill_tok_s": 38.969,
                "decode_tok_s": 22.346,
                "threads": null,
                "vector_projections": null,
                "adapter": {"vendor": "apple", "device": "Apple M2 Ultra", "backend": "metal", "driver": null},
                "divergence": null,
            })
        );
        let keys: Vec<&String> = entry.as_object().unwrap().keys().collect();
        assert_eq!(keys.len(), RUN_KEYS.len());
    }

    #[test]
    fn a_differing_digest_is_a_mismatch_with_a_divergence() {
        let other = "ab".repeat(32);
        let golden = run(&other, CHALLENGE);
        assert_eq!(golden["verdict"], "MISMATCH");
        assert_eq!(
            golden["divergence"],
            json!({"case": null, "position": null, "layer": null, "op": null})
        );
        assert!(check_run_entry(&golden).is_empty());
        let challenge = run(PUBLISHED_GOLDEN_DIGEST, &other);
        assert_eq!(challenge["verdict"], "MISMATCH");
        // A located divergence maps the trace op to the Proof Kit's names.
        let adapter = apple();
        let located = gpu_run_entry(&GpuProofRun {
            adapter: &adapter,
            golden_digest: other.clone(),
            challenge_digest: CHALLENGE.into(),
            reference_challenge_digest: CHALLENGE.into(),
            prefill_tok_s: 1.0,
            decode_tok_s: 1.0,
            divergence: Some(ProofDivergence::from_trace("haiku", 17, "layer12.gate")),
        });
        assert_eq!(
            located["divergence"],
            json!({"case": "haiku", "position": 17, "layer": 12, "op": "w_gate"})
        );
        assert!(check_run_entry(&located).is_empty());
    }

    #[test]
    fn every_trace_op_maps_to_a_proof_kit_op() {
        for (trace, layer, op) in [
            ("embed", None, "embed"),
            ("layer0.attn_norm", Some(0), "attn_norm"),
            ("layer3.q", Some(3), "wq"),
            ("layer3.k", Some(3), "wk"),
            ("layer3.v", Some(3), "wv"),
            ("layer3.q_rope", Some(3), "rope_q"),
            ("layer3.k_rope", Some(3), "rope_k"),
            ("layer3.attention", Some(3), "attention"),
            ("layer3.o_proj", Some(3), "wo"),
            ("layer3.attn_residual", Some(3), "attn_residual"),
            ("layer3.ffn_norm", Some(3), "ffn_norm"),
            ("layer35.gate", Some(35), "w_gate"),
            ("layer35.up", Some(35), "w_up"),
            ("layer35.silu", Some(35), "silu"),
            ("layer35.down", Some(35), "w_down"),
            ("layer35.ffn_residual", Some(35), "ffn_residual"),
            ("final_norm", None, "final_norm"),
            ("logits", None, "lm_head"),
        ] {
            assert_eq!(proof_op(trace), (layer, Some(op)), "{trace}");
            assert!(PROOF_OPS.contains(&op));
        }
        assert_eq!(proof_op("GPU refused: residual beyond 2^62"), (None, None));
    }

    #[test]
    fn the_checker_rejects_what_proof_kit_rejects() {
        let good = run(PUBLISHED_GOLDEN_DIGEST, CHALLENGE);
        let broken: [(&str, Value); 7] = [
            ("threads", json!(4)),
            ("backend", json!("cpu-scalar")),
            ("golden_digest", json!("ABC")),
            ("prefill_tok_s", json!(100_001.0)),
            (
                "divergence",
                json!({"case": "x", "position": 0, "layer": 0, "op": "wq"}),
            ),
            (
                "adapter",
                json!({"vendor": "", "device": null, "backend": "cuda", "driver": null}),
            ),
            ("verdict", json!("PASS")),
        ];
        for (key, value) in broken {
            let mut entry = good.clone();
            entry[key] = value;
            assert!(!check_run_entry(&entry).is_empty(), "{key} accepted");
        }
        let mut extra = good.clone();
        extra["gpu"] = json!(true);
        assert!(!check_run_entry(&extra).is_empty());
    }
}
