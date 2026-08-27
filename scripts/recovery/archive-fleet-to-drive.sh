#!/usr/bin/env bash
# Two-phase six-validator freeze, checkpoint binding, and immutable archive.
# Dry-run is the default for both mutating phases.
set -Eeuo pipefail
umask 077

SCRIPT_DIR="$(CDPATH='' cd -- "$(dirname -- "${BASH_SOURCE[0]}")" && pwd)"
REMOTE_HELPER="$SCRIPT_DIR/archive-node.sh"
ROLLOUT_TOOL="$SCRIPT_DIR/recovery_rollout.py"
DRIVE_REMOTE="${ARC_RECOVERY_DRIVE_REMOTE:-arc-drive:ARC Chain Recovery}"
SSH_USER="${ARC_RECOVERY_SSH_USER:-root}"

NODES=(
    'nyc=149.28.32.76'
    'lax=140.82.16.112'
    'ams=136.244.109.1'
    'lhr=104.238.171.11'
    'nrt=202.182.107.41'
    'sgp=149.28.153.31'
)
SENTINELS=(nyc lax)
REMAINING=(ams lhr nrt sgp)
SSH_OPTIONS=(-o BatchMode=yes -o ConnectTimeout=10 -o StrictHostKeyChecking=yes)
ARCHIVE_FLEET_TEMP_ROOT=""

cleanup_temporary_root() {
    if [ -n "$ARCHIVE_FLEET_TEMP_ROOT" ]; then
        rm -rf -- "$ARCHIVE_FLEET_TEMP_ROOT"
    fi
}

die() {
    printf 'archive fleet: %s\n' "$*" >&2
    exit 1
}

usage() {
    cat <<'EOF'
Usage:
  archive-fleet-to-drive.sh seal-freeze-plan --window ID --output /absolute/freeze.lock.json

  archive-fleet-to-drive.sh capture --freeze-plan /absolute/freeze.lock.json [--plan]
  ARC_RECOVERY_FREEZE_GO='FREEZE SHA256' archive-fleet-to-drive.sh capture \
    --freeze-plan /absolute/freeze.lock.json --execute

  archive-fleet-to-drive.sh seal --freeze-plan /absolute/freeze.lock.json \
    --manifest /absolute/rollout.lock.json \
    --validator-public-keys /absolute/validators.json \
    [--allow-unbound-legacy-wal] [--plan]
  ARC_RECOVERY_GO='GO ROLLOUT_SHA256' archive-fleet-to-drive.sh seal \
    --freeze-plan /absolute/freeze.lock.json \
    --manifest /absolute/rollout.lock.json \
    --validator-public-keys /absolute/validators.json \
    --allow-unbound-legacy-wal --execute

The freeze plan is sealed before the final checkpoint exists. `capture`
persistently fences and cleanly stops NYC, then LAX, reducing six equal-stake
validators below the five-validator quorum before any chain directory is
copied. It then fences/stops the remaining four writers and copies each
complete on-disk data directory offline. No legacy byte is deleted.

`seal` runs only after the final 5-of-6 checkpoint and rollout manifest exist.
The exact recovery exporter verifies each stopped WAL only with that capture's
own on-disk snapshot. A derivable pair is labelled canonical or a fork; a
missing, ambiguous, or torn pair is preserved_unclassified and is never
combined with the canonical reference snapshot. Every capture is bundled and
all six bundles plus the sealed public inputs are uploaded under the
rollout-manifest hash.
EOF
}

require_hash() {
    printf '%s\n' "$1" | grep -Eq '^[0-9a-f]{64}$' || \
        die "$2 must be exactly 64 lowercase hexadecimal characters"
}

