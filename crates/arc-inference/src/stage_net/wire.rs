//! Stage-to-stage wire format and the transport trait.
//!
//! One *message* carries one micro-batch: a fixed header followed by one
//! entry per sequence. An entry is either a token id (into the first stage,
//! or the last stage's sampled token travelling back) or a hidden state
//! encoded by [`super::codec`].
//!
//! ```text
//! message header (40 bytes, little endian)
//!   magic "ARCS" | version u8 | kind u8 | hop u16 | n_entries u32 | body_len u32
//!   msg_id u64 | t_send_ns u64 | encode_ns u32 | reserved u32
//! entry header (64 bytes)
//!   seq u64 | position u32 | kind u8 | codec u8 | reserved u16
//!   n_values u32 | wire_values u32 | payload_len u32 | token u32
//!   commitment [u8; 32]
//! entry payload (payload_len bytes)
//! ```
//!
//! `commitment` is the per-stage hash commitment: BLAKE3 of the canonical
//! `i64` bytes of the hidden state (or the logits hash for a sampled token).
//! It does not depend on the codec. `t_send_ns` and `encode_ns` are trace
//! fields used by the benchmark when both ends share a clock; a multi-host
//! runtime may leave them zero.
//!
//! `wire_values >= n_values` lets a benchmark tile the real hidden state up
//! to a larger model's width ("ballast") so a shaped link carries frames of
//! the size that model would send. Receivers decode all `wire_values` and keep
//! the first `n_values`. Production senders set the two equal.
//!
//! Transports implement [`StageSink`] and [`StageSource`], which move whole
//! framed messages. Implementations here: persistent tuned TCP
//! ([`TcpStageSink`] / [`TcpStageSource`]) and in-process channels
//! ([`mem_link`]). [`super::shaper::ShapedSink`] wraps any sink with WAN
//! delay, jitter and a bounded uplink.

use super::codec::{self, CodecChoice};
use arc_crypto::Hash256;
use std::io::{self, Read, Write};
use std::net::{SocketAddr, TcpListener, TcpStream};
use std::sync::mpsc;
use std::time::Duration;

