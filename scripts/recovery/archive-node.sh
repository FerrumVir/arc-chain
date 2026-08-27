#!/usr/bin/env bash
# Remote half of the two-phase ARC legacy freeze/archive protocol.
#
# A freeze capture is content-addressed by a separately sealed freeze plan. It
# is completed before the recovery checkpoint or rollout manifest exists. A
# later, independently authorized seal binds every unchanged capture to the
# exact recovery checkpoint. Fork captures are retained and labelled; they are
# never rewritten to look canonical.
set -Eeuo pipefail
umask 077

CAPTURE_BASE="/root/arc-recovery-captures"
BINDING_BASE="/root/arc-recovery-bindings"
SEAL_BASE="/root/arc-recovery-seal"
ARCHIVE_BASE="/root/arc-recovery-archive"
ARCHIVE_NODE_TEMP_PATH=""

cleanup_temporary_path() {
    if [ -d "$ARCHIVE_NODE_TEMP_PATH" ]; then
        find "$ARCHIVE_NODE_TEMP_PATH" -depth -delete 2>/dev/null || true
    elif [ -e "$ARCHIVE_NODE_TEMP_PATH" ]; then
        rm -f -- "$ARCHIVE_NODE_TEMP_PATH"
    fi
}
trap cleanup_temporary_path EXIT

die() {
    printf 'archive node: %s\n' "$*" >&2
    exit 1
}

usage() {
    cat <<'EOF'
Usage (operator orchestration only):
  archive-node.sh capture-live CAPTURE_SHA256 NODE
  archive-node.sh capture-and-freeze CAPTURE_SHA256 NODE
  archive-node.sh live-status CAPTURE_SHA256 NODE
  archive-node.sh freeze CAPTURE_SHA256 NODE
  archive-node.sh status CAPTURE_SHA256 NODE
  archive-node.sh stage-input MANIFEST_SHA256 NODE ROLE EXPECTED_SHA256 < FILE
  archive-node.sh bind CAPTURE_SHA256 NODE MANIFEST_SHA256 BINARY_SHA256 \
    GENESIS_SHA256 VALIDATORS_SHA256 CHECKPOINT_SHA256 SOURCE_HEIGHT \
    SOURCE_BLOCK_HASH STATE_ROOT CHECKPOINT_MANIFEST_HASH RECOVERY_EPOCH \
    VALIDATOR_SET_ID ALLOW_UNBOUND_LEGACY_WAL
  archive-node.sh bundle CAPTURE_SHA256 NODE MANIFEST_SHA256
  archive-node.sh verify-index CAPTURE_DIRECTORY

capture-live and freeze are intentionally separate: freeze also accepts a node
that is already stopped. All completed trees and bundles are create-only.
EOF
}

require_hash() {
    local value="$1"
    local label="$2"
    printf '%s\n' "$value" | grep -Eq '^[0-9a-f]{64}$' || \
        die "$label must be exactly 64 lowercase hexadecimal characters"
}

require_node() {
    case "$1" in
        nyc|lax|ams|lhr|nrt|sgp) ;;
        *) die "invalid node name: $1" ;;
    esac
}

require_uint() {
    printf '%s\n' "$1" | grep -Eq '^(0|[1-9][0-9]*)$' || die "$2 must be an unsigned integer"
}

normalize_hash() {
    local value="$1"
    value="${value#0x}"
    require_hash "$value" "$2"
    printf '%s\n' "$value"
}

