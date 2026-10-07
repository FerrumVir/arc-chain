//! Island frames: the binary messages that travel the stage ring.
//!
//! Every frame is `kind (u8) || body`, little-endian throughout. Activations
//! travel in the narrowest lossless width (`i8`, `i16`, `i32` or `i64` for
//! the whole vector), so a frame carries exactly the Q16 integers the stage
//! produced and every hash recomputed from it equals the sender's.

use crate::modern::ModernError;
use crate::modern::arith::Selection;

const STEP: u8 = 1;
const CLOSE: u8 = 2;
const REVEAL: u8 = 3;
const PING: u8 = 4;
const SHUTDOWN: u8 = 5;
const ERROR: u8 = 6;

fn invalid(what: impl Into<String>) -> ModernError {
    ModernError::Invalid(format!("island frame: {}", what.into()))
}

/// Byte writer.
#[derive(Default)]
pub struct Writer {
    pub bytes: Vec<u8>,
}

impl Writer {
    pub fn u8(&mut self, v: u8) {
        self.bytes.push(v);
    }
    pub fn u32(&mut self, v: u32) {
        self.bytes.extend_from_slice(&v.to_le_bytes());
    }
    pub fn u64(&mut self, v: u64) {
        self.bytes.extend_from_slice(&v.to_le_bytes());
    }
    pub fn i64(&mut self, v: i64) {
        self.bytes.extend_from_slice(&v.to_le_bytes());
    }
    pub fn count(&mut self, n: usize) {
        self.u32(u32::try_from(n).expect("island frames hold fewer than 2^32 elements"));
    }
    pub fn blob(&mut self, b: &[u8]) {
        self.count(b.len());
        self.bytes.extend_from_slice(b);
    }
    pub fn str(&mut self, s: &str) {
        self.blob(s.as_bytes());
    }
    pub fn u32s(&mut self, v: &[u32]) {
        self.count(v.len());
        for x in v {
            self.u32(*x);
        }
    }
    pub fn hashes(&mut self, v: &[[u8; 32]]) {
        self.count(v.len());
        for h in v {
            self.bytes.extend_from_slice(h);
        }
    }
    /// Activations in the narrowest width that holds every value exactly.
    pub fn acts(&mut self, v: &[i64]) {
        let width = activation_width(v);
        self.u8(width as u8);
        self.count(v.len());
        match width {
            1 => self.bytes.extend(v.iter().map(|&x| x as i8 as u8)),
            2 => v
                .iter()
                .for_each(|&x| self.bytes.extend_from_slice(&(x as i16).to_le_bytes())),
            4 => v
                .iter()
                .for_each(|&x| self.bytes.extend_from_slice(&(x as i32).to_le_bytes())),
            _ => v.iter().for_each(|&x| self.i64(x)),
        }
    }
    pub fn selection(&mut self, s: Selection) {
        self.u8(match s {
            Selection::Argmax => 0,
            Selection::Rp64Argmax => 1,
        });
    }
}

/// The narrowest of 1, 2, 4 and 8 bytes per value that holds `values`
/// exactly (two's complement).
pub fn activation_width(values: &[i64]) -> usize {
    let (lo, hi) = values
        .iter()
        .fold((0i64, 0i64), |(lo, hi), &x| (lo.min(x), hi.max(x)));
    if lo >= i64::from(i8::MIN) && hi <= i64::from(i8::MAX) {
        1
    } else if lo >= i64::from(i16::MIN) && hi <= i64::from(i16::MAX) {
        2
    } else if lo >= i64::from(i32::MIN) && hi <= i64::from(i32::MAX) {
        4
    } else {
        8
    }
}

/// Byte reader.
pub struct Reader<'a> {
    bytes: &'a [u8],
    at: usize,
}

