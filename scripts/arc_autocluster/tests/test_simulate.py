import json
import unittest

from arc_autocluster.simulate import HARDWARE, REGIONS, assumptions, inventory, simulate


class SimulatorTests(unittest.TestCase):
    def test_research_priors_and_exact_inventory_size(self):
        self.assertEqual(sum(r[1] for r in REGIONS), 100)
        self.assertEqual(sum(h[1] for h in HARDWARE), 100)
        for n in (100, 130, 1000, 10000):
            ds, counts = inventory(n, 7, .7)
            self.assertEqual(len(ds), n)
            self.assertEqual(len({d.id for d in ds}), n)
            self.assertEqual(sum(counts.values()), n)
            self.assertTrue(all(d.evidence == "synthetic" for d in ds))

    def test_deterministic_disjoint_capacity_valid_plans(self):
        first = simulate(1000)
        self.assertEqual(first, simulate(1000))
        self.assertGreater(first["regional_swarms"], 0)
        allocated = set()
        for row in first["swarms"]:
            ids = [s["device"] for s in row["stages"]] + [row["spare"]]
            self.assertEqual(len(ids), len(set(ids)))
            self.assertFalse(allocated.intersection(ids))
            allocated.update(ids)
            cursor = 0
            for s in row["stages"]:
                self.assertEqual(s["layers"][0], cursor)
                cursor = s["layers"][1]
                self.assertLessEqual(s["reserved_bytes"], s["usable_bytes"])
                self.assertLessEqual(s["reserved_bytes"], row["spare_usable_bytes"])
            self.assertEqual(cursor, 61)
            self.assertLessEqual(len(row["stages"]), 30)
        self.assertEqual(first["eligible_nodes"], len(allocated) + first["unallocated_eligible_nodes"])
        self.assertAlmostEqual(first["aggregate_tok_s"], sum(
            r["without_speculation"]["aggregate_tok_s"] for r in first["swarms"]))
        json.dumps(first, allow_nan=False)

    def test_zero_availability_no_fabricated_capacity(self):
        scenario = simulate(130, availability=0)
        self.assertEqual(scenario["eligible_nodes"], 0)
        self.assertEqual(scenario["aggregate_tok_s"], 0)
        self.assertEqual(scenario["swarms"], [])

    def test_assumptions_are_explicit_and_json_serializable(self):
        value = assumptions()
        self.assertIn("SYNTHETIC", value["classification"])
        self.assertIn("ENG-6", value["classification"])
        json.dumps(value, allow_nan=False)


if __name__ == "__main__":
    unittest.main()