pub const MAGIC: [u8; 4] = *b"ARCS";
pub const VERSION: u8 = 1;
pub const HEADER_LEN: usize = 40;
pub const ENTRY_HEADER_LEN: usize = 64;
/// Upper bound on a message body. A Kimi-width (7168) raw hidden state is
/// 56 KiB per sequence, so 256 MiB admits thousands of sequences per
/// micro-batch while refusing a hostile length field.
pub const MAX_BODY: usize = 256 << 20;
/// Upper bound on values in one hidden entry (ballast included): 4 Mi values,
/// about 570 times Kimi's hidden width.
pub const MAX_VALUES: usize = 1 << 22;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
#[repr(u8)]
pub enum MessageKind {
    Data = 0,
    Shutdown = 1,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum EntryBody {
    Token(u32),
    Hidden(Vec<i64>),
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Entry {
    pub seq: u64,
    pub position: u32,
    pub body: EntryBody,
    pub commitment: Hash256,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Message {
    pub kind: MessageKind,
    pub hop: u16,
    pub msg_id: u64,
    pub entries: Vec<Entry>,
}

/// Trace fields read from a received header.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct Trace {
    pub t_send_ns: u64,
    pub encode_ns: u32,
}

#[derive(Clone, Copy, Debug)]
pub struct EncodeOptions {
    pub codec: CodecChoice,
    /// Tile hidden states up to this many values on the wire (benchmark only).
    pub ballast_dim: Option<usize>,
}

impl Default for EncodeOptions {
    fn default() -> Self {
        Self {
            codec: CodecChoice::Auto,
            ballast_dim: None,
        }
    }
}

/// Byte accounting for one encoded message.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct EncodeStats {
    /// Bytes on the wire, headers included.
    pub wire_bytes: usize,
    /// Hidden values on the wire (ballast included).
    pub values: usize,
    /// Payload bytes the same values take as raw `i64`.
    pub raw_payload_bytes: usize,
    /// Payload bytes actually sent.
    pub payload_bytes: usize,
}

#[derive(Debug, thiserror::Error)]
pub enum WireError {
    #[error("bad magic or version")]
    BadMagic,
    #[error("body of {0} bytes exceeds the limit")]
    TooLarge(usize),
    #[error("truncated message")]
    Truncated,
    #[error("unknown message or entry kind {0}")]
    BadKind(u8),
    #[error("entry has wire_values {wire} < n_values {real}")]
    BadBallast { wire: u32, real: u32 },
    #[error("codec: {0}")]
    Codec(#[from] codec::CodecError),
    #[error("commitment mismatch for seq {seq} position {position}")]
    Commitment { seq: u64, position: u32 },
}

/// Encode `msg` into `out` (cleared first). `t_send_ns`/`encode_ns` are
/// patched in later with [`stamp`] once the message is about to leave.
pub fn encode(msg: &Message, opts: &EncodeOptions, out: &mut Vec<u8>) -> EncodeStats {
    out.clear();
    out.resize(HEADER_LEN, 0);
    let mut stats = EncodeStats::default();
    let mut tiled: Vec<i64> = Vec::new();
    for e in &msg.entries {
        let header_at = out.len();
        out.resize(header_at + ENTRY_HEADER_LEN, 0);
        let (kind, codec, n_values, wire_values, token, payload_len) = match &e.body {
            EntryBody::Token(t) => (0u8, 0u8, 0u32, 0u32, *t, 0usize),
            EntryBody::Hidden(h) => {
                let wire = match opts.ballast_dim {
                    Some(dim) if dim > h.len() && !h.is_empty() => {
                        tiled.clear();
                        tiled.extend(h.iter().cycle().take(dim));
                        &tiled[..]
                    }
                    _ => &h[..],
                };
                let start = out.len();
                let c = codec::encode_into(wire, opts.codec, out);
                let len = out.len() - start;
                stats.values += wire.len();
                stats.raw_payload_bytes += wire.len() * 8;
                stats.payload_bytes += len;
                (1u8, c as u8, h.len() as u32, wire.len() as u32, 0u32, len)
            }
        };
        let hdr = &mut out[header_at..header_at + ENTRY_HEADER_LEN];
        hdr[0..8].copy_from_slice(&e.seq.to_le_bytes());
        hdr[8..12].copy_from_slice(&e.position.to_le_bytes());
        hdr[12] = kind;
        hdr[13] = codec;
        hdr[16..20].copy_from_slice(&n_values.to_le_bytes());
        hdr[20..24].copy_from_slice(&wire_values.to_le_bytes());
        hdr[24..28].copy_from_slice(&(payload_len as u32).to_le_bytes());
        hdr[28..32].copy_from_slice(&token.to_le_bytes());
        hdr[32..64].copy_from_slice(&e.commitment.0);
    }
    let body_len = out.len() - HEADER_LEN;
    let h = &mut out[..HEADER_LEN];
    h[0..4].copy_from_slice(&MAGIC);
    h[4] = VERSION;
    h[5] = msg.kind as u8;
    h[6..8].copy_from_slice(&msg.hop.to_le_bytes());
    h[8..12].copy_from_slice(&(msg.entries.len() as u32).to_le_bytes());
    h[12..16].copy_from_slice(&(body_len as u32).to_le_bytes());
    h[16..24].copy_from_slice(&msg.msg_id.to_le_bytes());
    stats.wire_bytes = out.len();
    stats
}

/// Write the trace fields into an encoded message's header.
pub fn stamp(frame: &mut [u8], t_send_ns: u64, encode_ns: u32) {
    frame[24..32].copy_from_slice(&t_send_ns.to_le_bytes());
    frame[32..36].copy_from_slice(&encode_ns.to_le_bytes());
}

fn body_len_of(header: &[u8; HEADER_LEN]) -> Result<usize, WireError> {
    if header[0..4] != MAGIC || header[4] != VERSION {
        return Err(WireError::BadMagic);
    }
    let len = u32::from_le_bytes(header[12..16].try_into().expect("4")) as usize;
    if len > MAX_BODY {
        return Err(WireError::TooLarge(len));
    }
    Ok(len)
}

/// Decode a full frame. With `verify`, every hidden entry's commitment is
/// recomputed from the decoded values and checked against the header.
pub fn decode(frame: &[u8], verify: bool) -> Result<(Message, Trace), WireError> {
    if frame.len() < HEADER_LEN {
        return Err(WireError::Truncated);
    }
    let header: &[u8; HEADER_LEN] = frame[..HEADER_LEN].try_into().expect("header");
    let body_len = body_len_of(header)?;
    if frame.len() != HEADER_LEN + body_len {
        return Err(WireError::Truncated);
    }
    let kind = match header[5] {
        0 => MessageKind::Data,
        1 => MessageKind::Shutdown,
        k => return Err(WireError::BadKind(k)),
    };
    let hop = u16::from_le_bytes(header[6..8].try_into().expect("2"));
    let n_entries = u32::from_le_bytes(header[8..12].try_into().expect("4")) as usize;
    let msg_id = u64::from_le_bytes(header[16..24].try_into().expect("8"));
    let trace = Trace {
        t_send_ns: u64::from_le_bytes(header[24..32].try_into().expect("8")),
        encode_ns: u32::from_le_bytes(header[32..36].try_into().expect("4")),
    };
    if n_entries > body_len / ENTRY_HEADER_LEN {
        return Err(WireError::Truncated);
    }
    let mut entries = Vec::with_capacity(n_entries);
    let mut at = HEADER_LEN;
    for _ in 0..n_entries {
        if at + ENTRY_HEADER_LEN > frame.len() {
            return Err(WireError::Truncated);
        }
        let h = &frame[at..at + ENTRY_HEADER_LEN];
        at += ENTRY_HEADER_LEN;
        let seq = u64::from_le_bytes(h[0..8].try_into().expect("8"));
        let position = u32::from_le_bytes(h[8..12].try_into().expect("4"));
        let n_values = u32::from_le_bytes(h[16..20].try_into().expect("4"));
        let wire_values = u32::from_le_bytes(h[20..24].try_into().expect("4"));
        let payload_len = u32::from_le_bytes(h[24..28].try_into().expect("4")) as usize;
        let token = u32::from_le_bytes(h[28..32].try_into().expect("4"));
        let commitment = Hash256(h[32..64].try_into().expect("32"));
        if at + payload_len > frame.len() {
            return Err(WireError::Truncated);
        }
        let payload = &frame[at..at + payload_len];
        at += payload_len;
        let body = match h[12] {
            0 => EntryBody::Token(token),
            1 => {
                if wire_values < n_values {
                    return Err(WireError::BadBallast {
                        wire: wire_values,
                        real: n_values,
                    });
                }
                // Bound the allocation a hostile header can ask for: one
                // width byte describes at most one block, and no entry may
                // exceed MAX_VALUES.
                if wire_values as usize > MAX_VALUES
                    || wire_values as usize > payload_len.saturating_mul(codec::BLOCK)
                {
                    return Err(WireError::Truncated);
                }
                let mut values = codec::decode(h[13], payload, wire_values as usize)?;
                values.truncate(n_values as usize);
                if verify && codec::commit_i64(&values) != commitment {
                    return Err(WireError::Commitment { seq, position });
                }
                EntryBody::Hidden(values)
            }
            k => return Err(WireError::BadKind(k)),
        };
        entries.push(Entry {
            seq,
            position,
            body,
            commitment,
        });
    }
    if at != frame.len() {
        return Err(WireError::Truncated);
    }
    Ok((
        Message {
            kind,
            hop,
            msg_id,
            entries,
        },
        trace,
    ))
}

// ─── Transport trait ────────────────────────────────────────────────────────

/// Sending half of a stage-to-stage link. `send_frame` takes one complete
/// encoded message and returns once the transport owns it.
pub trait StageSink: Send {
    fn send_frame(&mut self, frame: &[u8]) -> io::Result<()>;
}

/// Receiving half of a stage-to-stage link. Fills `buf` with one complete
/// encoded message. Returns `Ok(false)` on a clean end of stream.
pub trait StageSource: Send {
    fn recv_frame(&mut self, buf: &mut Vec<u8>) -> io::Result<bool>;
}

impl<T: StageSink + ?Sized> StageSink for Box<T> {
    fn send_frame(&mut self, frame: &[u8]) -> io::Result<()> {
        (**self).send_frame(frame)
    }
}

impl<T: StageSource + ?Sized> StageSource for Box<T> {
    fn recv_frame(&mut self, buf: &mut Vec<u8>) -> io::Result<bool> {
        (**self).recv_frame(buf)
    }
}

// ─── In-process link ────────────────────────────────────────────────────────

pub struct MemSink(mpsc::Sender<Vec<u8>>);
pub struct MemSource(mpsc::Receiver<Vec<u8>>);

/// A link between two threads of one process.
pub fn mem_link() -> (MemSink, MemSource) {
    let (tx, rx) = mpsc::channel();
    (MemSink(tx), MemSource(rx))
}

impl StageSink for MemSink {
    fn send_frame(&mut self, frame: &[u8]) -> io::Result<()> {
        self.0
            .send(frame.to_vec())
            .map_err(|_| io::Error::new(io::ErrorKind::BrokenPipe, "receiver dropped"))
    }
}

impl StageSource for MemSource {
    fn recv_frame(&mut self, buf: &mut Vec<u8>) -> io::Result<bool> {
        match self.0.recv() {
            Ok(frame) => {
                *buf = frame;
                Ok(true)
            }
            Err(_) => Ok(false),
        }
    }
}

// ─── TCP link ───────────────────────────────────────────────────────────────

/// Socket tuning for a stage link.
#[derive(Clone, Copy, Debug)]
pub struct TcpTuning {
    /// Disable Nagle: a boundary message must leave as soon as it is written.
    pub nodelay: bool,
    /// Kernel send/receive buffer size. Should cover the link's
    /// bandwidth-delay product so a whole micro-batch fits in flight
    /// (100 Mb/s × 60 ms ≈ 750 KB).
    pub socket_buffer_bytes: usize,
}

impl Default for TcpTuning {
    fn default() -> Self {
        Self {
            nodelay: true,
            socket_buffer_bytes: 4 << 20,
        }
    }
}

fn tune(stream: &TcpStream, tuning: &TcpTuning) -> io::Result<()> {
    stream.set_nodelay(tuning.nodelay)?;
    #[cfg(unix)]
    {
        use std::os::fd::AsRawFd;
        let fd = stream.as_raw_fd();
        let size = tuning.socket_buffer_bytes.min(i32::MAX as usize) as libc::c_int;
        for opt in [libc::SO_SNDBUF, libc::SO_RCVBUF] {
            // SAFETY: `fd` is an open socket owned by `stream` for the whole
            // call; the option value is a live `c_int` of the size passed.
            // The kernel may clamp the size; that is not an error.
            unsafe {
                libc::setsockopt(
                    fd,
                    libc::SOL_SOCKET,
                    opt,
                    (&size as *const libc::c_int).cast(),
                    std::mem::size_of::<libc::c_int>() as libc::socklen_t,
                );
            }
        }
    }
    Ok(())
}

/// Persistent, tuned TCP sender. One connection per hop for the life of the
/// pipeline; each message is a single `write_all` of the already-framed
/// buffer, so no per-message connection setup or intermediate copy.
pub struct TcpStageSink {
    stream: TcpStream,
}

impl TcpStageSink {
    /// Connect with retries (the downstream stage may still be starting).
    pub fn connect(addr: SocketAddr, tuning: TcpTuning, attempts: u32) -> io::Result<Self> {
        let mut last = None;
        for i in 0..attempts.max(1) {
            match TcpStream::connect(addr) {
                Ok(stream) => {
                    tune(&stream, &tuning)?;
                    return Ok(Self { stream });
                }
                Err(e) => {
                    last = Some(e);
                    std::thread::sleep(Duration::from_millis(20 * (i as u64 + 1)));
                }
            }
        }
        Err(last.unwrap_or_else(|| io::Error::other("connect failed")))
    }
}

impl StageSink for TcpStageSink {
    fn send_frame(&mut self, frame: &[u8]) -> io::Result<()> {
        self.stream.write_all(frame)
    }
}

/// TCP receiver for one hop. Reads the fixed header, validates it, then reads
/// the body into the caller's reused buffer.
pub struct TcpStageSource {
    stream: TcpStream,
}

impl TcpStageSource {
    pub fn accept(listener: &TcpListener, tuning: TcpTuning) -> io::Result<Self> {
        let (stream, _) = listener.accept()?;
        tune(&stream, &tuning)?;
        Ok(Self { stream })
    }
}

impl StageSource for TcpStageSource {
    fn recv_frame(&mut self, buf: &mut Vec<u8>) -> io::Result<bool> {
        let mut header = [0u8; HEADER_LEN];
        match self.stream.read_exact(&mut header) {
            Ok(()) => {}
            Err(e) if e.kind() == io::ErrorKind::UnexpectedEof => return Ok(false),
            Err(e) => return Err(e),
        }
        let body = body_len_of(&header)
            .map_err(|e| io::Error::new(io::ErrorKind::InvalidData, e.to_string()))?;
        buf.clear();
        buf.extend_from_slice(&header);
        buf.resize(HEADER_LEN + body, 0);
        self.stream.read_exact(&mut buf[HEADER_LEN..])?;
        Ok(true)
    }
}

/// A connected loopback TCP pair, for tests and the benchmark.
pub fn tcp_loopback(tuning: TcpTuning) -> io::Result<(TcpStageSink, TcpStageSource)> {
    let listener = TcpListener::bind("127.0.0.1:0")?;
    let addr = listener.local_addr()?;
    let sink = TcpStageSink::connect(addr, tuning, 5)?;
    let source = TcpStageSource::accept(&listener, tuning)?;
    Ok((sink, source))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn sample() -> Message {
        let hidden: Vec<i64> = (0..300).map(|i| (i * 7919 - 1_000_000) as i64).collect();
        Message {
            kind: MessageKind::Data,
            hop: 2,
            msg_id: 99,
            entries: vec![
                Entry {
                    seq: 1,
                    position: 4,
                    commitment: codec::commit_i64(&hidden),
                    body: EntryBody::Hidden(hidden),
                },
                Entry {
                    seq: 2,
                    position: 4,
                    body: EntryBody::Token(1234),
                    commitment: Hash256([9; 32]),
                },
            ],
        }
    }

    #[test]
    fn encode_decode_roundtrip_all_codecs() {
        for codec in [CodecChoice::Raw, CodecChoice::Auto] {
            for ballast in [None, Some(1000)] {
                let msg = sample();
                let mut buf = Vec::new();
                let stats = encode(
                    &msg,
                    &EncodeOptions {
                        codec,
                        ballast_dim: ballast,
                    },
                    &mut buf,
                );
                assert_eq!(stats.wire_bytes, buf.len());
                stamp(&mut buf, 77, 5);
                let (back, trace) = decode(&buf, true).expect("decode");
                assert_eq!(back, msg);
                assert_eq!(trace.t_send_ns, 77);
                assert_eq!(trace.encode_ns, 5);
                if ballast.is_some() {
                    assert_eq!(stats.values, 1000);
                }
            }
        }
    }

    #[test]
    fn tampered_hidden_state_fails_commitment() {
        let msg = sample();
        let mut buf = Vec::new();
        encode(
            &msg,
            &EncodeOptions {
                codec: CodecChoice::Raw,
                ballast_dim: None,
            },
            &mut buf,
        );
        // Flip one bit in the first value of the first entry's payload.
        buf[HEADER_LEN + ENTRY_HEADER_LEN] ^= 1;
        assert!(matches!(
            decode(&buf, true),
            Err(WireError::Commitment { seq: 1, .. })
        ));
        // Without verification the bad value is returned (caller's choice).
        assert!(decode(&buf, false).is_ok());
    }

    #[test]
    fn rejects_truncated_and_oversized_frames() {
        let msg = sample();
        let mut buf = Vec::new();
        encode(&msg, &EncodeOptions::default(), &mut buf);
        assert!(decode(&buf[..buf.len() - 1], true).is_err());
        let mut bad = buf.clone();
        bad[0] = b'X';
        assert!(matches!(decode(&bad, true), Err(WireError::BadMagic)));
        let mut huge = buf.clone();
        huge[12..16].copy_from_slice(&u32::MAX.to_le_bytes());
        assert!(matches!(decode(&huge, true), Err(WireError::TooLarge(_))));
        let mut many = buf.clone();
        many[8..12].copy_from_slice(&1_000_000u32.to_le_bytes());
        assert!(decode(&many, true).is_err());
    }

    #[test]
    fn tcp_link_carries_frames_in_order() {
        let (mut sink, mut source) = tcp_loopback(TcpTuning::default()).expect("loopback");
        let t = std::thread::spawn(move || {
            for id in 0..50u64 {
                let mut msg = sample();
                msg.msg_id = id;
                let mut buf = Vec::new();
                encode(&msg, &EncodeOptions::default(), &mut buf);
                sink.send_frame(&buf).expect("send");
            }
        });
        let mut buf = Vec::new();
        for id in 0..50u64 {
            assert!(source.recv_frame(&mut buf).expect("recv"));
            let (msg, _) = decode(&buf, true).expect("decode");
            assert_eq!(msg.msg_id, id);
        }
        t.join().expect("sender");
        assert!(!source.recv_frame(&mut buf).expect("eof"));
    }
}
