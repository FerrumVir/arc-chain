//! Real-model execution must not start without an explicit, identity-bound
//! reference-qualification decision.
//!
//! Startup previously constructed `CanonicalI8Qualification` with
//! `reference_generation_qualified: true` unconditionally, beside a comment
//! claiming it could not make that decision. The executor's own check is
//! useless when its caller always passes true, so this exercises the ACTUAL
//! startup path by spawning the binary — a test of the resolver alone would not
//! show that startup calls it.
//!
//! These cases load no model. Each one must be refused before the artifact is
//! hashed, which is why the artifact path below deliberately does not exist:
//! if a case ever reports an artifact error instead of a qualification error,
//! the ordering has regressed.

use arc_crypto::signature::KeyPair;
use arc_crypto::{Hash256, hash_bytes};
use std::path::{Path, PathBuf};
use std::process::Command;

fn validator_keypair(seed: &str) -> KeyPair {
    let bytes = blake3::derive_key("ARC-chain-validator-keypair-v1", seed.as_bytes());
    KeyPair::Ed25519(ed25519_dalek::SigningKey::from_bytes(&bytes))
}

struct Fixture {
    dir: tempfile::TempDir,
    genesis: PathBuf,
    activation: PathBuf,
    allowed: [Hash256; 4],
    seed: String,
}

impl Fixture {
    fn new(tag: &str) -> Self {
        let dir = tempfile::tempdir().unwrap();
        let validator = validator_keypair(tag);
        let m = hash_bytes(format!("{tag}-model").as_bytes());
        let p = hash_bytes(format!("{tag}-profile").as_bytes());
        let g = hash_bytes(format!("{tag}-generation").as_bytes());
        let a = hash_bytes(format!("{tag}-assignment").as_bytes());

        let genesis = dir.path().join("genesis.toml");
        std::fs::write(
            &genesis,
            format!(
                "[chain]\nname = \"arc-qual-gate\"\nchain_id = \"0x415243\"\n\
                 validator_set_complete = false\ninstance_id = \"{instance}\"\n\n\
                 [[accounts]]\naddress = \"{v}\"\nbalance = 1_000_000_000_000\n\n\
                 [[validators]]\naddress = \"{v}\"\nstake = 6666667\n",
                instance = chain_instance_id(),
                v = validator.address().to_hex()
            ),
        )
        .unwrap();

        let activation = dir.path().join("activation.json");
        std::fs::write(
            &activation,
            serde_json::json!({
                "recovery_epoch": 0,
                "allowed_executions": [{
                    "model_hash": m.to_hex(), "profile_hash": p.to_hex(),
                    "generation_hash": g.to_hex(), "assignment_hash": a.to_hex(),
                }]
            })
            .to_string(),
        )
        .unwrap();

        Self {
            dir,
            genesis,
            activation,
            allowed: [m, p, g, a],
            seed: tag.to_string(),
        }
    }

    fn write_qualification(&self, name: &str, value: serde_json::Value) -> PathBuf {
        let path = self.dir.path().join(name);
        std::fs::write(&path, value.to_string()).unwrap();
        path
    }

    /// Start the node with the real-executor path selected and return its
    /// combined output. Bounded: every case here must refuse and exit.
    fn run_real_executor(&self, port: u16, p2p: u16, qualification: Option<&Path>) -> String {
        let data = self.dir.path().join(format!("data-{port}"));
        std::fs::create_dir_all(&data).unwrap();
        // Deliberately absent. A qualification refusal must happen first.
        let missing_artifact = self.dir.path().join("this-artifact-does-not-exist.gguf");
        assert!(!missing_artifact.exists());

        let mut cmd = Command::new(env!("CARGO_BIN_EXE_arc-node"));
        cmd.args([
            "--rpc",
            &format!("127.0.0.1:{port}"),
            "--p2p-port",
            &p2p.to_string(),
            "--data-dir",
            data.to_str().unwrap(),
            "--genesis",
            self.genesis.to_str().unwrap(),
            "--insecure-dev-validator-seed",
            "--validator-seed",
            &self.seed,
            "--stake",
            "6666667",
            "--native-inference-activation",
            self.activation.to_str().unwrap(),
            "--native-inference-runtime",
            "--native-inference-artifact",
            missing_artifact.to_str().unwrap(),
        ]);
        if let Some(q) = qualification {
            cmd.args(["--native-inference-qualification", q.to_str().unwrap()]);
        }
        let out = cmd.output().expect("arc-node must run");
        assert!(
            !out.status.success(),
            "startup must REFUSE real-model execution in this case, but it exited successfully"
        );
        format!(
            "{}{}",
            String::from_utf8_lossy(&out.stdout),
            String::from_utf8_lossy(&out.stderr)
        )
    }
}