require_commands() {
    local command_name
    for command_name in "$@"; do
        command -v "$command_name" >/dev/null 2>&1 || \
            die "required command is missing: $command_name"
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

# Build an index over every regular file in a tree except the index itself and
# its completion marker. Symlinks and special files are always rejected.
write_tree_index() {
    local root="$1"
    local index_name="$2"
    local complete_name="$3"
    python3 - "$root" "$index_name" "$complete_name" <<'PY'
import hashlib
import os
import pathlib
import stat
import sys

root = pathlib.Path(sys.argv[1])
index_name, complete_name = sys.argv[2:]
index_path = root / index_name
if index_path.exists() or (root / complete_name).exists():
    raise SystemExit("index or completion marker already exists")

rows = []
for base, dirs, files in os.walk(root, followlinks=False):
    dirs.sort()
    files.sort()
    for name in dirs:
        path = pathlib.Path(base) / name
        if path.is_symlink():
            raise SystemExit(f"symlink directory is forbidden: {path}")
    for name in files:
        path = pathlib.Path(base) / name
        rel = path.relative_to(root).as_posix()
        if rel in {index_name, complete_name}:
            continue
        mode = path.lstat().st_mode
        if not stat.S_ISREG(mode):
            raise SystemExit(f"non-regular capture member is forbidden: {rel}")
        digest = hashlib.sha256()
        with path.open("rb") as handle:
            for chunk in iter(lambda: handle.read(1024 * 1024), b""):
                digest.update(chunk)
        rows.append((rel, digest.hexdigest()))

with index_path.open("x", encoding="utf-8", newline="\n") as handle:
    for rel, digest in sorted(rows):
        handle.write(f"{digest}  {rel}\n")
PY
}

verify_tree_index() {
    local root="$1"
    local index_name="$2"
    local complete_name="$3"
    python3 - "$root" "$index_name" "$complete_name" <<'PY'
import hashlib
import os
import pathlib
import re
import stat
import sys

root = pathlib.Path(sys.argv[1])
index_name, complete_name = sys.argv[2:]
if not root.is_dir() or root.is_symlink():
    raise SystemExit("capture root must be a real directory")
index_path = root / index_name
complete_path = root / complete_name
if not index_path.is_file() or index_path.is_symlink():
    raise SystemExit(f"missing immutable index: {index_name}")
if not complete_path.is_file() or complete_path.is_symlink():
    raise SystemExit(f"missing completion marker: {complete_name}")

listed = set()
for line in index_path.read_text(encoding="utf-8").splitlines():
    match = re.fullmatch(r"([0-9a-f]{64})  ([A-Za-z0-9_.@/+:-]+)", line)
    if not match:
        raise SystemExit(f"malformed index line: {line!r}")
    expected, rel = match.groups()
    pure = pathlib.PurePosixPath(rel)
    if pure.is_absolute() or ".." in pure.parts or rel in listed:
        raise SystemExit(f"unsafe or duplicate indexed path: {rel}")
    path = root / pure
    try:
        mode = path.lstat().st_mode
    except FileNotFoundError:
        raise SystemExit(f"indexed file is missing: {rel}")
    if not stat.S_ISREG(mode):
        raise SystemExit(f"indexed member is not a regular file: {rel}")
    digest = hashlib.sha256()
    with path.open("rb") as handle:
        for chunk in iter(lambda: handle.read(1024 * 1024), b""):
            digest.update(chunk)
    if digest.hexdigest() != expected:
        raise SystemExit(f"indexed file changed: {rel}")
    listed.add(rel)

actual = set()
for base, dirs, files in os.walk(root, followlinks=False):
    for name in dirs:
        path = pathlib.Path(base) / name
        if path.is_symlink():
            raise SystemExit(f"symlink directory is forbidden: {path.relative_to(root)}")
    for name in files:
        path = pathlib.Path(base) / name
        rel = path.relative_to(root).as_posix()
        if rel in {index_name, complete_name}:
            continue
        if not stat.S_ISREG(path.lstat().st_mode):
            raise SystemExit(f"non-regular member is forbidden: {rel}")
        actual.add(rel)
if actual != listed:
    missing = sorted(listed - actual)
    unexpected = sorted(actual - listed)
    raise SystemExit(f"index coverage differs; missing={missing} unexpected={unexpected}")

index_digest = hashlib.sha256(index_path.read_bytes()).hexdigest()
marker = complete_path.read_text(encoding="utf-8")
if f"index_sha256={index_digest}\n" not in marker:
    raise SystemExit("completion marker does not bind the index")
PY
}

verify_capture_identity() {
    local root="$1" capture_id="$2" node="$3"
    grep -Fxq "capture_id=$capture_id" "$root/capture.inventory" || \
        die "capture inventory id differs from its immutable path"
    grep -Fxq "node=$node" "$root/capture.inventory" || \
        die "capture inventory node differs from its immutable path"
    grep -Fxq "capture_id=$capture_id" "$root/capture.complete" || \
        die "capture completion id differs from its immutable path"
    grep -Fxq "node=$node" "$root/capture.complete" || \
        die "capture completion node differs from its immutable path"
}

verify_binding_identity() {
    local root="$1" capture_id="$2" node="$3" manifest="$4"
    python3 - "$root/binding.json" "$capture_id" "$node" "$manifest" <<'PY'
import json
import sys
with open(sys.argv[1], "r", encoding="utf-8") as handle:
    value = json.load(handle)
if value.get("capture_id") != sys.argv[2]:
    raise SystemExit("checkpoint binding capture id differs from its immutable path")
if value.get("node") != sys.argv[3]:
    raise SystemExit("checkpoint binding node differs from its immutable path")
if value.get("rollout_manifest_sha256") != sys.argv[4]:
    raise SystemExit("checkpoint binding manifest differs from its immutable path")
PY
}

write_complete_marker() {
    local root="$1"
    local index_name="$2"
    local complete_name="$3"
    local schema="$4"
    shift 4
    local index_sha
    index_sha="$(hash_file "$root/$index_name")"
    {
        printf 'schema=%s\n' "$schema"
        printf 'index_sha256=%s\n' "$index_sha"
        printf '%s\n' "$@"
    } > "$root/$complete_name"
    chmod 400 -- "$root/$complete_name"
}

validate_json_file() {
    python3 - "$1" <<'PY'
import json
import sys
with open(sys.argv[1], "rb") as handle:
    value = json.load(handle)
if not isinstance(value, (dict, list)):
    raise SystemExit("endpoint did not return a JSON object or array")
PY
}

capture_json() {
    local base="$1"
    local endpoint="$2"
    local destination="$3"
    curl --fail --silent --show-error --location --max-time 20 \
        --retry 4 --retry-delay 1 --retry-all-errors \
        "$base$endpoint" --output "$destination"
    validate_json_file "$destination"
}

capture_optional_json() {
    local base="$1"
    local endpoint="$2"
    local destination="$3"
    if capture_json "$base" "$endpoint" "$destination" 2> "$destination.stderr"; then
        rm -f -- "$destination.stderr"
        return 0
    fi
    rm -f -- "$destination"
    {
        printf 'endpoint=%s\n' "$endpoint"
        printf 'status=unavailable-on-legacy-node\n'
    } > "$destination.unavailable"
    return 0
}

snapshot_pair_matches() {
    python3 - "$1" "$2" <<'PY'
import json
import sys

def load(path):
    with open(path, "r", encoding="utf-8") as handle:
        value = json.load(handle)
    if value.get("available") is not True:
        raise SystemExit("snapshot endpoint is not available")
    height = value.get("height")
    root = value.get("state_root")
    if not isinstance(height, int) or height < 0:
        raise SystemExit("snapshot height is invalid")
    if (
        not isinstance(root, str)
        or len(root.removeprefix("0x")) != 64
        or any(c not in "0123456789abcdefABCDEF" for c in root.removeprefix("0x"))
    ):
        raise SystemExit("snapshot state root is invalid")
    return height, root.removeprefix("0x").lower()

before, after = load(sys.argv[1]), load(sys.argv[2])
if before != after:
    raise SystemExit(f"snapshot boundary moved during capture: before={before} after={after}")
PY
}

capture_live() {
    local capture_id="$1"
    local node="$2"
    local defer_index="${3:-false}"
    require_hash "$capture_id" "capture id"
    require_node "$node"
    require_commands curl grep python3 mktemp mv chmod date find
    [ -d /root/arc-chain ] || die "/root/arc-chain is missing"

    local parent="$CAPTURE_BASE/$capture_id"
    local capture_root="$parent/$node"
    mkdir -p -- "$parent"
    [ ! -e "$capture_root" ] || die "capture already exists; refusing replacement: $capture_root"

    local temporary
    temporary="$(mktemp -d "/root/.arc-capture-$capture_id-$node.XXXXXX")"
    ARCHIVE_NODE_TEMP_PATH="$temporary"

    local rpc_base=""
    local port
    for port in 9090 9944; do
        if capture_json "http://127.0.0.1:$port" /health "$temporary/health-probe.json" 2>/dev/null; then
            rpc_base="http://127.0.0.1:$port"
            break
        fi
    done
    [ -n "$rpc_base" ] || die "no healthy loopback RPC on ports 9090 or 9944"

    local attempt attempt_root
    local captured=false
    for attempt in 1 2 3 4 5 6 7 8; do
        attempt_root="$temporary/attempt-$attempt"
        mkdir -- "$attempt_root" "$attempt_root/evidence"
        if capture_json "$rpc_base" /health "$attempt_root/evidence/health.json" &&
            capture_json "$rpc_base" /sync/snapshot/info "$attempt_root/evidence/snapshot-info-before.json" &&
            capture_json "$rpc_base" /block/latest "$attempt_root/evidence/latest-block.json" &&
            capture_json "$rpc_base" /sync/dag_state "$attempt_root/evidence/dag-state.json" &&
            capture_json "$rpc_base" /validators "$attempt_root/evidence/validators.json" &&
            capture_optional_json "$rpc_base" /network/info "$attempt_root/evidence/network-info.json" &&
            curl --fail --silent --show-error --location --max-time 120 \
                --retry 3 --retry-delay 1 --retry-all-errors \
                --dump-header "$attempt_root/evidence/snapshot.headers" \
                "$rpc_base/sync/snapshot" --output "$attempt_root/state.snapshot.lz4" &&
            grep -Eiq '^content-type:[[:space:]]*application/octet-stream([[:space:]]*;|[[:space:]]*$)' \
                "$attempt_root/evidence/snapshot.headers" &&
            [ -s "$attempt_root/state.snapshot.lz4" ] &&
            capture_json "$rpc_base" /sync/snapshot/info "$attempt_root/evidence/snapshot-info-after.json" &&
            snapshot_pair_matches \
                "$attempt_root/evidence/snapshot-info-before.json" \
                "$attempt_root/evidence/snapshot-info-after.json"; then
            captured=true
            break
        fi
        find "$attempt_root" -depth -delete
    done
    [ "$captured" = true ] || die "could not obtain one stable endpoint/snapshot capture after 8 attempts"

    {
        printf 'capture_id=%s\n' "$capture_id"
        printf 'node=%s\n' "$node"
        printf 'captured_at=%s\n' "$(date -u +%Y-%m-%dT%H:%M:%SZ)"
        printf 'rpc_origin=%s\n' "$rpc_base"
        printf 'snapshot_format=lz4-prepend-size-bincode\n'
    } > "$attempt_root/capture.context"

    mv -- "$attempt_root" "$capture_root"
    if [ "$defer_index" = false ]; then
        write_tree_index "$capture_root" live.files.sha256 live.complete
        write_complete_marker "$capture_root" live.files.sha256 live.complete arc.recovery.live-capture.v2 \
            "capture_id=$capture_id" "node=$node" "context_file=capture.context"
        chmod -R go-rwx -- "$capture_root"
        verify_tree_index "$capture_root" live.files.sha256 live.complete
    fi
    find "$temporary" -depth -delete
    ARCHIVE_NODE_TEMP_PATH=""
    printf 'archive node: LIVE CAPTURE BYTES READY capture=%s node=%s indexed=%s\n' \
        "$capture_id" "$node" "$([ "$defer_index" = false ] && printf true || printf false)"
}

stop_node_cleanly() {
    require_commands pgrep pkill systemctl sync sleep
    systemctl stop arc-self-heal.service 2>/dev/null || true
    systemctl stop arc-node.service 2>/dev/null || true
    if pgrep -x arc-node >/dev/null 2>&1; then
        pkill -TERM -x arc-node
    fi
    local shutdown_wait=0
    while pgrep -x arc-node >/dev/null 2>&1 && [ "$shutdown_wait" -lt 120 ]; do
        sleep 0.5
        shutdown_wait=$((shutdown_wait + 1))
    done
    if pgrep -x arc-node >/dev/null 2>&1; then
        die "arc-node did not complete a clean shutdown; refusing SIGKILL and freeze"
    fi
    sync
}

copy_stable_file() {
    local source="$1"
    local destination="$2"
    [ -s "$source" ] || die "required source file is missing or empty: $source"
    [ ! -e "$destination" ] || die "capture destination already exists: $destination"
    local before after source_sha destination_sha
    before="$(stat -c '%s:%Y' "$source")"
    cp --reflink=auto --sparse=always -- "$source" "$destination.partial"
    sync "$destination.partial"
    after="$(stat -c '%s:%Y' "$source")"
    [ "$before" = "$after" ] || die "source changed while it was copied: $source"
    source_sha="$(hash_file "$source")"
    destination_sha="$(hash_file "$destination.partial")"
    [ "$source_sha" = "$destination_sha" ] || die "copied file hash differs: $source"
    mv -- "$destination.partial" "$destination"
}

freeze_capture() {
    local capture_id="$1"
    local node="$2"
    require_hash "$capture_id" "capture id"
    require_node "$node"
    require_commands python3 grep stat cp mv sync date hostname uname find du
    local capture_root="$CAPTURE_BASE/$capture_id/$node"
    [ -d "$capture_root" ] || die "live capture is missing: $capture_root"
    [ ! -e "$capture_root/capture.complete" ] || {
        verify_tree_index "$capture_root" capture.files.sha256 capture.complete
        verify_capture_identity "$capture_root" "$capture_id" "$node"
        printf 'archive node: existing frozen capture verified capture=%s node=%s\n' "$capture_id" "$node"
        return 0
    }
    [ -s "$capture_root/state.snapshot.lz4" ] && \
        [ -s "$capture_root/evidence/snapshot-info-before.json" ] && \
        [ -s "$capture_root/evidence/snapshot-info-after.json" ] || die "live capture is incomplete"

    # Stop immediately after the snapshot command returns, before hashing a
    # potentially large capture. This minimizes the snapshot-to-final-WAL gap
    # on the two sentinels. The full immutable index is verified after stop and
    # before any final WAL byte is accepted. This accepts an already-stopped process
    # after an operator-initiated clean stop.
    stop_node_cleanly
    if [ ! -e "$capture_root/live.files.sha256" ] && [ ! -e "$capture_root/live.complete" ]; then
        write_tree_index "$capture_root" live.files.sha256 live.complete
        write_complete_marker "$capture_root" live.files.sha256 live.complete arc.recovery.live-capture.v2 \
            "capture_id=$capture_id" "node=$node" "context_file=capture.context"
        chmod -R go-rwx -- "$capture_root"
    fi
    verify_tree_index "$capture_root" live.files.sha256 live.complete
    copy_stable_file /root/arc-chain/arc-data/state.wal "$capture_root/state.wal"

    mkdir -- "$capture_root/legacy-public"
    local source destination
    for source in \
        /root/arc-chain/genesis.toml \
        /root/arc-chain/deploy/config/genesis.toml \
        /root/arc-chain/testnet-seeds.txt \
        /root/arc-chain/target/release/arc-node; do
        [ -f "$source" ] || continue
        case "$source" in
            */deploy/config/genesis.toml) destination="$capture_root/legacy-public/deploy-genesis.toml" ;;
            */target/release/arc-node) destination="$capture_root/legacy-public/arc-node" ;;
            *) destination="$capture_root/legacy-public/${source##*/}" ;;
        esac
        copy_stable_file "$source" "$destination"
    done

    {
        printf 'capture_id=%s\n' "$capture_id"
        printf 'node=%s\n' "$node"
        printf 'hostname=%s\n' "$(hostname)"
        printf 'frozen_at=%s\n' "$(date -u +%Y-%m-%dT%H:%M:%SZ)"
        printf 'kernel=%s\n' "$(uname -srmo)"
        printf 'archive_scope=public-chain-recovery-bundle-v2\n'
        printf 'state_wal_bytes=%s\n' "$(stat -c %s "$capture_root/state.wal")"
        printf 'state_wal_sha256=%s\n' "$(hash_file "$capture_root/state.wal")"
        printf 'snapshot_sha256=%s\n' "$(hash_file "$capture_root/state.snapshot.lz4")"
        if [ -d /root/arc-chain/arc-data/dag-wal ]; then
            printf 'legacy_dag_wal_bytes_retained_on_node=%s\n' \
                "$(du -s -B1 /root/arc-chain/arc-data/dag-wal | cut -f1)"
            printf 'legacy_dag_wal_segments_retained_on_node=%s\n' \
                "$(find /root/arc-chain/arc-data/dag-wal -maxdepth 1 -type f -name 'wal-*.bin' | wc -l | tr -d ' ')"
        fi
        printf 'excluded_private_material=true\n'
        printf 'excluded_service_environments=true\n'
        printf 'excluded_build_models_git_and_dag_trace=true\n'
    } > "$capture_root/capture.inventory"

    write_tree_index "$capture_root" capture.files.sha256 capture.complete
    write_complete_marker "$capture_root" capture.files.sha256 capture.complete arc.recovery.capture.v2 \
        "capture_id=$capture_id" "node=$node" "stopped=true"
    chmod -R a-w,go-rwx -- "$capture_root"
    verify_tree_index "$capture_root" capture.files.sha256 capture.complete
    printf 'archive node: FROZEN CAPTURE COMPLETE capture=%s node=%s\n' "$capture_id" "$node"
}

