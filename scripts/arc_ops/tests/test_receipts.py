"""The receipt reconciler, over synthetic replicas."""

import json
import os
import sys
import tempfile
import unittest

HERE = os.path.dirname(os.path.abspath(__file__))
sys.path.insert(0, os.path.dirname(os.path.dirname(HERE)))

from arc_ops import receipts  # noqa: E402

REQUEST = "20" * 32
REQUESTER = "cc" * 32

FINALIZED = {
    "request_id": REQUEST,
    "observed_status": "Finalized",
    "execution_price": 10,
    "reserved_max_payment": 100,
    "output_hash": "87" * 32,
    "terminal_transaction": {"block_height": 21},
    "settlement_credits": [
        {"payee": "aa" * 32, "amount": 6},
        {"payee": "bb" * 32, "amount": 4},
        {"payee": REQUESTER, "amount": 90},
    ],
}


def replicas(by_node):
    return lambda node, path: by_node.get(node)


class Reconcile(unittest.TestCase):
    def test_replicas_that_agree_and_add_up_pass(self):
        r = receipts.reconcile(["n0", "n1"], REQUEST, replicas({"n0": FINALIZED, "n1": dict(FINALIZED)}))
        self.assertEqual(r["status"], "PASS", r["problems"])
        self.assertEqual(r["answered"], 2)
        self.assertEqual(r["observed_status"], "Finalized")

    def test_a_different_settlement_on_one_replica_fails(self):
        other = dict(FINALIZED, settlement_credits=[{"payee": "aa" * 32, "amount": 100}])
        r = receipts.reconcile(["n0", "n1"], REQUEST, replicas({"n0": FINALIZED, "n1": other}))
        self.assertEqual(r["status"], "FAIL")
        self.assertIn("replicas record different settlements", r["problems"])

    def test_a_replica_without_the_newer_fields_is_not_a_disagreement(self):
        upgraded = dict(FINALIZED, requester=REQUESTER, expires_at=40, output_hex="0d00")
        r = receipts.reconcile(["n0", "n1"], REQUEST, replicas({"n0": upgraded, "n1": FINALIZED}))
        self.assertEqual(r["status"], "PASS", r["problems"])
        different = dict(upgraded, output_hex="ff00")
        r = receipts.reconcile(["n0", "n1"], REQUEST, replicas({"n0": upgraded, "n1": different}))
        self.assertEqual(r["status"], "FAIL")

    def test_credits_that_do_not_add_up_or_shortchange_the_requester_fail(self):
        short = dict(FINALIZED, settlement_credits=[{"payee": "aa" * 32, "amount": 99}])
        r = receipts.reconcile(["n0"], REQUEST, replicas({"n0": short}))
        self.assertEqual(r["status"], "FAIL")
        skim = dict(FINALIZED, requester=REQUESTER, settlement_credits=[
            {"payee": "aa" * 32, "amount": 30}, {"payee": REQUESTER, "amount": 70}])
        r = receipts.reconcile(["n0"], REQUEST, replicas({"n0": skim}))
        self.assertTrue(any("less than reservation - price" in p for p in r["problems"]))

    def test_a_refund_must_return_the_whole_reservation(self):
        refunded = {"request_id": REQUEST, "observed_status": "Refunded", "requester": REQUESTER,
                    "execution_price": 10, "reserved_max_payment": 100,
                    "settlement_credits": [{"payee": REQUESTER, "amount": 100}]}
        self.assertEqual(receipts.reconcile(["n0"], REQUEST, replicas({"n0": refunded}))["status"], "PASS")
        partial = dict(refunded, settlement_credits=[{"payee": REQUESTER, "amount": 90},
                                                     {"payee": "aa" * 32, "amount": 10}])
        self.assertEqual(receipts.reconcile(["n0"], REQUEST, replicas({"n0": partial}))["status"], "FAIL")

    def test_too_few_answers_is_incomplete_and_pending_is_not_judged(self):
        r = receipts.reconcile(["n0", "n1", "n2"], REQUEST, replicas({"n0": FINALIZED}))
        self.assertEqual(r["status"], "INCOMPLETE")
        pending = {"request_id": REQUEST, "observed_status": "Pending", "settlement_credits": []}
        self.assertEqual(receipts.reconcile(["n0"], REQUEST, replicas({"n0": pending}))["status"], "PASS")

    def test_request_ids_come_from_a_desktop_journal(self):
        path = os.path.join(tempfile.mkdtemp(), "native-requests.json")
        with open(path, "w") as fh:
            json.dump({"records": [
                {"kind": "request", "request_id": "01" * 32},
                {"kind": "refund", "request_id": "01" * 32},
                {"kind": "request", "request_id": "02" * 32},
            ]}, fh)
        self.assertEqual(receipts.request_ids_from_journal(path), ["01" * 32, "02" * 32])


if __name__ == "__main__":
    unittest.main()
