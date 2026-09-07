from __future__ import annotations

import base64
import hashlib
import http.server
import importlib.util
import inspect
import json
import os
import socket
import sys
import tempfile
import threading
import unittest
from pathlib import Path
from unittest import mock


ROOT = Path(__file__).parents[2]
SCRIPT = ROOT / "scripts/release/packaged-appimage-live-gate.py"


def load_module():
    spec = importlib.util.spec_from_file_location("arc_packaged_appimage_live_gate", SCRIPT)
    assert spec is not None and spec.loader is not None
    module = importlib.util.module_from_spec(spec)
    sys.modules[spec.name] = module
    spec.loader.exec_module(module)
    return module


GATE = load_module()


def digest(raw: bytes) -> str:
    return hashlib.sha256(raw).hexdigest()


class Fixture:
    def __init__(self, root: Path) -> None:
        self.root = root
        self.assets = root / "assets"
        self.assets.mkdir()
        self.commit = "a" * 40
        asset_rows = {}
        for index, name in enumerate(
            (GATE.APPIMAGE_NAME, GATE.APPIMAGE_SIGNATURE_NAME, GATE.NODE_NAME), start=1
        ):
            raw = f"exact-{name}-bytes\n".encode("ascii")
            (self.assets / name).write_bytes(raw)
            asset_rows[name] = {"id": index, "sha256": digest(raw), "size": len(raw)}
        self.binding = {
            "assets": asset_rows,
            "commit": self.commit,
            "legacy_source": {},
            "published_evidence": {},
            "release": {"id": 77, "immutable": True},
            "release_workflow": {"head_sha": self.commit},
            "repository": GATE.REPOSITORY,
            "schema": GATE.RELEASE_SCHEMA,
            "tag": GATE.TAG,
        }
        self.binding_path = root / "release-binding.json"
        self.binding_raw = GATE.canonical_json(self.binding)
        self.binding_path.write_bytes(self.binding_raw)
        key_line = base64.b64encode(bytes(range(42))).decode("ascii")
        self.pubkey = base64.b64encode(
            f"untrusted comment: fixture\n{key_line}\n".encode("ascii")
        ).decode("ascii")
        self.normalized_assets = {
            name: GATE.verify_bound_asset(self.binding, self.assets, name)
            for name in (GATE.APPIMAGE_NAME, GATE.APPIMAGE_SIGNATURE_NAME, GATE.NODE_NAME)
        }

    def attempt(self) -> tuple[dict, str, Path]:
        attempt, prompt = GATE.build_inference_attempt(
            self.binding,
            self.binding_raw,
            self.normalized_assets,
            {"gate_sha256": "f" * 64},
            self.pubkey,
        )
        path = self.root / "inference-attempt.json"
        path.write_bytes(GATE.canonical_json(attempt))
        return attempt, prompt, path


def successful_receipt(tx_hash: str | None = None) -> dict:
    tx_hash = tx_hash or "0x" + "1" * 64
    value = {
        "assignment_epoch": "0x" + "2" * 64,
        "block_hash": "0x" + "3" * 64,
        "block_height": 123456,
        "confirmed": True,
        "evidence_source": "successful mined CommunityInferenceReward receipt",
        "included": True,
        "index": 2,
        "input_hash": "0x" + "4" * 64,
        "job_id": "0x" + "5" * 64,
        "model_id": "0x" + "6" * 64,
        "output_hash": "0x" + "7" * 64,
        "receipt_url": f"/community/reward_receipt/{tx_hash}",
        "recovery_epoch": 9,
        "reward_arc": 2.5,
        "reward_base": 2_500_000_000,
        "status": "mined_success",
        "submitted": True,
        "success": True,
        "transaction_domain": "0x" + "8" * 64,
        "tx_hash": tx_hash,
        "tx_type": "0x25",
        "validator_approvals": 5,
        "validator_set_commitment": "0x" + "9" * 64,
        "validator_set_id": 4,
        "worker": "0x" + "a" * 64,
    }
    assert set(value) == set(GATE.TERMINAL_RECEIPT_FIELDS)
    return value


