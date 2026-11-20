use serde::{Deserialize, Deserializer, Serialize, Serializer};
use std::fmt;

#[derive(Clone, Copy, PartialEq, Eq, Hash, Default)]
pub struct Hash256(pub [u8; 32]);

impl Serialize for Hash256 {
    fn serialize<S: Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
        if serializer.is_human_readable() {
            serializer.serialize_str(&self.to_hex())
        } else {
            self.0.serialize(serializer)
        }
    }
}

impl<'de> Deserialize<'de> for Hash256 {
    fn deserialize<D: Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        if deserializer.is_human_readable() {
            let s = String::deserialize(deserializer)?;
            let hex_str = s.strip_prefix("0x").unwrap_or(&s);
            Self::from_hex(hex_str).map_err(serde::de::Error::custom)
        } else {
            let bytes = <[u8; 32]>::deserialize(deserializer)?;
            Ok(Self(bytes))
        }
    }
}

impl Hash256 {
    pub const ZERO: Self = Self([0u8; 32]);

    pub fn as_bytes(&self) -> &[u8; 32] {
        &self.0
    }

    pub fn to_hex(&self) -> String {
        hex::encode(self.0)
    }

    pub fn from_hex(s: &str) -> Result<Self, hex::FromHexError> {
        let bytes = hex::decode(s)?;
        if bytes.len() != 32 {
            return Err(hex::FromHexError::InvalidStringLength);
        }
        let mut arr = [0u8; 32];
        arr.copy_from_slice(&bytes);
        Ok(Self(arr))
    }
}

impl fmt::Debug for Hash256 {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "0x{}", &self.to_hex()[..16])
    }
}

impl fmt::Display for Hash256 {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "0x{}", self.to_hex())
    }
}

impl AsRef<[u8]> for Hash256 {
    fn as_ref(&self) -> &[u8] {
        &self.0
    }
}

#[inline]
pub fn hash_bytes(data: &[u8]) -> Hash256 {
    Hash256(*blake3::hash(data).as_bytes())
}

#[inline]
pub fn hash_pair(left: &Hash256, right: &Hash256) -> Hash256 {
    let mut hasher = blake3::Hasher::new();
    hasher.update(&left.0);
    hasher.update(&right.0);
    Hash256(*hasher.finalize().as_bytes())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_hash_deterministic() {
        let a = hash_bytes(b"hello world");
        let b = hash_bytes(b"hello world");
        assert_eq!(a, b);
    }

    #[test]
    fn test_hash_different_inputs() {
        let a = hash_bytes(b"hello");
        let b = hash_bytes(b"world");
        assert_ne!(a, b);
    }

    #[test]
    fn test_hash_pair_order_matters() {
        let a = hash_bytes(b"left");
        let b = hash_bytes(b"right");
        assert_ne!(hash_pair(&a, &b), hash_pair(&b, &a));
    }

    #[test]
    fn test_hex_roundtrip() {
        let h = hash_bytes(b"test");
        let hex_str = h.to_hex();
        let recovered = Hash256::from_hex(&hex_str).unwrap();
        assert_eq!(h, recovered);
    }
}
