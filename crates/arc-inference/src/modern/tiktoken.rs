//! The tiktoken byte-pair encoder of Moonlight-16B-A3B and Kimi K2 (spec §8
//! of `docs/protocol/integer-profile-mla-moe-dyadic-v1.md`).
//!
//! Both models ship the same `tiktoken.model` (one `base64(bytes) rank` line
//! per token) and the same split pattern; their Python wrappers differ only in
//! how many reserved special tokens follow the base vocabulary. Encoding
//! mirrors the wrappers' `encode(text)` with special tokens allowed:
//! chunking at 400,000 code points and at whitespace/non-whitespace runs
//! longer than 25,000 (Python `str.isspace`), leftmost special-token matching,
//! the split pattern, then tiktoken's lowest-rank-first byte-pair merge.
//! Tokenisation is not on the verification path: requests carry token ids.

use std::collections::HashMap;

use fancy_regex::Regex;
use serde_json::Value;

use super::ModernError;

/// Tokenizer identity (bound together with the `tiktoken.model` SHA-256).
pub const TOKENIZER_IDENTITY: &str = "arc.tiktoken-bpe.v1";

/// The split pattern of Moonlight's `tokenization_moonshot.py` and Kimi K2's
/// `tokenization_kimi.py` (identical in both).
pub const KIMI_PATTERN: &str = concat!(
    r"[\p{Han}]+",
    "|",
    r"[^\r\n\p{L}\p{N}]?[\p{Lu}\p{Lt}\p{Lm}\p{Lo}\p{M}&&[^\p{Han}]]*[\p{Ll}\p{Lm}\p{Lo}\p{M}&&[^\p{Han}]]+(?i:'s|'t|'re|'ve|'m|'ll|'d)?",
    "|",
    r"[^\r\n\p{L}\p{N}]?[\p{Lu}\p{Lt}\p{Lm}\p{Lo}\p{M}&&[^\p{Han}]]+[\p{Ll}\p{Lm}\p{Lo}\p{M}&&[^\p{Han}]]*(?i:'s|'t|'re|'ve|'m|'ll|'d)?",
    "|",
    r"\p{N}{1,3}",
    "|",
    r" ?[^\s\p{L}\p{N}]+[\r\n]*",
    "|",
    r"\s*[\r\n]+",
    "|",
    r"\s+(?!\S)",
    "|",
    r"\s+",
);

/// Reserved special tokens after the base vocabulary in Moonlight's wrapper.
pub const MOONLIGHT_SPECIAL_TOKENS: usize = 258;
/// Reserved special tokens after the base vocabulary in Kimi K2's wrapper.
pub const KIMI_SPECIAL_TOKENS: usize = 256;
/// The wrappers encode at most this many code points per call.
const MAX_ENCODE_CHARS: usize = 400_000;
/// The wrappers split runs of whitespace or non-whitespace longer than this.
const MAX_RUN_CHARS: usize = 25_000;

fn invalid(what: impl Into<String>) -> ModernError {
    ModernError::Invalid(what.into())
}

/// Python's `str.isspace` (characters of bidirectional class WS, B or S, or
/// general category Zs).
pub fn py_isspace(c: char) -> bool {
    matches!(
        c,
        '\u{9}'..='\u{d}'
            | '\u{1c}'..='\u{20}'
            | '\u{85}'
            | '\u{a0}'
            | '\u{1680}'
            | '\u{2000}'..='\u{200a}'
            | '\u{2028}'
            | '\u{2029}'
            | '\u{202f}'
            | '\u{205f}'
            | '\u{3000}'
    )
}

