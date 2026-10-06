//! Stage-boundary activations (spec §6): hashes and the boundary file.
//!
//! Boundary `l` is the residual stream entering layer `l`. A stage `[a, b)`
//! maps boundary `a` to boundary `b`, so the boundary file is the complete
//! state one stage hands the next. Its values are exact Q16 integers; the
//! file's bytes, and therefore every hash, are identical on every platform.

use std::path::Path;

use serde_json::{Value, json};

use super::{BOUNDARY_MAGIC, BOUNDARY_SCHEMA, PROFILE};
use crate::modern::arith::{ACTIVATION_LIMIT, Selection};
use crate::modern::{ModernError, hex_lower};

/// `BLAKE3(v as LE i64)` for one position's residual vector (spec §6.2).
pub fn activation_hash(values: &[i64]) -> [u8; 32] {
    let mut hasher = blake3::Hasher::new();
    for v in values {
        hasher.update(&v.to_le_bytes());
    }
    *hasher.finalize().as_bytes()
}

/// `BLAKE3(activation_hash(position 0) || ...)` over `positions * d_model`
/// values (spec §6.2).
pub fn boundary_digest(values: &[i64], d_model: usize) -> [u8; 32] {
    let mut hasher = blake3::Hasher::new();
    for row in values.chunks_exact(d_model.max(1)) {
        hasher.update(&activation_hash(row));
    }
    *hasher.finalize().as_bytes()
}

/// One sequence of a boundary file.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct BoundarySequence {
    pub id: String,
    /// Token ids forwarded at positions `0 .. P` (spec §6.4).
    pub tokens: Vec<u32>,
    /// Prompt length, selection rule, EOS ids and token budget of the
    /// generation the sequence came from (dyadic v1 §6.2).
    pub prompt_len: usize,
    pub selection: Selection,
    pub eos: Vec<u32>,
    pub max_tokens: usize,
    /// `tokens.len() * d_model` Q16 values, row-major by position.
    pub values: Vec<i64>,
}

impl BoundarySequence {
    /// Number of positions.
    pub fn positions(&self) -> usize {
        self.tokens.len()
    }
}

/// A boundary file (spec §6.3).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Boundary {
    /// The boundary index `l` (0 = embedding output, L = last layer output).
    pub layer: usize,
    pub d_model: usize,
    /// The model root of the weights that produced it (spec §4.7).
    pub model_root: String,
    pub sequences: Vec<BoundarySequence>,
}

fn invalid(what: impl Into<String>) -> ModernError {
    ModernError::Invalid(what.into())
}

fn align64(value: usize) -> usize {
    value.div_ceil(64) * 64
}

impl Boundary {
    /// The canonical header (spec §6.3).
    pub fn header_json(&self) -> Value {
        let sequences: Vec<Value> = self
            .sequences
            .iter()
            .map(|s| {
                json!({
                    "id": s.id,
                    "tokens": s.tokens,
                    "prompt_len": s.prompt_len,
                    "selection": s.selection.name(),
                    "eos": s.eos,
                    "max_tokens": s.max_tokens,
                    "digest": hex_lower(&boundary_digest(&s.values, self.d_model)),
                })
            })
            .collect();
        json!({
            "schema": BOUNDARY_SCHEMA,
            "profile": PROFILE,
            "model_root": self.model_root,
            "layer": self.layer,
            "d_model": self.d_model,
            "sequences": sequences,
        })
    }

    fn check(&self) -> Result<(), ModernError> {
        if self.d_model == 0 {
            return Err(invalid("boundary d_model must be positive"));
        }
        for s in &self.sequences {
            if s.tokens.is_empty() || s.values.len() != s.tokens.len() * self.d_model {
                return Err(invalid(format!(
                    "boundary sequence {} has {} values for {} positions",
                    s.id,
                    s.values.len(),
                    s.tokens.len()
                )));
            }
            if s.values
                .iter()
                .any(|v| u128::from(v.unsigned_abs()) > ACTIVATION_LIMIT)
            {
                return Err(ModernError::Domain(format!(
                    "boundary sequence {} holds a value beyond 2^62",
                    s.id
                )));
            }
        }
        Ok(())
    }

    /// Serialise to the file format (spec §6.3).
    pub fn to_bytes(&self) -> Result<Vec<u8>, ModernError> {
        self.check()?;
        let header = crate::model_package::canonical_json(&self.header_json())
            .map_err(|e| invalid(format!("canonical JSON: {e}")))?;
        let data_start = align64(16 + header.len());
        let values: usize = self.sequences.iter().map(|s| s.values.len()).sum();
        let mut out = Vec::with_capacity(data_start + 8 * values);
        out.extend_from_slice(BOUNDARY_MAGIC);
        out.extend_from_slice(&(header.len() as u64).to_le_bytes());
        out.extend_from_slice(header.as_bytes());
        out.resize(data_start, 0);
        for s in &self.sequences {
            for v in &s.values {
                out.extend_from_slice(&v.to_le_bytes());
            }
        }
        Ok(out)
    }

