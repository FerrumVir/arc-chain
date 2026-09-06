#!/usr/bin/env bash
set -Eeuo pipefail

TEST_DIR="$(CDPATH='' cd -- "$(dirname -- "$0")" && pwd)"
REPO_ROOT="$(CDPATH='' cd -- "$TEST_DIR/../.." && pwd)"
HELPER="$REPO_ROOT/scripts/release/wait-workflow-attempt.sh"
WORKDIR="$(mktemp -d "${TMPDIR:-/tmp}/arc-workflow-attempt.XXXXXX")"
cleanup() {
    rm -rf -- "$WORKDIR"
}
trap cleanup EXIT HUP INT TERM

mkdir -p "$WORKDIR/bin"
cat > "$WORKDIR/bin/gh" <<'MOCK'
#!/usr/bin/env bash
set -Eeuo pipefail
[ "$1" = api ]
[ "$2" = repos/FerrumVir/arc-chain/actions/runs/123/attempts/2 ]
count=0
[ ! -f "$ARC_TEST_COUNT" ] || count="$(cat "$ARC_TEST_COUNT")"
count=$((count + 1))
printf '%s\n' "$count" > "$ARC_TEST_COUNT"
if [ "$count" -eq 1 ]; then
  status=in_progress
  conclusion=null
else
  status=completed
  conclusion='"success"'
fi
printf '{"id":123,"run_attempt":2,"workflow_id":9,"path":".github/workflows/release.yml","event":"workflow_dispatch","head_branch":"main","head_sha":"%040d","head_repository":{"full_name":"FerrumVir/arc-chain"},"status":"%s","conclusion":%s}\n' 0 "$status" "$conclusion"
MOCK
chmod +x "$WORKDIR/bin/gh"

export ARC_TEST_COUNT="$WORKDIR/count"
export GH_TOKEN=test-token
export ARC_WORKFLOW_WAIT_MAX_POLLS=2
export ARC_WORKFLOW_WAIT_INTERVAL_SECONDS=0
PATH="$WORKDIR/bin:$PATH" "$HELPER" FerrumVir/arc-chain 123 2 success \
    > "$WORKDIR/result.json"
jq -e '.id == 123 and .run_attempt == 2 and .status == "completed"
  and .conclusion == "success"' "$WORKDIR/result.json" >/dev/null
[ "$(cat "$ARC_TEST_COUNT")" -eq 2 ]

if PATH="$WORKDIR/bin:$PATH" "$HELPER" FerrumVir/arc-chain 123 1 success \
    >"$WORKDIR/wrong.out" 2>"$WORKDIR/wrong.err"; then
    printf 'helper accepted a different attempt than the exact requested endpoint\n' >&2
    exit 1
fi
grep -Fq 'cannot read selected run attempt 123/1' "$WORKDIR/wrong.err"

if grep -Fq 'actions/runs/$RUN_ID"' "$HELPER"; then
    printf 'helper contains a moving latest-attempt endpoint\n' >&2
    exit 1
fi

printf 'exact workflow-attempt wait contract passed\n'
