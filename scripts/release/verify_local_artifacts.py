#!/usr/bin/env python3
"""Verify what scripts/release/build-local-artifacts.sh wrote, without touching it (R2).

    python3 scripts/release/verify_local_artifacts.py OUTPUT_DIR

Checks exactly the layout that script writes:

  checksums   SHA256SUMS exists; every entry (relative to OUTPUT_DIR/collected,
              as the script sums it) names a file that exists there with that
              digest; no regular file the script would have summed is left out
              - every file under collected/, and every desktop-unsigned/
              UNSIGNED-* artifact, which the script copies into collected/.
  provenance  the arc-node record under arc-node-provenance/ (the one
              latest-provenance.txt names, or the only one present) reports a
              successful build and a binary_sha256 equal to both
              collected/arc-node and its immutable arc-node-<digest> copy; when
              an arc CLI was built, arc-cli-provenance.txt matches collected/arc-cli.
  sbom        arc-chain.cdx.json is JSON with bomFormat "CycloneDX", specVersion
              "1.5" and a non-empty components list.
  unsigned    nothing claims to be signed: no *.sig, *.minisig or *.ticket file,
              no ticket stapled into an archived .app (Contents/CodeResources),
              desktop-unsigned/NOT-A-RELEASE.txt present, and every desktop
              artifact labelled UNSIGNED-. A ticket stapled into a .dmg cannot
              be seen without Apple's tooling and is reported as a limit.

Prints one JSON report. Exit: 0 every check passes, 1 any check fails,
2 OUTPUT_DIR is missing or is not a build-local-artifacts output (it has
neither collected/ nor arc-node-provenance/). Read-only: nothing under
OUTPUT_DIR is written, moved or deleted.
"""

import hashlib
import json
import os
import re
import sys
import tarfile
from typing import Any, Dict, List, Optional

SUMS_LINE = re.compile(r"^([0-9a-f]{64})  (.+)$")
RECORD_LINE = re.compile(r"^([a-z0-9_]+):\s*(.*?)\s*$")
SIGNED_SUFFIXES = (".sig", ".minisig", ".ticket")
# Where `xcrun stapler staple` puts a notarization ticket inside an app bundle
# (distinct from the code signature's Contents/_CodeSignature/CodeResources).
STAPLED_TICKET = re.compile(r"(^|/)[^/]+\.app/Contents/CodeResources$")
NOT_A_RELEASE_TEXT = "NOT a signed production release"
NODE_BINARIES = ("arc-node", "arc-cli")


def sha256_file(path: str) -> str:
    h = hashlib.sha256()
    with open(path, "rb") as fh:
        for block in iter(lambda: fh.read(1 << 20), b""):
            h.update(block)
    return h.hexdigest()


def read_record(path: str) -> Dict[str, str]:
    """The `key: value` lines of an arc-build-provenance record, first word of
    each value (the same rule as arc_soak.orchestrate.read_provenance)."""
    out: Dict[str, str] = {}
    with open(path, encoding="utf-8", errors="replace") as fh:
        for line in fh:
            m = RECORD_LINE.match(line)
            if m:
                out[m.group(1)] = m.group(2).split()[0] if m.group(2) else ""
    return out


def _regular(path: str) -> bool:
    return os.path.isfile(path) and not os.path.islink(path)


def _inside(path: str, root: str) -> bool:
    path, root = os.path.realpath(path), os.path.realpath(root)
    return path == root or path.startswith(root.rstrip(os.sep) + os.sep)


def _files(root: str) -> List[str]:
    """Regular files under root, relative, sorted (what `find . -type f` sums)."""
    out = []
    for base, _dirs, names in os.walk(root):
        for name in names:
            p = os.path.join(base, name)
            if _regular(p):
                out.append(os.path.relpath(p, root).replace(os.sep, "/"))
    return sorted(out)


def check_checksums(out_dir: str) -> Dict[str, Any]:
    collected = os.path.join(out_dir, "collected")
    sums = os.path.join(out_dir, "SHA256SUMS")
    r: Dict[str, Any] = {"entries": 0, "mismatched": [], "missing": [], "omitted": [],
                         "invalid_lines": [], "duplicates": [], "desktop_not_collected": [],
                         "symlinks_in_collected": []}
    if not _regular(sums):
        r["problem"] = "SHA256SUMS missing"
        r["pass"] = False
        return r
    listed: Dict[str, str] = {}
    with open(sums, encoding="utf-8", errors="replace") as fh:
        for n, line in enumerate(fh, 1):
            line = line.rstrip("\n")
            if not line.strip():
                continue
            m = SUMS_LINE.match(line)
            rel = m.group(2) if m else None
            if not m or rel.startswith("/") or ".." in rel.split("/"):
                r["invalid_lines"].append(n)
                continue
            if rel in listed:
                r["duplicates"].append(rel)
                continue
            listed[rel] = m.group(1)
    r["entries"] = len(listed)
    for rel, digest in sorted(listed.items()):
        p = os.path.join(collected, rel)
        if not _regular(p) or not _inside(p, collected):
            r["missing"].append(rel)
        elif sha256_file(p) != digest:
            r["mismatched"].append(rel)
    present = _files(collected) if os.path.isdir(collected) else []
    r["omitted"] = [rel for rel in present if rel not in listed]
    if os.path.isdir(collected):
        for base, dirs, names in os.walk(collected):
            for name in names + dirs:
                if os.path.islink(os.path.join(base, name)):
                    r["symlinks_in_collected"].append(os.path.relpath(os.path.join(base, name), collected))
    desktop = os.path.join(out_dir, "desktop-unsigned")
    if os.path.isdir(desktop):
        for name in sorted(os.listdir(desktop)):
            src = os.path.join(desktop, name)
            if not (name.startswith("UNSIGNED-") and _regular(src)):
                continue
            copy = os.path.join(collected, name)
            if name not in listed or not _regular(copy) or sha256_file(copy) != sha256_file(src):
                r["desktop_not_collected"].append(name)
    r["pass"] = not any(r[k] for k in ("mismatched", "missing", "omitted", "invalid_lines", "duplicates",
                                         "desktop_not_collected", "symlinks_in_collected"))
    return r


