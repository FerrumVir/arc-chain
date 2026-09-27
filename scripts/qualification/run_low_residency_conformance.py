#!/usr/bin/env python3
"""Offline full-resident vs strict row-process comparison; never downloads a model.

Runs reference, export, shared workers, coordinator in that order. Reference must
exit before any row heap exists. This does not activate/qualify a validator route.
--row-partitions N selects disjoint rows of every projection on N local daemons;
omitting it retains the original whole-layer layout. Neither proves WAN speed.
"""
import argparse
import hashlib
import json
import os
from pathlib import Path
import math
import re
import shutil
import signal
import subprocess
import sys
import tempfile
import time

GIB = 1024 ** 3
ROOT = Path(__file__).resolve().parents[2]
PACKAGE = ROOT / "docs/protocol/packages/llama-2-7b-q4km.manifest.json"
PROFILE = "arc.gguf-llama.i8-per-row.rope-interleaved.v1"
BINARIES = ("low_residency_conformance", "tensor_row_low_residency_export", "tensor_row_shared_worker")


def digest(path):
    result = hashlib.sha256()
    with Path(path).open("rb") as source:
        for chunk in iter(lambda: source.read(1024 * 1024), b""):
            result.update(chunk)
    return result.hexdigest()


def write_json(path, value, indent=2):
    with Path(path).open("x", encoding="utf-8") as out:
        json.dump(value, out, indent=indent, sort_keys=True)
        out.write("\n")


def kernel_environment(base, kernel, census=False):
    """Return an explicit child env; never let reference inherit coordinator mode."""
    return dict(base, ARC_FAST_CANONICAL_KERNEL="1" if kernel == "fast" else "0",
                ARC_QUALIFICATION_SIMD_CENSUS="1" if census else "0")


def validate_kernel_selection(kernel, reference_kernel):
    if kernel == "fast" and reference_kernel != "scalar":
        raise RuntimeError("fast distributed conformance requires a scalar reference")


def validate_kernel_report(report, expected, role, census):
    requested = report.get("fast_kernel_requested")
    enabled = report.get("fast_kernel_enabled")
    available = report.get("simd_available")
    if any(type(value) is not bool for value in (requested, enabled, available)):
        raise RuntimeError(f"{role} omitted observed kernel mode/availability")
    expected_fast = expected == "fast"
    if requested != expected_fast or enabled != expected_fast or (expected_fast and not available):
        raise RuntimeError(f"{role} observed kernel mode differs from requested {expected}")
    observation = {"requested_kernel": expected, "requested_fast_kernel": requested,
                   "effective_fast_kernel": enabled, "simd_available": available,
                   "projection_census": report.get("simd_projection_census")}
    if census and expected_fast:
        validate_projection_census(observation["projection_census"], role)
    elif observation["projection_census"] is not None:
        raise RuntimeError(f"{role} unexpectedly enabled SIMD census")
    return observation


def validate_projection_census(census, role):
    fields = ("attempted", "accepted", "refused_unavailable", "refused_shape",
              "refused_inner_dim_above_i32_bound", "refused_activation_out_of_domain",
              "refused_scale_multiply_would_overflow")
    if not isinstance(census, dict) or any(type(census.get(field)) is not int or census[field] < 0 for field in fields):
        raise RuntimeError(f"{role} omitted valid projection census")
    if census["attempted"] == 0 or census["accepted"] != census["attempted"] or any(census[field] for field in fields[2:]):
        raise RuntimeError(f"{role} fast path was unavailable or refused one or more projections")
    return census


