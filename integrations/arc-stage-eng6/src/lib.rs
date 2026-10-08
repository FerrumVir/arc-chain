//! Adapter to ARC-68's actual island interfaces, pinned in Cargo.toml.
//!
//! Pass `TunedTransport` to the island runtime. This integration crate pins
//! ARC-68 separately so its dependency graph does not change the main workspace.
//! It preserves ARC-68's u32 length prefix and opaque frame bytes; ARCS
//! benchmark framing is not substituted for the island protocol.
use arc_inference::stage_net::wire::{StageSink, StageSource, TcpTuning, tune};
use arc_inference_eng6::modern::mla::island::transport::MAX_FRAME;
pub use arc_inference_eng6::modern::mla::island::transport::{Link, Listener, Transport};
use std::io::{self, Read, Write};
use std::net::{Shutdown, TcpListener, TcpStream};

/// ARC-68 transport with ENG-9's persistent TCP socket tuning.
#[derive(Clone, Copy, Debug, Default)]
pub struct TunedTransport {
    pub tuning: TcpTuning,
}

struct TunedListener {
    socket: TcpListener,
    tuning: TcpTuning,
}

/// A failed or explicitly closed link stays closed; a partial frame cannot
/// subsequently be mistaken for the beginning of another frame.
pub struct TunedLink {
    socket: TcpStream,
    closed: bool,
}

impl TunedLink {
    pub fn new(socket: TcpStream, tuning: TcpTuning) -> io::Result<Self> {
        tune(&socket, &tuning)?;
        Ok(Self {
            socket,
            closed: false,
        })
    }

    pub fn close(&mut self) -> io::Result<()> {
        if self.closed {
            return Ok(());
        }
        self.closed = true;
        self.socket.shutdown(Shutdown::Both)
    }

    fn check_open(&self) -> io::Result<()> {
        if self.closed {
            Err(io::Error::new(
                io::ErrorKind::BrokenPipe,
                "island link closed",
            ))
        } else {
            Ok(())
        }
    }

    fn finish<T>(&mut self, result: io::Result<T>) -> io::Result<T> {
        if result.is_err() {
            let _ = self.close();
        }
        result
    }
}

impl Link for TunedLink {
    fn send(&mut self, frame: &[u8]) -> io::Result<()> {
        self.check_open()?;
        if frame.len() > MAX_FRAME {
            return Err(io::Error::new(
                io::ErrorKind::InvalidInput,
                "frame too large",
            ));
        }
        // Two writes avoid copying the payload into a length-prefixed buffer.
        let result = self
            .socket
            .write_all(&(frame.len() as u32).to_le_bytes())
            .and_then(|()| self.socket.write_all(frame));
        self.finish(result)
    }

    fn recv(&mut self) -> io::Result<Vec<u8>> {
        self.check_open()?;
        let result = (|| {
            let mut length = [0; 4];
            self.socket.read_exact(&mut length)?;
            let length = u32::from_le_bytes(length) as usize;
            if length > MAX_FRAME {
                return Err(io::Error::new(
                    io::ErrorKind::InvalidData,
                    "frame too large",
                ));
            }
            let mut frame = vec![0; length];
            self.socket.read_exact(&mut frame)?;
            Ok(frame)
        })();
        self.finish(result)
    }

    fn alive(&mut self) -> bool {
        if self.closed {
            return false;
        }
        if self.socket.set_nonblocking(true).is_err() {
            let _ = self.close();
            return false;
        }
        let mut byte = [0];
        let alive = match self.socket.peek(&mut byte) {
            Ok(0) => false,
            Ok(_) => true,
            Err(e) => e.kind() == io::ErrorKind::WouldBlock,
        };
        let restored = self.socket.set_nonblocking(false).is_ok();
        if !alive || !restored {
            let _ = self.close();
        }
        !self.closed
    }
}

impl Listener for TunedListener {
    fn accept(&mut self) -> io::Result<Box<dyn Link>> {
        let (socket, _) = self.socket.accept()?;
        Ok(Box::new(TunedLink::new(socket, self.tuning)?))
    }
    fn address(&self) -> String {
        self.socket
            .local_addr()
            .expect("bound TCP listener")
            .to_string()
    }
}

impl Transport for TunedTransport {
    fn listen(&self, address: &str) -> io::Result<Box<dyn Listener>> {
        Ok(Box::new(TunedListener {
            socket: TcpListener::bind(address)?,
            tuning: self.tuning,
        }))
    }
    fn connect(&self, address: &str) -> io::Result<Box<dyn Link>> {
        Ok(Box::new(TunedLink::new(
            TcpStream::connect(address)?,
            self.tuning,
        )?))
    }
}

