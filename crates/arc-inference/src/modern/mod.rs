pub mod arith;
pub mod bpe;
pub mod chat;
pub mod convert;
pub mod model;
pub mod package;
pub mod safetensors;
pub mod tables;

pub const PROFILE: &str = "arc.hf-llama.i8-dyadic-row.q16.v1";
pub const GENERATION_RP64: &str = "arc.hf-chat.no-bos.rp64-argmax.le-u32.v1";
pub const GENERATION_ARGMAX: &str = "arc.hf-chat.no-bos.argmax.le-u32.v1";
pub const TOKENIZER_IDENTITY: &str = "arc.hf-tokenizers.bytelevel-bpe.v1";

#[derive(Debug, thiserror::Error)]
pub enum ModernError {
    #[error("out of the profile's domain: {0}")]
    Domain(String),
    #[error("invalid input: {0}")]
    Invalid(String),
    #[error("i/o failure: {0}")]
    Io(String),
}

impl ModernError {
    pub(crate) fn io(context: &str, error: std::io::Error) -> Self {
        Self::Io(format!("{context}: {error}"))
    }
}

pub fn hex_lower(bytes: &[u8]) -> String {
    hex::encode(bytes)
}

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
