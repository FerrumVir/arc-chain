from dataclasses import replace
import itertools
import math
import unittest

from arc_autocluster.planner import (
    Consent, Device, Island, Link, Model, Policy, State, form, from_proof_result,
    partition, project,
)

NOW = 100
GOLDEN = "a" * 64  # fake executor fixture, NOT a Kimi golden


def device(i, memory=300, speed=1):
    return Device(str(i), "owner", "metro", "region", "lan", memory, speed,
                  NOW, Consent("owner", str(i), True, True, NOW + 1000))


def model():
    # Four layers: 100 weights + 10% headroom + 2 bytes KV = 113 after ceil.
    return Model("test-model", (100,) * 4, (10.0,) * 4, (1,) * 4,
                 context=1, max_sequences=2)


def links(a, b):
    return Link(.2, .3, 10000, NOW)


class FormationTests(unittest.TestCase):
    def test_fitting_includes_headroom_and_kv_and_prefers_fewer(self):
        m = model()
        self.assertIsNone(form([device(0, 400)], m, links, NOW,
                               Policy(require_spare=False)))
        p = form([device(0, 500), device(1, 500), device(2, 300)], m, links, NOW)
        self.assertEqual(len(p.stages), 1)
        self.assertEqual(p.tier, "T0")
        self.assertIsNotNone(p.spare)

    def test_contiguous_complete_heterogeneous_partition(self):
        m = Model("heterogeneous", (100,) * 6, (10.,) * 6, (1,) * 6,
                  context=1, max_sequences=1, headroom=0)
        stages = partition([device(0, 600, 2), device(1, 600, 1)], m)
        self.assertEqual([(s.start, s.end) for s in stages], [(0, 4), (4, 6)])
        self.assertEqual(stages[0].compute_ms, stages[1].compute_ms)
        self.assertIsNone(partition([device(0, 1000), device(1, 100)], m))

    def test_partition_matches_exhaustive_minimax_with_unequal_layers(self):
        m = Model("unequal", (15, 30, 20, 40, 10, 25), (2., 5., 3., 8., 1., 4.),
                  (1,) * 6, context=1, max_sequences=1, headroom=.1)
        ds = [device(0, 90, 2), device(1, 65, .5), device(2, 120, 1)]
        scores = []
        for cuts in itertools.combinations(range(1, 6), 2):
            spans = list(zip((0,) + cuts, cuts + (6,)))
            if all(m.reserved(a, b) <= d.usable_bytes for d, (a, b) in zip(ds, spans)):
                scores.append(max(sum(m.layer_ms[a:b]) / d.layer_speed
                                  for d, (a, b) in zip(ds, spans)))
        result = partition(ds, m)
        self.assertAlmostEqual(max(s.compute_ms for s in result), min(scores))

    def test_consent_defaults_closed_owner_bound_expiry_and_compute_setting(self):
        good = device(0, 1000)
        for grant in (Consent("owner", "0"), Consent("imposter", "0", True, True, 200),
                      Consent("owner", "0", True, True, NOW),
                      Consent("owner", "0", False, True, 200),
                      Consent("owner", "different", True, True, 200)):
            self.assertIsNone(form([replace(good, consent=grant)], model(), links, NOW,
                                   Policy(require_spare=False)))

    def test_offline_leased_stale_future_and_synthetic_excluded(self):
        for changes in ({"online": False}, {"leased": True}, {"measured_at": -1000},
                        {"measured_at": NOW + 1}, {"evidence": "synthetic"}):
            self.assertIsNone(form([replace(device(0, 1000), **changes)], model(), links,
                                   NOW, Policy(require_spare=False)))

    def test_missing_slow_lossy_stale_links_cannot_be_inferred_from_geo(self):
        ds = [device(i) for i in range(3)]
        bad = [None, Link(61, 70, 50, NOW), Link(10, 20, 50, NOW, .1),
               Link(10, 20, 50, -1000), Link(10, 20, 50, NOW + 1),
               Link(10, 20, 50, NOW, evidence="synthetic")]
        for edge in bad:
            self.assertIsNone(form(ds, model(), lambda a, b: edge, NOW))

    def test_lan_threshold_and_tb5_does_not_invent_rdma(self):
        ds = [replace(device(i), thunderbolt5=True) for i in range(3)]
        self.assertIsNone(form(ds, model(), lambda a, b: Link(.4, .6, 10000, NOW),
                               NOW, scope="lan"))
        self.assertIsNone(form(ds, model(), lambda a, b: Link(.2, .3, 1000, NOW),
                               NOW, scope="lan"))
        self.assertEqual(form(ds, model(), links, NOW, scope="lan").tier, "T1b")

    def test_spare_covers_largest_stage_and_is_not_active(self):
        p = form([device(i) for i in range(3)], model(), links, NOW)
        self.assertGreaterEqual(p.spare.usable_bytes, max(s.reserved_bytes for s in p.stages))
        self.assertNotIn(p.spare.id, {s.device.id for s in p.stages})
        self.assertIsNone(form([device(0), device(1), device(2, 110)], model(), links, NOW))

    def test_duplicate_ids_and_invalid_numbers_rejected(self):
        with self.assertRaises(ValueError):
            form([device(0), device(0)], model(), links, NOW)
        for speed in (math.nan, math.inf, 0, -1):
            with self.assertRaises(ValueError):
                device(0, speed=speed)

    def test_route_requires_feedback_and_every_spare_link(self):
        ds = [device(i) for i in range(3)]
        self.assertIsNone(form(ds, model(), lambda a, b: None if b.id == "0" else links(a, b), NOW))
        self.assertIsNone(form(ds, model(), lambda a, b: None if a.id == "2" else links(a, b), NOW))

    def test_order_independent_and_scope_is_enforced(self):
        ds = [device(i) for i in range(4)]
        self.assertEqual(form(ds, model(), links, NOW).id,
                         form(list(reversed(ds)), model(), links, NOW).id)
        ds[0] = replace(ds[0], region="elsewhere")
        self.assertIsNone(form(ds, model(), links, NOW))

    def test_stage_cap(self):
        self.assertIsNone(form([device(i) for i in range(3)], model(), links, NOW,
                               Policy(max_stages=1)))

    def test_small_optional_spare_cannot_prevent_a_feasible_plan(self):
        p = form([device(0, 500), device(1, 100)], model(), links, NOW,
                 Policy(require_spare=False))
        self.assertEqual(len(p.stages), 1)
        self.assertIsNone(p.spare)

    def test_reserving_largest_device_can_fit_an_otherwise_unprotected_plan(self):
        p = form([device(0, 600), device(1, 350), device(2, 120)], model(), links, NOW)
        self.assertEqual(p.spare.id, "0")
        self.assertEqual(len(p.stages), 2)

    def test_probe_quantiles_and_loss(self):
        edge = Link.from_samples(range(1, 101), 50, NOW, 110)
        self.assertEqual((edge.p95_ms, edge.p99_ms), (95, 99))
        self.assertAlmostEqual(edge.loss, 10 / 110)
        for samples in ([], [math.nan], [-1]):
            with self.assertRaises(ValueError):
                Link.from_samples(samples, 50, NOW, 10)

    def test_actual_proof_kit_field_names_and_no_ram_vram_addition(self):
        result = {"schema": "arc.proof-result.v1", "island": {
            "memory_class_gb": 128, "unified_memory": True,
            "gpu_vram_class_gb": None, "thunderbolt5": True,
            "download_mbps_class": 500}}
        d = from_proof_result(result, id="x", owner="o", metro="m", region="r", lan="l",
                              usable_bytes=100, layer_speed=2, measured_at=NOW,
                              consent=Consent("o", "x"))
        self.assertEqual(d.usable_bytes, 100)
        self.assertEqual(d.memory_class_gb, 128)
        self.assertFalse(d.eligible(NOW, 120))
        with self.assertRaises(ValueError):
            from_proof_result({"schema": "unknown"})


