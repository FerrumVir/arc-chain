//! Exact activation compression for stage boundaries.
//!
//! The hidden state that crosses a pipeline boundary is the residual stream:
//! Q16 fixed point held in `i64`. The next stage adds its attention and FFN
//! outputs into it at full precision, so quantizing it to INT8 or INT16 on the
//! wire would change every later bit. Instead this codec sends exactly the
//! bits the values use and no more:
//!
//! 1. zigzag-map each `i64` to a `u64` (small magnitudes of either sign become
//!    small numbers);
//! 2. split the vector into blocks of [`BLOCK`] values;
//! 3. per block, store the bit width of the widest value (one byte) and then
//!    every value at that width, LSB first.
//!
//! Decoding is the exact inverse, so `decode(encode(x)) == x` for every input,
//! including `i64::MIN` and `i64::MAX`. When packing would not save space the
//! encoder falls back to raw little-endian `i64`.
//!
//! The commitment is always BLAKE3 over the canonical little-endian `i64`
//! bytes — the same digest [`crate::distributed::serialize_activations`]
//! produces — so the codec choice never changes a per-stage commitment.

use arc_crypto::Hash256;

/// Values per bit-width block. 128 keeps the width table under 1% of a
/// typical packed payload while letting a single outlier only widen its own
/// block.
pub const BLOCK: usize = 128;

/// Codec identifiers carried in the entry header.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
#[repr(u8)]
pub enum Codec {
    /// Little-endian `i64`, 8 bytes per value. The legacy wire format.
    RawI64 = 0,
    /// Zigzag + per-block bit packing (this module).
    BitPack = 1,
}

impl Codec {
    pub fn from_u8(v: u8) -> Option<Self> {
        match v {
            0 => Some(Codec::RawI64),
            1 => Some(Codec::BitPack),
            _ => None,
        }
    }
}

/// Which codec the encoder may use.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum CodecChoice {
    /// Always raw `i64` (baseline for measurements).
    Raw,
    /// Bit-pack unless raw is smaller.
    Auto,
}

#[derive(Debug, thiserror::Error, PartialEq, Eq)]
pub enum CodecError {
    #[error("unknown codec id {0}")]
    UnknownCodec(u8),
    #[error("block width {0} exceeds 64 bits")]
    BadWidth(u8),
    #[error("payload length {got} does not match the {expected} bytes its header implies")]
    LengthMismatch { got: usize, expected: usize },
}

#[inline]
fn zigzag(v: i64) -> u64 {
    ((v << 1) ^ (v >> 63)) as u64
}

#[inline]
fn unzigzag(u: u64) -> i64 {
    ((u >> 1) as i64) ^ -((u & 1) as i64)
}

#[inline]
fn width_of(u: u64) -> u8 {
    (64 - u.leading_zeros()) as u8
}

/// Bytes a bit-packed payload of `n` values with these block widths occupies.
fn packed_len(n: usize, widths: &[u8]) -> usize {
    let mut bits: usize = 0;
    for (b, &w) in widths.iter().enumerate() {
        let len = BLOCK.min(n - b * BLOCK);
        bits += len * w as usize;
    }
    widths.len() + bits.div_ceil(8)
}

/// Encode `values` into `out` (appending). Returns the codec used.
pub fn encode_into(values: &[i64], choice: CodecChoice, out: &mut Vec<u8>) -> Codec {
    let raw_len = values.len() * 8;
    if choice == CodecChoice::Raw {
        write_raw(values, out);
        return Codec::RawI64;
    }
    let n_blocks = values.len().div_ceil(BLOCK);
    let mut widths = Vec::with_capacity(n_blocks);
    for block in values.chunks(BLOCK) {
        let mut acc = 0u64;
        for &v in block {
            acc |= zigzag(v);
        }
        widths.push(width_of(acc));
    }
    if packed_len(values.len(), &widths) >= raw_len {
        write_raw(values, out);
        return Codec::RawI64;
    }
    out.reserve(packed_len(values.len(), &widths));
    out.extend_from_slice(&widths);
    let mut acc: u128 = 0;
    let mut count: u32 = 0;
    for (block, &w) in values.chunks(BLOCK).zip(&widths) {
        if w == 0 {
            continue;
        }
        for &v in block {
            acc |= (zigzag(v) as u128) << count;
            count += w as u32;
            if count >= 64 {
                out.extend_from_slice(&(acc as u64).to_le_bytes());
                acc >>= 64;
                count -= 64;
            }
        }
    }
    let tail = count.div_ceil(8) as usize;
    out.extend_from_slice(&(acc as u64).to_le_bytes()[..tail]);
    Codec::BitPack
}

