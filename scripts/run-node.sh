#!/usr/bin/env bash
# ARC_RETIRED_LIVE_TOOL_V3_REQUIRED
set -euo pipefail
printf '%s\n' \
    'RETIRED: this legacy staked/public-bind launcher is disabled in the v0.7.12 recovery candidate.' \
    'Use Rust integration tests for local multi-node work; this command started no process and changed no data.' >&2
exit 78
