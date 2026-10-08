#!/usr/bin/env python3
"""Aggregate the per-OS desktop-updater isolation results into stage-c-summary.json (THROWAWAY LAB FILE).

Two kinds of per-OS input are read from one records directory (searched recursively):

  result-<linux|windows|macos>.json   written by `desktop_updater_check.py run` (schema
                                      arc.legacy-bridge.wave0-lab.stage-c-result.v1): the harness
  any *.json whose schema is          written by the OS jobs (desktop-os-result.v1): the released v0.7.11 app and/or the
  arc.legacy-bridge.wave0-lab.        native tauri-plugin-updater check, run on a real OS with the hosts mapped to a
  desktop-os-result.v1                local fake GitHub

and arc.legacy-bridge.wave0-lab.stage-c.v1 is written. One verdict is printed and recorded per OS:

  PASS      the harness result is present and DESKTOP_UPDATER_ISOLATED, and the desktop-os result (if there is one) PASSes
  FAIL      anything explicit went wrong (a NOT_ISOLATED run, a malformed or inconsistent file, a false criterion, a request the
            criteria deny, different source bytes between OS runs)
  UNPROVED  a desktop-os result is present but something it must prove is unknown or missing
  MISSING   the required result of that OS is not there (default mode: the harness result; --require-desktop-os: the desktop-os result)

Stage verdict: STAGE_C_FAIL if any OS is FAIL (or the runs read different sources); else STAGE_C_INCOMPLETE if any OS is
MISSING or UNPROVED; else STAGE_C_PASS. Exit status 0 only for STAGE_C_PASS.

Two modes. Default: the harness result of every OS is required, a desktop-os result is judged when there is one. With
--require-desktop-os (what the desktop workflow passes): the desktop-os result of every OS is required, the harness result is judged
when there is one (a present harness result that is not DESKTOP_UPDATER_ISOLATED still fails the OS) but its absence does not matter.

desktop-os-result.v1 is judged conservatively: only an explicit positive (true / "PASS") counts as proof, an explicit negative
(false / "FAIL") is a failure, and EVERY unknown or missing criterion is UNPROVED, never PASS. PASS needs, for the OS:
  * its own verdict PASS; at least one attempted tier (tiers.released_app / tiers.native_check) and every attempted tier PASS;
  * a PASSing case named "clean" and a PASSing case named "cached-bait", each with all five criteria (only_manifest_url,
    no_bundle_download, no_install, no_new_app_launch, no_new_files) explicitly true, its own verdict PASS, a request log whose
    total equals the sum of its counts and that contains no bundle, signature, v0.8 or v0.7.11 asset;
  * isolation.live_block true and hosts_mapped listed; for the released-app tier app.digest_match true and the v0.7.11 tag;
    plugin.version 2.10.1; manifest404_error_text containing the ReleaseNotFound text when a latest-404 case ran;
  * when the result lists `controls` (positive controls proving the recorder can see a bundle request), none of them FAILs or is unproved.
The `os` value is read tolerantly (macos, macos-arm64, darwin ... all mean macos).

The per-label `native` field of the harness says whether the REAL tauri-plugin-updater was exercised by it (assertions I8 and
I9): PASS, FAIL or SKIP (no native checker was supplied there). SKIP never fails the harness and never counts as native proof.

Usage: stage_c_summary.py build --records DIR --out FILE [--require-desktop-os]
  --require-desktop-os   the desktop-os result of every OS is required (see above)
"""
from __future__ import annotations

import argparse
import hashlib
import json
import os
import re
import sys
from pathlib import Path
from typing import Any, Dict, List, Optional, Tuple

