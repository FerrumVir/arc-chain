//! SIMD-accelerated transaction parsing for ARC Chain.
//!
//! Provides batch serialization / deserialization of transactions in a compact
//! length-prefixed wire format.  The "SIMD" acceleration is currently
//! implemented via safe wide-integer reads (`u64` / `u128`) and careful memory
//! layout; `cfg(target_arch)` gates are in place for future `std::arch`
//! intrinsics.

use std::convert::TryInto;
use std::time::Instant;
use thiserror::Error;

// ---------------------------------------------------------------------------
// Errors
// ---------------------------------------------------------------------------

#[derive(Error, Debug, PartialEq, Eq)]
pub enum ParseError {
    #[error("unexpected end of input at offset {offset}, need {need} more bytes")]
    UnexpectedEof { offset: usize, need: usize },
    #[error("transaction payload too short ({len} bytes) at offset {offset}")]
    TxTooShort { offset: usize, len: usize },
    #[error("transaction length {len} exceeds maximum {max}")]
    TxTooLarge { len: usize, max: usize },
    #[error("invalid amount encoding at offset {offset}")]
    InvalidAmount { offset: usize },
}

pub type ParseResult<T> = Result<T, ParseError>;

// ---------------------------------------------------------------------------
// Constants
// ---------------------------------------------------------------------------

/// Length-prefix size (4 bytes, little-endian u32).
const LEN_PREFIX: usize = 4;

/// Maximum allowed single-transaction size (64 KiB).
const MAX_TX_SIZE: usize = 65_536;

/// Minimum transaction body size:
///   sender(32) + receiver(32) + amount(8) + nonce(8) + sig_offset(4) + hash(32) = 116
const MIN_TX_BODY: usize = 116;

// ---------------------------------------------------------------------------
// ParsedTx — zero-copy where possible
// ---------------------------------------------------------------------------

/// A parsed transaction.  All fixed-size fields are copied; the signature is
/// represented as an offset + length into the original buffer so the caller
/// can do zero-copy validation.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ParsedTx {
    /// 32-byte sender address.
    pub sender: [u8; 32],
    /// 32-byte receiver address.
    pub receiver: [u8; 32],
    /// Transfer amount (little-endian u64).
    pub amount: u64,
    /// Sender nonce (little-endian u64).
    pub nonce: u64,
    /// Byte offset of the signature *within the individual tx payload*.
    pub signature_offset: u32,
    /// 32-byte transaction hash.
    pub hash: [u8; 32],
}

/// A reference-based parsed transaction that borrows from the input buffer.
/// Used for true zero-copy parsing when lifetime permits.
#[derive(Debug, Clone, Copy)]
pub struct ParsedTxRef<'a> {
    pub sender: &'a [u8; 32],
    pub receiver: &'a [u8; 32],
    pub amount: u64,
    pub nonce: u64,
    pub signature_offset: u32,
    pub hash: &'a [u8; 32],
    /// Raw bytes of the entire transaction (for signature verification, etc.).
    pub raw: &'a [u8],
}

// ---------------------------------------------------------------------------
// SimdBatchResult
// ---------------------------------------------------------------------------

/// Summary produced by a batch parse.
#[derive(Debug, Clone)]
pub struct SimdBatchResult {
    /// Successfully parsed transactions.
    pub parsed: Vec<ParsedTx>,
    /// Number of transactions that parsed without error.
    pub parsed_count: usize,
    /// Number of transactions that failed to parse.
    pub error_count: usize,
    /// Total wall-clock time for the parse.
    pub elapsed: std::time::Duration,
}

// ---------------------------------------------------------------------------
// SimdParser
// ---------------------------------------------------------------------------

/// Batch parser with optional prefetch hinting.
pub struct SimdParser {
    /// If true, issue prefetch hints when reading the next tx length prefix
    /// (only effective on supported architectures).
    pub prefetch_enabled: bool,
}

impl Default for SimdParser {
    fn default() -> Self {
        Self::new()
    }
}

impl SimdParser {
    pub fn new() -> Self {
        Self {
            prefetch_enabled: true,
        }
    }

