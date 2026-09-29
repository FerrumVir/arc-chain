#!/usr/bin/env python3
"""Drive the packaged ARC Linux AppImage through its real UI on a disposable CI runner.

Reuses the W3C WebKitWebDriver client and data-testid contract from the exact
release commit's scripts/release/packaged-appimage-live-gate.py. Unlike that
gate, the runner itself is the disposable host (no Lima VM, no SSH relay), so
production HTTPS egress is direct. The flow is the gate's: fresh onboarding
(observer, no model), managed arc-node start, dashboard, one Inference-screen
request, and (full mode) the UI's mined 0x25 settlement receipt.

No screenshot, page-source or element-text request is made while the fresh
recovery phrase is revealed.
"""
from __future__ import annotations

import argparse
import hashlib
import importlib.util
import json
import os
from pathlib import Path
import shutil
import subprocess
import sys
import time


def load_gate(path: Path):
    spec = importlib.util.spec_from_file_location("arc_appimage_gate", path)
    module = importlib.util.module_from_spec(spec)
    sys.modules["arc_appimage_gate"] = module
    spec.loader.exec_module(module)
    return module


def sha256(path: Path) -> str:
    digest = hashlib.sha256()
    with path.open("rb") as stream:
        for block in iter(lambda: stream.read(1 << 20), b""):
            digest.update(block)
    return digest.hexdigest()


