#!/usr/bin/env python3
"""Run one bounded, read-only Linux stake-zero community worker canary.

The published release asset and model are hash checked before execution. This
does not join P2P/consensus, submit transactions, or assert a reward; a mined
0x25 requires a separate client request and receipt check.
"""

from __future__ import annotations

import argparse
import hashlib
import importlib.util
import json
import os
import platform
import shutil
import signal
import socket
import stat
import subprocess
import sys
import tempfile
import threading
import time
import urllib.error
import urllib.parse
import urllib.request
from pathlib import Path
from typing import Any


REPOSITORY = "FerrumVir/arc-chain"
MODEL_URL = (
    "https://huggingface.co/TheBloke/Llama-2-7B-Chat-GGUF/resolve/"
    "191239b3e26b2882fb562ffccdd1cf0f65402adb/"
    "llama-2-7b-chat.Q4_K_M.gguf"
)
MODEL_SHA256 = "08a5566d61d7cb6b420c3e4387a39e0078e1f2fe5f055f3a03887385304d4bfa"
MODEL_SIZE = 4_081_004_224
MIN_MEMORY_BEFORE = 10 * 1024**3
MIN_MEMORY_AFTER = 1024**3
MIN_DISK_BEFORE = 10 * 1024**3
MIN_DISK_AFTER = 5 * 1024**3
COORDINATORS = (
    "https://149.28.32.76",
    "https://140.82.16.112",
    "https://136.244.109.1",
    "https://104.238.171.11",
    "https://202.182.107.41",
    "https://149.28.153.31",
)
ACCEPTANCE_COORDINATOR = "https://140.82.16.112"


def fail(message: str) -> None:
    raise RuntimeError(message)


def sha256(path: Path) -> str:
    digest = hashlib.sha256()
    with path.open("rb") as stream:
        for chunk in iter(lambda: stream.read(1024 * 1024), b""):
            digest.update(chunk)
    return digest.hexdigest()


def verify_published_assets(
    binding_path: Path, binary: Path, cli: Path, commit: str
) -> tuple[dict[str, dict[str, Any]], dict[str, Any]]:
    verifier_path = Path(__file__).with_name("published-artifact-acceptance.py")
    spec = importlib.util.spec_from_file_location("published_artifact_acceptance", verifier_path)
    if spec is None or spec.loader is None:
        fail("cannot load the existing published-asset verifier")
    verifier = importlib.util.module_from_spec(spec)
    spec.loader.exec_module(verifier)
    binding = verifier.load_object(binding_path, "exact published release binding")
    verifier.validate_binding(binding)
    if binding.get("repository") != REPOSITORY or binding.get("tag") != "v0.8.11" or binding.get("commit") != commit:
        fail("published release binding differs from requested source")
    if binary.name != "arc-node-linux-x86_64" or cli.name != "arc-cli-linux-x86_64" or binary.parent != cli.parent:
        fail("worker executables do not match the bound Linux release assets")
    verified = verifier.verify_named_files(
        binding, binary.parent, ["arc-node-linux-x86_64", "arc-cli-linux-x86_64"]
    )
    return verified, binding


def mem_available() -> int:
    for line in Path("/proc/meminfo").read_text().splitlines():
        if line.startswith("MemAvailable:"):
            return int(line.split()[1]) * 1024
    fail("cannot read MemAvailable from /proc/meminfo")


def http_json(url: str, timeout: float = 8.0) -> dict[str, Any]:
    request = urllib.request.Request(url, headers={"Accept": "application/json"})
    with urllib.request.urlopen(request, timeout=timeout) as response:
        raw = response.read(1024 * 1024 + 1)
    if len(raw) > 1024 * 1024:
        fail("HTTP status response exceeds 1 MiB")
    value = json.loads(raw)
    if not isinstance(value, dict):
        fail("HTTP response is not a JSON object")
    return value


def public_key_to_worker_id(address: str) -> str:
    if len(address) != 64 or any(char not in "0123456789abcdef" for char in address):
        fail("key tool did not return its expected 64-character public address")
    return f"0x{address}"


def require_local_port_free(port: int) -> None:
    with socket.socket(socket.AF_INET, socket.SOCK_STREAM) as probe:
        try:
            probe.bind(("127.0.0.1", port))
        except OSError as error:
            fail(f"required loopback RPC port {port} is unavailable: {error}")


