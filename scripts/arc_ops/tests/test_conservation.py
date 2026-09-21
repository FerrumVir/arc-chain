"""The conservation audit, over synthetic replicas."""

import os
import sys
import unittest

HERE = os.path.dirname(os.path.abspath(__file__))
sys.path.insert(0, os.path.dirname(os.path.dirname(HERE)))

from arc_ops import conservation  # noqa: E402


def replicas(balances_by_node, pending_by_node, height=100):
    def fetch(node, path):
        if path == "/consensus/diagnostics":
            return {"state_native_pending": pending_by_node[node]}
        if path == "/health":
            return {"height": height}
        if path.startswith("/account/"):
            return {"balance": balances_by_node[node][path.split("/")[-1]]}
        return None
    return fetch


class Audit(unittest.TestCase):
    ACCOUNTS = ["a", "b", "c"]

    def run_audit(self, balances, pending, reservation=100):
        fetch = replicas(balances, pending)
        return conservation.audit(list(balances), self.ACCOUNTS, 1_000, reservation, fetch)

    def test_value_moved_between_accounts_is_conserved(self):
        state = {"a": 900, "b": 1_060, "c": 1_040}
        r = self.run_audit({"n0": state, "n1": dict(state)}, {"n0": 0, "n1": 0})
        self.assertEqual(r["status"], "PASS")
        self.assertTrue(r["identical_where_same_height"])

    def test_value_held_in_escrow_by_pending_requests_is_accounted(self):
        state = {"a": 800, "b": 1_000, "c": 1_000}  # 200 sits in two escrows
        r = self.run_audit({"n0": state}, {"n0": 2})
        self.assertEqual(r["status"], "PASS")

    def test_value_created_or_destroyed_fails(self):
        state = {"a": 901, "b": 1_060, "c": 1_040}  # one unit from nowhere
        r = self.run_audit({"n0": state}, {"n0": 0})
        self.assertEqual(r["status"], "FAIL")

    def test_replicas_with_different_balances_are_not_identical(self):
        r = self.run_audit({"n0": {"a": 900, "b": 1_100, "c": 1_000},
                            "n1": {"a": 1_000, "b": 1_000, "c": 1_000}}, {"n0": 0, "n1": 0})
        self.assertFalse(r["identical_where_same_height"])


if __name__ == "__main__":
    unittest.main(verbosity=2)
