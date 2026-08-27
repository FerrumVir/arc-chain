#!/usr/bin/env bash
# Two-phase six-validator freeze, checkpoint binding, and immutable archive.
# Dry-run is the default for both mutating phases.
set -Eeuo pipefail
umask 077

SCRIPT_DIR="$(CDPATH='' cd -- "$(dirname -- "${BASH_SOURCE[0]}")" && pwd)"
REPO_ROOT="$(CDPATH='' cd -- "$SCRIPT_DIR/../.." && pwd)"
ORCHESTRATOR="$SCRIPT_DIR/archive-fleet-to-drive.sh"
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
  ARC_RECOVERY_FREEZE_GO='FREEZE PLAN_SHA256 CAPTURE CAPTURE_SHA256' archive-fleet-to-drive.sh capture \
    --freeze-plan /absolute/freeze.lock.json --execute

  archive-fleet-to-drive.sh seal --freeze-plan /absolute/freeze.lock.json \
    --manifest /absolute/rollout.lock.json \
    --validator-public-keys /absolute/validators.json \
    [--allow-unbound-legacy-wal] [--plan]
  ARC_RECOVERY_GO='GO ROLLOUT_SHA256 FREEZE PLAN_SHA256 CAPTURE CAPTURE_SHA256' archive-fleet-to-drive.sh seal \
    --freeze-plan /absolute/freeze.lock.json \
    --manifest /absolute/rollout.lock.json \
    --validator-public-keys /absolute/validators.json \
    --allow-unbound-legacy-wal --execute

  archive-fleet-to-drive.sh verify-complete --destination 'REMOTE:path/hash'

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
rollout-manifest hash. A canonical archive manifest and its checksum are
uploaded only after every object check passes; immutable COMPLETE.json is the
last object. Consumers must reject a destination without a valid COMPLETE.
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

current_source_commit() {
    local commit
    commit="$(git -C "$REPO_ROOT" rev-parse --verify 'HEAD^{commit}')" || \
        die "cannot resolve the recovery orchestrator source commit"
    printf '%s\n' "$commit" | grep -Eq '^[0-9a-f]{40}([0-9a-f]{24})?$' || \
        die "source commit is not a canonical 40- or 64-character object id"
    printf '%s\n' "$commit"
}

