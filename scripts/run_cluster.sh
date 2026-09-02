#!/usr/bin/env bash
# ARC_RETIRED_LIVE_TOOL_V3_REQUIRED
set -euo pipefail
printf '%s\n' \
    'RETIRED: run_cluster.sh launches seed-derived staked benchmark identities without an approved genesis.' \
    'No process was started or killed. Use the Rust multi-node tests until a v3 local harness is approved.' >&2
exit 78