impl<'a> Reader<'a> {
    pub fn new(bytes: &'a [u8]) -> Self {
        Self { bytes, at: 0 }
    }
    pub fn take(&mut self, n: usize) -> Result<&'a [u8], ModernError> {
        let end = self
            .at
            .checked_add(n)
            .filter(|&e| e <= self.bytes.len())
            .ok_or_else(|| invalid("truncated"))?;
        let out = &self.bytes[self.at..end];
        self.at = end;
        Ok(out)
    }
    pub fn u8(&mut self) -> Result<u8, ModernError> {
        Ok(self.take(1)?[0])
    }
    pub fn u32(&mut self) -> Result<u32, ModernError> {
        Ok(u32::from_le_bytes(self.take(4)?.try_into().expect("4")))
    }
    pub fn u64(&mut self) -> Result<u64, ModernError> {
        Ok(u64::from_le_bytes(self.take(8)?.try_into().expect("8")))
    }
    pub fn i64(&mut self) -> Result<i64, ModernError> {
        Ok(i64::from_le_bytes(self.take(8)?.try_into().expect("8")))
    }
    pub fn count(&mut self) -> Result<usize, ModernError> {
        let n = self.u32()? as usize;
        // Every element takes at least one byte: a length beyond the rest of
        // the frame is malformed, and refusing it bounds every allocation.
        if n > self.bytes.len() - self.at {
            return Err(invalid("length beyond the frame"));
        }
        Ok(n)
    }
    pub fn blob(&mut self) -> Result<&'a [u8], ModernError> {
        let n = self.count()?;
        self.take(n)
    }
    pub fn str(&mut self) -> Result<String, ModernError> {
        String::from_utf8(self.blob()?.to_vec()).map_err(|_| invalid("string is not UTF-8"))
    }
    pub fn u32s(&mut self) -> Result<Vec<u32>, ModernError> {
        let n = self.count()?;
        (0..n).map(|_| self.u32()).collect()
    }
    pub fn hashes(&mut self) -> Result<Vec<[u8; 32]>, ModernError> {
        let n = self.count()?;
        (0..n)
            .map(|_| Ok(self.take(32)?.try_into().expect("32")))
            .collect()
    }
    pub fn acts(&mut self) -> Result<Vec<i64>, ModernError> {
        let width = self.u8()? as usize;
        let n = self.count()?;
        let raw = self.take(n.checked_mul(width).ok_or_else(|| invalid("width"))?)?;
        Ok(match width {
            1 => raw.iter().map(|&b| i64::from(b as i8)).collect(),
            2 => raw
                .chunks_exact(2)
                .map(|c| i64::from(i16::from_le_bytes([c[0], c[1]])))
                .collect(),
            4 => raw
                .chunks_exact(4)
                .map(|c| i64::from(i32::from_le_bytes(c.try_into().expect("4"))))
                .collect(),
            8 => raw
                .chunks_exact(8)
                .map(|c| i64::from_le_bytes(c.try_into().expect("8")))
                .collect(),
            other => return Err(invalid(format!("activation width {other}"))),
        })
    }
    pub fn selection(&mut self) -> Result<Selection, ModernError> {
        match self.u8()? {
            0 => Ok(Selection::Argmax),
            1 => Ok(Selection::Rp64Argmax),
            other => Err(invalid(format!("selection {other}"))),
        }
    }
    pub fn done(&self) -> Result<(), ModernError> {
        if self.at == self.bytes.len() {
            Ok(())
        } else {
            Err(invalid("trailing bytes"))
        }
    }
}

/// One stage's commitment for one item: for every position of the item, the
/// activation hashes (spec §6.2) at boundaries `first_layer ..= end_layer`,
/// input boundary first. Position-major.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct StageCommit {
    pub first_layer: u32,
    pub end_layer: u32,
    pub hashes: Vec<[u8; 32]>,
}

impl StageCommit {
    /// Hashes per position: `end - first + 1`.
    pub fn per_position(&self) -> usize {
        (self.end_layer - self.first_layer) as usize + 1
    }

    pub fn positions(&self) -> usize {
        self.hashes.len() / self.per_position()
    }

    /// The hashes of position `i` of the item.
    pub fn position(&self, i: usize) -> &[[u8; 32]] {
        let n = self.per_position();
        &self.hashes[i * n..(i + 1) * n]
    }

    fn write(&self, w: &mut Writer) {
        w.u32(self.first_layer);
        w.u32(self.end_layer);
        w.hashes(&self.hashes);
    }

    fn read(r: &mut Reader<'_>) -> Result<Self, ModernError> {
        let commit = Self {
            first_layer: r.u32()?,
            end_layer: r.u32()?,
            hashes: r.hashes()?,
        };
        if commit.end_layer <= commit.first_layer
            || !commit.hashes.len().is_multiple_of(commit.per_position())
        {
            return Err(invalid("stage commit shape"));
        }
        Ok(commit)
    }
}

