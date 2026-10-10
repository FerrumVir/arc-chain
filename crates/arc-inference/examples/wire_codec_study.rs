//! Lossless wire-codec study (scratch branch, not for merge).
//!
//! Runs the exact 7B through four stages (layers 0-8, 8-16, 16-24, 24-32)
//! with `forward_shard_rows`, teacher-forced on the agreement experiment's
//! recorded sequences, and collects what crosses the wire, in the type it is
//! sent as (`i64`, Q16):
//!
//! * the stage-boundary hidden state after layers 8, 16 and 24 (the 2-way
//!   split sends the one after 16; the 4-way split all three), one message
//!   per row per hop;
//! * the partial sums of a column-sharded projection (the scheme in
//!   arc-crypto's inference_proof): Wq of the first layer after each
//!   boundary, its input columns cut 2 and 4 ways, one message of `d_model`
//!   exact `i64` accumulators per shard per row.
//!
//! Every message is encoded and decoded with each codec, checked to round-trip
//! exactly, and timed. Codecs: raw little-endian i64; i32 narrowing when every
//! value fits; #166's bit-pack (zigzag, 128-value blocks, one width byte each);
//! zigzag LEB128 varint; per-row delta then varint; zstd 1, 3, 9 and lz4 on
//! the raw bytes; and zstd 1 and lz4 after an 8-plane byte shuffle.
//!
//! usage: wire_codec_study --model GGUF --profile legacy|interleaved
//!        --arc FILE.json[,FILE.json...] --out FILE.json

use arc_inference::cached_integer_model::{
    CachedIntegerModel, I8Weights, KVCache, ShardRowsInput, ShardRowsOutput, layernorm,
    load_cached_model_canonical_i8, load_cached_model_canonical_i8_interleaved_rope,
};
use rayon::prelude::*;
use serde_json::{Value, json};
use std::collections::BTreeMap;
use std::time::Instant;

const ENDS: [usize; 4] = [8, 16, 24, 32];
const CODECS: [&str; 11] = [
    "raw i64",
    "i32 if it fits",
    "#166 bit-pack",
    "varint",
    "delta + varint",
    "zstd-1",
    "zstd-3",
    "zstd-9",
    "lz4",
    "shuffle + zstd-1",
    "shuffle + lz4",
];

fn zigzag(v: i64) -> u64 {
    ((v << 1) ^ (v >> 63)) as u64
}

fn unzigzag(u: u64) -> i64 {
    ((u >> 1) as i64) ^ -((u & 1) as i64)
}

fn raw_bytes(values: &[i64]) -> Vec<u8> {
    values.iter().flat_map(|v| v.to_le_bytes()).collect()
}

fn from_raw(bytes: &[u8]) -> Vec<i64> {
    bytes
        .chunks_exact(8)
        .map(|c| i64::from_le_bytes(c.try_into().expect("8 bytes")))
        .collect()
}

fn shuffle(raw: &[u8]) -> Vec<u8> {
    let n = raw.len() / 8;
    let mut out = vec![0u8; raw.len()];
    for (i, value) in raw.chunks_exact(8).enumerate() {
        for (plane, &byte) in value.iter().enumerate() {
            out[plane * n + i] = byte;
        }
    }
    out
}

fn unshuffle(planes: &[u8]) -> Vec<u8> {
    let n = planes.len() / 8;
    let mut out = vec![0u8; planes.len()];
    for (i, value) in out.chunks_exact_mut(8).enumerate() {
        for (plane, byte) in value.iter_mut().enumerate() {
            *byte = planes[plane * n + i];
        }
    }
    out
}

/// #166's stage_net codec: one tag byte, then per 128-value block the bit
/// width of its widest zigzagged value and every value at that width, LSB
/// first; raw i64 when packing would not be smaller.
fn bitpack_encode(values: &[i64]) -> Vec<u8> {
    const BLOCK: usize = 128;
    let widths: Vec<u8> = values
        .chunks(BLOCK)
        .map(|b| {
            (64 - b
                .iter()
                .fold(0u64, |acc, &v| acc | zigzag(v))
                .leading_zeros()) as u8
        })
        .collect();
    let bits: usize = values
        .chunks(BLOCK)
        .zip(&widths)
        .map(|(b, &w)| b.len() * w as usize)
        .sum();
    if widths.len() + bits.div_ceil(8) >= values.len() * 8 {
        let mut out = vec![0u8];
        out.extend(raw_bytes(values));
        return out;
    }
    let mut out = vec![1u8];
    out.extend_from_slice(&widths);
    let (mut acc, mut count): (u128, u32) = (0, 0);
    for (block, &w) in values.chunks(BLOCK).zip(&widths) {
        if w == 0 {
            continue;
        }
        for &v in block {
            acc |= u128::from(zigzag(v)) << count;
            count += u32::from(w);
            if count >= 64 {
                out.extend_from_slice(&(acc as u64).to_le_bytes());
                acc >>= 64;
                count -= 64;
            }
        }
    }
    let tail = count.div_ceil(8) as usize;
    out.extend_from_slice(&(acc as u64).to_le_bytes()[..tail]);
    out
}

