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
STOP_BASE="/root/arc-recovery-stops"
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
  archive-node.sh fence-stop CAPTURE_SHA256 NODE
  archive-node.sh stopped-status CAPTURE_SHA256 NODE
  archive-node.sh capture-offline CAPTURE_SHA256 NODE
  archive-node.sh status CAPTURE_SHA256 NODE
  archive-node.sh stage-input MANIFEST_SHA256 NODE ROLE EXPECTED_SHA256 < FILE
  archive-node.sh bind CAPTURE_SHA256 NODE MANIFEST_SHA256 BINARY_SHA256 \
    GENESIS_SHA256 VALIDATORS_SHA256 LEGACY_VALIDATORS_SHA256 SOURCE_SNAPSHOT_SHA256 \
    SOURCE_WAL_SHA256 CHECKPOINT_SHA256 SOURCE_HEIGHT SOURCE_BLOCK_HASH \
    SOURCE_STATE_ROOT TRANSITION_STATE_ROOT CHECKPOINT_MANIFEST_HASH \
    SOURCE_CONSENSUS_ROUND CREATED_AT_UNIX_MS \
    RECOVERY_EPOCH VALIDATOR_SET_ID ALLOW_UNBOUND_LEGACY_WAL
  archive-node.sh bundle CAPTURE_SHA256 NODE MANIFEST_SHA256
  archive-node.sh bundle-status CAPTURE_SHA256 NODE MANIFEST_SHA256
  archive-node.sh verify-index CAPTURE_DIRECTORY

fence-stop must complete on enough validators to halt quorum before
capture-offline copies any chain byte. All completed trees and bundles are
create-only.
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