SUMMARY_SCHEMA = "arc.legacy-bridge.wave0-lab.stage-c.v1"
RESULT_SCHEMA = "arc.legacy-bridge.wave0-lab.stage-c-result.v1"
OS_SCHEMA = "arc.legacy-bridge.wave0-lab.desktop-os-result.v1"
LABELS = ("linux", "windows", "macos")
ISOLATED = "DESKTOP_UPDATER_ISOLATED"
NOT_ISOLATED = "DESKTOP_UPDATER_NOT_ISOLATED"
REQUIRED_ASSERTIONS = tuple(f"I{n}" for n in range(1, 10))

REPO = "FerrumVir/arc-chain"
TAG = "v0.7.11"
PLUGIN_VERSION = "2.10.1"
RELEASE_NOT_FOUND = "Could not fetch a valid release JSON from the remote"  # tauri-plugin-updater 2.10.1 src/error.rs:25
CASE_NAMES = ("clean", "cached-bait")
TIER_NAMES = ("released_app", "native_check")
REQUIRED_CRITERIA = ("only_manifest_url", "no_bundle_download", "no_install", "no_new_app_launch", "no_new_files")
MANIFEST_PATHS = (f"/{REPO}/releases/latest/download/latest.json", f"/repos/{REPO}/releases/latest")
MANIFEST_PREFIX = f"/{REPO}/releases/download/"  # .../<Latest tag>/latest.json is where the endpoint redirects to
# the same patterns as desktop_updater_check.FORBIDDEN_PATH_PATTERNS (a test keeps the two lists equal)
FORBIDDEN_PATH_PATTERNS = [
    (re.compile(rf"^/{re.escape(REPO)}/releases/download/v0\.8\."), "a v0.8 release asset"),
    (re.compile(rf"^/{re.escape(REPO)}/releases/download/v0\.7\.11/"), "a v0.7.11 release asset"),
    (re.compile(r"(\.AppImage|\.deb|\.rpm|\.dmg|\.msi|-setup\.exe|\.app\.tar\.gz)$"), "an app bundle"),
    (re.compile(r"\.sig$"), "a signature file"),
]
POSITIVE = {"PASS", "PASSED", "OK", "PROVED", "TRUE", "YES", "ISOLATED", ISOLATED}
NEGATIVE = {"FAIL", "FAILED", "FALSE", "NO", "VIOLATED", NOT_ISOLATED, "NOT_ISOLATED"}
MAX_JSON_BYTES = 8 * 1024 * 1024
MAX_HASHED_EVIDENCE_BYTES = 64 * 1024 * 1024


def sha256_bytes(data: bytes) -> str:
    return hashlib.sha256(data).hexdigest()


def native_status(assertions: Dict[str, str]) -> str:
    states = [assertions.get("I8"), assertions.get("I9")]
    if "FAIL" in states:
        return "FAIL"
    if states == ["PASS", "PASS"]:
        return "PASS"
    return "SKIP"


# --------------------------------------------------------------------------------------------------------------
# the harness result (desktop_updater_check.py run)
# --------------------------------------------------------------------------------------------------------------