/// One sequence's work in a step: consecutive positions `start ..` with
/// their token ids, the boundary activations entering the next stage, and
/// what every stage so far committed.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Item {
    pub seq: u64,
    pub start: u32,
    pub prompt_len: u32,
    pub selection: Selection,
    /// Token ids forwarded at positions `start .. start + tokens.len()`.
    pub tokens: Vec<u32>,
    /// `tokens.len() * d_model` values entering the next stage (empty before
    /// the first stage and after the last).
    pub hidden: Vec<i64>,
    /// Benchmark padding: zero bytes carried through every hop, to emulate
    /// the activation size of a wider model on a real link.
    pub pad: u32,
    pub commits: Vec<StageCommit>,
    /// Logits hash per position (filled by the last stage).
    pub logits: Vec<[u8; 32]>,
    /// The token selected at the item's last position, when that position
    /// is the prompt's last or later (filled by the last stage).
    pub next: Option<u32>,
    /// The first stage that failed this item, and why. Later stages pass the
    /// item through untouched.
    pub error: Option<String>,
}

impl Item {
    /// A new item as the coordinator sends it.
    pub fn new(
        seq: u64,
        start: u32,
        prompt_len: u32,
        selection: Selection,
        tokens: Vec<u32>,
    ) -> Self {
        Self {
            seq,
            start,
            prompt_len,
            selection,
            tokens,
            hidden: Vec::new(),
            pad: 0,
            commits: Vec::new(),
            logits: Vec::new(),
            next: None,
            error: None,
        }
    }

    fn write(&self, w: &mut Writer) {
        w.u64(self.seq);
        w.u32(self.start);
        w.u32(self.prompt_len);
        w.selection(self.selection);
        w.u32s(&self.tokens);
        w.acts(&self.hidden);
        w.count(self.pad as usize);
        w.bytes.resize(w.bytes.len() + self.pad as usize, 0);
        w.count(self.commits.len());
        for c in &self.commits {
            c.write(w);
        }
        w.hashes(&self.logits);
        match self.next {
            Some(t) => {
                w.u8(1);
                w.u32(t);
            }
            None => w.u8(0),
        }
        match &self.error {
            Some(e) => {
                w.u8(1);
                w.str(e);
            }
            None => w.u8(0),
        }
    }

    fn read(r: &mut Reader<'_>) -> Result<Self, ModernError> {
        let seq = r.u64()?;
        let start = r.u32()?;
        let prompt_len = r.u32()?;
        let selection = r.selection()?;
        let tokens = r.u32s()?;
        let hidden = r.acts()?;
        let pad = r.count()?;
        r.take(pad)?;
        let n = r.count()?;
        let commits = (0..n)
            .map(|_| StageCommit::read(r))
            .collect::<Result<_, _>>()?;
        let logits = r.hashes()?;
        let next = match r.u8()? {
            0 => None,
            _ => Some(r.u32()?),
        };
        let error = match r.u8()? {
            0 => None,
            _ => Some(r.str()?),
        };
        Ok(Self {
            seq,
            start,
            prompt_len,
            selection,
            tokens,
            hidden,
            pad: pad as u32,
            commits,
            logits,
            next,
            error,
        })
    }
}

/// What one stage reveals of a sequence for an audit: the token ids and the
/// boundary activations it received (its activation log), so a verifier
/// holding only this stage's weights can re-execute it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Revealed {
    pub first_layer: u32,
    pub end_layer: u32,
    pub prompt_len: u32,
    pub tokens: Vec<u32>,
    /// `tokens.len() * d_model` input values (empty for the first stage).
    pub inputs: Vec<i64>,
}

/// A frame on the ring.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Frame {
    /// Work for some sequences. `id` names the micro-batch.
    Step { id: u64, items: Vec<Item> },
    /// The sequences are finished: drop their caches; with `forget` also
    /// their activation logs.
    Close { seqs: Vec<u64>, forget: bool },
    /// Every stage appends its activation log of `seq`.
    Reveal { seq: u64, stages: Vec<Revealed> },
    /// Travels the ring untouched (hop latency).
    Ping { id: u64, payload: Vec<u8> },
    /// Every stage forwards it, then exits.
    Shutdown,
    /// A stage could not process a frame at all.
    Error { stage: String, message: String },
}

