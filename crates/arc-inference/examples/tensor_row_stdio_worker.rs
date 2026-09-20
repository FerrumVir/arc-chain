//! Portable private-cohort row worker.  Build directly on a sidecar host:
//! `cargo build -p arc-inference --example tensor_row_stdio_worker --release`.
//!
//! It has no network listener.  Its only transport is length-prefixed frames
//! over stdin/stdout, intended to be reached through a persistent SSH session
//! configured with StrictHostKeyChecking=yes and a pinned known_hosts file.
//! The coordinator validates response call/input commitments and exact row
//! coverage; this worker validates its immutable row-file identity and shape.

use blake3::Hasher;
use std::convert::TryInto;
use std::fs::File;
use std::io::{self, Read, Write};

const FILE_MAGIC: &[u8; 8] = b"ARCROW01";
const FRAME_MAGIC: &[u8; 8] = b"ARCTP001";
const MAX_FILE: usize = 1_073_741_824;
const MAX_FRAME: usize = 4 * 1024 * 1024;
fn hash_values(values: &[i64]) -> [u8; 32] {
    let mut h = Hasher::new();
    for v in values {
        h.update(&v.to_le_bytes());
    }
    *h.finalize().as_bytes()
}

struct Shard {
    artifact: [u8; 32],
    profile: Vec<u8>,
    layer: i64,
    tensor: u8,
    start: u64,
    end: u64,
    worker: Vec<u8>,
    cols: usize,
    scales: Vec<i64>,
    data: Vec<i8>,
}
fn take<'a>(b: &mut &'a [u8], n: usize) -> io::Result<&'a [u8]> {
    if b.len() < n {
        return Err(io::Error::new(io::ErrorKind::UnexpectedEof, "short frame"));
    }
    let (a, c) = b.split_at(n);
    *b = c;
    Ok(a)
}
fn u16v(b: &mut &[u8]) -> io::Result<usize> {
    Ok(u16::from_le_bytes(take(b, 2)?.try_into().unwrap()) as usize)
}
fn u32v(b: &mut &[u8]) -> io::Result<usize> {
    Ok(u32::from_le_bytes(take(b, 4)?.try_into().unwrap()) as usize)
}
fn u64v(b: &mut &[u8]) -> io::Result<u64> {
    Ok(u64::from_le_bytes(take(b, 8)?.try_into().unwrap()))
}
fn i64v(b: &mut &[u8]) -> io::Result<i64> {
    Ok(i64::from_le_bytes(take(b, 8)?.try_into().unwrap()))
}
fn read_u16(f: &mut File) -> io::Result<usize> {
    let mut b = [0; 2];
    f.read_exact(&mut b)?;
    Ok(u16::from_le_bytes(b) as usize)
}
fn read_u64(f: &mut File) -> io::Result<u64> {
    let mut b = [0; 8];
    f.read_exact(&mut b)?;
    Ok(u64::from_le_bytes(b))
}
fn load(path: &std::path::Path) -> io::Result<Shard> {
    let mut f = File::open(path)?;
    if f.metadata()?.len() as usize > MAX_FILE {
        return Err(io::Error::new(
            io::ErrorKind::InvalidData,
            "row file exceeds 1GiB",
        ));
    }
    let mut magic = [0; 8];
    f.read_exact(&mut magic)?;
    if &magic != FILE_MAGIC {
        return Err(io::Error::new(io::ErrorKind::InvalidData, "row file magic"));
    }
    let mut artifact = [0; 32];
    f.read_exact(&mut artifact)?;
    let profile_len = read_u16(&mut f)?;
    if profile_len > 256 {
        return Err(io::Error::new(io::ErrorKind::InvalidData, "profile bound"));
    }
    let mut profile = vec![0; profile_len];
    f.read_exact(&mut profile)?;
    let mut layer_b = [0; 8];
    f.read_exact(&mut layer_b)?;
    let layer = i64::from_le_bytes(layer_b);
    let mut one = [0; 1];
    f.read_exact(&mut one)?;
    let tensor = one[0];
    let start = read_u64(&mut f)?;
    let end = read_u64(&mut f)?;
    let worker_len = read_u16(&mut f)?;
    if worker_len == 0 || worker_len > 256 {
        return Err(io::Error::new(
            io::ErrorKind::InvalidData,
            "worker id bound",
        ));
    }
    let mut worker = vec![0; worker_len];
    f.read_exact(&mut worker)?;
    let cols = read_u64(&mut f)? as usize;
    let rows = read_u64(&mut f)? as usize;
    if start >= end
        || end - start != rows as u64
        || cols == 0
        || cols > 131072
        || rows > 131072
        || rows.checked_mul(cols).filter(|n| *n <= MAX_FILE).is_none()
    {
        return Err(io::Error::new(
            io::ErrorKind::InvalidData,
            "invalid row shape",
        ));
    }
    let mut scales = Vec::with_capacity(rows);
    for _ in 0..rows {
        let mut b = [0; 8];
        f.read_exact(&mut b)?;
        scales.push(i64::from_le_bytes(b));
    }
    let mut raw = vec![0; rows * cols];
    f.read_exact(&mut raw)?;
    let mut extra = [0; 1];
    if f.read(&mut extra)? != 0 {
        return Err(io::Error::new(
            io::ErrorKind::InvalidData,
            "trailing row bytes",
        ));
    }
    let data = raw.into_iter().map(|v| v as i8).collect();
    Ok(Shard {
        artifact,
        profile,
        layer,
        tensor,
        start,
        end,
        worker,
        cols,
        scales,
        data,
    })
}
fn load_directory(path: &str) -> io::Result<Vec<Shard>> {
    let mut out = Vec::new();
    let mut total = 0usize;
    for entry in std::fs::read_dir(path)? {
        let path = entry?.path();
        if !path.is_file() {
            continue;
        }
        if out.len() >= 1024 {
            return Err(io::Error::new(
                io::ErrorKind::InvalidData,
                "too many row files",
            ));
        }
        let bytes = path.metadata()?.len() as usize;
        total = total
            .checked_add(bytes)
            .ok_or_else(|| io::Error::new(io::ErrorKind::InvalidData, "size overflow"))?;
        if total > MAX_FILE {
            return Err(io::Error::new(
                io::ErrorKind::InvalidData,
                "total resident rows exceed 1GiB",
            ));
        }
        out.push(load(&path)?);
    }
    if out.is_empty() {
        return Err(io::Error::new(io::ErrorKind::NotFound, "no row files"));
    }
    Ok(out)
}
fn frame(stdin: &mut impl Read) -> io::Result<Option<Vec<u8>>> {
    let mut n = [0; 4];
    let first = stdin.read(&mut n[..1])?;
    if first == 0 {
        return Ok(None);
    }
    stdin.read_exact(&mut n[1..])?;
    let n = u32::from_le_bytes(n) as usize;
    if n == 0 || n > MAX_FRAME {
        return Err(io::Error::new(io::ErrorKind::InvalidData, "frame bounds"));
    }
    let mut b = vec![0; n];
    stdin.read_exact(&mut b)?;
    Ok(Some(b))
}
fn main() -> io::Result<()> {
    let path = std::env::args().nth(1).ok_or_else(|| {
        io::Error::new(io::ErrorKind::InvalidInput, "usage: worker ROW_DIRECTORY")
    })?;
    let shards = load_directory(&path)?;
    let mut input = io::stdin();
    let mut output = io::stdout();
    loop {
        let raw = match frame(&mut input)? {
            Some(v) => v,
            None => return Ok(()),
        };
        let mut b = raw.as_slice();
        if take(&mut b, 8)? != FRAME_MAGIC {
            return Err(io::Error::new(io::ErrorKind::InvalidData, "frame magic"));
        }
        let call = take(&mut b, 32)?.to_vec();
        let input_hash = take(&mut b, 32)?.to_vec();
        let artifact = take(&mut b, 32)?;
        let profile_len = u16v(&mut b)?;
        let profile = take(&mut b, profile_len)?;
        let layer = i64v(&mut b)?;
        let tensor = take(&mut b, 1)?[0];
        let start = u64v(&mut b)?;
        let end = u64v(&mut b)?;
        let worker_len = u16v(&mut b)?;
        let worker = take(&mut b, worker_len)?;
        let count = u32v(&mut b)?;
        let shard = shards
            .iter()
            .find(|s| {
                artifact == s.artifact
                    && profile == s.profile.as_slice()
                    && layer == s.layer
                    && tensor == s.tensor
                    && start == s.start
                    && end == s.end
                    && worker == s.worker.as_slice()
            })
            .ok_or_else(|| io::Error::new(io::ErrorKind::PermissionDenied, "identity mismatch"))?;
        if count != shard.cols || count > 131072 {
            return Err(io::Error::new(
                io::ErrorKind::PermissionDenied,
                "shape mismatch",
            ));
        }
        let mut activation = Vec::with_capacity(count);
        for _ in 0..count {
            activation.push(i64v(&mut b)?);
        }
        if input_hash.as_slice() != hash_values(&activation) {
            return Err(io::Error::new(
                io::ErrorKind::PermissionDenied,
                "input hash mismatch",
            ));
        }
        let sum = activation
            .iter()
            .try_fold(0i64, |s, v| s.checked_add(v.checked_abs()?))
            .ok_or_else(|| io::Error::new(io::ErrorKind::InvalidData, "activation overflow"))?;
        let bound = sum
            .checked_mul(128)
            .ok_or_else(|| io::Error::new(io::ErrorKind::InvalidData, "dot overflow"))?;
        if shard
            .scales
            .iter()
            .any(|s| s.checked_abs().and_then(|v| bound.checked_mul(v)).is_none())
        {
            return Err(io::Error::new(io::ErrorKind::InvalidData, "scale overflow"));
        }
        let mut values = Vec::with_capacity(shard.scales.len());
        for row in 0..shard.scales.len() {
            let mut acc = 0i64;
            for col in 0..count {
                acc += shard.data[row * count + col] as i64 * activation[col];
            }
            values.push((acc * shard.scales[row]) >> 16);
        }
        if !b.is_empty() {
            return Err(io::Error::new(
                io::ErrorKind::InvalidData,
                "trailing frame bytes",
            ));
        }
        let mut reply = Vec::with_capacity(
            8 + 32
                + 32
                + 32
                + 2
                + profile.len()
                + 8
                + 1
                + 16
                + 2
                + worker.len()
                + 4
                + values.len() * 8,
        );
        reply.extend_from_slice(FRAME_MAGIC);
        reply.extend_from_slice(&call);
        reply.extend_from_slice(&input_hash);
        reply.extend_from_slice(&shard.artifact);
        reply.extend_from_slice(&(shard.profile.len() as u16).to_le_bytes());
        reply.extend_from_slice(&shard.profile);
        reply.extend_from_slice(&shard.layer.to_le_bytes());
        reply.push(shard.tensor);
        reply.extend_from_slice(&shard.start.to_le_bytes());
        reply.extend_from_slice(&shard.end.to_le_bytes());
        reply.extend_from_slice(&(shard.worker.len() as u16).to_le_bytes());
        reply.extend_from_slice(&shard.worker);
        reply.extend_from_slice(&(values.len() as u32).to_le_bytes());
        for v in values {
            reply.extend_from_slice(&v.to_le_bytes());
        }
        output.write_all(&(reply.len() as u32).to_le_bytes())?;
        output.write_all(&reply)?;
        output.flush()?;
    }
}
