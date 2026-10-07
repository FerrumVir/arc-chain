"""Reference engines: the model ARC's integer engine is compared against.

hf        Hugging Face transformers on the published weights (BF16 by
          default), greedy, on CPU or GPU. Free; used for small models in CI.
          With --prompt-ids-from it reads the exact prompt token ids from an
          ARC run, so both engines see identical inputs.
openai    any OpenAI-compatible /chat/completions endpoint (a provider API or
          a self-hosted vLLM/SGLang server running the official weights),
          temperature 0. Refuses to send anything unless prices are given
          explicitly and the worst-case cost fits --budget-usd (default 0).

Both write arc.quality-run.v1: {"schema", "engine", "cases": [{"id",
"prompt_tokens"?, "tokens"?, "text"?, "seconds", ...}]}.
"""

from __future__ import annotations

import json
import math
import os
import time
import urllib.error
import urllib.request
from pathlib import Path

RUN_SCHEMA = "arc.quality-run.v1"


def read_items(paths: list[str]) -> list[dict]:
    items = []
    for path in paths:
        for line in Path(path).read_text(encoding="utf-8").split("\n"):
            if line.strip():
                items.append(json.loads(line))
    return items


def messages_for(item: dict) -> list[dict]:
    messages = []
    if item.get("system") is not None:
        messages.append({"role": "system", "content": item["system"]})
    messages.append({"role": "user", "content": item["user"]})
    return messages


def load_run(path: str) -> dict:
    """Any run whose cases carry an id plus tokens and/or text.

    Accepts arc.modern-run.v1 (`arc-modern golden`), the arc-mla run schema
    (same case fields) and arc.quality-run.v1.
    """
    doc = json.loads(Path(path).read_text(encoding="utf-8"))
    cases = doc.get("cases")
    if not isinstance(cases, list) or any("id" not in case for case in cases):
        raise ValueError(f"{path}: not a run file (needs cases[] with ids)")
    return doc


# ---------------------------------------------------------------- hf -------


def run_hf(args) -> int:
    import torch
    from transformers import AutoModelForCausalLM, AutoTokenizer

    items = read_items(args.items)
    if args.limit:
        items = items[: args.limit]
    prompt_ids = {}
    if args.prompt_ids_from:
        for path in args.prompt_ids_from:
            for case in load_run(path)["cases"]:
                prompt_ids[case["id"]] = list(case["prompt_tokens"])
    eos = [int(x) for x in args.eos.split(",") if x.strip()]
    torch.manual_seed(0)
    if args.threads:
        torch.set_num_threads(args.threads)
    dtype = {"bfloat16": torch.bfloat16, "float32": torch.float32, "float16": torch.float16}[args.dtype]
    start = time.time()
    model = AutoModelForCausalLM.from_pretrained(args.model_dir, torch_dtype=dtype)
    model.eval()
    tokenizer = None
    load_seconds = time.time() - start
    cases = []
    for item in items:
        if item["id"] in prompt_ids:
            ids = prompt_ids[item["id"]]
            source = "arc-run"
        elif args.prompt_ids_from and not args.allow_template:
            raise SystemExit(f"{item['id']}: no prompt ids in the ARC run (pass --allow-template to render)")
        else:
            if tokenizer is None:
                tokenizer = AutoTokenizer.from_pretrained(args.model_dir)
            ids = tokenizer.apply_chat_template(
                messages_for(item), add_generation_prompt=True, tokenize=True, enable_thinking=False
            )
            source = "hf-chat-template"
        begin = time.time()
        with torch.no_grad():
            input_ids = torch.tensor([ids], dtype=torch.long)
            output = model.generate(
                input_ids,
                attention_mask=torch.ones_like(input_ids),
                max_new_tokens=int(item["max_tokens"]),
                do_sample=False,
                num_beams=1,
                temperature=None,
                top_p=None,
                top_k=None,
                eos_token_id=eos,
                pad_token_id=eos[0],
            )
        tokens = [int(t) for t in output[0, len(ids):]]
        for position, token in enumerate(tokens):
            if token in eos:
                tokens = tokens[: position + 1]
                break
        seconds = time.time() - begin
        cases.append({"id": item["id"], "prompt_tokens": ids, "prompt_source": source,
                      "tokens": tokens, "seconds": seconds})
        print(f"{item['id']}: {len(tokens)} tokens in {seconds:.1f} s", flush=True)
    total = sum(c["seconds"] for c in cases)
    generated = sum(len(c["tokens"]) for c in cases)
    run = {
        "schema": RUN_SCHEMA,
        "engine": {
            "kind": "hf-transformers",
            "label": args.label,
            "model_dir": str(args.model_dir),
            "dtype": args.dtype,
            "torch": torch.__version__,
            "transformers": __import__("transformers").__version__,
            "threads": torch.get_num_threads(),
            "decoding": "greedy (do_sample=False, num_beams=1)",
        },
        "timing": {"load_seconds": load_seconds, "seconds": total, "generated_tokens": generated},
        "cases": cases,
    }
    Path(args.out).write_text(json.dumps(run, indent=1) + "\n", encoding="utf-8")
    print(f"{len(cases)} cases, {generated} tokens, {total:.0f} s")
    return 0