fn bitpack_decode(bytes: &[u8], n: usize) -> Vec<i64> {
    const BLOCK: usize = 128;
    if bytes[0] == 0 {
        return from_raw(&bytes[1..]);
    }
    let n_blocks = n.div_ceil(BLOCK);
    let (widths, bits) = bytes[1..].split_at(n_blocks);
    let mut out = Vec::with_capacity(n);
    let (mut acc, mut count, mut pos): (u128, u32, usize) = (0, 0, 0);
    for (b, &w) in widths.iter().enumerate() {
        let len = BLOCK.min(n - b * BLOCK);
        if w == 0 {
            out.resize(out.len() + len, 0);
            continue;
        }
        let mask: u128 = (1u128 << w) - 1;
        for _ in 0..len {
            while count < u32::from(w) {
                let mut word = [0u8; 8];
                let end = (pos + 8).min(bits.len());
                word[..end - pos].copy_from_slice(&bits[pos..end]);
                acc |= u128::from(u64::from_le_bytes(word)) << count;
                pos += 8;
                count += 64;
            }
            out.push(unzigzag((acc & mask) as u64));
            acc >>= w;
            count -= u32::from(w);
        }
    }
    out
}

fn varint_encode(values: impl Iterator<Item = i64>, out: &mut Vec<u8>) {
    for v in values {
        let mut u = zigzag(v);
        while u >= 0x80 {
            out.push((u as u8) | 0x80);
            u >>= 7;
        }
        out.push(u as u8);
    }
}

fn varint_decode(bytes: &[u8], n: usize) -> Vec<i64> {
    let mut out = Vec::with_capacity(n);
    let (mut u, mut shift) = (0u64, 0u32);
    for &b in bytes {
        u |= u64::from(b & 0x7f) << shift;
        if b & 0x80 == 0 {
            out.push(unzigzag(u));
            u = 0;
            shift = 0;
        } else {
            shift += 7;
        }
    }
    out
}

struct Codecs {
    zstd1: zstd::bulk::Compressor<'static>,
    zstd3: zstd::bulk::Compressor<'static>,
    zstd9: zstd::bulk::Compressor<'static>,
    unzstd: zstd::bulk::Decompressor<'static>,
}

impl Codecs {
    fn new() -> Self {
        Self {
            zstd1: zstd::bulk::Compressor::new(1).expect("zstd level 1"),
            zstd3: zstd::bulk::Compressor::new(3).expect("zstd level 3"),
            zstd9: zstd::bulk::Compressor::new(9).expect("zstd level 9"),
            unzstd: zstd::bulk::Decompressor::new().expect("zstd decoder"),
        }
    }

    /// Encodes `values` with codec `c`, decodes it back, checks the round
    /// trip, and returns (encoded bytes, encode ns, decode ns).
    fn run(&mut self, c: usize, values: &[i64]) -> (usize, u64, u64) {
        let n = values.len();
        let started = Instant::now();
        let encoded: Vec<u8> = match c {
            0 => raw_bytes(values),
            1 => {
                if values.iter().all(|&v| i32::try_from(v).is_ok()) {
                    let mut out = vec![1u8];
                    out.extend(values.iter().flat_map(|&v| (v as i32).to_le_bytes()));
                    out
                } else {
                    let mut out = vec![0u8];
                    out.extend(raw_bytes(values));
                    out
                }
            }
            2 => bitpack_encode(values),
            3 => {
                let mut out = Vec::with_capacity(n * 4);
                varint_encode(values.iter().copied(), &mut out);
                out
            }
            4 => {
                let mut out = Vec::with_capacity(n * 4);
                let mut previous = 0i64;
                varint_encode(
                    values.iter().map(|&v| {
                        let d = v.wrapping_sub(previous);
                        previous = v;
                        d
                    }),
                    &mut out,
                );
                out
            }
            5 => self.zstd1.compress(&raw_bytes(values)).expect("zstd"),
            6 => self.zstd3.compress(&raw_bytes(values)).expect("zstd"),
            7 => self.zstd9.compress(&raw_bytes(values)).expect("zstd"),
            8 => lz4_flex::block::compress(&raw_bytes(values)),
            9 => self
                .zstd1
                .compress(&shuffle(&raw_bytes(values)))
                .expect("zstd"),
            _ => lz4_flex::block::compress(&shuffle(&raw_bytes(values))),
        };
        let encode_ns = started.elapsed().as_nanos() as u64;
        let started = Instant::now();
        let decoded: Vec<i64> = match c {
            0 => from_raw(&encoded),
            1 => {
                if encoded[0] == 1 {
                    encoded[1..]
                        .chunks_exact(4)
                        .map(|b| i64::from(i32::from_le_bytes(b.try_into().expect("4 bytes"))))
                        .collect()
                } else {
                    from_raw(&encoded[1..])
                }
            }
            2 => bitpack_decode(&encoded, n),
            3 => varint_decode(&encoded, n),
            4 => {
                let mut previous = 0i64;
                varint_decode(&encoded, n)
                    .into_iter()
                    .map(|d| {
                        previous = previous.wrapping_add(d);
                        previous
                    })
                    .collect()
            }
            5..=7 => from_raw(&self.unzstd.decompress(&encoded, n * 8).expect("unzstd")),
            8 => from_raw(&lz4_flex::block::decompress(&encoded, n * 8).expect("unlz4")),
            9 => from_raw(&unshuffle(
                &self.unzstd.decompress(&encoded, n * 8).expect("unzstd"),
            )),
            _ => from_raw(&unshuffle(
                &lz4_flex::block::decompress(&encoded, n * 8).expect("unlz4"),
            )),
        };
        let decode_ns = started.elapsed().as_nanos() as u64;
        assert!(decoded == values, "codec {} did not round-trip", CODECS[c]);
        (encoded.len(), encode_ns, decode_ns)
    }
}

