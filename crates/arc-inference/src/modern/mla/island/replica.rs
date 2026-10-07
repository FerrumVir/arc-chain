//! Replay-backed stage replicas for regional node churn.
//!
//! A session is a fresh stage cache, scoped to one connection. The relay
//! journals acknowledged inputs and response hashes. On disconnect, timeout,
//! or a lost response it opens the next replica, replays acknowledged inputs
//! (checking every response byte), then retries the pending input exactly once
//! on that fresh session. The old session can never forward a duplicate onto
//! the ring. This provides at-most-once visible results even if its last write
//! reached a worker which vanished before replying.
//!
//! This is warm replay, not KV mirroring. The relay is trusted and must remain
//! alive; its bounded in-memory journal is not coordinator crash recovery.

use std::sync::Arc;

use super::transport::{Link, Listener, Transport};
use super::wire::Frame;
use super::worker::{Downstream, StageWorker, incoming};
use crate::modern::ModernError;

/// ENG-9 can supply a transport/session implementation with overlap and its
/// own deadline policy. Every connect MUST create empty sequence caches.
pub trait StageSession: Send {
    fn exchange(&mut self, input: &[u8]) -> Result<Vec<u8>, ModernError>;
}

impl StageSession for Box<dyn Link> {
    fn exchange(&mut self, input: &[u8]) -> Result<Vec<u8>, ModernError> {
        self.send(input)
            .and_then(|()| self.recv())
            .map_err(|e| ModernError::Io(format!("stage session: {e}")))
    }
}

pub trait ReplicaConnector: Send + Sync {
    fn connect(&self, endpoint: &str) -> Result<Box<dyn StageSession>, ModernError>;
}

pub struct TransportConnector {
    pub transport: Arc<dyn Transport>,
    /// Pinned by the placement manifest before contacting any replica.
    pub stage_id: [u8; 32],
}

/// Identity covers the config, layer range and the bytes of every executed
/// segment (including embeddings/head), independent of package subsetting.
pub fn stage_identity(model: &crate::modern::mla::model::StageModel) -> [u8; 32] {
    let spec = model.stage();
    let identity = serde_json::json!({
        "protocol": "arc-regional-replica-v1",
        "config": model.config().to_json(),
        "range": [spec.first_layer, spec.end_layer],
        "segments": model.segments().iter().map(|s| s.to_json()).collect::<Vec<_>>(),
    });
    *blake3::hash(identity.to_string().as_bytes()).as_bytes()
}

impl ReplicaConnector for TransportConnector {
    fn connect(&self, endpoint: &str) -> Result<Box<dyn StageSession>, ModernError> {
        let mut link = self
            .transport
            .connect(endpoint)
            .map_err(|e| ModernError::Io(format!("replica {endpoint}: {e}")))?;
        let identity = link
            .recv()
            .map_err(|e| ModernError::Io(format!("replica identity: {e}")))?;
        if identity != self.stage_id {
            return Err(ModernError::Invalid(
                "replica stage identity differs from placement".into(),
            ));
        }
        Ok(Box::new(link))
    }
}

/// Ordered replicas of the SAME stage package/profile. Placement can order
/// heterogeneous nodes by measured service time. A failed endpoint is retired
/// for this relay's lifetime; reform at a request boundary to reuse it.
pub struct ReplicatedStage {
    connector: Arc<dyn ReplicaConnector>,
    endpoints: Vec<String>,
    next: usize,
    session: Option<Box<dyn StageSession>>,
    journal: Vec<(Vec<u8>, [u8; 32])>,
    journal_bytes: usize,
    journal_limit: usize,
    failed: Option<String>,
    pub failovers: usize,
    pub replayed_frames: usize,
}

impl ReplicatedStage {
    pub fn new(
        connector: Arc<dyn ReplicaConnector>,
        endpoints: Vec<String>,
        journal_limit: usize,
    ) -> Result<Self, ModernError> {
        if endpoints.is_empty() || journal_limit == 0 {
            return Err(ModernError::Invalid(
                "replicas and journal capacity required".into(),
            ));
        }
        Ok(Self {
            connector,
            endpoints,
            next: 0,
            session: None,
            journal: Vec::new(),
            journal_bytes: 0,
            journal_limit,
            failed: None,
            failovers: 0,
            replayed_frames: 0,
        })
    }

