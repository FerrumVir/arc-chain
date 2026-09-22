//! Read a GGUF file's metadata - and only its metadata - without a model
//! library, so startup can say what model it was given instead of assuming.
//!
//! Startup used to announce "Llama-2-7B, 32 layers, canonical INT8" for
//! whatever `--model` pointed at. The shape now comes from the file's own
//! header (`general.architecture`, `<arch>.block_count`, `general.name`), and
//! [`supported_adapter`] decides - explicitly - whether this node has a
//! conformant adapter for it. An architecture without one is refused, never
//! forced through the Llama path.
//!
//! Bounded: at most `MAX_KV` entries and `MAX_TENSORS` tensors, strings of at
//! most `MAX_STRING` bytes, arrays skipped element by element, so a hostile
//! header cannot make startup loop or allocate without limit. What it can
//! make startup hold is capped by those limits and by the file's own size,
//! which is not small. Two bounds are stricter than the Python reader's (1 MiB
//! strings instead of 16 MiB, at most 4,096 entries), so a file Python
//! accepts can still be refused here, never the reverse. Both refuse a
//! repeated metadata key or tensor name.

use std::collections::BTreeMap;
use std::io::Read;

const MAGIC: &[u8; 4] = b"GGUF";
const MAX_KV: u64 = 4_096;
const MAX_STRING: u64 = 1 << 20;
const MAX_ARRAY: u64 = 1 << 24;

#[derive(Debug, Clone, PartialEq)]
pub enum MetaValue {
    Uint(u64),
    Int(i64),
    Float(f64),
    Bool(bool),
    Str(String),
    /// Arrays are skipped; only their length is kept.
    Array {
        len: u64,
    },
}