def load_result(path: Path, label: str) -> Tuple[Dict, List[str]]:
    """(per-label record, problems). The record is empty when the file cannot be used at all."""
    raw = path.read_bytes()
    problems: List[str] = []
    try:
        document = json.loads(raw.decode("utf-8"))
    except ValueError as error:
        return {}, [f"{path.name}: not JSON ({error})"]
    if not isinstance(document, dict):
        return {}, [f"{path.name}: not a JSON object"]
    if document.get("schema") != RESULT_SCHEMA:
        problems.append(f"{path.name}: schema {document.get('schema')!r}, expected {RESULT_SCHEMA}")
    if document.get("label") != label:
        problems.append(f"{path.name}: label {document.get('label')!r}, expected {label!r}")
    assertions = document.get("assertions")
    states: Dict[str, str] = {}
    if not isinstance(assertions, list):
        problems.append(f"{path.name}: assertions missing")
    else:
        for entry in assertions:
            if isinstance(entry, dict) and isinstance(entry.get("id"), str) and entry.get("result") in ("PASS", "FAIL", "SKIP"):
                states[entry["id"]] = entry["result"]
            else:
                problems.append(f"{path.name}: malformed assertion {entry!r}"[:200])
        missing = [name for name in REQUIRED_ASSERTIONS if name not in states]
        if missing:
            problems.append(f"{path.name}: assertions {missing} are missing")
        # I1-I7 are never allowed to be anything but PASS in an isolated result; SKIP is only legal for I8 and I9
        skipped = [name for name in REQUIRED_ASSERTIONS[:7] if states.get(name) == "SKIP"]
        if skipped:
            problems.append(f"{path.name}: assertions {skipped} are SKIP but only I8 and I9 may be skipped")
    verdict = document.get("verdict")
    if verdict not in (ISOLATED, NOT_ISOLATED):
        problems.append(f"{path.name}: verdict {verdict!r}")
    elif states:
        recomputed = NOT_ISOLATED if "FAIL" in states.values() else ISOLATED
        if recomputed != verdict:
            problems.append(f"{path.name}: verdict {verdict} does not follow from its assertions ({recomputed})")
    count = document.get("request_count")
    if not isinstance(count, int) or isinstance(count, bool) or count < 1:
        problems.append(f"{path.name}: request_count {count!r}")
    plugin = document.get("plugin_semantics") if isinstance(document.get("plugin_semantics"), dict) else {}
    live = document.get("live_observation") if isinstance(document.get("live_observation"), dict) else {}
    record = {
        "verdict": verdict,
        "request_count": count,
        "sha256": sha256_bytes(raw),
        "native": native_status(states),
        "plugin_semantics": plugin.get("status"),
        "live_observation_supplied": live.get("supplied") is True,
        "failed_assertions": sorted(name for name, state in states.items() if state == "FAIL"),
        "python": document.get("python"),
        "platform": document.get("platform"),
        "asset": document.get("asset"),
        "assertions": dict(sorted(states.items())),
        "sources": document.get("sources") if isinstance(document.get("sources"), dict) else {},
    }
    return record, problems


# --------------------------------------------------------------------------------------------------------------
# the OS jobs' result (desktop-os-result.v1): PASS only on explicit proof
# --------------------------------------------------------------------------------------------------------------

def norm(value: Any) -> str:
    """PASS / FAIL / UNPROVED. Only an explicit positive is PASS, only an explicit negative is FAIL; anything else is UNPROVED."""
    if value is True:
        return "PASS"
    if value is False:
        return "FAIL"
    if isinstance(value, str):
        text = value.strip().upper()
        if text in POSITIVE:
            return "PASS"
        if text in NEGATIVE:
            return "FAIL"
    return "UNPROVED"


def canonical_os(value: Any) -> Optional[str]:
    """linux / windows / macos from the OS job's own spelling (ubuntu, Windows, macos-arm64, darwin ...); None when unrecognisable."""
    if not isinstance(value, str):
        return None
    text = value.strip().lower()
    if text.startswith(("mac", "darwin", "osx")):
        return "macos"
    if text.startswith(("win",)):
        return "windows"
    if text.startswith(("lin", "ubuntu")):
        return "linux"
    return None


def _inside(directory: Path, relative: str) -> Optional[Path]:
    try:
        candidate = (directory / relative).resolve()
        candidate.relative_to(directory.resolve())
    except (OSError, ValueError):
        return None
    return candidate


