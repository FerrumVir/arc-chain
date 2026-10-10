//! Exact speculative decoding with an external drafter
//! (`crate::draft_verify`): whatever a drafter proposes, the output (tokens
//! and output hash) equals plain exact decoding, on both arithmetic profiles,
//! both generation contracts, both projection kernels and every stage split,
//! and the caches it leaves hold exactly plain decoding's rows.

use super::{build_fixture_model, fixture};
use crate::cached_integer_model::{
    CANONICAL_REWARD_INFERENCE_PROFILE, CachedIntegerModel,
    GGUF_INTERLEAVED_ROPE_I8_INFERENCE_PROFILE, KVCache,
};
use crate::canonical_simd;
use crate::draft_verify::{
    DraftError, DraftPolicy, DraftVerifyConfig, DraftVerifyError, ExactSemantics, NoDrafter,
    TokenDrafter, Verifier, generate_with_drafter,
};
use std::sync::MutexGuard;

/// Positions in the widened fixture model: room for a 20-token prompt and
/// 48 generated tokens.
const WIDE_MAX_SEQ: usize = 96;
const MAX_TOKENS: u32 = 48;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Profile {
    LegacySplitHalf,
    GgufInterleaved,
}

const PROFILES: [Profile; 2] = [Profile::LegacySplitHalf, Profile::GgufInterleaved];
const SEMANTICS: [ExactSemantics; 2] = [ExactSemantics::Worker, ExactSemantics::V2];

/// The KAT model with a longer context window (the RoPE tables are the only
/// thing `max_seq` changes) in `profile`.
fn wide_model(profile: Profile) -> CachedIntegerModel {
    let mut recipe = fixture();
    recipe.max_seq = WIDE_MAX_SEQ;
    let mut model = build_fixture_model(&recipe);
    let identity = match profile {
        Profile::LegacySplitHalf => CANONICAL_REWARD_INFERENCE_PROFILE,
        Profile::GgufInterleaved => {
            model
                .canonicalize_gguf_interleaved_rope_rows()
                .expect("the fixture is a complete canonical I8 model");
            GGUF_INTERLEAVED_ROPE_I8_INFERENCE_PROFILE
        }
    };
    assert_eq!(model.canonical_execution_profile(), Some(identity));
    model
}

fn prompts() -> Vec<Vec<u32>> {
    let mut long = Vec::with_capacity(20);
    let mut state = 0x2545_f491_u32;
    for _ in 0..20 {
        state = state.wrapping_mul(1_664_525).wrapping_add(1_013_904_223);
        long.push((state >> 16) % 23);
    }
    vec![fixture().generation_prompt, long]
}

/// Holds the kernel switch for a test and runs work under each kernel this
/// CPU has, restoring the previous choice afterwards (also on a panic).
struct KernelLegs {
    _lock: MutexGuard<'static, ()>,
    previous: bool,
}

impl KernelLegs {
    fn hold() -> Self {
        let lock = canonical_simd::kernel_switch_guard();
        let previous = canonical_simd::fast_canonical_kernel_enabled();
        Self {
            _lock: lock,
            previous,
        }
    }

    fn each(&self, mut work: impl FnMut(&str)) {
        let mut legs = vec![(false, "scalar")];
        if canonical_simd::dotprod_available() {
            legs.push((true, "vectorised"));
        }
        for (vectorised, name) in legs {
            canonical_simd::set_fast_canonical_kernel(vectorised);
            work(name);
        }
        canonical_simd::set_fast_canonical_kernel(self.previous);
    }
}

impl Drop for KernelLegs {
    fn drop(&mut self) {
        canonical_simd::set_fast_canonical_kernel(self.previous);
    }
}

/// Plain exact decoding: the production contract the run must equal.
fn plain(
    model: &CachedIntegerModel,
    semantics: ExactSemantics,
    prompt: &[u32],
    eos: &[u32],
) -> (Vec<u32>, String) {
    let (tokens, hash) = match semantics {
        ExactSemantics::Worker => model.try_generate(prompt, MAX_TOKENS, eos),
        ExactSemantics::V2 => model.try_generate_v2(prompt, MAX_TOKENS, eos),
    }
    .expect("the generation fits the widened window");
    (tokens, hex::encode(hash.0))
}

