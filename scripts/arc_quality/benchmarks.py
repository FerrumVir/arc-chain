"""Benchmark items: prompts, answer extraction and scoring.

Every benchmark is generation-based and greedy, so the same item runs on any
engine that takes a chat turn and returns text: ARC's integer engine, a local
BF16 model, or an OpenAI-compatible API. Prompts are zero-shot and short, so
absolute scores are NOT comparable with vendor-reported numbers (those use
other prompts and shot counts). What the harness measures is the paired
difference between two engines on identical inputs.

Item (one JSON line, schema arc.quality-items.v1):
  id          "<benchmark>/<source id>"
  benchmark   mmlu_pro | gsm8k | humaneval | mbpp | toolcall
  system      optional system message (null: the model's default)
  user        the user turn
  max_tokens  generation cap
  gold        benchmark-specific answer data
"""

from __future__ import annotations

import hashlib
import json
import re
from fractions import Fraction
from pathlib import Path

from . import execsandbox

DATA_DIR = Path(__file__).resolve().parent / "data"
LETTERS = "ABCDEFGHIJKLMNOP"
SELECTION_SALT = "arc-quality-v1"

MAX_TOKENS = {"mmlu_pro": 16, "gsm8k": 384, "humaneval": 384, "mbpp": 320, "toolcall": 96}
BENCHMARKS = tuple(MAX_TOKENS)
CODE_BENCHMARKS = ("humaneval", "mbpp")


def selection_rank(benchmark: str, source_id: str) -> str:
    """Order used to draw a fixed subset: SHA-256 of a salted id."""
    return hashlib.sha256(f"{SELECTION_SALT}:{benchmark}:{source_id}".encode()).hexdigest()


# --------------------------------------------------------------- prompts -----


def mmlu_pro_item(row: dict) -> dict:
    options = list(row["options"])
    lines = [f"{LETTERS[i]}. {option}" for i, option in enumerate(options)]
    user = (
        f"The following is a multiple-choice question about {row['category']}. "
        'Reply with the letter of the correct option only, in the form "Answer: X".\n\n'
        f"Question: {row['question'].strip()}\n\nOptions:\n" + "\n".join(lines)
    )
    return {
        "id": f"mmlu_pro/{row['question_id']}",
        "benchmark": "mmlu_pro",
        "source_id": str(row["question_id"]),
        "system": None,
        "user": user,
        "max_tokens": MAX_TOKENS["mmlu_pro"],
        "gold": {"letter": LETTERS[int(row["answer_index"])], "options": len(options), "category": row["category"]},
    }


def gsm8k_item(row: dict, index: int) -> dict:
    final = row["answer"].split("####")[-1].strip().replace(",", "")
    user = (
        f"{row['question'].strip()}\n\nSolve the problem step by step. "
        'End your reply with a final line of the form "Answer: <number>".'
    )
    return {
        "id": f"gsm8k/{index:04d}",
        "benchmark": "gsm8k",
        "source_id": f"test-{index:04d}",
        "system": None,
        "user": user,
        "max_tokens": MAX_TOKENS["gsm8k"],
        "gold": {"number": final},
    }


def humaneval_item(row: dict) -> dict:
    user = (
        "Complete the following Python function. Reply with the whole function, including "
        "the signature, in one ```python code block and nothing else.\n\n"
        f"```python\n{row['prompt']}```"
    )
    number = row["task_id"].split("/")[-1]
    return {
        "id": f"humaneval/{int(number):03d}",
        "benchmark": "humaneval",
        "source_id": row["task_id"],
        "system": None,
        "user": user,
        "max_tokens": MAX_TOKENS["humaneval"],
        "gold": {"prompt": row["prompt"], "test": row["test"], "entry_point": row["entry_point"]},
    }


def mbpp_item(row: dict) -> dict:
    tests = list(row["test_list"])
    user = (
        f"{row['prompt'].strip()}\n\nYour code must pass these tests:\n" + "\n".join(tests) +
        "\n\nReply with the Python code only, in one ```python code block."
    )
    return {
        "id": f"mbpp/{int(row['task_id']):03d}",
        "benchmark": "mbpp",
        "source_id": str(row["task_id"]),
        "system": None,
        "user": user,
        "max_tokens": MAX_TOKENS["mbpp"],
        "gold": {"tests": tests, "imports": list(row.get("test_imports") or [])},
    }


def toolcall_items() -> list[dict]:
    tools = json.loads((DATA_DIR / "toolcall_tools.json").read_text(encoding="utf-8"))
    items = []
    for line in (DATA_DIR / "toolcall_v1.jsonl").read_text(encoding="utf-8").split("\n"):
        if not line.strip():
            continue
        row = json.loads(line)
        offered = [{"name": name, **tools[name]} for name in row["tools"]]
        user = (
            "You can call exactly one of these tools:\n"
            + json.dumps(offered, indent=1, ensure_ascii=False)
            + f"\n\nRequest: {row['user']}\n\n"
            'Reply with only a JSON object of the form {"name": <tool name>, "arguments": {...}} and nothing else.'
        )
        items.append({
            "id": f"toolcall/{row['id']}",
            "benchmark": "toolcall",
            "source_id": row["id"],
            "system": None,
            "user": user,
            "max_tokens": MAX_TOKENS["toolcall"],
            "gold": {"call": row["call"]},
        })
    return items


# ------------------------------------------------------------ extraction -----