    fn recover(&mut self) -> Result<(), ModernError> {
        while let Some(endpoint) = self.endpoints.get(self.next) {
            self.next += 1;
            if self.next > 1 {
                self.failovers += 1;
            }
            let Ok(mut session) = self.connector.connect(endpoint) else {
                continue;
            };
            let mut valid = true;
            for (input, expected) in &self.journal {
                match session.exchange(input) {
                    Ok(output) if blake3::hash(&output).as_bytes() == expected => {
                        self.replayed_frames += 1;
                    }
                    _ => {
                        valid = false;
                        break;
                    }
                }
            }
            if valid {
                self.session = Some(session);
                return Ok(());
            }
        }
        Err(ModernError::Io(
            "stage replicas exhausted; no unchecked continuation".into(),
        ))
    }

    /// ENG-1 supplies ordinary Step frames containing multiple independent
    /// streams; ENG-8 supplies a Tree frame. A slow replica applies backpressure
    /// only to this stage relay; other stages can process other frames.
    pub fn process(&mut self, frame: Frame) -> Result<Frame, ModernError> {
        if let Some(reason) = &self.failed {
            return Err(ModernError::Invalid(format!(
                "relay permanently refused: {reason}"
            )));
        }
        let result = self.process_inner(frame);
        if let Err(error) = &result {
            // Never resume with a smaller frame after journal exhaustion, or
            // with state advanced by a rejected/model-error response.
            self.failed = Some(error.to_string());
            self.session = None;
        }
        result
    }

    fn process_inner(&mut self, frame: Frame) -> Result<Frame, ModernError> {
        let input = frame.encode();
        // Refuse before executing anything, rather than silently lose replay
        // coverage. Production admission must budget this journal explicitly.
        let cost = input.len().saturating_add(32);
        if cost > self.journal_limit.saturating_sub(self.journal_bytes) {
            return Err(ModernError::Invalid(
                "stage replay journal capacity exceeded".into(),
            ));
        }
        loop {
            if self.session.is_none() {
                self.recover()?;
            }
            match self.session.as_mut().expect("recovered").exchange(&input) {
                Ok(output) => {
                    let decoded = Frame::decode(&output)?;
                    if let Frame::Error { stage, message } = decoded {
                        // A model refusal is not evidence that another replica
                        // should retry a different computation.
                        return Err(ModernError::Invalid(format!("{stage}: {message}")));
                    }
                    self.journal
                        .push((input, *blake3::hash(&output).as_bytes()));
                    self.journal_bytes += cost;
                    return Ok(decoded);
                }
                Err(_) => self.session = None,
            }
        }
    }
}

/// Serve one fresh replica session. Returning drops all KV. Reconnection must
/// construct a new worker, never reuse the old session's partial state.
pub fn serve_session(worker: StageWorker, link: Box<dyn Link>) -> Result<(), ModernError> {
    serve_session_with(worker, link, &mut |_| true)
}

/// Fault/delay hook for deterministic churn tests. Returning false loses the
/// reply AFTER execution, before any byte is acknowledged to the relay.
pub fn serve_session_with(
    mut worker: StageWorker,
    mut link: Box<dyn Link>,
    before_reply: &mut dyn FnMut(usize) -> bool,
) -> Result<(), ModernError> {
    link.send(&stage_identity(worker.model()))
        .map_err(|e| ModernError::Io(format!("replica identity: {e}")))?;
    let mut frames = 0;
    while let Ok(bytes) = link.recv() {
        let frame = Frame::decode(&bytes)?;
        let shutdown = matches!(frame, Frame::Shutdown);
        let output = worker.process(frame).unwrap_or_else(|e| Frame::Error {
            stage: worker.name(),
            message: e.to_string(),
        });
        frames += 1;
        if !before_reply(frames) {
            return Ok(());
        }
        link.send(&output.encode())
            .map_err(|e| ModernError::Io(format!("replica reply: {e}")))?;
        if shutdown {
            break;
        }
    }
    Ok(())
}

/// A relay occupies one existing ring address. Each stage has its own relay,
/// allowing frames for other streams to overlap across stages as before.
pub fn serve_relay(
    mut stage: ReplicatedStage,
    listener: Box<dyn Listener>,
    transport: Arc<dyn Transport>,
    next: String,
) -> Result<(), ModernError> {
    let mut downstream = Downstream::new(transport, next);
    for bytes in incoming(listener) {
        let frame = Frame::decode(&bytes)?;
        let shutdown = matches!(frame, Frame::Shutdown);
        let output = stage.process(frame).unwrap_or_else(|e| Frame::Error {
            stage: "replicated stage".into(),
            message: e.to_string(),
        });
        downstream.send(&output.encode())?;
        if shutdown {
            break;
        }
    }
    Ok(())
}
