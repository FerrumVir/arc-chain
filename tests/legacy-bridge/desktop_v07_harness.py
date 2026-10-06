#!/usr/bin/env python3
"""Faithful harness of the released v0.7.11 ARC desktop app's node lifecycle.

The v0.7.11 desktop app is a Tauri GUI. Its node handling is three small Rust
functions, reproduced here line for line (citations are to tag v0.7.11):

* `read_arc_node_version` (desktop/src-tauri/src/commands.rs:1097-1118):
  run `arc-node --version` and take the second whitespace token.
* `ensure_binary` (commands.rs:974-1075), called first by every
  `start_node`/`restart_node` (commands.rs:110, 140): if the managed binary's
  version differs from the app version "0.7.11", download
  `https://github.com/FerrumVir/arc-chain/releases/latest/download/<asset>`
  with no checksum or signature, write it to `arc-node.download`, rename it
  over `~/.arc/bin/arc-node`, and chmod 0755. Any HTTP error fails the start.
* `NodeManager::start` (node_manager.rs:96-240): pick the first free
  RPC/P2P pair in +10 steps (node_manager.rs:498-512), then spawn
  `--rpc 127.0.0.1:<p> --p2p-port <q> --data-dir ~/.arc --validator-seed
  <phrase> --eth-rpc-port 0 --seeds-file <bundled> --genesis <bundled>
  [--community-mode] [--model <m>]`, never `--stake`, with piped output and,
  on Windows, CREATE_NO_WINDOW | CREATE_NEW_PROCESS_GROUP. `stop` kills the
  direct child (node_manager.rs:317-327).

The only deliberate difference: "github.com" is a local HTTP server that plays
GitHub's `releases/latest/download` redirect, so the test can choose which
release is "Latest". `check-source` verifies those constructs still read the
same in the v0.7.11 tag before the scenario runs.
"""

from __future__ import annotations

import argparse
import hashlib
import json
import os
import platform
import shutil
import socket
import subprocess
import sys
import threading
import time
import urllib.error
import urllib.request
from http.server import BaseHTTPRequestHandler, ThreadingHTTPServer
from pathlib import Path

WINDOWS = os.name == "nt"
CREATE_NO_WINDOW = 0x08000000
CREATE_NEW_PROCESS_GROUP = 0x00000200
REPO = "FerrumVir/arc-chain"
# Not a wallet anyone uses: a fixed phrase so leaks are easy to search for.
SEED_PHRASE = "harness amber cabin echo hazard orbit planet quantum rally sunset velvet window"

HERE = Path(__file__).resolve().parent
sys.path.insert(0, str(HERE))
import snapshot_tree  # noqa: E402


def creation_flags() -> int:
    return CREATE_NO_WINDOW | CREATE_NEW_PROCESS_GROUP if WINDOWS else 0


# ---------------------------------------------------------------- v0.7.11 --


def platform_release_asset() -> str:
    """commands.rs:1120-1128"""
    machine = platform.machine().lower()
    if sys.platform == "darwin":
        return "arc-node-macos-arm64" if machine in ("arm64", "aarch64") else "arc-node-macos-x86_64"
    if WINDOWS and machine in ("amd64", "x86_64"):
        return "arc-node-windows-x86_64.exe"
    if sys.platform.startswith("linux") and machine in ("x86_64", "amd64"):
        return "arc-node-linux-x86_64"
    raise SystemExit(f"no v0.7.11 desktop asset for {sys.platform}/{machine}")


def managed_binary_path(home: Path) -> Path:
    """node_manager.rs:455-465"""
    return home / ".arc" / "bin" / ("arc-node.exe" if WINDOWS else "arc-node")


def resolve_data_dir(value: str, home: Path) -> Path:
    """node_manager.rs:483-491"""
    if value.startswith("~/"):
        return home / value[2:]
    return Path(value)


def read_arc_node_version(binary: Path) -> str | None:
    """commands.rs:1097-1118"""
    try:
        output = subprocess.run(
            [str(binary), "--version"],
            capture_output=True,
            timeout=60,
            creationflags=creation_flags() if WINDOWS else 0,
        )
    except OSError:
        return None
    if output.returncode != 0:
        return None
    tokens = output.stdout.decode("utf-8", "replace").split()
    return tokens[1] if len(tokens) > 1 else None


