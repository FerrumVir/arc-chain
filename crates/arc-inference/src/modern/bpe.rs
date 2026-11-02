//! Byte-level BPE tokenizer for Hugging Face `tokenizer.json` files.
//!
//! Implements exactly the configuration SmolLM3 (and Llama-3) uses and refuses
//! every other one: no normalizer; added tokens split out of the text first,
//! leftmost-longest; a `Split` pre-tokenizer with one regex and `Isolated`
//! behaviour; a `ByteLevel` mapping without prefix space or extra regex; a BPE
//! model without dropout, unknown token, prefixes or byte fallback, optionally
//! with `ignore_merges`; and a post-processor that adds nothing. The merge
//! loop reproduces the reference implementation's priority order (lowest
//! merge rank first, then leftmost), including how it discards stale queue
//! entries, so ids match the `tokenizers` library.
//!
//! Tokenisation is not on the verification path: requests carry token ids.

use std::cmp::Ordering;
use std::collections::{BinaryHeap, HashMap};

use serde_json::Value;

use super::ModernError;

/// An added (special or user) token matched before pre-tokenisation.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AddedToken {
    pub id: u32,
    pub content: String,
    pub special: bool,
}

/// A loaded byte-level BPE tokenizer.
pub struct ByteLevelBpe {
    vocab: HashMap<String, u32>,
    id_to_token: HashMap<u32, String>,
    merges: HashMap<(u32, u32), (u32, u32)>,
    ignore_merges: bool,
    added: Vec<AddedToken>,
    split: fancy_regex::Regex,
    byte_to_char: [char; 256],
    char_to_byte: HashMap<char, u8>,
}

/// GPT-2's reversible byte to printable-character table.
pub fn bytes_to_unicode() -> [char; 256] {
    let mut table = ['\0'; 256];
    let mut extra = 0u32;
    for b in 0u32..256 {
        let printable = (u32::from(b'!')..=u32::from(b'~')).contains(&b)
            || (0xA1..=0xAC).contains(&b)
            || (0xAE..=0xFF).contains(&b);
        let code = if printable {
            b
        } else {
            extra += 1;
            255 + extra
        };
        table[b as usize] = char::from_u32(code).unwrap_or('\0');
    }
    table
}

fn invalid(what: impl Into<String>) -> ModernError {
    ModernError::Invalid(what.into())
}

fn field_is(value: &Value, key: &str, expected: &Value) -> bool {
    value.get(key) == Some(expected)
}