capture_id_for_freeze_plan_hash() {
    local freeze_sha="$1"
    require_hash "$freeze_sha" "freeze plan hash"
    python3 - "$freeze_sha" <<'PY'
import hashlib
import sys

print(hashlib.sha256(b"ARC recovery capture v2\0" + bytes.fromhex(sys.argv[1])).hexdigest())
PY
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
    require_commands python3 git
    require_absolute_file "$ORCHESTRATOR" "archive orchestrator"
    require_absolute_file "$REMOTE_HELPER" "remote archive helper"
    local helper_sha orchestrator_sha source_commit
    helper_sha="$(hash_file "$REMOTE_HELPER")"
    orchestrator_sha="$(hash_file "$ORCHESTRATOR")"
    source_commit="$(current_source_commit)"
    python3 - "$output" "$window" "$helper_sha" "$orchestrator_sha" "$source_commit" "${NODES[@]}" <<'PY'
import datetime
import hashlib
import json
import os
import pathlib
import sys

output = pathlib.Path(sys.argv[1])
window, helper_sha, orchestrator_sha, source_commit = sys.argv[2:6]
nodes = []
for entry in sys.argv[6:]:
    name, host = entry.split("=", 1)
    nodes.append({"name": name, "host": host})
plan = {
    "schema": "arc.recovery.freeze-plan.v2",
    "window": window,
    "created_at": datetime.datetime.now(datetime.timezone.utc).replace(microsecond=0).isoformat().replace("+00:00", "Z"),
    "sentinels": ["nyc", "lax"],
    "nodes": nodes,
    "remote_helper_sha256": helper_sha,
    "orchestrator_sha256": orchestrator_sha,
    "source_commit": source_commit,
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
    local digest capture_id
    digest="$(hash_file "$output")"
    capture_id="$(capture_id_for_freeze_plan_hash "$digest")"
    printf 'archive fleet: sealed freeze plan %s\n' "$output"
    printf 'archive fleet: freeze plan sha256 %s\n' "$digest"
    printf 'archive fleet: capture id %s\n' "$capture_id"
    printf "archive fleet: execution authorization ARC_RECOVERY_FREEZE_GO='FREEZE %s CAPTURE %s'\n" \
        "$digest" "$capture_id"
}

freeze_plan_hash() {
    local plan="$1"
    require_absolute_file "$plan" "freeze plan"
    require_absolute_file "$ORCHESTRATOR" "archive orchestrator"
    require_absolute_file "$REMOTE_HELPER" "remote archive helper"
    local helper_sha orchestrator_sha source_commit
    helper_sha="$(hash_file "$REMOTE_HELPER")"
    orchestrator_sha="$(hash_file "$ORCHESTRATOR")"
    source_commit="$(current_source_commit)"
    python3 - "$plan" "$helper_sha" "$orchestrator_sha" "$source_commit" "${NODES[@]}" <<'PY'
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
helper_sha, orchestrator_sha, source_commit = sys.argv[2:5]
expected_nodes = []
for entry in sys.argv[5:]:
    name, host = entry.split("=", 1)
    expected_nodes.append({"name": name, "host": host})
value = json.loads(path.read_text(encoding="utf-8"))
if set(value) != {
    "schema", "window", "created_at", "sentinels", "nodes",
    "remote_helper_sha256", "orchestrator_sha256", "source_commit",
}:
    raise SystemExit("freeze plan has missing or unknown fields")
if value["schema"] != "arc.recovery.freeze-plan.v2":
    raise SystemExit("unsupported freeze plan schema")
if not isinstance(value["window"], str) or not re.fullmatch(r"[A-Za-z0-9][A-Za-z0-9._:@+-]{0,127}", value["window"]):
    raise SystemExit("freeze plan window is invalid")
if not isinstance(value["created_at"], str) or not re.fullmatch(r"[0-9]{4}-[0-9]{2}-[0-9]{2}T[0-9]{2}:[0-9]{2}:[0-9]{2}Z", value["created_at"]):
    raise SystemExit("freeze plan timestamp is invalid")
if value["sentinels"] != ["nyc", "lax"] or value["nodes"] != expected_nodes:
    raise SystemExit("freeze plan fleet or sentinel order differs from the reviewed six-node topology")
if value["remote_helper_sha256"] != helper_sha:
    raise SystemExit("remote helper bytes differ from the sealed freeze plan")
if value["orchestrator_sha256"] != orchestrator_sha:
    raise SystemExit("orchestrator bytes differ from the sealed freeze plan")
if value["source_commit"] != source_commit:
    raise SystemExit("source commit differs from the sealed freeze plan")
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
REMOTE_HELPER_SHA=""

install_helpers() {
    local expected_sha="$1" node host
    require_hash "$expected_sha" "sealed remote helper hash"
    REMOTE_HELPER_SHA="$(hash_file "$REMOTE_HELPER")"
    [ "$REMOTE_HELPER_SHA" = "$expected_sha" ] || \
        die "remote helper bytes changed after freeze-plan verification"
    REMOTE_HELPER_PATH="/root/.arc-recovery-archive-node-$REMOTE_HELPER_SHA.sh"
    for node in nyc lax ams lhr nrt sgp; do
        host="$(host_for "$node")"
        scp -q "${SSH_OPTIONS[@]}" "$REMOTE_HELPER" "$SSH_USER@$host:$REMOTE_HELPER_PATH"
        ssh "${SSH_OPTIONS[@]}" "$SSH_USER@$host" -- sh -c \
            'chmod 500 -- "$1"; test "$(sha256sum "$1" | cut -d" " -f1)" = "$2"' \
            sh "$REMOTE_HELPER_PATH" "$REMOTE_HELPER_SHA"
    done
}

run_remote() {
    local node="$1"
    shift
    local host
    host="$(host_for "$node")"
    [ -n "$REMOTE_HELPER_PATH" ] && [ -n "$REMOTE_HELPER_SHA" ] || \
        die "remote helper is not installed for this sealed execution"
    ssh "${SSH_OPTIONS[@]}" "$SSH_USER@$host" -- sh -c \
        'helper=$1 expected=$2; shift 2; test -f "$helper" && test ! -L "$helper"; actual=$(sha256sum "$helper" | cut -d" " -f1); test "$actual" = "$expected" || { printf "remote helper hash mismatch\n" >&2; exit 1; }; exec "$helper" "$@"' \
        sh "$REMOTE_HELPER_PATH" "$REMOTE_HELPER_SHA" "$@"
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
    require_commands python3 ssh scp grep git
    [ -x "$REMOTE_HELPER" ] || die "remote helper is missing or not executable"
    local freeze_sha capture_id
    freeze_sha="$(freeze_plan_hash "$freeze_plan")"
    capture_id="$(capture_id_for_freeze_plan_hash "$freeze_sha")"
    printf 'ARC staged legacy freeze plan\n'
    printf '  freeze:   %s\n' "$freeze_sha"
    printf '  capture:  %s\n' "$capture_id"
    printf '  sentinels: %s (quorum is halted after the second clean stop)\n' "${SENTINELS[*]}"
    printf '  remaining: AMS LHR NRT SGP fence/stop after quorum halt; then all-six offline copy\n'
    remote_readiness "$capture_id"
    if [ "$execute" != true ]; then
        printf 'archive fleet: PLAN ONLY; no service or remote/local file was changed\n'
        return 0
    fi
    local expected_go="FREEZE $freeze_sha CAPTURE $capture_id"
    [ "${ARC_RECOVERY_FREEZE_GO:-}" = "$expected_go" ] || \
        die "execution requires ARC_RECOVERY_FREEZE_GO='$expected_go'"

    [ "$(freeze_plan_hash "$freeze_plan")" = "$freeze_sha" ] || \
        die "freeze plan or source bindings changed before execution"
    install_helpers "$(manifest_field "$freeze_plan" remote_helper_sha256)"
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
    local manifest="$1" freeze_plan="$2" freeze_sha="$3" capture_id="$4"
    PYTHONPATH="$SCRIPT_DIR" python3 - "$manifest" "$freeze_plan" "$freeze_sha" "$capture_id" <<'PY'
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
freeze_sha, capture_id = sys.argv[3:5]
captured = sorted((entry["name"], entry["host"]) for entry in freeze["nodes"])
rollout = sorted((entry["name"], entry["host"]) for entry in manifest["validators"])
if captured != rollout:
    rr.fail("rollout validator names/hosts differ from the sealed freeze plan")
if manifest["archive"] != {
    "freeze_plan_sha256": freeze_sha,
    "capture_id": capture_id,
}:
    rr.fail("rollout archive binding differs from the exact freeze plan and capture id")
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

copy_shared_input() {
    local source="$1" expected_sha="$2" destination_root="$3" name="$4"
    require_hash "$expected_sha" "shared input hash"
    [ -f "$source" ] && [ ! -L "$source" ] || \
        die "shared input is missing, non-regular, or a symlink: $source"
    [ ! -e "$destination_root/$name" ] || die "duplicate shared input name: $name"
    cp -- "$source" "$destination_root/$name"
    chmod 400 -- "$destination_root/$name"
    [ "$(hash_file "$destination_root/$name")" = "$expected_sha" ] || \
        die "shared input changed while staging: $source"
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

build_archive_metadata() {
    local shared_root="$1" statuses="$2" metadata_root="$3" complete_root="$4"
    local freeze_sha="$5" capture_id="$6" manifest_sha="$7" source_commit="$8"
    local orchestrator_sha="$9" helper_sha="${10}"
    local canonical_count="${11}" fork_count="${12}" unclassified_count="${13}"
    mkdir -p -- "$metadata_root" "$complete_root"
    python3 - "$shared_root" "$statuses" "$metadata_root/SHA256SUMS" \
        "$metadata_root/ARCHIVE-MANIFEST.json" \
        "$metadata_root/ARCHIVE-MANIFEST.json.sha256" \
        "$complete_root/COMPLETE.json" "$freeze_sha" "$capture_id" \
        "$manifest_sha" "$source_commit" "$orchestrator_sha" "$helper_sha" \
        "$canonical_count" "$fork_count" "$unclassified_count" <<'PY'
import hashlib
import json
import os
import pathlib
import re
import stat
import sys

(shared_root_raw, statuses_raw, sums_raw, manifest_raw, manifest_sidecar_raw,
 complete_raw, freeze_sha, capture_id, rollout_sha, source_commit,
 orchestrator_sha, helper_sha, canonical_raw, fork_raw,
 unclassified_raw) = sys.argv[1:]
shared_root = pathlib.Path(shared_root_raw)
statuses_path = pathlib.Path(statuses_raw)
sums_path = pathlib.Path(sums_raw)
manifest_path = pathlib.Path(manifest_raw)
manifest_sidecar_path = pathlib.Path(manifest_sidecar_raw)
complete_path = pathlib.Path(complete_raw)
hash_re = re.compile(r"^[0-9a-f]{64}$")
commit_re = re.compile(r"^[0-9a-f]{40}(?:[0-9a-f]{24})?$")
nodes = ("nyc", "lax", "ams", "lhr", "nrt", "sgp")
classifications = {
    "valid_canonical", "valid_noncanonical_fork", "preserved_unclassified"
}

for label, value in (
    ("freeze plan", freeze_sha), ("capture id", capture_id),
    ("rollout manifest", rollout_sha), ("orchestrator", orchestrator_sha),
    ("remote helper", helper_sha),
):
    if not hash_re.fullmatch(value):
        raise SystemExit(f"{label} hash is malformed")
if not commit_re.fullmatch(source_commit):
    raise SystemExit("source commit is malformed")

def sha256(path):
    digest = hashlib.sha256()
    with path.open("rb") as handle:
        for chunk in iter(lambda: handle.read(1024 * 1024), b""):
            digest.update(chunk)
    return digest.hexdigest()

def canonical(value):
    return (json.dumps(value, sort_keys=True, separators=(",", ":")) + "\n").encode()

def create(path, payload):
    path.parent.mkdir(parents=True, exist_ok=True)
    fd = os.open(path, os.O_WRONLY | os.O_CREAT | os.O_EXCL, 0o444)
    with os.fdopen(fd, "wb") as handle:
        handle.write(payload)
        handle.flush()
        os.fsync(handle.fileno())

rows = [json.loads(line) for line in statuses_path.read_text(encoding="utf-8").splitlines() if line]
if len(rows) != 6 or {row.get("node") for row in rows} != set(nodes):
    raise SystemExit("bundle status must contain each reviewed validator exactly once")
if len({row.get("node") for row in rows}) != len(rows):
    raise SystemExit("bundle status contains a duplicate validator")

bundle_objects = []
sums = {}
for row in sorted(rows, key=lambda item: nodes.index(item["node"])):
    expected_keys = {
        "schema", "capture_id", "node", "rollout_manifest_sha256",
        "classification", "bundle", "inventory",
    }
    if set(row) != expected_keys or row["schema"] != "arc.recovery.bundle-status.v1":
        raise SystemExit("bundle status has missing, unknown, or unsupported fields")
    if row["capture_id"] != capture_id or row["rollout_manifest_sha256"] != rollout_sha:
        raise SystemExit("bundle status differs from the sealed capture/rollout")
    if row["classification"] not in classifications:
        raise SystemExit("bundle status classification is invalid")
    expected_prefix = f"legacy-{row['node']}"
    normalized = {
        "node": row["node"],
        "classification": row["classification"],
    }
    for label, expected_suffix in (("bundle", ".tar.zst"), ("inventory", ".inventory")):
        item = row[label]
        if set(item) != {"name", "size", "sha256", "sidecar_name", "sidecar_sha256"}:
            raise SystemExit(f"{label} status fields are not exact")
        expected_name = expected_prefix + expected_suffix
        if item["name"] != expected_name or item["sidecar_name"] != expected_name + ".sha256":
            raise SystemExit(f"{label} filename is not canonical")
        if isinstance(item["size"], bool) or not isinstance(item["size"], int) or item["size"] <= 0:
            raise SystemExit(f"{label} size must be positive")
        if not hash_re.fullmatch(item["sha256"]) or not hash_re.fullmatch(item["sidecar_sha256"]):
            raise SystemExit(f"{label} hash is malformed")
        for name, digest in ((item["name"], item["sha256"]), (item["sidecar_name"], item["sidecar_sha256"])):
            if name in sums:
                raise SystemExit("archive object filename is duplicated")
            sums[name] = digest
        normalized[label] = item
    bundle_objects.append(normalized)

shared_inputs = []
for path in sorted(shared_root.iterdir(), key=lambda item: item.name):
    details = path.lstat()
    if stat.S_ISLNK(details.st_mode) or not stat.S_ISREG(details.st_mode):
        raise SystemExit(f"shared archive input is not a regular non-symlink: {path}")
    if not re.fullmatch(r"[A-Za-z0-9][A-Za-z0-9._-]{0,127}", path.name):
        raise SystemExit(f"shared archive input has unsafe name: {path.name}")
    digest = sha256(path)
    if path.name in sums:
        raise SystemExit("shared archive input collides with a bundle object")
    sums[path.name] = digest
    shared_inputs.append({"name": path.name, "size": details.st_size, "sha256": digest})

create(sums_path, "".join(f"{digest}  {name}\n" for name, digest in sorted(sums.items())).encode())
sums_entry = {"name": sums_path.name, "size": sums_path.stat().st_size, "sha256": sha256(sums_path)}
counts = {
    "valid_canonical": int(canonical_raw),
    "valid_noncanonical_fork": int(fork_raw),
    "preserved_unclassified": int(unclassified_raw),
}
observed_counts = {item: sum(row["classification"] == item for row in rows) for item in classifications}
if counts != observed_counts or sum(counts.values()) != 6:
    raise SystemExit("classification counts differ from the six bundle statuses")

archive_manifest = {
    "schema": "arc.recovery.archive-manifest.v1",
    "freeze_plan_sha256": freeze_sha,
    "capture_id": capture_id,
    "rollout_manifest_sha256": rollout_sha,
    "source_commit": source_commit,
    "orchestrator_sha256": orchestrator_sha,
    "remote_helper_sha256": helper_sha,
    "classification_counts": counts,
    "shared_inputs": shared_inputs,
    "validator_bundles": bundle_objects,
    "sha256sums": sums_entry,
}
create(manifest_path, canonical(archive_manifest))
archive_manifest_sha = sha256(manifest_path)
create(manifest_sidecar_path, f"{archive_manifest_sha}  {manifest_path.name}\n".encode())
object_count = len(shared_inputs) + (6 * 4) + 3
complete = {
    "schema": "arc.recovery.archive-complete.v1",
    "freeze_plan_sha256": freeze_sha,
    "capture_id": capture_id,
    "rollout_manifest_sha256": rollout_sha,
    "source_commit": source_commit,
    "archive_manifest_sha256": archive_manifest_sha,
    "object_count_before_complete": object_count,
    "validator_bundle_count": 6,
}
create(complete_path, canonical(complete))
for directory in {sums_path.parent, manifest_path.parent, complete_path.parent}:
    directory_fd = os.open(directory, os.O_RDONLY)
    try:
        os.fsync(directory_fd)
    finally:
        os.close(directory_fd)
print(archive_manifest_sha)
PY
}

verify_remote_complete() (
    local destination="$1" expected_complete="${2:-}" expected_manifest="${3:-}" expected_sidecar="${4:-}"
    local temporary
    temporary="$(mktemp -d)"
    trap 'rm -rf -- "$temporary"' EXIT
    rclone cat "$destination/COMPLETE.json" > "$temporary/COMPLETE.json" || \
        die "archive destination has no readable COMPLETE.json"
    rclone cat "$destination/ARCHIVE-MANIFEST.json" > "$temporary/ARCHIVE-MANIFEST.json" || \
        die "archive destination has no readable archive manifest"
    rclone cat "$destination/ARCHIVE-MANIFEST.json.sha256" > "$temporary/ARCHIVE-MANIFEST.json.sha256" || \
        die "archive destination has no readable archive manifest sidecar"
    if [ -n "$expected_complete" ]; then
        cmp --silent "$expected_complete" "$temporary/COMPLETE.json" || \
            die "existing COMPLETE.json differs from this sealed archive"
        cmp --silent "$expected_manifest" "$temporary/ARCHIVE-MANIFEST.json" || \
            die "remote archive manifest differs from this sealed archive"
        cmp --silent "$expected_sidecar" "$temporary/ARCHIVE-MANIFEST.json.sha256" || \
            die "remote archive manifest sidecar differs from this sealed archive"
    fi
    python3 - "$temporary/COMPLETE.json" "$temporary/ARCHIVE-MANIFEST.json" \
        "$temporary/ARCHIVE-MANIFEST.json.sha256" <<'PY'
import hashlib
import json
import pathlib
import re
import sys

complete_path, manifest_path, sidecar_path = map(pathlib.Path, sys.argv[1:])
complete = json.loads(complete_path.read_text(encoding="utf-8"))
manifest = json.loads(manifest_path.read_text(encoding="utf-8"))
canonical = lambda value: (json.dumps(value, sort_keys=True, separators=(",", ":")) + "\n").encode()
if complete_path.read_bytes() != canonical(complete) or manifest_path.read_bytes() != canonical(manifest):
    raise SystemExit("archive completion evidence is not canonical JSON")
complete_keys = {
    "schema", "freeze_plan_sha256", "capture_id", "rollout_manifest_sha256",
    "source_commit", "archive_manifest_sha256", "object_count_before_complete",
    "validator_bundle_count",
}
if set(complete) != complete_keys or complete["schema"] != "arc.recovery.archive-complete.v1":
    raise SystemExit("COMPLETE.json has missing, unknown, or unsupported fields")
manifest_sha = hashlib.sha256(manifest_path.read_bytes()).hexdigest()
if complete["archive_manifest_sha256"] != manifest_sha:
    raise SystemExit("COMPLETE.json does not bind the archive manifest bytes")
if sidecar_path.read_text(encoding="ascii") != f"{manifest_sha}  ARCHIVE-MANIFEST.json\n":
    raise SystemExit("archive manifest checksum sidecar differs")
if manifest.get("schema") != "arc.recovery.archive-manifest.v1":
    raise SystemExit("archive manifest schema is unsupported")
for field in ("freeze_plan_sha256", "capture_id", "rollout_manifest_sha256", "source_commit"):
    if manifest.get(field) != complete[field]:
        raise SystemExit(f"COMPLETE.json {field} differs from archive manifest")
bundles = manifest.get("validator_bundles")
if not isinstance(bundles, list) or len(bundles) != 6 or len({row.get("node") for row in bundles}) != 6:
    raise SystemExit("archive manifest does not bind six unique validator bundles")
if complete["validator_bundle_count"] != 6:
    raise SystemExit("COMPLETE.json validator bundle count is not six")
expected_count = len(manifest.get("shared_inputs", [])) + 24 + 3
if complete["object_count_before_complete"] != expected_count:
    raise SystemExit("COMPLETE.json object count differs from the archive manifest")
for value in (
    complete["freeze_plan_sha256"], complete["capture_id"],
    complete["rollout_manifest_sha256"], complete["archive_manifest_sha256"],
):
    if not isinstance(value, str) or not re.fullmatch(r"[0-9a-f]{64}", value):
        raise SystemExit("archive completion hash is malformed")
print(manifest_sha)
PY
)

verify_complete_phase() {
    local destination=""
    while [ "$#" -gt 0 ]; do
        case "$1" in
            --destination) [ "$#" -ge 2 ] || die "--destination needs a value"; destination="$2"; shift 2 ;;
            -h|--help) usage; return 0 ;;
            *) die "unknown verify-complete option: $1" ;;
        esac
    done
    [ -n "$destination" ] || die "verify-complete requires --destination"
    require_commands python3 rclone mktemp cmp
    local archive_manifest_sha
    archive_manifest_sha="$(verify_remote_complete "$destination")"
    require_hash "$archive_manifest_sha" "verified archive manifest hash"
    printf 'archive fleet: VERIFIED COMPLETE destination=%s archive_manifest=%s\n' \
        "$destination" "$archive_manifest_sha"
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
    require_commands python3 ssh scp rclone grep mktemp cp find git
    require_absolute_file "$manifest" "rollout manifest"
    require_absolute_file "$validators" "validator public-key file"
    [ -x "$REMOTE_HELPER" ] || die "remote helper is missing or not executable"
    [ -f "$ROLLOUT_TOOL" ] || die "recovery rollout verifier is missing"
    local freeze_sha capture_id verification_output manifest_sha
    freeze_sha="$(freeze_plan_hash "$freeze_plan")"
    capture_id="$(capture_id_for_freeze_plan_hash "$freeze_sha")"
    verification_output="$(verify_rollout_and_capture_topology "$manifest" "$freeze_plan" "$freeze_sha" "$capture_id")"
    printf '%s\n' "$verification_output"
    manifest_sha="$(printf '%s\n' "$verification_output" | tail -n 1)"
    require_hash "$manifest_sha" "rollout manifest hash"
    local validator_sha
    validator_sha="$(hash_file "$validators")"

    local binary genesis checkpoint legacy_validator_set source_snapshot source_wal caddy
    local binary_sha genesis_sha checkpoint_sha legacy_validator_set_sha source_snapshot_sha source_wal_sha caddy_sha
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
    caddy="$(manifest_field "$manifest" artifacts.caddy.path)"
    caddy_sha="$(manifest_field "$manifest" artifacts.caddy.sha256)"
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
    printf '  freeze plan:          %s\n' "$freeze_sha"
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
    local expected_go="GO $manifest_sha FREEZE $freeze_sha CAPTURE $capture_id"
    [ "${ARC_RECOVERY_GO:-}" = "$expected_go" ] || \
        die "execution requires ARC_RECOVERY_GO='$expected_go'"

    [ "$(freeze_plan_hash "$freeze_plan")" = "$freeze_sha" ] || \
        die "freeze plan or source bindings changed before execution"
    install_helpers "$(manifest_field "$freeze_plan" remote_helper_sha256)"
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

    # Re-hash every completed remote bundle in parallel and collect a strict,
    # canonical status record before constructing any top-level archive index.
    pids=() names=()
    for node in nyc lax ams lhr nrt sgp; do
        run_remote "$node" bundle-status "$capture_id" "$node" "$manifest_sha" \
            > "$log_root/$node-bundle-status.json" 2> "$log_root/$node-bundle-status.log" &
        pids+=("$!")
        names+=("$node")
    done
    failed=0
    for index in "${!pids[@]}"; do
        if ! wait "${pids[$index]}"; then
            printf 'archive fleet: verified bundle status failed: %s\n' "${names[$index]}" >&2
            sed -n '1,120p' "$log_root/${names[$index]}-bundle-status.log" >&2
            failed=1
        fi
    done
    [ "$failed" -eq 0 ] || die "one or more bundles could not be re-hashed; no Drive upload was attempted"
    : > "$log_root/bundle-statuses.jsonl"
    for node in nyc lax ams lhr nrt sgp; do
        cat "$log_root/$node-bundle-status.json" >> "$log_root/bundle-statuses.jsonl"
    done

    local shared_root="$log_root/shared-inputs"
    local metadata_root="$log_root/archive-metadata"
    local complete_root="$log_root/archive-complete"
    mkdir -p -- "$shared_root"
    local source_commit orchestrator_sha helper_sha rollout_tool_sha schema_sha
    source_commit="$(current_source_commit)"
    orchestrator_sha="$(hash_file "$ORCHESTRATOR")"
    helper_sha="$(hash_file "$REMOTE_HELPER")"
    rollout_tool_sha="$(hash_file "$ROLLOUT_TOOL")"
    schema_sha="$(hash_file "$SCRIPT_DIR/recovery-manifest.schema.json")"
    copy_shared_input "$freeze_plan" "$freeze_sha" "$shared_root" freeze-plan.json
    copy_shared_input "${freeze_plan}.sha256" "$(hash_file "${freeze_plan}.sha256")" \
        "$shared_root" freeze-plan.json.sha256
    copy_shared_input "$ORCHESTRATOR" "$orchestrator_sha" "$shared_root" archive-fleet-to-drive.sh
    copy_shared_input "$REMOTE_HELPER" "$helper_sha" "$shared_root" archive-node.sh
    copy_shared_input "$ROLLOUT_TOOL" "$rollout_tool_sha" "$shared_root" recovery_rollout.py
    copy_shared_input "$SCRIPT_DIR/recovery-manifest.schema.json" "$schema_sha" \
        "$shared_root" recovery-manifest.schema.json
    copy_shared_input "$binary" "$binary_sha" "$shared_root" arc-node
    copy_shared_input "$genesis" "$genesis_sha" "$shared_root" genesis.toml
    copy_shared_input "$validators" "$validator_sha" "$shared_root" validator-public-keys.json
    copy_shared_input "$legacy_validator_set" "$legacy_validator_set_sha" \
        "$shared_root" legacy-validator-set-40m.json
    copy_shared_input "$source_snapshot" "$source_snapshot_sha" "$shared_root" source.snapshot.lz4
    copy_shared_input "$source_wal" "$source_wal_sha" "$shared_root" source.state.wal
    copy_shared_input "$checkpoint" "$checkpoint_sha" "$shared_root" recovery.arcchkpt
    copy_shared_input "$caddy" "$caddy_sha" "$shared_root" caddy
    copy_shared_input "$manifest" "$manifest_sha" "$shared_root" rollout-manifest.json
    copy_shared_input "${manifest}.sha256" "$(hash_file "${manifest}.sha256")" \
        "$shared_root" rollout-manifest.json.sha256
    printf '%s\n' "$source_commit" > "$shared_root/source-commit.txt"
    printf '%s\n' "$capture_id" > "$shared_root/capture-id.txt"
    python3 - "$shared_root/archive-seal-options.json" "$allow_unbound" <<'PY'
import json
import os
import pathlib
import sys

path = pathlib.Path(sys.argv[1])
payload = (json.dumps(
    {"allow_unbound_legacy_wal": sys.argv[2] == "true"},
    sort_keys=True,
    separators=(",", ":"),
) + "\n").encode()
fd = os.open(path, os.O_WRONLY | os.O_CREAT | os.O_EXCL, 0o400)
with os.fdopen(fd, "wb") as handle:
    handle.write(payload)
    handle.flush()
    os.fsync(handle.fileno())
PY
    chmod 400 -- "$shared_root/source-commit.txt" "$shared_root/capture-id.txt"

    local archive_manifest_sha
    archive_manifest_sha="$(build_archive_metadata \
        "$shared_root" "$log_root/bundle-statuses.jsonl" "$metadata_root" "$complete_root" \
        "$freeze_sha" "$capture_id" "$manifest_sha" "$source_commit" \
        "$orchestrator_sha" "$helper_sha" "$canonical_count" "$fork_count" "$unclassified_count")"
    require_hash "$archive_manifest_sha" "archive manifest hash"

    local destination="$DRIVE_REMOTE/$manifest_sha"
    local existing_capture complete_exists=false
    if existing_capture="$(rclone cat "$destination/capture-id.txt" 2>/dev/null)"; then
        [ "$existing_capture" = "$capture_id" ] || \
            die "Drive destination is already bound to a different freeze capture"
    fi
    if rclone cat "$destination/COMPLETE.json" > "$log_root/existing-COMPLETE.json" 2>/dev/null; then
        complete_exists=true
        verify_remote_complete "$destination" "$complete_root/COMPLETE.json" \
            "$metadata_root/ARCHIVE-MANIFEST.json" \
            "$metadata_root/ARCHIVE-MANIFEST.json.sha256" >/dev/null
        printf 'archive fleet: existing COMPLETE.json exactly matches; verification-only resume\n'
    fi
    rclone mkdir "$destination"
    if [ "$complete_exists" != true ]; then
        rclone copy "$shared_root" "$destination" --immutable --checksum --metadata \
            --retries 5 --low-level-retries 20
    fi

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
                if [ "$complete_exists" != true ]; then
                    for suffix in tar.zst tar.zst.sha256 inventory inventory.sha256; do
                        filename="legacy-$node.$suffix"
                        rclone copyto "$source_dir/$filename" "$destination/$filename" \
                            --sftp-ssh "$ssh_command" --immutable --checksum --metadata \
                            --transfers 1 --checkers 1 --retries 5 --low-level-retries 20
                    done
                fi
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
    rclone check "$shared_root" "$destination" --checksum --one-way --checkers 4
    if [ "$complete_exists" != true ]; then
        rclone copy "$metadata_root" "$destination" --immutable --checksum --metadata \
            --retries 5 --low-level-retries 20
    fi
    rclone check "$metadata_root" "$destination" --checksum --one-way --checkers 4
    if [ "$complete_exists" != true ]; then
        # This is deliberately the final remote mutation. A failed or partial
        # run remains resumable, but no consumer may accept it as complete.
        upload_immutable "$complete_root/COMPLETE.json" "$destination/COMPLETE.json"
    fi
    verify_remote_complete "$destination" "$complete_root/COMPLETE.json" \
        "$metadata_root/ARCHIVE-MANIFEST.json" \
        "$metadata_root/ARCHIVE-MANIFEST.json.sha256" >/dev/null
    printf 'archive fleet: COMPLETE capture=%s rollout=%s archive_manifest=%s canonical=%s forks=%s preserved_unclassified=%s destination=%s\n' \
        "$capture_id" "$manifest_sha" "$archive_manifest_sha" "$canonical_count" \
        "$fork_count" "$unclassified_count" "$destination"
}

COMMAND="${1:-}"
if [ -n "$COMMAND" ]; then
    shift
fi
case "$COMMAND" in
    seal-freeze-plan) seal_freeze_plan "$@" ;;
    capture) capture_phase "$@" ;;
    seal) seal_phase "$@" ;;
    verify-complete) verify_complete_phase "$@" ;;
    -h|--help|help|'') usage ;;
    *) usage >&2; exit 2 ;;
esac