require_absolute_file() {
    case "$1" in /*) ;; *) die "$2 must be an absolute path" ;; esac
    [ -f "$1" ] && [ ! -L "$1" ] || die "$2 is missing, non-regular, or a symlink: $1"
}

require_commands() {
    local command_name
    for command_name in "$@"; do
        command -v "$command_name" >/dev/null 2>&1 || die "required command is missing: $command_name"
    done
}

hash_file() {
    python3 - "$1" <<'PY'
import hashlib
import sys
digest = hashlib.sha256()
with open(sys.argv[1], "rb") as handle:
    for chunk in iter(lambda: handle.read(1024 * 1024), b""):
        digest.update(chunk)
print(digest.hexdigest())
PY
}

host_for() {
    local wanted="$1"
    local entry
    for entry in "${NODES[@]}"; do
        if [ "${entry%%=*}" = "$wanted" ]; then
            printf '%s\n' "${entry#*=}"
            return 0
        fi
    done
    die "unknown node: $wanted"
}

seal_freeze_plan() {
    local window="" output=""
    while [ "$#" -gt 0 ]; do
        case "$1" in
            --window) [ "$#" -ge 2 ] || die "--window needs a value"; window="$2"; shift 2 ;;
            --output) [ "$#" -ge 2 ] || die "--output needs a value"; output="$2"; shift 2 ;;
            -h|--help) usage; return 0 ;;
            *) die "unknown seal-freeze-plan option: $1" ;;
        esac
    done
    printf '%s\n' "$window" | grep -Eq '^[A-Za-z0-9][A-Za-z0-9._:@+-]{0,127}$' || \
        die "--window must be a short reviewable change/window identifier"
    case "$output" in /*.json) ;; *) die "--output must be an absolute .json path" ;; esac
    require_commands python3
    python3 - "$output" "$window" "${NODES[@]}" <<'PY'
import datetime
import hashlib
import json
import os
import pathlib
import sys

output = pathlib.Path(sys.argv[1])
window = sys.argv[2]
nodes = []
for entry in sys.argv[3:]:
    name, host = entry.split("=", 1)
    nodes.append({"name": name, "host": host})
plan = {
    "schema": "arc.recovery.freeze-plan.v1",
    "window": window,
    "created_at": datetime.datetime.now(datetime.timezone.utc).replace(microsecond=0).isoformat().replace("+00:00", "Z"),
    "sentinels": ["nyc", "lax"],
    "nodes": nodes,
}
payload = (json.dumps(plan, sort_keys=True, separators=(",", ":")) + "\n").encode()
digest = hashlib.sha256(payload).hexdigest()
sidecar = output.with_name(output.name + ".sha256")
if output.exists() or sidecar.exists():
    raise SystemExit("freeze plan or sidecar already exists; refusing replacement")
output.parent.mkdir(parents=True, exist_ok=True)
created = []
try:
    fd = os.open(output, os.O_WRONLY | os.O_CREAT | os.O_EXCL, 0o444)
    with os.fdopen(fd, "wb") as handle:
        handle.write(payload)
        handle.flush()
        os.fsync(handle.fileno())
    created.append(output)
    fd = os.open(sidecar, os.O_WRONLY | os.O_CREAT | os.O_EXCL, 0o444)
    with os.fdopen(fd, "w", encoding="ascii", newline="\n") as handle:
        handle.write(f"{digest}  {output.name}\n")
        handle.flush()
        os.fsync(handle.fileno())
    created.append(sidecar)
    directory_fd = os.open(output.parent, os.O_RDONLY)
    try:
        os.fsync(directory_fd)
    finally:
        os.close(directory_fd)
except Exception:
    for path in reversed(created):
        path.chmod(0o600)
        path.unlink()
    raise
print(digest)
PY
    local digest
    digest="$(hash_file "$output")"
    printf 'archive fleet: sealed freeze plan %s\n' "$output"
    printf 'archive fleet: capture id %s\n' "$digest"
    printf "archive fleet: execution authorization ARC_RECOVERY_FREEZE_GO='FREEZE %s'\n" "$digest"
}

freeze_plan_hash() {
    local plan="$1"
    require_absolute_file "$plan" "freeze plan"
    python3 - "$plan" "${NODES[@]}" <<'PY'
import hashlib
import json
import pathlib
import re
import stat
import sys

path = pathlib.Path(sys.argv[1])
sidecar = path.with_name(path.name + ".sha256")
for candidate in (path, sidecar):
    details = candidate.lstat()
    if stat.S_ISLNK(details.st_mode) or not stat.S_ISREG(details.st_mode) or details.st_mode & 0o222:
        raise SystemExit(f"sealed freeze plan input is mutable or not regular: {candidate}")
expected_nodes = []
for entry in sys.argv[2:]:
    name, host = entry.split("=", 1)
    expected_nodes.append({"name": name, "host": host})
value = json.loads(path.read_text(encoding="utf-8"))
if set(value) != {"schema", "window", "created_at", "sentinels", "nodes"}:
    raise SystemExit("freeze plan has missing or unknown fields")
if value["schema"] != "arc.recovery.freeze-plan.v1":
    raise SystemExit("unsupported freeze plan schema")
if not isinstance(value["window"], str) or not re.fullmatch(r"[A-Za-z0-9][A-Za-z0-9._:@+-]{0,127}", value["window"]):
    raise SystemExit("freeze plan window is invalid")
if not isinstance(value["created_at"], str) or not re.fullmatch(r"[0-9]{4}-[0-9]{2}-[0-9]{2}T[0-9]{2}:[0-9]{2}:[0-9]{2}Z", value["created_at"]):
    raise SystemExit("freeze plan timestamp is invalid")
if value["sentinels"] != ["nyc", "lax"] or value["nodes"] != expected_nodes:
    raise SystemExit("freeze plan fleet or sentinel order differs from the reviewed six-node topology")
payload = (json.dumps(value, sort_keys=True, separators=(",", ":")) + "\n").encode()
if path.read_bytes() != payload:
    raise SystemExit("freeze plan is not canonical JSON")
digest = hashlib.sha256(payload).hexdigest()
if sidecar.read_text(encoding="ascii") != f"{digest}  {path.name}\n":
    raise SystemExit("freeze plan checksum sidecar differs")
print(digest)
PY
}

REMOTE_HELPER_PATH=""

install_helpers() {
    local helper_sha node host
    helper_sha="$(hash_file "$REMOTE_HELPER")"
    REMOTE_HELPER_PATH="/root/.arc-recovery-archive-node-$helper_sha.sh"
    for node in nyc lax ams lhr nrt sgp; do
        host="$(host_for "$node")"
        scp -q "${SSH_OPTIONS[@]}" "$REMOTE_HELPER" "$SSH_USER@$host:$REMOTE_HELPER_PATH"
        ssh "${SSH_OPTIONS[@]}" "$SSH_USER@$host" -- sh -c \
            'chmod 500 -- "$1"; test "$(sha256sum "$1" | cut -d" " -f1)" = "$2"' \
            sh "$REMOTE_HELPER_PATH" "$helper_sha"
    done
}

run_remote() {
    local node="$1"
    shift
    local host
    host="$(host_for "$node")"
    ssh "${SSH_OPTIONS[@]}" "$SSH_USER@$host" -- "$REMOTE_HELPER_PATH" "$@"
}

remote_readiness() {
    local capture_id="$1"
    local node host
    for node in nyc lax ams lhr nrt sgp; do
        host="$(host_for "$node")"
        ssh "${SSH_OPTIONS[@]}" "$SSH_USER@$host" -- sh -c \
            'test -d /root/arc-chain && test ! -L /root/arc-chain && command -v curl >/dev/null && command -v python3 >/dev/null && command -v sha256sum >/dev/null && command -v zstd >/dev/null && { test ! -e "$1" || test -d "$1"; }' \
            sh "/root/arc-recovery-captures/$capture_id/$node"
        printf '  ready:    %s %s\n' "$node" "$host"
    done
}

ensure_stopped() {
    local capture_id="$1" node="$2"
    if run_remote "$node" stopped-status "$capture_id" "$node" >/dev/null 2>&1; then
        run_remote "$node" stopped-status "$capture_id" "$node"
        return 0
    fi
    run_remote "$node" fence-stop "$capture_id" "$node"
    run_remote "$node" stopped-status "$capture_id" "$node"
}

ensure_offline_capture() {
    local capture_id="$1" node="$2"
    if run_remote "$node" status "$capture_id" "$node" >/dev/null 2>&1; then
        run_remote "$node" status "$capture_id" "$node"
        return 0
    fi
    run_remote "$node" capture-offline "$capture_id" "$node"
    run_remote "$node" status "$capture_id" "$node"
}

capture_phase() {
    local freeze_plan="" execute=false
    while [ "$#" -gt 0 ]; do
        case "$1" in
            --freeze-plan) [ "$#" -ge 2 ] || die "--freeze-plan needs a value"; freeze_plan="$2"; shift 2 ;;
            --execute) execute=true; shift ;;
            --plan) execute=false; shift ;;
            -h|--help) usage; return 0 ;;
            *) die "unknown capture option: $1" ;;
        esac
    done
    require_commands python3 ssh scp grep
    [ -x "$REMOTE_HELPER" ] || die "remote helper is missing or not executable"
    local capture_id
    capture_id="$(freeze_plan_hash "$freeze_plan")"
    printf 'ARC staged legacy freeze plan\n'
    printf '  capture:  %s\n' "$capture_id"
    printf '  sentinels: %s (quorum is halted after the second clean stop)\n' "${SENTINELS[*]}"
    printf '  remaining: AMS LHR NRT SGP fence/stop after quorum halt; then all-six offline copy\n'
    remote_readiness "$capture_id"
    if [ "$execute" != true ]; then
        printf 'archive fleet: PLAN ONLY; no service or remote/local file was changed\n'
        return 0
    fi
    local expected_go="FREEZE $capture_id"
    [ "${ARC_RECOVERY_FREEZE_GO:-}" = "$expected_go" ] || \
        die "execution requires ARC_RECOVERY_FREEZE_GO='$expected_go'"

    install_helpers
    printf 'archive fleet: persistently fencing and stopping first sentinel NYC\n'
    ensure_stopped "$capture_id" nyc
    printf 'archive fleet: persistently fencing and stopping second sentinel LAX\n'
    ensure_stopped "$capture_id" lax
    run_remote nyc stopped-status "$capture_id" nyc >/dev/null
    run_remote lax stopped-status "$capture_id" lax >/dev/null
    printf 'archive fleet: QUORUM HALTED (at most 4/6 legacy validators are running; 5 are required)\n'

    local log_root node
    log_root="$(mktemp -d)"
    ARCHIVE_FLEET_TEMP_ROOT="$log_root"
    trap cleanup_temporary_root EXIT
    local pids=() names=()
    for node in "${REMAINING[@]}"; do
        (
            ensure_stopped "$capture_id" "$node"
        ) > "$log_root/$node-stop.log" 2>&1 &
        pids+=("$!")
        names+=("$node")
    done
    local failed=0 index
    for index in "${!pids[@]}"; do
        if wait "${pids[$index]}"; then
            sed -n '1,30p' "$log_root/${names[$index]}-stop.log"
        else
            printf 'archive fleet: persistent writer stop failed: %s\n' "${names[$index]}" >&2
            sed -n '1,100p' "$log_root/${names[$index]}-stop.log" >&2
            failed=1
        fi
    done
    [ "$failed" -eq 0 ] || die "at least one post-halt writer stop failed; stopped nodes remain persistently fenced"
    for node in nyc lax ams lhr nrt sgp; do
        run_remote "$node" stopped-status "$capture_id" "$node" >/dev/null
    done
    printf 'archive fleet: ALL LEGACY WRITERS HALTED; beginning offline all-six data copies\n'

    pids=() names=()
    for node in nyc lax ams lhr nrt sgp; do
        ensure_offline_capture "$capture_id" "$node" > "$log_root/$node-capture.log" 2>&1 &
        pids+=("$!")
        names+=("$node")
    done
    failed=0
    for index in "${!pids[@]}"; do
        if wait "${pids[$index]}"; then
            sed -n '1,30p' "$log_root/${names[$index]}-capture.log"
        else
            printf 'archive fleet: offline data capture failed: %s\n' "${names[$index]}" >&2
            sed -n '1,100p' "$log_root/${names[$index]}-capture.log" >&2
            failed=1
        fi
    done
    [ "$failed" -eq 0 ] || die "at least one stopped data directory was not captured; no SIGKILL or overwrite was attempted"
    for node in nyc lax ams lhr nrt sgp; do
        run_remote "$node" status "$capture_id" "$node"
    done
    printf 'archive fleet: OFFLINE CAPTURE COMPLETE capture=%s; all six legacy nodes remain fenced/stopped\n' "$capture_id"
    printf 'archive fleet: next create/sign/seal the recovery checkpoint from an accepted capture; do not restart legacy nodes\n'
}

manifest_field() {
    local manifest="$1" path="$2"
    python3 - "$manifest" "$path" <<'PY'
import json
import sys
with open(sys.argv[1], "r", encoding="utf-8") as handle:
    value = json.load(handle)
for part in sys.argv[2].split("."):
    value = value[part]
if isinstance(value, bool):
    print(str(value).lower())
elif isinstance(value, (str, int)):
    print(value)
else:
    raise SystemExit("manifest field is not a scalar")
PY
}

verify_rollout_and_capture_topology() {
    local manifest="$1" freeze_plan="$2"
    PYTHONPATH="$SCRIPT_DIR" python3 - "$manifest" "$freeze_plan" <<'PY'
import json
import pathlib
import sys
import recovery_rollout as rr

manifest_path = pathlib.Path(sys.argv[1])
manifest, digest = rr.load_sealed_manifest(manifest_path)
if manifest["mode"] != "production":
    rr.fail("fleet archive sealing requires a production rollout manifest")
rr.verify_artifacts(manifest)
rr.RecoveryRollout(manifest, digest).verify_checkpoint()
freeze = json.loads(pathlib.Path(sys.argv[2]).read_text(encoding="utf-8"))
captured = sorted((entry["name"], entry["host"]) for entry in freeze["nodes"])
rollout = sorted((entry["name"], entry["host"]) for entry in manifest["validators"])
if captured != rollout:
    rr.fail("rollout validator names/hosts differ from the sealed freeze plan")
print(digest)
PY
}

stage_file() {
    local node="$1" manifest="$2" role="$3" path="$4" expected_sha="$5"
    run_remote "$node" stage-input "$manifest" "$node" "$role" "$expected_sha" < "$path"
}

upload_immutable() {
    local source="$1" destination="$2"
    rclone copyto "$source" "$destination" --immutable --checksum --metadata \
        --retries 5 --low-level-retries 20
}

summarize_binding_statuses() {
    python3 -c '
import json, sys
expected = {"nyc", "lax", "ams", "lhr", "nrt", "sgp"}
rows = [json.loads(line) for line in sys.stdin if line.strip()]
names = [row.get("node") for row in rows]
if len(rows) != 6 or set(names) != expected or len(names) != len(set(names)):
    raise SystemExit("binding status stream must contain each reviewed validator exactly once")
allowed = {"valid_canonical", "valid_noncanonical_fork", "preserved_unclassified"}
if any(row.get("classification") not in allowed for row in rows):
    raise SystemExit("binding status stream contains an unknown classification")
counts = [sum(row["classification"] == item for row in rows) for item in (
    "valid_canonical", "valid_noncanonical_fork", "preserved_unclassified"
)]
print(*counts)
'
}

verify_reference_pair() {
    local binary="$1" genesis="$2" validators="$3" legacy_validators="$4"
    local snapshot="$5" source_wal="$6" source_round="$7" created_at="$8"
    local recovery_epoch="$9" validator_set_id="${10}" source_height="${11}"
    local source_hash="${12}" source_state_root="${13}" transition_state_root="${14}"
    local checkpoint_manifest="${15}" allow_unbound="${16}"
    local temporary
    temporary="$(mktemp -d)"
    cp -- "$source_wal" "$temporary/state.wal"
    local command=(
        "$binary" recovery export
        --data-dir "$temporary"
        --snapshot "$snapshot"
        --genesis "$genesis"
        --validator-public-keys "$validators"
        --legacy-validator-set "$legacy_validators"
        --output "$temporary/reference.arcchkpt"
        --source-consensus-round "$source_round"
        --created-at-unix-ms "$created_at"
        --recovery-epoch "$recovery_epoch"
        --validator-set-id "$validator_set_id"
    )
    if [ "$allow_unbound" = true ]; then
        command+=(--allow-unbound-legacy-wal)
    fi
    "${command[@]}" > "$temporary/summary.json" 2> "$temporary/export.stderr"
    python3 - "$temporary/summary.json" "$source_height" "$source_hash" \
        "$source_state_root" "$transition_state_root" "$checkpoint_manifest" \
        "$source_round" "$created_at" "$recovery_epoch" "$validator_set_id" <<'PY'
import json
import sys

(path, source_height, source_hash, source_state_root, transition_state_root,
 checkpoint_manifest, source_round, created_at, recovery_epoch,
 validator_set_id) = sys.argv[1:]
value = json.load(open(path, encoding="utf-8"))

def bare(raw):
    if not isinstance(raw, str):
        raise SystemExit("reference export omitted a hash")
    raw = raw.removeprefix("0x")
    if len(raw) != 64 or any(char not in "0123456789abcdef" for char in raw):
        raise SystemExit("reference export emitted a malformed hash")
    return raw

expected = {
    "status": "EXPORTED_UNSIGNED",
    "source_height": int(source_height),
    "source_block_hash": bare(source_hash),
    "source_state_root": bare(source_state_root),
    "full_state_root": bare(transition_state_root),
    "manifest_hash": bare(checkpoint_manifest),
    "source_consensus_round": int(source_round),
    "created_at_unix_ms": int(created_at),
    "recovery_epoch": int(recovery_epoch),
    "validator_set_id": int(validator_set_id),
    "source_validator_count": 8,
    "source_validator_stake": 40_000_000,
    "source_validator_set_hash": "80d7c2d229fea4171732fd04451372d849fab7baefed143a2a445ae72f472ecd",
}
for field, wanted in expected.items():
    got = value.get(field)
    if field.endswith(("hash", "root")):
        got = bare(got)
    if got != wanted:
        raise SystemExit(f"sealed reference snapshot/WAL {field} differs: expected {wanted!r}, got {got!r}")
PY
    find "$temporary" -depth -delete
    printf 'archive fleet: PASS sealed source snapshot/WAL independently reproduces the selected checkpoint\n'
}

seal_phase() {
    local freeze_plan="" manifest="" validators="" execute=false allow_unbound=false
    while [ "$#" -gt 0 ]; do
        case "$1" in
            --freeze-plan) [ "$#" -ge 2 ] || die "--freeze-plan needs a value"; freeze_plan="$2"; shift 2 ;;
            --manifest) [ "$#" -ge 2 ] || die "--manifest needs a value"; manifest="$2"; shift 2 ;;
            --validator-public-keys) [ "$#" -ge 2 ] || die "--validator-public-keys needs a value"; validators="$2"; shift 2 ;;
            --allow-unbound-legacy-wal) allow_unbound=true; shift ;;
            --execute) execute=true; shift ;;
            --plan) execute=false; shift ;;
            -h|--help) usage; return 0 ;;
            *) die "unknown seal option: $1" ;;
        esac
    done
    require_commands python3 ssh scp rclone grep mktemp cp find
    require_absolute_file "$manifest" "rollout manifest"
    require_absolute_file "$validators" "validator public-key file"
    [ -x "$REMOTE_HELPER" ] || die "remote helper is missing or not executable"
    [ -f "$ROLLOUT_TOOL" ] || die "recovery rollout verifier is missing"
    local capture_id verification_output manifest_sha
    capture_id="$(freeze_plan_hash "$freeze_plan")"
    verification_output="$(verify_rollout_and_capture_topology "$manifest" "$freeze_plan")"
    printf '%s\n' "$verification_output"
    manifest_sha="$(printf '%s\n' "$verification_output" | tail -n 1)"
    require_hash "$manifest_sha" "rollout manifest hash"
    local validator_sha
    validator_sha="$(hash_file "$validators")"

    local binary genesis checkpoint legacy_validator_set source_snapshot source_wal
    local binary_sha genesis_sha checkpoint_sha legacy_validator_set_sha source_snapshot_sha source_wal_sha
    local source_height source_hash source_state_root transition_state_root checkpoint_manifest
    local source_round created_at_unix_ms recovery_epoch validator_set_id
    binary="$(manifest_field "$manifest" artifacts.binary.path)"
    binary_sha="$(manifest_field "$manifest" artifacts.binary.sha256)"
    genesis="$(manifest_field "$manifest" artifacts.genesis.path)"
    genesis_sha="$(manifest_field "$manifest" artifacts.genesis.sha256)"
    checkpoint="$(manifest_field "$manifest" artifacts.checkpoint.path)"
    checkpoint_sha="$(manifest_field "$manifest" artifacts.checkpoint.sha256)"
    legacy_validator_set="$(manifest_field "$manifest" artifacts.legacy_validator_set.path)"
    legacy_validator_set_sha="$(manifest_field "$manifest" artifacts.legacy_validator_set.sha256)"
    source_snapshot="$(manifest_field "$manifest" artifacts.source_snapshot.path)"
    source_snapshot_sha="$(manifest_field "$manifest" artifacts.source_snapshot.sha256)"
    source_wal="$(manifest_field "$manifest" artifacts.source_wal.path)"
    source_wal_sha="$(manifest_field "$manifest" artifacts.source_wal.sha256)"
    source_height="$(manifest_field "$manifest" chain.source_height)"
    source_hash="$(manifest_field "$manifest" chain.source_block_hash)"
    source_state_root="$(manifest_field "$manifest" chain.source_state_root)"
    transition_state_root="$(manifest_field "$manifest" chain.full_state_root)"
    checkpoint_manifest="$(manifest_field "$manifest" chain.approved_checkpoint_manifest_hash)"
    source_round="$(manifest_field "$manifest" chain.source_consensus_round)"
    created_at_unix_ms="$(manifest_field "$manifest" chain.created_at_unix_ms)"
    recovery_epoch="$(manifest_field "$manifest" chain.recovery_epoch)"
    validator_set_id="$(manifest_field "$manifest" chain.validator_set_id)"

    verify_reference_pair \
        "$binary" "$genesis" "$validators" "$legacy_validator_set" \
        "$source_snapshot" "$source_wal" "$source_round" "$created_at_unix_ms" \
        "$recovery_epoch" "$validator_set_id" "$source_height" "$source_hash" \
        "$source_state_root" "$transition_state_root" "$checkpoint_manifest" "$allow_unbound"

    printf 'ARC immutable legacy archive seal plan\n'
    printf '  capture:              %s\n' "$capture_id"
    printf '  rollout manifest:     %s\n' "$manifest_sha"
    printf '  validator public set: %s\n' "$validator_sha"
    printf '  legacy source set:    %s\n' "$legacy_validator_set_sha"
    printf '  paired snapshot/WAL:  %s / %s\n' "$source_snapshot_sha" "$source_wal_sha"
    printf '  selected checkpoint:  H=%s hash=%s source_root=%s transition_root=%s\n' \
        "$source_height" "$source_hash" "$source_state_root" "$transition_state_root"
    printf '  unbound legacy WAL:   %s (explicitly persisted in binding evidence)\n' "$allow_unbound"
    printf '  destination:          %s/%s\n' "$DRIVE_REMOTE" "$manifest_sha"
    local node host
    for node in nyc lax ams lhr nrt sgp; do
        host="$(host_for "$node")"
        ssh "${SSH_OPTIONS[@]}" "$SSH_USER@$host" -- sh -c \
            'test -s "$1/data-dir/state.wal" && test -s "$1/capture.files.sha256" && test -s "$1/capture.complete" && ! pgrep -x arc-node >/dev/null' \
            sh "/root/arc-recovery-captures/$capture_id/$node"
        printf '  capture present/stopped: %s\n' "$node"
    done
    rclone lsd "$DRIVE_REMOTE" >/dev/null
    if [ "$execute" != true ]; then
        printf 'archive fleet: PLAN ONLY; no remote or Drive file was changed\n'
        return 0
    fi
    local expected_go="GO $manifest_sha"
    [ "${ARC_RECOVERY_GO:-}" = "$expected_go" ] || \
        die "execution requires ARC_RECOVERY_GO='$expected_go'"

    install_helpers
    local log_root
    log_root="$(mktemp -d)"
    ARCHIVE_FLEET_TEMP_ROOT="$log_root"
    trap cleanup_temporary_root EXIT
    local pids=() names=()
    for node in nyc lax ams lhr nrt sgp; do
        (
            stage_file "$node" "$manifest_sha" binary "$binary" "$binary_sha"
            stage_file "$node" "$manifest_sha" genesis "$genesis" "$genesis_sha"
            stage_file "$node" "$manifest_sha" validators "$validators" "$validator_sha"
            stage_file "$node" "$manifest_sha" legacy-validators "$legacy_validator_set" "$legacy_validator_set_sha"
            stage_file "$node" "$manifest_sha" checkpoint "$checkpoint" "$checkpoint_sha"
            stage_file "$node" "$manifest_sha" rollout-manifest "$manifest" "$manifest_sha"
        ) > "$log_root/$node-stage.log" 2>&1 &
        pids+=("$!")
        names+=("$node")
    done
    local failed=0 index
    for index in "${!pids[@]}"; do
        if wait "${pids[$index]}"; then
            sed -n '1,40p' "$log_root/${names[$index]}-stage.log"
        else
            printf 'archive fleet: exact verifier-input staging failed: %s\n' "${names[$index]}" >&2
            sed -n '1,120p' "$log_root/${names[$index]}-stage.log" >&2
            failed=1
        fi
    done
    [ "$failed" -eq 0 ] || die "staging failed; capture bytes remain immutable and no bundle was created"

    pids=() names=()
    for node in nyc lax ams lhr nrt sgp; do
        run_remote "$node" bind \
            "$capture_id" "$node" "$manifest_sha" \
            "$binary_sha" "$genesis_sha" "$validator_sha" "$legacy_validator_set_sha" \
            "$source_snapshot_sha" "$source_wal_sha" "$checkpoint_sha" \
            "$source_height" "$source_hash" "$source_state_root" "$transition_state_root" \
            "$checkpoint_manifest" "$source_round" "$created_at_unix_ms" \
            "$recovery_epoch" "$validator_set_id" "$allow_unbound" \
            > "$log_root/$node-bind.log" 2>&1 &
        pids+=("$!")
        names+=("$node")
    done
    failed=0
    for index in "${!pids[@]}"; do
        if wait "${pids[$index]}"; then
            sed -n '1,40p' "$log_root/${names[$index]}-bind.log"
        else
            printf 'archive fleet: snapshot/WAL semantic export failed: %s\n' "${names[$index]}" >&2
            sed -n '1,160p' "$log_root/${names[$index]}-bind.log" >&2
            failed=1
        fi
    done
    [ "$failed" -eq 0 ] || die "at least one capture could not produce immutable classification evidence; no bundle or upload was attempted"

    local status
    : > "$log_root/binding-statuses.jsonl"
    for node in nyc lax ams lhr nrt sgp; do
        status="$(run_remote "$node" binding-status "$manifest_sha" "$node")"
        printf '  binding: %s\n' "$status"
        printf '%s\n' "$status" >> "$log_root/binding-statuses.jsonl"
    done
    local canonical_count fork_count unclassified_count
    read -r canonical_count fork_count unclassified_count < <(
        summarize_binding_statuses < "$log_root/binding-statuses.jsonl"
    )
    [ "$canonical_count" -ge 1 ] || \
        die "none of the six preserved captures matches the selected checkpoint H/hash/root"
    printf 'archive fleet: classification complete canonical=%s forks=%s preserved_unclassified=%s; all six remain labelled and retained\n' \
        "$canonical_count" "$fork_count" "$unclassified_count"

    pids=() names=()
    for node in nyc lax ams lhr nrt sgp; do
        run_remote "$node" bundle "$capture_id" "$node" "$manifest_sha" \
            > "$log_root/$node-bundle.log" 2>&1 &
        pids+=("$!")
        names+=("$node")
    done
    failed=0
    for index in "${!pids[@]}"; do
        if wait "${pids[$index]}"; then
            sed -n '1,40p' "$log_root/${names[$index]}-bundle.log"
        else
            printf 'archive fleet: immutable bundle failed: %s\n' "${names[$index]}" >&2
            sed -n '1,120p' "$log_root/${names[$index]}-bundle.log" >&2
            failed=1
        fi
    done
    [ "$failed" -eq 0 ] || die "at least one fork bundle failed; no Drive upload was attempted"

    local destination="$DRIVE_REMOTE/$manifest_sha"
    local existing_capture
    if existing_capture="$(rclone cat "$destination/capture-id.txt" 2>/dev/null)"; then
        [ "$existing_capture" = "$capture_id" ] || \
            die "Drive destination is already bound to a different freeze capture"
    fi
    rclone mkdir "$destination"
    printf '%s\n' "$capture_id" > "$log_root/capture-id.txt"
    {
        printf '%s  arc-node\n' "$binary_sha"
        printf '%s  genesis.toml\n' "$genesis_sha"
        printf '%s  validator-public-keys.json\n' "$validator_sha"
        printf '%s  legacy-validator-set-40m.json\n' "$legacy_validator_set_sha"
        printf '%s  source.snapshot.lz4\n' "$source_snapshot_sha"
        printf '%s  source.state.wal\n' "$source_wal_sha"
        printf '%s  recovery.arcchkpt\n' "$checkpoint_sha"
        printf '%s  rollout-manifest.json\n' "$manifest_sha"
        printf '%s  rollout-manifest.json.sha256\n' "$(hash_file "$manifest.sha256")"
        printf '%s  capture-id.txt\n' "$(hash_file "$log_root/capture-id.txt")"
    } > "$log_root/SHA256SUMS"
    upload_immutable "$log_root/capture-id.txt" "$destination/capture-id.txt"
    upload_immutable "$binary" "$destination/arc-node"
    upload_immutable "$genesis" "$destination/genesis.toml"
    upload_immutable "$validators" "$destination/validator-public-keys.json"
    upload_immutable "$legacy_validator_set" "$destination/legacy-validator-set-40m.json"
    upload_immutable "$source_snapshot" "$destination/source.snapshot.lz4"
    upload_immutable "$source_wal" "$destination/source.state.wal"
    upload_immutable "$checkpoint" "$destination/recovery.arcchkpt"
    upload_immutable "$manifest" "$destination/rollout-manifest.json"
    upload_immutable "$manifest.sha256" "$destination/rollout-manifest.json.sha256"
    upload_immutable "$log_root/SHA256SUMS" "$destination/SHA256SUMS"

    # The stopped fleet contains roughly 159 GiB. Stream at most three hosts
    # concurrently from SFTP into the operator's Drive remote. Validators
    # receive no Drive configuration or credentials, and each node stays at a
    # single transfer so parallelism is explicit and bounded.
    local upload_order=(nyc lax ams lhr nrt sgp)
    local upload_index source_dir ssh_command suffix filename
    failed=0
    for upload_index in 0 3; do
        pids=()
        names=()
        for node in "${upload_order[@]:upload_index:3}"; do
            (
                host="$(host_for "$node")"
                source_dir=":sftp:/root/arc-recovery-archive/$manifest_sha"
                ssh_command="ssh -o BatchMode=yes -o StrictHostKeyChecking=yes $SSH_USER@$host"
                for suffix in tar.zst tar.zst.sha256 inventory inventory.sha256; do
                    filename="legacy-$node.$suffix"
                    rclone copyto "$source_dir/$filename" "$destination/$filename" \
                        --sftp-ssh "$ssh_command" --immutable --checksum --metadata \
                        --transfers 1 --checkers 1 --retries 5 --low-level-retries 20
                done
                rclone check "$source_dir" "$destination" \
                    --sftp-ssh "$ssh_command" --checksum --one-way --checkers 1 \
                    --include "legacy-$node.tar.zst" \
                    --include "legacy-$node.tar.zst.sha256" \
                    --include "legacy-$node.inventory" \
                    --include "legacy-$node.inventory.sha256"
                printf 'archive fleet: uploaded and hash-checked preserved classified capture %s\n' "$node"
            ) > "$log_root/$node-upload.log" 2>&1 &
            pids+=("$!")
            names+=("$node")
        done
        for index in "${!pids[@]}"; do
            if wait "${pids[$index]}"; then
                sed -n '1,80p' "$log_root/${names[$index]}-upload.log"
            else
                printf 'archive fleet: immutable SFTP-to-Drive upload/check failed: %s\n' \
                    "${names[$index]}" >&2
                sed -n '1,160p' "$log_root/${names[$index]}-upload.log" >&2
                failed=1
            fi
        done
    done
    [ "$failed" -eq 0 ] || \
        die "one or more preserved validator uploads failed; COMPLETE was not emitted"
    printf 'archive fleet: COMPLETE capture=%s manifest=%s canonical=%s forks=%s preserved_unclassified=%s destination=%s\n' \
        "$capture_id" "$manifest_sha" "$canonical_count" "$fork_count" "$unclassified_count" "$destination"
}

COMMAND="${1:-}"
if [ -n "$COMMAND" ]; then
    shift
fi
case "$COMMAND" in
    seal-freeze-plan) seal_freeze_plan "$@" ;;
    capture) capture_phase "$@" ;;
    seal) seal_phase "$@" ;;
    -h|--help|help|'') usage ;;
    *) usage >&2; exit 2 ;;
esac
