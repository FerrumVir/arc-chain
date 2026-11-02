//! Modern Llama-family models (SmolLM3-3B) on ARC's integer engine.
//!
//! This module implements the additional arithmetic profile
//! `arc.hf-llama.i8-dyadic-row.q16.v1`, specified in
//! `docs/protocol/integer-profile-hf-llama-dyadic-v1.md`. Nothing in consensus,
//! rewards or native inference refers to it: the network's canonical model and
//! its bindings are unchanged. The pieces are:
//!
//! * [`convert`]: BF16 safetensors to the integer package, pure integer
//!   arithmetic, byte-identical on every OS;
//! * [`package`]: the `arc.integer-package.v1` file format and its manifest;
//! * [`model`]: the integer forward pass (GQA, NoPE layers, tied INT8
//!   embedding, i32 KV cache) and generation;
//! * [`arith`] and [`tables`]: the operators and their exact tables;
//! * [`bpe`] and [`chat`]: the byte-level BPE tokenizer and the chat prompt.

pub mod arith;
pub mod bpe;
pub mod chat;
pub mod convert;
pub mod model;
pub mod package;
pub mod safetensors;
pub mod tables;

/// Arithmetic profile implemented by this module.
pub const PROFILE: &str = "arc.hf-llama.i8-dyadic-row.q16.v1";
/// Default generation semantics: no BOS, repetition-penalised argmax.
pub const GENERATION_RP64: &str = "arc.hf-chat.no-bos.rp64-argmax.le-u32.v1";
/// Diagnostic generation semantics: no BOS, plain argmax.
pub const GENERATION_ARGMAX: &str = "arc.hf-chat.no-bos.argmax.le-u32.v1";
/// Tokenizer identity (bound together with the tokenizer file's SHA-256).
pub const TOKENIZER_IDENTITY: &str = "arc.hf-tokenizers.bytelevel-bpe.v1";

/// Why a modern-profile operation refused.
#[derive(Debug, thiserror::Error)]
pub enum ModernError {
    /// An intermediate left the domain in which the profile is defined.
    #[error("out of the profile's domain: {0}")]
    Domain(String),
    /// Malformed or unsupported input (shape, file format, configuration).
    #[error("invalid input: {0}")]
    Invalid(String),
    /// File-system or I/O failure.
    #[error("i/o failure: {0}")]
    Io(String),
}

impl ModernError {
    pub(crate) fn io(context: &str, error: std::io::Error) -> Self {
        Self::Io(format!("{context}: {error}"))
    }
}

/// Lower-case hex of a byte slice.
pub fn hex_lower(bytes: &[u8]) -> String {
    hex::encode(bytes)
}

/// BLAKE3 of an identity string, as hex (profile and semantics commitments).
pub fn identity_blake3(identity: &str) -> String {
    hex_lower(blake3::hash(identity.as_bytes()).as_bytes())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn identity_commitments_match_the_specification() {
        assert_eq!(
            identity_blake3(PROFILE),
            "3eb41a4fe376be020e2b93ff4ca10d55977d113278546ba2ee74622c3db1fa48"
        );
        assert_eq!(
            identity_blake3(GENERATION_RP64),
            "f267f6818464cfaed9afe66eec8b29efd59e7208dde72b711487e73756450fdb"
        );
        assert_eq!(
            identity_blake3(GENERATION_ARGMAX),
            "f04974ecfda896adb86c944b8a1dcd4c9060eec8193f8ecb6db3bac48c3b75a6"
        );
    }
}