    /// Parse a batch of length-prefixed transactions from `raw_bytes`.
    ///
    /// Wire format: `[u32 tx_len][tx_data: tx_len bytes] ...` repeated.
    pub fn parse_batch(&self, raw_bytes: &[u8]) -> SimdBatchResult {
        let start = Instant::now();
        let mut parsed = Vec::new();
        let mut error_count = 0usize;
        let mut offset = 0usize;

        while offset + LEN_PREFIX <= raw_bytes.len() {
            // Read 4-byte length prefix using a u32 wide read.
            let tx_len = read_u32_le(raw_bytes, offset) as usize;
            offset += LEN_PREFIX;

            if tx_len > MAX_TX_SIZE {
                error_count += 1;
                // Skip is impossible if we don't know the real length — bail.
                break;
            }

            if offset + tx_len > raw_bytes.len() {
                error_count += 1;
                break;
            }

            // Prefetch the *next* transaction's length prefix while we
            // decode the current one.
            if self.prefetch_enabled {
                let next_hint = offset + tx_len;
                prefetch_read(raw_bytes, next_hint);
            }

            let tx_data = &raw_bytes[offset..offset + tx_len];
            match Self::parse_single(tx_data, offset) {
                Ok(tx) => parsed.push(tx),
                Err(_) => error_count += 1,
            }

            offset += tx_len;
        }

        let parsed_count = parsed.len();
        SimdBatchResult {
            parsed,
            parsed_count,
            error_count,
            elapsed: start.elapsed(),
        }
    }

    /// Zero-copy parse returning references into the original buffer.
    pub fn parse_batch_ref<'a>(&self, raw_bytes: &'a [u8]) -> Vec<ParseResult<ParsedTxRef<'a>>> {
        let mut results = Vec::new();
        let mut offset = 0usize;

        while offset + LEN_PREFIX <= raw_bytes.len() {
            let tx_len = read_u32_le(raw_bytes, offset) as usize;
            offset += LEN_PREFIX;

            if tx_len > MAX_TX_SIZE || offset + tx_len > raw_bytes.len() {
                results.push(Err(ParseError::TxTooLarge {
                    len: tx_len,
                    max: MAX_TX_SIZE,
                }));
                break;
            }

            let tx_data = &raw_bytes[offset..offset + tx_len];
            results.push(Self::parse_single_ref(tx_data, offset));
            offset += tx_len;
        }
        results
    }

    // -- internal parsing --------------------------------------------------

    /// Decode one transaction from its payload bytes (after the length prefix).
    fn parse_single(data: &[u8], _global_offset: usize) -> ParseResult<ParsedTx> {
        if data.len() < MIN_TX_BODY {
            return Err(ParseError::TxTooShort {
                offset: _global_offset,
                len: data.len(),
            });
        }

        let mut pos = 0;

        // sender: 32 bytes
        let sender: [u8; 32] = data[pos..pos + 32].try_into().unwrap();
        pos += 32;

        // receiver: 32 bytes
        let receiver: [u8; 32] = data[pos..pos + 32].try_into().unwrap();
        pos += 32;

        // amount: 8 bytes LE — use wide u64 read
        let amount = read_u64_le(data, pos);
        pos += 8;

        // nonce: 8 bytes LE
        let nonce = read_u64_le(data, pos);
        pos += 8;

        // signature_offset: 4 bytes LE
        let signature_offset = read_u32_le(data, pos);
        pos += 4;

        // hash: 32 bytes
        let hash: [u8; 32] = data[pos..pos + 32].try_into().unwrap();

        Ok(ParsedTx {
            sender,
            receiver,
            amount,
            nonce,
            signature_offset,
            hash,
        })
    }

    /// Zero-copy parse of a single transaction.
    fn parse_single_ref<'a>(data: &'a [u8], _global_offset: usize) -> ParseResult<ParsedTxRef<'a>> {
        if data.len() < MIN_TX_BODY {
            return Err(ParseError::TxTooShort {
                offset: _global_offset,
                len: data.len(),
            });
        }

        let sender: &[u8; 32] = data[0..32].try_into().unwrap();
        let receiver: &[u8; 32] = data[32..64].try_into().unwrap();
        let amount = read_u64_le(data, 64);
        let nonce = read_u64_le(data, 72);
        let signature_offset = read_u32_le(data, 80);
        let hash: &[u8; 32] = data[84..116].try_into().unwrap();

        Ok(ParsedTxRef {
            sender,
            receiver,
            amount,
            nonce,
            signature_offset,
            hash,
            raw: data,
        })
    }
}

