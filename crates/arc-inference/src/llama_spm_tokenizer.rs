//! Versioned, metadata-backed tokenizer for GGUF LLaMA SentencePiece models.
//!
//! This is deliberately narrower than [`CachedIntegerModel::encode`]: it
//! implements llama.cpp's historical SPM score-ordered pair merge using the
//! GGUF vocabulary, scores, token types, and byte fallback table.  Callers
//! must bind this profile in their generation identity; it is not a silent
//! replacement for the legacy greedy encoder.

use std::collections::HashMap;

use crate::InferenceError;

/// Identity for the LLaMA-only GGUF tokenizer implemented in this module.
pub const GGUF_LLAMA_SPM_TOKENIZER_PROFILE_V1: &str =
    arc_types::transaction::GGUF_LLAMA_SPM_TOKENIZER_PROFILE_V1;

#[cfg(feature = "candle")]
const TOKEN_TYPE_UNKNOWN: i32 = 2;
const TOKEN_TYPE_CONTROL: i32 = 3;
#[cfg(feature = "candle")]
const TOKEN_TYPE_USER_DEFINED: i32 = 4;
#[cfg(feature = "candle")]
const TOKEN_TYPE_BYTE: i32 = 6;

/// Header-only LLaMA GGUF tokenizer.  It does not read tensor weights.
#[derive(Debug, Clone)]
pub struct LlamaGgufSpmTokenizer {
    bos_token: u32,
    eos_tokens: Vec<u32>,
    pieces: Vec<String>,
    scores: Vec<f32>,
    types: Vec<i32>,
    ids: HashMap<String, u32>,
    byte_ids: [Option<u32>; 256],
    specials: Vec<(String, u32)>,
}

#[derive(Debug, Clone)]
struct Symbol {
    bytes: Vec<u8>,
    prev: Option<usize>,
    next: Option<usize>,
    active: bool,
}

impl LlamaGgufSpmTokenizer {
    /// Read only GGUF header metadata and reject every tokenizer model other
    /// than the score-merge LLaMA SPM contract.
    #[cfg(feature = "candle")]
    pub fn from_gguf(path: &str) -> Result<Self, InferenceError> {
        use candle_core::quantized::gguf_file;

        let mut reader = std::fs::File::open(path)
            .map_err(|error| InferenceError::Runtime(format!("open GGUF tokenizer: {error}")))?;
        let content = gguf_file::Content::read(&mut reader).map_err(|error| {
            InferenceError::Runtime(format!("read GGUF tokenizer metadata: {error}"))
        })?;

        match content.metadata.get("general.architecture") {
            Some(gguf_file::Value::String(architecture)) if architecture == "llama" => {}
            Some(gguf_file::Value::String(architecture)) => {
                return Err(InferenceError::Runtime(format!(
                    "GGUF SPM tokenizer profile requires general.architecture=llama, found {architecture:?}"
                )));
            }
            _ => {
                return Err(InferenceError::Runtime(
                    "GGUF missing general.architecture".into(),
                ));
            }
        }
        match content.metadata.get("tokenizer.ggml.model") {
            Some(gguf_file::Value::String(model)) if model == "llama" => {}
            Some(gguf_file::Value::String(model)) => {
                return Err(InferenceError::Runtime(format!(
                    "GGUF SPM tokenizer profile requires tokenizer.ggml.model=llama, found {model:?}"
                )));
            }
            _ => {
                return Err(InferenceError::Runtime(
                    "GGUF missing tokenizer.ggml.model".into(),
                ));
            }
        }

        let pieces = required_strings(&content, "tokenizer.ggml.tokens")?;
        let scores = required_scores(&content, "tokenizer.ggml.scores")?;
        let types = required_i32s(&content, "tokenizer.ggml.token_type")?;
        if scores.len() != pieces.len() || types.len() != pieces.len() {
            return Err(InferenceError::Runtime(format!(
                "GGUF tokenizer metadata lengths differ: tokens={}, scores={}, types={}",
                pieces.len(),
                scores.len(),
                types.len()
            )));
        }
        let bos_token = match content.metadata.get("tokenizer.ggml.bos_token_id") {
            Some(gguf_file::Value::U32(value)) => *value,
            Some(gguf_file::Value::U64(value)) => *value as u32,
            _ => {
                return Err(InferenceError::Runtime(
                    "GGUF missing tokenizer.ggml.bos_token_id".into(),
                ));
            }
        };
        if bos_token as usize >= pieces.len() {
            return Err(InferenceError::Runtime(
                "GGUF BOS token is outside tokenizer vocabulary".into(),
            ));
        }
        let eos_tokens = required_token_ids(&content, "tokenizer.ggml.eos_token_id")?;
        Self::from_parts(bos_token, eos_tokens, pieces, scores, types)
    }

