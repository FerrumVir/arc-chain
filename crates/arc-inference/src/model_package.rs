//! The model package manifest (`arc.model-package.v1`) on the node side.
//!
//! `scripts/arc_conformance/package_manifest.py` derives a manifest from an
//! artifact, a profile and a generation contract; a reviewer approves a
//! package by the manifest's hash. This module checks a pinned manifest
//! against what the node actually loaded, so a node never executes a package
//! other than the one approved: the manifest's own hash, the artifact bytes,
//! the profile and generation commitments, the graph, the tokenizer, the
//! tensor inventory and the KV cost must all be what this node measured.
//!
//! The manifest hash is BLAKE3 over Python's canonical JSON
//! (`json.dumps(sort_keys=True, separators=(",", ":"), ensure_ascii=True)`)
//! of every field but `manifest_blake3`. [`canonical_json`] reproduces that
//! encoding exactly, including Python's float `repr`.

use serde_json::Value;

pub const PACKAGE_SCHEMA: &str = "arc.model-package.v1";
/// A manifest is a few kilobytes; anything near this is not one.
pub const MAX_MANIFEST_BYTES: usize = 1 << 20;

#[derive(Debug, thiserror::Error)]
pub enum PackageError {
    #[error("package manifest exceeds {0} bytes")]
    TooLarge(usize),
    #[error("package manifest is not valid JSON: {0}")]
    Json(String),
    #[error("package manifest cannot be encoded canonically: {0}")]
    Encoding(String),
    #[error("package manifest is missing {0}")]
    Missing(String),
    #[error("package manifest {field} is {found}, but this node has {expected}")]
    Mismatch {
        field: String,
        expected: String,
        found: String,
    },
}

/// Python's `repr(float)`: the shortest digits that round-trip, fixed
/// notation for decimal exponents from -4 to 16, scientific otherwise with
/// a signed, at-least-two-digit exponent.
pub fn python_float_repr(value: f64) -> Result<String, PackageError> {
    if !value.is_finite() {
        return Err(PackageError::Encoding(format!("non-finite number {value}")));
    }
    if value == 0.0 {
        return Ok(if value.is_sign_negative() {
            "-0.0"
        } else {
            "0.0"
        }
        .to_string());
    }
    // Rust's `{:e}` prints the shortest round-trip digits: "d.ddde-7".
    let formatted = format!("{:e}", value.abs());
    let (mantissa, exponent) = formatted
        .split_once('e')
        .ok_or_else(|| PackageError::Encoding(formatted.clone()))?;
    let digits: String = mantissa.chars().filter(|c| *c != '.').collect();
    let exponent: i32 = exponent
        .parse()
        .map_err(|_| PackageError::Encoding(formatted.clone()))?;
    // The decimal point sits `decpt` digits into `digits`.
    let decpt = exponent + 1;
    let body = if decpt <= -4 || decpt > 16 {
        let (first, rest) = digits.split_at(1);
        let fraction = if rest.is_empty() {
            String::new()
        } else {
            format!(".{rest}")
        };
        let sign = if exponent < 0 { '-' } else { '+' };
        format!("{first}{fraction}e{sign}{:02}", exponent.abs())
    } else if decpt <= 0 {
        format!("0.{}{digits}", "0".repeat((-decpt) as usize))
    } else if decpt as usize >= digits.len() {
        format!("{digits}{}.0", "0".repeat(decpt as usize - digits.len()))
    } else {
        let (whole, fraction) = digits.split_at(decpt as usize);
        format!("{whole}.{fraction}")
    };
    Ok(if value < 0.0 {
        format!("-{body}")
    } else {
        body
    })
}

fn write_string(text: &str, out: &mut String) {
    out.push('"');
    for c in text.chars() {
        match c {
            '"' => out.push_str("\\\""),
            '\\' => out.push_str("\\\\"),
            '\n' => out.push_str("\\n"),
            '\r' => out.push_str("\\r"),
            '\t' => out.push_str("\\t"),
            '\u{08}' => out.push_str("\\b"),
            '\u{0c}' => out.push_str("\\f"),
            // ensure_ascii: everything outside ' '..='~' as \uXXXX, astral
            // characters as a surrogate pair, lowercase hex.
            c if !(' '..='~').contains(&c) => {
                let mut units = [0u16; 2];
                for unit in c.encode_utf16(&mut units) {
                    out.push_str(&format!("\\u{unit:04x}"));
                }
            }
            c => out.push(c),
        }
    }
    out.push('"');
}

