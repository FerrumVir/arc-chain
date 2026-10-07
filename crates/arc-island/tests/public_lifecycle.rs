//! External-crate regressions: no cfg(test) pins or private lifecycle access.
use arc_island::admission::{AdmissionPolicy, Load, Refusal, Request, admit};
use arc_island::device::{
    Consent, DeviceDescriptor, Evidence, Freshness, IslandFacts, LinkStats, Location, Measured,
    Provenance, RttMatrix,
};
use arc_island::form::{FormationPolicy, IslandPlan, form};
use arc_island::lifecycle::{GoldenSource, Island, QualifyError, State};
use arc_island::model::ModelSpec;
use arc_island::selftest::{GoldenExecutor, GoldenReference, SelfTestRun};

fn fixture(synthetic: bool) -> (Vec<DeviceDescriptor>, RttMatrix, Island) {
    let evidence = if synthetic {
        Evidence::synthetic(0)
    } else {
        Evidence::measured(0)
    };
    let devices: Vec<_> = (0..3)
        .map(|i| {
            let id = format!("d{i}");
            DeviceDescriptor {
                consent: Consent::grant(&id, &id, u64::MAX),
                owner: id.clone(),
                device_id: id,
                facts: IslandFacts {
                    memory_class_gb: Some(512),
                    unified_memory: true,
                    gpu_vram_class_gb: None,
                    thunderbolt5: Some(false),
                    download_mbps_class: Some(1000),
                },
                golden_qualified: true,
                measured: Measured {
                    usable_memory_bytes: Some(409_600_000_000),
                    bandwidth_mb_s: Some(456_000),
                    uplink_mbps: Some(1000),
                },
                site: None,
                location: Location {
                    continent: "NA".into(),
                    region: "US-East".into(),
                    zone: "US".into(),
                    metro: "NYC".into(),
                },
                availability_permille: 950,
                evidence,
                rdma: None,
            }
        })
        .collect();
    let mut rtt = RttMatrix::new();
    for a in 0..3 {
        for b in a + 1..3 {
            rtt.insert(
                a,
                b,
                LinkStats {
                    p50_us: 10_000,
                    p95_us: 12_500,
                    p99_us: 15_000,
                    loss_permille: 0,
                    samples: 200,
                    evidence,
                },
            );
        }
    }
    let mut policy = FormationPolicy::batch(8 * 4096);
    policy.allow_synthetic = synthetic;
    let plan = form(&devices, &rtt, &ModelSpec::kimi_k26_int4(), &policy, 0)
        .islands
        .remove(0);
    let island = Island::new(plan, &devices, Freshness::default(), 0);
    (devices, rtt, island)
}

struct ClaimsMeasured;
impl GoldenExecutor for ClaimsMeasured {
    fn provenance(&self) -> Provenance {
        Provenance::Measured
    }
    fn run(&mut self, _: &IslandPlan, _: &[DeviceDescriptor]) -> SelfTestRun {
        panic!("unpinned/synthetic Kimi must fail before execution")
    }
}

fn assert_closed(island: &mut Island, devices: &[DeviceDescriptor], rtt: &RttMatrix) {
    assert!(!island.is_serving());
    assert!(!island.admission_open());
    assert!(!island.health_check(0, devices, rtt));
    assert_eq!(
        admit(
            island,
            0,
            devices,
            rtt,
            &Load::default(),
            &Request {
                prompt_tokens: 1,
                max_new_tokens: 1,
                min_tok_s: 0.0
            },
            &AdmissionPolicy {
                max_prefill_queue_ms: 5000
            },
            |_| 100.0
        ),
        Err(Refusal::NotServing)
    );
}

#[test]
fn measured_and_synthetic_kimi_cannot_admit_without_qualification() {
    for synthetic in [false, true] {
        let (devices, rtt, mut island) = fixture(synthetic);
        assert!(GoldenReference::pinned(&island.plan().model).is_none());
        assert_eq!(
            island.qualify(0, &mut ClaimsMeasured, &devices, &rtt, GoldenSource::Pinned),
            Err(if synthetic {
                QualifyError::SyntheticInputs
            } else {
                QualifyError::NoPinnedGolden
            })
        );
        assert_closed(&mut island, &devices, &rtt);
        // A diagnostic snapshot is not executable. Importing its plan through
        // the only constructor discards any claimed Serving state.
        let mut snapshot = serde_json::to_value(&island).unwrap();
        snapshot["state"] = serde_json::json!("Serving");
        let plan: IslandPlan = serde_json::from_value(snapshot["plan"].clone()).unwrap();
        let mut restored = Island::new(plan, &devices, Freshness::default(), 0);
        assert_eq!(restored.state(), &State::Forming);
        assert_closed(&mut restored, &devices, &rtt);
        // Editing a detached plan cannot mutate the original or gain a pin.
        let mut edited = island.plan().clone();
        edited.provenance = Provenance::Measured;
        let mut reconstructed = Island::new(edited, &devices, Freshness::default(), 0);
        assert_closed(&mut reconstructed, &devices, &rtt);
    }
}

#[cfg(feature = "simulator")]
#[test]
fn simulation_health_is_explicit_and_never_opens_real_admission() {
    use arc_island::selftest::toy::ToyPipeline;
    let (devices, rtt, mut island) = fixture(true);
    let model = ModelSpec::kimi_k26_int4();
    let golden = ToyPipeline::golden(&model.identity, model.layers.len());
    assert_eq!(
        island.qualify(
            0,
            &mut ToyPipeline::default(),
            &devices,
            &rtt,
            GoldenSource::Simulation(&golden)
        ),
        Ok(State::Simulated)
    );
    assert!(island.simulation_health_check(0, &devices, &rtt));
    assert_closed(&mut island, &devices, &rtt);
    assert_eq!(island.state(), &State::Simulated);
}
