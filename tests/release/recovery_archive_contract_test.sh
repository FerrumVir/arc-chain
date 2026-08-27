#!/usr/bin/env bash
set -uo pipefail

TEST_DIR="$(CDPATH='' cd -- "$(dirname -- "$0")" && pwd)"
REPO_ROOT="$(CDPATH='' cd -- "$TEST_DIR/../.." && pwd)"
# shellcheck source=/dev/null
. "$TEST_DIR/helpers/testlib.sh"

ORCHESTRATOR="$REPO_ROOT/scripts/recovery/archive-fleet-to-drive.sh"
NODE_HELPER="$REPO_ROOT/scripts/recovery/archive-node.sh"

archive_requires_exact_manifest_authorization() {
    for required in \
        'EXPECTED_GO="GO $MANIFEST_SHA256"' \
        '"${ARC_RECOVERY_GO:-}" != "$EXPECTED_GO"' \
        "grep -Eq '^[0-9a-f]{64}\$'" \
        'EXECUTE=false'
    do
        grep -Fq "$required" "$ORCHESTRATOR" || {
            printf 'archive execution gate is missing: %s\n' "$required"
            return 1
        }
    done
    "$ORCHESTRATOR" --help >/dev/null || return 1
    "$ORCHESTRATOR" --manifest nope --plan >/dev/null 2>&1 && return 1
    return 0
}

archive_freezes_every_legacy_node_without_forced_kill() {
    for node in nyc lax ams lhr nrt sgp; do
        grep -Fq "'$node=" "$ORCHESTRATOR" || {
            printf 'archive fleet omits node: %s\n' "$node"
            return 1
        }
    done
    grep -Fq 'pkill -TERM -x arc-node' "$NODE_HELPER" || return 1
    grep -Fq 'refusing SIGKILL and archive' "$NODE_HELPER" || return 1
    if grep -Eq 'kill[[:space:]]+-9|pkill[[:space:]]+-KILL' "$NODE_HELPER" "$ORCHESTRATOR"; then
        printf 'legacy archive path can force-kill a node before WAL flush\n'
        return 1
    fi
}

archive_is_create_only_and_hash_checked() {
    for required in \
        'existing archive checksum failed; refusing replacement' \
        'partial archive exists; refusing replacement' \
        'sha256sum --check' \
        'rclone copyto' \
        '--checksum --metadata' \
        'rclone check' \
        'ARC Chain Recovery'
    do
        grep -Fq -- "$required" "$NODE_HELPER" "$ORCHESTRATOR" || {
            printf 'archive integrity contract is missing: %s\n' "$required"
            return 1
        }
    done
    if grep -Eq 'rm[[:space:]]+-rf[[:space:]].*(arc-chain|arc-data|arc-recovery-archive)|rclone[[:space:]]+(delete|purge)' \
        "$NODE_HELPER" "$ORCHESTRATOR"; then
        printf 'archive path contains a legacy-data or Drive deletion operation\n'
        return 1
    fi
}

archive_scripts_are_lintable() {
    bash -n "$NODE_HELPER" "$ORCHESTRATOR" || return 1
    shellcheck -S warning "$NODE_HELPER" "$ORCHESTRATOR"
}

run_test 'fleet archive requires an exact lowercase manifest hash and GO phrase' archive_requires_exact_manifest_authorization
run_test 'all six validators get a clean TERM-only freeze before archival' archive_freezes_every_legacy_node_without_forced_kill
run_test 'legacy archives and Drive objects are create-only and hash-checked' archive_is_create_only_and_hash_checked
run_test 'fleet archive scripts pass shell syntax and warning lint' archive_scripts_are_lintable

finish_tests
