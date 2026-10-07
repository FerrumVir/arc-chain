//! Admission control on the island's ingress (research-6 §3.5). Every
//! request first re-runs the island's health check: identity, consent,
//! measured and fresh evidence, memory and links of every member and spare.
//! A request is then admitted only if the island is serving (qualified
//! against a pinned golden on measured inputs) with its full spare count,
//! its KV fits the island's budget, the predicted per-stream speed with it
//! admitted meets the request's floor, and the prefill queue is short
//! enough. Otherwise the router sends it elsewhere.

use crate::device::{DeviceDescriptor, RttSource};
use crate::lifecycle::Island;
use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub struct AdmissionPolicy {
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
    /// Not qualified for real serving (forming, recovering, simulated or
    /// dissolved), possibly after this request's health check.
    NotServing,
    /// Serving but below the research-6 spare count: paused until
    /// replenished.
    SparesBelowPolicy {
        missing: usize,
    },
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

/// Decides one request at `now_ms`. `per_stream_tok_s(n)` predicts
/// per-stream speed with `n` sequences in flight (for example from
/// [`crate::perf`]).
#[allow(clippy::too_many_arguments)]
pub fn admit(
    island: &mut Island,
    now_ms: u64,
    devices: &[DeviceDescriptor],
    rtt: &dyn RttSource,
    load: &Load,
    req: &Request,
    policy: &AdmissionPolicy,
    per_stream_tok_s: impl Fn(u32) -> f64,
) -> Result<(), Refusal> {
    island.health_check(now_ms, devices, rtt);
    if !island.is_serving() {
        return Err(Refusal::NotServing);
    }
    if island.spare_shortfall() > 0 {
        return Err(Refusal::SparesBelowPolicy {
            missing: island.spare_shortfall(),
        });
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
    use crate::device::{Evidence, Freshness, RttMatrix};
    use crate::form::tests::{device, mesh};
    use crate::form::{FormationPolicy, form};
    use crate::lifecycle::{GoldenSource, State};
    use crate::model::ModelSpec;
    use crate::selftest::test_pins::MeasuredToy;
    use crate::selftest::toy::ToyPipeline;

    const KV: u64 = 8 * 4096;

    fn island(extra: usize, executor_measured: bool) -> (Vec<DeviceDescriptor>, RttMatrix, Island) {
        let model = ModelSpec::uniform("u", 12, 1_000_000_000, 100);
        let n = 4 + extra;
        let devices: Vec<_> = (0..n)
            .map(|i| device(&format!("d{i}"), 8, false, None, "NYC"))
            .collect();
        let rtt = mesh(n, 5_000, &[]);
        let plan = form(&devices, &rtt, &model, &FormationPolicy::batch(KV), 0).islands[0].clone();
        let mut island = Island::new(plan, &devices, Freshness::default(), 0);
        if executor_measured {
            let r = island.qualify(
                0,
                &mut MeasuredToy::default(),
                &devices,
                &rtt,
                GoldenSource::Pinned,
            );
            assert_eq!(r, Ok(State::Serving));
        } else {
            let golden = ToyPipeline::golden(&model.identity, 12);
            let r = island.qualify(
                0,
                &mut ToyPipeline::default(),
                &devices,
                &rtt,
                GoldenSource::Simulation(&golden),
            );
            assert_eq!(r, Ok(State::Simulated));
        }
        (devices, rtt, island)
    }

    const POLICY: AdmissionPolicy = AdmissionPolicy {
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
        let (devices, rtt, mut isl) = island(0, true);
        let speed = |n: u32| 20.0 / f64::from(n);
        assert_eq!(
            admit(
                &mut isl,
                1,
                &devices,
                &rtt,
                &Load::default(),
                &req(1_000, 5.0),
                &POLICY,
                speed
            ),
            Ok(())
        );
        let busy = Load {
            active_sequences: 3,
            ..Load::default()
        };
        assert_eq!(
            admit(
                &mut isl,
                2,
                &devices,
                &rtt,
                &busy,
                &req(1_000, 5.0),
                &POLICY,
                speed
            ),
            Ok(())
        );
        let busier = Load {
            active_sequences: 4,
            ..Load::default()
        };
        assert!(matches!(
            admit(
                &mut isl,
                3,
                &devices,
                &rtt,
                &busier,
                &req(1_000, 5.0),
                &POLICY,
                speed
            ),
            Err(Refusal::TooSlow { .. })
        ));
    }

    #[test]
    fn refuses_on_kv_and_queue() {
        let (devices, rtt, mut isl) = island(0, true);
        let speed = |_: u32| 30.0;
        let full = Load {
            kv_positions_in_use: KV - 100,
            ..Load::default()
        };
        assert!(matches!(
            admit(
                &mut isl,
                1,
                &devices,
                &rtt,
                &full,
                &req(1_000, 1.0),
                &POLICY,
                speed
            ),
            Err(Refusal::KvBudget { .. })
        ));
        let queued = Load {
            prefill_queue_ms: 9_000,
            ..Load::default()
        };
        assert!(matches!(
            admit(
                &mut isl,
                1,
                &devices,
                &rtt,
                &queued,
                &req(10, 1.0),
                &POLICY,
                speed
            ),
            Err(Refusal::PrefillQueue { .. })
        ));
    }

    #[test]
    fn simulated_islands_never_admit() {
        let (devices, rtt, mut isl) = island(0, false);
        assert_eq!(
            admit(
                &mut isl,
                1,
                &devices,
                &rtt,
                &Load::default(),
                &req(10, 1.0),
                &POLICY,
                |_| 30.0
            ),
            Err(Refusal::NotServing)
        );
    }

    #[test]
    fn every_admission_rechecks_health_and_spares() {
        let speed = |_: u32| 30.0;
        // A spare's owner withdraws: paused until replenished.
        let (mut devices, rtt, mut isl) = island(1, true);
        let spare = isl.plan.spares[0].device;
        devices[spare].consent.withdraw();
        assert_eq!(
            admit(
                &mut isl,
                1,
                &devices,
                &rtt,
                &Load::default(),
                &req(10, 1.0),
                &POLICY,
                speed
            ),
            Err(Refusal::SparesBelowPolicy { missing: 1 })
        );
        assert!(isl.is_serving(), "members are not dissolved");
        let extra = devices.len() - 1;
        isl.replenish_spare(2, extra, &devices, &rtt).unwrap();
        assert_eq!(
            admit(
                &mut isl,
                3,
                &devices,
                &rtt,
                &Load::default(),
                &req(10, 1.0),
                &POLICY,
                speed
            ),
            Ok(())
        );
        // A member's evidence goes stale: the spare takes over and the island
        // must recover and re-qualify before admitting again.
        let m = isl.plan.members[0];
        devices[m].evidence = Evidence::synthetic(0);
        assert_eq!(
            admit(
                &mut isl,
                4,
                &devices,
                &rtt,
                &Load::default(),
                &req(10, 1.0),
                &POLICY,
                speed
            ),
            Err(Refusal::NotServing)
        );
        assert_eq!(isl.state, State::Recovering { stage: 0 });
        // Expired links dissolve the island.
        let (devices, rtt, mut isl) = island(0, true);
        let late = Freshness::default().link_ttl_ms + 1;
        assert_eq!(
            admit(
                &mut isl,
                late,
                &devices,
                &rtt,
                &Load::default(),
                &req(10, 1.0),
                &POLICY,
                speed
            ),
            Err(Refusal::NotServing)
        );
        assert!(isl.is_dissolved());
    }
}
