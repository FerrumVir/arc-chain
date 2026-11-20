#!/usr/bin/env python3
"""Run npm's audit with exact, owner-accepted, expiring advisory exceptions."""

import argparse
from collections import Counter
from datetime import date, datetime, timezone
import json
from pathlib import Path
import re
import shutil
import subprocess
import sys


ROOT = Path(__file__).resolve().parents[2]
LEVELS = {name: rank for rank, name in enumerate(
    ("info", "low", "moderate", "high", "critical"))}
GHSA = r"GHSA-[23456789cfghjmpqrvwx]{4}-[23456789cfghjmpqrvwx]{4}-[23456789cfghjmpqrvwx]{4}"
ADVISORY_URL = re.compile(r"https://github\.com/advisories/(" + GHSA + r")")


class GateError(ValueError):
    pass


class Parser(argparse.ArgumentParser):
    def error(self, message):
        raise GateError(message)


def require(condition, message):
    if not condition:
        raise GateError(message)


def nonempty(value):
    return isinstance(value, str) and bool(value.strip())


def unique_object(pairs):
    result = {}
    for key, value in pairs:
        require(key not in result, f"duplicate JSON key: {key}")
        result[key] = value
    return result


def read_json(text):
    def invalid_constant(value):
        raise GateError(f"invalid JSON constant: {value}")
    return json.loads(text, object_pairs_hook=unique_object,
                      parse_constant=invalid_constant)


def read_exceptions(path, today):
    data = read_json(path.read_text(encoding="utf-8"))
    require(isinstance(data, dict) and set(data) == {"_comment", "exceptions"},
            "exceptions file must contain _comment and exceptions")
    require(nonempty(data["_comment"]), "exception policy comment is missing")
    require(isinstance(data["exceptions"], list), "exceptions must be an array")
    result = {}
    fields = {"advisory", "package", "added", "owner", "expires", "reason", "removal"}
    for entry in data["exceptions"]:
        require(isinstance(entry, dict) and set(entry) == fields,
                "invalid exception fields")
        require(all(nonempty(value) for value in entry.values()),
                "exception fields must be nonempty strings")
        advisory = entry["advisory"]
        require(re.fullmatch(GHSA, advisory), f"invalid advisory id: {advisory}")
        require(advisory not in result, f"duplicate exception: {advisory}")
        dates = {}
        for field in ("added", "expires"):
            require(re.fullmatch(r"\d{4}-\d{2}-\d{2}", entry[field]),
                    f"invalid {field} date: {advisory}")
            dates[field] = date.fromisoformat(entry[field])
        require(dates["added"] <= dates["expires"], f"reversed dates: {advisory}")
        require(dates["added"] <= today, f"future exception: {advisory}")
        require(today <= dates["expires"], f"expired exception: {advisory}")
        result[advisory] = entry
    return result


def advisory_key(item, package):
    require(isinstance(item, dict), f"invalid via entry: {package}")
    require(item.get("name") == package and item.get("dependency") == package,
            f"advisory package mismatch: {package}")
    require(item.get("severity") in LEVELS, f"invalid advisory severity: {package}")
    require(type(item.get("source")) is int and item["source"] > 0
            and nonempty(item.get("title")) and nonempty(item.get("range")),
            f"invalid advisory record: {package}")
    url = item.get("url")
    match = ADVISORY_URL.fullmatch(url) if isinstance(url, str) else None
    require(match is not None, f"unrecognized advisory URL: {package}: {url}")
    return (match.group(1), package)


