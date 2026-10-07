#!/usr/bin/env python3
"""Convert a pinned checkpoint to ARC weight slices one source shard at a time.

    python scripts/arc_mla/stream_slices.py --arc-mla target/release/arc-mla \\
        --source-manifest docs/protocol/packages/kimi-k26.source.json \\
        --work WORKDIR --out SLICES [--layers A:B] [--embed] [--head] \\
        [--expert-groups G] [--keep-source] [--resume] [--report REPORT.json]

1. Fetch config.json and the pinned model.safetensors.index.json (SHA-256
   checked) into WORKDIR.
2. ``arc-mla slice-plan`` orders the selected units (embed, layer.N, head) so
   that each step needs one shard set; a shard is deleted after the last step
   that reads it.
3. For each step: download the step's shards (length and SHA-256 checked while
   downloading), run ``arc-mla slice`` (which checks them again and writes the
   content-addressed slices and one record per unit), then delete the shards no
   later step needs. The whole model is never on one disk: at most one step's
   shards plus the slices written so far.
4. ``arc-mla slice-manifest`` writes SLICES/manifest.json.

The report records, per step, the download time and bytes, the conversion
time, the converter's peak resident memory and the disk in use, so the
resources per shard are measured, not estimated. Standard library only.
"""

from __future__ import annotations

import argparse
import json
import os
import platform
import subprocess
import sys
import time
from pathlib import Path

sys.path.insert(0, str(Path(__file__).resolve().parent.parent / "arc_modern"))

from fetch_source import fetch  # noqa: E402


def tree_bytes(path: Path) -> int:
    if not path.exists():
        return 0
    return sum(f.stat().st_size for f in path.rglob("*") if f.is_file())


def run(cmd: list) -> tuple:
    """Run cmd; return (stdout, seconds, peak RSS in bytes or None)."""
    start = time.time()
    # stderr passes through, so only stdout is read and nothing can block.
    proc = subprocess.Popen(cmd, stdout=subprocess.PIPE)
    stdout = proc.stdout.read()
    proc.stdout.close()
    rss = None
    if hasattr(os, "wait4"):
        # wait4 reports the resource use of this child alone.
        _, status, usage = os.wait4(proc.pid, 0)
        proc.returncode = os.waitstatus_to_exitcode(status)
        rss = usage.ru_maxrss * (1 if sys.platform == "darwin" else 1024)
    else:
        proc.wait()
    seconds = time.time() - start
    if proc.returncode != 0:
        raise SystemExit(f"{' '.join(cmd[:2])} failed with exit code {proc.returncode}")
    return stdout.decode("utf-8"), seconds, rss


def unit_args(units: list) -> list:
    """arc-mla selection flags for a list of unit names (consecutive layers)."""
    args, layers = [], []
    for u in units:
        if u == "embed":
            args.append("--embed")
        elif u == "head":
            args.append("--head")
        else:
            layers.append(int(u.split(".")[1]))
    if layers:
        if layers != list(range(layers[0], layers[-1] + 1)):
            raise SystemExit(f"step layers {layers} are not consecutive")
        args += ["--layers", f"{layers[0]}:{layers[-1] + 1}"]
    return args


