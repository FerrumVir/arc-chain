//! Minimal reader for Hugging Face safetensors files (BF16 tensors only).
//!
//! Layout: an 8-byte little-endian header length `N`, `N` bytes of JSON
//! (`name -> {dtype, shape, data_offsets: [begin, end]}` plus an optional
//! `__metadata__` object), then the data region. Offsets are relative to the
//! start of the data region. Every offset and length is validated before any
//! tensor is read.

use std::collections::BTreeMap;
use std::fs::File;
use std::io::{Read, Seek, SeekFrom};
use std::path::{Path, PathBuf};

use super::ModernError;

/// Refuse headers larger than this; real files carry a few tens of KiB.
const MAX_HEADER_BYTES: u64 = 64 << 20;

/// One tensor's metadata, with absolute file offsets.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TensorInfo {
    pub dtype: String,
    pub shape: Vec<usize>,
    /// Absolute byte offset of the first element.
    pub begin: u64,
    /// Absolute byte offset one past the last element.
    pub end: u64,
}

impl TensorInfo {
    /// Number of elements.
    pub fn elements(&self) -> usize {
        self.shape.iter().product()
    }
}

/// An opened safetensors file: its path and its validated tensor table.
#[derive(Debug, Clone)]
pub struct SafetensorsFile {
    pub path: PathBuf,
    pub tensors: BTreeMap<String, TensorInfo>,
}

fn element_bytes(dtype: &str) -> Option<u64> {
    match dtype {
        "BF16" | "F16" | "I16" | "U16" => Some(2),
        "F32" | "I32" | "U32" => Some(4),
        "F64" | "I64" | "U64" => Some(8),
        "I8" | "U8" | "BOOL" => Some(1),
        _ => None,
    }
}

impl SafetensorsFile {
    /// Read and validate the header of `path`.
    pub fn open(path: &Path) -> Result<Self, ModernError> {
        let context = path.display().to_string();
        let mut file = File::open(path).map_err(|e| ModernError::io(&context, e))?;
        let file_len = file
            .metadata()
            .map_err(|e| ModernError::io(&context, e))?
            .len();
        let mut len_bytes = [0u8; 8];
        file.read_exact(&mut len_bytes)
            .map_err(|e| ModernError::io(&context, e))?;
        let header_len = u64::from_le_bytes(len_bytes);
        if header_len > MAX_HEADER_BYTES || 8 + header_len > file_len {
            return Err(ModernError::Invalid(format!(
                "{context}: safetensors header length {header_len} is invalid"
            )));
        }
        let mut header = vec![0u8; header_len as usize];
        file.read_exact(&mut header)
            .map_err(|e| ModernError::io(&context, e))?;
        let value: serde_json::Value = serde_json::from_slice(&header)
            .map_err(|e| ModernError::Invalid(format!("{context}: header JSON: {e}")))?;
        let object = value
            .as_object()
            .ok_or_else(|| ModernError::Invalid(format!("{context}: header is not an object")))?;
        let data_start = 8 + header_len;
        let data_len = file_len - data_start;
        let mut tensors = BTreeMap::new();
        for (name, entry) in object {
            if name == "__metadata__" {
                continue;
            }
            let bad =
                |what: &str| ModernError::Invalid(format!("{context}: tensor {name}: {what}"));
            let dtype = entry
                .get("dtype")
                .and_then(|d| d.as_str())
                .ok_or_else(|| bad("missing dtype"))?
                .to_string();
            let shape: Vec<usize> = entry
                .get("shape")
                .and_then(|s| s.as_array())
                .ok_or_else(|| bad("missing shape"))?
                .iter()
                .map(|d| d.as_u64().and_then(|v| usize::try_from(v).ok()))
                .collect::<Option<Vec<usize>>>()
                .ok_or_else(|| bad("shape is not a list of sizes"))?;
            let offsets = entry
                .get("data_offsets")
                .and_then(|o| o.as_array())
                .ok_or_else(|| bad("missing data_offsets"))?;
            let (Some(begin), Some(end)) = (
                offsets.first().and_then(|v| v.as_u64()),
                offsets.get(1).and_then(|v| v.as_u64()),
            ) else {
                return Err(bad("data_offsets must be two integers"));
            };
            if offsets.len() != 2 || begin > end || end > data_len {
                return Err(bad("data_offsets out of range"));
            }
            let width = element_bytes(&dtype).ok_or_else(|| bad("unsupported dtype"))?;
            let elements = shape
                .iter()
                .try_fold(1u64, |acc, &d| acc.checked_mul(d as u64))
                .ok_or_else(|| bad("shape overflows"))?;
            if elements.checked_mul(width) != Some(end - begin) {
                return Err(bad("byte length does not match dtype and shape"));
            }
            tensors.insert(
                name.clone(),
                TensorInfo {
                    dtype,
                    shape,
                    begin: data_start + begin,
                    end: data_start + end,
                },
            );
        }
        Ok(Self {
            path: path.to_path_buf(),
            tensors,
        })
    }