fn base64_decode(text: &str) -> Option<Vec<u8>> {
    fn value(c: u8) -> Option<u32> {
        match c {
            b'A'..=b'Z' => Some(u32::from(c - b'A')),
            b'a'..=b'z' => Some(u32::from(c - b'a') + 26),
            b'0'..=b'9' => Some(u32::from(c - b'0') + 52),
            b'+' => Some(62),
            b'/' => Some(63),
            _ => None,
        }
    }
    let body = text.trim_end_matches('=').as_bytes();
    let mut out = Vec::with_capacity(body.len() * 3 / 4);
    let (mut acc, mut bits) = (0u32, 0u32);
    for &c in body {
        acc = (acc << 6) | value(c)?;
        bits += 6;
        if bits >= 8 {
            bits -= 8;
            out.push((acc >> bits) as u8);
            acc &= (1 << bits) - 1;
        }
    }
    Some(out)
}

/// A loaded tiktoken vocabulary with its special tokens.
pub struct TiktokenBpe {
    ranks: HashMap<Vec<u8>, u32>,
    tokens: Vec<Vec<u8>>,
    /// `(name, id)` by id.
    specials: Vec<(String, u32)>,
    special_names: HashMap<u32, String>,
    pattern: Regex,
}

impl TiktokenBpe {
    /// Load `tiktoken.model` and, optionally, the special-token names of
    /// `tokenizer_config.json` (`added_tokens_decoder`). Ids
    /// `n_base .. n_base + n_special` are special; unnamed ones are
    /// `<|reserved_token_{id}|>`.
    pub fn from_files(
        model: &[u8],
        tokenizer_config: Option<&[u8]>,
        n_special: usize,
    ) -> Result<Self, ModernError> {
        let text =
            std::str::from_utf8(model).map_err(|_| invalid("tiktoken.model is not UTF-8"))?;
        let mut tokens: Vec<Vec<u8>> = Vec::new();
        let mut ranks = HashMap::new();
        for (line_number, line) in text.lines().enumerate() {
            if line.trim().is_empty() {
                continue;
            }
            let (encoded, rank) = line
                .split_once(' ')
                .ok_or_else(|| invalid(format!("tiktoken.model line {}", line_number + 1)))?;
            let bytes = base64_decode(encoded).ok_or_else(|| {
                invalid(format!("tiktoken.model line {} base64", line_number + 1))
            })?;
            let rank: u32 = rank
                .trim()
                .parse()
                .map_err(|_| invalid(format!("tiktoken.model line {} rank", line_number + 1)))?;
            if rank as usize != tokens.len() || ranks.insert(bytes.clone(), rank).is_some() {
                return Err(invalid(format!(
                    "tiktoken.model ranks must be 0, 1, 2, ... without repeats (line {})",
                    line_number + 1
                )));
            }
            tokens.push(bytes);
        }
        for byte in 0..=255u8 {
            if !ranks.contains_key(&[byte][..]) {
                return Err(invalid(format!("tiktoken.model lacks the byte {byte}")));
            }
        }
        let n_base = tokens.len() as u32;
        let mut names: HashMap<u32, String> = HashMap::new();
        if let Some(config) = tokenizer_config {
            let value: Value = serde_json::from_slice(config)
                .map_err(|e| invalid(format!("tokenizer_config.json: {e}")))?;
            if let Some(added) = value.get("added_tokens_decoder").and_then(Value::as_object) {
                for (id, entry) in added {
                    let id: u32 = id
                        .parse()
                        .map_err(|_| invalid("added_tokens_decoder id is not a number"))?;
                    let content = entry
                        .get("content")
                        .and_then(Value::as_str)
                        .ok_or_else(|| invalid("added_tokens_decoder entry has no content"))?;
                    names.insert(id, content.to_string());
                }
            }
        }
        let mut specials = Vec::with_capacity(n_special);
        let mut special_names = HashMap::new();
        for offset in 0..n_special as u32 {
            let id = n_base + offset;
            let name = names
                .get(&id)
                .cloned()
                .unwrap_or_else(|| format!("<|reserved_token_{id}|>"));
            if name.is_empty() {
                return Err(invalid("special token names must be non-empty"));
            }
            specials.push((name.clone(), id));
            special_names.insert(id, name);
        }
        for (a, _) in &specials {
            for (b, _) in &specials {
                if a != b && b.starts_with(a.as_str()) {
                    return Err(invalid(format!(
                        "special token {a} is a prefix of {b}; leftmost matching would be ambiguous"
                    )));
                }
            }
        }
        let pattern = Regex::new(KIMI_PATTERN)
            .map_err(|e| invalid(format!("tokenizer split pattern: {e}")))?;
        Ok(Self {
            ranks,
            tokens,
            specials,
            special_names,
            pattern,
        })
    }