    #[cfg(not(feature = "candle"))]
    pub fn from_gguf(_path: &str) -> Result<Self, InferenceError> {
        Err(InferenceError::Runtime("candle feature not enabled".into()))
    }

    #[cfg(feature = "candle")]
    fn from_parts(
        bos_token: u32,
        eos_tokens: Vec<u32>,
        pieces: Vec<String>,
        scores: Vec<f32>,
        types: Vec<i32>,
    ) -> Result<Self, InferenceError> {
        if pieces.len() != scores.len() || pieces.len() != types.len() {
            return Err(InferenceError::Runtime(
                "SPM tokenizer metadata lengths differ".into(),
            ));
        }
        let mut ids = HashMap::with_capacity(pieces.len());
        let mut byte_ids = [None; 256];
        let mut specials = Vec::new();
        for (index, (piece, token_type)) in pieces.iter().zip(&types).enumerate() {
            let id = index as u32;
            if ids.insert(piece.clone(), id).is_some() {
                return Err(InferenceError::Runtime(format!(
                    "GGUF tokenizer has a duplicate piece at ID {id}: {piece:?}"
                )));
            }
            if *token_type == TOKEN_TYPE_BYTE {
                let Some(byte) = parse_byte_piece(piece) else {
                    return Err(InferenceError::Runtime(format!(
                        "GGUF byte token {id} is not <0xHH>: {piece:?}"
                    )));
                };
                byte_ids[byte as usize] = Some(id);
            }
            if matches!(
                *token_type,
                TOKEN_TYPE_CONTROL | TOKEN_TYPE_USER_DEFINED | TOKEN_TYPE_UNKNOWN
            ) {
                specials.push((piece.clone(), id));
            }
        }
        specials.sort_by(|left, right| right.0.len().cmp(&left.0.len()).then(left.1.cmp(&right.1)));
        Ok(Self {
            bos_token,
            eos_tokens,
            pieces,
            scores,
            types,
            ids,
            byte_ids,
            specials,
        })
    }