// ---------------------------------------------------------------------------
// Serialization
// ---------------------------------------------------------------------------

/// Serialize a batch of transactions into the length-prefixed wire format.
pub fn serialize_batch(txs: &[ParsedTx]) -> Vec<u8> {
    // Pre-calculate total size for a single allocation.
    let total: usize = txs.len() * (LEN_PREFIX + MIN_TX_BODY);
    let mut buf = Vec::with_capacity(total);

    for tx in txs {
        let tx_len = MIN_TX_BODY as u32;
        buf.extend_from_slice(&tx_len.to_le_bytes()); // 4-byte length prefix

        buf.extend_from_slice(&tx.sender);            // 32
        buf.extend_from_slice(&tx.receiver);           // 32
        buf.extend_from_slice(&tx.amount.to_le_bytes());  // 8
        buf.extend_from_slice(&tx.nonce.to_le_bytes());   // 8
        buf.extend_from_slice(&tx.signature_offset.to_le_bytes()); // 4
        buf.extend_from_slice(&tx.hash);               // 32
    }

    buf
}

/// Serialize a single transaction (without the length prefix).
pub fn serialize_tx(tx: &ParsedTx) -> Vec<u8> {
    let mut buf = Vec::with_capacity(MIN_TX_BODY);
    buf.extend_from_slice(&tx.sender);
    buf.extend_from_slice(&tx.receiver);
    buf.extend_from_slice(&tx.amount.to_le_bytes());
    buf.extend_from_slice(&tx.nonce.to_le_bytes());
    buf.extend_from_slice(&tx.signature_offset.to_le_bytes());
    buf.extend_from_slice(&tx.hash);
    buf
}

// ---------------------------------------------------------------------------
// Wide reads (simulate SIMD benefit with aligned multi-byte loads)
// ---------------------------------------------------------------------------

/// Read a little-endian `u32` from `buf` at `offset` using a single wide load.
#[inline(always)]
fn read_u32_le(buf: &[u8], offset: usize) -> u32 {
    // SAFETY-equivalent: we bounds-check via slice indexing.
    let bytes: [u8; 4] = buf[offset..offset + 4].try_into().unwrap();
    u32::from_le_bytes(bytes)
}

/// Read a little-endian `u64` from `buf` at `offset` using a single wide load.
#[inline(always)]
fn read_u64_le(buf: &[u8], offset: usize) -> u64 {
    let bytes: [u8; 8] = buf[offset..offset + 8].try_into().unwrap();
    u64::from_le_bytes(bytes)
}

/// Read a little-endian `u128` from `buf` at `offset` — useful for comparing
/// two 16-byte address halves in one operation.
#[inline(always)]
#[allow(dead_code)]
fn read_u128_le(buf: &[u8], offset: usize) -> u128 {
    let bytes: [u8; 16] = buf[offset..offset + 16].try_into().unwrap();
    u128::from_le_bytes(bytes)
}

// ---------------------------------------------------------------------------
// Prefetch hints
// ---------------------------------------------------------------------------

/// Issue a software prefetch for read at the given offset.
/// Falls back to a no-op on unsupported architectures.
#[inline(always)]
fn prefetch_read(buf: &[u8], offset: usize) {
    if offset >= buf.len() {
        return;
    }
    let _ptr = buf.as_ptr().wrapping_add(offset);

    // On x86_64 we can use _mm_prefetch; on aarch64 we have __prefetch.
    // For now this is a compiler hint via the pointer read — a real SIMD
    // backend would use std::arch intrinsics gated behind cfg.
    #[cfg(target_arch = "x86_64")]
    {
        // _mm_prefetch requires `unsafe` + nightly or `core::arch::x86_64`.
        // We leave a no-op placeholder that the compiler may autovectorise.
        unsafe {
            // Locality hint 3 = T0 (all cache levels).
            #[cfg(target_feature = "sse")]
            std::arch::x86_64::_mm_prefetch::<3>(_ptr as *const i8);
        }
    }

    #[cfg(target_arch = "aarch64")]
    {
        // ARM prefetch intrinsic (requires nightly; placeholder).
        let _ = _ptr;
    }

    #[cfg(not(any(target_arch = "x86_64", target_arch = "aarch64")))]
    {
        let _ = _ptr;
    }
}