fn write_value(value: &Value, out: &mut String) -> Result<(), PackageError> {
    match value {
        Value::Null => out.push_str("null"),
        Value::Bool(flag) => out.push_str(if *flag { "true" } else { "false" }),
        Value::Number(number) => {
            if let Some(unsigned) = number.as_u64() {
                out.push_str(&unsigned.to_string());
            } else if let Some(signed) = number.as_i64() {
                out.push_str(&signed.to_string());
            } else {
                let float = number
                    .as_f64()
                    .ok_or_else(|| PackageError::Encoding(number.to_string()))?;
                out.push_str(&python_float_repr(float)?);
            }
        }
        Value::String(text) => write_string(text, out),
        Value::Array(items) => {
            out.push('[');
            for (index, item) in items.iter().enumerate() {
                if index > 0 {
                    out.push(',');
                }
                write_value(item, out)?;
            }
            out.push(']');
        }
        Value::Object(map) => {
            // Python sorts keys by code point; UTF-8 byte order is the same.
            let mut keys: Vec<&String> = map.keys().collect();
            keys.sort();
            out.push('{');
            for (index, key) in keys.into_iter().enumerate() {
                if index > 0 {
                    out.push(',');
                }
                write_string(key, out);
                out.push(':');
                write_value(&map[key], out)?;
            }
            out.push('}');
        }
    }
    Ok(())
}

/// Python's canonical encoding of `value`.
pub fn canonical_json(value: &Value) -> Result<String, PackageError> {
    let mut out = String::new();
    write_value(value, &mut out)?;
    Ok(out)
}

/// BLAKE3 (hex) of the canonical encoding of every field but `manifest_blake3`.
pub fn manifest_body_blake3(manifest: &Value) -> Result<String, PackageError> {
    let mut body = manifest
        .as_object()
        .ok_or_else(|| PackageError::Json("the manifest is not an object".into()))?
        .clone();
    body.remove("manifest_blake3");
    let encoded = canonical_json(&Value::Object(body))?;
    Ok(blake3::hash(encoded.as_bytes()).to_hex().to_string())
}

/// What a node measured of the package it loaded.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct LoadedPackage {
    pub artifact_blake3: String,
    pub artifact_bytes: u64,
    pub profile_commitment: String,
    pub generation_commitment: String,
    pub n_layers: u64,
    pub d_model: u64,
    pub n_heads: u64,
    pub n_kv_heads: u64,
    pub d_head: u64,
    pub d_kv: u64,
    pub d_ff: u64,
    pub vocab_size: u64,
    pub max_seq: u64,
    pub tokenizer_tokens: u64,
    pub vocab_blake3: String,
    pub bos: u64,
    pub eos: Vec<u64>,
    pub tensor_count: u64,
    pub inventory_blake3: String,
    pub kv_bytes_per_position: u64,
}

fn field<'a>(manifest: &'a Value, path: &str) -> Result<&'a Value, PackageError> {
    path.split('.')
        .try_fold(manifest, |value, key| value.get(key))
        .ok_or_else(|| PackageError::Missing(path.to_string()))
}

fn expect_str(manifest: &Value, path: &str, expected: &str) -> Result<(), PackageError> {
    let found = field(manifest, path)?;
    if found.as_str() != Some(expected) {
        return Err(PackageError::Mismatch {
            field: path.to_string(),
            expected: expected.to_string(),
            found: found.to_string(),
        });
    }
    Ok(())
}

fn expect_u64(manifest: &Value, path: &str, expected: u64) -> Result<(), PackageError> {
    let found = field(manifest, path)?;
    if found.as_u64() != Some(expected) {
        return Err(PackageError::Mismatch {
            field: path.to_string(),
            expected: expected.to_string(),
            found: found.to_string(),
        });
    }
    Ok(())
}

