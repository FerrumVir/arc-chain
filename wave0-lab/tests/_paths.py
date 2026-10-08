"""Shared path setup for the Wave 0 lab tests (THROWAWAY LAB FILE)."""
from __future__ import annotations

import os
import sys
from pathlib import Path

TESTS = Path(__file__).resolve().parent
LAB = TESTS.parent
ROOT = LAB.parent
for extra in (LAB, LAB / "stage_b", LAB / "stage_b" / "guest"):
    if str(extra) not in sys.path:
        sys.path.insert(0, str(extra))

# The guest probes load the repository snapshot tool from /opt/arc-w0 inside the VM; offline they use the checkout.
os.environ.setdefault("ARC_W0_SNAPSHOT_TOOL", str(ROOT / "tests" / "legacy-bridge" / "snapshot_tree.py"))
