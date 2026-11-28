import ast
import errno
import hashlib
import importlib.util
import json
import os
from pathlib import Path
import re
import select
import signal
import socket
import subprocess
import sys
import tempfile
import threading
import time
from types import SimpleNamespace
import unittest
from unittest import mock


REPO_ROOT = Path(__file__).resolve().parents[3]
SOURCE = Path(__file__).resolve().parents[1] / "qualify_worker_startup.py"
SPEC = importlib.util.spec_from_file_location("qualify_worker_startup", SOURCE)
assert SPEC is not None and SPEC.loader is not None
QUALIFY = importlib.util.module_from_spec(SPEC)
sys.modules[SPEC.name] = QUALIFY
SPEC.loader.exec_module(QUALIFY)


def write_synthetic_repo(root: Path, overrides: dict[str, object] | None = None) -> None:
    cfg = overrides or {}
    (root / "docs" / "protocol" / "packages").mkdir(parents=True, exist_ok=True)
    (root / "crates" / "arc-legacy-bridge" / "pins").mkdir(parents=True, exist_ok=True)

    version = str(cfg.get("version", "0.8.28"))
    (root / "Cargo.toml").write_text(
        f'[workspace]\nmembers = []\n\n[workspace.package]\nversion = "{version}"\n',
        encoding="utf-8",
    )

    manifest_bytes = int(cfg.get("manifest_bytes", 128))
    pin_size = int(cfg.get("pin_size", manifest_bytes))
    manifest_sha256 = str(cfg.get("manifest_sha256", "11" * 32))
    pin_sha256 = str(cfg.get("pin_sha256", manifest_sha256))
    manifest_blake3 = str(cfg.get("manifest_blake3", "22" * 32))
    file_name = str(cfg.get("file_name", "fixture-model.gguf"))
    model_url = str(cfg.get("model_url", f"https://example.invalid/models/{file_name}"))

    (root / "docs" / "protocol" / "packages" / "llama-2-7b-q4km.manifest.json").write_text(
        json.dumps(
            {
                "artifact": {
                    "bytes": manifest_bytes,
                    "sha256": manifest_sha256,
                    "blake3": manifest_blake3,
                }
            }
        ),
        encoding="utf-8",
    )

    origins = list(
        cfg.get(
            "origins",
            [
                "https://198.51.100.1",
                "https://198.51.100.2",
                "https://198.51.100.3",
                "https://198.51.100.4",
                "https://198.51.100.5",
                "https://198.51.100.6",
            ],
        )
    )
    (root / "crates" / "arc-legacy-bridge" / "pins" / "active.json").write_text(
        json.dumps(
            {
                "community_rpc_origins": origins,
                "model": {
                    "file_name": file_name,
                    "size": pin_size,
                    "sha256": pin_sha256,
                    "url": model_url,
                },
            }
        ),
        encoding="utf-8",
    )
    (root / "genesis.toml").write_text(
        str(cfg.get("genesis", '[chain]\nname = "arc-testnet"\n')),
        encoding="utf-8",
    )
    (root / "testnet-seeds.txt").write_text(
        str(
            cfg.get(
                "seeds",
                "\n".join(f"198.51.100.{idx}:9091" for idx in range(1, 7)) + "\n",
            )
        ),
        encoding="utf-8",
    )
    if "main_rs" in cfg:
        main_rs_path = root / "crates" / "arc-node" / "src" / "main.rs"
        main_rs_path.parent.mkdir(parents=True, exist_ok=True)
        main_rs_path.write_text(str(cfg["main_rs"]), encoding="utf-8")


def make_http_result(
    status: int,
    headers: dict[str, str],
    body: object,
) -> object:
    raw = json.dumps(body).encode("utf-8") if body is not None else b""
    lowered = {k.lower(): v for k, v in headers.items()}
    return QUALIFY.HttpResult(status=status, headers=lowered, raw_body=raw, json_body=body)


def valid_loading_health_body(
    version: str = "0.8.28",
    binary_sha256: str | None = None,
) -> dict[str, object]:
    return {
        "status": "loading",
        "version": version,
        "height": 0,
        "peers": 0,
        "chain_participation_enabled": False,
        "degraded_reason": "worker (full_integer_worker) is loading its canonical model",
        "startup_phase": "loading_model",
        "binary_sha256": binary_sha256,
        "features": ["candle"],
    }


def valid_readiness_body(ready: bool, model_blake3: str = "22" * 32) -> dict[str, object]:
    return {
        "schema": "arc.inference.readiness.v1",
        "safe_to_dispatch": ready,
        "community_dispatch_ready": False,
        "local_model_ready": ready,
        "sharded_pipeline_ready": False,
        "live_community_workers": 0,
        "model_id": f"0x{model_blake3}" if ready else None,
        "required_community_execution_profile": QUALIFY.CANONICAL_EXECUTION_PROFILE,
        "mutation_free_observation": True,
    }


def make_signed_register_envelope(
    coordinator_hex: str,
    worker_addr_hex: str,
    model_blake3: str,
) -> dict[str, object]:
    return {
        "version": 2,
        "target_coordinator": coordinator_hex,
        "transaction_domain": None,
        "timestamp_unix_ms": 1700000000000,
        "nonce": "89" * 32,
        "payload_hash": "9a" * 32,
        "signature": {"Ed25519": {"public_key": [1] * 32, "signature": [2] * 64}},
        "payload": {
            "worker_id": f"0x{worker_addr_hex}",
            "name": "node-45454545",
            "capabilities": ["inference"],
            "model": "arc-32L-4096d-32h-32000v",
            "model_id": f"0x{model_blake3}",
            "execution_profile": QUALIFY.CANONICAL_EXECUTION_PROFILE,
            "platform": "linux-x86_64",
        },
    }


def send_raw_http(port: int, payload: bytes, timeout_secs: float = 2.0) -> bytes:
    with socket.create_connection(("127.0.0.1", port), timeout=timeout_secs) as sock:
        sock.settimeout(timeout_secs)
        sock.sendall(payload)
        chunks = bytearray()
        while True:
            try:
                part = sock.recv(4096)
            except (socket.timeout, TimeoutError, ConnectionResetError):
                break
            if not part:
                break
            chunks.extend(part)
        return bytes(chunks)


