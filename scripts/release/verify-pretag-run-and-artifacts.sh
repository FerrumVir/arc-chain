#!/usr/bin/env bash
# Re-resolve a selected pre-tag run immediately before an exact-ID download.
set -Eeuo pipefail

die() {
    printf 'pre-tag live verification: %s\n' "$*" >&2
    exit 1
}

[ "$#" -eq 5 ] || die \
    "usage: $0 REPOSITORY COMMIT RUN_ID RUN_ATTEMPT EXPECTED_ARTIFACTS_JSON"

REPOSITORY="$1"
COMMIT="$2"
RUN_ID="$3"
RUN_ATTEMPT="$4"
EXPECTED_ARTIFACTS_JSON="$5"
SCRIPT_DIR="$(CDPATH='' cd -- "$(dirname -- "${BASH_SOURCE[0]}")" && pwd)"

[ "$REPOSITORY" = FerrumVir/arc-chain ] || die "unexpected repository"
printf '%s\n' "$COMMIT" | grep -Eq '^[0-9a-f]{40}$' || die "invalid commit"
printf '%s\n' "$RUN_ID" | grep -Eq '^[1-9][0-9]*$' || die "invalid run ID"
printf '%s\n' "$RUN_ATTEMPT" | grep -Eq '^[1-9][0-9]*$' || die "invalid run attempt"
[ -n "${GH_TOKEN:-}" ] || die "GH_TOKEN is required"
command -v gh >/dev/null 2>&1 || die "gh is required"
command -v jq >/dev/null 2>&1 || die "jq is required"

TEMPORARY="$(mktemp -d "${RUNNER_TEMP:-/tmp}/arc-pretag-live.XXXXXXXX")"
cleanup() {
    chmod -R u+rwX -- "$TEMPORARY" 2>/dev/null || true
    rm -rf -- "$TEMPORARY"
}
trap cleanup EXIT HUP INT TERM

WORKFLOW_ID="$(gh api \
    "repos/$REPOSITORY/actions/workflows/release-signing-preflight.yml" \
    --jq '.id')"
gh api "repos/$REPOSITORY/actions/runs/$RUN_ID" > "$TEMPORARY/run.json"
jq -e \
    --arg repository "$REPOSITORY" \
    --arg sha "$COMMIT" \
    --argjson workflow_id "$WORKFLOW_ID" \
    --argjson run_id "$RUN_ID" \
    --argjson attempt "$RUN_ATTEMPT" \
    '.id == $run_id
      and .workflow_id == $workflow_id
      and .head_repository.full_name == $repository
      and .head_branch == "main"
      and .head_sha == $sha
      and .event == "workflow_dispatch"
      and .status == "completed"
      and .conclusion == "success"
      and .run_attempt == $attempt' \
    "$TEMPORARY/run.json" >/dev/null || die \
        "selected run/attempt is no longer the completed successful exact-main preflight"

gh api --paginate \
    "repos/$REPOSITORY/actions/runs/$RUN_ID/artifacts?per_page=100" \
    --jq '.artifacts[]' | jq -s '{artifacts: .}' > "$TEMPORARY/artifacts.json"
python3 "$SCRIPT_DIR/select-pretag-artifacts.py" \
    --api-json "$TEMPORARY/artifacts.json" \
    --repository "$REPOSITORY" \
    --commit "$COMMIT" \
    --run-id "$RUN_ID" \
    --run-attempt "$RUN_ATTEMPT" \
    --expected-artifacts-json "$EXPECTED_ARTIFACTS_JSON" >/dev/null

printf 'Verified exact pre-tag run %s attempt %s and all nine artifact IDs/digests\n' \
    "$RUN_ID" "$RUN_ATTEMPT"
