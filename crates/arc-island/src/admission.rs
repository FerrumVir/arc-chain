//! Admission control on the island's ingress (research-6 §3.5). A request is
//! admitted only if the island is serving and healthy, its KV fits the
//! island's budget, the predicted per-stream speed with it admitted meets
//! the request's floor, and the prefill queue is short enough. Otherwise
//! the router sends it elsewhere.

use crate::lifecycle::Island;
use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub struct AdmissionPolicy {
    /// Interactive classes require a warm spare (research-6 §3.5 item 4).
    pub require_warm_spare: bool,
    pub max_prefill_queue_ms: u64,
}

/// What the island is carrying now.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize)]
pub struct Load {
    pub active_sequences: u32,
    pub kv_positions_in_use: u64,
    pub prefill_queue_ms: u64,
}

#[derive(Debug, Clone, Copy, PartialEq, Serialize, Deserialize)]
pub struct Request {
    pub prompt_tokens: u64,
    pub max_new_tokens: u64,
    /// The request's per-stream speed floor.
    pub min_tok_s: f64,
}

#[derive(Debug, Clone, Copy, PartialEq, Serialize, Deserialize)]
pub enum Refusal {
    NotServing,
    NoWarmSpare,
    KvBudget {
        need: u64,
        free: u64,
    },
    TooSlow {
        predicted_tok_s: f64,
        required_tok_s: f64,
    },
    PrefillQueue {
        queue_ms: u64,
    },
}

/// Decides one request. `per_stream_tok_s(n)` predicts per-stream speed with
/// `n` sequences in flight (for example from [`crate::perf`]).
pub fn admit(
    island: &Island,
    load: &Load,
    req: &Request,
    policy: &AdmissionPolicy,
    per_stream_tok_s: impl Fn(u32) -> f64,
) -> Result<(), Refusal> {
    if !island.is_serving() {
        return Err(Refusal::NotServing);
    }
    if policy.require_warm_spare && island.warm_spares() == 0 {
        return Err(Refusal::NoWarmSpare);
    }
    let need = req.prompt_tokens + req.max_new_tokens;
    let free = island
        .plan
        .kv_positions
        .saturating_sub(load.kv_positions_in_use);
    if need > free {
        return Err(Refusal::KvBudget { need, free });
    }
    let predicted = per_stream_tok_s(load.active_sequences + 1);
    if predicted < req.min_tok_s {
        return Err(Refusal::TooSlow {
            predicted_tok_s: predicted,
            required_tok_s: req.min_tok_s,
        });
    }
    if load.prefill_queue_ms > policy.max_prefill_queue_ms {
        return Err(Refusal::PrefillQueue {
            queue_ms: load.prefill_queue_ms,
        });
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::form::tests::{device, mesh};
    use crate::form::{FormationPolicy, form};
    use crate::model::ModelSpec;
    use crate::selftest::ToyPipeline;

    fn serving(spare: bool) -> Island {
        let model = ModelSpec::uniform("u", 12, 1_000_000_000, 100);
        let n = if spare { 4 } else { 3 };
        let devices: Vec<_> = (0..n)
            .map(|i| device(&format!("d{i}"), 8, false, None, "NYC"))
            .collect();
        let rtt = mesh(n, 5_000, &[]);
        let plan =
            form(&devices, &rtt, &model, &FormationPolicy::batch(8 * 4096)).islands[0].clone();
        let mut island = Island::new(plan, 0);
        island.qualify(
            0,
            &mut ToyPipeline::default(),
            &devices,
            &ToyPipeline::golden(12),
        );
        island
    }

    const POLICY: AdmissionPolicy = AdmissionPolicy {
        require_warm_spare: true,
        max_prefill_queue_ms: 5_000,
    };

    fn req(tokens: u64, min: f64) -> Request {
        Request {
            prompt_tokens: tokens,
            max_new_tokens: 512,
            min_tok_s: min,
        }
    }

    #[test]
    fn admits_within_every_limit() {
        let island = serving(true);
        let speed = |n: u32| 20.0 / f64::from(n);
        assert_eq!(
            admit(&island, &Load::default(), &req(1_000, 5.0), &POLICY, speed),
            Ok(())
        );
        let busy = Load {
            active_sequences: 3,
            ..Load::default()
        };
        assert!(matches!(
            admit(&island, &busy, &req(1_000, 5.0), &POLICY, speed),
            Ok(())
        ));
        let busier = Load {
            active_sequences: 4,
            ..Load::default()
        };
        assert!(matches!(
            admit(&island, &busier, &req(1_000, 5.0), &POLICY, speed),
            Err(Refusal::TooSlow { .. })
        ));
    }

    #[test]
    fn refuses_on_kv_health_and_queue() {
        let island = serving(true);
        let speed = |_: u32| 30.0;
        let full = Load {
            kv_positions_in_use: 8 * 4096 - 100,
            ..Load::default()
        };
        assert!(matches!(
            admit(&island, &full, &req(1_000, 1.0), &POLICY, speed),
            Err(Refusal::KvBudget { .. })
        ));
        let queued = Load {
            prefill_queue_ms: 9_000,
            ..Load::default()
        };
        assert!(matches!(
            admit(&island, &queued, &req(10, 1.0), &POLICY, speed),
            Err(Refusal::PrefillQueue { .. })
        ));
        let lonely = serving(false);
        assert_eq!(
            admit(&lonely, &Load::default(), &req(10, 1.0), &POLICY, speed),
            Err(Refusal::NoWarmSpare)
        );
        let batch = AdmissionPolicy {
            require_warm_spare: false,
            ..POLICY
        };
        assert_eq!(
            admit(&lonely, &Load::default(), &req(10, 1.0), &batch, speed),
            Ok(())
        );
        let mut gone = serving(true);
        gone.dissolve(1, crate::lifecycle::DissolveReason::Requested);
        assert_eq!(
            admit(&gone, &Load::default(), &req(10, 1.0), &POLICY, speed),
            Err(Refusal::NotServing)
        );
    }
}
