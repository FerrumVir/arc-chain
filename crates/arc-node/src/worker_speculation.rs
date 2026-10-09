//! Opt-in lossless speculative decoding for community jobs (`--speculative`).
//!
//! The worker's contract is `CachedIntegerModel::try_generate`. With
//! speculation on, [`generate_community_job`] runs the same contract through
//! `try_generate_speculative`, which returns the same tokens and output hash
//! for any drafter: the target still chooses every token, and drafts only
//! decide how many positions one target pass verifies. The result body, the
//! signed attestation and the engine label are therefore byte-identical with
//! the flag on or off, and validators recompute without speculation.

use arc_crypto::Hash256;
use arc_inference::cached_integer_model::{
    CachedIntegerModel, GenerationError, load_cached_model_binary, load_cached_model_canonical_i8,
};
use arc_inference::speculative::{
    DraftModelDrafter, Drafter, GenerationSemantics, MAX_DRAFT_LIMIT, NgramDrafter,
    SpeculativeConfig, SpeculativeStats, check_draft_compatible,
};
use std::fmt;
use std::path::PathBuf;
use std::str::FromStr;
use std::sync::Arc;

/// The drafter named by `--speculative`.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum SpeculativeDrafterSpec {
    /// `ngram`: prompt-lookup drafting from the job's own tokens; no weights.
    Ngram,
    /// `draft:PATH`: a small model that shares the worker model's tokenizer,
    /// as an `.arc-int8` cache or a GGUF file.
    DraftModel(PathBuf),
}

impl FromStr for SpeculativeDrafterSpec {
    type Err = String;

    fn from_str(value: &str) -> Result<Self, Self::Err> {
        if value == "ngram" {
            return Ok(Self::Ngram);
        }
        match value.strip_prefix("draft:") {
            Some(path) if !path.trim().is_empty() => Ok(Self::DraftModel(PathBuf::from(path))),
            _ => Err(format!("expected `ngram` or `draft:<path>`, got {value:?}")),
        }
    }
}

impl fmt::Display for SpeculativeDrafterSpec {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Ngram => f.write_str("ngram"),
            Self::DraftModel(path) => write!(f, "draft:{}", path.display()),
        }
    }
}

#[derive(Clone)]
enum WorkerDrafter {
    Ngram,
    DraftModel(Arc<CachedIntegerModel>),
}

/// A loaded `--speculative` setting, shared by every job the worker runs.
/// Each job gets a fresh drafter, so no state crosses jobs.
#[derive(Clone)]
pub struct WorkerSpeculation {
    drafter: WorkerDrafter,
    config: SpeculativeConfig,
}

impl WorkerSpeculation {
    /// N-gram drafting with up to `max_draft` guesses per pass.
    pub fn ngram(max_draft: usize) -> Result<Self, String> {
        Ok(Self {
            drafter: WorkerDrafter::Ngram,
            config: Self::config(max_draft)?,
        })
    }

    /// Draft with an already loaded model, after checking that it shares the
    /// target's tokenizer.
    pub fn with_draft_model(
        target: &CachedIntegerModel,
        draft: Arc<CachedIntegerModel>,
        max_draft: usize,
    ) -> Result<Self, String> {
        check_draft_compatible(target, &draft)?;
        Ok(Self {
            drafter: WorkerDrafter::DraftModel(draft),
            config: Self::config(max_draft)?,
        })
    }

    /// Load what `spec` names and check it against the worker's model.
    pub fn load(
        spec: &SpeculativeDrafterSpec,
        max_draft: usize,
        target: &CachedIntegerModel,
    ) -> Result<Self, String> {
        match spec {
            SpeculativeDrafterSpec::Ngram => Self::ngram(max_draft),
            SpeculativeDrafterSpec::DraftModel(path) => {
                Self::config(max_draft)?;
                let display = path.display();
                let source = path
                    .to_str()
                    .ok_or_else(|| format!("draft model path {display} is not valid UTF-8"))?;
                let loaded = if source.ends_with(".arc-int8") {
                    load_cached_model_binary(source)
                } else {
                    load_cached_model_canonical_i8(source)
                };
                let mut draft = loaded
                    .map_err(|error| format!("cannot load draft model {display}: {error}"))?;
                // Drafts never reach a result, but one resident copy of the
                // weights keeps the drafter's memory to its I8 size.
                draft.enforce_canonical_i8_profile();
                Self::with_draft_model(target, Arc::new(draft), max_draft)
            }
        }
    }