    /// Size of the base (byte-pair) vocabulary.
    pub fn n_base(&self) -> usize {
        self.tokens.len()
    }

    /// The id of a special token by name.
    pub fn special_id(&self, name: &str) -> Option<u32> {
        self.specials
            .iter()
            .find(|(n, _)| n == name)
            .map(|&(_, id)| id)
    }

    /// tiktoken's merge: repeatedly join the adjacent pair whose bytes have
    /// the lowest rank (leftmost on ties) until no pair is a token.
    fn byte_pair_merge(&self, piece: &[u8]) -> Vec<(usize, u32)> {
        let rank_of = |bytes: &[u8]| self.ranks.get(bytes).copied().unwrap_or(u32::MAX);
        let mut parts: Vec<(usize, u32)> = Vec::with_capacity(piece.len() + 1);
        let mut min_rank: (u32, usize) = (u32::MAX, usize::MAX);
        for i in 0..piece.len() - 1 {
            let rank = rank_of(&piece[i..i + 2]);
            if rank < min_rank.0 {
                min_rank = (rank, i);
            }
            parts.push((i, rank));
        }
        parts.push((piece.len() - 1, u32::MAX));
        parts.push((piece.len(), u32::MAX));
        let get_rank = |parts: &[(usize, u32)], i: usize| -> u32 {
            if i + 3 < parts.len() {
                rank_of(&piece[parts[i].0..parts[i + 3].0])
            } else {
                u32::MAX
            }
        };
        while min_rank.0 != u32::MAX {
            let i = min_rank.1;
            if i > 0 {
                parts[i - 1].1 = get_rank(&parts, i - 1);
            }
            parts[i].1 = get_rank(&parts, i);
            parts.remove(i + 1);
            min_rank = (u32::MAX, usize::MAX);
            for (j, &(_, rank)) in parts[..parts.len() - 1].iter().enumerate() {
                if rank < min_rank.0 {
                    min_rank = (rank, j);
                }
            }
        }
        parts
    }

    fn byte_pair_encode(&self, piece: &[u8], out: &mut Vec<u32>) -> Result<(), ModernError> {
        if let Some(&rank) = self.ranks.get(piece) {
            out.push(rank);
            return Ok(());
        }
        if piece.len() < 2 {
            return Err(invalid("a single byte is missing from the vocabulary"));
        }
        let parts = self.byte_pair_merge(piece);
        for pair in parts.windows(2) {
            let rank = self
                .ranks
                .get(&piece[pair[0].0..pair[1].0])
                .copied()
                .ok_or_else(|| invalid("byte-pair merge produced an unknown token"))?;
            out.push(rank);
        }
        Ok(())
    }

    fn encode_ordinary(&self, text: &str, out: &mut Vec<u32>) -> Result<(), ModernError> {
        for found in self.pattern.find_iter(text) {
            let found = found.map_err(|e| invalid(format!("tokenizer split pattern: {e}")))?;
            self.byte_pair_encode(found.as_str().as_bytes(), out)?;
        }
        Ok(())
    }

