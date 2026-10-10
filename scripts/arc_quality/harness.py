"""ARC quality harness: is ARC's integer engine as good as the reference model?

Run from scripts/ as `python -m arc_quality <command>`:

  fetch-data  download the pinned benchmark files (data/datasets.json)
  prepare     draw a fixed subset, write items (JSONL) and engine cases
              (arc.modern-cases.v1, the input of `arc-modern golden` and
              `arc-mla golden`)
  reference   run the reference model: `hf` (transformers) or `openai`
              (any OpenAI-compatible endpoint; budget-guarded)
  score       extract answers from a run and grade them
  compare     paired comparison of ARC vs the reference under the tolerance
              policy (policy.json); writes the report JSON and Markdown

The engines never import this package; it only measures. See
docs/quality/kimi-quality-harness.md.
"""

from __future__ import annotations

import argparse
import hashlib
import json
import sys
import time
import urllib.request
from pathlib import Path

from . import benchmarks, provenance, reference, report

HERE = Path(__file__).resolve().parent
SCORED_SCHEMA = "arc.quality-scored.v1"
ITEMS_SCHEMA = "arc.quality-items.v1"


def _sha256(path: Path) -> str:
    digest = hashlib.sha256()
    with path.open("rb") as handle:
        for block in iter(lambda: handle.read(1 << 20), b""):
            digest.update(block)
    return digest.hexdigest()


def local_name(spec: dict) -> str:
    return spec["repo"].replace("/", "__") + "__" + spec["path"].replace("/", "__")


def cmd_fetch_data(args) -> int:
    pins = json.loads((HERE / "data" / "datasets.json").read_text(encoding="utf-8"))
    wanted = [x for x in args.only.split(",") if x] if args.only else list(pins["datasets"])
    target = Path(args.dir)
    target.mkdir(parents=True, exist_ok=True)
    for name in wanted:
        spec = pins["datasets"][name]
        dest = target / local_name(spec)
        if dest.exists() and dest.stat().st_size == spec["bytes"] and _sha256(dest) == spec["sha256"]:
            print(f"{name}: already present")
            continue
        url = pins["url_template"].format(**spec)
        last = None
        for attempt in range(1, 6):
            try:
                request = urllib.request.Request(url, headers={"User-Agent": "arc-quality/1"})
                with urllib.request.urlopen(request, timeout=120) as response:
                    data = response.read()
                break
            except OSError as error:
                last = error
                time.sleep(5 * attempt)
        else:
            raise SystemExit(f"could not fetch {url}: {last}")
        sha = hashlib.sha256(data).hexdigest()
        if len(data) != spec["bytes"] or sha != spec["sha256"]:
            raise SystemExit(f"{name}: got {len(data)} bytes, SHA-256 {sha}; pinned {spec['bytes']} / {spec['sha256']}")
        dest.write_bytes(data)
        print(f"{name}: {spec['repo']}@{spec['revision'][:12]} {spec['path']} ({len(data)} bytes, verified)")
    return 0


def _parquet_rows(path: Path) -> list[dict]:
    import pyarrow.parquet as pq

    return pq.read_table(path).to_pylist()


def all_items(benchmark: str, data_dir: Path | None) -> list[dict]:
    if benchmark == "toolcall":
        return benchmarks.toolcall_items()
    if data_dir is None:
        raise SystemExit(f"{benchmark} needs --data-dir (run fetch-data first)")
    pins = json.loads((HERE / "data" / "datasets.json").read_text(encoding="utf-8"))["datasets"]
    path = data_dir / local_name(pins[benchmark])
    if not path.exists():
        raise SystemExit(f"{path} is missing; run fetch-data")
    if _sha256(path) != pins[benchmark]["sha256"]:
        raise SystemExit(f"{path} does not match its pinned SHA-256")
    rows = _parquet_rows(path)
    if benchmark == "mmlu_pro":
        return [benchmarks.mmlu_pro_item(r) for r in rows]
    if benchmark == "gsm8k":
        return [benchmarks.gsm8k_item(r, i) for i, r in enumerate(rows)]
    if benchmark == "humaneval":
        return [benchmarks.humaneval_item(r) for r in rows]
    if benchmark == "mbpp":
        return [benchmarks.mbpp_item(r) for r in rows]
    raise SystemExit(f"unknown benchmark {benchmark}")