/// Check the manifest at `bytes` is the one pinned by `pinned_blake3` and
/// describes exactly the package this node loaded. Every refusal names the
/// field.
pub fn verify_loaded_package(
    bytes: &[u8],
    pinned_blake3: &str,
    loaded: &LoadedPackage,
) -> Result<(), PackageError> {
    if bytes.len() > MAX_MANIFEST_BYTES {
        return Err(PackageError::TooLarge(MAX_MANIFEST_BYTES));
    }
    let manifest: Value =
        serde_json::from_slice(bytes).map_err(|error| PackageError::Json(error.to_string()))?;
    expect_str(&manifest, "schema", PACKAGE_SCHEMA)?;
    let recomputed = manifest_body_blake3(&manifest)?;
    expect_str(&manifest, "manifest_blake3", &recomputed)?;
    if !recomputed.eq_ignore_ascii_case(pinned_blake3.trim_start_matches("0x")) {
        return Err(PackageError::Mismatch {
            field: "manifest_blake3 (pinned by the qualification record)".into(),
            expected: pinned_blake3.to_string(),
            found: recomputed,
        });
    }
    expect_str(&manifest, "artifact.format", "gguf")?;
    expect_str(&manifest, "artifact.blake3", &loaded.artifact_blake3)?;
    expect_u64(&manifest, "artifact.bytes", loaded.artifact_bytes)?;
    expect_str(
        &manifest,
        "execution.profile_commitment",
        &loaded.profile_commitment,
    )?;
    expect_str(
        &manifest,
        "generation.commitment",
        &loaded.generation_commitment,
    )?;
    for (path, value) in [
        ("graph.n_layers", loaded.n_layers),
        ("graph.d_model", loaded.d_model),
        ("graph.n_heads", loaded.n_heads),
        ("graph.n_kv_heads", loaded.n_kv_heads),
        ("graph.d_head", loaded.d_head),
        ("graph.d_kv", loaded.d_kv),
        ("graph.d_ff", loaded.d_ff),
        ("graph.vocab_size", loaded.vocab_size),
        ("graph.max_seq", loaded.max_seq),
        ("tokenizer.tokens", loaded.tokenizer_tokens),
        ("tokenizer.bos", loaded.bos),
        ("tensors.count", loaded.tensor_count),
        ("memory.kv_bytes_per_position", loaded.kv_bytes_per_position),
    ] {
        expect_u64(&manifest, path, value)?;
    }
    expect_str(&manifest, "tokenizer.vocab_blake3", &loaded.vocab_blake3)?;
    expect_str(
        &manifest,
        "tensors.inventory_blake3",
        &loaded.inventory_blake3,
    )?;
    let eos = field(&manifest, "tokenizer.eos")?;
    let listed: Option<Vec<u64>> = eos
        .as_array()
        .map(|ids| ids.iter().filter_map(Value::as_u64).collect());
    if listed.as_ref() != Some(&loaded.eos) {
        return Err(PackageError::Mismatch {
            field: "tokenizer.eos".into(),
            expected: format!("{:?}", loaded.eos),
            found: eos.to_string(),
        });
    }
    // The engine stops only on the EOS ids above. A tokenizer that also
    // declares end-of-turn/end-of-message ids would keep generating past the
    // model's real stop, so such a package is refused; padding is harmless.
    let ignored = field(&manifest, "tokenizer.ignored_special_ids")?;
    let stops_ignored: Vec<&str> = ignored
        .as_object()
        .map(|ids| {
            ids.keys()
                .map(String::as_str)
                .filter(|key| matches!(*key, "eot_token_id" | "eom_token_id"))
                .collect()
        })
        .unwrap_or_default();
    if !stops_ignored.is_empty() {
        return Err(PackageError::Mismatch {
            field: "tokenizer.ignored_special_ids".into(),
            expected: "no end-of-generation ids the engine does not honour".into(),
            found: stops_ignored.join(", "),
        });
    }
    let outputs = field(&manifest, "supported_outputs")?;
    if outputs.as_array().map(|items| items.as_slice()) != Some(&[Value::from("token_ids")][..]) {
        return Err(PackageError::Mismatch {
            field: "supported_outputs".into(),
            expected: "[\"token_ids\"]".into(),
            found: outputs.to_string(),
        });
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    const CANONICAL_MANIFEST: &str =
        include_str!("../../../docs/protocol/packages/llama-2-7b-q4km.manifest.json");
    const CANONICAL_MANIFEST_BLAKE3: &str =
        "fecaf64104b3be988ff5f3ddf3c38e6739787f9cd390b64835dc59aecb0309bf";

    #[test]
    fn floats_are_written_as_python_writes_them() {
        for (value, python) in [
            (10000.0, "10000.0"),
            (9.999999974752427e-07, "9.999999974752427e-07"),
            (1e-05, "1e-05"),
            (0.0001, "0.0001"),
            (1e16, "1e+16"),
            (1e15, "1000000000000000.0"),
            (123.456, "123.456"),
            (0.5, "0.5"),
            (-2.5, "-2.5"),
            (0.0, "0.0"),
            (1.5e300, "1.5e+300"),
            (f64::from(1e-6f32), "9.999999974752427e-07"),
        ] {
            assert_eq!(python_float_repr(value).unwrap(), python, "{value:e}");
        }
        assert!(python_float_repr(f64::NAN).is_err());
    }

    #[test]
    fn strings_and_keys_are_written_as_python_writes_them() {
        // Expected: Python's own `json.dumps(value, sort_keys=True,
        // separators=(",", ":"), ensure_ascii=True)`, pasted verbatim. Every
        // character outside ' '..='~' is escaped, astral ones as surrogates.
        let value = serde_json::json!({"b": "é\n\"\\\u{7f}", "a": [true, null, 1, -2], "😀": 1});
        assert_eq!(
            canonical_json(&value).unwrap(),
            r#"{"a":[true,null,1,-2],"b":"\u00e9\n\"\\\u007f","\ud83d\ude00":1}"#
        );
    }

    #[test]
    fn the_committed_manifest_hashes_to_the_value_python_recorded() {
        let manifest: Value = serde_json::from_str(CANONICAL_MANIFEST).unwrap();
        assert_eq!(manifest["manifest_blake3"], CANONICAL_MANIFEST_BLAKE3);
        assert_eq!(
            manifest_body_blake3(&manifest).unwrap(),
            CANONICAL_MANIFEST_BLAKE3
        );
    }

    fn loaded() -> LoadedPackage {
        let manifest: Value = serde_json::from_str(CANONICAL_MANIFEST).unwrap();
        let text = |path: &str| {
            field(&manifest, path)
                .unwrap()
                .as_str()
                .unwrap()
                .to_string()
        };
        let number = |path: &str| field(&manifest, path).unwrap().as_u64().unwrap();
        LoadedPackage {
            artifact_blake3: text("artifact.blake3"),
            artifact_bytes: number("artifact.bytes"),
            profile_commitment: text("execution.profile_commitment"),
            generation_commitment: text("generation.commitment"),
            n_layers: number("graph.n_layers"),
            d_model: number("graph.d_model"),
            n_heads: number("graph.n_heads"),
            n_kv_heads: number("graph.n_kv_heads"),
            d_head: number("graph.d_head"),
            d_kv: number("graph.d_kv"),
            d_ff: number("graph.d_ff"),
            vocab_size: number("graph.vocab_size"),
            max_seq: number("graph.max_seq"),
            tokenizer_tokens: number("tokenizer.tokens"),
            vocab_blake3: text("tokenizer.vocab_blake3"),
            bos: number("tokenizer.bos"),
            eos: vec![2],
            tensor_count: number("tensors.count"),
            inventory_blake3: text("tensors.inventory_blake3"),
            kv_bytes_per_position: number("memory.kv_bytes_per_position"),
        }
    }

    #[test]
    fn the_pinned_package_matching_what_was_loaded_is_accepted() {
        verify_loaded_package(
            CANONICAL_MANIFEST.as_bytes(),
            CANONICAL_MANIFEST_BLAKE3,
            &loaded(),
        )
        .unwrap();
    }

    #[test]
    fn any_difference_from_what_was_loaded_is_refused_by_name() {
        let refused = |loaded: LoadedPackage, field: &str| match verify_loaded_package(
            CANONICAL_MANIFEST.as_bytes(),
            CANONICAL_MANIFEST_BLAKE3,
            &loaded,
        ) {
            Err(PackageError::Mismatch { field: named, .. }) => assert_eq!(named, field),
            other => panic!("expected a mismatch on {field}, got {other:?}"),
        };
        refused(
            LoadedPackage {
                artifact_blake3: "00".repeat(32),
                ..loaded()
            },
            "artifact.blake3",
        );
        refused(
            LoadedPackage {
                artifact_bytes: 1,
                ..loaded()
            },
            "artifact.bytes",
        );
        refused(
            LoadedPackage {
                n_layers: 31,
                ..loaded()
            },
            "graph.n_layers",
        );
        refused(
            LoadedPackage {
                vocab_blake3: "11".repeat(32),
                ..loaded()
            },
            "tokenizer.vocab_blake3",
        );
        refused(
            LoadedPackage {
                inventory_blake3: "22".repeat(32),
                ..loaded()
            },
            "tensors.inventory_blake3",
        );
        refused(
            LoadedPackage {
                eos: vec![2, 3],
                ..loaded()
            },
            "tokenizer.eos",
        );
        refused(
            LoadedPackage {
                generation_commitment: "33".repeat(32),
                ..loaded()
            },
            "generation.commitment",
        );
    }

    #[test]
    fn a_package_whose_tokenizer_stops_where_the_engine_does_not_is_refused() {
        // A correctly hashed manifest for a tokenizer that declares an
        // end-of-turn id: approving it would not make the engine stop there.
        let mut manifest: Value = serde_json::from_str(CANONICAL_MANIFEST).unwrap();
        manifest["tokenizer"]["ignored_special_ids"] = serde_json::json!({"eot_token_id": 7});
        let hash = manifest_body_blake3(&manifest).unwrap();
        manifest["manifest_blake3"] = Value::from(hash.clone());
        let bytes = serde_json::to_vec(&manifest).unwrap();
        match verify_loaded_package(&bytes, &hash, &loaded()) {
            Err(PackageError::Mismatch { field, found, .. }) => {
                assert_eq!(field, "tokenizer.ignored_special_ids");
                assert_eq!(found, "eot_token_id");
            }
            other => panic!("expected the tokenizer refusal, got {other:?}"),
        }
        // Padding does not affect stopping.
        manifest["tokenizer"]["ignored_special_ids"] = serde_json::json!({"padding_token_id": 0});
        let hash = manifest_body_blake3(&manifest).unwrap();
        manifest["manifest_blake3"] = Value::from(hash.clone());
        let bytes = serde_json::to_vec(&manifest).unwrap();
        verify_loaded_package(&bytes, &hash, &loaded()).unwrap();
    }

    #[test]
    fn an_edited_or_unpinned_manifest_is_refused() {
        // Another package entirely.
        match verify_loaded_package(CANONICAL_MANIFEST.as_bytes(), &"44".repeat(32), &loaded()) {
            Err(PackageError::Mismatch { field, .. }) => assert!(field.contains("pinned")),
            other => panic!("expected the pin to refuse, got {other:?}"),
        }
        // A field edited without re-hashing.
        let edited = CANONICAL_MANIFEST.replace("\"max_seq\": 4096", "\"max_seq\": 8192");
        assert_ne!(edited, CANONICAL_MANIFEST);
        match verify_loaded_package(edited.as_bytes(), CANONICAL_MANIFEST_BLAKE3, &loaded()) {
            Err(PackageError::Mismatch { field, .. }) => assert_eq!(field, "manifest_blake3"),
            other => panic!("expected the self-hash to refuse, got {other:?}"),
        }
        assert!(matches!(
            verify_loaded_package(b"not json", CANONICAL_MANIFEST_BLAKE3, &loaded()),
            Err(PackageError::Json(_))
        ));
    }
}
