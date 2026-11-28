#!/usr/bin/env python3
"""CI guard: pin which shipped binary links which crates, per release target.

The ARC desktop product is two binaries. The Tauri app (``arc-desktop``) does
not compile QUIC (``quinn`` / ``quinn-proto``); the ``arc-node`` sidecar it
downloads and runs does (``arc-net`` uses it; QUIC starts only at stake > 0).
"The desktop app does not include the library" is therefore true of the app
binary only, never of the installed product. This check pins both facts from
``cargo tree`` for every shipped target, so neither can change without a
reviewed edit to ``scripts/ci/desktop-dependency-boundary.json``.

What it covers: the targets and features in that policy file (mirroring
``.github/workflows/release-signing-preflight.yml``), ``normal`` dependency
edges (code linked into the binary; build and dev dependencies are not
shipped), resolved from the committed ``Cargo.lock`` (``--locked``). What it
does not cover: crates loaded at runtime from outside the build (none today),
and whether a linked crate's vulnerable code path is reachable. That is a
separate, human judgement and no part of this check.

Usage:
  python3 scripts/ci/desktop_dependency_boundary.py [--offline]
  python3 scripts/ci/desktop_dependency_boundary.py --recorded DIR   # tests: read
      DIR/<graph id>-<target>.txt (cargo tree --prefix none -f '{p}' output) instead of running cargo
  python3 scripts/ci/desktop_dependency_boundary.py --policy FILE    # another policy file
Exit 0 when every graph matches its policy, 1 when one does not, 2 on errors.
"""
from __future__ import annotations

import argparse
import json
import subprocess
import sys
from pathlib import Path

REPO = Path(__file__).resolve().parents[2]
DEFAULT_POLICY = REPO / "scripts" / "ci" / "desktop-dependency-boundary.json"


def crate_names(tree_output: str) -> set[str]:
    """Package names from `cargo tree --prefix none -f '{p}'` lines like `quinn-proto v0.11.18`."""
    names = set()
    for line in tree_output.splitlines():
        parts = line.split()
        if len(parts) >= 2 and parts[1].startswith("v"):
            names.add(parts[0])
    return names


def cargo_tree(graph: dict, target: str, offline: bool) -> str:
    cmd = ["cargo", "tree", "--manifest-path", str(REPO / graph["manifest"]), "--locked", "-p", graph["package"],
           "--target", target, "-e", "normal", "--prefix", "none", "-f", "{p}", *graph["cargo_args"]]
    if offline:
        cmd.append("--offline")
    done = subprocess.run(cmd, cwd=REPO, capture_output=True, text=True)
    if done.returncode != 0:
        raise RuntimeError(f"cargo tree failed for {graph['id']} {target}: {done.stderr.strip()[:2000]}")
    return done.stdout


def check(policy: dict, read_tree) -> list[str]:
    problems = []
    for graph in policy["graphs"]:
        for target in graph["targets"]:
            names = crate_names(read_tree(graph, target))
            if not names:
                problems.append(f"{graph['id']} {target}: empty dependency graph (cargo tree output not understood)")
                continue
            if graph["package"] not in names:
                problems.append(f"{graph['id']} {target}: {graph['package']} is not in its own graph")
            for crate in graph["must_not_contain"]:
                if crate in names:
                    problems.append(f"{graph['id']} {target}: now links {crate}, which the policy says the {graph['role']} does not")
            for crate in graph["must_contain"]:
                if crate not in names:
                    problems.append(f"{graph['id']} {target}: no longer links {crate}, which the policy says the {graph['role']} does")
            print(f"{graph['id']:8} {target:28} {len(names):4} crates  "
                  f"absent ok: {', '.join(c for c in graph['must_not_contain'] if c not in names) or '-'}  "
                  f"present ok: {', '.join(c for c in graph['must_contain'] if c in names) or '-'}")
    return problems


def main(argv: list[str]) -> int:
    parser = argparse.ArgumentParser(description=__doc__.split("\n\n")[0])
    parser.add_argument("--policy", default=str(DEFAULT_POLICY))
    parser.add_argument("--recorded", help="read recorded cargo tree output from this directory instead of running cargo")
    parser.add_argument("--offline", action="store_true", help="pass --offline to cargo tree")
    args = parser.parse_args(argv)
    try:
        policy = json.loads(Path(args.policy).read_text(encoding="utf-8"))
        if args.recorded:
            base = Path(args.recorded)
            read_tree = lambda graph, target: (base / f"{graph['id']}-{target}.txt").read_text(encoding="utf-8")
        else:
            read_tree = lambda graph, target: cargo_tree(graph, target, args.offline)
        problems = check(policy, read_tree)
    except (OSError, ValueError, KeyError, RuntimeError) as error:
        print(f"desktop-dependency-boundary: error: {error}", file=sys.stderr)
        return 2
    for problem in problems:
        print(f"desktop-dependency-boundary: {problem}", file=sys.stderr)
    if problems:
        print("desktop-dependency-boundary: the dependency boundary changed. If intended, update "
              "scripts/ci/desktop-dependency-boundary.json and any public statement about it in the same PR.", file=sys.stderr)
        return 1
    print("desktop-dependency-boundary: every shipped graph matches the policy.")
    return 0


if __name__ == "__main__":
    sys.exit(main(sys.argv[1:]))