def parse_counts(text: str) -> list[tuple[str, int | None]]:
    """"mmlu_pro=30,gsm8k=all" -> [("mmlu_pro", 30), ("gsm8k", None)]."""
    counts = []
    for part in text.split(","):
        if not part.strip():
            continue
        name, _, count = part.partition("=")
        name = name.strip()
        if name not in benchmarks.BENCHMARKS:
            raise SystemExit(f"unknown benchmark {name}; known: {', '.join(benchmarks.BENCHMARKS)}")
        counts.append((name, None if count.strip() in ("", "all") else int(count)))
    return counts


def select(items: list[dict], count: int | None) -> list[dict]:
    ranked = sorted(items, key=lambda it: benchmarks.selection_rank(it["benchmark"], it["source_id"]))
    chosen = ranked if count is None else ranked[:count]
    return sorted(chosen, key=lambda it: it["id"])


def engine_case(item: dict, profile: dict) -> dict:
    case_defaults = profile["cases"]
    case = {"id": item["id"], "user": item["user"]}
    if item.get("system") is not None:
        case["system"] = item["system"]
    for key in ("today", "thinking"):
        if key in case_defaults:
            case[key] = case_defaults[key]
    case["max_tokens"] = int(item["max_tokens"])
    case["eos"] = list(case_defaults["eos"])
    case["selection"] = case_defaults.get("selection", "argmax")
    return case


def cmd_prepare(args) -> int:
    profile = json.loads(Path(args.profile).read_text(encoding="utf-8"))
    data_dir = Path(args.data_dir) if args.data_dir else None
    chosen = []
    for name, count in parse_counts(args.benchmarks):
        chosen += select(all_items(name, data_dir), count)
    if args.shard:
        index, _, total = args.shard.partition("/")
        index, total = int(index), int(total)
        chosen = [item for n, item in enumerate(chosen) if n % total == index]
    Path(args.out_items).write_text(
        "".join(json.dumps(item, ensure_ascii=False) + "\n" for item in chosen), encoding="utf-8")
    cases = {
        "schema": "arc.modern-cases.v1",
        "note": f"ARC quality harness items ({args.benchmarks}) for {profile['name']}; greedy argmax.",
        "cases": [engine_case(item, profile) for item in chosen],
    }
    Path(args.out_cases).write_text(json.dumps(cases, indent=1, ensure_ascii=False) + "\n", encoding="utf-8")
    counts = {}
    for item in chosen:
        counts[item["benchmark"]] = counts.get(item["benchmark"], 0) + 1
    print(json.dumps({"items": len(chosen), "by_benchmark": counts,
                      "max_new_tokens": sum(int(i["max_tokens"]) for i in chosen)}))
    return 0


def _decoder(path: str | None):
    if not path:
        return None
    from tokenizers import Tokenizer

    tokenizer = Tokenizer.from_file(path)
    return lambda ids: tokenizer.decode(ids, skip_special_tokens=True)


def cmd_score(args) -> int:
    items = {item["id"]: item for item in reference.read_items(args.items)}
    decode = _decoder(args.tokenizer)
    results = []
    run_meta = []
    for path in args.run:
        run = reference.load_run(path)
        run_meta.append({
            "file": Path(path).name,
            "sha256": _sha256(Path(path)),
            "schema": run.get("schema"),
            "engine": run.get("engine"),
            "kernel": run.get("kernel"),
            "profile": run.get("profile"),
            "package": run.get("package"),
            "platform": run.get("platform"),
            "matrix_digest": run.get("matrix_digest"),
            "timing": run.get("timing"),
            "usage": run.get("usage"),
        })
        for case in run["cases"]:
            item = items.get(case["id"])
            if item is None:
                continue
            tokens = case.get("tokens")
            if decode is not None and tokens is not None:
                text = decode(tokens)
            else:
                text = case.get("text")
            graded = benchmarks.score(item, text, allow_exec=args.allow_exec)
            results.append({
                "id": item["id"],
                "benchmark": item["benchmark"],
                "item_sha256": provenance.digest(item),
                **graded,
                "text": text,
                "tokens": tokens,
                "prompt_tokens": case.get("prompt_tokens"),
                "output_hash": case.get("output_hash"),
                "logits_digest": case.get("logits_digest"),
            })
    missing = sorted(set(items) - {r["id"] for r in results})
    evidence = None
    if args.provenance:
        evidence = json.loads(Path(args.provenance).read_text(encoding="utf-8"))
        if provenance.identity(evidence) is None:
            raise ValueError("invalid model provenance manifest")
        if sorted(evidence.get("run_sha256", [])) != sorted(r["sha256"] for r in run_meta):
            raise ValueError("provenance manifest does not bind these run files")
        if args.tokenizer and _sha256(Path(args.tokenizer)) != evidence["tokenizer_sha256"]:
            raise ValueError("provenance tokenizer digest mismatch")
    doc = {
        "schema": SCORED_SCHEMA,
        "provenance": evidence,
        "engine": {"label": args.label},
        "run": run_meta,
        "missing": missing,
        "results": sorted(results, key=lambda r: r["id"]),
    }
    Path(args.out).write_text(json.dumps(doc, indent=1, ensure_ascii=False) + "\n", encoding="utf-8")
    correct = sum(1 for r in results if r["correct"])
    print(f"{args.label}: {correct}/{len(results)} correct, {len(missing)} missing")
    return 1 if missing and not args.allow_missing else 0