capture_status() {
    local capture_id="$1"
    local node="$2"
    require_hash "$capture_id" "capture id"
    require_node "$node"
    require_commands pgrep python3
    local capture_root="$CAPTURE_BASE/$capture_id/$node"
    verify_tree_index "$capture_root" capture.files.sha256 capture.complete
    verify_capture_identity "$capture_root" "$capture_id" "$node"
    if pgrep -x arc-node >/dev/null 2>&1; then
        die "capture is complete but arc-node is running"
    fi
    printf '{"capture_id":"%s","node":"%s","capture_complete":true,"stopped":true}\n' \
        "$capture_id" "$node"
}

live_status() {
    local capture_id="$1"
    local node="$2"
    require_hash "$capture_id" "capture id"
    require_node "$node"
    local capture_root="$CAPTURE_BASE/$capture_id/$node"
    if [ -e "$capture_root/capture.complete" ]; then
        verify_tree_index "$capture_root" capture.files.sha256 capture.complete
        printf '{"capture_id":"%s","node":"%s","live_complete":true,"frozen":true}\n' \
            "$capture_id" "$node"
        return 0
    fi
    verify_tree_index "$capture_root" live.files.sha256 live.complete
    printf '{"capture_id":"%s","node":"%s","live_complete":true,"frozen":false}\n' \
        "$capture_id" "$node"
}