# Slice the immutable WAL only at the exact checkpoint-prefix boundary emitted
# by the read-only recovery exporter. This helper is intentionally testable in
# isolation: it never guesses the boundary from file hashes or process exit.
write_offline_wal_evidence() {
    python3 - "$1" "$2" "$3" "$4" <<'PY'
import hashlib
import json
import pathlib
import sys

original = pathlib.Path(sys.argv[1])
summary_path = pathlib.Path(sys.argv[2])
output = pathlib.Path(sys.argv[3])
evidence = pathlib.Path(sys.argv[4])

def digest(path):
    value = hashlib.sha256()
    with path.open("rb") as handle:
        for chunk in iter(lambda: handle.read(1024 * 1024), b""):
            value.update(chunk)
    return value.hexdigest()

original_sha = digest(original)
original_size = original.stat().st_size
summary = json.loads(summary_path.read_text(encoding="utf-8"))
required = (
    "source_wal_original_bytes",
    "source_wal_accepted_prefix_bytes",
    "source_wal_quarantined_tail_bytes",
    "source_wal_tail_reason",
)
if any(field not in summary for field in required):
    raise SystemExit("recovery exporter omitted exact accepted-prefix/tail metadata")
reported_original = summary["source_wal_original_bytes"]
accepted = summary["source_wal_accepted_prefix_bytes"]
tail_bytes = summary["source_wal_quarantined_tail_bytes"]
tail_reason = summary["source_wal_tail_reason"]
if not all(isinstance(value, int) and not isinstance(value, bool) for value in (
    reported_original, accepted, tail_bytes
)):
    raise SystemExit("recovery exporter WAL byte fields are not integers")
if reported_original != original_size or not 0 <= accepted <= original_size:
    raise SystemExit("recovery exporter WAL boundary does not fit the immutable capture")
if tail_bytes != original_size - accepted:
    raise SystemExit("recovery exporter tail bytes do not complement the accepted prefix")
if tail_bytes > 0 and (not isinstance(tail_reason, str) or not tail_reason.strip()):
    raise SystemExit("recovery exporter ignored a WAL tail without an exact reason")
if tail_bytes == 0 and tail_reason not in (None, "", "none"):
    raise SystemExit("recovery exporter reported a tail reason when every WAL byte was accepted")

prefix_sha = None
tail_sha = None
reconstructed_sha = original_sha
if tail_bytes > 0:
    prefix = evidence / "recovered-state.wal"
    tail = evidence / "quarantined-wal-tail.bin"
    remaining = accepted
    with original.open("rb") as source, prefix.open("xb") as prefix_handle:
        while remaining:
            chunk = source.read(min(1024 * 1024, remaining))
            if not chunk:
                raise SystemExit("immutable WAL ended before accepted prefix boundary")
            prefix_handle.write(chunk)
            remaining -= len(chunk)
        with tail.open("xb") as tail_handle:
            while True:
                chunk = source.read(1024 * 1024)
                if not chunk:
                    break
                tail_handle.write(chunk)
    if prefix.stat().st_size != accepted or tail.stat().st_size != tail_bytes:
        raise SystemExit("sliced prefix/tail sizes do not match exporter metadata")
    prefix_sha = digest(prefix)
    tail_sha = digest(tail)
    reconstructed = hashlib.sha256()
    for path in (prefix, tail):
        with path.open("rb") as handle:
            for chunk in iter(lambda: handle.read(1024 * 1024), b""):
                reconstructed.update(chunk)
    reconstructed_sha = reconstructed.hexdigest()
    if reconstructed_sha != original_sha:
        raise SystemExit("accepted prefix plus quarantined tail does not reconstruct immutable WAL")

value = {
    "schema": "arc.recovery.offline-wal-recovery.v2",
    "capture_wal_sha256": original_sha,
    "capture_wal_bytes": original_size,
    "accepted_prefix_bytes": accepted,
    "accepted_prefix_sha256": prefix_sha,
    "quarantined_tail_bytes": tail_bytes,
    "quarantined_tail_sha256": tail_sha,
    "tail_reason": tail_reason,
    "prefix_plus_tail_sha256": reconstructed_sha,
    "prefix_plus_tail_reconstructs_capture": reconstructed_sha == original_sha,
}
output.write_text(json.dumps(value, sort_keys=True, separators=(",", ":")) + "\n", encoding="utf-8")
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

capture_pre_stop_evidence() {
    local evidence="$1"
    local rpc_base="" port
    for port in 9090 9944; do
        if capture_json "http://127.0.0.1:$port" /health "$evidence/health.json" 2>/dev/null; then
            rpc_base="http://127.0.0.1:$port"
            break
        fi
    done
    if [ -z "$rpc_base" ]; then
        printf 'status=no-healthy-loopback-rpc\n' > "$evidence/rpc.unavailable"
        return 0
    fi
    printf '%s\n' "$rpc_base" > "$evidence/rpc-origin.txt"
    capture_optional_json "$rpc_base" /block/latest "$evidence/latest-block.json"
    capture_optional_json "$rpc_base" /sync/dag_state "$evidence/dag-state.json"
    capture_optional_json "$rpc_base" /validators "$evidence/validators.json"
    capture_optional_json "$rpc_base" /network/info "$evidence/network-info.json"
}

verify_legacy_restart_fence() {
    local service fence
    for service in arc-self-heal.service arc-node.service; do
        fence="/etc/systemd/system/$service.d/arc-recovery-freeze.conf"
        [ -f "$fence" ] && [ ! -L "$fence" ] || die "persistent legacy restart fence is missing: $service"
        grep -Fxq 'RefuseManualStart=yes' "$fence" || die "legacy fence does not refuse manual starts: $service"
        grep -Fxq 'Restart=no' "$fence" || die "legacy fence does not disable restarts: $service"
        systemctl is-active --quiet "$service" && die "legacy service remains active: $service"
        systemctl is-enabled --quiet "$service" && die "legacy service remains enabled: $service"
    done
}

install_legacy_restart_fence() {
    local evidence_root="$1"
    local self_heal_active self_heal_enabled legacy_node_active legacy_node_enabled
    self_heal_active="$(systemctl is-active arc-self-heal.service 2>/dev/null || true)"
    self_heal_enabled="$(systemctl is-enabled arc-self-heal.service 2>/dev/null || true)"
    legacy_node_active="$(systemctl is-active arc-node.service 2>/dev/null || true)"
    legacy_node_enabled="$(systemctl is-enabled arc-node.service 2>/dev/null || true)"

    local unit fence temporary
    for unit in arc-self-heal.service arc-node.service; do
        fence="/etc/systemd/system/$unit.d/arc-recovery-freeze.conf"
        mkdir -p -- "${fence%/*}"
        if [ -e "$fence" ]; then
            [ -f "$fence" ] && [ ! -L "$fence" ] || die "legacy restart fence is not a regular file: $fence"
            grep -Fxq 'RefuseManualStart=yes' "$fence" || die "existing legacy restart fence differs: $fence"
            grep -Fxq 'Restart=no' "$fence" || die "existing legacy restart fence differs: $fence"
        else
            temporary="$(mktemp "${fence}.partial.XXXXXX")"
            {
                printf '[Unit]\nRefuseManualStart=yes\n\n'
                printf '[Service]\nRestart=no\n'
            } > "$temporary"
            chmod 0644 -- "$temporary"
            mv -- "$temporary" "$fence"
        fi
    done
    systemctl daemon-reload
    systemctl disable --now arc-self-heal.service arc-node.service 2>/dev/null || true
    verify_legacy_restart_fence

    python3 - "$evidence_root/legacy-service-fence.json" \
        "$self_heal_active" "$self_heal_enabled" "$legacy_node_active" "$legacy_node_enabled" <<'PY'
import datetime
import json
import pathlib
import sys

output = pathlib.Path(sys.argv[1])
value = {
    "schema": "arc.recovery.legacy-service-fence.v1",
    "captured_at": datetime.datetime.now(datetime.timezone.utc).replace(microsecond=0).isoformat().replace("+00:00", "Z"),
    "prior": {
        "arc-self-heal.service": {"active": sys.argv[2], "enabled": sys.argv[3]},
        "arc-node.service": {"active": sys.argv[4], "enabled": sys.argv[5]},
    },
    "fence": {
        "services": ["arc-self-heal.service", "arc-node.service"],
        "disabled": True,
        "inactive": True,
        "persistent_drop_in": "arc-recovery-freeze.conf",
        "refuse_manual_start": True,
        "restart": "no",
    },
}
with output.open("x", encoding="utf-8", newline="\n") as handle:
    json.dump(value, handle, sort_keys=True, separators=(",", ":"))
    handle.write("\n")
PY
}

stop_node_cleanly() {
    local evidence_root="$1"
    require_commands pgrep pkill systemctl sync sleep python3 grep mkdir mktemp mv chmod
    install_legacy_restart_fence "$evidence_root"
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

verify_stop_identity() {
    local root="$1" capture_id="$2" node="$3"
    grep -Fxq "capture_id=$capture_id" "$root/stop.complete" || \
        die "stop evidence id differs from its immutable path"
    grep -Fxq "node=$node" "$root/stop.complete" || \
        die "stop evidence node differs from its immutable path"
}

fence_stop() {
    local capture_id="$1" node="$2"
    require_hash "$capture_id" "capture id"
    require_node "$node"
    require_commands curl python3 mktemp mv chmod date find pgrep
    local parent="$STOP_BASE/$capture_id" stop_root="$STOP_BASE/$capture_id/$node"
    mkdir -p -- "$parent"
    if [ -e "$stop_root" ]; then
        verify_tree_index "$stop_root" stop.files.sha256 stop.complete
        verify_stop_identity "$stop_root" "$capture_id" "$node"
        stopped_status "$capture_id" "$node"
        return 0
    fi
    local temporary
    temporary="$(mktemp -d "$parent/.${node}.stop.XXXXXX")"
    ARCHIVE_NODE_TEMP_PATH="$temporary"
    mkdir -- "$temporary/evidence"
    capture_pre_stop_evidence "$temporary/evidence"
    stop_node_cleanly "$temporary/evidence"
    verify_legacy_restart_fence
    pgrep -x arc-node >/dev/null 2>&1 && die "legacy arc-node restarted after the persistent fence"
    {
        printf 'schema=arc.recovery.offline-stop.v1\n'
        printf 'capture_id=%s\n' "$capture_id"
        printf 'node=%s\n' "$node"
        printf 'stopped_at=%s\n' "$(date -u +%Y-%m-%dT%H:%M:%SZ)"
        printf 'persistent_restart_fence=true\n'
    } > "$temporary/stop.context"
    write_tree_index "$temporary" stop.files.sha256 stop.complete
    write_complete_marker "$temporary" stop.files.sha256 stop.complete arc.recovery.offline-stop.v1 \
        "capture_id=$capture_id" "node=$node" "stopped=true"
    chmod -R a-w,go-rwx -- "$temporary"
    mv -- "$temporary" "$stop_root"
    ARCHIVE_NODE_TEMP_PATH=""
    verify_tree_index "$stop_root" stop.files.sha256 stop.complete
    verify_stop_identity "$stop_root" "$capture_id" "$node"
    printf 'archive node: LEGACY WRITER FENCED capture=%s node=%s\n' "$capture_id" "$node"
}

stopped_status() {
    local capture_id="$1" node="$2"
    require_hash "$capture_id" "capture id"
    require_node "$node"
    require_commands pgrep python3
    local stop_root="$STOP_BASE/$capture_id/$node"
    verify_tree_index "$stop_root" stop.files.sha256 stop.complete
    verify_stop_identity "$stop_root" "$capture_id" "$node"
    verify_legacy_restart_fence
    pgrep -x arc-node >/dev/null 2>&1 && die "legacy writer is running after freeze"
    printf '{"capture_id":"%s","node":"%s","stopped":true,"restart_fenced":true}\n' \
        "$capture_id" "$node"
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

write_regular_tree_inventory() {
    local root="$1" output="$2"
    python3 - "$root" "$output" <<'PY'
import hashlib
import os
import pathlib
import stat
import sys

root = pathlib.Path(sys.argv[1])
output = pathlib.Path(sys.argv[2])
if not root.is_dir() or root.is_symlink():
    raise SystemExit(f"data directory is missing, not a directory, or a symlink: {root}")
rows = []
for base, dirs, files in os.walk(root, followlinks=False):
    dirs.sort()
    files.sort()
    for name in dirs:
        path = pathlib.Path(base) / name
        if path.is_symlink():
            raise SystemExit(f"symlink directory is forbidden in offline data: {path}")
    for name in files:
        path = pathlib.Path(base) / name
        mode = path.lstat().st_mode
        if not stat.S_ISREG(mode):
            raise SystemExit(f"non-regular member is forbidden in offline data: {path}")
        digest = hashlib.sha256()
        with path.open("rb") as handle:
            for chunk in iter(lambda: handle.read(1024 * 1024), b""):
                digest.update(chunk)
        rel = path.relative_to(root).as_posix()
        rows.append(f"{digest.hexdigest()}  {rel}\n")
with output.open("x", encoding="utf-8", newline="\n") as handle:
    handle.writelines(rows)
PY
}

copy_stopped_data_tree() {
    local source="$1" destination="$2" evidence="$3"
    [ ! -e "$destination" ] || die "offline data destination already exists: $destination"
    write_regular_tree_inventory "$source" "$evidence/source-data.files.sha256"
    mkdir -- "$destination"
    cp --archive --reflink=auto --sparse=always -- "$source/." "$destination/"
    sync
    write_regular_tree_inventory "$destination" "$evidence/copied-data.files.sha256"
    cmp --silent "$evidence/source-data.files.sha256" "$evidence/copied-data.files.sha256" || \
        die "offline data directory differs after copy"
}

capture_offline() {
    local capture_id="$1" node="$2"
    require_hash "$capture_id" "capture id"
    require_node "$node"
    require_commands python3 grep stat cp cmp mv sync date hostname uname find du pgrep mktemp chmod
    stopped_status "$capture_id" "$node" >/dev/null
    local parent="$CAPTURE_BASE/$capture_id" capture_root="$CAPTURE_BASE/$capture_id/$node"
    local stop_root="$STOP_BASE/$capture_id/$node"
    mkdir -p -- "$parent"
    if [ -e "$capture_root" ]; then
        verify_tree_index "$capture_root" capture.files.sha256 capture.complete
        verify_capture_identity "$capture_root" "$capture_id" "$node"
        printf 'archive node: existing offline capture verified capture=%s node=%s\n' "$capture_id" "$node"
        return 0
    fi
    [ -d /root/arc-chain/arc-data ] && [ ! -L /root/arc-chain/arc-data ] || \
        die "stopped legacy data directory is missing or a symlink"
    pgrep -x arc-node >/dev/null 2>&1 && die "refusing offline copy while arc-node is running"

    local temporary
    temporary="$(mktemp -d "$parent/.${node}.capture.XXXXXX")"
    ARCHIVE_NODE_TEMP_PATH="$temporary"
    mkdir -- "$temporary/evidence" "$temporary/evidence/freeze" \
        "$temporary/legacy-public" "$temporary/on-disk-snapshots"
    cp --archive -- "$stop_root/." "$temporary/evidence/freeze/"
    verify_tree_index "$temporary/evidence/freeze" stop.files.sha256 stop.complete
    copy_stopped_data_tree /root/arc-chain/arc-data "$temporary/data-dir" "$temporary/evidence"

    local source destination
    for source in \
        /root/arc-chain/arc-data.snapshot.lz4 \
        /root/arc-chain/arc-data/state.snapshot.lz4; do
        [ -f "$source" ] && [ ! -L "$source" ] || continue
        destination="$temporary/on-disk-snapshots/${source//\//_}"
        copy_stable_file "$source" "$destination"
    done
    for source in \
        /root/arc-chain/genesis.toml \
        /root/arc-chain/deploy/config/genesis.toml \
        /root/arc-chain/testnet-seeds.txt \
        /root/arc-chain/target/release/arc-node; do
        [ -f "$source" ] || continue
        case "$source" in
            */deploy/config/genesis.toml) destination="$temporary/legacy-public/deploy-genesis.toml" ;;
            */target/release/arc-node) destination="$temporary/legacy-public/arc-node" ;;
            *) destination="$temporary/legacy-public/${source##*/}" ;;
        esac
        copy_stable_file "$source" "$destination"
    done

    {
        printf 'capture_id=%s\n' "$capture_id"
        printf 'node=%s\n' "$node"
        printf 'hostname=%s\n' "$(hostname)"
        printf 'captured_offline_at=%s\n' "$(date -u +%Y-%m-%dT%H:%M:%SZ)"
        printf 'kernel=%s\n' "$(uname -srmo)"
        printf 'archive_scope=complete-stopped-legacy-data-v3\n'
        printf 'complete_data_dir=true\n'
        printf 'state_wal_bytes=%s\n' "$(stat -c %s "$temporary/data-dir/state.wal")"
        printf 'state_wal_sha256=%s\n' "$(hash_file "$temporary/data-dir/state.wal")"
        printf 'data_dir_bytes=%s\n' "$(du -s -B1 "$temporary/data-dir" | cut -f1)"
        printf 'on_disk_snapshot_count=%s\n' "$(find "$temporary/on-disk-snapshots" -maxdepth 1 -type f | wc -l | tr -d ' ')"
        printf 'excluded_outside_data_dir_private_material=true\n'
        printf 'excluded_service_environments=true\n'
        printf 'excluded_build_models_and_git=true\n'
    } > "$temporary/capture.inventory"

    [ -s "$temporary/data-dir/state.wal" ] || die "offline data copy has no final state.wal"
    write_tree_index "$temporary" capture.files.sha256 capture.complete
    write_complete_marker "$temporary" capture.files.sha256 capture.complete arc.recovery.capture.v3 \
        "capture_id=$capture_id" "node=$node" "stopped=true" "complete_data_dir=true"
    chmod -R a-w,go-rwx -- "$temporary"
    mv -- "$temporary" "$capture_root"
    ARCHIVE_NODE_TEMP_PATH=""
    verify_tree_index "$capture_root" capture.files.sha256 capture.complete
    verify_capture_identity "$capture_root" "$capture_id" "$node"
    printf 'archive node: OFFLINE DATA CAPTURE COMPLETE capture=%s node=%s\n' "$capture_id" "$node"
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
    verify_legacy_restart_fence
    printf '{"capture_id":"%s","node":"%s","capture_complete":true,"stopped":true}\n' \
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
    "classification": value["classification"],
    "canonical_match": value["canonical_match"],
    "source_height": None if value["exported"] is None else value["exported"]["source_height"],
    "source_block_hash": None if value["exported"] is None else value["exported"]["source_block_hash"],
    "source_state_root": None if value["exported"] is None else value["exported"]["source_state_root"],
    "full_state_root": None if value["exported"] is None else value["exported"]["full_state_root"],
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
        legacy-validators) filename=legacy-validator-set-40m.json; mode=400 ;;
        source-snapshot) filename=source.snapshot.lz4; mode=400 ;;
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