def ensure_binary(home: Path, github: str, desktop_version: str) -> dict[str, object]:
    """commands.rs:974-1075; `desktop_version` is the app's CARGO_PKG_VERSION."""
    target = managed_binary_path(home)
    if target.exists():
        version = read_arc_node_version(target)
        if version == desktop_version:
            return {"already_installed": True, "version": version}
    asset = platform_release_asset()
    url = f"{github}/{REPO}/releases/latest/download/{asset}"
    target.parent.mkdir(parents=True, exist_ok=True)
    try:
        with urllib.request.urlopen(url, timeout=600) as response:
            body = response.read()
    except urllib.error.HTTPError as error:
        raise RuntimeError(f"release asset {asset} returned HTTP {error.code}") from None
    temporary = target.with_suffix(".download")
    temporary.write_bytes(body)
    for attempt in range(20):
        try:
            os.replace(temporary, target)
            break
        except PermissionError:
            # Windows releases a just-killed process's image lock lazily.
            if attempt == 19:
                raise
            time.sleep(0.5)
    if not WINDOWS:
        os.chmod(target, 0o755)
    return {"already_installed": False, "downloaded_bytes": len(body), "url": url}


def port_available(port: int) -> bool:
    with socket.socket(socket.AF_INET, socket.SOCK_STREAM) as probe:
        try:
            probe.bind(("127.0.0.1", port))
        except OSError:
            return False
    return True


def choose_port_pair(rpc: int, p2p: int) -> tuple[int, int]:
    """node_manager.rs:498-512"""
    for index in range(5):
        if port_available(rpc + index * 10) and port_available(p2p + index * 10):
            return rpc + index * 10, p2p + index * 10
    raise RuntimeError("ports and 5 fallbacks all busy")


class DesktopNode:
    """node_manager.rs:96-240 (start) and 317-327 (stop)."""

    def __init__(self, home: Path, resources: dict[str, Path], log_path: Path) -> None:
        self.home = home
        self.resources = resources
        self.log_path = log_path
        self.process: subprocess.Popen[bytes] | None = None
        self.rpc_port = 0

    def start(self, config: dict[str, object]) -> list[str]:
        binary = managed_binary_path(self.home)
        data_dir = resolve_data_dir(str(config["data_dir"]), self.home)
        data_dir.mkdir(parents=True, exist_ok=True)
        rpc_port, p2p_port = choose_port_pair(int(config["rpc_port"]), int(config["p2p_port"]))
        args = [
            str(binary),
            "--rpc",
            f"127.0.0.1:{rpc_port}",
            "--p2p-port",
            str(p2p_port),
            "--data-dir",
            str(data_dir),
            "--validator-seed",
            SEED_PHRASE,
            "--eth-rpc-port",
            "0",
            "--seeds-file",
            str(self.resources["seeds"]),
            "--genesis",
            str(self.resources["genesis"]),
        ]
        if config["role"] == "worker" and config.get("model_path"):
            args.append("--community-mode")
        if config.get("model_path"):
            args += ["--model", str(config["model_path"])]
        environment = dict(os.environ, HOME=str(self.home))
        log = self.log_path.open("ab")
        self.process = subprocess.Popen(
            args,
            stdout=log,
            stderr=subprocess.STDOUT,
            env=environment,
            creationflags=creation_flags(),
        )
        self.rpc_port = rpc_port
        return args

    def stop(self) -> None:
        if self.process is not None:
            self.process.kill()
            self.process.wait(timeout=60)
            self.process = None


def start_node(node: DesktopNode, github: str, desktop_version: str, config: dict[str, object]) -> list[str]:
    """commands.rs:98-127: ensure_binary(...)? then NodeManager::start."""
    ensure_binary(node.home, github, desktop_version)
    return node.start(config)


# -------------------------------------------------------------- fixtures --


class FakeGitHub(ThreadingHTTPServer):
    """GitHub's release download and latest-redirect routes, from memory."""

    def __init__(self) -> None:
        super().__init__(("127.0.0.1", 0), Handler)
        self.releases: dict[str, dict[str, bytes]] = {}
        self.latest = ""
        self.requests: list[str] = []

    @property
    def base(self) -> str:
        return f"http://127.0.0.1:{self.server_address[1]}"


