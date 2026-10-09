#!/usr/bin/env python3
"""Summarise the kimi-k26-slices.yml evidence: digests across runners and per-shard resources.

    python scripts/arc_mla/slice_report.py ARTIFACTS_DIR --summary-md OUT.md --out OUT.json

ARTIFACTS_DIR holds the downloaded artifacts: tiny-<os>/slices-summary.json from
the tiny legs and kimi-<os>/{manifest.json, stream-report.json, ...} from the
real-shard legs. Exits 1 when the same input gave different digests on two
runners, or when a cross-check recorded a mismatch.
"""

from __future__ import annotations

import argparse
import json
import sys
from pathlib import Path


def load(path: Path):
    return json.loads(path.read_text(encoding="utf-8")) if path.exists() else None


def gib(n) -> str:
    return "–" if n is None else f"{n / 2**30:.2f} GiB"


def main(argv: list) -> int:
    parser = argparse.ArgumentParser(description=__doc__.splitlines()[0])
    parser.add_argument("artifacts")
    parser.add_argument("--summary-md", required=True)
    parser.add_argument("--out", required=True)
    args = parser.parse_args(argv)
    root = Path(args.artifacts)
    ok = True
    md = ["## Kimi-K2.6 weight slices", ""]

    tiny = {d.name[len("tiny-"):]: load(d / "slices-summary.json") for d in sorted(root.glob("tiny-*"))}
    if tiny:
        variants = sorted({v for s in tiny.values() if s for v in s})
        md += ["### Tiny packed checkpoints: slice manifest digest per runner", "",
               "| variant | " + " | ".join(tiny) + " | same |", "|---" * (len(tiny) + 2) + "|"]
        for v in variants:
            got = [(s or {}).get(v) for s in tiny.values()]
            same = None not in got and len(set(got)) == 1
            ok &= same
            md.append(f"| {v} | " + " | ".join(f"`{g[:16]}…`" if g else "missing" for g in got)
                      + f" | {'yes' if same else '**NO**'} |")
        md.append("")

    kimi = {d.name[len("kimi-"):]: d for d in sorted(root.glob("kimi-*")) if d.is_dir()}
    result = {"tiny": tiny, "kimi": {}}
    if kimi:
        manifests = {os_: load(d / "manifest.json") for os_, d in kimi.items()}
        digests = {os_: (m or {}).get("manifest_blake3") for os_, m in manifests.items()}
        same = None not in digests.values() and len(set(digests.values())) == 1
        ok &= same
        md += ["### Kimi-K2.6 shards 1-2 (layers 0-1): manifest digest per runner", "",
               "| runner | manifest_blake3 |", "|---|---|"]
        md += [f"| {os_} | `{d}` |" for os_, d in digests.items()]
        md += ["", f"Same on every runner: **{'yes' if same else 'NO'}**.", ""]
        first = next((m for m in manifests.values() if m), None)
        if first:
            md += ["| segment | bytes | BLAKE3 |", "|---|---|---|"]
            md += [f"| {s['name']} | {s['bytes']:,} | `{s['blake3']}` |" for s in first["segments"]]
            sizes = {}
            for s in first["slices"]:
                kind = "experts" if s["experts"] else s["name"]
                sizes.setdefault(kind, []).append(s["bytes"])
            md += ["", f"{len(first['slices'])} slices ({first['expert_groups']} expert groups per MoE layer); "
                   f"pending preparation: {', '.join(first['pending']) or 'none'}.", ""]
        for os_, d in kimi.items():
            report = load(d / "stream-report.json")
            entry = {"manifest_blake3": digests[os_]}
            if report:
                md += [f"### Resources per shard ({os_}, CI runner)", "",
                       "| step | shard | source | download | convert (incl. SHA-256 re-check) | peak RSS | disk after step |",
                       "|---|---|---|---|---|---|---|"]
                for s in report["steps"]:
                    if s.get("skipped"):
                        continue
                    md.append(f"| {','.join(s['units'])} | {','.join(x.split('-')[1] for x in s['shards'])} | "
                              f"{gib(s['source_bytes'])} | {s['download_seconds']:.1f} s | {s['slice_seconds']:.1f} s | "
                              f"{gib(s['peak_rss_bytes'])} | {gib(s['disk_bytes_after_slicing'])} |")
                md += ["", f"Peak disk {gib(report['peak_disk_bytes'])}; total {report['total_seconds']:.0f} s; "
                       f"{report['platform']['cpus']} CPUs.", ""]
                entry["stream"] = report
            for name, label in (("verify.json", "every slice and segment re-hashed"),
                                ("manifest-rerun.json", "second conversion on 1 thread"),
                                ("python-manifest.json", "independent Python preparer"),
                                ("ct-check.json", "compressed-tensors unpack_from_int32")):
                value = load(d / name)
                if value is None:
                    continue
                if name == "verify.json":
                    good = value.get("verified") is True
                elif name == "ct-check.json":
                    good = value.get("all_equal") is True
                    label += f" ({value['compressed_tensors']}, {value['checked']} expert projections)"
                else:
                    good = value.get("manifest_blake3") == digests[os_]
                ok &= good
                entry[name] = good
                md.append(f"- {os_}: {label}: **{'match' if good else 'MISMATCH'}**")
            md.append("")
            result["kimi"][os_] = entry
    result["all_match"] = ok
    md.append(f"All match: **{ok}**.")
    Path(args.summary_md).write_text("\n".join(md) + "\n", encoding="utf-8")
    Path(args.out).write_text(json.dumps(result, indent=1) + "\n", encoding="utf-8")
    print("\n".join(md))
    return 0 if ok else 1


if __name__ == "__main__":
    sys.exit(main(sys.argv[1:]))
