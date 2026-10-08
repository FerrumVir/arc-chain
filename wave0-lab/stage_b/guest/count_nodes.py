#!/usr/bin/env python3
"""Print the number of node or launcher processes under the ARC directory (runs inside the Wave 0 guest VM).

THROWAWAY LAB FILE. Uses the same scan as the sampler (probe.node_processes), so the stop check and the
`node_procs` samples agree. Read-only. A shell `pgrep -f` is not used: it matches the command line of the
very shell that runs it.

Usage: count_nodes.py --arc-dir DIR
"""
from __future__ import annotations

import argparse
import os
import sys

sys.path.insert(0, os.path.dirname(os.path.abspath(__file__)))
import probe  # noqa: E402


def main() -> int:
    parser = argparse.ArgumentParser(description=__doc__, formatter_class=argparse.RawDescriptionHelpFormatter)
    parser.add_argument("--arc-dir", required=True)
    args = parser.parse_args()
    print(len(probe.node_processes(args.arc_dir)))
    return 0


if __name__ == "__main__":
    sys.exit(main())
