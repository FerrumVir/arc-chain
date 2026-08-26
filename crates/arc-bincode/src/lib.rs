//! Maintained compatibility facade for ARC's historical bincode-v1 wire format.
//!
//! ARC hashes, signatures, WAL records, snapshots, and peer messages all commit
//! to the bytes emitted by bincode 1's free `serialize` function. The bincode
//! project is unmaintained, so this crate keeps the tiny API ARC uses while
//! delegating serde encoding to the maintained `cu-bincode` fork. Its legacy
//! configuration is deliberately bincode compatible: little-endian fixed-width
//! integers, `u64` sequence lengths, and `u32` enum tags.

use serde::{Deserialize, Serialize};

/// Maximum encoded value size on compatibility persistence paths.
///
/// This matches the node's maximum state-sync response size. Unlike bincode 1,
/// even the compatibility API never permits a forged length to request an
/// unbounded allocation.
const MAX_COMPAT_VALUE_BYTES: usize = 256 * 1024 * 1024;

fn compat_config() -> impl cu_bincode::config::Config {
    cu_bincode::config::legacy().with_limit::<MAX_COMPAT_VALUE_BYTES>()
}

/// Serialization or deserialization failure.
#[derive(Debug, thiserror::Error)]
pub enum Error {
    #[error("wire serialization failed: {0}")]
    Serialize(#[source] cu_bincode::error::EncodeError),
    #[error("wire deserialization failed: {0}")]
    Deserialize(#[source] cu_bincode::error::DecodeError),
    #[error("wire payload is {actual} bytes; limit is {limit}")]
    InputLimit { actual: usize, limit: usize },
    #[error("trailing bytes remain after wire value")]
    TrailingBytes,
}

/// Compatibility result type retained for existing ARC call sites.
pub type Result<T> = std::result::Result<T, Error>;

/// Serialize with bincode 1's free-function wire contract.
pub fn serialize<T>(value: &T) -> Result<Vec<u8>>
where
    T: Serialize + ?Sized,
{
    // Passing a reference supports unsized values such as slices while
    // producing the same bytes as serializing the value itself.
    cu_bincode::serde::encode_to_vec(value, compat_config()).map_err(Error::Serialize)
}

/// Deserialize with bincode 1's free-function contract.
///
/// Trailing bytes remain accepted for compatibility. Untrusted network/RPC
/// inputs must use [`deserialize_limited_exact`] instead.
pub fn deserialize<'de, T>(bytes: &'de [u8]) -> Result<T>
where
    T: Deserialize<'de>,
{
    cu_bincode::serde::borrow_decode_from_slice(bytes, compat_config())
        .map(|(value, _consumed)| value)
        .map_err(Error::Deserialize)
}

/// Deserialize an untrusted payload with a compile-time allocation/input cap
/// and reject any ignored suffix.
pub fn deserialize_limited_exact<'de, T, const LIMIT: usize>(bytes: &'de [u8]) -> Result<T>
where
    T: Deserialize<'de>,
{
    if bytes.len() > LIMIT {
        return Err(Error::InputLimit {
            actual: bytes.len(),
            limit: LIMIT,
        });
    }

    let (value, consumed) = cu_bincode::serde::borrow_decode_from_slice(
        bytes,
        cu_bincode::config::legacy().with_limit::<LIMIT>(),
    )
    .map_err(Error::Deserialize)?;
    if consumed != bytes.len() {
        return Err(Error::TrailingBytes);
    }
    Ok(value)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[derive(Debug, PartialEq, Serialize, Deserialize)]
    enum Choice {
        Empty,
        Number(u32),
    }

    #[derive(Debug, PartialEq, Serialize, Deserialize)]
    struct Fixture {
        enabled: bool,
        count: u64,
        label: String,
        bytes: Vec<u8>,
        choice: Choice,
    }

    #[test]
    fn emits_frozen_bincode_v1_fixture() {
        let value = Fixture {
            enabled: true,
            count: 0x0102_0304_0506_0708,
            label: "arc".to_string(),
            bytes: vec![0xaa, 0xbb],
            choice: Choice::Number(7),
        };
        let encoded = serialize(&value).unwrap();
        assert_eq!(
            hex::encode(&encoded),
            "01080706050403020103000000000000006172630200000000000000aabb0100000007000000"
        );
        assert_eq!(deserialize::<Fixture>(&encoded).unwrap(), value);
    }

    #[test]
    fn limited_exact_rejects_suffix_and_forged_allocation() {
        let mut encoded = serialize(&42u64).unwrap();
        encoded.push(0xff);
        assert!(matches!(
            deserialize_limited_exact::<u64, 64>(&encoded),
            Err(Error::TrailingBytes)
        ));

        let forged_vec_len = u64::MAX.to_le_bytes();
        assert!(matches!(
            deserialize_limited_exact::<Vec<u8>, 64>(&forged_vec_len),
            Err(Error::Deserialize(_))
        ));
    }

    #[test]
    fn compatibility_deserialize_retains_trailing_byte_behavior() {
        let mut encoded = serialize(&7u64).unwrap();
        encoded.extend_from_slice(&[0xaa, 0xbb]);
        assert_eq!(deserialize::<u64>(&encoded).unwrap(), 7);
    }
}