#[derive(Default)]
struct Stream {
    messages: u64,
    values: u64,
    max_abs: u64,
    fits_i32: u64,
    bytes: [u64; CODECS.len()],
    enc_ns: [u64; CODECS.len()],
    dec_ns: [u64; CODECS.len()],
}

impl Stream {
    fn add(&mut self, codecs: &mut Codecs, values: &[i64]) {
        self.messages += 1;
        self.values += values.len() as u64;
        let max = values.iter().map(|v| v.unsigned_abs()).max().unwrap_or(0);
        self.max_abs = self.max_abs.max(max);
        if values.iter().all(|&v| i32::try_from(v).is_ok()) {
            self.fits_i32 += 1;
        }
        for c in 0..CODECS.len() {
            let (bytes, enc, dec) = codecs.run(c, values);
            self.bytes[c] += bytes as u64;
            self.enc_ns[c] += enc;
            self.dec_ns[c] += dec;
        }
    }

    fn json(&self) -> Value {
        let codecs: BTreeMap<&str, Value> = CODECS
            .iter()
            .enumerate()
            .map(|(c, name)| {
                (
                    *name,
                    json!({"bytes": self.bytes[c], "enc_ns": self.enc_ns[c], "dec_ns": self.dec_ns[c]}),
                )
            })
            .collect();
        json!({
            "messages": self.messages,
            "values": self.values,
            "max_abs": self.max_abs,
            "fits_i32": self.fits_i32,
            "codecs": codecs,
        })
    }
}

/// Exact column-sharded partial sums of `w x`: one `i64` accumulator per
/// output row and shard, before the per-row scale.
fn partial_sums(w: &I8Weights, x: &[i64], shards: usize) -> Vec<Vec<i64>> {
    let cols = w.n_cols;
    let per = cols / shards;
    (0..shards)
        .map(|s| {
            let (c0, c1) = (s * per, if s + 1 == shards { cols } else { (s + 1) * per });
            (0..w.n_rows)
                .into_par_iter()
                .map(|i| {
                    w.data[i * cols + c0..i * cols + c1]
                        .iter()
                        .zip(&x[c0..c1])
                        .map(|(&a, &b)| i64::from(a) * b)
                        .sum::<i64>()
                })
                .collect()
        })
        .collect()
}

fn tokens(value: &Value) -> Vec<u32> {
    value
        .as_array()
        .expect("tokens")
        .iter()
        .map(|t| t.as_u64().expect("token") as u32)
        .collect()
}