fn null_or(value: &Value, key: &str, allowed: &Value) -> bool {
    match value.get(key) {
        None | Some(Value::Null) => true,
        Some(v) => v == allowed,
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct Merge {
    pos: usize,
    rank: u32,
    new_id: u32,
}

impl Ord for Merge {
    fn cmp(&self, other: &Self) -> Ordering {
        // A min-heap on rank, then on position, inside a max-heap.
        other
            .rank
            .cmp(&self.rank)
            .then_with(|| other.pos.cmp(&self.pos))
    }
}

impl PartialOrd for Merge {
    fn partial_cmp(&self, other: &Self) -> Option<Ordering> {
        Some(self.cmp(other))
    }
}

#[derive(Debug, Clone, Copy)]
struct Symbol {
    id: u32,
    prev: Option<usize>,
    next: Option<usize>,
    alive: bool,
}

impl ByteLevelBpe {
    /// Parse a `tokenizer.json`, refusing any configuration not implemented here.
    pub fn from_json(bytes: &[u8]) -> Result<Self, ModernError> {
        let root: Value =
            serde_json::from_slice(bytes).map_err(|e| invalid(format!("tokenizer.json: {e}")))?;
        if !matches!(root.get("normalizer"), None | Some(Value::Null)) {
            return Err(invalid("tokenizer normalizer is not supported"));
        }
        let pre = root
            .get("pre_tokenizer")
            .ok_or_else(|| invalid("tokenizer has no pre_tokenizer"))?;
        let steps = pre
            .get("pretokenizers")
            .and_then(Value::as_array)
            .filter(|_| pre.get("type").and_then(Value::as_str) == Some("Sequence"))
            .ok_or_else(|| invalid("pre_tokenizer must be a Sequence"))?;
        let [split_step, byte_step] = steps.as_slice() else {
            return Err(invalid("pre_tokenizer must be [Split, ByteLevel]"));
        };
        let pattern = split_step
            .pointer("/pattern/Regex")
            .and_then(Value::as_str)
            .ok_or_else(|| invalid("Split pattern must be a Regex"))?;
        if !field_is(split_step, "type", &Value::from("Split"))
            || !field_is(split_step, "behavior", &Value::from("Isolated"))
            || !field_is(split_step, "invert", &Value::Bool(false))
        {
            return Err(invalid("Split must be Isolated and not inverted"));
        }
        if !field_is(byte_step, "type", &Value::from("ByteLevel"))
            || !field_is(byte_step, "add_prefix_space", &Value::Bool(false))
            || !field_is(byte_step, "use_regex", &Value::Bool(false))
        {
            return Err(invalid(
                "ByteLevel must not add a prefix space or split again",
            ));
        }
        let model = root
            .get("model")
            .ok_or_else(|| invalid("tokenizer has no model"))?;
        let empty = Value::from("");
        if !field_is(model, "type", &Value::from("BPE"))
            || !null_or(model, "dropout", &Value::Null)
            || !null_or(model, "unk_token", &Value::Null)
            || !null_or(model, "continuing_subword_prefix", &empty)
            || !null_or(model, "end_of_word_suffix", &empty)
            || !null_or(model, "fuse_unk", &Value::Bool(false))
            || !null_or(model, "byte_fallback", &Value::Bool(false))
        {
            return Err(invalid("only a plain byte-level BPE model is supported"));
        }
        let ignore_merges = model
            .get("ignore_merges")
            .and_then(Value::as_bool)
            .unwrap_or(false);
        let post_ok = match root.get("post_processor") {
            None | Some(Value::Null) => true,
            Some(post) => {
                let kind = post.get("type").and_then(Value::as_str);
                let single_is_plain = post.get("single")
                    == Some(&serde_json::json!([{"Sequence": {"id": "A", "type_id": 0}}]));
                kind == Some("ByteLevel") || (kind == Some("TemplateProcessing") && single_is_plain)
            }
        };
        if !post_ok {
            return Err(invalid("the post-processor must not add tokens"));
        }
        let vocab_json = model
            .get("vocab")
            .and_then(Value::as_object)
            .ok_or_else(|| invalid("BPE vocab missing"))?;
        let mut vocab = HashMap::with_capacity(vocab_json.len());
        let mut id_to_token = HashMap::with_capacity(vocab_json.len() + 512);
        for (token, id) in vocab_json {
            let id = id
                .as_u64()
                .and_then(|v| u32::try_from(v).ok())
                .ok_or_else(|| invalid(format!("vocab id of {token:?}")))?;
            vocab.insert(token.clone(), id);
            id_to_token.insert(id, token.clone());
        }
        let merge_list = model
            .get("merges")
            .and_then(Value::as_array)
            .ok_or_else(|| invalid("BPE merges missing"))?;
        let mut merges = HashMap::with_capacity(merge_list.len());
        for (rank, entry) in merge_list.iter().enumerate() {
            let (left, right) = match entry {
                Value::String(joined) => joined
                    .split_once(' ')
                    .map(|(a, b)| (a.to_string(), b.to_string()))
                    .ok_or_else(|| invalid(format!("merge {rank} is malformed")))?,
                Value::Array(pair) => match pair.as_slice() {
                    [Value::String(a), Value::String(b)] => (a.clone(), b.clone()),
                    _ => return Err(invalid(format!("merge {rank} is malformed"))),
                },
                _ => return Err(invalid(format!("merge {rank} is malformed"))),
            };
            let lookup = |token: &str| {
                vocab.get(token).copied().ok_or_else(|| {
                    invalid(format!(
                        "merge {rank} uses {token:?}, absent from the vocab"
                    ))
                })
            };
            let pair = (lookup(&left)?, lookup(&right)?);
            let merged = lookup(&format!("{left}{right}"))?;
            let rank = u32::try_from(rank).map_err(|_| invalid("too many merges"))?;
            merges.insert(pair, (rank, merged));
        }
        let mut added = Vec::new();
        for entry in root
            .get("added_tokens")
            .and_then(Value::as_array)
            .map(Vec::as_slice)
            .unwrap_or_default()
        {
            for flag in ["single_word", "lstrip", "rstrip"] {
                if entry.get(flag).and_then(Value::as_bool) == Some(true) {
                    return Err(invalid(format!("added token flag {flag} is not supported")));
                }
            }
            let id = entry
                .get("id")
                .and_then(Value::as_u64)
                .and_then(|v| u32::try_from(v).ok())
                .ok_or_else(|| invalid("added token id"))?;
            let content = entry
                .get("content")
                .and_then(Value::as_str)
                .filter(|c| !c.is_empty())
                .ok_or_else(|| invalid("added token content"))?
                .to_string();
            let special = entry
                .get("special")
                .and_then(Value::as_bool)
                .unwrap_or(false);
            id_to_token.insert(id, content.clone());
            added.push(AddedToken {
                id,
                content,
                special,
            });
        }
        let split = fancy_regex::Regex::new(pattern)
            .map_err(|e| invalid(format!("Split regex does not compile: {e}")))?;
        let byte_to_char = bytes_to_unicode();
        let char_to_byte = byte_to_char
            .iter()
            .enumerate()
            .map(|(b, &c)| (c, b as u8))
            .collect();
        Ok(Self {
            vocab,
            id_to_token,
            merges,
            ignore_merges,
            added,
            split,
            byte_to_char,
            char_to_byte,
        })
    }

    /// Number of ids (model vocabulary plus added tokens).
    pub fn id_count(&self) -> usize {
        self.id_to_token.len()
    }

    /// The added tokens, in file order.
    pub fn added_tokens(&self) -> &[AddedToken] {
        &self.added
    }

    /// The leftmost added token at or after byte `from`; the longest one
    /// among those starting at the same position.
    fn next_added(&self, text: &str, from: usize) -> Option<(usize, usize, u32)> {
        let mut best: Option<(usize, usize, u32)> = None;
        for token in &self.added {
            if let Some(found) = text[from..].find(token.content.as_str()) {
                let start = from + found;
                let len = token.content.len();
                let better = match best {
                    None => true,
                    Some((s, l, _)) => start < s || (start == s && len > l),
                };
                if better {
                    best = Some((start, len, token.id));
                }
            }
        }
        best
    }

    /// Encode `text` to token ids (no BOS or EOS is added).
    pub fn encode(&self, text: &str) -> Result<Vec<u32>, ModernError> {
        let mut ids = Vec::new();
        let mut position = 0;
        while position < text.len() {
            match self.next_added(text, position) {
                Some((start, len, id)) => {
                    self.encode_plain(&text[position..start], &mut ids)?;
                    ids.push(id);
                    position = start + len;
                }
                None => {
                    self.encode_plain(&text[position..], &mut ids)?;
                    position = text.len();
                }
            }
        }
        Ok(ids)
    }

    fn encode_plain(&self, text: &str, ids: &mut Vec<u32>) -> Result<(), ModernError> {
        let mut last = 0;
        for found in self.split.find_iter(text) {
            let found = found.map_err(|e| invalid(format!("Split regex failed: {e}")))?;
            if found.start() > last {
                self.encode_word(&text[last..found.start()], ids)?;
            }
            self.encode_word(found.as_str(), ids)?;
            last = found.end();
        }
        if last < text.len() {
            self.encode_word(&text[last..], ids)?;
        }
        Ok(())
    }

    fn encode_word(&self, word: &str, ids: &mut Vec<u32>) -> Result<(), ModernError> {
        if word.is_empty() {
            return Ok(());
        }
        let mapped: String = word
            .bytes()
            .map(|b| self.byte_to_char[b as usize])
            .collect();
        if self.ignore_merges
            && let Some(&id) = self.vocab.get(&mapped)
        {
            ids.push(id);
            return Ok(());
        }
        let mut symbols = Vec::with_capacity(mapped.len());
        let mut buffer = [0u8; 4];
        for (index, c) in mapped.chars().enumerate() {
            let id = *self
                .vocab
                .get(c.encode_utf8(&mut buffer) as &str)
                .ok_or_else(|| invalid(format!("byte character {c:?} is not in the vocab")))?;
            symbols.push(Symbol {
                id,
                prev: index.checked_sub(1),
                next: Some(index + 1),
                alive: true,
            });
        }
        if let Some(last) = symbols.last_mut() {
            last.next = None;
        }
        let mut queue = BinaryHeap::new();
        for pos in 0..symbols.len().saturating_sub(1) {
            if let Some(&(rank, new_id)) = self.merges.get(&(symbols[pos].id, symbols[pos + 1].id))
            {
                queue.push(Merge { pos, rank, new_id });
            }
        }
        while let Some(top) = queue.pop() {
            let current = symbols[top.pos];
            let Some(next_pos) = current.next else {
                continue;
            };
            if !current.alive {
                continue;
            }
            let right = symbols[next_pos];
            match self.merges.get(&(current.id, right.id)) {
                Some(&(_, new_id)) if new_id == top.new_id => {}
                _ => continue,
            }
            symbols[top.pos].id = top.new_id;
            symbols[top.pos].next = right.next;
            symbols[next_pos].alive = false;
            if let Some(after) = right.next {
                symbols[after].prev = Some(top.pos);
            }
            let merged = symbols[top.pos];
            if let Some(prev) = merged.prev
                && let Some(&(rank, new_id)) = self.merges.get(&(symbols[prev].id, merged.id))
            {
                queue.push(Merge {
                    pos: prev,
                    rank,
                    new_id,
                });
            }
            if let Some(after) = merged.next
                && let Some(&(rank, new_id)) = self.merges.get(&(merged.id, symbols[after].id))
            {
                queue.push(Merge {
                    pos: top.pos,
                    rank,
                    new_id,
                });
            }
        }
        ids.extend(symbols.iter().filter(|s| s.alive).map(|s| s.id));
        Ok(())
    }

    /// The token string of `id` (byte-level characters for vocab tokens).
    pub fn token(&self, id: u32) -> Option<&str> {
        self.id_to_token.get(&id).map(String::as_str)
    }

    /// Whether `id` is an added token marked special.
    pub fn is_special(&self, id: u32) -> bool {
        self.added.iter().any(|t| t.id == id && t.special)
    }

    /// Decode ids to text, like the ByteLevel decoder; optionally drop
    /// special tokens. Unknown ids are skipped.
    pub fn decode(&self, ids: &[u32], skip_special: bool) -> String {
        let mut bytes = Vec::new();
        for &id in ids {
            if skip_special && self.is_special(id) {
                continue;
            }
            let Some(token) = self.token(id) else {
                continue;
            };
            let mapped: Option<Vec<u8>> = token
                .chars()
                .map(|c| self.char_to_byte.get(&c).copied())
                .collect();
            match mapped {
                Some(raw) => bytes.extend(raw),
                None => bytes.extend_from_slice(token.as_bytes()),
            }
        }
        String::from_utf8_lossy(&bytes).into_owned()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A tiny tokenizer.json with the SmolLM3 pre-tokenizer and a few merges.
    fn tiny() -> ByteLevelBpe {
        let table = bytes_to_unicode();
        let mut vocab = serde_json::Map::new();
        for (b, c) in table.iter().enumerate() {
            vocab.insert(c.to_string(), Value::from(b as u64));
        }
        let space = table[b' ' as usize].to_string();
        let newline = table[b'\n' as usize].to_string();
        let mut merges = Vec::new();
        let mut add = |a: &str, b: &str, next: &mut u64| {
            let joined = format!("{a}{b}");
            if !vocab.contains_key(&joined) {
                vocab.insert(joined, Value::from(*next));
                *next += 1;
            }
            merges.push(serde_json::json!([a, b]));
        };
        let mut next = 256u64;
        add("h", "e", &mut next);
        add("l", "l", &mut next);
        add("he", "ll", &mut next);
        add("hell", "o", &mut next);
        add(&space, "w", &mut next);
        add("o", "r", &mut next);
        add(&newline, &newline, &mut next);
        add("a", "a", &mut next);
        let regex = r"(?i:'s|'t|'re|'ve|'m|'ll|'d)|[^\r\n\p{L}\p{N}]?\p{L}+|\p{N}{1,3}| ?[^\s\p{L}\p{N}]+[\r\n]*|\s*[\r\n]+|\s+(?!\S)|\s+";
        let json = serde_json::json!({
            "version": "1.0",
            "added_tokens": [
                {"id": 900, "content": "<|im_start|>", "single_word": false, "lstrip": false, "rstrip": false, "normalized": false, "special": true},
                {"id": 901, "content": "<|im_end|>", "single_word": false, "lstrip": false, "rstrip": false, "normalized": false, "special": true},
                {"id": 902, "content": "<|im", "single_word": false, "lstrip": false, "rstrip": false, "normalized": false, "special": false}
            ],
            "normalizer": null,
            "pre_tokenizer": {"type": "Sequence", "pretokenizers": [
                {"type": "Split", "pattern": {"Regex": regex}, "behavior": "Isolated", "invert": false},
                {"type": "ByteLevel", "add_prefix_space": false, "trim_offsets": true, "use_regex": false}
            ]},
            "post_processor": {"type": "TemplateProcessing", "single": [{"Sequence": {"id": "A", "type_id": 0}}], "pair": [], "special_tokens": {}},
            "decoder": {"type": "ByteLevel", "add_prefix_space": true, "trim_offsets": true, "use_regex": true},
            "model": {"type": "BPE", "dropout": null, "unk_token": null, "continuing_subword_prefix": null,
                      "end_of_word_suffix": null, "fuse_unk": false, "byte_fallback": false, "ignore_merges": true,
                      "vocab": vocab, "merges": merges}
        });
        ByteLevelBpe::from_json(json.to_string().as_bytes()).unwrap()
    }

    fn id(t: &ByteLevelBpe, token: &str) -> u32 {
        t.vocab[token]
    }

    #[test]
    fn the_byte_table_is_a_bijection_onto_printable_characters() {
        let table = bytes_to_unicode();
        let mut seen: Vec<char> = table.to_vec();
        seen.sort_unstable();
        seen.dedup();
        assert_eq!(seen.len(), 256);
        assert_eq!(table[b'A' as usize], 'A');
        assert_eq!(table[b' ' as usize], '\u{120}');
        assert_eq!(table[b'\n' as usize], '\u{10A}');
        assert_eq!(table[0], '\u{100}');
    }

    #[test]
    fn merges_apply_in_rank_order_and_whole_words_short_circuit() {
        let t = tiny();
        assert_eq!(t.encode("hello").unwrap(), vec![id(&t, "hello")]);
        let space = bytes_to_unicode()[b' ' as usize];
        // " world" is one regex piece; " w" and "or" merge, the rest stay bytes.
        assert_eq!(
            t.encode(" world").unwrap(),
            vec![
                id(&t, &format!("{space}w")),
                id(&t, "or"),
                id(&t, "l"),
                id(&t, "d")
            ]
        );
        // "aaa": the leftmost equal-rank pair merges first.
        assert_eq!(t.encode("aaa").unwrap(), vec![id(&t, "aa"), id(&t, "a")]);
    }

    #[test]
    fn added_tokens_split_first_leftmost_longest() {
        let t = tiny();
        let ids = t.encode("<|im_start|>hello<|im_end|>").unwrap();
        assert_eq!(ids, vec![900, id(&t, "hello"), 901]);
        // "<|im" alone matches the shorter added token.
        let ids = t.encode("<|imx").unwrap();
        assert_eq!(ids[0], 902);
        assert_eq!(t.decode(&[900, id(&t, "hello"), 901], true), "hello");
        assert_eq!(
            t.decode(&[900, id(&t, "hello"), 901], false),
            "<|im_start|>hello<|im_end|>"
        );
    }

    #[test]
    fn the_split_regex_isolates_numbers_newlines_and_spaces() {
        let t = tiny();
        let text = "ab 12345\n\n  x's  ";
        let ids = t.encode(text).unwrap();
        assert_eq!(t.decode(&ids, false), text);
        // "12345" splits as "123" + "45" (numbers in runs of at most three).
        let digits = t.encode("12345").unwrap();
        assert_eq!(
            digits,
            vec![
                id(&t, "1"),
                id(&t, "2"),
                id(&t, "3"),
                id(&t, "4"),
                id(&t, "5")
            ]
        );
        // Unicode text round-trips through the byte-level mapping.
        let unicode = "café 中文 😀";
        assert_eq!(t.decode(&t.encode(unicode).unwrap(), false), unicode);
    }

    #[test]
    fn unsupported_configurations_are_refused() {
        let base = serde_json::json!({
            "normalizer": {"type": "NFC"},
            "pre_tokenizer": null,
            "model": {"type": "BPE", "vocab": {}, "merges": []}
        });
        assert!(ByteLevelBpe::from_json(base.to_string().as_bytes()).is_err());
    }
}