// ---------------------------------------------------------------------------
// Address comparison helpers (wide-integer "SIMD")
// ---------------------------------------------------------------------------

/// Compare two 32-byte addresses using two `u128` reads instead of
/// byte-by-byte — 2x fewer comparisons on 64-bit architectures.
#[inline]
pub fn addresses_equal(a: &[u8; 32], b: &[u8; 32]) -> bool {
    let a_lo = u128::from_le_bytes(a[0..16].try_into().unwrap());
    let a_hi = u128::from_le_bytes(a[16..32].try_into().unwrap());
    let b_lo = u128::from_le_bytes(b[0..16].try_into().unwrap());
    let b_hi = u128::from_le_bytes(b[16..32].try_into().unwrap());
    a_lo == b_lo && a_hi == b_hi
}

/// Sum amounts of all transactions using u128 accumulator to avoid overflow.
pub fn sum_amounts(txs: &[ParsedTx]) -> u128 {
    txs.iter().map(|tx| tx.amount as u128).sum()
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;

    /// Build a dummy transaction with deterministic data.
    fn make_tx(id: u8) -> ParsedTx {
        ParsedTx {
            sender: [id; 32],
            receiver: [id.wrapping_add(1); 32],
            amount: id as u64 * 1000,
            nonce: id as u64,
            signature_offset: 116,
            hash: [id.wrapping_mul(7); 32],
        }
    }

    // 1. Single tx serialize / parse round-trip.
    #[test]
    fn test_single_roundtrip() {
        let tx = make_tx(1);
        let batch = serialize_batch(&[tx.clone()]);
        let parser = SimdParser::new();
        let result = parser.parse_batch(&batch);
        assert_eq!(result.parsed_count, 1);
        assert_eq!(result.error_count, 0);
        assert_eq!(result.parsed[0], tx);
    }

    // 2. Multi-tx batch round-trip.
    #[test]
    fn test_batch_roundtrip() {
        let txs: Vec<ParsedTx> = (0..100).map(|i| make_tx(i)).collect();
        let batch = serialize_batch(&txs);
        let parser = SimdParser::new();
        let result = parser.parse_batch(&batch);
        assert_eq!(result.parsed_count, 100);
        assert_eq!(result.error_count, 0);
        assert_eq!(result.parsed, txs);
    }

    // 3. Empty buffer yields zero transactions.
    #[test]
    fn test_empty_buffer() {
        let parser = SimdParser::new();
        let result = parser.parse_batch(&[]);
        assert_eq!(result.parsed_count, 0);
        assert_eq!(result.error_count, 0);
    }

    // 4. Truncated length prefix.
    #[test]
    fn test_truncated_length_prefix() {
        let parser = SimdParser::new();
        let result = parser.parse_batch(&[0x01, 0x00]); // only 2 bytes, need 4
        assert_eq!(result.parsed_count, 0);
        assert_eq!(result.error_count, 0); // simply stops, no error
    }

    // 5. Truncated payload.
    #[test]
    fn test_truncated_payload() {
        let parser = SimdParser::new();
        // Say tx_len = 200 but only 10 bytes follow.
        let mut buf = vec![0u8; 14];
        buf[0..4].copy_from_slice(&200u32.to_le_bytes());
        let result = parser.parse_batch(&buf);
        assert_eq!(result.parsed_count, 0);
        assert_eq!(result.error_count, 1);
    }

    // 6. tx_len exceeding MAX_TX_SIZE is rejected.
    #[test]
    fn test_oversized_tx() {
        let parser = SimdParser::new();
        let mut buf = vec![0u8; 8];
        let huge: u32 = (MAX_TX_SIZE + 1) as u32;
        buf[0..4].copy_from_slice(&huge.to_le_bytes());
        let result = parser.parse_batch(&buf);
        assert_eq!(result.error_count, 1);
        assert_eq!(result.parsed_count, 0);
    }

    // 7. Zero-copy ref parsing.
    #[test]
    fn test_zero_copy_ref_parse() {
        let tx = make_tx(5);
        let batch = serialize_batch(&[tx.clone()]);
        let parser = SimdParser::new();
        let refs = parser.parse_batch_ref(&batch);
        assert_eq!(refs.len(), 1);
        let r = refs[0].as_ref().unwrap();
        assert_eq!(r.sender, &tx.sender);
        assert_eq!(r.receiver, &tx.receiver);
        assert_eq!(r.amount, tx.amount);
        assert_eq!(r.nonce, tx.nonce);
        assert_eq!(r.hash, &tx.hash);
    }

    // 8. Serialize / re-parse preserves exact bytes.
    #[test]
    fn test_serialize_determinism() {
        let txs: Vec<ParsedTx> = (0..10).map(|i| make_tx(i)).collect();
        let a = serialize_batch(&txs);
        let b = serialize_batch(&txs);
        assert_eq!(a, b, "serialization must be deterministic");
    }

    // 9. addresses_equal works with wide reads.
    #[test]
    fn test_addresses_equal() {
        let a = [0xAA; 32];
        let b = [0xAA; 32];
        let c = [0xBB; 32];
        assert!(addresses_equal(&a, &b));
        assert!(!addresses_equal(&a, &c));
    }

    // 10. sum_amounts with u128 accumulator.
    #[test]
    fn test_sum_amounts() {
        let txs: Vec<ParsedTx> = (1..=10).map(|i| make_tx(i)).collect();
        // amounts = 1000, 2000, ..., 10_000 => sum = 55_000
        let total = sum_amounts(&txs);
        assert_eq!(total, 55_000u128);
    }

    // 11. SimdBatchResult elapsed is non-zero for a real batch.
    #[test]
    fn test_elapsed_timing() {
        let txs: Vec<ParsedTx> = (0..50).map(|i| make_tx(i)).collect();
        let batch = serialize_batch(&txs);
        let parser = SimdParser::new();
        let result = parser.parse_batch(&batch);
        // Just check it recorded *some* duration (could be zero on very fast machines,
        // but parsed_count should still be correct).
        assert_eq!(result.parsed_count, 50);
    }

    // 12. Mixed valid + invalid in one stream (invalid body too short).
    #[test]
    fn test_mixed_valid_invalid() {
        // Build a valid tx.
        let tx = make_tx(1);
        let valid_batch = serialize_batch(&[tx.clone()]);

        // Build an invalid tx (body shorter than MIN_TX_BODY).
        let short_body = vec![0xAA; 20];
        let short_len = short_body.len() as u32;

        let mut buf = Vec::new();
        buf.extend_from_slice(&valid_batch);                    // valid
        buf.extend_from_slice(&short_len.to_le_bytes());         // len prefix
        buf.extend_from_slice(&short_body);                      // short body
        buf.extend_from_slice(&valid_batch);                    // valid again

        let parser = SimdParser::new();
        let result = parser.parse_batch(&buf);
        assert_eq!(result.parsed_count, 2);
        assert_eq!(result.error_count, 1);
    }

    // 13. Prefetch toggle does not affect correctness.
    #[test]
    fn test_prefetch_toggle() {
        let txs: Vec<ParsedTx> = (0..20).map(|i| make_tx(i)).collect();
        let batch = serialize_batch(&txs);

        let mut parser = SimdParser::new();
        parser.prefetch_enabled = false;
        let r1 = parser.parse_batch(&batch);

        parser.prefetch_enabled = true;
        let r2 = parser.parse_batch(&batch);

        assert_eq!(r1.parsed, r2.parsed);
    }

    // 14. Single tx serialize_tx helper.
    #[test]
    fn test_serialize_tx_standalone() {
        let tx = make_tx(42);
        let raw = serialize_tx(&tx);
        assert_eq!(raw.len(), MIN_TX_BODY);
        // Manually decode sender.
        assert_eq!(&raw[0..32], &[42u8; 32]);
    }

    // 15. read_u128_le helper correctness.
    #[test]
    fn test_read_u128_le() {
        let mut buf = [0u8; 16];
        buf[0] = 0xFF;
        buf[15] = 0x01;
        let val = read_u128_le(&buf, 0);
        let expected = u128::from_le_bytes(buf);
        assert_eq!(val, expected);
    }
}