fn main() -> Result<(), String> {
    let mut args = std::env::args().skip(1);
    let (mut model_path, mut profile, mut arc, mut out_path) =
        (String::new(), String::new(), String::new(), String::new());
    while let Some(flag) = args.next() {
        let mut value = || args.next().ok_or_else(|| format!("{flag} needs a value"));
        match flag.as_str() {
            "--model" => model_path = value()?,
            "--profile" => profile = value()?,
            "--arc" => arc = value()?,
            "--out" => out_path = value()?,
            other => return Err(format!("unknown argument {other}")),
        }
    }
    let mut sequences: Vec<(String, Vec<u32>)> = Vec::new();
    for path in arc.split(',').filter(|p| !p.is_empty()) {
        let text = std::fs::read_to_string(path).map_err(|e| format!("{path}: {e}"))?;
        let run: Value = serde_json::from_str(&text).map_err(|e| format!("{path}: {e}"))?;
        if run["profile"].as_str() != Some(profile.as_str()) {
            continue;
        }
        for rec in run["prompts"].as_array().ok_or("no prompts")? {
            sequences.push((
                rec["id"].as_str().unwrap_or("?").to_string(),
                tokens(&rec["fed"]),
            ));
        }
    }
    if sequences.is_empty() {
        return Err(format!("no {profile} sequences in {arc}"));
    }
    let started = Instant::now();
    let model: CachedIntegerModel = if profile == "interleaved" {
        load_cached_model_canonical_i8_interleaved_rope(&model_path)
    } else {
        load_cached_model_canonical_i8(&model_path)
    }
    .map_err(|e| format!("load {model_path}: {e}"))?;
    let n_layers = model.config.n_layers;
    if n_layers != 32 || !model.has_canonical_i8_profile() {
        return Err("expected the canonical 32-layer 7B".into());
    }
    let max_rows = ENDS
        .iter()
        .map(|&e| model.max_shard_rows(e))
        .min()
        .unwrap_or(1);
    println!(
        "{} sequences, profile {}, loaded in {:.0} s, chunks of {max_rows} rows",
        sequences.len(),
        model.arithmetic_profile(),
        started.elapsed().as_secs_f64()
    );

    let mut codecs = Codecs::new();
    let mut streams: BTreeMap<String, Stream> = BTreeMap::new();
    let mut rows = 0u64;
    for (id, fed) in &sequences {
        let seq_started = Instant::now();
        let mut caches: Vec<KVCache> = ENDS.iter().map(|_| KVCache::new(n_layers)).collect();
        let mut position = 0;
        for chunk in fed.chunks(max_rows) {
            let mut input = ShardRowsInput::Tokens(chunk.to_vec());
            let mut boundaries: Vec<Vec<Vec<i64>>> = Vec::new();
            let mut start = 0;
            for (&end, cache) in ENDS.iter().zip(caches.iter_mut()) {
                match model
                    .forward_shard_rows(input, cache, start, end, position)
                    .map_err(|e| format!("{id}: stage {start}..{end}: {e}"))?
                {
                    ShardRowsOutput::Hidden(hidden) => {
                        boundaries.push(hidden.clone());
                        input = ShardRowsInput::Hidden(hidden);
                    }
                    ShardRowsOutput::Logits(_) => {
                        input = ShardRowsInput::Hidden(Vec::new());
                    }
                }
                start = end;
            }
            drop(input);
            position += chunk.len();
            for (b, hidden) in boundaries.iter().enumerate() {
                let layer = ENDS[b];
                let weights = &model.layers[layer].wq;
                let norm = &model.layers[layer].attn_norm;
                for row in hidden {
                    streams
                        .entry(format!("hidden after layer {layer}"))
                        .or_default()
                        .add(&mut codecs, row);
                    let x = layernorm(row, norm);
                    let four = partial_sums(weights, &x, 4);
                    let two: Vec<Vec<i64>> = (0..2)
                        .map(|s| {
                            four[2 * s]
                                .iter()
                                .zip(&four[2 * s + 1])
                                .map(|(a, b)| a + b)
                                .collect()
                        })
                        .collect();
                    for shard in &four {
                        streams
                            .entry(format!("Wq partial sums, 4 shards, layer {layer}"))
                            .or_default()
                            .add(&mut codecs, shard);
                    }
                    for shard in &two {
                        streams
                            .entry(format!("Wq partial sums, 2 shards, layer {layer}"))
                            .or_default()
                            .add(&mut codecs, shard);
                    }
                }
            }
            rows += chunk.len() as u64;
        }
        println!(
            "{id}: {} rows in {:.0} s",
            fed.len(),
            seq_started.elapsed().as_secs_f64()
        );
    }
    let report = json!({
        "label": "MEASURED on a CI runner",
        "profile": profile,
        "rows": rows,
        "codecs": CODECS,
        "rayon_threads": rayon::current_num_threads(),
        "total_s": started.elapsed().as_secs_f64(),
        "streams": streams.iter().map(|(k, v)| (k.clone(), v.json())).collect::<BTreeMap<_, _>>(),
    });
    std::fs::write(
        &out_path,
        serde_json::to_string_pretty(&report).map_err(|e| e.to_string())?,
    )
    .map_err(|e| format!("{out_path}: {e}"))?;
    println!(
        "wrote {out_path} in {:.0} s",
        started.elapsed().as_secs_f64()
    );
    Ok(())
}