class BindingTests(unittest.TestCase):
    def test_release_binding_and_exact_assets(self) -> None:
        with tempfile.TemporaryDirectory() as raw:
            fixture = Fixture(Path(raw))
            binding, binding_raw = GATE.validate_release_binding(fixture.binding_path)
            self.assertEqual(binding, fixture.binding)
            self.assertEqual(binding_raw, fixture.binding_raw)
            self.assertEqual(
                GATE.verify_bound_asset(binding, fixture.assets, GATE.APPIMAGE_NAME)["id"], 1
            )

    def test_asset_substitution_fails(self) -> None:
        with tempfile.TemporaryDirectory() as raw:
            fixture = Fixture(Path(raw))
            (fixture.assets / GATE.NODE_NAME).write_bytes(b"substituted")
            with self.assertRaisesRegex(GATE.GateError, "differs from immutable"):
                GATE.verify_bound_asset(fixture.binding, fixture.assets, GATE.NODE_NAME)

    def test_nonimmutable_binding_fails(self) -> None:
        with tempfile.TemporaryDirectory() as raw:
            fixture = Fixture(Path(raw))
            fixture.binding["release"]["immutable"] = False
            fixture.binding_path.write_bytes(GATE.canonical_json(fixture.binding))
            with self.assertRaisesRegex(GATE.GateError, "not immutable"):
                GATE.validate_release_binding(fixture.binding_path)

    def test_duplicate_required_asset_ids_fail(self) -> None:
        with tempfile.TemporaryDirectory() as raw:
            fixture = Fixture(Path(raw))
            fixture.binding["assets"][GATE.NODE_NAME]["id"] = fixture.binding[
                "assets"
            ][GATE.APPIMAGE_NAME]["id"]
            fixture.binding_path.write_bytes(GATE.canonical_json(fixture.binding))
            with self.assertRaisesRegex(GATE.GateError, "IDs are not distinct"):
                GATE.validate_release_binding(fixture.binding_path)


class AttemptTests(unittest.TestCase):
    def test_attempt_is_256_bit_release_and_plan_bound(self) -> None:
        with tempfile.TemporaryDirectory() as raw:
            fixture = Fixture(Path(raw))
            attempt, prompt, path = fixture.attempt()
            validated, attempt_raw, rebuilt_prompt = GATE.validate_inference_attempt(
                path,
                fixture.binding,
                fixture.binding_raw,
                fixture.normalized_assets,
            )
            self.assertEqual(validated, attempt)
            self.assertEqual(rebuilt_prompt, prompt)
            self.assertEqual(attempt_raw, GATE.canonical_json(attempt))
            self.assertRegex(attempt["plan"]["challenge"], r"^[0-9a-f]{64}$")
            self.assertEqual(attempt["state"], "armed-no-retry")
            self.assertEqual(
                attempt["plan"]["total_guest_timeout_seconds"],
                GATE.GUEST_GATE_TIMEOUT_SECONDS,
            )
            self.assertIn(attempt["plan_sha256"], prompt)
            self.assertIn(attempt["plan"]["challenge"], prompt)
            self.assertEqual(
                attempt["plan"]["updater_public_key_sha256"],
                digest(GATE.canonical_updater_public_key_bytes(fixture.pubkey)),
            )

    def test_updater_key_digest_normalizes_newline_but_rejects_alternate_key(self) -> None:
        with tempfile.TemporaryDirectory() as raw:
            fixture = Fixture(Path(raw))
            canonical = GATE.canonical_updater_public_key_bytes(fixture.pubkey)
            self.assertEqual(
                canonical,
                GATE.canonical_updater_public_key_bytes(f"\n{fixture.pubkey}\n"),
            )
            alternate = base64.b64encode(b"different key document").decode("ascii")
            self.assertNotEqual(
                digest(canonical),
                digest(GATE.canonical_updater_public_key_bytes(alternate)),
            )
            # Python accepts non-zero pad bits unless canonical re-encoding is
            # checked (YR== decodes to the same byte as canonical YQ==).
            with self.assertRaisesRegex(GATE.GateError, "non-canonical base64"):
                GATE.canonical_updater_public_key_bytes("YR==")
            # Python accepts non-zero pad bits unless canonical re-encoding is
            # checked (YR== decodes to the same byte as canonical YQ==).
            with self.assertRaisesRegex(GATE.GateError, "non-canonical base64"):
                GATE.canonical_updater_public_key_bytes("YR==")

    def test_attempt_tamper_fails_closed(self) -> None:
        with tempfile.TemporaryDirectory() as raw:
            fixture = Fixture(Path(raw))
            attempt, _, path = fixture.attempt()
            attempt["plan"]["inference_submit_click_budget"] = 2
            path.write_bytes(GATE.canonical_json(attempt))
            with self.assertRaisesRegex(GATE.GateError, "plan digest mismatch"):
                GATE.validate_inference_attempt(
                    path,
                    fixture.binding,
                    fixture.binding_raw,
                    fixture.normalized_assets,
                )

    def test_attempt_prompt_hash_tamper_fails_closed(self) -> None:
        with tempfile.TemporaryDirectory() as raw:
            fixture = Fixture(Path(raw))
            attempt, _, path = fixture.attempt()
            attempt["prompt_sha256"] = "0" * 64
            path.write_bytes(GATE.canonical_json(attempt))
            with self.assertRaisesRegex(GATE.GateError, "prompt digest mismatch"):
                GATE.validate_inference_attempt(
                    path,
                    fixture.binding,
                    fixture.binding_raw,
                    fixture.normalized_assets,
                )

    def test_guest_attempt_copy_must_equal_durable_host_attempt(self) -> None:
        with tempfile.TemporaryDirectory() as raw:
            fixture = Fixture(Path(raw))
            expected, prompt, path = fixture.attempt()
            expected_raw = GATE.canonical_json(expected)
            _, copied_raw, rebuilt_prompt, input_hash = (
                GATE.validate_copied_inference_attempt(
                    path,
                    fixture.binding,
                    fixture.binding_raw,
                    fixture.normalized_assets,
                    expected,
                    expected_raw,
                )
            )
            self.assertEqual(copied_raw, expected_raw)
            self.assertEqual(rebuilt_prompt, prompt)
            self.assertEqual(
                input_hash,
                f"0x{GATE.independent_blake3_short(prompt.encode('utf-8'))}",
            )

            swapped, _ = GATE.build_inference_attempt(
                fixture.binding,
                fixture.binding_raw,
                fixture.normalized_assets,
                {"gate_sha256": "f" * 64},
                fixture.pubkey,
            )
            path.write_bytes(GATE.canonical_json(swapped))
            with self.assertRaisesRegex(GATE.GateError, "durable host arming"):
                GATE.validate_copied_inference_attempt(
                    path,
                    fixture.binding,
                    fixture.binding_raw,
                    fixture.normalized_assets,
                    expected,
                    expected_raw,
                )


