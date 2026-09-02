#!/usr/bin/env bash
# ARC_RETIRED_LIVE_TOOL_V3_REQUIRED
set -euo pipefail
printf '%s\n' \
    'RETIRED: testnet.sh launches seed-derived staked identities outside the v3 keyfile contract.' \
    'No process or state was changed. Use the Rust multi-node tests until a v3 local harness is approved.' >&2
exit 78