    pub fn profile(&self) -> &'static str {
        GGUF_LLAMA_SPM_TOKENIZER_PROFILE_V1
    }

    /// The BOS id that [`Self::encode_prompt`] prepends.
    pub fn bos_token(&self) -> u32 {
        self.bos_token
    }

    /// Number of pieces in the vocabulary.
    pub fn vocab_len(&self) -> usize {
        self.pieces.len()
    }

    /// BLAKE3 over the vocabulary, as the package manifest records it
    /// (`tokenizer.vocab_blake3`): for each id in order, the piece's UTF-8
    /// length (u32 LE) and bytes, its score (f32 LE) and its type (i32 LE).
    pub fn vocabulary_digest(&self) -> [u8; 32] {
        let mut hasher = blake3::Hasher::new();
        for ((piece, score), kind) in self.pieces.iter().zip(&self.scores).zip(&self.types) {
            hasher.update(&(piece.len() as u32).to_le_bytes());
            hasher.update(piece.as_bytes());
            hasher.update(&score.to_le_bytes());
            hasher.update(&kind.to_le_bytes());
        }
        *hasher.finalize().as_bytes()
    }

    /// End-of-generation ids, as read from the file.
    pub fn eos_tokens(&self) -> &[u32] {
        &self.eos_tokens
    }

    /// Decode generated content while stopping before EOG and suppressing
    /// BOS/control pieces. This is the content-facing companion to token
    /// trace APIs, which retain the terminal EOG for exact comparisons.
    pub fn decode_generated_content(&self, tokens: &[u32]) -> String {
        let mut output = String::new();
        let mut fallback = Vec::new();
        let flush = |output: &mut String, fallback: &mut Vec<u8>| {
            if !fallback.is_empty() {
                output.push_str(&String::from_utf8_lossy(fallback));
                fallback.clear();
            }
        };
        for &id in tokens {
            if self.eos_tokens.contains(&id) {
                break;
            }
            if id == self.bos_token {
                continue;
            }
            let Some(piece) = self.pieces.get(id as usize) else {
                flush(&mut output, &mut fallback);
                output.push_str(&format!("[{id}]"));
                continue;
            };
            if self.types.get(id as usize) == Some(&TOKEN_TYPE_CONTROL) {
                continue;
            }
            if let Some(byte) = parse_byte_piece(piece) {
                fallback.push(byte);
                continue;
            }
            flush(&mut output, &mut fallback);
            output.push_str(&piece.replace('▁', " "));
        }
        flush(&mut output, &mut fallback);
        output
    }

    /// llama_tokenize-compatible input encoding: prepend one BOS and apply
    /// the SPM leading-space rule.  Literal control, unknown, and
    /// user-defined pieces are partitioned as special tokens first.
    pub fn encode_prompt(&self, text: &str) -> Result<Vec<u32>, InferenceError> {
        let mut output = vec![self.bos_token];
        let mut previous_was_special = true;
        for fragment in self.partition_specials(text) {
            match fragment {
                Fragment::Special(token) => {
                    output.push(token);
                    previous_was_special = true;
                }
                Fragment::Raw(raw) => {
                    if raw.is_empty() {
                        continue;
                    }
                    let mut escaped = String::new();
                    if previous_was_special {
                        escaped.push('▁');
                    }
                    escaped.push_str(&raw.replace(' ', "▁"));
                    self.encode_spm_escaped(&escaped, &mut output)?;
                    previous_was_special = false;
                }
            }
        }
        Ok(output)
    }

    fn partition_specials<'a>(&'a self, text: &'a str) -> Vec<Fragment<'a>> {
        let mut fragments = Vec::new();
        let mut raw_start = 0;
        let mut cursor = 0;
        while cursor < text.len() {
            let mut matched = None;
            for (piece, id) in &self.specials {
                if text[cursor..].starts_with(piece) {
                    matched = Some((piece.len(), *id));
                    break;
                }
            }
            if let Some((length, id)) = matched {
                if raw_start < cursor {
                    fragments.push(Fragment::Raw(&text[raw_start..cursor]));
                }
                fragments.push(Fragment::Special(id));
                cursor += length;
                raw_start = cursor;
            } else {
                let width = text[cursor..]
                    .chars()
                    .next()
                    .expect("valid UTF-8")
                    .len_utf8();
                cursor += width;
            }
        }
        if raw_start < text.len() {
            fragments.push(Fragment::Raw(&text[raw_start..]));
        }
        fragments
    }

    fn encode_spm_escaped(&self, text: &str, output: &mut Vec<u32>) -> Result<(), InferenceError> {
        let mut symbols = Vec::new();
        for character in text.chars() {
            let mut bytes = [0; 4];
            symbols.push(Symbol {
                bytes: character.encode_utf8(&mut bytes).as_bytes().to_vec(),
                prev: None,
                next: None,
                active: true,
            });
        }
        for index in 0..symbols.len() {
            symbols[index].prev = index.checked_sub(1);
            symbols[index].next = (index + 1 < symbols.len()).then_some(index + 1);
        }

        // llama.cpp selects the available adjacent pair with highest token
        // score; equal scores choose the smaller left index.  Recomputing the
        // candidates after each merge is slower than its priority queue but
        // preserves those semantics for bounded coordinator prompts.
        loop {
            let mut best: Option<(usize, usize, f32)> = None;
            for left in 0..symbols.len() {
                if !symbols[left].active {
                    continue;
                }
                let Some(right) = symbols[left].next else {
                    continue;
                };
                let mut joined = symbols[left].bytes.clone();
                joined.extend_from_slice(&symbols[right].bytes);
                let Ok(piece) = std::str::from_utf8(&joined) else {
                    continue;
                };
                let Some(&id) = self.ids.get(piece) else {
                    continue;
                };
                let score = self.scores[id as usize];
                if best.map_or(true, |(best_left, _, best_score)| {
                    score > best_score || (score == best_score && left < best_left)
                }) {
                    best = Some((left, right, score));
                }
            }
            let Some((left, right, _)) = best else { break };
            let right_next = symbols[right].next;
            let right_bytes = std::mem::take(&mut symbols[right].bytes);
            symbols[left].bytes.extend_from_slice(&right_bytes);
            symbols[left].next = right_next;
            if let Some(next) = right_next {
                symbols[next].prev = Some(left);
            }
            symbols[right].active = false;
        }

        let mut current = (!symbols.is_empty()).then_some(0usize);
        while let Some(index) = current {
            let symbol = &symbols[index];
            if let Ok(piece) = std::str::from_utf8(&symbol.bytes) {
                if let Some(&id) = self.ids.get(piece) {
                    output.push(id);
                    current = symbol.next;
                    continue;
                }
            }
            for &byte in &symbol.bytes {
                let Some(id) = self.byte_ids[byte as usize] else {
                    return Err(InferenceError::Runtime(format!(
                        "GGUF SPM tokenizer is missing byte fallback <0x{byte:02X}>"
                    )));
                };
                output.push(id);
            }
            current = symbol.next;
        }
        Ok(())
    }
}