/// The tokens a drafter must be shown before it proposes the token at
/// `generated` (the exact prefix, ending with the pending token).
fn expected_stream(
    model: &CachedIntegerModel,
    semantics: ExactSemantics,
    prompt: &[u32],
    generated: &[u32],
) -> Vec<u32> {
    let mut stream = vec![model.config.bos_token];
    stream.extend_from_slice(prompt);
    if semantics == ExactSemantics::Worker {
        stream.push(prompt.last().copied().unwrap_or(0));
    }
    stream.extend_from_slice(generated);
    stream
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Script {
    /// Always proposes plain decoding's continuation.
    Right,
    /// Always proposes a token plain decoding does not choose.
    Wrong,
    /// Right or wrong per slot, from a seeded hash.
    Mixed(u64),
    /// Too many tokens, with an out-of-vocabulary token second.
    Overlong,
    /// Right on the first call, then fails.
    FailsSecond,
}

const SCRIPTS: [Script; 7] = [
    Script::Right,
    Script::Wrong,
    Script::Mixed(1),
    Script::Mixed(2),
    Script::Mixed(3),
    Script::Overlong,
    Script::FailsSecond,
];

/// A drafter that knows plain decoding's output and proposes from it as
/// scripted, checking that it is shown the exact prefix every time.
struct Scripted<'a> {
    script: Script,
    reference: &'a [u32],
    model: &'a CachedIntegerModel,
    semantics: ExactSemantics,
    prompt: &'a [u32],
    vocab: u32,
    calls: usize,
}

impl TokenDrafter for Scripted<'_> {
    fn label(&self) -> &str {
        "scripted"
    }

    fn propose(
        &mut self,
        stream: &[u32],
        generated: &[u32],
        max_draft: usize,
    ) -> Result<Vec<u32>, DraftError> {
        self.calls += 1;
        assert!(max_draft > 0, "asked for no drafts");
        assert_eq!(
            generated,
            &self.reference[..generated.len()],
            "the history is not plain decoding's"
        );
        assert_eq!(
            stream,
            expected_stream(self.model, self.semantics, self.prompt, generated).as_slice(),
            "the drafter was not shown the exact prefix"
        );
        if self.script == Script::FailsSecond && self.calls >= 2 {
            return Err(DraftError("scripted failure".into()));
        }
        let at = generated.len();
        let continuation = &self.reference[at.min(self.reference.len())..];
        let mut drafts: Vec<u32> = continuation
            .iter()
            .take(max_draft)
            .enumerate()
            .map(|(slot, &right)| {
                let wrong = (right + 1) % self.vocab;
                match self.script {
                    Script::Right | Script::Overlong | Script::FailsSecond => right,
                    Script::Wrong => wrong,
                    Script::Mixed(seed) => {
                        let h = (seed ^ ((at as u64) << 20) ^ slot as u64)
                            .wrapping_mul(0x9e37_79b9_7f4a_7c15);
                        if (h >> 61) < 6 { right } else { wrong }
                    }
                }
            })
            .collect();
        if self.script == Script::Overlong {
            drafts.insert(1.min(drafts.len()), self.vocab);
            drafts.extend([0, 1, 2]);
        }
        Ok(drafts)
    }
}

fn policies() -> Vec<DraftPolicy> {
    vec![
        DraftPolicy::OFF,
        DraftPolicy::fixed(1),
        DraftPolicy::fixed(2),
        DraftPolicy::fixed(3),
        DraftPolicy::fixed(7),
        DraftPolicy::MEASURED,
        DraftPolicy {
            min: 1,
            start: 1,
            cap: 15,
            probe_after: 2,
        },
    ]
}

/// Every way to cut `n_layers` layers into contiguous stages, as the
/// exclusive end layer of every stage.
fn stage_splits(n_layers: usize) -> Vec<Vec<usize>> {
    (0..(1usize << (n_layers - 1)))
        .map(|cuts| {
            (1..n_layers)
                .filter(|&layer| cuts & (1 << (layer - 1)) != 0)
                .chain([n_layers])
                .collect()
        })
        .collect()
}

