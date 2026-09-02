#!/usr/bin/env bash
# ARC_RETIRED_LIVE_TOOL_V3_REQUIRED
set -euo pipefail
printf '%s\n' \
    'RETIRED: create-testnet.sh generates identities and genesis fields rejected by the v3 contract.' \
    'No key or config was written. Use the Rust multi-node tests until a v3 local harness is approved.' >&2
exit 78