/// Move ENG-9 frames over any ARC-68 Link. EOF remains an error because the
/// upstream Link interface cannot distinguish clean EOF from a truncated frame.
/// The runtime's explicit Shutdown frame is the graceful termination signal.
pub struct StageLink(pub Box<dyn Link>);
impl StageSink for StageLink {
    fn send_frame(&mut self, frame: &[u8]) -> io::Result<()> {
        self.0.send(frame)
    }
}
impl StageSource for StageLink {
    fn recv_frame(&mut self, buf: &mut Vec<u8>) -> io::Result<bool> {
        *buf = self.0.recv()?;
        Ok(true)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use arc_inference_eng6::modern::arith::Selection;
    use arc_inference_eng6::modern::mla::boundary::activation_hash;
    use arc_inference_eng6::modern::mla::island::{
        commit::Ledger,
        transport::TcpTransport,
        wire::{Frame, Item, StageCommit},
    };

    #[test]
    fn actual_eng6_frames_tokens_and_commitments_cross_tuned_tcp() {
        // Both directions exercise interoperability with ARC-68's own TCP.
        for (server, client) in [
            (
                Box::new(TunedTransport::default()) as Box<dyn Transport>,
                Box::new(TcpTransport) as Box<dyn Transport>,
            ),
            (
                Box::new(TcpTransport) as Box<dyn Transport>,
                Box::new(TunedTransport::default()) as Box<dyn Transport>,
            ),
        ] {
            let mut listener = server.listen("127.0.0.1:0").unwrap();
            let address = listener.address();
            let mut item = Item::new(7, 0, 2, Selection::Argmax, vec![42, 43]);
            item.hidden = vec![i64::MIN, -1, 0, 127, 1 << 40, i64::MAX];
            let hash = activation_hash(&item.hidden);
            item.commits = vec![
                StageCommit {
                    first_layer: 0,
                    end_layer: 1,
                    hashes: vec![[1; 32], hash, [5; 32], hash],
                    logits: vec![],
                    selected: None,
                },
                StageCommit {
                    first_layer: 1,
                    end_layer: 2,
                    hashes: vec![hash, [2; 32], hash, [6; 32]],
                    logits: vec![[3; 32], [4; 32]],
                    selected: Some(19),
                },
            ];
            let mut expected_ledger = Ledger::new(2);
            expected_ledger.record(0, 2, &item.commits);
            assert!(expected_ledger.complete());
            let frames = vec![
                Frame::Step {
                    id: 11,
                    items: vec![item],
                },
                Frame::Close {
                    seqs: vec![7],
                    forget: true,
                },
                Frame::Error {
                    stage: "test".into(),
                    message: "injected".into(),
                },
                Frame::Shutdown,
            ];
            let expected = frames.clone();
            let worker = std::thread::spawn(move || {
                let mut link = listener.accept().unwrap();
                for expected in expected {
                    let bytes = link.recv().unwrap();
                    assert_eq!(Frame::decode(&bytes).unwrap(), expected);
                    link.send(&bytes).unwrap();
                }
            });
            let mut link = client.connect(&address).unwrap();
            for frame in frames {
                let bytes = frame.encode();
                link.send(&bytes).unwrap();
                let returned = link.recv().unwrap();
                assert_eq!(returned, bytes);
                if let Frame::Step { items, .. } = Frame::decode(&returned).unwrap() {
                    let item = &items[0];
                    assert_eq!(
                        arc_inference::stage_net::codec::commit_i64(&item.hidden).0,
                        hash
                    );
                    let mut ledger = Ledger::new(2);
                    ledger.record(0, 2, &item.commits);
                    assert!(ledger.complete());
                    assert_eq!(item.tokens, vec![42, 43]);
                    assert!(item.commits[0].logits.is_empty());
                    assert_eq!(item.commits[0].selected, None);
                    assert_eq!(item.commits[1].logits, vec![[3; 32], [4; 32]]);
                    assert_eq!(item.commits[1].selected, Some(19));
                    let head = ledger.stage_commits(1, 2).unwrap();
                    assert_eq!(head[0].logits, Some([3; 32]));
                    assert_eq!(head[0].selected, None);
                    assert_eq!(head[1].logits, Some([4; 32]));
                    assert_eq!(head[1].selected, Some(19));
                    assert_eq!(
                        ledger.boundary_digests(),
                        expected_ledger.boundary_digests()
                    );
                    for (first, end) in [(0, 1), (1, 2)] {
                        assert_eq!(
                            ledger.stage_commits(first, end),
                            expected_ledger.stage_commits(first, end)
                        );
                        assert_eq!(
                            ledger.stage_root(7, first, end),
                            expected_ledger.stage_root(7, first, end)
                        );
                    }
                    // The last-stage root must bind both new fields, including
                    // selected-token presence, even when all activations agree.
                    for change in 0..3 {
                        let mut altered = item.commits.clone();
                        match change {
                            0 => altered[1].logits[0][0] ^= 1,
                            1 => altered[1].selected = Some(20),
                            _ => altered[1].selected = None,
                        }
                        let mut changed = Ledger::new(2);
                        changed.record(0, 2, &altered);
                        assert!(changed.complete());
                        assert_eq!(changed.boundary_digests(), ledger.boundary_digests());
                        assert_ne!(changed.stage_root(7, 1, 2), ledger.stage_root(7, 1, 2));
                    }
                    // Native Ledger rejects logits/token metadata on a
                    // non-head stage and missing logits at the head.
                    for change in 0..3 {
                        let mut malformed = item.commits.clone();
                        match change {
                            0 => malformed[0].logits = vec![[3; 32], [4; 32]],
                            1 => malformed[0].selected = Some(19),
                            _ => malformed[1].logits.clear(),
                        }
                        let mut rejected = Ledger::new(2);
                        rejected.record(0, 2, &malformed);
                        assert!(!rejected.complete());
                        assert!(!rejected.malformed.is_empty());
                    }
                }
            }
            worker.join().unwrap();
            assert!(link.recv().is_err());
            assert!(!link.alive());
        }
    }

    #[test]
    fn stage_adapter_carries_exact_arcs_frames() {
        use arc_inference::stage_net::wire::{
            self, EncodeOptions, Entry, EntryBody, Message, MessageKind,
        };
        let transport = TunedTransport::default();
        let mut listener = transport.listen("127.0.0.1:0").unwrap();
        let mut sender = StageLink(transport.connect(&listener.address()).unwrap());
        let mut receiver = StageLink(listener.accept().unwrap());
        let hidden = vec![-129, 0, i64::MAX];
        let msg = Message {
            kind: MessageKind::Data,
            hop: 1,
            msg_id: 3,
            entries: vec![Entry {
                seq: 2,
                position: 1,
                commitment: arc_inference::stage_net::codec::commit_i64(&hidden),
                body: EntryBody::Hidden(hidden),
            }],
        };
        let mut bytes = Vec::new();
        wire::encode(&msg, &EncodeOptions::default(), &mut bytes);
        sender.send_frame(&bytes).unwrap();
        let mut received = Vec::new();
        assert!(receiver.recv_frame(&mut received).unwrap());
        assert_eq!(received, bytes);
        assert_eq!(wire::decode(&received, true).unwrap().0, msg);
        drop(sender);
        assert!(receiver.recv_frame(&mut received).is_err());
    }

    #[test]
    fn truncated_oversized_and_closed_links_fail_without_reuse() {
        for bytes in [
            vec![],
            vec![1, 0],
            vec![4, 0, 0, 0, 42],
            ((MAX_FRAME + 1) as u32).to_le_bytes().to_vec(),
        ] {
            let listener = TcpListener::bind("127.0.0.1:0").unwrap();
            let mut peer = TcpStream::connect(listener.local_addr().unwrap()).unwrap();
            let mut link =
                TunedLink::new(listener.accept().unwrap().0, TcpTuning::default()).unwrap();
            peer.write_all(&bytes).unwrap();
            peer.shutdown(Shutdown::Both).unwrap();
            let error = link.recv().unwrap_err();
            assert!(matches!(
                error.kind(),
                io::ErrorKind::UnexpectedEof | io::ErrorKind::InvalidData
            ));
            assert!(!link.alive());
            assert_eq!(
                link.send(b"later").unwrap_err().kind(),
                io::ErrorKind::BrokenPipe
            );
            assert_eq!(link.recv().unwrap_err().kind(), io::ErrorKind::BrokenPipe);
            link.close().unwrap();
        }
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let peer = TcpStream::connect(listener.local_addr().unwrap()).unwrap();
        let mut link = TunedLink::new(listener.accept().unwrap().0, TcpTuning::default()).unwrap();
        assert!(link.alive());
        link.close().unwrap();
        assert!(!link.alive());
        assert!(link.send(b"closed").is_err());
        drop(peer);
    }
}