bind_capture() {
    local capture_id="$1" node="$2" manifest="$3"
    local binary_sha="$4" genesis_sha="$5" validators_sha="$6" legacy_validators_sha="$7"
    local source_snapshot_sha="$8" source_wal_sha="$9" checkpoint_sha="${10}"
    local source_height="${11}" source_hash="${12}" source_state_root="${13}"
    local transition_state_root="${14}" checkpoint_manifest="${15}" source_round="${16}"
    local created_at_unix_ms="${17}" recovery_epoch="${18}"
    local validator_set_id="${19}" allow_unbound="${20}"
    require_hash "$capture_id" "capture id"
    require_node "$node"
    require_hash "$manifest" "manifest hash"
    require_hash "$binary_sha" "binary hash"
    require_hash "$genesis_sha" "genesis hash"
    require_hash "$validators_sha" "validator public-key hash"
    require_hash "$legacy_validators_sha" "legacy validator-set hash"
    require_hash "$source_snapshot_sha" "source snapshot hash"
    require_hash "$source_wal_sha" "source WAL hash"
    require_hash "$checkpoint_sha" "checkpoint hash"
    require_uint "$source_height" "source height"
    require_uint "$source_round" "source consensus round"
    require_uint "$created_at_unix_ms" "checkpoint creation timestamp"
    source_hash="$(normalize_hash "$source_hash" "source block hash")"
    source_state_root="$(normalize_hash "$source_state_root" "source state root")"
    transition_state_root="$(normalize_hash "$transition_state_root" "transition state root")"
    checkpoint_manifest="$(normalize_hash "$checkpoint_manifest" "checkpoint manifest hash")"
    require_uint "$recovery_epoch" "recovery epoch"
    require_uint "$validator_set_id" "validator-set id"
    case "$allow_unbound" in true|false) ;; *) die "allow-unbound flag must be true or false" ;; esac
    require_commands python3 mktemp mv chmod cp find sync

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
        "legacy-validator-set-40m.json:$legacy_validators_sha" \
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
    "$stage_root/arc-node" recovery inspect \
        --checkpoint "$stage_root/recovery.arcchkpt" \
        > "$temporary/final-checkpoint.inspect.json" 2> "$temporary/final-checkpoint.inspect.stderr"

    # A final validator capture is classified only from that validator's own
    # stopped data directory and its own on-disk snapshot.  The independently
    # sealed canonical source snapshot is reference evidence; substituting it
    # here would turn a real fork (or an unpaired capture) into invented state.
    python3 - "$capture_root" "$temporary/capture.snapshot.lz4" \
        "$temporary/capture-snapshot.selection.json" <<'PY'