def evaluate_case(case: Any, index: int, tier_states: Dict[str, str]) -> dict:
    """One case of a desktop-os result. verdict: PASS | FAIL | UNPROVED, with the reasons for anything but PASS."""
    fails: List[str] = []
    unproved: List[str] = []
    if not isinstance(case, dict):
        return {"id": f"case[{index}]", "name": None, "tier": None, "scenario": None, "verdict": "UNPROVED", "criteria": {}, "requests_total": None, "reasons": ["not an object"]}
    name, tier, scenario = case.get("name"), case.get("tier"), case.get("scenario")
    ident = f"{name}/{tier}/{scenario}"
    criteria = case.get("criteria") if isinstance(case.get("criteria"), dict) else {}
    states = {key: norm(criteria.get(key)) for key in REQUIRED_CRITERIA}
    for key, state in states.items():
        if state == "FAIL":
            fails.append(f"criteria.{key} is false")
        elif state != "PASS":
            unproved.append(f"criteria.{key} is {'missing' if key not in criteria else repr(criteria.get(key))}")
    declared = norm(case.get("verdict"))
    if declared == "FAIL":
        fails.append("its own verdict is FAIL")
    elif declared != "PASS":
        unproved.append(f"its own verdict is {case.get('verdict')!r}")
    if name not in CASE_NAMES:
        unproved.append(f"unknown case name {name!r}")
    if tier not in TIER_NAMES:
        unproved.append(f"unknown tier {tier!r}")
    elif tier_states.get(tier) == "NOT_ATTEMPTED":
        unproved.append(f"tier {tier} was not attempted")
    elif tier_states.get(tier) != "PASS":
        unproved.append(f"tier {tier} is {tier_states.get(tier)}")
    requests = case.get("requests") if isinstance(case.get("requests"), dict) else {}
    total = requests.get("total")
    rows = requests.get("by_host_path")
    parsed: List[Tuple[str, str, int]] = []
    if not isinstance(rows, list) or not rows:
        unproved.append("requests.by_host_path is missing or empty: no recorded requests to check the criteria against")
    else:
        for row in rows:
            if isinstance(row, (list, tuple)) and len(row) == 3 and isinstance(row[0], str) and isinstance(row[1], str) and isinstance(row[2], int) and not isinstance(row[2], bool) and row[2] >= 1:
                parsed.append((row[0], row[1], row[2]))
            else:
                unproved.append(f"requests.by_host_path row {row!r} is not [host, path, count]")
                break
        else:
            if not isinstance(total, int) or isinstance(total, bool) or total != sum(count for _, _, count in parsed):
                unproved.append(f"requests.total {total!r} is not the sum of the counts ({sum(count for _, _, count in parsed)})")
    if scenario == "latest-404" or states.get("no_bundle_download") == "PASS":
        # latest-404: nothing but the manifest may be asked for. Any scenario: a declared no_bundle_download contradicts a bundle, signature or v0.8 request.
        patterns = FORBIDDEN_PATH_PATTERNS if scenario == "latest-404" else [(pattern, label) for pattern, label in FORBIDDEN_PATH_PATTERNS if label != "a v0.7.11 release asset"]
        denied = sorted({f"{host}{path} ({label})" for host, path, _ in parsed for pattern, label in patterns if pattern.search(path)})
        if denied:
            fails.append(f"the recorded requests contain requests the criteria deny: {denied}")
    if scenario == "latest-404":
        extra = sorted({f"{host}{path}" for host, path, _ in parsed if path not in MANIFEST_PATHS and not (path.startswith(MANIFEST_PREFIX) and path.endswith("/latest.json"))})
        if extra and not fails and states.get("only_manifest_url") == "PASS":
            unproved.append(f"only_manifest_url is declared true but the log also holds {extra}")
    verdict = "FAIL" if fails else ("UNPROVED" if unproved else "PASS")
    return {
        "id": ident, "name": name, "tier": tier, "scenario": scenario, "verdict": verdict, "criteria": states,
        "requests_total": total if isinstance(total, int) and not isinstance(total, bool) else None,
        "reasons": fails + unproved,
    }


