"""Shared path setup for the Wave 0 desktop lab tests (THROWAWAY LAB FILE)."""
from __future__ import annotations

import sys
from pathlib import Path

TESTS = Path(__file__).resolve().parent
LAB = TESTS.parent            # wave0-lab-desktop/
ROOT = LAB.parent             # repository root
for extra in (LAB, LAB / "lib"):
    if str(extra) not in sys.path:
        sys.path.insert(0, str(extra))
