use std::convert::TryInto;
use std::time::Instant;
use thiserror::Error;

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

const LEN_PREFIX: usize = 4;

const MAX_TX_SIZE: usize = 65_536;

const MIN_TX_BODY: usize = 116;

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ParsedTx {
    pub sender: [u8; 32],
    pub receiver: [u8; 32],
    pub amount: u64,
    pub nonce: u64,
    pub signature_offset: u32,
    pub hash: [u8; 32],
}

#[derive(Debug, Clone, Copy)]
pub struct ParsedTxRef<'a> {
    pub sender: &'a [u8; 32],
    pub receiver: &'a [u8; 32],
    pub amount: u64,
    pub nonce: u64,
    pub signature_offset: u32,
    pub hash: &'a [u8; 32],
    pub raw: &'a [u8],
}

#[derive(Debug, Clone)]
pub struct SimdBatchResult {
    pub parsed: Vec<ParsedTx>,
    pub parsed_count: usize,
    pub error_count: usize,
    pub elapsed: std::time::Duration,
}

pub struct SimdParser {
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

    pub fn parse_batch(&self, raw_bytes: &[u8]) -> SimdBatchResult {
        let start = Instant::now();
        let mut parsed = Vec::new();
        let mut error_count = 0usize;
        let mut offset = 0usize;

        while offset + LEN_PREFIX <= raw_bytes.len() {
            let tx_len = read_u32_le(raw_bytes, offset) as usize;
            offset += LEN_PREFIX;

            if tx_len > MAX_TX_SIZE {
                error_count += 1;
                break;
            }

            if offset + tx_len > raw_bytes.len() {
                error_count += 1;
                break;
            }

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

    fn parse_single(data: &[u8], _global_offset: usize) -> ParseResult<ParsedTx> {
        if data.len() < MIN_TX_BODY {
            return Err(ParseError::TxTooShort {
                offset: _global_offset,
                len: data.len(),
            });
        }

        let mut pos = 0;

        let sender: [u8; 32] = data[pos..pos + 32].try_into().unwrap();
        pos += 32;

        let receiver: [u8; 32] = data[pos..pos + 32].try_into().unwrap();
        pos += 32;

        let amount = read_u64_le(data, pos);
        pos += 8;

        let nonce = read_u64_le(data, pos);
        pos += 8;

        let signature_offset = read_u32_le(data, pos);
        pos += 4;

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

pub fn serialize_batch(txs: &[ParsedTx]) -> Vec<u8> {
    let total: usize = txs.len() * (LEN_PREFIX + MIN_TX_BODY);
    let mut buf = Vec::with_capacity(total);

    for tx in txs {
        let tx_len = MIN_TX_BODY as u32;
        buf.extend_from_slice(&tx_len.to_le_bytes());

        buf.extend_from_slice(&tx.sender);
        buf.extend_from_slice(&tx.receiver);
        buf.extend_from_slice(&tx.amount.to_le_bytes());
        buf.extend_from_slice(&tx.nonce.to_le_bytes());
        buf.extend_from_slice(&tx.signature_offset.to_le_bytes());
        buf.extend_from_slice(&tx.hash);
    }

    buf
}

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

#[inline(always)]
fn read_u32_le(buf: &[u8], offset: usize) -> u32 {
    let bytes: [u8; 4] = buf[offset..offset + 4].try_into().unwrap();
    u32::from_le_bytes(bytes)
}

#[inline(always)]
fn read_u64_le(buf: &[u8], offset: usize) -> u64 {
    let bytes: [u8; 8] = buf[offset..offset + 8].try_into().unwrap();
    u64::from_le_bytes(bytes)
}

#[inline(always)]
#[allow(dead_code)]
fn read_u128_le(buf: &[u8], offset: usize) -> u128 {
    let bytes: [u8; 16] = buf[offset..offset + 16].try_into().unwrap();
    u128::from_le_bytes(bytes)
}

#[inline(always)]
fn prefetch_read(buf: &[u8], offset: usize) {
    if offset >= buf.len() {
        return;
    }
    let _ptr = buf.as_ptr().wrapping_add(offset);

    #[cfg(target_arch = "x86_64")]
    {
        unsafe {
            #[cfg(target_feature = "sse")]
            std::arch::x86_64::_mm_prefetch::<3>(_ptr as *const i8);
        }
    }

    #[cfg(target_arch = "aarch64")]
    {
        let _ = _ptr;
    }

    #[cfg(not(any(target_arch = "x86_64", target_arch = "aarch64")))]
    {
        let _ = _ptr;
    }
}

#[inline]
pub fn addresses_equal(a: &[u8; 32], b: &[u8; 32]) -> bool {
    let a_lo = u128::from_le_bytes(a[0..16].try_into().unwrap());
    let a_hi = u128::from_le_bytes(a[16..32].try_into().unwrap());
    let b_lo = u128::from_le_bytes(b[0..16].try_into().unwrap());
    let b_hi = u128::from_le_bytes(b[16..32].try_into().unwrap());
    a_lo == b_lo && a_hi == b_hi
}

pub fn sum_amounts(txs: &[ParsedTx]) -> u128 {
    txs.iter().map(|tx| tx.amount as u128).sum()
}

#[cfg(test)]
mod tests {
    use super::*;

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

    #[test]
    fn test_single_roundtrip() {
        let tx = make_tx(1);
        let batch = serialize_batch(std::slice::from_ref(&tx));
        let parser = SimdParser::new();
        let result = parser.parse_batch(&batch);
        assert_eq!(result.parsed_count, 1);
        assert_eq!(result.error_count, 0);
        assert_eq!(result.parsed[0], tx);
    }

    #[test]
    fn test_batch_roundtrip() {
        let txs: Vec<ParsedTx> = (0..100).map(make_tx).collect();
        let batch = serialize_batch(&txs);
        let parser = SimdParser::new();
        let result = parser.parse_batch(&batch);
        assert_eq!(result.parsed_count, 100);
        assert_eq!(result.error_count, 0);
        assert_eq!(result.parsed, txs);
    }

    #[test]
    fn test_empty_buffer() {
        let parser = SimdParser::new();
        let result = parser.parse_batch(&[]);
        assert_eq!(result.parsed_count, 0);
        assert_eq!(result.error_count, 0);
    }

    #[test]
    fn test_truncated_length_prefix() {
        let parser = SimdParser::new();
        let result = parser.parse_batch(&[0x01, 0x00]);
        assert_eq!(result.parsed_count, 0);
        assert_eq!(result.error_count, 0);
    }

    #[test]
    fn test_truncated_payload() {
        let parser = SimdParser::new();
        let mut buf = vec![0u8; 14];
        buf[0..4].copy_from_slice(&200u32.to_le_bytes());
        let result = parser.parse_batch(&buf);
        assert_eq!(result.parsed_count, 0);
        assert_eq!(result.error_count, 1);
    }

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

    #[test]
    fn test_zero_copy_ref_parse() {
        let tx = make_tx(5);
        let batch = serialize_batch(std::slice::from_ref(&tx));
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

    #[test]
    fn test_serialize_determinism() {
        let txs: Vec<ParsedTx> = (0..10).map(make_tx).collect();
        let a = serialize_batch(&txs);
        let b = serialize_batch(&txs);
        assert_eq!(a, b, "serialization must be deterministic");
    }

    #[test]
    fn test_addresses_equal() {
        let a = [0xAA; 32];
        let b = [0xAA; 32];
        let c = [0xBB; 32];
        assert!(addresses_equal(&a, &b));
        assert!(!addresses_equal(&a, &c));
    }

    #[test]
    fn test_sum_amounts() {
        let txs: Vec<ParsedTx> = (1..=10).map(make_tx).collect();
        let total = sum_amounts(&txs);
        assert_eq!(total, 55_000u128);
    }

    #[test]
    fn test_elapsed_timing() {
        let txs: Vec<ParsedTx> = (0..50).map(make_tx).collect();
        let batch = serialize_batch(&txs);
        let parser = SimdParser::new();
        let result = parser.parse_batch(&batch);
        assert_eq!(result.parsed_count, 50);
    }

    #[test]
    fn test_mixed_valid_invalid() {
        let tx = make_tx(1);
        let valid_batch = serialize_batch(std::slice::from_ref(&tx));

        let short_body = vec![0xAA; 20];
        let short_len = short_body.len() as u32;

        let mut buf = Vec::new();
        buf.extend_from_slice(&valid_batch);
        buf.extend_from_slice(&short_len.to_le_bytes());
        buf.extend_from_slice(&short_body);
        buf.extend_from_slice(&valid_batch);

        let parser = SimdParser::new();
        let result = parser.parse_batch(&buf);
        assert_eq!(result.parsed_count, 2);
        assert_eq!(result.error_count, 1);
    }

    #[test]
    fn test_prefetch_toggle() {
        let txs: Vec<ParsedTx> = (0..20).map(make_tx).collect();
        let batch = serialize_batch(&txs);

        let mut parser = SimdParser::new();
        parser.prefetch_enabled = false;
        let r1 = parser.parse_batch(&batch);

        parser.prefetch_enabled = true;
        let r2 = parser.parse_batch(&batch);

        assert_eq!(r1.parsed, r2.parsed);
    }

    #[test]
    fn test_serialize_tx_standalone() {
        let tx = make_tx(42);
        let raw = serialize_tx(&tx);
        assert_eq!(raw.len(), MIN_TX_BODY);
        assert_eq!(&raw[0..32], &[42u8; 32]);
    }

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
