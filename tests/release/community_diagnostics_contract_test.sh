#!/usr/bin/env bash
set -uo pipefail

TEST_DIR="$(CDPATH='' cd -- "$(dirname -- "$0")" && pwd)"
REPO_ROOT="$(CDPATH='' cd -- "$TEST_DIR/../.." && pwd)"
# shellcheck source=/dev/null
. "$TEST_DIR/helpers/testlib.sh"

DIAG="$REPO_ROOT/scripts/arc-diagnose.sh"

diagnostic_uses_only_active_six_seed_fleet() {
    local expected_count
    expected_count="$(grep -Eo 'https?://[0-9]+([.][0-9]+){3}:9090' "$DIAG" | sort -u | wc -l | tr -d ' ')"
    [ "$expected_count" -eq 6 ] || {
        printf 'diagnostic must contain exactly six unique public RPC origins; found %s\n' "$expected_count"
        return 1
    }
    if grep -Eq '216[.]238[.]120[.]27|139[.]84[.]237[.]49|SAO|JNB|8 seeds' "$DIAG"; then
        printf 'diagnostic still references retired validators or the old eight-seed claim\n'
        return 1
    fi
}

diagnostic_never_prints_process_secrets() {
    if grep -Eq 'pgrep[[:space:]]+-f|ps[[:space:]].*(-o|command)|/proc/.*/cmdline|printenv' "$DIAG"; then
        printf 'diagnostic reads or prints a process command line/environment\n'
        return 1
    fi
    grep -Fq 'arguments intentionally redacted' "$DIAG" \
        && grep -Fq 'contains no process arguments or validator secrets' "$DIAG"
}

diagnostic_proves_chain_agreement_not_dag_motion() {
    # shellcheck disable=SC2016 # Intentional production-source literals.
    for required in \
        '"$url/block/$COMMON_HEIGHT"' \
        'header["state_root"]' \
        'distinct block/state pairs' \
        'all-zero proof hash' \
        'neither proves chain agreement' \
        'strict quorum needs 5'
    do
        grep -Fq "$required" "$DIAG" || {
            printf 'diagnostic is missing canonical-chain check: %s\n' "$required"
            return 1
        }
    done
}

diagnostic_cli_is_fail_closed() {
    bash -n "$DIAG" || return 1
    "$DIAG" --help >/dev/null || return 1
    "$DIAG" --port 0 >/dev/null 2>&1 && {
        printf 'diagnostic accepted invalid port zero\n'
        return 1
    }
    "$DIAG" --timeout nope >/dev/null 2>&1 && {
        printf 'diagnostic accepted a nonnumeric timeout\n'
        return 1
    }
    return 0
}

run_test 'community diagnostic targets only the active six-validator fleet' diagnostic_uses_only_active_six_seed_fleet
run_test 'support output cannot leak process arguments or validator material' diagnostic_never_prints_process_secrets
run_test 'diagnostic gates same-height hash/root/proof instead of DAG motion' diagnostic_proves_chain_agreement_not_dag_motion
run_test 'diagnostic CLI rejects invalid safety-critical arguments' diagnostic_cli_is_fail_closed

finish_tests
