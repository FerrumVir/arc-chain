"""prepare -> (fake engine runs) -> score -> compare, and the API budget guard.

No model, network or GPU: engine runs are written by hand in the shapes that
`arc-modern golden` and `reference hf` produce.
"""

from __future__ import annotations

import contextlib
import io
import json
import tempfile
import unittest
from pathlib import Path

from arc_quality import benchmarks, harness, report

HERE = Path(__file__).resolve().parents[1]


def quiet(argv: list[str]) -> int:
    with contextlib.redirect_stdout(io.StringIO()):
        return harness.main(argv)


def right_answer(item: dict) -> str:
    return json.dumps(item["gold"]["call"])


WRONG = '{"name": "none", "arguments": {}}'


class Pipeline(unittest.TestCase):
    def setUp(self):
        self.tmp = tempfile.TemporaryDirectory()
        self.dir = Path(self.tmp.name)
        self.items = self.dir / "items.jsonl"
        self.cases = self.dir / "cases.json"
        self.assertEqual(quiet([
            "prepare", "--profile", str(HERE / "models" / "smollm3-3b.json"),
            "--benchmarks", "toolcall=all", "--out-items", str(self.items), "--out-cases", str(self.cases),
        ]), 0)
        self.item_list = [json.loads(x) for x in self.items.read_text().splitlines()]

    def tearDown(self):
        self.tmp.cleanup()

    def write_run(self, name: str, texts: dict, schema="arc.modern-run.v1", tokens=None) -> Path:
        cases = []
        for item in self.item_list:
            case = {"id": item["id"], "text": texts[item["id"]], "prompt_tokens": [1, 2, 3]}
            if tokens is not None:
                case["tokens"] = tokens(item)
            cases.append(case)
        path = self.dir / name
        path.write_text(json.dumps({"schema": schema, "cases": cases}))
        return path

    def score(self, run: Path, label: str) -> Path:
        out = self.dir / f"{label}-scored.json"
        self.assertEqual(quiet(["score", "--items", str(self.items), "--run", str(run),
                                "--label", label, "--out", str(out)]), 0)
        return out

    def test_cases_are_engine_input(self):
        cases = json.loads(self.cases.read_text())
        self.assertEqual(cases["schema"], "arc.modern-cases.v1")
        first = cases["cases"][0]
        self.assertEqual(first["eos"], [128012])
        self.assertEqual(first["selection"], "argmax")
        self.assertEqual(first["today"], "06 October 2026")
        self.assertFalse(first["thinking"])
        self.assertEqual(len(cases["cases"]), 16)

    def test_shards_partition_the_items(self):
        seen = []
        for index in range(3):
            out = self.dir / f"s{index}.jsonl"
            quiet(["prepare", "--profile", str(HERE / "models" / "smollm3-3b.json"), "--benchmarks",
                   "toolcall=5", "--shard", f"{index}/3", "--out-items", str(out),
                   "--out-cases", str(self.dir / f"s{index}.json")])
            seen += [json.loads(x)["id"] for x in out.read_text().splitlines()]
        self.assertEqual(len(seen), 5)
        self.assertEqual(len(set(seen)), 5)

    def test_identical_engines_pass_smoke(self):
        texts = {item["id"]: right_answer(item) for item in self.item_list}
        arc = self.score(self.write_run("arc.json", texts, tokens=lambda it: [7, len(it["id"])]), "arc")
        ref = self.score(self.write_run("ref.json", texts, "arc.quality-run.v1",
                                        tokens=lambda it: [7, len(it["id"])]), "ref")
        out = self.dir / "report.json"
        md = self.dir / "report.md"
        self.assertEqual(quiet(["compare", "--items", str(self.items), "--arc", str(arc), "--reference", str(ref),
                                "--label", "test", "--out", str(out), "--summary-md", str(md)]), 0)
        doc = json.loads(out.read_text())
        group = doc["groups"]["toolcall"]
        self.assertEqual(group["n"], 16)
        self.assertEqual(group["arc_accuracy"], 1.0)
        self.assertEqual(group["token_level"]["identical_generations"], 16)
        self.assertEqual(group["answer_agreement"], 1.0)
        # 16 items cannot certify a 3-point margin: the interval is too wide.
        self.assertEqual(group["policy"]["verdict"], "INCONCLUSIVE")
        self.assertFalse(doc["overall"]["certifying"])
        self.assertIn("| toolcall | 16 |", md.read_text())

    def test_worse_engine_fails(self):
        good = {item["id"]: right_answer(item) for item in self.item_list}
        bad = dict.fromkeys(good, WRONG)
        arc = self.score(self.write_run("arc.json", bad), "arc")
        ref = self.score(self.write_run("ref.json", good), "ref")
        out = self.dir / "report.json"
        self.assertEqual(quiet(["compare", "--items", str(self.items), "--arc", str(arc), "--reference", str(ref),
                                "--out", str(out)]), 0)
        doc = json.loads(out.read_text())
        self.assertEqual(doc["groups"]["toolcall"]["policy"]["verdict"], "FAIL")
        self.assertEqual(doc["overall"]["verdict"], "FAIL")
        self.assertEqual(quiet(["compare", "--items", str(self.items), "--arc", str(arc), "--reference", str(ref),
                                "--out", str(out), "--enforce"]), 1)

    def test_noise_floor_and_logit_track(self):
        good = {item["id"]: right_answer(item) for item in self.item_list}
        arc = self.score(self.write_run("arc.json", good), "arc")
        ref = self.score(self.write_run("ref.json", good), "ref")
        ref_b = self.score(self.write_run("refb.json", good), "refb")
        ppl = self.dir / "quality-x.json"
        ppl.write_text(json.dumps({
            "schema": "arc.modern-quality.v1", "text": "t", "scored_tokens": 1022,
            "bf16_reference": {"ppl": 6.7756}, "integer_engine": {"ppl": 6.7906, "logits_digest": "ab"},
            "ppl_delta_percent": 0.221, "top1_agreement": 0.94,
        }))
        items = [json.loads(x) for x in self.items.read_text().splitlines()]
        policy = json.loads((HERE / "policy.json").read_text())
        doc = report.compare(items, [str(arc)], [str(ref)], policy, [str(ref_b)], [str(ppl)])
        self.assertEqual(doc["noise_floor"]["verdict"], "PASS")
        self.assertEqual(doc["logit_track"][0]["verdict"], "PASS")
        self.assertIn("logit track", report.markdown(doc))

    def test_missing_items_fail_compare(self):
        texts = {item["id"]: right_answer(item) for item in self.item_list}
        arc = self.score(self.write_run("arc.json", texts), "arc")
        partial = dict(list(texts.items())[:-1])
        run = self.dir / "ref.json"
        run.write_text(json.dumps({"cases": [{"id": k, "text": v} for k, v in partial.items()]}))
        ref_out = self.dir / "ref-scored.json"
        self.assertEqual(quiet(["score", "--items", str(self.items), "--run", str(run), "--label", "ref",
                                "--out", str(ref_out)]), 1)
        self.assertEqual(quiet(["compare", "--items", str(self.items), "--arc", str(arc), "--reference",
                                str(ref_out), "--out", str(self.dir / "r.json")]), 1)

    def test_api_budget_guard(self):
        base = ["reference", "openai", "--base-url", "http://127.0.0.1:9", "--model", "m",
                "--items", str(self.items), "--out", str(self.dir / "api.json")]
        with self.assertRaises(SystemExit) as no_prices:
            quiet(base)
        self.assertIn("price", str(no_prices.exception.code))
        with self.assertRaises(SystemExit) as over:
            quiet(base + ["--price-in-per-mtok", "1", "--price-out-per-mtok", "4"])
        self.assertIn("exceeds --budget-usd", str(over.exception.code))
        self.assertEqual(quiet(base + ["--price-in-per-mtok", "1", "--price-out-per-mtok", "4",
                                       "--budget-usd", "5", "--dry-run"]), 0)
        self.assertFalse((self.dir / "api.json").exists())

    def test_every_benchmark_has_a_cap(self):
        self.assertEqual(set(benchmarks.MAX_TOKENS), set(benchmarks.BENCHMARKS))


if __name__ == "__main__":
    unittest.main()