    fn encode_with_specials(&self, text: &str, out: &mut Vec<u32>) -> Result<(), ModernError> {
        let mut start = 0;
        loop {
            let mut next: Option<(usize, usize, u32)> = None;
            for (name, id) in &self.specials {
                if let Some(at) = text[start..].find(name.as_str()) {
                    let at = start + at;
                    if next.is_none_or(|(best, _, _)| at < best) {
                        next = Some((at, name.len(), *id));
                    }
                }
            }
            match next {
                Some((at, len, id)) => {
                    self.encode_ordinary(&text[start..at], out)?;
                    out.push(id);
                    start = at + len;
                }
                None => {
                    self.encode_ordinary(&text[start..], out)?;
                    return Ok(());
                }
            }
        }
    }

    /// Encode text as the reference wrappers do, special tokens allowed.
    pub fn encode(&self, text: &str) -> Result<Vec<u32>, ModernError> {
        let mut out = Vec::new();
        let starts: Vec<usize> = text.char_indices().map(|(i, _)| i).collect();
        let mut chunk_start = 0;
        while chunk_start < starts.len() {
            let chunk_end = (chunk_start + MAX_ENCODE_CHARS).min(starts.len());
            let byte_start = starts[chunk_start];
            let byte_end = starts.get(chunk_end).copied().unwrap_or(text.len());
            for piece in split_runs(&text[byte_start..byte_end]) {
                self.encode_with_specials(piece, &mut out)?;
            }
            chunk_start = chunk_end;
        }
        Ok(out)
    }

    /// Decode ids to text (UTF-8 with replacement); special ids decode to
    /// their names.
    pub fn decode(&self, ids: &[u32]) -> String {
        let mut bytes = Vec::new();
        for &id in ids {
            if let Some(token) = self.tokens.get(id as usize) {
                bytes.extend_from_slice(token);
            } else if let Some(name) = self.special_names.get(&id) {
                bytes.extend_from_slice(name.as_bytes());
            }
        }
        String::from_utf8_lossy(&bytes).into_owned()
    }
}

/// The wrappers' `_split_whitespaces_or_nonwhitespaces(s, 25000)`: cut where
/// a run of whitespace or of non-whitespace would exceed the limit.
fn split_runs(text: &str) -> Vec<&str> {
    let mut pieces = Vec::new();
    let mut run = 0usize;
    let mut run_is_space = text.chars().next().is_some_and(py_isspace);
    let mut slice_start = 0usize;
    for (at, c) in text.char_indices() {
        let is_space = py_isspace(c);
        if run_is_space != is_space {
            run = 1;
            run_is_space = is_space;
        } else {
            run += 1;
            if run > MAX_RUN_CHARS {
                pieces.push(&text[slice_start..at]);
                slice_start = at;
                run = 1;
            }
        }
    }
    pieces.push(&text[slice_start..]);
    pieces
}

