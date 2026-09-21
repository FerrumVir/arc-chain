#!/usr/bin/env bash
# Recorded soak for the local validator candidate (checklist R8).
#
# Thin entry point. The run is orchestrated by scripts/arc_soak/orchestrate.py,
# which only RECORDS, and judged by scripts/arc_soak/analyze.py, which is the
# only thing that decides the verdict and this script's exit status:
#
#   0 PASS   1 FAIL   2 INCOMPLETE
#
# The previous bash implementation printed its verdict through `| tee`, so no
# FAIL it printed could ever change the exit status - Codex reproduced FAIL
# and a missing-evidence PASS all exiting 0. That cannot recur here: this
# script `exec`s the orchestrator and returns whatever the analyzer decided.
#
#   scripts/arc-soak.sh --analyzer-tests                 # 0.4 s, no nodes
#   scripts/arc-soak.sh --self-test --binary B --provenance P   # ~9 min, 4 nodes
#   scripts/arc-soak.sh --hours 24  --binary B --provenance P
#
# Nothing schedules a soak. Starting one is a deliberate decision about a day.
set -euo pipefail
cd "$(dirname "$0")"
if [[ ${1:-} == --analyzer-tests ]]; then
  exec python3 -m unittest arc_soak.tests.test_analyze
fi
exec python3 -m arc_soak.orchestrate "$@"