def download_model(path: Path) -> None:
    digest = hashlib.sha256()
    count = 0
    request = urllib.request.Request(MODEL_URL, headers={"User-Agent": "ARC-community-worker-verification"})
    with urllib.request.urlopen(request, timeout=90) as response, path.open("xb") as output:
        while chunk := response.read(1024 * 1024):
            count += len(chunk)
            if count > MODEL_SIZE:
                fail("model download exceeds pinned size")
            digest.update(chunk)
            output.write(chunk)
            if count % (64 * 1024 * 1024) < len(chunk):
                print(json.dumps({"event": "model-download", "bytes": count}), flush=True)
        output.flush()
        os.fsync(output.fileno())
    if count != MODEL_SIZE or digest.hexdigest() != MODEL_SHA256:
        path.unlink(missing_ok=True)
        fail("downloaded model does not match the pinned SHA-256 and size")
    os.chmod(path, 0o400)


def exact_worker_visible(
    row: dict[str, Any], readiness: dict[str, Any], worker_id: str
) -> bool:
    return (
        row.get("worker_id") == worker_id
        and row.get("model_id") == readiness.get("model_id")
        and row.get("execution_profile") == readiness.get("required_community_execution_profile")
        and "inference" in row.get("capabilities", [])
    )


def poll_worker(coordinator: str, worker_id: str) -> tuple[dict[str, Any], dict[str, Any]] | None:
    try:
        query = urllib.parse.urlencode({"limit": 1, "worker_id": worker_id})
        board = http_json(f"{coordinator}/workers/scoreboard?{query}")
        workers = board.get("workers")
        row = next((w for w in workers if isinstance(w, dict) and w.get("worker_id") == worker_id), None) if isinstance(workers, list) else None
        if row is None:
            return None
        ready = http_json(f"{coordinator}/inference/readiness")
        if exact_worker_visible(row, ready, worker_id):
            return row, ready
    except (OSError, ValueError, urllib.error.URLError, RuntimeError):
        return None
    return None