fn write_raw(values: &[i64], out: &mut Vec<u8>) {
    out.reserve(values.len() * 8);
    for &v in values {
        out.extend_from_slice(&v.to_le_bytes());
    }
}

/// Decode `n` values from `payload`, which must be exactly the bytes
/// [`encode_into`] wrote for them.
pub fn decode(codec: u8, payload: &[u8], n: usize) -> Result<Vec<i64>, CodecError> {
    let codec = Codec::from_u8(codec).ok_or(CodecError::UnknownCodec(codec))?;
    match codec {
        Codec::RawI64 => {
            if payload.len() != n * 8 {
                return Err(CodecError::LengthMismatch {
                    got: payload.len(),
                    expected: n * 8,
                });
            }
            Ok(payload
                .chunks_exact(8)
                .map(|c| i64::from_le_bytes(c.try_into().expect("chunk of 8")))
                .collect())
        }
        Codec::BitPack => {
            let n_blocks = n.div_ceil(BLOCK);
            if payload.len() < n_blocks {
                return Err(CodecError::LengthMismatch {
                    got: payload.len(),
                    expected: n_blocks,
                });
            }
            let (widths, bits) = payload.split_at(n_blocks);
            if let Some(&w) = widths.iter().find(|&&w| w > 64) {
                return Err(CodecError::BadWidth(w));
            }
            let expected = packed_len(n, widths);
            if payload.len() != expected {
                return Err(CodecError::LengthMismatch {
                    got: payload.len(),
                    expected,
                });
            }
            let mut out = Vec::with_capacity(n);
            let mut acc: u128 = 0;
            let mut count: u32 = 0;
            let mut pos = 0usize;
            for (b, &w) in widths.iter().enumerate() {
                let len = BLOCK.min(n - b * BLOCK);
                if w == 0 {
                    out.resize(out.len() + len, 0);
                    continue;
                }
                let mask: u128 = if w == 64 {
                    u64::MAX as u128
                } else {
                    (1u128 << w) - 1
                };
                for _ in 0..len {
                    while count < w as u32 {
                        if pos + 8 <= bits.len() {
                            let word =
                                u64::from_le_bytes(bits[pos..pos + 8].try_into().expect("8 bytes"));
                            acc |= (word as u128) << count;
                            count += 64;
                            pos += 8;
                        } else {
                            // Length was checked above, so the bytes exist.
                            acc |= (bits[pos] as u128) << count;
                            count += 8;
                            pos += 1;
                        }
                    }
                    out.push(unzigzag((acc & mask) as u64));
                    acc >>= w;
                    count -= w as u32;
                }
            }
            Ok(out)
        }
    }
}

