#!/usr/bin/env python3
"""Drive the packaged ARC Linux AppImage through its real UI on a disposable CI runner.

Reuses the W3C WebKitWebDriver client and data-testid contract from the exact
release commit's scripts/release/packaged-appimage-live-gate.py. Unlike that
gate, the runner itself is the disposable host (no Lima VM, no SSH relay), so
production HTTPS egress is direct. The flow is the gate's: fresh onboarding
(observer, no model), managed arc-node start, dashboard, one Inference-screen
request, and (full mode) the UI's mined 0x25 settlement receipt. Paid mode
instead funds the fresh wallet from the app's faucet, submits one native paid
request that must finalize, replays its journaled signed bytes (must not be
admitted twice), and submits a short-expiry request that must be refunded.

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
    parser.add_argument("--mode", choices=("rehearsal", "full", "paid"), required=True)
    parser.add_argument("--wallet-host", default="")
    parser.add_argument("--price", default="0.01")
    parser.add_argument("--reserve", default="0.02")
    parser.add_argument("--expiry-blocks", default="6000")
    parser.add_argument("--refund-expiry-blocks", default="60")
    parser.add_argument("--finalize-wait-seconds", type=int, default=2400)
    args = parser.parse_args()
    if args.mode == "paid":
        allowed = {"https://" + ip for ip in ("149.28.32.76", "140.82.16.112", "136.244.109.1",
                                               "104.238.171.11", "202.182.107.41", "149.28.153.31")}
        if args.wallet_host not in allowed:
            raise SystemExit("paid mode requires --wallet-host naming one validator's public HTTPS edge")
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
    squashfs = work / "squashfs-root"
    app_run = squashfs / "AppRun"
    result["apprun"] = {"is_symlink": app_run.is_symlink(), "resolves_to": str(app_run.resolve().relative_to(squashfs.resolve()))}
    candidates = sorted(p for p in squashfs.rglob(gate.EXPECTED_APP_BINARY) if p.is_file() and not p.is_symlink())
    if len(candidates) != 1:
        raise RuntimeError(f"expected exactly one {gate.EXPECTED_APP_BINARY} ELF, found {candidates}")
    app_binary = candidates[0]
    result["app_binary_path"] = str(app_binary.relative_to(squashfs))
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
    if args.wallet_host:
        # Documented chain-host override (commands.rs chain_host): pins balance,
        # faucet, native context/submit/receipt reads to this one validator.
        environment["ARC_WALLET_HOST"] = args.wallet_host
        result["wallet_host"] = args.wallet_host
    port = gate.free_loopback_port()
    driver_out = (evidence / "webkit-driver.stdout").open("xb")
    driver_err = (evidence / "webkit-driver.stderr").open("xb")
    driver = subprocess.Popen(["/usr/bin/WebKitWebDriver", "--host=127.0.0.1", f"--port={port}"],
                              cwd=work, env=environment, stdin=subprocess.DEVNULL,
                              stdout=driver_out, stderr=driver_err, start_new_session=True)
    client = gate.W3CClient("127.0.0.1", port)
    testid = gate.testid
    failure = None
    seed_visible = False

    def snap(name: str) -> None:
        """Best-effort evidence screenshot; the UI flow never depends on it."""
        last = None
        for _ in range(5):
            try:
                result["screenshots"].append(client.screenshot(evidence / name))
                return
            except Exception as error:  # small/blank frames are retried
                last = error
                time.sleep(2)
        result["screenshots"].append({"name": name, "error": str(last)[:200]})

    try:
        gate.wait_tcp_listener(driver, port)
        client.new_session(appimage)
        app = gate.wait_exact_process(gate.EXPECTED_APP_BINARY, result["app_binary_sha256"], timeout=45)[0]
        result["app_process"] = app
        if client.elements(testid("production-browser-blocker")) or client.elements(testid("synthetic-preview-banner")):
            raise RuntimeError("packaged app rendered a blocker/preview banner")
        client.wait_element(testid("step-welcome"), 30)
        snap("01-welcome.png")
        client.click(client.element(testid("btn-continue-welcome")))
        client.wait_element(testid("step-identity"), 15)
        address = gate.element_text(client, "identity-address", 30).strip().lower()
        result["identity_address"] = address if address.startswith("0x") else "0x" + address
        snap("02-identity-blurred.png")
        seed_visible = True
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
        seed_visible = False
        client.wait_element(testid("step-model"), 15)
        client.click(client.element(testid("tier-skip")))
        client.click(client.element(testid("btn-continue-model")))
        client.wait_element(testid("step-launch"), 15)
        snap("03-observer-launch.png")
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
        snap("04-dashboard.png")
        result["tests"].append({"name": "managed node started from dashboard", "status": "passed"})

        if args.mode == "paid":
            paid_flow(client, gate, testid, args, profile, evidence, result, snap)
            return_value = None
        else:
            return_value = free_request(client, gate, testid, args, result, snap)
    except BaseException as error:  # preserve every partial observation
        failure = f"{type(error).__name__}: {error}"
        try:
            if not seed_visible:
                snap("99-failure.png")
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
        print(json.dumps({k: result.get(k) for k in ("mode", "pass", "failure", "inference", "settlement", "dashboard", "paid")}, indent=2, default=str))
    return 0 if failure is None else 1


def free_request(client, gate, testid, args, result, snap):
    if True:
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
        snap("05-inference-result.png")
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
            snap("06-settlement.png")
            result["tests"].append({"name": "UI shows mined 0x25 settlement", "status": "passed"})
    return None


def read_journal(profile: Path) -> list:
    found = sorted(profile.rglob("native-requests.json"))
    if len(found) != 1:
        raise RuntimeError(f"expected one native request journal, found {found}")
    data = json.loads(found[0].read_text())
    records = data.get("records", data) if isinstance(data, dict) else data
    return records if isinstance(records, list) else []


def journal_entry(profile: Path, known: set) -> dict:
    fresh = [r for r in read_journal(profile) if r.get("request_id") and r.get("request_id") not in known
             and str(r.get("kind", "")).lower().endswith("request")]
    if len(fresh) != 1:
        raise RuntimeError(f"expected exactly one new journaled request, found {len(fresh)}")
    return fresh[0]



def click_centered(client, element_id: str) -> None:
    """Scroll the element to the viewport centre, then click it (as a user would).

    At the 1280x900 window the native paid card's buttons sit at the bottom edge,
    under the Inference screen's sticky composer, so a W3C click on the default
    scroll position is intercepted (HTTP 400). Centring first keeps the click real.
    """
    client.request("POST", client._path("execute/sync"),
                   {"script": "arguments[0].scrollIntoView({block: 'center', inline: 'nearest'});",
                    "args": [{client.ELEMENT_KEY: element_id}]})
    time.sleep(0.3)
    client.click(element_id)

def paid_flow(client, gate, testid, args, profile, evidence, result, snap):
    import urllib.request

    paid: dict = {"requests": []}
    result["paid"] = paid

    # 1. Fund the fresh wallet through the app's own faucet button.
    client.click(client.element(testid("nav-wallet")))
    client.wait_element(testid("wallet-screen"), 20)
    client.click(client.wait_element(testid("btn-faucet"), 30))

    def faucet_done():
        errors = client.elements(testid("faucet-error"))
        if errors:
            raise RuntimeError("faucet failed: " + client.text(errors[0])[:300])
        ok = client.elements(testid("faucet-success"))
        return ok[0] if ok else None

    paid["faucet"] = client.text(client.wait(faucet_done, "faucet result", 120))[:300]

    def funded():
        text = client.text(client.element(testid("wallet-balance"))).replace(",", "")
        digits = "".join(ch for ch in text if ch.isdigit() or ch == ".")
        try:
            return text if digits and float(digits) >= float(args.reserve) * 2 else None
        except ValueError:
            return None

    paid["balance_after_faucet"] = client.wait(funded, "faucet credit mined into the wallet balance", 300)
    snap("10-wallet-funded.png")
    result["tests"].append({"name": "wallet funded from the app faucet", "status": "passed"})

    # 2. The paid panel on the Inference screen.
    client.click(client.element(testid("nav-inference")))
    client.wait_element(testid("inference-screen"), 15)
    client.wait_element(testid("native-paid-card"), 120)
    status = client.elements(testid("native-context-status"))
    paid["context_status"] = client.text(status[0])[:400] if status else None
    snap("11-native-panel.png")
    known: set = set()

    def submit(expiry_blocks: str, label: str) -> dict:
        for tid, value in (("native-prompt", args.prompt), ("native-max-tokens", args.max_tokens),
                           ("native-price", args.price), ("native-reserve", args.reserve),
                           ("native-expiry", expiry_blocks)):
            field = client.element(testid(tid))
            client.clear(field)
            client.send_keys(field, value)
        click_centered(client, client.element(testid("btn-native-review")))

        def reviewed():
            errors = client.elements(testid("native-form-error"))
            if errors:
                raise RuntimeError("paid form refused: " + client.text(errors[0])[:300])
            found = client.elements(testid("native-review"))
            return found[0] if found else None

        client.wait(reviewed, "paid request review", 60)
        if client.elements(testid("native-positions-error")):
            raise RuntimeError("paid request exceeds executor positions: " +
                               client.text(client.element(testid("native-positions-error")))[:300])
        sign = client.element(testid("btn-native-sign"))
        if client.attribute(sign, "disabled") is not None:
            raise RuntimeError("sign button disabled (panel not compatible or admission closed)")
        snap(f"12-{label}-review.png")
        click_centered(client, sign)
        started = time.time()

        def journaled():
            try:
                return journal_entry(profile, known)
            except RuntimeError:
                return None

        entry = client.wait(journaled, f"{label} signed and journaled", 120)
        known.add(entry["request_id"])
        record = {"label": label, "request_id": entry["request_id"], "tx_hash": entry.get("tx_hash"),
                  "nonce": entry.get("nonce"), "expires_at": entry.get("expires_at"),
                  "signed_at_unix": started}
        paid["requests"].append(record)
        return record, entry

    def row_for(request_id: str):
        prefix = request_id.lower().removeprefix("0x")[:8]
        for row in client.elements(testid("native-request-row")):
            if prefix in client.text(row).lower():
                return row
        return None

    def wait_phase(record: dict, wanted: set, failing: set, seconds: int, what: str) -> str:
        phases = []

        def check():
            row = row_for(record["request_id"])
            if row is None:
                return None
            phase = client.attribute(row, "data-phase")
            if not phases or phases[-1] != phase:
                phases.append(phase)
            if phase in failing:
                raise RuntimeError(f"{record['label']} reached {phase}: " + client.text(row)[:400])
            return phase if phase in wanted else None

        try:
            return client.wait(check, what, seconds)
        finally:
            record["phases_seen"] = phases

    # 3. Request A must be admitted, executed by the validators and finalized.
    a, a_entry = submit(args.expiry_blocks, "paid-a")
    wait_phase(a, {"finalized"}, {"rejected", "dropped", "refund_due", "refunded"},
               args.finalize_wait_seconds, "paid request A finalized")
    row = row_for(a["request_id"])
    a["row_text"] = client.text(row)[:1200]
    outputs = client.elements(testid("native-output"))
    a["output_text"] = client.text(outputs[0])[:800] if outputs else None
    a["finalized_at_unix"] = time.time()
    snap("13-paid-a-finalized.png")
    result["tests"].append({"name": "native paid request finalized in the app", "status": "passed"})

    # 4. Replay: resubmitting A's exact signed bytes must not admit or charge it twice.
    signed = a_entry.get("signed_tx")
    if signed:
        body = json.dumps(signed).encode()
        request = urllib.request.Request(args.wallet_host + "/tx/submit_signed", data=body,
                                         headers={"Content-Type": "application/json"}, method="POST")
        try:
            with urllib.request.urlopen(request, timeout=20) as response:
                a["replay"] = {"http_status": response.status, "body": response.read(2000).decode("utf-8", "replace")}
        except urllib.error.HTTPError as error:
            a["replay"] = {"http_status": error.code, "body": error.read(2000).decode("utf-8", "replace")}
    else:
        a["replay"] = {"skipped": "journal no longer holds the signed bytes"}

    # 5. Request B expires before any certificate can exist; the app claims the reservation back.
    b, _ = submit(args.refund_expiry_blocks, "paid-b-refund")
    wait_phase(b, {"refund_due"}, {"rejected", "dropped", "finalized"}, 900, "request B refund due")
    snap("14-paid-b-refund-due.png")
    click_centered(client, client.wait_element(testid("btn-native-refund"), 60))
    wait_phase(b, {"refunded"}, {"rejected"}, 900, "request B refunded")
    b["row_text"] = client.text(row_for(b["request_id"]))[:1200]
    snap("15-paid-b-refunded.png")
    result["tests"].append({"name": "expired paid request refunded through the app", "status": "passed"})

    journal_copy = evidence / "native-requests-journal.json"
    journal_copy.write_text(json.dumps(read_journal(profile), indent=2, default=str) + "\n")


if __name__ == "__main__":
    raise SystemExit(main())