binding_status() {
    local manifest="$1"
    local node="$2"
    require_hash "$manifest" "manifest hash"
    require_node "$node"
    require_commands python3
    local binding_root="$BINDING_BASE/$manifest/$node"
    verify_tree_index "$binding_root" binding.files.sha256 binding.complete
    python3 - "$binding_root/binding.json" <<'PY'
import json
import sys
with open(sys.argv[1], "r", encoding="utf-8") as handle:
    value = json.load(handle)
print(json.dumps({
    "node": value["node"],
    "canonical_match": value["canonical_match"],
    "source_height": value["exported"]["source_height"],
    "source_block_hash": value["exported"]["source_block_hash"],
    "full_state_root": value["exported"]["full_state_root"],
}, sort_keys=True, separators=(",", ":")))
PY
}

stage_input() {
    local manifest="$1"
    local node="$2"
    local role="$3"
    local expected_sha="$4"
    require_hash "$manifest" "manifest hash"
    require_node "$node"
    require_hash "$expected_sha" "expected input hash"
    require_commands python3 mktemp mv chmod
    local filename mode
    case "$role" in
        binary) filename=arc-node; mode=500 ;;
        genesis) filename=genesis.toml; mode=400 ;;
        validators) filename=validator-public-keys.json; mode=400 ;;
        checkpoint) filename=recovery.arcchkpt; mode=400 ;;
        rollout-manifest) filename=rollout-manifest.json; mode=400 ;;
        *) die "invalid staged input role: $role" ;;
    esac
    local stage_root="$SEAL_BASE/$manifest/$node"
    mkdir -p -- "$stage_root"
    local temporary
    temporary="$(mktemp "$stage_root/.${filename}.upload.XXXXXX")"
    ARCHIVE_NODE_TEMP_PATH="$temporary"
    cat > "$temporary"
    [ "$(hash_file "$temporary")" = "$expected_sha" ] || die "staged $role input hash mismatch"
    if [ -e "$stage_root/$filename" ]; then
        [ -f "$stage_root/$filename" ] && [ ! -L "$stage_root/$filename" ] || \
            die "existing staged $role input is not a regular file"
        [ "$(hash_file "$stage_root/$filename")" = "$expected_sha" ] || \
            die "existing staged $role input differs; refusing replacement"
        rm -f -- "$temporary"
        ARCHIVE_NODE_TEMP_PATH=""
        printf 'archive node: existing staged input verified role=%s node=%s\n' "$role" "$node"
        return 0
    fi
    chmod "$mode" -- "$temporary"
    mv -- "$temporary" "$stage_root/$filename"
    ARCHIVE_NODE_TEMP_PATH=""
    printf 'archive node: staged input role=%s node=%s sha256=%s\n' "$role" "$node" "$expected_sha"
}