enum Fragment<'a> {
    Raw(&'a str),
    Special(u32),
}

fn parse_byte_piece(piece: &str) -> Option<u8> {
    let hex = piece.strip_prefix("<0x")?.strip_suffix('>')?;
    (hex.len() == 2)
        .then(|| u8::from_str_radix(hex, 16).ok())
        .flatten()
}

#[cfg(feature = "candle")]
fn required_strings(
    content: &candle_core::quantized::gguf_file::Content,
    key: &str,
) -> Result<Vec<String>, InferenceError> {
    use candle_core::quantized::gguf_file::Value;
    match content.metadata.get(key) {
        Some(Value::Array(values)) => values
            .iter()
            .map(|value| match value {
                Value::String(text) => Ok(text.clone()),
                _ => Err(InferenceError::Runtime(format!(
                    "GGUF {key} is not a string array"
                ))),
            })
            .collect(),
        _ => Err(InferenceError::Runtime(format!("GGUF missing {key}"))),
    }
}

#[cfg(feature = "candle")]
fn required_scores(
    content: &candle_core::quantized::gguf_file::Content,
    key: &str,
) -> Result<Vec<f32>, InferenceError> {
    use candle_core::quantized::gguf_file::Value;
    match content.metadata.get(key) {
        Some(Value::Array(values)) => values
            .iter()
            .map(|value| match value {
                Value::F32(score) => Ok(*score),
                Value::I32(score) => Ok(*score as f32),
                _ => Err(InferenceError::Runtime(format!(
                    "GGUF {key} is not an f32/i32 array"
                ))),
            })
            .collect(),
        _ => Err(InferenceError::Runtime(format!("GGUF missing {key}"))),
    }
}

#[cfg(feature = "candle")]
fn required_i32s(
    content: &candle_core::quantized::gguf_file::Content,
    key: &str,
) -> Result<Vec<i32>, InferenceError> {
    use candle_core::quantized::gguf_file::Value;
    match content.metadata.get(key) {
        Some(Value::Array(values)) => values
            .iter()
            .map(|value| match value {
                Value::I32(kind) => Ok(*kind),
                _ => Err(InferenceError::Runtime(format!(
                    "GGUF {key} is not an i32 array"
                ))),
            })
            .collect(),
        _ => Err(InferenceError::Runtime(format!("GGUF missing {key}"))),
    }
}

#[cfg(feature = "candle")]
fn required_token_ids(
    content: &candle_core::quantized::gguf_file::Content,
    key: &str,
) -> Result<Vec<u32>, InferenceError> {
    use candle_core::quantized::gguf_file::Value;
    match content.metadata.get(key) {
        Some(Value::U32(value)) => Ok(vec![*value]),
        Some(Value::U64(value)) => Ok(vec![*value as u32]),
        Some(Value::Array(values)) => values
            .iter()
            .map(|value| match value {
                Value::U32(id) => Ok(*id),
                Value::U64(id) => Ok(*id as u32),
                _ => Err(InferenceError::Runtime(format!(
                    "GGUF {key} is not a u32 array"
                ))),
            })
            .collect(),
        _ => Err(InferenceError::Runtime(format!("GGUF missing {key}"))),
    }
}
