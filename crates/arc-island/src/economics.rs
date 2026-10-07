//! Assumption-based operator cost, not an ARC tariff or a live price quote.
//!
//! Charges all allocated members and warm spares for the whole day, including
//! downtime. Output volume is discounted for demand utilisation and observed
//! *simulated* lease availability. Unallocated machines are outside this cost
//! boundary. Capital, coordination and metered egress are explicit inputs.

use serde::{Deserialize, Serialize};

/// Every field is an ASSUMED input; replace with operator measurements/quotes
/// before making any cost claim. No API or cloud price is fetched here.
#[derive(Debug, Clone, Copy, PartialEq, Serialize, Deserialize)]
pub struct CostAssumptions {
    pub electricity_usd_per_kwh: f64,
    pub active_watts: f64,
    pub spare_watts: f64,
    pub hardware_usd_per_allocated_node_hour: f64,
    pub coordination_usd_per_swarm_hour: f64,
    pub egress_usd_per_decimal_gb: f64,
    pub demand_utilisation: f64,
}

impl Default for CostAssumptions {
    fn default() -> Self {
        Self {
            electricity_usd_per_kwh: 0.15,
            active_watts: 120.0,
            spare_watts: 35.0,
            hardware_usd_per_allocated_node_hour: 0.02,
            coordination_usd_per_swarm_hour: 0.05,
            egress_usd_per_decimal_gb: 0.01,
            demand_utilisation: 0.7,
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Serialize, Deserialize)]
pub struct CostProjection {
    pub energy_usd_per_day: f64,
    pub hardware_usd_per_day: f64,
    pub coordination_usd_per_day: f64,
    pub egress_usd_per_day: f64,
    pub total_usd_per_day: f64,
    /// Throughput ceiling × demand utilisation × lease availability fraction.
    pub projected_tokens_per_day: f64,
    /// Undefined, NOT free, when projected output is zero.
    pub usd_per_million_tokens: Option<f64>,
    pub lease_available_fraction: f64,
}

/// Cost for one scenario, with either plain or speculative batched throughput.
/// Egress is the corresponding fully loaded bytes/day ceiling (including
/// verification positions); it receives the same output-volume discount.
#[allow(clippy::too_many_arguments)]
pub fn project_cost(
    a: CostAssumptions,
    members: usize,
    spares: usize,
    swarms: usize,
    tokens_per_day_ceiling: f64,
    egress_bytes_per_day_ceiling: f64,
    lease_available_fraction: f64,
) -> Result<CostProjection, &'static str> {
    let values = [
        a.electricity_usd_per_kwh,
        a.active_watts,
        a.spare_watts,
        a.hardware_usd_per_allocated_node_hour,
        a.coordination_usd_per_swarm_hour,
        a.egress_usd_per_decimal_gb,
        a.demand_utilisation,
        tokens_per_day_ceiling,
        egress_bytes_per_day_ceiling,
        lease_available_fraction,
    ];
    if values.iter().any(|x| !x.is_finite() || *x < 0.0)
        || a.demand_utilisation > 1.0
        || lease_available_fraction > 1.0
    {
        return Err(
            "cost inputs must be finite, nonnegative; utilisation/availability must be in 0..=1",
        );
    }
    let productive = a.demand_utilisation * lease_available_fraction;
    let tokens = tokens_per_day_ceiling * productive;
    let energy = (members as f64 * a.active_watts + spares as f64 * a.spare_watts) / 1000.0
        * 24.0
        * a.electricity_usd_per_kwh;
    let hardware = (members as f64 + spares as f64) * a.hardware_usd_per_allocated_node_hour * 24.0;
    let coordination = swarms as f64 * a.coordination_usd_per_swarm_hour * 24.0;
    let egress = egress_bytes_per_day_ceiling * productive / 1e9 * a.egress_usd_per_decimal_gb;
    let total = energy + hardware + coordination + egress;
    let per_million = if tokens > 0.0 {
        Some(total / tokens * 1e6)
    } else {
        None
    };
    if !total.is_finite() || per_million.is_some_and(|value| !value.is_finite()) {
        return Err("cost projection exceeds finite numeric range");
    }
    Ok(CostProjection {
        energy_usd_per_day: energy,
        hardware_usd_per_day: hardware,
        coordination_usd_per_day: coordination,
        egress_usd_per_day: egress,
        total_usd_per_day: total,
        projected_tokens_per_day: tokens,
        usd_per_million_tokens: per_million,
        lease_available_fraction,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn charges_spares_and_downtime_and_counts_egress_once() {
        let a = CostAssumptions::default();
        let p = project_cost(a, 60, 3, 1, 1e6, 1e9, 0.5).unwrap();
        assert!((p.projected_tokens_per_day - 350_000.0).abs() < 1e-9);
        assert!((p.energy_usd_per_day - 7.305 * 24.0 * 0.15).abs() < 1e-9);
        assert!((p.hardware_usd_per_day - 63.0 * 0.02 * 24.0).abs() < 1e-9);
        assert!((p.egress_usd_per_day - 0.0035).abs() < 1e-9);
        assert!((p.usd_per_million_tokens.unwrap() - p.total_usd_per_day / 0.35).abs() < 1e-9);
        let idle = project_cost(a, 60, 3, 1, 1e6, 1e9, 0.0).unwrap();
        assert!(idle.total_usd_per_day > 0.0);
        assert_eq!(idle.usd_per_million_tokens, None);
    }

    #[test]
    fn no_capacity_has_no_cost_per_token_and_bad_inputs_are_rejected() {
        let a = CostAssumptions::default();
        assert_eq!(
            project_cost(a, 0, 0, 0, 0.0, 0.0, 0.0)
                .unwrap()
                .usd_per_million_tokens,
            None
        );
        for bad in [f64::NAN, f64::INFINITY, -1.0, 1.1] {
            assert!(project_cost(a, 1, 1, 1, 1.0, 1.0, bad).is_err());
        }
        let huge = CostAssumptions {
            active_watts: f64::MAX,
            ..a
        };
        assert!(project_cost(huge, usize::MAX, 0, 1, 1.0, 0.0, 1.0).is_err());
    }
}