/// Runs one generation and holds it to plain decoding.
#[allow(clippy::too_many_arguments)]
fn check(
    model: &CachedIntegerModel,
    semantics: ExactSemantics,
    prompt: &[u32],
    eos: &[u32],
    reference: &(Vec<u32>, String),
    script: Option<Script>,
    policy: DraftPolicy,
    stage_ends: &[usize],
    leg: &str,
) {
    let config = DraftVerifyConfig {
        semantics,
        stage_ends: stage_ends.to_vec(),
        policy,
    };
    let case = format!(
        "{leg}, {semantics:?}, prompt {prompt:?}, eos {eos:?}, {script:?}, {policy:?}, stages {stage_ends:?}"
    );
    let mut scripted = script.map(|script| Scripted {
        script,
        reference: &reference.0,
        model,
        semantics,
        prompt,
        vocab: u32::try_from(model.config.vocab_size).expect("small vocabulary"),
        calls: 0,
    });
    let output = match scripted.as_mut() {
        None => generate_with_drafter(model, prompt, MAX_TOKENS, eos, &mut NoDrafter, &config),
        Some(drafter) => generate_with_drafter(model, prompt, MAX_TOKENS, eos, drafter, &config),
    }
    .unwrap_or_else(|e| panic!("{case}: {e}"));
    let calls = scripted.as_ref().map_or(0, |drafter| drafter.calls);
    assert_eq!(
        output.tokens, reference.0,
        "{case}: tokens differ from plain decoding"
    );
    assert_eq!(
        hex::encode(output.output_hash.0),
        reference.1,
        "{case}: output hash differs from plain decoding"
    );

    let stats = &output.stats;
    assert!(stats.accepted <= stats.drafted, "{case}: {stats:?}");
    assert_eq!(
        stats.rows_verified,
        stats.drafted + stats.passes,
        "{case}: {stats:?}"
    );
    assert!(stats.rows_rolled_back <= stats.drafted, "{case}: {stats:?}");
    assert_eq!(
        stats.draft_lengths.values().sum::<usize>(),
        stats.passes,
        "{case}: {stats:?}"
    );
    assert!(
        stats
            .draft_lengths
            .keys()
            .all(|&k| k <= policy.cap.max(policy.min)),
        "{case}: a pass exceeded the cap: {stats:?}"
    );
    match script {
        None => assert_eq!(stats.passes, 0, "{case}: {stats:?}"),
        Some(Script::Right) => {
            assert_eq!(stats.accepted, stats.drafted, "{case}: {stats:?}");
            assert!(stats.rows_rolled_back <= 1, "{case}: {stats:?}");
        }
        Some(Script::Wrong) => assert_eq!(stats.accepted, 0, "{case}: {stats:?}"),
        Some(Script::FailsSecond) => {
            assert_eq!(
                stats.drafter_errors,
                usize::from(calls >= 2),
                "{case}: {stats:?}"
            );
            assert!(calls <= 2, "{case}: a failed drafter was asked again");
        }
        _ => {}
    }
    if policy == DraftPolicy::OFF {
        assert_eq!(stats.passes, 0, "{case}: {stats:?}");
        assert_eq!(
            stats.plain_steps,
            output.tokens.len() - usize::from(semantics == ExactSemantics::V2),
            "{case}"
        );
    }
}

