"""Fast tests for the operational checks. Synthetic facts; no nodes."""

import copy
import os
import sys
import unittest

HERE = os.path.dirname(os.path.abspath(__file__))
sys.path.insert(0, os.path.dirname(os.path.dirname(HERE)))

from arc_ops import check  # noqa: E402

NOW = 1_800_000_000.0


def healthy(n_nodes=4, height=1000):
    nodes = {}
    for i in range(n_nodes):
        nodes[f"n{i}"] = {
            "health": {"height": height - i, "peers": n_nodes - 1, "dag_bootstrapping": False},
            "finality": {"finality_lag": 1},
            "diag": {"state_native_pending": 2},
            "latest_block": {"timestamp": (NOW - 1) * 1000},
            "common_block": {"hash": "aa", "state_root": "bb"},
        }
    receipt = {"observed_status": "Finalized", "reserved_max_payment": 100,
               "settlement_credits": [{"amount": 90}, {"amount": 4}, {"amount": 3}, {"amount": 3}],
               "admission_transaction": {"block_height": 900}}
    return {
        "collected_t": NOW,
        "nodes": nodes,
        "common_height": height - 8,
        "native": {"from": 400, "to": 997,
                   "txs": [{"type": "NativeInferenceRequest", "success": True, "request_id": "r1"},
                           {"type": "NativeInferenceFinalize", "success": True, "request_id": "r1"}],
                   "receipts": {"r1": receipt},
                   "second_receipts": {"r1": copy.deepcopy(receipt)}},
    }


def judge(c):
    th = dict(check.DEFAULTS)
    return check.evaluate(c, th)


class Evaluate(unittest.TestCase):
    def test_a_healthy_set_passes(self):
        v = judge(healthy())
        self.assertEqual(v["status"], "PASS", v)
        self.assertEqual(v["facts"]["native_window"]["finalizes"], 1)

    def test_disagreement_at_the_common_height_fails(self):
        c = healthy()
        c["nodes"]["n2"]["common_block"] = {"hash": "zz", "state_root": "bb"}
        v = judge(c)
        self.assertEqual(v["status"], "FAIL")
        self.assertTrue(any("DISAGREEMENT" in f for f in v["fail"]))

    def test_a_stale_chain_fails(self):
        c = healthy()
        for n in c["nodes"].values():
            n["latest_block"]["timestamp"] = (NOW - 600) * 1000
        self.assertEqual(judge(c)["status"], "FAIL")

    def test_finality_lag_fails(self):
        c = healthy()
        c["nodes"]["n1"]["finality"]["finality_lag"] = 500
        self.assertEqual(judge(c)["status"], "FAIL")

    def test_a_partitioned_node_fails_and_a_missing_peer_warns(self):
        c = healthy()
        c["nodes"]["n3"]["health"]["peers"] = 0
        self.assertEqual(judge(c)["status"], "FAIL")
        c = healthy()
        c["nodes"]["n3"]["health"]["peers"] = 2
        self.assertEqual(judge(c)["status"], "WARN")

    def test_too_few_answers_is_incomplete_not_pass(self):
        c = healthy()
        for k in ("n1", "n2"):
            c["nodes"][k]["health"] = None
        v = judge(c)
        self.assertEqual(v["status"], "INCOMPLETE")
        self.assertEqual(check.EXIT[v["status"]], 2)

    def test_a_failed_native_transaction_fails(self):
        c = healthy()
        c["native"]["txs"][1]["success"] = False
        self.assertEqual(judge(c)["status"], "FAIL")

    def test_credits_that_do_not_reconcile_fail(self):
        c = healthy()
        c["native"]["receipts"]["r1"]["settlement_credits"] = [{"amount": 50}]
        v = judge(c)
        self.assertEqual(v["status"], "FAIL")
        self.assertTrue(any("credits" in f for f in v["fail"]))

    def test_two_nodes_disagreeing_about_a_settlement_fails(self):
        c = healthy()
        c["native"]["second_receipts"]["r1"]["settlement_credits"] = [{"amount": 100}]
        self.assertEqual(judge(c)["status"], "FAIL")

    def test_a_long_pending_request_and_refunds_warn(self):
        c = healthy()
        c["native"]["receipts"]["r1"] = {"observed_status": "Pending",
                                         "admission_transaction": {"block_height": 1}}
        c["native"]["to"] = 3_000  # pending for 2,999 blocks
        self.assertEqual(judge(c)["status"], "WARN")
        c = healthy()
        c["native"]["txs"].append({"type": "NativeInferenceRefund", "success": True,
                                   "request_id": "r1"})
        self.assertEqual(judge(c)["status"], "WARN", "1 of 2 settled refunded")

    def test_bootstrapping_warns(self):
        c = healthy()
        c["nodes"]["n0"]["health"]["dag_bootstrapping"] = True
        self.assertEqual(judge(c)["status"], "WARN")


class Collect(unittest.TestCase):
    def test_collection_reads_every_node_and_the_settlement_window(self):
        calls = []

        def fetch(node, path):
            calls.append((node, path))
            if path == "/health":
                return {"height": 110, "peers": 1}
            if path == "/finality/latest":
                return {"finality_lag": 0}
            if path == "/consensus/diagnostics":
                return {"state_native_pending": 0}
            if path == "/block/latest":
                return {"header": {"timestamp": NOW * 1000}}
            if path.startswith("/blocks?from="):
                start = int(path.split("from=")[1].split("&")[0])
                if start > 110:
                    return {"blocks": []}
                return {"blocks": [{"height": h, "tx_count": 1 if h == 50 else 0}
                                   for h in range(start, 111)]}
            if path == "/block/50/txs":
                return {"transactions": [{"hash": "t1"}]}
            if path == "/tx/t1/full":
                return {"tx_type": "NativeInferenceFinalize", "success": True,
                        "body": {"request_id": "r9"}}
            if path.startswith("/native-inference/receipt/"):
                return {"observed_status": "Finalized", "reserved_max_payment": 100,
                        "settlement_credits": [{"amount": 100}]}
            if path.startswith("/block/"):
                return {"hash": "h", "header": {"state_root": "s"}}
            return None

        c = check.collect(["a", "b"], fetch, window=100)
        self.assertEqual(c["common_height"], 105)
        self.assertEqual([t["request_id"] for t in c["native"]["txs"]], ["r9"])
        self.assertIn("r9", c["native"]["second_receipts"], "a second node's view is read")
        v = check.evaluate(c, dict(check.DEFAULTS))
        self.assertEqual(v["status"], "PASS", v)


if __name__ == "__main__":
    unittest.main(verbosity=2)