def cmd_compare(args) -> int:
    items = reference.read_items(args.items)
    policy = json.loads(Path(args.policy).read_text(encoding="utf-8"))
    result = report.compare(items, args.arc, args.reference, policy, args.reference_b, args.ppl, args.label)
    Path(args.out).write_text(json.dumps(result, indent=1, ensure_ascii=False) + "\n", encoding="utf-8")
    text = report.markdown(result)
    if args.summary_md:
        Path(args.summary_md).write_text(text, encoding="utf-8")
    print(text)
    if result["items"]["missing"] and not args.allow_missing:
        print(f"missing items: {result['items']['missing'][:20]}")
        return 1
    if args.enforce and result["overall"]["verdict"] != "PASS":
        return 1
    return 0


def main(argv: list[str]) -> int:
    parser = argparse.ArgumentParser(prog="arc_quality", description=__doc__,
                                     formatter_class=argparse.RawDescriptionHelpFormatter)
    sub = parser.add_subparsers(dest="command", required=True)

    p = sub.add_parser("fetch-data")
    p.add_argument("--dir", required=True)
    p.add_argument("--only", default="", help="comma-separated dataset names")
    p.set_defaults(func=cmd_fetch_data)

    p = sub.add_parser("prepare")
    p.add_argument("--profile", required=True, help="models/<model>.json")
    p.add_argument("--benchmarks", required=True, help='e.g. "mmlu_pro=30,gsm8k=20,toolcall=all"')
    p.add_argument("--data-dir")
    p.add_argument("--shard", default="", help="i/N: keep every N-th item starting at i")
    p.add_argument("--out-items", required=True)
    p.add_argument("--out-cases", required=True)
    p.set_defaults(func=cmd_prepare)

    p = sub.add_parser("reference")
    reference.add_parsers(p.add_subparsers(dest="kind", required=True))

    p = sub.add_parser("score")
    p.add_argument("--items", action="append", required=True)
    p.add_argument("--run", action="append", required=True)
    p.add_argument("--label", required=True)
    p.add_argument("--provenance", help="audited model and run identity manifest (see quality docs)")
    p.add_argument("--tokenizer", help="tokenizer.json: decode token ids (one decoder for every engine)")
    p.add_argument("--allow-exec", action="store_true",
                   help="execute model-written code for humaneval/mbpp (disposable machines only)")
    p.add_argument("--allow-missing", action="store_true")
    p.add_argument("--out", required=True)
    p.set_defaults(func=cmd_score)

    p = sub.add_parser("compare")
    p.add_argument("--items", action="append", required=True)
    p.add_argument("--arc", action="append", required=True, help="scored ARC file(s)")
    p.add_argument("--reference", action="append", required=True, help="scored reference file(s)")
    p.add_argument("--reference-b", action="append", help="a second, independent reference run (noise floor)")
    p.add_argument("--ppl", action="append", help="arc.modern-quality.v1 perplexity reports")
    p.add_argument("--policy", default=str(HERE / "policy.json"))
    p.add_argument("--label", default="")
    p.add_argument("--allow-missing", action="store_true")
    p.add_argument("--enforce", action="store_true", help="exit 1 unless the policy verdict is PASS")
    p.add_argument("--out", required=True)
    p.add_argument("--summary-md")
    p.set_defaults(func=cmd_compare)

    args = parser.parse_args(argv)
    return args.func(args)


if __name__ == "__main__":
    sys.exit(main(sys.argv[1:]))