def evaluate_os_result(document: dict, raw: bytes, path: Path, records: Path) -> dict:
    """desktop-os-result.v1 -> record with verdict PASS | FAIL | UNPROVED and the reasons."""
    fails: List[str] = []
    unproved: List[str] = []
    own = norm(document.get("verdict"))
    if own == "FAIL":
        fails.append("its own verdict is FAIL")
    elif own != "PASS":
        unproved.append(f"its own verdict is {document.get('verdict')!r}, not PASS")

    tiers = document.get("tiers") if isinstance(document.get("tiers"), dict) else {}
    tier_states: Dict[str, str] = {}
    for tier in TIER_NAMES:
        entry = tiers.get(tier)
        if not isinstance(entry, dict) or entry.get("attempted") is not True:
            tier_states[tier] = "NOT_ATTEMPTED"
            continue
        tier_states[tier] = norm(entry.get("result"))
        why = f" (status {entry.get('status')!r}" + (f": {entry.get('reason')}" if entry.get("reason") else "") + ")"
        if tier_states[tier] == "FAIL":
            fails.append(f"tier {tier} result is FAIL{why}")
        elif tier_states[tier] != "PASS":
            unproved.append(f"tier {tier} was attempted but its result is {entry.get('result')!r}{why}")
    if all(state == "NOT_ATTEMPTED" for state in tier_states.values()):
        unproved.append("neither tier (released_app, native_check) was attempted")

    cases_raw = document.get("cases") if isinstance(document.get("cases"), list) else []
    if not cases_raw:
        unproved.append("no cases")
    cases = [evaluate_case(case, index, tier_states) for index, case in enumerate(cases_raw)]
    for case in cases:  # one unknown criterion anywhere keeps the whole OS from PASS, however many other cases pass
        (fails if case["verdict"] == "FAIL" else unproved).extend(f"case {case['id']}: {reason}" for reason in case["reasons"])
    passing = {case["name"] for case in cases if case["verdict"] == "PASS"}
    for name in CASE_NAMES:
        if name not in passing:
            blocked = [f"{case['id']}: {'; '.join(case['reasons'])}" for case in cases if case["name"] == name and case["verdict"] == "UNPROVED"]
            unproved.append(f"no passing case named {name!r}" + (f" ({'; '.join(blocked)})" if blocked else ""))

    released = tier_states["released_app"] != "NOT_ATTEMPTED"
    app = document.get("app") if isinstance(document.get("app"), dict) else {}
    if released:
        if app.get("digest_match") is False:
            fails.append("app.digest_match is false: the app that ran is not the released asset")
        elif app.get("digest_match") is not True:
            unproved.append("app.digest_match is not true")
        if isinstance(app.get("asset_sha256"), str) and isinstance(app.get("release_digest"), str) and app["asset_sha256"] and app["release_digest"]:
            if app["asset_sha256"].lower().replace("sha256:", "") != app["release_digest"].lower().replace("sha256:", ""):
                fails.append("app.asset_sha256 differs from app.release_digest")
        if app.get("tag") != TAG:
            (fails if isinstance(app.get("tag"), str) and app.get("tag") else unproved).append(f"app.tag is {app.get('tag')!r}, not {TAG}")
    plugin = document.get("plugin") if isinstance(document.get("plugin"), dict) else {}
    version = plugin.get("version")
    if any(state != "NOT_ATTEMPTED" for state in tier_states.values()):
        if not isinstance(version, str) or not version:
            unproved.append("plugin.version is missing")
        elif PLUGIN_VERSION not in version:
            (fails if re.search(r"\d+\.\d+\.\d+", version) else unproved).append(f"plugin.version {version!r} is not the shipped tauri-plugin-updater {PLUGIN_VERSION}")
    if any(case["scenario"] == "latest-404" for case in cases):
        text = document.get("manifest404_error_text")
        if not isinstance(text, str) or RELEASE_NOT_FOUND not in text:
            unproved.append(f"manifest404_error_text is {text!r}, not the ReleaseNotFound text")
    controls = document.get("controls") if isinstance(document.get("controls"), list) else []
    for index, control in enumerate(controls):
        state = norm(control.get("verdict")) if isinstance(control, dict) else "UNPROVED"
        if state == "FAIL":
            fails.append(f"control[{index}] FAILED: the recorder did not see a request the control caused, so its 'no bundle' findings prove nothing")
        elif state != "PASS":
            unproved.append(f"control[{index}] is not proven ({control.get('verdict') if isinstance(control, dict) else control!r})")
    isolation = document.get("isolation") if isinstance(document.get("isolation"), dict) else {}
    if isolation.get("live_block") is not True:
        unproved.append("isolation.live_block is not true: nothing shows the live GitHub was unreachable during the run")
    if not (isinstance(isolation.get("hosts_mapped"), list) and isolation["hosts_mapped"]):
        unproved.append("isolation.hosts_mapped is missing or empty")

    directory = path.parent
    listed = sorted({name for case in cases_raw if isinstance(case, dict) and isinstance(case.get("evidence_files"), list) for name in case["evidence_files"] if isinstance(name, str)})
    present: Dict[str, str] = {}
    for name in listed:
        target = _inside(directory, name)
        if target is not None and target.is_file() and target.stat().st_size <= MAX_HASHED_EVIDENCE_BYTES:
            present[name] = sha256_bytes(target.read_bytes())
    verdict = "FAIL" if fails else ("UNPROVED" if unproved else "PASS")
    try:
        shown = path.resolve().relative_to(Path(records).resolve()).as_posix()
    except ValueError:
        shown = path.name
    return {
        "file": shown,
        "sha256": sha256_bytes(raw),
        "verdict": verdict,
        "reasons": fails + unproved,
        "tiers": tier_states,
        "cases": cases,
        "plugin_version": version if isinstance(version, str) else None,
        "digest_match": app.get("digest_match") if isinstance(app.get("digest_match"), bool) else None,
        "live_block": isolation.get("live_block") if isinstance(isolation.get("live_block"), bool) else None,
        "runner": document.get("runner") if isinstance(document.get("runner"), dict) else None,
        "os_reported": document.get("os"),
        "controls": len(controls),
        "evidence": {"listed": len(listed), "present": len(present), "missing_from_records": sorted(set(listed) - set(present)), "sha256": present},
    }


