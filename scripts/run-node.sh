#!/usr/bin/env bash
set -euo pipefail
printf '%s\n' \
    'RETIRED: this legacy staked/public-bind launcher is disabled in the v0.8.0 recovery candidate.' \
    'Use Rust integration tests for local multi-node work; this command started no process and changed no data.' >&2
exit 78