parse_source_round() {
    python3 - "$1" <<'PY'
import json
import sys
with open(sys.argv[1], "r", encoding="utf-8") as handle:
    value = json.load(handle)
round_value = value.get("current_round")
if not isinstance(round_value, int) or round_value < 0:
    raise SystemExit("captured current_round is not an unsigned integer")
print(round_value)
PY
}

bind_capture() {
    local capture_id="$1" node="$2" manifest="$3"
    local binary_sha="$4" genesis_sha="$5" validators_sha="$6" checkpoint_sha="$7"
    local source_height="$8" source_hash="$9" state_root="${10}" checkpoint_manifest="${11}"
    local recovery_epoch="${12}" validator_set_id="${13}" allow_unbound="${14}"
    require_hash "$capture_id" "capture id"
    require_node "$node"
    require_hash "$manifest" "manifest hash"
    require_hash "$binary_sha" "binary hash"
    require_hash "$genesis_sha" "genesis hash"
    require_hash "$validators_sha" "validator public-key hash"
    require_hash "$checkpoint_sha" "checkpoint hash"
    require_uint "$source_height" "source height"
    source_hash="$(normalize_hash "$source_hash" "source block hash")"
    state_root="$(normalize_hash "$state_root" "state root")"
    checkpoint_manifest="$(normalize_hash "$checkpoint_manifest" "checkpoint manifest hash")"
    require_uint "$recovery_epoch" "recovery epoch"
    require_uint "$validator_set_id" "validator-set id"
    case "$allow_unbound" in true|false) ;; *) die "allow-unbound flag must be true or false" ;; esac
    require_commands python3 mktemp mv chmod

    local capture_root="$CAPTURE_BASE/$capture_id/$node"
    local stage_root="$SEAL_BASE/$manifest/$node"
    local binding_parent="$BINDING_BASE/$manifest"
    local binding_root="$binding_parent/$node"
    verify_tree_index "$capture_root" capture.files.sha256 capture.complete
    verify_capture_identity "$capture_root" "$capture_id" "$node"
    mkdir -p -- "$binding_parent"
    if [ -e "$binding_root" ]; then
        verify_tree_index "$binding_root" binding.files.sha256 binding.complete
        verify_binding_identity "$binding_root" "$capture_id" "$node" "$manifest"
        printf 'archive node: existing checkpoint binding verified manifest=%s node=%s\n' "$manifest" "$node"
        return 0
    fi
    local role filename expected
    for role in \
        "arc-node:$binary_sha" \
        "genesis.toml:$genesis_sha" \
        "validator-public-keys.json:$validators_sha" \
        "recovery.arcchkpt:$checkpoint_sha" \
        "rollout-manifest.json:$manifest"; do
        filename="${role%%:*}"
        expected="${role#*:}"
        [ -f "$stage_root/$filename" ] && [ ! -L "$stage_root/$filename" ] || \
            die "staged input is missing: $filename"
        [ "$(hash_file "$stage_root/$filename")" = "$expected" ] || \
            die "staged input changed: $filename"
    done

    local temporary
    temporary="$(mktemp -d "$binding_parent/.${node}.binding.XXXXXX")"
    ARCHIVE_NODE_TEMP_PATH="$temporary"
    local source_round
    source_round="$(parse_source_round "$capture_root/evidence/dag-state.json")"
    "$stage_root/arc-node" recovery inspect \
        --checkpoint "$stage_root/recovery.arcchkpt" \
        > "$temporary/final-checkpoint.inspect.json" 2> "$temporary/final-checkpoint.inspect.stderr"

    local export_command=(
        "$stage_root/arc-node" recovery export
        --data-dir "$capture_root"
        --snapshot "$capture_root/state.snapshot.lz4"
        --genesis "$stage_root/genesis.toml"
        --validator-public-keys "$stage_root/validator-public-keys.json"
        --output "$temporary/candidate.arcchkpt"
        --source-consensus-round "$source_round"
        --recovery-epoch "$recovery_epoch"
        --validator-set-id "$validator_set_id"
    )
    if [ "$allow_unbound" = true ]; then
        export_command+=(--allow-unbound-legacy-wal)
    fi
    "${export_command[@]}" > "$temporary/export-summary.json" 2> "$temporary/export.stderr"

    python3 - \
        "$temporary/final-checkpoint.inspect.json" "$temporary/export-summary.json" \
        "$temporary/binding.json" "$capture_id" "$node" "$manifest" "$source_round" \
        "$source_height" "$source_hash" "$state_root" "$checkpoint_manifest" \
        "$recovery_epoch" "$validator_set_id" "$allow_unbound" <<'PY'