    /// Parse a boundary file, refusing any inconsistency, including data that
    /// does not hash to the header's digests.
    pub fn from_bytes(bytes: &[u8]) -> Result<Self, ModernError> {
        if bytes.len() < 16 || &bytes[..8] != BOUNDARY_MAGIC {
            return Err(invalid("not an ARC boundary file"));
        }
        let mut len_bytes = [0u8; 8];
        len_bytes.copy_from_slice(&bytes[8..16]);
        let header_len = usize::try_from(u64::from_le_bytes(len_bytes))
            .map_err(|_| invalid("boundary header length"))?;
        if header_len > bytes.len() - 16 {
            return Err(invalid("boundary header length exceeds the file"));
        }
        let text = &bytes[16..16 + header_len];
        let header: Value = serde_json::from_slice(text)
            .map_err(|e| invalid(format!("boundary header JSON: {e}")))?;
        let canonical = crate::model_package::canonical_json(&header)
            .map_err(|e| invalid(format!("canonical JSON: {e}")))?;
        if canonical.as_bytes() != text {
            return Err(invalid("boundary header is not canonical JSON"));
        }
        let object = header
            .as_object()
            .ok_or_else(|| invalid("boundary header is not an object"))?;
        let mut keys: Vec<&str> = object.keys().map(String::as_str).collect();
        keys.sort_unstable();
        if keys
            != [
                "d_model",
                "layer",
                "model_root",
                "profile",
                "schema",
                "sequences",
            ]
        {
            return Err(invalid(format!("boundary header fields {keys:?}")));
        }
        if header["schema"] != BOUNDARY_SCHEMA || header["profile"] != PROFILE {
            return Err(invalid("boundary schema or profile mismatch"));
        }
        let size = |v: &Value, what: &str| {
            v.as_u64()
                .and_then(|n| usize::try_from(n).ok())
                .ok_or_else(|| invalid(format!("boundary {what}")))
        };
        let ids = |v: &Value, what: &str| -> Result<Vec<u32>, ModernError> {
            v.as_array()
                .ok_or_else(|| invalid(format!("boundary {what}")))?
                .iter()
                .map(|x| {
                    x.as_u64()
                        .and_then(|n| u32::try_from(n).ok())
                        .ok_or_else(|| invalid(format!("boundary {what}")))
                })
                .collect()
        };
        let d_model = size(&header["d_model"], "d_model")?;
        let layer = size(&header["layer"], "layer")?;
        let model_root = header["model_root"]
            .as_str()
            .ok_or_else(|| invalid("boundary model_root"))?
            .to_string();
        let mut offset = align64(16 + header_len);
        if bytes[16 + header_len..offset.min(bytes.len())]
            .iter()
            .any(|&b| b != 0)
        {
            return Err(invalid("non-zero boundary header padding"));
        }
        let mut sequences = Vec::new();
        for entry in header["sequences"]
            .as_array()
            .ok_or_else(|| invalid("boundary sequences"))?
        {
            let fields = entry
                .as_object()
                .ok_or_else(|| invalid("boundary sequence"))?;
            let mut keys: Vec<&str> = fields.keys().map(String::as_str).collect();
            keys.sort_unstable();
            if keys
                != [
                    "digest",
                    "eos",
                    "id",
                    "max_tokens",
                    "prompt_len",
                    "selection",
                    "tokens",
                ]
            {
                return Err(invalid(format!("boundary sequence fields {keys:?}")));
            }
            let tokens = ids(&entry["tokens"], "tokens")?;
            let count = tokens.len() * d_model;
            let end = offset + 8 * count;
            if end > bytes.len() {
                return Err(invalid("boundary file is truncated"));
            }
            let values: Vec<i64> = bytes[offset..end]
                .chunks_exact(8)
                .map(|c| i64::from_le_bytes([c[0], c[1], c[2], c[3], c[4], c[5], c[6], c[7]]))
                .collect();
            offset = end;
            let digest = hex_lower(&boundary_digest(&values, d_model));
            if entry["digest"].as_str() != Some(digest.as_str()) {
                return Err(invalid(format!(
                    "boundary sequence {} does not hash to its header digest",
                    entry["id"]
                )));
            }
            sequences.push(BoundarySequence {
                id: entry["id"]
                    .as_str()
                    .ok_or_else(|| invalid("boundary sequence id"))?
                    .to_string(),
                tokens,
                prompt_len: size(&entry["prompt_len"], "prompt_len")?,
                selection: Selection::parse(
                    entry["selection"]
                        .as_str()
                        .ok_or_else(|| invalid("boundary selection"))?,
                )?,
                eos: ids(&entry["eos"], "eos")?,
                max_tokens: size(&entry["max_tokens"], "max_tokens")?,
                values,
            });
        }
        if offset != bytes.len() {
            return Err(invalid("boundary file has trailing bytes"));
        }
        let boundary = Self {
            layer,
            d_model,
            model_root,
            sequences,
        };
        boundary.check()?;
        Ok(boundary)
    }