# --------------------------------------------------------------------------------------------------------------
# discovery and the stage verdict
# --------------------------------------------------------------------------------------------------------------

def discover(records: Path) -> Tuple[Dict[str, List[Path]], List[Tuple[Path, bytes, Any]], List[str]]:
    """(harness files per label, desktop-os documents, problems). Recursive, sorted, byte-identical duplicates collapsed."""
    harness: Dict[str, List[Path]] = {label: [] for label in LABELS}
    theirs: List[Tuple[Path, bytes, Any]] = []
    problems: List[str] = []
    seen_digests: Dict[str, set] = {}
    for current, dirnames, filenames in os.walk(str(records)):
        dirnames.sort()
        for name in sorted(filenames):
            path = Path(current) / name
            match = re.fullmatch(r"result-(linux|windows|macos)\.json", name)
            if not match and not name.endswith(".json"):
                continue
            try:
                if path.stat().st_size > MAX_JSON_BYTES:
                    continue
                raw = path.read_bytes()
            except OSError:
                continue
            try:
                document = json.loads(raw.decode("utf-8"))
            except ValueError:
                if match:
                    harness[match.group(1)].append(path)  # load_result reports it as not JSON
                elif name == "result.json":
                    problems.append(f"{path.name} in {Path(current).name or '.'}: not JSON")
                continue
            if match and not (isinstance(document, dict) and document.get("schema") == OS_SCHEMA):
                harness[match.group(1)].append(path)
                continue
            if isinstance(document, dict) and document.get("schema") == OS_SCHEMA:
                digest = sha256_bytes(raw)
                key = str(document.get("os"))
                if digest in seen_digests.setdefault(key, set()):
                    continue  # the same document delivered twice (two artifact layouts)
                seen_digests[key].add(digest)
                theirs.append((path, raw, document))
    for label in LABELS:
        unique: Dict[str, Path] = {}
        for path in harness[label]:
            unique.setdefault(sha256_bytes(path.read_bytes()), path)
        harness[label] = list(unique.values())
    return harness, theirs, problems


