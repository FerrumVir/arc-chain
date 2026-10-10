"""Answer extraction and grading (standard library only)."""

from __future__ import annotations

import unittest

from arc_quality import benchmarks


class Letters(unittest.TestCase):
    def test_answer_forms(self):
        for text, want in [
            ("Answer: C", "C"),
            ("The answer is (J).", "J"),
            ("**Answer: B**", "B"),
            ("D. 14.0 amp", "D"),
            ("(E)", "E"),
            ("answer：A", "A"),
        ]:
            self.assertEqual(benchmarks.extract_letter(text, 10), want, text)

    def test_letters_beyond_the_options_are_ignored(self):
        self.assertIsNone(benchmarks.extract_letter("Answer: K", 10))
        self.assertEqual(benchmarks.extract_letter("Answer: D", 4), "D")
        self.assertIsNone(benchmarks.extract_letter("Answer: E", 4))

    def test_no_letter(self):
        self.assertIsNone(benchmarks.extract_letter("none of these", 10))


class Numbers(unittest.TestCase):
    def test_answer_line_wins(self):
        text = "16 - 3 - 4 = 9 eggs, 9 * 2 = 18 dollars.\nAnswer: $18"
        self.assertEqual(benchmarks.extract_number(text), "18")

    def test_last_answer_line(self):
        self.assertEqual(benchmarks.extract_number("Answer: 3\nWait.\nAnswer: 1,250"), "1250")

    def test_fallback_last_number(self):
        self.assertEqual(benchmarks.extract_number("so the total is 42."), "42")

    def test_decimals_and_negatives(self):
        self.assertEqual(benchmarks.extract_number("Answer: -2.50"), "-5/2")
        self.assertIsNone(benchmarks.extract_number("no digits here"))

    def test_gsm8k_grading(self):
        item = benchmarks.gsm8k_item({"question": "q", "answer": "work\n#### 1,000"}, 7)
        self.assertEqual(item["id"], "gsm8k/0007")
        self.assertTrue(benchmarks.score(item, "Answer: 1000.00")["correct"])
        self.assertFalse(benchmarks.score(item, "Answer: 999")["correct"])
        self.assertFalse(benchmarks.score(item, "")["correct"])


class Code(unittest.TestCase):
    HUMANEVAL = {
        "task_id": "HumanEval/3",
        "prompt": "from typing import List\n\n\ndef add(xs: List[int]) -> int:\n    \"\"\"Sum.\"\"\"\n",
        "test": "def check(candidate):\n    assert candidate([1, 2, 3]) == 6\n",
        "entry_point": "add",
    }

    def test_extract_code(self):
        self.assertEqual(benchmarks.extract_code("x\n```python\na = 1\n```\ny"), "a = 1\n")
        self.assertEqual(benchmarks.extract_code("```\nb = 2\n```"), "b = 2\n")
        self.assertEqual(benchmarks.extract_code("```python\nc = 3\n"), "c = 3\n")
        self.assertEqual(benchmarks.extract_code("d = 4"), "d = 4")

    def test_exec_requires_permission(self):
        item = benchmarks.humaneval_item(self.HUMANEVAL)
        with self.assertRaises(PermissionError):
            benchmarks.score(item, "```python\ndef add(xs):\n    return sum(xs)\n```")

    def test_humaneval_full_function_and_body(self):
        item = benchmarks.humaneval_item(self.HUMANEVAL)
        self.assertEqual(item["id"], "humaneval/003")
        whole = "```python\ndef add(xs: List[int]) -> int:\n    return sum(xs)\n```"
        self.assertTrue(benchmarks.score(item, whole, allow_exec=True)["correct"])
        body = "```python\n    return sum(xs)\n```"
        self.assertTrue(benchmarks.score(item, body, allow_exec=True)["correct"])
        wrong = "```python\ndef add(xs):\n    return 0\n```"
        result = benchmarks.score(item, wrong, allow_exec=True)
        self.assertFalse(result["correct"])
        self.assertEqual(result["exec"]["status"], "failed")

    def test_timeout(self):
        from arc_quality import execsandbox

        result = execsandbox.run_program("while True:\n    pass\n", timeout=1.0)
        self.assertEqual(result["status"], "timeout")
        self.assertFalse(result["passed"])

    def test_mbpp(self):
        item = benchmarks.mbpp_item({
            "task_id": 11, "prompt": "Write f.", "test_imports": ["import math"],
            "test_list": ["assert f(4) == 2.0"],
        })
        self.assertTrue(benchmarks.score(item, "```python\ndef f(x):\n    return math.sqrt(x)\n```",
                                         allow_exec=True)["correct"])
        self.assertFalse(benchmarks.score(item, "```python\ndef f(x):\n    return x\n```",
                                          allow_exec=True)["correct"])


class ToolCalls(unittest.TestCase):
    def setUp(self):
        self.items = {item["source_id"]: item for item in benchmarks.toolcall_items()}

    def test_set_is_well_formed(self):
        self.assertEqual(len(self.items), 16)
        for item in self.items.values():
            self.assertIn(item["gold"]["call"]["name"], item["user"])

    def test_exact_call(self):
        item = self.items["currency-eur-usd"]
        text = 'Sure. {"name": "convert_currency", "arguments": {"amount": 250.0, "from": "EUR", "to": "usd"}}'
        self.assertTrue(benchmarks.score(item, text)["correct"])

    def test_string_number_and_argument_string(self):
        item = self.items["currency-eur-usd"]
        text = '{"name": "convert_currency", "arguments": "{\\"amount\\": \\"250\\", \\"from\\": \\"EUR\\", \\"to\\": \\"USD\\"}"}'
        self.assertTrue(benchmarks.score(item, text)["correct"])

    def test_wrong_tool_or_argument(self):
        item = self.items["weather-city"]
        self.assertFalse(benchmarks.score(item, '{"name": "get_time", "arguments": {"city": "Lisbon"}}')["correct"])
        self.assertFalse(benchmarks.score(
            item, '{"name": "get_weather", "arguments": {"city": "Lisbon", "unit": "fahrenheit"}}')["correct"])
        self.assertFalse(benchmarks.score(item, "I cannot do that.")["correct"])

    def test_answer_key_is_canonical(self):
        item = self.items["time-zone"]
        a = benchmarks.score(item, '{"name":"get_time","arguments":{"city":"Tokyo"}}')
        b = benchmarks.score(item, '{"arguments": {"city": "tokyo"}, "name": "get_time"}')
        self.assertEqual(a["answer_key"], b["answer_key"])


class Selection(unittest.TestCase):
    def test_rank_is_pinned(self):
        # Changing the salt or the id format redraws every subset; keep it fixed.
        self.assertEqual(benchmarks.selection_rank("gsm8k", "test-0001"), "ba9059bf3490276f0decd675d6b7b721088e07ae625e6f4ced9a98110ab61467")
        self.assertNotEqual(benchmarks.selection_rank("gsm8k", "a"), benchmarks.selection_rank("mbpp", "a"))


if __name__ == "__main__":
    unittest.main()