/// BLAKE3 over the canonical little-endian `i64` bytes of `values`, streamed
/// in 4 KiB chunks. Identical to the hash
/// [`crate::distributed::serialize_activations`] returns.
pub fn commit_i64(values: &[i64]) -> Hash256 {
    let mut hasher = blake3::Hasher::new();
    let mut buf = [0u8; 4096];
    for chunk in values.chunks(buf.len() / 8) {
        for (i, v) in chunk.iter().enumerate() {
            buf[i * 8..i * 8 + 8].copy_from_slice(&v.to_le_bytes());
        }
        hasher.update(&buf[..chunk.len() * 8]);
    }
    Hash256(*hasher.finalize().as_bytes())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn lcg(seed: &mut u64) -> u64 {
        *seed = seed
            .wrapping_mul(6364136223846793005)
            .wrapping_add(1442695040888963407);
        *seed
    }

    fn roundtrip(values: &[i64]) -> (Codec, usize) {
        let mut buf = Vec::new();
        let codec = encode_into(values, CodecChoice::Auto, &mut buf);
        let back = decode(codec as u8, &buf, values.len()).expect("decode");
        assert_eq!(back, values, "codec {codec:?} is not exact");
        let mut raw = Vec::new();
        let raw_codec = encode_into(values, CodecChoice::Raw, &mut raw);
        assert_eq!(raw_codec, Codec::RawI64);
        assert_eq!(decode(0, &raw, values.len()).expect("raw"), values);
        (codec, buf.len())
    }

    #[test]
    fn exact_on_edge_values() {
        roundtrip(&[]);
        roundtrip(&[0]);
        roundtrip(&[i64::MIN, i64::MAX, 0, -1, 1]);
        roundtrip(&vec![0; 1000]);
        roundtrip(&vec![i64::MIN; 300]);
        roundtrip(&vec![-1; 129]);
        let mut v = vec![3i64; 257];
        v[200] = i64::MAX;
        roundtrip(&v);
    }

    #[test]
    fn exact_on_random_widths() {
        let mut seed = 7u64;
        for bits in [1u32, 2, 7, 8, 17, 21, 31, 33, 47, 63] {
            for n in [1usize, 5, 127, 128, 129, 1000, 7168] {
                let values: Vec<i64> = (0..n)
                    .map(|_| {
                        let r = lcg(&mut seed) as i64;
                        r >> (64 - bits)
                    })
                    .collect();
                roundtrip(&values);
            }
        }
    }

    #[test]
    fn q16_activations_pack_well_below_raw() {
        // Typical residual stream magnitudes: a few units in Q16 (~2^18).
        let mut seed = 11u64;
        let values: Vec<i64> = (0..7168)
            .map(|_| ((lcg(&mut seed) >> 40) as i64 - (1 << 23)) >> 4)
            .collect();
        let (codec, len) = roundtrip(&values);
        assert_eq!(codec, Codec::BitPack);
        assert!(len * 2 < values.len() * 8, "packed {len} bytes");
    }

    #[test]
    fn falls_back_to_raw_when_packing_does_not_help() {
        let mut seed = 3u64;
        let values: Vec<i64> = (0..300).map(|_| lcg(&mut seed) as i64).collect();
        let mut buf = Vec::new();
        let codec = encode_into(&values, CodecChoice::Auto, &mut buf);
        // Full-width values: packing costs the width bytes on top of raw.
        assert_eq!(codec, Codec::RawI64);
        assert_eq!(buf.len(), 300 * 8);
    }

    #[test]
    fn rejects_malformed_payloads() {
        let values = vec![5i64; 200];
        let mut buf = Vec::new();
        let codec = encode_into(&values, CodecChoice::Auto, &mut buf);
        assert_eq!(codec, Codec::BitPack);
        assert!(matches!(
            decode(1, &buf[..buf.len() - 1], 200),
            Err(CodecError::LengthMismatch { .. })
        ));
        let mut longer = buf.clone();
        longer.push(0);
        assert!(decode(1, &longer, 200).is_err());
        let mut bad = buf.clone();
        bad[0] = 65;
        assert_eq!(decode(1, &bad, 200), Err(CodecError::BadWidth(65)));
        assert_eq!(decode(9, &buf, 200), Err(CodecError::UnknownCodec(9)));
        assert!(decode(0, &[0u8; 15], 2).is_err());
    }

    #[test]
    fn commitment_matches_legacy_serializer() {
        let mut seed = 5u64;
        for n in [0usize, 1, 511, 512, 513, 4096, 7168] {
            let values: Vec<i64> = (0..n).map(|_| lcg(&mut seed) as i64 >> 20).collect();
            let (_, legacy) = crate::distributed::serialize_activations(&values);
            assert_eq!(commit_i64(&values), legacy, "n = {n}");
        }
    }
}