class EnvironmentTests(unittest.TestCase):
    def test_app_environment_is_allowlist_not_inherited(self) -> None:
        with tempfile.TemporaryDirectory() as raw:
            profile = Path(raw)
            poisoned = {
                "ARC_WALLET_HOST": "https://attacker.invalid",
                "HTTP_PROXY": "http://attacker.invalid",
                "LD_LIBRARY_PATH": "/attacker",
                "LD_PRELOAD": "/attacker.so",
                "GTK_MODULES": "attacker",
                "PYTHONPATH": "/attacker",
                "DISPLAY": ":99",
                "DBUS_SESSION_BUS_ADDRESS": "unix:path=/tmp/dbus",
            }
            with mock.patch.dict(os.environ, poisoned, clear=True):
                value = GATE.sanitized_app_environment(
                    profile, GATE.GUEST_HOST_ALIAS, 54321, "f" * 64
                )
            for forbidden in (
                "ARC_WALLET_HOST",
                "HTTP_PROXY",
                "LD_LIBRARY_PATH",
                "LD_PRELOAD",
                "GTK_MODULES",
                "PYTHONPATH",
            ):
                self.assertNotIn(forbidden, value)
            self.assertEqual(value["DISPLAY"], ":99")
            self.assertEqual(value["TAURI_WEBVIEW_AUTOMATION"], "true")
            self.assertIn(f"@{GATE.GUEST_HOST_ALIAS}:54321", value["HTTPS_PROXY"])
            self.assertEqual(
                set(value),
                {
                    "APPIMAGE_EXTRACT_AND_RUN",
                    "DBUS_SESSION_BUS_ADDRESS",
                    "DISPLAY",
                    "HOME",
                    "HTTPS_PROXY",
                    "LANG",
                    "NO_AT_BRIDGE",
                    "NO_PROXY",
                    "PATH",
                    "TAURI_AUTOMATION",
                    "TAURI_WEBVIEW_AUTOMATION",
                    "WEBKIT_DISABLE_COMPOSITING_MODE",
                    "XDG_CACHE_HOME",
                    "XDG_CONFIG_HOME",
                    "XDG_DATA_HOME",
                    "https_proxy",
                    "no_proxy",
                },
            )