def extract_letter(text: str, options: int = 10) -> str | None:
    allowed = LETTERS[:options]
    for pattern in (
        rf"[Aa]nswer\s*(?:is)?\s*[:：]?\s*\**\(?([{allowed}])\)?(?![A-Za-z])",
        rf"^\s*\**\(?([{allowed}])\)?(?:[.):\s*]|$)",
        rf"(?<![A-Za-z])\(?([{allowed}])\)?(?![A-Za-z])",
    ):
        match = re.search(pattern, text, flags=re.MULTILINE)
        if match:
            return match.group(1)
    return None


_NUMBER = r"-?\$?\s*[\d,]*\.?\d+"


def _to_fraction(text: str) -> Fraction | None:
    cleaned = text.replace(",", "").replace("$", "").replace(" ", "").rstrip(".")
    try:
        return Fraction(cleaned)
    except (ValueError, ZeroDivisionError):
        return None


def extract_number(text: str) -> str | None:
    answers = re.findall(rf"[Aa]nswer\s*[:：]\s*\**\s*({_NUMBER})", text)
    candidates = answers or re.findall(_NUMBER, text)
    for candidate in reversed(candidates):
        value = _to_fraction(candidate)
        if value is not None:
            return str(value)
    return None


def extract_code(text: str) -> str:
    fenced = re.search(r"```(?:python|py|Python)?[ \t]*\n(.*?)```", text, flags=re.DOTALL)
    if fenced:
        return fenced.group(1)
    opened = re.search(r"```(?:python|py|Python)?[ \t]*\n(.*)$", text, flags=re.DOTALL)
    if opened:  # the generation hit max_tokens inside the block
        return opened.group(1)
    return text


def extract_json_call(text: str) -> dict | None:
    decoder = json.JSONDecoder()
    for match in re.finditer(r"\{", text):
        try:
            value, _ = decoder.raw_decode(text[match.start():])
        except json.JSONDecodeError:
            continue
        if isinstance(value, dict) and "name" in value:
            return value
    return None


def _normal(value):
    if isinstance(value, bool) or value is None:
        return value
    if isinstance(value, (int, float)):
        return str(Fraction(value).limit_denominator(10**9))
    if isinstance(value, str):
        number = _to_fraction(value.strip())
        if number is not None and re.fullmatch(r"\s*-?[\d,]*\.?\d+\s*", value):
            return str(number)
        return " ".join(value.split()).casefold()
    if isinstance(value, dict):
        return {str(k): _normal(v) for k, v in sorted(value.items())}
    if isinstance(value, list):
        return [_normal(v) for v in value]
    return str(value)


def normal_call(call: dict) -> dict:
    arguments = call.get("arguments", call.get("parameters", {}))
    if isinstance(arguments, str):
        try:
            arguments = json.loads(arguments)
        except json.JSONDecodeError:
            pass
    return {"name": _normal(call.get("name")), "arguments": _normal(arguments)}


# --------------------------------------------------------------- scoring -----


def _import_lines(prompt: str) -> str:
    return "\n".join(line for line in prompt.splitlines() if re.match(r"\s*(import|from)\s", line))


def humaneval_program(gold: dict, code: str) -> str:
    entry = gold["entry_point"]
    if re.search(rf"^\s*def\s+{re.escape(entry)}\s*\(", code, flags=re.MULTILINE):
        body = _import_lines(gold["prompt"]) + "\n\n" + code
    else:  # the model continued the function body
        body = gold["prompt"] + code
    return f"{body}\n\n{gold['test']}\n\ncheck({entry})\n"


def mbpp_program(gold: dict, code: str) -> str:
    return "\n".join(gold["imports"]) + "\n" + code + "\n\n" + "\n".join(gold["tests"]) + "\n"


def short_hash(text: str) -> str:
    return hashlib.sha256(text.encode("utf-8")).hexdigest()[:16]


def score(item: dict, text: str | None, allow_exec: bool = False) -> dict:
    """Return {answer, answer_key, correct[, exec]} for one generated text.

    `answer_key` is a short canonical form used to test whether two engines
    gave the same answer (for code: a hash of the extracted program).
    """
    benchmark = item["benchmark"]
    gold = item["gold"]
    text = text or ""
    if benchmark == "mmlu_pro":
        letter = extract_letter(text, gold.get("options", 10))
        return {"answer": letter, "answer_key": letter, "correct": letter == gold["letter"]}
    if benchmark == "gsm8k":
        number = extract_number(text)
        expected = _to_fraction(gold["number"])
        correct = number is not None and expected is not None and Fraction(number) == expected
        return {"answer": number, "answer_key": number, "correct": correct}
    if benchmark in CODE_BENCHMARKS:
        code = extract_code(text).strip("\n")
        result = {"answer": code[:2000], "answer_key": short_hash(code)}
        if not allow_exec:
            raise PermissionError(
                "code benchmarks execute model-written programs; pass --allow-exec "
                "(only on a disposable machine such as a CI runner)"
            )
        program = humaneval_program(gold, code) if benchmark == "humaneval" else mbpp_program(gold, code)
        outcome = execsandbox.run_program(program)
        result["exec"] = outcome
        result["correct"] = bool(outcome["passed"])
        return result
    if benchmark == "toolcall":
        call = extract_json_call(text)
        if call is None:
            return {"answer": None, "answer_key": None, "correct": False}
        got = normal_call(call)
        key = json.dumps(got, sort_keys=True, ensure_ascii=False)
        return {"answer": got, "answer_key": key, "correct": got == normal_call(gold["call"])}
    raise ValueError(f"unknown benchmark {benchmark}")