class WorkerStartupQualificationTests(unittest.TestCase):
    @unittest.skipUnless(sys.platform.startswith("linux"), "Linux runner TCP semantics")
    def test_released_port_accepts_time_wait_and_rejects_live_listeners(self) -> None:
        for address in ("127.0.0.1", "0.0.0.0"):
            with socket.socket(socket.AF_INET, socket.SOCK_STREAM) as listener:
                listener.setsockopt(socket.SOL_SOCKET, socket.SO_REUSEADDR, 1)
                listener.bind((address, 0))
                listener.listen(1)
                port = listener.getsockname()[1]
                with self.assertRaisesRegex(QUALIFY.QualificationError, "remained bound"):
                    QUALIFY.verify_loopback_port_released(port, timeout_secs=0.05)
            QUALIFY.verify_loopback_port_released(port, timeout_secs=0.2)

        with socket.socket(socket.AF_INET, socket.SOCK_STREAM) as listener:
            listener.setsockopt(socket.SOL_SOCKET, socket.SO_REUSEADDR, 1)
            listener.settimeout(2.0)
            listener.bind(("127.0.0.1", 0))
            listener.listen(1)
            port = listener.getsockname()[1]
            with socket.create_connection(("127.0.0.1", port), timeout=2.0) as client:
                connection, _ = listener.accept()
                with connection:
                    connection.settimeout(2.0)
                    connection.shutdown(socket.SHUT_WR)
                    self.assertEqual(client.recv(1), b"")
                    client.shutdown(socket.SHUT_WR)
                    self.assertEqual(connection.recv(1), b"")

        with socket.socket(socket.AF_INET, socket.SOCK_STREAM) as probe:
            with self.assertRaises(OSError) as raised:
                probe.bind(("127.0.0.1", port))
            self.assertEqual(raised.exception.errno, errno.EADDRINUSE)
        QUALIFY.verify_loopback_port_released(port, timeout_secs=0.2)

    def test_log_head_and_tail_are_independent_of_read_chunk_boundaries(self) -> None:
        payload = b"HEAD-LINE\n" + bytes(range(256)) * 8 + b"\nTAIL-LINE\n"
        budget = 256 - len(QUALIFY.TRUNCATION_MARKER)
        head_bytes = budget // 2
        expected = payload[:head_bytes] + QUALIFY.TRUNCATION_MARKER + payload[-(budget - head_bytes):]
        for chunk_bytes in (1, 127, 256, 257, 1024, len(payload)):
            with self.subTest(chunk_bytes=chunk_bytes):
                reader, writer = os.pipe()
                capture = QUALIFY.BoundedPipeLogCapture(
                    os.fdopen(reader, "rb", buffering=0),
                    phase_name="chunk-invariant",
                    max_retained_bytes=256,
                    max_total_bytes=8192,
                )
                try:
                    with capture._lock:
                        for start in range(0, len(payload), chunk_bytes):
                            capture._ingest_chunk_locked(payload[start:start + chunk_bytes])
                    capture.close(join_timeout_secs=2.0)
                    with tempfile.TemporaryDirectory() as tmp:
                        output = Path(tmp) / "retained.log"
                        capture.write_retained_log(output)
                        self.assertEqual(output.read_bytes(), expected)
                finally:
                    os.close(writer)
                    capture.close(join_timeout_secs=2.0)

    def test_broad_exception_catch_is_confined_to_main_failure_reporting(self) -> None:
        tree = ast.parse(SOURCE.read_text(encoding="utf-8"))
        main = next(node for node in tree.body if isinstance(node, ast.FunctionDef) and node.name == "main")
        broad_handlers = [
            node
            for node in ast.walk(tree)
            if isinstance(node, ast.ExceptHandler)
            and (
                node.type is None
                or any(
                    isinstance(error_type, ast.Name) and error_type.id in ("Exception", "BaseException")
                    for error_type in ast.walk(node.type)
                )
            )
        ]
        self.assertEqual(
            len(broad_handlers),
            1,
            "Catch expected failures in cleanup; reserve the single broad catch for main failure reporting",
        )
        self.assertIn(broad_handlers[0], list(ast.walk(main)))

    def test_load_repo_pins_reads_checked_in_repo_fixtures_and_binds_source_markers(self) -> None:
        pins = QUALIFY.load_repo_pins(REPO_ROOT)
        self.assertEqual(
            pins.candidate_version,
            QUALIFY.parse_workspace_version(REPO_ROOT / "Cargo.toml"),
        )
        self.assertEqual(pins.model_size, 4081004224)
        self.assertEqual(
            pins.model_sha256,
            "08a5566d61d7cb6b420c3e4387a39e0078e1f2fe5f055f3a03887385304d4bfa",
        )
        self.assertEqual(
            pins.model_blake3,
            "934efc12a2ed8372a944e5aaedf059a8a0f42c0906f6b2f1fb3626bdeb1ffa67",
        )
        self.assertEqual(pins.model_file_name, "llama-2-7b-chat.Q4_K_M.gguf")
        self.assertEqual(len(pins.production_ipv4s), 6)
        self.assertEqual(
            set(pins.production_ipv4s),
            {
                "149.28.32.76",
                "140.82.16.112",
                "136.244.109.1",
                "104.238.171.11",
                "202.182.107.41",
                "149.28.153.31",
            },
        )
        self.assertEqual(
            pins.canonical_execution_profile,
            "INT8 integer (per-row, cross-platform deterministic)",
        )

    def test_load_repo_pins_rejects_mismatched_size_sha_extra_seed_or_drifted_source_marker(
        self,
    ) -> None:
        with tempfile.TemporaryDirectory() as tmp:
            root = Path(tmp) / "ok"
            write_synthetic_repo(root)
            pins = QUALIFY.load_repo_pins(root)
            self.assertEqual(pins.candidate_version, "0.8.28")
            self.assertEqual(pins.model_size, 128)

            bad_size = Path(tmp) / "bad_size"
            write_synthetic_repo(bad_size, {"manifest_bytes": 128, "pin_size": 256})
            with self.assertRaisesRegex(QUALIFY.QualificationError, "model size mismatch"):
                QUALIFY.load_repo_pins(bad_size)

            bad_sha = Path(tmp) / "bad_sha"
            write_synthetic_repo(
                bad_sha, {"manifest_sha256": "11" * 32, "pin_sha256": "33" * 32}
            )
            with self.assertRaisesRegex(QUALIFY.QualificationError, "model sha256 mismatch"):
                QUALIFY.load_repo_pins(bad_sha)

            extra_ip = Path(tmp) / "extra_ip"
            write_synthetic_repo(
                extra_ip,
                {"seeds": "198.51.100.1:9091\n203.0.113.99:9091\n"},
            )
            with self.assertRaisesRegex(QUALIFY.QualificationError, "absent from pins"):
                QUALIFY.load_repo_pins(extra_ip)

            drifted_main = Path(tmp) / "drifted_main"
            write_synthetic_repo(
                drifted_main,
                {"main_rs": 'fn main() { println!("altered log line"); }\n'},
            )
            with self.assertRaisesRegex(
                QUALIFY.QualificationError, "missing bound qualification log literal"
            ):
                QUALIFY.load_repo_pins(drifted_main)

    def test_address_normalization_enforces_exact_wire_formats(self) -> None:
        bare = "ab" * 32
        prefixed = f"0x{bare}"
        self.assertEqual(QUALIFY.normalize_bare_hex64(bare, "bare"), bare)
        self.assertEqual(QUALIFY.normalize_prefixed_hex64(prefixed, "prefixed"), bare)

        for bad_bare in (prefixed, bare.upper(), "ab" * 31, None, 123):
            with self.assertRaisesRegex(QUALIFY.QualificationError, "without 0x prefix"):
                QUALIFY.normalize_bare_hex64(bad_bare, "bare")

        for bad_prefixed in (bare, f"0x{bare.upper()}", f"0X{bare}", "0x1234", None):
            with self.assertRaisesRegex(QUALIFY.QualificationError, "0x-prefixed"):
                QUALIFY.normalize_prefixed_hex64(bad_prefixed, "prefixed")

    def test_validate_caller_environment_rejects_inherited_arc_vars(self) -> None:
        for var_name in (
            "ARC_COMMUNITY_RPC_URLS",
            "ARC_PUBLIC_SOCKET",
            "ARC_VALIDATOR_KEY_FILE",
            "ARC_MODEL_URL",
            "arc_custom_override",
        ):
            with self.assertRaisesRegex(QUALIFY.QualificationError, "inherited ARC_"):
                QUALIFY.validate_caller_environment({"PATH": "/usr/bin", var_name: "1"})

        with tempfile.TemporaryDirectory() as tmp:
            isolated_home = Path(tmp) / "home"
            isolated_tmp = Path(tmp) / "tmp"
            child_env = QUALIFY.build_isolated_child_env(
                {
                    "PATH": "/usr/bin",
                    "HTTP_PROXY": "http://10.0.0.1:8080",
                    "HTTPS_PROXY": "http://10.0.0.1:8080",
                    "HOME": "/root",
                },
                isolated_home=isolated_home,
                isolated_tmp=isolated_tmp,
            )
            self.assertEqual(child_env["HOME"], str(isolated_home))
            self.assertEqual(child_env["TMPDIR"], str(isolated_tmp))
            self.assertEqual(child_env["RAYON_NUM_THREADS"], "1")
            self.assertEqual(child_env["NO_PROXY"], "127.0.0.1,localhost")
            self.assertNotIn("HTTP_PROXY", child_env)
            self.assertNotIn("HTTPS_PROXY", child_env)

    def test_resource_floor_accepts_nominal_16gb_runner_and_rejects_below_floor(self) -> None:
        gib = QUALIFY.GIB_BYTES
        with tempfile.TemporaryDirectory() as tmp:
            base = Path(tmp)
            meminfo_16g = base / "meminfo_16g"
            meminfo_16g.write_text(
                f"MemTotal:       {int(15.6 * 1024 * 1024)} kB\n"
                f"MemAvailable:   {int(13.0 * 1024 * 1024)} kB\n",
                encoding="utf-8",
            )
            cgroup_unlimited = base / "cg_unlimited"
            cgroup_unlimited.mkdir()
            (cgroup_unlimited / "memory.max").write_text("max\n", encoding="utf-8")
            (cgroup_unlimited / "memory.current").write_text(f"{2 * gib}\n", encoding="utf-8")

            ok_snapshot = QUALIFY.measure_runner_resources(
                disk_path=base,
                meminfo_path=meminfo_16g,
                cgroup_dir=cgroup_unlimited,
                disk_usage_fn=lambda _: SimpleNamespace(total=40 * gib, free=14 * gib),
            )
            self.assertEqual(ok_snapshot.nominal_ram_gb, 16)
            QUALIFY.enforce_runner_resource_floor(ok_snapshot)

            low_disk = QUALIFY.measure_runner_resources(
                disk_path=base,
                meminfo_path=meminfo_16g,
                cgroup_dir=cgroup_unlimited,
                disk_usage_fn=lambda _: SimpleNamespace(total=40 * gib, free=9 * gib),
            )
            with self.assertRaisesRegex(QUALIFY.QualificationError, "free disk space"):
                QUALIFY.enforce_runner_resource_floor(low_disk)

            meminfo_12g = base / "meminfo_12g"
            meminfo_12g.write_text(
                f"MemTotal:       {12 * 1024 * 1024} kB\n"
                f"MemAvailable:   {12 * 1024 * 1024} kB\n",
                encoding="utf-8",
            )
            low_nominal = QUALIFY.measure_runner_resources(
                disk_path=base,
                meminfo_path=meminfo_12g,
                cgroup_dir=cgroup_unlimited,
                disk_usage_fn=lambda _: SimpleNamespace(total=40 * gib, free=20 * gib),
            )
            self.assertEqual(low_nominal.nominal_ram_gb, 12)
            with self.assertRaisesRegex(QUALIFY.QualificationError, "nominal RAM"):
                QUALIFY.enforce_runner_resource_floor(low_nominal)

            cgroup_tight = base / "cg_tight"
            cgroup_tight.mkdir()
            (cgroup_tight / "memory.max").write_text(f"{16 * gib}\n", encoding="utf-8")
            (cgroup_tight / "memory.current").write_text(f"{7 * gib}\n", encoding="utf-8")
            (cgroup_tight / "memory.stat").write_text(
                f"inactive_file {1 * gib}\n", encoding="utf-8"
            )
            capped_snapshot = QUALIFY.measure_runner_resources(
                disk_path=base,
                meminfo_path=meminfo_16g,
                cgroup_dir=cgroup_tight,
                disk_usage_fn=lambda _: SimpleNamespace(total=40 * gib, free=20 * gib),
            )
            self.assertEqual(capped_snapshot.effective_available_bytes, 10 * gib)
            with self.assertRaisesRegex(QUALIFY.QualificationError, "effective available RAM"):
                QUALIFY.enforce_runner_resource_floor(capped_snapshot)

    def test_verify_model_artifact_rejects_symlink_wrong_size_and_wrong_sha(self) -> None:
        with tempfile.TemporaryDirectory() as tmp:
            root = Path(tmp)
            model_bytes = b"arc-qualification-fixture-gguf-bytes"
            model_sha = hashlib.sha256(model_bytes).hexdigest()
            model_file = root / "fixture.gguf"
            model_file.write_bytes(model_bytes)

            pins = QUALIFY.RepoPins(
                candidate_version="0.8.28",
                model_file_name="fixture.gguf",
                model_size=len(model_bytes),
                model_sha256=model_sha,
                model_blake3="22" * 32,
                model_url="https://example.invalid/fixture.gguf",
                production_ipv4s=("198.51.100.1",),
                canonical_execution_profile=QUALIFY.CANONICAL_EXECUTION_PROFILE,
            )
            verified = QUALIFY.verify_model_artifact(model_file, pins)
            self.assertEqual(verified["sha256"], model_sha)
            self.assertEqual(verified["bytes"], len(model_bytes))

            symlink_path = root / "link.gguf"
            symlink_path.symlink_to(model_file)
            with self.assertRaisesRegex(QUALIFY.QualificationError, "must not be a symlink"):
                QUALIFY.verify_model_artifact(symlink_path, pins)

            wrong_size = root / "short.gguf"
            wrong_size.write_bytes(b"short")
            with self.assertRaisesRegex(QUALIFY.QualificationError, "byte size mismatch"):
                QUALIFY.verify_model_artifact(wrong_size, pins)

            wrong_sha = root / "corrupt.gguf"
            wrong_sha.write_bytes(b"X" * len(model_bytes))
            with self.assertRaisesRegex(QUALIFY.QualificationError, "sha256 mismatch"):
                QUALIFY.verify_model_artifact(wrong_sha, pins)

    def test_verify_production_ipv4_blocked_fails_if_any_ip_connects(self) -> None:
        ips = ("198.51.100.1", "198.51.100.2")

        def always_refuse(_target: tuple[str, int], _timeout: float) -> None:
            raise OSError(errno.ECONNREFUSED, "refused")

        checks = QUALIFY.verify_production_ipv4_blocked(ips, connect_fn=always_refuse)
        self.assertEqual(len(checks), 2)
        self.assertTrue(all(item["blocked"] is True for item in checks))
        self.assertEqual(checks[0]["error"], "ECONNREFUSED")

        def second_reachable(target: tuple[str, int], _timeout: float) -> None:
            if target[0] == "198.51.100.1":
                raise OSError(errno.ECONNREFUSED, "refused")
            return None

        with self.assertRaisesRegex(QUALIFY.QualificationError, "still reachable"):
            QUALIFY.verify_production_ipv4_blocked(ips, connect_fn=second_reachable)

    def test_build_worker_command_enforces_literal_loopback_and_exact_flags(self) -> None:
        cmd = QUALIFY.build_worker_command(
            node_binary=Path("/tmp/arc-node"),
            rpc_port=19944,
            data_dir=Path("/tmp/private/data"),
            genesis_path=Path("/tmp/genesis.toml"),
            keyfile_path=Path("/tmp/private/key.json"),
            model_path=Path("/tmp/model.gguf"),
            coordinator_url="http://127.0.0.1:18080",
        )
        self.assertIn("--full-integer-worker", cmd)
        self.assertIn("--community-mode", cmd)
        self.assertNotIn("--community", cmd)
        self.assertNotIn("--allow-insecure-community-rpc", cmd)
        self.assertEqual(cmd[cmd.index("--threads") + 1], "1")
        self.assertEqual(cmd[cmd.index("--stake") + 1], "0")
        self.assertEqual(cmd[cmd.index("--p2p-port") + 1], "0")
        self.assertEqual(cmd[cmd.index("--community-rpc-url") + 1], "http://127.0.0.1:18080")

        for bad_url in (
            "https://149.28.153.31",
            "http://localhost:18080",
            "http://0.0.0.0:18080",
            "http://127.0.0.2:18080",
        ):
            with self.assertRaisesRegex(QUALIFY.QualificationError, "literal loopback"):
                QUALIFY.build_worker_command(
                    node_binary=Path("/tmp/arc-node"),
                    rpc_port=19944,
                    data_dir=Path("/tmp/private/data"),
                    genesis_path=Path("/tmp/genesis.toml"),
                    keyfile_path=Path("/tmp/private/key.json"),
                    model_path=Path("/tmp/model.gguf"),
                    coordinator_url=bad_url,
                )

    def test_loading_health_validator_enforces_30s_window_headers_and_fields(self) -> None:
        sha = "ab" * 32
        ok_initial = make_http_result(
            200, {"Connection": "close"}, valid_loading_health_body(binary_sha256=None)
        )
        QUALIFY.validate_loading_health(ok_initial, "0.8.28", sha, 0.4)

        ok_hashed = make_http_result(
            200, {"Connection": "close"}, valid_loading_health_body(binary_sha256=sha)
        )
        QUALIFY.validate_loading_health(ok_hashed, "0.8.28", sha, 1.2)

        with self.assertRaisesRegex(QUALIFY.QualificationError, "exceeding 30.0s"):
            QUALIFY.validate_loading_health(ok_hashed, "0.8.28", sha, 30.0)

        keep_alive = make_http_result(
            200, {"Connection": "keep-alive"}, valid_loading_health_body(binary_sha256=sha)
        )
        with self.assertRaisesRegex(QUALIFY.QualificationError, "Connection: close"):
            QUALIFY.validate_loading_health(keep_alive, "0.8.28", sha, 0.5)

        wrong_sha = make_http_result(
            200, {"Connection": "close"}, valid_loading_health_body(binary_sha256="cd" * 32)
        )
        with self.assertRaisesRegex(QUALIFY.QualificationError, "binary_sha256 mismatch"):
            QUALIFY.validate_loading_health(wrong_sha, "0.8.28", sha, 0.5)

        bad_features_body = valid_loading_health_body(binary_sha256=sha)
        bad_features_body["features"] = ["candle", "benchmark-tools"]
        bad_features = make_http_result(200, {"Connection": "close"}, bad_features_body)
        with self.assertRaisesRegex(QUALIFY.QualificationError, "benchmark-tools"):
            QUALIFY.validate_loading_health(bad_features, "0.8.28", sha, 0.5)

    def test_loading_readiness_worker_status_and_mutations_require_503(self) -> None:
        headers = {"Connection": "close", "Retry-After": "5"}
        ok_readiness = make_http_result(503, headers, valid_readiness_body(ready=False))
        QUALIFY.validate_loading_readiness(ok_readiness)

        premature_200 = make_http_result(200, headers, valid_readiness_body(ready=False))
        with self.assertRaisesRegex(QUALIFY.QualificationError, "expected 503"):
            QUALIFY.validate_loading_readiness(premature_200)

        extra_key = valid_readiness_body(ready=False)
        extra_key["extra"] = True
        with self.assertRaisesRegex(QUALIFY.QualificationError, "keys mismatch"):
            QUALIFY.validate_loading_readiness(make_http_result(503, headers, extra_key))

        ok_worker_status = make_http_result(
            503,
            headers,
            {"status": "loading", "role": "full_integer_worker", "error": "initializing"},
        )
        QUALIFY.validate_loading_worker_status(ok_worker_status)
        with self.assertRaisesRegex(QUALIFY.QualificationError, "expected 503"):
            QUALIFY.validate_loading_worker_status(
                make_http_result(200, headers, {"status": "loading"})
            )

        ok_unavailable = make_http_result(503, headers, {"error": "loading"})
        QUALIFY.validate_loading_unavailable_route("POST /inference/run", ok_unavailable)
        with self.assertRaisesRegex(QUALIFY.QualificationError, "expected 503"):
            QUALIFY.validate_loading_unavailable_route(
                "POST /inference/run",
                make_http_result(200, headers, {"error": "loading"}),
            )

    def test_ready_validators_enforce_handoff_identity_and_exact_wire_formats(self) -> None:
        sha = "ab" * 32
        addr = "12" * 32
        blake3 = "34" * 32

        ready_health = make_http_result(
            200,
            {},
            {
                "status": "degraded",
                "version": "0.8.28",
                "chain_participation_enabled": False,
                "binary_sha256": sha,
                "features": ["candle"],
            },
        )
        QUALIFY.validate_ready_health(ready_health, "0.8.28", sha)

        still_loading = make_http_result(200, {}, valid_loading_health_body(binary_sha256=sha))
        with self.assertRaisesRegex(QUALIFY.QualificationError, "leave loading state"):
            QUALIFY.validate_ready_health(still_loading, "0.8.28", sha)

        missing_sha = make_http_result(
            200,
            {},
            {
                "status": "degraded",
                "version": "0.8.28",
                "chain_participation_enabled": False,
                "binary_sha256": None,
                "features": ["candle"],
            },
        )
        with self.assertRaisesRegex(QUALIFY.QualificationError, "without 0x prefix"):
            QUALIFY.validate_ready_health(missing_sha, "0.8.28", sha)

        node_info = make_http_result(
            200,
            {},
            {"version": "0.8.28", "stake": 0, "validator": addr},
        )
        QUALIFY.validate_ready_node_info(node_info, "0.8.28", addr)
        with self.assertRaisesRegex(QUALIFY.QualificationError, "without 0x prefix"):
            QUALIFY.validate_ready_node_info(
                make_http_result(200, {}, {"version": "0.8.28", "stake": 0, "validator": f"0x{addr}"}),
                "0.8.28",
                addr,
            )
        with self.assertRaisesRegex(QUALIFY.QualificationError, "validator mismatch"):
            QUALIFY.validate_ready_node_info(node_info, "0.8.28", "ff" * 32)

        readiness = make_http_result(200, {}, valid_readiness_body(ready=True, model_blake3=blake3))
        QUALIFY.validate_ready_readiness(readiness, blake3)
        bare_model_id = valid_readiness_body(ready=True, model_blake3=blake3)
        bare_model_id["model_id"] = blake3
        with self.assertRaisesRegex(QUALIFY.QualificationError, "0x-prefixed"):
            QUALIFY.validate_ready_readiness(make_http_result(200, {}, bare_model_id), blake3)
        with self.assertRaisesRegex(QUALIFY.QualificationError, "model_id mismatch"):
            QUALIFY.validate_ready_readiness(readiness, "99" * 32)

        worker_status = make_http_result(
            200,
            {},
            {
                "schema": QUALIFY.WORKER_STATUS_SCHEMA,
                "worker_id": f"0x{addr}",
                "coordinators_total": 1,
                "jobs_claimed": 0,
                "jobs_completed": 0,
            },
        )
        QUALIFY.validate_ready_worker_status(worker_status, addr)
        with self.assertRaisesRegex(QUALIFY.QualificationError, "0x-prefixed"):
            QUALIFY.validate_ready_worker_status(
                make_http_result(
                    200,
                    {},
                    {
                        "schema": QUALIFY.WORKER_STATUS_SCHEMA,
                        "worker_id": addr,
                        "coordinators_total": 1,
                        "jobs_claimed": 0,
                        "jobs_completed": 0,
                    },
                ),
                addr,
            )

    def test_causal_prepared_marker_allows_post_prepare_before_health_handoff_and_rejects_premature(
        self,
    ) -> None:
        addr = "45" * 32
        blake3 = "67" * 32

        r_fd, w_fd = os.pipe()
        read_stream = os.fdopen(r_fd, "rb", buffering=0)
        capture = QUALIFY.BoundedPipeLogCapture(read_stream, phase_name="causal-ok")
        coordinator = QUALIFY.LoopbackCommunityCoordinator()
        try:
            coordinator.attach_log_observer(capture, allow_requests_after_prepared=True)
            os.write(
                w_fd,
                b"2026-11-28T00:00:01Z INFO arc_node: Loading model from /models/llama.gguf...\n"
                b"2026-11-28T00:01:03Z INFO arc_node: Model loaded in 62.4s - 32 layers held / 32 total, 6400 MB shard weights, vocab 32000\n",
            )
            info_resp = QUALIFY.http_request(coordinator.port, "GET", "/network/info")
            self.assertEqual(info_resp.status, 200)
            self.assertEqual(
                info_resp.json_body["validator_address"],
                f"0x{coordinator.coordinator_address_hex}",
            )

            envelope = make_signed_register_envelope(
                coordinator.coordinator_address_hex, addr, blake3
            )
            reg_resp = QUALIFY.http_request(
                coordinator.port,
                "POST",
                "/community/register",
                json_payload=envelope,
            )
            self.assertEqual(reg_resp.status, 503)

            captured, total, overflowed = coordinator.snapshot_state()
            summary = QUALIFY.validate_post_ready_registration(
                requests=captured,
                total_requests=total,
                coordinator_overflowed=overflowed,
                expected_address_hex=addr,
                expected_model_blake3=blake3,
                expected_coordinator_hex=coordinator.coordinator_address_hex,
            )
            self.assertEqual(summary["network_info_count"], 1)
            self.assertEqual(summary["register_count"], 1)
            self.assertEqual(summary["causal_prepared_marker_evidence"]["layers_held"], 32)
            self.assertEqual(summary["causal_prepared_marker_evidence"]["total_layers"], 32)
        finally:
            coordinator.attach_log_observer(None, allow_requests_after_prepared=False)
            os.close(w_fd)
            capture.close()
            coordinator.close()

        r_fd2, w_fd2 = os.pipe()
        read_stream2 = os.fdopen(r_fd2, "rb", buffering=0)
        capture2 = QUALIFY.BoundedPipeLogCapture(read_stream2, phase_name="causal-premature")
        coordinator2 = QUALIFY.LoopbackCommunityCoordinator()
        try:
            coordinator2.attach_log_observer(capture2, allow_requests_after_prepared=True)
            os.write(
                w_fd2,
                b"2026-11-28T00:00:01Z INFO arc_node: Loading model from /models/llama.gguf...\n"
                b"2026-11-28T00:00:10Z INFO arc_node: Model loaded in 9.0s - 16 layers held / 32 total, 3200 MB shard weights, vocab 32000\n",
            )
            premature_resp = QUALIFY.http_request(coordinator2.port, "GET", "/network/info")
            self.assertEqual(premature_resp.status, 409)

            os.write(
                w_fd2,
                b"2026-11-28T00:01:03Z INFO arc_node: Model loaded in 62.4s - 32 layers held / 32 total, 6400 MB shard weights, vocab 32000\n",
            )
            late_info = QUALIFY.http_request(coordinator2.port, "GET", "/network/info")
            self.assertEqual(late_info.status, 200)
            late_reg = QUALIFY.http_request(
                coordinator2.port,
                "POST",
                "/community/register",
                json_payload=make_signed_register_envelope(
                    coordinator2.coordinator_address_hex, addr, blake3
                ),
            )
            self.assertEqual(late_reg.status, 503)

            captured2, total2, overflowed2 = coordinator2.snapshot_state()
            self.assertIs(captured2[0]["prepared_before_request"], False)
            self.assertIs(captured2[1]["prepared_before_request"], True)
            with self.assertRaisesRegex(
                QUALIFY.QualificationError, "before canonical model preparation completed"
            ):
                QUALIFY.validate_post_ready_registration(
                    requests=captured2,
                    total_requests=total2,
                    coordinator_overflowed=overflowed2,
                    expected_address_hex=addr,
                    expected_model_blake3=blake3,
                    expected_coordinator_hex=coordinator2.coordinator_address_hex,
                )
        finally:
            coordinator2.attach_log_observer(None, allow_requests_after_prepared=False)
            os.close(w_fd2)
            capture2.close()
            coordinator2.close()

        r_fd3, w_fd3 = os.pipe()
        read_stream3 = os.fdopen(r_fd3, "rb", buffering=0)
        capture3 = QUALIFY.BoundedPipeLogCapture(read_stream3, phase_name="causal-run1")
        coordinator3 = QUALIFY.LoopbackCommunityCoordinator()
        try:
            coordinator3.attach_log_observer(capture3, allow_requests_after_prepared=False)
            os.write(
                w_fd3,
                b"Model loaded in 1.0s - 32 layers held / 32 total, 6400 MB shard weights, vocab 32000\n",
            )
            run1_resp = QUALIFY.http_request(coordinator3.port, "GET", "/network/info")
            self.assertEqual(run1_resp.status, 409)
            self.assertIs(coordinator3.snapshot_requests()[0]["prepared_before_request"], False)
        finally:
            coordinator3.attach_log_observer(None, allow_requests_after_prepared=False)
            os.close(w_fd3)
            capture3.close()
            coordinator3.close()

    def test_coordinator_rejects_oversize_malformed_stalled_and_overflowed_requests(self) -> None:
        coordinator = QUALIFY.LoopbackCommunityCoordinator(
            request_timeout_secs=0.25,
            max_body_bytes=256,
            max_captured_requests=2,
        )
        stalled_sock: socket.socket | None = None
        try:
            start = time.monotonic()
            oversize_raw = send_raw_http(
                coordinator.port,
                b"POST /community/register HTTP/1.1\r\n"
                b"Host: 127.0.0.1\r\n"
                b"Content-Length: 999999999\r\n"
                b"Connection: close\r\n\r\n",
            )
            self.assertLess(time.monotonic() - start, 1.5)
            self.assertIn(b" 413 ", oversize_raw.splitlines()[0])

            chunked_raw = send_raw_http(
                coordinator.port,
                b"POST /community/register HTTP/1.1\r\n"
                b"Host: 127.0.0.1\r\n"
                b"Transfer-Encoding: chunked\r\n"
                b"Connection: close\r\n\r\n",
            )
            self.assertIn(b" 400 ", chunked_raw.splitlines()[0])

            dup_cl_raw = send_raw_http(
                coordinator.port,
                b"POST /community/register HTTP/1.1\r\n"
                b"Host: 127.0.0.1\r\n"
                b"Content-Length: 2\r\n"
                b"Content-Length: 2\r\n"
                b"Connection: close\r\n\r\n{}",
            )
            self.assertIn(b" 400 ", dup_cl_raw.splitlines()[0])

            bad_cl_raw = send_raw_http(
                coordinator.port,
                b"POST /community/register HTTP/1.1\r\n"
                b"Host: 127.0.0.1\r\n"
                b"Content-Length: -5\r\n"
                b"Connection: close\r\n\r\n",
            )
            self.assertIn(b" 400 ", bad_cl_raw.splitlines()[0])

            trickle_start = time.monotonic()
            trickle_chunks = bytearray()
            with socket.create_connection(("127.0.0.1", coordinator.port), timeout=2.0) as tsock:
                tsock.settimeout(2.0)
                tsock.sendall(
                    b"POST /community/register HTTP/1.1\r\n"
                    b"Host: 127.0.0.1\r\n"
                    b"Content-Length: 100\r\n"
                    b"Connection: close\r\n\r\n"
                )
                for _ in range(12):
                    readable, _, _ = select.select([tsock], [], [], 0.06)
                    if readable:
                        break
                    try:
                        tsock.sendall(b"x")
                    except OSError:
                        break
                while True:
                    try:
                        part = tsock.recv(4096)
                    except (socket.timeout, TimeoutError, ConnectionResetError):
                        break
                    if not part:
                        break
                    trickle_chunks.extend(part)
            self.assertLess(time.monotonic() - trickle_start, 2.0)
            self.assertTrue(trickle_chunks)
            self.assertIn(b" 408 ", bytes(trickle_chunks).splitlines()[0])

            self.assertTrue(coordinator.overflowed())
            captured, total, overflowed = coordinator.snapshot_state()
            self.assertEqual(len(captured), 2)
            self.assertGreaterEqual(total, 5)
            self.assertTrue(overflowed)
            with self.assertRaisesRegex(QUALIFY.QualificationError, "overflowed"):
                QUALIFY.validate_post_ready_registration(
                    requests=captured,
                    total_requests=total,
                    coordinator_overflowed=overflowed,
                    expected_address_hex="11" * 32,
                    expected_model_blake3="22" * 32,
                    expected_coordinator_hex=coordinator.coordinator_address_hex,
                )

            stalled_sock = socket.create_connection(("127.0.0.1", coordinator.port), timeout=2.0)
            stalled_sock.sendall(b"GET /network/info HTTP/1.1\r\nHost: 127.0.0.1\r\n")
            time.sleep(0.05)
            close_start = time.monotonic()
            coordinator.close(timeout_secs=2.0)
            self.assertLess(time.monotonic() - close_start, 1.5)
            self.assertEqual(coordinator.active_thread_count(), 0)
        finally:
            if stalled_sock is not None:
                stalled_sock.close()
            coordinator.close(timeout_secs=2.0)

    def test_bounded_pipe_log_capture_truncation_markers_stop_and_child_overflow_cleanup(
        self,
    ) -> None:
        r_fd, w_fd = os.pipe()
        read_stream = os.fdopen(r_fd, "rb", buffering=0)
        capture = QUALIFY.BoundedPipeLogCapture(
            read_stream,
            phase_name="bounded-truncate",
            max_retained_bytes=256,
            max_total_bytes=8192,
        )
        try:
            middle_marker = (
                b"Model loaded in 5.5s - 32 layers held / 32 total, 6400 MB shard weights, vocab 32000\n"
                b"Community-mode HTTP registration started (worker_id=0x11)\n"
            )
            os.write(w_fd, b"HEAD-LINE\n" + (b"A" * 1024) + b"\n" + middle_marker + (b"Z" * 1024) + b"\nTAIL-LINE\n")
            os.close(w_fd)
            w_fd = -1
            self.assertTrue(capture.drain_until_eof(timeout_secs=2.0))
            capture.close(join_timeout_secs=2.0)

            with tempfile.TemporaryDirectory() as tmp:
                out_log = Path(tmp) / "retained.log"
                meta = capture.write_retained_log(out_log)
                self.assertTrue(meta["truncated"])
                self.assertFalse(meta["overflowed"])
                self.assertTrue(meta["pipe_eof"])
                self.assertLessEqual(meta["retained_bytes"], 256)
                raw = out_log.read_bytes()
                self.assertTrue(raw.startswith(b"HEAD-LINE\n"))
                self.assertIn(QUALIFY.TRUNCATION_MARKER, raw)
                self.assertTrue(raw.endswith(b"TAIL-LINE\n"))
                self.assertIn("Community-mode HTTP registration started", capture.seen_markers())
                self.assertIsNotNone(capture.model_prepared_record())
                self.assertEqual(capture.model_prepared_record()["layers_held"], 32)
        finally:
            if w_fd != -1:
                os.close(w_fd)
            capture.close(join_timeout_secs=2.0)

        r_open, w_open = os.pipe()
        open_stream = os.fdopen(r_open, "rb", buffering=0)
        open_capture = QUALIFY.BoundedPipeLogCapture(open_stream, phase_name="open-writer-stop")
        try:
            close_start = time.monotonic()
            open_capture.close(join_timeout_secs=2.0)
            self.assertLess(time.monotonic() - close_start, 1.0)
        finally:
            os.close(w_open)

        overflow_fired = threading.Event()
        proc = subprocess.Popen(
            [
                sys.executable,
                "-c",
                "import os, subprocess, sys, time\n"
                "child = subprocess.Popen([sys.executable, '-c', 'import time; time.sleep(30)'])\n"
                "chunk = 'X' * 512 + '\\n'\n"
                "for _ in range(40):\n"
                "    sys.stdout.write(chunk)\n"
                "    sys.stdout.flush()\n"
                "time.sleep(30)\n",
            ],
            stdout=subprocess.PIPE,
            stderr=subprocess.STDOUT,
            start_new_session=True,
            close_fds=True,
        )
        assert proc.stdout is not None
        pgid = proc.pid

        def on_overflow() -> None:
            overflow_fired.set()
            try:
                os.killpg(pgid, signal.SIGTERM)
            except ProcessLookupError:
                pass

        overflow_capture = QUALIFY.BoundedPipeLogCapture(
            proc.stdout,
            phase_name="overflow-child",
            max_retained_bytes=256,
            max_total_bytes=1024,
            on_overflow=on_overflow,
        )
        try:
            self.assertTrue(overflow_fired.wait(timeout=3.0))
            self.assertTrue(overflow_capture.overflowed)
            QUALIFY.terminate_process_group(proc, grace_secs=2.0)
            self.assertFalse(QUALIFY.process_group_alive(pgid))
            self.assertEqual(QUALIFY.list_live_pids_in_process_group(pgid), [])
            overflow_capture.close(join_timeout_secs=2.0)
        finally:
            QUALIFY.terminate_process_group(proc, grace_secs=1.0)
            overflow_capture.close(join_timeout_secs=2.0)

    def test_termination_signal_guard_unwinds_cleanup_and_restores_handlers(self) -> None:
        orig_term = signal.getsignal(signal.SIGTERM)
        orig_int = signal.getsignal(signal.SIGINT)
        proc = subprocess.Popen(
            [
                sys.executable,
                "-c",
                "import subprocess, sys, time\n"
                "subprocess.Popen([sys.executable, '-c', 'import time; time.sleep(30)'])\n"
                "sys.stdout.write('ready\\n')\n"
                "sys.stdout.flush()\n"
                "time.sleep(30)\n",
            ],
            stdout=subprocess.PIPE,
            stderr=subprocess.STDOUT,
            start_new_session=True,
            close_fds=True,
        )
        assert proc.stdout is not None
        pgid = proc.pid
        capture = QUALIFY.BoundedPipeLogCapture(proc.stdout, phase_name="sigterm-guard")
        coordinator = QUALIFY.LoopbackCommunityCoordinator()
        cleanup_completed = False
        caught: BaseException | None = None
        try:
            ready_deadline = time.monotonic() + 3.0
            while time.monotonic() < ready_deadline and capture.total_bytes_seen == 0:
                time.sleep(0.02)
            self.assertGreater(capture.total_bytes_seen, 0)
            with QUALIFY.TerminationSignalGuard() as guard:
                try:
                    os.kill(os.getpid(), signal.SIGTERM)
                    self.fail("SIGTERM did not raise QualificationInterrupted")
                except QUALIFY.QualificationInterrupted as interrupted:
                    caught = interrupted
                finally:
                    os.kill(os.getpid(), signal.SIGTERM)
                    QUALIFY.terminate_process_group(proc, grace_secs=2.0)
                    capture.close(join_timeout_secs=2.0)
                    coordinator.close(timeout_secs=2.0)
                    cleanup_completed = True
            self.assertEqual(guard.received_signal, signal.SIGTERM)
            self.assertEqual(guard.additional_signals, [signal.SIGTERM])
        finally:
            QUALIFY.terminate_process_group(proc, grace_secs=1.0)
            capture.close(join_timeout_secs=2.0)
            coordinator.close(timeout_secs=2.0)

        self.assertIsInstance(caught, QUALIFY.QualificationInterrupted)
        self.assertTrue(cleanup_completed)
        self.assertFalse(QUALIFY.process_group_alive(pgid))
        self.assertEqual(coordinator.active_thread_count(), 0)
        self.assertEqual(signal.getsignal(signal.SIGTERM), orig_term)
        self.assertEqual(signal.getsignal(signal.SIGINT), orig_int)

    def test_main_handles_sigterm_and_cleanup_errors_without_faking_pass(self) -> None:
        orig_term = signal.getsignal(signal.SIGTERM)
        orig_int = signal.getsignal(signal.SIGINT)
        with tempfile.TemporaryDirectory() as tmp:
            root = Path(tmp)
            repo = root / "repo"
            model_bytes = b"fixture-model-payload"
            model_sha = hashlib.sha256(model_bytes).hexdigest()
            write_synthetic_repo(
                repo,
                {
                    "manifest_bytes": len(model_bytes),
                    "manifest_sha256": model_sha,
                    "file_name": "fixture.gguf",
                },
            )
            model_path = root / "fixture.gguf"
            model_path.write_bytes(model_bytes)
            fake_node = root / "arc-node"
            fake_node.write_text("#!/bin/sh\nexit 0\n", encoding="utf-8")
            os.chmod(fake_node, 0o755)
            private_dir = root / "private"
            private_dir.mkdir(mode=0o700)
            keyfile = private_dir / "worker.json"
            secret_hex = "fe" * 32
            keyfile.write_text(
                json.dumps(
                    {
                        "scheme": "ed25519",
                        "address": "11" * 32,
                        "public_key": "22" * 32,
                        "secret_key": secret_hex,
                    }
                ),
                encoding="utf-8",
            )
            os.chmod(keyfile, 0o600)
            out_sigterm = root / "evidence-sigterm"
            fake_resources = QUALIFY.ResourceSnapshot(
                mem_total_bytes=16 * QUALIFY.GIB_BYTES,
                mem_available_bytes=14 * QUALIFY.GIB_BYTES,
                cgroup_limit_bytes=None,
                cgroup_available_bytes=None,
                effective_total_bytes=16 * QUALIFY.GIB_BYTES,
                effective_available_bytes=14 * QUALIFY.GIB_BYTES,
                nominal_ram_gb=16,
                disk_path=str(root),
                disk_total_bytes=40 * QUALIFY.GIB_BYTES,
                disk_free_bytes=20 * QUALIFY.GIB_BYTES,
            )

            def interrupt_run1(**_kwargs: object) -> object:
                os.kill(os.getpid(), signal.SIGTERM)
                raise AssertionError("unreachable")

            clean_env = {"PATH": os.environ.get("PATH", "/usr/bin")}
            with (
                mock.patch.dict(os.environ, clean_env, clear=True),
                mock.patch.object(QUALIFY, "measure_runner_resources", return_value=fake_resources),
                mock.patch.object(
                    QUALIFY, "run_sigterm_during_loading_phase", side_effect=interrupt_run1
                ),
            ):
                rc = QUALIFY.main(
                    [
                        "--repo-root",
                        str(repo),
                        "--node-binary",
                        str(fake_node),
                        "--validator-key-file",
                        str(keyfile),
                        "--model",
                        str(model_path),
                        "--output-dir",
                        str(out_sigterm),
                        "--private-dir",
                        str(private_dir),
                        "--candidate-commit",
                        "deadbeef",
                    ]
                )
            self.assertEqual(rc, 128 + signal.SIGTERM)
            summary = json.loads((out_sigterm / "summary.json").read_text(encoding="utf-8"))
            self.assertIs(summary["pass"], False)
            self.assertIs(summary["interrupted"], True)
            self.assertEqual(summary["interrupted_signal"], "SIGTERM")
            self.assertTrue((out_sigterm / "coordinator-requests.json").is_file())
            self.assertEqual(signal.getsignal(signal.SIGTERM), orig_term)
            self.assertEqual(signal.getsignal(signal.SIGINT), orig_int)

            out_cleanup_fail = root / "evidence-cleanup-fail"
            real_close = QUALIFY.LoopbackCommunityCoordinator.close

            def failing_close(self_coord: object, timeout_secs: float = 5.0) -> None:
                real_close(self_coord, timeout_secs=timeout_secs)
                raise QUALIFY.QualificationError("simulated lingering coordinator handler")

            with (
                mock.patch.dict(os.environ, clean_env, clear=True),
                mock.patch.object(QUALIFY, "measure_runner_resources", return_value=fake_resources),
                mock.patch.object(
                    QUALIFY,
                    "run_sigterm_during_loading_phase",
                    return_value=({"ok": True}, {"retained_bytes": 0}, 19999),
                ),
                mock.patch.object(
                    QUALIFY,
                    "run_complete_load_and_handoff_phase",
                    return_value=({"ok": True}, {"retained_bytes": 0}),
                ),
                mock.patch.object(
                    QUALIFY.LoopbackCommunityCoordinator,
                    "close",
                    autospec=True,
                    side_effect=failing_close,
                ),
            ):
                rc2 = QUALIFY.main(
                    [
                        "--repo-root",
                        str(repo),
                        "--node-binary",
                        str(fake_node),
                        "--validator-key-file",
                        str(keyfile),
                        "--model",
                        str(model_path),
                        "--output-dir",
                        str(out_cleanup_fail),
                        "--private-dir",
                        str(private_dir),
                        "--candidate-commit",
                        "deadbeef",
                    ]
                )
            self.assertEqual(rc2, 1)
            summary2 = json.loads((out_cleanup_fail / "summary.json").read_text(encoding="utf-8"))
            self.assertIs(summary2["pass"], False)
            self.assertTrue(summary2.get("cleanup_errors"))
            self.assertIn("lingering coordinator handler", summary2["cleanup_errors"][0])

    def test_secret_leak_guard_and_keyfile_boundaries(self) -> None:
        secret_hex = "fe" * 32
        with tempfile.TemporaryDirectory() as tmp:
            root = Path(tmp)
            evidence_dir = root / "evidence"
            private_dir = root / "private"
            evidence_dir.mkdir()
            private_dir.mkdir(mode=0o700)

            (evidence_dir / "clean.json").write_text('{"ok": true}\n', encoding="utf-8")
            QUALIFY.assert_no_secrets_in_directory(evidence_dir, (secret_hex,))

            (evidence_dir / "leaked.txt").write_text(
                f"oops {secret_hex.upper()}\n", encoding="utf-8"
            )
            with self.assertRaisesRegex(QUALIFY.QualificationError, "secret material leaked"):
                QUALIFY.assert_no_secrets_in_directory(evidence_dir, (secret_hex,))
            (evidence_dir / "leaked.txt").unlink()

            dummy_target = private_dir / "secret.txt"
            dummy_target.write_text("private", encoding="utf-8")
            link_in_evidence = evidence_dir / "sneaky-link"
            link_in_evidence.symlink_to(dummy_target)
            with self.assertRaisesRegex(QUALIFY.QualificationError, "must not contain symlinks"):
                QUALIFY.assert_no_secrets_in_directory(evidence_dir, (secret_hex,))
            link_in_evidence.unlink()

            keyfile_in_evidence = evidence_dir / "bad.json"
            keyfile_in_evidence.write_text("{}", encoding="utf-8")
            os.chmod(keyfile_in_evidence, 0o600)
            with self.assertRaisesRegex(
                QUALIFY.QualificationError, "outside the uploaded evidence"
            ):
                QUALIFY.ensure_persistent_keyfile(
                    private_dir=private_dir,
                    output_dir=evidence_dir,
                    arc_cli_binary=None,
                    existing_keyfile=keyfile_in_evidence,
                    child_env={},
                )

            bad_mode_key = private_dir / "bad-mode.json"
            bad_mode_key.write_text(
                json.dumps(
                    {
                        "scheme": "ed25519",
                        "address": "11" * 32,
                        "public_key": "22" * 32,
                        "secret_key": secret_hex,
                    }
                ),
                encoding="utf-8",
            )
            os.chmod(bad_mode_key, 0o644)
            with self.assertRaisesRegex(QUALIFY.QualificationError, "mode 0600"):
                QUALIFY.ensure_persistent_keyfile(
                    private_dir=private_dir,
                    output_dir=evidence_dir,
                    arc_cli_binary=None,
                    existing_keyfile=bad_mode_key,
                    child_env={},
                )

    def test_linux_candidate_workflow_isolates_worker_startup_and_retains_iptables_blocks(
        self,
    ) -> None:
        workflow_path = REPO_ROOT / ".github" / "workflows" / "linux-candidate.yml"
        text = workflow_path.read_text(encoding="utf-8")
        self.assertIn("- worker-startup", text)

        jobs_match = re.search(r"^jobs:\n(.*)$", text, flags=re.MULTILINE | re.DOTALL)
        self.assertIsNotNone(jobs_match)
        jobs_body = jobs_match.group(1)

        job_blocks: dict[str, str] = {}
        current_name: str | None = None
        current_lines: list[str] = []
        for line in jobs_body.splitlines():
            top_job = re.match(r"^  ([a-z0-9-]+):\s*$", line)
            if top_job:
                if current_name is not None:
                    job_blocks[current_name] = "\n".join(current_lines)
                current_name = top_job.group(1)
                current_lines = []
            elif current_name is not None:
                current_lines.append(line)
        if current_name is not None:
            job_blocks[current_name] = "\n".join(current_lines)

        self.assertIn("worker-startup", job_blocks)
        for job_name, block in job_blocks.items():
            if_match = re.search(r"^    if:\s*(.+)$", block, flags=re.MULTILINE)
            self.assertIsNotNone(if_match, f"job {job_name} must declare an explicit if condition")
            cond = if_match.group(1)
            if job_name == "worker-startup":
                self.assertEqual(cond.strip(), "inputs.qualification == 'worker-startup'")
            else:
                isolates_other = bool(
                    re.search(r"inputs\.qualification\s*==\s*'[^']+'", cond)
                ) and "worker-startup" not in cond
                excludes_worker_startup = "inputs.qualification != 'worker-startup'" in cond
                self.assertTrue(
                    isolates_other or excludes_worker_startup,
                    f"job {job_name} if condition does not exclude worker-startup: {cond}",
                )

        ws_block = job_blocks["worker-startup"]
        self.assertIn("CARGO_BUILD_JOBS: 1", ws_block)
        self.assertIn("RAYON_NUM_THREADS: 1", ws_block)
        self.assertIn("toolchain: nightly-2026-03-16", ws_block)
        self.assertIn(
            "cargo build --locked --release -p arc-node --features candle --bin arc-node",
            ws_block,
        )
        self.assertIn("python3 scripts/ci/no_comments.py", ws_block)
        self.assertIn("scripts/qualification/tests/test_worker_startup_qualification.py", ws_block)
        self.assertIn("scripts/qualification/qualify_worker_startup.py", ws_block)
        self.assertIn("iptables -I OUTPUT", ws_block)
        self.assertIn("iptables -C OUTPUT", ws_block)
        self.assertNotIn("iptables -D", ws_block)
        self.assertNotIn("blocked_ips", ws_block)
        self.assertIn('rm -rf "$MODEL_DIR" "$PRIVATE_DIR"', ws_block)
        self.assertIn("timeout --signal=TERM --kill-after=30s 20m", ws_block)
        self.assertIn("--require-blocked-ipv4", ws_block)
        self.assertNotIn("worker-startup-model", ws_block.split("actions/upload-artifact", 1)[1])
        self.assertNotIn("worker-startup-private", ws_block.split("actions/upload-artifact", 1)[1])


if __name__ == "__main__":
    unittest.main()