import json
import sys

(inspect_path, export_path, output_path, capture_id, node, manifest, source_round,
 source_height, source_hash, state_root, checkpoint_manifest, recovery_epoch,
 validator_set_id, allow_unbound) = sys.argv[1:]

with open(inspect_path, "r", encoding="utf-8") as handle:
    final = json.load(handle)
with open(export_path, "r", encoding="utf-8") as handle:
    exported = json.load(handle)

def bare(value, field):
    if not isinstance(value, str):
        raise SystemExit(f"{field} is not a hash")
    value = value.removeprefix("0x")
    if len(value) != 64 or any(c not in "0123456789abcdef" for c in value):
        raise SystemExit(f"{field} is not a lowercase 32-byte hash")
    return value

expected = {
    "source_height": int(source_height),
    "source_block_hash": source_hash,
    "full_state_root": state_root,
    "manifest_hash": checkpoint_manifest,
    "recovery_epoch": int(recovery_epoch),
    "validator_set_id": int(validator_set_id),
}
if final.get("source_height") != expected["source_height"]:
    raise SystemExit("sealed checkpoint source height differs from rollout manifest")
if final.get("status") != "UNTRUSTED_INSPECTION":
    raise SystemExit("sealed checkpoint inspection returned an unexpected status")
for key in ("source_block_hash", "full_state_root", "manifest_hash"):
    if bare(final.get(key), f"final checkpoint {key}") != expected[key]:
        raise SystemExit(f"sealed checkpoint {key} differs from rollout manifest")