import hashlib
import json
import pathlib
import shutil
import stat
import sys

root = pathlib.Path(sys.argv[1])
output = pathlib.Path(sys.argv[2])
selection_path = pathlib.Path(sys.argv[3])
candidates = [
    root / "data-dir" / "state.snapshot.lz4",
    root / "on-disk-snapshots" / "_root_arc-chain_arc-data_state.snapshot.lz4",
    root / "on-disk-snapshots" / "_root_arc-chain_arc-data.snapshot.lz4",
]
rows = []
for path in candidates:
    if not path.exists() and not path.is_symlink():
        continue
    row = {"path": path.relative_to(root).as_posix()}
    try:
        mode = path.lstat().st_mode
        if path.is_symlink() or not stat.S_ISREG(mode):
            row["error"] = "candidate is not a regular non-symlink file"
        else:
            digest = hashlib.sha256()
            with path.open("rb") as handle:
                for chunk in iter(lambda: handle.read(1024 * 1024), b""):
                    digest.update(chunk)
            row["sha256"] = digest.hexdigest()
            row["bytes"] = path.stat().st_size
    except OSError as error:
        row["error"] = str(error)
    rows.append(row)

usable = [row for row in rows if "sha256" in row]
digests = {row["sha256"] for row in usable}
if any("error" in row for row in rows):
    status = "preserved_unclassified"
    reason = "a capture-local snapshot candidate is unsafe"
