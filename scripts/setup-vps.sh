#!/usr/bin/env bash
# ARC_RETIRED_LIVE_TOOL_V3_REQUIRED
set -euo pipefail
printf '%s\n' \
    'RETIRED: setup-vps.sh cannot safely install toolchains or run deployment benchmarks on a host.' \
    'No package, repository, model, or service was changed. Use reviewed local development tooling.' >&2
exit 78