class ReceiptTests(unittest.TestCase):
    def test_exact_successful_terminal_receipt_passes(self) -> None:
        receipt = successful_receipt()
        self.assertEqual(
            GATE.validate_terminal_receipt(receipt, receipt["tx_hash"]), receipt
        )

    def test_extra_receipt_field_fails(self) -> None:
        receipt = successful_receipt()
        receipt["invented"] = True
        with self.assertRaisesRegex(GATE.GateError, "fields differ"):
            GATE.validate_terminal_receipt(receipt, receipt["tx_hash"])

    def test_failed_or_underapproved_receipt_fails(self) -> None:
        receipt = successful_receipt()
        receipt["validator_approvals"] = 4
        with self.assertRaisesRegex(GATE.GateError, "five validator approvals"):
            GATE.validate_terminal_receipt(receipt, receipt["tx_hash"])
        receipt = successful_receipt()
        receipt["status"] = "mined_failed"
        with self.assertRaisesRegex(GATE.GateError, "not a successful mined"):
            GATE.validate_terminal_receipt(receipt, receipt["tx_hash"])

    def test_receipt_must_equal_every_ui_product_identity(self) -> None:
        receipt = successful_receipt()
        binding = {
            key: receipt[key]
            for key in (
                "input_hash",
                "job_id",
                "model_id",
                "output_hash",
                "receipt_url",
                "tx_hash",
                "tx_type",
                "worker",
            )
        }
        GATE.validate_receipt_product_binding(receipt, binding)
        for field in binding:
            mismatched = dict(binding)
            mismatched[field] = "wrong"
            with self.assertRaisesRegex(GATE.GateError, field):
                GATE.validate_receipt_product_binding(receipt, mismatched)


class HashTests(unittest.TestCase):
    def test_independent_blake3_has_known_answers_and_one_chunk_bound(self) -> None:
        self.assertEqual(
            GATE.independent_blake3_short(b""),
            "af1349b9f5f9a1a6a0404dea36dcc9499bcb25c9adc112b7cc9a93cae41f3262",
        )
        self.assertEqual(
            GATE.independent_blake3_short(b"abc"),
            "6437b3ac38465133ffb63b75273a8db548c558465d79db03fd359c6cd5bd9d85",
        )
        with self.assertRaisesRegex(GATE.GateError, "one reviewed chunk"):
            GATE.independent_blake3_short(bytes(1025))

    def test_arc_hash_uses_pinned_b3sum_over_exact_bytes(self) -> None:
        expected = "a" * 64
        completed = mock.Mock(stdout=f"{expected}\n".encode("ascii"))
        with mock.patch.object(GATE, "run_command", return_value=completed) as invoked:
            self.assertEqual(GATE.arc_blake3_hash(b"exact prompt"), f"0x{expected}")
        invoked.assert_called_once_with(
            ["/usr/bin/b3sum", "--no-names"],
            input_data=b"exact prompt",
            timeout=15,
        )

    def test_arc_hash_rejects_malformed_tool_output(self) -> None:
        completed = mock.Mock(stdout=b"not-a-digest\n")
        with mock.patch.object(GATE, "run_command", return_value=completed):
            with self.assertRaisesRegex(GATE.GateError, "malformed BLAKE3"):
                GATE.arc_blake3_hash(b"prompt")


class UpdaterSignatureTests(unittest.TestCase):
    def test_tauri_outer_base64_and_single_minisign_key_are_strict(self) -> None:
        key_line = base64.b64encode(bytes(range(42))).decode("ascii")
        document = f"untrusted comment: test key\n{key_line}\n".encode("ascii")
        with tempfile.TemporaryDirectory() as raw:
            path = Path(raw) / "key.b64"
            path.write_bytes(base64.b64encode(document) + b"\n")
            self.assertEqual(
                GATE.decode_tauri_minisign_document(path, "test key"), document
            )
            self.assertEqual(GATE.minisign_public_key_line(document), key_line)

    def test_tauri_outer_base64_rejects_noncanonical_input(self) -> None:
        with tempfile.TemporaryDirectory() as raw:
            path = Path(raw) / "bad.b64"
            path.write_bytes(b"not base64!\n")
            with self.assertRaisesRegex(GATE.GateError, "strict outer-base64"):
                GATE.decode_tauri_minisign_document(path, "bad key")
            path.write_bytes(b"YR==\n")
            with self.assertRaisesRegex(GATE.GateError, "not canonical"):
                GATE.decode_tauri_minisign_document(path, "bad key")
            path.write_bytes(b"YR==\n")
            with self.assertRaisesRegex(GATE.GateError, "not canonical"):
                GATE.decode_tauri_minisign_document(path, "bad key")