elif not usable:
    status = "preserved_unclassified"
    reason = "no capture-local on-disk snapshot was preserved"
elif len(digests) != 1:
    status = "preserved_unclassified"
    reason = "capture-local snapshot candidates disagree"
else:
    selected = usable[0]
    source = root / selected["path"]
    shutil.copyfile(source, output)
    output.chmod(0o400)
    status = "selected"
    reason = None

value = {
    "schema": "arc.recovery.capture-snapshot-selection.v1",
    "status": status,
    "reason": reason,
    "candidates": rows,
    "selected": None if status != "selected" else usable[0],
}
selection_path.write_text(
    json.dumps(value, sort_keys=True, separators=(",", ":")) + "\n",
    encoding="utf-8",
)
PY

    local snapshot_status export_exit_code=125
    snapshot_status="$(python3 -c 'import json,sys; print(json.load(open(sys.argv[1]))["status"])' \
        "$temporary/capture-snapshot.selection.json")"
    if [ "$snapshot_status" = selected ]; then
        local working_data="$temporary/offline-working-data"
        mkdir -- "$working_data"
        cp --archive --reflink=auto --sparse=always -- "$capture_root/data-dir/." "$working_data/"
        chmod -R u+rwX -- "$working_data"
        sync
        local capture_wal_before working_wal_before
        capture_wal_before="$(hash_file "$capture_root/data-dir/state.wal")"
        working_wal_before="$(hash_file "$working_data/state.wal")"
        [ "$capture_wal_before" = "$working_wal_before" ] || \
            die "offline working WAL differs before recovery export"

        local export_command=(
            "$stage_root/arc-node" recovery export
            --data-dir "$working_data"
            --snapshot "$temporary/capture.snapshot.lz4"
            --genesis "$stage_root/genesis.toml"
            --validator-public-keys "$stage_root/validator-public-keys.json"
            --legacy-validator-set "$stage_root/legacy-validator-set-40m.json"
            --output "$temporary/candidate.arcchkpt"
            --source-consensus-round "$source_round"
            --created-at-unix-ms "$created_at_unix_ms"
            --recovery-epoch "$recovery_epoch"
            --validator-set-id "$validator_set_id"
        )
        if [ "$allow_unbound" = true ]; then
            export_command+=(--allow-unbound-legacy-wal)
        fi
        if "${export_command[@]}" > "$temporary/export-summary.json" 2> "$temporary/export.stderr"; then
            export_exit_code=0
        else
            export_exit_code="$?"
        fi

        if [ "$export_exit_code" -eq 0 ] && python3 - \
            "$capture_root/data-dir/state.wal" "$temporary/export-summary.json" \
            "$temporary/offline-wal-recovery.json" "$temporary" \
            2> "$temporary/wal-evidence.stderr" <<'PY'
import hashlib
import json
import pathlib
import sys

original = pathlib.Path(sys.argv[1])
summary_path = pathlib.Path(sys.argv[2])
output = pathlib.Path(sys.argv[3])
evidence = pathlib.Path(sys.argv[4])

def digest(path):
    value = hashlib.sha256()
    with path.open("rb") as handle:
        for chunk in iter(lambda: handle.read(1024 * 1024), b""):
            value.update(chunk)
    return value.hexdigest()

original_sha = digest(original)
original_size = original.stat().st_size
summary = json.loads(summary_path.read_text(encoding="utf-8"))
required = (
    "source_wal_original_bytes",
    "source_wal_accepted_prefix_bytes",
    "source_wal_quarantined_tail_bytes",
    "source_wal_tail_reason",
)
if any(field not in summary for field in required):
    raise SystemExit("recovery exporter omitted exact accepted-prefix/tail metadata")