def build(records: Path, require_desktop_os: bool = False) -> Tuple[dict, List[str]]:
    records = Path(records)
    harness_files, os_documents, notes = discover(records)
    notes = list(notes)
    global_failed = bool(notes)
    per_label: Dict[str, dict] = {}
    missing: List[str] = []
    mine_state: Dict[str, str] = {}
    for label in LABELS:
        files = harness_files[label]
        if not files:
            missing.append(label)
            mine_state[label] = "MISSING"
            continue
        if len(files) > 1:
            global_failed = True
            mine_state[label] = "FAIL"
            notes.append(f"result-{label}.json exists {len(files)} times with different contents: " + ", ".join(sorted(p.as_posix() for p in files)))
            continue
        record, problems = load_result(files[0], label)
        mine_state[label] = "PASS"
        if problems:
            mine_state[label] = "FAIL"
            notes.extend(problems)
        if record:
            per_label[label] = record
            if record["verdict"] != ISOLATED:
                mine_state[label] = "FAIL"
                notes.append(f"{label}: verdict {record['verdict']}" + (f" (failed: {', '.join(record['failed_assertions'])})" if record["failed_assertions"] else ""))
            elif record["plugin_semantics"] != "confirmed":
                mine_state[label] = "FAIL"
                notes.append(f"{label}: the tauri-plugin-updater source was not confirmed (plugin_semantics {record['plugin_semantics']!r})")
    blob_sets = {label: {name: entry.get("git_blob") for name, entry in record["sources"].items()} for label, record in per_label.items()}
    distinct = {json.dumps(blobs, sort_keys=True) for blobs in blob_sets.values()}
    if len(distinct) > 1:
        global_failed = True
        notes.append("the results read different source bytes (git blob ids differ between OS runs): " + ", ".join(sorted(blob_sets)))

    desktop_os: Dict[str, dict] = {}
    for path, raw, document in os_documents:
        label = canonical_os(document.get("os"))
        if label is None:
            global_failed = True
            notes.append(f"{path.name}: desktop-os result with os {document.get('os')!r}, expected one of {', '.join(LABELS)}")
            continue
        if label in desktop_os:
            global_failed = True
            desktop_os[label]["verdict"] = "FAIL"
            desktop_os[label]["reasons"].append(f"a second, different desktop-os result for {label} exists: {path.name}")
            notes.append(f"{label}: two different desktop-os results (second: {path.as_posix()})")
            continue
        desktop_os[label] = evaluate_os_result(document, raw, path, records)

    os_verdicts: Dict[str, str] = {}
    for label in LABELS:
        theirs = desktop_os.get(label)
        if mine_state[label] == "FAIL" or (theirs is not None and theirs["verdict"] == "FAIL"):
            os_verdicts[label] = "FAIL"
        elif require_desktop_os:  # the desktop-os result is the required evidence, the harness result is judged when present
            if theirs is None:
                os_verdicts[label] = "MISSING"
                notes.append(f"{label}: no desktop-os result (--require-desktop-os)")
            else:
                os_verdicts[label] = "UNPROVED" if theirs["verdict"] == "UNPROVED" else "PASS"
        elif mine_state[label] == "MISSING":
            os_verdicts[label] = "MISSING"
        elif theirs is not None and theirs["verdict"] == "UNPROVED":
            os_verdicts[label] = "UNPROVED"
        else:
            os_verdicts[label] = "PASS"
        if theirs is not None and theirs["verdict"] != "PASS":
            notes.append(f"{label}: desktop-os {theirs['verdict']}: " + "; ".join(theirs["reasons"][:6]) + (" ..." if len(theirs["reasons"]) > 6 else ""))
    if missing and not require_desktop_os:
        notes.append("missing results: " + ", ".join(missing))
    if global_failed or "FAIL" in os_verdicts.values():
        verdict = "STAGE_C_FAIL"
    elif "MISSING" in os_verdicts.values() or "UNPROVED" in os_verdicts.values():
        verdict = "STAGE_C_INCOMPLETE"
    else:
        verdict = "STAGE_C_PASS"
    summary = {
        "schema": SUMMARY_SCHEMA,
        "verdict": verdict,
        "os_verdicts": os_verdicts,
        "labels": {label: {k: v for k, v in record.items() if k != "sources"} for label, record in sorted(per_label.items())},
        "desktop_os": {label: desktop_os[label] for label in LABELS if label in desktop_os},
        "desktop_os_absent": [label for label in LABELS if label not in desktop_os],
        "missing": missing,
        "native_verified_labels": sorted({label for label, record in per_label.items() if record["native"] == "PASS"} | {label for label, record in desktop_os.items() if record["tiers"].get("native_check") == "PASS"}),
        "require_desktop_os": bool(require_desktop_os),
        "sources_identical_across_labels": len(distinct) <= 1,
        "notes": notes,
    }
    return summary, notes


