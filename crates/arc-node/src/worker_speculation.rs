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
use std::path::{Path, PathBuf};
use std::str::FromStr;
use std::sync::Arc;

/// A draft model file that `--speculative draft:PATH` accepts, pinned by
/// size and SHA-256.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct PinnedDraftModel {
    /// What the file is, for logs.
    pub name: &'static str,
    /// Exact file size in bytes.
    pub bytes: u64,
    /// Lowercase hex SHA-256 of the whole file.
    pub sha256: &'static str,
}

/// The draft model files the worker accepts. A draft model never changes an
/// output, so this pin is about resources and identity: a known file with a
/// known memory footprint, not whatever a path happens to hold.
pub const PINNED_DRAFT_MODELS: &[PinnedDraftModel] = &[PinnedDraftModel {
    // huggingface.co/TheBloke/TinyLlama-1.1B-Chat-v1.0-GGUF at commit
    // 52e7645ba7c309695bec7ac98f4f005b139cf465, file
    // tinyllama-1.1b-chat-v1.0.Q8_0.gguf: the pin speculative-bench.yml uses.
    name: "TinyLlama-1.1B-Chat-v1.0 Q8_0 GGUF",
    bytes: 1_170_781_568,
    sha256: "a4c9bb1dbaa372f6381a035fa5c02ef087aaa1ff1f843a56a22328114f03fc59",
}];

/// What a draft model adds to the worker's memory.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct DraftMemory {
    /// The loaded draft model's resident weights and tables.
    pub resident_bytes: usize,
    /// Draft K/V cache per position of a job: one K and one V row of `d_kv`
    /// i64 values per layer.
    pub kv_bytes_per_position: usize,
}

/// Check `path` against `pins`: its size must equal a pin's, and the whole
/// file's SHA-256 must then match that pin. The size is checked first, so an
/// unrelated large file is refused without being read.
fn check_pinned_draft<'a>(
    path: &Path,
    pins: &'a [PinnedDraftModel],
) -> Result<&'a PinnedDraftModel, String> {
    let display = path.display();
    let bytes = std::fs::metadata(path)
        .map_err(|error| format!("cannot read draft model {display}: {error}"))?
        .len();
    let Some(pin) = pins.iter().find(|pin| pin.bytes == bytes) else {
        let pinned: Vec<String> = pins
            .iter()
            .map(|pin| format!("{} ({} bytes)", pin.name, pin.bytes))
            .collect();
        return Err(format!(
            "draft model {display} is {bytes} bytes and matches no pinned draft model; pinned: {}",
            pinned.join(", ")
        ));
    };
    let digest =
        sha256_file(path).map_err(|error| format!("cannot hash draft model {display}: {error}"))?;
    if digest != pin.sha256 {
        return Err(format!(
            "draft model {display} has SHA-256 {digest}, not the pinned {} ({})",
            pin.name, pin.sha256
        ));
    }
    Ok(pin)
}

