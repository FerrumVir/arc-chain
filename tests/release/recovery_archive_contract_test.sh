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
    'ensure_capture_and_freeze "$capture_id" nyc',
    'ensure_capture_and_freeze "$capture_id" lax',
    'QUORUM HALTED',
    'for node in "${REMAINING[@]}"; do',
    'run_remote "$node" freeze "$capture_id" "$node"',
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
    grep -Fq 'already-stopped process' "$NODE_HELPER" || return 1
    if grep -Eq 'kill[[:space:]]+-9|pkill[[:space:]]+-KILL' "$NODE_HELPER" "$ORCHESTRATOR"; then
        printf 'freeze path can force-kill a node before WAL flush\n'
        return 1
    fi
}

capture_requires_snapshot_endpoints_final_wal_and_exact_export() {
    for required in \
        '/sync/snapshot/info' \
        '/sync/snapshot' \
        'application/octet-stream' \
        'state.snapshot.lz4' \
        'state.wal' \
        'snapshot_pair_matches' \
        'recovery export' \
        '--data-dir "$capture_root"' \
        '--snapshot "$capture_root/state.snapshot.lz4"' \
        '--validator-public-keys "$stage_root/validator-public-keys.json"' \
        '--source-consensus-round "$source_round"' \
        '--allow-unbound-legacy-wal'
    do
        grep -Fq -- "$required" "$NODE_HELPER" || {
            printf 'snapshot-assisted recovery export contract is missing: %s\n' "$required"
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
        'canonical_match' \
        'canonical_count' \
        'none of the six preserved captures matches' \
        'all non-matching forks remain labelled and retained' \
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

archives_are_create_only_and_exclude_private_noncanonical_bulk() {
    for required in \
        'existing archive checksum failed; refusing replacement' \
        'partial archive or evidence exists; refusing replacement' \
        'archive_scope=public-chain-recovery-bundle-v2' \
        'excluded_private_material=true' \
        'excluded_service_environments=true' \
        'excluded_build_models_git_and_dag_trace=true'
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
run_test 'NYC and LAX halt quorum before the remaining live captures and clean stops' \
    freeze_halts_quorum_before_remaining_snapshots_and_stops
run_test 'every capture includes stable LZ4 evidence, final WAL, and exact recovery export' \
    capture_requires_snapshot_endpoints_final_wal_and_exact_export
run_test 'capture index fails on changed, missing, and unexpected bytes' \
    capture_index_detects_changed_missing_and_unexpected_bytes
run_test 'all six forks are labelled and retained while a real canonical match gates sealing' \
    forks_are_labelled_retained_and_only_canonical_match_gates_seal
run_test 'archives are create-only and exclude private keys, secrets, build/model/Git, and DAG bulk' \
    archives_are_create_only_and_exclude_private_noncanonical_bulk
run_test 'fleet archive scripts pass shell syntax and warning lint' archive_scripts_are_lintable

finish_tests