    /// Write the file; returns `(bytes, BLAKE3 hex of the file)`.
    pub fn write(&self, path: &Path) -> Result<(u64, String), ModernError> {
        let bytes = self.to_bytes()?;
        std::fs::write(path, &bytes)
            .map_err(|e| ModernError::io(&path.display().to_string(), e))?;
        Ok((
            bytes.len() as u64,
            hex_lower(blake3::hash(&bytes).as_bytes()),
        ))
    }

    /// Read and validate a boundary file.
    pub fn read(path: &Path) -> Result<Self, ModernError> {
        let bytes =
            std::fs::read(path).map_err(|e| ModernError::io(&path.display().to_string(), e))?;
        Self::from_bytes(&bytes)
    }

    /// Per-sequence digests as hex, in order.
    pub fn digests(&self) -> Vec<String> {
        self.sequences
            .iter()
            .map(|s| hex_lower(&boundary_digest(&s.values, self.d_model)))
            .collect()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn sample() -> Boundary {
        Boundary {
            layer: 7,
            d_model: 3,
            model_root: "ab".repeat(32),
            sequences: vec![
                BoundarySequence {
                    id: "a".into(),
                    tokens: vec![5, 6],
                    prompt_len: 1,
                    selection: Selection::Rp64Argmax,
                    eos: vec![9],
                    max_tokens: 4,
                    values: vec![1, -2, 3, i64::from(i32::MAX) + 7, 0, -(1 << 40)],
                },
                BoundarySequence {
                    id: "b".into(),
                    tokens: vec![1],
                    prompt_len: 1,
                    selection: Selection::Argmax,
                    eos: vec![],
                    max_tokens: 1,
                    values: vec![0, 0, 1],
                },
            ],
        }
    }

    #[test]
    fn boundary_files_round_trip_and_are_aligned() {
        let b = sample();
        let bytes = b.to_bytes().unwrap();
        assert_eq!(&bytes[..8], BOUNDARY_MAGIC);
        let header_len = u64::from_le_bytes(bytes[8..16].try_into().unwrap()) as usize;
        let data_start = align64(16 + header_len);
        assert_eq!(bytes.len(), data_start + 8 * 9);
        assert_eq!(Boundary::from_bytes(&bytes).unwrap(), b);
        // Serialising twice gives the same bytes.
        assert_eq!(b.to_bytes().unwrap(), bytes);
    }

    #[test]
    fn tampered_boundaries_are_refused() {
        let bytes = sample().to_bytes().unwrap();
        // Flip one data bit: the digest no longer matches.
        let mut flipped = bytes.clone();
        let last = flipped.len() - 1;
        flipped[last] ^= 1;
        assert!(Boundary::from_bytes(&flipped).is_err());
        // Trailing bytes and truncation.
        let mut longer = bytes.clone();
        longer.push(0);
        assert!(Boundary::from_bytes(&longer).is_err());
        assert!(Boundary::from_bytes(&bytes[..bytes.len() - 8]).is_err());
        // Values beyond 2^62 cannot be written.
        let mut big = sample();
        big.sequences[0].values[0] = (1 << 62) + 1;
        assert!(big.to_bytes().is_err());
    }

    #[test]
    fn digests_chain_per_position_hashes() {
        let values = [1i64, 2, 3, 4];
        let expected = {
            let mut h = blake3::Hasher::new();
            h.update(&activation_hash(&[1, 2]));
            h.update(&activation_hash(&[3, 4]));
            *h.finalize().as_bytes()
        };
        assert_eq!(boundary_digest(&values, 2), expected);
        let single: Vec<u8> = [1i64, 2].iter().flat_map(|v| v.to_le_bytes()).collect();
        assert_eq!(activation_hash(&[1, 2]), *blake3::hash(&single).as_bytes());
    }
}