#[derive(Debug, thiserror::Error)]
pub enum MetaError {
    #[error("not a GGUF file")]
    NotGguf,
    #[error("unsupported GGUF version {0}")]
    Version(u32),
    #[error("GGUF header exceeds a bound: {0}")]
    Bound(&'static str),
    #[error("GGUF value type {0} is unknown")]
    Type(u32),
    #[error("GGUF header repeats {0:?}")]
    Duplicate(String),
    #[error("reading the GGUF header: {0}")]
    Io(#[from] std::io::Error),
}

fn read_u32(r: &mut impl Read) -> Result<u32, MetaError> {
    let mut b = [0u8; 4];
    r.read_exact(&mut b)?;
    Ok(u32::from_le_bytes(b))
}

fn read_u64(r: &mut impl Read) -> Result<u64, MetaError> {
    let mut b = [0u8; 8];
    r.read_exact(&mut b)?;
    Ok(u64::from_le_bytes(b))
}

fn read_string(r: &mut impl Read) -> Result<String, MetaError> {
    let len = read_u64(r)?;
    if len > MAX_STRING {
        return Err(MetaError::Bound("string length"));
    }
    let mut buf = vec![0u8; len as usize];
    r.read_exact(&mut buf)?;
    Ok(String::from_utf8_lossy(&buf).into_owned())
}

fn read_scalar(r: &mut impl Read, ty: u32) -> Result<MetaValue, MetaError> {
    let mut b1 = [0u8; 1];
    let mut b2 = [0u8; 2];
    Ok(match ty {
        0 => {
            r.read_exact(&mut b1)?;
            MetaValue::Uint(b1[0] as u64)
        }
        1 => {
            r.read_exact(&mut b1)?;
            MetaValue::Int(b1[0] as i8 as i64)
        }
        2 => {
            r.read_exact(&mut b2)?;
            MetaValue::Uint(u16::from_le_bytes(b2) as u64)
        }
        3 => {
            r.read_exact(&mut b2)?;
            MetaValue::Int(i16::from_le_bytes(b2) as i64)
        }
        4 => MetaValue::Uint(read_u32(r)? as u64),
        5 => MetaValue::Int(read_u32(r)? as i32 as i64),
        6 => MetaValue::Float(f32::from_bits(read_u32(r)?) as f64),
        7 => {
            r.read_exact(&mut b1)?;
            MetaValue::Bool(b1[0] != 0)
        }
        8 => MetaValue::Str(read_string(r)?),
        10 => MetaValue::Uint(read_u64(r)?),
        11 => MetaValue::Int(read_u64(r)? as i64),
        12 => MetaValue::Float(f64::from_bits(read_u64(r)?)),
        other => return Err(MetaError::Type(other)),
    })
}

fn read_value(r: &mut impl Read, ty: u32) -> Result<MetaValue, MetaError> {
    if ty != 9 {
        return read_scalar(r, ty);
    }
    let elem = read_u32(r)?;
    let len = read_u64(r)?;
    if len > MAX_ARRAY || elem == 9 {
        return Err(MetaError::Bound("array"));
    }
    for _ in 0..len {
        read_scalar(r, elem)?;
    }
    Ok(MetaValue::Array { len })
}

/// The header key-values of a GGUF stream.
pub fn read_metadata(r: &mut impl Read) -> Result<BTreeMap<String, MetaValue>, MetaError> {
    read_prefix(r).map(|(_, metadata)| metadata)
}

/// Magic, version, the tensor count and the metadata entries.
fn read_prefix(r: &mut impl Read) -> Result<(u64, BTreeMap<String, MetaValue>), MetaError> {
    let mut magic = [0u8; 4];
    r.read_exact(&mut magic)?;
    if &magic != MAGIC {
        return Err(MetaError::NotGguf);
    }
    let version = read_u32(r)?;
    if !(2..=3).contains(&version) {
        return Err(MetaError::Version(version));
    }
    let tensor_count = read_u64(r)?;
    let kv_count = read_u64(r)?;
    if kv_count > MAX_KV {
        return Err(MetaError::Bound("metadata entries"));
    }
    let mut out = BTreeMap::new();
    for _ in 0..kv_count {
        let key = read_string(r)?;
        let ty = read_u32(r)?;
        let value = read_value(r, ty)?;
        // The Python reader refuses a repeated key: keeping either copy here
        // would describe a file the manifest derivation rejects.
        if out.contains_key(&key) {
            return Err(MetaError::Duplicate(key));
        }
        out.insert(key, value);
    }
    Ok((tensor_count, out))
}

const MAX_TENSORS: u64 = 65_536;
const MAX_DIMS: u32 = 4;

/// One tensor's header entry, dimensions in the file's own order (the
/// innermost, `ne0`, first).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TensorEntry {
    pub name: String,
    pub ggml_type: u32,
    pub dims: Vec<u64>,
}

/// The metadata and the tensor inventory: everything before the tensor data.
pub fn read_header(
    r: &mut impl Read,
) -> Result<(BTreeMap<String, MetaValue>, Vec<TensorEntry>), MetaError> {
    let (tensor_count, metadata) = read_prefix(r)?;
    if tensor_count > MAX_TENSORS {
        return Err(MetaError::Bound("tensor count"));
    }
    let mut tensors = Vec::with_capacity(tensor_count as usize);
    let mut names = std::collections::BTreeSet::new();
    for _ in 0..tensor_count {
        let name = read_string(r)?;
        if !names.insert(name.clone()) {
            return Err(MetaError::Duplicate(name));
        }
        let n_dims = read_u32(r)?;
        if !(1..=MAX_DIMS).contains(&n_dims) {
            return Err(MetaError::Bound("tensor dimensions"));
        }
        let dims = (0..n_dims)
            .map(|_| read_u64(r))
            .collect::<Result<Vec<_>, _>>()?;
        let ggml_type = read_u32(r)?;
        let _offset = read_u64(r)?;
        tensors.push(TensorEntry {
            name,
            ggml_type,
            dims,
        });
    }
    Ok((metadata, tensors))
}

pub fn read_header_from_path(
    path: &std::path::Path,
) -> Result<(BTreeMap<String, MetaValue>, Vec<TensorEntry>), MetaError> {
    let mut file = std::io::BufReader::new(std::fs::File::open(path)?);
    read_header(&mut file)
}

/// BLAKE3 over the tensor inventory, as the package manifest records it
/// (`tensors.inventory_blake3`): sorted by name; each entry is the name's
/// length (u32 LE) and bytes, the GGML type (u32 LE), the dimension count
/// (u32 LE) and each dimension (u64 LE) in file order.
pub fn inventory_digest(tensors: &[TensorEntry]) -> [u8; 32] {
    let mut sorted: Vec<&TensorEntry> = tensors.iter().collect();
    sorted.sort_by(|a, b| a.name.as_bytes().cmp(b.name.as_bytes()));
    let mut hasher = blake3::Hasher::new();
    for tensor in sorted {
        hasher.update(&(tensor.name.len() as u32).to_le_bytes());
        hasher.update(tensor.name.as_bytes());
        hasher.update(&tensor.ggml_type.to_le_bytes());
        hasher.update(&(tensor.dims.len() as u32).to_le_bytes());
        for dim in &tensor.dims {
            hasher.update(&dim.to_le_bytes());
        }
    }
    *hasher.finalize().as_bytes()
}

/// What startup needs to know about a model file.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ModelShape {
    pub architecture: String,
    pub name: Option<String>,
    pub block_count: u64,
}