class RelayTests(unittest.TestCase):
    def test_relay_rejects_other_targets_and_moves_only_exact_lax_bytes(self) -> None:
        upstream = socket.socket(socket.AF_INET, socket.SOCK_STREAM)
        upstream.bind(("127.0.0.1", 0))
        upstream.listen(1)
        upstream_port = upstream.getsockname()[1]

        def echo() -> None:
            connection, _ = upstream.accept()
            with connection:
                payload = connection.recv(16)
                connection.sendall(payload.upper())
            upstream.close()

        echo_thread = threading.Thread(target=echo, daemon=True)
        echo_thread.start()
        token = "e" * 64
        relay = GATE.RestrictedConnectRelay(upstream_port, token)
        relay.start()
        authorization = "Basic " + base64.b64encode(f"arc:{token}".encode()).decode()

        with socket.create_connection(("127.0.0.1", relay.port), timeout=3) as rejected:
            rejected.sendall(
                (
                    "CONNECT 149.28.32.76:443 HTTP/1.1\r\n"
                    "Host: 149.28.32.76:443\r\n"
                    f"Proxy-Authorization: {authorization}\r\n\r\n"
                ).encode("ascii")
            )
            self.assertTrue(rejected.recv(256).startswith(b"HTTP/1.1 403"))

        with socket.create_connection(("127.0.0.1", relay.port), timeout=3) as accepted:
            accepted.sendall(
                (
                    f"CONNECT {GATE.LAX_HOST}:443 HTTP/1.1\r\n"
                    f"Host: {GATE.LAX_HOST}:443\r\n"
                    f"Proxy-Authorization: {authorization}\r\n\r\n"
                ).encode("ascii")
            )
            response = accepted.recv(256)
            self.assertTrue(response.startswith(b"HTTP/1.1 200"))
            accepted.sendall(b"arc")
            self.assertEqual(accepted.recv(3), b"ARC")
        echo_thread.join(timeout=3)
        relay.close()
        summary = relay.summary()
        self.assertEqual(summary["accepted_connections"], 1)
        self.assertEqual(summary["rejected_connections"], 1)
        accepted_rows = [row for row in summary["events"] if row["accepted"]]
        self.assertEqual(accepted_rows[0]["target"], f"{GATE.LAX_HOST}:443")
        self.assertNotIn(token, json.dumps(summary))

    def test_relay_requires_authentication(self) -> None:
        listener = socket.socket(socket.AF_INET, socket.SOCK_STREAM)
        listener.bind(("127.0.0.1", 0))
        listener.listen(1)
        relay = GATE.RestrictedConnectRelay(listener.getsockname()[1], "d" * 64)
        relay.start()
        with socket.create_connection(("127.0.0.1", relay.port), timeout=3) as client:
            client.sendall(
                f"CONNECT {GATE.LAX_HOST}:443 HTTP/1.1\r\nHost: {GATE.LAX_HOST}:443\r\n\r\n".encode(
                    "ascii"
                )
            )
            self.assertTrue(client.recv(256).startswith(b"HTTP/1.1 407"))
        relay.close()
        listener.close()
        with self.assertRaisesRegex(GATE.GateError, "accepted no"):
            relay.summary()


class W3CTests(unittest.TestCase):
    def test_wait_propagates_fatal_gate_error_without_retry(self) -> None:
        client = GATE.W3CClient("127.0.0.1", 1)
        calls = 0

        def fatal() -> None:
            nonlocal calls
            calls += 1
            raise GATE.GateError("inference failed")

        with self.assertRaisesRegex(GATE.GateError, "inference failed"):
            client.wait(fatal, "must abort", 1)
        self.assertEqual(calls, 1)

    def test_direct_w3c_session_uses_native_webkit_capability(self) -> None:
        captured: list[tuple[str, str, object]] = []

        class Handler(http.server.BaseHTTPRequestHandler):
            def log_message(self, *_args) -> None:
                return

            def _handle(self) -> None:
                length = int(self.headers.get("Content-Length", "0"))
                payload = json.loads(self.rfile.read(length)) if length else None
                captured.append((self.command, self.path, payload))
                if self.command == "POST" and self.path == "/session":
                    value = {"sessionId": "sealed-session", "capabilities": {}}
                else:
                    value = None
                raw = json.dumps({"value": value}).encode()
                self.send_response(200)
                self.send_header("Content-Type", "application/json")
                self.send_header("Content-Length", str(len(raw)))
                self.end_headers()
                self.wfile.write(raw)

            do_POST = _handle
            do_DELETE = _handle

        server = http.server.ThreadingHTTPServer(("127.0.0.1", 0), Handler)
        thread = threading.Thread(target=server.serve_forever, daemon=True)
        thread.start()
        client = GATE.W3CClient("127.0.0.1", server.server_port)
        client.new_session(Path("/published/arc-desktop-linux-x86_64.AppImage"))
        client.close()
        server.shutdown()
        server.server_close()
        thread.join(timeout=3)
        capability = captured[0][2]["capabilities"]["alwaysMatch"]
        self.assertEqual(
            capability["webkitgtk:browserOptions"]["binary"],
            "/published/arc-desktop-linux-x86_64.AppImage",
        )
        self.assertIs(capability["acceptInsecureCerts"], False)
        self.assertEqual(captured[-1][:2], ("DELETE", "/session/sealed-session"))

    def test_transcript_forbids_execute_and_page_source(self) -> None:
        with tempfile.TemporaryDirectory() as raw:
            path = Path(raw) / "w3c.json"
            base = {
                "duration_ms": 1,
                "method": "POST",
                "path": "/session",
                "request_sha256": "a" * 64,
                "response_bytes": 2,
                "response_sha256": "b" * 64,
                "status": 200,
            }
            end = {**base, "method": "DELETE", "path": "/session/s"}
            path.write_bytes(GATE.canonical_json([base, end]))
            self.assertEqual(len(GATE.validate_w3c_transcript(path)), 2)
            execute = {**base, "path": "/session/s/execute/sync"}
            path.write_bytes(GATE.canonical_json([base, execute, end]))
            with self.assertRaisesRegex(GATE.GateError, "forbidden"):
                GATE.validate_w3c_transcript(path)