reported_original = summary["source_wal_original_bytes"]
accepted = summary["source_wal_accepted_prefix_bytes"]
tail_bytes = summary["source_wal_quarantined_tail_bytes"]
tail_reason = summary["source_wal_tail_reason"]
if not all(isinstance(value, int) and not isinstance(value, bool) for value in (
    reported_original, accepted, tail_bytes
)):
    raise SystemExit("recovery exporter WAL byte fields are not integers")
if reported_original != original_size or not 0 <= accepted <= original_size:
    raise SystemExit("recovery exporter WAL boundary does not fit the immutable capture")
if tail_bytes != original_size - accepted:
    raise SystemExit("recovery exporter tail bytes do not complement the accepted prefix")
if tail_bytes > 0 and (not isinstance(tail_reason, str) or not tail_reason.strip()):
    raise SystemExit("recovery exporter ignored a WAL tail without an exact reason")
if tail_bytes == 0 and tail_reason not in (None, "", "none"):
    raise SystemExit("recovery exporter reported a tail reason when every WAL byte was accepted")

prefix_sha = None
tail_sha = None
reconstructed_sha = original_sha
if tail_bytes > 0:
    prefix = evidence / "recovered-state.wal"
    tail = evidence / "quarantined-wal-tail.bin"
    remaining = accepted
    with original.open("rb") as source, prefix.open("xb") as prefix_handle:
        while remaining:
            chunk = source.read(min(1024 * 1024, remaining))
            if not chunk:
                raise SystemExit("immutable WAL ended before accepted prefix boundary")
            prefix_handle.write(chunk)
            remaining -= len(chunk)
        with tail.open("xb") as tail_handle:
            while True:
                chunk = source.read(1024 * 1024)
                if not chunk:
                    break
                tail_handle.write(chunk)
    if prefix.stat().st_size != accepted or tail.stat().st_size != tail_bytes:
        raise SystemExit("sliced prefix/tail sizes do not match exporter metadata")
    prefix_sha = digest(prefix)
    tail_sha = digest(tail)
    reconstructed = hashlib.sha256()
    for path in (prefix, tail):
        with path.open("rb") as handle:
            for chunk in iter(lambda: handle.read(1024 * 1024), b""):
                reconstructed.update(chunk)
    reconstructed_sha = reconstructed.hexdigest()
    if reconstructed_sha != original_sha:
        raise SystemExit("accepted prefix plus quarantined tail does not reconstruct immutable WAL")

value = {
    "schema": "arc.recovery.offline-wal-recovery.v2",
    "capture_wal_sha256": original_sha,
    "capture_wal_bytes": original_size,
    "accepted_prefix_bytes": accepted,
    "accepted_prefix_sha256": prefix_sha,
    "quarantined_tail_bytes": tail_bytes,
    "quarantined_tail_sha256": tail_sha,
    "tail_reason": tail_reason,
    "prefix_plus_tail_sha256": reconstructed_sha,
    "prefix_plus_tail_reconstructs_capture": reconstructed_sha == original_sha,
}
output.write_text(json.dumps(value, sort_keys=True, separators=(",", ":")) + "\n", encoding="utf-8")
PY
        then
            :
        elif [ "$export_exit_code" -eq 0 ]; then
            export_exit_code=126
            printf 'offline WAL evidence rejected: ' >> "$temporary/export.stderr"
            cat "$temporary/wal-evidence.stderr" >> "$temporary/export.stderr"
        fi
        [ "$(hash_file "$capture_root/data-dir/state.wal")" = "$capture_wal_before" ] || \
            die "immutable capture WAL changed during offline export"
        find "$working_data" -depth -delete
    else
        : > "$temporary/export-summary.json"
        python3 - "$temporary/capture-snapshot.selection.json" "$temporary/export.stderr" <<'PY'
import json
import pathlib
import sys
selection = json.loads(pathlib.Path(sys.argv[1]).read_text(encoding="utf-8"))
pathlib.Path(sys.argv[2]).write_text(
    f"offline export not attempted: {selection['reason']}\n", encoding="utf-8"
)
PY
    fi
    printf '%s\n' "$export_exit_code" > "$temporary/export.exit-code"

    python3 - \
        "$temporary/final-checkpoint.inspect.json" "$temporary/export-summary.json" \
        "$temporary/binding.json" "$capture_id" "$node" "$manifest" "$source_round" \
        "$source_height" "$source_hash" "$source_state_root" "$transition_state_root" \
        "$checkpoint_manifest" "$created_at_unix_ms" "$recovery_epoch" "$validator_set_id" \
        "$allow_unbound" "$legacy_validators_sha" "$source_snapshot_sha" "$source_wal_sha" \
        "$export_exit_code" <<'PY'
import json
import pathlib
import sys

(inspect_path, export_path, output_path, capture_id, node, manifest, source_round,
 source_height, source_hash, source_state_root, transition_state_root,
 checkpoint_manifest, created_at_unix_ms, recovery_epoch, validator_set_id,
 allow_unbound, legacy_validators_sha, source_snapshot_sha, source_wal_sha,
 export_exit_code) = sys.argv[1:]

with open(inspect_path, "r", encoding="utf-8") as handle:
    final = json.load(handle)
def bare(value, field):
    if not isinstance(value, str):
        raise ValueError(f"{field} is not a hash")
    value = value.removeprefix("0x")
    if len(value) != 64 or any(c not in "0123456789abcdef" for c in value):
        raise ValueError(f"{field} is not a lowercase 32-byte hash")
    return value