/// Stream a file through SHA-256 with a bounded buffer.
fn sha256_file(path: &Path) -> std::io::Result<String> {
    use sha2::{Digest, Sha256};
    use std::io::Read;

    let mut file = std::fs::File::open(path)?;
    let mut hasher = Sha256::new();
    let mut buffer = vec![0_u8; 1024 * 1024];
    loop {
        let read = file.read(&mut buffer)?;
        if read == 0 {
            break;
        }
        hasher.update(&buffer[..read]);
    }
    Ok(hex::encode(hasher.finalize()))
}

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
    ///
    /// A draft model file must match one of [`PINNED_DRAFT_MODELS`] by size
    /// and SHA-256 before it is loaded.
    ///
    /// Memory: `ngram` keeps about 1 MB of match tables and nothing else. A
    /// draft model stays resident beside the worker model: about 1.6 GB for
    /// the pinned TinyLlama-1.1B in this engine's layout (its i64 embedding
    /// table alone is 32,000 x 2,048 x 8 bytes, about 524 MB), plus about
    /// 88 KiB of draft K/V cache per position of each job.
    /// [`Self::draft_memory`] reports the exact figures for the loaded file.
    pub fn load(
        spec: &SpeculativeDrafterSpec,
        max_draft: usize,
        target: &CachedIntegerModel,
    ) -> Result<Self, String> {
        match spec {
            SpeculativeDrafterSpec::Ngram => Self::ngram(max_draft),
            SpeculativeDrafterSpec::DraftModel(path) => {
                Self::config(max_draft)?;
                check_pinned_draft(path, PINNED_DRAFT_MODELS)?;
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

    /// The memory a draft model adds, or `None` for `ngram`.
    pub fn draft_memory(&self) -> Option<DraftMemory> {
        match &self.drafter {
            WorkerDrafter::Ngram => None,
            WorkerDrafter::DraftModel(model) => Some(DraftMemory {
                resident_bytes: model.memory_bytes(),
                kv_bytes_per_position: model.config.n_layers
                    * 2
                    * model.config.d_kv
                    * std::mem::size_of::<i64>(),
            }),
        }
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
        assert!(error.contains("cannot read draft model"), "{error}");
        assert!(WorkerSpeculation::load(&missing, 0, &target).is_err());
    }

    #[test]
    fn draft_model_files_must_match_a_pin_by_size_and_sha256() {
        use sha2::{Digest, Sha256};
        use std::io::Write;

        let mut file = tempfile::NamedTempFile::new().unwrap();
        file.write_all(b"not really a model").unwrap();
        file.flush().unwrap();
        let path = file.path();
        let digest = hex::encode(Sha256::digest(b"not really a model"));
        let right = [PinnedDraftModel {
            name: "fixture",
            bytes: 18,
            sha256: Box::leak(digest.into_boxed_str()),
        }];
        assert_eq!(check_pinned_draft(path, &right).unwrap().name, "fixture");

        // Same size, different bytes.
        let wrong_hash = [PinnedDraftModel {
            sha256: "00".repeat(32).leak(),
            ..right[0]
        }];
        let error = check_pinned_draft(path, &wrong_hash).unwrap_err();
        assert!(error.contains("not the pinned fixture"), "{error}");

        // A size no pin has is refused before the file is hashed.
        let wrong_size = [PinnedDraftModel {
            bytes: 19,
            ..right[0]
        }];
        let error = check_pinned_draft(path, &wrong_size).unwrap_err();
        assert!(error.contains("matches no pinned draft model"), "{error}");

        // The worker's own pin list refuses any other file, so loading falls
        // back before a model is read.
        let target = synthetic_canonical_model(SyntheticModelSpec::tiny(26));
        let spec = SpeculativeDrafterSpec::DraftModel(path.to_path_buf());
        let Err(error) = WorkerSpeculation::load(&spec, 3, &target) else {
            panic!("an unpinned draft model must be refused");
        };
        assert!(error.contains("matches no pinned draft model"), "{error}");
        assert!(PINNED_DRAFT_MODELS.iter().all(|pin| pin.sha256.len() == 64));
    }

    #[test]
    fn draft_memory_reports_the_loaded_drafter_and_nothing_for_ngram() {
        let target = synthetic_canonical_model(SyntheticModelSpec::tiny(27));
        assert_eq!(WorkerSpeculation::ngram(3).unwrap().draft_memory(), None);
        let draft = Arc::new(synthetic_canonical_model(SyntheticModelSpec::tiny(28)));
        let speculation =
            WorkerSpeculation::with_draft_model(&target, Arc::clone(&draft), 3).unwrap();
        let memory = speculation.draft_memory().unwrap();
        assert_eq!(memory.resident_bytes, draft.memory_bytes());
        // 3 layers x (K + V) x d_kv 32 x 8 bytes.
        assert_eq!(memory.kv_bytes_per_position, 3 * 2 * 32 * 8);
    }
}