impl Frame {
    pub fn encode(&self) -> Vec<u8> {
        let mut w = Writer::default();
        match self {
            Frame::Step { id, items } => {
                w.u8(STEP);
                w.u64(*id);
                w.count(items.len());
                for item in items {
                    item.write(&mut w);
                }
            }
            Frame::Close { seqs, forget } => {
                w.u8(CLOSE);
                w.u8(u8::from(*forget));
                w.count(seqs.len());
                for s in seqs {
                    w.u64(*s);
                }
            }
            Frame::Reveal { seq, stages } => {
                w.u8(REVEAL);
                w.u64(*seq);
                w.count(stages.len());
                for s in stages {
                    w.u32(s.first_layer);
                    w.u32(s.end_layer);
                    w.u32(s.prompt_len);
                    w.u32s(&s.tokens);
                    w.acts(&s.inputs);
                }
            }
            Frame::Ping { id, payload } => {
                w.u8(PING);
                w.u64(*id);
                w.blob(payload);
            }
            Frame::Shutdown => w.u8(SHUTDOWN),
            Frame::Error { stage, message } => {
                w.u8(ERROR);
                w.str(stage);
                w.str(message);
            }
        }
        w.bytes
    }

    pub fn decode(bytes: &[u8]) -> Result<Self, ModernError> {
        let mut r = Reader::new(bytes);
        let frame = match r.u8()? {
            STEP => {
                let id = r.u64()?;
                let n = r.count()?;
                let items = (0..n)
                    .map(|_| Item::read(&mut r))
                    .collect::<Result<_, _>>()?;
                Frame::Step { id, items }
            }
            CLOSE => {
                let forget = r.u8()? != 0;
                let n = r.count()?;
                let seqs = (0..n).map(|_| r.u64()).collect::<Result<_, _>>()?;
                Frame::Close { seqs, forget }
            }
            REVEAL => {
                let seq = r.u64()?;
                let n = r.count()?;
                let stages = (0..n)
                    .map(|_| {
                        Ok(Revealed {
                            first_layer: r.u32()?,
                            end_layer: r.u32()?,
                            prompt_len: r.u32()?,
                            tokens: r.u32s()?,
                            inputs: r.acts()?,
                        })
                    })
                    .collect::<Result<_, ModernError>>()?;
                Frame::Reveal { seq, stages }
            }
            PING => Frame::Ping {
                id: r.u64()?,
                payload: r.blob()?.to_vec(),
            },
            SHUTDOWN => Frame::Shutdown,
            ERROR => Frame::Error {
                stage: r.str()?,
                message: r.str()?,
            },
            other => return Err(invalid(format!("kind {other}"))),
        };
        r.done()?;
        Ok(frame)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn frames_round_trip_and_activations_use_the_narrowest_exact_width() {
        let wide = vec![0, -1, i64::from(i32::MAX) + 1, -(1 << 40)];
        for (values, width) in [
            (vec![0i64, -128, 127], 1usize),
            (vec![-129, 32_767], 2),
            (vec![1 << 20, -(1 << 31)], 4),
            (wide.clone(), 8),
            (vec![], 1),
        ] {
            assert_eq!(activation_width(&values), width, "{values:?}");
            let mut w = Writer::default();
            w.acts(&values);
            assert_eq!(w.bytes.len(), 5 + width * values.len());
            assert_eq!(Reader::new(&w.bytes).acts().unwrap(), values);
        }
        let mut item = Item::new(7, 3, 2, Selection::Rp64Argmax, vec![4, 9]);
        item.hidden = wide;
        item.pad = 17;
        item.commits.push(StageCommit {
            first_layer: 0,
            end_layer: 2,
            hashes: vec![[5; 32]; 6],
        });
        item.logits = vec![[1; 32], [2; 32]];
        item.next = Some(11);
        item.error = Some("e".into());
        let frames = [
            Frame::Step {
                id: 3,
                items: vec![item.clone(), Item::new(8, 0, 1, Selection::Argmax, vec![1])],
            },
            Frame::Close {
                seqs: vec![7, 8],
                forget: true,
            },
            Frame::Reveal {
                seq: 7,
                stages: vec![Revealed {
                    first_layer: 2,
                    end_layer: 4,
                    prompt_len: 2,
                    tokens: vec![4, 9],
                    inputs: vec![1, -70_000],
                }],
            },
            Frame::Ping {
                id: 1,
                payload: vec![9; 100],
            },
            Frame::Shutdown,
            Frame::Error {
                stage: "[0, 2)".into(),
                message: "m".into(),
            },
        ];
        for frame in frames {
            let bytes = frame.encode();
            assert_eq!(Frame::decode(&bytes).unwrap(), frame);
            // Every strict prefix is refused, and so is a trailing byte.
            for cut in 0..bytes.len() {
                assert!(Frame::decode(&bytes[..cut]).is_err());
            }
            let mut longer = bytes.clone();
            longer.push(0);
            assert!(Frame::decode(&longer).is_err());
        }
    }
}
