#!/usr/bin/env bash
set -uo pipefail

TEST_DIR="$(CDPATH='' cd -- "$(dirname -- "$0")" && pwd)"
REPO_ROOT="$(CDPATH='' cd -- "$TEST_DIR/../.." && pwd)"
# shellcheck source=/dev/null
. "$TEST_DIR/helpers/testlib.sh"

ORCHESTRATOR="$REPO_ROOT/scripts/recovery/archive-fleet-to-drive.sh"
NODE_HELPER="$REPO_ROOT/scripts/recovery/archive-node.sh"

freeze_and_seal_have_independent_exact_authorizations() {
    for required in \
        "expected_go=\"FREEZE \$capture_id\"" \
        '"${ARC_RECOVERY_FREEZE_GO:-}" = "$expected_go"' \
        "expected_go=\"GO \$manifest_sha\"" \
        '"${ARC_RECOVERY_GO:-}" = "$expected_go"' \
        'execute=false' \
        "grep -Eq '^[0-9a-f]{64}\$'"
    do
        grep -Fq "$required" "$ORCHESTRATOR" || {
            printf 'two-phase execution gate is missing: %s\n' "$required"
            return 1
        }
    done
    "$ORCHESTRATOR" --help >/dev/null || return 1
    "$ORCHESTRATOR" capture --freeze-plan /does/not/exist --plan >/dev/null 2>&1 && return 1
    return 0
}

freeze_plan_is_canonical_create_only_and_tamper_evident() (
    local fixture
    fixture="$(mktemp -d)"
    trap 'rm -rf -- "$fixture"' EXIT
    local plan="$fixture/freeze.lock.json"
    "$ORCHESTRATOR" seal-freeze-plan --window release-contract --output "$plan" >/dev/null || return 1
    [ -f "$plan" ] && [ -f "$plan.sha256" ] || return 1
    python3 - "$plan" <<'PY' || return 1
import hashlib
import json
import pathlib
import stat
import sys
path = pathlib.Path(sys.argv[1])
assert stat.S_IMODE(path.stat().st_mode) & 0o222 == 0
assert stat.S_IMODE(path.with_name(path.name + ".sha256").stat().st_mode) & 0o222 == 0
value = json.loads(path.read_text())
assert value["schema"] == "arc.recovery.freeze-plan.v1"
assert value["sentinels"] == ["nyc", "lax"]
assert [item["name"] for item in value["nodes"]] == ["nyc", "lax", "ams", "lhr", "nrt", "sgp"]
payload = (json.dumps(value, sort_keys=True, separators=(",", ":")) + "\n").encode()
assert path.read_bytes() == payload
digest = hashlib.sha256(payload).hexdigest()
assert path.with_name(path.name + ".sha256").read_text() == f"{digest}  {path.name}\n"
PY
    "$ORCHESTRATOR" seal-freeze-plan --window replacement --output "$plan" >/dev/null 2>&1 && return 1
    return 0
)

freeze_halts_quorum_before_remaining_snapshots_and_stops() {
    for node in nyc lax ams lhr nrt sgp; do
        grep -Fq "'$node=" "$ORCHESTRATOR" || {
            printf 'archive fleet omits node: %s\n' "$node"
            return 1
        }
    done
    python3 - "$ORCHESTRATOR" <<'PY' || return 1
import pathlib
import sys
text = pathlib.Path(sys.argv[1]).read_text()
markers = [
    'ensure_stopped "$capture_id" nyc',
    'ensure_stopped "$capture_id" lax',
    'QUORUM HALTED',
    'for node in "${REMAINING[@]}"; do',
    'ALL LEGACY WRITERS HALTED',
    'for node in nyc lax ams lhr nrt sgp; do',
    'ensure_offline_capture "$capture_id" "$node"',
]
positions = []
cursor = 0
for marker in markers:
    cursor = text.index(marker, cursor)
    positions.append(cursor)
    cursor += len(marker)
assert positions == sorted(positions), positions
PY
    grep -Fq 'pkill -TERM -x arc-node' "$NODE_HELPER" || return 1
    grep -Fq 'refusing SIGKILL and freeze' "$NODE_HELPER" || return 1
    grep -Fq 'copy_stopped_data_tree' "$NODE_HELPER" || return 1
    grep -Fq 'refusing offline copy while arc-node is running' "$NODE_HELPER" || return 1
    for required in \
        'arc-self-heal.service arc-node.service' \
        'RefuseManualStart=yes' \
        'Restart=no' \
        'systemctl disable --now' \
        'legacy-service-fence.json' \
        'verify_legacy_restart_fence'
    do
        grep -Fq -- "$required" "$NODE_HELPER" || {
            printf 'persistent reboot-safe legacy service fence is missing: %s\n' "$required"
            return 1
        }
    done
    if grep -Eq 'kill[[:space:]]+-9|pkill[[:space:]]+-KILL' "$NODE_HELPER" "$ORCHESTRATOR"; then
        printf 'freeze path can force-kill a node before WAL flush\n'
        return 1
    fi
}