    fn config(max_draft: usize) -> Result<SpeculativeConfig, String> {
        if max_draft == 0 || max_draft > MAX_DRAFT_LIMIT {
            return Err(format!(
                "the draft length must be between 1 and {MAX_DRAFT_LIMIT}, got {max_draft}"
            ));
        }
        Ok(SpeculativeConfig::with_max_draft(max_draft))
    }

    /// `ngram` or `draft-model`, for logs.
    pub fn label(&self) -> &'static str {
        match self.drafter {
            WorkerDrafter::Ngram => "ngram",
            WorkerDrafter::DraftModel(_) => "draft-model",
        }
    }

    /// Most guesses one target pass verifies (`k`).
    pub fn max_draft(&self) -> usize {
        self.config.max_draft
    }

    fn new_drafter(&self) -> Box<dyn Drafter> {
        match &self.drafter {
            WorkerDrafter::Ngram => Box::new(NgramDrafter::new(
                arc_inference::speculative::DEFAULT_MIN_NGRAM,
                arc_inference::speculative::DEFAULT_MAX_NGRAM,
            )),
            WorkerDrafter::DraftModel(model) => Box::new(DraftModelDrafter::new(Arc::clone(model))),
        }
    }
}

/// The outcome of one community job.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct CommunityJobOutput {
    pub tokens: Vec<u32>,
    /// BLAKE3 of the little-endian token ids: the value the worker submits
    /// and signs, and the value validators recompute.
    pub output_hash: Hash256,
    /// Present when the job ran speculatively. Local diagnostics only; never
    /// part of a result body or attestation.
    pub speculation: Option<SpeculativeStats>,
}