def main() -> int:
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--gate", type=Path, required=True)
    parser.add_argument("--appimage", type=Path, required=True)
    parser.add_argument("--arc-node", type=Path, required=True)
    parser.add_argument("--runtime-root", type=Path, required=True)
    parser.add_argument("--prompt", required=True)
    parser.add_argument("--max-tokens", default="8")
    parser.add_argument("--mode", choices=("rehearsal", "full"), required=True)
    args = parser.parse_args()
    gate = load_gate(args.gate)

    root = args.runtime_root
    root.mkdir(mode=0o700)
    work, profile, evidence = root / "work", root / "profile", root / "evidence"
    for path in (work, profile, evidence, profile / "home", profile / "cache", profile / "config", profile / "data"):
        path.mkdir(mode=0o700, exist_ok=path == root)
    result: dict = {"mode": args.mode, "started_at_unix": time.time(), "tests": [], "screenshots": []}

    managed_bin = profile / "home" / ".arc" / "bin"
    managed_bin.mkdir(parents=True, mode=0o700)
    node = managed_bin / "arc-node"
    shutil.copyfile(args.arc_node, node)
    node.chmod(0o500)
    result["arc_node_sha256"] = sha256(node)
    version = subprocess.run([str(node), "--version"], capture_output=True, text=True, timeout=15)
    result["arc_node_version"] = (version.stdout + version.stderr).strip()

    appimage = args.appimage.resolve()
    appimage.chmod(0o500)
    result["appimage_sha256"] = sha256(appimage)
    subprocess.run([str(appimage), "--appimage-extract"], cwd=work, check=True, capture_output=True, timeout=180)
    app_binary = (work / "squashfs-root" / "AppRun").resolve(strict=True)
    if app_binary.name != gate.EXPECTED_APP_BINARY:
        raise RuntimeError(f"AppRun target is {app_binary.name}, not {gate.EXPECTED_APP_BINARY}")
    result["app_binary_sha256"] = sha256(app_binary)

    environment = {key: os.environ[key] for key in ("DBUS_SESSION_BUS_ADDRESS", "DISPLAY", "XAUTHORITY", "XDG_RUNTIME_DIR") if key in os.environ}
    environment.update({
        "APPIMAGE_EXTRACT_AND_RUN": "1",
        "HOME": str(profile / "home"),
        "LANG": "C.UTF-8",
        "NO_AT_BRIDGE": "1",
        "TAURI_AUTOMATION": "true",
        "TAURI_WEBVIEW_AUTOMATION": "true",
        "PATH": "/usr/local/sbin:/usr/local/bin:/usr/sbin:/usr/bin:/sbin:/bin",
        "WEBKIT_DISABLE_COMPOSITING_MODE": "1",
        "XDG_CACHE_HOME": str(profile / "cache"),
        "XDG_CONFIG_HOME": str(profile / "config"),
        "XDG_DATA_HOME": str(profile / "data"),
    })
    port = gate.free_loopback_port()
    driver_out = (evidence / "webkit-driver.stdout").open("xb")
    driver_err = (evidence / "webkit-driver.stderr").open("xb")
    driver = subprocess.Popen(["/usr/bin/WebKitWebDriver", "--host=127.0.0.1", f"--port={port}"],
                              cwd=work, env=environment, stdin=subprocess.DEVNULL,
                              stdout=driver_out, stderr=driver_err, start_new_session=True)
    client = gate.W3CClient("127.0.0.1", port)
    testid = gate.testid
    failure = None
    try:
        gate.wait_tcp_listener(driver, port)
        client.new_session(appimage)
        app = gate.wait_exact_process(gate.EXPECTED_APP_BINARY, result["app_binary_sha256"], timeout=45)[0]
        result["app_process"] = app
        if client.elements(testid("production-browser-blocker")) or client.elements(testid("synthetic-preview-banner")):
            raise RuntimeError("packaged app rendered a blocker/preview banner")
        client.wait_element(testid("step-welcome"), 30)
        result["screenshots"].append(client.screenshot(evidence / "01-welcome.png"))
        client.click(client.element(testid("btn-continue-welcome")))
        client.wait_element(testid("step-identity"), 15)
        address = gate.element_text(client, "identity-address", 30).strip().lower()
        result["identity_address"] = address if address.startswith("0x") else "0x" + address
        result["screenshots"].append(client.screenshot(evidence / "02-identity-blurred.png"))
        client.click(client.element(testid("btn-reveal-seed")))

        def continue_ready():
            if client.elements(testid("seed-error")):
                raise RuntimeError("native seed reveal failed")
            found = client.elements(testid("btn-continue-identity"))
            if not found:
                return None
            return found[0] if client.attribute(found[0], "disabled") is None else None

        client.click(client.wait(continue_ready, "enabled identity acknowledgement", 30))
        client.wait_element(testid("step-model"), 15)
        client.click(client.element(testid("tier-skip")))
        client.click(client.element(testid("btn-continue-model")))
        client.wait_element(testid("step-launch"), 15)
        result["screenshots"].append(client.screenshot(evidence / "03-observer-launch.png"))
        client.click(client.element(testid("btn-launch")))
        result["tests"].append({"name": "onboarding observer launch", "status": "passed"})

        def dashboard_or_error():
            errors = client.elements(testid("launch-error"))
            if errors:
                raise RuntimeError("launch failed: " + client.text(errors[0])[:500])
            dashboards = client.elements(testid("dashboard"))
            return dashboards[0] if dashboards else None

        client.wait(dashboard_or_error, "dashboard after onboarding", 120)
        node_process = gate.wait_exact_process("arc-node", result["arc_node_sha256"], timeout=45)[0]
        result["managed_node_process"] = node_process
        time.sleep(20)
        height = gate.element_text(client, "node-block-height", 30)
        peers_tile = client.wait_element(f'{testid("stat-peers")} .stat-value', 30)
        sidebar = gate.element_text(client, "sidebar-status", 30)
        result["dashboard"] = {"local_block_height_text": height, "local_peers_text": client.text(peers_tile), "sidebar_status": sidebar}
        result["screenshots"].append(client.screenshot(evidence / "04-dashboard.png"))
        result["tests"].append({"name": "managed node started from dashboard", "status": "passed"})

        client.click(client.element(testid("nav-inference")))
        client.wait_element(testid("inference-screen"), 15)
        prompt_element = client.element(testid("inference-prompt"))
        client.clear(prompt_element)
        client.send_keys(prompt_element, args.prompt)
        tokens = client.element(testid("inference-max-tokens"))
        client.clear(tokens)
        client.send_keys(tokens, args.max_tokens)
        if client.property(prompt_element, "value") != args.prompt:
            raise RuntimeError("typed prompt differs")
        result["submit_clicked_at_unix"] = time.time()
        client.click(client.element(testid("btn-run-inference")))

        def inference_or_error():
            errors = client.elements(testid("inference-error"))
            if errors:
                raise RuntimeError("inference failed: " + client.text(errors[0])[:500])
            found = client.elements(testid("inference-result"))
            return found[0] if found else None

        element = client.wait(inference_or_error, "inference result", gate.INFERENCE_WAIT_SECONDS)
        result["inference"] = {
            "completed_at_unix": time.time(),
            "output": gate.element_text(client, "inference-output", 10).strip(),
            "output_hash": client.attribute(element, "data-output-hash"),
            "model_id": client.attribute(element, "data-model-id"),
            "routed_via": client.attribute(element, "data-routed-via"),
            "coordinator": client.attribute(element, "data-coordinator"),
        }
        for label in ("inference-community-worker", "inference-coordinator"):
            found = client.elements(testid(label))
            result["inference"][label] = client.text(found[0]) if found else None
        result["screenshots"].append(client.screenshot(evidence / "05-inference-result.png"))
        result["tests"].append({"name": "inference screen request returned a result", "status": "passed"})

        settlements = client.elements(testid("community-settlement"))
        if args.mode == "full" or settlements:
            def mined():
                found = client.elements(testid("community-settlement"))
                if not found:
                    return None
                status = client.attribute(found[0], "data-receipt-status")
                if status in {"mined_failed", "receipt_unavailable"}:
                    raise RuntimeError("settlement terminal failure: " + str(status))
                return found[0] if status == "mined_success" else None

            settlement = client.wait(mined, "mined 0x25 settlement in UI", gate.RECEIPT_UI_WAIT_SECONDS)
            result["settlement"] = {name: client.attribute(settlement, "data-" + name) for name in (
                "receipt-status", "tx-type", "tx-hash", "job-id", "worker", "receipt-url", "submitted")}
            result["screenshots"].append(client.screenshot(evidence / "06-settlement.png"))
            result["tests"].append({"name": "UI shows mined 0x25 settlement", "status": "passed"})
    except BaseException as error:  # preserve every partial observation
        failure = f"{type(error).__name__}: {error}"
        try:
            result["screenshots"].append(client.screenshot(evidence / "99-failure.png"))
        except BaseException:
            pass
    finally:
        try:
            client.close()
        except BaseException as error:
            result["session_close_error"] = str(error)[:300]
        driver.terminate()
        try:
            driver.wait(timeout=20)
        except subprocess.TimeoutExpired:
            driver.kill()
        result["finished_at_unix"] = time.time()
        result["failure"] = failure
        result["pass"] = failure is None
        (evidence / "ui-request-result.json").write_text(json.dumps(result, indent=2, default=str) + "\n")
        print(json.dumps({k: result.get(k) for k in ("mode", "pass", "failure", "inference", "settlement", "dashboard")}, indent=2, default=str))
    return 0 if failure is None else 1


if __name__ == "__main__":
    raise SystemExit(main())