expected = {
    "source_height": int(source_height),
    "source_block_hash": source_hash,
    "source_state_root": source_state_root,
    "full_state_root": transition_state_root,
    "manifest_hash": checkpoint_manifest,
    "source_consensus_round": int(source_round),
    "created_at_unix_ms": int(created_at_unix_ms),
    "recovery_epoch": int(recovery_epoch),
    "validator_set_id": int(validator_set_id),
    "source_validator_count": 8,
    "source_validator_stake": 40_000_000,
    "source_validator_set_hash": "80d7c2d229fea4171732fd04451372d849fab7baefed143a2a445ae72f472ecd",
}
if final.get("source_height") != expected["source_height"]:
    raise SystemExit("sealed checkpoint source height differs from rollout manifest")
if final.get("status") != "UNTRUSTED_INSPECTION":
    raise SystemExit("sealed checkpoint inspection returned an unexpected status")
for key in ("source_block_hash", "source_state_root", "full_state_root", "manifest_hash"):
    if bare(final.get(key), f"final checkpoint {key}") != expected[key]:
        raise SystemExit(f"sealed checkpoint {key} differs from rollout manifest")
for key in ("source_consensus_round", "created_at_unix_ms", "recovery_epoch", "validator_set_id"):
    if final.get(key) != expected[key]:
        raise SystemExit(f"sealed checkpoint {key} differs from rollout manifest")
for key in ("source_validator_count", "source_validator_stake"):
    if final.get(key) != expected[key]:
        raise SystemExit(f"sealed checkpoint {key} violates the fixed legacy source-set contract")
if bare(final.get("source_validator_set_hash"), "final checkpoint source_validator_set_hash") != expected["source_validator_set_hash"]:
    raise SystemExit("sealed checkpoint source_validator_set_hash differs from the audited 8-validator source set")

actual = None
classification = "preserved_unclassified"
classification_reason = None
if int(export_exit_code) != 0:
    classification_reason = f"recovery export exited nonzero ({export_exit_code})"
else:
    try:
        exported = json.loads(pathlib.Path(export_path).read_text(encoding="utf-8"))
        actual = {
            "source_height": exported.get("source_height"),
            "source_block_hash": bare(exported.get("source_block_hash"), "export source_block_hash"),
            "source_state_root": bare(exported.get("source_state_root"), "export source_state_root"),
            "full_state_root": bare(exported.get("full_state_root"), "export full_state_root"),
            "source_consensus_round": exported.get("source_consensus_round"),
            "created_at_unix_ms": exported.get("created_at_unix_ms"),
            "recovery_epoch": exported.get("recovery_epoch"),
            "validator_set_id": exported.get("validator_set_id"),
            "manifest_hash": bare(exported.get("manifest_hash"), "export manifest_hash"),
            "payload_hash": bare(exported.get("payload_hash"), "export payload_hash"),
            "source_validator_count": exported.get("source_validator_count"),
            "source_validator_stake": exported.get("source_validator_stake"),
            "source_validator_set_hash": bare(
                exported.get("source_validator_set_hash"), "export source_validator_set_hash"
            ),
        }
        if exported.get("status") != "EXPORTED_UNSIGNED":
            raise ValueError("recovery export returned an unexpected status")
        if any(actual[key] != expected[key] for key in (
            "source_validator_count", "source_validator_stake", "source_validator_set_hash",
        )):
            raise ValueError("exported source validator set violates the fixed legacy contract")
        canonical_fields = (
            "source_height", "source_block_hash", "source_state_root", "full_state_root",
            "source_consensus_round", "created_at_unix_ms", "recovery_epoch",
            "validator_set_id", "manifest_hash", "source_validator_count",
            "source_validator_stake", "source_validator_set_hash",
        )
        if all(actual[key] == expected[key] for key in canonical_fields):
            classification = "valid_canonical"
        else:
            classification = "valid_noncanonical_fork"
            classification_reason = "internally valid export differs from the selected sealed checkpoint"
    except (OSError, json.JSONDecodeError, TypeError, ValueError) as error:
        classification_reason = f"export output is not an internally valid capture-local pair: {error}"

canonical_match = classification == "valid_canonical"
binding = {
    "schema": "arc.recovery.capture-binding.v3",
    "capture_id": capture_id,
    "node": node,
    "rollout_manifest_sha256": manifest,
    "source_consensus_round": int(source_round),
    "created_at_unix_ms": int(created_at_unix_ms),
    "allow_unbound_legacy_wal": allow_unbound == "true",
    "legacy_validator_set_artifact_sha256": legacy_validators_sha,
    "source_snapshot_artifact_sha256": source_snapshot_sha,
    "reference_source_wal_artifact_sha256": source_wal_sha,
    "export_exit_code": int(export_exit_code),
    "classification": classification,
    "classification_reason": classification_reason,
    "canonical_match": canonical_match,
    "expected": expected,
    "exported": actual,
}
with open(output_path, "x", encoding="utf-8", newline="\n") as handle:
    json.dump(binding, handle, sort_keys=True, separators=(",", ":"))
    handle.write("\n")
