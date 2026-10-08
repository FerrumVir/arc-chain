//! Expert parallelism over placement-selected regional nodes. Integer i128
//! partial sums preserve the profile regardless of placement. Each contacted
//! device adds an RPC: measure regional latency before making speed claims.

use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex};

use super::transport::{Link, Listener, Transport};
use super::wire::{Reader, Writer};
use crate::modern::ModernError;
use crate::modern::mla::model::{ExpertPool, StageModel};

/// Which device holds which routed expert.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ExpertPlacement {
    pub devices: usize,
    /// This device's index.
    pub local: usize,
}

impl ExpertPlacement {
    pub fn device_of(&self, expert: usize) -> usize {
        expert % self.devices.max(1)
    }
}

fn encode_request(layer: usize, x: &[i64], experts: &[(usize, i64)]) -> Vec<u8> {
    let mut w = Writer::default();
    w.u32(layer as u32);
    w.acts(x);
    w.count(experts.len());
    for &(e, weight) in experts {
        w.u32(e as u32);
        w.i64(weight);
    }
    w.bytes
}

/// Remote experts reached over links, one per remote device.
pub struct RemoteExperts {
    placement: ExpertPlacement,
    owners: Option<Vec<usize>>,
    links: Vec<Option<Mutex<Box<dyn Link>>>>,
    /// Requests sent.
    pub calls: AtomicU64,
}

impl RemoteExperts {
    /// `devices[d]` is the address of device `d`'s expert server (ignored for
    /// the local device).
    pub fn connect(
        transport: &dyn Transport,
        placement: ExpertPlacement,
        devices: &[String],
    ) -> Result<Self, ModernError> {
        Self::connect_placed(transport, placement, devices, None)
    }

    /// Explicit expert-id -> device-id map, supplied by ENG-7's placement.
    /// The map must cover every routed expert; use `None` for legacy modulo.
    pub fn connect_placed(
        transport: &dyn Transport,
        placement: ExpertPlacement,
        devices: &[String],
        owners: Option<Vec<usize>>,
    ) -> Result<Self, ModernError> {
        if owners
            .as_ref()
            .is_some_and(|map| map.is_empty() || map.iter().any(|&d| d >= placement.devices))
        {
            return Err(ModernError::Invalid(
                "expert owner outside device list".into(),
            ));
        }
        if devices.len() != placement.devices || placement.local >= placement.devices {
            return Err(ModernError::Invalid(
                "expert placement and device list differ".into(),
            ));
        }
        let links = devices
            .iter()
            .enumerate()
            .map(|(d, address)| {
                if d == placement.local {
                    Ok(None)
                } else {
                    transport
                        .connect(address)
                        .map(|l| Some(Mutex::new(l)))
                        .map_err(|e| {
                            ModernError::Io(format!("expert device {d} at {address}: {e}"))
                        })
                }
            })
            .collect::<Result<_, _>>()?;
        Ok(Self {
            placement,
            owners,
            links,
            calls: AtomicU64::new(0),
        })
    }
}

impl RemoteExperts {
    fn owner(&self, expert: usize) -> usize {
        self.owners.as_ref().map_or_else(
            || self.placement.device_of(expert),
            |map| map.get(expert).copied().unwrap_or(usize::MAX),
        )
    }
}

impl ExpertPool for RemoteExperts {
    fn is_remote(&self, _layer: usize, expert: usize) -> bool {
        self.owner(expert) != self.placement.local
    }

    fn evaluate(
        &self,
        layer: usize,
        x: &[i64],
        experts: &[(usize, i64)],
    ) -> Result<Vec<i128>, ModernError> {
        if experts
            .iter()
            .any(|&(e, _)| self.owner(e) >= self.placement.devices)
        {
            return Err(ModernError::Invalid("expert missing from placement".into()));
        }
        let mut total = vec![0i128; x.len()];
        for (d, link) in self.links.iter().enumerate() {
            let mine: Vec<(usize, i64)> = experts
                .iter()
                .copied()
                .filter(|&(e, _)| self.owner(e) == d)
                .collect();
            if mine.is_empty() {
                continue;
            }
            let link = link
                .as_ref()
                .ok_or_else(|| ModernError::Invalid("a local expert was sent remote".into()))?;
            let mut link = link.lock().expect("expert link");
            self.calls.fetch_add(1, Ordering::Relaxed);
            link.send(&encode_request(layer, x, &mine))
                .map_err(|e| ModernError::Io(format!("expert device {d}: {e}")))?;
            let reply = link
                .recv()
                .map_err(|e| ModernError::Io(format!("expert device {d}: {e}")))?;
            let mut r = Reader::new(&reply);
            if r.u8()? != 0 {
                return Err(ModernError::Invalid(format!(
                    "expert device {d}: {}",
                    r.str()?
                )));
            }
            let n = r.count()?;
            if n != total.len() {
                return Err(ModernError::Invalid(format!(
                    "expert device {d}: partial width"
                )));
            }
            for acc in &mut total {
                let v = i128::from_le_bytes(r.take(16)?.try_into().expect("16"));
                *acc = acc
                    .checked_add(v)
                    .ok_or_else(|| ModernError::Domain("routed expert sum beyond i128".into()))?;
            }
            r.done()?;
        }
        Ok(total)
    }
}

fn answer(model: &StageModel, request: &[u8]) -> Result<Vec<i128>, ModernError> {
    let mut r = Reader::new(request);
    let layer = r.u32()? as usize;
    let x = r.acts()?;
    let n = r.count()?;
    let experts = (0..n)
        .map(|_| Ok((r.u32()? as usize, r.i64()?)))
        .collect::<Result<Vec<_>, ModernError>>()?;
    r.done()?;
    model.expert_partial(layer, &x, &experts)
}

/// Serve expert requests for `model`'s layers on every connection `listener`
/// accepts, until the listener fails.
pub fn serve_experts(model: Arc<StageModel>, mut listener: Box<dyn Listener>) {
    while let Ok(mut link) = listener.accept() {
        let model = model.clone();
        std::thread::spawn(move || {
            while let Ok(request) = link.recv() {
                let mut w = Writer::default();
                match answer(&model, &request) {
                    Ok(partial) => {
                        w.u8(0);
                        w.count(partial.len());
                        for v in partial {
                            w.bytes.extend_from_slice(&v.to_le_bytes());
                        }
                    }
                    Err(e) => {
                        w.u8(1);
                        w.str(&e.to_string());
                    }
                }
                if link.send(&w.bytes).is_err() {
                    return;
                }
            }
        });
    }
}
