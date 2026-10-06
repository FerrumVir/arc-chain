"""Hugging Face comparisons for the modern-model (SmolLM3-3B) CI evidence.

Subcommands (all paths are explicit; nothing is downloaded here):

  corpus          build the tokenizer corpus, dating chat cases with today's date
  compare-tokens  compare `arc-modern tokenize` output with HF tokenizers and
                  transformers.apply_chat_template
  tokenize-text   HF token ids of a text file (input to both perplexity runs)
  bf16-ppl        BF16 reference perplexity with transformers/torch on CPU and
                  comparison with `arc-modern ppl`
  matrix-report   aggregate golden runs from every OS/kernel into one table

The integer engine never imports this file; it only produces evidence.
"""

from __future__ import annotations

import argparse
import datetime as dt
import json
import math
import sys
from pathlib import Path


def read_jsonl(path: Path) -> list[dict]:
    return [json.loads(line) for line in path.read_text(encoding="utf-8").splitlines() if line.strip()]


def today_string() -> str:
    return dt.datetime.now().strftime("%d %B %Y")


def cmd_corpus(args: argparse.Namespace) -> int:
    rows = []
    for path in args.inputs:
        for row in read_jsonl(Path(path)):
            row = dict(row)
            row["id"] = f"{Path(path).stem}:{row['id']}"
            if "chat" in row:
                chat = dict(row["chat"])
                chat["today"] = today_string()
                row["chat"] = chat
            rows.append(row)
    Path(args.out).write_text("".join(json.dumps(r, ensure_ascii=False) + "\n" for r in rows), encoding="utf-8")
    print(f"{len(rows)} corpus rows")
    return 0


def hf_render(tokenizer, chat: dict) -> str:
    messages = []
    if chat.get("system") is not None:
        messages.append({"role": "system", "content": chat["system"]})
    messages.append({"role": "user", "content": chat["user"]})
    return tokenizer.apply_chat_template(
        messages,
        tokenize=False,
        add_generation_prompt=True,
        enable_thinking=bool(chat.get("thinking", False)),
    )


def cmd_compare_tokens(args: argparse.Namespace) -> int:
    from tokenizers import Tokenizer
    from transformers import AutoTokenizer

    model_dir = Path(args.model_dir)
    fast = Tokenizer.from_file(str(model_dir / "tokenizer.json"))
    templated = AutoTokenizer.from_pretrained(str(model_dir))
    corpus = {row["id"]: row for row in read_jsonl(Path(args.corpus))}
    rust = {row["id"]: row for row in read_jsonl(Path(args.rust))}
    results = []
    failures = 0
    for row_id, row in corpus.items():
        got = rust.get(row_id)
        if got is None:
            results.append({"id": row_id, "ok": False, "error": "missing from Rust output"})
            failures += 1
            continue
        entry = {"id": row_id}
        if "chat" in row:
            # The date is part of the template; re-render if midnight passed.
            reference_text = hf_render(templated, row["chat"])
            if row["chat"]["today"] not in reference_text:
                entry["note"] = "date rolled over during the check; compared with today's rendering"
            entry["template_equal"] = reference_text == got["text"]
            text = reference_text
        else:
            text = row["text"]
            entry["template_equal"] = None
        reference_ids = fast.encode(text, add_special_tokens=False).ids
        entry["ids_equal"] = reference_ids == got["ids"]
        entry["tokens"] = len(reference_ids)
        entry["ok"] = entry["ids_equal"] and entry["template_equal"] is not False
        if not entry["ok"]:
            failures += 1
            entry["reference_ids"] = reference_ids
            entry["rust_ids"] = got["ids"]
            entry["reference_text"] = text
            entry["rust_text"] = got.get("text")
        results.append(entry)
    report = {
        "schema": "arc.modern-tokenizer-check.v1",
        "rows": len(results),
        "failures": failures,
        "chat_rows": sum(1 for r in corpus.values() if "chat" in r),
        "results": results,
    }
    Path(args.out).write_text(json.dumps(report, indent=1, ensure_ascii=False) + "\n", encoding="utf-8")
    print(f"tokenizer check: {len(results) - failures}/{len(results)} rows identical")
    for entry in results:
        if not entry["ok"]:
            print(json.dumps(entry, ensure_ascii=False)[:2000])
    return 1 if failures else 0