def read_daemon_simd_report(path, worker_id, expected, census):
    events = []
    for line in path.read_text().splitlines():
        if line.startswith("{"):
            try:
                event = json.loads(line)
            except json.JSONDecodeError:
                continue
            if event.get("event") == "row_service_stopped":
                events.append(event)
    if len(events) != 1 or not isinstance(events[0].get("stats"), dict):
        raise RuntimeError(f"worker {worker_id} omitted final service diagnostics")
    stats = events[0]["stats"]
    requested = stats.get("fast_kernel_requested")
    enabled = stats.get("fast_kernel_enabled")
    available = stats.get("simd_available")
    if stats.get("worker_id") != worker_id or any(type(value) is not bool for value in (requested, enabled, available)):
        raise RuntimeError(f"worker {worker_id} omitted its observed kernel identity/mode")
    expected_fast = expected == "fast"
    if requested != expected_fast or enabled != expected_fast or (expected_fast and not available):
        raise RuntimeError(f"worker {worker_id} did not run requested {expected} mode")
    if stats.get("completed_calls", 0) <= 0 or stats.get("refused_calls", 0) != 0:
        raise RuntimeError(f"worker {worker_id} did not serve clean projection calls")
    census_data = stats.get("projection_census")
    if census and expected_fast:
        validate_projection_census(census_data, "worker " + worker_id)
    elif census_data is not None:
        raise RuntimeError(f"worker {worker_id} unexpectedly enabled SIMD census")
    return {"worker_id": worker_id, "requested_kernel": expected,
            "requested_fast_kernel": requested, "effective_fast_kernel": enabled,
            "simd_available": available, "completed_calls": stats["completed_calls"],
            "refused_calls": stats["refused_calls"], "projection_census": census_data}


def read_numbers(path):
    values = {}
    for line in Path(path).read_text().splitlines():
        parts = line.replace(":", "").split()
        if len(parts) >= 2 and parts[1].isdigit():
            values[parts[0]] = int(parts[1]) * (1024 if len(parts) == 3 and parts[2] == "kB" else 1)
    return values


def cgroup_snapshot():
    """Read only the caller's existing cgroup; never create or reset one."""
    try:
        member = next(line[3:] for line in Path("/proc/self/cgroup").read_text().splitlines()
                      if line.startswith("0::"))
        candidate = Path("/sys/fs/cgroup") / member.lstrip("/")
        # In a cgroup namespace the mount itself can already represent our group.
        if not (candidate / "memory.current").exists():
            candidate = Path("/sys/fs/cgroup")
        result = {"path": str(candidate)}
        for key in ("current", "max"):
            text = (candidate / ("memory." + key)).read_text().strip()
            result[key] = None if text == "max" else int(text)
        try:
            result["peak"] = int((candidate / "memory.peak").read_text().strip())
        except (OSError, ValueError):
            result["peak"] = None
        result["inactive_file"] = read_numbers(candidate / "memory.stat").get("inactive_file", 0)
        return result
    except (OSError, ValueError, StopIteration):
        return None


def resources(directory):
    if sys.platform != "linux":
        raise RuntimeError("runner requires Linux /proc resource checks; no assumed memory capacity")
    info = read_numbers("/proc/meminfo")
    if "MemAvailable" not in info:
        raise RuntimeError("MemAvailable is unavailable; refusing unmeasured capacity")
    physical = os.sysconf("SC_PHYS_PAGES") * os.sysconf("SC_PAGE_SIZE")
    available = min(info["MemAvailable"], physical)
    group = cgroup_snapshot()
    if group is None:
        raise RuntimeError("cgroup memory constraint unreadable; refusing unmeasured capacity")
    if group["max"] is not None:
        # Reclaimable inactive file cache is not a resident row heap. Preserve
        # both raw and adjusted readings; this remains a preflight, not RSS proof.
        group_available = max(0, group["max"] - group["current"] + group["inactive_file"])
        available = min(available, group_available)
    return {"physical_bytes_sysconf": physical, "mem_available_bytes": info["MemAvailable"],
            "effective_available_bytes": available, "disk_free_bytes": shutil.disk_usage(directory).free,
            "cgroup": group}


def require_capacity(snapshot, memory, disk=0):
    if snapshot["effective_available_bytes"] < memory:
        raise RuntimeError(f"insufficient available memory: need {memory} bytes, "
                           f"observed {snapshot['effective_available_bytes']}")
    if snapshot["disk_free_bytes"] < disk:
        raise RuntimeError(f"insufficient free disk: need {disk} bytes, observed {snapshot['disk_free_bytes']}")


