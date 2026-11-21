#!/usr/bin/env bash
set -euo pipefail
cd "$(dirname "$0")"
if [[ ${1:-} == --analyzer-tests ]]; then
  exec python3 -m unittest arc_soak.tests.test_analyze
fi
exec python3 -m arc_soak.orchestrate "$@"