/// Moonlight-16B-A3B-Instruct's chat prompt for one user turn (spec §8.3).
pub fn render_moonlight_chat(system: Option<&str>, user: &str) -> String {
    format!(
        "<|im_system|>system<|im_middle|>{}<|im_end|><|im_user|>user<|im_middle|>{user}<|im_end|><|im_assistant|>assistant<|im_middle|>",
        system.unwrap_or("You are a helpful assistant")
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    fn b64(bytes: &[u8]) -> String {
        const ALPHABET: &[u8; 64] =
            b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789+/";
        let mut out = String::new();
        for chunk in bytes.chunks(3) {
            let n = chunk
                .iter()
                .enumerate()
                .fold(0u32, |acc, (i, &b)| acc | (u32::from(b) << (16 - 8 * i)));
            for i in 0..4 {
                if i <= chunk.len() {
                    out.push(ALPHABET[((n >> (18 - 6 * i)) & 63) as usize] as char);
                } else {
                    out.push('=');
                }
            }
        }
        out
    }

    /// Bytes 0..=255 as ranks 0..=255, then the given merges in rank order.
    fn vocab(merges: &[&str]) -> Vec<u8> {
        let mut lines = Vec::new();
        for b in 0..=255u8 {
            lines.push(format!("{} {}", b64(&[b]), b));
        }
        for (i, m) in merges.iter().enumerate() {
            lines.push(format!("{} {}", b64(m.as_bytes()), 256 + i));
        }
        (lines.join("\n") + "\n").into_bytes()
    }

    #[test]
    fn base64_round_trips() {
        let samples: [&[u8]; 6] = [b"", b"a", b"ab", b"abc", b"abcd", &[0, 255, 128, 7, 9]];
        for sample in samples {
            assert_eq!(base64_decode(&b64(sample)).unwrap(), sample);
        }
        assert!(base64_decode("a*b=").is_none());
    }

    #[test]
    fn merges_take_the_lowest_rank_first_leftmost_on_ties() {
        // "ab" (256) before "bc" (257) before "abc" (258).
        let t = TiktokenBpe::from_files(&vocab(&["ab", "bc", "abc", "aa"]), None, 2).unwrap();
        let mut out = Vec::new();
        t.byte_pair_encode(b"abc", &mut out).unwrap();
        assert_eq!(out, vec![258]); // whole piece is a token
        out.clear();
        t.byte_pair_encode(b"abcbc", &mut out).unwrap();
        // ab|c|b|c -> (ab)(c)(bc)? "ab" merges first, then "bc" at the end;
        // then "abc" from (ab)(c).
        assert_eq!(out, vec![258, 257]);
        out.clear();
        t.byte_pair_encode(b"aaa", &mut out).unwrap();
        // Tied pairs: the leftmost merges first.
        assert_eq!(out, vec![259, u32::from(b'a')]);
    }

    #[test]
    fn the_split_pattern_follows_the_reference_classes() {
        let t = TiktokenBpe::from_files(&vocab(&[]), None, 0).unwrap();
        let pieces: Vec<&str> = t
            .pattern
            .find_iter("Hello world, 12345 don't 你好世界\n\n  x")
            .map(|m| m.unwrap().as_str())
            .collect();
        assert_eq!(
            pieces,
            vec![
                "Hello",
                " world",
                ",",
                " ",
                "123",
                "45",
                " don't",
                " ",
                "你好世界",
                "\n\n",
                " ",
                " x"
            ]
        );
    }

    #[test]
    fn specials_are_matched_leftmost_and_named_from_the_config() {
        let config = br#"{"added_tokens_decoder": {"258": {"content": "<|im_end|>"}, "259": {"content": "[EOS]"}}}"#;
        let t = TiktokenBpe::from_files(&vocab(&["ab"]), Some(config), 4).unwrap();
        assert_eq!(t.n_base(), 257);
        // Ids 257.. are special: 257 reserved, 258 <|im_end|>, 259 [EOS], 260 reserved.
        assert_eq!(t.special_id("<|im_end|>"), Some(258));
        assert_eq!(t.special_id("<|reserved_token_257|>"), Some(257));
        let ids = t.encode("ab<|im_end|>[EOS]x").unwrap();
        assert_eq!(ids, vec![256, 258, 259, u32::from(b'x')]);
        assert_eq!(t.decode(&ids), "ab<|im_end|>[EOS]x");
    }

    #[test]
    fn long_runs_are_split_like_the_python_wrapper() {
        let text = "a".repeat(MAX_RUN_CHARS + 5);
        let pieces = split_runs(&text);
        assert_eq!(pieces.len(), 2);
        assert_eq!(pieces[0].len(), MAX_RUN_CHARS);
        assert_eq!(split_runs(""), vec![""]);
        assert!(py_isspace('\u{1c}') && py_isspace('\u{3000}') && !py_isspace('\u{200b}'));
    }

    #[test]
    fn the_moonlight_chat_prompt_matches_its_template() {
        assert_eq!(
            render_moonlight_chat(None, "Hi"),
            "<|im_system|>system<|im_middle|>You are a helpful assistant<|im_end|><|im_user|>user<|im_middle|>Hi<|im_end|><|im_assistant|>assistant<|im_middle|>"
        );
    }
}