pub fn model_shape(meta: &BTreeMap<String, MetaValue>) -> Option<ModelShape> {
    let architecture = match meta.get("general.architecture")? {
        MetaValue::Str(s) => s.clone(),
        _ => return None,
    };
    let block_count = match meta.get(&format!("{architecture}.block_count"))? {
        MetaValue::Uint(n) => *n,
        MetaValue::Int(n) if *n > 0 => *n as u64,
        _ => return None,
    };
    let name = match meta.get("general.name") {
        Some(MetaValue::Str(s)) => Some(s.clone()),
        _ => None,
    };
    Some(ModelShape {
        architecture,
        name,
        block_count,
    })
}

pub fn read_shape_from_path(path: &std::path::Path) -> Result<Option<ModelShape>, MetaError> {
    let mut file = std::io::BufReader::new(std::fs::File::open(path)?);
    Ok(model_shape(&read_metadata(&mut file)?))
}

/// Architectures this node has a conformant integer adapter for, and the
/// execution profile each runs under. Anything else is refused at startup -
/// an explicit, versioned list rather than an assumption that every model is
/// Llama.
pub fn supported_adapter(architecture: &str) -> Option<&'static str> {
    match architecture {
        "llama" => Some(crate::cached_integer_model::CANONICAL_REWARD_INFERENCE_PROFILE),
        _ => None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn string(buf: &mut Vec<u8>, s: &str) {
        buf.extend_from_slice(&(s.len() as u64).to_le_bytes());
        buf.extend_from_slice(s.as_bytes());
    }

    fn header(entries: &[(&str, u32, Vec<u8>)]) -> Vec<u8> {
        let mut buf = Vec::new();
        buf.extend_from_slice(MAGIC);
        buf.extend_from_slice(&3u32.to_le_bytes());
        buf.extend_from_slice(&0u64.to_le_bytes());
        buf.extend_from_slice(&(entries.len() as u64).to_le_bytes());
        for (key, ty, value) in entries {
            string(&mut buf, key);
            buf.extend_from_slice(&ty.to_le_bytes());
            buf.extend_from_slice(value);
        }
        buf
    }

    fn str_value(s: &str) -> Vec<u8> {
        let mut v = Vec::new();
        string(&mut v, s);
        v
    }

    /// A header with no metadata and one 4-element F32 tensor per name.
    fn with_tensors(names: &[&str]) -> Vec<u8> {
        let mut buf = Vec::new();
        buf.extend_from_slice(MAGIC);
        buf.extend_from_slice(&3u32.to_le_bytes());
        buf.extend_from_slice(&(names.len() as u64).to_le_bytes());
        buf.extend_from_slice(&0u64.to_le_bytes());
        for name in names {
            string(&mut buf, name);
            buf.extend_from_slice(&1u32.to_le_bytes());
            buf.extend_from_slice(&4u64.to_le_bytes());
            buf.extend_from_slice(&0u32.to_le_bytes());
            buf.extend_from_slice(&0u64.to_le_bytes());
        }
        buf
    }

    #[test]
    fn a_repeated_key_or_tensor_name_is_refused_as_the_python_reader_refuses_it() {
        let repeated = header(&[
            ("general.architecture", 8, str_value("llama")),
            ("general.architecture", 8, str_value("qwen2")),
        ]);
        assert!(matches!(
            read_metadata(&mut repeated.as_slice()),
            Err(MetaError::Duplicate(key)) if key == "general.architecture"
        ));
        let (_, tensors) = read_header(&mut with_tensors(&["a", "b"]).as_slice()).unwrap();
        assert_eq!(tensors.len(), 2);
        assert!(matches!(
            read_header(&mut with_tensors(&["a", "a"]).as_slice()),
            Err(MetaError::Duplicate(name)) if name == "a"
        ));
    }

    #[test]
    fn a_llama_header_yields_its_real_shape_and_a_supported_adapter() {
        let mut array = Vec::new();
        array.extend_from_slice(&4u32.to_le_bytes()); // u32 elements
        array.extend_from_slice(&3u64.to_le_bytes());
        for n in [1u32, 2, 3] {
            array.extend_from_slice(&n.to_le_bytes());
        }
        let bytes = header(&[
            ("general.architecture", 8, str_value("llama")),
            ("general.name", 8, str_value("Llama-2-13B")),
            ("llama.block_count", 4, 40u32.to_le_bytes().to_vec()),
            ("tokenizer.ggml.token_type", 9, array),
        ]);
        let meta = read_metadata(&mut bytes.as_slice()).unwrap();
        let shape = model_shape(&meta).unwrap();
        assert_eq!(shape.architecture, "llama");
        assert_eq!(shape.block_count, 40, "not an assumed 32");
        assert_eq!(shape.name.as_deref(), Some("Llama-2-13B"));
        assert!(supported_adapter(&shape.architecture).is_some());
    }

    #[test]
    fn another_architecture_has_no_adapter() {
        let bytes = header(&[
            ("general.architecture", 8, str_value("qwen2")),
            ("qwen2.block_count", 4, 28u32.to_le_bytes().to_vec()),
        ]);
        let shape = model_shape(&read_metadata(&mut bytes.as_slice()).unwrap()).unwrap();
        assert_eq!(shape.block_count, 28);
        assert_eq!(supported_adapter(&shape.architecture), None);
    }

    #[test]
    fn a_hostile_or_foreign_header_is_refused() {
        assert!(matches!(
            read_metadata(&mut &b"NOPE\x03\0\0\0"[..]),
            Err(MetaError::NotGguf)
        ));
        let mut huge = Vec::new();
        huge.extend_from_slice(MAGIC);
        huge.extend_from_slice(&3u32.to_le_bytes());
        huge.extend_from_slice(&0u64.to_le_bytes());
        huge.extend_from_slice(&u64::MAX.to_le_bytes());
        assert!(matches!(
            read_metadata(&mut huge.as_slice()),
            Err(MetaError::Bound(_))
        ));
        let long = header(&[("general.architecture", 8, {
            let mut v = Vec::new();
            v.extend_from_slice(&(MAX_STRING + 1).to_le_bytes());
            v
        })]);
        assert!(matches!(
            read_metadata(&mut long.as_slice()),
            Err(MetaError::Bound(_))
        ));
    }
}
