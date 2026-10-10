#!/usr/bin/env python3
"""Wire-codec study report (scratch branch, not for merge): totals every leg's
codec measurements per message group and prints ratio and throughput tables."""
import glob
import json
import os
import sys

GROUPS = [
    ("hidden state, 2-way split (after layer 16)", lambda s: s == "hidden after layer 16"),
    ("hidden state, 4-way split (after 8, 16, 24)", lambda s: s.startswith("hidden after layer")),
    ("Wq partial sums, 2 shards", lambda s: s.startswith("Wq partial sums, 2 shards")),
    ("Wq partial sums, 4 shards", lambda s: s.startswith("Wq partial sums, 4 shards")),
]


def main(root, out_md, out_json, run_id):
    legs = []
    for path in sorted(glob.glob(os.path.join(root, "**", "codec-*.json"), recursive=True)):
        with open(path) as f:
            leg = json.load(f)
        name = os.path.basename(path)[len("codec-"):-len(".json")]
        leg["runner"] = "x86" if name.startswith("x86") else "arm64"
        legs.append(leg)
    if not legs:
        sys.exit("no legs")
    codecs = legs[0]["codecs"]
    md = [f"# Lossless wire codecs on the exact 7B's stage traffic (MEASURED, CI run {run_id})", ""]
    rows_total = sum(l["rows"] for l in legs if l["runner"] == "arm64")
    md.append(
        f"Teacher-forced on the agreement experiment's 60 prompts (both profiles), {rows_total} rows on arm64. "
        "Every message is one row's `i64` vector (4096 values, 32 KB raw) and every codec round-tripped exactly. "
        "Ratio = raw bytes / encoded bytes; throughput = raw MB per second of encode or decode, one thread."
    )
    out = {"run_id": run_id, "groups": {}}
    for runner in ["arm64", "x86"]:
        chosen = [l for l in legs if l["runner"] == runner]
        if not chosen:
            continue
        for title, match in GROUPS:
            agg = {"messages": 0, "values": 0, "max_abs": 0, "fits_i32": 0,
                   "codecs": {c: {"bytes": 0, "enc_ns": 0, "dec_ns": 0} for c in codecs}}
            for leg in chosen:
                for name, s in leg["streams"].items():
                    if not match(name):
                        continue
                    for k in ("messages", "values", "fits_i32"):
                        agg[k] += s[k]
                    agg["max_abs"] = max(agg["max_abs"], s["max_abs"])
                    for c in codecs:
                        for k in ("bytes", "enc_ns", "dec_ns"):
                            agg["codecs"][c][k] += s["codecs"][c][k]
            if not agg["messages"]:
                continue
            out["groups"][f"{runner}: {title}"] = agg
            raw = agg["codecs"]["raw i64"]["bytes"]
            md += ["", f"## {title} ({runner})", "",
                   f"{agg['messages']} messages; largest |value| {agg['max_abs']} "
                   f"(2^{agg['max_abs'].bit_length()}); every value fits i32 in "
                   f"{100 * agg['fits_i32'] / agg['messages']:.1f}% of messages.", "",
                   "| codec | ratio | bits/value | KB per message | encode MB/s | decode MB/s |",
                   "|---|---:|---:|---:|---:|---:|"]
            for c in codecs:
                v = agg["codecs"][c]
                md.append(
                    f"| {c} | {raw / v['bytes']:.2f}x | {8 * v['bytes'] / agg['values']:.1f} "
                    f"| {v['bytes'] / agg['messages'] / 1024:.1f} "
                    f"| {raw / 1e6 / (v['enc_ns'] / 1e9):.0f} | {raw / 1e6 / (v['dec_ns'] / 1e9):.0f} |"
                )
    with open(out_md, "w") as f:
        f.write("\n".join(md) + "\n")
    with open(out_json, "w") as f:
        json.dump(out, f, indent=1)
    print("\n".join(md))


if __name__ == "__main__":
    main(*sys.argv[1:5])
