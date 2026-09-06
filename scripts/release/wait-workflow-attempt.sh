#!/usr/bin/env bash
# Wait for one immutable GitHub Actions run attempt, never the moving latest attempt.
set -Eeuo pipefail

die() {
    printf 'workflow-attempt wait: %s\n' "$*" >&2
    exit 1
}

[ "$#" -eq 4 ] || die \
    "usage: $0 REPOSITORY RUN_ID RUN_ATTEMPT EXPECTED_CONCLUSION"

REPOSITORY="$1"
RUN_ID="$2"
RUN_ATTEMPT="$3"
EXPECTED_CONCLUSION="$4"
MAX_POLLS="${ARC_WORKFLOW_WAIT_MAX_POLLS:-360}"
INTERVAL_SECONDS="${ARC_WORKFLOW_WAIT_INTERVAL_SECONDS:-10}"

[ "$REPOSITORY" = FerrumVir/arc-chain ] || die "unexpected repository"
printf '%s\n' "$RUN_ID" | grep -Eq '^[1-9][0-9]*$' || die "invalid run ID"
printf '%s\n' "$RUN_ATTEMPT" | grep -Eq '^[1-9][0-9]*$' || die "invalid run attempt"
printf '%s\n' "$MAX_POLLS" | grep -Eq '^[1-9][0-9]*$' || die "invalid poll limit"
printf '%s\n' "$INTERVAL_SECONDS" | grep -Eq '^(0|[1-9][0-9]*)$' \
    || die "invalid poll interval"
case "$EXPECTED_CONCLUSION" in
    success|failure|cancelled) ;;
    *) die "expected conclusion must be success, failure, or cancelled" ;;
esac
[ -n "${GH_TOKEN:-}" ] || die "GH_TOKEN is required"
command -v gh >/dev/null 2>&1 || die "gh is required"
command -v jq >/dev/null 2>&1 || die "jq is required"

attempt_url="repos/$REPOSITORY/actions/runs/$RUN_ID/attempts/$RUN_ATTEMPT"
poll=1
while [ "$poll" -le "$MAX_POLLS" ]; do
    run_json="$(gh api "$attempt_url")" \
        || die "cannot read selected run attempt $RUN_ID/$RUN_ATTEMPT"
    printf '%s' "$run_json" | jq -e \
        --arg repository "$REPOSITORY" \
        --argjson id "$RUN_ID" \
        --argjson attempt "$RUN_ATTEMPT" '
        .id == $id and .run_attempt == $attempt
        and .head_repository.full_name == $repository
        and (.status == "queued" or .status == "in_progress"
          or .status == "waiting" or .status == "pending"
          or .status == "requested" or .status == "completed")' \
        >/dev/null || die "selected run attempt identity/status differs"
    status="$(printf '%s' "$run_json" | jq -er '.status')"
    if [ "$status" = completed ]; then
        conclusion="$(printf '%s' "$run_json" | jq -er '.conclusion')"
        [ "$conclusion" = "$EXPECTED_CONCLUSION" ] || die \
            "run $RUN_ID attempt $RUN_ATTEMPT concluded $conclusion, expected $EXPECTED_CONCLUSION"
        printf '%s' "$run_json" | jq -cS \
            '{id,run_attempt,workflow_id,path,event,head_branch,head_sha,status,conclusion}'
        exit 0
    fi
    [ "$poll" -lt "$MAX_POLLS" ] || break
    sleep "$INTERVAL_SECONDS"
    poll=$((poll + 1))
done

die "run $RUN_ID attempt $RUN_ATTEMPT did not complete within $MAX_POLLS polls"