for key in ("recovery_epoch", "validator_set_id"):
    if final.get(key) != expected[key]:
        raise SystemExit(f"sealed checkpoint {key} differs from rollout manifest")

actual = {
    "source_height": exported.get("source_height"),
    "source_block_hash": bare(exported.get("source_block_hash"), "export source_block_hash"),
    "full_state_root": bare(exported.get("full_state_root"), "export full_state_root"),
    "recovery_epoch": exported.get("recovery_epoch"),
    "validator_set_id": exported.get("validator_set_id"),
    "manifest_hash": bare(exported.get("manifest_hash"), "export manifest_hash"),
    "payload_hash": bare(exported.get("payload_hash"), "export payload_hash"),
}
if exported.get("status") != "EXPORTED_UNSIGNED":
    raise SystemExit("snapshot-assisted recovery export returned an unexpected status")
canonical_match = all(actual[key] == expected[key] for key in (
    "source_height", "source_block_hash", "full_state_root",
    "recovery_epoch", "validator_set_id",
))
binding = {
    "schema": "arc.recovery.capture-binding.v2",
    "capture_id": capture_id,
    "node": node,
    "rollout_manifest_sha256": manifest,
    "source_consensus_round": int(source_round),
    "allow_unbound_legacy_wal": allow_unbound == "true",
    "canonical_match": canonical_match,
    "expected": expected,
    "exported": actual,
}
with open(output_path, "x", encoding="utf-8", newline="\n") as handle:
    json.dump(binding, handle, sort_keys=True, separators=(",", ":"))
    handle.write("\n")
PY

    write_tree_index "$temporary" binding.files.sha256 binding.complete
    write_complete_marker "$temporary" binding.files.sha256 binding.complete arc.recovery.capture-binding.v2 \
        "capture_id=$capture_id" "node=$node" "rollout_manifest_sha256=$manifest"
    chmod -R a-w,go-rwx -- "$temporary"
    mv -- "$temporary" "$binding_root"
    ARCHIVE_NODE_TEMP_PATH=""
    verify_tree_index "$binding_root" binding.files.sha256 binding.complete
    verify_binding_identity "$binding_root" "$capture_id" "$node" "$manifest"
    local canonical
    canonical="$(python3 -c 'import json,sys; print(str(json.load(open(sys.argv[1]))["canonical_match"]).lower())' "$binding_root/binding.json")"
    printf 'archive node: BINDING COMPLETE manifest=%s node=%s canonical_match=%s\n' \
        "$manifest" "$node" "$canonical"
}