class ProjectionTests(unittest.TestCase):
    def setUp(self):
        self.plan = form([device(i) for i in range(3)], model(), links, NOW)

    def test_ring_network_math_and_aggregate_ceiling(self):
        p = project(self.plan, links)
        expected = 2 * (.2 / 2 + 1) + (14336 + 8) * 8 / (10000 * 1000)
        self.assertAlmostEqual(p["network_ms"], expected)
        self.assertEqual(p["hop_count"], 2)
        self.assertAlmostEqual(p["single_answer_tok_s"], 1000 / (40 + expected))
        self.assertLessEqual(p["aggregate_tok_s"], 1000 / 20)
        self.assertAlmostEqual(p["tokens_per_day"], p["aggregate_tok_s"] * 86400)

    def test_speculation_includes_cost_and_rejection(self):
        p = project(self.plan, links, draft_depth=3, acceptance=.7)
        self.assertAlmostEqual(p["expected_committed_tokens"], 1 + .7 + .49 + .343)
        self.assertGreater(p["pass_latency_ms"], project(self.plan, links)["pass_latency_ms"])
        rejected = project(self.plan, links, draft_depth=3, acceptance=0)
        self.assertLess(rejected["single_answer_tok_s"], project(self.plan, links)["single_answer_tok_s"])
        self.assertEqual(project(self.plan, links, draft_depth=3, acceptance=1)["expected_committed_tokens"], 4)