capture_requires_all_writers_stopped_complete_data_and_exact_export() {
    for required in \
        'fence-stop' \
        'stopped-status' \
        'capture-offline' \
        'complete_data_dir=true' \
        'source-data.files.sha256' \
        'copied-data.files.sha256' \
        'data-dir/state.wal' \
        'recovery export' \
        '--data-dir "$working_data"' \
        '--snapshot "$temporary/capture.snapshot.lz4"' \
        'capture-snapshot.selection.json' \
        'offline-wal-recovery.json' \
        'quarantined-wal-tail.bin' \
        'preserved_unclassified' \
        '--validator-public-keys "$stage_root/validator-public-keys.json"' \
        '--legacy-validator-set "$stage_root/legacy-validator-set-40m.json"' \
        'source_validator_count' \
        'source_validator_stake' \
        'source_validator_set_hash' \
        'legacy_validator_set_artifact_sha256' \
        'source_snapshot_artifact_sha256' \
        'reference_source_wal_artifact_sha256' \
        '--source-consensus-round "$source_round"' \
        '--created-at-unix-ms "$created_at_unix_ms"' \
        '--allow-unbound-legacy-wal'
    do
        grep -Fq -- "$required" "$NODE_HELPER" || {
            printf 'offline recovery export contract is missing: %s\n' "$required"
            return 1
        }
    done
    if grep -Fq '/sync/snapshot' "$NODE_HELPER"; then
        printf 'offline freeze must not depend on a live snapshot RPC\n'
        return 1
    fi
}

legacy_source_set_is_manifest_bound_staged_and_archived() {
    for required in \
        'artifacts.legacy_validator_set.path' \
        'artifacts.legacy_validator_set.sha256' \
        'artifacts.source_snapshot.sha256' \
        'artifacts.source_wal.sha256' \
        'stage_file "$node" "$manifest_sha" legacy-validators' \
        'legacy-validator-set-40m.json' \
        'upload_immutable "$legacy_validator_set"' \
        'upload_immutable "$source_snapshot"' \
        'upload_immutable "$source_wal"' \
        'verify_reference_pair'
    do
        grep -Fq -- "$required" "$ORCHESTRATOR" || {
            printf 'legacy source validator artifact is not sealed end to end: %s\n' "$required"
            return 1
        }
    done
}

capture_index_detects_changed_missing_and_unexpected_bytes() (
    local fixture
    fixture="$(mktemp -d)"
    trap 'rm -rf -- "$fixture"' EXIT
    mkdir -p "$fixture/evidence"
    printf 'snapshot-v1' > "$fixture/state.snapshot.lz4"
    printf 'wal-v1' > "$fixture/state.wal"
    printf '{"height":1,"state_root":"0x%064d"}\n' 0 > "$fixture/evidence/snapshot-info.json"
    python3 - "$fixture" <<'PY' || return 1
import hashlib
import pathlib
import sys
root = pathlib.Path(sys.argv[1])
rows = []
for path in sorted(p for p in root.rglob("*") if p.is_file()):
    rel = path.relative_to(root).as_posix()
    rows.append(f"{hashlib.sha256(path.read_bytes()).hexdigest()}  {rel}\n")
index = "".join(rows).encode()
(root / "capture.files.sha256").write_bytes(index)
index_sha = hashlib.sha256(index).hexdigest()
(root / "capture.complete").write_text(
    "schema=arc.recovery.capture.v2\n" + f"index_sha256={index_sha}\n"
)
PY
    "$NODE_HELPER" verify-index "$fixture" >/dev/null || return 1
    printf 'tamper' >> "$fixture/state.snapshot.lz4"
    "$NODE_HELPER" verify-index "$fixture" >/dev/null 2>&1 && return 1
    printf 'snapshot-v1' > "$fixture/state.snapshot.lz4"
    rm "$fixture/state.wal"
    "$NODE_HELPER" verify-index "$fixture" >/dev/null 2>&1 && return 1
    printf 'wal-v1' > "$fixture/state.wal"
    printf 'not indexed' > "$fixture/unexpected"
    "$NODE_HELPER" verify-index "$fixture" >/dev/null 2>&1 && return 1
    return 0
)