    /// Read a BF16 tensor's raw 16-bit patterns.
    pub fn read_bf16(&self, name: &str) -> Result<Vec<u16>, ModernError> {
        let info = self
            .tensors
            .get(name)
            .ok_or_else(|| ModernError::Invalid(format!("tensor {name} not found")))?;
        if info.dtype != "BF16" {
            return Err(ModernError::Invalid(format!(
                "tensor {name} is {}, not BF16",
                info.dtype
            )));
        }
        let context = self.path.display().to_string();
        let mut file = File::open(&self.path).map_err(|e| ModernError::io(&context, e))?;
        file.seek(SeekFrom::Start(info.begin))
            .map_err(|e| ModernError::io(&context, e))?;
        let len = usize::try_from(info.end - info.begin)
            .map_err(|_| ModernError::Invalid(format!("tensor {name} is too large")))?;
        let mut bytes = vec![0u8; len];
        file.read_exact(&mut bytes)
            .map_err(|e| ModernError::io(&context, e))?;
        Ok(bytes
            .chunks_exact(2)
            .map(|pair| u16::from_le_bytes([pair[0], pair[1]]))
            .collect())
    }
}

/// Serialise BF16 tensors into a safetensors file (used to build test models).
pub fn write_bf16_safetensors(
    path: &Path,
    tensors: &[(String, Vec<usize>, Vec<u16>)],
) -> Result<(), ModernError> {
    let mut header = serde_json::Map::new();
    let mut offset = 0u64;
    for (name, shape, values) in tensors {
        let elements: usize = shape.iter().product();
        if elements != values.len() {
            return Err(ModernError::Invalid(format!(
                "tensor {name}: shape does not match data"
            )));
        }
        let end = offset + 2 * values.len() as u64;
        header.insert(
            name.clone(),
            serde_json::json!({"dtype": "BF16", "shape": shape, "data_offsets": [offset, end]}),
        );
        offset = end;
    }
    let header_text = serde_json::to_string(&serde_json::Value::Object(header))
        .map_err(|e| ModernError::Invalid(format!("safetensors header: {e}")))?;
    let mut bytes = Vec::with_capacity(8 + header_text.len() + offset as usize);
    bytes.extend_from_slice(&(header_text.len() as u64).to_le_bytes());
    bytes.extend_from_slice(header_text.as_bytes());
    for (_, _, values) in tensors {
        for v in values {
            bytes.extend_from_slice(&v.to_le_bytes());
        }
    }
    std::fs::write(path, bytes).map_err(|e| ModernError::io(&path.display().to_string(), e))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn temp_path(name: &str) -> PathBuf {
        std::env::temp_dir().join(format!(
            "arc-modern-safetensors-{}-{name}",
            std::process::id()
        ))
    }

    #[test]
    fn round_trips_bf16_tensors_and_validates_offsets() {
        let path = temp_path("roundtrip.safetensors");
        let tensors = vec![
            (
                "a.weight".to_string(),
                vec![2, 3],
                vec![1u16, 2, 3, 4, 5, 0xFFFF],
            ),
            ("b".to_string(), vec![1], vec![0x3F80u16]),
        ];
        write_bf16_safetensors(&path, &tensors).unwrap();
        let file = SafetensorsFile::open(&path).unwrap();
        assert_eq!(file.tensors.len(), 2);
        assert_eq!(file.tensors["a.weight"].shape, vec![2, 3]);
        assert_eq!(file.tensors["a.weight"].elements(), 6);
        assert_eq!(
            file.read_bf16("a.weight").unwrap(),
            vec![1, 2, 3, 4, 5, 0xFFFF]
        );
        assert_eq!(file.read_bf16("b").unwrap(), vec![0x3F80]);
        assert!(file.read_bf16("missing").is_err());
        // Truncating the data region makes the declared offsets invalid.
        let bytes = std::fs::read(&path).unwrap();
        std::fs::write(&path, &bytes[..bytes.len() - 1]).unwrap();
        assert!(SafetensorsFile::open(&path).is_err());
        std::fs::remove_file(&path).unwrap();
    }
}