class ContractTests(unittest.TestCase):
    def test_guest_authenticates_raw_appimage_before_any_execution(self) -> None:
        source = inspect.getsource(GATE.run_guest)
        verified = source.index("verify_updater_signature_in_guest(")
        chmodded = source.index("os.chmod(appimage, 0o500)")
        extracted = source.index('"--appimage-extract"')
        launched = source.index("client.new_session(appimage)")
        self.assertLess(verified, chmodded)
        self.assertLess(chmodded, extracted)
        self.assertLess(extracted, launched)

    def test_guest_parser_requires_staged_updater_public_key(self) -> None:
        parsed = GATE.build_parser().parse_args(
            [
                "guest-run",
                "--runtime-root",
                "/var/tmp/arc-packaged-live-v080-aaaaaaaa-aaaaaa",
                "--binding",
                "/tmp/binding",
                "--asset-directory",
                "/tmp/assets",
                "--updater-public-key",
                "/tmp/assets/updater-public-key.b64",
                "--proxy-host",
                GATE.GUEST_HOST_ALIAS,
                "--proxy-port",
                "12345",
                "--proxy-token-file",
                "/tmp/token",
                "--attempt",
                "/tmp/attempt",
            ]
        )
        self.assertEqual(parsed.updater_public_key.name, "updater-public-key.b64")

    def test_contract_is_explicitly_linux_only_and_no_recovery_mount(self) -> None:
        contract = GATE.implementation_contract()
        self.assertIn("does not prove the macOS", contract["package"]["platform_claim"])
        self.assertEqual(contract["vm"]["mounts"], [])
        self.assertEqual(contract["vm"]["image_digest"], GATE.LIMA_IMAGE_DIGEST)
        self.assertEqual(contract["apt"]["snapshot"], GATE.APT_SNAPSHOT)
        self.assertIn("b3sum", contract["apt"]["packages"])
        self.assertIn("minisign", contract["apt"]["packages"])
        self.assertEqual(contract["dispatch_budget"]["inference_submit_clicks"], 1)
        self.assertEqual(contract["dispatch_budget"]["automatic_retries"], 0)
        self.assertEqual(
            contract["dispatch_budget"]["total_guest_timeout_seconds"],
            GATE.GUEST_GATE_TIMEOUT_SECONDS,
        )
        self.assertEqual(
            contract["dispatch_budget"]["host_guest_command_timeout_seconds"],
            GATE.HOST_GUEST_COMMAND_TIMEOUT_SECONDS,
        )
        self.assertGreater(
            GATE.HOST_GUEST_COMMAND_TIMEOUT_SECONDS,
            GATE.GUEST_GATE_TIMEOUT_SECONDS,
        )
        self.assertEqual(
            contract["dispatch_budget"]["host_guest_command_timeout_seconds"],
            GATE.HOST_GUEST_COMMAND_TIMEOUT_SECONDS,
        )
        self.assertGreater(
            GATE.HOST_GUEST_COMMAND_TIMEOUT_SECONDS,
            GATE.GUEST_GATE_TIMEOUT_SECONDS,
        )
        self.assertGreater(
            GATE.GUEST_GATE_TIMEOUT_SECONDS,
            GATE.INFERENCE_WAIT_SECONDS + GATE.RECEIPT_UI_WAIT_SECONDS,
        )
        self.assertIn("POST /session", contract["w3c"]["endpoints"])
        self.assertNotIn("execute sync", contract["w3c"]["endpoints"])

    def test_lima_config_pins_image_and_disables_mounts_and_proxy_propagation(self) -> None:
        config = GATE.lima_config().decode("ascii")
        self.assertIn(GATE.LIMA_IMAGE_DIGEST, config)
        self.assertIn("mounts: []", config)
        self.assertIn("propagateProxyEnv: false", config)
        self.assertIn("loadDotSSHPubKeys: false", config)
        self.assertIn("forwardAgent: false", config)
        self.assertIn("system: false", config)
        self.assertIn("user: false", config)

    def test_create_file_is_create_only_and_private(self) -> None:
        with tempfile.TemporaryDirectory() as raw:
            path = Path(raw) / "receipt.json"
            fsync_kinds: list[str] = []
            real_fsync = os.fsync

            def recording_fsync(fd: int) -> None:
                fsync_kinds.append(
                    "directory" if GATE.stat.S_ISDIR(os.fstat(fd).st_mode) else "file"
                )
                real_fsync(fd)

            with mock.patch.object(GATE.os, "fsync", side_effect=recording_fsync):
                GATE.create_file(path, b"{}\n", 0o400)
            self.assertEqual(path.stat().st_mode & 0o777, 0o400)
            self.assertEqual(fsync_kinds, ["file", "directory"])
            with self.assertRaises(FileExistsError):
                GATE.create_file(path, b"replacement\n", 0o400)

    def test_host_output_directory_is_private_and_parent_durable(self) -> None:
        with tempfile.TemporaryDirectory() as raw:
            parent = Path(raw)
            os.chmod(parent, 0o700)
            output = parent / "one-shot-evidence"
            fsync_kinds: list[str] = []
            real_fsync = os.fsync

            def recording_fsync(fd: int) -> None:
                fsync_kinds.append(
                    "directory" if GATE.stat.S_ISDIR(os.fstat(fd).st_mode) else "file"
                )
                real_fsync(fd)

            with mock.patch.object(GATE.os, "fsync", side_effect=recording_fsync):
                GATE.create_durable_host_output_dir(output)
            self.assertTrue(output.is_dir())
            self.assertFalse(output.is_symlink())
            self.assertEqual(output.stat().st_mode & 0o777, 0o700)
            self.assertEqual(fsync_kinds, ["directory", "directory"])
            with self.assertRaises(FileExistsError):
                GATE.create_durable_host_output_dir(output)

    def test_host_envelope_rejects_vm_source_runtime_and_transport_mutations(self) -> None:
        with tempfile.TemporaryDirectory() as raw:
            root = Path(raw)
            fixture = Fixture(root)
            (root / "lima.yaml").write_bytes(GATE.lima_config())
            ssh_stdout = b""
            ssh_stderr = b"pinned host key accepted\n"
            (root / "ssh.stdout").write_bytes(ssh_stdout)
            (root / "ssh.stderr").write_bytes(ssh_stderr)
            identities = {
                "/opt/homebrew/bin/limactl": {
                    "path": "/opt/homebrew/bin/limactl",
                    "resolved_path": "/opt/homebrew/Cellar/lima/2.1.1/bin/limactl",
                    "sha256": "1" * 64,
                    "size": 123,
                },
                "/usr/bin/ssh": {
                    "path": "/usr/bin/ssh",
                    "resolved_path": "/usr/bin/ssh",
                    "sha256": "2" * 64,
                    "size": 456,
                },
            }
            expected_assets = {
                name: {"name": name, **fixture.binding["assets"][name]}
                for name in (GATE.APPIMAGE_NAME, GATE.APPIMAGE_SIGNATURE_NAME, GATE.NODE_NAME)
            }
            gate_path = Path(GATE.__file__).resolve()
            receipt = {
                "disposable_vm": {
                    "config_sha256": digest(GATE.lima_config()),
                    "deleted_after_evidence_copy": True,
                    "image_digest": GATE.LIMA_IMAGE_DIGEST,
                    "image_url": GATE.LIMA_IMAGE_URL,
                    "mounts": [],
                    "name": "arc-packaged-live-v080-aaaaaaaa-aaaaaa",
                    "preexisting_instances_unchanged": True,
                    "recovery_enclave_accessed": False,
                },
                "host_runtime": {
                    "limactl": identities["/opt/homebrew/bin/limactl"],
                    "limactl_version": f"limactl version {GATE.LIMA_VERSION}",
                    "ssh": identities["/usr/bin/ssh"],
                },
                "release": {
                    "assets": expected_assets,
                    "binding_sha256": digest(fixture.binding_raw),
                    "commit": fixture.commit,
                    "release_id": fixture.binding["release"]["id"],
                    "repository": GATE.REPOSITORY,
                    "tag": GATE.TAG,
                },
                "source": {
                    "commit": fixture.commit,
                    "gate_path": gate_path.relative_to(ROOT).as_posix(),
                    "gate_sha256": GATE.sha256_file(gate_path),
                    "tree_clean": True,
                },
                "transport": {
                    "accepted_connect_count": 1,
                    "identity_sha256": "3" * 64,
                    "known_hosts_sha256": "4" * 64,
                    "relay_log_sha256": "5" * 64,
                    "relay_target": f"{GATE.LAX_HOST}:{GATE.LAX_PORT}",
                    "relay_token_sha256": "6" * 64,
                    "remote": f"{GATE.LAX_USER}@{GATE.LAX_HOST}:127.0.0.1:{GATE.LAX_PORT}",
                    "ssh_options_sha256": "7" * 64,
                    "ssh_stderr_sha256": digest(ssh_stderr),
                    "ssh_stdout_sha256": digest(ssh_stdout),
                },
            }

            def executable(path: Path, _label: str) -> dict:
                return identities[os.fspath(path)]

            with mock.patch.object(GATE, "executable_identity", side_effect=executable):
                GATE.validate_host_receipt_envelope(
                    receipt, fixture.binding, fixture.binding_raw, root
                )
                mutations = (
                    ("VM mount", lambda value: value["disposable_vm"].update(mounts=["/Users"])),
                    ("source", lambda value: value["source"].update(gate_sha256="0" * 64)),
                    ("runtime", lambda value: value["host_runtime"]["ssh"].update(sha256="0" * 64)),
                    ("transport", lambda value: value["transport"].update(remote="root@other")),
                )
                for label, mutate in mutations:
                    changed = json.loads(json.dumps(receipt))
                    mutate(changed)
                    with self.subTest(label=label), self.assertRaises(GATE.GateError):
                        GATE.validate_host_receipt_envelope(
                            changed, fixture.binding, fixture.binding_raw, root
                        )

    def test_connect_relay_observations_are_exact_and_byte_carrying(self) -> None:
        transport = {
            "accepted_connect_count": 1,
            "relay_token_sha256": "6" * 64,
        }
        relay = {
            "accepted_connections": 1,
            "events": [
                {
                    "accepted": True,
                    "bytes_client_to_lax": 100,
                    "bytes_lax_to_client": 200,
                    "peer": "127.0.0.1",
                    "reason": "exact_lax_tls",
                    "target": f"{GATE.LAX_HOST}:{GATE.LAX_PORT}",
                }
            ],
            "listen": "127.0.0.1",
            "listen_port": 12345,
            "rejected_connections": 0,
            "started_at": "2026-09-06T00:00:00Z",
            "target": f"{GATE.LAX_HOST}:{GATE.LAX_PORT}",
            "token_sha256": "6" * 64,
            "upstream": "127.0.0.1:23456",
        }
        GATE.validate_connect_relay_evidence(relay, transport)
        for field in ("target", "bytes_lax_to_client"):
            changed = json.loads(json.dumps(relay))
            changed["events"][0][field] = "other" if field == "target" else 0
            with self.subTest(field=field), self.assertRaises(GATE.GateError):
                GATE.validate_connect_relay_evidence(changed, transport)

    def test_package_tree_rejects_escaping_symlink_and_entry_overflow(self) -> None:
        with tempfile.TemporaryDirectory() as raw:
            parent = Path(raw)
            tree = parent / "tree"
            tree.mkdir()
            (parent / "outside").write_text("outside", encoding="ascii")
            (tree / "escape").symlink_to(parent / "outside")
            with self.assertRaisesRegex(GATE.GateError, "symlink escapes"):
                GATE.package_tree_manifest(tree)
            (tree / "escape").unlink()
            (tree / "one").write_text("1", encoding="ascii")
            (tree / "two").write_text("2", encoding="ascii")
            with mock.patch.object(GATE, "MAX_PACKAGE_TREE_ENTRIES", 1):
                with self.assertRaisesRegex(GATE.GateError, "entry bound"):
                    GATE.package_tree_manifest(tree)


if __name__ == "__main__":
    unittest.main()