def validate_layout_plan(plan, partitions):
    """Do not let an older inspector silently run the layer-only proof."""
    expected = "layers" if partitions is None else "partial-rows"
    if plan.get("layout") != expected:
        raise RuntimeError("inspected layout differs from requested proof")
    if partitions is not None:
        if plan.get("row_partitions") != partitions or len(plan["bundles"]) != partitions:
            raise RuntimeError("inspected partial worker count mismatch")
        for rank, spec in enumerate(plan["bundles"]):
            if (spec.get("row_partition") != f"{rank}/{partitions}" or not spec["include_output"]
                    or spec["layers"] != list(range(plan["graph"]["layers"]))):
                raise RuntimeError("inspected partial plan omits a layer/output/rank")
    if (len({spec["worker_id"] for spec in plan["bundles"]}) != len(plan["bundles"])
            or any(not 0 < spec["serialized_row_bytes"] <= GIB for spec in plan["bundles"])
            or sum(spec["serialized_row_bytes"] for spec in plan["bundles"]) != plan["total_row_file_bytes"]):
        raise RuntimeError("inspected bundle identity/aggregate capacity mismatch")


def validate_export_manifest(spec, manifest, plan, artifact):
    if (manifest["serialized_row_bytes"] != spec["serialized_row_bytes"]
            or manifest["worker_id"] != spec["worker_id"]
            or manifest["artifact_blake3"] != artifact or manifest["execution_profile"] != PROFILE):
        raise RuntimeError("exported manifest differs from bounded plan")
    partition = spec.get("row_partition")
    if partition is None:
        if manifest.get("format") != "arc.tensor-row-low-residency-bundle.v1":
            raise RuntimeError("whole-layer export format mismatch")
        return
    rank, count = map(int, partition.split("/"))
    if (manifest.get("format") != "arc.tensor-row-offline-partition-bundle.v1"
            or manifest.get("row_partition") != {"rank": rank, "count": count}
            or "resident_layers" in manifest or "resident_output" in manifest):
        raise RuntimeError("partial manifest format/rank or production residency mismatch")
    graph = plan["graph"]
    tensors = {"wq": graph["width"], "wk": graph["kv_width"], "wv": graph["kv_width"],
               "wo": graph["width"], "w_gate": graph["ff_width"], "w_up": graph["ff_width"],
               "w_down": graph["width"]}
    expected = {(layer, tensor): rows for layer in spec["layers"] for tensor, rows in tensors.items()}
    expected[(None, "lm_head")] = graph["vocab"]
    seen = set()
    for file in manifest["files"]:
        a = file["assignment"]
        key = (a["layer"], a["tensor"])
        rows = expected.get(key)
        if rows is None or key in seen:
            raise RuntimeError("unknown or duplicate partial projection")
        seen.add(key)
        if (a["artifact_id"] != artifact or a["execution_profile"] != PROFILE
                or a["worker_id"] != spec["worker_id"] or rows < count
                or (a["row_start"], a["row_end"]) != (rows * rank // count, rows * (rank + 1) // count)):
            raise RuntimeError("partial manifest identity/range mismatch")
    if seen != expected.keys():
        raise RuntimeError("partial manifest omits required projection, including output head")
    if sum(file["bytes"] for file in manifest["files"]) != manifest["serialized_row_bytes"]:
        raise RuntimeError("partial manifest byte total mismatch")


def validate_trace(trace, report):
    graph = report["graph"]
    tokens, positions = trace["output_token_ids"], trace["positions"]
    eos = graph["eos"]
    if (not tokens or len(tokens) > report["max_tokens"]
            or any(type(token) is not int or not 0 <= token < graph["vocab"] for token in tokens)
            or any(token in eos for token in tokens[:-1])
            or (len(tokens) < report["max_tokens"] and tokens[-1] not in eos)):
        raise RuntimeError("malformed generation budget/EOS trace")
    consumed = [graph["bos"], *report["prompt_token_ids"],
                *(tokens[:-1] if tokens[-1] in eos else tokens)]
    if len(positions) != len(consumed) or len(consumed) > graph["max_seq"]:
        raise RuntimeError("malformed forward position count")
    for index, (position, token) in enumerate(zip(positions, consumed)):
        if (type(position.get("position")) is not int or position["position"] != index
                or type(position.get("input_token")) is not int or position["input_token"] != token
                or type(position.get("logit_count")) is not int or position["logit_count"] != graph["vocab"]):
            raise RuntimeError("malformed position index/input/logit shape")
        for field in ("logits_blake3_le_i64", "kv_blake3"):
            if not isinstance(position.get(field), str) or not re.fullmatch(r"[0-9a-f]{64}", position[field]):
                raise RuntimeError("malformed position commitment")
    if not isinstance(trace.get("output_hash"), str) or not re.fullmatch(r"[0-9a-f]{64}", trace["output_hash"]):
        raise RuntimeError("malformed output commitment")
    timings = trace.get("forward_ms", [])
    if len(timings) != len(positions) or any(not isinstance(n, (int, float)) or not math.isfinite(n) or n < 0 for n in timings):
        raise RuntimeError("malformed forward timing shape")


def compare(reference, coordinator):
    """Equality excludes timing/RSS; never let empty matching reports pass."""
    for key in ("schema", "artifact_blake3", "artifact_bytes", "profile", "graph", "prompt_token_ids",
                "max_tokens", "generation_semantics", "warmup_count", "binary_blake3"):
        if key not in reference or reference[key] != coordinator.get(key):
            raise RuntimeError(f"conformance input/identity mismatch: {key}")
    if reference.get("mode") != "reference" or coordinator.get("mode") != "coordinator":
        raise RuntimeError("conformance mode mismatch")
    if (reference["schema"] != "arc.low-residency-conformance.v1" or reference["profile"] != PROFILE
            or reference["generation_semantics"] != "generation-v2/BOS-once/repetition-penalty/EOS-included"
            or type(reference["fast_kernel_enabled"]) is not bool
            or type(reference["artifact_bytes"]) is not int or reference["artifact_bytes"] <= 0):
        raise RuntimeError("unsupported or malformed conformance identity")
    for key in ("artifact_blake3", "binary_blake3"):
        if not isinstance(reference[key], str) or not re.fullmatch(r"[0-9a-f]{64}", reference[key]):
            raise RuntimeError("malformed input commitment")
    if (type(coordinator.get("non_answer_events")) is not int or coordinator["non_answer_events"] != 0
            or type(coordinator.get("local_primary_rows")) is not int or coordinator["local_primary_rows"] != 0):
        raise RuntimeError("coordinator did not remain on strict remote primary coverage")
    if type(coordinator.get("row_answers")) is not int or coordinator["row_answers"] <= 0:
        raise RuntimeError("coordinator returned no row-worker answers")
    graph = reference["graph"]
    prompt = reference["prompt_token_ids"]
    if (type(graph.get("vocab")) is not int or graph["vocab"] <= 0
            or type(graph.get("bos")) is not int or not 0 <= graph["bos"] < graph["vocab"]
            or not graph.get("eos") or any(type(n) is not int or not 0 <= n < graph["vocab"] for n in graph["eos"])
            or type(graph.get("max_seq")) is not int or graph["max_seq"] <= 0
            or not prompt or len(prompt) > 16 or prompt[0] == graph["bos"]
            or any(type(n) is not int or not 0 <= n < graph["vocab"] for n in prompt)
            or type(reference["max_tokens"]) is not int or not 1 <= reference["max_tokens"] <= 4
            or type(reference["warmup_count"]) is not int or reference["warmup_count"] not in (0, 1)):
        raise RuntimeError("malformed graph or conformance input bounds")
    for a, b in [(reference["measured"], coordinator["measured"])] + list(zip(
            reference.get("warmup_runs", []), coordinator.get("warmup_runs", []))):
        validate_trace(a, reference)
        validate_trace(b, coordinator)
        for key in ("output_token_ids", "output_hash", "positions"):
            if a[key] != b.get(key):
                raise RuntimeError(f"exact conformance mismatch: {key}")
    expected = reference["warmup_count"]
    if len(reference.get("warmup_runs", [])) != expected or len(coordinator.get("warmup_runs", [])) != expected:
        raise RuntimeError("missing warmup trace")
    return {"exact_output_tokens_and_hash": True, "exact_position_logit_and_kv_digests": True,
            "compared_positions": len(reference["measured"]["positions"])}


def require_clean_exits(records):
    bad = [record["name"] for record in records if record.get("returncode") != 0]
    if bad:
        raise RuntimeError("nonzero or unreaped child exit: " + ", ".join(bad))


class Children:
    def __init__(self, output, environment):
        self.output, self.environment = output, environment
        self.items, self.records = [], []
        self.sampled_aggregate_rss_peak_bytes = 0

    def sample(self):
        total = 0
        for process, _ in self.items:
            if process.poll() is None:
                try:
                    total += read_numbers(f"/proc/{process.pid}/status").get("VmRSS", 0)
                except (OSError, ValueError):
                    pass
        self.sampled_aggregate_rss_peak_bytes = max(self.sampled_aggregate_rss_peak_bytes, total)

    def start(self, name, command, environment=None):
        stdout = (self.output / (name + ".stdout.log")).open("x")
        stderr = (self.output / (name + ".stderr.log")).open("x")
        try:
            process = subprocess.Popen([str(x) for x in command], stdout=stdout, stderr=stderr,
                                       env=self.environment if environment is None else environment,
                                       start_new_session=True)
        finally:
            stdout.close()
            stderr.close()
        record = {"name": name, "pid": process.pid, "argv": [str(x) for x in command],
                  "started_unix_seconds": time.time(), "returncode": None}
        selected_environment = self.environment if environment is None else environment
        record["kernel_environment"] = {
            "ARC_FAST_CANONICAL_KERNEL": selected_environment.get("ARC_FAST_CANONICAL_KERNEL", "0"),
            "ARC_QUALIFICATION_SIMD_CENSUS": selected_environment.get("ARC_QUALIFICATION_SIMD_CENSUS", "0"),
        }
        self.items.append((process, record))
        self.records.append(record)
        return process

    def stop(self, processes, timeout=20):
        """Gracefully stop selected long-lived daemons for final diagnostics."""
        running = [process for process in processes if process.poll() is None]
        for process in running:
            try:
                os.killpg(process.pid, signal.SIGTERM)
            except ProcessLookupError:
                pass
        deadline = time.monotonic() + timeout
        for process in running:
            try:
                process.wait(timeout=max(0.01, deadline - time.monotonic()))
            except subprocess.TimeoutExpired:
                try:
                    os.killpg(process.pid, signal.SIGKILL)
                except ProcessLookupError:
                    pass
                process.wait(timeout=10)
            self.record_exit(process)

    def wait(self, process, timeout):
        end = time.monotonic() + timeout
        while process.poll() is None:
            self.sample()
            if time.monotonic() >= end:
                raise RuntimeError(f"child process {process.pid} exceeded {timeout}s deadline")
            time.sleep(0.1)
        self.sample()
        self.record_exit(process)
        if process.returncode:
            raise RuntimeError(f"child process {process.pid} exited {process.returncode}; see stderr log")

    def record_exit(self, process):
        for child, record in self.items:
            if child is process and record["returncode"] is None:
                record.update(returncode=child.returncode, ended_unix_seconds=time.time())

    def close(self):
        for process, _ in self.items:
            if process.poll() is None:
                try:
                    os.killpg(process.pid, signal.SIGTERM)
                except ProcessLookupError:
                    pass
        deadline = time.monotonic() + 20
        for process, _ in self.items:
            try:
                process.wait(timeout=max(0.01, deadline - time.monotonic()))
            except subprocess.TimeoutExpired:
                try:
                    os.killpg(process.pid, signal.SIGKILL)
                except ProcessLookupError:
                    pass
                process.wait(timeout=10)
            self.record_exit(process)


def parser():
    result = argparse.ArgumentParser(description=__doc__)
    result.add_argument("--binaries-dir", required=True, type=Path)
    result.add_argument("--model", required=True, type=Path)
    result.add_argument("--output-dir", required=True, type=Path)
    result.add_argument("--prompt-ids", default="6324", help="1..16 explicit IDs, no leading BOS")
    result.add_argument("--max-tokens", default=2, type=int, choices=range(1, 5))
    result.add_argument("--warmups", default=0, type=int, choices=(0, 1))
    result.add_argument("--kernel", default="scalar", choices=("scalar", "fast"))
    result.add_argument("--reference-kernel", default="scalar", choices=("scalar", "fast"),
                        help="independent reference mode; fast distributed mode requires scalar reference")
    result.add_argument("--row-partitions", type=int, choices=range(2, 17),
                        help="offline-only disjoint rows per tensor on 2..16 daemons; default uses layer bundles")
    result.add_argument("--timeout-seconds", default=1800, type=int)
    result.add_argument("--total-timeout-seconds", default=2700, type=int,
                        help="global deadline including all phases; cleanup/evidence writing follow it")
    return result


def run(args):
    output = args.output_dir
    if not output.is_absolute() or not args.model.is_absolute() or not args.binaries_dir.is_absolute():
        raise RuntimeError("model, binary and output paths must be absolute")
    prompt = [int(value) for value in args.prompt_ids.split(",")]
    if (not 1 <= len(prompt) <= 16 or not 60 <= args.timeout_seconds <= 3600
            or not 60 <= args.total_timeout_seconds <= 5400):
        raise RuntimeError("prompt count or timeout outside bounded limits")
    validate_kernel_selection(args.kernel, args.reference_kernel)
    output.mkdir(mode=0o700, exist_ok=False)
    package = json.loads(PACKAGE.read_text())
    if package["execution"]["profile"] != PROFILE:
        raise RuntimeError("package profile differs from supported conformance profile")
    if prompt[0] == package["tokenizer"]["bos"] or any(not 0 <= n < package["graph"]["vocab_size"] for n in prompt):
        raise RuntimeError("invalid prompt IDs or leading BOS")
    binaries = {name: args.binaries_dir / name for name in BINARIES}
    census_enabled = args.kernel == "fast" or args.reference_kernel == "fast"
    base_environment = dict(os.environ, ARC_BATCHED_PREFILL="0",
                            ARC_QUALIFICATION_SIMD_CENSUS="1" if census_enabled else "0")
    environments = {
        "inspect": kernel_environment(base_environment, "scalar", False),
        "reference": kernel_environment(base_environment, args.reference_kernel, args.reference_kernel == "fast"),
        "coordinator": kernel_environment(base_environment, args.kernel, args.kernel == "fast"),
        "worker": kernel_environment(base_environment, args.kernel, args.kernel == "fast"),
        "export": kernel_environment(base_environment, "scalar", False),
    }
    children = Children(output, base_environment)
    summary = {"schema": "arc.low-residency-conformance-run.v1", "pass": False,
               "scope": "offline numerical comparison only; not quality, paid path, SSH, distributed latency or fleet readiness",
               "inputs": {"prompt_token_ids": prompt, "max_tokens": args.max_tokens, "warmups": args.warmups,
                          "kernel": args.kernel, "reference_kernel": args.reference_kernel,
                          "simd_census_enabled": census_enabled,
                          "model": str(args.model), "row_partitions": args.row_partitions},
               "package_manifest_sha256": digest(PACKAGE), "resource_checks": [],
               "package_manifest_blake3": package["manifest_blake3"],
               "profile_commitment": package["execution"]["profile_commitment"],
               "generation_commitment": package["generation"]["commitment"],
               "measurement_caveat": "RSS samples cover direct child processes at ~100ms and may miss peaks; "
                   "process getrusage excludes daemons; existing cgroup peak is not reset and can include unrelated/prior work. "
                   "No validator, legacy shard, SSH relay, OS overhead or real-host capacity qualification is performed. "
                   "When SIMD census is enabled, counter atomics add measurement overhead and timings are not speed evidence."}
    runtime = None
    def expired(_signum, _frame):
        raise TimeoutError(f"global conformance deadline expired after {args.total_timeout_seconds}s")
    old_alarm = signal.signal(signal.SIGALRM, expired)
    try:
        signal.alarm(args.total_timeout_seconds)
        for name, path in binaries.items():
            if not path.is_file() or not os.access(path, os.X_OK):
                raise RuntimeError(f"required executable missing: {path}")
        summary["binary_sha256"] = {name: digest(path) for name, path in binaries.items()}
        if args.model.stat().st_size != package["artifact"]["bytes"] or digest(args.model) != package["artifact"]["sha256"]:
            raise RuntimeError("model size/SHA256 differs from pinned package; no model compute started")
        common = ["--model", args.model, "--artifact", package["artifact"]["blake3"],
                  "--prompt-ids", args.prompt_ids, "--max-tokens", str(args.max_tokens), "--warmups", str(args.warmups)]

        def checkpoint(phase, memory, disk=0):
            snapshot = resources(output)
            summary["resource_checks"].append(dict(phase=phase, required_memory_bytes=memory,
                                                    required_disk_bytes=disk, **snapshot))
            require_capacity(snapshot, memory, disk)

        checkpoint("inspect", 2 * GIB)
        layout_args = [] if args.row_partitions is None else ["--row-partitions", str(args.row_partitions)]
        child = children.start("inspect", [binaries[BINARIES[0]], "inspect", *common, *layout_args, "--output", output / "inspect.json"], environments["inspect"])
        children.wait(child, args.timeout_seconds)
        plan = json.loads((output / "inspect.json").read_text())
        if plan["artifact_blake3"] != package["artifact"]["blake3"] or plan["profile"] != PROFILE:
            raise RuntimeError("inspected artifact/profile pin mismatch")
        validate_layout_plan(plan, args.row_partitions)
        summary["layout"] = plan["layout"]
        positions = 1 + len(prompt) + args.max_tokens
        kv_bytes = plan["kv_bytes_per_position"] * positions
        disk = plan["total_row_file_bytes"] + GIB
        baseline_budget = max(12 * GIB, package["memory"]["prepared_total_bytes"] + kv_bytes + 3 * GIB)
        checkpoint("reference", baseline_budget, disk)
        reference = children.start("reference", [binaries[BINARIES[0]], "reference", *common,
                                                  "--output", output / "reference.json"], environments["reference"])
        children.wait(reference, args.timeout_seconds)
        summary["reference_exited_before_row_preparation"] = reference.returncode == 0
        bundles = output / "bundles"
        bundles.mkdir()
        manifests = []
        for spec in plan["bundles"]:
            checkpoint("export-" + spec["worker_id"], 2 * GIB, spec["serialized_row_bytes"] + GIB)
            dest = bundles / spec["worker_id"]
            command = [binaries[BINARIES[1]], "--model", args.model, "--artifact", package["artifact"]["blake3"],
                       "--worker-id", spec["worker_id"], "--layers", ",".join(map(str, spec["layers"])), "--output-dir", dest]
            if spec["include_output"]:
                command.append("--include-output")
            if spec.get("row_partition") is not None:
                command.extend(["--row-partition", spec["row_partition"]])
            children.wait(children.start("export-" + spec["worker_id"], command, environments["export"]), args.timeout_seconds)
            manifest = json.loads((dest / "manifest.json").read_text())
            validate_export_manifest(spec, manifest, plan, package["artifact"]["blake3"])
            manifests.append((dest, manifest))
        # Short private runtime paths avoid Unix sockaddr length limits on CI.
        runtime = Path(tempfile.mkdtemp(prefix="arc-proof-", dir="/tmp"))
        config, daemons = [], []
        remaining_rows = plan["total_row_file_bytes"]
        for dest, manifest in manifests:
            checkpoint("daemon-" + manifest["worker_id"], remaining_rows + kv_bytes + 2 * GIB)
            socket = runtime / (manifest["worker_id"] + ".sock")
            name = "daemon-" + manifest["worker_id"]
            daemon = children.start(name, [binaries[BINARIES[2]], "serve", "--rows-dir", dest / "rows",
                "--artifact", package["artifact"]["blake3"], "--socket", socket, "--max-clients", "2"],
                environments["worker"])
            daemons.append(daemon)
            end = time.monotonic() + 180
            while True:
                children.sample()
                if daemon.poll() is not None or time.monotonic() >= end:
                    raise RuntimeError(f"{name} failed or did not become ready within 180s")
                lines = (output / (name + ".stderr.log")).read_text().splitlines()
                started = [json.loads(line) for line in lines if line.startswith('{"')]
                started = [item for item in started if item.get("event") == "row_service_started"]
                if started:
                    stats = started[-1]["stats"]
                    if stats["resident_bundle_copies"] != 1 or stats["worker_id"] != manifest["worker_id"]:
                        raise RuntimeError("daemon resident-copy/worker identity mismatch")
                    break
                time.sleep(0.1)
            config.append({"worker_id": manifest["worker_id"], "socket": str(socket),
                           "assignments": [entry["assignment"] for entry in manifest["files"]]})
            remaining_rows -= manifest["serialized_row_bytes"]
        # Compact assignments keep even 16 x 225 projections within the
        # coordinator's existing 1 MiB configuration bound.
        if len(json.dumps(config, sort_keys=True).encode()) + 1 > 1024 * 1024:
            raise RuntimeError("worker assignment configuration exceeds 1 MiB")
        write_json(output / "workers.json", config, indent=None)
        checkpoint("coordinator", kv_bytes + 2 * GIB)
        coordinator = children.start("coordinator", [binaries[BINARIES[0]], "coordinator", *common, *layout_args,
            "--workers", output / "workers.json", "--output", output / "coordinator.json"], environments["coordinator"])
        children.wait(coordinator, args.timeout_seconds)
        if any(daemon.poll() is not None for daemon in daemons):
            raise RuntimeError("a row daemon exited before comparison completed")
        children.stop(daemons)
        daemon_reports = [read_daemon_simd_report(output / ("daemon-" + manifest["worker_id"] + ".stderr.log"),
                                                   manifest["worker_id"], args.kernel, census_enabled)
                          for _, manifest in manifests]
        expected = json.loads((output / "reference.json").read_text())
        actual = json.loads((output / "coordinator.json").read_text())
        if actual.get("row_partitions") != args.row_partitions:
            raise RuntimeError("coordinator did not use the requested partial-row proof")
        reference_kernel = validate_kernel_report(expected, args.reference_kernel, "reference", census_enabled)
        coordinator_kernel = validate_kernel_report(actual, args.kernel, "coordinator", census_enabled)
        summary["kernel_observations"] = {
            "reference": reference_kernel,
            "coordinator": coordinator_kernel,
            "workers": daemon_reports,
            "simd_path_qualified": bool(args.kernel == "fast" and args.reference_kernel == "scalar" and census_enabled and
                all(item["effective_fast_kernel"] for item in [reference_kernel, coordinator_kernel, *daemon_reports]
                    if item["requested_kernel"] == "fast")),
            "measurement_overhead": "projection census atomics enabled; do not use these timings as a speedup claim"
                if census_enabled else "projection census disabled",
        }
        if args.kernel == "fast" and not summary["kernel_observations"]["simd_path_qualified"]:
            raise RuntimeError("fast distributed run lacks accepted SIMD census from every compute process")
        summary.update(compare(expected, actual))
        summary["pass"] = True
    except Exception as error:
        summary["error"] = str(error)
    finally:
        signal.alarm(0)
        signal.signal(signal.SIGALRM, old_alarm)
        try:
            children.close()
            require_clean_exits(children.records)
        except Exception as error:
            summary["pass"] = False
            summary["cleanup_error"] = str(error)
        summary["processes"] = children.records
        summary["sampled_direct_children_rss_peak_bytes"] = children.sampled_aggregate_rss_peak_bytes
        summary["cgroup_after_cleanup"] = cgroup_snapshot()
        if runtime is not None and all(process.poll() is not None for process, _ in children.items):
            shutil.rmtree(runtime)
        write_json(output / "summary.json", summary)
    return summary


def main():
    def interrupted(signum, _frame):
        raise InterruptedError(f"conformance interrupted by signal {signum}")
    signal.signal(signal.SIGTERM, interrupted)
    signal.signal(signal.SIGINT, interrupted)
    try:
        summary = run(parser().parse_args())
        print(json.dumps({"pass": summary["pass"], "error": summary.get("error") or summary.get("cleanup_error")}))
        return 0 if summary["pass"] else 1
    except Exception as error:
        print(f"conformance refused: {error}", file=sys.stderr)
        return 1


if __name__ == "__main__":
    raise SystemExit(main())