class Handler(BaseHTTPRequestHandler):
    def do_GET(self) -> None:  # noqa: N802 - http.server API
        server: FakeGitHub = self.server  # type: ignore[assignment]
        server.requests.append(self.path)
        parts = self.path.strip("/").split("/")
        body = None
        if parts[:4] == ["FerrumVir", "arc-chain", "releases", "latest"] and len(parts) == 6:
            body = server.releases.get(server.latest, {}).get(parts[5])
        elif parts[:4] == ["FerrumVir", "arc-chain", "releases", "download"] and len(parts) == 6:
            body = server.releases.get(parts[4], {}).get(parts[5])
        if body is None:
            self.send_response(404)
            self.send_header("Content-Length", "0")
            self.end_headers()
            return
        self.send_response(200)
        self.send_header("Content-Type", "application/octet-stream")
        self.send_header("Content-Length", str(len(body)))
        self.end_headers()
        self.wfile.write(body)

    def log_message(self, *_args: object) -> None:
        return


def sha256(path: Path) -> str:
    return snapshot_tree.sha256_file(path)


def http_json(port: int, path: str) -> dict[str, object]:
    with urllib.request.urlopen(f"http://127.0.0.1:{port}{path}", timeout=5) as response:
        return json.loads(response.read().decode("utf-8"))


def wait_health(port: int, seconds: int) -> dict[str, object]:
    deadline = time.time() + seconds
    while time.time() < deadline:
        try:
            return http_json(port, "/health")
        except (OSError, ValueError):
            time.sleep(1)
    raise AssertionError(f"nothing answered /health on 127.0.0.1:{port} within {seconds}s")


def process_tree(pid: int) -> list[dict[str, object]]:
    """The process and its children: id, executable path, and command line."""
    if WINDOWS:
        script = (
            f"Get-CimInstance Win32_Process | Where-Object {{ $_.ProcessId -eq {pid} -or $_.ParentProcessId -eq {pid} }} "
            "| Select-Object ProcessId,ParentProcessId,ExecutablePath,CommandLine | ConvertTo-Json -Compress"
        )
        raw = subprocess.run(
            ["powershell", "-NoProfile", "-Command", script], capture_output=True, text=True, check=True
        ).stdout.strip()
        rows = json.loads(raw) if raw else []
        rows = rows if isinstance(rows, list) else [rows]
        return [
            {"pid": row["ProcessId"], "exe": row["ExecutablePath"], "cmdline": row["CommandLine"] or ""}
            for row in rows
        ]
    exe = os.readlink(f"/proc/{pid}/exe")
    cmdline = Path(f"/proc/{pid}/cmdline").read_bytes().replace(b"\0", b" ").decode("utf-8", "replace")
    return [{"pid": pid, "exe": exe, "cmdline": cmdline}]


def node_process(pid: int, node_binary: Path) -> dict[str, object]:
    for row in process_tree(pid):
        if Path(str(row["exe"])).resolve() == node_binary.resolve():
            return row
    raise AssertionError(f"the pinned node {node_binary} is not running under pid {pid}: {process_tree(pid)}")


def pid_alive(pid: int) -> bool:
    if WINDOWS:
        out = subprocess.run(
            ["powershell", "-NoProfile", "-Command", f"Get-Process -Id {pid} -ErrorAction SilentlyContinue"],
            capture_output=True,
            text=True,
        ).stdout
        return bool(out.strip())
    return Path(f"/proc/{pid}").exists()


def assert_no_seed(paths: list[Path], extra: str) -> None:
    needle = SEED_PHRASE.encode("utf-8")
    assert SEED_PHRASE not in extra, "the seed phrase reached the bridged node's command line"
    for root in paths:
        for path in [root] if root.is_file() else root.rglob("*"):
            if path.is_file() and needle in path.read_bytes():
                raise AssertionError(f"the seed phrase leaked into {path}")


# -------------------------------------------------------------- scenario --


