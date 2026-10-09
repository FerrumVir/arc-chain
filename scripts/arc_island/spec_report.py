"""Render `arc-island spec-bench` results: plain decoding vs synchronous and
asynchronous pipelined speculation on the emulated WAN.

Every number is an EMULATED measurement on the machine that ran the bench:
a synthetic MLA/MoE model (not Kimi), stage processes on one host joined by
loopback TCP, every stage's outgoing hop shaped. It is never a real-WAN or a
Kimi speed. Runs that differ from plain decoding by a single byte are refused.

    python3 spec_report.py spec-1ms.json spec-5ms.json ... --out report.md
"""
import argparse
import json
import math


def load(paths):
    """Rows and run descriptions of every file; refuses inexact runs."""
    rows, runs = [], []
    for path in paths:
        with open(path) as source:
            data = json.load(source)
        if data.get("schema") != "arc-island-spec-bench-v1":
            raise ValueError(f"{path}: unknown schema")
        if data["failures"]:
            raise ValueError(f"{path}: exactness failures: {data['failures']}")
        for row in data["rows"]:
            if not (row["bit_exact_vs_single_process"] and row["ledger_equal_to_plain"]):
                raise ValueError(f"{path}: a run differs from plain decoding")
        rows.extend(data["rows"])
        runs.append(data)
    if not rows:
        raise ValueError("no measurements")
    return rows, runs


def rate(row):
    """Mean per-answer decode tokens/s of a run."""
    value = row["per_answer_decode_tok_s_mean"]
    if value is None or not math.isfinite(value) or value <= 0:
        raise ValueError("a run has no per-answer decode rate")
    return value


def drafter(row):
    if row["mode"] == "plain":
        return "none"
    if row["acceptance_nominal"] is not None:
        return f"scripted a={row['acceptance_nominal']:g}"
    return row["drafter"]


def drafter_order(name):
    """Scripted drafters by acceptance, then the others by name."""
    if name.startswith("scripted a="):
        return (0, float(name.split("=")[1]), name)
    return (1, 0.0, name)


def shape(row):
    if row["mode"] == "sync":
        return f"k={row['rows'] - 1}"
    if row["mode"] == "async":
        return f"{row['rows']}x{row['depth']}"
    return "-"


def waste(row):
    """Decode positions sent per decode token: 1 is plain decoding."""
    spec = row.get("speculation")
    decode_tokens = row["generated_tokens"] - row["answers"]
    if not spec or decode_tokens <= 0:
        return 1.0
    return spec["positions"] / decode_tokens


def acceptance(row):
    value = row.get("acceptance_measured")
    return "-" if value is None else f"{value:.2f}"


def cells(rows):
    """Runs grouped by island: (stages, uplink Mbit/s, one-way hop ms)."""
    grouped = {}
    for row in rows:
        key = (row["stages"], row["uplink_mbit"], row["hop_ms"])
        grouped.setdefault(key, []).append(row)
    return grouped


def summary(rows):
    """Per island and drafter: plain, the best synchronous and the best
    pipelined shape of those swept, and the ratios."""
    out = []
    for key, group in sorted(cells(rows).items()):
        plain = [r for r in group if r["mode"] == "plain"]
        if len(plain) != 1:
            raise ValueError(f"{key}: expected one plain run")
        base = rate(plain[0])
        by_drafter = {}
        for row in group:
            if row["mode"] != "plain":
                by_drafter.setdefault(drafter(row), []).append(row)
        for name in sorted(by_drafter, key=drafter_order):
            runs = by_drafter[name]
            sync = max((r for r in runs if r["mode"] == "sync"), key=rate, default=None)
            pipe = max((r for r in runs if r["mode"] == "async"), key=rate, default=None)
            out.append({
                "stages": key[0], "uplink_mbit": key[1], "hop_ms": key[2],
                "drafter": name, "plain": base, "sync": sync, "async": pipe,
            })
    return out