def run_worker(args: argparse.Namespace) -> None:
    if platform.system() != "Linux" or platform.machine().lower() not in {"x86_64", "amd64"}:
        fail("this workflow requires a Linux x86_64 runner")
    if not 15 <= args.max_runtime_minutes <= 90:
        fail("max runtime must be 15–90 minutes")
    if mem_available() < MIN_MEMORY_BEFORE:
        fail("MemAvailable is below the 10 GiB pre-load gate")
    if shutil.disk_usage(args.work_root).free < MIN_DISK_BEFORE:
        fail("free disk is below the 10 GiB pre-download gate")
    require_local_port_free(19944)
    verified_assets, binding = verify_published_assets(
        args.binding, args.binary, args.cli, args.commit
    )
    asset_hash = verified_assets["arc-node-linux-x86_64"]["sha256"]
    cli_hash = verified_assets["arc-cli-linux-x86_64"]["sha256"]
    if not os.access(args.binary, os.X_OK):
        fail("verified Linux node asset is not executable")
    if not os.access(args.cli, os.X_OK):
        fail("verified Linux CLI asset is not executable")

    args.evidence.mkdir(parents=True, exist_ok=True, mode=0o700)
    evidence: dict[str, Any] = {
        "schema": "arc.linux.community-worker-canary.v1",
        "repository": REPOSITORY,
        "tag": "v0.8.11",
        "commit": args.commit,
        "node_sha256": asset_hash,
        "cli_sha256": cli_hash,
        "release_binding_sha256": sha256(args.binding),
        "release_id": binding["release"]["id"],
        "release_run_id": binding["release_workflow"]["run_id"],
        "release_run_attempt": binding["release_workflow"]["run_attempt"],
        "model_sha256": MODEL_SHA256,
        "model_size_bytes": MODEL_SIZE,
        "model_url": MODEL_URL,
        "runner_architecture": platform.machine(),
        "stake": 0,
        "p2p": False,
        "coordinators": list(COORDINATORS),
        "min_memavailable_before_bytes": mem_available(),
        "started_unix": int(time.time()),
        "result": "not_started",
    }
    with tempfile.TemporaryDirectory(prefix="arc-community-worker-") as temp:
        root = Path(temp)
        os.chmod(root, 0o700)
        model = root / "model.gguf"
        keyfile = root / "worker-key.json"
        data = root / "data"
        data.mkdir(mode=0o700)
        log_path = args.evidence / "worker.log"
        try:
            download_model(model)
            mem_before_load = mem_available()
            evidence["min_memavailable_before_model_load_bytes"] = mem_before_load
            if mem_before_load < MIN_MEMORY_BEFORE:
                fail("MemAvailable fell below the 10 GiB pre-load gate during model download")
            if shutil.disk_usage(root).free < MIN_DISK_AFTER:
                fail("free disk is below the 5 GiB post-model gate")
            subprocess.run(
                [str(args.cli), "keygen", "--scheme", "ed25519", "--output", str(keyfile)],
                check=True, stdout=subprocess.DEVNULL, stderr=subprocess.PIPE,
            )
            os.chmod(keyfile, 0o600)
            key_id = subprocess.run(
                [str(args.cli), "keygen", "--verify-keyfile", str(keyfile)],
                check=True, stdout=subprocess.PIPE, stderr=subprocess.PIPE, text=True,
            ).stdout.strip()
            worker_id = public_key_to_worker_id(key_id)
            evidence["keygen_argv"] = [
                str(args.cli), "keygen", "--scheme", "ed25519", "--output", str(keyfile)
            ]
            command = [
                str(args.binary), "--rpc", "127.0.0.1:19944", "--p2p-port", "0",
                "--eth-rpc-port", "0", "--stake", "0", "--community-mode",
                "--full-integer-worker", "--threads", "1", "--node-name",
                "github-linux-x86_64-community-verification", "--data-dir", str(data),
                "--model", str(model), "--validator-key-file", str(keyfile),
            ]
            for url in COORDINATORS:
                command.extend(("--community-rpc-url", url))
            evidence["node_argv"] = command
            evidence["compute_threads"] = 1
            evidence["tokio_worker_threads"] = 1
            log_tail = bytearray()
            log_lock = threading.Lock()

            def collect_log(stream: Any) -> None:
                while chunk := stream.read(64 * 1024):
                    with log_lock:
                        log_tail.extend(chunk)
                        if len(log_tail) > 2 * 1024 * 1024:
                            del log_tail[:len(log_tail) - 2 * 1024 * 1024]

            def stop_on_signal(_signum: int, _frame: Any) -> None:
                raise KeyboardInterrupt

            signal.signal(signal.SIGTERM, stop_on_signal)
            process = subprocess.Popen(
                command,
                stdout=subprocess.PIPE,
                stderr=subprocess.STDOUT,
                close_fds=True,
                env={**os.environ, "TOKIO_WORKER_THREADS": "1"},
            )
            assert process.stdout is not None
            reader = threading.Thread(target=collect_log, args=(process.stdout,), daemon=True)
            reader.start()
            try:
                evidence["pid"] = process.pid
                try:
                    deadline = time.monotonic() + 600
                    local_ready: dict[str, Any] | None = None
                    while time.monotonic() < deadline:
                        if process.poll() is not None:
                            fail(f"worker exited during startup with status {process.returncode}")
                        try:
                            local_ready = http_json("http://127.0.0.1:19944/inference/readiness", 2)
                            if local_ready.get("local_model_ready") is True:
                                break
                        except (OSError, ValueError, urllib.error.URLError, RuntimeError):
                            pass
                        time.sleep(3)
                    if not local_ready or local_ready.get("local_model_ready") is not True:
                        fail("local node did not report its complete canonical model loaded in 10 minutes")
                    available = mem_available()
                    evidence["min_memavailable_after_load_bytes"] = available
                    if available < MIN_MEMORY_AFTER:
                        fail("MemAvailable fell below 1 GiB after model load")
                    evidence["worker_id"] = worker_id
                    ready = None
                    deadline = time.monotonic() + 600
                    while time.monotonic() < deadline:
                        if process.poll() is not None:
                            fail(f"worker exited before registration with status {process.returncode}")
                        ready = poll_worker(ACCEPTANCE_COORDINATOR, worker_id)
                        if ready and ready[1].get("community_dispatch_ready") is True and ready[1].get("live_community_workers", 0) > 0:
                            break
                        time.sleep(15)
                    if (
                        not ready
                        or ready[1].get("community_dispatch_ready") is not True
                        or ready[1].get("live_community_workers", 0) <= 0
                    ):
                        fail("worker did not appear eligible at the app acceptance coordinator within 10 minutes")
                    row, readiness = ready
                    evidence["registration"] = {
                        "coordinator": ACCEPTANCE_COORDINATOR,
                        "worker_id": row.get("worker_id"),
                        "model_id": row.get("model_id"),
                        "execution_profile": row.get("execution_profile"),
                        "community_dispatch_ready": readiness.get("community_dispatch_ready"),
                        "live_community_workers": readiness.get("live_community_workers"),
                    }
                    evidence["ready_unix"] = int(time.time())
                    evidence["result"] = "eligible; awaiting external app inference; not a reward proof"
                    print("::notice::Community worker is loaded and eligible at the acceptance coordinator. Start the single packaged-app inference now; this worker stays available for the configured window.", flush=True)
                    print(json.dumps({"event": "worker-ready", **evidence["registration"]}), flush=True)
                    end = time.monotonic() + args.max_runtime_minutes * 60
                    next_health_check = time.monotonic()
                    while time.monotonic() < end:
                        if process.poll() is not None:
                            fail(f"worker exited during keepalive with status {process.returncode}")
                        if time.monotonic() >= next_health_check:
                            available = mem_available()
                            if available < MIN_MEMORY_AFTER:
                                fail("MemAvailable fell below 1 GiB during keepalive")
                            current = poll_worker(ACCEPTANCE_COORDINATOR, worker_id)
                            if not current:
                                fail("exact worker registration, model or execution profile disappeared during keepalive")
                            evidence.setdefault("health_observations", []).append({
                                "unix": int(time.time()),
                                "memavailable_bytes": available,
                                "community_dispatch_ready": current[1].get("community_dispatch_ready"),
                                "live_community_workers": current[1].get("live_community_workers"),
                                "worker_visible": True,
                            })
                            next_health_check = time.monotonic() + 60
                        time.sleep(min(15, max(1, end - time.monotonic())))
                    evidence["result"] = "worker remained available for configured window; reward not asserted"
                finally:
                    if process.poll() is None:
                        process.send_signal(signal.SIGTERM)
                        try:
                            process.wait(timeout=20)
                        except subprocess.TimeoutExpired:
                            process.kill()
                            process.wait(timeout=10)
                    evidence["exit_status"] = process.returncode
            finally:
                reader.join(timeout=10)
                with log_lock:
                    log_path.write_bytes(log_tail)
                os.chmod(log_path, 0o400)
        finally:
            evidence["stopped_unix"] = int(time.time())
            evidence["log_sha256"] = sha256(log_path) if log_path.exists() else None
            (args.evidence / "worker-evidence.json").write_text(
                json.dumps(evidence, sort_keys=True, separators=(",", ":")) + "\n",
                encoding="utf-8",
            )
            os.chmod(args.evidence / "worker-evidence.json", 0o600)


def parser() -> argparse.ArgumentParser:
    root = argparse.ArgumentParser(description=__doc__)
    sub = root.add_subparsers(dest="command", required=True)
    run = sub.add_parser("run")
    run.add_argument("--commit", required=True)
    run.add_argument("--binding", required=True, type=Path)
    run.add_argument("--binary", required=True, type=Path)
    run.add_argument("--cli", required=True, type=Path)
    run.add_argument("--work-root", required=True, type=Path)
    run.add_argument("--evidence", required=True, type=Path)
    run.add_argument("--max-runtime-minutes", type=int, required=True)
    run.set_defaults(func=run_worker)
    return root


def main() -> int:
    args = parser().parse_args()
    try:
        args.func(args)
    except (RuntimeError, OSError, subprocess.SubprocessError, ValueError) as error:
        print(f"Linux community worker canary failed: {error}", file=sys.stderr)
        return 1
    except KeyboardInterrupt:
        print("Linux community worker canary stopped after bounded cleanup", file=sys.stderr)
        return 130
    return 0


if __name__ == "__main__":
    raise SystemExit(main())