def render(summary: dict) -> str:
    lines = [f"Stage C verdict: {summary['verdict']}", ""]
    for label in LABELS:
        record = summary["labels"].get(label)
        theirs = summary["desktop_os"].get(label)
        harness = (f"harness {record['verdict']} requests {record['request_count']!s} native {record['native']} plugin source {record['plugin_semantics']}  {record['platform']}" if record else "harness (no result)")
        desktop = (f"desktop-os {theirs['verdict']} (tiers {', '.join(f'{t} {s}' for t, s in theirs['tiers'].items())}; {len(theirs['cases'])} cases)" if theirs else "desktop-os (none)")
        lines.append(f"  {summary['os_verdicts'][label]:9} {label:8} {harness} | {desktop}")
    for note in summary["notes"]:
        lines.append(f"  NOTE  {note}")
    lines.append(f"  native tauri-plugin-updater verified on: {summary['native_verified_labels'] or 'no OS (replica evidence only)'}")
    return "\n".join(lines) + "\n"


def main(argv: List[str] = None) -> int:  # type: ignore[assignment]
    parser = argparse.ArgumentParser(description=__doc__, formatter_class=argparse.RawDescriptionHelpFormatter)
    sub = parser.add_subparsers(dest="command", required=True)
    one = sub.add_parser("build")
    one.add_argument("--records", type=Path, required=True)
    one.add_argument("--out", type=Path, required=True)
    one.add_argument("--require-desktop-os", action="store_true", help="an OS without a desktop-os result is UNPROVED instead of being judged on the harness alone")
    args = parser.parse_args(argv)
    for stream in (sys.stdout, sys.stderr):
        if hasattr(stream, "reconfigure"):  # Windows consoles default to a legacy code page
            stream.reconfigure(encoding="utf-8", errors="replace")
    summary, _ = build(args.records, args.require_desktop_os)
    args.out.parent.mkdir(parents=True, exist_ok=True)
    args.out.write_bytes((json.dumps(summary, indent=2, sort_keys=True) + "\n").encode("utf-8"))
    table = render(summary)
    sys.stdout.write(table)
    step_summary = os.environ.get("GITHUB_STEP_SUMMARY")
    if step_summary:
        with open(step_summary, "a", encoding="utf-8") as handle:
            handle.write("```\n" + table + "```\n")
    return 0 if summary["verdict"] == "STAGE_C_PASS" else 1


if __name__ == "__main__":
    sys.exit(main())