/// Every drafter script under every policy on the whole model, and the
/// end-of-sequence and short-prompt cases on a two-stage split, equal plain
/// decoding.
#[test]
fn golden_draft_verify_equals_plain_decoding() {
    let legs = KernelLegs::hold();
    for profile in PROFILES {
        let model = wide_model(profile);
        let n_layers = model.config.n_layers;
        let whole = vec![n_layers];
        let split = vec![n_layers / 2, n_layers];
        let mut prompts = prompts();
        let long = prompts.pop().expect("prompts");
        let short = prompts.pop().expect("prompts");
        legs.each(|kernel| {
            let leg = format!("{profile:?}, {kernel}");
            for semantics in SEMANTICS {
                let reference = plain(&model, semantics, &long, &[]);
                for policy in policies() {
                    check(
                        &model,
                        semantics,
                        &long,
                        &[],
                        &reference,
                        None,
                        policy,
                        &whole,
                        &leg,
                    );
                    for script in SCRIPTS {
                        check(
                            &model,
                            semantics,
                            &long,
                            &[],
                            &reference,
                            Some(script),
                            policy,
                            &whole,
                            &leg,
                        );
                    }
                }

                // An end-of-sequence token plain decoding emits mid-way, so a
                // pass can stop inside an accepted run.
                let eos = [reference.0[reference.0.len() / 2]];
                let stopped = plain(&model, semantics, &long, &eos);
                for policy in [DraftPolicy::fixed(3), DraftPolicy::MEASURED, policies()[6]] {
                    for script in [
                        Script::Right,
                        Script::Mixed(1),
                        Script::Overlong,
                        Script::FailsSecond,
                    ] {
                        check(
                            &model,
                            semantics,
                            &long,
                            &eos,
                            &stopped,
                            Some(script),
                            policy,
                            &split,
                            &leg,
                        );
                    }
                }

                let reference = plain(&model, semantics, &short, &[]);
                for script in [Script::Right, Script::Wrong, Script::Mixed(2)] {
                    check(
                        &model,
                        semantics,
                        &short,
                        &[],
                        &reference,
                        Some(script),
                        DraftPolicy::MEASURED,
                        &split,
                        &leg,
                    );
                }
            }
        });
    }
}

/// Every stage split verifies exactly, with the measured policy.
#[test]
fn golden_draft_verify_every_stage_split() {
    let legs = KernelLegs::hold();
    for profile in PROFILES {
        let model = wide_model(profile);
        legs.each(|kernel| {
            for semantics in SEMANTICS {
                let prompt = prompts().pop().expect("prompts");
                let reference = plain(&model, semantics, &prompt, &[]);
                let leg = format!("{profile:?}, {kernel}");
                for stage_ends in stage_splits(model.config.n_layers) {
                    for script in [Script::Right, Script::Mixed(4)] {
                        check(
                            &model,
                            semantics,
                            &prompt,
                            &[],
                            &reference,
                            Some(script),
                            DraftPolicy::MEASURED,
                            &stage_ends,
                            &leg,
                        );
                    }
                }
            }
        });
    }
}

/// Joins stage caches layer by layer.
fn merged(caches: &[KVCache], n_layers: usize) -> KVCache {
    let mut merged = KVCache::new(n_layers);
    merged.seq_len = caches[0].seq_len;
    for cache in caches {
        assert_eq!(cache.seq_len, merged.seq_len, "stages disagree on seq_len");
        for (layer, (k, v)) in cache.k_data.iter().zip(&cache.v_data).enumerate() {
            if !k.is_empty() {
                merged.k_data[layer].clone_from(k);
                merged.v_data[layer].clone_from(v);
            }
        }
    }
    merged
}