def run(args: argparse.Namespace) -> None:
    evidence: Path = args.evidence.resolve()
    evidence.mkdir(parents=True, exist_ok=True)
    pins = json.loads(args.pins.read_text(encoding="utf-8"))
    asset = platform_release_asset()
    node_tag = pins["node_release"]["tag"]
    node_asset = {
        "arc-node-linux-x86_64": "arc-node-linux-x86_64",
        "arc-node-windows-x86_64.exe": "arc-node-windows-x86_64.exe",
        "arc-node-macos-arm64": "arc-node-macos-arm64",
        "arc-node-macos-x86_64": "arc-node-macos-x86_64",
    }[asset]
    pinned_node_sha = pins["node_release"]["assets"][node_asset]["sha256"]
    report: dict[str, object] = {"platform": asset, "node_release": node_tag}

    home = (evidence / "home").resolve()
    shutil.rmtree(home, ignore_errors=True)
    arc = home / ".arc"
    (arc / "bin").mkdir(parents=True)
    resources = {"seeds": args.legacy_seeds.resolve(), "genesis": args.legacy_genesis.resolve()}
    github = FakeGitHub()
    threading.Thread(target=github.serve_forever, daemon=True).start()
    github.releases["v0.7.7"] = {asset: args.legacy_node.read_bytes()}
    github.releases["v0.7.11"] = {"latest.json": b"{}"}  # desktop-only, like the real one
    github.releases["v0.7.12"] = {asset: args.bridge.read_bytes()}
    config = {"rpc_port": 9944, "p2p_port": 9945, "data_dir": "~/.arc", "role": "worker", "model_path": None}
    node = DesktopNode(home, resources, evidence / "desktop-node.log")

    print("== v0.7.7 era: the app and its node agree; the node writes real v0.7 state")
    shutil.copyfile(args.legacy_node, managed_binary_path(home))
    if not WINDOWS:
        os.chmod(managed_binary_path(home), 0o755)
    github.latest = "v0.7.7"
    start_node(node, github.base, "0.7.7", config)
    wait_health(node.rpc_port, 90)
    time.sleep(10)
    node.stop()
    v07_files = sorted(p.name for p in arc.iterdir())
    report["v07_data_entries"] = v07_files
    assert (arc / "state.wal").is_file(), f"v0.7.7 wrote no state WAL: {v07_files}"

    print("== today: a v0.7.11 app cannot start its node because Latest has no arc-node asset")
    github.latest = "v0.7.11"
    try:
        start_node(node, github.base, "0.7.11", config)
        raise AssertionError("a v0.7.11 desktop started a node from a desktop-only Latest")
    except RuntimeError as error:
        report["stranded_error"] = str(error)
        assert "returned HTTP 404" in str(error)
    snapshot_tree.write_json(evidence / "v07-data.before.json", {"entries": snapshot_tree.snapshot(arc, True)})

    print("== the bridge is Latest: the next start installs it and the node joins as v0.8 stake 0")
    github.latest = "v0.7.12"
    started = time.time()
    argv = start_node(node, github.base, "0.7.11", config)
    report["spawn_argv"] = [arg if arg != SEED_PHRASE else "<seed phrase>" for arg in argv]
    assert "--stake" not in argv, "the v0.7 desktop never passed --stake"
    health = wait_health(node.rpc_port, 240)
    report["first_start_seconds"] = round(time.time() - started, 1)
    info = http_json(node.rpc_port, "/node/info")
    report["health"], report["node_info"] = health, info
    assert info["stake"] == 0, info
    assert info["version"] == pins["node_release"]["version"], info
    assert health["chain_participation_enabled"] is False, health
    bridge_root = arc / "legacy-bridge"
    node_binary = bridge_root / "releases" / node_tag / node_asset
    assert sha256(node_binary) == pinned_node_sha
    running = node_process(node.process.pid, node_binary)
    report["node_process"] = {k: v for k, v in running.items() if k != "cmdline"}
    cmdline = str(running["cmdline"])
    for required in ("--stake 0", "--min-stake 0", "--no-community"):
        assert required in cmdline, f"{required} missing from {cmdline}"
    for forbidden in ("--validator-seed", "--model", "--shard-range", "--insecure-dev-validator-seed"):
        assert forbidden not in cmdline, f"{forbidden} in {cmdline}"
    states = list(bridge_root.glob("nodes/desktop-*/bridge-state.json"))
    assert len(states) == 1, states
    state = json.loads(states[0].read_text(encoding="utf-8"))
    report["bridge_state"] = state
    assert state["legacy_kind"] == "desktop" and state["stake"] == 0
    assert state["compute"].startswith("off: the updated ARC desktop app asks")
    assert (states[0].parent / "data" / "genesis.network-hash").is_file()
    assert not (arc / "genesis.network-hash").exists(), "v0.8 initialized the v0.7 data directory"
    log_text = (evidence / "desktop-node.log").read_text(encoding="utf-8", errors="replace")
    assert "Your ARC node is upgrading to the new network" in log_text
    assert "Settings > Check for updates > Install" in log_text
    snapshot_tree.write_json(evidence / "v07-data.after.json", {"entries": snapshot_tree.snapshot(arc, True)})
    before = json.loads((evidence / "v07-data.before.json").read_text(encoding="utf-8"))["entries"]
    after = json.loads((evidence / "v07-data.after.json").read_text(encoding="utf-8"))["entries"]
    assert before == after and "state.wal" in before, "the v0.7 data changed"
    assert_no_seed([bridge_root], cmdline)

    print("== stop and start again: the launcher is fetched again, the verified cache is reused")
    node_pid = int(running["pid"])
    address = state["node_address"]
    node.stop()
    for _ in range(30):
        if not pid_alive(node_pid):
            break
        time.sleep(1)
    assert not pid_alive(node_pid), "stopping the desktop's child left the v0.8 node running"
    start_node(node, github.base, "0.7.11", config)
    wait_health(node.rpc_port, 120)
    state = json.loads(states[0].read_text(encoding="utf-8"))
    assert state["node_address"] == address, "a restart changed the identity"
    bridge_log = (bridge_root / "bridge.log").read_text(encoding="utf-8")
    assert f"reusing the verified {node_tag} release cache" in bridge_log
    after_restart = snapshot_tree.snapshot(arc, True)
    assert after_restart == before, "the v0.7 data changed across a restart"
    node.stop()
    report["fake_github_requests"] = github.requests
    (evidence / "desktop-report.json").write_text(json.dumps(report, indent=2) + "\n", encoding="utf-8")
    github.shutdown()
    print(f"PASS: v0.7.11 desktop on {asset} bridged to {node_tag} at stake 0; v0.7 data untouched")