def render(rows, runs):
    first = runs[0]
    assumed = first["assumed"]
    lines = [
        "# Asynchronous pipelined speculation: emulated WAN measurements",
        "",
        "**Emulated measurements on CI runners, not real-WAN and not Kimi speeds.** "
        + first["scope"],
        "",
        "Labels: " + "; ".join(sorted({run["label"] for run in runs})) + ".",
        "",
        f"Model: synthetic {first['model']['config'].get('n_layers')}-layer MLA/MoE "
        f"(d_model {first['model']['config'].get('d_model')}), "
        f"{assumed['requests']} answers of {assumed['max_tokens']} tokens per run "
        f"(prompt {assumed['prompt_len']}), RP64 greedy selection, "
        f"{assumed['wire_bytes_per_position']} bytes per position on every shaped hop, "
        f"jitter {assumed['jitter_fraction_of_hop']:g} of the hop delay. "
        "Hop delay is one-way per shaped hop (S hops per round trip).",
        "",
        "Shapes: plain = existing scheduler, one position per round trip; "
        "sync k = one pass of k drafts plus the verified token per round trip; "
        "async RxD = D passes of R positions in flight, each launched before "
        "the previous returns. Best = fastest of the shapes swept (a maximum "
        "over configurations, so it is mildly optimistic).",
        "",
        "## Per-answer decode tokens/s: plain vs best synchronous vs best pipelined",
        "",
        "| Stages | Uplink Mbit/s | Hop ms | Drafter | Plain | Best sync (shape) "
        "| Best async (shape) | Async / plain | Async / sync | Acceptance (measured) "
        "| Async positions sent per decode token |",
        "|---:|---:|---:|---|---:|---:|---:|---:|---:|---:|---:|",
    ]
    for s in summary(rows):
        sync, pipe = s["sync"], s["async"]
        sync_text = f"{rate(sync):.2f} ({shape(sync)})" if sync else "-"
        pipe_text = f"{rate(pipe):.2f} ({shape(pipe)})" if pipe else "-"
        vs_plain = f"{rate(pipe) / s['plain']:.2f}x" if pipe else "-"
        vs_sync = f"{rate(pipe) / rate(sync):.2f}x" if pipe and sync else "-"
        lines.append(
            f"| {s['stages']} | {s['uplink_mbit']:g} | {s['hop_ms']:g} | {s['drafter']} "
            f"| {s['plain']:.2f} | {sync_text} | {pipe_text} | {vs_plain} | {vs_sync} "
            f"| {acceptance(pipe) if pipe else '-'} | {waste(pipe) if pipe else 1.0:.2f} |"
        )
    lines.extend([
        "",
        "## Every run",
        "",
        "Rolled back = positions stages computed and then dropped; skipped = "
        "positions a stage never computed because the rollback was already queued "
        "behind them (early cancellation). Both are summed over stages.",
        "",
        "| Stages | Uplink | Hop ms | Drafter | Mode | Shape | tok/s mean | tok/s median "
        "| Answer s | Acceptance | Passes | Positions / decode token | Rollbacks "
        "| Rolled back | Skipped |",
        "|---:|---:|---:|---|---|---|---:|---:|---:|---:|---:|---:|---:|---:|---:|",
    ])
    for key, group in sorted(cells(rows).items()):
        ordered = sorted(group, key=lambda r: (
            r["mode"] != "plain", drafter_order(drafter(r)), r["mode"], r["rows"], r["depth"]))
        for row in ordered:
            spec = row.get("speculation") or {}
            lines.append(
                f"| {key[0]} | {key[1]:g} | {key[2]:g} | {drafter(row)} | {row['mode']} "
                f"| {shape(row)} | {rate(row):.2f} "
                f"| {row['per_answer_decode_tok_s_median']:.2f} "
                f"| {row['answer_seconds_mean']:.3f} | {acceptance(row)} "
                f"| {spec.get('passes', '-')} | {waste(row):.2f} "
                f"| {spec.get('rollbacks', '-')} | {row['stage_rolled_back_positions']:g} "
                f"| {row['stage_skipped_positions']:g} |"
            )
    return "\n".join(lines) + "\n"


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("inputs", nargs="+")
    parser.add_argument("--out", required=True)
    args = parser.parse_args()
    rows, runs = load(args.inputs)
    with open(args.out, "w") as out:
        out.write(render(rows, runs))


if __name__ == "__main__":
    main()