/// After rejected drafts are rolled back, every stage holds exactly the rows
/// plain decoding leaves, byte for byte, and the same forwarded tokens.
#[test]
fn golden_draft_verify_leaves_plain_decodings_cache() {
    for profile in PROFILES {
        let model = wide_model(profile);
        let n_layers = model.config.n_layers;
        let prompt = prompts().pop().expect("prompts");
        let semantics = ExactSemantics::Worker;
        let reference = plain(&model, semantics, &prompt, &[]);
        let plain_config = DraftVerifyConfig {
            semantics,
            stage_ends: vec![n_layers],
            policy: DraftPolicy::OFF,
        };
        let mut baseline = Verifier::new(&model, &plain_config).expect("whole model");
        baseline
            .generate(&prompt, MAX_TOKENS as usize, &[], &mut NoDrafter)
            .expect("plain run");
        let want = merged(baseline.caches(), n_layers);
        assert_eq!(baseline.stream().len(), want.seq_len);
        for stage_ends in stage_splits(n_layers) {
            for script in [Script::Wrong, Script::Mixed(6), Script::Overlong] {
                let config = DraftVerifyConfig {
                    semantics,
                    stage_ends: stage_ends.clone(),
                    policy: DraftPolicy::MEASURED,
                };
                let mut drafter = Scripted {
                    script,
                    reference: &reference.0,
                    model: &model,
                    semantics,
                    prompt: &prompt,
                    vocab: 23,
                    calls: 0,
                };
                let mut run = Verifier::new(&model, &config).expect("valid split");
                run.generate(&prompt, MAX_TOKENS as usize, &[], &mut drafter)
                    .expect("draft run");
                let case = format!("{profile:?}, {script:?}, stages {stage_ends:?}");
                assert_eq!(
                    run.stream(),
                    baseline.stream(),
                    "{case}: forwarded tokens differ"
                );
                let got = merged(run.caches(), n_layers);
                assert_eq!(got.seq_len, want.seq_len, "{case}: seq_len");
                assert!(
                    got.k_data == want.k_data && got.v_data == want.v_data,
                    "{case}: K/V rows differ from plain decoding's"
                );
                assert_eq!(run.finish().tokens, reference.0, "{case}: tokens");
            }
        }
    }
}

/// Stage lists that do not cut the layers into contiguous stages are refused.
#[test]
fn golden_draft_verify_refuses_bad_stage_lists() {
    let model = wide_model(Profile::LegacySplitHalf);
    let n_layers = model.config.n_layers;
    for stage_ends in [
        vec![],
        vec![2],
        vec![2, 2, n_layers],
        vec![3, 1, n_layers],
        vec![n_layers + 1],
    ] {
        let config = DraftVerifyConfig {
            semantics: ExactSemantics::Worker,
            stage_ends: stage_ends.clone(),
            policy: DraftPolicy::MEASURED,
        };
        let refused = generate_with_drafter(&model, &[1, 2], 4, &[], &mut NoDrafter, &config);
        assert!(
            matches!(refused, Err(DraftVerifyError::BadStages { .. })),
            "{stage_ends:?} was not refused: {refused:?}"
        );
    }
}

/// A drafter in another process, speaking the line protocol, through the
/// whole loop: the output is still plain decoding's.
#[cfg(unix)]
#[test]
fn golden_draft_verify_process_drafter_round_trip() {
    use crate::draft_verify::ProcessDrafter;
    use std::process::Command;

    // Proposes [5, 6], or [5] when asked for one token.
    let script = "echo 'READY 23'\n\
        while read -r cmd max rest; do\n\
          case \"$cmd\" in\n\
            PROPOSE) if [ \"$max\" -ge 2 ]; then echo 'OK 7 2 5 6'; else echo 'OK 7 1 5'; fi ;;\n\
            QUIT) exit 0 ;;\n\
            *) echo 'ERR unknown request' ;;\n\
          esac\n\
        done\n";
    let model = wide_model(Profile::GgufInterleaved);
    let prompt = fixture().generation_prompt;
    let reference = plain(&model, ExactSemantics::Worker, &prompt, &[]);
    let mut drafter =
        ProcessDrafter::spawn(Command::new("/bin/sh").arg("-c").arg(script), "sh", 23)
            .expect("the shell drafter starts");
    let config = DraftVerifyConfig {
        semantics: ExactSemantics::Worker,
        stage_ends: vec![2, model.config.n_layers],
        policy: DraftPolicy::fixed(2),
    };
    let output = generate_with_drafter(&model, &prompt, MAX_TOKENS, &[], &mut drafter, &config)
        .expect("the run completes");
    assert_eq!(output.tokens, reference.0);
    assert_eq!(hex::encode(output.output_hash.0), reference.1);
    assert_eq!(output.stats.drafter_errors, 0);
    assert!(output.stats.passes > 0);
    assert_eq!(drafter.reported_micros(), 7 * output.stats.passes as u64);

    let refused = ProcessDrafter::spawn(Command::new("/bin/sh").arg("-c").arg(script), "sh", 32000);
    assert!(refused.is_err(), "a vocabulary mismatch must be refused");
}