/// Run one community job: `try_generate` with the model's EOS tokens, or its
/// speculative twin when `speculation` is set. Both return the same tokens
/// and hash, and the same typed error for a request that does not fit.
pub fn generate_community_job(
    model: &CachedIntegerModel,
    prompt: &[u32],
    max_tokens: u32,
    speculation: Option<&WorkerSpeculation>,
) -> Result<CommunityJobOutput, GenerationError> {
    let eos_tokens = &model.config.eos_tokens;
    match speculation {
        None => {
            let (tokens, output_hash) = model.try_generate(prompt, max_tokens, eos_tokens)?;
            Ok(CommunityJobOutput {
                tokens,
                output_hash,
                speculation: None,
            })
        }
        Some(speculation) => {
            let mut drafter = speculation.new_drafter();
            let out = model.try_generate_speculative(
                prompt,
                max_tokens,
                eos_tokens,
                GenerationSemantics::LegacyV1,
                drafter.as_mut(),
                speculation.config,
            )?;
            Ok(CommunityJobOutput {
                tokens: out.tokens,
                output_hash: out.output_hash,
                speculation: Some(out.stats),
            })
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use arc_inference::speculative::{SyntheticModelSpec, synthetic_canonical_model};

    #[test]
    fn the_flag_accepts_ngram_and_draft_paths_only() {
        assert_eq!(
            "ngram".parse::<SpeculativeDrafterSpec>(),
            Ok(SpeculativeDrafterSpec::Ngram)
        );
        assert_eq!(
            "draft:/opt/arc/tinyllama-1.1b.arc-int8".parse::<SpeculativeDrafterSpec>(),
            Ok(SpeculativeDrafterSpec::DraftModel(PathBuf::from(
                "/opt/arc/tinyllama-1.1b.arc-int8"
            )))
        );
        for bad in [
            "", "draft", "draft:", "draft:  ", "NGRAM", "ngram ", "eagle",
        ] {
            assert!(
                bad.parse::<SpeculativeDrafterSpec>().is_err(),
                "{bad:?} must be refused"
            );
        }
        for text in ["ngram", "draft:models/tinyllama.gguf"] {
            let spec: SpeculativeDrafterSpec = text.parse().unwrap();
            assert_eq!(spec.to_string(), text);
        }
    }

    #[test]
    fn speculative_jobs_are_byte_identical_to_plain_jobs() {
        let target = synthetic_canonical_model(SyntheticModelSpec::tiny(21));
        let other = Arc::new(synthetic_canonical_model(SyntheticModelSpec::tiny(22)));
        let same = Arc::new(synthetic_canonical_model(SyntheticModelSpec::tiny(21)));
        let prompts: [&[u32]; 3] = [
            &[7],
            &[5, 9, 13, 21, 34, 55, 3],
            &[10, 11, 12, 13, 10, 11, 12, 13],
        ];
        for prompt in prompts {
            for max_tokens in [1u32, 16, 40] {
                let plain = generate_community_job(&target, prompt, max_tokens, None).unwrap();
                assert!(plain.speculation.is_none());
                for k in [1usize, 3, 7] {
                    let settings = [
                        WorkerSpeculation::ngram(k).unwrap(),
                        WorkerSpeculation::with_draft_model(&target, Arc::clone(&same), k).unwrap(),
                        WorkerSpeculation::with_draft_model(&target, Arc::clone(&other), k)
                            .unwrap(),
                    ];
                    for speculation in &settings {
                        let case =
                            format!("{prompt:?} max={max_tokens} {} k={k}", speculation.label());
                        let out =
                            generate_community_job(&target, prompt, max_tokens, Some(speculation))
                                .unwrap();
                        // Everything a result body carries is a function of
                        // these tokens: the hash, the count and the text.
                        assert_eq!(out.tokens, plain.tokens, "{case}");
                        assert_eq!(out.output_hash, plain.output_hash, "{case}");
                        assert_eq!(target.decode(&out.tokens), target.decode(&plain.tokens));
                        assert!(out.speculation.is_some(), "{case}");
                    }
                }
            }
        }
    }

    #[test]
    fn requests_that_do_not_fit_fail_the_same_way_with_and_without_speculation() {
        let target = synthetic_canonical_model(SyntheticModelSpec::tiny(23));
        let prompt = [1u32; 500];
        let plain = generate_community_job(&target, &prompt, 12, None).unwrap_err();
        let speculation = WorkerSpeculation::ngram(3).unwrap();
        let speculative =
            generate_community_job(&target, &prompt, 12, Some(&speculation)).unwrap_err();
        assert_eq!(plain, speculative);
    }

    #[test]
    fn bad_draft_lengths_and_incompatible_drafts_are_refused() {
        let target = synthetic_canonical_model(SyntheticModelSpec::tiny(24));
        assert!(WorkerSpeculation::ngram(0).is_err());
        assert!(WorkerSpeculation::ngram(MAX_DRAFT_LIMIT + 1).is_err());
        assert_eq!(
            WorkerSpeculation::ngram(MAX_DRAFT_LIMIT)
                .unwrap()
                .max_draft(),
            MAX_DRAFT_LIMIT
        );

        let wider = Arc::new(synthetic_canonical_model(SyntheticModelSpec {
            vocab_size: 80,
            ..SyntheticModelSpec::tiny(25)
        }));
        assert!(WorkerSpeculation::with_draft_model(&target, wider, 3).is_err());

        let missing = SpeculativeDrafterSpec::DraftModel(PathBuf::from(
            "/nonexistent/arc-speculative-draft.arc-int8",
        ));
        let Err(error) = WorkerSpeculation::load(&missing, 3, &target) else {
            panic!("a missing draft model must be refused");
        };
        assert!(error.contains("cannot load draft model"), "{error}");
        assert!(WorkerSpeculation::load(&missing, 0, &target).is_err());
    }
}