forks_are_labelled_retained_and_only_canonical_match_gates_seal() {
    for required in \
        'valid_canonical' \
        'valid_noncanonical_fork' \
        'preserved_unclassified' \
        'classification_reason' \
        'canonical_match' \
        'canonical_count' \
        'fork_count' \
        'unclassified_count' \
        'none of the six preserved captures matches' \
        'all six remain labelled and retained' \
        'for node in nyc lax ams lhr nrt sgp' \
        'rclone copyto' \
        '--immutable --checksum' \
        'rclone check' \
        'capture-id.txt'
    do
        grep -Fq -- "$required" "$NODE_HELPER" "$ORCHESTRATOR" || {
            printf 'fork preservation/seal contract is missing: %s\n' "$required"
            return 1
        }
    done
}

mixed_all_six_classifications_are_counted_without_dropping_invalid_evidence() (
    # Sourcing defines the exact production summarizer; the empty command only
    # prints usage and performs no mutation.
    . "$ORCHESTRATOR" >/dev/null
    local summary
    summary="$(printf '%s\n' \
        '{"node":"nyc","classification":"valid_canonical"}' \
        '{"node":"lax","classification":"valid_noncanonical_fork"}' \
        '{"node":"ams","classification":"valid_noncanonical_fork"}' \
        '{"node":"lhr","classification":"valid_noncanonical_fork"}' \
        '{"node":"nrt","classification":"valid_noncanonical_fork"}' \
        '{"node":"sgp","classification":"preserved_unclassified"}' \
        | summarize_binding_statuses)" || return 1
    [ "$summary" = '1 4 1' ] || return 1
    printf '%s\n' \
        '{"node":"nyc","classification":"valid_canonical"}' \
        '{"node":"nyc","classification":"valid_noncanonical_fork"}' \
        '{"node":"ams","classification":"valid_noncanonical_fork"}' \
        '{"node":"lhr","classification":"valid_noncanonical_fork"}' \
        '{"node":"nrt","classification":"valid_noncanonical_fork"}' \
        '{"node":"sgp","classification":"preserved_unclassified"}' \
        | summarize_binding_statuses >/dev/null 2>&1 && return 1
    return 0
)

offline_wal_boundary_slices_and_reconstructs_exact_immutable_bytes() (
    . "$NODE_HELPER" >/dev/null
    local fixture="$TEST_TMP/wal-boundary"
    mkdir -p "$fixture/evidence"
    printf 'committed-tail' > "$fixture/state.wal"
    cat > "$fixture/export-summary.json" <<'JSON'
{"source_wal_original_bytes":14,"source_wal_accepted_prefix_bytes":9,"source_wal_quarantined_tail_bytes":5,"source_wal_tail_reason":"uncommitted records after the last complete SetBlock + Checkpoint"}
JSON
    write_offline_wal_evidence \
        "$fixture/state.wal" "$fixture/export-summary.json" \
        "$fixture/offline-wal-recovery.json" "$fixture/evidence" || return 1
    [ "$(cat "$fixture/evidence/recovered-state.wal")" = committed ] || return 1
    [ "$(cat "$fixture/evidence/quarantined-wal-tail.bin")" = -tail ] || return 1
    cat "$fixture/evidence/recovered-state.wal" \
        "$fixture/evidence/quarantined-wal-tail.bin" > "$fixture/reconstructed.wal"
    cmp --silent "$fixture/state.wal" "$fixture/reconstructed.wal" || return 1
    python3 - "$fixture/offline-wal-recovery.json" <<'PY' || return 1
import json
import pathlib
import sys
value = json.loads(pathlib.Path(sys.argv[1]).read_text(encoding="utf-8"))
assert value["accepted_prefix_bytes"] == 9
assert value["quarantined_tail_bytes"] == 5
assert value["prefix_plus_tail_reconstructs_capture"] is True
assert value["prefix_plus_tail_sha256"] == value["capture_wal_sha256"]
PY

    cat > "$fixture/missing-boundary.json" <<'JSON'
{"status":"EXPORTED_UNSIGNED"}
JSON
    mkdir "$fixture/rejected"
    write_offline_wal_evidence \
        "$fixture/state.wal" "$fixture/missing-boundary.json" \
        "$fixture/should-not-exist.json" "$fixture/rejected" \
        >/dev/null 2>&1 && return 1
    [ ! -e "$fixture/should-not-exist.json" ] || return 1
    return 0
)