def _record_path(prov_dir: str) -> Dict[str, Any]:
    latest = os.path.join(prov_dir, "latest-provenance.txt")
    records = sorted(n for n in os.listdir(prov_dir)
                     if n.startswith("provenance-") and n.endswith(".txt") and _regular(os.path.join(prov_dir, n)))
    if os.path.islink(latest) or os.path.exists(latest):
        target = os.path.realpath(latest)
        if not _inside(target, prov_dir) or not _regular(target):
            return {"problem": "latest-provenance.txt does not name a record inside arc-node-provenance/"}
        return {"path": target, "records": len(records)}
    if len(records) == 1:
        return {"path": os.path.join(prov_dir, records[0]), "records": 1}
    return {"problem": f"{len(records)} provenance records and no latest-provenance.txt: which one describes this build is ambiguous",
            "records": len(records)}


def check_provenance(out_dir: str) -> Dict[str, Any]:
    prov_dir = os.path.join(out_dir, "arc-node-provenance")
    r: Dict[str, Any] = {"problems": []}
    node = os.path.join(out_dir, "collected", "arc-node")
    if not os.path.isdir(prov_dir):
        r["problems"].append("arc-node-provenance/ missing")
    else:
        found = _record_path(prov_dir)
        if "problem" in found:
            r["problems"].append(found["problem"])
        else:
            rec = read_record(found["path"])
            r["record"] = os.path.relpath(found["path"], out_dir)
            r["records_present"] = found["records"]
            r["binary_sha256"] = rec.get("binary_sha256")
            r["build_exit"] = rec.get("build_exit")
            if rec.get("build_exit") != "0":
                r["problems"].append(f"the record reports build_exit {rec.get('build_exit')!r}")
            if not re.fullmatch(r"[0-9a-f]{64}", rec.get("binary_sha256") or ""):
                r["problems"].append("the record names no binary_sha256")
            if not rec.get("input_digest_after_build"):
                r["problems"].append("the record has no unchanged input digest after the build")
            immutable = rec.get("immutable_copy")
            if immutable:
                copy = os.path.join(prov_dir, os.path.basename(immutable))
                r["immutable_copy"] = os.path.relpath(copy, out_dir)
                if _regular(copy):
                    r["immutable_sha256"] = sha256_file(copy)
                    if r["immutable_sha256"] != rec.get("binary_sha256"):
                        r["problems"].append("the immutable copy's digest differs from the record")
                else:
                    r["problems"].append("the immutable copy the record names is not in arc-node-provenance/")
            else:
                r["problems"].append("the record names no immutable_copy")
            if _regular(node):
                r["collected_sha256"] = sha256_file(node)
                if r["collected_sha256"] != rec.get("binary_sha256"):
                    r["problems"].append("collected/arc-node differs from the record's binary_sha256")
    if not _regular(node):
        r["problems"].append("collected/arc-node missing")
    cli_record = os.path.join(out_dir, "arc-cli-provenance.txt")
    cli = os.path.join(out_dir, "collected", "arc-cli")
    if _regular(cli_record) or _regular(cli):
        cr: Dict[str, Any] = {}
        if not (_regular(cli_record) and _regular(cli)):
            r["problems"].append("arc CLI: the binary and arc-cli-provenance.txt must both be present")
        else:
            cr["binary_sha256"] = read_record(cli_record).get("binary_sha256")
            cr["collected_sha256"] = sha256_file(cli)
            if cr["binary_sha256"] != cr["collected_sha256"]:
                r["problems"].append("collected/arc-cli differs from arc-cli-provenance.txt")
        r["arc_cli"] = cr
    else:
        r["arc_cli"] = {"built": False}
    r["pass"] = not r["problems"]
    return r