# ------------------------------------------------------------ openai -------


def estimate(items: list[dict], chars_per_token: float) -> dict:
    """Worst case: every reply uses max_tokens; input from a chars/token ratio
    (3.0 overestimates English for modern BPE vocabularies)."""
    input_tokens = sum(
        math.ceil(sum(len(m["content"]) for m in messages_for(item)) / chars_per_token) + 16 for item in items
    )
    output_tokens = sum(int(item["max_tokens"]) for item in items)
    return {"items": len(items), "input_tokens": input_tokens, "output_tokens_max": output_tokens}


def cost_usd(input_tokens: int, output_tokens: int, price_in: float, price_out: float) -> float:
    return input_tokens * price_in / 1e6 + output_tokens * price_out / 1e6


def post_json(url: str, body: dict, headers: dict, timeout: float) -> dict:
    data = json.dumps(body).encode("utf-8")
    last = None
    for attempt in range(1, 7):
        request = urllib.request.Request(url, data=data, headers=headers, method="POST")
        try:
            with urllib.request.urlopen(request, timeout=timeout) as response:
                return json.loads(response.read().decode("utf-8"))
        except urllib.error.HTTPError as error:
            last = f"HTTP {error.code}: {error.read()[:300]!r}"
            if error.code not in (408, 409, 429) and error.code < 500:
                break
        except (urllib.error.URLError, TimeoutError) as error:
            last = str(error)
        time.sleep(min(60, 2**attempt))
    raise RuntimeError(f"{url}: {last}")