def check_source(repo: Path) -> None:
    """The constructs this harness reproduces, read back from tag v0.7.11."""

    def show(path: str) -> str:
        return subprocess.run(
            ["git", "-C", str(repo), "show", f"v0.7.11:{path}"], capture_output=True, text=True, check=True
        ).stdout

    commands = show("desktop/src-tauri/src/commands.rs")
    manager = show("desktop/src-tauri/src/node_manager.rs")
    conf = json.loads(show("desktop/src-tauri/tauri.conf.json"))
    expected = {
        commands: [
            "ensure_binary(app.clone()).await?;",
            "Some(ref v) if v == EXPECTED_NODE_VERSION",
            '"https://github.com/FerrumVir/arc-chain/releases/latest/download/{}"',
            'let tmp = target.with_extension("download");',
            "std::fs::rename(&tmp, &target)",
            "stdout.split_whitespace().nth(1)",
            '("linux", "x86_64") => Some("arc-node-linux-x86_64")',
            '("windows", "x86_64") => Some("arc-node-windows-x86_64.exe")',
        ],
        manager: [
            '.arg(format!("127.0.0.1:{}", rpc_port))',
            '.arg("--data-dir")',
            '.arg("--validator-seed")',
            '.arg("--eth-rpc-port")',
            'cmd.arg("--seeds-file").arg(seeds);',
            'cmd.arg("--genesis").arg(genesis);',
            'if config.role == "worker" && config.model_path.is_some() {',
            "let rpc = preferred_rpc + (i * 10);",
            "let _ = child.kill().await;",
        ],
    }
    for source, needles in expected.items():
        for needle in needles:
            assert needle in source, f"v0.7.11 no longer contains {needle!r}"
    assert "--stake" not in manager, "the v0.7.11 desktop passes --stake after all"
    assert conf["version"] == "0.7.11"
    assert conf["plugins"]["updater"]["endpoints"] == [
        "https://github.com/FerrumVir/arc-chain/releases/latest/download/latest.json"
    ]
    print("v0.7.11 desktop source matches every construct this harness reproduces")


def main() -> int:
    parser = argparse.ArgumentParser(description=__doc__, formatter_class=argparse.RawDescriptionHelpFormatter)
    sub = parser.add_subparsers(dest="command", required=True)
    source = sub.add_parser("check-source")
    source.add_argument("--repo", type=Path, required=True)
    scenario = sub.add_parser("run")
    scenario.add_argument("--bridge", type=Path, required=True)
    scenario.add_argument("--legacy-node", type=Path, required=True)
    scenario.add_argument("--legacy-seeds", type=Path, required=True)
    scenario.add_argument("--legacy-genesis", type=Path, required=True)
    scenario.add_argument("--pins", type=Path, required=True)
    scenario.add_argument("--evidence", type=Path, required=True)
    args = parser.parse_args()
    if args.command == "check-source":
        check_source(args.repo)
    else:
        run(args)
    return 0


if __name__ == "__main__":
    sys.exit(main())