def cmd_tokenize_text(args: argparse.Namespace) -> int:
    from tokenizers import Tokenizer

    fast = Tokenizer.from_file(str(Path(args.model_dir) / "tokenizer.json"))
    text = Path(args.text).read_text(encoding="utf-8")
    ids = fast.encode(text, add_special_tokens=False).ids[: args.max_tokens]
    Path(args.out).write_text(json.dumps({"source": args.text, "tokens": ids}) + "\n", encoding="utf-8")
    print(f"{len(ids)} tokens")
    return 0


def cmd_bf16_ppl(args: argparse.Namespace) -> int:
    import torch
    from transformers import AutoModelForCausalLM

    torch.manual_seed(0)
    torch.set_num_threads(max(1, args.threads))
    tokens = json.loads(Path(args.tokens).read_text())["tokens"]
    model = AutoModelForCausalLM.from_pretrained(args.model_dir, torch_dtype=torch.bfloat16)
    model.eval()
    nll_sum = 0.0
    scored = 0
    argmax = []
    with torch.no_grad():
        for start in range(0, len(tokens), args.window):
            chunk = tokens[start : start + args.window]
            if len(chunk) < 2:
                continue
            ids = torch.tensor([chunk], dtype=torch.long)
            logits = model(ids).logits[0, :-1].float()
            logp = torch.log_softmax(logits, dim=-1)
            target = torch.tensor(chunk[1:], dtype=torch.long)
            nll_sum += float(-logp.gather(1, target[:, None]).sum())
            scored += len(chunk) - 1
            argmax.extend(int(i) for i in logits.argmax(dim=-1))
    bf16 = {"scored_tokens": scored, "nll_sum": nll_sum, "ppl": math.exp(nll_sum / scored)}
    rust = json.loads(Path(args.rust).read_text())
    if rust["scored_tokens"] != scored or rust["window"] != args.window:
        print(f"scored tokens differ: rust {rust['scored_tokens']} vs bf16 {scored}")
        return 1
    agree = sum(1 for a, b in zip(argmax, rust["argmax"]) if a == b)
    report = {
        "schema": "arc.modern-quality.v1",
        "text": args.text_label,
        "window": args.window,
        "scored_tokens": scored,
        "bf16_reference": {
            "implementation": "transformers AutoModelForCausalLM, torch_dtype=bfloat16, CPU",
            "torch": torch.__version__,
            **bf16,
        },
        "integer_engine": {
            "profile": rust["profile"],
            "kernel": rust["kernel"],
            "ppl": rust["ppl"],
            "nll_sum": rust["nll_sum"],
            "logits_digest": rust["logits_digest"],
            "tok_s": rust["tok_s"],
        },
        "ppl_delta_percent": 100.0 * (rust["ppl"] / bf16["ppl"] - 1.0),
        "top1_agreement": agree / scored,
    }
    Path(args.out).write_text(json.dumps(report, indent=1) + "\n")
    print(json.dumps(report, indent=1))
    return 0