def validate_report(report):
    require(isinstance(report, dict) and "error" not in report,
            "npm did not return a successful audit report")
    require(type(report.get("auditReportVersion")) is int
            and report["auditReportVersion"] == 2, "unsupported audit report version")
    findings = report.get("vulnerabilities")
    require(isinstance(findings, dict), "vulnerabilities must be an object")
    counts = Counter()
    direct = {}
    links = {}
    for package, finding in findings.items():
        require(nonempty(package) and isinstance(finding, dict)
                and finding.get("name") == package, "invalid vulnerability name")
        severity = finding.get("severity")
        require(isinstance(severity, str) and severity in LEVELS,
                f"invalid severity: {package}")
        counts[severity] += 1
        nodes, via = finding.get("nodes"), finding.get("via")
        require(isinstance(nodes, list) and nodes and all(nonempty(n) for n in nodes),
                f"missing vulnerability paths: {package}")
        require(isinstance(via, list) and via, f"missing advisory causes: {package}")
        require(type(finding.get("isDirect")) is bool
                and isinstance(finding.get("effects"), list)
                and all(nonempty(n) for n in finding["effects"])
                and nonempty(finding.get("range")), f"invalid vulnerability schema: {package}")
        fix = finding.get("fixAvailable")
        require(type(fix) is bool or (isinstance(fix, dict)
                and nonempty(fix.get("name")) and nonempty(fix.get("version"))
                and type(fix.get("isSemVerMajor")) is bool),
                f"invalid fixAvailable: {package}")
        direct[package], links[package] = set(), []
        for item in via:
            if isinstance(item, str):
                require(item in findings, f"unresolved via link: {package} -> {item}")
                links[package].append(item)
            else:
                key = advisory_key(item, package)
                require(LEVELS[item["severity"]] <= LEVELS[severity],
                        f"understated advisory severity: {package}")
                direct[package].add(key)
    metadata = report.get("metadata")
    require(isinstance(metadata, dict), "missing audit metadata")
    dependencies = metadata.get("dependencies")
    require(isinstance(dependencies, dict)
            and {"prod", "dev", "optional", "peer", "peerOptional", "total"} <= dependencies.keys()
            and all(type(n) is int and n >= 0 for n in dependencies.values()),
            "invalid dependency metadata")
    totals = metadata.get("vulnerabilities")
    require(isinstance(totals, dict) and set(totals) == set(LEVELS) | {"total"},
            "invalid vulnerability totals")
    for severity in LEVELS:
        require(type(totals[severity]) is int and totals[severity] == counts[severity],
                f"inconsistent {severity} vulnerability count")
    require(type(totals["total"]) is int and totals["total"] == len(findings),
            "inconsistent total vulnerability count")

    roots = {}
    for package in findings:
        pending, seen, causes = [package], set(), set()
        while pending:
            current = pending.pop()
            if current in seen:
                continue
            seen.add(current)
            causes.update(direct[current])
            pending.extend(links[current])
        require(causes, f"no root advisory reachable: {package}")
        roots[package] = causes
    return findings, roots


def main(argv=None):
    args = argparse.Namespace(package="unknown", level="unknown", omit_dev=False)
    count = excepted = blocking = 0
    used = set()
    parser = Parser(description=__doc__, allow_abbrev=False)
    parser.add_argument("--package", required=True)
    parser.add_argument("--level", choices=list(LEVELS)[1:], required=True)
    parser.add_argument("--omit-dev", action="store_true")
    parser.add_argument("--lock-only", action="store_true")
    try:
        parser.parse_args(argv, namespace=args)
        exceptions = read_exceptions(ROOT / "npm-audit-exceptions.json",
                                     datetime.now(timezone.utc).date())
        package_dir = Path(args.package).resolve(strict=True)
        require(package_dir.is_dir() and (package_dir / "package.json").is_file()
                and (package_dir / "package-lock.json").is_file(),
                "package must contain package.json and package-lock.json")
        npm = shutil.which("npm")
        require(npm is not None, "npm executable not found")
        command = [npm, "audit", "--json", f"--audit-level={args.level}"]
        if args.omit_dev:
            command.append("--omit=dev")
        if args.lock_only:
            command.append("--package-lock-only")
        audit = subprocess.run(command, cwd=package_dir, capture_output=True,
                               text=True, encoding="utf-8", timeout=120, check=False)
        require(audit.returncode in (0, 1), f"npm audit failed: exit {audit.returncode}")
        findings, roots = validate_report(read_json(audit.stdout))
        selected = {p: f for p, f in findings.items()
                    if LEVELS[f["severity"]] >= LEVELS[args.level]}
        count = len(selected)
        require(audit.returncode == int(bool(selected)),
                "npm exit status disagrees with audit findings")
        for package, finding in sorted(selected.items()):
            uncovered = []
            for advisory, cause in sorted(roots[package]):
                if advisory in exceptions and exceptions[advisory]["package"] == cause:
                    used.add(advisory)
                else:
                    uncovered.append((advisory, cause))
            if uncovered:
                blocking += 1
                for advisory, cause in uncovered:
                    for path in findings[cause]["nodes"]:
                        print(f"BLOCK advisory={advisory} package={cause} path={path} "
                              f"finding={package} finding_paths={','.join(finding['nodes'])}")
            else:
                excepted += 1
        for advisory in sorted(exceptions.keys() - used):
            print(f"WARNING exception matched no gated advisory: {advisory}")
        return int(bool(blocking))
    except (OSError, ValueError, TypeError, KeyError, RecursionError,
            subprocess.SubprocessError) as error:
        print(f"ERROR npm-audit-gate: {error}", file=sys.stderr)
        return 2
    finally:
        print(f"npm-audit-gate package={args.package} level={args.level} "
              f"omit_dev={int(args.omit_dev)} findings={count} excepted={excepted} "
              f"blocking={blocking} exceptions_used={','.join(sorted(used)) or 'none'}")


if __name__ == "__main__":
    sys.exit(main())