#[test]
fn real_execution_is_refused_without_a_qualification_record() {
    let fx = Fixture::new("qual-gate-missing");
    let out = fx.run_real_executor(9971, 9171, None);
    assert!(
        out.contains("explicit reference-qualification record"),
        "must name the missing decision; got:\n{out}"
    );
    // Ordering: the absent artifact must NOT be what failed.
    assert!(
        !out.contains("this-artifact-does-not-exist"),
        "qualification must be rejected BEFORE the artifact is touched; got:\n{out}"
    );
}

#[test]
fn real_execution_is_refused_when_the_record_says_not_qualified() {
    let fx = Fixture::new("qual-gate-false");
    let q = fx.write_qualification(
        "not-qualified.json",
        serde_json::json!({
            "model_hash": fx.allowed[0].to_hex(),
            "profile_hash": fx.allowed[1].to_hex(),
            "generation_hash": fx.allowed[2].to_hex(),
            "reference_generation_qualified": false,
            "decided_by": "release engineering",
            "decided_at": "2026-09-20",
            "evidence": "reference comparison not yet run",
        }),
    );
    let out = fx.run_real_executor(9972, 9172, Some(&q));
    assert!(
        out.contains("reference_generation_qualified = false"),
        "must refuse a negative decision; got:\n{out}"
    );
}

#[test]
fn real_execution_is_refused_for_a_qualification_bound_to_another_identity() {
    let fx = Fixture::new("qual-gate-identity");
    let other = hash_bytes(b"a different model entirely");
    let q = fx.write_qualification(
        "wrong-identity.json",
        serde_json::json!({
            // Affirmative and complete, but for a model this activation does
            // not allow. A decision is valid only for what it was made against.
            "model_hash": other.to_hex(),
            "profile_hash": fx.allowed[1].to_hex(),
            "generation_hash": fx.allowed[2].to_hex(),
            "reference_generation_qualified": true,
            "decided_by": "release engineering",
            "decided_at": "2026-09-20",
            "evidence": "reference outputs for a different artifact",
        }),
    );
    let out = fx.run_real_executor(9973, 9173, Some(&q));
    assert!(
        out.contains("bound to a different execution identity") && out.contains("model_hash"),
        "must refuse and name the mismatching field; got:\n{out}"
    );
}

#[test]
fn real_execution_is_refused_when_the_decision_has_no_author_or_basis() {
    let fx = Fixture::new("qual-gate-incomplete");
    let q = fx.write_qualification(
        "incomplete.json",
        serde_json::json!({
            "model_hash": fx.allowed[0].to_hex(),
            "profile_hash": fx.allowed[1].to_hex(),
            "generation_hash": fx.allowed[2].to_hex(),
            "reference_generation_qualified": true,
        }),
    );
    let out = fx.run_real_executor(9974, 9174, Some(&q));
    assert!(
        out.contains("decided_by") && out.contains("evidence"),
        "an unattributed decision must be refused; got:\n{out}"
    );
}

/// A fresh chain-run identity for every execution of this test. The genesis
/// is otherwise byte-identical run to run, so without it the certificates of
/// one run would verify against the next one's committee.
fn chain_instance_id() -> String {
    format!(
        "{}-{}",
        std::process::id(),
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_nanos()
    )
}