drive_upload_is_operator_owned_bounded_parallel_and_aggregate_checked() {
    for required in \
        'local upload_order=(nyc lax ams lhr nrt sgp)' \
        'for upload_index in 0 3; do' \
        '"${upload_order[@]:upload_index:3}"' \
        '--transfers 1 --checkers 1 --retries 5 --low-level-retries 20' \
        'rclone check "$source_dir" "$destination"' \
        '"$log_root/$node-upload.log"' \
        'one or more preserved validator uploads failed; COMPLETE was not emitted'
    do
        grep -Fq -- "$required" "$ORCHESTRATOR" || {
            printf 'bounded operator-owned archive upload contract is missing: %s\n' "$required"
            return 1
        }
    done
    if grep -Eq 'run_remote[^\n]*(rclone|DRIVE_REMOTE)|scp[^\n]*(rclone\.conf|Drive)' \
        "$ORCHESTRATOR"; then
        printf 'archive flow can distribute Drive configuration or credentials to validators\n'
        return 1
    fi
}

archives_are_create_only_and_exclude_private_noncanonical_bulk() {
    for required in \
        'existing archive checksum failed; refusing replacement' \
        'partial archive or evidence exists; refusing replacement' \
        'archive_scope=complete-stopped-legacy-data-v3' \
        'complete_data_dir=true' \
        'excluded_outside_data_dir_private_material=true' \
        'excluded_service_environments=true' \
        'excluded_build_models_and_git=true'
    do
        grep -Fq -- "$required" "$NODE_HELPER" || {
            printf 'archive create-only/exclusion contract is missing: %s\n' "$required"
            return 1
        }
    done
    if grep -Eq 'rclone[[:space:]]+(delete|purge)|rm[[:space:]]+-rf[[:space:]]+--[[:space:]]+/(root/)?arc-(chain|data|recovery-captures|recovery-archive)' \
        "$NODE_HELPER" "$ORCHESTRATOR"; then
        printf 'archive path can delete legacy/capture/Drive data\n'
        return 1
    fi
}

archive_scripts_are_lintable() {
    bash -n "$NODE_HELPER" "$ORCHESTRATOR" || return 1
    shellcheck -S warning "$NODE_HELPER" "$ORCHESTRATOR"
}

run_test 'freeze capture and final checkpoint seal require independent exact hashes' \
    freeze_and_seal_have_independent_exact_authorizations
run_test 'freeze plan is canonical, create-only, read-only, and checksum-bound' \
    freeze_plan_is_canonical_create_only_and_tamper_evident
run_test 'NYC and LAX halt quorum before every all-six offline data copy' \
    freeze_halts_quorum_before_remaining_snapshots_and_stops
run_test 'every capture is a complete stopped data directory classified by exact export' \
    capture_requires_all_writers_stopped_complete_data_and_exact_export
run_test 'legacy source validator set is manifest-bound, staged, verified, and archived' \
    legacy_source_set_is_manifest_bound_staged_and_archived
run_test 'capture index fails on changed, missing, and unexpected bytes' \
    capture_index_detects_changed_missing_and_unexpected_bytes
run_test 'all six forks are labelled and retained while a real canonical match gates sealing' \
    forks_are_labelled_retained_and_only_canonical_match_gates_seal
run_test 'one canonical, four forks, and one unclassified capture are all retained and counted' \
    mixed_all_six_classifications_are_counted_without_dropping_invalid_evidence
run_test 'exporter boundary slices exact WAL prefix/tail and reconstructs immutable bytes' \
    offline_wal_boundary_slices_and_reconstructs_exact_immutable_bytes
run_test 'Drive upload uses two bounded three-node batches and aggregates every check' \
    drive_upload_is_operator_owned_bounded_parallel_and_aggregate_checked
run_test 'archives are create-only, retain complete chain data, and exclude out-of-tree secrets/build/Git' \
    archives_are_create_only_and_exclude_private_noncanonical_bulk
run_test 'fleet archive scripts pass shell syntax and warning lint' archive_scripts_are_lintable

finish_tests