PY

    write_tree_index "$temporary" binding.files.sha256 binding.complete
    write_complete_marker "$temporary" binding.files.sha256 binding.complete arc.recovery.capture-binding.v3 \
        "capture_id=$capture_id" "node=$node" "rollout_manifest_sha256=$manifest"
    chmod -R a-w,go-rwx -- "$temporary"
    mv -- "$temporary" "$binding_root"
    ARCHIVE_NODE_TEMP_PATH=""
    verify_tree_index "$binding_root" binding.files.sha256 binding.complete
    verify_binding_identity "$binding_root" "$capture_id" "$node" "$manifest"
    local classification
    classification="$(python3 -c 'import json,sys; print(json.load(open(sys.argv[1]))["classification"])' "$binding_root/binding.json")"
    printf 'archive node: BINDING COMPLETE manifest=%s node=%s classification=%s\n' \
        "$manifest" "$node" "$classification"
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

    local canonical classification
    canonical="$(python3 -c 'import json,sys; print(str(json.load(open(sys.argv[1]))["canonical_match"]).lower())' "$binding_root/binding.json")"
    classification="$(python3 -c 'import json,sys; print(json.load(open(sys.argv[1]))["classification"])' "$binding_root/binding.json")"
    {
        printf 'manifest_sha256=%s\n' "$manifest"
        printf 'capture_id=%s\n' "$capture_id"
        printf 'node=%s\n' "$node"
        printf 'classification=%s\n' "$classification"
        printf 'canonical_match=%s\n' "$canonical"
        printf 'archive_scope=complete-stopped-legacy-data-v3\n'
        printf 'complete_data_dir=true\n'
        printf 'excluded_outside_data_dir_private_material=true\n'
        printf 'excluded_service_environments=true\n'
        printf 'excluded_build_models_and_git=true\n'
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
    printf 'archive node: BUNDLE COMPLETE node=%s classification=%s bytes=%s sha256=%s\n' \
        "$node" "$classification" "$(stat -c %s "$archive")" "$(hash_file "$archive")"
}

bundle_status() {
    local capture_id="$1" node="$2" manifest="$3"
    require_hash "$capture_id" "capture id"
    require_node "$node"
    require_hash "$manifest" "manifest hash"
    require_commands python3 stat
    local archive_root="$ARCHIVE_BASE/$manifest"
    local archive="$archive_root/legacy-$node.tar.zst"
    local checksum="$archive.sha256"
    local inventory="$archive_root/legacy-$node.inventory"
    local inventory_checksum="$inventory.sha256"
    local candidate
    for candidate in "$archive" "$checksum" "$inventory" "$inventory_checksum"; do
        [ -f "$candidate" ] && [ ! -L "$candidate" ] || \
            die "bundle status input is missing, non-regular, or a symlink: $candidate"
    done

    local archive_sha inventory_sha checksum_sha inventory_checksum_sha classification
    archive_sha="$(hash_file "$archive")"
    inventory_sha="$(hash_file "$inventory")"
    [ "$(cat "$checksum")" = "$archive_sha  ${archive##*/}" ] || \
        die "archive sidecar does not exactly bind the verified archive"
    [ "$(cat "$inventory_checksum")" = "$inventory_sha  ${inventory##*/}" ] || \
        die "inventory sidecar does not exactly bind the verified inventory"
    grep -Fxq "manifest_sha256=$manifest" "$inventory" || \
        die "bundle inventory rollout manifest differs"
    grep -Fxq "capture_id=$capture_id" "$inventory" || \
        die "bundle inventory capture id differs"
    grep -Fxq "node=$node" "$inventory" || die "bundle inventory node differs"
    classification="$(sed -n 's/^classification=//p' "$inventory")"
    case "$classification" in
        valid_canonical|valid_noncanonical_fork|preserved_unclassified) ;;
        *) die "bundle inventory has an invalid classification" ;;
    esac
    checksum_sha="$(hash_file "$checksum")"
    inventory_checksum_sha="$(hash_file "$inventory_checksum")"

    python3 - "$capture_id" "$node" "$manifest" "$classification" \
        "${archive##*/}" "$(stat -c %s "$archive")" "$archive_sha" \
        "${checksum##*/}" "$checksum_sha" \
        "${inventory##*/}" "$(stat -c %s "$inventory")" "$inventory_sha" \
        "${inventory_checksum##*/}" "$inventory_checksum_sha" <<'PY'
import json
import sys

(capture_id, node, manifest, classification, archive_name, archive_size,
 archive_sha, archive_sidecar_name, archive_sidecar_sha, inventory_name,
 inventory_size, inventory_sha, inventory_sidecar_name,
 inventory_sidecar_sha) = sys.argv[1:]
value = {
    "schema": "arc.recovery.bundle-status.v1",
    "capture_id": capture_id,
    "node": node,
    "rollout_manifest_sha256": manifest,
    "classification": classification,
    "bundle": {
        "name": archive_name,
        "size": int(archive_size),
        "sha256": archive_sha,
        "sidecar_name": archive_sidecar_name,
        "sidecar_sha256": archive_sidecar_sha,
    },
    "inventory": {
        "name": inventory_name,
        "size": int(inventory_size),
        "sha256": inventory_sha,
        "sidecar_name": inventory_sidecar_name,
        "sidecar_sha256": inventory_sidecar_sha,
    },
}
print(json.dumps(value, sort_keys=True, separators=(",", ":")))
PY
}

ACTION="${1:-}"
case "$ACTION" in
    fence-stop)
        [ "$#" -eq 3 ] || { usage >&2; exit 2; }
        fence_stop "$2" "$3"
        ;;
    stopped-status)
        [ "$#" -eq 3 ] || { usage >&2; exit 2; }
        stopped_status "$2" "$3"
        ;;
    capture-offline)
        [ "$#" -eq 3 ] || { usage >&2; exit 2; }
        capture_offline "$2" "$3"
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
        [ "$#" -eq 21 ] || { usage >&2; exit 2; }
        bind_capture "$2" "$3" "$4" "$5" "$6" "$7" "$8" "$9" \
            "${10}" "${11}" "${12}" "${13}" "${14}" "${15}" "${16}" \
            "${17}" "${18}" "${19}" "${20}" "${21}"
        ;;
    bundle)
        [ "$#" -eq 4 ] || { usage >&2; exit 2; }
        bundle_capture "$2" "$3" "$4"
        ;;
    bundle-status)
        [ "$#" -eq 4 ] || { usage >&2; exit 2; }
        bundle_status "$2" "$3" "$4"
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