class LifecycleTests(unittest.TestCase):
    def setUp(self):
        self.devices = {str(i): device(i) for i in range(3)}
        self.plan = form(list(self.devices.values()), model(), links, NOW,
                         Policy(require_spare=False))
        self.island = Island(self.plan, GOLDEN)

    def ready(self):
        self.assertTrue(self.island.health_check(self.devices, links, NOW))
        self.assertTrue(self.island.qualify(lambda p: GOLDEN, lambda p: True))

    def test_full_lifecycle_admission_budget_and_dissolution(self):
        self.assertFalse(self.island.admit("r", 1, self.devices, links, NOW))
        self.ready()
        self.assertFalse(self.island.admit("long", 2, self.devices, links, NOW))
        self.assertFalse(self.island.admit("slow", 1, self.devices, links, NOW, min_tok_s=100))
        self.assertFalse(self.island.admit("queue", 1, self.devices, links, NOW, prefill_queue_ms=2000))
        self.assertTrue(self.island.admit("a", 1, self.devices, links, NOW))
        self.assertFalse(self.island.admit("a", 1, self.devices, links, NOW))
        self.assertTrue(self.island.admit("b", 1, self.devices, links, NOW))
        self.assertFalse(self.island.admit("c", 1, self.devices, links, NOW))
        self.island.complete("a")
        self.island.complete("b")
        self.assertEqual(self.island.state, State.READY)
        self.island.dissolve()
        self.assertFalse(self.island.health_check(self.devices, links, NOW))

    def test_golden_mismatch_and_executor_error_close_admission(self):
        self.island.health_check(self.devices, links, NOW)
        self.assertFalse(self.island.qualify(lambda p: "b" * 64, lambda p: True))
        self.assertEqual(self.island.state, State.DISSOLVED)
        island = Island(self.plan, GOLDEN)
        island.health_check(self.devices, links, NOW)
        def failing_executor(p):
            raise RuntimeError("backend unavailable")
        with self.assertRaises(RuntimeError):
            island.qualify(failing_executor, lambda p: True)
        self.assertEqual(island.state, State.DISSOLVED)

    def test_current_owner_revocation_and_capacity_shrink_stop_serve(self):
        for changes in ({"consent": Consent("owner", "0")}, {"owner": "other"},
                        {"usable_bytes": 1}, {"online": False}):
            self.island = Island(self.plan, GOLDEN)
            self.ready()
            current = dict(self.devices)
            current["0"] = replace(current["0"], **changes)
            self.assertFalse(self.island.admit("r", 1, current, links, NOW))
            self.assertEqual(self.island.state, State.DISSOLVED)

    def test_stale_health_is_rechecked_on_admission(self):
        self.ready()
        self.assertFalse(self.island.admit("r", 1, self.devices, links, NOW + 121))

    def test_throttled_compute_uses_current_measurement_for_admission(self):
        self.ready()
        self.devices["0"] = replace(self.devices["0"], layer_speed=.01)
        self.assertFalse(self.island.admit("r", 1, self.devices, links, NOW, min_tok_s=10))

    def test_spare_promotion_restores_checkpoint_and_requalifies(self):
        self.ready()
        failed = self.plan.stages[0].device.id
        self.devices[failed] = replace(self.devices[failed], online=False)
        before = self.plan.id
        observed = []
        def recover(plan, checkpoint):
            observed.append(checkpoint)
            return checkpoint
        self.assertTrue(self.island.promote(failed, self.devices, links, NOW, "committed-kv-digest", recover))
        self.assertEqual(observed, ["committed-kv-digest"])
        self.assertNotEqual(before, self.island.plan.id)
        self.assertIsNone(self.island.plan.spare)
        self.assertFalse(self.island.admit("r", 1, self.devices, links, NOW))
        self.assertTrue(self.island.qualify(lambda p: GOLDEN, lambda p: False))
        self.assertTrue(self.island.admit("r", 1, self.devices, links, NOW))

    def test_bad_recovery_or_revoked_spare_dissolves(self):
        self.ready()
        self.assertFalse(self.island.promote("0", self.devices, links, NOW, "good", lambda p, d: "bad"))
        self.assertEqual(self.island.state, State.DISSOLVED)
        self.island = Island(self.plan, GOLDEN)
        self.ready()
        sid = self.plan.spare.id
        self.devices[sid] = replace(self.devices[sid], consent=Consent("owner", sid))
        self.assertFalse(self.island.promote("0", self.devices, links, NOW, "good", lambda p, d: d))

    def test_unwarmed_spare_and_redundancy_policy_fail_closed(self):
        strict = Island(replace(self.plan, policy=Policy()), GOLDEN)
        strict.health_check(self.devices, links, NOW)
        self.assertFalse(strict.qualify(lambda p: GOLDEN, lambda p: False))
        self.island = Island(replace(self.plan, policy=Policy()), GOLDEN)
        self.ready()
        self.assertTrue(self.island.promote("0", self.devices, links, NOW, "ok", lambda p, d: d))
        self.assertFalse(self.island.qualify(lambda p: GOLDEN, lambda p: False))

    def test_replenish_redundancy_before_reopening(self):
        self.island = Island(replace(self.plan, policy=Policy()), GOLDEN)
        self.ready()
        self.assertTrue(self.island.promote("0", self.devices, links, NOW, "ok", lambda p, d: d))
        self.devices["new"] = device("new")
        self.assertTrue(self.island.replenish_spare("new", self.devices, links, NOW))
        self.assertTrue(self.island.qualify(lambda p: GOLDEN, lambda p: True))
        self.assertTrue(self.island.admit("r", 1, self.devices, links, NOW))

    def test_synthetic_formation_can_never_serve(self):
        ds = [replace(d, evidence="synthetic") for d in self.devices.values()]
        p = form(ds, model(), links, NOW, Policy(allow_synthetic=True))
        island = Island(p, GOLDEN)
        self.assertFalse(island.health_check({d.id: d for d in ds}, links, NOW))


if __name__ == "__main__":
    unittest.main()