def cmd_matrix_report(args: argparse.Namespace) -> int:
    runs = []
    for path in sorted(Path(args.runs).rglob("*.json")):
        try:
            doc = json.loads(path.read_text())
        except (OSError, json.JSONDecodeError):
            continue
        if doc.get("schema") == "arc.modern-run.v1":
            runs.append((path, doc))
    if not runs:
        print("no runs found")
        return 1
    golden = None
    if args.golden:
        golden = json.loads(Path(args.golden).read_text())
    rows = []
    for path, doc in runs:
        timing = doc.get("timing", {})
        census = doc.get("census", {})
        rows.append({
            "file": path.name,
            "os": doc.get("platform", {}).get("os"),
            "arch": doc.get("platform", {}).get("arch"),
            "kernel": doc.get("kernel"),
            "threads": doc.get("threads"),
            "package_sha256": doc["package"]["sha256"],
            "matrix_digest": doc["matrix_digest"],
            "decode_tok_s": timing.get("decode_tok_s"),
            "prefill_tok_s": timing.get("prefill_tok_s"),
            "simd_accepted": census.get("accepted"),
            "simd_refused": sum(v for k, v in census.items() if k.startswith("refused")),
        })
    digests = {r["matrix_digest"] for r in rows}
    packages = {r["package_sha256"] for r in rows}
    agree = len(digests) == 1 and len(packages) == 1
    golden_digest = golden.get("matrix_digest") if golden else None
    golden_ok = golden_digest is None or (agree and golden_digest in digests)
    summary = {
        "schema": "arc.modern-matrix-report.v1",
        "label": args.label,
        "runs": rows,
        "all_package_hashes_equal": len(packages) == 1,
        "all_matrix_digests_equal": len(digests) == 1,
        "reference_matrix_digest": golden_digest,
        "reference_matches": golden_ok if golden else None,
    }
    Path(args.out).write_text(json.dumps(summary, indent=1) + "\n")
    lines = [
        f"### {args.label}",
        "",
        "| OS | arch | kernel | threads | package sha256 | matrix digest | decode tok/s (CI runner) | SIMD accepted/refused |",
        "|---|---|---|---|---|---|---|---|",
    ]
    for r in rows:
        tok_s = r["decode_tok_s"]
        lines.append(
            f"| {r['os']} | {r['arch']} | {r['kernel']} | {r['threads']} | `{r['package_sha256'][:16]}…` | "
            f"`{r['matrix_digest'][:16]}…` | {tok_s:.2f} | {r['simd_accepted']}/{r['simd_refused']} |"
            if isinstance(tok_s, (int, float))
            else f"| {r['os']} | {r['arch']} | {r['kernel']} | {r['threads']} | `{r['package_sha256'][:16]}…` | "
            f"`{r['matrix_digest'][:16]}…` | n/a | {r['simd_accepted']}/{r['simd_refused']} |"
        )
    lines.append("")
    lines.append(f"All package hashes equal: **{len(packages) == 1}**; all matrix digests equal: **{len(digests) == 1}**.")
    if golden:
        lines.append(f"Independent Python reference digest `{golden_digest}` matches: **{golden_ok}**.")
    Path(args.summary_md).write_text("\n".join(lines) + "\n")
    print("\n".join(lines))
    return 0 if agree and golden_ok else 1


def main(argv: list[str]) -> int:
    parser = argparse.ArgumentParser(description=__doc__, formatter_class=argparse.RawDescriptionHelpFormatter)
    sub = parser.add_subparsers(dest="command", required=True)

    p = sub.add_parser("corpus")
    p.add_argument("--out", required=True)
    p.add_argument("inputs", nargs="+")
    p.set_defaults(func=cmd_corpus)

    p = sub.add_parser("compare-tokens")
    p.add_argument("--model-dir", required=True)
    p.add_argument("--corpus", required=True)
    p.add_argument("--rust", required=True)
    p.add_argument("--out", required=True)
    p.set_defaults(func=cmd_compare_tokens)

    p = sub.add_parser("tokenize-text")
    p.add_argument("--model-dir", required=True)
    p.add_argument("--text", required=True)
    p.add_argument("--max-tokens", type=int, default=1024)
    p.add_argument("--out", required=True)
    p.set_defaults(func=cmd_tokenize_text)

    p = sub.add_parser("bf16-ppl")
    p.add_argument("--model-dir", required=True)
    p.add_argument("--tokens", required=True)
    p.add_argument("--rust", required=True)
    p.add_argument("--window", type=int, default=512)
    p.add_argument("--threads", type=int, default=4)
    p.add_argument("--text-label", default="")
    p.add_argument("--out", required=True)
    p.set_defaults(func=cmd_bf16_ppl)

    p = sub.add_parser("matrix-report")
    p.add_argument("--runs", required=True)
    p.add_argument("--golden")
    p.add_argument("--label", default="hash matrix")
    p.add_argument("--out", required=True)
    p.add_argument("--summary-md", required=True)
    p.set_defaults(func=cmd_matrix_report)

    args = parser.parse_args(argv)
    return args.func(args)


if __name__ == "__main__":
    sys.exit(main(sys.argv[1:]))