bundle_capture() {
    local capture_id="$1" node="$2" manifest="$3"
    require_hash "$capture_id" "capture id"
    require_node "$node"
    require_hash "$manifest" "manifest hash"
    require_commands python3 tar zstd stat mv sync chmod
    local capture_root="$CAPTURE_BASE/$capture_id/$node"
    local binding_root="$BINDING_BASE/$manifest/$node"
    verify_tree_index "$capture_root" capture.files.sha256 capture.complete
    verify_capture_identity "$capture_root" "$capture_id" "$node"
    verify_tree_index "$binding_root" binding.files.sha256 binding.complete
    verify_binding_identity "$binding_root" "$capture_id" "$node" "$manifest"

    local archive_root="$ARCHIVE_BASE/$manifest"
    local archive="$archive_root/legacy-$node.tar.zst"
    local checksum="$archive.sha256"
    local inventory="$archive_root/legacy-$node.inventory"
    local inventory_checksum="$inventory.sha256"
    mkdir -p -- "$ARCHIVE_BASE" "$archive_root"
    chmod 700 -- "$ARCHIVE_BASE" "$archive_root"
    if [ -s "$archive" ] && [ -s "$checksum" ] && [ -s "$inventory" ] && [ -s "$inventory_checksum" ]; then
        [ "$(hash_file "$archive")" = "$(cut -d' ' -f1 "$checksum")" ] || \
            die "existing archive checksum failed; refusing replacement"
        [ "$(hash_file "$inventory")" = "$(cut -d' ' -f1 "$inventory_checksum")" ] || \
            die "existing inventory checksum failed; refusing replacement"
        grep -Fxq "manifest_sha256=$manifest" "$inventory" || \
            die "existing archive belongs to a different rollout manifest"
        grep -Fxq "capture_id=$capture_id" "$inventory" || \
            die "existing archive belongs to a different freeze capture"
        grep -Fxq "node=$node" "$inventory" || \
            die "existing archive belongs to a different validator"
        printf 'archive node: existing verified archive node=%s bytes=%s sha256=%s\n' \
            "$node" "$(stat -c %s "$archive")" "$(hash_file "$archive")"
        return 0
    fi
    if [ -e "$archive" ] || [ -e "$checksum" ] || [ -e "$inventory" ] || [ -e "$inventory_checksum" ]; then
        die "partial archive or evidence exists; refusing replacement"
    fi

    local canonical
    canonical="$(python3 -c 'import json,sys; print(str(json.load(open(sys.argv[1]))["canonical_match"]).lower())' "$binding_root/binding.json")"
    {
        printf 'manifest_sha256=%s\n' "$manifest"
        printf 'capture_id=%s\n' "$capture_id"
        printf 'node=%s\n' "$node"
        printf 'canonical_match=%s\n' "$canonical"
        printf 'archive_scope=public-chain-recovery-bundle-v2\n'
        printf 'excluded_private_material=true\n'
        printf 'excluded_service_environments=true\n'
        printf 'excluded_build_models_git_and_dag_trace=true\n'
        printf 'capture_index_sha256=%s\n' "$(hash_file "$capture_root/capture.files.sha256")"
        printf 'binding_index_sha256=%s\n' "$(hash_file "$binding_root/binding.files.sha256")"
    } > "$inventory"

    local temporary="$archive.partial"
    ARCHIVE_NODE_TEMP_PATH="$temporary"
    tar --create --zstd --numeric-owner --acls --xattrs --sparse --one-file-system \
        --file "$temporary" --directory /root \
        "arc-recovery-captures/$capture_id/$node" \
        "arc-recovery-bindings/$manifest/$node"
    sync "$temporary"
    mv -- "$temporary" "$archive"
    ARCHIVE_NODE_TEMP_PATH=""
    printf '%s  %s\n' "$(hash_file "$archive")" "${archive##*/}" > "$checksum"
    printf '%s  %s\n' "$(hash_file "$inventory")" "${inventory##*/}" > "$inventory_checksum"
    chmod 400 -- "$archive" "$checksum" "$inventory" "$inventory_checksum"
    printf 'archive node: BUNDLE COMPLETE node=%s canonical_match=%s bytes=%s sha256=%s\n' \
        "$node" "$canonical" "$(stat -c %s "$archive")" "$(hash_file "$archive")"
}

ACTION="${1:-}"
case "$ACTION" in
    capture-live)
        [ "$#" -eq 3 ] || { usage >&2; exit 2; }
        capture_live "$2" "$3"
        ;;
    capture-and-freeze)
        [ "$#" -eq 3 ] || { usage >&2; exit 2; }
        capture_live "$2" "$3" true
        freeze_capture "$2" "$3"
        ;;
    live-status)
        [ "$#" -eq 3 ] || { usage >&2; exit 2; }
        live_status "$2" "$3"
        ;;
    freeze)
        [ "$#" -eq 3 ] || { usage >&2; exit 2; }
        freeze_capture "$2" "$3"
        ;;
    status)
        [ "$#" -eq 3 ] || { usage >&2; exit 2; }
        capture_status "$2" "$3"
        ;;
    stage-input)
        [ "$#" -eq 5 ] || { usage >&2; exit 2; }
        stage_input "$2" "$3" "$4" "$5"
        ;;
    bind)
        [ "$#" -eq 15 ] || { usage >&2; exit 2; }
        bind_capture "$2" "$3" "$4" "$5" "$6" "$7" "$8" "$9" \
            "${10}" "${11}" "${12}" "${13}" "${14}" "${15}"
        ;;
    bundle)
        [ "$#" -eq 4 ] || { usage >&2; exit 2; }
        bundle_capture "$2" "$3" "$4"
        ;;
    binding-status)
        [ "$#" -eq 3 ] || { usage >&2; exit 2; }
        binding_status "$2" "$3"
        ;;
    verify-index)
        [ "$#" -eq 2 ] || { usage >&2; exit 2; }
        require_commands python3
        verify_tree_index "$2" capture.files.sha256 capture.complete
        printf 'archive node: capture index verified: %s\n' "$2"
        ;;
    -h|--help|help|'') usage ;;
    *) usage >&2; exit 2 ;;
esac