def main(argv: list) -> int:
    parser = argparse.ArgumentParser(description=__doc__.splitlines()[0])
    parser.add_argument("--arc-mla", required=True)
    parser.add_argument("--source-manifest", required=True)
    parser.add_argument("--work", required=True, help="where source shards are downloaded and deleted")
    parser.add_argument("--out", required=True, help="slice directory")
    parser.add_argument("--layers")
    parser.add_argument("--embed", action="store_true")
    parser.add_argument("--head", action="store_true")
    parser.add_argument("--expert-groups", default="1")
    parser.add_argument("--experts")
    parser.add_argument("--threads")
    parser.add_argument("--keep-source", action="store_true", help="do not delete shards after use")
    parser.add_argument("--discard", action="store_true",
                        help="hash the slices and record them without storing the slice files")
    parser.add_argument("--resume", action="store_true", help="skip steps whose unit records exist")
    parser.add_argument("--report")
    args = parser.parse_args(argv)

    manifest_path = Path(args.source_manifest).resolve()
    manifest = json.loads(manifest_path.read_text(encoding="utf-8"))
    if manifest.get("schema") != "arc.hf-source.v1":
        raise SystemExit("not an arc.hf-source.v1 manifest")
    work, out = Path(args.work), Path(args.out)
    work.mkdir(parents=True, exist_ok=True)
    out.mkdir(parents=True, exist_ok=True)
    template = manifest.get("url_template", "https://huggingface.co/{repo}/resolve/{revision}/{name}")
    pinned = {f["name"]: f for f in manifest["files"]}

    def get(entry: dict) -> tuple:
        name = entry["name"]
        if "/" in name or "\\" in name or name.startswith("."):
            raise SystemExit(f"refusing non-plain file name {name!r}")
        url = template.format(repo=manifest["repo"], revision=manifest["revision"], name=name)
        start = time.time()
        status = fetch(url, work / name, int(entry["bytes"]), entry["sha256"])
        return status, time.time() - start

    small = [pinned["config.json"]] + ([manifest["index"]] if "index" in manifest else [])
    for entry in small:
        get(entry)

    common = ["--source-dir", str(work), "--source-manifest", str(manifest_path),
              "--expert-groups", args.expert_groups]
    if args.experts:
        common += ["--experts", args.experts]
    select = []
    if args.layers:
        select += ["--layers", args.layers]
    if args.embed:
        select.append("--embed")
    if args.head:
        select.append("--head")
    plan_path = out / "plan.json"
    run([args.arc_mla, "slice-plan", *common, *select, "--out", str(plan_path)])
    plan = json.loads(plan_path.read_text(encoding="utf-8"))

    def release(step: dict) -> list:
        """Delete the shards no later step reads (unless --keep-source)."""
        released = []
        if not args.keep_source:
            for name in step["release"]:
                path = work / name
                if path.exists():
                    path.unlink()
                    released.append(name)
        return released

    steps = []
    peak_disk = 0
    start_all = time.time()
    for i, step in enumerate(plan["steps"]):
        record = {"step": i, "units": step["units"], "shards": step["shards"]}
        done = all((out / "units" / f"{u}.json").exists() for u in step["units"])
        if args.resume and done:
            # A run interrupted after converting this step but before deleting
            # its shards left them on disk; delete them now, before the next
            # step downloads anything, so at most one step's shards are held.
            record["skipped"] = "unit records exist"
            record["deleted"] = release(step)
            steps.append(record)
            continue
        download_seconds, download_bytes = 0.0, 0
        for name in step["shards"]:
            status, seconds = get(pinned[name])
            download_seconds += seconds
            if status == "downloaded":
                download_bytes += int(pinned[name]["bytes"])
        source_bytes = sum((work / n).stat().st_size for n in step["shards"])
        cmd = [args.arc_mla, "slice", *common, "--out-dir", str(out), *unit_args(step["units"])]
        if args.threads:
            cmd += ["--threads", args.threads]
        if args.discard:
            cmd.append("--discard")
        stdout, seconds, rss = run(cmd)
        report = json.loads(stdout)
        disk = tree_bytes(work) + tree_bytes(out)
        peak_disk = max(peak_disk, disk)
        released = release(step)
        record.update({
            "download_seconds": round(download_seconds, 3),
            "downloaded_bytes": download_bytes,
            "source_bytes": source_bytes,
            "slice_seconds": round(seconds, 3),
            "slice_units": report["units"],
            "peak_rss_bytes": rss,
            "disk_bytes_after_slicing": disk,
            "deleted": released,
        })
        steps.append(record)
        print(f"step {i}: {','.join(step['units'])} from {','.join(step['shards'])}: "
              f"download {download_seconds:.1f} s, convert {seconds:.1f} s, "
              f"peak RSS {rss if rss is None else round(rss / 2**20)} MiB, disk {disk / 2**30:.2f} GiB",
              flush=True)

    manifest_out = out / "manifest.json"
    summary, seconds, _ = run([args.arc_mla, "slice-manifest", *common, "--out-dir", str(out),
                               "--out", str(manifest_out)])
    result = {
        "schema": "arc.slice-stream-report.v1",
        "source": {"repo": manifest["repo"], "revision": manifest["revision"]},
        "plan_peak_source_bytes": plan["peak_source_bytes"],
        "expert_groups": int(args.expert_groups),
        "steps": steps,
        "peak_disk_bytes": peak_disk,
        "total_seconds": round(time.time() - start_all, 3),
        "manifest": json.loads(summary),
        "platform": {"system": platform.system(), "machine": platform.machine(),
                     "cpus": os.cpu_count()},
    }
    if args.report:
        Path(args.report).write_text(json.dumps(result, indent=1) + "\n", encoding="utf-8")
    print(json.dumps(result["manifest"], indent=1))
    return 0


if __name__ == "__main__":
    sys.exit(main(sys.argv[1:]))