def check_sbom(out_dir: str) -> Dict[str, Any]:
    path = os.path.join(out_dir, "arc-chain.cdx.json")
    r: Dict[str, Any] = {"problems": []}
    try:
        with open(path, encoding="utf-8") as fh:
            doc = json.load(fh)
    except OSError:
        doc = None
        r["problems"].append("arc-chain.cdx.json missing")
    except ValueError:
        doc = None
        r["problems"].append("arc-chain.cdx.json is not valid JSON")
    if isinstance(doc, dict):
        if doc.get("bomFormat") != "CycloneDX":
            r["problems"].append(f"bomFormat is {doc.get('bomFormat')!r}, not 'CycloneDX'")
        if doc.get("specVersion") != "1.5":
            r["problems"].append(f"specVersion is {doc.get('specVersion')!r}, not '1.5'")
        components = doc.get("components")
        r["components"] = len(components) if isinstance(components, list) else None
        if not (isinstance(components, list) and components):
            r["problems"].append("components is missing or empty")
    elif doc is not None:
        r["problems"].append("arc-chain.cdx.json is not a JSON object")
    r["pass"] = not r["problems"]
    return r


def check_unsigned(out_dir: str) -> Dict[str, Any]:
    r: Dict[str, Any] = {"signature_files": [], "stapled_tickets": [], "unreadable_archives": [],
                         "missing_labels": [], "unlabelled_desktop_artifacts": [],
                         "limits": ["a notarization ticket stapled into a .dmg is not visible without "
                                    "Apple's tooling (xcrun stapler validate); not checked"]}
    for base, _dirs, names in os.walk(out_dir):
        for name in names:
            rel = os.path.relpath(os.path.join(base, name), out_dir)
            if name.endswith(SIGNED_SUFFIXES):
                r["signature_files"].append(rel)
            if name.endswith(".app.tar.gz") and _regular(os.path.join(base, name)):
                try:
                    with tarfile.open(os.path.join(base, name), "r:gz") as tar:
                        for member in tar.getmembers():
                            if STAPLED_TICKET.search(member.name):
                                r["stapled_tickets"].append(f"{rel}:{member.name}")
                            if member.name.endswith(SIGNED_SUFFIXES):
                                r["signature_files"].append(f"{rel}:{member.name}")
                except (OSError, tarfile.TarError, EOFError):
                    r["unreadable_archives"].append(rel)
    desktop = os.path.join(out_dir, "desktop-unsigned")
    label = os.path.join(desktop, "NOT-A-RELEASE.txt")
    if not _regular(label):
        r["missing_labels"].append("desktop-unsigned/NOT-A-RELEASE.txt")
    else:
        with open(label, encoding="utf-8", errors="replace") as fh:
            if NOT_A_RELEASE_TEXT not in fh.read():
                r["missing_labels"].append("desktop-unsigned/NOT-A-RELEASE.txt does not say it is not a release")
    if os.path.isdir(desktop):
        artifacts = [n for n in sorted(os.listdir(desktop)) if n != "NOT-A-RELEASE.txt"]
        if not artifacts:
            r["missing_labels"].append("desktop-unsigned/ holds no UNSIGNED- artifact")
        r["unlabelled_desktop_artifacts"] += [f"desktop-unsigned/{n}" for n in artifacts if not n.startswith("UNSIGNED-")]
    else:
        r["missing_labels"].append("desktop-unsigned/ missing")
    collected = os.path.join(out_dir, "collected")
    if os.path.isdir(collected):
        r["unlabelled_desktop_artifacts"] += [f"collected/{n}" for n in sorted(os.listdir(collected))
                                              if n not in NODE_BINARIES and not n.startswith("UNSIGNED-")]
    r["pass"] = not any(r[k] for k in ("signature_files", "stapled_tickets", "unreadable_archives",
                                         "missing_labels", "unlabelled_desktop_artifacts"))
    return r


def verify(out_dir: str) -> Optional[Dict[str, Any]]:
    """The report, or None when OUTPUT_DIR is not a build-local-artifacts output."""
    out_dir = os.path.abspath(out_dir)
    if not os.path.isdir(out_dir) or not (os.path.isdir(os.path.join(out_dir, "collected"))
                                          or os.path.isdir(os.path.join(out_dir, "arc-node-provenance"))):
        return None
    checks = {"checksums": check_checksums(out_dir), "provenance": check_provenance(out_dir),
              "sbom": check_sbom(out_dir), "unsigned": check_unsigned(out_dir)}
    return {"output_dir": out_dir, "status": "PASS" if all(c["pass"] for c in checks.values()) else "FAIL",
            "checks": checks}


def main(argv: Optional[List[str]] = None) -> int:
    args = sys.argv[1:] if argv is None else argv
    if len(args) != 1 or args[0] in ("-h", "--help"):
        print("usage: verify_local_artifacts.py OUTPUT_DIR", file=sys.stderr)
        return 2
    report = verify(args[0])
    if report is None:
        print(json.dumps({"output_dir": os.path.abspath(args[0]), "status": "NOT_AN_OUTPUT",
                          "reason": "missing, or has neither collected/ nor arc-node-provenance/"}, sort_keys=True))
        return 2
    print(json.dumps(report, indent=2, sort_keys=True))
    return 0 if report["status"] == "PASS" else 1


if __name__ == "__main__":
    sys.exit(main())