def run_openai(args) -> int:
    items = read_items(args.items)
    if args.limit:
        items = items[: args.limit]
    volume = estimate(items, args.chars_per_token)
    print(json.dumps({"estimate": volume}, indent=1))
    if args.price_in_per_mtok is None or args.price_out_per_mtok is None:
        raise SystemExit(
            "refusing: pass --price-in-per-mtok and --price-out-per-mtok explicitly "
            "(0 and 0 for a self-hosted server you already run)"
        )
    worst = cost_usd(volume["input_tokens"], volume["output_tokens_max"],
                     args.price_in_per_mtok, args.price_out_per_mtok)
    print(f"worst-case cost: ${worst:.2f} (budget ${args.budget_usd:.2f})")
    if worst > args.budget_usd:
        raise SystemExit("refusing: the worst-case cost exceeds --budget-usd; no request was sent")
    if args.dry_run:
        return 0
    if not args.out:
        raise SystemExit("--out is required unless --dry-run")
    key = os.environ.get(args.api_key_env, "") if args.api_key_env else ""
    headers = {"Content-Type": "application/json", "User-Agent": "arc-quality/1"}
    if key:
        headers["Authorization"] = f"Bearer {key}"
    extra = json.loads(args.extra_body) if args.extra_body else {}
    url = args.base_url.rstrip("/") + "/chat/completions"
    spent_in = spent_out = 0
    cases = []
    for item in items:
        body = {
            "model": args.model,
            "messages": messages_for(item),
            "max_tokens": int(item["max_tokens"]),
            "temperature": 0,
            "top_p": 1,
            "stream": False,
            **extra,
        }
        begin = time.time()
        reply = post_json(url, body, headers, args.timeout)
        choice = reply["choices"][0]
        usage = reply.get("usage") or {}
        spent_in += int(usage.get("prompt_tokens", 0))
        spent_out += int(usage.get("completion_tokens", 0))
        cases.append({
            "id": item["id"],
            "text": choice["message"].get("content") or "",
            "finish_reason": choice.get("finish_reason"),
            "usage": usage,
            "model": reply.get("model"),
            "system_fingerprint": reply.get("system_fingerprint"),
            "seconds": time.time() - begin,
        })
        spent = cost_usd(spent_in, spent_out, args.price_in_per_mtok, args.price_out_per_mtok)
        print(f"{item['id']}: {usage.get('completion_tokens')} tokens, spent ${spent:.4f}", flush=True)
        if spent > args.budget_usd:
            print("stopping: the budget is used up")
            break
    run = {
        "schema": RUN_SCHEMA,
        "engine": {
            "kind": "openai-compatible",
            "label": args.label,
            "base_url": args.base_url,
            "model": args.model,
            "decoding": "temperature 0, top_p 1",
            "extra_body": extra,
        },
        "usage": {"prompt_tokens": spent_in, "completion_tokens": spent_out,
                  "cost_usd": cost_usd(spent_in, spent_out, args.price_in_per_mtok, args.price_out_per_mtok)},
        "cases": cases,
    }
    Path(args.out).write_text(json.dumps(run, indent=1, ensure_ascii=False) + "\n", encoding="utf-8")
    return 0


def add_parsers(sub) -> None:
    p = sub.add_parser("hf", help="Hugging Face transformers, greedy")
    p.add_argument("--model-dir", required=True)
    p.add_argument("--items", action="append", required=True)
    p.add_argument("--prompt-ids-from", action="append", help="ARC run(s) whose prompt_tokens to reuse")
    p.add_argument("--allow-template", action="store_true",
                   help="render items missing from --prompt-ids-from with the HF chat template")
    p.add_argument("--eos", required=True, help="comma-separated end-of-turn token ids")
    p.add_argument("--dtype", choices=["bfloat16", "float32", "float16"], default="bfloat16")
    p.add_argument("--threads", type=int, default=0)
    p.add_argument("--limit", type=int, default=0)
    p.add_argument("--label", default="BF16 reference (transformers)")
    p.add_argument("--out", required=True)
    p.set_defaults(func=run_hf)

    p = sub.add_parser("openai", help="OpenAI-compatible chat API, temperature 0, budget-guarded")
    p.add_argument("--base-url", required=True)
    p.add_argument("--model", required=True)
    p.add_argument("--items", action="append", required=True)
    p.add_argument("--api-key-env", default="", help="name of the environment variable holding the key")
    p.add_argument("--budget-usd", type=float, default=0.0)
    p.add_argument("--price-in-per-mtok", type=float)
    p.add_argument("--price-out-per-mtok", type=float)
    p.add_argument("--chars-per-token", type=float, default=3.0)
    p.add_argument("--extra-body", default="", help="JSON merged into each request (e.g. thinking off)")
    p.add_argument("--timeout", type=float, default=300.0)
    p.add_argument("--limit", type=int, default=0)
    p.add_argument("--dry-run", action="store_true", help="print the volume and cost estimate only")
    p.add_argument("--label", default="API reference")
    p.add_argument("--out", default="")
    p.set_defaults(func=run_openai)
