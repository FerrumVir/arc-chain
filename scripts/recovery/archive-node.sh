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
LIVE_OBSERVATION_BASE="/root/arc-recovery-live-observations"
BINDING_BASE="/root/arc-recovery-bindings"
SEAL_BASE="/root/arc-recovery-seal"
ARCHIVE_BASE="/root/arc-recovery-archive"
PERSISTED_HEAD_BASE="/root/arc-recovery-persisted-heads"
NETWORK_FENCE_STATE="/etc/arc-recovery/network-fence"
NETWORK_FENCE_UNIT="/etc/systemd/system/arc-legacy-maintenance-fence.service"
ARCHIVE_NODE_TEMP_PATH=""

# Every semantic Python block runs through one normalized, inode/hash-pinned
# root-owned interpreter under an empty environment and isolated mode.  The
# guard is re-proved before every invocation so a path swap cannot silently
# change the meaning of a recovery receipt.
ARC_RECOVERY_PYTHON_LINK="/usr/bin/python3"
[ -e "$ARC_RECOVERY_PYTHON_LINK" ] && [ ! -d "$ARC_RECOVERY_PYTHON_LINK" ] || {
    printf 'archive node: pinned Python entrypoint is missing\n' >&2
    exit 1
}
ARC_RECOVERY_PYTHON_PATH="$(readlink -f -- "$ARC_RECOVERY_PYTHON_LINK")"
case "$ARC_RECOVERY_PYTHON_PATH" in /usr/bin/python3|/usr/bin/python3.[0-9]*) ;; *)
    printf 'archive node: normalized Python path is outside the reviewed /usr/bin closure\n' >&2
    exit 1
esac
[ -f "$ARC_RECOVERY_PYTHON_PATH" ] && [ ! -L "$ARC_RECOVERY_PYTHON_PATH" ] || {
    printf 'archive node: normalized Python interpreter is not a regular file\n' >&2
    exit 1
}
arc_recovery_python_projection() {
    stat -c '%u:%g:%a:%h:%d:%i' -- "$1" 2>/dev/null || \
        stat -f '%u:%g:%Lp:%l:%d:%i' -- "$1"
}
arc_recovery_sha256_file() {
    if [ -x /usr/bin/sha256sum ]; then
        /usr/bin/sha256sum -- "$1" | /usr/bin/cut -d' ' -f1
    else
        /usr/bin/shasum -a 256 -- "$1" | /usr/bin/cut -d' ' -f1
    fi
}
ARC_RECOVERY_PYTHON_PROJECTION="$(arc_recovery_python_projection "$ARC_RECOVERY_PYTHON_PATH")"
case "$ARC_RECOVERY_PYTHON_PROJECTION" in
    0:0:755:*:*:*) ;;
    *) printf 'archive node: normalized Python interpreter inode is unsafe\n' >&2; exit 1 ;;
esac
if [ "$(uname -s)" = Linux ]; then
    case "$ARC_RECOVERY_PYTHON_PROJECTION" in 0:0:755:1:*:*) ;; *)
        printf 'archive node: normalized Python interpreter link count is unsafe\n' >&2
        exit 1
    esac
fi
case "$ARC_RECOVERY_PYTHON_PROJECTION" in
    *:*:*:*:*:*) ;;
    *) {
    printf 'archive node: normalized Python interpreter inode is unsafe\n' >&2
    exit 1
    } ;;
esac
ARC_RECOVERY_PYTHON_SHA256="$(arc_recovery_sha256_file "$ARC_RECOVERY_PYTHON_PATH")"
ARC_RECOVERY_PYTHON_DEVICE="$(printf '%s\n' "$ARC_RECOVERY_PYTHON_PROJECTION" | /usr/bin/cut -d: -f5)"
ARC_RECOVERY_PYTHON_INODE="$(printf '%s\n' "$ARC_RECOVERY_PYTHON_PROJECTION" | /usr/bin/cut -d: -f6)"

python3() {
    local current_path current_projection current_sha
    current_path="$(readlink -f -- "$ARC_RECOVERY_PYTHON_LINK")"
    [ "$current_path" = "$ARC_RECOVERY_PYTHON_PATH" ] || \
        die "pinned Python normalized path changed"
    current_projection="$(arc_recovery_python_projection "$current_path")"
    [ "$current_projection" = "$ARC_RECOVERY_PYTHON_PROJECTION" ] || \
        die "pinned Python inode projection changed"
    current_sha="$(arc_recovery_sha256_file "$current_path")"
    [ "$current_sha" = "$ARC_RECOVERY_PYTHON_SHA256" ] || \
        die "pinned Python content hash changed"
    /usr/bin/env -i PATH=/usr/bin:/bin LC_ALL=C TZ=UTC PYTHONHASHSEED=0 \
        "$ARC_RECOVERY_PYTHON_PATH" -I "$@"
}

cleanup_temporary_path() {
    # Once stop.intent exists, a persistent restart fence may already have
    # been installed even if its next durable marker was interrupted. Preserve
    # the journal across SSH loss or reboot; only a lock holder may reconcile it.
    if [ -n "$ARCHIVE_NODE_TEMP_PATH" ]; then
        if { [ -f "$ARCHIVE_NODE_TEMP_PATH/01-prefreeze-runtime-safety-intent.json" ] \
                && [ ! -L "$ARCHIVE_NODE_TEMP_PATH/01-prefreeze-runtime-safety-intent.json" ]; } \
            || { [ -f "$ARCHIVE_NODE_TEMP_PATH/02-fast-cgroup-freeze-intent.json" ] \
                && [ ! -L "$ARCHIVE_NODE_TEMP_PATH/02-fast-cgroup-freeze-intent.json" ]; } \
            || { [ -f "$ARCHIVE_NODE_TEMP_PATH/stop.intent.json" ] \
                && [ ! -L "$ARCHIVE_NODE_TEMP_PATH/stop.intent.json" ]; } \
            || { [ -f "$ARCHIVE_NODE_TEMP_PATH/04-pre-fence-quiesce-intent.json" ] \
                && [ ! -L "$ARCHIVE_NODE_TEMP_PATH/04-pre-fence-quiesce-intent.json" ]; }; then
            return 0
        fi
    fi
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
  archive-node.sh stage-recovery-barrier NODE
  archive-node.sh capture-live-observations CAPTURE_SHA256 NODE FREEZE_SHA256 \
    WRITER_PID WRITER_START_TICKS BOOT_ID EXECUTABLE_PATH EXECUTABLE_SHA256 \
    ARGV_SHA256 RPC_ORIGIN
  archive-node.sh live-observations-status CAPTURE_SHA256 NODE FREEZE_SHA256
  archive-node.sh live-observations-eligible CAPTURE_SHA256 NODE FREEZE_SHA256 \
    WRITER_PID WRITER_START_TICKS BOOT_ID
  archive-node.sh legacy-height-bracket CAPTURE_SHA256 NODE FREEZE_SHA256 \
    WRITER_PID WRITER_START_TICKS BOOT_ID EXECUTABLE_PATH EXECUTABLE_SHA256 \
    ARGV_SHA256 RPC_ORIGIN PUBLIC_BEFORE PUBLIC_LATEST PUBLIC_AFTER \
    PUBLIC_LATEST_BLOCK_HASH CHALLENGE_SHA256
  archive-node.sh fence-stop CAPTURE_SHA256 NODE FREEZE_SHA256 VALIDATOR_ADDRESS STAKE \
    WRITER_PID WRITER_START_TICKS BOOT_ID WRITER_CGROUP_SHA256 WRITER_SUPERVISION_MODE \
    SUPERVISOR_UNIT SUPERVISOR_MAIN_PID \
    SUPERVISOR_START_TICKS SUPERVISOR_EXECUTABLE_PATH SUPERVISOR_EXECUTABLE_SHA256 \
    SUPERVISOR_ARGV_SHA256 SUPERVISOR_CONTEXT_SHA256 \
    EXECUTABLE_PATH EXECUTABLE_SHA256 ARGV_SHA256 DATA_DIR
  archive-node.sh quarantine CAPTURE_SHA256 NODE FREEZE_SHA256 VALIDATOR_ADDRESS STAKE \
    WRITER_PID WRITER_START_TICKS BOOT_ID WRITER_CGROUP_SHA256 WRITER_SUPERVISION_MODE \
    SUPERVISOR_UNIT SUPERVISOR_MAIN_PID \
    SUPERVISOR_START_TICKS SUPERVISOR_EXECUTABLE_PATH SUPERVISOR_EXECUTABLE_SHA256 \
    SUPERVISOR_ARGV_SHA256 SUPERVISOR_CONTEXT_SHA256 \
    EXECUTABLE_PATH EXECUTABLE_SHA256 ARGV_SHA256 DATA_DIR
  archive-node.sh quarantine-status CAPTURE_SHA256 NODE FREEZE_SHA256
  archive-node.sh quarantine-restart-arm CAPTURE_SHA256 NODE FREEZE_SHA256
  archive-node.sh quarantine-restart-status CAPTURE_SHA256 NODE FREEZE_SHA256
  archive-node.sh quarantine-monitor-receipt CAPTURE_SHA256 NODE FREEZE_SHA256
  archive-node.sh quarantine-retire CAPTURE_SHA256 NODE FREEZE_SHA256 \
    ROLLOUT_MANIFEST_SHA256 ARCHIVE_MANIFEST_SHA256 \
    LEGACY_MAINTENANCE_BOUNDARY_SHA256 LEGACY_MAINTENANCE_EVIDENCE_BUNDLE_SHA256
  archive-node.sh quarantine-retire-status CAPTURE_SHA256 NODE FREEZE_SHA256 \
    ROLLOUT_MANIFEST_SHA256 ARCHIVE_MANIFEST_SHA256 \
    LEGACY_MAINTENANCE_BOUNDARY_SHA256 LEGACY_MAINTENANCE_EVIDENCE_BUNDLE_SHA256
  archive-node.sh quarantine-public-cross-proof CAPTURE_SHA256 NODE FREEZE_SHA256 \
    PUBLIC_INFO_AFTER PUBLIC_LATEST_HEIGHT PUBLIC_LATEST_HASH CHALLENGE_SHA256
  archive-node.sh persisted-head CAPTURE_SHA256 NODE FREEZE_SHA256 BINARY_SHA256 \
    GENESIS_SHA256 VALIDATORS_SHA256 LEGACY_VALIDATORS_SHA256 BOOT_ID
  archive-node.sh stopped-status CAPTURE_SHA256 NODE [FREEZE_SHA256 VALIDATOR_ADDRESS STAKE \
    WRITER_PID WRITER_START_TICKS BOOT_ID WRITER_CGROUP_SHA256 WRITER_SUPERVISION_MODE \
    SUPERVISOR_UNIT SUPERVISOR_MAIN_PID \
    SUPERVISOR_START_TICKS SUPERVISOR_EXECUTABLE_PATH SUPERVISOR_EXECUTABLE_SHA256 \
    SUPERVISOR_ARGV_SHA256 SUPERVISOR_CONTEXT_SHA256 \
    EXECUTABLE_PATH EXECUTABLE_SHA256 ARGV_SHA256 DATA_DIR]
  archive-node.sh stopped-status-challenged CAPTURE_SHA256 NODE FREEZE_SHA256 \
    VALIDATOR_ADDRESS STAKE WRITER_PID WRITER_START_TICKS BOOT_ID WRITER_CGROUP_SHA256 \
    WRITER_SUPERVISION_MODE SUPERVISOR_UNIT SUPERVISOR_MAIN_PID SUPERVISOR_START_TICKS \
    SUPERVISOR_EXECUTABLE_PATH SUPERVISOR_EXECUTABLE_SHA256 SUPERVISOR_ARGV_SHA256 \
    SUPERVISOR_CONTEXT_SHA256 EXECUTABLE_PATH EXECUTABLE_SHA256 ARGV_SHA256 DATA_DIR \
    FIXED_HOST CHALLENGE_SHA256
  archive-node.sh capture-offline CAPTURE_SHA256 NODE
  archive-node.sh status CAPTURE_SHA256 NODE
  archive-node.sh sealed-source-status CAPTURE_SHA256 NODE FREEZE_SHA256 \
    VALIDATOR_ADDRESS STAKE WRITER_PID WRITER_START_TICKS BOOT_ID WRITER_CGROUP_SHA256 \
    WRITER_SUPERVISION_MODE SUPERVISOR_UNIT \
    SUPERVISOR_MAIN_PID SUPERVISOR_START_TICKS SUPERVISOR_EXECUTABLE_PATH \
    SUPERVISOR_EXECUTABLE_SHA256 SUPERVISOR_ARGV_SHA256 SUPERVISOR_CONTEXT_SHA256 EXECUTABLE_PATH \
    EXECUTABLE_SHA256 ARGV_SHA256 DATA_DIR
  archive-node.sh stage-input MANIFEST_SHA256 NODE ROLE EXPECTED_SHA256 < FILE
  archive-node.sh validator-key-identity MANIFEST_SHA256 NODE CLI_SHA256 \
    KEYFILE_SHA256 EXPECTED_ADDRESS
  archive-node.sh validator-key-identity-transient NODE CLI_SHA256 \
    KEYFILE_SHA256 EXPECTED_ADDRESS CHALLENGE_SHA256 < ARC_CLI
  archive-node.sh bind CAPTURE_SHA256 NODE MANIFEST_SHA256 BINARY_SHA256 \
    GENESIS_SHA256 VALIDATORS_SHA256 LEGACY_VALIDATORS_SHA256 SOURCE_SNAPSHOT_SHA256 \
    SOURCE_WAL_SHA256 CHECKPOINT_SHA256 SOURCE_HEIGHT SOURCE_BLOCK_HASH \
    SOURCE_STATE_ROOT TRANSITION_STATE_ROOT CHECKPOINT_MANIFEST_HASH \
    SOURCE_CONSENSUS_ROUND CREATED_AT_UNIX_MS \
    RECOVERY_EPOCH VALIDATOR_SET_ID ALLOW_UNBOUND_LEGACY_WAL
  archive-node.sh stream-bundle CAPTURE_SHA256 NODE MANIFEST_SHA256 > legacy-NODE.tar.zst
  archive-node.sh stream-inventory CAPTURE_SHA256 NODE MANIFEST_SHA256
  archive-node.sh verify-index CAPTURE_DIRECTORY

fence-stop must complete for all six controlled writers before capture-offline
content-indexes any chain byte. The original tree stays in place; stream-bundle
emits it without creating another full validator copy.
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

require_safe_absolute_path() {
    case "$1" in
        /*) ;;
        *) die "$2 must be absolute" ;;
    esac
    case "$1" in
        *$'\n'*|*$'\r'*|*..*) die "$2 contains an unsafe component" ;;
    esac
    printf '%s\n' "$1" | grep -Eq '^/[A-Za-z0-9._/@%+=,-]+$' || \
        die "$2 contains unsupported characters"
}

prepare_owned_partial_directory() {
    local path="$1" expected_owner="$2"
    if [ -e "$path" ] || [ -L "$path" ]; then
        [ -d "$path" ] && [ ! -L "$path" ] || die "partial path is not a real directory: $path"
        [ -f "$path/.arc-recovery-partial-owner" ] && [ ! -L "$path/.arc-recovery-partial-owner" ] || \
            die "partial path has no recovery ownership marker: $path"
        [ "$(cat "$path/.arc-recovery-partial-owner")" = "$expected_owner" ] || \
            die "partial path ownership marker differs: $path"
        find "$path" -xdev -depth -delete
    fi
    mkdir -- "$path"
    printf '%s\n' "$expected_owner" > "$path/.arc-recovery-partial-owner"
    chmod 400 "$path/.arc-recovery-partial-owner"
    python3 - "$path" "$expected_owner" <<'PY'
import os
import pathlib
import subprocess
import stat
import sys

root = pathlib.Path(sys.argv[1])
expected = (sys.argv[2] + "\n").encode()
owner = root / ".arc-recovery-partial-owner"
details = owner.lstat()
if owner.is_symlink() or not stat.S_ISREG(details.st_mode) or details.st_mode & 0o222:
    raise SystemExit("recovery partial owner is not immutable and regular")
if owner.read_bytes() != expected:
    raise SystemExit("recovery partial owner bytes differ before durability barrier")
for target, directory in ((owner, False), (root, True), (root.parent, True)):
    flags = os.O_RDONLY | getattr(os, "O_NOFOLLOW", 0)
    if directory:
        flags |= getattr(os, "O_DIRECTORY", 0)
    descriptor = os.open(target, flags)
    try:
        os.fsync(descriptor)
    finally:
        os.close(descriptor)
PY
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
import secrets
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
import hashlib
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
    raise SystemExit("recovery exporter WAL boundary does not fit the content-sealed capture")
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
    prefix_digest = hashlib.sha256()
    tail_digest = hashlib.sha256()
    reconstructed = hashlib.sha256()
    remaining = accepted
    prefix_size = 0
    tail_size = 0
    with original.open("rb") as source:
        while remaining:
            chunk = source.read(min(1024 * 1024, remaining))
            if not chunk:
                raise SystemExit("immutable WAL ended before accepted prefix boundary")
            prefix_digest.update(chunk)
            reconstructed.update(chunk)
            prefix_size += len(chunk)
            remaining -= len(chunk)
        while True:
            chunk = source.read(1024 * 1024)
            if not chunk:
                break
            tail_digest.update(chunk)
            reconstructed.update(chunk)
            tail_size += len(chunk)
    if prefix_size != accepted or tail_size != tail_bytes:
        raise SystemExit("sliced prefix/tail sizes do not match exporter metadata")
    prefix_sha = prefix_digest.hexdigest()
    tail_sha = tail_digest.hexdigest()
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
import subprocess
import sys

root = pathlib.Path(sys.argv[1])
index_name, complete_name = sys.argv[2:]
index_path = root / index_name
temporary_path = root / f".{index_name}.partial"
complete_temporary_path = root / f".{complete_name}.partial"
if index_path.exists() or (root / complete_name).exists():
    raise SystemExit("index or completion marker already exists")
if temporary_path.exists() or temporary_path.is_symlink():
    mode = temporary_path.lstat().st_mode
    if not stat.S_ISREG(mode) or temporary_path.is_symlink():
        raise SystemExit("unsafe partial index marker")
    temporary_path.unlink()
if complete_temporary_path.exists() or complete_temporary_path.is_symlink():
    mode = complete_temporary_path.lstat().st_mode
    if not stat.S_ISREG(mode) or complete_temporary_path.is_symlink():
        raise SystemExit("unsafe partial completion marker")
    complete_temporary_path.unlink()

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

descriptor = os.open(temporary_path, os.O_WRONLY | os.O_CREAT | os.O_EXCL | getattr(os, "O_NOFOLLOW", 0), 0o400)
with os.fdopen(descriptor, "w", encoding="utf-8", newline="\n") as handle:
    for rel, digest in sorted(rows):
        handle.write(f"{digest}  {rel}\n")
    handle.flush(); os.fsync(handle.fileno())
os.rename(temporary_path, index_path)
directory = os.open(root, os.O_RDONLY | getattr(os, "O_DIRECTORY", 0))
try:
    os.fsync(directory)
finally:
    os.close(directory)
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
    python3 - "$root" "$complete_name" "$schema" "$index_sha" "$@" <<'PY'
import os
import pathlib
import stat
import sys

root = pathlib.Path(sys.argv[1])
complete_name, schema, index_sha = sys.argv[2:5]
fields = sys.argv[5:]
output = root / complete_name
temporary = root / f".{complete_name}.partial"
if output.exists() or output.is_symlink():
    raise SystemExit("completion marker already exists")
if temporary.exists() or temporary.is_symlink():
    mode = temporary.lstat().st_mode
    if not stat.S_ISREG(mode) or temporary.is_symlink():
        raise SystemExit("unsafe partial completion marker")
    temporary.unlink()
payload = (f"schema={schema}\nindex_sha256={index_sha}\n" + "".join(f"{field}\n" for field in fields)).encode()
descriptor = os.open(temporary, os.O_WRONLY | os.O_CREAT | os.O_EXCL | getattr(os, "O_NOFOLLOW", 0), 0o400)
with os.fdopen(descriptor, "wb") as handle:
    handle.write(payload); handle.flush(); os.fsync(handle.fileno())
os.rename(temporary, output)
directory = os.open(root, os.O_RDONLY | getattr(os, "O_DIRECTORY", 0))
try:
    os.fsync(directory)
finally:
    os.close(directory)
PY
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

# These three live HTTP responses are preserved only as bounded diagnostic
# evidence. They are not consensus state and are never reward evidence.  The
# persistent per-endpoint intent journal makes an interrupted GET a one-way
# transition: a retry records the interruption instead of issuing that GET
# again. The final directory and receipt are create-only.
capture_live_observation_receipt_at() {
    local partial="$1" final="$2" capture_id="$3" node="$4" freeze_sha="$5" rpc_origin="$6"
    [ "$partial" != "$final" ] || die "live-observation partial/final paths collide"
    case "$rpc_origin" in
        http://127.0.0.1:[1-9]* ) ;;
        *) die "live-observation origin is not loopback HTTP" ;;
    esac
    python3 - "$partial" "$capture_id" "$node" "$freeze_sha" "$rpc_origin" <<'PY'
import datetime
import hashlib
import http.client
import json
import os
import pathlib
import re
import stat
import sys
import time
import urllib.parse

partial_raw, capture_id, node, freeze_sha, rpc_origin = sys.argv[1:]
partial = pathlib.Path(partial_raw)
endpoints = (
    ("inference-results", "/inference/results"),
    ("workers-scoreboard", "/workers/scoreboard"),
    ("inference-attestations", "/inference/attestations"),
)
labels = ["diagnostic", "noncanonical", "nonreward"]
max_body_bytes = 8 * 1024 * 1024
timeout_seconds = 20
hash_re = re.compile(r"[0-9a-f]{64}")
if not hash_re.fullmatch(capture_id) or not hash_re.fullmatch(freeze_sha):
    raise SystemExit("live-observation identity hash is malformed")
if node not in {"nyc", "lax", "ams", "lhr", "nrt", "sgp"}:
    raise SystemExit("live-observation node is invalid")
origin = urllib.parse.urlsplit(rpc_origin)
if (origin.scheme, origin.hostname, origin.path, origin.query, origin.fragment) != (
        "http", "127.0.0.1", "", "", "") or origin.port is None:
    raise SystemExit("live-observation origin must be an exact loopback HTTP origin")

def now():
    return datetime.datetime.now(datetime.timezone.utc).isoformat(timespec="microseconds").replace("+00:00", "Z")

def enforce_deadline(connection, deadline, response=None):
    remaining = deadline - time.monotonic()
    if remaining <= 0:
        raise TimeoutError("live-observation total deadline expired")
    sock = connection.sock
    if sock is None and response is not None:
        sock = getattr(getattr(response.fp, "raw", None), "_sock", None)
    if sock is None:
        raise ConnectionError("live-observation response socket disappeared")
    sock.settimeout(max(0.001, remaining))

def fsync_dir(path):
    descriptor = os.open(path, os.O_RDONLY | getattr(os, "O_DIRECTORY", 0) | getattr(os, "O_NOFOLLOW", 0))
    try:
        os.fsync(descriptor)
    finally:
        os.close(descriptor)

def create(path, payload, mode=0o400):
    path.parent.mkdir(mode=0o700, parents=True, exist_ok=True)
    descriptor = os.open(path, os.O_WRONLY | os.O_CREAT | os.O_EXCL | getattr(os, "O_NOFOLLOW", 0), mode)
    with os.fdopen(descriptor, "wb") as handle:
        handle.write(payload)
        handle.flush()
        os.fsync(handle.fileno())
    fsync_dir(path.parent)

def canonical(value):
    return (json.dumps(value, sort_keys=True, separators=(",", ":")) + "\n").encode()

owner_payload = (
    f"schema=arc.recovery.live-observation-partial.v1 capture={capture_id} "
    f"node={node} freeze={freeze_sha}\n"
).encode()
if partial.exists() or partial.is_symlink():
    if partial.is_symlink() or not partial.is_dir():
        raise SystemExit("live-observation partial is not a real directory")
    owner = partial / ".arc-recovery-partial-owner"
    if (owner.is_symlink() or not owner.is_file() or owner.lstat().st_mode & 0o222
            or owner.read_bytes() != owner_payload):
        raise SystemExit("live-observation partial ownership differs")
else:
    partial.parent.mkdir(mode=0o700, parents=True, exist_ok=True)
    os.mkdir(partial, 0o700)
    create(partial / ".arc-recovery-partial-owner", owner_payload)
    fsync_dir(partial)
    fsync_dir(partial.parent)

for directory in (partial / "journal", partial / "observations", partial / "raw"):
    if directory.exists() or directory.is_symlink():
        if directory.is_symlink() or not directory.is_dir():
            raise SystemExit(f"unsafe live-observation directory: {directory.name}")
    else:
        os.mkdir(directory, 0o700)
        fsync_dir(partial)

# Fail closed on unexpected/symlink partial content rather than normalizing it.
allowed = {pathlib.PurePosixPath(".arc-recovery-partial-owner")}
for index, (slug, _) in enumerate(endpoints):
    allowed.update({
        pathlib.PurePosixPath(f"journal/{index:02d}-{slug}.attempt.json"),
        pathlib.PurePosixPath(f"observations/{index:02d}-{slug}.json"),
        pathlib.PurePosixPath(f"raw/{index:02d}-{slug}.body"),
    })
allowed.add(pathlib.PurePosixPath("receipt.json"))
allowed.add(pathlib.PurePosixPath("live-observations.files.sha256"))
allowed.add(pathlib.PurePosixPath("live-observations.complete"))
allowed_directories = {
    pathlib.PurePosixPath("journal"), pathlib.PurePosixPath("observations"),
    pathlib.PurePosixPath("raw"),
}
for base, dirs, files in os.walk(partial, followlinks=False):
    for name in dirs:
        path = pathlib.Path(base) / name
        rel = pathlib.PurePosixPath(path.relative_to(partial).as_posix())
        if path.is_symlink() or rel not in allowed_directories:
            raise SystemExit("unexpected/symlink directory is forbidden in live-observation partial")
    for name in files:
        path = pathlib.Path(base) / name
        rel = pathlib.PurePosixPath(path.relative_to(partial).as_posix())
        if rel not in allowed or path.is_symlink() or not stat.S_ISREG(path.lstat().st_mode):
            raise SystemExit(f"unexpected or unsafe live-observation partial member: {rel}")

receipt_path = partial / "receipt.json"
if receipt_path.exists() or receipt_path.is_symlink():
    if receipt_path.is_symlink() or not receipt_path.is_file() or receipt_path.lstat().st_mode & 0o222:
        raise SystemExit("unsafe live-observation receipt")
    value = json.loads(receipt_path.read_text(encoding="utf-8"))
    if value.get("schema") != "arc.recovery.legacy-live-observations.v1":
        raise SystemExit("existing live-observation receipt schema differs")
    raise SystemExit(0)

empty_sha = hashlib.sha256(b"").hexdigest()
rows = []
for index, (slug, endpoint) in enumerate(endpoints):
    attempt_path = partial / f"journal/{index:02d}-{slug}.attempt.json"
    result_path = partial / f"observations/{index:02d}-{slug}.json"
    raw_path = partial / f"raw/{index:02d}-{slug}.body"
    if result_path.exists() or result_path.is_symlink():
        if result_path.is_symlink() or not result_path.is_file() or result_path.lstat().st_mode & 0o222:
            raise SystemExit("unsafe live-observation result journal")
        result = json.loads(result_path.read_text(encoding="utf-8"))
        if result.get("endpoint") != endpoint:
            raise SystemExit("live-observation result endpoint differs")
        rows.append(result)
        continue

    created_attempt = False
    if attempt_path.exists() or attempt_path.is_symlink():
        if attempt_path.is_symlink() or not attempt_path.is_file() or attempt_path.lstat().st_mode & 0o222:
            raise SystemExit("unsafe live-observation attempt journal")
        attempt = json.loads(attempt_path.read_text(encoding="utf-8"))
        if set(attempt) != {"schema", "endpoint", "started_at", "node"} or attempt != {
            "schema": "arc.recovery.legacy-live-observation-attempt.v1",
            "endpoint": endpoint,
            "started_at": attempt["started_at"],
            "node": node,
        } or not isinstance(attempt["started_at"], str):
            raise SystemExit("live-observation attempt journal differs")
    else:
        attempt = {
            "schema": "arc.recovery.legacy-live-observation-attempt.v1",
            "endpoint": endpoint,
            "started_at": now(),
            "node": node,
        }
        create(attempt_path, canonical(attempt))
        created_attempt = True

    body = b""
    body_file = None
    status_code = None
    error = None
    raw_complete = False
    if not created_attempt:
        # A previous process crossed the durable request-intent boundary. Never
        # issue the request a second time; preserve any durable raw prefix.
        error = "interrupted_after_durable_attempt_intent"
        if raw_path.exists() or raw_path.is_symlink():
            if raw_path.is_symlink() or not raw_path.is_file() or raw_path.lstat().st_mode & 0o222:
                raise SystemExit("unsafe interrupted live-observation raw body")
            body = raw_path.read_bytes()
            if len(body) > max_body_bytes:
                raise SystemExit("interrupted live-observation raw body exceeds cap")
            body_file = raw_path.relative_to(partial).as_posix()
    else:
        connection = None
        chunks = []
        try:
            deadline = time.monotonic() + timeout_seconds
            connection = http.client.HTTPConnection("127.0.0.1", origin.port, timeout=timeout_seconds)
            connection.connect()
            enforce_deadline(connection, deadline)
            connection.request("GET", endpoint, headers={
                "Host": f"127.0.0.1:{origin.port}",
                "User-Agent": "arc-recovery-live-observation/1",
                "Accept": "application/json",
                "Connection": "close",
            })
            enforce_deadline(connection, deadline)
            response = connection.getresponse()
            status_code = response.status
            remaining = max_body_bytes
            exceeded = False
            while True:
                enforce_deadline(connection, deadline, response)
                chunk = response.read(min(1024 * 1024, remaining + 1))
                if not chunk:
                    raw_complete = True
                    break
                if len(chunk) > remaining:
                    chunks.append(chunk[:remaining])
                    exceeded = True
                    break
                chunks.append(chunk)
                remaining -= len(chunk)
                if response.length == 0 or response.isclosed():
                    raw_complete = True
                    break
                if remaining == 0:
                    extra = response.read(1)
                    if extra:
                        exceeded = True
                    else:
                        raw_complete = True
                    break
            body = b"".join(chunks)
            create(raw_path, body)
            body_file = raw_path.relative_to(partial).as_posix()
            if exceeded:
                error = "response_body_limit_exceeded"
                raw_complete = False
        except Exception as exc:
            error = f"{type(exc).__name__}:{str(exc)}"[:512]
            raw_complete = False
            body = b"".join(chunks)
            if status_code is not None:
                create(raw_path, body)
                body_file = raw_path.relative_to(partial).as_posix()
        finally:
            if connection is not None:
                connection.close()

    result = {
        "schema": "arc.recovery.legacy-live-observation.v1",
        "node": node,
        "endpoint": endpoint,
        "captured_at": now(),
        "http_status": status_code,
        "error": error,
        "raw_body_file": body_file,
        "raw_bytes": len(body),
        "raw_bytes_sha256": hashlib.sha256(body).hexdigest() if body else empty_sha,
        "raw_complete": raw_complete,
        "labels": labels,
    }
    create(result_path, canonical(result))
    rows.append(result)

receipt = {
    "schema": "arc.recovery.legacy-live-observations.v1",
    "capture_id": capture_id,
    "freeze_plan_sha256": freeze_sha,
    "node": node,
    "rpc_origin": rpc_origin,
    "created_at": min(json.loads((partial / f"journal/{index:02d}-{slug}.attempt.json").read_text())["started_at"] for index, (slug, _) in enumerate(endpoints)),
    "completed_at": now(),
    "labels": labels,
    "diagnostic": True,
    "canonical": False,
    "reward_evidence": False,
    "constraints": {
        "max_body_bytes": max_body_bytes,
        "timeout_seconds": timeout_seconds,
        "endpoints": [endpoint for _, endpoint in endpoints],
    },
    "observations": rows,
}
create(receipt_path, canonical(receipt))
fsync_dir(partial)
PY

    if [ ! -e "$partial/live-observations.files.sha256" ]; then
        write_tree_index "$partial" live-observations.files.sha256 live-observations.complete
    fi
    if [ ! -e "$partial/live-observations.complete" ]; then
        write_complete_marker "$partial" live-observations.files.sha256 live-observations.complete \
            arc.recovery.legacy-live-observations.v1 "capture_id=$capture_id" \
            "node=$node" "freeze_plan_sha256=$freeze_sha" \
            "labels=diagnostic,noncanonical,nonreward"
    fi
    verify_tree_index "$partial" live-observations.files.sha256 live-observations.complete
    if [ -e "$final" ] || [ -L "$final" ]; then
        die "live-observation final path appeared during create-only publication"
    fi
    mv -- "$partial" "$final"
    chmod -R a-w,go-rwx "$final"
    python3 - "$final" <<'PY'
import os, pathlib, sys
root = pathlib.Path(sys.argv[1])
for path in (root, root.parent):
    descriptor = os.open(path, os.O_RDONLY | getattr(os, "O_DIRECTORY", 0) | getattr(os, "O_NOFOLLOW", 0))
    try: os.fsync(descriptor)
    finally: os.close(descriptor)
PY
}

verify_live_observation_receipt() {
    local root="$1" capture_id="$2" node="$3" freeze_sha="$4"
    verify_tree_index "$root" live-observations.files.sha256 live-observations.complete
    python3 - "$root" "$capture_id" "$node" "$freeze_sha" <<'PY'
import hashlib
import json
import pathlib
import re
import stat
import sys

root = pathlib.Path(sys.argv[1])
capture_id, node, freeze_sha = sys.argv[2:]
allowed_directories = {"journal", "observations", "raw"}
for path in (root, *root.rglob("*")):
    if path.lstat().st_mode & 0o222:
        raise SystemExit(f"live-observation tree member remains writable: {path.relative_to(root) if path != root else '.'}")
    if path != root and path.is_dir() and path.relative_to(root).as_posix() not in allowed_directories:
        raise SystemExit("live-observation tree has an unexpected directory")
receipt_path = root / "receipt.json"
if receipt_path.is_symlink() or not receipt_path.is_file():
    raise SystemExit("live-observation receipt is missing or unsafe")
value = json.loads(receipt_path.read_text(encoding="utf-8"))
canonical = (json.dumps(value, sort_keys=True, separators=(",", ":")) + "\n").encode()
if receipt_path.read_bytes() != canonical:
    raise SystemExit("live-observation receipt is not canonical JSON")
expected_keys = {
    "schema", "capture_id", "freeze_plan_sha256", "node", "rpc_origin",
    "created_at", "completed_at", "labels", "diagnostic", "canonical",
    "reward_evidence", "constraints", "observations",
}
endpoints = ["/inference/results", "/workers/scoreboard", "/inference/attestations"]
labels = ["diagnostic", "noncanonical", "nonreward"]
timestamp_re = re.compile(r"[0-9]{4}-[0-9]{2}-[0-9]{2}T[0-9]{2}:[0-9]{2}:[0-9]{2}\.[0-9]{6}Z")
if set(value) != expected_keys or value.get("schema") != "arc.recovery.legacy-live-observations.v1":
    raise SystemExit("live-observation receipt fields/schema differ")
if (value["capture_id"], value["node"], value["freeze_plan_sha256"]) != (capture_id, node, freeze_sha):
    raise SystemExit("live-observation receipt identity differs")
if not re.fullmatch(r"http://127\.0\.0\.1:[1-9][0-9]{0,4}", value["rpc_origin"]):
    raise SystemExit("live-observation receipt origin is not loopback")
if (value["labels"] != labels or value["diagnostic"] is not True
        or value["canonical"] is not False or value["reward_evidence"] is not False):
    raise SystemExit("live-observation diagnostic/noncanonical/nonreward labels differ")
if not timestamp_re.fullmatch(value.get("created_at", "")) or not timestamp_re.fullmatch(value.get("completed_at", "")):
    raise SystemExit("live-observation receipt timestamp is malformed")
if value["constraints"] != {
    "max_body_bytes": 8 * 1024 * 1024,
    "timeout_seconds": 20,
    "endpoints": endpoints,
}:
    raise SystemExit("live-observation bounds/endpoints differ")
rows = value["observations"]
if not isinstance(rows, list) or [row.get("endpoint") for row in rows] != endpoints:
    raise SystemExit("live-observation receipt does not contain exactly the reviewed endpoints")
for index, row in enumerate(rows):
    if set(row) != {
        "schema", "node", "endpoint", "captured_at", "http_status", "error",
        "raw_body_file", "raw_bytes", "raw_bytes_sha256", "raw_complete", "labels",
    } or row["schema"] != "arc.recovery.legacy-live-observation.v1" or row["node"] != node or row["labels"] != labels:
        raise SystemExit("live-observation row fields/labels differ")
    if (not timestamp_re.fullmatch(row.get("captured_at", ""))
            or row["error"] is not None and not isinstance(row["error"], str)
            or not isinstance(row["raw_complete"], bool)):
        raise SystemExit("live-observation row timestamp/error/completion state is malformed")
    if row["http_status"] is not None and (isinstance(row["http_status"], bool) or not 100 <= row["http_status"] <= 599):
        raise SystemExit("live-observation HTTP status is malformed")
    if isinstance(row["raw_bytes"], bool) or not isinstance(row["raw_bytes"], int) or not 0 <= row["raw_bytes"] <= 8 * 1024 * 1024:
        raise SystemExit("live-observation body size exceeds its bound")
    if not isinstance(row["raw_bytes_sha256"], str) or not re.fullmatch(r"[0-9a-f]{64}", row["raw_bytes_sha256"]):
        raise SystemExit("live-observation raw hash is malformed")
    body_name = row["raw_body_file"]
    slug = "inference-results" if index == 0 else "workers-scoreboard" if index == 1 else "inference-attestations"
    result_path = root / f"observations/{index:02d}-{slug}.json"
    if result_path.is_symlink() or not result_path.is_file():
        raise SystemExit("live-observation result journal is missing or unsafe")
    result_value = json.loads(result_path.read_text(encoding="utf-8"))
    if result_value != row or result_path.read_bytes() != (json.dumps(result_value, sort_keys=True, separators=(",", ":")) + "\n").encode():
        raise SystemExit("live-observation result journal differs from the receipt")
    attempt_path = root / f"journal/{index:02d}-{slug}.attempt.json"
    if attempt_path.is_symlink() or not attempt_path.is_file():
        raise SystemExit("live-observation attempt journal is missing or unsafe")
    attempt = json.loads(attempt_path.read_text(encoding="utf-8"))
    if (set(attempt) != {"schema", "endpoint", "started_at", "node"}
            or attempt.get("schema") != "arc.recovery.legacy-live-observation-attempt.v1"
            or attempt.get("endpoint") != endpoints[index] or attempt.get("node") != node
            or not timestamp_re.fullmatch(attempt.get("started_at", ""))
            or attempt_path.read_bytes() != (json.dumps(attempt, sort_keys=True, separators=(",", ":")) + "\n").encode()):
        raise SystemExit("live-observation attempt journal is malformed")
    if body_name is None:
        if row["raw_bytes"] != 0 or row["raw_bytes_sha256"] != hashlib.sha256(b"").hexdigest():
            raise SystemExit("bodyless live-observation row has nonempty raw identity")
    else:
        expected = f"raw/{index:02d}-{slug}.body"
        if body_name != expected:
            raise SystemExit("live-observation raw body path is noncanonical")
        body_path = root / pathlib.PurePosixPath(body_name)
        if body_path.is_symlink() or not stat.S_ISREG(body_path.lstat().st_mode):
            raise SystemExit("live-observation raw body is unsafe")
        body = body_path.read_bytes()
        if len(body) != row["raw_bytes"] or hashlib.sha256(body).hexdigest() != row["raw_bytes_sha256"]:
            raise SystemExit("live-observation raw body identity differs")
marker = (root / "live-observations.complete").read_text(encoding="utf-8")
for exact in (
    "schema=arc.recovery.legacy-live-observations.v1\n",
    f"capture_id={capture_id}\n", f"node={node}\n",
    f"freeze_plan_sha256={freeze_sha}\n",
    "labels=diagnostic,noncanonical,nonreward\n",
):
    if exact not in marker:
        raise SystemExit("live-observation completion marker identity differs")
PY
}

live_observations_status() {
    local capture_id="$1" node="$2" freeze_sha="$3"
    require_hash "$capture_id" "capture id"
    require_node "$node"
    require_hash "$freeze_sha" "freeze plan hash"
    local root="$LIVE_OBSERVATION_BASE/$capture_id/$node"
    verify_live_observation_receipt "$root" "$capture_id" "$node" "$freeze_sha"
    python3 - "$root" "$capture_id" "$node" "$freeze_sha" <<'PY'
import hashlib, json, pathlib, sys
root = pathlib.Path(sys.argv[1])
value = {
    "schema": "arc.recovery.legacy-live-observations-status.v1",
    "capture_id": sys.argv[2], "node": sys.argv[3], "freeze_plan_sha256": sys.argv[4],
    "root_sha256": hashlib.sha256((root / "live-observations.files.sha256").read_bytes()).hexdigest(),
    "receipt_sha256": hashlib.sha256((root / "receipt.json").read_bytes()).hexdigest(),
    "labels": ["diagnostic", "noncanonical", "nonreward"],
}
print(json.dumps(value, sort_keys=True, separators=(",", ":")))
PY
}

live_observations_eligible() {
    local capture_id="$1" node="$2" freeze_sha="$3" writer_pid="$4" start_ticks="$5" boot_id="$6"
    require_hash "$capture_id" "capture id"
    require_node "$node"
    require_hash "$freeze_sha" "freeze plan hash"
    require_uint "$writer_pid" "writer pid"
    require_uint "$start_ticks" "writer start ticks"
    require_commands python3
    printf '%s\n' "$boot_id" | grep -Eq '^[0-9a-f-]{36}$' || die "boot id is malformed"
    [ ! -e "$STOP_BASE/$capture_id/$node" ] && [ ! -L "$STOP_BASE/$capture_id/$node" ] || \
        die "live-observation fleet is ineligible because a writer is already stopped/fenced"
    [ ! -e "$STOP_BASE/$capture_id/.${node}.stop.partial" ] && \
        [ ! -L "$STOP_BASE/$capture_id/.${node}.stop.partial" ] || \
        die "live-observation fleet is ineligible because a stop/fence transaction began"
    [ -f /etc/arc-recovery/legacy-start-allowed ] && \
        [ ! -L /etc/arc-recovery/legacy-start-allowed ] || \
        die "live-observation fleet is ineligible because the restart fence is armed"
    python3 - "$writer_pid" "$start_ticks" "$boot_id" <<'PY'
import pathlib, sys
pid, start, boot = int(sys.argv[1]), int(sys.argv[2]), sys.argv[3]
if pathlib.Path("/proc/sys/kernel/random/boot_id").read_text().strip() != boot:
    raise SystemExit("sealed writer boot changed before fleet live-observation capture")
proc = pathlib.Path(f"/proc/{pid}")
if not proc.is_dir() or (proc / "comm").read_text().strip() != "arc-node":
    raise SystemExit("sealed writer is not live before fleet live-observation capture")
fields = (proc / "stat").read_text().rsplit(")", 1)[1].split()
if int(fields[19]) != start:
    raise SystemExit("sealed writer start time changed before fleet live-observation capture")
writers = sorted(int(path.parent.name) for path in pathlib.Path("/proc").glob("[0-9]*/comm")
                 if path.read_text(errors="replace").strip() == "arc-node")
if writers != [pid]:
    raise SystemExit("host does not contain exactly its sealed writer")
PY
    printf '{"capture_id":"%s","node":"%s","eligible":true}\n' "$capture_id" "$node"
}

legacy_height_bracket() {
    [ "$#" -eq 15 ] || die "legacy-height-bracket requires exactly 15 arguments"
    local capture_id="$1" node="$2" freeze_sha="$3" writer_pid="$4" start_ticks="$5"
    local boot_id="$6" executable_path="$7" executable_sha="$8" argv_sha="$9"
    local rpc_origin="${10}" public_before="${11}" public_latest="${12}"
    local public_after="${13}" public_hash="${14}" challenge="${15}"
    require_hash "$capture_id" "capture id"
    require_node "$node"
    require_hash "$freeze_sha" "freeze plan hash"
    require_uint "$writer_pid" "writer pid"
    require_uint "$start_ticks" "writer start ticks"
    require_safe_absolute_path "$executable_path" "writer executable path"
    require_hash "$executable_sha" "writer executable hash"
    require_hash "$argv_sha" "writer argv hash"
    require_uint "$public_before" "public before height"
    require_uint "$public_latest" "public latest height"
    require_uint "$public_after" "public after height"
    require_hash "$public_hash" "public latest block hash"
    require_hash "$challenge" "legacy-height challenge"
    printf '%s\n' "$rpc_origin" | grep -Eq '^http://127\.0\.0\.1:[1-9][0-9]{0,4}$' || \
        die "legacy-height RPC origin is not exact loopback HTTP"
    python3 -I - "$capture_id" "$node" "$freeze_sha" "$writer_pid" "$start_ticks" \
        "$boot_id" "$executable_path" "$executable_sha" "$argv_sha" "$rpc_origin" \
        "$public_before" "$public_latest" "$public_after" "$public_hash" "$challenge" <<'PY'
import datetime
import hashlib
import http.client
import json
import os
import pathlib
import re
import stat
import sys
import urllib.parse

(capture_id, node, freeze_sha, pid_raw, start_raw, boot_id, executable,
 executable_sha, argv_sha, rpc_origin, public_before_raw, public_latest_raw,
 public_after_raw, public_hash, challenge) = sys.argv[1:]
pid = int(pid_raw); start = int(start_raw)
public_before = int(public_before_raw); public_latest = int(public_latest_raw)
public_after = int(public_after_raw)
canonical = lambda value: (json.dumps(value, sort_keys=True, separators=(",", ":")) + "\n").encode()
digest_bytes = lambda value: hashlib.sha256(value).hexdigest()
hash_re = re.compile(r"[0-9a-f]{64}")
if not public_before <= public_latest <= public_after:
    raise SystemExit("public legacy-height row is internally inconsistent")
if any(hash_re.fullmatch(value) is None for value in (capture_id, freeze_sha, public_hash, challenge)):
    raise SystemExit("legacy-height bracket hash input is malformed")
expected_capture = hashlib.sha256(b"ARC recovery capture v2\0" + bytes.fromhex(freeze_sha)).hexdigest()
if capture_id != expected_capture:
    raise SystemExit("legacy-height bracket capture id differs")
plan_path = pathlib.Path(f"/root/.arc-recovery-plans/{freeze_sha}/freeze.lock.json")
plan_raw = plan_path.read_bytes()
if plan_path.is_symlink() or digest_bytes(plan_raw) != freeze_sha:
    raise SystemExit("legacy-height bracket freeze plan is missing or changed")
plan = json.loads(plan_raw)
rows = [row for row in plan.get("nodes", []) if row.get("name") == node]
if len(rows) != 1:
    raise SystemExit("legacy-height bracket node is missing or ambiguous")
frozen = rows[0]
expected_identity = {
    "writer_pid": pid, "writer_start_ticks": start, "boot_id": boot_id,
    "executable_path": executable, "executable_sha256": executable_sha,
    "argv_sha256": argv_sha, "rpc_origin": rpc_origin,
}
if any(frozen.get(field) != wanted for field, wanted in expected_identity.items()):
    raise SystemExit("legacy-height bracket writer identity differs from freeze plan")

root = pathlib.Path(f"/root/arc-recovery-height-brackets/{capture_id}/{node}")
output = root / f"{challenge}.json"
def fsync_dir(path):
    descriptor = os.open(path, os.O_RDONLY | getattr(os, "O_DIRECTORY", 0) | getattr(os, "O_NOFOLLOW", 0))
    try: os.fsync(descriptor)
    finally: os.close(descriptor)

if output.exists() or output.is_symlink():
    details = output.lstat(); raw = output.read_bytes(); value = json.loads(raw)
    if (output.is_symlink() or not stat.S_ISREG(details.st_mode)
            or stat.S_IMODE(details.st_mode) != 0o400 or details.st_uid != 0
            or details.st_gid != 0 or details.st_nlink != 1 or raw != canonical(value)):
        raise SystemExit("existing legacy-height bracket proof is unsafe")
    expected = {
        "capture_id": capture_id, "node": node, "freeze_plan_sha256": freeze_sha,
        "challenge": challenge, "rpc_origin": rpc_origin,
        "public_info_before_height": public_before,
        "public_latest_block_height": public_latest,
        "public_info_after_height": public_after,
        "public_latest_block_hash": public_hash,
    }
    if value.get("schema") != "arc.recovery.authenticated-legacy-height-bracket.v1" or any(value.get(key) != wanted for key, wanted in expected.items()):
        raise SystemExit("existing legacy-height bracket proof differs")
    sys.stdout.buffer.write(raw)
    raise SystemExit(0)

stop_root = pathlib.Path(f"/root/arc-recovery-stops/{capture_id}")
if stop_root.exists() or stop_root.is_symlink():
    raise SystemExit("refusing first legacy-height bracket after a stop transaction began")

def file_digest(path):
    result = hashlib.sha256()
    with open(path, "rb") as handle:
        for chunk in iter(lambda: handle.read(1024 * 1024), b""):
            result.update(chunk)
    return result.hexdigest()

def validate_writer():
    if pathlib.Path("/proc/sys/kernel/random/boot_id").read_text().strip() != boot_id:
        raise SystemExit("legacy-height bracket writer boot changed")
    proc = pathlib.Path(f"/proc/{pid}")
    if not proc.is_dir() or proc.joinpath("comm").read_text().strip() != "arc-node":
        raise SystemExit("legacy-height bracket exact writer is not live")
    fields = proc.joinpath("stat").read_text().rsplit(")", 1)[1].split()
    if int(fields[19]) != start or os.readlink(proc / "exe") != executable:
        raise SystemExit("legacy-height bracket writer PID/start/executable changed")
    if file_digest(proc / "exe") != executable_sha or file_digest(proc / "cmdline") != argv_sha:
        raise SystemExit("legacy-height bracket writer executable/argv changed")
    writers = sorted(int(path.parent.name) for path in pathlib.Path("/proc").glob("[0-9]*/comm")
                     if path.read_text(errors="replace").strip() == "arc-node")
    if writers != [pid]:
        raise SystemExit("legacy-height bracket host writer set is ambiguous")

origin = urllib.parse.urlsplit(rpc_origin)
if (origin.scheme, origin.hostname, origin.path, origin.query, origin.fragment) != ("http", "127.0.0.1", "", "", "") or origin.port is None:
    raise SystemExit("legacy-height bracket origin differs")
def fetch(path):
    connection = http.client.HTTPConnection("127.0.0.1", origin.port, timeout=10)
    try:
        connection.request("GET", path, headers={"Accept": "application/json", "Connection": "close"})
        response = connection.getresponse()
        if response.status != 200:
            raise SystemExit(f"legacy-height bracket {path} returned HTTP {response.status}")
        raw = response.read(1024 * 1024 + 1)
        if len(raw) > 1024 * 1024:
            raise SystemExit("legacy-height bracket body exceeded 1 MiB")
    finally:
        connection.close()
    try: value = json.loads(raw)
    except (UnicodeDecodeError, json.JSONDecodeError):
        raise SystemExit("legacy-height bracket body is invalid JSON")
    return value, digest_bytes(raw)
def height(value):
    answer = value.get("block_height")
    if isinstance(answer, bool) or not isinstance(answer, int) or answer < 0:
        raise SystemExit("legacy-height bracket /info height is invalid")
    return answer

validate_writer()
started_at = datetime.datetime.now(datetime.timezone.utc).strftime("%Y-%m-%dT%H:%M:%SZ")
before, before_sha = fetch("/info")
latest, latest_sha = fetch("/block/latest")
after, after_sha = fetch("/info")
validate_writer()
before_height = height(before); after_height = height(after)
header = latest.get("header") if isinstance(latest, dict) else None
if not isinstance(header, dict):
    raise SystemExit("legacy-height bracket latest block has no header")
latest_height = header.get("height", latest.get("height"))
latest_hash = latest.get("hash", header.get("hash"))
if (isinstance(latest_height, bool) or not isinstance(latest_height, int)
        or latest_height < 0 or hash_re.fullmatch(str(latest_hash)) is None
        or not before_height <= latest_height <= after_height):
    raise SystemExit("legacy-height bracket observations are inconsistent")
if public_latest == latest_height and public_hash != latest_hash:
    raise SystemExit("public and authenticated latest block hashes disagree at the same height")
completed_at = datetime.datetime.now(datetime.timezone.utc).strftime("%Y-%m-%dT%H:%M:%SZ")
value = {
    "schema": "arc.recovery.authenticated-legacy-height-bracket.v1",
    "capture_id": capture_id, "node": node, "freeze_plan_sha256": freeze_sha,
    "challenge": challenge, "rpc_origin": rpc_origin,
    "writer_pid": pid, "writer_start_ticks": start, "boot_id": boot_id,
    "executable_sha256": executable_sha, "argv_sha256": argv_sha,
    "started_at": started_at, "completed_at": completed_at,
    "public_info_before_height": public_before,
    "public_latest_block_height": public_latest,
    "public_info_after_height": public_after,
    "public_latest_block_hash": public_hash,
    "authenticated_info_before_height": before_height,
    "authenticated_latest_block_height": latest_height,
    "authenticated_info_after_height": after_height,
    "authenticated_latest_block_hash": latest_hash,
    "authenticated_info_before_body_sha256": before_sha,
    "authenticated_latest_block_body_sha256": latest_sha,
    "authenticated_info_after_body_sha256": after_sha,
    "conservative_height_floor": max(public_after, after_height),
}
raw = canonical(value)
root.mkdir(mode=0o700, parents=True, exist_ok=True)
for directory in (root.parent.parent, root.parent, root):
    details = directory.lstat()
    if directory.is_symlink() or not stat.S_ISDIR(details.st_mode) or details.st_uid != 0 or details.st_gid != 0:
        raise SystemExit("legacy-height bracket proof directory is unsafe")
descriptor = os.open(output, os.O_WRONLY | os.O_CREAT | os.O_EXCL | getattr(os, "O_NOFOLLOW", 0), 0o400)
with os.fdopen(descriptor, "wb") as handle:
    handle.write(raw); handle.flush(); os.fsync(handle.fileno())
fsync_dir(root)
sys.stdout.buffer.write(raw)
PY
}

capture_live_observations() {
    local capture_id="$1" node="$2" freeze_sha="$3" writer_pid="$4" start_ticks="$5"
    local boot_id="$6" executable_path="$7" executable_sha="$8" argv_sha="$9" rpc_origin="${10}"
    require_hash "$capture_id" "capture id"
    require_node "$node"
    require_hash "$freeze_sha" "freeze plan hash"
    require_uint "$writer_pid" "writer pid"
    require_uint "$start_ticks" "writer start ticks"
    require_safe_absolute_path "$executable_path" "writer executable path"
    require_hash "$executable_sha" "writer executable hash"
    require_hash "$argv_sha" "writer argv hash"
    printf '%s\n' "$boot_id" | grep -Eq '^[0-9a-f-]{36}$' || die "boot id is malformed"
    printf '%s\n' "$rpc_origin" | grep -Eq '^http://127\.0\.0\.1:[1-9][0-9]{0,4}$' || \
        die "RPC origin is not an exact loopback HTTP origin"
    require_commands python3 flock chmod mv

    local parent="$LIVE_OBSERVATION_BASE/$capture_id"
    local root="$parent/$node" partial="$parent/.${node}.live-observations.partial"
    if [ -e "$LIVE_OBSERVATION_BASE" ] || [ -L "$LIVE_OBSERVATION_BASE" ]; then
        [ -d "$LIVE_OBSERVATION_BASE" ] && [ ! -L "$LIVE_OBSERVATION_BASE" ] || \
            die "global live-observation root is unsafe"
    else
        mkdir -- "$LIVE_OBSERVATION_BASE"
    fi
    [ -d "$LIVE_OBSERVATION_BASE" ] && [ ! -L "$LIVE_OBSERVATION_BASE" ] || \
        die "global live-observation root is unsafe"
    if [ -e "$parent" ] || [ -L "$parent" ]; then
        [ -d "$parent" ] && [ ! -L "$parent" ] || die "live-observation parent is unsafe"
    else
        mkdir -- "$parent"
    fi
    [ -d "$parent" ] && [ ! -L "$parent" ] || die "live-observation parent is unsafe"
    local lock_path="$parent/.${node}.live-observations.lock"
    if [ -e "$lock_path" ] || [ -L "$lock_path" ]; then
        [ -f "$lock_path" ] && [ ! -L "$lock_path" ] || die "live-observation lock is unsafe"
    fi
    : >> "$lock_path"
    chmod 600 -- "$lock_path"
    exec 6>> "$lock_path"
    flock -x 6

    if [ -e "$root" ] || [ -L "$root" ]; then
        verify_live_observation_receipt "$root" "$capture_id" "$node" "$freeze_sha"
        live_observations_status "$capture_id" "$node" "$freeze_sha"
        return 0
    fi
    [ ! -e "$STOP_BASE/$capture_id/$node" ] && [ ! -L "$STOP_BASE/$capture_id/$node" ] || \
        die "refusing first live-observation receipt after this writer was stopped/fenced"
    [ ! -e "$STOP_BASE/$capture_id/.${node}.stop.partial" ] && \
        [ ! -L "$STOP_BASE/$capture_id/.${node}.stop.partial" ] || \
        die "refusing live-observation capture after a stop/fence transaction began"
    [ -f /etc/arc-recovery/legacy-start-allowed ] && \
        [ ! -L /etc/arc-recovery/legacy-start-allowed ] || \
        die "refusing live-observation capture after the persistent restart fence was armed"

    python3 - "$capture_id" "$node" "$freeze_sha" "$writer_pid" "$start_ticks" \
        "$boot_id" "$executable_path" "$executable_sha" "$argv_sha" "$rpc_origin" <<'PY'
import hashlib, json, os, pathlib, sys
(capture_id, node, freeze_sha, pid_raw, start_raw, boot_id, executable,
 executable_sha, argv_sha, rpc_origin) = sys.argv[1:]
pid = int(pid_raw); start = int(start_raw)
plan = pathlib.Path(f"/root/.arc-recovery-plans/{freeze_sha}/freeze.lock.json")
if plan.is_symlink() or not plan.is_file() or hashlib.sha256(plan.read_bytes()).hexdigest() != freeze_sha:
    raise SystemExit("pinned freeze plan is missing, unsafe, or changed")
expected_capture = hashlib.sha256(b"ARC recovery capture v2\0" + bytes.fromhex(freeze_sha)).hexdigest()
if capture_id != expected_capture:
    raise SystemExit("capture id does not derive from the pinned freeze plan")
value = json.loads(plan.read_text(encoding="utf-8"))
rows = [row for row in value.get("nodes", []) if row.get("name") == node]
if len(rows) != 1:
    raise SystemExit("pinned freeze plan node is missing or ambiguous")
row = rows[0]
expected = {
    "writer_pid": pid, "writer_start_ticks": start, "boot_id": boot_id,
    "executable_path": executable, "executable_sha256": executable_sha,
    "argv_sha256": argv_sha, "rpc_origin": rpc_origin,
}
for field, wanted in expected.items():
    if row.get(field) != wanted:
        raise SystemExit(f"live-observation writer field differs from freeze plan: {field}")
if pathlib.Path("/proc/sys/kernel/random/boot_id").read_text().strip() != boot_id:
    raise SystemExit("writer boot changed before live-observation capture")
proc = pathlib.Path(f"/proc/{pid}")
if not proc.is_dir() or (proc / "comm").read_text().strip() != "arc-node":
    raise SystemExit("exact writer is not live before live-observation capture")
fields = (proc / "stat").read_text().rsplit(")", 1)[1].split()
if int(fields[19]) != start:
    raise SystemExit("writer start time changed before live-observation capture")
writers = sorted(int(path.parent.name) for path in pathlib.Path("/proc").glob("[0-9]*/comm")
                 if path.read_text(errors="replace").strip() == "arc-node")
if writers != [pid]:
    raise SystemExit("live-observation host does not have exactly the sealed writer")
if os.readlink(proc / "exe") != executable:
    raise SystemExit("writer executable path changed before live-observation capture")
def digest(path):
    result = hashlib.sha256()
    with open(path, "rb") as handle:
        for chunk in iter(lambda: handle.read(1024 * 1024), b""):
            result.update(chunk)
    return result.hexdigest()
if digest(proc / "exe") != executable_sha or digest(proc / "cmdline") != argv_sha:
    raise SystemExit("writer executable/argv changed before live-observation capture")
PY
    capture_live_observation_receipt_at "$partial" "$root" "$capture_id" "$node" "$freeze_sha" "$rpc_origin"
    verify_live_observation_receipt "$root" "$capture_id" "$node" "$freeze_sha"
    live_observations_status "$capture_id" "$node" "$freeze_sha"
}

verify_merged_legacy_fence_config() {
    local unit="$1"
    python3 - "$unit" <<'PY'
import subprocess
import sys

unit = sys.argv[1]
merged = subprocess.check_output(["systemctl", "cat", unit], text=True)
scalars = {}
conditions = []
section = None
for raw in merged.splitlines():
    line = raw.strip()
    if not line or line.startswith(("#", ";")):
        continue
    if line.startswith("[") and line.endswith("]"):
        section = line[1:-1].strip()
        continue
    if "=" not in line:
        continue
    key, value = (item.strip() for item in line.split("=", 1))
    if ((section == "Unit" and key in {
                "RefuseManualStart", "RefuseManualStop", "StopWhenUnneeded",
                "BindsTo", "PartOf", "PropagatesStopTo",
            })
            or (section == "Service" and key in {
                "ExecReload", "Restart", "KillMode", "SendSIGKILL", "OOMPolicy",
                "WatchdogSec", "RuntimeMaxSec",
            })):
        scalars[key] = value
    if section == "Unit" and key == "ConditionPathExists":
        if value == "":
            conditions.clear()
        else:
            conditions.append(value)
if scalars != {
    "ExecReload": "", "Restart": "no", "RefuseManualStart": "yes", "RefuseManualStop": "yes",
    "KillMode": "process", "SendSIGKILL": "no",
    "StopWhenUnneeded": "no", "BindsTo": "", "PartOf": "", "PropagatesStopTo": "",
    "OOMPolicy": "continue", "WatchdogSec": "0", "RuntimeMaxSec": "infinity",
}:
    raise SystemExit(f"merged systemd fence assignments differ for {unit}: {scalars}")
if "/etc/arc-recovery/legacy-start-allowed" not in conditions:
    raise SystemExit(f"merged systemd activation condition was reset for {unit}")
PY
}

verify_merged_legacy_timer_fence_config() {
    local unit="$1"
    python3 - "$unit" <<'PY'
import subprocess
import sys

unit = sys.argv[1]
merged = subprocess.check_output(["systemctl", "cat", unit], text=True)
scalars = {}
conditions = []
section = None
for raw in merged.splitlines():
    line = raw.strip()
    if not line or line.startswith(("#", ";")):
        continue
    if line.startswith("[") and line.endswith("]"):
        section = line[1:-1].strip(); continue
    if "=" not in line:
        continue
    key, value = (item.strip() for item in line.split("=", 1))
    if section == "Unit" and key in {
        "RefuseManualStart", "RefuseManualStop", "StopWhenUnneeded",
        "BindsTo", "PartOf", "PropagatesStopTo",
    }:
        scalars[key] = value
    if section == "Unit" and key == "ConditionPathExists":
        if value == "": conditions.clear()
        else: conditions.append(value)
if scalars != {
    "RefuseManualStart": "yes", "RefuseManualStop": "yes",
    "StopWhenUnneeded": "no", "BindsTo": "", "PartOf": "", "PropagatesStopTo": "",
}:
    raise SystemExit(f"merged timer fence assignments differ for {unit}: {scalars}")
if "/etc/arc-recovery/legacy-start-allowed" not in conditions:
    raise SystemExit(f"merged timer activation condition was reset for {unit}")
PY
}

verify_merged_legacy_start_barrier_config() {
    local unit="$1"
    python3 - "$unit" <<'PY'
import subprocess
import sys

unit = sys.argv[1]
conditions = []
section = None
for raw in subprocess.check_output(["systemctl", "cat", unit], text=True).splitlines():
    line = raw.strip()
    if not line or line.startswith(("#", ";")): continue
    if line.startswith("[") and line.endswith("]"):
        section = line[1:-1].strip(); continue
    if section != "Unit" or "=" not in line: continue
    key, value = (item.strip() for item in line.split("=", 1))
    if key == "ConditionPathExists":
        if value == "": conditions.clear()
        else: conditions.append(value)
if "/etc/arc-recovery/legacy-start-allowed" not in conditions:
    raise SystemExit(f"merged start barrier condition is absent for {unit}")
PY
}

unit_enablement_state() {
    local unit="$1" state
    state="$(systemctl is-enabled "$unit" 2>/dev/null || true)"
    case "$state" in
        disabled|masked|masked-runtime|static|indirect|generated|transient|not-found) ;;
        *) die "legacy unit has a non-terminal enablement state: $unit state=${state:-unknown}" ;;
    esac
    printf '%s\n' "$state"
}

disable_and_verify_unit() {
    local unit="$1"
    # Static/indirect units can make `systemctl disable` return nonzero even
    # though no enablement symlink exists. Treat the command as best effort,
    # then accept only an explicit, reviewed terminal state for this unit.
    systemctl disable "$unit" >/dev/null 2>&1 || true
    unit_enablement_state "$unit" >/dev/null
}

verify_closed_dropin_directory() {
    local unit="$1"
    python3 - "/etc/systemd/system/$unit.d" <<'PY'
import pathlib
import stat
import sys

root = pathlib.Path(sys.argv[1])
details = root.lstat()
if (root.is_symlink() or not root.is_dir() or details.st_uid != 0
        or details.st_gid != 0 or details.st_mode & 0o022):
    raise SystemExit("systemd drop-in directory is unsafe")
entries = []
for path in root.glob("*.conf"):
    mode = path.lstat().st_mode
    if (path.is_symlink() or not stat.S_ISREG(mode)
            or path.lstat().st_uid != 0 or path.lstat().st_gid != 0):
        raise SystemExit(f"systemd drop-in entry is non-regular: {path}")
    entries.append(path.name)
if not entries or sorted(entries)[-1] != "zzzz-arc-recovery-freeze.conf":
    raise SystemExit("recovery fence is not the lexically final systemd drop-in")
PY
}

persist_legacy_restart_fence_files() {
    local unit last_dropin
    if [ "$#" -eq 0 ]; then
        set -- arc-self-heal.service arc-node.service arc-node-update.service arc-node-update.timer
    fi
    # Gate state is owned by the caller. The same exact files are first staged
    # fail-open behind a durable allow marker and later become fail-closed when
    # the capture transaction atomically removes that marker.
    for unit in "$@"; do
        case "$unit" in
            arc-self-heal.service|arc-node.service|arc-node-update.service|arc-node-update.timer) ;;
            *) die "refusing an unreviewed legacy fence target: $unit" ;;
        esac
        python3 - "$unit" <<'PY'
import os
import pathlib
import secrets
import stat
import sys

unit = sys.argv[1]
dropin_name = f"{unit}.d"
fence_name = "zzzz-arc-recovery-freeze.conf"
expected = b"[Unit]\nConditionPathExists=/etc/arc-recovery/legacy-start-allowed\n"
flags = os.O_RDONLY | getattr(os, "O_DIRECTORY", 0) | getattr(os, "O_NOFOLLOW", 0)
systemd = os.open("/etc/systemd/system", flags)
try:
    details = os.fstat(systemd)
    if (not stat.S_ISDIR(details.st_mode) or details.st_uid != 0 or details.st_gid != 0
            or details.st_mode & 0o022):
        raise SystemExit("systemd unit directory is unsafe")
    try:
        child_details = os.stat(dropin_name, dir_fd=systemd, follow_symlinks=False)
    except FileNotFoundError:
        os.mkdir(dropin_name, 0o755, dir_fd=systemd)
        child_details = os.stat(dropin_name, dir_fd=systemd, follow_symlinks=False)
    if (not stat.S_ISDIR(child_details.st_mode) or child_details.st_uid != 0
            or child_details.st_gid != 0 or child_details.st_mode & 0o022):
        raise SystemExit("systemd drop-in directory is unsafe")
    child = os.open(dropin_name, flags, dir_fd=systemd)
    try:
        current = None
        try:
            current = os.stat(fence_name, dir_fd=child, follow_symlinks=False)
        except FileNotFoundError:
            pass
        if current is None:
            temporary = f".{fence_name}.partial.{secrets.token_hex(8)}"
            descriptor = os.open(
                temporary,
                os.O_WRONLY | os.O_CREAT | os.O_EXCL | getattr(os, "O_NOFOLLOW", 0),
                0o444,
                dir_fd=child,
            )
            try:
                os.write(descriptor, expected)
                os.fchmod(descriptor, 0o444)
                os.fsync(descriptor)
            except Exception:
                os.close(descriptor)
                os.unlink(temporary, dir_fd=child)
                raise
            else:
                os.close(descriptor)
            os.rename(temporary, fence_name, src_dir_fd=child, dst_dir_fd=child)
            current = os.stat(fence_name, dir_fd=child, follow_symlinks=False)
        if (not stat.S_ISREG(current.st_mode) or current.st_uid != 0 or current.st_gid != 0
                or current.st_mode & 0o222):
            raise SystemExit("persistent legacy restart fence inode is unsafe")
        descriptor = os.open(fence_name, os.O_RDONLY | getattr(os, "O_NOFOLLOW", 0), dir_fd=child)
        try:
            if os.read(descriptor, len(expected) + 1) != expected:
                raise SystemExit("persistent legacy restart fence bytes differ")
            os.fsync(descriptor)
        finally:
            os.close(descriptor)
        os.fsync(child)
    finally:
        os.close(child)
    os.fsync(systemd)
finally:
    os.close(systemd)
PY
        last_dropin="$(find "/etc/systemd/system/$unit.d" -maxdepth 1 -type f -name '*.conf' \
            -printf '%f\n' | LC_ALL=C sort | tail -n 1)"
        [ "$last_dropin" = zzzz-arc-recovery-freeze.conf ] || \
            die "a later systemd drop-in can override the recovery fence: $unit"
        verify_closed_dropin_directory "$unit"
    done
}

stage_recovery_barrier() {
    local node="$1" marker="/etc/arc-recovery/legacy-start-allowed"
    require_node "$node"
    require_commands systemctl python3 pgrep sha256sum sync stat flock
    mkdir -p -- "$STOP_BASE"
    [ -d "$STOP_BASE" ] && [ ! -L "$STOP_BASE" ] || die "stop root is unsafe"
    local global_lock="$STOP_BASE/.host-writer-recovery.lock"
    [ ! -L "$global_lock" ] || die "global writer-recovery lock is a symlink"
    : >> "$global_lock"
    chmod 0600 -- "$global_lock"
    exec 7>> "$global_lock"
    flock -x 7
    python3 - <<'PY'
import errno
import os
import stat

parent = os.open("/etc", os.O_RDONLY | getattr(os, "O_DIRECTORY", 0) | getattr(os, "O_NOFOLLOW", 0))
try:
    try:
        details = os.stat("arc-recovery", dir_fd=parent, follow_symlinks=False)
    except FileNotFoundError:
        os.mkdir("arc-recovery", 0o700, dir_fd=parent)
        details = os.stat("arc-recovery", dir_fd=parent, follow_symlinks=False)
    if not stat.S_ISDIR(details.st_mode) or details.st_uid != 0 or details.st_gid != 0:
        raise SystemExit("recovery gate directory is unsafe")
    child = os.open(
        "arc-recovery",
        os.O_RDONLY | getattr(os, "O_DIRECTORY", 0) | getattr(os, "O_NOFOLLOW", 0),
        dir_fd=parent,
    )
    try:
        os.fchmod(child, 0o700); os.fsync(child)
    finally:
        os.close(child)
    os.fsync(parent)
finally:
    os.close(parent)
PY
    [ "$(stat -c %d /etc/arc-recovery)" = "$(stat -c %d /etc/systemd/system)" ] || \
        die "recovery allow marker is not on the systemd-unit filesystem"

    python3 - "$marker" <<'PY'
import os
import pathlib
import stat
import sys

path = pathlib.Path(sys.argv[1])
payload = b"schema=arc.recovery.legacy-start-allow.v1\n"
parent_details = path.parent.lstat()
if (path.parent.is_symlink() or not stat.S_ISDIR(parent_details.st_mode)
        or parent_details.st_uid != 0 or parent_details.st_gid != 0
        or stat.S_IMODE(parent_details.st_mode) != 0o700):
    raise SystemExit("recovery allow-marker directory is unsafe")
for directory_path in (path.parent.parent, path.parent):
    descriptor = os.open(
        directory_path,
        os.O_RDONLY | getattr(os, "O_DIRECTORY", 0) | getattr(os, "O_NOFOLLOW", 0),
    )
    try: os.fsync(descriptor)
    finally: os.close(descriptor)
fences = [
    pathlib.Path(f"/etc/systemd/system/{unit}.d/zzzz-arc-recovery-freeze.conf")
    for unit in (
        "arc-self-heal.service", "arc-node.service",
        "arc-node-update.service", "arc-node-update.timer",
    )
]
if path.exists() or path.is_symlink():
    mode = path.lstat().st_mode
    if path.is_symlink() or not stat.S_ISREG(mode) or mode & 0o222 or path.read_bytes() != payload:
        raise SystemExit("legacy-start allow marker is unsafe or differs")
else:
    if any(candidate.exists() or candidate.is_symlink() for candidate in fences):
        raise SystemExit("persistent recovery fence exists without its allow marker; never recreate the commit gate")
    descriptor = os.open(path, os.O_WRONLY | os.O_CREAT | os.O_EXCL | getattr(os, "O_NOFOLLOW", 0), 0o400)
    with os.fdopen(descriptor, "wb") as handle:
        handle.write(payload); handle.flush(); os.fsync(handle.fileno())
    directory = os.open(path.parent, os.O_RDONLY | getattr(os, "O_DIRECTORY", 0))
    try: os.fsync(directory)
    finally: os.close(directory)
PY

    local self_heal_pid node_pid selected_unit other_unit selected_pid unit
    self_heal_pid="$(systemctl show arc-self-heal.service --property=MainPID --value)"
    node_pid="$(systemctl show arc-node.service --property=MainPID --value)"
    case "$self_heal_pid:$node_pid" in
        [1-9]*:0) selected_unit=arc-self-heal.service; other_unit=arc-node.service; selected_pid="$self_heal_pid" ;;
        0:[1-9]*) selected_unit=arc-node.service; other_unit=arc-self-heal.service; selected_pid="$node_pid" ;;
        *) die "staging requires exactly one reviewed live legacy supervisor" ;;
    esac
    [ "$(systemctl show "$selected_unit" --property=ActiveState --value)" = active ] || \
        die "selected supervisor is not active during barrier staging"
    case "$(systemctl show "$selected_unit" --property=Job --value)" in ''|0) ;; *) die "selected supervisor has a pending job" ;; esac

    # Preparation does not issue a stop to an unsealed legacy unit. Every
    # alternative must already be process-free and quiescent; the production
    # fleet satisfies this invariant, and a divergent host fails closed.
    for unit in "$other_unit" arc-node-update.service arc-node-update.timer; do
        case "$(systemctl show "$unit" --property=ActiveState --value 2>/dev/null || printf 'not-found')" in inactive|failed|not-found) ;;
            *) die "alternative activation source is active before fence staging: $unit" ;;
        esac
        case "$(systemctl show "$unit" --property=Job --value 2>/dev/null || true)" in ''|0) ;;
            *) die "alternative activation source has a pending job before fence staging: $unit" ;;
        esac
    done
    [ "$(systemctl show "$other_unit" --property=MainPID --value 2>/dev/null || printf 0)" = 0 ] || \
        die "alternative node service has a MainPID before fence staging"
    [ "$(systemctl show arc-node-update.service --property=MainPID --value 2>/dev/null || printf 0)" = 0 ] || \
        die "updater service has a MainPID before fence staging"
    disable_and_verify_unit "$other_unit"
    disable_and_verify_unit arc-node-update.service
    disable_and_verify_unit arc-node-update.timer

    # `systemctl disable` removes boot-activation symlinks. Make those metadata
    # removals power-loss durable before a reboot can observe the still-present,
    # intentionally fail-open allow marker, then re-prove the terminal state.
    sync
    for unit in "$other_unit" arc-node-update.service arc-node-update.timer; do
        unit_enablement_state "$unit" >/dev/null
        case "$(systemctl show "$unit" --property=ActiveState --value 2>/dev/null || printf 'not-found')" in
            inactive|failed|not-found) ;;
            *) die "alternative activation source changed after durable disable sync: $unit" ;;
        esac
        case "$(systemctl show "$unit" --property=Job --value 2>/dev/null || true)" in
            ''|0) ;;
            *) die "alternative activation source gained a job after durable disable sync: $unit" ;;
        esac
        if [ "${unit##*.}" = service ]; then
            [ "$(systemctl show "$unit" --property=MainPID --value 2>/dev/null || printf 0)" = 0 ] || \
                die "alternative service gained a MainPID after durable disable sync: $unit"
        fi
    done

    persist_legacy_restart_fence_files
    systemctl daemon-reload
    for unit in arc-self-heal.service arc-node.service arc-node-update.service; do
        verify_merged_legacy_start_barrier_config "$unit"
    done
    verify_merged_legacy_start_barrier_config arc-node-update.timer
    [ "$(systemctl show "$selected_unit" --property=MainPID --value)" = "$selected_pid" ] || \
        die "selected supervisor changed while staging barriers"
    [ "$(systemctl show "$selected_unit" --property=ActiveState --value)" = active ] || \
        die "selected supervisor stopped while staging barriers"
    case "$(systemctl show "$selected_unit" --property=Job --value)" in ''|0) ;; *) die "selected supervisor gained a job while staging barriers" ;; esac

    # Preparation never reparents a live writer. A detached legacy writer is
    # sealed in its existing root-session cgroup; capture freezes that cgroup
    # independently after freezing the systemd supervisor.
    python3 - "$node" "$selected_unit" "$selected_pid" "$marker" <<'PY'
import hashlib
import json
import os
import pathlib
import re
import stat
import subprocess
import sys

node, unit, main_raw, marker_raw = sys.argv[1:]
main_pid = int(main_raw); marker = pathlib.Path(marker_raw)
writer_pids = []
for entry in pathlib.Path("/proc").iterdir():
    if not entry.name.isdigit(): continue
    try:
        if entry.joinpath("comm").read_text().strip() == "arc-node": writer_pids.append(int(entry.name))
    except (FileNotFoundError, PermissionError, ProcessLookupError): pass
if len(writer_pids) != 1: raise SystemExit(f"staging found an ambiguous writer set: {writer_pids}")
writer_pid = writer_pids[0]
def cgroup(pid):
    rows = []
    for line in pathlib.Path(f"/proc/{pid}/cgroup").read_text().splitlines():
        hierarchy, controllers, path = line.split(":", 2)
        if hierarchy == "0" and controllers == "": rows.append(path)
    if len(rows) != 1 or not re.fullmatch(r"/[A-Za-z0-9._@/-]+", rows[0]) or ".." in rows[0]:
        raise SystemExit("writer unified cgroup is unsafe")
    return rows[0]
supervisor_cgroup = subprocess.check_output(
    ["systemctl", "show", unit, "--property=ControlGroup", "--value"], text=True,
).strip()
if cgroup(main_pid) != supervisor_cgroup or unit not in supervisor_cgroup:
    raise SystemExit("selected supervisor control group differs")
writer_cgroup = cgroup(writer_pid)
writer_entry = pathlib.Path("/sys/fs/cgroup") / writer_cgroup.lstrip("/")
if writer_entry.is_symlink() or not writer_entry.is_dir():
    raise SystemExit("writer cgroup is unsafe")
writer_details = writer_entry.stat()
writer_start = int(pathlib.Path(f"/proc/{writer_pid}/stat").read_text().split()[21])
writer_executable = os.readlink(f"/proc/{writer_pid}/exe")
writer_executable_sha = hashlib.sha256(pathlib.Path(f"/proc/{writer_pid}/exe").read_bytes()).hexdigest()
writer_argv_sha = hashlib.sha256(pathlib.Path(f"/proc/{writer_pid}/cmdline").read_bytes()).hexdigest()
if writer_cgroup == supervisor_cgroup:
    writer_mode = "systemd-unit"
else:
    stat_fields = pathlib.Path(f"/proc/{writer_pid}/stat").read_text().split()
    if (not re.fullmatch(r"/user\.slice/user-0\.slice/session-[1-9][0-9]*\.scope", writer_cgroup)
            or len(stat_fields) < 22 or int(stat_fields[3]) != 1):
        raise SystemExit("detached writer is outside the reviewed root-session shape")
    writer_mode = "detached-root-session"
    observed = set()
    for current, directories, _files in os.walk(writer_entry, followlinks=False):
        directories.sort()
        current_path = pathlib.Path(current)
        if current_path.is_symlink():
            raise SystemExit("writer cgroup subtree contains a symlink")
        procs = current_path / "cgroup.procs"
        if procs.is_symlink() or not procs.is_file():
            raise SystemExit("writer cgroup inventory is unsafe")
        observed.update(int(value) for value in procs.read_text().splitlines())
    if observed != {writer_pid}:
        raise SystemExit("detached writer is not the sole process in its cgroup subtree")
if (int(pathlib.Path(f"/proc/{writer_pid}/stat").read_text().split()[21]) != writer_start
        or os.readlink(f"/proc/{writer_pid}/exe") != writer_executable
        or hashlib.sha256(pathlib.Path(f"/proc/{writer_pid}/exe").read_bytes()).hexdigest() != writer_executable_sha
        or hashlib.sha256(pathlib.Path(f"/proc/{writer_pid}/cmdline").read_bytes()).hexdigest() != writer_argv_sha):
    raise SystemExit("writer identity changed during barrier staging")
if marker.is_symlink() or marker.read_bytes() != b"schema=arc.recovery.legacy-start-allow.v1\n":
    raise SystemExit("legacy-start allow marker changed during staging")
dropins = {}
for target in ("arc-self-heal.service", "arc-node.service", "arc-node-update.service", "arc-node-update.timer"):
    path = pathlib.Path(f"/etc/systemd/system/{target}.d/zzzz-arc-recovery-freeze.conf")
    mode = path.lstat().st_mode
    if path.is_symlink() or not stat.S_ISREG(mode): raise SystemExit("staged fence is unsafe")
    dropins[target] = hashlib.sha256(path.read_bytes()).hexdigest()
print(json.dumps({
    "schema": "arc.recovery.barrier-stage-status.v1", "node": node,
    "selected_unit": unit, "selected_main_pid": main_pid, "writer_pid": writer_pid,
    "writer_start_ticks": writer_start,
    "writer_executable_path": writer_executable,
    "writer_executable_sha256": writer_executable_sha,
    "writer_argv_sha256": writer_argv_sha,
    "writer_cgroup": writer_cgroup, "writer_supervision_mode": writer_mode,
    "writer_cgroup_identity": {
        "path": writer_cgroup, "device": writer_details.st_dev, "inode": writer_details.st_ino,
    },
    "allow_marker_sha256": hashlib.sha256(marker.read_bytes()).hexdigest(),
    "dropin_sha256": dropins, "alternatives_inactive_no_jobs": True,
    "alternative_enablement_sync_completed": True,
}, sort_keys=True, separators=(",", ":")))
PY
}

stage_prefreeze_runtime_safety() {
    local root="$1" capture_id="$2" node="$3" freeze_sha="$4" boot_id="$5"
    local unit="$6" supervisor_pid="$7" supervisor_context_sha="$8"
    local runtime_dropin="/run/systemd/system/$unit.d/zzzy-arc-recovery-prefreeze-safety.conf"
    python3 - "$root" "$capture_id" "$node" "$freeze_sha" "$boot_id" \
        "$unit" "$supervisor_pid" "$supervisor_context_sha" "$runtime_dropin" <<'PY'
import hashlib
import json
import os
import pathlib
import re
import secrets
import shutil
import stat
import subprocess
import sys

(root_raw, capture_id, node, freeze_sha, boot_id, unit, supervisor_pid_raw,
 context_sha, runtime_path_raw) = sys.argv[1:]
root = pathlib.Path(root_raw); output = root / "01-prefreeze-runtime-safety-intent.json"
supervisor_pid = int(supervisor_pid_raw); runtime_path = pathlib.Path(runtime_path_raw)
runtime_bytes = b"[Unit]\nRefuseManualStart=yes\nRefuseManualStop=yes\nIgnoreOnIsolate=yes\nStopWhenUnneeded=no\nBindsTo=\nPartOf=\nPropagatesStopTo=\nOnFailure=\nOnSuccess=\nFailureAction=none\nSuccessAction=none\nJobTimeoutAction=none\n\n[Service]\nExecReload=\nExecStop=\nExecStopPost=\nRestart=no\nKillMode=process\nSendSIGKILL=no\nSendSIGHUP=no\nOOMPolicy=continue\nWatchdogSec=0\nRuntimeMaxSec=infinity\n"
def canonical(value): return (json.dumps(value, sort_keys=True, separators=(",", ":")) + "\n").encode()
def prop(name): return subprocess.check_output(["systemctl", "show", unit, f"--property={name}", "--value"], text=True).strip()
fixed = {
    "schema": "arc.recovery.prefreeze-runtime-safety-intent.v1",
    "capture_id": capture_id, "node": node, "freeze_plan_sha256": freeze_sha,
    "boot_id": boot_id, "supervisor_unit": unit, "supervisor_main_pid": supervisor_pid,
    "supervisor_context_sha256": context_sha,
    "runtime_dropin_path": str(runtime_path),
    "runtime_dropin_sha256": hashlib.sha256(runtime_bytes).hexdigest(),
}
if not re.fullmatch(r"[0-9a-f]{64}", context_sha):
    raise SystemExit("prefreeze supervisor context hash is malformed")
if output.exists() or output.is_symlink():
    if output.is_symlink() or not output.is_file(): raise SystemExit("prefreeze safety intent is unsafe")
    raw = output.read_bytes(); value = json.loads(raw)
    if raw != canonical(value) or any(value.get(key) != expected for key, expected in fixed.items()):
        raise SystemExit("prefreeze safety intent differs")
    if value.get("pre_recovery_oom_policy") not in {"stop", "continue"} or not re.fullmatch(
        r"[0-9a-f]{64}", value.get("pre_recovery_unit_configuration_sha256", ""),
    ) or not isinstance(value.get("pre_recovery_unit_sources"), list) or not value["pre_recovery_unit_sources"]:
        raise SystemExit("prefreeze safety baseline differs")
else:
    if pathlib.Path("/proc/sys/kernel/random/boot_id").read_text().strip() != boot_id:
        raise SystemExit("sealed boot ended before prefreeze safety staging")
    if prop("MainPID") != str(supervisor_pid) or prop("ActiveState") != "active" or prop("Job") not in {"", "0"}:
        raise SystemExit("supervisor changed before prefreeze safety staging")
    pre_oom = prop("OOMPolicy")
    if pre_oom not in {"stop", "continue"}:
        raise SystemExit("unreviewed pre-recovery OOM policy")
    unit_configuration = subprocess.check_output(["systemctl", "cat", unit])
    source_paths = re.findall(rb"(?m)^# (/[^\n]+)$", unit_configuration)
    if not source_paths:
        raise SystemExit("prefreeze supervisor unit has no content sources")
    sources = []
    for raw_path in source_paths:
        path = pathlib.Path(raw_path.decode("utf-8"))
        if path.is_symlink() or not path.is_file():
            raise SystemExit("prefreeze supervisor unit source is unsafe")
        sources.append({"path": str(path), "sha256": hashlib.sha256(path.read_bytes()).hexdigest()})
    if len({row["path"] for row in sources}) != len(sources):
        raise SystemExit("prefreeze supervisor unit source is duplicated")
    value = {
        **fixed,
        "pre_recovery_oom_policy": pre_oom,
        "pre_recovery_unit_configuration_sha256": hashlib.sha256(unit_configuration).hexdigest(),
        "pre_recovery_unit_sources": sources,
    }
    temporary = output.with_name(f".{output.name}.partial")
    if temporary.exists() or temporary.is_symlink():
        if temporary.is_symlink() or not temporary.is_file(): raise SystemExit("prefreeze safety intent partial is unsafe")
        temporary.unlink()
    descriptor = os.open(temporary, os.O_WRONLY | os.O_CREAT | os.O_EXCL | getattr(os, "O_NOFOLLOW", 0), 0o400)
    with os.fdopen(descriptor, "wb") as handle: handle.write(canonical(value)); handle.flush(); os.fsync(handle.fileno())
    os.rename(temporary, output)
    descriptor = os.open(root, os.O_RDONLY | getattr(os, "O_DIRECTORY", 0))
    try: os.fsync(descriptor)
    finally: os.close(descriptor)
expected_sources = set()
for row in value["pre_recovery_unit_sources"]:
    if (not isinstance(row, dict) or set(row) != {"path", "sha256"}
            or not re.fullmatch(r"[0-9a-f]{64}", row.get("sha256", ""))):
        raise SystemExit("prefreeze unit source manifest is malformed")
    source = pathlib.Path(row["path"])
    if source.is_symlink() or not source.is_file() or hashlib.sha256(source.read_bytes()).hexdigest() != row["sha256"]:
        raise SystemExit("prefreeze unit source changed before runtime safety staging")
    expected_sources.add(row["path"])
current_sources = {
    raw.decode("utf-8") for raw in re.findall(
        rb"(?m)^# (/[^\n]+)$", subprocess.check_output(["systemctl", "cat", unit]),
    )
}
allowed_sources = expected_sources | ({str(runtime_path)} if runtime_path.is_file() and not runtime_path.is_symlink() else set())
if current_sources != allowed_sources:
    raise SystemExit("supervisor unit source set changed before runtime safety staging")
PY
    python3 - "$unit" <<'PY'
import os
import secrets
import stat
import sys

unit = sys.argv[1]; directory_name = f"{unit}.d"
name = "zzzy-arc-recovery-prefreeze-safety.conf"
expected = b"[Unit]\nRefuseManualStart=yes\nRefuseManualStop=yes\nIgnoreOnIsolate=yes\nStopWhenUnneeded=no\nBindsTo=\nPartOf=\nPropagatesStopTo=\nOnFailure=\nOnSuccess=\nFailureAction=none\nSuccessAction=none\nJobTimeoutAction=none\n\n[Service]\nExecReload=\nExecStop=\nExecStopPost=\nRestart=no\nKillMode=process\nSendSIGKILL=no\nSendSIGHUP=no\nOOMPolicy=continue\nWatchdogSec=0\nRuntimeMaxSec=infinity\n"
flags = os.O_RDONLY | getattr(os, "O_DIRECTORY", 0) | getattr(os, "O_NOFOLLOW", 0)
root = os.open("/run/systemd/system", flags)
try:
    details = os.fstat(root)
    if (not stat.S_ISDIR(details.st_mode) or details.st_uid != 0 or details.st_gid != 0
            or details.st_mode & 0o022): raise SystemExit("runtime systemd directory is unsafe")
    try: details = os.stat(directory_name, dir_fd=root, follow_symlinks=False)
    except FileNotFoundError:
        os.mkdir(directory_name, 0o755, dir_fd=root)
        details = os.stat(directory_name, dir_fd=root, follow_symlinks=False)
    if (not stat.S_ISDIR(details.st_mode) or details.st_uid != 0 or details.st_gid != 0
            or details.st_mode & 0o022): raise SystemExit("prefreeze drop-in directory is unsafe")
    child = os.open(directory_name, flags, dir_fd=root)
    try:
        try: current = os.stat(name, dir_fd=child, follow_symlinks=False)
        except FileNotFoundError: current = None
        if current is None:
            temporary = f".{name}.partial.{secrets.token_hex(8)}"
            descriptor = os.open(temporary, os.O_WRONLY | os.O_CREAT | os.O_EXCL | getattr(os, "O_NOFOLLOW", 0), 0o444, dir_fd=child)
            try:
                os.write(descriptor, expected); os.fchmod(descriptor, 0o444); os.fsync(descriptor)
            finally: os.close(descriptor)
            os.rename(temporary, name, src_dir_fd=child, dst_dir_fd=child)
            current = os.stat(name, dir_fd=child, follow_symlinks=False)
        if (not stat.S_ISREG(current.st_mode) or current.st_uid != 0 or current.st_gid != 0):
            raise SystemExit("prefreeze runtime safety inode is unsafe")
        descriptor = os.open(name, os.O_RDONLY | getattr(os, "O_NOFOLLOW", 0), dir_fd=child)
        try:
            if os.read(descriptor, len(expected) + 1) != expected: raise SystemExit("prefreeze runtime safety bytes differ")
            os.fsync(descriptor)
        finally: os.close(descriptor)
        os.fsync(child)
    finally: os.close(child)
    os.fsync(root)
finally: os.close(root)
PY
    systemctl daemon-reload
    [ "$(systemctl show "$unit" --property=OOMPolicy --value)" = continue ] || \
        die "prefreeze runtime OOM safety was not applied"
    [ "$(systemctl show "$unit" --property=WatchdogUSec --value)" = 0 ] || \
        die "prefreeze runtime watchdog safety was not applied"
    [ "$(systemctl show "$unit" --property=RuntimeMaxUSec --value)" = infinity ] || \
        die "prefreeze runtime limit safety was not applied"
    [ "$(systemctl show "$unit" --property=Restart --value)" = no ] || \
        die "prefreeze runtime restart safety was not applied"
    [ "$(systemctl show "$unit" --property=KillMode --value)" = process ] || \
        die "prefreeze runtime KillMode safety was not applied"
    [ "$(systemctl show "$unit" --property=SendSIGKILL --value)" = no ] || \
        die "prefreeze runtime SIGKILL safety was not applied"
    [ "$(systemctl show "$unit" --property=SendSIGHUP --value)" = no ] || \
        die "prefreeze runtime SIGHUP safety was not applied"
    [ "$(systemctl show "$unit" --property=IgnoreOnIsolate --value)" = yes ] || \
        die "prefreeze runtime isolate safety was not applied"
    [ "$(systemctl show "$unit" --property=StopWhenUnneeded --value)" = no ] || \
        die "prefreeze runtime automatic-stop safety was not applied"
    [ "$(systemctl show "$unit" --property=RefuseManualStart --value)" = yes ] || \
        die "prefreeze runtime manual-start safety was not applied"
    [ "$(systemctl show "$unit" --property=RefuseManualStop --value)" = yes ] || \
        die "prefreeze runtime manual-stop safety was not applied"
    [ "$(systemctl show "$unit" --property=CanReload --value)" = no ] || \
        die "prefreeze runtime reload capability was not cleared"
    local lifecycle_property lifecycle_value
    for lifecycle_property in ExecReload ExecStop ExecStopPost OnFailure OnSuccess; do
        lifecycle_value="$(systemctl show "$unit" --property="$lifecycle_property" --value)"
        [ -z "$lifecycle_value" ] || \
            die "prefreeze runtime lifecycle hook remains: $lifecycle_property"
    done
    for lifecycle_property in SuccessAction FailureAction JobTimeoutAction; do
        lifecycle_value="$(systemctl show "$unit" --property="$lifecycle_property" --value)"
        [ "$lifecycle_value" = none ] || \
            die "prefreeze runtime lifecycle action remains: $lifecycle_property=$lifecycle_value"
    done
    [ -z "$(systemctl show "$unit" --property=StopPropagatedFrom --value)" ] || \
        die "prefreeze runtime has a reverse stop propagation edge"
    [ -z "$(systemctl show "$unit" --property=ReloadPropagatedFrom --value)" ] || \
        die "prefreeze runtime has a reverse reload propagation edge"
}

fast_cgroup_freeze() {
    require_commands python3 systemctl systemd-escape
    local root="$1" capture_id="$2" node="$3" freeze_sha="$4" boot_id="$5"
    local writer_supervision_mode="$6" unit="$7" supervisor_pid="$8"
    local supervisor_start_ticks="$9" supervisor_executable_path="${10}"
    local supervisor_executable_sha="${11}" supervisor_argv_sha="${12}"
    local supervisor_context_sha="${13}" writer_pid="${14}" writer_start_ticks="${15}"
    local writer_cgroup_sha="${16}" writer_executable_path="${17}"
    local writer_executable_sha="${18}" writer_argv_sha="${19}"
    python3 - "$root" "$capture_id" "$node" "$freeze_sha" "$boot_id" \
        "$writer_supervision_mode" "$unit" "$supervisor_pid" "$supervisor_start_ticks" \
        "$supervisor_executable_path" "$supervisor_executable_sha" "$supervisor_argv_sha" \
        "$supervisor_context_sha" "$writer_pid" "$writer_start_ticks" "$writer_cgroup_sha" \
        "$writer_executable_path" "$writer_executable_sha" "$writer_argv_sha" <<'PY'
import hashlib
import json
import os
import pathlib
import re
import shutil
import subprocess
import secrets
import stat
import sys
import time

(root_raw, capture_id, node, freeze_sha, boot_id, writer_mode, unit,
 supervisor_pid_raw, supervisor_start_raw, supervisor_executable_path,
 supervisor_executable_sha, supervisor_argv_sha, supervisor_context_sha,
 writer_pid_raw, writer_start_raw, writer_cgroup_sha, writer_executable_path,
 writer_executable_sha, writer_argv_sha) = sys.argv[1:]
root = pathlib.Path(root_raw)
supervisor_pid, supervisor_start = int(supervisor_pid_raw), int(supervisor_start_raw)
writer_pid, writer_start = int(writer_pid_raw), int(writer_start_raw)
intent_path = root / "02-fast-cgroup-freeze-intent.json"
frozen_path = root / "03-fast-cgroups-frozen.json"
leaf_move_path = root / "02-writer-leaf-move-intent.json"
leaf_receipt_path = root / "02-writer-cgroup-frozen.json"
parent_release_path = root / "02-writer-parent-released.json"
leaf_name = "arc-recovery-writer"

def canonical(value):
    return (json.dumps(value, sort_keys=True, separators=(",", ":")) + "\n").encode()

def publish(path, value):
    payload = canonical(value)
    if path.exists():
        if path.is_symlink() or path.read_bytes() != payload:
            raise SystemExit(f"fast cgroup-freeze event differs: {path.name}")
        return
    temporary = path.with_name(f".{path.name}.partial")
    if temporary.exists() or temporary.is_symlink():
        if temporary.is_symlink() or not temporary.is_file():
            raise SystemExit(f"unsafe fast cgroup-freeze partial: {path.name}")
        temporary.unlink()
    descriptor = os.open(temporary, os.O_WRONLY | os.O_CREAT | os.O_EXCL | getattr(os, "O_NOFOLLOW", 0), 0o400)
    with os.fdopen(descriptor, "wb") as handle:
        handle.write(payload); handle.flush(); os.fsync(handle.fileno())
    os.rename(temporary, path)
    descriptor = os.open(root, os.O_RDONLY | getattr(os, "O_DIRECTORY", 0))
    try: os.fsync(descriptor)
    finally: os.close(descriptor)

def proc_fields(pid):
    raw = pathlib.Path(f"/proc/{pid}/stat").read_text(encoding="ascii")
    end = raw.rfind(")")
    fields = raw[end + 2:].split()
    if end < 0 or len(fields) < 20:
        raise SystemExit("process stat is truncated during fast cgroup freeze")
    return fields

def proc_start(pid):
    return int(proc_fields(pid)[19])

def digest(path):
    value = hashlib.sha256()
    with open(path, "rb") as handle:
        for chunk in iter(lambda: handle.read(1024 * 1024), b""):
            value.update(chunk)
    return value.hexdigest()

def unified_cgroup(pid):
    rows = []
    for line in pathlib.Path(f"/proc/{pid}/cgroup").read_text(encoding="utf-8").splitlines():
        hierarchy, controllers, path = line.split(":", 2)
        if hierarchy == "0" and controllers == "":
            rows.append(path)
    if len(rows) != 1 or not re.fullmatch(r"/[A-Za-z0-9._@/-]+", rows[0]) or ".." in rows[0]:
        raise SystemExit("process unified cgroup is missing or unsafe")
    return rows[0]

def cgroup_entry(role, path):
    target = pathlib.Path("/sys/fs/cgroup") / path.lstrip("/")
    if path == "/" or target.is_symlink() or not target.is_dir():
        raise SystemExit("fast freeze target cgroup is missing, root, or a symlink")
    details = target.stat()
    return {"role": role, "path": path, "device": details.st_dev, "inode": details.st_ino}

def exact_identity(entry, label):
    if (not isinstance(entry, dict) or set(entry) != {"role", "path", "device", "inode"}
            or not isinstance(entry["path"], str) or entry["path"] == "/"
            or not re.fullmatch(r"/[A-Za-z0-9._@/-]+", entry["path"])
            or ".." in entry["path"]
            or any(isinstance(entry[name], bool) or not isinstance(entry[name], int) or entry[name] <= 0
                   for name in ("device", "inode"))):
        raise SystemExit(f"{label} cgroup identity is malformed")
    target = pathlib.Path("/sys/fs/cgroup") / entry["path"].lstrip("/")
    details = target.lstat()
    if (target.is_symlink() or not target.is_dir() or details.st_dev != entry["device"]
            or details.st_ino != entry["inode"]):
        raise SystemExit(f"{label} cgroup identity changed")
    return target

# The prepare audit binds the original writer cgroup path/device/inode.  A
# detached writer may move only into the deterministic recovery leaf under that
# exact parent, and only after the durable leaf-move intent below exists.
plan_path = pathlib.Path(f"/root/.arc-recovery-plans/{freeze_sha}/freeze.lock.json")
if plan_path.is_symlink() or not plan_path.is_file():
    raise SystemExit("pinned freeze plan is missing during fast freeze")
plan_raw = plan_path.read_bytes(); plan = json.loads(plan_raw)
plan_rows = [row for row in plan.get("nodes", []) if isinstance(row, dict) and row.get("name") == node]
if (hashlib.sha256(plan_raw).hexdigest() != freeze_sha or plan_raw != canonical(plan)
        or plan.get("schema") != "arc.recovery.freeze-plan.v5" or len(plan_rows) != 1):
    raise SystemExit("pinned freeze plan differs during fast freeze")
plan_row = plan_rows[0]
prepared_parent = {
    "role": "writer-parent",
    "path": plan_row.get("writer_cgroup_path"),
    "device": plan_row.get("writer_cgroup_device"),
    "inode": plan_row.get("writer_cgroup_inode"),
}
prepared_parent_path = exact_identity(prepared_parent, "prepared writer parent")
if (plan_row.get("writer_pid") != writer_pid or plan_row.get("writer_start_ticks") != writer_start
        or plan_row.get("writer_cgroup_sha256") != writer_cgroup_sha
        or plan_row.get("writer_supervision_mode") != writer_mode):
    raise SystemExit("pinned writer cgroup contract differs during fast freeze")
recovery_leaf_path = prepared_parent["path"].rstrip("/") + "/" + leaf_name

def ancestor_pids():
    rows = []
    current = os.getpid()
    while current > 1 and current not in rows:
        rows.append(current)
        try:
            current = int(proc_fields(current)[1])
        except FileNotFoundError:
            break
    return rows

def cgroup_subtree_pids(path):
    base = pathlib.Path("/sys/fs/cgroup") / path.lstrip("/")
    observed = set()
    for current, dirs, _files in os.walk(base, followlinks=False):
        dirs.sort()
        current_path = pathlib.Path(current)
        if current_path.is_symlink():
            raise SystemExit("fast-freeze cgroup subtree contains a symlink")
        procs = current_path / "cgroup.procs"
        if procs.is_symlink() or not procs.is_file():
            raise SystemExit("fast-freeze cgroup process inventory is unsafe")
        observed.update(int(value) for value in procs.read_text(encoding="ascii").splitlines())
    return sorted(observed)

def proc_ppid(pid):
    return int(proc_fields(pid)[1])

def is_descendant(pid, ancestor):
    current = pid; observed = set()
    while current > 1 and current not in observed:
        if current == ancestor: return True
        observed.add(current)
        try: current = proc_ppid(current)
        except FileNotFoundError: return False
    return current == ancestor

def duration_seconds(value):
    match = re.fullmatch(r"([0-9]+(?:\.[0-9]+)?)([smhd]?)", value)
    if not match: return None
    return float(match.group(1)) * {"": 1, "s": 1, "m": 60, "h": 3600, "d": 86400}[match.group(2)]

def validate_supervisor_members(path):
    allowed = {supervisor_pid}
    if writer_mode == "systemd-unit": allowed.add(writer_pid)
    sleep_candidate = shutil.which("sleep")
    if unit == "arc-self-heal.service" and not sleep_candidate:
        raise SystemExit("reviewed self-heal supervisor has no sleep executable")
    sleep_path = os.path.realpath(sleep_candidate) if sleep_candidate else None
    sleep_sha = digest(sleep_path) if sleep_path else None
    for pid in cgroup_subtree_pids(path):
        if pid in allowed: continue
        if not is_descendant(pid, supervisor_pid):
            raise SystemExit("unreviewed non-descendant exists in supervisor cgroup before freeze")
        proc = pathlib.Path("/proc") / str(pid)
        try:
            argv = [part.decode("utf-8") for part in proc.joinpath("cmdline").read_bytes().rstrip(b"\0").split(b"\0")]
            executable = os.path.realpath(os.readlink(proc / "exe"))
        except (FileNotFoundError, ProcessLookupError, UnicodeDecodeError):
            raise SystemExit("supervisor member changed during fast allowlist validation")
        seconds = duration_seconds(argv[1]) if len(argv) == 2 else None
        if (unit != "arc-self-heal.service" or executable != sleep_path
                or digest(proc / "exe") != sleep_sha or seconds is None or seconds > 60):
            raise SystemExit("unreviewed process exists in supervisor cgroup before freeze")

def frozen_state(entry):
    base = pathlib.Path("/sys/fs/cgroup") / entry["path"].lstrip("/")
    details = base.stat()
    if details.st_dev != entry["device"] or details.st_ino != entry["inode"]:
        raise SystemExit("fast-frozen cgroup path/inode was replaced")
    values = {}
    for line in base.joinpath("cgroup.events").read_text(encoding="ascii").splitlines():
        key, _, value = line.partition(" ")
        values[key] = value
    if values.get("frozen") not in {"0", "1"}:
        raise SystemExit("cgroup.events has no exact frozen state")
    return int(values["frozen"])

# The runtime safety intent is durable before this controller starts. Reload
# and re-project it on every identity check so a unit-file edit, daemon reload,
# or automatic lifecycle change cannot race any freezer write.
safety_path = root / "01-prefreeze-runtime-safety-intent.json"
if safety_path.is_symlink() or not safety_path.is_file():
    raise SystemExit("prefreeze runtime safety intent is missing")
safety_raw = safety_path.read_bytes(); safety = json.loads(safety_raw)
expected_safety_keys = {
    "schema", "capture_id", "node", "freeze_plan_sha256", "boot_id",
    "supervisor_unit", "supervisor_main_pid", "supervisor_context_sha256",
    "runtime_dropin_path", "runtime_dropin_sha256", "pre_recovery_oom_policy",
    "pre_recovery_unit_configuration_sha256", "pre_recovery_unit_sources",
}
runtime_path = pathlib.Path(f"/run/systemd/system/{unit}.d/zzzy-arc-recovery-prefreeze-safety.conf")
runtime_expected = b"[Unit]\nRefuseManualStart=yes\nRefuseManualStop=yes\nIgnoreOnIsolate=yes\nStopWhenUnneeded=no\nBindsTo=\nPartOf=\nPropagatesStopTo=\nOnFailure=\nOnSuccess=\nFailureAction=none\nSuccessAction=none\nJobTimeoutAction=none\n\n[Service]\nExecReload=\nExecStop=\nExecStopPost=\nRestart=no\nKillMode=process\nSendSIGKILL=no\nSendSIGHUP=no\nOOMPolicy=continue\nWatchdogSec=0\nRuntimeMaxSec=infinity\n"
if (not isinstance(safety, dict) or set(safety) != expected_safety_keys
        or safety_raw != canonical(safety)
        or safety.get("capture_id") != capture_id
        or safety.get("node") != node
        or safety.get("freeze_plan_sha256") != freeze_sha
        or safety.get("boot_id") != boot_id
        or safety.get("supervisor_unit") != unit
        or safety.get("supervisor_main_pid") != supervisor_pid
        or safety.get("supervisor_context_sha256") != supervisor_context_sha
        or safety.get("runtime_dropin_path") != str(runtime_path)
        or safety.get("runtime_dropin_sha256") != hashlib.sha256(runtime_expected).hexdigest()
        or safety.get("pre_recovery_oom_policy") not in {"stop", "continue"}
        or not re.fullmatch(r"[0-9a-f]{64}", safety.get("pre_recovery_unit_configuration_sha256", ""))):
    raise SystemExit("prefreeze runtime safety intent differs from sealed freeze")

def unit_property(name):
    return subprocess.check_output(
        ["systemctl", "show", unit, f"--property={name}", "--value"], text=True,
    ).strip()

def validate_unit_projection():
    if (runtime_path.is_symlink() or not runtime_path.is_file()
            or runtime_path.read_bytes() != runtime_expected):
        raise SystemExit("prefreeze runtime safety bytes changed during fast freeze")
    expected_source_paths = set()
    sources = safety.get("pre_recovery_unit_sources")
    if not isinstance(sources, list) or not sources:
        raise SystemExit("prefreeze runtime safety has no unit source manifest")
    for row in sources:
        if (not isinstance(row, dict) or set(row) != {"path", "sha256"}
                or not isinstance(row.get("path"), str) or not row["path"].startswith("/")
                or not re.fullmatch(r"[0-9a-f]{64}", row.get("sha256", ""))):
            raise SystemExit("prefreeze unit source manifest is malformed")
        source = pathlib.Path(row["path"])
        if (source.is_symlink() or not source.is_file()
                or hashlib.sha256(source.read_bytes()).hexdigest() != row["sha256"]):
            raise SystemExit("supervisor unit source changed during fast freeze")
        if row["path"] in expected_source_paths:
            raise SystemExit("prefreeze unit source manifest contains a duplicate")
        expected_source_paths.add(row["path"])
    current_headers = {
        value.decode("utf-8") for value in re.findall(
            rb"(?m)^# (/[^\n]+)$", subprocess.check_output(["systemctl", "cat", unit]),
        )
    }
    if current_headers != expected_source_paths | {str(runtime_path)}:
        raise SystemExit("supervisor unit source set changed during fast freeze")
    expected_properties = {
        "CanReload": "no", "RefuseManualStart": "yes", "RefuseManualStop": "yes",
        "IgnoreOnIsolate": "yes", "Restart": "no", "KillMode": "process",
        "SendSIGKILL": "no", "SendSIGHUP": "no",
        "OOMPolicy": "continue", "WatchdogUSec": "0", "RuntimeMaxUSec": "infinity",
        "StopWhenUnneeded": "no", "BindsTo": "", "PartOf": "", "PropagatesStopTo": "",
        "StopPropagatedFrom": "", "ReloadPropagatedFrom": "",
        "ExecReload": "", "ExecStop": "", "ExecStopPost": "",
        "OnFailure": "", "OnSuccess": "", "SuccessAction": "none",
        "FailureAction": "none", "JobTimeoutAction": "none",
    }
    if any(unit_property(name) != value for name, value in expected_properties.items()):
        raise SystemExit("effective prefreeze runtime safety projection changed")

detached_parent_terminal = False

def validate_identities():
    global detached_parent_terminal
    if pathlib.Path("/proc/sys/kernel/random/boot_id").read_text().strip() != boot_id:
        raise SystemExit("host rebooted after fast freeze intent; a new sealed audit is required")
    if proc_start(supervisor_pid) != supervisor_start or proc_start(writer_pid) != writer_start:
        raise SystemExit("sealed PID/start changed during fast cgroup freeze")
    for pid, executable_path, executable_sha, argv_sha, role in (
        (supervisor_pid, supervisor_executable_path, supervisor_executable_sha, supervisor_argv_sha, "supervisor"),
        (writer_pid, writer_executable_path, writer_executable_sha, writer_argv_sha, "writer"),
    ):
        proc = pathlib.Path("/proc") / str(pid)
        if (os.readlink(proc / "exe") != executable_path
                or digest(proc / "exe") != executable_sha
                or hashlib.sha256(proc.joinpath("cmdline").read_bytes()).hexdigest() != argv_sha):
            raise SystemExit(f"sealed {role} executable/argv changed during fast cgroup freeze")
    if proc_fields(supervisor_pid)[0] in {"T", "t"} or proc_fields(writer_pid)[0] in {"T", "t"}:
        raise SystemExit("sealed target was already job-control stopped before cgroup freeze")
    supervisor_cgroup = unified_cgroup(supervisor_pid)
    writer_cgroup = unified_cgroup(writer_pid)
    if unit not in supervisor_cgroup:
        raise SystemExit("supervisor is outside its sealed unit cgroup")
    if (subprocess.check_output(
            ["systemctl", "show", unit, "--property=MainPID", "--value"], text=True,
        ).strip() != str(supervisor_pid)
            or subprocess.check_output(
                ["systemctl", "show", unit, "--property=ActiveState", "--value"], text=True,
            ).strip() != "active"
            or subprocess.check_output(
                ["systemctl", "show", unit, "--property=Job", "--value"], text=True,
            ).strip() not in {"", "0"}
            or subprocess.check_output(
                ["systemctl", "show", unit, "--property=ControlGroup", "--value"], text=True,
            ).strip() != supervisor_cgroup):
        raise SystemExit("selected supervisor unit changed during fast cgroup freeze")
    if writer_mode == "systemd-unit":
        if (writer_cgroup != supervisor_cgroup
                or hashlib.sha256(pathlib.Path(f"/proc/{writer_pid}/cgroup").read_bytes()).hexdigest()
                != writer_cgroup_sha):
            raise SystemExit("systemd writer is outside the reviewed supervisor cgroup")
    elif writer_mode == "detached-root-session":
        if (not re.fullmatch(r"/user\.slice/user-0\.slice/session-[1-9][0-9]*\.scope", prepared_parent["path"])
                or proc_fields(writer_pid)[1] != "1"):
            raise SystemExit("detached writer relationship differs from the sealed audit")
        exact_identity(prepared_parent, "detached writer parent scope")
        leaf_sealed = False
        durable_fast_writer = {}
        durable_intent_raw = None
        if intent_path.exists() or intent_path.is_symlink():
            if intent_path.is_symlink() or not intent_path.is_file():
                raise SystemExit("durable fast-freeze intent is unsafe")
            durable_intent_raw = intent_path.read_bytes()
            durable_intent = json.loads(durable_intent_raw)
            durable_fast_writer = durable_intent.get("writer", {}) if isinstance(durable_intent, dict) else {}
            if (durable_intent_raw != canonical(durable_intent)
                    or durable_intent.get("schema") != "arc.recovery.fast-cgroup-freeze-intent.v1"
                    or durable_intent.get("freeze_plan_sha256") != freeze_sha
                    or durable_intent.get("boot_id") != boot_id
                    or durable_fast_writer.get("pid") != writer_pid
                    or durable_fast_writer.get("start_ticks") != writer_start
                    or durable_fast_writer.get("parent_scope_cgroup") != prepared_parent
                    or durable_fast_writer.get("recovery_leaf_path") != recovery_leaf_path):
                raise SystemExit("durable fast-freeze intent differs")
        if writer_cgroup == prepared_parent["path"]:
            if hashlib.sha256(pathlib.Path(f"/proc/{writer_pid}/cgroup").read_bytes()).hexdigest() != writer_cgroup_sha:
                raise SystemExit("detached writer parent cgroup bytes differ from the sealed audit")
        elif writer_cgroup == recovery_leaf_path:
            if leaf_move_path.is_symlink() or not leaf_move_path.is_file():
                raise SystemExit("detached writer moved before a durable recovery-leaf intent")
            move_raw = leaf_move_path.read_bytes(); move = json.loads(move_raw)
            if (move_raw != canonical(move)
                    or move.get("schema") != "arc.recovery.detached-writer-leaf-move-intent.v1"
                    or move.get("freeze_plan_sha256") != freeze_sha
                    or move.get("boot_id") != boot_id or move.get("writer_pid") != writer_pid
                    or move.get("writer_start_ticks") != writer_start
                    or move.get("parent_scope_cgroup") != prepared_parent
                    or move.get("recovery_leaf_path") != recovery_leaf_path):
                raise SystemExit("detached writer recovery-leaf intent differs")
            leaf = pathlib.Path("/sys/fs/cgroup") / recovery_leaf_path.lstrip("/")
            details = leaf.lstat()
            if leaf.is_symlink() or not leaf.is_dir() or details.st_dev != prepared_parent["device"]:
                raise SystemExit("detached writer recovery leaf is unsafe")
            if leaf_receipt_path.exists() or leaf_receipt_path.is_symlink():
                if leaf_receipt_path.is_symlink() or not leaf_receipt_path.is_file():
                    raise SystemExit("detached writer recovery-leaf receipt is unsafe")
                receipt_raw = leaf_receipt_path.read_bytes(); receipt = json.loads(receipt_raw)
                expected_leaf = {"role": "writer", "path": recovery_leaf_path,
                                 "device": details.st_dev, "inode": details.st_ino}
                if (durable_intent_raw is None
                        or leaf_move_path.is_symlink() or not leaf_move_path.is_file()):
                    raise SystemExit("detached writer leaf receipt has no durable intent chain")
                move_raw = leaf_move_path.read_bytes()
                if (receipt_raw != canonical(receipt)
                        or receipt.get("schema") != "arc.recovery.fast-cgroup-progress.v1"
                        or receipt.get("freeze_intent_sha256")
                        != hashlib.sha256(durable_intent_raw).hexdigest()
                        or receipt.get("leaf_move_intent_sha256") != hashlib.sha256(move_raw).hexdigest()
                        or receipt.get("cgroup") != expected_leaf
                        or receipt.get("parent_scope_cgroup") != prepared_parent
                        or receipt.get("recovery_leaf_path") != recovery_leaf_path
                        or receipt.get("writer_pid") != writer_pid
                        or receipt.get("writer_start_ticks") != writer_start
                        or receipt.get("observed_local_freeze") != 1
                        or receipt.get("observed_frozen") is not True
                        or receipt.get("observed_populated") is not True):
                    raise SystemExit("detached writer recovery-leaf receipt differs")
                events = dict(line.split(" ", 1) for line in leaf.joinpath(
                    "cgroup.events"
                ).read_text(encoding="ascii").splitlines())
                if (leaf.joinpath("cgroup.freeze").read_text(encoding="ascii").strip() != "1"
                        or events.get("frozen") != "1" or events.get("populated") != "1"
                        or cgroup_subtree_pids(recovery_leaf_path) != [writer_pid]):
                    raise SystemExit("detached writer recovery leaf is not independently frozen and sealed")
                leaf_sealed = True
        else:
            raise SystemExit("detached writer is outside its sealed parent or recovery leaf")
        scope_unit = pathlib.PurePosixPath(prepared_parent["path"]).name
        scope_property = lambda name: subprocess.check_output(
            ["systemctl", "show", scope_unit, f"--property={name}", "--value"], text=True,
        ).strip()
        active_scope = scope_property("ActiveState") == "active"
        if detached_parent_terminal and active_scope:
            raise SystemExit("detached writer scope reactivated after terminal leaf-sealed state")
        safety_path = globals().get("scope_safety_path")
        if safety_path is None and durable_fast_writer.get("scope_runtime_safety_path"):
            safety_path = pathlib.Path(durable_fast_writer["scope_runtime_safety_path"])
        safety_bytes = globals().get("scope_safety_bytes")
        safety_sources = globals().get("scope_safety_sources") or durable_fast_writer.get("scope_runtime_sources")
        safety_properties = globals().get("scope_safety_properties") or durable_fast_writer.get("scope_properties")
        safety_sha = durable_fast_writer.get("scope_runtime_safety_sha256")
        if safety_bytes is not None:
            safety_sha = hashlib.sha256(safety_bytes).hexdigest()
        if safety_path is not None:
            try: safety_details = safety_path.lstat()
            except FileNotFoundError: raise SystemExit("detached writer scope runtime safety disappeared")
            if (safety_path.is_symlink() or not stat.S_ISREG(safety_details.st_mode)
                    or safety_details.st_uid != 0 or safety_details.st_gid != 0
                    or safety_details.st_mode & 0o022
                    or not re.fullmatch(r"[0-9a-f]{64}", safety_sha or "")
                    or digest(safety_path) != safety_sha):
                raise SystemExit("detached writer scope runtime safety file differs")
        if active_scope and (scope_property("Names") != scope_unit
                or scope_property("ControlGroup") != prepared_parent["path"]
                or scope_property("Job") not in {"", "0"}
                or scope_property("FreezerState") not in {"running", "frozen"}):
            raise SystemExit("detached writer active scope lifecycle differs")
        if active_scope and safety_path is not None and (
            scope_property("DefaultDependencies") != "no"
            or scope_property("RefuseManualStop") != "yes"
            or scope_property("IgnoreOnIsolate") != "yes"
            or scope_property("StopWhenUnneeded") != "no"
            or scope_property("BindsTo") or scope_property("PartOf")
            or scope_property("PropagatesStopTo")
            or scope_property("Conflicts") or scope_property("Before")
            or scope_property("Upholds")
            or scope_property("OnFailure") or scope_property("OnSuccess")
            or scope_property("StopPropagatedFrom")
            or scope_property("BoundBy") or scope_property("ConflictedBy")
            or scope_property("UpheldBy")
            or scope_property("OnFailureOf") or scope_property("OnSuccessOf")
            or scope_property("SuccessAction") != "none"
            or scope_property("FailureAction") != "none"
            or scope_property("JobTimeoutAction") != "none"
            or scope_property("KillMode") != "process"
            or scope_property("SendSIGKILL") != "no"
            or scope_property("SendSIGHUP") != "no"
            or scope_property("OOMPolicy") != "continue"
            or scope_property("RuntimeMaxUSec") != "infinity"
            or scope_property("TimeoutStopUSec") != "infinity"
        ):
            raise SystemExit("detached writer scope runtime safety differs")
        if not active_scope:
            sealed_invocation = (
                safety_properties.get("InvocationID") if isinstance(safety_properties, dict) else None
            ) or durable_fast_writer.get("scope_invocation_id")
            if (not leaf_sealed or not re.fullmatch(r"[0-9a-f]{32}", sealed_invocation or "")
                    or scope_property("ActiveState") not in {"inactive", "failed"}
                    or scope_property("MainPID") not in {"", "0"}
                    or scope_property("Job") not in {"", "0"}
                    or scope_property("InvocationID") not in {"", sealed_invocation}
                    or scope_property("ControlGroup") not in {"", prepared_parent["path"]}):
                raise SystemExit("detached writer terminal scope is not leaf-sealed/provenance-safe")
            detached_parent_terminal = True
        if active_scope and safety_sources is not None:
            raw = subprocess.check_output(["systemctl", "cat", scope_unit])
            headers = [value.decode("utf-8") for value in re.findall(rb"(?m)^# (/[^\n]+)$", raw)]
            current_sources = []
            for source_raw in headers:
                source = pathlib.Path(source_raw)
                details = source.lstat()
                if (source.is_symlink() or not stat.S_ISREG(details.st_mode)
                        or details.st_uid != 0 or details.st_gid != 0
                        or details.st_mode & 0o022):
                    raise SystemExit("detached writer scope source is unsafe")
                current_sources.append({"path": source_raw, "sha256": digest(source)})
            if current_sources != safety_sources:
                raise SystemExit("detached writer scope source manifest changed")
        if active_scope and safety_properties is not None:
            for name, expected in safety_properties.items():
                observed = scope_property(name) or ("0" if name == "Job" else "")
                if observed != expected:
                    raise SystemExit(f"detached writer scope property changed: {name}")
    else:
        raise SystemExit("writer supervision mode is unsupported")
    validate_supervisor_members(supervisor_cgroup)
    validate_unit_projection()
    return supervisor_cgroup, writer_cgroup

supervisor_cgroup, writer_location = validate_identities()
freeze_targets = [cgroup_entry("supervisor", supervisor_cgroup)]
if writer_mode == "detached-root-session":
    freeze_targets.append(prepared_parent)
for helper_pid in ancestor_pids():
    helper_cgroup = unified_cgroup(helper_pid)
    for entry in freeze_targets:
        target = entry["path"].rstrip("/")
        if helper_cgroup == target or helper_cgroup.startswith(target + "/"):
            raise SystemExit("recovery helper or ancestor is inside a freeze target")
if writer_mode == "detached-root-session" and cgroup_subtree_pids(prepared_parent["path"]) != [writer_pid]:
    raise SystemExit("detached writer is not the sole process in its root-session subtree")

# Prove the volatile, high-priority activation-gate substrate before freezing
# anything. A submount or UnitPath priority drift is a preflight rejection, not
# a failure discovered after the live writer is already frozen.
unit_paths = subprocess.check_output(
    ["systemctl", "show", "--property=UnitPath", "--value"], text=True,
).split()
required_unit_paths = ("/etc/systemd/system.control", "/run/systemd/system.control", "/etc/systemd/system")
if (len(unit_paths) != len(set(unit_paths)) or any(path not in unit_paths for path in required_unit_paths)
        or not (unit_paths.index(required_unit_paths[0]) < unit_paths.index(required_unit_paths[1])
                < unit_paths.index(required_unit_paths[2]))):
    raise SystemExit(f"systemd UnitPath priority is unsupported: {unit_paths}")
mount_rows = []
for line in pathlib.Path("/proc/self/mountinfo").read_text(encoding="ascii").splitlines():
    left, separator, right = line.partition(" - "); fields = left.split(); after = right.split()
    if separator and len(fields) > 4 and len(after) >= 2:
        mount_rows.append((fields[0], fields[2], fields[4], after[0], after[1]))
def deepest_mount(target):
    rows = [row for row in mount_rows if target == row[2] or target.startswith(row[2].rstrip("/") + "/")]
    if not rows: raise SystemExit(f"no mount covers recovery control path: {target}")
    return max(rows, key=lambda row: len(row[2]))
run_mounts = {deepest_mount(path) for path in ("/run", "/run/systemd", "/run/systemd/system.control")}
run_mount = next(iter(run_mounts)) if len(run_mounts) == 1 else None
if run_mount is None or run_mount[2] != "/run" or run_mount[3] != "tmpfs":
    raise SystemExit(f"recovery control paths are not on one exact /run tmpfs: {run_mounts}")
for legacy_unit in ("arc-self-heal.service", "arc-node.service", "arc-node-update.service", "arc-node-update.timer"):
    for persistent in (
        pathlib.Path(f"/etc/systemd/system.control/{legacy_unit}"),
        pathlib.Path(f"/etc/systemd/system.control/{legacy_unit}.d"),
        pathlib.Path(f"/run/systemd/system.control/{legacy_unit}.d"),
    ):
        if persistent.exists() or persistent.is_symlink():
            raise SystemExit(f"persistent high-priority legacy control override exists: {legacy_unit}")

scope_safety_path = None
scope_safety_bytes = None
scope_safety_sources = None
scope_safety_properties = None
scope_unit = None
scope_invocation_id = None
if writer_mode == "detached-root-session":
    scope_unit = pathlib.PurePosixPath(prepared_parent["path"]).name
    scope_safety_path = pathlib.Path(f"/run/systemd/system.control/{scope_unit}.d/zzzy-arc-recovery-writer-scope-safety.conf")
    scope_safety_bytes = b"[Unit]\nDefaultDependencies=no\nRefuseManualStop=yes\nIgnoreOnIsolate=yes\nStopWhenUnneeded=no\nBindsTo=\nPartOf=\nPropagatesStopTo=\nConflicts=\nUpholds=\nOnFailure=\nOnSuccess=\nFailureAction=none\nSuccessAction=none\nJobTimeoutAction=none\n\n[Scope]\nKillMode=process\nSendSIGKILL=no\nSendSIGHUP=no\nOOMPolicy=continue\nRuntimeMaxSec=infinity\nTimeoutStopSec=infinity\n"
    flags = os.O_RDONLY | getattr(os, "O_DIRECTORY", 0) | getattr(os, "O_NOFOLLOW", 0)
    for persistent in (
        pathlib.Path(f"/etc/systemd/system.control/{scope_unit}"),
        pathlib.Path(f"/etc/systemd/system.control/{scope_unit}.d"),
    ):
        if persistent.exists() or persistent.is_symlink():
            raise SystemExit("persistent high-priority writer scope override exists")
    systemd_root = os.open("/run/systemd", flags)
    try:
        try: control_details = os.stat("system.control", dir_fd=systemd_root, follow_symlinks=False)
        except FileNotFoundError:
            os.mkdir("system.control", 0o755, dir_fd=systemd_root)
            control_details = os.stat("system.control", dir_fd=systemd_root, follow_symlinks=False)
        if (not stat.S_ISDIR(control_details.st_mode) or control_details.st_uid != 0
                or control_details.st_gid != 0 or control_details.st_mode & 0o022):
            raise SystemExit("runtime high-priority systemd control directory is unsafe")
        runtime_root = os.open("system.control", flags, dir_fd=systemd_root)
        try:
            directory_name = f"{scope_unit}.d"
            try: details = os.stat(directory_name, dir_fd=runtime_root, follow_symlinks=False)
            except FileNotFoundError:
                os.mkdir(directory_name, 0o755, dir_fd=runtime_root)
                details = os.stat(directory_name, dir_fd=runtime_root, follow_symlinks=False)
            if (not stat.S_ISDIR(details.st_mode) or details.st_uid != 0 or details.st_gid != 0
                    or details.st_mode & 0o022): raise SystemExit("writer scope drop-in directory is unsafe")
            child = os.open(directory_name, flags, dir_fd=runtime_root)
            try:
                name = scope_safety_path.name
                try: current = os.stat(name, dir_fd=child, follow_symlinks=False)
                except FileNotFoundError: current = None
                if current is None:
                    temporary = f".{name}.partial.{secrets.token_hex(8)}"
                    descriptor = os.open(temporary, os.O_WRONLY | os.O_CREAT | os.O_EXCL | getattr(os, "O_NOFOLLOW", 0), 0o444, dir_fd=child)
                    try:
                        os.write(descriptor, scope_safety_bytes); os.fchmod(descriptor, 0o444); os.fsync(descriptor)
                    finally: os.close(descriptor)
                    os.rename(temporary, name, src_dir_fd=child, dst_dir_fd=child)
                descriptor = os.open(name, os.O_RDONLY | getattr(os, "O_NOFOLLOW", 0), dir_fd=child)
                try:
                    if os.read(descriptor, len(scope_safety_bytes) + 1) != scope_safety_bytes:
                        raise SystemExit("writer scope runtime safety bytes differ")
                    os.fsync(descriptor)
                finally: os.close(descriptor)
                os.fsync(child)
            finally: os.close(child)
            os.fsync(runtime_root)
        finally: os.close(runtime_root)
        os.fsync(systemd_root)
    finally: os.close(systemd_root)
    terminal_writer_contract = None
    if detached_parent_terminal:
        # PID1 may have discarded the transient scope source after stopping its
        # parent slice.  At this one-way phase, use only the canonical intent's
        # sealed provenance plus the still root-owned runtime safety file; do
        # not daemon-reload or project a dead unit as though it were active.
        terminal_intent_raw = intent_path.read_bytes(); terminal_intent = json.loads(terminal_intent_raw)
        terminal_writer_contract = terminal_intent.get("writer", {})
        if (terminal_intent_raw != canonical(terminal_intent)
                or terminal_intent.get("schema") != "arc.recovery.fast-cgroup-freeze-intent.v1"
                or terminal_writer_contract.get("parent_scope_cgroup") != prepared_parent
                or terminal_writer_contract.get("recovery_leaf_path") != recovery_leaf_path
                or terminal_writer_contract.get("scope_runtime_safety_path") != str(scope_safety_path)
                or terminal_writer_contract.get("scope_runtime_safety_sha256")
                != hashlib.sha256(scope_safety_bytes).hexdigest()):
            raise SystemExit("terminal detached scope intent projection differs")
        scope_safety_sources = terminal_writer_contract.get("scope_runtime_sources")
    else:
        subprocess.check_call(["systemctl", "daemon-reload"])
        raw = subprocess.check_output(["systemctl", "cat", scope_unit])
        headers = [value.decode("utf-8") for value in re.findall(rb"(?m)^# (/[^\n]+)$", raw)]
        if not headers or len(headers) != len(set(headers)) or str(scope_safety_path) not in headers:
            raise SystemExit("detached writer scope source manifest is incomplete")
        scope_safety_sources = []
        for source_raw in headers:
            source = pathlib.Path(source_raw); details = source.lstat()
            if (source.is_symlink() or not stat.S_ISREG(details.st_mode)
                    or details.st_uid != 0 or details.st_gid != 0
                    or details.st_mode & 0o022):
                raise SystemExit("detached writer scope source is unsafe")
            scope_safety_sources.append({"path": source_raw, "sha256": digest(source)})
    scope_property_names = (
        "Names", "Id", "Following", "LoadState", "Transient", "FragmentPath",
        "ActiveState", "SubState", "Job", "InvocationID", "ControlGroup", "Controller",
        "DefaultDependencies", "RefuseManualStop", "IgnoreOnIsolate", "StopWhenUnneeded", "Slice", "Requires",
        "Wants", "RequiresMountsFor", "Conflicts", "Before", "After", "BindsTo", "PartOf",
        "Upholds", "PropagatesStopTo", "StopPropagatedFrom", "BoundBy", "ConflictedBy",
        "UpheldBy", "OnFailure", "OnSuccess",
        "OnFailureOf", "OnSuccessOf", "SuccessAction", "FailureAction", "JobTimeoutAction",
        "KillMode", "SendSIGKILL", "SendSIGHUP", "FinalKillSignal", "OOMPolicy",
        "RuntimeMaxUSec", "TimeoutStopUSec",
    )
    scope_prop = lambda name: subprocess.check_output(
        ["systemctl", "show", scope_unit, f"--property={name}", "--value"], text=True,
    ).strip()
    scope_safety_properties = (
        terminal_writer_contract.get("scope_properties") if terminal_writer_contract is not None else {
            name: (scope_prop(name) or ("0" if name == "Job" else ""))
            for name in scope_property_names
        }
    )
    if not isinstance(scope_safety_properties, dict):
        raise SystemExit("detached writer scope property contract is missing")
    exact_scope = scope_safety_properties
    mount_points = []
    for line in pathlib.Path("/proc/self/mountinfo").read_text(encoding="ascii").splitlines():
        fields = line.split()
        if len(fields) > 4 and ("/root" == fields[4] or "/root".startswith(fields[4].rstrip("/") + "/")):
            mount_points.append(fields[4])
    allowed_mount_units = {
        subprocess.check_output(["systemd-escape", "--path", "--suffix=mount", path], text=True).strip()
        for path in mount_points
    }
    if not allowed_mount_units:
        raise SystemExit("detached writer scope has no reviewed /root mount dependency")
    requires = set(exact_scope["Requires"].split())
    after = set(exact_scope["After"].split())
    fixed_after = {
        "user-runtime-dir@0.service", "user@0.service", "user-0.slice",
        "systemd-logind.service",
    }
    if (exact_scope["Names"] != scope_unit or exact_scope["Id"] != scope_unit
            or exact_scope["Following"] or exact_scope["LoadState"] != "loaded"
            or exact_scope["Transient"] != "yes"
            or exact_scope["FragmentPath"] != f"/run/systemd/transient/{scope_unit}"
            or exact_scope["ActiveState"] != "active"
            or exact_scope["SubState"] not in {"running", "abandoned"}
            or exact_scope["Job"] != "0" or exact_scope["ControlGroup"] != prepared_parent["path"]
            or exact_scope["Controller"] or exact_scope["DefaultDependencies"] != "no"
            or exact_scope["RefuseManualStop"] != "yes"
            or exact_scope["IgnoreOnIsolate"] != "yes" or exact_scope["StopWhenUnneeded"] != "no"
            or exact_scope["Slice"] != "user-0.slice"
            or "user-0.slice" not in requires
            or not (requires - {"user-0.slice"}).issubset(allowed_mount_units)
            or set(exact_scope["Wants"].split()) != {"user-runtime-dir@0.service", "user@0.service"}
            or set(exact_scope["RequiresMountsFor"].split()) != {"/root"}
            or exact_scope["Conflicts"] or exact_scope["Before"]
            or not fixed_after.issubset(after)
            or not (after - fixed_after).issubset(allowed_mount_units)
            or any(exact_scope[name] for name in (
                "BindsTo", "PartOf", "Upholds", "PropagatesStopTo", "StopPropagatedFrom",
                "BoundBy", "ConflictedBy", "UpheldBy", "OnFailure", "OnSuccess",
                "OnFailureOf", "OnSuccessOf",
            ))
            or exact_scope["SuccessAction"] != "none" or exact_scope["FailureAction"] != "none"
            or exact_scope["JobTimeoutAction"] != "none" or exact_scope["KillMode"] != "process"
            or exact_scope["SendSIGKILL"] != "no" or exact_scope["SendSIGHUP"] != "no"
            or exact_scope["FinalKillSignal"] != "9"
            or exact_scope["OOMPolicy"] != "continue" or exact_scope["RuntimeMaxUSec"] != "infinity"
            or exact_scope["TimeoutStopUSec"] != "infinity"):
        raise SystemExit(f"detached writer scope has an unreviewed lifecycle closure: {exact_scope}")
    validate_identities()
    scope_invocation_id = (
        terminal_writer_contract.get("scope_invocation_id") if terminal_writer_contract is not None
        else subprocess.check_output(
            ["systemctl", "show", scope_unit, "--property=InvocationID", "--value"], text=True,
        ).strip()
    )
    if not re.fullmatch(r"[0-9a-f]{32}", scope_invocation_id):
        raise SystemExit("detached writer scope InvocationID is malformed")

# `systemctl cat` reads backing files immediately, even before daemon-reload.
# Bind its pre-recovery projection before the exact recovery drop-in is written;
# retries reuse only this canonical durable value instead of hashing the newly
# installed barrier and falsely reporting drift.
if intent_path.exists():
    if intent_path.is_symlink() or not intent_path.is_file():
        raise SystemExit("fast cgroup-freeze intent is unsafe")
    existing_raw = intent_path.read_bytes()
    existing = json.loads(existing_raw)
    if existing_raw != canonical(existing):
        raise SystemExit("fast cgroup-freeze intent is not canonical")
    pre_recovery_unit_configuration_sha = existing.get("pre_recovery_unit_configuration_sha256")
    if not isinstance(pre_recovery_unit_configuration_sha, str) or not re.fullmatch(
        r"[0-9a-f]{64}", pre_recovery_unit_configuration_sha,
    ):
        raise SystemExit("fast cgroup-freeze intent has no pre-recovery unit projection")
else:
    pre_recovery_unit_configuration_sha = safety.get("pre_recovery_unit_configuration_sha256")
    if not isinstance(pre_recovery_unit_configuration_sha, str) or not re.fullmatch(
        r"[0-9a-f]{64}", pre_recovery_unit_configuration_sha,
    ):
        raise SystemExit("prefreeze runtime safety has no original unit projection")

intent = {
    "schema": "arc.recovery.fast-cgroup-freeze-intent.v1",
    "capture_id": capture_id, "node": node, "freeze_plan_sha256": freeze_sha,
    "boot_id": boot_id, "supervisor_unit": unit, "cgroups": freeze_targets,
    "pre_recovery_unit_configuration_sha256": pre_recovery_unit_configuration_sha,
    "prefreeze_runtime_safety_intent_sha256": hashlib.sha256(safety_raw).hexdigest(),
    "supervisor": {
        "pid": supervisor_pid, "start_ticks": supervisor_start,
        "executable_path": supervisor_executable_path,
        "executable_sha256": supervisor_executable_sha,
        "argv_sha256": supervisor_argv_sha, "context_sha256": supervisor_context_sha,
    },
    "writer": {
        "pid": writer_pid, "start_ticks": writer_start,
        "cgroup_sha256": writer_cgroup_sha, "supervision_mode": writer_mode,
        "executable_path": writer_executable_path,
        "executable_sha256": writer_executable_sha, "argv_sha256": writer_argv_sha,
        "scope_unit": scope_unit, "scope_invocation_id": scope_invocation_id,
        "scope_runtime_safety_path": None if scope_safety_path is None else str(scope_safety_path),
        "scope_runtime_safety_sha256": None if scope_safety_bytes is None else hashlib.sha256(scope_safety_bytes).hexdigest(),
        "scope_runtime_sources": scope_safety_sources,
        "scope_properties": scope_safety_properties,
        "parent_scope_cgroup": None if writer_mode == "systemd-unit" else prepared_parent,
        "recovery_leaf_path": None if writer_mode == "systemd-unit" else recovery_leaf_path,
    },
}
publish(intent_path, intent)

freeze_order = [entry["role"] for entry in freeze_targets]
intent_sha = hashlib.sha256(intent_path.read_bytes()).hexdigest()

def local_freeze(entry):
    base = exact_identity(entry, f"{entry['role']} local-freeze")
    value = base.joinpath("cgroup.freeze").read_text(encoding="ascii").strip()
    if value not in {"0", "1"}:
        raise SystemExit(f"cgroup.freeze has no exact local state: {entry['path']}")
    return int(value)

def write_local_freeze(entry, value):
    base = exact_identity(entry, f"{entry['role']} freezer target")
    directory = os.open(base, os.O_RDONLY | getattr(os, "O_DIRECTORY", 0) | getattr(os, "O_NOFOLLOW", 0))
    try:
        details = os.fstat(directory)
        if details.st_dev != entry["device"] or details.st_ino != entry["inode"]:
            raise SystemExit("opened cgroup differs from its sealed identity")
        freezer = os.open("cgroup.freeze", os.O_WRONLY | getattr(os, "O_NOFOLLOW", 0), dir_fd=directory)
        try:
            validate_identities()
            os.write(freezer, str(value).encode("ascii"))
        finally:
            os.close(freezer)
    finally:
        os.close(directory)

def wait_frozen(entry, value):
    deadline = time.monotonic() + 10
    while frozen_state(entry) != value:
        if time.monotonic() >= deadline:
            raise SystemExit(f"cgroup did not reach frozen={value}: {entry['path']}")
        time.sleep(0.01)

def load_leaf_receipt():
    if leaf_receipt_path.is_symlink() or not leaf_receipt_path.is_file():
        raise SystemExit("detached recovery-leaf receipt is missing")
    raw = leaf_receipt_path.read_bytes(); value = json.loads(raw)
    leaf = value.get("cgroup") if isinstance(value, dict) else None
    if (raw != canonical(value) or value.get("schema") != "arc.recovery.fast-cgroup-progress.v1"
            or value.get("freeze_intent_sha256") != intent_sha or value.get("role") != "writer"
            or value.get("parent_scope_cgroup") != prepared_parent
            or value.get("recovery_leaf_path") != recovery_leaf_path
            or value.get("writer_pid") != writer_pid or value.get("writer_start_ticks") != writer_start
            or value.get("observed_local_freeze") != 1 or value.get("observed_frozen") is not True
            or value.get("observed_populated") is not True):
        raise SystemExit("detached recovery-leaf receipt differs")
    exact_identity(leaf, "detached recovery leaf")
    return value, raw, leaf

def load_parent_release(leaf, leaf_raw):
    if parent_release_path.is_symlink() or not parent_release_path.is_file():
        raise SystemExit("detached writer parent-release receipt is missing")
    raw = parent_release_path.read_bytes(); value = json.loads(raw)
    if (raw != canonical(value)
            or value != {
                "schema": "arc.recovery.detached-writer-parent-release.v1",
                "freeze_intent_sha256": intent_sha,
                "leaf_sealed_receipt_sha256": hashlib.sha256(leaf_raw).hexdigest(),
                "parent_scope_cgroup": prepared_parent,
                "recovery_leaf": leaf,
                "parent_local_freeze": 0,
                "leaf_local_freeze": 1,
                "leaf_observed_frozen": True,
            }):
        raise SystemExit("detached writer parent-release receipt differs")
    return raw

def validate_complete_fast_marker():
    raw = frozen_path.read_bytes(); observed = json.loads(raw)
    if writer_mode == "detached-root-session":
        leaf_value, leaf_raw, leaf = load_leaf_receipt()
        release_raw = load_parent_release(leaf, leaf_raw)
        final_cgroups = [freeze_targets[0], leaf]
        progress = {
            "supervisor": hashlib.sha256((root / "02-supervisor-cgroup-frozen.json").read_bytes()).hexdigest(),
            "writer-parent": hashlib.sha256((root / "02-writer-parent-cgroup-frozen.json").read_bytes()).hexdigest(),
            "writer-leaf-move-intent": hashlib.sha256(leaf_move_path.read_bytes()).hexdigest(),
            "writer": hashlib.sha256(leaf_raw).hexdigest(),
            "writer-parent-release": hashlib.sha256(release_raw).hexdigest(),
        }
        if (local_freeze(leaf) != 1 or frozen_state(leaf) != 1
                or cgroup_subtree_pids(leaf["path"]) != [writer_pid]
                or local_freeze(prepared_parent) != 0
                or cgroup_subtree_pids(prepared_parent["path"]) != [writer_pid]):
            raise SystemExit("detached recovery leaf/parent state changed after sealing")
    else:
        leaf = None; final_cgroups = [freeze_targets[0]]
        progress = {
            "supervisor": hashlib.sha256((root / "02-supervisor-cgroup-frozen.json").read_bytes()).hexdigest(),
        }
    if (raw != canonical(observed)
            or observed.get("schema") != "arc.recovery.fast-cgroups-frozen.v1"
            or observed.get("freeze_intent_sha256") != intent_sha
            or observed.get("cgroups") != final_cgroups
            or observed.get("writer_parent_scope_cgroup") != (
                prepared_parent if writer_mode == "detached-root-session" else None
            )
            or observed.get("writer_recovery_leaf") != leaf
            or observed.get("freeze_order") != list(progress)
            or observed.get("per_cgroup_progress_sha256") != progress
            or observed.get("all_cgroups_frozen") is not True):
        raise SystemExit("fast-frozen cgroup marker differs")
    if (root / "50-cgroups-thaw-intent.json").exists() or (root / "50-cgroups-thawed.json").exists() or (root / "40-stable-inactive.json").exists():
        return
    if any(frozen_state(entry) != 1 for entry in final_cgroups):
        raise SystemExit("cgroup was unexpectedly thawed before durable thaw intent")
    validate_identities()

if frozen_path.exists() or frozen_path.is_symlink():
    if frozen_path.is_symlink() or not frozen_path.is_file():
        raise SystemExit("fast-frozen cgroup marker is unsafe")
    validate_complete_fast_marker()
    raise SystemExit(0)

parent_already_released = False
if writer_mode == "detached-root-session" and (
        parent_release_path.exists() or parent_release_path.is_symlink()):
    # A durable parent-release receipt is a one-way phase transition.  Retrying
    # must never write `1` back to the parent: the session scope may have gained
    # an unrelated member after the owned leaf was sealed.
    _leaf_value, _leaf_raw, released_leaf = load_leaf_receipt()
    load_parent_release(released_leaf, _leaf_raw)
    if (local_freeze(prepared_parent) != 0
            or local_freeze(released_leaf) != 1
            or frozen_state(released_leaf) != 1
            or unified_cgroup(writer_pid) != released_leaf["path"]
            or cgroup_subtree_pids(released_leaf["path"]) != [writer_pid]
            or cgroup_subtree_pids(prepared_parent["path"]) != [writer_pid]):
        raise SystemExit("released detached parent/leaf retry state differs")
    parent_already_released = True

for entry in freeze_targets:
    if entry["role"] == "writer-parent" and parent_already_released:
        continue
    if (entry["role"] == "writer-parent"
            and (leaf_receipt_path.exists() or leaf_receipt_path.is_symlink())):
        # A leaf receipt without the release receipt is recoverable only while
        # the whole parent subtree is still exactly the sealed writer in the
        # independently frozen owned leaf.  Prove that immediately before the
        # idempotent parent re-freeze.
        _leaf_value, _leaf_raw, pending_leaf = load_leaf_receipt()
        if (local_freeze(pending_leaf) != 1 or frozen_state(pending_leaf) != 1
                or unified_cgroup(writer_pid) != pending_leaf["path"]
                or cgroup_subtree_pids(pending_leaf["path"]) != [writer_pid]
                or cgroup_subtree_pids(prepared_parent["path"]) != [writer_pid]):
            raise SystemExit("detached parent retry gained unreviewed membership")
    base = pathlib.Path("/sys/fs/cgroup") / entry["path"].lstrip("/")
    write_local_freeze(entry, 1)
    wait_frozen(entry, 1)
    if entry["role"] == "supervisor" and subprocess.check_output(
        ["systemctl", "show", unit, "--property=FreezerState", "--value"], text=True,
    ).strip() not in {"running", "frozen"}:
        raise SystemExit("selected supervisor has an invalid advisory PID1 freezer state")
    if entry["role"] == "writer-parent" and writer_mode == "detached-root-session" and subprocess.check_output(
        ["systemctl", "show", pathlib.PurePosixPath(prepared_parent["path"]).name,
         "--property=FreezerState", "--value"], text=True,
    ).strip() not in {"running", "frozen"}:
        raise SystemExit("detached writer scope has an invalid advisory PID1 freezer state")
    publish(root / f"02-{entry['role']}-cgroup-frozen.json", {
        "schema": "arc.recovery.fast-cgroup-progress.v1",
        "freeze_intent_sha256": intent_sha,
        "role": entry["role"], "cgroup": entry,
        "freeze_order": freeze_order,
        "observed_frozen": True,
    })
    validate_identities()

if writer_mode == "detached-root-session":
    parent_progress_raw = (root / "02-writer-parent-cgroup-frozen.json").read_bytes()
    move_intent = {
        "schema": "arc.recovery.detached-writer-leaf-move-intent.v1",
        "freeze_plan_sha256": freeze_sha, "boot_id": boot_id,
        "freeze_intent_sha256": intent_sha,
        "writer_parent_frozen_receipt_sha256": hashlib.sha256(parent_progress_raw).hexdigest(),
        "writer_pid": writer_pid, "writer_start_ticks": writer_start,
        "writer_executable_sha256": writer_executable_sha,
        "writer_argv_sha256": writer_argv_sha,
        "parent_scope_cgroup": prepared_parent,
        "recovery_leaf_path": recovery_leaf_path,
        "parent_observed_frozen": True,
    }
    publish(leaf_move_path, move_intent)
    move_raw = leaf_move_path.read_bytes()
    parent_directory = os.open(
        prepared_parent_path,
        os.O_RDONLY | getattr(os, "O_DIRECTORY", 0) | getattr(os, "O_NOFOLLOW", 0),
    )
    try:
        current = os.fstat(parent_directory)
        if current.st_dev != prepared_parent["device"] or current.st_ino != prepared_parent["inode"]:
            raise SystemExit("opened detached parent differs before recovery-leaf creation")
        try:
            leaf_details = os.stat(leaf_name, dir_fd=parent_directory, follow_symlinks=False)
        except FileNotFoundError:
            os.mkdir(leaf_name, 0o755, dir_fd=parent_directory)
            leaf_details = os.stat(leaf_name, dir_fd=parent_directory, follow_symlinks=False)
        if (not stat.S_ISDIR(leaf_details.st_mode) or leaf_details.st_dev != prepared_parent["device"]):
            raise SystemExit("detached recovery leaf is not an exact cgroup directory")
        leaf_directory = os.open(
            leaf_name,
            os.O_RDONLY | getattr(os, "O_DIRECTORY", 0) | getattr(os, "O_NOFOLLOW", 0),
            dir_fd=parent_directory,
        )
        try:
            opened_leaf = os.fstat(leaf_directory)
            if opened_leaf.st_dev != leaf_details.st_dev or opened_leaf.st_ino != leaf_details.st_ino:
                raise SystemExit("opened detached recovery leaf differs")
            leaf = {"role": "writer", "path": recovery_leaf_path,
                    "device": opened_leaf.st_dev, "inode": opened_leaf.st_ino}
            exact_identity(leaf, "detached recovery leaf")
            leaf_root = pathlib.Path("/sys/fs/cgroup") / recovery_leaf_path.lstrip("/")
            child_dirs = [entry.name for entry in leaf_root.iterdir() if entry.is_dir()]
            if child_dirs:
                raise SystemExit(f"detached recovery leaf contains child cgroups: {child_dirs}")
            current_location = unified_cgroup(writer_pid)
            leaf_members = cgroup_subtree_pids(recovery_leaf_path)
            if current_location == prepared_parent["path"]:
                if leaf_members:
                    raise SystemExit("detached recovery leaf was populated before the sealed writer move")
                validate_identities()
                if local_freeze(prepared_parent) != 1 or frozen_state(prepared_parent) != 1:
                    raise SystemExit("detached parent thawed before the sealed writer move")
                # Set and verify the child's own freezer request before moving
                # the writer.  Effective `frozen 1` inherited from the parent
                # alone is not an independent safety barrier.
                freezer = os.open("cgroup.freeze", os.O_WRONLY | getattr(os, "O_NOFOLLOW", 0), dir_fd=leaf_directory)
                try:
                    os.write(freezer, b"1")
                finally:
                    os.close(freezer)
                if local_freeze(leaf) != 1:
                    raise SystemExit("detached recovery leaf local freeze did not arm before writer move")
                procs = os.open("cgroup.procs", os.O_WRONLY | getattr(os, "O_NOFOLLOW", 0), dir_fd=leaf_directory)
                try:
                    os.write(procs, str(writer_pid).encode("ascii"))
                finally:
                    os.close(procs)
            elif current_location == recovery_leaf_path and leaf_members == [writer_pid]:
                if local_freeze(leaf) != 1:
                    if local_freeze(prepared_parent) != 1 or frozen_state(prepared_parent) != 1:
                        raise SystemExit("detached writer is in a locally-thawed leaf without a frozen parent")
                    freezer = os.open("cgroup.freeze", os.O_WRONLY | getattr(os, "O_NOFOLLOW", 0), dir_fd=leaf_directory)
                    try:
                        os.write(freezer, b"1")
                    finally:
                        os.close(freezer)
            else:
                raise SystemExit("detached writer/leaf retry membership differs")
            validate_identities()
            if (unified_cgroup(writer_pid) != recovery_leaf_path
                    or cgroup_subtree_pids(recovery_leaf_path) != [writer_pid]
                    or cgroup_subtree_pids(prepared_parent["path"]) != [writer_pid]):
                raise SystemExit("detached writer did not move exclusively into the recovery leaf")
            if local_freeze(leaf) != 1:
                raise SystemExit("detached recovery leaf is not independently frozen after writer move")
        finally:
            os.close(leaf_directory)
    finally:
        os.close(parent_directory)
    wait_frozen(leaf, 1)
    if local_freeze(leaf) != 1 or cgroup_subtree_pids(leaf["path"]) != [writer_pid]:
        raise SystemExit("detached recovery leaf is not locally frozen with the exact writer")
    events = dict(line.split(" ", 1) for line in exact_identity(leaf, "detached recovery leaf").joinpath(
        "cgroup.events"
    ).read_text(encoding="ascii").splitlines())
    if events.get("frozen") != "1" or events.get("populated") != "1":
        raise SystemExit("detached recovery leaf is not frozen and populated")
    publish(leaf_receipt_path, {
        "schema": "arc.recovery.fast-cgroup-progress.v1",
        "freeze_intent_sha256": intent_sha,
        "leaf_move_intent_sha256": hashlib.sha256(move_raw).hexdigest(),
        "role": "writer", "cgroup": leaf,
        "parent_scope_cgroup": prepared_parent,
        "recovery_leaf_path": recovery_leaf_path,
        "writer_pid": writer_pid, "writer_start_ticks": writer_start,
        "freeze_order": ["supervisor", "writer-parent", "writer"],
        "observed_local_freeze": 1,
        "observed_frozen": True, "observed_populated": True,
    })
    leaf_raw = leaf_receipt_path.read_bytes()
    # The parent was frozen only to make leaf creation/reparenting atomic with
    # respect to the writer.  Once the child has its own local freeze bit and a
    # durable inode-bound receipt, release the parent.  Stopping user-0.slice
    # can no longer wake the writer because the child remains locally frozen.
    if local_freeze(prepared_parent) != 0:
        write_local_freeze(prepared_parent, 0)
    wait_frozen(prepared_parent, 0)
    if (local_freeze(leaf) != 1 or frozen_state(leaf) != 1
            or cgroup_subtree_pids(leaf["path"]) != [writer_pid]):
        raise SystemExit("detached recovery leaf changed while releasing its parent")
    publish(parent_release_path, {
        "schema": "arc.recovery.detached-writer-parent-release.v1",
        "freeze_intent_sha256": intent_sha,
        "leaf_sealed_receipt_sha256": hashlib.sha256(leaf_raw).hexdigest(),
        "parent_scope_cgroup": prepared_parent, "recovery_leaf": leaf,
        "parent_local_freeze": 0, "leaf_local_freeze": 1,
        "leaf_observed_frozen": True,
    })
    release_raw = parent_release_path.read_bytes()
    final_cgroups = [freeze_targets[0], leaf]
    progress_sha256 = {
        "supervisor": hashlib.sha256((root / "02-supervisor-cgroup-frozen.json").read_bytes()).hexdigest(),
        "writer-parent": hashlib.sha256(parent_progress_raw).hexdigest(),
        "writer-leaf-move-intent": hashlib.sha256(move_raw).hexdigest(),
        "writer": hashlib.sha256(leaf_raw).hexdigest(),
        "writer-parent-release": hashlib.sha256(release_raw).hexdigest(),
    }
else:
    leaf = None
    final_cgroups = [freeze_targets[0]]
    progress_sha256 = {
        "supervisor": hashlib.sha256((root / "02-supervisor-cgroup-frozen.json").read_bytes()).hexdigest(),
    }
validate_identities()
publish(frozen_path, {
    "schema": "arc.recovery.fast-cgroups-frozen.v1",
    "freeze_intent_sha256": intent_sha,
    "cgroups": final_cgroups,
    "writer_parent_scope_cgroup": prepared_parent if writer_mode == "detached-root-session" else None,
    "writer_recovery_leaf": leaf,
    "freeze_order": list(progress_sha256),
    "per_cgroup_progress_sha256": progress_sha256,
    "all_cgroups_frozen": True,
})
PY
}

pre_fence_quiesce_phase() {
    local phase="$1" root="$2" capture_id="$3" node="$4" freeze_sha="$5"
    local boot_id="$6" writer_supervision_mode="$7" unit="$8"
    local supervisor_pid="$9" supervisor_start_ticks="${10}"
    local supervisor_executable_path="${11}" supervisor_executable_sha="${12}"
    local supervisor_argv_sha="${13}" supervisor_context_sha="${14}"
    local writer_pid="${15}" writer_start_ticks="${16}" writer_cgroup_sha="${17}"
    local writer_executable_path="${18}" writer_executable_sha="${19}" writer_argv_sha="${20}"
    python3 - "$phase" "$root" "$capture_id" "$node" "$freeze_sha" "$boot_id" \
        "$writer_supervision_mode" "$unit" "$supervisor_pid" "$supervisor_start_ticks" \
        "$supervisor_executable_path" "$supervisor_executable_sha" "$supervisor_argv_sha" \
        "$supervisor_context_sha" "$writer_pid" "$writer_start_ticks" "$writer_cgroup_sha" \
        "$writer_executable_path" "$writer_executable_sha" "$writer_argv_sha" <<'PY'
import hashlib
import json
import os
import pathlib
import re
import stat
import subprocess
import sys
import time

(phase, root_raw, capture_id, node, freeze_sha, boot_id, writer_mode, unit,
 supervisor_pid_raw, supervisor_start_raw, supervisor_executable_path,
 supervisor_executable_sha, supervisor_argv_sha, supervisor_context_sha,
 writer_pid_raw, writer_start_raw, writer_cgroup_sha, writer_executable_path,
 writer_executable_sha, writer_argv_sha) = sys.argv[1:]
root = pathlib.Path(root_raw)
supervisor_pid, supervisor_start = int(supervisor_pid_raw), int(supervisor_start_raw)
writer_pid, writer_start = int(writer_pid_raw), int(writer_start_raw)
intent_path = root / "04-pre-fence-quiesce-intent.json"
frozen_path = root / "05-cgroups-frozen.json"

def canonical(value):
    return (json.dumps(value, sort_keys=True, separators=(",", ":")) + "\n").encode()

def publish(path, value):
    payload = canonical(value)
    if path.exists():
        if path.is_symlink() or path.read_bytes() != payload:
            raise SystemExit(f"durable pre-fence event differs: {path.name}")
        return
    temporary = path.with_name(f".{path.name}.partial")
    if temporary.exists() or temporary.is_symlink():
        if temporary.is_symlink() or not temporary.is_file():
            raise SystemExit(f"unsafe pre-fence event partial: {path.name}")
        temporary.unlink()
    descriptor = os.open(temporary, os.O_WRONLY | os.O_CREAT | os.O_EXCL | getattr(os, "O_NOFOLLOW", 0), 0o400)
    with os.fdopen(descriptor, "wb") as handle:
        handle.write(payload); handle.flush(); os.fsync(handle.fileno())
    os.rename(temporary, path)
    descriptor = os.open(root, os.O_RDONLY | getattr(os, "O_DIRECTORY", 0))
    try:
        os.fsync(descriptor)
    finally:
        os.close(descriptor)

def proc_start(pid):
    raw = pathlib.Path(f"/proc/{pid}/stat").read_text(encoding="ascii")
    end = raw.rfind(")")
    fields = raw[end + 2:].split()
    if end < 0 or len(fields) < 20:
        raise SystemExit("process stat is truncated during pre-fence quiescence")
    return int(fields[19])

def unified_cgroup(pid):
    raw = pathlib.Path(f"/proc/{pid}/cgroup").read_bytes()
    rows = [line.split(":", 2)[2] for line in raw.decode("utf-8").splitlines()
            if line.startswith("0::")]
    if (len(rows) != 1 or not re.fullmatch(r"/[A-Za-z0-9._@/-]+", rows[0])
            or ".." in pathlib.PurePosixPath(rows[0]).parts):
        raise SystemExit("network-quarantine process cgroup is ambiguous")
    return rows[0], raw

def exact_cgroup(role, path):
    base = pathlib.Path("/sys/fs/cgroup") / path.lstrip("/")
    details = base.lstat()
    if base.is_symlink() or not base.is_dir():
        raise SystemExit(f"network-quarantine {role} cgroup is unsafe")
    members = set()
    for current, directories, _files in os.walk(base, followlinks=False):
        directories.sort()
        current_path = pathlib.Path(current)
        if current_path.is_symlink():
            raise SystemExit(f"network-quarantine {role} cgroup contains a symlink")
        procs = current_path / "cgroup.procs"
        if procs.is_symlink() or not procs.is_file():
            raise SystemExit(f"network-quarantine {role} cgroup inventory is unsafe")
        members.update(int(value) for value in procs.read_text(encoding="ascii").splitlines())
    return {"role": role, "path": path, "device": details.st_dev,
            "inode": details.st_ino}, sorted(members)

def proc_ppid(pid):
    raw = pathlib.Path(f"/proc/{pid}/stat").read_text(encoding="ascii")
    end = raw.rfind(")")
    fields = raw[end + 2:].split()
    if end < 0 or len(fields) < 2:
        raise SystemExit("process stat is truncated during ancestry verification")
    return int(fields[1])

def digest(path):
    value = hashlib.sha256()
    with open(path, "rb") as handle:
        for chunk in iter(lambda: handle.read(1024 * 1024), b""):
            value.update(chunk)
    return value.hexdigest()

def unified_cgroup(pid):
    rows = []
    for line in pathlib.Path(f"/proc/{pid}/cgroup").read_text(encoding="utf-8").splitlines():
        hierarchy, controllers, path = line.split(":", 2)
        if hierarchy == "0" and controllers == "":
            rows.append(path)
    if len(rows) != 1 or not re.fullmatch(r"/[A-Za-z0-9._@/-]+", rows[0]) or ".." in rows[0]:
        raise SystemExit("process unified cgroup is missing or unsafe")
    return rows[0]

def cgroup_root(path):
    candidate = pathlib.Path("/sys/fs/cgroup") / path.lstrip("/")
    if path == "/" or candidate.is_symlink() or not candidate.is_dir():
        raise SystemExit("refusing root, missing, or symlink cgroup")
    return candidate

def cgroup_identity(path):
    details = cgroup_root(path).stat()
    return {"path": path, "device": details.st_dev, "inode": details.st_ino}

def systemctl_value(prop):
    return subprocess.check_output(
        ["systemctl", "show", unit, f"--property={prop}", "--value"], text=True,
    ).strip()

def process_identity(pid):
    proc = pathlib.Path("/proc") / str(pid)
    return {
        "pid": pid,
        "start_ticks": proc_start(pid),
        "ppid": proc_ppid(pid),
        "executable_path": os.readlink(proc / "exe"),
        "executable_sha256": digest(proc / "exe"),
        "argv_sha256": hashlib.sha256(proc.joinpath("cmdline").read_bytes()).hexdigest(),
        "cgroup": unified_cgroup(pid),
    }

def subtree_pids(path):
    base = cgroup_root(path)
    rows = []
    for current, dirs, _files in os.walk(base, followlinks=False):
        dirs.sort()
        current_path = pathlib.Path(current)
        if current_path.is_symlink():
            raise SystemExit("cgroup subtree contains a symlink directory")
        procs = current_path / "cgroup.procs"
        if not procs.is_file() or procs.is_symlink():
            raise SystemExit("cgroup subtree process inventory is unsafe")
        pids = sorted({int(value) for value in procs.read_text(encoding="ascii").splitlines()})
        rows.append({
            "relative": current_path.relative_to(base).as_posix(),
            "device": current_path.stat().st_dev,
            "inode": current_path.stat().st_ino,
            "pids": pids,
        })
    return rows

def all_subtree_pids(path):
    return sorted({pid for row in subtree_pids(path) for pid in row["pids"]})

def is_descendant(pid, ancestor):
    observed = set()
    current = pid
    while current > 1 and current not in observed:
        if current == ancestor:
            return True
        observed.add(current)
        try:
            current = proc_ppid(current)
        except FileNotFoundError:
            return False
    return current == ancestor

def duration_seconds(value):
    match = re.fullmatch(r"([0-9]+(?:\.[0-9]+)?)([smhd]?)", value)
    if not match:
        return None
    scale = {"": 1, "s": 1, "m": 60, "h": 3600, "d": 86400}[match.group(2)]
    return float(match.group(1)) * scale

def validate_supervisor_members(path):
    allowed = {supervisor_pid}
    if writer_mode == "systemd-unit":
        allowed.add(writer_pid)
    for pid in all_subtree_pids(path):
        if pid in allowed:
            continue
        if not is_descendant(pid, supervisor_pid):
            raise SystemExit(f"unreviewed non-descendant exists in supervisor cgroup: pid={pid}")
        proc = pathlib.Path("/proc") / str(pid)
        try:
            argv = [part.decode("utf-8") for part in proc.joinpath("cmdline").read_bytes().rstrip(b"\0").split(b"\0")]
            executable = os.path.realpath(os.readlink(proc / "exe"))
        except (FileNotFoundError, ProcessLookupError, UnicodeDecodeError):
            raise SystemExit("supervisor cgroup member changed during pre-fence validation")
        seconds = duration_seconds(argv[1]) if len(argv) == 2 else None
        if pathlib.Path(executable).name != "sleep" or seconds is None or seconds > 60:
            raise SystemExit(f"unreviewed active child exists in supervisor cgroup: pid={pid}")

def helper_outside(paths):
    current = os.getpid()
    observed = set()
    while current > 1 and current not in observed:
        observed.add(current)
        helper_cgroup = unified_cgroup(current)
        for target in paths:
            if helper_cgroup == target or helper_cgroup.startswith(target.rstrip("/") + "/"):
                raise SystemExit("recovery helper or ancestor is inside a cgroup it would freeze")
        try:
            current = proc_ppid(current)
        except FileNotFoundError:
            break

def masks(pid):
    task_rows = []
    any_unblocked = False
    for task in sorted(pathlib.Path(f"/proc/{pid}/task").iterdir(), key=lambda item: int(item.name)):
        values = {}
        for line in task.joinpath("status").read_text(encoding="ascii").splitlines():
            key, separator, value = line.partition(":")
            if separator and key in {"SigIgn", "SigPnd", "ShdPnd", "SigBlk", "SigCgt", "TracerPid", "NSpid"}:
                values[key] = value.strip().lower()
        if set(values) != {"SigIgn", "SigPnd", "ShdPnd", "SigBlk", "SigCgt", "TracerPid", "NSpid"}:
            raise SystemExit("task signal masks are incomplete")
        term_bit = 1 << (15 - 1)
        if values["TracerPid"] != "0":
            raise SystemExit("frozen target is ptraced")
        if pathlib.Path(f"/proc/{task.name}/stat").read_text(encoding="ascii").split(")", 1)[1].split()[0] in {"T", "t"}:
            raise SystemExit("frozen target task was job-control stopped or traced before cgroup freeze")
        if int(values["SigIgn"], 16) & term_bit:
            raise SystemExit("frozen target ignores SIGTERM")
        if int(values["SigPnd"], 16) & term_bit or int(values["ShdPnd"], 16) & term_bit:
            raise SystemExit("SIGTERM was already pending at the frozen baseline")
        if not int(values["SigBlk"], 16) & term_bit:
            any_unblocked = True
        namespace_pids = values["NSpid"].split()
        if not namespace_pids or not all(value.isdigit() for value in namespace_pids):
            raise SystemExit("frozen target namespace PID inventory is malformed")
        if namespace_pids[-1] == "1" and not int(values["SigCgt"], 16) & term_bit:
            raise SystemExit("PID-namespace init target has no caught SIGTERM disposition")
        task_rows.append({"tid": int(task.name), **values})
    if not task_rows:
        raise SystemExit("frozen target has no task inventory")
    if not any_unblocked:
        raise SystemExit("SIGTERM is blocked in every frozen target task")
    return task_rows

def load_event(path, schema):
    if path.is_symlink() or not path.is_file():
        raise SystemExit(f"required fast-freeze event is missing: {path.name}")
    raw = path.read_bytes(); value = json.loads(raw)
    if raw != canonical(value) or value.get("schema") != schema:
        raise SystemExit(f"fast-freeze event differs: {path.name}")
    return value, raw

fast_intent, fast_intent_raw = load_event(
    root / "02-fast-cgroup-freeze-intent.json", "arc.recovery.fast-cgroup-freeze-intent.v1",
)
fast_frozen, fast_frozen_raw = load_event(
    root / "03-fast-cgroups-frozen.json", "arc.recovery.fast-cgroups-frozen.v1",
)
roles = fast_frozen.get("cgroups")
if (not isinstance(roles, list)
        or [entry.get("role") for entry in roles] not in (["supervisor"], ["supervisor", "writer"])):
    raise SystemExit("fast-frozen final role inventory differs")
for entry in roles:
    if (not isinstance(entry, dict) or set(entry) != {"role", "path", "device", "inode"}
            or {"path": entry["path"], "device": entry["device"], "inode": entry["inode"]}
            != cgroup_identity(entry["path"])):
        raise SystemExit("fast-frozen cgroup identity changed before pre-fence quiescence")
parent_scope_cgroup = fast_frozen.get("writer_parent_scope_cgroup")
recovery_leaf = fast_frozen.get("writer_recovery_leaf")

def local_freeze(entry):
    value = cgroup_root(entry["path"]).joinpath("cgroup.freeze").read_text(encoding="ascii").strip()
    if value not in {"0", "1"}: raise SystemExit("cgroup has no exact local freeze state")
    return int(value)

def validate_fast():
    if pathlib.Path("/proc/sys/kernel/random/boot_id").read_text().strip() != boot_id:
        raise SystemExit("host rebooted before the persistent fence was proven")
    if proc_start(supervisor_pid) != supervisor_start or proc_start(writer_pid) != writer_start:
        raise SystemExit("sealed supervisor/writer PID changed before pre-fence quiescence")
    if pathlib.Path(f"/proc/{supervisor_pid}/stat").read_text(encoding="ascii").split(")", 1)[1].split()[0] in {"T", "t"}:
        raise SystemExit("sealed supervisor was job-control stopped before cgroup freeze")
    if pathlib.Path(f"/proc/{writer_pid}/stat").read_text(encoding="ascii").split(")", 1)[1].split()[0] in {"T", "t"}:
        raise SystemExit("sealed writer was job-control stopped before cgroup freeze")
    if systemctl_value("MainPID") != str(supervisor_pid):
        raise SystemExit("sealed supervisor MainPID changed before pre-fence quiescence")
    if systemctl_value("ActiveState") != "active" or systemctl_value("Job"):
        raise SystemExit("sealed supervisor has a pending job or non-active state before quiescence")
    supervisor_cgroup = unified_cgroup(supervisor_pid)
    writer_cgroup = unified_cgroup(writer_pid)
    if systemctl_value("ControlGroup") != supervisor_cgroup or unit not in supervisor_cgroup:
        raise SystemExit("sealed supervisor cgroup differs before quiescence")
    if writer_mode == "systemd-unit":
        if (writer_cgroup != supervisor_cgroup
                or hashlib.sha256(pathlib.Path(f"/proc/{writer_pid}/cgroup").read_bytes()).hexdigest()
                != writer_cgroup_sha
                or parent_scope_cgroup is not None or recovery_leaf is not None
                or [entry["role"] for entry in roles] != ["supervisor"]):
            raise SystemExit("systemd writer is outside the supervisor cgroup")
    elif writer_mode == "detached-root-session":
        writer_entry = next((entry for entry in roles if entry["role"] == "writer"), None)
        fast_writer = fast_intent.get("writer", {})
        scope_unit = fast_writer.get("scope_unit")
        scope_prop = lambda name: subprocess.check_output(
            ["systemctl", "show", scope_unit, f"--property={name}", "--value"], text=True,
        ).strip()
        safety_path = pathlib.Path(fast_writer.get("scope_runtime_safety_path", ""))
        try: safety_details = safety_path.lstat()
        except FileNotFoundError: raise SystemExit("detached scope runtime safety disappeared before quiescence")
        parent_path = pathlib.Path("/sys/fs/cgroup") / parent_scope_cgroup.get("path", "").lstrip("/") \
            if isinstance(parent_scope_cgroup, dict) else pathlib.Path("/")
        try: parent_details = parent_path.lstat()
        except FileNotFoundError: raise SystemExit("detached writer parent cgroup disappeared before quiescence")
        leaf_events = dict(line.split(" ", 1) for line in cgroup_root(
            writer_entry["path"] if isinstance(writer_entry, dict) else "/"
        ).joinpath("cgroup.events").read_text(encoding="ascii").splitlines())
        scope_active = scope_prop("ActiveState") == "active"
        if (not isinstance(parent_scope_cgroup, dict)
                or parent_scope_cgroup != fast_writer.get("parent_scope_cgroup")
                or recovery_leaf != writer_entry
                or fast_writer.get("recovery_leaf_path") != writer_entry.get("path")
                or writer_cgroup != writer_entry.get("path")
                or not re.fullmatch(r"/user\.slice/user-0\.slice/session-[1-9][0-9]*\.scope",
                                    parent_scope_cgroup.get("path", ""))
                or writer_entry["path"] != parent_scope_cgroup["path"].rstrip("/") + "/arc-recovery-writer"
                or parent_path.is_symlink() or not parent_path.is_dir()
                or parent_details.st_dev != parent_scope_cgroup["device"]
                or parent_details.st_ino != parent_scope_cgroup["inode"]
                or safety_path
                    != pathlib.Path(f"/run/systemd/system.control/{scope_unit}.d/zzzy-arc-recovery-writer-scope-safety.conf")
                or safety_path.is_symlink() or not stat.S_ISREG(safety_details.st_mode)
                or safety_details.st_uid != 0 or safety_details.st_gid != 0
                or safety_details.st_mode & 0o022
                or digest(safety_path) != fast_writer.get("scope_runtime_safety_sha256")
                or local_freeze(writer_entry) != 1
                or leaf_events.get("frozen") != "1" or leaf_events.get("populated") != "1"
                or all_subtree_pids(writer_entry["path"]) != [writer_pid]):
            raise SystemExit("detached writer recovery leaf/parent provenance differs before quiescence")
        if scope_active:
            if (scope_prop("ControlGroup") != parent_scope_cgroup["path"]
                    or scope_prop("InvocationID") != fast_writer.get("scope_invocation_id")
                    or scope_prop("Job") not in {"", "0"}):
                raise SystemExit("active detached writer scope identity changed before quiescence")
        elif (scope_prop("ActiveState") not in {"inactive", "failed"}
                or scope_prop("MainPID") not in {"", "0"}
                or scope_prop("Job") not in {"", "0"}
                or scope_prop("InvocationID") not in {"", fast_writer.get("scope_invocation_id")}
                or scope_prop("ControlGroup") not in {"", parent_scope_cgroup["path"]}):
            raise SystemExit("terminal detached writer scope lost sealed provenance before quiescence")
    else:
        raise SystemExit("writer supervision mode is unsupported")
    return supervisor_cgroup, writer_cgroup

supervisor_cgroup, writer_cgroup = validate_fast()
if roles[0] != {"role": "supervisor", **cgroup_identity(supervisor_cgroup)}:
    raise SystemExit("fast-frozen supervisor identity differs before pre-fence quiescence")
helper_outside([entry["path"] for entry in roles])
static = {
    "schema": "arc.recovery.pre-fence-quiesce-intent.v1",
    "capture_id": capture_id,
    "node": node,
    "freeze_plan_sha256": freeze_sha,
    "boot_id": boot_id,
    "supervisor_unit": unit,
    "supervisor_invocation_id": systemctl_value("InvocationID"),
    "cgroups": roles,
    "writer_parent_scope_cgroup": parent_scope_cgroup,
    "writer_recovery_leaf": recovery_leaf,
    "supervisor": {
        "pid": supervisor_pid, "start_ticks": supervisor_start,
        "executable_path": supervisor_executable_path,
        "executable_sha256": supervisor_executable_sha,
        "argv_sha256": supervisor_argv_sha,
        "context_sha256": supervisor_context_sha,
    },
    "writer": {
        "pid": writer_pid, "start_ticks": writer_start,
        "cgroup_sha256": writer_cgroup_sha, "supervision_mode": writer_mode,
        "executable_path": writer_executable_path,
        "executable_sha256": writer_executable_sha, "argv_sha256": writer_argv_sha,
        "parent_scope_cgroup": parent_scope_cgroup,
        "recovery_leaf": recovery_leaf,
    },
    "fast_freeze_intent_sha256": hashlib.sha256(fast_intent_raw).hexdigest(),
    "fast_frozen_context_sha256": hashlib.sha256(fast_frozen_raw).hexdigest(),
}

if phase == "intent":
    value = {**static, "pre_freeze_subtrees": {
        entry["role"]: subtree_pids(entry["path"]) for entry in roles
    }}
    if intent_path.exists():
        raw = intent_path.read_bytes()
        observed = json.loads(raw)
        if raw != canonical(observed) or any(observed.get(key) != value for key, value in static.items()):
            raise SystemExit("existing pre-fence quiesce intent differs")
    else:
        publish(intent_path, value)
    raise SystemExit(0)

if phase != "freeze":
    raise SystemExit("unsupported pre-fence quiesce phase")
if not intent_path.is_file() or intent_path.is_symlink():
    raise SystemExit("durable pre-fence intent is missing")
intent_raw = intent_path.read_bytes()
intent = json.loads(intent_raw)
if intent_raw != canonical(intent) or any(intent.get(key) != value for key, value in static.items()):
    raise SystemExit("durable pre-fence intent changed before cgroup freeze")
if (root / "50-cgroups-thaw-intent.json").exists() or (root / "50-cgroups-thawed.json").exists() or (root / "40-stable-inactive.json").exists():
    raise SystemExit(0)

def frozen_value(path):
    values = {}
    for line in (cgroup_root(path) / "cgroup.events").read_text(encoding="ascii").splitlines():
        key, _, value = line.partition(" ")
        values[key] = value
    if values.get("frozen") not in {"0", "1"}:
        raise SystemExit("cgroup.events has no exact frozen state")
    return int(values["frozen"])

for entry in roles:
    path = entry["path"]
    directory_path = cgroup_root(path)
    details = directory_path.stat()
    if details.st_dev != entry["device"] or details.st_ino != entry["inode"]:
        raise SystemExit("sealed cgroup path/inode changed before freeze")
    if entry["role"] == "supervisor":
        if systemctl_value("FreezerState") not in {"running", "frozen"}:
            raise SystemExit("supervisor has an invalid advisory PID1 freezer state")
    else:
        directory = os.open(directory_path, os.O_RDONLY | getattr(os, "O_DIRECTORY", 0) | getattr(os, "O_NOFOLLOW", 0))
        try:
            current = os.fstat(directory)
            if current.st_dev != entry["device"] or current.st_ino != entry["inode"]:
                raise SystemExit("opened cgroup descriptor differs from sealed identity")
            freezer = os.open("cgroup.freeze", os.O_WRONLY | getattr(os, "O_NOFOLLOW", 0), dir_fd=directory)
            try:
                validate_fast()
                os.write(freezer, b"1")
            finally:
                os.close(freezer)
            deadline = time.monotonic() + 10
            while frozen_value(path) != 1:
                if time.monotonic() >= deadline:
                    raise SystemExit(f"cgroup did not freeze: {path}")
                time.sleep(0.02)
        finally:
            os.close(directory)
    if frozen_value(path) != 1:
        raise SystemExit(f"cgroup is not frozen: {path}")
    if entry["role"] == "writer" and local_freeze(entry) != 1:
        raise SystemExit("detached writer recovery leaf lost its local freeze bit")

validate_fast()
validate_supervisor_members(supervisor_cgroup)
if writer_mode == "detached-root-session" and all_subtree_pids(writer_cgroup) != [writer_pid]:
    raise SystemExit("detached writer is not the only process in its frozen recovery leaf")
supervisor_identity = process_identity(supervisor_pid)
writer_identity = process_identity(writer_pid)
expected_supervisor = {
    "pid": supervisor_pid, "start_ticks": supervisor_start,
    "executable_path": supervisor_executable_path,
    "executable_sha256": supervisor_executable_sha,
    "argv_sha256": supervisor_argv_sha,
}
expected_writer = {
    "pid": writer_pid, "start_ticks": writer_start,
    "executable_path": writer_executable_path,
    "executable_sha256": writer_executable_sha,
    "argv_sha256": writer_argv_sha,
}
if any(supervisor_identity.get(key) != value for key, value in expected_supervisor.items()):
    raise SystemExit("supervisor identity changed before full cgroup freeze")
if any(writer_identity.get(key) != value for key, value in expected_writer.items()):
    raise SystemExit("writer identity changed before full cgroup freeze")
post_subtrees = {entry["role"]: subtree_pids(entry["path"]) for entry in roles}
post_members = {
    entry["role"]: [process_identity(pid) for pid in all_subtree_pids(entry["path"])]
    for entry in roles
}
post_processes = {
    "supervisor": supervisor_identity,
    "writer": writer_identity,
}
value = {
    "schema": "arc.recovery.cgroups-frozen.v1",
    "pre_fence_intent_sha256": hashlib.sha256(intent_raw).hexdigest(),
    "capture_id": capture_id,
    "node": node,
    "freeze_plan_sha256": freeze_sha,
    "boot_id": boot_id,
    "cgroups": roles,
    "writer_parent_scope_cgroup": parent_scope_cgroup,
    "writer_recovery_leaf": recovery_leaf,
    "post_freeze_subtrees": post_subtrees,
    "post_freeze_members": post_members,
    "post_freeze_processes": post_processes,
    "signal_baseline": {
        "supervisor": masks(supervisor_pid),
        "writer": masks(writer_pid),
    },
    "all_cgroups_frozen": True,
    "helper_and_ancestors_outside": True,
}
publish(frozen_path, value)
PY
}

install_legacy_network_quarantine() {
    local root="$1" capture_id="$2" node="$3" freeze_sha="$4" boot_id="$5"
    local writer_supervision_mode="$6" supervisor_unit="$7" supervisor_pid="$8"
    local supervisor_start_ticks="$9" supervisor_executable_path="${10}"
    local supervisor_executable_sha="${11}" supervisor_argv_sha="${12}"
    local writer_pid="${14}" writer_start_ticks="${15}" writer_cgroup_sha="${16}"
    local writer_executable_path="${17}" writer_executable_sha="${18}" writer_argv_sha="${19}"
    require_commands python3 systemctl sync
    python3 - "$root" "$capture_id" "$node" "$freeze_sha" "$boot_id" \
        "$writer_supervision_mode" "$supervisor_unit" "$supervisor_pid" \
        "$supervisor_start_ticks" "$supervisor_executable_path" \
        "$supervisor_executable_sha" "$supervisor_argv_sha" \
        "$writer_pid" "$writer_start_ticks" "$writer_cgroup_sha" \
        "$writer_executable_path" "$writer_executable_sha" "$writer_argv_sha" \
        "$NETWORK_FENCE_STATE" "$NETWORK_FENCE_UNIT" \
        "$ARC_RECOVERY_PYTHON_PATH" <<'PY'
import datetime
import hashlib
import http.client
import json
import os
import pathlib
import re
import stat
import subprocess
import sys
import time
import urllib.parse

(root_raw, capture_id, node, freeze_sha, boot_id, writer_mode, supervisor_unit,
 supervisor_pid_raw, supervisor_start_raw, supervisor_executable,
 supervisor_executable_sha, supervisor_argv_sha, writer_pid_raw,
 writer_start_raw, writer_cgroup_sha, writer_executable, writer_executable_sha,
 writer_argv_sha, state_raw, unit_raw, python_exec_raw) = sys.argv[1:]
root = pathlib.Path(root_raw)
state = pathlib.Path(state_raw)
unit_path = pathlib.Path(unit_raw)
python_exec = pathlib.Path(python_exec_raw)
receipt_path = root / "08-network-quarantine.json"
writer_pid, writer_start = int(writer_pid_raw), int(writer_start_raw)
supervisor_pid, supervisor_start = int(supervisor_pid_raw), int(supervisor_start_raw)
table_name = "arc_legacy_maintenance_v1"
priority = -310
nft_path = pathlib.Path("/usr/sbin/nft")
hash_re = re.compile(r"[0-9a-f]{64}")
boot_re = re.compile(r"[0-9a-f]{8}-[0-9a-f]{4}-[0-9a-f]{4}-[0-9a-f]{4}-[0-9a-f]{12}")
if (not hash_re.fullmatch(capture_id) or not hash_re.fullmatch(freeze_sha)
        or not hash_re.fullmatch(writer_cgroup_sha)
        or not hash_re.fullmatch(supervisor_executable_sha)
        or not hash_re.fullmatch(supervisor_argv_sha)
        or not hash_re.fullmatch(writer_executable_sha)
        or not hash_re.fullmatch(writer_argv_sha)
        or not boot_re.fullmatch(boot_id)
        or writer_mode not in {"systemd-unit", "detached-root-session"}
        or supervisor_unit not in {"arc-self-heal.service", "arc-node.service"}
        or node not in {"nyc", "lax", "ams", "lhr", "nrt", "sgp"}):
    raise SystemExit("network-quarantine identity is malformed")

def canonical(value):
    return (json.dumps(value, sort_keys=True, separators=(",", ":")) + "\n").encode()

def sha(data):
    return hashlib.sha256(data).hexdigest()

def fsync_dir(path):
    descriptor = os.open(path, os.O_RDONLY | getattr(os, "O_DIRECTORY", 0) | getattr(os, "O_NOFOLLOW", 0))
    try: os.fsync(descriptor)
    finally: os.close(descriptor)

def secure_dir(path, mode, create=False):
    if create and not path.exists() and not path.is_symlink():
        os.mkdir(path, mode)
        fsync_dir(path.parent)
    details = path.lstat()
    if (path.is_symlink() or not stat.S_ISDIR(details.st_mode) or details.st_uid != 0
            or details.st_gid != 0 or stat.S_IMODE(details.st_mode) != mode):
        raise SystemExit(f"unsafe network-quarantine directory: {path}")

def create_exact(path, payload, mode):
    if path.exists() or path.is_symlink():
        details = path.lstat()
        if (path.is_symlink() or not stat.S_ISREG(details.st_mode) or details.st_uid != 0
                or details.st_gid != 0 or details.st_nlink != 1
                or stat.S_IMODE(details.st_mode) != mode or path.read_bytes() != payload):
            raise SystemExit(f"network-quarantine create-only file differs: {path}")
        return
    temporary = path.with_name(f".{path.name}.partial")
    if temporary.exists() or temporary.is_symlink():
        details = temporary.lstat()
        if (temporary.is_symlink() or not stat.S_ISREG(details.st_mode)
                or details.st_uid != 0 or details.st_gid != 0 or details.st_nlink != 1):
            raise SystemExit(f"unsafe network-quarantine partial: {temporary}")
        # This is our fixed-name, root-owned partial inside an already sealed
        # directory.  A complete write is promoted; an interrupted write is
        # discarded so a power-loss retry can finish the same transaction.
        if temporary.read_bytes() == payload:
            os.chmod(temporary, mode)
            os.replace(temporary, path)
            fsync_dir(path.parent)
            return
        temporary.unlink()
    descriptor = os.open(
        temporary, os.O_WRONLY | os.O_CREAT | os.O_EXCL | getattr(os, "O_NOFOLLOW", 0), mode,
    )
    with os.fdopen(descriptor, "wb") as handle:
        handle.write(payload); handle.flush(); os.fsync(handle.fileno())
    os.chmod(temporary, mode)
    os.replace(temporary, path)
    fsync_dir(path.parent)

def secure_file(path, mode, expected=None):
    details = path.lstat()
    if (path.is_symlink() or not stat.S_ISREG(details.st_mode) or details.st_uid != 0
            or details.st_gid != 0 or details.st_nlink != 1
            or stat.S_IMODE(details.st_mode) != mode):
        raise SystemExit(f"unsafe network-quarantine file: {path}")
    raw = path.read_bytes()
    if expected is not None and raw != expected:
        raise SystemExit(f"network-quarantine file bytes differ: {path}")
    return raw

def digest_file(path):
    value = hashlib.sha256()
    with open(path, "rb") as handle:
        for chunk in iter(lambda: handle.read(1024 * 1024), b""):
            value.update(chunk)
    return value.hexdigest()

def proc_start(pid):
    raw = pathlib.Path(f"/proc/{pid}/stat").read_text(encoding="ascii")
    end = raw.rfind(")"); fields = raw[end + 2:].split()
    if end < 0 or len(fields) < 20: raise SystemExit("writer stat is truncated before quarantine")
    return int(fields[19])

def verify_writer():
    if pathlib.Path("/proc/sys/kernel/random/boot_id").read_text().strip() != boot_id:
        raise SystemExit("sealed boot ended before network quarantine")
    proc = pathlib.Path(f"/proc/{writer_pid}")
    if (proc_start(writer_pid) != writer_start or os.readlink(proc / "exe") != writer_executable
            or digest_file(proc / "exe") != writer_executable_sha
            or sha(proc.joinpath("cmdline").read_bytes()) != writer_argv_sha):
        raise SystemExit("sealed writer changed before network quarantine")
    if proc.joinpath("comm").read_text().strip() != "arc-node":
        raise SystemExit("sealed network-quarantine writer is not arc-node")
    _path, cgroup_raw = unified_cgroup(writer_pid)
    if sha(cgroup_raw) != writer_cgroup_sha:
        raise SystemExit("sealed writer cgroup bytes changed before network quarantine")

def verify_supervisor():
    proc = pathlib.Path(f"/proc/{supervisor_pid}")
    if (proc_start(supervisor_pid) != supervisor_start
            or os.readlink(proc / "exe") != supervisor_executable
            or digest_file(proc / "exe") != supervisor_executable_sha
            or sha(proc.joinpath("cmdline").read_bytes()) != supervisor_argv_sha):
        raise SystemExit("sealed supervisor changed before network quarantine")
    main_pid = subprocess.check_output(
        ["/usr/bin/systemctl", "show", supervisor_unit, "--property=MainPID", "--value"],
        text=True,
    ).strip()
    if main_pid != str(supervisor_pid):
        raise SystemExit("selected supervisor MainPID changed before network quarantine")
    supervisor_path, _ = unified_cgroup(supervisor_pid)
    writer_path, _ = unified_cgroup(writer_pid)
    supervisor_cgroup, supervisor_members = exact_cgroup("supervisor", supervisor_path)
    writer_cgroup, writer_members = exact_cgroup("writer", writer_path)
    if writer_mode == "systemd-unit":
        if (writer_pid != supervisor_pid or writer_path != supervisor_path
                or supervisor_members != [writer_pid]):
            raise SystemExit("systemd-unit writer/supervisor containment differs")
    else:
        if (writer_pid == supervisor_pid or writer_path == supervisor_path
                or supervisor_members != [supervisor_pid] or writer_members != [writer_pid]):
            raise SystemExit("detached writer/supervisor containment differs")
    return supervisor_cgroup, writer_cgroup

def normalize_nonowned(value):
    def scrub(item):
        if isinstance(item, dict):
            return {key: scrub(val) for key, val in sorted(item.items())
                    if key not in {"handle", "position", "index", "packets", "bytes"}}
        if isinstance(item, list): return [scrub(entry) for entry in item]
        return item
    rows = []
    for entry in value.get("nftables", []):
        if "metainfo" in entry: continue
        obj = next(iter(entry.values())) if isinstance(entry, dict) and len(entry) == 1 else None
        if isinstance(obj, dict) and (
                (entry.get("table", {}).get("family"), entry.get("table", {}).get("name")) == ("inet", table_name)
                or (obj.get("family"), obj.get("table")) == ("inet", table_name)):
            continue
        rows.append(scrub(entry))
    return rows

def nft_json(*args):
    raw = subprocess.check_output([str(nft_path), "--json", *args])
    value = json.loads(raw)
    if not isinstance(value, dict) or not isinstance(value.get("nftables"), list):
        raise SystemExit("nft returned malformed JSON")
    return value, raw

def table_exists():
    return subprocess.run(
        [str(nft_path), "list", "table", "inet", table_name],
        stdout=subprocess.DEVNULL, stderr=subprocess.DEVNULL, check=False,
    ).returncode == 0

def extract_match(expr, interface_key):
    iface = None; payloads = []; verdict = None; counter = False
    for row in expr:
        if "counter" in row: counter = True
        if "accept" in row: verdict = "accept"
        if "drop" in row: verdict = "drop"
        match = row.get("match")
        if not isinstance(match, dict): continue
        left, right = match.get("left"), match.get("right")
        if left == {"meta": {"key": interface_key}}:
            iface = (match.get("op"), right)
        elif isinstance(left, dict) and isinstance(left.get("payload"), dict):
            payload = left["payload"]
            if isinstance(right, dict) and isinstance(right.get("set"), list):
                values = sorted(right["set"])
            else: values = [right]
            payloads.append((payload.get("protocol"), payload.get("field"), values))
    return iface, payloads, verdict, counter

def validate_owned_ast(value):
    rows = [entry for entry in value["nftables"] if "metainfo" not in entry]
    tables = [entry["table"] for entry in rows if "table" in entry]
    if len(tables) != 1 or any(tables[0].get(k) != v for k, v in {
        "family": "inet", "name": table_name,
        "comment": f"arc-recovery:capture={capture_id}:node={node}",
    }.items()):
        raise SystemExit("owned network-quarantine table identity differs")
    expected_chains = [
        ("prerouting", "prerouting"), ("input", "input"),
        ("forward", "forward"), ("output", "output"),
    ]
    chains = [entry["chain"] for entry in rows if "chain" in entry]
    if len(chains) != 4:
        raise SystemExit("owned network-quarantine chain count differs")
    for chain, (name, hook) in zip(chains, expected_chains):
        if any(chain.get(k) != v for k, v in {
                "family": "inet", "table": table_name, "name": name,
                "type": "filter", "hook": hook, "prio": priority, "policy": "accept",
        }.items()):
            raise SystemExit(f"owned network-quarantine chain differs: {name}")
    rules = [entry["rule"] for entry in rows if "rule" in entry]
    expected = []
    for chain, _hook in expected_chains:
        if chain in {"prerouting", "input"}:
            expected.extend([
                (chain, "iifname", "loopback", [], "accept"),
                (chain, "iifname", "ssh", [("tcp","dport",[22])], "accept"),
                (chain, "iifname", "dhcp4", [("udp","sport",[67]),("udp","dport",[68])], "accept"),
                (chain, "iifname", "dhcp6", [("udp","sport",[547]),("udp","dport",[546])], "accept"),
                (chain, "iifname", "icmpv6-control", [("icmpv6","type",[2,133,134,135,136])], "accept"),
                (chain, "iifname", "deny", [], "drop"),
            ])
        elif chain == "output":
            expected.extend([
                (chain, "oifname", "loopback", [], "accept"),
                (chain, "oifname", "ssh", [("tcp","sport",[22])], "accept"),
                (chain, "oifname", "dhcp4", [("udp","sport",[68]),("udp","dport",[67])], "accept"),
                (chain, "oifname", "dhcp6", [("udp","sport",[546]),("udp","dport",[547])], "accept"),
                (chain, "oifname", "icmpv6-control", [("icmpv6","type",[2,133,134,135,136])], "accept"),
                (chain, "oifname", "deny", [], "drop"),
            ])
        else:
            expected.append((chain, None, "deny-all", [], "drop"))
    if len(rules) != len(expected):
        raise SystemExit("owned network-quarantine rule count differs")
    normalized = []
    for rule, wanted in zip(rules, expected):
        chain, interface, slug, payloads, verdict = wanted
        comment = f"arc-recovery:{chain}:{interface or 'all'}:{slug}"
        if (rule.get("family"), rule.get("table"), rule.get("chain"), rule.get("comment")) != (
                "inet", table_name, chain, comment):
            raise SystemExit(f"owned network-quarantine rule order/comment differs: {comment}")
        iface, got_payloads, got_verdict, counter = extract_match(
            rule.get("expr", []), interface,
        ) if interface is not None else (None, [], next(
            (key for row in rule.get("expr", []) for key in ("accept", "drop") if key in row), None,
        ), any("counter" in row for row in rule.get("expr", [])))
        wanted_iface = None if interface is None else (("==", "lo") if slug == "loopback" else ("!=", "lo"))
        if (iface != wanted_iface or got_payloads != payloads
                or got_verdict != verdict or counter is not True):
            raise SystemExit(f"owned network-quarantine rule AST differs: {comment}")
        normalized.append({"chain": chain, "interface": interface, "slug": slug,
                           "payload": payloads,
                           "verdict": verdict, "comment": comment})
    return normalized

verify_writer()
supervisor_cgroup, writer_cgroup = verify_supervisor()
nft_details = nft_path.lstat()
if (nft_path.is_symlink() or not stat.S_ISREG(nft_details.st_mode) or nft_details.st_uid != 0
        or nft_details.st_gid != 0 or nft_details.st_mode & 0o022 or nft_details.st_nlink != 1):
    raise SystemExit("pinned nft tool is unsafe")
nft_sha = digest_file(nft_path)

def socket_inventory():
    rows = []
    for family, name in (("ipv4", "tcp"), ("ipv6", "tcp6"),
                         ("ipv4", "udp"), ("ipv6", "udp6")):
        path = pathlib.Path("/proc/net") / name
        for line in path.read_text(encoding="ascii").splitlines()[1:]:
            fields = line.split()
            if len(fields) < 10: raise SystemExit(f"socket inventory is malformed: {path}")
            address_hex, port_hex = fields[1].split(":", 1)
            protocol = "tcp" if name.startswith("tcp") else "udp"
            state_code = fields[3]
            # TCP LISTEN and every bound UDP socket are relevant.  Connected
            # TCP flows are deliberately not called listeners.
            if protocol == "tcp" and state_code != "0A": continue
            port = int(port_hex, 16)
            if port == 0: continue
            rows.append({"family": family, "protocol": protocol,
                         "local_address_hex": address_hex.lower(), "port": port,
                         "state_hex": state_code.lower(), "inode": int(fields[9]),
                         "quarantine_coverage": (
                             "explicit-ssh-allow" if protocol == "tcp" and port == 22
                             else "maintenance-dhcp-allow" if protocol == "udp" and port in (68,546)
                             else "nonloopback-deny-before-conntrack"
                         )})
    return sorted(rows, key=lambda row: (row["family"], row["protocol"], row["port"], row["inode"]))

def network_configuration():
    result = {}
    ip_path = pathlib.Path("/usr/sbin/ip")
    details = ip_path.lstat()
    if (ip_path.is_symlink() or not stat.S_ISREG(details.st_mode) or details.st_uid != 0
            or details.st_gid != 0 or details.st_mode & 0o022):
        raise SystemExit("pinned ip tool is unsafe")
    result["ip_tool"] = {"path": str(ip_path), "sha256": digest_file(ip_path)}
    for name, args in (
        ("addresses", ["-json", "address", "show"]),
        ("routes_v4", ["-4", "-json", "route", "show", "table", "all"]),
        ("routes_v6", ["-6", "-json", "route", "show", "table", "all"]),
        ("rules_v4", ["-4", "-json", "rule", "show"]),
        ("rules_v6", ["-6", "-json", "rule", "show"]),
    ):
        raw = subprocess.check_output([str(ip_path), *args])
        json.loads(raw)
        result[name + "_sha256"] = sha(raw)
    configs = []
    for base_raw in ("/etc/netplan", "/etc/systemd/network", "/etc/NetworkManager/system-connections"):
        base = pathlib.Path(base_raw)
        if not base.exists() and not base.is_symlink(): continue
        if base.is_symlink() or not base.is_dir(): raise SystemExit(f"network configuration root is unsafe: {base}")
        for path in sorted(base.rglob("*")):
            if path.is_symlink(): raise SystemExit(f"network configuration contains a symlink: {path}")
            if path.is_dir(): continue
            details = path.lstat()
            if not stat.S_ISREG(details.st_mode): raise SystemExit(f"network configuration is non-regular: {path}")
            configs.append({"path": str(path), "sha256": digest_file(path),
                            "mode": stat.S_IMODE(details.st_mode), "uid": details.st_uid,
                            "gid": details.st_gid, "nlink": details.st_nlink})
    result["configuration_files"] = configs
    result["dhcp4_allow_present"] = True
    result["dhcp6_allow_present"] = True
    result["icmpv6_ndp_ra_allow_present"] = True
    return result

secure_dir(pathlib.Path("/etc/arc-recovery"), 0o700)
if state.exists() or state.is_symlink():
    secure_dir(state, 0o700)
else:
    secure_dir(state, 0o700, create=True)
owner = {
    "schema": "arc.recovery.legacy-network-fence-owner.v1",
    "capture_id": capture_id, "node": node, "freeze_plan_sha256": freeze_sha,
    "table": {"family": "inet", "name": table_name}, "priority": priority,
}
owner_raw = canonical(owner)
create_exact(state / "owner.json", owner_raw, 0o400)

chains = (("prerouting", "prerouting"), ("input", "input"),
          ("forward", "forward"), ("output", "output"))
lines = [f'table inet {table_name} {{',
         f' comment "arc-recovery:capture={capture_id}:node={node}"']
for chain, hook in chains:
    lines.extend([f' chain {chain} {{',
                  f'  type filter hook {hook} priority {priority}; policy accept;'])
    if chain in {"prerouting", "input"}:
        prefix = f"arc-recovery:{chain}:iifname"
        lines.extend([
            f'  iifname "lo" counter accept comment "{prefix}:loopback"',
            f'  iifname != "lo" tcp dport 22 counter accept comment "{prefix}:ssh"',
            f'  iifname != "lo" udp sport 67 udp dport 68 counter accept comment "{prefix}:dhcp4"',
            f'  iifname != "lo" udp sport 547 udp dport 546 counter accept comment "{prefix}:dhcp6"',
            f'  iifname != "lo" icmpv6 type {{ 2, 133, 134, 135, 136 }} counter accept comment "{prefix}:icmpv6-control"',
            f'  iifname != "lo" counter drop comment "{prefix}:deny"',
        ])
    elif chain == "output":
        prefix = f"arc-recovery:{chain}:oifname"
        lines.extend([
            f'  oifname "lo" counter accept comment "{prefix}:loopback"',
            f'  oifname != "lo" tcp sport 22 counter accept comment "{prefix}:ssh"',
            f'  oifname != "lo" udp sport 68 udp dport 67 counter accept comment "{prefix}:dhcp4"',
            f'  oifname != "lo" udp sport 546 udp dport 547 counter accept comment "{prefix}:dhcp6"',
            f'  oifname != "lo" icmpv6 type {{ 2, 133, 134, 135, 136 }} counter accept comment "{prefix}:icmpv6-control"',
            f'  oifname != "lo" counter drop comment "{prefix}:deny"',
        ])
    else:
        lines.append(f'  counter drop comment "arc-recovery:{chain}:all:deny-all"')
    lines.append(' }')
lines.append('}')
policy_raw = ("\n".join(lines) + "\n").encode()
policy_path = state / "policy.nft"
create_exact(policy_path, policy_raw, 0o400)
pinned_nft_path = state / "nft"
pinned_nft_raw = nft_path.read_bytes()
create_exact(pinned_nft_path, pinned_nft_raw, 0o500)
helper_raw = (
    "#!/bin/sh\nset -efu\numask 077\n"
    f"NFT='{pinned_nft_path}'\nPOLICY='{policy_path}'\nTABLE='{table_name}'\n"
    f"EXPECTED_NFT_SHA256='{nft_sha}'\n"
    'test "$(/usr/bin/stat -c %U:%G:%a:%h "$NFT")" = root:root:500:1\n'
    'test "$(/usr/bin/sha256sum "$NFT" | /usr/bin/cut -d" " -f1)" = "$EXPECTED_NFT_SHA256"\n'
    'if "$NFT" list table inet "$TABLE" >/dev/null 2>&1; then\n'
    '  echo "owned nft table already exists; refusing replacement" >&2\n  exit 73\nfi\n'
    'exec "$NFT" -f "$POLICY"\n'
).encode()
helper_path = state / "apply"
create_exact(helper_path, helper_raw, 0o500)
ensure_path = state / "ensure"
validator_path = state / "validate"
monitor_path = state / "monitor"
unit_raw = (
    "[Unit]\nDescription=ARC legacy maintenance network quarantine monitor\nDefaultDependencies=no\n"
    "Wants=network-pre.target\n"
    "After=local-fs.target firewalld.service ip6tables-restore.service ip6tables.service "
    "iptables-restore.service iptables.service netfilter-persistent.service nftables.service ufw.service\n"
    "Before=network-pre.target network.target network-online.target "
    "arc-self-heal.service arc-node.service arc-node-update.service arc-node-update.timer\n"
    "RefuseManualStop=yes\nIgnoreOnIsolate=yes\n\n[Service]\nType=simple\n"
    f"ExecStartPre={ensure_path}\n"
    f"ExecStartPre=/usr/bin/env -i PATH=/usr/bin:/bin LC_ALL=C TZ=UTC PYTHONHASHSEED=0 "
    f"{python_exec} -I {validator_path}\n"
    f"ExecStart=/usr/bin/env -i PATH=/usr/bin:/bin LC_ALL=C TZ=UTC PYTHONHASHSEED=0 "
    f"{python_exec} -I {monitor_path}\n"
    "Restart=always\nRestartPreventExitStatus=77\nRestartSec=1\n"
    "NoNewPrivileges=yes\nProtectHome=yes\nPrivateTmp=yes\nProtectControlGroups=no\n\n"
    "[Install]\nWantedBy=multi-user.target\n"
).encode()
create_exact(unit_path, unit_raw, 0o400)
dependency_paths = []
dependency_raw_by_path = {}
for legacy_unit in ("arc-self-heal.service", "arc-node.service",
                    "arc-node-update.service", "arc-node-update.timer"):
    directory = pathlib.Path(f"/etc/systemd/system/{legacy_unit}.d")
    details = directory.lstat()
    if (directory.is_symlink() or not stat.S_ISDIR(details.st_mode) or details.st_uid != 0
            or details.st_gid != 0 or details.st_mode & 0o022):
        raise SystemExit(f"legacy network dependency directory is unsafe: {directory}")
    dependency = directory / "zzzy-arc-recovery-network-fence.conf"
    dependency_raw = (
        "[Unit]\nRequires=arc-legacy-maintenance-fence.service\n"
        "After=arc-legacy-maintenance-fence.service\n"
        + (f"\n[Service]\nExecStartPre=/usr/bin/env -i PATH=/usr/bin:/bin LC_ALL=C "
           f"TZ=UTC PYTHONHASHSEED=0 {python_exec} -I {validator_path}\n"
           if legacy_unit.endswith(".service") else "")
    ).encode()
    create_exact(dependency, dependency_raw, 0o400)
    dependency_paths.append(dependency)
    dependency_raw_by_path[str(dependency)] = dependency_raw

baseline_path = state / "preexisting-ruleset.json"
if baseline_path.exists() or baseline_path.is_symlink():
    baseline_raw = secure_file(baseline_path, 0o400)
    baseline = json.loads(baseline_raw)
else:
    if table_exists():
        raise SystemExit("owned nft table preexists its create-only baseline")
    baseline, baseline_raw = nft_json("list", "ruleset")
    create_exact(baseline_path, baseline_raw, 0o400)
baseline_structural_sha = sha(canonical(normalize_nonowned(baseline)))
for path, mode in ((state / "owner.json", 0o400), (policy_path, 0o400),
                   (pinned_nft_path, 0o500), (helper_path, 0o500),
                   (baseline_path, 0o400), (unit_path, 0o400),
                   *((path, 0o400) for path in dependency_paths)):
    secure_file(path, mode)
fsync_dir(state); fsync_dir(state.parent); fsync_dir(unit_path.parent)

subprocess.run(["/usr/bin/systemctl", "daemon-reload"], check=True)
subprocess.run(["/usr/bin/systemctl", "enable", "arc-legacy-maintenance-fence.service"],
               stdout=subprocess.DEVNULL, check=True)
subprocess.run(["/usr/bin/sync"], check=True)
enabled = subprocess.check_output(
    ["/usr/bin/systemctl", "is-enabled", "arc-legacy-maintenance-fence.service"], text=True,
).strip()
if enabled != "enabled": raise SystemExit("network-quarantine unit is not durably enabled")
if not table_exists():
    subprocess.run([str(helper_path)], check=True)
owned, owned_raw = nft_json("list", "table", "inet", table_name)
normalized_ast = validate_owned_ast(owned)
stateless_ruleset_raw = subprocess.check_output(
    [str(nft_path), "--stateless", "list", "table", "inet", table_name],
)
stateless_ruleset_sha = sha(stateless_ruleset_raw)
ensure_raw = (
    "#!/bin/bash\nset -Eeuo pipefail\numask 077\n"
    f"NFT='{pinned_nft_path}'\nPOLICY='{policy_path}'\nAPPLY='{helper_path}'\n"
    f"TABLE='{table_name}'\nEXPECTED_NFT_SHA256='{nft_sha}'\n"
    f"EXPECTED_POLICY_SHA256='{sha(policy_raw)}'\nEXPECTED_APPLY_SHA256='{sha(helper_raw)}'\n"
    f"EXPECTED_STATELESS_SHA256='{stateless_ruleset_sha}'\n"
    'test "$(/usr/bin/stat -c %u:%g:%a:%h "$NFT")" = 0:0:500:1\n'
    'test "$(/usr/bin/stat -c %u:%g:%a:%h "$POLICY")" = 0:0:400:1\n'
    'test "$(/usr/bin/stat -c %u:%g:%a:%h "$APPLY")" = 0:0:500:1\n'
    'test "$(/usr/bin/sha256sum "$NFT" | /usr/bin/cut -d" " -f1)" = "$EXPECTED_NFT_SHA256"\n'
    'test "$(/usr/bin/sha256sum "$POLICY" | /usr/bin/cut -d" " -f1)" = "$EXPECTED_POLICY_SHA256"\n'
    'test "$(/usr/bin/sha256sum "$APPLY" | /usr/bin/cut -d" " -f1)" = "$EXPECTED_APPLY_SHA256"\n'
    'if ! "$NFT" list table inet "$TABLE" >/dev/null 2>&1; then "$APPLY"; fi\n'
    'OBSERVED_STATELESS_SHA256="$("$NFT" --stateless list table inet "$TABLE" | '
    '/usr/bin/sha256sum | /usr/bin/cut -d" " -f1)"\n'
    'test "$OBSERVED_STATELESS_SHA256" = "$EXPECTED_STATELESS_SHA256"\n'
).encode()
create_exact(ensure_path, ensure_raw, 0o500)
current, _ = nft_json("list", "ruleset")
if sha(canonical(normalize_nonowned(current))) != baseline_structural_sha:
    raise SystemExit("nonowned firewall changed while installing network quarantine")
if receipt_path.exists() or receipt_path.is_symlink():
    existing_raw = secure_file(receipt_path, 0o400)
    existing = json.loads(existing_raw)
    if (existing_raw != canonical(existing)
            or existing.get("schema") != "arc.recovery.legacy-network-quarantine.v1"
            or (existing.get("capture_id"), existing.get("node"),
                existing.get("freeze_plan_sha256")) != (capture_id, node, freeze_sha)
            or existing.get("owned_rule_ast_sha256") != sha(canonical(normalized_ast))
            or existing.get("owned_ruleset_stateless_sha256") != sha(stateless_ruleset_raw)
            or existing.get("preexisting_firewall_structural_sha256") != baseline_structural_sha):
        raise SystemExit("existing network-quarantine receipt differs on retry")
    raise SystemExit(0)

plan_path = pathlib.Path(f"/root/.arc-recovery-plans/{freeze_sha}/freeze.lock.json")
plan_raw = secure_file(plan_path, 0o400)
if sha(plan_raw) != freeze_sha: raise SystemExit("pinned freeze plan changed before quarantine")
plan = json.loads(plan_raw)
if plan_raw != canonical(plan) or plan.get("schema") != "arc.recovery.freeze-plan.v5":
    raise SystemExit("pinned freeze plan is not canonical v5")
matches = [row for row in plan.get("nodes", []) if row.get("name") == node]
if len(matches) != 1: raise SystemExit("pinned quarantine node is not unique")
origin = urllib.parse.urlsplit(matches[0].get("rpc_origin", ""))
if (origin.scheme, origin.hostname, origin.path, origin.query, origin.fragment) != (
        "http", "127.0.0.1", "", "", "") or origin.port is None:
    raise SystemExit("pinned quarantine RPC origin is not exact loopback HTTP")

def rpc(path):
    connection = http.client.HTTPConnection("127.0.0.1", origin.port, timeout=5)
    try:
        connection.request("GET", path, headers={
            "Host": f"127.0.0.1:{origin.port}", "Accept": "application/json",
            "Connection": "close", "User-Agent": "arc-recovery-network-quarantine/1",
        })
        response = connection.getresponse(); body = response.read(8 * 1024 * 1024 + 1)
        if response.status != 200 or len(body) > 8 * 1024 * 1024:
            raise RuntimeError(f"loopback RPC failed: {path} status={response.status} bytes={len(body)}")
        value = json.loads(body)
        if not isinstance(value, dict): raise RuntimeError(f"loopback RPC returned non-object: {path}")
        return value, body
    finally: connection.close()

stable = None
for attempt in range(10):
    verify_writer()
    info_before, info_before_raw = rpc("/info")
    latest, latest_raw = rpc("/block/latest")
    height = latest.get("header", {}).get("height")
    block_hash = latest.get("hash")
    state_root = latest.get("header", {}).get("state_root")
    if (isinstance(height, bool) or not isinstance(height, int) or height < 1
            or not isinstance(block_hash, str) or not hash_re.fullmatch(block_hash)
            or not isinstance(state_root, str) or not hash_re.fullmatch(state_root)):
        raise SystemExit("quarantine latest block identity is malformed")
    exact, exact_raw = rpc(f"/block/{height}")
    health, health_raw = rpc("/health")
    info_after, info_after_raw = rpc("/info")
    heights = [info_before.get("block_height"), height,
               exact.get("header", {}).get("height"), info_after.get("block_height")]
    if (heights == [height] * 4 and exact.get("hash") == block_hash
            and exact.get("header", {}).get("state_root") == state_root):
        stable = {
            "rpc_origin": f"http://127.0.0.1:{origin.port}",
            "info_before_height": height, "latest_height": height,
            "block_height": height, "info_after_height": height,
            "block_hash": block_hash,
            "state_root": state_root,
            "response_sha256": {
                "/info:before": sha(info_before_raw), "/block/latest": sha(latest_raw),
                f"/block/{height}": sha(exact_raw), "/health": sha(health_raw),
                "/info:after": sha(info_after_raw),
            },
            "stable_attempt": attempt + 1,
        }
        break
    time.sleep(0.2)
if stable is None:
    raise SystemExit("post-quarantine loopback head did not stabilize")
verify_writer()
owned_after, owned_after_raw = nft_json("list", "table", "inet", table_name)
normalized_ast_after = validate_owned_ast(owned_after)
if normalized_ast_after != normalized_ast:
    raise SystemExit("owned network-quarantine AST changed during loopback proof")
current_after, _ = nft_json("list", "ruleset")
if sha(canonical(normalize_nonowned(current_after))) != baseline_structural_sha:
    raise SystemExit("nonowned firewall changed during loopback proof")

receipt = {
    "schema": "arc.recovery.legacy-network-quarantine.v1",
    "capture_id": capture_id, "node": node, "freeze_plan_sha256": freeze_sha,
    "boot_id": boot_id,
    "writer": {"pid": writer_pid, "start_ticks": writer_start,
               "cgroup_sha256": writer_cgroup_sha,
               "executable_path": writer_executable,
               "executable_sha256": writer_executable_sha, "argv_sha256": writer_argv_sha},
    "table": {"family": "inet", "name": table_name, "priority": priority,
              "hooks": ["prerouting", "input", "forward", "output"],
              "policy": "accept", "loopback_retained": True, "ssh_unmatched_retained": True},
    "quarantine_policy": {
        "mode": "deny-all-nonloopback-except-host-maintenance",
        "families": ["ipv4", "ipv6"], "directions": ["input", "output", "forward"],
        "priority_before_conntrack": True, "established_bypass": False,
        "allowed": ["loopback", "ssh-tcp-22", "dhcpv4-67-68", "dhcpv6-546-547",
                    "icmpv6-ndp-ra-packet-too-big"],
        "legacy_rpc_p2p_web_dynamic_all_blocked": True,
    },
    "listener_inventory": socket_inventory(),
    "network_configuration": network_configuration(),
    "persistence": {"unit_path": str(unit_path), "unit_enabled": True,
                    "state_path": str(state), "automatic_unfence": False},
    "tool_sha256": {str(nft_path): nft_sha},
    "file_sha256": {"owner.json": sha(owner_raw), "policy.nft": sha(policy_raw),
                    "nft": sha(pinned_nft_raw), "apply": sha(helper_raw), "ensure": sha(ensure_raw),
                    str(unit_path): sha(unit_raw),
                    **{str(path): sha(dependency_raw_by_path[str(path)]) for path in dependency_paths},
                    "preexisting-ruleset.json": sha(baseline_raw)},
    "preexisting_firewall_structural_sha256": baseline_structural_sha,
    "owned_rule_ast_sha256": sha(canonical(normalized_ast)),
    "owned_ruleset_stateless_sha256": sha(stateless_ruleset_raw),
    "loopback_head": stable,
    "installed_at": datetime.datetime.now(datetime.timezone.utc).replace(
        microsecond=0).isoformat().replace("+00:00", "Z"),
    "global_absence_claimed": False,
    "threat_model": {"legacy_binary": "reviewed-non-adversarial-exact-hash",
                     "legacy_binary_sha256": writer_executable_sha},
}
payload = canonical(receipt)
if receipt_path.exists() or receipt_path.is_symlink():
    secure_file(receipt_path, 0o400, payload)
else:
    create_exact(receipt_path, payload, 0o400)
fsync_dir(root)
PY
    harden_legacy_network_quarantine "$root" "$capture_id" "$node" "$freeze_sha"
    verify_legacy_network_quarantine "$root" "$capture_id" "$node" "$freeze_sha"
}

harden_legacy_network_quarantine() {
    local root="$1" capture_id="$2" node="$3" freeze_sha="$4"
    python3 - "$root" "$capture_id" "$node" "$freeze_sha" \
        "$NETWORK_FENCE_STATE" "$NETWORK_FENCE_UNIT" \
        "$ARC_RECOVERY_PYTHON_PATH" "$ARC_RECOVERY_PYTHON_SHA256" \
        "$ARC_RECOVERY_PYTHON_DEVICE" "$ARC_RECOVERY_PYTHON_INODE" <<'PY'
import hashlib
import json
import os
import pathlib
import re
import stat
import subprocess
import sys

(root_raw, capture_id, node, freeze_sha, state_raw, unit_raw, python_raw,
 python_sha, python_device_raw, python_inode_raw) = sys.argv[1:]
root = pathlib.Path(root_raw); state = pathlib.Path(state_raw)
unit_path = pathlib.Path(unit_raw); python_path = pathlib.Path(python_raw)
python_device = int(python_device_raw); python_inode = int(python_inode_raw)
receipt_path = root / "08-network-quarantine-monitor.json"
base_path = root / "08-network-quarantine.json"
contract_path = state / "monitor-contract.json"
validator_path = state / "validate"
monitor_path = state / "monitor"
incident_intent = state / "incident.intent.json"
incident_commit = state / "incident.committed.json"
hash_re = re.compile(r"[0-9a-f]{64}")
reviewed_loaders = {
    "firewalld.service", "ip6tables-restore.service", "ip6tables.service",
    "iptables-restore.service", "iptables.service", "netfilter-persistent.service",
    "nftables.service", "ufw.service",
}
suspicious_name = re.compile(
    r"(?:firewall|firewalld|iptables|ip6tables|nftables|netfilter|ufw|fail2ban|docker|libvirt)",
    re.IGNORECASE,
)
suspicious_content = re.compile(
    rb"(?:^|[ /])(?:nft|iptables|ip6tables|iptables-restore|ip6tables-restore|ufw|firewalld|netfilter-persistent)(?:[ \n]|$)",
    re.IGNORECASE,
)

def canonical(value):
    return (json.dumps(value, sort_keys=True, separators=(",", ":")) + "\n").encode()

def sha(raw):
    return hashlib.sha256(raw).hexdigest()

def digest(path):
    result = hashlib.sha256()
    with open(path, "rb") as handle:
        for chunk in iter(lambda: handle.read(1024 * 1024), b""):
            result.update(chunk)
    return result.hexdigest()

def fsync_dir(path):
    descriptor = os.open(
        path, os.O_RDONLY | getattr(os, "O_DIRECTORY", 0) | getattr(os, "O_NOFOLLOW", 0),
    )
    try: os.fsync(descriptor)
    finally: os.close(descriptor)

def secure(path, mode):
    details = path.lstat()
    if (path.is_symlink() or not stat.S_ISREG(details.st_mode) or details.st_uid != 0
            or details.st_gid != 0 or details.st_nlink != 1
            or stat.S_IMODE(details.st_mode) != mode):
        raise SystemExit(f"unsafe network-quarantine monitor file: {path}")
    return path.read_bytes()

def create(path, payload, mode):
    if path.exists() or path.is_symlink():
        if path.is_symlink() or secure(path, mode) != payload:
            raise SystemExit(f"network-quarantine monitor create-only file differs: {path}")
        return
    temporary = path.with_name(f".{path.name}.partial")
    if temporary.exists() or temporary.is_symlink():
        details = temporary.lstat()
        if (temporary.is_symlink() or not stat.S_ISREG(details.st_mode)
                or details.st_uid != 0 or details.st_gid != 0 or details.st_nlink != 1):
            raise SystemExit(f"unsafe network-quarantine monitor partial: {temporary}")
        if temporary.read_bytes() == payload:
            os.chmod(temporary, mode); os.replace(temporary, path); fsync_dir(path.parent); return
        temporary.unlink()
    descriptor = os.open(
        temporary, os.O_WRONLY | os.O_CREAT | os.O_EXCL | getattr(os, "O_NOFOLLOW", 0), mode,
    )
    with os.fdopen(descriptor, "wb") as handle:
        handle.write(payload); handle.flush(); os.fsync(handle.fileno())
    os.chmod(temporary, mode); os.replace(temporary, path); fsync_dir(path.parent)

def proc_start(pid):
    raw = pathlib.Path(f"/proc/{pid}/stat").read_text(encoding="ascii")
    end = raw.rfind(")"); fields = raw[end + 2:].split()
    if end < 0 or len(fields) < 20: raise SystemExit("monitor process stat is truncated")
    return int(fields[19])

def cgroup(pid):
    raw = pathlib.Path(f"/proc/{pid}/cgroup").read_bytes()
    rows = [line.split(":", 2)[2] for line in raw.decode().splitlines() if line.startswith("0::")]
    if len(rows) != 1 or not re.fullmatch(r"/[A-Za-z0-9._@/-]+", rows[0]):
        raise SystemExit("monitor process cgroup is ambiguous")
    path = rows[0]; base = pathlib.Path("/sys/fs/cgroup") / path.lstrip("/")
    details = base.lstat()
    if base.is_symlink() or not base.is_dir():
        raise SystemExit("monitor cgroup is unsafe")
    return {"path": path, "device": details.st_dev, "inode": details.st_ino}

def loader_inventory():
    output = subprocess.check_output([
        "/usr/bin/systemctl", "list-unit-files", "--type=service", "--state=enabled",
        "--no-legend", "--no-pager",
    ], text=True)
    enabled = sorted({line.split()[0] for line in output.splitlines() if line.split()})
    rows = []
    for name in enabled:
        if not re.fullmatch(r"[A-Za-z0-9_.@:-]+\.service", name):
            raise SystemExit(f"enabled service name is unsafe: {name}")
        configuration = subprocess.check_output(["/usr/bin/systemctl", "cat", name])
        if not (name in reviewed_loaders or suspicious_name.search(name)
                or suspicious_content.search(configuration)):
            continue
        if name not in reviewed_loaders:
            raise SystemExit(f"unreviewed enabled firewall loader/mutator: {name}")
        sources = []
        for raw_path in re.findall(rb"(?m)^# (/[^\n]+)$", configuration):
            source = pathlib.Path(raw_path.decode())
            details = source.lstat()
            if (source.is_symlink() or not stat.S_ISREG(details.st_mode)
                    or details.st_uid != 0 or details.st_gid != 0 or details.st_mode & 0o022):
                raise SystemExit(f"firewall loader source is unsafe: {source}")
            sources.append({"path": str(source), "sha256": digest(source)})
        if not sources:
            raise SystemExit(f"reviewed firewall loader has no exact source inventory: {name}")
        rows.append({"unit": name, "enablement": "enabled",
                     "unit_configuration_sha256": sha(configuration), "sources": sources})
    return rows

base_raw = secure(base_path, 0o400); base = json.loads(base_raw)
if (base_raw != canonical(base)
        or base.get("schema") != "arc.recovery.legacy-network-quarantine.v1"
        or (base.get("capture_id"), base.get("node"), base.get("freeze_plan_sha256"))
        != (capture_id, node, freeze_sha)):
    raise SystemExit("network-quarantine base receipt differs before monitor hardening")
plan_path = pathlib.Path(f"/root/.arc-recovery-plans/{freeze_sha}/freeze.lock.json")
plan_raw = secure(plan_path, 0o400); plan = json.loads(plan_raw)
if sha(plan_raw) != freeze_sha or plan_raw != canonical(plan):
    raise SystemExit("pinned plan changed before network monitor hardening")
matches = [row for row in plan.get("nodes", []) if row.get("name") == node]
if len(matches) != 1: raise SystemExit("monitor node plan row is not unique")
row = matches[0]
writer = base.get("writer", {})
supervisor_pid = row.get("supervisor_main_pid")
if (writer.get("pid") != row.get("writer_pid")
        or writer.get("start_ticks") != row.get("writer_start_ticks")
        or not isinstance(supervisor_pid, int) or supervisor_pid <= 1
        or proc_start(supervisor_pid) != row.get("supervisor_start_ticks")):
    raise SystemExit("monitor process contract differs from the pinned plan")
supervisor_proc = pathlib.Path(f"/proc/{supervisor_pid}")
if (os.readlink(supervisor_proc / "exe") != row.get("supervisor_executable_path")
        or digest(supervisor_proc / "exe") != row.get("supervisor_executable_sha256")
        or sha(supervisor_proc.joinpath("cmdline").read_bytes()) != row.get("supervisor_argv_sha256")):
    raise SystemExit("monitor supervisor executable/argv changed")
python_details = python_path.lstat()
if (python_path.is_symlink() or not stat.S_ISREG(python_details.st_mode)
        or python_details.st_uid != 0 or python_details.st_gid != 0
        or stat.S_IMODE(python_details.st_mode) != 0o755 or python_details.st_nlink != 1
        or python_details.st_dev != python_device or python_details.st_ino != python_inode
        or digest(python_path) != python_sha):
    raise SystemExit("pinned semantic Python projection changed")
interpreter = {"normalized_path": str(python_path), "sha256": python_sha,
               "device": python_device, "inode": python_inode,
               "uid": 0, "gid": 0, "mode": 0o755, "nlink": 1,
               "isolated": True, "environment": {
                   "PATH": "/usr/bin:/bin", "LC_ALL": "C", "TZ": "UTC", "PYTHONHASHSEED": "0",
               }}
loaders = loader_inventory()
contract = {
    "schema": "arc.recovery.legacy-network-fence-monitor-contract.v1",
    "capture_id": capture_id, "node": node, "freeze_plan_sha256": freeze_sha,
    "network_quarantine_receipt_sha256": sha(base_raw),
    "sealed_boot_id": base["boot_id"],
    "table": {"family": "inet", "name": "arc_legacy_maintenance_v1"},
    "nft_path": str(state / "nft"), "nft_sha256": base["file_sha256"]["nft"],
    "policy_path": str(state / "policy.nft"), "policy_sha256": base["file_sha256"]["policy.nft"],
    "owner_path": str(state / "owner.json"), "owner_sha256": base["file_sha256"]["owner.json"],
    "apply_path": str(state / "apply"), "apply_sha256": base["file_sha256"]["apply"],
    "ensure_path": str(state / "ensure"), "ensure_sha256": base["file_sha256"]["ensure"],
    "owned_ruleset_stateless_sha256": base["owned_ruleset_stateless_sha256"],
    "writer": {**writer, "cgroup": {
        "path": row["writer_cgroup_path"], "device": row["writer_cgroup_device"],
        "inode": row["writer_cgroup_inode"],
    }},
    "supervisor": {"unit": row["supervisor_unit"], "pid": supervisor_pid,
                   "start_ticks": row["supervisor_start_ticks"],
                   "executable_path": row["supervisor_executable_path"],
                   "executable_sha256": row["supervisor_executable_sha256"],
                   "argv_sha256": row["supervisor_argv_sha256"],
                   "cgroup": cgroup(supervisor_pid)},
    "writer_supervision_mode": row["writer_supervision_mode"],
    "allow_marker_path": "/etc/arc-recovery/legacy-start-allowed",
    "allow_marker_sha256": sha(b"schema=arc.recovery.legacy-start-allow.v1\n"),
    "incident_intent_path": str(incident_intent),
    "incident_commit_path": str(incident_commit),
    "firewall_loader_inventory": loaders, "semantic_interpreter": interpreter,
    "automatic_unfence": False, "global_absence_claimed": False,
}
contract_raw = canonical(contract); create(contract_path, contract_raw, 0o400)
contract_sha = sha(contract_raw)

validator_source = r'''
import hashlib,json,os,pathlib,re,stat,subprocess,sys
CONTRACT_PATH=pathlib.Path(__CONTRACT_PATH__)
EXPECTED_CONTRACT_SHA256=__CONTRACT_SHA__
def canonical(value): return (json.dumps(value,sort_keys=True,separators=(",",":"))+"\n").encode()
def digest(path):
    value=hashlib.sha256()
    with open(path,"rb") as handle:
        for chunk in iter(lambda:handle.read(1024*1024),b""): value.update(chunk)
    return value.hexdigest()
def secure(path,mode):
    details=path.lstat()
    if (path.is_symlink() or not stat.S_ISREG(details.st_mode) or details.st_uid!=0
            or details.st_gid!=0 or details.st_nlink!=1 or stat.S_IMODE(details.st_mode)!=mode):
        raise SystemExit(f"unsafe network-fence validator input: {path}")
    return path.read_bytes()
raw=secure(CONTRACT_PATH,0o400);contract=json.loads(raw)
if (hashlib.sha256(raw).hexdigest()!=EXPECTED_CONTRACT_SHA256 or raw!=canonical(contract)
        or contract.get("schema")!="arc.recovery.legacy-network-fence-monitor-contract.v1"):
    raise SystemExit("network-fence monitor contract differs")
interp=contract["semantic_interpreter"]; executable=pathlib.Path(os.path.realpath(sys.executable));details=executable.lstat()
if (sys.flags.isolated!=1 or str(executable)!=interp["normalized_path"]
        or details.st_dev!=interp["device"] or details.st_ino!=interp["inode"]
        or digest(executable)!=interp["sha256"]
        or {key:os.environ.get(key) for key in interp["environment"]}!=interp["environment"]
        or set(os.environ)!=set(interp["environment"])):
    raise SystemExit("network-fence semantic interpreter/environment changed")
for key in ("incident_intent_path","incident_commit_path"):
    path=pathlib.Path(contract[key])
    if path.exists() or path.is_symlink(): raise SystemExit("network-fence incident is latched")
for key,mode,hash_key in (("nft_path",0o500,"nft_sha256"),("policy_path",0o400,"policy_sha256"),
                          ("owner_path",0o400,"owner_sha256"),("apply_path",0o500,"apply_sha256"),
                          ("ensure_path",0o500,"ensure_sha256")):
    if hashlib.sha256(secure(pathlib.Path(contract[key]),mode)).hexdigest()!=contract[hash_key]:
        raise SystemExit("network-fence pinned tool/policy input changed")
table=contract["table"];nft=contract["nft_path"]
stateless=subprocess.check_output([nft,"--stateless","list","table",table["family"],table["name"]])
if hashlib.sha256(stateless).hexdigest()!=contract["owned_ruleset_stateless_sha256"]:
    raise SystemExit("network-fence live stateless AST changed")
runtime=json.loads(subprocess.check_output([nft,"--json","list","table",table["family"],table["name"]]))
tables=[row["table"] for row in runtime.get("nftables",[]) if "table" in row]
if (len(tables)!=1 or tables[0].get("family")!=table["family"] or tables[0].get("name")!=table["name"]
        or tables[0].get("comment")!=f"arc-recovery:capture={contract['capture_id']}:node={contract['node']}"):
    raise SystemExit("network-fence live table owner changed")
if "--fast" not in sys.argv[1:]:
    reviewed={"firewalld.service","ip6tables-restore.service","ip6tables.service",
              "iptables-restore.service","iptables.service","netfilter-persistent.service",
              "nftables.service","ufw.service"}
    suspicious_name=re.compile(r"(?:firewall|firewalld|iptables|ip6tables|nftables|netfilter|ufw|fail2ban|docker|libvirt)",re.I)
    suspicious_content=re.compile(rb"(?:^|[ /])(?:nft|iptables|ip6tables|iptables-restore|ip6tables-restore|ufw|firewalld|netfilter-persistent)(?:[ \n]|$)",re.I)
    output=subprocess.check_output(["/usr/bin/systemctl","list-unit-files","--type=service","--state=enabled","--no-legend","--no-pager"],text=True)
    rows=[]
    for name in sorted({line.split()[0] for line in output.splitlines() if line.split()}):
        configuration=subprocess.check_output(["/usr/bin/systemctl","cat",name])
        if not (name in reviewed or suspicious_name.search(name) or suspicious_content.search(configuration)): continue
        if name not in reviewed: raise SystemExit(f"unreviewed enabled firewall loader/mutator: {name}")
        sources=[]
        for raw_path in re.findall(rb"(?m)^# (/[^\n]+)$",configuration):
            source=pathlib.Path(raw_path.decode());sources.append({"path":str(source),"sha256":digest(source)})
        rows.append({"unit":name,"enablement":"enabled","unit_configuration_sha256":hashlib.sha256(configuration).hexdigest(),"sources":sources})
    if rows!=contract["firewall_loader_inventory"]: raise SystemExit("enabled firewall loader inventory changed")
'''
validator_raw = validator_source.replace(
    "__CONTRACT_PATH__", repr(str(contract_path)),
).replace("__CONTRACT_SHA__", repr(contract_sha)).lstrip().encode()
create(validator_path, validator_raw, 0o500); validator_sha = sha(validator_raw)

monitor_source = r'''
import datetime,hashlib,json,os,pathlib,stat,subprocess,sys,time
CONTRACT_PATH=pathlib.Path(__CONTRACT_PATH__)
EXPECTED_CONTRACT_SHA256=__CONTRACT_SHA__
VALIDATOR_PATH=pathlib.Path(__VALIDATOR_PATH__)
EXPECTED_VALIDATOR_SHA256=__VALIDATOR_SHA__
PYTHON_PATH=__PYTHON_PATH__
def canonical(value): return (json.dumps(value,sort_keys=True,separators=(",",":"))+"\n").encode()
def now(): return datetime.datetime.now(datetime.timezone.utc).isoformat(timespec="microseconds").replace("+00:00","Z")
def fsync_dir(path):
    fd=os.open(path,os.O_RDONLY|getattr(os,"O_DIRECTORY",0)|getattr(os,"O_NOFOLLOW",0))
    try: os.fsync(fd)
    finally: os.close(fd)
def create(path,value):
    payload=canonical(value)
    if path.exists() or path.is_symlink():
        if path.is_symlink() or path.read_bytes()!=payload: raise SystemExit("network-fence incident receipt differs")
        return payload
    temporary=path.with_name(f".{path.name}.partial")
    if temporary.exists() or temporary.is_symlink():
        if temporary.is_symlink() or not temporary.is_file(): raise SystemExit("unsafe incident partial")
        temporary.unlink()
    fd=os.open(temporary,os.O_WRONLY|os.O_CREAT|os.O_EXCL|getattr(os,"O_NOFOLLOW",0),0o400)
    with os.fdopen(fd,"wb") as handle: handle.write(payload);handle.flush();os.fsync(handle.fileno())
    os.rename(temporary,path);fsync_dir(path.parent);return payload
raw=CONTRACT_PATH.read_bytes();contract=json.loads(raw)
if hashlib.sha256(raw).hexdigest()!=EXPECTED_CONTRACT_SHA256 or raw!=canonical(contract): raise SystemExit("monitor contract changed")
if hashlib.sha256(VALIDATOR_PATH.read_bytes()).hexdigest()!=EXPECTED_VALIDATOR_SHA256: raise SystemExit("monitor validator changed")
interp=contract["semantic_interpreter"];executable=pathlib.Path(os.path.realpath(sys.executable));details=executable.lstat()
if (sys.flags.isolated!=1 or str(executable)!=interp["normalized_path"]
        or details.st_dev!=interp["device"] or details.st_ino!=interp["inode"]
        or hashlib.sha256(executable.read_bytes()).hexdigest()!=interp["sha256"]
        or {key:os.environ.get(key) for key in interp["environment"]}!=interp["environment"]
        or set(os.environ)!=set(interp["environment"])):
    raise SystemExit("monitor semantic interpreter/environment changed")
intent_path=pathlib.Path(contract["incident_intent_path"]);commit_path=pathlib.Path(contract["incident_commit_path"])
def freeze(entry):
    base=pathlib.Path("/sys/fs/cgroup")/entry["path"].lstrip("/");details=base.lstat()
    if (base.is_symlink() or not base.is_dir() or details.st_dev!=entry["device"] or details.st_ino!=entry["inode"]):
        raise SystemExit("incident cgroup identity changed")
    directory=os.open(base,os.O_RDONLY|getattr(os,"O_DIRECTORY",0)|getattr(os,"O_NOFOLLOW",0))
    try:
        freezer=os.open("cgroup.freeze",os.O_WRONLY|getattr(os,"O_NOFOLLOW",0),dir_fd=directory)
        try: os.write(freezer,b"1")
        finally: os.close(freezer)
    finally: os.close(directory)
    deadline=time.monotonic()+5
    while dict(line.split(" ",1) for line in (base/"cgroup.events").read_text().splitlines()).get("frozen")!="1":
        if time.monotonic()>=deadline: raise SystemExit("incident cgroup did not freeze")
        time.sleep(0.01)
def fail_closed(reason,detail_sha):
    if intent_path.exists() or intent_path.is_symlink():
        intent_raw=intent_path.read_bytes();intent=json.loads(intent_raw)
        if intent_raw!=canonical(intent) or intent.get("monitor_contract_sha256")!=EXPECTED_CONTRACT_SHA256:
            raise SystemExit("incident intent differs")
    else:
        intent={"schema":"arc.recovery.legacy-network-fence-incident-intent.v1",
                "monitor_contract_sha256":EXPECTED_CONTRACT_SHA256,"capture_id":contract["capture_id"],
                "node":contract["node"],"freeze_plan_sha256":contract["freeze_plan_sha256"],
                "observed_boot_id":pathlib.Path("/proc/sys/kernel/random/boot_id").read_text().strip(),
                "reason":reason,"detail_sha256":detail_sha,"detected_at":now(),
                "automatic_clear":False,"global_absence_claimed":False}
        intent_raw=create(intent_path,intent)
    frozen=[];observed_boot=pathlib.Path("/proc/sys/kernel/random/boot_id").read_text().strip()
    if observed_boot==contract["sealed_boot_id"]:
        targets=([contract["supervisor"]["cgroup"]] if contract["writer_supervision_mode"]=="detached-root-session" else [])
        targets.append(contract["writer"]["cgroup"]);seen=set()
        for target in targets:
            identity=(target["path"],target["device"],target["inode"])
            if identity in seen: continue
            seen.add(identity);freeze(target);frozen.append(target)
    marker=pathlib.Path(contract["allow_marker_path"]);expected=b"schema=arc.recovery.legacy-start-allow.v1\n"
    if marker.exists() or marker.is_symlink():
        if marker.is_symlink() or marker.read_bytes()!=expected: raise SystemExit("incident allow marker differs")
        parent=os.open(marker.parent,os.O_RDONLY|getattr(os,"O_DIRECTORY",0)|getattr(os,"O_NOFOLLOW",0))
        try: os.unlink(marker.name,dir_fd=parent);os.fsync(parent)
        finally: os.close(parent)
    else: fsync_dir(marker.parent)
    create(commit_path,{"schema":"arc.recovery.legacy-network-fence-incident-committed.v1",
        "incident_intent_sha256":hashlib.sha256(intent_raw).hexdigest(),"monitor_contract_sha256":EXPECTED_CONTRACT_SHA256,
        "frozen_cgroups":frozen,"allow_marker_absent":True,"allow_marker_parent_fsynced":True,
        "automatic_unfence":False,"global_absence_claimed":False,"committed_at":now()})
    raise SystemExit(77)
def validate(fast):
    env={"PATH":"/usr/bin:/bin","LC_ALL":"C","TZ":"UTC","PYTHONHASHSEED":"0"}
    result=subprocess.run(["/usr/bin/env","-i",*(f"{key}={value}" for key,value in env.items()),
        PYTHON_PATH,"-I",str(VALIDATOR_PATH),*(["--fast"] if fast else [])],
        stdout=subprocess.DEVNULL,stderr=subprocess.PIPE,check=False)
    return result.returncode,hashlib.sha256(result.stderr).hexdigest()
if intent_path.exists() or intent_path.is_symlink(): fail_closed("reconcile-existing-incident",hashlib.sha256(b"existing").hexdigest())
code,detail=validate(False)
if code!=0: fail_closed("startup-validation-failed",detail)
cycle=0
while True:
    time.sleep(0.1);cycle+=1;code,detail=validate(cycle%100!=0)
    if code!=0: fail_closed("continuous-validation-failed",detail)
'''
monitor_raw = monitor_source.replace(
    "__CONTRACT_PATH__", repr(str(contract_path)),
).replace("__CONTRACT_SHA__", repr(contract_sha)).replace(
    "__VALIDATOR_PATH__", repr(str(validator_path)),
).replace("__VALIDATOR_SHA__", repr(validator_sha)).replace(
    "__PYTHON_PATH__", repr(str(python_path)),
).lstrip().encode()
create(monitor_path, monitor_raw, 0o500); monitor_sha = sha(monitor_raw)

unit_raw = secure(unit_path, 0o400)
dependency_paths = [
    pathlib.Path(f"/etc/systemd/system/{name}.d/zzzy-arc-recovery-network-fence.conf")
    for name in ("arc-self-heal.service", "arc-node.service",
                 "arc-node-update.service", "arc-node-update.timer")
]
dependency_sha = {str(path): sha(secure(path, 0o400)) for path in dependency_paths}
for service in ("arc-self-heal.service", "arc-node.service", "arc-node-update.service"):
    raw = pathlib.Path(f"/etc/systemd/system/{service}.d/zzzy-arc-recovery-network-fence.conf").read_text()
    expected = (
        f"ExecStartPre=/usr/bin/env -i PATH=/usr/bin:/bin LC_ALL=C TZ=UTC "
        f"PYTHONHASHSEED=0 {python_path} -I {validator_path}"
    )
    if raw.count(expected) != 1:
        raise SystemExit(f"legacy service lacks the exact network validator: {service}")
subprocess.run(["/usr/bin/systemctl", "daemon-reload"], check=True)
subprocess.run(["/usr/bin/systemctl", "enable", "arc-legacy-maintenance-fence.service"],
               stdout=subprocess.DEVNULL, check=True)
subprocess.run(["/usr/bin/sync"], check=True)
if subprocess.check_output(
        ["/usr/bin/systemctl", "is-active", "arc-legacy-maintenance-fence.service"], text=True,
).strip() != "active":
    subprocess.run(["/usr/bin/systemctl", "start", "arc-legacy-maintenance-fence.service"], check=True)
if subprocess.check_output(
        ["/usr/bin/systemctl", "is-active", "arc-legacy-maintenance-fence.service"], text=True,
).strip() != "active":
    raise SystemExit("network-quarantine continuous monitor is not active")
clean_env = {"PATH": "/usr/bin:/bin", "LC_ALL": "C", "TZ": "UTC", "PYTHONHASHSEED": "0"}
subprocess.run([
    "/usr/bin/env", "-i", *(f"{key}={value}" for key, value in clean_env.items()),
    str(python_path), "-I", str(validator_path),
], check=True)
value = {
    "schema": "arc.recovery.legacy-network-quarantine-monitor.v1",
    "capture_id": capture_id, "node": node, "freeze_plan_sha256": freeze_sha,
    "network_quarantine_receipt_sha256": sha(base_raw),
    "monitor_contract_sha256": contract_sha, "semantic_interpreter": interpreter,
    "firewall_loader_inventory": loaders,
    "file_sha256": {str(contract_path): contract_sha, str(validator_path): validator_sha,
                    str(monitor_path): monitor_sha, str(unit_path): sha(unit_raw), **dependency_sha},
    "unit": {"name": "arc-legacy-maintenance-fence.service", "active": True,
             "enabled": True, "continuous_poll_interval_milliseconds": 100,
             "full_loader_revalidation_interval_seconds": 10},
    "legacy_exec_start_pre": {
        name: str(validator_path) for name in (
            "arc-self-heal.service", "arc-node.service", "arc-node-update.service",
        )
    },
    "incident_latched": False, "continuous_fail_closed": True,
    "automatic_unfence": False, "global_absence_claimed": False,
}
create(receipt_path, canonical(value), 0o400); fsync_dir(root)
PY
}

verify_legacy_network_quarantine() {
    local root="$1" capture_id="$2" node="$3" freeze_sha="$4"
    python3 - "$root" "$capture_id" "$node" "$freeze_sha" \
        "$NETWORK_FENCE_STATE" "$NETWORK_FENCE_UNIT" <<'PY'
import hashlib, json, os, pathlib, re, stat, subprocess, sys
root, capture_id, node, freeze_sha, state_raw, unit_raw = sys.argv[1:]
root = pathlib.Path(root); state = pathlib.Path(state_raw); unit = pathlib.Path(unit_raw)
canonical = lambda value: (json.dumps(value, sort_keys=True, separators=(",", ":")) + "\n").encode()
def secure(path, mode):
    details = path.lstat()
    if (path.is_symlink() or not stat.S_ISREG(details.st_mode) or details.st_uid != 0
            or details.st_gid != 0 or details.st_nlink != 1 or stat.S_IMODE(details.st_mode) != mode):
        raise SystemExit(f"unsafe network-quarantine evidence: {path}")
    return path.read_bytes()
receipt_raw = secure(root / "08-network-quarantine.json", 0o400)
receipt = json.loads(receipt_raw)
if (receipt_raw != canonical(receipt)
        or receipt.get("schema") != "arc.recovery.legacy-network-quarantine.v1"
        or (receipt.get("capture_id"), receipt.get("node"), receipt.get("freeze_plan_sha256"))
        != (capture_id, node, freeze_sha)
        or receipt.get("table") != {"family":"inet","name":"arc_legacy_maintenance_v1","priority":-310,
            "hooks":["prerouting","input","forward","output"],"policy":"accept",
            "loopback_retained":True,"ssh_unmatched_retained":True}
        or receipt.get("quarantine_policy") != {
            "mode":"deny-all-nonloopback-except-host-maintenance",
            "families":["ipv4","ipv6"],"directions":["input","output","forward"],
            "priority_before_conntrack":True,"established_bypass":False,
            "allowed":["loopback","ssh-tcp-22","dhcpv4-67-68","dhcpv6-546-547",
                       "icmpv6-ndp-ra-packet-too-big"],
            "legacy_rpc_p2p_web_dynamic_all_blocked":True}
        or receipt.get("threat_model", {}).get("legacy_binary") != "reviewed-non-adversarial-exact-hash"
        or receipt.get("global_absence_claimed") is not False):
    raise SystemExit("network-quarantine receipt identity/coverage differs")
monitor_raw = secure(root / "08-network-quarantine-monitor.json", 0o400)
monitor = json.loads(monitor_raw)
monitor_keys = {
    "schema", "capture_id", "node", "freeze_plan_sha256",
    "network_quarantine_receipt_sha256", "monitor_contract_sha256",
    "semantic_interpreter", "firewall_loader_inventory", "file_sha256", "unit",
    "legacy_exec_start_pre", "incident_latched", "continuous_fail_closed",
    "automatic_unfence", "global_absence_claimed",
}
if (monitor_raw != canonical(monitor) or set(monitor) != monitor_keys
        or monitor["schema"] != "arc.recovery.legacy-network-quarantine-monitor.v1"
        or (monitor["capture_id"], monitor["node"], monitor["freeze_plan_sha256"])
        != (capture_id, node, freeze_sha)
        or monitor["network_quarantine_receipt_sha256"] != hashlib.sha256(receipt_raw).hexdigest()
        or not re.fullmatch(r"[0-9a-f]{64}", monitor["monitor_contract_sha256"])
        or monitor["incident_latched"] is not False
        or monitor["continuous_fail_closed"] is not True
        or monitor["automatic_unfence"] is not False
        or monitor["global_absence_claimed"] is not False
        or monitor["unit"] != {
            "name": "arc-legacy-maintenance-fence.service", "active": True,
            "enabled": True, "continuous_poll_interval_milliseconds": 100,
            "full_loader_revalidation_interval_seconds": 10,
        }
        or monitor["legacy_exec_start_pre"] != {
            name: str(state / "validate") for name in (
                "arc-self-heal.service", "arc-node.service", "arc-node-update.service",
            )
        }):
    raise SystemExit("network-quarantine monitor receipt differs")
interpreter = monitor.get("semantic_interpreter")
if (not isinstance(interpreter, dict) or set(interpreter) != {
        "normalized_path", "sha256", "device", "inode", "uid", "gid", "mode", "nlink",
        "isolated", "environment",
    } or interpreter["uid"] != 0 or interpreter["gid"] != 0
        or interpreter["mode"] != 0o755 or interpreter["nlink"] != 1
        or interpreter["isolated"] is not True
        or interpreter["environment"] != {
            "PATH": "/usr/bin:/bin", "LC_ALL": "C", "TZ": "UTC", "PYTHONHASHSEED": "0",
        }):
    raise SystemExit("network-quarantine semantic interpreter receipt differs")
interpreter_path = pathlib.Path(interpreter["normalized_path"])
interpreter_details = interpreter_path.lstat()
if (interpreter_path.is_symlink() or not stat.S_ISREG(interpreter_details.st_mode)
        or interpreter_details.st_uid != interpreter["uid"]
        or interpreter_details.st_gid != interpreter["gid"]
        or stat.S_IMODE(interpreter_details.st_mode) != interpreter["mode"]
        or interpreter_details.st_nlink != interpreter["nlink"]
        or interpreter_details.st_dev != interpreter["device"]
        or interpreter_details.st_ino != interpreter["inode"]
        or hashlib.sha256(interpreter_path.read_bytes()).hexdigest() != interpreter["sha256"]):
    raise SystemExit("network-quarantine semantic interpreter changed")
file_hashes = monitor.get("file_sha256")
expected_monitor_files = {
    str(state / "monitor-contract.json"): 0o400,
    str(state / "validate"): 0o500,
    str(state / "monitor"): 0o500,
    str(unit): 0o400,
    **{
        f"/etc/systemd/system/{name}.d/zzzy-arc-recovery-network-fence.conf": 0o400
        for name in (
            "arc-self-heal.service", "arc-node.service",
            "arc-node-update.service", "arc-node-update.timer",
        )
    },
}
if not isinstance(file_hashes, dict) or set(file_hashes) != set(expected_monitor_files):
    raise SystemExit("network-quarantine monitor file inventory differs")
for path_raw, mode in expected_monitor_files.items():
    raw = secure(pathlib.Path(path_raw), mode)
    if hashlib.sha256(raw).hexdigest() != file_hashes[path_raw]:
        raise SystemExit(f"network-quarantine monitor file changed: {path_raw}")
contract_raw = secure(state / "monitor-contract.json", 0o400)
if hashlib.sha256(contract_raw).hexdigest() != monitor["monitor_contract_sha256"]:
    raise SystemExit("network-quarantine monitor contract hash differs")
if (state / "incident.intent.json").exists() or (state / "incident.intent.json").is_symlink() \
        or (state / "incident.committed.json").exists() or (state / "incident.committed.json").is_symlink():
    raise SystemExit("network-quarantine monitor incident is latched")
clean_env = ["PATH=/usr/bin:/bin", "LC_ALL=C", "TZ=UTC", "PYTHONHASHSEED=0"]
subprocess.run([
    "/usr/bin/env", "-i", *clean_env, str(interpreter_path), "-I", str(state / "validate"),
], check=True)
nft_path=pathlib.Path("/usr/sbin/nft"); nft_details=nft_path.lstat(); state_details=state.lstat()
if (nft_path.is_symlink() or not stat.S_ISREG(nft_details.st_mode) or nft_details.st_uid!=0
        or nft_details.st_gid!=0 or nft_details.st_mode & 0o022 or nft_details.st_nlink!=1
        or state.is_symlink() or not stat.S_ISDIR(state_details.st_mode) or state_details.st_uid!=0
        or state_details.st_gid!=0 or stat.S_IMODE(state_details.st_mode)!=0o700):
    raise SystemExit("network-quarantine tool/state inode is unsafe")
if hashlib.sha256(nft_path.read_bytes()).hexdigest() != receipt.get("tool_sha256", {}).get("/usr/sbin/nft"):
    raise SystemExit("network-quarantine nft tool hash differs")

def payload_matches(expr):
    result=[]
    for row in expr:
        match=row.get("match") if isinstance(row,dict) else None
        if not isinstance(match,dict): continue
        left=match.get("left")
        if isinstance(left,dict) and isinstance(left.get("payload"),dict):
            right=match.get("right")
            values=sorted(right.get("set",[])) if isinstance(right,dict) else [right]
            result.append((left["payload"].get("protocol"),left["payload"].get("field"),values))
    return result

runtime=json.loads(subprocess.check_output(
    ["/usr/sbin/nft","--json","list","table","inet","arc_legacy_maintenance_v1"]
))
rows=[entry for entry in runtime.get("nftables",[]) if "metainfo" not in entry]
tables=[entry["table"] for entry in rows if "table" in entry]
if (len(tables)!=1 or tables[0].get("family")!="inet"
        or tables[0].get("name")!="arc_legacy_maintenance_v1"
        or tables[0].get("comment")!=f"arc-recovery:capture={capture_id}:node={node}"
        or set(tables[0])!={"family","name","handle","comment"}):
    raise SystemExit("live network-quarantine table identity differs")
chains=[entry["chain"] for entry in rows if "chain" in entry]
expected_chains=[("prerouting","prerouting"),("input","input"),("forward","forward"),("output","output")]
if len(chains)!=4 or any(set(chain)!={"family","table","name","handle","type","hook","prio","policy"}
        or any(chain.get(key)!=wanted for key,wanted in {
        "family":"inet","table":"arc_legacy_maintenance_v1","name":name,
        "type":"filter","hook":hook,"prio":-310,"policy":"accept"}.items())
        for chain,(name,hook) in zip(chains,expected_chains)):
    raise SystemExit("live network-quarantine chain AST differs")
expected=[]
for chain,_hook in expected_chains:
    if chain in {"prerouting","input"}:
        expected += [(chain,"iifname",slug) for slug in
                     ("loopback","ssh","dhcp4","dhcp6","icmpv6-control","deny")]
    elif chain=="output":
        expected += [(chain,"oifname",slug) for slug in
                     ("loopback","ssh","dhcp4","dhcp6","icmpv6-control","deny")]
    else:
        expected += [(chain,None,"deny-all")]
rules=[entry["rule"] for entry in rows if "rule" in entry]
if len(rules)!=len(expected): raise SystemExit("live network-quarantine rule count differs")
semantic=[]
for rule,(chain,interface,slug) in zip(rules,expected):
    comment=f"arc-recovery:{chain}:{interface or 'all'}:{slug}"
    if (rule.get("family"),rule.get("table"),rule.get("chain"),rule.get("comment")) != (
            "inet","arc_legacy_maintenance_v1",chain,comment):
        raise SystemExit(f"live network-quarantine rule order differs: {comment}")
    if set(rule)!={"family","table","chain","handle","expr","comment"}:
        raise SystemExit(f"live network-quarantine rule has unknown attributes: {comment}")
    expr=rule.get("expr",[])
    counters=[row.get("counter") for row in expr if isinstance(row,dict) and "counter" in row]
    verdicts=[key for row in expr if isinstance(row,dict) for key in ("accept","drop") if key in row]
    iface=[]
    for row in expr:
        match=row.get("match") if isinstance(row,dict) else None
        if interface is not None and isinstance(match,dict) and match.get("left")=={"meta":{"key":interface}}:
            iface.append((match.get("op"),match.get("right")))
    wanted_iface=None if interface is None else (("==","lo") if slug=="loopback" else ("!=","lo"))
    wanted_verdict="drop" if slug in {"deny","deny-all"} else "accept"
    payload=payload_matches(expr)
    if slug in {"loopback","deny","deny-all"}: wanted_payload=[]
    elif slug=="ssh": wanted_payload=[("tcp","sport" if chain=="output" else "dport",[22])]
    elif slug=="dhcp4": wanted_payload=[("udp","sport",[68 if chain=="output" else 67]),
                                         ("udp","dport",[67 if chain=="output" else 68])]
    elif slug=="dhcp6": wanted_payload=[("udp","sport",[546 if chain=="output" else 547]),
                                         ("udp","dport",[547 if chain=="output" else 546])]
    else: wanted_payload=[("icmpv6","type",[2,133,134,135,136])]
    expected_expr=[]
    if interface is not None:
        expected_expr.append(("interface",interface,wanted_iface[0],wanted_iface[1]))
    expected_expr.extend(("payload",protocol,field,values) for protocol,field,values in wanted_payload)
    expected_expr.extend((("counter",),("verdict",wanted_verdict)))
    actual_expr=[]
    for row in expr:
        if not isinstance(row,dict) or len(row)!=1:
            raise SystemExit(f"live network-quarantine expr has unknown attributes: {comment}")
        key=next(iter(row))
        if key=="match":
            match=row[key]
            if not isinstance(match,dict) or set(match)!={"op","left","right"}:
                raise SystemExit(f"live network-quarantine match has unknown attributes: {comment}")
            left=match["left"]; right=match["right"]
            if left=={"meta":{"key":interface}}:
                actual_expr.append(("interface",interface,match["op"],right))
            elif isinstance(left,dict) and set(left)=={"payload"} and isinstance(left["payload"],dict) \
                    and set(left["payload"])=={"protocol","field"}:
                values=sorted(right["set"]) if isinstance(right,dict) and set(right)=={"set"} else [right]
                actual_expr.append(("payload",left["payload"]["protocol"],left["payload"]["field"],values))
            else:
                raise SystemExit(f"live network-quarantine has an unknown match expression: {comment}")
        elif key=="counter":
            if not isinstance(row[key],dict) or set(row[key])!={"packets","bytes"}:
                raise SystemExit(f"live network-quarantine counter attributes differ: {comment}")
            actual_expr.append(("counter",))
        elif key in {"accept","drop"} and row[key] is None:
            actual_expr.append(("verdict",key))
        else:
            raise SystemExit(f"live network-quarantine has an unknown expr: {comment}")
    if actual_expr!=expected_expr:
        raise SystemExit(f"live network-quarantine expr order/shape differs: {comment}")
    if ((iface!=[wanted_iface] if interface is not None else bool(iface))
            or verdicts!=[wanted_verdict] or len(counters)!=1 or payload!=wanted_payload):
        raise SystemExit(f"live network-quarantine rule expression differs: {comment}")
    semantic.append({"chain":chain,"interface":interface,"slug":slug,"payload":payload,
                     "verdict":wanted_verdict,"comment":comment})
if hashlib.sha256(canonical(semantic)).hexdigest() != receipt.get("owned_rule_ast_sha256"):
    raise SystemExit("live network-quarantine normalized AST hash differs from receipt")
stateless=subprocess.check_output(
    ["/usr/sbin/nft","--stateless","list","table","inet","arc_legacy_maintenance_v1"]
)
if hashlib.sha256(stateless).hexdigest()!=receipt.get("owned_ruleset_stateless_sha256"):
    raise SystemExit("live network-quarantine exact stateless ruleset hash differs from receipt")

def normalize_nonowned(value):
    def scrub(item):
        if isinstance(item,dict):
            return {key:scrub(val) for key,val in sorted(item.items())
                    if key not in {"handle","position","index","packets","bytes"}}
        if isinstance(item,list): return [scrub(entry) for entry in item]
        return item
    result=[]
    for entry in value.get("nftables",[]):
        if "metainfo" in entry: continue
        obj=next(iter(entry.values())) if isinstance(entry,dict) and len(entry)==1 else None
        if isinstance(obj,dict) and ((entry.get("table",{}).get("family"),entry.get("table",{}).get("name"))==
                ("inet","arc_legacy_maintenance_v1") or
                (obj.get("family"),obj.get("table"))==("inet","arc_legacy_maintenance_v1")):
            continue
        result.append(scrub(entry))
    return result
baseline=json.loads((state/"preexisting-ruleset.json").read_bytes())
current=json.loads(subprocess.check_output(["/usr/sbin/nft","--json","list","ruleset"]))
baseline_sha=hashlib.sha256(canonical(normalize_nonowned(baseline))).hexdigest()
current_sha=hashlib.sha256(canonical(normalize_nonowned(current))).hexdigest()
if current_sha!=baseline_sha or current_sha!=receipt.get("preexisting_firewall_structural_sha256"):
    raise SystemExit("nonowned firewall changed under network quarantine")
dependency_paths=[pathlib.Path(f"/etc/systemd/system/{name}.d/zzzy-arc-recovery-network-fence.conf")
                  for name in ("arc-self-heal.service","arc-node.service",
                               "arc-node-update.service","arc-node-update.timer")]
for path, mode in ((state / "owner.json", 0o400), (state / "policy.nft", 0o400),
                   (state / "nft", 0o500), (state / "apply", 0o500), (state / "ensure", 0o500),
                   (state / "preexisting-ruleset.json", 0o400), (unit, 0o400),
                   *((path,0o400) for path in dependency_paths)):
    raw = secure(path, mode)
    key = str(path) if path in dependency_paths or path == unit else path.name
    if hashlib.sha256(raw).hexdigest() != receipt["file_sha256"].get(key):
        raise SystemExit(f"network-quarantine persisted file changed: {path}")
if subprocess.run(["/usr/sbin/nft", "list", "table", "inet", "arc_legacy_maintenance_v1"],
                  stdout=subprocess.DEVNULL, stderr=subprocess.DEVNULL).returncode != 0:
    raise SystemExit("network-quarantine table is absent")
if subprocess.check_output(["/usr/bin/systemctl", "is-active", "arc-legacy-maintenance-fence.service"], text=True).strip() != "active":
    raise SystemExit("network-quarantine unit is not active")
if subprocess.check_output(["/usr/bin/systemctl", "is-enabled", "arc-legacy-maintenance-fence.service"], text=True).strip() != "enabled":
    raise SystemExit("network-quarantine unit is not enabled")
for legacy_unit in ("arc-self-heal.service","arc-node.service",
                    "arc-node-update.service","arc-node-update.timer"):
    requires=subprocess.check_output(
        ["/usr/bin/systemctl","show",legacy_unit,"--property=Requires","--value"],text=True,
    ).split()
    after=subprocess.check_output(
        ["/usr/bin/systemctl","show",legacy_unit,"--property=After","--value"],text=True,
    ).split()
    if "arc-legacy-maintenance-fence.service" not in requires or "arc-legacy-maintenance-fence.service" not in after:
        raise SystemExit(f"legacy starter can bypass network-quarantine dependency: {legacy_unit}")
    if legacy_unit.endswith(".service"):
        merged = subprocess.check_output(["/usr/bin/systemctl", "cat", legacy_unit], text=True)
        expected = (
            f"ExecStartPre=/usr/bin/env -i PATH=/usr/bin:/bin LC_ALL=C TZ=UTC "
            f"PYTHONHASHSEED=0 {interpreter_path} -I {state / 'validate'}"
        )
        if merged.count(expected) != 1:
            raise SystemExit(f"legacy starter exact ExecStartPre validator differs: {legacy_unit}")
unit_after = subprocess.check_output(
    ["/usr/bin/systemctl", "show", "arc-legacy-maintenance-fence.service",
     "--property=After", "--value"], text=True,
).split()
loader_units = [row["unit"] for row in monitor["firewall_loader_inventory"]]
if any(name not in unit_after for name in loader_units):
    raise SystemExit("network-quarantine monitor is not ordered after every enabled firewall loader")
PY
}

quarantine_restart_arm() {
    local capture_id="$1" node="$2" freeze_sha="$3" root
    require_hash "$capture_id" "capture id"; require_node "$node"; require_hash "$freeze_sha" "freeze plan hash"
    local partial="$STOP_BASE/$capture_id/.$node.stop.partial" final="$STOP_BASE/$capture_id/$node"
    if [ -d "$final" ] && [ ! -L "$final" ]; then root="$final"
    elif [ -d "$partial" ] && [ ! -L "$partial" ]; then root="$partial"
    else die "network-quarantine restart-arm journal is missing"
    fi
    verify_legacy_network_quarantine "$root" "$capture_id" "$node" "$freeze_sha"
    python3 - "$root" "$capture_id" "$node" "$freeze_sha" "$NETWORK_FENCE_STATE" <<'PY'
import datetime,hashlib,json,os,pathlib,re,stat,subprocess,sys,time
root=pathlib.Path(sys.argv[1]);capture,node,freeze,state_raw=sys.argv[2:]
state=pathlib.Path(state_raw);marker=pathlib.Path("/etc/arc-recovery/legacy-start-allowed")
arm_path=root/"09-quarantine-restart-arm.json"
frozen_path=root/"09-quarantine-restart-supervisor-frozen.json"
commit_path=root/"09-quarantine-restart-committed.json"
units=("arc-self-heal.service","arc-node.service","arc-node-update.service","arc-node-update.timer")
marker_payload=b"schema=arc.recovery.legacy-start-allow.v1\n"
barrier_payload=b"[Unit]\nConditionPathExists=/etc/arc-recovery/legacy-start-allowed\n"
partial_arm_path=pathlib.Path(f"/root/arc-recovery-stops/{capture}/.{node}.stop.partial/09-quarantine-restart-arm.json")
final_arm_path=pathlib.Path(f"/root/arc-recovery-stops/{capture}/{node}/09-quarantine-restart-arm.json")
arm_barrier_payload=("[Unit]\n"
    f"ConditionPathExists=!{partial_arm_path}\n"
    f"ConditionPathExists=!{final_arm_path}\n").encode()
canonical=lambda value:(json.dumps(value,sort_keys=True,separators=(",",":"))+"\n").encode()
sha=lambda raw:hashlib.sha256(raw).hexdigest()
now=lambda:datetime.datetime.now(datetime.timezone.utc).isoformat(timespec="microseconds").replace("+00:00","Z")
def digest(path):
    value=hashlib.sha256()
    with open(path,"rb") as handle:
        for chunk in iter(lambda:handle.read(1024*1024),b""):value.update(chunk)
    return value.hexdigest()
def fsync_dir(path):
    fd=os.open(path,os.O_RDONLY|getattr(os,"O_DIRECTORY",0)|getattr(os,"O_NOFOLLOW",0))
    try:os.fsync(fd)
    finally:os.close(fd)
def secure(path,mode=0o400):
    details=path.lstat()
    if (path.is_symlink() or not stat.S_ISREG(details.st_mode) or details.st_uid!=0
            or details.st_gid!=0 or details.st_nlink!=1 or stat.S_IMODE(details.st_mode)!=mode):
        raise SystemExit(f"unsafe quarantine restart source: {path}")
    return path.read_bytes()
def publish(path,value):
    payload=canonical(value)
    if path.exists() or path.is_symlink():
        if path.is_symlink() or secure(path)!=payload:raise SystemExit(f"quarantine restart receipt differs: {path.name}")
        return payload
    temporary=path.with_name(f".{path.name}.partial")
    if temporary.exists() or temporary.is_symlink():
        if temporary.is_symlink() or not temporary.is_file():raise SystemExit("unsafe quarantine restart partial")
        if temporary.read_bytes()==payload:
            os.chmod(temporary,0o400);os.replace(temporary,path);fsync_dir(path.parent);return payload
        temporary.unlink()
    fd=os.open(temporary,os.O_WRONLY|os.O_CREAT|os.O_EXCL|getattr(os,"O_NOFOLLOW",0),0o400)
    with os.fdopen(fd,"wb") as handle:handle.write(payload);handle.flush();os.fsync(handle.fileno())
    os.replace(temporary,path);fsync_dir(path.parent);return payload
def create_exact(path,payload,mode):
    if path.exists() or path.is_symlink():
        if path.is_symlink() or secure(path,mode)!=payload:
            raise SystemExit(f"quarantine restart create-only file differs: {path}")
        return
    temporary=path.with_name(f".{path.name}.partial")
    if temporary.exists() or temporary.is_symlink():
        if temporary.is_symlink() or not temporary.is_file():
            raise SystemExit("unsafe quarantine restart barrier partial")
        if temporary.read_bytes()==payload:
            os.chmod(temporary,mode);os.replace(temporary,path);fsync_dir(path.parent);return
        temporary.unlink()
    fd=os.open(temporary,os.O_WRONLY|os.O_CREAT|os.O_EXCL|getattr(os,"O_NOFOLLOW",0),mode)
    with os.fdopen(fd,"wb") as handle:handle.write(payload);handle.flush();os.fsync(handle.fileno())
    os.chmod(temporary,mode);os.replace(temporary,path);fsync_dir(path.parent)
def prop(unit,name):
    return subprocess.check_output(["/usr/bin/systemctl","show",unit,f"--property={name}","--value"],text=True).strip()
def proc_start(pid):
    raw=pathlib.Path(f"/proc/{pid}/stat").read_text();end=raw.rfind(")");fields=raw[end+2:].split()
    if end<0 or len(fields)<20:raise SystemExit("quarantine restart process stat is truncated")
    return int(fields[19])
def verify_process(pid,start,executable,executable_sha,argv_sha,label):
    proc=pathlib.Path(f"/proc/{pid}")
    if (proc_start(pid)!=start or os.readlink(proc/"exe")!=executable
            or digest(proc/"exe")!=executable_sha or sha((proc/"cmdline").read_bytes())!=argv_sha):
        raise SystemExit(f"sealed {label} changed before quarantine restart arm")
def unified(pid):
    raw=pathlib.Path(f"/proc/{pid}/cgroup").read_bytes()
    rows=[line.split(":",2)[2] for line in raw.decode().splitlines() if line.startswith("0::")]
    if len(rows)!=1 or not re.fullmatch(r"/[A-Za-z0-9._@/-]+",rows[0]):raise SystemExit("process cgroup is ambiguous")
    return rows[0],raw
def cgroup_identity(role,path):
    base=pathlib.Path("/sys/fs/cgroup")/path.lstrip("/");details=base.lstat()
    if base.is_symlink() or not base.is_dir():raise SystemExit(f"{role} cgroup is unsafe")
    members=set()
    for current,directories,_files in os.walk(base,followlinks=False):
        directories.sort();current_path=pathlib.Path(current)
        if current_path.is_symlink():raise SystemExit("cgroup subtree contains a symlink")
        members.update(int(value) for value in (current_path/"cgroup.procs").read_text().splitlines())
    return {"role":role,"path":path,"device":details.st_dev,"inode":details.st_ino},sorted(members)
plan_path=pathlib.Path(f"/root/.arc-recovery-plans/{freeze}/freeze.lock.json")
plan_raw=secure(plan_path);plan=json.loads(plan_raw)
if sha(plan_raw)!=freeze or plan_raw!=canonical(plan):raise SystemExit("quarantine restart freeze plan changed")
rows=[row for row in plan["nodes"] if row.get("name")==node]
if len(rows)!=1:raise SystemExit("quarantine restart node is not unique")
row=rows[0];base_raw=secure(root/"08-network-quarantine.json");base=json.loads(base_raw)
monitor_raw=secure(root/"08-network-quarantine-monitor.json");monitor=json.loads(monitor_raw)
if (base_raw!=canonical(base) or monitor_raw!=canonical(monitor)
        or monitor["network_quarantine_receipt_sha256"]!=sha(base_raw)
        or monitor["incident_latched"] is not False):raise SystemExit("quarantine restart network evidence differs")
existing_arm=None;existing_arm_raw=None
if arm_path.exists() or arm_path.is_symlink():
    existing_arm_raw=secure(arm_path);existing_arm=json.loads(existing_arm_raw)
    if (existing_arm_raw!=canonical(existing_arm)
            or existing_arm.get("schema")!="arc.recovery.quarantine-live-restart-arm.v1"):
        raise SystemExit("existing quarantine restart arm differs")
interpreter=monitor["semantic_interpreter"];env=interpreter["environment"]
subprocess.run(["/usr/bin/env","-i",*(f"{key}={value}" for key,value in env.items()),
    interpreter["normalized_path"],"-I",str(state/"validate")],check=True)
boot=pathlib.Path("/proc/sys/kernel/random/boot_id").read_text().strip()
selected=row["supervisor_unit"];supervisor_pid=row["supervisor_main_pid"];writer_pid=row["writer_pid"]
writer_mode=row["writer_supervision_mode"]
if boot==row["boot_id"]:
    verify_process(supervisor_pid,row["supervisor_start_ticks"],row["supervisor_executable_path"],
                   row["supervisor_executable_sha256"],row["supervisor_argv_sha256"],"supervisor")
    verify_process(writer_pid,row["writer_start_ticks"],row["executable_path"],
                   row["executable_sha256"],row["argv_sha256"],"writer")
    writer_path,writer_cgroup_raw=unified(writer_pid)
    if writer_path!=row["writer_cgroup_path"] or sha(writer_cgroup_raw)!=row["writer_cgroup_sha256"]:
        raise SystemExit("quarantine restart writer cgroup changed")
    required={"MainPID":str(supervisor_pid),"ActiveState":"active","Restart":"no",
              "RefuseManualStart":"yes","RefuseManualStop":"yes","KillMode":"process",
              "SendSIGKILL":"no","OOMPolicy":"continue","WatchdogUSec":"0","RuntimeMaxUSec":"infinity"}
    if any(prop(selected,key)!=wanted for key,wanted in required.items()) or prop(selected,"Job") not in {"","0"}:
        raise SystemExit("selected supervisor remains restart-capable")
elif existing_arm is None:
    raise SystemExit("sealed boot ended before a durable quarantine restart arm")
barrier_hashes={}
for unit in units:
    raw=secure(pathlib.Path(f"/etc/systemd/system/{unit}.d/zzzz-arc-recovery-freeze.conf"),0o444)
    if raw!=barrier_payload:raise SystemExit(f"persistent start barrier differs: {unit}")
    barrier_hashes[unit]=sha(raw)
    merged=subprocess.check_output(["/usr/bin/systemctl","cat",unit],text=True)
    section=None;conditions=[]
    for raw_line in merged.splitlines():
        line=raw_line.strip()
        if not line or line.startswith(("#",";")):continue
        if line.startswith("[") and line.endswith("]"):section=line[1:-1];continue
        if section=="Unit" and line.startswith("ConditionPathExists="):
            value=line.split("=",1)[1]
            if value=="":conditions.clear()
            else:conditions.append(value)
    if str(marker) not in conditions:raise SystemExit(f"merged start condition differs: {unit}")
    if boot==row["boot_id"] and unit!=selected:
        if prop(unit,"ActiveState") not in {"inactive","failed"} or prop(unit,"Job") not in {"","0"}:
            raise SystemExit(f"alternative activation source is active: {unit}")
        if unit.endswith(".service") and prop(unit,"MainPID")!="0":raise SystemExit(f"alternative service has a PID: {unit}")
runtime_intent_raw=secure(root/"01-prefreeze-runtime-safety-intent.json")
runtime_intent=json.loads(runtime_intent_raw);runtime_path=pathlib.Path(runtime_intent["runtime_dropin_path"])
if runtime_intent_raw!=canonical(runtime_intent) or runtime_intent["supervisor_unit"]!=selected:
    raise SystemExit("runtime Restart=no safety intent changed")
if boot==row["boot_id"]:
    runtime_raw=secure(runtime_path,0o444);runtime_safety_sha=sha(runtime_raw)
    if runtime_safety_sha!=runtime_intent["runtime_dropin_sha256"]:
        raise SystemExit("runtime Restart=no safety changed")
else:
    if runtime_path.exists() or runtime_path.is_symlink():
        raise SystemExit("sealed-boot runtime Restart=no drop-in survived reboot")
    runtime_safety_sha=existing_arm.get("selected_runtime_safety_sha256")
    if runtime_safety_sha!=runtime_intent["runtime_dropin_sha256"]:
        raise SystemExit("historical runtime Restart=no safety hash differs")
if marker.exists() or marker.is_symlink():
    marker_raw=secure(marker);details=marker.lstat()
    if marker_raw!=marker_payload:raise SystemExit("legacy start allow marker differs")
    marker_identity={"path":str(marker),"sha256":sha(marker_raw),"device":details.st_dev,"inode":details.st_ino,
                     "uid":details.st_uid,"gid":details.st_gid,"mode":stat.S_IMODE(details.st_mode),"size":details.st_size}
elif existing_arm is not None:
    marker_identity=existing_arm["allow_marker"]
else:raise SystemExit("allow marker absent without durable quarantine restart arm")
freeze_required=writer_mode=="detached-root-session";supervisor_cgroup=None
if boot==row["boot_id"]:
    supervisor_path,_=unified(supervisor_pid);supervisor_cgroup,members=cgroup_identity("supervisor",supervisor_path)
    if freeze_required and (members!=[supervisor_pid] or supervisor_path==row["writer_cgroup_path"]):
        raise SystemExit("detached supervisor containment differs")
    if not freeze_required and (supervisor_pid!=writer_pid or supervisor_path!=row["writer_cgroup_path"]):
        raise SystemExit("systemd-unit writer containment differs")
else:
    supervisor_cgroup=existing_arm.get("detached_supervisor_cgroup")
arm_start_barrier_hashes={}
for unit in units:
    path=pathlib.Path(f"/etc/systemd/system/{unit}.d/zzzx-arc-recovery-quarantine-arm.conf")
    if not path.exists() and not path.is_symlink():
        if boot!=row["boot_id"] or existing_arm is not None:
            raise SystemExit(f"durable quarantine arm start barrier is missing: {unit}")
        create_exact(path,arm_barrier_payload,0o444)
    raw=secure(path,0o444)
    if raw!=arm_barrier_payload:raise SystemExit(f"durable quarantine arm start barrier differs: {unit}")
    arm_start_barrier_hashes[unit]=sha(raw)
subprocess.run(["/usr/bin/systemctl","daemon-reload"],check=True)
subprocess.run(["/usr/bin/sync"],check=True)
for unit in units:
    merged=subprocess.check_output(["/usr/bin/systemctl","cat",unit],text=True)
    section=None;conditions=[]
    for raw_line in merged.splitlines():
        line=raw_line.strip()
        if not line or line.startswith(("#",";")):continue
        if line.startswith("[") and line.endswith("]"):section=line[1:-1];continue
        if section=="Unit" and line.startswith("ConditionPathExists="):
            value=line.split("=",1)[1]
            if value=="":conditions.clear()
            else:conditions.append(value)
    for wanted in (str(marker),f"!{partial_arm_path}",f"!{final_arm_path}"):
        if conditions.count(wanted)!=1:
            raise SystemExit(f"merged durable quarantine arm condition differs: {unit}")
if boot!=row["boot_id"]:
    arc_pids=[path.parent.name for path in pathlib.Path("/proc").glob("[0-9]*/comm")
              if path.read_text(errors="replace").strip()=="arc-node"]
    if arc_pids:raise SystemExit(f"legacy writer restarted after quarantine arm: {arc_pids}")
    for unit in units:
        if prop(unit,"ActiveState") not in {"inactive","failed"} or prop(unit,"Job") not in {"","0"}:
            raise SystemExit(f"legacy activation source survived reboot: {unit}")
        if unit.endswith(".service") and prop(unit,"MainPID")!="0":
            raise SystemExit(f"legacy service has a PID after reboot: {unit}")
fixed={"capture_id":capture,"node":node,"freeze_plan_sha256":freeze,"sealed_boot_id":row["boot_id"],
       "network_quarantine_receipt_sha256":sha(base_raw),"network_quarantine_monitor_sha256":sha(monitor_raw),
       "selected_unit":selected,"selected_main_pid":supervisor_pid,"selected_start_ticks":row["supervisor_start_ticks"],
       "writer":{"pid":writer_pid,"start_ticks":row["writer_start_ticks"],"executable_path":row["executable_path"],
                 "executable_sha256":row["executable_sha256"],"argv_sha256":row["argv_sha256"],
                 "cgroup_sha256":row["writer_cgroup_sha256"]},
       "writer_supervision_mode":writer_mode,"allow_marker":marker_identity,
       "persistent_start_barrier_sha256":barrier_hashes,
       "quarantine_arm_start_barrier_sha256":arm_start_barrier_hashes,
       "prefreeze_runtime_safety_intent_sha256":sha(runtime_intent_raw),
       "selected_runtime_safety_sha256":runtime_safety_sha,
       "detached_supervisor_cgroup":supervisor_cgroup,
       "detached_supervisor_freeze_required":freeze_required,
       "alternatives_inactive_no_jobs":True,"monitor_active_and_exact":True,
       "writer_verified_live":True,"global_absence_claimed":False}
new_arm=existing_arm is None
if not new_arm:
    arm_raw=existing_arm_raw;arm=existing_arm
    for key,wanted in fixed.items():
        if arm.get(key)!=wanted:raise SystemExit(f"existing quarantine restart arm field differs: {key}")
else:
    if boot!=row["boot_id"]:raise SystemExit("cannot create first quarantine restart arm after reboot")
    arm={"schema":"arc.recovery.quarantine-live-restart-arm.v1",**fixed,"published_at":now()}
    arm_raw=canonical(arm)
arm_sha=sha(arm_raw);frozen_raw=None
if freeze_required and boot==row["boot_id"]:
    target=arm["detached_supervisor_cgroup"];base_cgroup=pathlib.Path("/sys/fs/cgroup")/target["path"].lstrip("/")
    details=base_cgroup.lstat()
    if details.st_dev!=target["device"] or details.st_ino!=target["inode"]:raise SystemExit("supervisor cgroup inode changed")
    directory=os.open(base_cgroup,os.O_RDONLY|getattr(os,"O_DIRECTORY",0)|getattr(os,"O_NOFOLLOW",0))
    try:
        freezer=os.open("cgroup.freeze",os.O_WRONLY|getattr(os,"O_NOFOLLOW",0),dir_fd=directory)
        try:os.write(freezer,b"1")
        finally:os.close(freezer)
    finally:os.close(directory)
    deadline=time.monotonic()+10
    while dict(line.split(" ",1) for line in (base_cgroup/"cgroup.events").read_text().splitlines()).get("frozen")!="1":
        if time.monotonic()>=deadline:raise SystemExit("detached supervisor did not freeze")
        time.sleep(.01)
    frozen={"schema":"arc.recovery.quarantine-detached-supervisor-frozen.v1",
            "quarantine_restart_arm_sha256":arm_sha,"cgroup":target,"selected_unit":selected,
            "selected_main_pid":supervisor_pid,"selected_start_ticks":row["supervisor_start_ticks"],
            "member_pids":[supervisor_pid],"observed_local_freeze":1,"observed_frozen":True}
if new_arm:
    if publish(arm_path,arm)!=arm_raw:raise SystemExit("published quarantine restart arm bytes differ")
if freeze_required and boot==row["boot_id"]:
    frozen_raw=publish(frozen_path,frozen)
elif frozen_path.exists() or frozen_path.is_symlink():
    if not freeze_required:raise SystemExit("unexpected supervisor freeze receipt")
    frozen_raw=secure(frozen_path);frozen=json.loads(frozen_raw)
    if (frozen_raw!=canonical(frozen)
            or frozen.get("schema")!="arc.recovery.quarantine-detached-supervisor-frozen.v1"
            or frozen.get("quarantine_restart_arm_sha256")!=arm_sha
            or frozen.get("cgroup")!=arm["detached_supervisor_cgroup"]
            or frozen.get("selected_unit")!=selected
            or frozen.get("selected_main_pid")!=supervisor_pid
            or frozen.get("selected_start_ticks")!=row["supervisor_start_ticks"]
            or frozen.get("member_pids")!=[supervisor_pid]
            or frozen.get("observed_local_freeze")!=1
            or frozen.get("observed_frozen") is not True):
        raise SystemExit("detached supervisor historical freeze receipt differs")
if commit_path.exists() or commit_path.is_symlink():
    commit_raw=secure(commit_path);commit=json.loads(commit_raw)
    if (commit_raw!=canonical(commit) or commit.get("schema")!="arc.recovery.quarantine-live-restart-committed.v1"
            or commit.get("quarantine_restart_arm_sha256")!=arm_sha
            or commit.get("detached_supervisor_frozen_sha256")!=(None if frozen_raw is None else sha(frozen_raw))
            or commit.get("allow_marker_absent") is not True or commit.get("restart_prevented") is not True):
        raise SystemExit("quarantine restart commit differs")
else:
    if marker.exists() or marker.is_symlink():
        parent=os.open(marker.parent,os.O_RDONLY|getattr(os,"O_DIRECTORY",0)|getattr(os,"O_NOFOLLOW",0))
        try:
            opened=os.open(marker.name,os.O_RDONLY|getattr(os,"O_NOFOLLOW",0),dir_fd=parent)
            try:
                details=os.fstat(opened);raw=os.read(opened,len(marker_payload)+1)
                observed={"path":str(marker),"sha256":sha(raw),"device":details.st_dev,"inode":details.st_ino,
                          "uid":details.st_uid,"gid":details.st_gid,"mode":stat.S_IMODE(details.st_mode),"size":details.st_size}
                if observed!=arm["allow_marker"]:raise SystemExit("allow marker identity changed before unlink")
            finally:os.close(opened)
            os.unlink(marker.name,dir_fd=parent);os.fsync(parent)
        finally:os.close(parent)
    else:fsync_dir(marker.parent)
    if marker.exists() or marker.is_symlink():raise SystemExit("allow marker remains after commit")
    same_boot=boot==row["boot_id"]
    commit={"schema":"arc.recovery.quarantine-live-restart-committed.v1",
            "quarantine_restart_arm_sha256":arm_sha,
            "detached_supervisor_frozen_sha256":None if frozen_raw is None else sha(frozen_raw),
            "sealed_boot_id":row["boot_id"],"observed_boot_id":boot,"allow_marker_path":str(marker),
            "allow_marker_absent":True,"allow_marker_parent_fsynced":True,
            "durability_basis":"same-boot-live-writer-unlink-parent-fsynced" if same_boot else "post-reboot-arm-reconciled-parent-fsynced",
            "writer_state":"exact-live" if same_boot else "absent-after-reboot",
            "detached_supervisor_frozen":freeze_required and same_boot,
            "restart_prevented":True,"automatic_unfence":False,"global_absence_claimed":False}
    publish(commit_path,commit)
if marker.exists() or marker.is_symlink():raise SystemExit("allow marker reappeared after commit")
PY
    quarantine_restart_status "$capture_id" "$node" "$freeze_sha"
}

quarantine_restart_status() {
    local capture_id="$1" node="$2" freeze_sha="$3" root
    require_hash "$capture_id" "capture id"; require_node "$node"; require_hash "$freeze_sha" "freeze plan hash"
    local partial="$STOP_BASE/$capture_id/.$node.stop.partial" final="$STOP_BASE/$capture_id/$node"
    if [ -d "$final" ] && [ ! -L "$final" ]; then root="$final"
    elif [ -d "$partial" ] && [ ! -L "$partial" ]; then root="$partial"
    else die "quarantine restart status journal is missing"
    fi
    verify_legacy_network_quarantine "$root" "$capture_id" "$node" "$freeze_sha"
    python3 - "$root" "$capture_id" "$node" "$freeze_sha" <<'PY'
import hashlib,json,os,pathlib,stat,subprocess,sys
root=pathlib.Path(sys.argv[1]);capture,node,freeze=sys.argv[2:]
canonical=lambda value:(json.dumps(value,sort_keys=True,separators=(",",":"))+"\n").encode()
def load(name,schema):
    path=root/name;details=path.lstat()
    if (path.is_symlink() or not stat.S_ISREG(details.st_mode) or details.st_uid!=0
            or details.st_gid!=0 or details.st_nlink!=1 or stat.S_IMODE(details.st_mode)!=0o400):
        raise SystemExit(f"unsafe quarantine restart receipt: {name}")
    raw=path.read_bytes();value=json.loads(raw)
    if raw!=canonical(value) or value.get("schema")!=schema:raise SystemExit(f"quarantine restart receipt differs: {name}")
    return value,raw
arm,arm_raw=load("09-quarantine-restart-arm.json","arc.recovery.quarantine-live-restart-arm.v1")
commit,commit_raw=load("09-quarantine-restart-committed.json","arc.recovery.quarantine-live-restart-committed.v1")
if ((arm["capture_id"],arm["node"],arm["freeze_plan_sha256"])!=(capture,node,freeze)
        or commit["quarantine_restart_arm_sha256"]!=hashlib.sha256(arm_raw).hexdigest()
        or commit["allow_marker_absent"] is not True or commit["allow_marker_parent_fsynced"] is not True
        or commit["restart_prevented"] is not True):
    raise SystemExit("quarantine restart arm/commit chain differs")
marker=pathlib.Path("/etc/arc-recovery/legacy-start-allowed")
if marker.exists() or marker.is_symlink():raise SystemExit("legacy start allow marker reappeared")
partial_arm_path=pathlib.Path(f"/root/arc-recovery-stops/{capture}/.{node}.stop.partial/09-quarantine-restart-arm.json")
final_arm_path=pathlib.Path(f"/root/arc-recovery-stops/{capture}/{node}/09-quarantine-restart-arm.json")
arm_barrier_payload=("[Unit]\n"
    f"ConditionPathExists=!{partial_arm_path}\n"
    f"ConditionPathExists=!{final_arm_path}\n").encode()
for unit,wanted in arm["persistent_start_barrier_sha256"].items():
    path=pathlib.Path(f"/etc/systemd/system/{unit}.d/zzzz-arc-recovery-freeze.conf")
    if path.is_symlink() or hashlib.sha256(path.read_bytes()).hexdigest()!=wanted:
        raise SystemExit(f"persistent quarantine restart barrier changed: {unit}")
for unit,wanted in arm["quarantine_arm_start_barrier_sha256"].items():
    path=pathlib.Path(f"/etc/systemd/system/{unit}.d/zzzx-arc-recovery-quarantine-arm.conf")
    details=path.lstat();raw=path.read_bytes()
    if (path.is_symlink() or not stat.S_ISREG(details.st_mode) or details.st_uid!=0
            or details.st_gid!=0 or details.st_nlink!=1 or stat.S_IMODE(details.st_mode)!=0o444
            or raw!=arm_barrier_payload or hashlib.sha256(raw).hexdigest()!=wanted):
        raise SystemExit(f"durable quarantine arm start barrier changed: {unit}")
    merged=subprocess.check_output(["/usr/bin/systemctl","cat",unit],text=True)
    section=None;conditions=[]
    for raw_line in merged.splitlines():
        line=raw_line.strip()
        if not line or line.startswith(("#",";")):continue
        if line.startswith("[") and line.endswith("]"):section=line[1:-1];continue
        if section=="Unit" and line.startswith("ConditionPathExists="):
            value=line.split("=",1)[1]
            if value=="":conditions.clear()
            else:conditions.append(value)
    for condition in (str(marker),f"!{partial_arm_path}",f"!{final_arm_path}"):
        if conditions.count(condition)!=1:
            raise SystemExit(f"merged quarantine restart condition changed: {unit}")
boot=pathlib.Path("/proc/sys/kernel/random/boot_id").read_text().strip()
same_boot=boot==arm["sealed_boot_id"];writer=arm["writer"];writer_state="absent-after-reboot"
if same_boot:
    proc=pathlib.Path(f"/proc/{writer['pid']}");raw=(proc/"stat").read_text();end=raw.rfind(")")
    fields=raw[end+2:].split()
    if (int(fields[19])!=writer["start_ticks"] or os.readlink(proc/"exe")!=writer["executable_path"]
            or hashlib.sha256((proc/"exe").read_bytes()).hexdigest()!=writer["executable_sha256"]
            or hashlib.sha256((proc/"cmdline").read_bytes()).hexdigest()!=writer["argv_sha256"]):
        raise SystemExit("exact writer changed after quarantine restart commit")
    writer_state="exact-live"
else:
    arc_pids=[path.parent.name for path in pathlib.Path("/proc").glob("[0-9]*/comm")
              if path.read_text(errors="replace").strip()=="arc-node"]
    if arc_pids:raise SystemExit(f"legacy writer restarted after reboot: {arc_pids}")
for unit in arm["persistent_start_barrier_sha256"]:
    active=subprocess.check_output(["/usr/bin/systemctl","show",unit,"--property=ActiveState","--value"],text=True).strip()
    job=subprocess.check_output(["/usr/bin/systemctl","show",unit,"--property=Job","--value"],text=True).strip()
    main_pid=(subprocess.check_output(["/usr/bin/systemctl","show",unit,"--property=MainPID","--value"],text=True).strip()
              if unit.endswith(".service") else "0")
    if unit!=arm["selected_unit"] or not same_boot:
        if active not in {"inactive","failed"} or job not in {"","0"} or main_pid!="0":
            raise SystemExit(f"legacy activation source became active: {unit}")
if same_boot:
    selected=arm["selected_unit"]
    required={"ActiveState":"active","MainPID":str(arm["selected_main_pid"]),"Restart":"no",
              "RefuseManualStart":"yes","RefuseManualStop":"yes","KillMode":"process",
              "SendSIGKILL":"no","OOMPolicy":"continue","WatchdogUSec":"0","RuntimeMaxUSec":"infinity"}
    for key,wanted in required.items():
        got=subprocess.check_output(["/usr/bin/systemctl","show",selected,f"--property={key}","--value"],text=True).strip()
        if got!=wanted:raise SystemExit(f"selected quarantine restart property changed: {key}")
detached_frozen=False
if arm["detached_supervisor_freeze_required"] and (root/"09-quarantine-restart-supervisor-frozen.json").exists():
    frozen,frozen_raw=load("09-quarantine-restart-supervisor-frozen.json","arc.recovery.quarantine-detached-supervisor-frozen.v1")
    if commit["detached_supervisor_frozen_sha256"]!=hashlib.sha256(frozen_raw).hexdigest():
        raise SystemExit("detached supervisor freeze chain differs")
    if same_boot:
        base=pathlib.Path("/sys/fs/cgroup")/frozen["cgroup"]["path"].lstrip("/")
        details=base.lstat();events=dict(line.split(" ",1) for line in (base/"cgroup.events").read_text().splitlines())
        if (details.st_dev!=frozen["cgroup"]["device"] or details.st_ino!=frozen["cgroup"]["inode"]
                or events.get("frozen")!="1"):
            raise SystemExit("detached self-heal supervisor thawed during proof window")
        detached_frozen=True
elif commit["detached_supervisor_frozen_sha256"] is not None:
    raise SystemExit("detached supervisor freeze receipt is missing")
status={"schema":"arc.recovery.quarantine-live-restart-status.v1",
        "capture_id":capture,"node":node,"freeze_plan_sha256":freeze,
        "quarantine_restart_arm_sha256":hashlib.sha256(arm_raw).hexdigest(),
        "quarantine_restart_commit_sha256":hashlib.sha256(commit_raw).hexdigest(),
        "observed_boot_id":boot,"same_boot":same_boot,"writer_state":writer_state,
        "selected_unit":arm["selected_unit"],"detached_supervisor_frozen":detached_frozen,
        "allow_marker_absent":True,"persistent_start_barrier_active":True,
        "network_quarantine_active":True,"monitor_active":True,"restart_prevented":True,
        "automatic_unfence":False,"global_absence_claimed":False}
print(json.dumps(status,sort_keys=True,separators=(",",":")))
PY
}

quarantine_monitor_receipt() {
    local capture_id="$1" node="$2" freeze_sha="$3" root
    require_hash "$capture_id" "capture id"; require_node "$node"; require_hash "$freeze_sha" "freeze plan hash"
    local partial="$STOP_BASE/$capture_id/.$node.stop.partial" final="$STOP_BASE/$capture_id/$node"
    if [ -d "$final" ] && [ ! -L "$final" ]; then root="$final"
    elif [ -d "$partial" ] && [ ! -L "$partial" ]; then root="$partial"
    else die "quarantine monitor receipt journal is missing"
    fi
    verify_legacy_network_quarantine "$root" "$capture_id" "$node" "$freeze_sha"
    python3 - "$root/08-network-quarantine-monitor.json" "$capture_id" "$node" "$freeze_sha" <<'PY'
import json,pathlib,stat,sys
path=pathlib.Path(sys.argv[1]);capture,node,freeze=sys.argv[2:];details=path.lstat()
if (path.is_symlink() or not stat.S_ISREG(details.st_mode) or details.st_uid!=0
        or details.st_gid!=0 or details.st_nlink!=1 or stat.S_IMODE(details.st_mode)!=0o400):
    raise SystemExit("quarantine monitor receipt is unsafe")
raw=path.read_bytes();value=json.loads(raw)
canonical=(json.dumps(value,sort_keys=True,separators=(",",":"))+"\n").encode()
if (raw!=canonical or value.get("schema")!="arc.recovery.legacy-network-quarantine-monitor.v1"
        or (value.get("capture_id"),value.get("node"),value.get("freeze_plan_sha256"))!=(capture,node,freeze)):
    raise SystemExit("quarantine monitor receipt identity differs")
sys.stdout.buffer.write(raw)
PY
}

quarantine_status() {
    local capture_id="$1" node="$2" freeze_sha="$3"
    require_hash "$capture_id" "capture id"; require_node "$node"; require_hash "$freeze_sha" "freeze plan hash"
    local partial="$STOP_BASE/$capture_id/.${node}.stop.partial"
    local final="$STOP_BASE/$capture_id/$node" root
    if [ -d "$final" ] && [ ! -L "$final" ]; then root="$final"
    elif [ -d "$partial" ] && [ ! -L "$partial" ]; then root="$partial"
    else die "network-quarantine stop journal is missing"
    fi
    verify_legacy_network_quarantine "$root" "$capture_id" "$node" "$freeze_sha"
    python3 - "$root" "$capture_id" "$node" "$freeze_sha" <<'PY'
import hashlib, json, pathlib, re, subprocess, sys
root=pathlib.Path(sys.argv[1]); capture,node,freeze=sys.argv[2:]
receipt_raw=(root/"08-network-quarantine.json").read_bytes()
receipt=json.loads(receipt_raw)
value=json.loads(subprocess.check_output(["/usr/sbin/nft","--json","list","table","inet","arc_legacy_maintenance_v1"]))
stateless_raw=subprocess.check_output(["/usr/sbin/nft","--stateless","list","table","inet","arc_legacy_maintenance_v1"])
stateless_sha=hashlib.sha256(stateless_raw).hexdigest()
if stateless_sha!=receipt["owned_ruleset_stateless_sha256"]:
    raise SystemExit("network-quarantine status snapshot rules differ from receipt")
canonical=lambda item:(json.dumps(item,sort_keys=True,separators=(",",":"))+"\n").encode()
rows=[entry for entry in value.get("nftables",[]) if "metainfo" not in entry]
if any(not isinstance(entry,dict) or len(entry)!=1 or next(iter(entry)) not in {"table","chain","rule"}
       for entry in rows):
    raise SystemExit("network-quarantine status snapshot contains an unknown AST object")
tables=[entry["table"] for entry in rows if "table" in entry]
if (len(tables)!=1 or set(tables[0])!={"family","name","handle","comment"}
        or (tables[0].get("family"),tables[0].get("name"),tables[0].get("comment")) !=
           ("inet","arc_legacy_maintenance_v1",f"arc-recovery:capture={capture}:node={node}")):
    raise SystemExit("network-quarantine status table AST differs")
chain_specs=[("prerouting","prerouting"),("input","input"),("forward","forward"),("output","output")]
chains=[entry["chain"] for entry in rows if "chain" in entry]
if len(chains)!=len(chain_specs): raise SystemExit("network-quarantine status chain count differs")
for chain,(name,hook) in zip(chains,chain_specs):
    if (set(chain)!={"family","table","name","handle","type","hook","prio","policy"}
            or any(chain.get(key)!=wanted for key,wanted in {"family":"inet","table":"arc_legacy_maintenance_v1",
                "name":name,"type":"filter","hook":hook,"prio":-310,"policy":"accept"}.items())):
        raise SystemExit(f"network-quarantine status chain AST differs: {name}")
expected=[]
for chain,_hook in chain_specs:
    if chain in {"prerouting","input"}:
        interface="iifname"
        expected += [(chain,interface,"loopback",[],"accept"),
                     (chain,interface,"ssh",[("tcp","dport",[22])],"accept"),
                     (chain,interface,"dhcp4",[("udp","sport",[67]),("udp","dport",[68])],"accept"),
                     (chain,interface,"dhcp6",[("udp","sport",[547]),("udp","dport",[546])],"accept"),
                     (chain,interface,"icmpv6-control",[("icmpv6","type",[2,133,134,135,136])],"accept"),
                     (chain,interface,"deny",[],"drop")]
    elif chain=="output":
        interface="oifname"
        expected += [(chain,interface,"loopback",[],"accept"),
                     (chain,interface,"ssh",[("tcp","sport",[22])],"accept"),
                     (chain,interface,"dhcp4",[("udp","sport",[68]),("udp","dport",[67])],"accept"),
                     (chain,interface,"dhcp6",[("udp","sport",[546]),("udp","dport",[547])],"accept"),
                     (chain,interface,"icmpv6-control",[("icmpv6","type",[2,133,134,135,136])],"accept"),
                     (chain,interface,"deny",[],"drop")]
    else: expected.append((chain,None,"deny-all",[],"drop"))
rules=[entry["rule"] for entry in rows if "rule" in entry]
if len(rules)!=len(expected): raise SystemExit("network-quarantine status rule count differs")
counters={}
semantic=[]
for rule,(chain,interface,slug,payloads,verdict) in zip(rules,expected):
    comment=f"arc-recovery:{chain}:{interface or 'all'}:{slug}"
    if (set(rule)!={"family","table","chain","handle","expr","comment"}
            or (rule.get("family"),rule.get("table"),rule.get("chain"),rule.get("comment")) !=
               ("inet","arc_legacy_maintenance_v1",chain,comment)):
        raise SystemExit(f"network-quarantine status rule AST differs: {comment}")
    expected_expr=[]
    if interface is not None:
        expected_expr.append(("interface",interface,"==" if slug=="loopback" else "!=","lo"))
    expected_expr.extend(("payload",protocol,field,values) for protocol,field,values in payloads)
    expected_expr.extend((("counter",),("verdict",verdict)))
    actual_expr=[]; counter_value=None
    for expr in rule.get("expr",[]):
        if not isinstance(expr,dict) or len(expr)!=1:
            raise SystemExit(f"network-quarantine status expr shape differs: {comment}")
        key=next(iter(expr))
        if key=="match":
            match=expr[key]
            if not isinstance(match,dict) or set(match)!={"op","left","right"}:
                raise SystemExit(f"network-quarantine status match shape differs: {comment}")
            left,right=match["left"],match["right"]
            if interface is not None and left=={"meta":{"key":interface}}:
                actual_expr.append(("interface",interface,match["op"],right))
            elif (isinstance(left,dict) and set(left)=={"payload"}
                    and isinstance(left["payload"],dict)
                    and set(left["payload"])=={"protocol","field"}):
                values=sorted(right["set"]) if isinstance(right,dict) and set(right)=={"set"} else [right]
                actual_expr.append(("payload",left["payload"]["protocol"],left["payload"]["field"],values))
            else: raise SystemExit(f"network-quarantine status match is unknown: {comment}")
        elif key=="counter":
            counter_value=expr[key]
            if (not isinstance(counter_value,dict) or set(counter_value)!={"packets","bytes"}
                    or any(isinstance(counter_value[field],bool) or not isinstance(counter_value[field],int)
                           or counter_value[field]<0 for field in ("packets","bytes"))):
                raise SystemExit(f"network-quarantine status counter differs: {comment}")
            actual_expr.append(("counter",))
        elif key in {"accept","drop"} and expr[key] is None: actual_expr.append(("verdict",key))
        else: raise SystemExit(f"network-quarantine status expr is unknown: {comment}")
    if actual_expr!=expected_expr or counter_value is None:
        raise SystemExit(f"network-quarantine status expr order/shape differs: {comment}")
    if any(
            isinstance(counter_value[key],bool) or not isinstance(counter_value[key],int) or counter_value[key]<0
            for key in ("packets","bytes")):
        raise SystemExit("owned network-quarantine counter is malformed")
    if comment in counters: raise SystemExit("owned network-quarantine counter comment is duplicated")
    counters[comment]=counter_value
    semantic.append({"chain":chain,"interface":interface,"slug":slug,"payload":payloads,
                     "verdict":verdict,"comment":comment})
if hashlib.sha256(canonical(semantic)).hexdigest()!=receipt.get("owned_rule_ast_sha256"):
    raise SystemExit("network-quarantine status semantic AST differs from receipt")
status={"schema":"arc.recovery.legacy-network-quarantine-status.v1",
        "capture_id":capture,"node":node,"freeze_plan_sha256":freeze,
        "receipt_sha256":hashlib.sha256(receipt_raw).hexdigest(),
        "table":{"family":"inet","name":"arc_legacy_maintenance_v1","priority":-310},
        "rule_counters":counters,"listener_inventory":receipt["listener_inventory"],
        "counter_snapshot_sha256":hashlib.sha256((json.dumps(counters,sort_keys=True,separators=(",",":"))+"\n").encode()).hexdigest(),
        "owned_ruleset_stateless_sha256":stateless_sha,
        "loopback_head":receipt["loopback_head"],
        "quarantine_policy":receipt["quarantine_policy"],
        "active":True,"enabled":True}
print(json.dumps(status,sort_keys=True,separators=(",",":")))
PY
}

quarantine_public_cross_proof() {
    local capture_id="$1" node="$2" freeze_sha="$3" public_info_after="$4"
    local public_latest_height="$5" public_latest_hash="$6" challenge="$7" status restart_status
    require_hash "$capture_id" "capture id"; require_node "$node"; require_hash "$freeze_sha" "freeze plan hash"
    require_uint "$public_info_after" "public info-after height"
    require_uint "$public_latest_height" "public latest height"
    require_hash "$public_latest_hash" "public latest block hash"
    require_hash "$challenge" "network-quarantine cross-proof challenge"
    restart_status="$(quarantine_restart_status "$capture_id" "$node" "$freeze_sha")"
    python3 -c 'import json,sys; value=json.loads(sys.argv[1]); assert value["schema"]=="arc.recovery.quarantine-live-restart-status.v1" and value["writer_state"]=="exact-live" and value["restart_prevented"] is True' "$restart_status"
    status="$(quarantine_status "$capture_id" "$node" "$freeze_sha")"
    local partial="$STOP_BASE/$capture_id/.${node}.stop.partial"
    local final="$STOP_BASE/$capture_id/$node" root
    if [ -d "$final" ] && [ ! -L "$final" ]; then root="$final"; else root="$partial"; fi
    local proof after_status
    proof="$(python3 - "$root" "$capture_id" "$node" "$freeze_sha" "$public_info_after" \
        "$public_latest_height" "$public_latest_hash" "$challenge" "$status" <<'PY'
import hashlib, http.client, json, pathlib, re, sys, urllib.parse
(root_raw,capture,node,freeze,info_raw,latest_raw,public_hash,challenge,status_raw)=sys.argv[1:]
root=pathlib.Path(root_raw); public_info_after=int(info_raw); public_latest_height=int(latest_raw)
canonical=lambda value:(json.dumps(value,sort_keys=True,separators=(",",":"))+"\n").encode()
hash_re=re.compile(r"[0-9a-f]{64}")
receipt_raw=(root/"08-network-quarantine.json").read_bytes(); receipt=json.loads(receipt_raw)
status=json.loads(status_raw)
status_keys={"schema","capture_id","node","freeze_plan_sha256","receipt_sha256","table",
             "rule_counters","counter_snapshot_sha256","owned_ruleset_stateless_sha256",
             "listener_inventory","loopback_head","quarantine_policy","active","enabled"}
if (not isinstance(status,dict) or set(status)!=status_keys
        or status.get("schema")!="arc.recovery.legacy-network-quarantine-status.v1"
        or status.get("capture_id")!=capture or status.get("node")!=node
        or status.get("freeze_plan_sha256")!=freeze
        or status.get("receipt_sha256")!=hashlib.sha256(receipt_raw).hexdigest()
        or status.get("active") is not True or status.get("enabled") is not True):
    raise SystemExit("network-quarantine cross-proof status object differs")
head=receipt.get("loopback_head",{})
if (head.get("latest_height",-1)<public_info_after
        or head.get("info_after_height")!=head.get("latest_height")
        or not hash_re.fullmatch(head.get("block_hash", ""))
        or not hash_re.fullmatch(head.get("state_root", ""))):
    raise SystemExit("stable fenced head does not cover public info-after height")
origin=urllib.parse.urlsplit(head.get("rpc_origin", ""))
if (origin.scheme,origin.hostname,origin.path,origin.query,origin.fragment)!=("http","127.0.0.1","","","") or origin.port is None:
    raise SystemExit("fenced head RPC origin is not exact loopback")
def block(height):
    connection=http.client.HTTPConnection("127.0.0.1",origin.port,timeout=5)
    try:
        path=f"/block/{height}"
        connection.request("GET",path,headers={"Host":f"127.0.0.1:{origin.port}",
            "Accept":"application/json","Connection":"close",
            "User-Agent":"arc-recovery-quarantine-cross-proof/1"})
        response=connection.getresponse(); body=response.read(8*1024*1024+1)
        if response.status!=200 or len(body)>8*1024*1024: raise SystemExit(f"fenced block query failed: {path}")
        value=json.loads(body); header=value.get("header",{})
        result={"height":header.get("height"),"block_hash":value.get("hash"),
                "state_root":header.get("state_root"),"response_sha256":hashlib.sha256(body).hexdigest()}
        if (result["height"]!=height or not hash_re.fullmatch(result.get("block_hash", ""))
                or not hash_re.fullmatch(result.get("state_root", ""))):
            raise SystemExit(f"fenced block tuple is malformed: {path}")
        return result
    finally: connection.close()
public_after_tuple=block(public_info_after)
public_latest_tuple=block(public_latest_height)
if public_latest_tuple["block_hash"]!=public_hash:
    raise SystemExit("fenced node disagrees with the authenticated public latest hash")
fenced_tuple={"height":head["latest_height"],"block_hash":head["block_hash"],
              "state_root":head["state_root"]}
proof={"schema":"arc.recovery.legacy-network-quarantine-public-cross-proof.v1",
       "capture_id":capture,"node":node,"freeze_plan_sha256":freeze,"challenge":challenge,
       "network_quarantine_receipt_sha256":hashlib.sha256(receipt_raw).hexdigest(),
       "quarantine_status_sha256":hashlib.sha256(canonical(status)).hexdigest(),
       "quarantine_status":status,
       "rule_counters":status["rule_counters"],
       "public_info_after_block":public_after_tuple,
       "public_latest_block":public_latest_tuple,"fenced_head":fenced_tuple,
       "fenced_head_covers_public_info_after":True,"public_latest_hash_matches":True,
       "global_absence_claimed":False}
print(json.dumps(proof,sort_keys=True,separators=(",",":")))
PY
)"
    # RPC work is deliberately bracketed by complete live fence proofs.  The
    # status embedded in the returned receipt is the post-query snapshot, so a
    # table/tool/unit/non-owned-firewall drift during the query cannot be hidden
    # behind the earlier status.
    after_status="$(quarantine_status "$capture_id" "$node" "$freeze_sha")"
    python3 - "$proof" "$after_status" <<'PY'
import hashlib,json,sys
canonical=lambda value:(json.dumps(value,sort_keys=True,separators=(",",":"))+"\n").encode()
proof=json.loads(sys.argv[1]); status=json.loads(sys.argv[2])
if (proof.get("network_quarantine_receipt_sha256")!=status.get("receipt_sha256")
        or status.get("capture_id")!=proof.get("capture_id")
        or status.get("node")!=proof.get("node")
        or status.get("freeze_plan_sha256")!=proof.get("freeze_plan_sha256")):
    raise SystemExit("post-query network-quarantine status identity differs")
proof["quarantine_status"]=status
proof["quarantine_status_sha256"]=hashlib.sha256(canonical(status)).hexdigest()
proof["rule_counters"]=status["rule_counters"]
print(json.dumps(proof,sort_keys=True,separators=(",",":")))
PY
}

quarantine_stability_sample() {
    local capture_id="$1" node="$2" freeze_sha="$3" challenge="$4" sample_index="$5"
    require_hash "$capture_id" "capture id"; require_node "$node"; require_hash "$freeze_sha" "freeze plan hash"
    require_hash "$challenge" "network-quarantine stability challenge"
    case "$sample_index" in 0|1) ;; *) die "network-quarantine stability sample index must be 0 or 1" ;; esac
    local restart_status
    restart_status="$(quarantine_restart_status "$capture_id" "$node" "$freeze_sha")"
    python3 -c 'import json,sys; value=json.loads(sys.argv[1]); assert value["schema"]=="arc.recovery.quarantine-live-restart-status.v1" and value["writer_state"]=="exact-live" and value["restart_prevented"] is True' "$restart_status"
    local partial="$STOP_BASE/$capture_id/.${node}.stop.partial"
    local final="$STOP_BASE/$capture_id/$node" root before after
    if [ -d "$final" ] && [ ! -L "$final" ]; then root="$final"
    elif [ -d "$partial" ] && [ ! -L "$partial" ]; then root="$partial"
    else die "network-quarantine stability journal is missing"
    fi
    before="$(quarantine_status "$capture_id" "$node" "$freeze_sha")"
    local sample
    sample="$(python3 - "$root" "$capture_id" "$node" "$freeze_sha" "$challenge" \
        "$sample_index" "$before" <<'PY'
import datetime,hashlib,http.client,json,os,pathlib,re,subprocess,sys,time,urllib.parse
(root_raw,capture,node,freeze,challenge,index_raw,status_raw)=sys.argv[1:]
root=pathlib.Path(root_raw);index=int(index_raw);status=json.loads(status_raw)
canonical=lambda value:(json.dumps(value,sort_keys=True,separators=(",",":"))+"\n").encode()
digest=lambda raw:hashlib.sha256(raw).hexdigest();hash_re=re.compile(r"[0-9a-f]{64}")
receipt_raw=(root/"08-network-quarantine.json").read_bytes();receipt=json.loads(receipt_raw)
if (status.get("schema")!="arc.recovery.legacy-network-quarantine-status.v1"
        or (status.get("capture_id"),status.get("node"),status.get("freeze_plan_sha256"))!=(capture,node,freeze)
        or status.get("receipt_sha256")!=digest(receipt_raw)
        or status.get("active") is not True or status.get("enabled") is not True):
    raise SystemExit("network-quarantine stability status differs")
writer=receipt.get("writer",{});pid=writer.get("pid")
if isinstance(pid,bool) or not isinstance(pid,int) or pid<=1:raise SystemExit("stability writer PID is malformed")
proc=pathlib.Path(f"/proc/{pid}")
stat_fields=(proc/"stat").read_text().split();start_ticks=int(stat_fields[21])
exe=os.readlink(proc/"exe");exe_raw=pathlib.Path(exe).read_bytes()
cmdline=(proc/"cmdline").read_bytes();cgroup=(proc/"cgroup").read_bytes()
if (start_ticks!=writer.get("start_ticks") or exe!=writer.get("executable_path")
        or digest(exe_raw)!=writer.get("executable_sha256")
        or digest(cmdline)!=writer.get("argv_sha256") or digest(cgroup)!=writer.get("cgroup_sha256")):
    raise SystemExit("network-quarantine stability writer identity differs")
ss_raw=subprocess.check_output(["/usr/sbin/ss","-H","-ltnup"])
ss_lines=ss_raw.decode("utf-8",errors="strict").splitlines()
def owned(protocol,port):
    rows=[]
    for line in ss_lines:
        fields=line.split()
        if not fields or fields[0] not in ({"tcp"} if protocol=="tcp" else {"udp","UNCONN"}):continue
        if not any(token.rsplit(":",1)[-1]==str(port) for token in fields if ":" in token):continue
        if re.search(rf"pid={pid}(?:,|\))",line) is None:continue
        rows.append(line)
    if len(rows)!=1:raise SystemExit(f"stability listener ownership differs: {protocol}/{port}")
    return digest((rows[0]+"\n").encode())
listeners={"rpc_tcp_9090_ss_sha256":owned("tcp",9090),
           "p2p_udp_9091_ss_sha256":owned("udp",9091),"writer_pid":pid}
origin=urllib.parse.urlsplit(receipt.get("loopback_head",{}).get("rpc_origin",""))
if (origin.scheme,origin.hostname,origin.path,origin.query,origin.fragment)!=("http","127.0.0.1","","","") or origin.port is None:
    raise SystemExit("stability RPC origin differs")
def rpc(path):
    connection=http.client.HTTPConnection("127.0.0.1",origin.port,timeout=5)
    try:
        connection.request("GET",path,headers={"Host":f"127.0.0.1:{origin.port}",
            "Accept":"application/json","Connection":"close",
            "User-Agent":"arc-recovery-quarantine-stability/1"})
        response=connection.getresponse();body=response.read(8*1024*1024+1)
        if response.status!=200 or len(body)>8*1024*1024:raise SystemExit(f"stability RPC failed: {path}")
        value=json.loads(body)
        if not isinstance(value,dict):raise SystemExit(f"stability RPC is not an object: {path}")
        return value,body
    finally:connection.close()
started=datetime.datetime.now(datetime.timezone.utc).strftime("%Y-%m-%dT%H:%M:%SZ")
head=None
for attempt in range(10):
    info_before,info_before_raw=rpc("/info");latest,latest_raw=rpc("/block/latest")
    height=latest.get("header",{}).get("height");block_hash=latest.get("hash")
    state_root=latest.get("header",{}).get("state_root")
    if (isinstance(height,bool) or not isinstance(height,int) or height<1
            or hash_re.fullmatch(str(block_hash)) is None or hash_re.fullmatch(str(state_root)) is None):
        raise SystemExit("stability latest tuple is malformed")
    exact,exact_raw=rpc(f"/block/{height}");info_after,info_after_raw=rpc("/info")
    if ([info_before.get("block_height"),exact.get("header",{}).get("height"),info_after.get("block_height")]
            ==[height,height,height] and exact.get("hash")==block_hash
            and exact.get("header",{}).get("state_root")==state_root):
        head={"height":height,"block_hash":block_hash,"state_root":state_root,
              "response_sha256":{"info_before":digest(info_before_raw),
                  "latest":digest(latest_raw),"exact":digest(exact_raw),"info_after":digest(info_after_raw)},
              "stable_attempt":attempt+1}
        break
    time.sleep(0.2)
if head is None:raise SystemExit("network-quarantine stability head did not stabilize")
counter=status.get("rule_counters",{}).get("arc-recovery:output:oifname:deny",{}).get("packets")
if isinstance(counter,bool) or not isinstance(counter,int) or counter<0:
    raise SystemExit("network-quarantine stability output deny counter differs")
completed=datetime.datetime.now(datetime.timezone.utc).strftime("%Y-%m-%dT%H:%M:%SZ")
value={"schema":"arc.recovery.legacy-network-quarantine-stability-sample.v1",
       "capture_id":capture,"node":node,"freeze_plan_sha256":freeze,"challenge":challenge,
       "sample_index":index,"started_at":started,"completed_at":completed,
       "quarantine_status_before":status,"quarantine_status_before_sha256":digest(canonical(status)),
       "writer":{"pid":pid,"start_ticks":start_ticks,"executable_sha256":digest(exe_raw),
                 "argv_sha256":digest(cmdline),"cgroup_sha256":digest(cgroup)},
       "listener_ownership":listeners,"head":head,"output_deny_packets":counter,
       "ss_sha256":digest(ss_raw),"global_absence_claimed":False}
print(json.dumps(value,sort_keys=True,separators=(",",":")))
PY
)"
    after="$(quarantine_status "$capture_id" "$node" "$freeze_sha")"
    python3 - "$sample" "$after" <<'PY'
import hashlib,json,sys
canonical=lambda value:(json.dumps(value,sort_keys=True,separators=(",",":"))+"\n").encode()
value=json.loads(sys.argv[1]);after=json.loads(sys.argv[2])
before=value["quarantine_status_before"]
if (after.get("receipt_sha256")!=before.get("receipt_sha256")
        or after.get("owned_ruleset_stateless_sha256")!=before.get("owned_ruleset_stateless_sha256")
        or after.get("capture_id")!=value.get("capture_id") or after.get("node")!=value.get("node")
        or after.get("freeze_plan_sha256")!=value.get("freeze_plan_sha256")
        or after.get("active") is not True or after.get("enabled") is not True):
    raise SystemExit("network-quarantine stability post-RPC status differs")
value["quarantine_status_after"]=after
value["quarantine_status_after_sha256"]=hashlib.sha256(canonical(after)).hexdigest()
print(json.dumps(value,sort_keys=True,separators=(",",":")))
PY
}

pre_fence_quiesce() {
    local root="$1" marker
    for marker in stop.intent.json 06-pre-mask-activation-gate.json \
        06-restart-barrier-armed.json 07-restart-barrier-committed.json \
        10-fence-verified.json 40-stable-inactive.json \
        50-cgroups-thaw-intent.json 50-cgroups-thawed.json; do
        if [ -e "$root/$marker" ] || [ -L "$root/$marker" ]; then
            [ -f "$root/$marker" ] && [ ! -L "$root/$marker" ] || \
                die "pre-fence resume marker is unsafe: $marker"
            return 0
        fi
    done
    stage_prefreeze_runtime_safety "$root" "$2" "$3" "$4" "$5" \
        "$7" "$8" "${13}"
    install_legacy_network_quarantine "$@"
    # The quarantine-only phase intentionally leaves the exact writer live.
    # Fleet orchestration performs external counter challenges and the public
    # height cross-proof over authenticated loopback before a later fence-stop
    # invocation enters the irreversible cgroup-freeze/TERM transaction.
    if [ "${ARC_RECOVERY_QUARANTINE_ONLY:-false}" = true ]; then
        return 0
    fi
    fast_cgroup_freeze "$@"
    if [ -e "$root/05-cgroups-frozen.json" ] || [ -L "$root/05-cgroups-frozen.json" ]; then
        [ -f "$root/05-cgroups-frozen.json" ] && [ ! -L "$root/05-cgroups-frozen.json" ] || \
            die "detailed frozen-cgroup marker is unsafe"
        return 0
    fi
    pre_fence_quiesce_phase intent "$@"
    pre_fence_quiesce_phase freeze "$@"
}

verify_exact_writer() {
    local evidence_root="$1" freeze_sha="$2" validator="$3" stake="$4" writer_pid="$5"
    local start_ticks="$6" boot_id="$7" writer_cgroup_sha="$8" writer_supervision_mode="$9"
    local unit="${10}" unit_main_pid="${11}" supervisor_start_ticks="${12}"
    local supervisor_executable_path="${13}" supervisor_executable_sha="${14}"
    local supervisor_argv_sha="${15}" supervisor_context_sha="${16}"
    local executable_path="${17}" executable_sha="${18}" argv_sha="${19}" data_dir="${20}"
    python3 - "$evidence_root/writer-contract.json" "$freeze_sha" "$validator" "$stake" \
        "$writer_pid" "$start_ticks" "$boot_id" "$writer_cgroup_sha" \
        "$writer_supervision_mode" "$unit" "$unit_main_pid" \
        "$supervisor_start_ticks" "$supervisor_executable_path" \
        "$supervisor_executable_sha" "$supervisor_argv_sha" "$supervisor_context_sha" \
        "$executable_path" "$executable_sha" "$argv_sha" "$data_dir" <<'PY'
import hashlib
import json
import os
import pathlib
import re
import signal
import shutil
import subprocess
import sys
import urllib.request

(output_raw, freeze_sha, validator, stake_raw, pid_raw, start_raw, boot_id,
 writer_cgroup_sha, writer_supervision_mode, unit, main_raw,
 supervisor_start_raw, supervisor_executable_path,
 supervisor_executable_sha, supervisor_argv_sha, supervisor_context_sha, executable_path,
 executable_sha, argv_sha, data_dir_raw) = sys.argv[1:]
output = pathlib.Path(output_raw)
pid, stake, start_ticks, unit_main_pid, supervisor_start_ticks = map(
    int, (pid_raw, stake_raw, start_raw, main_raw, supervisor_start_raw)
)
if unit not in {"arc-node.service", "arc-self-heal.service"}:
    raise SystemExit("sealed writer supervisor unit is not reviewed")
if not re.fullmatch(r"[0-9a-f]{64}", validator):
    raise SystemExit("sealed writer validator address is malformed")
if not all(re.fullmatch(r"[0-9a-f]{64}", value) for value in (
    freeze_sha, executable_sha, argv_sha, writer_cgroup_sha, supervisor_executable_sha,
    supervisor_argv_sha, supervisor_context_sha,
)):
    raise SystemExit("sealed writer hash is malformed")
if pathlib.Path("/proc/sys/kernel/random/boot_id").read_text().strip() != boot_id:
    raise SystemExit("host rebooted after writer audit")
proc = pathlib.Path("/proc") / str(pid)
if not proc.is_dir():
    raise SystemExit("exact audited writer PID no longer exists")
pids = []
for entry in pathlib.Path("/proc").iterdir():
    if not entry.name.isdigit():
        continue
    try:
        if (entry / "comm").read_text().strip() == "arc-node":
            pids.append(int(entry.name))
    except (FileNotFoundError, PermissionError, ProcessLookupError):
        pass
if pids != [pid]:
    raise SystemExit(f"arc-node process set differs from exact audit: {pids}")
stat_fields = (proc / "stat").read_text().split()
if len(stat_fields) < 22 or int(stat_fields[21]) != start_ticks:
    raise SystemExit("writer PID was reused or restarted after audit")
argv_raw = (proc / "cmdline").read_bytes()
if hashlib.sha256(argv_raw).hexdigest() != argv_sha:
    raise SystemExit("writer argv differs from sealed audit")
if os.readlink(proc / "exe") != executable_path:
    raise SystemExit("writer executable path differs from sealed audit")
digest = hashlib.sha256()
with (proc / "exe").open("rb") as handle:
    for chunk in iter(lambda: handle.read(1024 * 1024), b""):
        digest.update(chunk)
if digest.hexdigest() != executable_sha:
    raise SystemExit("writer executable bytes differ from sealed audit")
cgroup = (proc / "cgroup").read_text()
if writer_supervision_mode == "systemd-unit":
    if (hashlib.sha256(cgroup.encode("utf-8")).hexdigest() != writer_cgroup_sha
            or unit not in cgroup):
        raise SystemExit("writer left the sealed supervisor unit")
elif writer_supervision_mode == "detached-root-session":
    fast_root = output.parent.parent
    fast_path = fast_root / "02-fast-cgroup-freeze-intent.json"
    frozen_path = fast_root / "03-fast-cgroups-frozen.json"
    if any(path.is_symlink() or not path.is_file() for path in (fast_path, frozen_path)):
        raise SystemExit("detached writer leaf provenance is missing")
    fast_raw = fast_path.read_bytes(); fast = json.loads(fast_raw)
    frozen_raw = frozen_path.read_bytes(); frozen = json.loads(frozen_raw)
    canonical = lambda value: (json.dumps(value, sort_keys=True, separators=(",", ":")) + "\n").encode()
    fast_writer = fast.get("writer", {})
    parent_identity = fast_writer.get("parent_scope_cgroup")
    leaf_identity = frozen.get("writer_recovery_leaf")
    unified = [line.split(":", 2)[2] for line in cgroup.splitlines() if line.startswith("0::")]
    if (fast_raw != canonical(fast) or frozen_raw != canonical(frozen)
            or fast.get("schema") != "arc.recovery.fast-cgroup-freeze-intent.v1"
            or frozen.get("schema") != "arc.recovery.fast-cgroups-frozen.v1"
            or fast_writer.get("cgroup_sha256") != writer_cgroup_sha
            or not isinstance(parent_identity, dict)
            or not isinstance(leaf_identity, dict)
            or frozen.get("writer_parent_scope_cgroup") != parent_identity
            or fast_writer.get("recovery_leaf_path") != leaf_identity.get("path", "")
            or len(unified) != 1 or unified[0] != leaf_identity.get("path")
            or unit in cgroup or int(stat_fields[3]) != 1):
        raise SystemExit("detached writer supervision relationship changed")
    leaf = pathlib.Path("/sys/fs/cgroup") / leaf_identity["path"].lstrip("/")
    details = leaf.lstat(); events = dict(
        line.split(" ", 1) for line in leaf.joinpath("cgroup.events").read_text().splitlines()
    )
    if (leaf.is_symlink() or details.st_dev != leaf_identity.get("device")
            or details.st_ino != leaf_identity.get("inode")
            or leaf.joinpath("cgroup.freeze").read_text(encoding="ascii").strip() != "1"
            or events.get("frozen") != "1" or events.get("populated") != "1"
            or set(int(value) for value in leaf.joinpath("cgroup.procs").read_text().splitlines()) != {pid}):
        raise SystemExit("detached writer recovery leaf lost local freeze/identity")
else:
    raise SystemExit("writer supervision mode is unsupported")
actual_main = int(subprocess.check_output(
    ["systemctl", "show", unit, "--property=MainPID", "--value"], text=True
).strip())
if actual_main != unit_main_pid:
    raise SystemExit("supervisor MainPID differs from sealed audit")
supervisor_proc = pathlib.Path("/proc") / str(unit_main_pid)
if not supervisor_proc.is_dir():
    raise SystemExit("exact audited supervisor PID no longer exists")
supervisor_stat = supervisor_proc.joinpath("stat").read_text().split()
if len(supervisor_stat) < 22 or int(supervisor_stat[21]) != supervisor_start_ticks:
    raise SystemExit("supervisor PID was reused or restarted after audit")
if os.readlink(supervisor_proc / "exe") != supervisor_executable_path:
    raise SystemExit("supervisor executable path differs from sealed audit")
supervisor_digest = hashlib.sha256()
with supervisor_proc.joinpath("exe").open("rb") as handle:
    for chunk in iter(lambda: handle.read(1024 * 1024), b""):
        supervisor_digest.update(chunk)
if supervisor_digest.hexdigest() != supervisor_executable_sha:
    raise SystemExit("supervisor executable bytes differ from sealed audit")
if hashlib.sha256(supervisor_proc.joinpath("cmdline").read_bytes()).hexdigest() != supervisor_argv_sha:
    raise SystemExit("supervisor argv differs from sealed audit")
if unit not in supervisor_proc.joinpath("cgroup").read_text():
    raise SystemExit("supervisor MainPID is outside the sealed systemd unit")

def signal_ignored(process, signal_number):
    for line in process.joinpath("status").read_text(encoding="ascii").splitlines():
        if line.startswith("SigIgn:"):
            return bool(int(line.split(":", 1)[1].strip(), 16) & (1 << (signal_number - 1)))
    raise SystemExit("process status has no SigIgn mask")

if signal_ignored(proc, signal.SIGTERM) or signal_ignored(supervisor_proc, signal.SIGTERM):
    raise SystemExit("writer or supervisor now ignores SIGTERM")
if unit_main_pid == pid and (
    supervisor_start_ticks != start_ticks
    or supervisor_executable_path != executable_path
    or supervisor_executable_sha != executable_sha
    or supervisor_argv_sha != argv_sha
):
    raise SystemExit("direct supervisor identity conflicts with the sealed writer")
try:
    supervisor_argv = [item.decode("utf-8") for item in supervisor_proc.joinpath("cmdline").read_bytes().rstrip(b"\0").split(b"\0")]
except UnicodeDecodeError:
    raise SystemExit("supervisor argv is not UTF-8")
payloads = []
if pathlib.Path(supervisor_executable_path).name in {"bash", "sh", "dash"}:
    if len(supervisor_argv) < 2:
        raise SystemExit("interpreted supervisor has no script payload")
    payload_path = pathlib.Path(os.path.realpath(supervisor_argv[1]))
    if not payload_path.is_absolute() or not payload_path.is_file() or payload_path.is_symlink():
        raise SystemExit("interpreted supervisor payload is unsafe")
    payload_text = payload_path.read_text(encoding="utf-8")
    if re.search(r"(?:^|[;\s])trap(?:\s|$)", payload_text):
        raise SystemExit("interpreted supervisor has a signal/exit trap; TERM quiescence is unreviewed")
    payloads.append({"path": str(payload_path), "sha256": hashlib.sha256(payload_path.read_bytes()).hexdigest()})
fast_intent_path = output.parent.parent / "02-fast-cgroup-freeze-intent.json"
if fast_intent_path.is_symlink() or not fast_intent_path.is_file():
    raise SystemExit("pre-recovery unit projection is missing")
fast_intent_raw = fast_intent_path.read_bytes()
fast_intent = json.loads(fast_intent_raw)
if (fast_intent.get("schema") != "arc.recovery.fast-cgroup-freeze-intent.v1"
        or fast_intent_raw != (
            json.dumps(fast_intent, sort_keys=True, separators=(",", ":")) + "\n"
        ).encode()
        or fast_intent.get("supervisor_unit") != unit
        or fast_intent.get("supervisor", {}).get("context_sha256") != supervisor_context_sha
        or not re.fullmatch(
            r"[0-9a-f]{64}",
            fast_intent.get("pre_recovery_unit_configuration_sha256", ""),
        )):
    raise SystemExit("pre-recovery unit projection differs from the sealed supervisor")
unit_configuration_sha = fast_intent["pre_recovery_unit_configuration_sha256"]
safety_path = output.parent.parent / "01-prefreeze-runtime-safety-intent.json"
if safety_path.is_symlink() or not safety_path.is_file():
    raise SystemExit("prefreeze runtime safety contract is missing")
safety_raw = safety_path.read_bytes(); safety = json.loads(safety_raw)
unit_hooks = {
    hook: subprocess.check_output(["systemctl", "show", unit, f"--property={hook}", "--value"], text=True).strip()
    for hook in ("ExecReload", "ExecStop", "ExecStopPost", "OnFailure", "OnSuccess", "SuccessAction", "FailureAction", "JobTimeoutAction")
}
if any(value not in {"", "none"} for value in unit_hooks.values()):
    raise SystemExit("supervisor gained an unreviewed lifecycle hook")
automatic_lifecycle = {
    prop: subprocess.check_output(
        ["systemctl", "show", unit, f"--property={prop}", "--value"], text=True,
    ).strip()
    for prop in (
        "WatchdogUSec", "RuntimeMaxUSec", "RuntimeRandomizedExtraUSec",
        "StopWhenUnneeded", "BindsTo", "PartOf", "PropagatesStopTo", "OOMPolicy",
        "Requires", "Requisite", "Conflicts", "Upholds", "UpheldBy",
        "TriggeredBy", "RequiredBy", "WantedBy", "BoundBy", "ConflictedBy",
        "OnFailureOf", "OnSuccessOf",
        "CanReload", "StopPropagatedFrom", "ReloadPropagatedFrom",
    )
}
if (
    automatic_lifecycle["WatchdogUSec"] != "0"
    or automatic_lifecycle["RuntimeMaxUSec"] != "infinity"
    or automatic_lifecycle["RuntimeRandomizedExtraUSec"] != "0"
    or automatic_lifecycle["StopWhenUnneeded"] != "no"
    or automatic_lifecycle["BindsTo"]
    or automatic_lifecycle["PartOf"]
    or automatic_lifecycle["PropagatesStopTo"]
    or set(automatic_lifecycle["Requires"].split()) != {"-.mount", "system.slice", "sysinit.target"}
    or automatic_lifecycle["Requisite"]
    or set(automatic_lifecycle["Conflicts"].split()) != {"shutdown.target"}
    or any(automatic_lifecycle[prop] for prop in (
        "Upholds", "UpheldBy", "TriggeredBy", "RequiredBy", "BoundBy", "ConflictedBy",
        "OnFailureOf", "OnSuccessOf", "StopPropagatedFrom", "ReloadPropagatedFrom",
    ))
    or automatic_lifecycle["CanReload"] != "no"
    or automatic_lifecycle["OOMPolicy"] != "continue"
):
    raise SystemExit("supervisor gained an automatic stop/kill source")
if (safety.get("schema") != "arc.recovery.prefreeze-runtime-safety-intent.v1"
        or safety_raw != (json.dumps(safety, sort_keys=True, separators=(",", ":")) + "\n").encode()
        or hashlib.sha256(safety_raw).hexdigest() != fast_intent.get("prefreeze_runtime_safety_intent_sha256")
        or safety.get("pre_recovery_unit_configuration_sha256") != unit_configuration_sha
        or safety.get("pre_recovery_oom_policy") not in {"stop", "continue"}):
    raise SystemExit("prefreeze runtime safety contract differs from the frozen supervisor")
# Reconstruct the content-sealed pre-recovery context while independently
# proving that the live frozen unit has the safer runtime overlay applied.
sealed_automatic_lifecycle = dict(automatic_lifecycle)
sealed_automatic_lifecycle["OOMPolicy"] = safety["pre_recovery_oom_policy"]
invocation_id = subprocess.check_output(
    ["systemctl", "show", unit, "--property=InvocationID", "--value"], text=True
).strip()
control_group = subprocess.check_output(
    ["systemctl", "show", unit, "--property=ControlGroup", "--value"], text=True
).strip()
sleep_identity = None
if unit == "arc-self-heal.service":
    sleep_candidate = shutil.which("sleep")
    if not sleep_candidate:
        raise SystemExit("self-heal supervisor has no reviewed sleep executable")
    sleep_path = pathlib.Path(os.path.realpath(sleep_candidate))
    sleep_identity = {"path": str(sleep_path), "sha256": hashlib.sha256(sleep_path.read_bytes()).hexdigest(), "argv_policy": "sleep-duration-max-60s-v1", "max_seconds": 60}
cgroup_procs = pathlib.Path("/sys/fs/cgroup") / control_group.lstrip("/") / "cgroup.procs"
if not cgroup_procs.is_file():
    raise SystemExit("supervisor cgroup process inventory is unavailable")
for member_raw in cgroup_procs.read_text(encoding="ascii").splitlines():
    member = int(member_raw)
    if member in {unit_main_pid, pid}:
        continue
    member_proc = pathlib.Path("/proc") / str(member)
    try:
        member_exe = os.readlink(member_proc / "exe")
        member_argv = [item.decode("utf-8") for item in member_proc.joinpath("cmdline").read_bytes().rstrip(b"\0").split(b"\0")]
    except (FileNotFoundError, ProcessLookupError, UnicodeDecodeError):
        raise SystemExit("supervisor cgroup membership changed during freeze verification")
    duration_match = re.fullmatch(r"([0-9]+(?:\.[0-9]+)?)([smhd]?)", member_argv[1]) if len(member_argv) == 2 else None
    duration_seconds = None if duration_match is None else float(duration_match.group(1)) * {"": 1, "s": 1, "m": 60, "h": 3600, "d": 86400}[duration_match.group(2)]
    if (not sleep_identity or member_exe != sleep_identity["path"]
            or duration_seconds is None or duration_seconds > sleep_identity["max_seconds"]):
        raise SystemExit("unreviewed process exists in supervisor cgroup")
supervisor_context = {
    "schema": "arc.recovery.supervisor-context.v1",
    "unit": unit,
    "unit_configuration_sha256": unit_configuration_sha,
    "lifecycle_hooks": unit_hooks,
    "automatic_lifecycle": sealed_automatic_lifecycle,
    "invocation_id": invocation_id,
    "control_group": control_group,
    "interpreter_payloads": payloads,
    "allowed_transient_sleep": sleep_identity,
    "term_traps_rejected": True,
}
actual_supervisor_context_sha = hashlib.sha256(
    (json.dumps(supervisor_context, sort_keys=True, separators=(",", ":")) + "\n").encode()
).hexdigest()
if actual_supervisor_context_sha != supervisor_context_sha:
    raise SystemExit("supervisor script/unit/invocation context differs from sealed audit")
argv = [item.decode("utf-8") for item in argv_raw.rstrip(b"\0").split(b"\0")]
data_raw = None
for index, item in enumerate(argv):
    if item == "--data-dir":
        data_raw = argv[index + 1]
    elif item.startswith("--data-dir="):
        data_raw = item.split("=", 1)[1]
if data_raw is None:
    data_raw = "arc-data"
candidate = pathlib.Path(data_raw)
if not candidate.is_absolute():
    candidate = pathlib.Path(os.readlink(proc / "cwd")) / candidate
data_dir = pathlib.Path(os.path.realpath(candidate))
if str(data_dir) != data_dir_raw or not data_dir.is_dir() or data_dir.is_symlink():
    raise SystemExit("writer real data directory differs from sealed audit")
# The exact PID/start/executable/argv tuple is frozen before this verifier.
# Validator address and stake are therefore inherited from the content-sealed
# live audit; querying RPC here would require thawing the writer and reopen the
# historical self-heal race.
value = {
    "schema": "arc.recovery.exact-writer.v3",
    "freeze_plan_sha256": freeze_sha,
    "validator_address": validator,
    "stake": stake,
    "writer_pid": pid,
    "writer_start_ticks": start_ticks,
    "writer_cgroup_sha256": writer_cgroup_sha,
    "writer_supervision_mode": writer_supervision_mode,
    "boot_id": boot_id,
    "supervisor_unit": unit,
    "supervisor_main_pid": unit_main_pid,
    "supervisor_start_ticks": supervisor_start_ticks,
    "supervisor_executable_path": supervisor_executable_path,
    "supervisor_executable_sha256": supervisor_executable_sha,
    "supervisor_argv_sha256": supervisor_argv_sha,
    "supervisor_context": supervisor_context,
    "supervisor_context_sha256": supervisor_context_sha,
    "executable_path": executable_path,
    "executable_sha256": executable_sha,
    "argv_sha256": argv_sha,
    "data_dir": str(data_dir),
}
payload = (json.dumps(value, sort_keys=True, separators=(",", ":")) + "\n").encode()
temporary = output.with_name(f".{output.name}.partial")
if temporary.exists() or temporary.is_symlink():
    if temporary.is_symlink() or not temporary.is_file():
        raise SystemExit("unsafe writer-contract partial")
    temporary.unlink()
descriptor = os.open(temporary, os.O_WRONLY | os.O_CREAT | os.O_EXCL | getattr(os, "O_NOFOLLOW", 0), 0o400)
with os.fdopen(descriptor, "wb") as handle:
    handle.write(payload); handle.flush(); os.fsync(handle.fileno())
os.rename(temporary, output)
directory = os.open(output.parent, os.O_RDONLY | getattr(os, "O_DIRECTORY", 0))
try: os.fsync(directory)
finally: os.close(directory)
PY
}

verify_or_arm_stop_journal() {
    local root="$1" capture_id="$2" node="$3" freeze_sha="$4" validator="$5" stake="$6"
    local writer_pid="$7" start_ticks="$8" boot_id="$9" writer_cgroup_sha="${10}"
    local writer_supervision_mode="${11}" unit="${12}" unit_main_pid="${13}"
    local supervisor_start_ticks="${14}" supervisor_executable_path="${15}"
    local supervisor_executable_sha="${16}" supervisor_argv_sha="${17}"
    local supervisor_context_sha="${18}" executable_path="${19}"
    local executable_sha="${20}" argv_sha="${21}" data_dir="${22}"
    python3 - "$root" "$capture_id" "$node" "$freeze_sha" "$validator" "$stake" \
        "$writer_pid" "$start_ticks" "$boot_id" "$writer_cgroup_sha" \
        "$writer_supervision_mode" "$unit" "$unit_main_pid" "$supervisor_start_ticks" \
        "$supervisor_executable_path" "$supervisor_executable_sha" "$supervisor_argv_sha" "$supervisor_context_sha" \
        "$executable_path" "$executable_sha" "$argv_sha" "$data_dir" <<'PY'
import datetime
import hashlib
import json
import os
import pathlib
import stat
import sys

(root_raw, capture_id, node, freeze_sha, validator, stake_raw, pid_raw,
 start_raw, boot_id, writer_cgroup_sha, writer_supervision_mode, unit,
 main_raw, supervisor_start_raw, supervisor_executable_path,
 supervisor_executable_sha, supervisor_argv_sha, supervisor_context_sha, executable_path,
 executable_sha, argv_sha, data_dir) = sys.argv[1:]
root = pathlib.Path(root_raw)
contract_path = root / "evidence" / "writer-contract.json"
intent_path = root / "stop.intent.json"
armed_path = root / "stop.armed"
expected_contract = {
    "schema": "arc.recovery.exact-writer.v3",
    "freeze_plan_sha256": freeze_sha,
    "validator_address": validator,
    "stake": int(stake_raw),
    "writer_pid": int(pid_raw),
    "writer_start_ticks": int(start_raw),
    "writer_cgroup_sha256": writer_cgroup_sha,
    "writer_supervision_mode": writer_supervision_mode,
    "boot_id": boot_id,
    "supervisor_unit": unit,
    "supervisor_main_pid": int(main_raw),
    "supervisor_start_ticks": int(supervisor_start_raw),
    "supervisor_executable_path": supervisor_executable_path,
    "supervisor_executable_sha256": supervisor_executable_sha,
    "supervisor_argv_sha256": supervisor_argv_sha,
    "supervisor_context_sha256": supervisor_context_sha,
    "executable_path": executable_path,
    "executable_sha256": executable_sha,
    "argv_sha256": argv_sha,
    "data_dir": data_dir,
}
observed_contract = json.loads(contract_path.read_text(encoding="utf-8"))
supervisor_context = observed_contract.get("supervisor_context")
if not isinstance(supervisor_context, dict) or supervisor_context.get("schema") != "arc.recovery.supervisor-context.v1":
    raise SystemExit("durable stop journal supervisor context is malformed")
supervisor_context_payload = (json.dumps(supervisor_context, sort_keys=True, separators=(",", ":")) + "\n").encode()
if hashlib.sha256(supervisor_context_payload).hexdigest() != supervisor_context_sha:
    raise SystemExit("durable stop journal supervisor context hash differs")
expected_contract["supervisor_context"] = supervisor_context
canonical_contract = (json.dumps(expected_contract, sort_keys=True, separators=(",", ":")) + "\n").encode()
if contract_path.is_symlink() or contract_path.read_bytes() != canonical_contract:
    raise SystemExit("durable stop journal writer contract differs from sealed freeze plan")
contract_sha = hashlib.sha256(canonical_contract).hexdigest()
prefence_intent_path = root / "04-pre-fence-quiesce-intent.json"
frozen_context_path = root / "05-cgroups-frozen.json"
for path, schema in (
    (prefence_intent_path, "arc.recovery.pre-fence-quiesce-intent.v1"),
    (frozen_context_path, "arc.recovery.cgroups-frozen.v1"),
):
    if path.is_symlink() or not path.is_file():
        raise SystemExit("pre-fence cgroup proof is missing or unsafe")
    raw = path.read_bytes()
    value = json.loads(raw)
    if (value.get("schema") != schema
            or value.get("capture_id") != capture_id
            or value.get("node") != node
            or value.get("freeze_plan_sha256") != freeze_sha
            or raw != (json.dumps(value, sort_keys=True, separators=(",", ":")) + "\n").encode()):
        raise SystemExit("pre-fence cgroup proof differs from the sealed stop")
prefence_intent_sha = hashlib.sha256(prefence_intent_path.read_bytes()).hexdigest()
frozen_context_sha = hashlib.sha256(frozen_context_path.read_bytes()).hexdigest()
network_quarantine_path = root / "08-network-quarantine.json"
if network_quarantine_path.is_symlink() or not network_quarantine_path.is_file():
    raise SystemExit("network-quarantine receipt is missing before stop intent")
network_quarantine_raw = network_quarantine_path.read_bytes()
network_quarantine = json.loads(network_quarantine_raw)
if (network_quarantine_raw != (json.dumps(network_quarantine, sort_keys=True, separators=(",", ":")) + "\n").encode()
        or network_quarantine.get("schema") != "arc.recovery.legacy-network-quarantine.v1"
        or (network_quarantine.get("capture_id"), network_quarantine.get("node"),
            network_quarantine.get("freeze_plan_sha256")) != (capture_id, node, freeze_sha)
        or network_quarantine.get("writer", {}).get("pid") != int(pid_raw)
        or network_quarantine.get("writer", {}).get("start_ticks") != int(start_raw)):
    raise SystemExit("network-quarantine receipt differs before stop intent")
network_quarantine_sha = hashlib.sha256(network_quarantine_raw).hexdigest()

if intent_path.exists() or armed_path.exists() or intent_path.is_symlink() or armed_path.is_symlink():
    if not intent_path.exists() or intent_path.is_symlink():
        raise SystemExit("durable stop armed marker exists without its intent")
    details = intent_path.lstat()
    if not stat.S_ISREG(details.st_mode) or details.st_mode & 0o222:
        raise SystemExit("durable stop intent is mutable or non-regular")
    intent = json.loads(intent_path.read_text(encoding="utf-8"))
    expected_invariants = {
        "schema": "arc.recovery.stop-intent.v1",
        "capture_id": capture_id,
        "node": node,
        "freeze_plan_sha256": freeze_sha,
        "writer_contract_sha256": contract_sha,
        "pre_fence_intent_sha256": prefence_intent_sha,
        "frozen_context_sha256": frozen_context_sha,
        "network_quarantine_sha256": network_quarantine_sha,
    }
    if {key: intent.get(key) for key in expected_invariants} != expected_invariants:
        raise SystemExit("durable stop intent differs from sealed freeze plan")
    if set(intent) != set(expected_invariants) | {"intent_at"} or not isinstance(intent["intent_at"], str):
        raise SystemExit("durable stop intent fields are not exact")
    canonical_intent = (json.dumps(intent, sort_keys=True, separators=(",", ":")) + "\n").encode()
    if intent_path.read_bytes() != canonical_intent:
        raise SystemExit("durable stop intent is not canonical JSON")
    if armed_path.exists() or armed_path.is_symlink():
        details = armed_path.lstat()
        if not stat.S_ISREG(details.st_mode) or details.st_mode & 0o222 or armed_path.is_symlink():
            raise SystemExit("durable stop armed marker is mutable or non-regular")
        expected_armed = f"schema=arc.recovery.stop-armed.v1\nintent_sha256={hashlib.sha256(canonical_intent).hexdigest()}\n"
        if armed_path.read_text(encoding="ascii") != expected_armed:
            raise SystemExit("durable stop armed marker differs from its intent")
    raise SystemExit(0)

# Every byte transitively bound by stop.intent must reach stable storage first.
for base, dirs, files in os.walk(root / "evidence", followlinks=False):
    for name in dirs:
        if (pathlib.Path(base) / name).is_symlink():
            raise SystemExit("pre-stop evidence contains a symlink directory")
    for name in files:
        candidate = pathlib.Path(base) / name
        if candidate.is_symlink() or not stat.S_ISREG(candidate.lstat().st_mode):
            raise SystemExit("pre-stop evidence contains a non-regular file")
        descriptor = os.open(candidate, os.O_RDONLY | getattr(os, "O_NOFOLLOW", 0))
        try: os.fsync(descriptor)
        finally: os.close(descriptor)
for directory_path in (root / "evidence", root, root.parent):
    descriptor = os.open(directory_path, os.O_RDONLY | getattr(os, "O_DIRECTORY", 0))
    try: os.fsync(descriptor)
    finally: os.close(descriptor)

intent_at = datetime.datetime.now(datetime.timezone.utc).replace(microsecond=0).isoformat().replace("+00:00", "Z")
intent = {
    "schema": "arc.recovery.stop-intent.v1",
    "capture_id": capture_id,
    "node": node,
    "freeze_plan_sha256": freeze_sha,
    "writer_contract_sha256": contract_sha,
    "pre_fence_intent_sha256": prefence_intent_sha,
    "frozen_context_sha256": frozen_context_sha,
    "network_quarantine_sha256": network_quarantine_sha,
    "intent_at": intent_at,
}
canonical_intent = (json.dumps(intent, sort_keys=True, separators=(",", ":")) + "\n").encode()
def publish(path, payload):
    temporary = path.with_name(f".{path.name}.partial")
    if temporary.exists() or temporary.is_symlink():
        details = temporary.lstat()
        if not stat.S_ISREG(details.st_mode) or temporary.is_symlink():
            raise SystemExit("unsafe durable stop marker partial")
        temporary.unlink()
    fd = os.open(temporary, os.O_WRONLY | os.O_CREAT | os.O_EXCL | getattr(os, "O_NOFOLLOW", 0), 0o400)
    with os.fdopen(fd, "wb") as handle:
        handle.write(payload); handle.flush(); os.fsync(handle.fileno())
    os.rename(temporary, path)
    directory = os.open(path.parent, os.O_RDONLY | getattr(os, "O_DIRECTORY", 0))
    try: os.fsync(directory)
    finally: os.close(directory)

publish(intent_path, canonical_intent)
for base, dirs, files in os.walk(root, followlinks=False):
    for name in dirs:
        if (pathlib.Path(base) / name).is_symlink():
            raise SystemExit("durable stop journal contains a symlink directory")
    for name in files:
        candidate = pathlib.Path(base) / name
        if candidate.is_symlink() or not stat.S_ISREG(candidate.lstat().st_mode):
            raise SystemExit("durable stop journal contains a non-regular file")
        descriptor = os.open(candidate, os.O_RDONLY | getattr(os, "O_NOFOLLOW", 0))
        try: os.fsync(descriptor)
        finally: os.close(descriptor)
for directory in (root / "evidence", root, root.parent):
    descriptor = os.open(directory, os.O_RDONLY)
    try: os.fsync(descriptor)
    finally: os.close(descriptor)
PY
}

arm_stop_journal() {
    local root="$1"
    python3 - "$root" <<'PY'
import hashlib
import os
import pathlib
import stat
import sys

root = pathlib.Path(sys.argv[1])
intent = root / "stop.intent.json"
fence = root / "10-fence-verified.json"
armed = root / "stop.armed"
for path in (intent, fence):
    mode = path.lstat().st_mode
    if path.is_symlink() or not stat.S_ISREG(mode) or mode & 0o222:
        raise SystemExit("stop intent/fence proof is missing or unsafe before arming")
payload = f"schema=arc.recovery.stop-armed.v1\nintent_sha256={hashlib.sha256(intent.read_bytes()).hexdigest()}\n".encode()
if armed.exists() or armed.is_symlink():
    if armed.is_symlink() or armed.read_bytes() != payload:
        raise SystemExit("existing stop armed marker differs")
else:
    temporary = armed.with_name(f".{armed.name}.partial")
    if temporary.exists() or temporary.is_symlink():
        if temporary.is_symlink() or not temporary.is_file():
            raise SystemExit("unsafe armed-marker partial")
        temporary.unlink()
    descriptor = os.open(temporary, os.O_WRONLY | os.O_CREAT | os.O_EXCL | getattr(os, "O_NOFOLLOW", 0), 0o400)
    with os.fdopen(descriptor, "wb") as handle:
        handle.write(payload); handle.flush(); os.fsync(handle.fileno())
    os.rename(temporary, armed)
directory = os.open(root, os.O_RDONLY | getattr(os, "O_DIRECTORY", 0))
try: os.fsync(directory)
finally: os.close(directory)
PY
}

# Current v5 barrier transaction.
commit_restart_barrier() {
    local root="$1" selected_unit="$2" selected_main_pid="$3" sealed_boot_id="$4"
    local marker="/etc/arc-recovery/legacy-start-allowed" current_boot other_unit unit
    case "$selected_unit" in
        arc-self-heal.service) other_unit=arc-node.service ;;
        arc-node.service) other_unit=arc-self-heal.service ;;
        *) die "selected restart-barrier unit is not reviewed" ;;
    esac
    current_boot="$(cat /proc/sys/kernel/random/boot_id)"

    local live_quarantine_barrier=false
    if [ ! -e "$marker" ] && [ ! -L "$marker" ] \
            && [ ! -e "$root/06-restart-barrier-armed.json" ] \
            && [ ! -L "$root/06-restart-barrier-armed.json" ]; then
        [ -f "$root/09-quarantine-restart-arm.json" ] \
            && [ ! -L "$root/09-quarantine-restart-arm.json" ] \
            && [ -f "$root/09-quarantine-restart-committed.json" ] \
            && [ ! -L "$root/09-quarantine-restart-committed.json" ] || \
            die "allow marker is absent without a stop arm or live quarantine arm"
        live_quarantine_barrier=true
    fi

    # Marker absence plus a durable arm is the commit point even when a crash
    # interrupted publication of the commit receipt. Re-fsync the parent and
    # publish the inferred receipt without touching a stale PID or cgroup.
    if [ ! -e "$marker" ] && [ ! -L "$marker" ] \
            && [ "$live_quarantine_barrier" = false ]; then
        python3 - "$root" "$selected_unit" "$selected_main_pid" "$sealed_boot_id" <<'PY'
import hashlib
import json
import os
import pathlib
import stat
import subprocess
import sys

root = pathlib.Path(sys.argv[1]); selected = sys.argv[2]
selected_pid = int(sys.argv[3]); sealed_boot = sys.argv[4]
observed_boot = pathlib.Path("/proc/sys/kernel/random/boot_id").read_text().strip()
arm_path = root / "06-restart-barrier-armed.json"
commit_path = root / "07-restart-barrier-committed.json"
marker = pathlib.Path("/etc/arc-recovery/legacy-start-allowed")
barrier_bytes = b"[Unit]\nConditionPathExists=/etc/arc-recovery/legacy-start-allowed\n"
units = ("arc-self-heal.service", "arc-node.service", "arc-node-update.service", "arc-node-update.timer")
def canonical(value): return (json.dumps(value, sort_keys=True, separators=(",", ":")) + "\n").encode()
if marker.exists() or marker.is_symlink(): raise SystemExit("restart-barrier marker unexpectedly exists")
if arm_path.is_symlink() or not arm_path.is_file(): raise SystemExit("restart-barrier arm is missing")
arm_raw = arm_path.read_bytes(); arm = json.loads(arm_raw)
if (arm_raw != canonical(arm) or arm.get("schema") != "arc.recovery.restart-barrier-arm.v1"
        or arm.get("selected_unit") != selected
        or arm.get("selected_main_pid") != selected_pid
        or arm.get("sealed_boot_id") != sealed_boot
        or arm.get("allow_marker_observed_present") is not True
        or arm.get("all_cgroups_frozen") is not True
        or arm.get("effective_control_masks") is not True
        or arm.get("control_masks") != {unit: "/dev/null" for unit in units}
        or arm.get("pre_mask_activation_gate_sha256") != arm.get("source_sha256", {}).get("06-pre-mask-activation-gate.json")):
    raise SystemExit("restart-barrier arm differs")
def prop(unit, name):
    return subprocess.check_output(["systemctl", "show", unit, f"--property={name}", "--value"], text=True).strip()
for unit in units:
    for persistent_control in (
        pathlib.Path(f"/etc/systemd/system.control/{unit}"),
        pathlib.Path(f"/etc/systemd/system.control/{unit}.d"),
        pathlib.Path(f"/run/systemd/system.control/{unit}.d"),
    ):
        if persistent_control.exists() or persistent_control.is_symlink():
            raise SystemExit(f"persistent systemd control override exists: {unit}")
    path = pathlib.Path(f"/etc/systemd/system/{unit}.d/zzzz-arc-recovery-freeze.conf")
    details = path.lstat()
    if (path.is_symlink() or not stat.S_ISREG(details.st_mode)
            or details.st_uid != 0 or details.st_gid != 0 or details.st_mode & 0o222
            or path.read_bytes() != barrier_bytes):
        raise SystemExit(f"persistent start barrier differs: {unit}")
if observed_boot == sealed_boot:
    for unit in units:
        mask = pathlib.Path(f"/run/systemd/system.control/{unit}")
        if (not mask.is_symlink() or os.readlink(mask) != "/dev/null"
                or prop(unit, "LoadState") != "masked"
                or prop(unit, "FragmentPath") != f"/run/systemd/system.control/{unit}"
                or prop(unit, "UnitFileState") != "masked-runtime"
                or prop(unit, "Job") not in {"", "0"}):
            raise SystemExit(f"same-boot inferred control mask differs: {unit}")
else:
    for unit in units:
        mask = pathlib.Path(f"/run/systemd/system.control/{unit}")
        if mask.exists() or mask.is_symlink():
            raise SystemExit(f"sealed-boot control mask survived reboot: {unit}")
        if (prop(unit, "ActiveState") not in {"inactive", "failed"}
                or (unit.endswith(".service") and prop(unit, "MainPID") != "0")
                or prop(unit, "Job") not in {"", "0"}):
            raise SystemExit(f"post-reboot legacy unit is not stably fenced: {unit}")
parent = os.open("/etc/arc-recovery", os.O_RDONLY | getattr(os, "O_DIRECTORY", 0) | getattr(os, "O_NOFOLLOW", 0))
try: os.fsync(parent)
finally: os.close(parent)
etc = os.open("/etc", os.O_RDONLY | getattr(os, "O_DIRECTORY", 0) | getattr(os, "O_NOFOLLOW", 0))
try: os.fsync(etc)
finally: os.close(etc)
basis = "same-boot-reconciled-parent-fsync" if observed_boot == sealed_boot else "post-reboot-marker-absence-parent-fsync"
value = {
    "schema": "arc.recovery.restart-barrier-committed.v2",
    "barrier_arm_sha256": hashlib.sha256(arm_raw).hexdigest(),
    "sealed_boot_id": sealed_boot, "observed_boot_id": observed_boot,
    "allow_marker_path": str(marker), "allow_marker_absent": True,
    "unlink_parent_fsynced": True, "durability_basis": basis,
    "selected_unit": selected, "selected_main_pid_on_sealed_boot": selected_pid,
    "reboot_requires_zero_pid_signals": observed_boot != sealed_boot,
}
payload = canonical(value)
if commit_path.exists() or commit_path.is_symlink():
    if commit_path.is_symlink() or not commit_path.is_file(): raise SystemExit("restart-barrier commit is unsafe")
    existing = json.loads(commit_path.read_bytes())
    if (commit_path.read_bytes() != canonical(existing)
            or existing.get("schema") != value["schema"]
            or existing.get("barrier_arm_sha256") != value["barrier_arm_sha256"]
            or existing.get("sealed_boot_id") != sealed_boot
            or existing.get("allow_marker_absent") is not True
            or existing.get("unlink_parent_fsynced") is not True):
        raise SystemExit("restart-barrier commit receipt differs")
else:
    temporary = commit_path.with_name(f".{commit_path.name}.partial")
    if temporary.exists() or temporary.is_symlink():
        if temporary.is_symlink() or not temporary.is_file(): raise SystemExit("unsafe commit partial")
        temporary.unlink()
    descriptor = os.open(temporary, os.O_WRONLY | os.O_CREAT | os.O_EXCL | getattr(os, "O_NOFOLLOW", 0), 0o400)
    with os.fdopen(descriptor, "wb") as handle:
        handle.write(payload); handle.flush(); os.fsync(handle.fileno())
    os.rename(temporary, commit_path)
    directory = os.open(root, os.O_RDONLY | getattr(os, "O_DIRECTORY", 0) | getattr(os, "O_NOFOLLOW", 0))
    try: os.fsync(directory)
    finally: os.close(directory)
PY
        return 0
    fi
    [ ! -L "$marker" ] || die "legacy-start allow marker is a symlink"
    [ "$current_boot" = "$sealed_boot_id" ] || \
        die "sealed boot ended before barrier commit; marker remains present, so require a fresh audit/plan and send zero stale signals"

    # Install exact volatile lifecycle safety for the four-unit activation
    # closure. These files vanish on reboot while the still-present marker
    # permits the prepared legacy unit to recover normally.
    python3 - "$selected_unit" <<'PY'
import os
import secrets
import stat
import sys

selected = sys.argv[1]
units = ("arc-self-heal.service", "arc-node.service", "arc-node-update.service", "arc-node-update.timer")
service = b"[Unit]\nRefuseManualStart=yes\nRefuseManualStop=yes\nIgnoreOnIsolate=yes\nStopWhenUnneeded=no\nBindsTo=\nPartOf=\nPropagatesStopTo=\nOnFailure=\nOnSuccess=\nFailureAction=none\nSuccessAction=none\nJobTimeoutAction=none\n\n[Service]\nExecReload=\nExecStop=\nExecStopPost=\nRestart=no\nKillMode=process\nSendSIGKILL=no\nSendSIGHUP=no\nOOMPolicy=continue\nWatchdogSec=0\nRuntimeMaxSec=infinity\n"
timer = b"[Unit]\nRefuseManualStart=yes\nRefuseManualStop=yes\nIgnoreOnIsolate=yes\nStopWhenUnneeded=no\nBindsTo=\nPartOf=\nPropagatesStopTo=\nOnFailure=\nOnSuccess=\nFailureAction=none\nSuccessAction=none\nJobTimeoutAction=none\n"
flags = os.O_RDONLY | getattr(os, "O_DIRECTORY", 0) | getattr(os, "O_NOFOLLOW", 0)
root = os.open("/run/systemd/system", flags)
try:
    details = os.fstat(root)
    if (not stat.S_ISDIR(details.st_mode) or details.st_uid != 0 or details.st_gid != 0
            or details.st_mode & 0o022): raise SystemExit("runtime systemd directory is unsafe")
    for unit in units:
        directory_name = f"{unit}.d"; expected = timer if unit.endswith(".timer") else service
        try: details = os.stat(directory_name, dir_fd=root, follow_symlinks=False)
        except FileNotFoundError:
            os.mkdir(directory_name, 0o755, dir_fd=root)
            details = os.stat(directory_name, dir_fd=root, follow_symlinks=False)
        if (not stat.S_ISDIR(details.st_mode) or details.st_uid != 0 or details.st_gid != 0
                or details.st_mode & 0o022): raise SystemExit("runtime drop-in directory is unsafe")
        child = os.open(directory_name, flags, dir_fd=root)
        try:
            name = "zzzy-arc-recovery-prefreeze-safety.conf"
            try: current = os.stat(name, dir_fd=child, follow_symlinks=False)
            except FileNotFoundError: current = None
            if current is None:
                temporary = f".{name}.partial.{secrets.token_hex(8)}"
                descriptor = os.open(temporary, os.O_WRONLY | os.O_CREAT | os.O_EXCL | getattr(os, "O_NOFOLLOW", 0), 0o444, dir_fd=child)
                try:
                    os.write(descriptor, expected); os.fchmod(descriptor, 0o444); os.fsync(descriptor)
                finally: os.close(descriptor)
                os.rename(temporary, name, src_dir_fd=child, dst_dir_fd=child)
                current = os.stat(name, dir_fd=child, follow_symlinks=False)
            if (not stat.S_ISREG(current.st_mode) or current.st_uid != 0 or current.st_gid != 0):
                raise SystemExit("runtime safety overlay inode is unsafe")
            descriptor = os.open(name, os.O_RDONLY | getattr(os, "O_NOFOLLOW", 0), dir_fd=child)
            try:
                if os.read(descriptor, len(expected) + 1) != expected: raise SystemExit("runtime safety overlay bytes differ")
                os.fsync(descriptor)
            finally: os.close(descriptor)
            os.fsync(child)
        finally: os.close(child)
    os.fsync(root)
finally: os.close(root)
PY
    systemctl daemon-reload
    [ "$(systemctl show "$selected_unit" --property=MainPID --value)" = "$selected_main_pid" ] || \
        die "selected supervisor changed after lifecycle safety reload"
    [ "$(systemctl show "$selected_unit" --property=ActiveState --value)" = active ] || \
        die "selected supervisor is not active after lifecycle safety reload"
    case "$(systemctl show "$selected_unit" --property=FreezerState --value)" in running|frozen) ;;
        *) die "selected supervisor has an invalid advisory PID1 freezer state before activation masking" ;;
    esac
    case "$(systemctl show "$selected_unit" --property=Job --value)" in ''|0) ;; *) die "selected supervisor has a job before activation masking" ;; esac
    local existing_control_masks=0 control_mask_path
    for unit in arc-self-heal.service arc-node.service arc-node-update.service arc-node-update.timer; do
        control_mask_path="/run/systemd/system.control/$unit"
        if [ -L "$control_mask_path" ]; then
            [ "$(readlink "$control_mask_path")" = /dev/null ] || \
                die "existing high-priority control mask differs: $unit"
            existing_control_masks=$((existing_control_masks + 1))
        elif [ -e "$control_mask_path" ]; then
            die "existing high-priority control-mask path is not a symlink: $unit"
        fi
    done
    if [ "$existing_control_masks" -eq 0 ]; then
        for unit in arc-self-heal.service arc-node.service arc-node-update.service arc-node-update.timer; do
            [ "$(systemctl show "$unit" --property=Names --value 2>/dev/null)" = "$unit" ] || \
                die "legacy unit Names closure differs before masking: $unit"
            [ "$(systemctl show "$unit" --property=Id --value 2>/dev/null)" = "$unit" ] || \
                die "legacy unit canonical Id differs before masking: $unit"
            [ -z "$(systemctl show "$unit" --property=Following --value 2>/dev/null)" ] || \
                die "legacy unit follows an alias before masking: $unit"
            case "$(systemctl show "$unit" --property=Job --value 2>/dev/null || true)" in ''|0) ;;
                *) die "legacy activation source has a job before control masking: $unit" ;;
            esac
            if [ "$unit" != "$selected_unit" ]; then
                case "$(systemctl show "$unit" --property=ActiveState --value 2>/dev/null || printf not-found)" in inactive|failed|not-found) ;;
                    *) die "alternative activation source is active before control masking: $unit" ;;
                esac
            fi
        done
        [ "$(systemctl is-enabled "$selected_unit" 2>/dev/null)" = enabled ] || \
            die "selected supervisor is not enabled before volatile activation masking"
        case " $(systemctl show "$selected_unit" --property=WantedBy --value) " in
            *" multi-user.target "*) ;;
            *) die "selected supervisor has no multi-user.target boot edge before activation masking" ;;
        esac
    else
        [ -f "$root/06-pre-mask-activation-gate.json" ] && \
            [ ! -L "$root/06-pre-mask-activation-gate.json" ] || \
            die "volatile control masks exist without a durable pre-mask activation gate"
    fi

    # Seal the complete unmasked activation/lifecycle projection before the
    # first volatile mask is created. A retry after any partial mask set can
    # converge from this fsynced event without trying to reconstruct facts that
    # PID1 intentionally hides behind LoadState=masked.
    python3 - "$root" "$selected_unit" "$selected_main_pid" "$sealed_boot_id" "$existing_control_masks" <<'PY'
import hashlib
import json
import os
import pathlib
import re
import stat
import subprocess
import sys

root = pathlib.Path(sys.argv[1]); selected = sys.argv[2]
selected_pid = int(sys.argv[3]); sealed_boot = sys.argv[4]; mask_count = int(sys.argv[5])
event_path = root / "06-pre-mask-activation-gate.json"
units = ("arc-self-heal.service", "arc-node.service", "arc-node-update.service", "arc-node-update.timer")
marker = pathlib.Path("/etc/arc-recovery/legacy-start-allowed")
marker_payload = b"schema=arc.recovery.legacy-start-allow.v1\n"
service = b"[Unit]\nRefuseManualStart=yes\nRefuseManualStop=yes\nIgnoreOnIsolate=yes\nStopWhenUnneeded=no\nBindsTo=\nPartOf=\nPropagatesStopTo=\nOnFailure=\nOnSuccess=\nFailureAction=none\nSuccessAction=none\nJobTimeoutAction=none\n\n[Service]\nExecReload=\nExecStop=\nExecStopPost=\nRestart=no\nKillMode=process\nSendSIGKILL=no\nSendSIGHUP=no\nOOMPolicy=continue\nWatchdogSec=0\nRuntimeMaxSec=infinity\n"
timer = b"[Unit]\nRefuseManualStart=yes\nRefuseManualStop=yes\nIgnoreOnIsolate=yes\nStopWhenUnneeded=no\nBindsTo=\nPartOf=\nPropagatesStopTo=\nOnFailure=\nOnSuccess=\nFailureAction=none\nSuccessAction=none\nJobTimeoutAction=none\n"
def canonical(value): return (json.dumps(value, sort_keys=True, separators=(",", ":")) + "\n").encode()
def load(path, schema):
    if path.is_symlink() or not path.is_file(): raise SystemExit(f"pre-mask source is missing: {path}")
    raw = path.read_bytes(); value = json.loads(raw)
    if raw != canonical(value) or value.get("schema") != schema: raise SystemExit(f"pre-mask source differs: {path}")
    return value, raw
def prop(unit, name):
    return subprocess.check_output(["systemctl", "show", unit, f"--property={name}", "--value"], text=True).strip()
def enabled(unit):
    result = subprocess.run(["systemctl", "is-enabled", unit], text=True, stdout=subprocess.PIPE, stderr=subprocess.DEVNULL, check=False)
    return result.stdout.strip()
def proc_start(pid):
    raw = pathlib.Path(f"/proc/{pid}/stat").read_text(encoding="ascii"); end = raw.rfind(")")
    fields = raw[end + 2:].split()
    if end < 0 or len(fields) < 20: raise SystemExit("pre-mask process stat is truncated")
    return int(fields[19])
def cgroup_frozen(entry):
    base = pathlib.Path("/sys/fs/cgroup") / entry["path"].lstrip("/"); details = base.lstat()
    if (base.is_symlink() or not base.is_dir() or details.st_dev != entry["device"]
            or details.st_ino != entry["inode"]): raise SystemExit("pre-mask cgroup identity differs")
    values = dict(line.split(" ", 1) for line in base.joinpath("cgroup.events").read_text(encoding="ascii").splitlines())
    if values.get("frozen") != "1": raise SystemExit("pre-mask cgroup is not frozen")
def mount_projection():
    rows = []
    for line in pathlib.Path("/proc/self/mountinfo").read_text(encoding="ascii").splitlines():
        left, separator, right = line.partition(" - "); fields = left.split(); after = right.split()
        if separator and len(fields) > 4 and len(after) >= 2:
            rows.append({"id": fields[0], "major_minor": fields[2], "mountpoint": fields[4], "fstype": after[0], "source": after[1]})
    def deepest(target):
        candidates = [row for row in rows if target == row["mountpoint"] or target.startswith(row["mountpoint"].rstrip("/") + "/")]
        if not candidates: raise SystemExit(f"no mount covers {target}")
        return max(candidates, key=lambda row: len(row["mountpoint"]))
    observed = {target: deepest(target) for target in ("/run", "/run/systemd", "/run/systemd/system.control")}
    baseline = observed["/run"]
    if (baseline["mountpoint"] != "/run" or baseline["fstype"] != "tmpfs"
            or any(row != baseline for row in observed.values())):
        raise SystemExit(f"systemd control path is not on the exact /run tmpfs: {observed}")
    return baseline
def unit_path_projection():
    values = subprocess.check_output(["systemctl", "show", "--property=UnitPath", "--value"], text=True).split()
    required = ("/etc/systemd/system.control", "/run/systemd/system.control", "/etc/systemd/system")
    if (len(values) != len(set(values)) or any(path not in values for path in required)
            or not (values.index(required[0]) < values.index(required[1]) < values.index(required[2]))):
        raise SystemExit(f"systemd UnitPath priority differs: {values}")
    return values

if pathlib.Path("/proc/sys/kernel/random/boot_id").read_text().strip() != sealed_boot:
    raise SystemExit("boot changed before the pre-mask activation gate")
live_arm_raw = None; live_commit_raw = None
if marker.exists() or marker.is_symlink():
    marker_details = marker.lstat()
    if (marker.is_symlink() or not stat.S_ISREG(marker_details.st_mode)
            or marker_details.st_uid != 0 or marker_details.st_gid != 0
            or stat.S_IMODE(marker_details.st_mode) != 0o400
            or marker.read_bytes() != marker_payload): raise SystemExit("pre-mask allow marker differs")
    marker_identity = {
        "path": str(marker), "sha256": hashlib.sha256(marker_payload).hexdigest(),
        "device": marker_details.st_dev, "inode": marker_details.st_ino,
        "uid": marker_details.st_uid, "gid": marker_details.st_gid,
        "mode": stat.S_IMODE(marker_details.st_mode), "size": marker_details.st_size,
    }
else:
    live_arm, live_arm_raw = load(
        root / "09-quarantine-restart-arm.json",
        "arc.recovery.quarantine-live-restart-arm.v1",
    )
    live_commit, live_commit_raw = load(
        root / "09-quarantine-restart-committed.json",
        "arc.recovery.quarantine-live-restart-committed.v1",
    )
    marker_identity = live_arm.get("allow_marker")
    if (live_commit.get("quarantine_restart_arm_sha256") != hashlib.sha256(live_arm_raw).hexdigest()
            or live_commit.get("allow_marker_absent") is not True
            or live_commit.get("allow_marker_parent_fsynced") is not True
            or live_commit.get("restart_prevented") is not True
            or live_arm.get("sealed_boot_id") != sealed_boot
            or live_arm.get("selected_unit") != selected
            or live_arm.get("selected_main_pid") != selected_pid
            or not isinstance(marker_identity, dict)
            or marker_identity.get("path") != str(marker)
            or marker_identity.get("sha256") != hashlib.sha256(marker_payload).hexdigest()):
        raise SystemExit("live quarantine restart arm cannot source the pre-mask marker identity")
schemas = {
    "02-fast-cgroup-freeze-intent.json": "arc.recovery.fast-cgroup-freeze-intent.v1",
    "03-fast-cgroups-frozen.json": "arc.recovery.fast-cgroups-frozen.v1",
    "04-pre-fence-quiesce-intent.json": "arc.recovery.pre-fence-quiesce-intent.v1",
    "05-cgroups-frozen.json": "arc.recovery.cgroups-frozen.v1",
    "08-network-quarantine.json": "arc.recovery.legacy-network-quarantine.v1",
    "stop.intent.json": "arc.recovery.stop-intent.v1",
}
values = {}; sources = {}
for name, schema in schemas.items():
    values[name], raw = load(root / name, schema); sources[name] = hashlib.sha256(raw).hexdigest()
if live_arm_raw is not None:
    if live_arm.get("freeze_plan_sha256") != values["02-fast-cgroup-freeze-intent.json"].get("freeze_plan_sha256"):
        raise SystemExit("live quarantine restart arm freeze plan differs from stop sources")
    sources["09-quarantine-restart-arm.json"] = hashlib.sha256(live_arm_raw).hexdigest()
    sources["09-quarantine-restart-committed.json"] = hashlib.sha256(live_commit_raw).hexdigest()
progress_hashes = values["03-fast-cgroups-frozen.json"].get("per_cgroup_progress_sha256")
if not isinstance(progress_hashes, dict) or not progress_hashes:
    raise SystemExit("fast-freeze per-cgroup progress inventory is missing before masking")
progress_files = {
    "supervisor": "02-supervisor-cgroup-frozen.json",
    "writer-parent": "02-writer-parent-cgroup-frozen.json",
    "writer-leaf-move-intent": "02-writer-leaf-move-intent.json",
    "writer": "02-writer-cgroup-frozen.json",
    "writer-parent-release": "02-writer-parent-released.json",
}
for role, expected_sha in progress_hashes.items():
    progress_name = progress_files.get(role)
    schema = {
        "writer-leaf-move-intent": "arc.recovery.detached-writer-leaf-move-intent.v1",
        "writer-parent-release": "arc.recovery.detached-writer-parent-release.v1",
    }.get(role, "arc.recovery.fast-cgroup-progress.v1")
    if progress_name is None: raise SystemExit(f"unreviewed fast-freeze progress role: {role}")
    progress, progress_raw = load(root / progress_name, schema)
    if hashlib.sha256(progress_raw).hexdigest() != expected_sha:
        raise SystemExit(f"fast-freeze per-cgroup progress differs: {role}")
    sources[progress_name] = expected_sha
contract, contract_raw = load(root / "evidence" / "writer-contract.json", "arc.recovery.exact-writer.v3")
sources["evidence/writer-contract.json"] = hashlib.sha256(contract_raw).hexdigest()
freeze_sha = contract.get("freeze_plan_sha256")
plan_path = pathlib.Path(f"/root/.arc-recovery-plans/{freeze_sha}/freeze.lock.json")
plan_raw = plan_path.read_bytes() if plan_path.is_file() and not plan_path.is_symlink() else b""
if hashlib.sha256(plan_raw).hexdigest() != freeze_sha: raise SystemExit("pinned freeze plan differs before masking")
plan = json.loads(plan_raw)
if plan_raw != canonical(plan) or plan.get("schema") != "arc.recovery.freeze-plan.v5": raise SystemExit("pinned freeze plan is not canonical v5")
node_name = values["stop.intent.json"].get("node")
matches = [row for row in plan.get("nodes", []) if isinstance(row, dict) and row.get("name") == node_name]
if len(matches) != 1 or not isinstance(matches[0].get("prepare_barrier"), dict):
    raise SystemExit("pinned prepare barrier is missing for this node")
prepare_barrier = matches[0]["prepare_barrier"]
prepare_sha = hashlib.sha256(canonical(prepare_barrier)).hexdigest()
boot_activation = prepare_barrier.get("boot_activation")
if not isinstance(boot_activation, dict): raise SystemExit("pinned boot activation proof is missing")
if subprocess.check_output(["systemctl", "get-default"], text=True).strip() != boot_activation.get("default_target"):
    raise SystemExit("boot default target changed after the pinned prepare audit")
default_projection = {
    name: prop(boot_activation["default_target"], name)
    for name in ("Names", "Id", "Following", "LoadState", "FragmentPath", "Requires", "Wants")
}
if default_projection != boot_activation.get("default_target_projection"):
    raise SystemExit("boot default-target projection changed after the pinned prepare audit")
def verify_symlink_identity(row, label, resolved=False):
    if not isinstance(row, dict): raise SystemExit(f"{label} symlink contract is missing")
    path = pathlib.Path(row.get("path", "")); details = path.lstat()
    if (not path.is_symlink() or details.st_dev != row.get("device") or details.st_ino != row.get("inode")
            or details.st_uid != row.get("uid") or details.st_gid != row.get("gid")
            or os.readlink(path) != row.get("target")):
        raise SystemExit(f"{label} symlink identity changed after the pinned prepare audit")
    if resolved:
        target = pathlib.Path(os.path.realpath(path))
        if (str(target) != row.get("resolved_path") or target.is_symlink() or not target.is_file()
                or hashlib.sha256(target.read_bytes()).hexdigest() != row.get("resolved_sha256")):
            raise SystemExit(f"{label} resolved unit changed after the pinned prepare audit")
verify_symlink_identity(boot_activation.get("default_target_symlink"), "default-target")
verify_symlink_identity(boot_activation.get("selected_enablement_symlink"), "selected enablement", resolved=True)
if (boot_activation.get("selected_reached_from_multi_user") is not True
        or boot_activation.get("precommit_reboot_fail_open") is not True):
    raise SystemExit("pinned precommit reboot fail-open proof differs")
sources[str(plan_path)] = freeze_sha
pinned_unit_states = prepare_barrier.get("unit_states")
pinned_activation = prepare_barrier.get("activation_closure")
if (not isinstance(pinned_unit_states, dict) or set(pinned_unit_states) != set(units)
        or not isinstance(pinned_activation, dict) or set(pinned_activation) != set(units)):
    raise SystemExit("pinned alternative activation closure is missing")
reverse_activation_fields = (
    "Names", "Id", "Following", "RequiredBy", "WantedBy", "BoundBy",
    "UpheldBy", "TriggeredBy", "OnFailureOf", "OnSuccessOf",
)
alternative_activation_closure = {}
terminal_enablement = {"disabled", "masked", "masked-runtime", "static", "indirect", "generated", "transient", "not-found"}
for alternative in units:
    if alternative == selected: continue
    current_enablement = enabled(alternative)
    pinned_state = pinned_unit_states.get(alternative, {})
    current_closure = {name: prop(alternative, name) for name in reverse_activation_fields}
    expected_closure = {name: pinned_activation.get(alternative, {}).get(name) for name in reverse_activation_fields}
    if (current_enablement != pinned_state.get("enablement") or current_enablement not in terminal_enablement
            or current_closure != expected_closure):
        raise SystemExit(f"alternative activation closure changed before masking: {alternative}")
    alternative_activation_closure[alternative] = {
        "enablement": current_enablement, "reverse_activation": current_closure,
    }
cgroups = values["05-cgroups-frozen.json"].get("cgroups")
if not isinstance(cgroups, list) or not cgroups: raise SystemExit("pre-mask cgroup inventory differs")
for entry in cgroups: cgroup_frozen(entry)
fast_frozen = values["03-fast-cgroups-frozen.json"]
prefence = values["04-pre-fence-quiesce-intent.json"]
frozen = values["05-cgroups-frozen.json"]
writer_parent_scope_cgroup = fast_frozen.get("writer_parent_scope_cgroup")
writer_recovery_leaf = fast_frozen.get("writer_recovery_leaf")
for value_name, source in (("pre-fence", prefence), ("frozen", frozen)):
    if (source.get("writer_parent_scope_cgroup") != writer_parent_scope_cgroup
            or source.get("writer_recovery_leaf") != writer_recovery_leaf):
        raise SystemExit(f"{value_name} writer parent/leaf identity differs")
if writer_recovery_leaf is not None:
    if (writer_recovery_leaf not in cgroups or not isinstance(writer_parent_scope_cgroup, dict)):
        raise SystemExit("detached writer leaf is not a final frozen role")
    parent_path = pathlib.Path("/sys/fs/cgroup") / writer_parent_scope_cgroup["path"].lstrip("/")
    parent_details = parent_path.lstat()
    leaf_path = pathlib.Path("/sys/fs/cgroup") / writer_recovery_leaf["path"].lstrip("/")
    events = dict(line.split(" ", 1) for line in leaf_path.joinpath("cgroup.events").read_text().splitlines())
    if (parent_path.is_symlink() or parent_details.st_dev != writer_parent_scope_cgroup["device"]
            or parent_details.st_ino != writer_parent_scope_cgroup["inode"]
            or leaf_path.joinpath("cgroup.freeze").read_text(encoding="ascii").strip() != "1"
            or events.get("frozen") != "1" or events.get("populated") != "1"
            or set(int(value) for value in leaf_path.joinpath("cgroup.procs").read_text().splitlines())
            != {contract.get("writer_pid")}):
        raise SystemExit("detached writer leaf lost local freeze/identity before masking")
if (proc_start(selected_pid) != contract.get("supervisor_start_ticks")
        or proc_start(contract.get("writer_pid")) != contract.get("writer_start_ticks")):
    raise SystemExit("sealed process identity changed before masking")
run_mount = mount_projection(); unit_path = unit_path_projection()
for unit in units:
    for persistent in (
        pathlib.Path(f"/etc/systemd/system.control/{unit}"),
        pathlib.Path(f"/etc/systemd/system.control/{unit}.d"),
        pathlib.Path(f"/run/systemd/system.control/{unit}.d"),
    ):
        if persistent.exists() or persistent.is_symlink(): raise SystemExit(f"persistent control override exists: {unit}")

if event_path.exists() or event_path.is_symlink():
    existing, _ = load(event_path, "arc.recovery.pre-mask-activation-gate.v1")
    if (existing.get("freeze_plan_sha256") != freeze_sha or existing.get("sealed_boot_id") != sealed_boot
            or existing.get("selected_unit") != selected or existing.get("selected_main_pid") != selected_pid
            or existing.get("source_sha256") != sources or existing.get("prepare_barrier_sha256") != prepare_sha
            or existing.get("allow_marker") != marker_identity or existing.get("cgroups") != cgroups
            or existing.get("writer_parent_scope_cgroup") != writer_parent_scope_cgroup
            or existing.get("writer_recovery_leaf") != writer_recovery_leaf
            or existing.get("alternative_activation_closure") != alternative_activation_closure
            or existing.get("run_mount") != run_mount or existing.get("unit_path") != unit_path
            or existing.get("volatile_control_masks_absent_when_published") is not True
            or existing.get("persistent_control_masks_absent") is not True):
        raise SystemExit("durable pre-mask activation gate differs")
    if mask_count == 0:
        # With no masks, recompute the full projection below and require exact
        # equality; with partial masks, the durable event is the only valid
        # representation of PID1's intentionally hidden pre-mask state.
        pass
    else:
        raise SystemExit(0)
elif mask_count != 0:
    raise SystemExit("volatile masks exist before a durable pre-mask activation gate")

properties = (
    "Names", "Id", "Following", "ActiveState", "SubState", "MainPID", "Job",
    "ControlGroup", "FreezerState", "InvocationID", "WantedBy",
)
def normalized_prop(unit, name):
    value = prop(unit, name)
    return value or ("0" if name == "Job" else "")
unit_states = {unit: {name: normalized_prop(unit, name) for name in properties} for unit in units}
merged_unit_sources = {}
sealed_merged_sources = prepare_barrier.get("merged_unit_sources")
if not isinstance(sealed_merged_sources, dict) or set(sealed_merged_sources) != set(units):
    raise SystemExit("pinned prepare merged-source inventory differs")
for unit in units:
    merged = subprocess.check_output(["systemctl", "cat", unit])
    headers = [value.decode("utf-8") for value in re.findall(rb"(?m)^# (/[^\n]+)$", merged)]
    if not headers or len(headers) != len(set(headers)):
        raise SystemExit(f"pre-mask merged source manifest is incomplete: {unit}")
    rows = []
    for source_raw in headers:
        source = pathlib.Path(source_raw); details = source.lstat()
        if (source.is_symlink() or not stat.S_ISREG(details.st_mode)
                or details.st_uid != 0 or details.st_gid != 0 or details.st_mode & 0o022):
            raise SystemExit(f"pre-mask merged source is unsafe: {source}")
        rows.append({"path": source_raw, "sha256": hashlib.sha256(source.read_bytes()).hexdigest()})
    expected_rows = list(sealed_merged_sources[unit]) + [{
        "path": f"/run/systemd/system/{unit}.d/zzzy-arc-recovery-prefreeze-safety.conf",
        "sha256": hashlib.sha256(timer if unit.endswith(".timer") else service).hexdigest(),
    }]
    if ({row["path"]: row["sha256"] for row in rows}
            != {row["path"]: row["sha256"] for row in expected_rows}
            or len(rows) != len(expected_rows)):
        raise SystemExit(f"pre-mask merged source manifest drifted from the pinned prepare audit: {unit}")
    conditions = []; section = None
    for raw_line in merged.decode("utf-8").splitlines():
        line = raw_line.strip()
        if not line or line.startswith(("#", ";")): continue
        if line.startswith("[") and line.endswith("]"):
            section = line[1:-1].strip(); continue
        if section != "Unit" or "=" not in line: continue
        key, value = (item.strip() for item in line.split("=", 1))
        if key == "ConditionPathExists":
            if value == "": conditions.clear()
            else: conditions.append(value)
    if "/etc/arc-recovery/legacy-start-allowed" not in conditions:
        raise SystemExit(f"condition-only persistent start barrier is not effective before masking: {unit}")
    merged_unit_sources[unit] = rows
for unit, row in unit_states.items():
    if row["Names"] != unit or row["Id"] != unit or row["Following"] or row["Job"] != "0":
        raise SystemExit(f"pre-mask unit alias/job closure differs: {unit}")
    row["enablement"] = enabled(unit)
selected_state = unit_states[selected]
if (selected_state["ActiveState"] != "active" or selected_state["SubState"] != "running"
        or selected_state["MainPID"] != str(selected_pid)
        or selected_state["FreezerState"] not in {"running", "frozen"}
        or selected_state["InvocationID"] != contract["supervisor_context"]["invocation_id"]
        or selected_state["enablement"] != "enabled"
        or "multi-user.target" not in selected_state["WantedBy"].split()):
    raise SystemExit("selected pre-mask boot/lifecycle state differs")
for unit, row in unit_states.items():
    if unit != selected and (row["ActiveState"] not in {"inactive", "failed"}
            or (unit.endswith(".service") and row["MainPID"] != "0")):
        raise SystemExit(f"alternative pre-mask state differs: {unit}")
lifecycle_expected = {
    "Restart": "no", "KillMode": "process", "SendSIGKILL": "no", "SendSIGHUP": "no",
    "IgnoreOnIsolate": "yes", "OOMPolicy": "continue",
    "WatchdogUSec": "0", "RuntimeMaxUSec": "infinity", "CanReload": "no",
    "ExecReload": "", "ExecStop": "", "ExecStopPost": "", "RefuseManualStart": "yes",
    "RefuseManualStop": "yes", "StopWhenUnneeded": "no", "BindsTo": "", "PartOf": "",
    "PropagatesStopTo": "", "StopPropagatedFrom": "", "ReloadPropagatedFrom": "",
    "OnFailure": "", "OnSuccess": "", "SuccessAction": "none", "FailureAction": "none",
    "JobTimeoutAction": "none",
}
if any(prop(selected, name) != wanted for name, wanted in lifecycle_expected.items()):
    raise SystemExit("selected pre-mask lifecycle safety differs")
runtime_hashes = {}
for unit in units:
    expected = timer if unit.endswith(".timer") else service
    path = pathlib.Path(f"/run/systemd/system/{unit}.d/zzzy-arc-recovery-prefreeze-safety.conf")
    if path.is_symlink() or path.read_bytes() != expected: raise SystemExit(f"runtime safety differs before masking: {unit}")
    runtime_hashes[unit] = hashlib.sha256(expected).hexdigest()
for unit in units:
    mask = pathlib.Path(f"/run/systemd/system.control/{unit}")
    if mask.exists() or mask.is_symlink(): raise SystemExit(f"volatile mask appeared before gate publication: {unit}")
fast_writer = values["02-fast-cgroup-freeze-intent.json"].get("writer", {})
detached_scope = None
if fast_writer.get("scope_unit") is not None:
    scope_unit = fast_writer["scope_unit"]
    sealed_scope_properties = fast_writer.get("scope_properties")
    if not isinstance(writer_parent_scope_cgroup, dict):
        raise SystemExit("detached scope parent identity is missing before masking")
    scope_runtime_path = pathlib.Path(fast_writer.get("scope_runtime_safety_path", ""))
    try: scope_runtime_details = scope_runtime_path.lstat()
    except FileNotFoundError: raise SystemExit("detached scope runtime safety disappeared before masking")
    scope_parent_path = pathlib.Path("/sys/fs/cgroup") / writer_parent_scope_cgroup["path"].lstrip("/")
    try: scope_parent_details = scope_parent_path.lstat()
    except FileNotFoundError: raise SystemExit("detached scope parent cgroup disappeared before masking")
    if not isinstance(sealed_scope_properties, dict) or not sealed_scope_properties:
        raise SystemExit("detached scope property contract is missing before masking")
    if (scope_runtime_path
            != pathlib.Path(f"/run/systemd/system.control/{scope_unit}.d/zzzy-arc-recovery-writer-scope-safety.conf")
            or scope_runtime_path.is_symlink() or not stat.S_ISREG(scope_runtime_details.st_mode)
            or scope_runtime_details.st_uid != 0 or scope_runtime_details.st_gid != 0
            or scope_runtime_details.st_mode & 0o022
            or hashlib.sha256(scope_runtime_path.read_bytes()).hexdigest()
            != fast_writer.get("scope_runtime_safety_sha256")
            or scope_parent_path.is_symlink() or not scope_parent_path.is_dir()
            or scope_parent_details.st_dev != writer_parent_scope_cgroup["device"]
            or scope_parent_details.st_ino != writer_parent_scope_cgroup["inode"]):
        raise SystemExit("detached scope runtime safety file differs before masking")
    scope_properties = {name: normalized_prop(scope_unit, name) for name in sealed_scope_properties}
    scope_active = prop(scope_unit, "ActiveState") == "active"
    if scope_active:
        if (scope_properties != sealed_scope_properties
                or prop(scope_unit, "FreezerState") not in {"running", "frozen"}
                or prop(scope_unit, "Job") not in {"", "0"}):
            raise SystemExit("detached scope property/frozen projection differs before masking")
        parent_state = "active-sealed"
    else:
        if (prop(scope_unit, "ActiveState") not in {"inactive", "failed"}
                or prop(scope_unit, "Job") not in {"", "0"}
                or prop(scope_unit, "InvocationID") not in {"", sealed_scope_properties["InvocationID"]}
                or prop(scope_unit, "ControlGroup") not in {"", writer_parent_scope_cgroup["path"]}):
            raise SystemExit("detached scope terminal projection is not provenance-safe")
        parent_state = "terminal-after-leaf-seal"
    detached_scope = {
        "unit": scope_unit, "properties": sealed_scope_properties,
        "parent_state": parent_state,
        "runtime_safety_path": fast_writer.get("scope_runtime_safety_path"),
        "runtime_safety_sha256": fast_writer.get("scope_runtime_safety_sha256"),
        "sources": fast_writer.get("scope_runtime_sources"),
    }
value = {
    "schema": "arc.recovery.pre-mask-activation-gate.v1",
    "freeze_plan_sha256": freeze_sha, "sealed_boot_id": sealed_boot,
    "selected_unit": selected, "selected_main_pid": selected_pid,
    "source_sha256": sources, "prepare_barrier_sha256": prepare_sha,
    "allow_marker": marker_identity, "run_mount": run_mount, "unit_path": unit_path,
    "unit_states": unit_states, "selected_lifecycle": lifecycle_expected,
    "alternative_activation_closure": alternative_activation_closure,
    "merged_unit_sources": merged_unit_sources,
    "runtime_safety_sha256": runtime_hashes, "detached_scope": detached_scope,
    "cgroups": cgroups, "writer_parent_scope_cgroup": writer_parent_scope_cgroup,
    "writer_recovery_leaf": writer_recovery_leaf, "all_cgroups_frozen": True,
    "volatile_control_masks_absent_when_published": True,
    "persistent_control_masks_absent": True,
}
payload = canonical(value)
if event_path.exists() or event_path.is_symlink():
    if event_path.is_symlink() or event_path.read_bytes() != payload: raise SystemExit("pre-mask gate projection changed")
else:
    temporary = event_path.with_name(f".{event_path.name}.partial")
    if temporary.exists() or temporary.is_symlink():
        if temporary.is_symlink() or not temporary.is_file(): raise SystemExit("unsafe pre-mask gate partial")
        temporary.unlink()
    descriptor = os.open(temporary, os.O_WRONLY | os.O_CREAT | os.O_EXCL | getattr(os, "O_NOFOLLOW", 0), 0o400)
    with os.fdopen(descriptor, "wb") as handle:
        handle.write(payload); handle.flush(); os.fsync(handle.fileno())
    os.rename(temporary, event_path)
    directory = os.open(root, os.O_RDONLY | getattr(os, "O_DIRECTORY", 0) | getattr(os, "O_NOFOLLOW", 0))
    try: os.fsync(directory)
    finally: os.close(directory)
PY

    # `/run/systemd/system.control` precedes `/etc/systemd/system` in the unit
    # load path. Use exact high-priority volatile masks and prove PID1's
    # effective LoadState, never ordinary lower-priority runtime masks.
    python3 - "$selected_unit" <<'PY'
import os
import pathlib
import stat
import sys

selected = sys.argv[1]
run_mounts = []
for line in pathlib.Path("/proc/self/mountinfo").read_text(encoding="ascii").splitlines():
    left, separator, right = line.partition(" - ")
    fields = left.split(); right_fields = right.split()
    if separator and len(fields) > 4 and fields[4] == "/run" and right_fields:
        run_mounts.append(right_fields[0])
if run_mounts != ["tmpfs"]:
    raise SystemExit(f"/run is not one exact volatile tmpfs mount: {run_mounts}")
for unit in (
    "arc-self-heal.service", "arc-node.service",
    "arc-node-update.service", "arc-node-update.timer",
):
    for persistent in (
        pathlib.Path(f"/etc/systemd/system.control/{unit}"),
        pathlib.Path(f"/etc/systemd/system.control/{unit}.d"),
    ):
        if persistent.exists() or persistent.is_symlink():
            raise SystemExit(f"persistent systemd control override exists: {unit}")
parent = os.open("/run/systemd", os.O_RDONLY | getattr(os, "O_DIRECTORY", 0) | getattr(os, "O_NOFOLLOW", 0))
try:
    try: details = os.stat("system.control", dir_fd=parent, follow_symlinks=False)
    except FileNotFoundError:
        os.mkdir("system.control", 0o755, dir_fd=parent)
        details = os.stat("system.control", dir_fd=parent, follow_symlinks=False)
    if (not stat.S_ISDIR(details.st_mode) or details.st_uid != 0 or details.st_gid != 0
            or details.st_mode & 0o022): raise SystemExit("systemd control directory is unsafe")
    control = os.open("system.control", os.O_RDONLY | getattr(os, "O_DIRECTORY", 0) | getattr(os, "O_NOFOLLOW", 0), dir_fd=parent)
    try:
        for unit in (
            "arc-self-heal.service", "arc-node.service",
            "arc-node-update.service", "arc-node-update.timer",
        ):
            try: details = os.stat(unit, dir_fd=control, follow_symlinks=False)
            except FileNotFoundError:
                os.symlink("/dev/null", unit, dir_fd=control)
                details = os.stat(unit, dir_fd=control, follow_symlinks=False)
            if not stat.S_ISLNK(details.st_mode) or os.readlink(unit, dir_fd=control) != "/dev/null":
                raise SystemExit(f"systemd control mask differs: {unit}")
        os.fsync(control)
    finally: os.close(control)
    os.fsync(parent)
finally: os.close(parent)
PY
    systemctl daemon-reload
    for unit in arc-self-heal.service arc-node.service arc-node-update.service arc-node-update.timer; do
        [ "$(systemctl show "$unit" --property=LoadState --value)" = masked ] || \
            die "high-priority systemd control mask is ineffective: $unit"
        [ "$(systemctl show "$unit" --property=FragmentPath --value)" = "/run/systemd/system.control/$unit" ] || \
            die "systemd control mask fragment path differs: $unit"
        [ "$(systemctl show "$unit" --property=UnitFileState --value)" = masked-runtime ] || \
            die "systemd control mask is not volatile: $unit"
        case "$(systemctl show "$unit" --property=Job --value)" in ''|0) ;; *) die "masked legacy unit has a pending job: $unit" ;; esac
        if [ "$unit" != "$selected_unit" ]; then
            case "$(systemctl show "$unit" --property=ActiveState --value)" in inactive|failed) ;;
                *) die "masked alternative legacy unit became active: $unit" ;;
            esac
            if [[ "$unit" == *.service ]]; then
                [ "$(systemctl show "$unit" --property=MainPID --value)" = 0 ] || \
                    die "masked alternative service has a MainPID: $unit"
            fi
        fi
    done
    [ "$(systemctl show "$selected_unit" --property=MainPID --value)" = "$selected_main_pid" ] || \
        die "selected supervisor changed under the effective control mask"
    [ "$(systemctl show "$selected_unit" --property=ActiveState --value)" = active ] || \
        die "selected supervisor stopped under the effective control mask"
    case "$(systemctl show "$selected_unit" --property=FreezerState --value)" in running|frozen) ;;
        *) die "selected supervisor has an invalid advisory PID1 freezer state under the control mask" ;;
    esac
    [ "$(systemctl show "$selected_unit" --property=Restart --value)" = no ] || die "selected restart safety changed"
    [ "$(systemctl show "$selected_unit" --property=KillMode --value)" = process ] || die "selected KillMode safety changed"
    [ "$(systemctl show "$selected_unit" --property=SendSIGKILL --value)" = no ] || die "selected SIGKILL safety changed"
    [ "$(systemctl show "$selected_unit" --property=OOMPolicy --value)" = continue ] || die "selected OOM safety changed"
    [ "$(systemctl show "$selected_unit" --property=WatchdogUSec --value)" = 0 ] || die "selected watchdog safety changed"
    [ "$(systemctl show "$selected_unit" --property=RuntimeMaxUSec --value)" = infinity ] || die "selected runtime-limit safety changed"

    python3 - "$root" "$selected_unit" "$selected_main_pid" "$sealed_boot_id" <<'PY'
import hashlib
import json
import os
import pathlib
import re
import stat
import subprocess
import sys

root = pathlib.Path(sys.argv[1]); selected = sys.argv[2]
selected_pid = int(sys.argv[3]); sealed_boot = sys.argv[4]
marker = pathlib.Path("/etc/arc-recovery/legacy-start-allowed")
arm_path = root / "06-restart-barrier-armed.json"
commit_path = root / "07-restart-barrier-committed.json"
units = ("arc-self-heal.service", "arc-node.service", "arc-node-update.service", "arc-node-update.timer")
barrier_bytes = b"[Unit]\nConditionPathExists=/etc/arc-recovery/legacy-start-allowed\n"
service = b"[Unit]\nRefuseManualStart=yes\nRefuseManualStop=yes\nIgnoreOnIsolate=yes\nStopWhenUnneeded=no\nBindsTo=\nPartOf=\nPropagatesStopTo=\nOnFailure=\nOnSuccess=\nFailureAction=none\nSuccessAction=none\nJobTimeoutAction=none\n\n[Service]\nExecReload=\nExecStop=\nExecStopPost=\nRestart=no\nKillMode=process\nSendSIGKILL=no\nSendSIGHUP=no\nOOMPolicy=continue\nWatchdogSec=0\nRuntimeMaxSec=infinity\n"
timer = b"[Unit]\nRefuseManualStart=yes\nRefuseManualStop=yes\nIgnoreOnIsolate=yes\nStopWhenUnneeded=no\nBindsTo=\nPartOf=\nPropagatesStopTo=\nOnFailure=\nOnSuccess=\nFailureAction=none\nSuccessAction=none\nJobTimeoutAction=none\n"
def canonical(value): return (json.dumps(value, sort_keys=True, separators=(",", ":")) + "\n").encode()
def load(path, schema):
    if path.is_symlink() or not path.is_file(): raise SystemExit(f"barrier source is missing: {path.name}")
    raw = path.read_bytes(); value = json.loads(raw)
    if raw != canonical(value) or value.get("schema") != schema: raise SystemExit(f"barrier source differs: {path.name}")
    return value, raw
current_boot = pathlib.Path("/proc/sys/kernel/random/boot_id").read_text().strip()
if current_boot != sealed_boot: raise SystemExit("sealed boot ended before restart-barrier arm")
marker_payload = b"schema=arc.recovery.legacy-start-allow.v1\n"
live_arm_raw = None; live_commit_raw = None
if marker.exists() or marker.is_symlink():
    marker_details = marker.lstat()
    if (marker.is_symlink() or not stat.S_ISREG(marker_details.st_mode) or marker_details.st_uid != 0
            or marker_details.st_gid != 0 or stat.S_IMODE(marker_details.st_mode) != 0o400
            or marker.read_bytes() != marker_payload): raise SystemExit("allow marker differs before arm")
    marker_identity = {
        "device": marker_details.st_dev, "inode": marker_details.st_ino,
        "uid": marker_details.st_uid, "gid": marker_details.st_gid,
        "mode": stat.S_IMODE(marker_details.st_mode), "size": marker_details.st_size,
    }
else:
    live_arm, live_arm_raw = load(
        root / "09-quarantine-restart-arm.json",
        "arc.recovery.quarantine-live-restart-arm.v1",
    )
    live_commit, live_commit_raw = load(
        root / "09-quarantine-restart-committed.json",
        "arc.recovery.quarantine-live-restart-committed.v1",
    )
    source_marker = live_arm.get("allow_marker")
    if (live_commit.get("quarantine_restart_arm_sha256") != hashlib.sha256(live_arm_raw).hexdigest()
            or live_commit.get("allow_marker_absent") is not True
            or live_commit.get("allow_marker_parent_fsynced") is not True
            or live_commit.get("restart_prevented") is not True
            or live_arm.get("sealed_boot_id") != sealed_boot
            or live_arm.get("selected_unit") != selected
            or live_arm.get("selected_main_pid") != selected_pid
            or not isinstance(source_marker, dict)
            or source_marker.get("path") != str(marker)
            or source_marker.get("sha256") != hashlib.sha256(marker_payload).hexdigest()):
        raise SystemExit("live quarantine restart arm cannot source the stop barrier marker")
    marker_identity = {
        key: source_marker[key] for key in ("device", "inode", "uid", "gid", "mode", "size")
    }
sources = {}
schemas = {
    "02-fast-cgroup-freeze-intent.json": "arc.recovery.fast-cgroup-freeze-intent.v1",
    "03-fast-cgroups-frozen.json": "arc.recovery.fast-cgroups-frozen.v1",
    "04-pre-fence-quiesce-intent.json": "arc.recovery.pre-fence-quiesce-intent.v1",
    "05-cgroups-frozen.json": "arc.recovery.cgroups-frozen.v1",
    "08-network-quarantine.json": "arc.recovery.legacy-network-quarantine.v1",
    "06-pre-mask-activation-gate.json": "arc.recovery.pre-mask-activation-gate.v1",
    "stop.intent.json": "arc.recovery.stop-intent.v1",
}
values = {}
for name, schema in schemas.items():
    values[name], raw = load(root / name, schema); sources[name] = hashlib.sha256(raw).hexdigest()
if live_arm_raw is not None:
    if live_arm.get("freeze_plan_sha256") != values["02-fast-cgroup-freeze-intent.json"].get("freeze_plan_sha256"):
        raise SystemExit("live quarantine restart arm freeze plan differs from stop sources")
    sources["09-quarantine-restart-arm.json"] = hashlib.sha256(live_arm_raw).hexdigest()
    sources["09-quarantine-restart-committed.json"] = hashlib.sha256(live_commit_raw).hexdigest()
progress_hashes = values["03-fast-cgroups-frozen.json"].get("per_cgroup_progress_sha256")
if not isinstance(progress_hashes, dict) or not progress_hashes:
    raise SystemExit("fast-freeze per-cgroup progress inventory is missing before arm")
progress_files = {
    "supervisor": ("02-supervisor-cgroup-frozen.json", "arc.recovery.fast-cgroup-progress.v1"),
    "writer-parent": ("02-writer-parent-cgroup-frozen.json", "arc.recovery.fast-cgroup-progress.v1"),
    "writer-leaf-move-intent": ("02-writer-leaf-move-intent.json", "arc.recovery.detached-writer-leaf-move-intent.v1"),
    "writer": ("02-writer-cgroup-frozen.json", "arc.recovery.fast-cgroup-progress.v1"),
    "writer-parent-release": ("02-writer-parent-released.json", "arc.recovery.detached-writer-parent-release.v1"),
}
for role, expected_sha in progress_hashes.items():
    if role not in progress_files: raise SystemExit(f"unreviewed fast-freeze progress role: {role}")
    progress_name, progress_schema = progress_files[role]
    progress, progress_raw = load(root / progress_name, progress_schema)
    if hashlib.sha256(progress_raw).hexdigest() != expected_sha:
        raise SystemExit(f"fast-freeze per-cgroup progress differs: {role}")
    sources[progress_name] = expected_sha
contract, contract_raw = load(root / "evidence" / "writer-contract.json", "arc.recovery.exact-writer.v3")
sources["evidence/writer-contract.json"] = hashlib.sha256(contract_raw).hexdigest()
freeze_sha = contract.get("freeze_plan_sha256")
if (not isinstance(freeze_sha, str) or len(freeze_sha) != 64
        or any(values[name].get("freeze_plan_sha256") != freeze_sha for name in schemas)):
    raise SystemExit("barrier sources are not bound to one freeze plan")
pre_mask_gate = values["06-pre-mask-activation-gate.json"]
def verify_pre_mask_sources():
    manifests = pre_mask_gate.get("merged_unit_sources")
    if not isinstance(manifests, dict) or set(manifests) != set(units):
        raise SystemExit("pre-mask merged-source manifest is missing before arm")
    for unit, rows in manifests.items():
        if not isinstance(rows, list) or not rows:
            raise SystemExit(f"pre-mask merged-source manifest is empty: {unit}")
        expected_paths = set()
        for row in rows:
            if (not isinstance(row, dict) or set(row) != {"path", "sha256"}
                    or not re.fullmatch(r"[0-9a-f]{64}", row.get("sha256", ""))):
                raise SystemExit(f"pre-mask merged-source row is malformed: {unit}")
            source = pathlib.Path(row["path"]); details = source.lstat()
            if (source.is_symlink() or not stat.S_ISREG(details.st_mode)
                    or details.st_uid != 0 or details.st_gid != 0 or details.st_mode & 0o022
                    or hashlib.sha256(source.read_bytes()).hexdigest() != row["sha256"]):
                raise SystemExit(f"pre-mask merged source changed after sealing: {source}")
            if row["path"] in expected_paths:
                raise SystemExit(f"pre-mask merged-source manifest is duplicated: {source}")
            expected_paths.add(row["path"])
        actual_dropins = set()
        unit_path = pre_mask_gate.get("unit_path")
        if not isinstance(unit_path, list) or not unit_path or any(not isinstance(path, str) for path in unit_path):
            raise SystemExit("pre-mask systemd UnitPath contract is malformed")
        for unit_root in unit_path:
            if unit_root.endswith("/system.control"): continue
            directory = pathlib.Path(unit_root) / f"{unit}.d"
            if not directory.exists(): continue
            if directory.is_symlink() or not directory.is_dir():
                raise SystemExit(f"systemd drop-in directory changed after sealing: {directory}")
            for entry in directory.glob("*.conf"):
                if entry.is_symlink() or not entry.is_file():
                    raise SystemExit(f"systemd drop-in changed after sealing: {entry}")
                actual_dropins.add(str(entry))
        expected_dropins = {path for path in expected_paths if pathlib.PurePosixPath(path).parent.name == f"{unit}.d"}
        if actual_dropins != expected_dropins:
            raise SystemExit(f"systemd drop-in set changed after pre-mask sealing: {unit}")
        for control_override in (
            pathlib.Path(f"/etc/systemd/system.control/{unit}"),
            pathlib.Path(f"/etc/systemd/system.control/{unit}.d"),
            pathlib.Path(f"/run/systemd/system.control/{unit}.d"),
        ):
            if control_override.exists() or control_override.is_symlink():
                raise SystemExit(f"unreviewed systemd control override exists: {control_override}")
verify_pre_mask_sources()
frozen = values["05-cgroups-frozen.json"]
cgroups = frozen.get("cgroups")
if not isinstance(cgroups, list) or not cgroups or frozen.get("all_cgroups_frozen") is not True:
    raise SystemExit("frozen cgroup contract differs")
def subtree(entry):
    base = pathlib.Path("/sys/fs/cgroup") / entry["path"].lstrip("/")
    details = base.stat()
    if base.is_symlink() or details.st_dev != entry["device"] or details.st_ino != entry["inode"]:
        raise SystemExit("frozen cgroup path/inode differs before arm")
    events = dict(line.split(" ", 1) for line in base.joinpath("cgroup.events").read_text().splitlines())
    if events.get("frozen") != "1": raise SystemExit("cgroup thawed before barrier arm")
    pids = set()
    for current, dirs, _files in os.walk(base, followlinks=False):
        dirs.sort(); current_path = pathlib.Path(current)
        if current_path.is_symlink(): raise SystemExit("frozen cgroup subtree is unsafe")
        procs = current_path / "cgroup.procs"
        if procs.is_symlink() or not procs.is_file(): raise SystemExit("frozen cgroup inventory is unsafe")
        pids.update(int(value) for value in procs.read_text().splitlines())
    return sorted(pids)
for entry in cgroups:
    expected = sorted(member["pid"] for member in frozen["post_freeze_members"][entry["role"]])
    if subtree(entry) != expected: raise SystemExit("frozen cgroup membership changed before arm")
writer_parent_scope_cgroup = frozen.get("writer_parent_scope_cgroup")
writer_recovery_leaf = frozen.get("writer_recovery_leaf")
for source_name in ("03-fast-cgroups-frozen.json", "04-pre-fence-quiesce-intent.json"):
    if (values[source_name].get("writer_parent_scope_cgroup") != writer_parent_scope_cgroup
            or values[source_name].get("writer_recovery_leaf") != writer_recovery_leaf):
        raise SystemExit(f"writer parent/leaf identity differs before arm: {source_name}")
if writer_recovery_leaf is not None:
    leaf_path = pathlib.Path("/sys/fs/cgroup") / writer_recovery_leaf["path"].lstrip("/")
    leaf_events = dict(line.split(" ", 1) for line in leaf_path.joinpath("cgroup.events").read_text().splitlines())
    if (writer_recovery_leaf not in cgroups
            or leaf_path.joinpath("cgroup.freeze").read_text(encoding="ascii").strip() != "1"
            or leaf_events.get("frozen") != "1" or leaf_events.get("populated") != "1"
            or subtree(writer_recovery_leaf) != [contract["writer_pid"]]):
        raise SystemExit("detached writer leaf lost local freeze/sole membership before arm")
def prop(unit, name):
    return subprocess.check_output(["systemctl", "show", unit, f"--property={name}", "--value"], text=True).strip()
def proc_fields(pid):
    raw = pathlib.Path(f"/proc/{pid}/stat").read_text(encoding="ascii")
    end = raw.rfind(")"); fields = raw[end + 2:].split()
    if end < 0 or len(fields) < 20: raise SystemExit("process stat is truncated before barrier arm")
    return fields
def proc_start(pid): return int(proc_fields(pid)[19])
def term_pending(pid):
    bit = 1 << 14
    tasks = sorted(pathlib.Path(f"/proc/{pid}/task").iterdir(), key=lambda value: int(value.name))
    if not tasks: raise SystemExit("sealed process has no task inventory before barrier arm")
    for task in tasks:
        values = {}
        for line in task.joinpath("status").read_text(encoding="ascii").splitlines():
            key, separator, value = line.partition(":")
            if separator and key in {"SigIgn", "SigPnd", "ShdPnd", "TracerPid"}:
                values[key] = value.strip()
        if set(values) != {"SigIgn", "SigPnd", "ShdPnd", "TracerPid"} or values["TracerPid"] != "0":
            raise SystemExit("sealed task signal inventory changed before barrier arm")
        if any(int(values[key], 16) & bit for key in ("SigIgn", "SigPnd", "ShdPnd")):
            raise SystemExit("sealed task gained ignored or pending SIGTERM before barrier commit")
writer_roles = [entry for entry in cgroups if entry.get("role") == "writer"]
fast_intent = values["02-fast-cgroup-freeze-intent.json"]
scope_contract = fast_intent.get("writer", {})
scope_expected = None
if writer_roles:
    if not isinstance(writer_parent_scope_cgroup, dict) or writer_recovery_leaf != writer_roles[0]:
        raise SystemExit("detached writer parent/leaf provenance is missing before arm")
    writer_scope = pathlib.PurePosixPath(writer_parent_scope_cgroup["path"]).name
    scope_path = pathlib.Path(scope_contract.get("scope_runtime_safety_path", ""))
    scope_sources = scope_contract.get("scope_runtime_sources")
    sealed_scope_properties = scope_contract.get("scope_properties")
    try: scope_path_details = scope_path.lstat()
    except FileNotFoundError: raise SystemExit("detached writer scope safety disappeared before arm")
    if (scope_contract.get("scope_unit") != writer_scope
            or not re.fullmatch(r"[0-9a-f]{32}", scope_contract.get("scope_invocation_id", ""))
            or not isinstance(scope_sources, list) or not scope_sources
            or not isinstance(sealed_scope_properties, dict) or not sealed_scope_properties
            or scope_path != pathlib.Path(f"/run/systemd/system.control/{writer_scope}.d/zzzy-arc-recovery-writer-scope-safety.conf")
            or scope_path.is_symlink() or not stat.S_ISREG(scope_path_details.st_mode)
            or scope_path_details.st_uid != 0 or scope_path_details.st_gid != 0
            or scope_path_details.st_mode & 0o022
            or hashlib.sha256(scope_path.read_bytes()).hexdigest() != scope_contract.get("scope_runtime_safety_sha256")):
        raise SystemExit("detached writer scope safety contract differs before arm")
    for persistent in (
        pathlib.Path(f"/etc/systemd/system.control/{writer_scope}"),
        pathlib.Path(f"/etc/systemd/system.control/{writer_scope}.d"),
    ):
        if persistent.exists() or persistent.is_symlink():
            raise SystemExit("persistent high-priority detached scope override exists before arm")
    scope_expected = dict(sealed_scope_properties)
    if (scope_expected.get("InvocationID") != scope_contract["scope_invocation_id"]
            or scope_expected.get("ControlGroup") != writer_parent_scope_cgroup["path"]):
        raise SystemExit("detached writer scope identity contract differs before arm")
    parent_path = pathlib.Path("/sys/fs/cgroup") / writer_parent_scope_cgroup["path"].lstrip("/")
    parent_details = parent_path.lstat()
    if (parent_path.is_symlink() or not parent_path.is_dir()
            or parent_details.st_dev != writer_parent_scope_cgroup["device"]
            or parent_details.st_ino != writer_parent_scope_cgroup["inode"]):
        raise SystemExit("detached writer parent scope inode changed before arm")
    gate_scope = pre_mask_gate.get("detached_scope")
    if (not isinstance(gate_scope, dict)
            or gate_scope.get("unit") != writer_scope
            or gate_scope.get("properties") != scope_expected
            or gate_scope.get("runtime_safety_path") != str(scope_path)
            or gate_scope.get("runtime_safety_sha256") != scope_contract.get("scope_runtime_safety_sha256")
            or gate_scope.get("sources") != scope_sources
            or gate_scope.get("parent_state") not in {"active-sealed", "terminal-after-leaf-seal"}):
        raise SystemExit("pre-mask detached parent-state contract differs before arm")

    def verify_scope_parent_projection(prior_state=None):
        active = prop(writer_scope, "ActiveState") == "active"
        if active:
            if prior_state == "terminal-after-leaf-seal":
                raise SystemExit("detached writer scope reactivated after terminal state")
            current_sources = []
            for header in re.findall(rb"(?m)^# (/[^\n]+)$", subprocess.check_output(["systemctl", "cat", writer_scope])):
                source = pathlib.Path(header.decode("utf-8")); details = source.lstat()
                if (source.is_symlink() or not stat.S_ISREG(details.st_mode)
                        or details.st_uid != 0 or details.st_gid != 0 or details.st_mode & 0o022):
                    raise SystemExit("detached writer scope source is unsafe before arm")
                current_sources.append({"path": str(source), "sha256": hashlib.sha256(source.read_bytes()).hexdigest()})
            if (current_sources != scope_sources
                    or any((prop(writer_scope, key) or ("0" if key == "Job" else "")) != wanted
                           for key, wanted in scope_expected.items())
                    or prop(writer_scope, "Names") != writer_scope
                    or prop(writer_scope, "ControlGroup") != writer_parent_scope_cgroup["path"]
                    or prop(writer_scope, "FreezerState") not in {"running", "frozen"}
                    or prop(writer_scope, "Job") not in {"", "0"}):
                raise SystemExit("detached writer active scope projection differs before arm")
            return "active-sealed"
        if (prop(writer_scope, "ActiveState") not in {"inactive", "failed"}
                or prop(writer_scope, "MainPID") not in {"", "0"}
                or prop(writer_scope, "Job") not in {"", "0"}
                or prop(writer_scope, "InvocationID") not in {"", scope_contract["scope_invocation_id"]}
                or prop(writer_scope, "ControlGroup") not in {"", writer_parent_scope_cgroup["path"]}):
            raise SystemExit("detached writer terminal scope is not provenance-safe before arm")
        return "terminal-after-leaf-seal"

    observed_parent_state = verify_scope_parent_projection(gate_scope["parent_state"])
    scope_parent_state = observed_parent_state
    if arm_path.exists() or arm_path.is_symlink():
        if arm_path.is_symlink() or not arm_path.is_file():
            raise SystemExit("restart-barrier arm is unsafe")
        prior_arm_raw = arm_path.read_bytes(); prior_arm = json.loads(prior_arm_raw)
        prior_scope = prior_arm.get("detached_scope_safety") if isinstance(prior_arm, dict) else None
        recorded_state = prior_scope.get("parent_state") if isinstance(prior_scope, dict) else None
        if (prior_arm_raw != canonical(prior_arm)
                or prior_arm.get("schema") != "arc.recovery.restart-barrier-arm.v1"
                or recorded_state not in {"active-sealed", "terminal-after-leaf-seal"}
                or (recorded_state == "terminal-after-leaf-seal" and observed_parent_state != recorded_state)):
            raise SystemExit("durable arm detached parent-state differs")
        scope_parent_state = recorded_state
if (prop(selected, "LoadState") != "masked"
        or prop(selected, "FragmentPath") != f"/run/systemd/system.control/{selected}"
        or prop(selected, "UnitFileState") != "masked-runtime"
        or prop(selected, "MainPID") != str(selected_pid)
        or prop(selected, "ActiveState") != "active"
        or prop(selected, "FreezerState") not in {"running", "frozen"}
        or prop(selected, "Job") not in {"", "0"}): raise SystemExit("selected masked/frozen state differs before arm")
selected_safety = {
    "Restart": "no", "KillMode": "process", "SendSIGKILL": "no", "SendSIGHUP": "no",
    "IgnoreOnIsolate": "yes",
    "OOMPolicy": "continue", "WatchdogUSec": "0", "RuntimeMaxUSec": "infinity",
    "CanReload": "no", "ExecReload": "", "ExecStop": "", "ExecStopPost": "",
    "OnFailure": "", "OnSuccess": "", "SuccessAction": "none",
    "FailureAction": "none", "JobTimeoutAction": "none",
}
if any(prop(selected, name) != wanted for name, wanted in selected_safety.items()):
    raise SystemExit("selected lifecycle safety differs before arm")
sealed_invocation_id = values["04-pre-fence-quiesce-intent.json"].get("supervisor_invocation_id")
if prop(selected, "InvocationID") != sealed_invocation_id:
    raise SystemExit("selected supervisor invocation changed before arm")
if (proc_start(selected_pid) != contract["supervisor_start_ticks"]
        or proc_start(contract["writer_pid"]) != contract["writer_start_ticks"]):
    raise SystemExit("sealed process PID/start changed before barrier arm")
term_pending(selected_pid)
if contract["writer_pid"] != selected_pid: term_pending(contract["writer_pid"])
barriers = {}; runtime = {}; masks = {}
for unit in units:
    barrier = pathlib.Path(f"/etc/systemd/system/{unit}.d/zzzz-arc-recovery-freeze.conf")
    expected_runtime = timer if unit.endswith(".timer") else service
    runtime_path = pathlib.Path(f"/run/systemd/system/{unit}.d/zzzy-arc-recovery-prefreeze-safety.conf")
    mask = pathlib.Path(f"/run/systemd/system.control/{unit}")
    if (barrier.is_symlink() or barrier.read_bytes() != barrier_bytes
            or barrier.lstat().st_mode & 0o222): raise SystemExit(f"persistent barrier differs: {unit}")
    if runtime_path.is_symlink() or runtime_path.read_bytes() != expected_runtime: raise SystemExit(f"runtime safety differs: {unit}")
    if not mask.is_symlink() or os.readlink(mask) != "/dev/null" or prop(unit, "LoadState") != "masked":
        raise SystemExit(f"effective control mask differs: {unit}")
    if (prop(unit, "FragmentPath") != f"/run/systemd/system.control/{unit}"
            or prop(unit, "UnitFileState") != "masked-runtime"):
        raise SystemExit(f"effective control mask is not the exact volatile fragment: {unit}")
    if unit != selected:
        if (prop(unit, "ActiveState") not in {"inactive", "failed"}
                or (unit.endswith(".service") and prop(unit, "MainPID") != "0")):
            raise SystemExit(f"alternative unit activated before arm: {unit}")
    masks[unit] = "/dev/null"
    if prop(unit, "Job") not in {"", "0"}: raise SystemExit(f"legacy unit has a job: {unit}")
    barriers[unit] = hashlib.sha256(barrier_bytes).hexdigest()
    runtime[unit] = hashlib.sha256(expected_runtime).hexdigest()
arm = {
    "schema": "arc.recovery.restart-barrier-arm.v1",
    "freeze_plan_sha256": freeze_sha, "sealed_boot_id": sealed_boot,
    "selected_unit": selected, "selected_main_pid": selected_pid,
    "allow_marker_path": str(marker), "allow_marker_sha256": hashlib.sha256(marker_payload).hexdigest(),
    "allow_marker_identity": marker_identity,
    "allow_marker_observed_present": True, "source_sha256": sources,
    "persistent_start_barrier_sha256": barriers, "runtime_safety_sha256": runtime,
    "control_masks": masks, "effective_control_masks": True,
    "pre_mask_activation_gate_sha256": sources["06-pre-mask-activation-gate.json"],
    "prepare_barrier_sha256": values["06-pre-mask-activation-gate.json"]["prepare_barrier_sha256"],
    "detached_scope_safety": None if scope_expected is None else {
        "unit": writer_scope, "properties": scope_expected,
        "parent_state": scope_parent_state,
        "sources": scope_sources,
        "runtime_safety_sha256": scope_contract["scope_runtime_safety_sha256"],
        "parent_scope_cgroup": writer_parent_scope_cgroup,
        "recovery_leaf": writer_recovery_leaf,
    },
    "cgroups": cgroups, "writer_parent_scope_cgroup": writer_parent_scope_cgroup,
    "writer_recovery_leaf": writer_recovery_leaf, "all_cgroups_frozen": True,
}
payload = canonical(arm)
if arm_path.exists() or arm_path.is_symlink():
    if arm_path.is_symlink() or arm_path.read_bytes() != payload: raise SystemExit("restart-barrier arm differs")
else:
    temporary = arm_path.with_name(f".{arm_path.name}.partial")
    if temporary.exists() or temporary.is_symlink():
        if temporary.is_symlink() or not temporary.is_file(): raise SystemExit("unsafe arm partial")
        temporary.unlink()
    descriptor = os.open(temporary, os.O_WRONLY | os.O_CREAT | os.O_EXCL | getattr(os, "O_NOFOLLOW", 0), 0o400)
    with os.fdopen(descriptor, "wb") as handle:
        handle.write(payload); handle.flush(); os.fsync(handle.fileno())
    os.rename(temporary, arm_path)
    directory = os.open(root, os.O_RDONLY | getattr(os, "O_DIRECTORY", 0) | getattr(os, "O_NOFOLLOW", 0))
    try: os.fsync(directory)
    finally: os.close(directory)
# Revalidate the exact frozen/masked state after the durable arm, then make the
# single commit mutation through a sealed parent dirfd and fsync it.
verify_pre_mask_sources()
for entry in cgroups: subtree(entry)
if (prop(selected, "FreezerState") not in {"running", "frozen"} or prop(selected, "Job") not in {"", "0"}
        or prop(selected, "InvocationID") != sealed_invocation_id
        or any(prop(selected, name) != wanted for name, wanted in selected_safety.items())):
    raise SystemExit("selected state changed after durable barrier arm")
term_pending(selected_pid)
if contract["writer_pid"] != selected_pid: term_pending(contract["writer_pid"])
if writer_roles:
    writer_scope = pathlib.PurePosixPath(writer_parent_scope_cgroup["path"]).name
    leaf_path = pathlib.Path("/sys/fs/cgroup") / writer_recovery_leaf["path"].lstrip("/")
    leaf_events = dict(line.split(" ", 1) for line in leaf_path.joinpath("cgroup.events").read_text().splitlines())
    verify_scope_parent_projection(scope_parent_state)
    if (leaf_path.joinpath("cgroup.freeze").read_text(encoding="ascii").strip() != "1"
            or leaf_events.get("frozen") != "1" or leaf_events.get("populated") != "1"
            or subtree(writer_recovery_leaf) != [contract["writer_pid"]]):
        raise SystemExit("detached writer scope changed after durable barrier arm")
for masked_unit in units:
    mask = pathlib.Path(f"/run/systemd/system.control/{masked_unit}")
    if (not mask.is_symlink() or os.readlink(mask) != "/dev/null"
            or prop(masked_unit, "LoadState") != "masked"
            or prop(masked_unit, "FragmentPath") != f"/run/systemd/system.control/{masked_unit}"
            or prop(masked_unit, "UnitFileState") != "masked-runtime"
            or prop(masked_unit, "Job") not in {"", "0"}):
        raise SystemExit(f"activation gate changed after durable arm: {masked_unit}")
    if (masked_unit != selected and (
            prop(masked_unit, "ActiveState") not in {"inactive", "failed"}
            or (masked_unit.endswith(".service") and prop(masked_unit, "MainPID") != "0"))):
        raise SystemExit(f"alternative activation source changed after durable arm: {masked_unit}")
parent = os.open("/etc/arc-recovery", os.O_RDONLY | getattr(os, "O_DIRECTORY", 0) | getattr(os, "O_NOFOLLOW", 0))
try:
    if live_arm_raw is None:
        opened = os.open("legacy-start-allowed", os.O_RDONLY | getattr(os, "O_NOFOLLOW", 0), dir_fd=parent)
        try:
            opened_details = os.fstat(opened)
            named_details = os.stat("legacy-start-allowed", dir_fd=parent, follow_symlinks=False)
            expected_identity = arm["allow_marker_identity"]
            observed_identity = {
                "device": opened_details.st_dev, "inode": opened_details.st_ino,
                "uid": opened_details.st_uid, "gid": opened_details.st_gid,
                "mode": stat.S_IMODE(opened_details.st_mode), "size": opened_details.st_size,
            }
            if (not stat.S_ISREG(opened_details.st_mode) or observed_identity != expected_identity
                    or named_details.st_dev != opened_details.st_dev
                    or named_details.st_ino != opened_details.st_ino
                    or os.read(opened, len(marker_payload) + 1) != marker_payload):
                raise SystemExit("allow marker identity/bytes changed before unlink")
            final_named = os.stat("legacy-start-allowed", dir_fd=parent, follow_symlinks=False)
            if final_named.st_dev != opened_details.st_dev or final_named.st_ino != opened_details.st_ino:
                raise SystemExit("allow marker pathname changed before unlink")
        finally: os.close(opened)
        os.unlink("legacy-start-allowed", dir_fd=parent)
    else:
        try: os.stat("legacy-start-allowed", dir_fd=parent, follow_symlinks=False)
        except FileNotFoundError: pass
        else: raise SystemExit("allow marker reappeared after the live quarantine commit")
    try: os.stat("legacy-start-allowed", dir_fd=parent, follow_symlinks=False)
    except FileNotFoundError: pass
    else: raise SystemExit("allow marker pathname remains after unlink")
    os.fsync(parent)
    try: os.stat("legacy-start-allowed", dir_fd=parent, follow_symlinks=False)
    except FileNotFoundError: pass
    else: raise SystemExit("allow marker reappeared after parent fsync")
finally: os.close(parent)
etc = os.open("/etc", os.O_RDONLY | getattr(os, "O_DIRECTORY", 0) | getattr(os, "O_NOFOLLOW", 0))
try: os.fsync(etc)
finally: os.close(etc)
commit = {
    "schema": "arc.recovery.restart-barrier-committed.v2",
    "barrier_arm_sha256": hashlib.sha256(payload).hexdigest(),
    "sealed_boot_id": sealed_boot, "observed_boot_id": current_boot,
    "allow_marker_path": str(marker), "allow_marker_absent": True,
    "unlink_parent_fsynced": True, "durability_basis": (
        "same-boot-preproof-unlink-parent-fsynced" if live_arm_raw is not None
        else "same-boot-unlink-parent-fsynced"
    ),
    "selected_unit": selected, "selected_main_pid_on_sealed_boot": selected_pid,
    "reboot_requires_zero_pid_signals": False,
}
commit_payload = canonical(commit)
if commit_path.exists() or commit_path.is_symlink():
    if commit_path.is_symlink() or commit_path.read_bytes() != commit_payload: raise SystemExit("restart-barrier commit differs")
else:
    temporary = commit_path.with_name(f".{commit_path.name}.partial")
    if temporary.exists() or temporary.is_symlink():
        if temporary.is_symlink() or not temporary.is_file(): raise SystemExit("unsafe commit partial")
        temporary.unlink()
    descriptor = os.open(temporary, os.O_WRONLY | os.O_CREAT | os.O_EXCL | getattr(os, "O_NOFOLLOW", 0), 0o400)
    with os.fdopen(descriptor, "wb") as handle:
        handle.write(commit_payload); handle.flush(); os.fsync(handle.fileno())
    os.rename(temporary, commit_path)
    directory = os.open(root, os.O_RDONLY | getattr(os, "O_DIRECTORY", 0) | getattr(os, "O_NOFOLLOW", 0))
    try: os.fsync(directory)
    finally: os.close(directory)
PY
}

verify_legacy_restart_fence() {
    local root="$1"
    python3 - "$root" <<'PY'
import hashlib
import json
import os
import pathlib
import subprocess
import stat
import sys

root = pathlib.Path(sys.argv[1])
marker = pathlib.Path("/etc/arc-recovery/legacy-start-allowed")
expected = b"[Unit]\nConditionPathExists=/etc/arc-recovery/legacy-start-allowed\n"
units = ("arc-self-heal.service", "arc-node.service", "arc-node-update.service", "arc-node-update.timer")
def canonical(value): return (json.dumps(value, sort_keys=True, separators=(",", ":")) + "\n").encode()
def load(path, schema):
    if path.is_symlink() or not path.is_file(): raise SystemExit(f"restart-fence evidence missing: {path.name}")
    raw = path.read_bytes(); value = json.loads(raw)
    if raw != canonical(value) or value.get("schema") != schema: raise SystemExit(f"restart-fence evidence differs: {path.name}")
    return value, raw
def prop(unit, name):
    return subprocess.check_output(["systemctl", "show", unit, f"--property={name}", "--value"], text=True).strip()
if marker.exists() or marker.is_symlink():
    raise SystemExit("legacy-start authorization marker exists")
arm, arm_raw = load(root / "06-restart-barrier-armed.json", "arc.recovery.restart-barrier-arm.v1")
commit, _ = load(root / "07-restart-barrier-committed.json", "arc.recovery.restart-barrier-committed.v2")
if (commit.get("barrier_arm_sha256") != hashlib.sha256(arm_raw).hexdigest()
        or commit.get("allow_marker_absent") is not True
        or arm.get("control_masks") != {unit: "/dev/null" for unit in units}):
    raise SystemExit("restart-fence arm/commit chain differs")
for unit in units:
    for persistent_control in (
        pathlib.Path(f"/etc/systemd/system.control/{unit}"),
        pathlib.Path(f"/etc/systemd/system.control/{unit}.d"),
    ):
        if persistent_control.exists() or persistent_control.is_symlink():
            raise SystemExit(f"persistent systemd control override exists: {unit}")
    path = pathlib.Path(f"/etc/systemd/system/{unit}.d/zzzz-arc-recovery-freeze.conf")
    details = path.lstat()
    if (path.is_symlink() or not stat.S_ISREG(details.st_mode)
            or details.st_uid != 0 or details.st_gid != 0 or details.st_mode & 0o222
            or path.read_bytes() != expected):
        raise SystemExit(f"persistent condition-only start barrier differs: {unit}")
    for directory in (
        pathlib.Path(f"/etc/systemd/system/{unit}.d"),
        pathlib.Path(f"/run/systemd/system/{unit}.d"),
        pathlib.Path(f"/usr/local/lib/systemd/system/{unit}.d"),
        pathlib.Path(f"/usr/lib/systemd/system/{unit}.d"),
    ):
        if not directory.exists(): continue
        if directory.is_symlink() or not directory.is_dir():
            raise SystemExit(f"systemd drop-in directory is unsafe: {directory}")
        for entry in directory.glob("*.conf"):
            if entry.is_symlink() or not entry.is_file():
                raise SystemExit(f"systemd drop-in is unsafe: {entry}")
            if entry.name > path.name:
                raise SystemExit(f"persistent start barrier is overrideable: {unit} {entry}")
observed_boot = pathlib.Path("/proc/sys/kernel/random/boot_id").read_text().strip()
if observed_boot == arm.get("sealed_boot_id"):
    for unit in units:
        mask = pathlib.Path(f"/run/systemd/system.control/{unit}")
        if (not mask.is_symlink() or os.readlink(mask) != "/dev/null"
                or prop(unit, "LoadState") != "masked"
                or prop(unit, "FragmentPath") != f"/run/systemd/system.control/{unit}"
                or prop(unit, "UnitFileState") != "masked-runtime"
                or prop(unit, "Job") not in {"", "0"}):
            raise SystemExit(f"same-boot volatile control mask differs: {unit}")
else:
    for unit in units:
        mask = pathlib.Path(f"/run/systemd/system.control/{unit}")
        if mask.exists() or mask.is_symlink():
            raise SystemExit(f"sealed-boot volatile control mask survived reboot: {unit}")
        load_state = prop(unit, "LoadState")
        if load_state == "not-found": continue
        merged = subprocess.check_output(["systemctl", "cat", unit], text=True)
        conditions = []; section = None
        for raw_line in merged.splitlines():
            line = raw_line.strip()
            if not line or line.startswith(("#", ";")): continue
            if line.startswith("[") and line.endswith("]"):
                section = line[1:-1].strip(); continue
            if section != "Unit" or "=" not in line: continue
            key, value = (item.strip() for item in line.split("=", 1))
            if key == "ConditionPathExists":
                if value == "": conditions.clear()
                else: conditions.append(value)
        if "/etc/arc-recovery/legacy-start-allowed" not in conditions:
            raise SystemExit(f"merged persistent start condition was reset: {unit}")
PY
}

record_committed_restart_barrier() {
    local root="$1" selected_unit="$2" selected_main_pid="$3"
    verify_legacy_restart_fence "$root"
    python3 - "$root" "$selected_unit" "$selected_main_pid" <<'PY'
import hashlib
import json
import os
import pathlib
import sys

root = pathlib.Path(sys.argv[1]); selected = sys.argv[2]; selected_pid = int(sys.argv[3])
arm_path = root / "06-restart-barrier-armed.json"
commit_path = root / "07-restart-barrier-committed.json"
evidence_path = root / "evidence" / "legacy-service-fence.json"
verified_path = root / "10-fence-verified.json"
def canonical(value): return (json.dumps(value, sort_keys=True, separators=(",", ":")) + "\n").encode()
def load(path, schema):
    if path.is_symlink() or not path.is_file(): raise SystemExit(f"restart-barrier evidence missing: {path.name}")
    raw = path.read_bytes(); value = json.loads(raw)
    if raw != canonical(value) or value.get("schema") != schema: raise SystemExit(f"restart-barrier evidence differs: {path.name}")
    return value, raw
arm, arm_raw = load(arm_path, "arc.recovery.restart-barrier-arm.v1")
commit, commit_raw = load(commit_path, "arc.recovery.restart-barrier-committed.v2")
if (arm.get("selected_unit") != selected or arm.get("selected_main_pid") != selected_pid
        or arm.get("all_cgroups_frozen") is not True
        or arm.get("control_masks") != {
            unit: "/dev/null" for unit in (
                "arc-self-heal.service", "arc-node.service",
                "arc-node-update.service", "arc-node-update.timer",
            )
        }
        or commit.get("barrier_arm_sha256") != hashlib.sha256(arm_raw).hexdigest()
        or commit.get("allow_marker_absent") is not True
        or commit.get("unlink_parent_fsynced") is not True):
    raise SystemExit("restart-barrier arm/commit chain differs")
legacy = {
    "schema": "arc.recovery.legacy-service-fence.v5",
    "freeze_plan_sha256": arm["freeze_plan_sha256"],
    "barrier_arm_sha256": hashlib.sha256(arm_raw).hexdigest(),
    "barrier_commit_sha256": hashlib.sha256(commit_raw).hexdigest(),
    "selected_unit": selected, "selected_main_pid_on_sealed_boot": selected_pid,
    "persistent_condition_only_start_barriers": True,
    "allow_marker_absent": True, "all_cgroups_frozen_at_commit": True,
    "effective_four_unit_control_masks_on_sealed_boot": True,
    "recovery_sigkill_allowed": False,
}
verified = {
    "schema": "arc.recovery.fence-verified.v3",
    "freeze_plan_sha256": arm["freeze_plan_sha256"],
    "barrier_arm_sha256": hashlib.sha256(arm_raw).hexdigest(),
    "barrier_commit_sha256": hashlib.sha256(commit_raw).hexdigest(),
    "legacy_service_fence_sha256": hashlib.sha256(canonical(legacy)).hexdigest(),
    "persistent_condition_only_start_barriers": True,
    "allow_marker_absent": True, "recovery_sigkill_allowed": False,
}
for path, value in ((evidence_path, legacy), (verified_path, verified)):
    payload = canonical(value)
    if path.exists() or path.is_symlink():
        if path.is_symlink() or path.read_bytes() != payload: raise SystemExit(f"fence publication differs: {path.name}")
        continue
    temporary = path.with_name(f".{path.name}.partial")
    if temporary.exists() or temporary.is_symlink():
        if temporary.is_symlink() or not temporary.is_file(): raise SystemExit("unsafe fence publication partial")
        temporary.unlink()
    descriptor = os.open(temporary, os.O_WRONLY | os.O_CREAT | os.O_EXCL | getattr(os, "O_NOFOLLOW", 0), 0o400)
    with os.fdopen(descriptor, "wb") as handle:
        handle.write(payload); handle.flush(); os.fsync(handle.fileno())
    os.rename(temporary, path)
    directory = os.open(path.parent, os.O_RDONLY | getattr(os, "O_DIRECTORY", 0) | getattr(os, "O_NOFOLLOW", 0))
    try: os.fsync(directory)
    finally: os.close(directory)
PY
}

stop_node_cleanly() {
    local root="$1" writer_pid="$2" writer_start_ticks="$3" boot_id="$4"
    local writer_cgroup_sha="$5" writer_executable_path="$6"
    local writer_executable_sha="$7" writer_argv_sha="$8"
    local supervisor_pid="$9" supervisor_start_ticks="${10}" supervisor_unit="${11}"
    local supervisor_executable_path="${12}" supervisor_executable_sha="${13}"
    local supervisor_argv_sha="${14}"
    local capture_id="${15}" node="${16}" freeze_sha="${17}"
    require_commands pgrep systemctl sync python3 find sort tail
    # This verifier is intentionally the first state-sensitive operation in
    # the stop controller.  No barrier commit, pidfd signal, or cgroup thaw is
    # allowed unless the exact persistent host quarantine is still active.
    verify_legacy_network_quarantine "$root" "$capture_id" "$node" "$freeze_sha"
    commit_restart_barrier "$root" "$supervisor_unit" "$supervisor_pid" "$boot_id"
    record_committed_restart_barrier "$root" "$supervisor_unit" "$supervisor_pid"
    arm_stop_journal "$root"
    python3 - "$root" "$writer_pid" "$writer_start_ticks" "$boot_id" \
        "$writer_cgroup_sha" "$writer_executable_path" "$writer_executable_sha" \
        "$writer_argv_sha" "$supervisor_pid" "$supervisor_start_ticks" \
        "$supervisor_unit" "$supervisor_executable_path" \
        "$supervisor_executable_sha" "$supervisor_argv_sha" \
        "$capture_id" "$node" "$freeze_sha" <<'PY'
import hashlib
import json
import os
import pathlib
import re
import select
import signal
import stat
import subprocess
import sys
import time

(root_raw, writer_pid_raw, writer_start_raw, boot_id, writer_cgroup_sha,
 writer_executable_path, writer_executable_sha, writer_argv_sha,
 supervisor_pid_raw, supervisor_start_raw, supervisor_unit,
 supervisor_executable_path, supervisor_executable_sha,
 supervisor_argv_sha, capture_id, node, freeze_sha) = sys.argv[1:]
root = pathlib.Path(root_raw)
writer_pid, writer_start = int(writer_pid_raw), int(writer_start_raw)
supervisor_pid, supervisor_start = int(supervisor_pid_raw), int(supervisor_start_raw)
other_unit = "arc-node.service" if supervisor_unit == "arc-self-heal.service" else "arc-self-heal.service"
allowed_enablement = {
    "disabled", "masked", "masked-runtime", "static",
    "indirect", "generated", "transient", "not-found",
}

def canonical(value):
    return (json.dumps(value, sort_keys=True, separators=(",", ":")) + "\n").encode()

def load_json(path, schema):
    if path.is_symlink() or not path.is_file():
        raise SystemExit(f"required cgroup-stop evidence is missing or unsafe: {path.name}")
    raw = path.read_bytes()
    value = json.loads(raw)
    if (not isinstance(value, dict) or value.get("schema") != schema
            or raw != canonical(value)):
        raise SystemExit(f"cgroup-stop evidence differs: {path.name}")
    return value, raw

def check_network_fence():
    receipt_path=root/"08-network-quarantine.json"
    receipt, _raw=load_json(receipt_path,"arc.recovery.legacy-network-quarantine.v1")
    if ((receipt.get("capture_id"),receipt.get("node"),receipt.get("freeze_plan_sha256"))
            !=(capture_id,node,freeze_sha)):
        raise SystemExit("network quarantine identity changed inside stop controller")
    nft=pathlib.Path("/usr/sbin/nft"); details=nft.lstat()
    if (nft.is_symlink() or not stat.S_ISREG(details.st_mode) or details.st_uid!=0
            or details.st_gid!=0 or details.st_mode & 0o022 or details.st_nlink!=1
            or digest(nft)!=receipt.get("tool_sha256",{}).get("/usr/sbin/nft")):
        raise SystemExit("network quarantine nft tool changed inside stop controller")
    stateless=subprocess.check_output(
        [str(nft),"--stateless","list","table","inet","arc_legacy_maintenance_v1"]
    )
    if hashlib.sha256(stateless).hexdigest()!=receipt.get("owned_ruleset_stateless_sha256"):
        raise SystemExit("network quarantine exact ruleset changed inside stop controller")
    if subprocess.check_output(
            ["/usr/bin/systemctl","is-active","arc-legacy-maintenance-fence.service"],text=True,
    ).strip()!="active" or subprocess.check_output(
            ["/usr/bin/systemctl","is-enabled","arc-legacy-maintenance-fence.service"],text=True,
    ).strip()!="enabled":
        raise SystemExit("network quarantine persistence changed inside stop controller")
    baseline=json.loads(pathlib.Path(
        "/etc/arc-recovery/network-fence/preexisting-ruleset.json"
    ).read_bytes())
    current=json.loads(subprocess.check_output([str(nft),"--json","list","ruleset"]))
    def scrub(item):
        if isinstance(item,dict):
            return {key:scrub(value) for key,value in sorted(item.items())
                    if key not in {"handle","position","index","packets","bytes"}}
        if isinstance(item,list): return [scrub(value) for value in item]
        return item
    def nonowned(value):
        result=[]
        for entry in value.get("nftables",[]):
            if "metainfo" in entry: continue
            obj=next(iter(entry.values())) if isinstance(entry,dict) and len(entry)==1 else None
            if isinstance(obj,dict) and ((entry.get("table",{}).get("family"),entry.get("table",{}).get("name"))
                    ==("inet","arc_legacy_maintenance_v1") or
                    (obj.get("family"),obj.get("table"))==("inet","arc_legacy_maintenance_v1")):
                continue
            result.append(scrub(entry))
        return result
    observed=hashlib.sha256(canonical(nonowned(current))).hexdigest()
    expected=hashlib.sha256(canonical(nonowned(baseline))).hexdigest()
    if observed!=expected or observed!=receipt.get("preexisting_firewall_structural_sha256"):
        raise SystemExit("nonowned firewall changed inside stop controller")

def fsync_dir(path):
    descriptor = os.open(path, os.O_RDONLY | getattr(os, "O_DIRECTORY", 0))
    try:
        os.fsync(descriptor)
    finally:
        os.close(descriptor)

def event(name, value):
    path = root / name
    payload = canonical(value)
    if path.exists():
        if path.is_symlink() or path.read_bytes() != payload:
            raise SystemExit(f"durable cgroup-stop event differs: {name}")
        return
    temporary = path.with_name(f".{path.name}.partial")
    if temporary.exists() or temporary.is_symlink():
        if temporary.is_symlink() or not temporary.is_file():
            raise SystemExit(f"unsafe cgroup-stop event partial: {name}")
        temporary.unlink()
    descriptor = os.open(
        temporary,
        os.O_WRONLY | os.O_CREAT | os.O_EXCL | getattr(os, "O_NOFOLLOW", 0),
        0o400,
    )
    with os.fdopen(descriptor, "wb") as handle:
        handle.write(payload); handle.flush(); os.fsync(handle.fileno())
    os.rename(temporary, path)
    fsync_dir(root)

def digest(path):
    value = hashlib.sha256()
    with open(path, "rb") as handle:
        for chunk in iter(lambda: handle.read(1024 * 1024), b""):
            value.update(chunk)
    return value.hexdigest()

def proc_fields(pid):
    raw = pathlib.Path(f"/proc/{pid}/stat").read_text(encoding="ascii")
    end = raw.rfind(")")
    fields = raw[end + 2:].split()
    if end < 0 or len(fields) < 20:
        raise RuntimeError("process stat is truncated")
    return fields

def proc_start(pid):
    return int(proc_fields(pid)[19])

def sealed_pid_state(pid, start):
    # PID/start-ticks are meaningful only within the boot in which they were
    # sealed.  A coincidental numeric PID and start-tick pair after reboot is
    # a replacement process, never the historical target.
    current_boot = pathlib.Path("/proc/sys/kernel/random/boot_id").read_text().strip()
    if current_boot != boot_id:
        try:
            proc_start(pid)
            return "reused"
        except (FileNotFoundError, ProcessLookupError):
            return "absent"
    try:
        return "same" if proc_start(pid) == start else "reused"
    except (FileNotFoundError, ProcessLookupError):
        return "absent"

def unified_cgroup(pid):
    rows = []
    for line in pathlib.Path(f"/proc/{pid}/cgroup").read_text(encoding="utf-8").splitlines():
        hierarchy, controllers, path = line.split(":", 2)
        if hierarchy == "0" and controllers == "":
            rows.append(path)
    if len(rows) != 1:
        raise SystemExit("target unified cgroup is missing or ambiguous")
    return rows[0]

def systemctl_value(unit, prop):
    return subprocess.check_output(
        ["systemctl", "show", unit, f"--property={prop}", "--value"], text=True,
    ).strip()

def unit_enablement_state(unit):
    result = subprocess.run(
        ["systemctl", "is-enabled", unit], text=True,
        stdout=subprocess.PIPE, stderr=subprocess.DEVNULL, check=False,
    )
    state = result.stdout.strip()
    if state not in allowed_enablement:
        raise SystemExit(f"legacy unit enablement is unsafe: {unit} state={state or 'unknown'}")
    return state

def no_job(unit):
    return systemctl_value(unit, "Job") in {"", "0"}

def verify_identity(pid, start, executable_path, executable_sha, argv_sha, expected_cgroup=None):
    proc = pathlib.Path("/proc") / str(pid)
    if proc_start(pid) != start:
        raise SystemExit("sealed process PID/start changed before TERM")
    if os.readlink(proc / "exe") != executable_path or digest(proc / "exe") != executable_sha:
        raise SystemExit("sealed process executable changed before TERM")
    if hashlib.sha256(proc.joinpath("cmdline").read_bytes()).hexdigest() != argv_sha:
        raise SystemExit("sealed process argv changed before TERM")
    if expected_cgroup is not None and unified_cgroup(pid) != expected_cgroup:
        raise SystemExit("sealed process moved out of its frozen cgroup")
    # The task-level verifier below rejects ptracing, job-control stops,
    # blocked-only delivery, namespace-init default dispositions, and ambiguous
    # thread-directed TERM. Call it here even when no TERM is pending.
    term_pending(pid)

def term_pending(pid):
    term_bit = 1 << (signal.SIGTERM - 1)
    shared_pending = False
    any_unblocked = False
    tasks = sorted(pathlib.Path(f"/proc/{pid}/task").iterdir(), key=lambda item: int(item.name))
    if not tasks:
        raise SystemExit("target process has no task inventory")
    for task in tasks:
        masks = {}
        for line in task.joinpath("status").read_text(encoding="ascii").splitlines():
            key, separator, value = line.partition(":")
            if separator and key in {"SigPnd", "ShdPnd", "SigIgn", "SigBlk", "SigCgt"}:
                masks[key] = int(value.strip(), 16)
            elif separator and key in {"TracerPid", "NSpid"}:
                masks[key] = value.strip()
        if set(masks) != {"SigPnd", "ShdPnd", "SigIgn", "SigBlk", "SigCgt", "TracerPid", "NSpid"}:
            raise SystemExit("target task signal masks are incomplete")
        if masks["TracerPid"] != "0":
            raise SystemExit("target task is ptraced")
        if proc_fields(int(task.name))[0] in {"T", "t"}:
            raise SystemExit("target task is job-control stopped or traced")
        if masks["SigIgn"] & term_bit:
            raise SystemExit("target task ignores SIGTERM")
        if masks["SigPnd"] & term_bit:
            raise SystemExit("thread-directed SIGTERM is pending; process-directed replay inference is unsafe")
        if not masks["SigBlk"] & term_bit:
            any_unblocked = True
        namespace_pids = masks["NSpid"].split()
        if not namespace_pids or not all(value.isdigit() for value in namespace_pids):
            raise SystemExit("target namespace PID inventory is malformed")
        if namespace_pids[-1] == "1" and not masks["SigCgt"] & term_bit:
            raise SystemExit("PID-namespace init target has no caught SIGTERM disposition")
        shared_pending = shared_pending or bool(masks["ShdPnd"] & term_bit)
    if not any_unblocked:
        raise SystemExit("SIGTERM is blocked in every target task")
    return shared_pending

def cgroup_base(entry):
    path = pathlib.Path("/sys/fs/cgroup") / entry["path"].lstrip("/")
    if entry["path"] == "/" or path.is_symlink() or not path.is_dir():
        raise SystemExit("sealed cgroup is missing, root, or a symlink")
    details = path.stat()
    if details.st_dev != entry["device"] or details.st_ino != entry["inode"]:
        raise SystemExit("sealed cgroup path/inode was replaced")
    return path

def cgroup_frozen(entry):
    values = {}
    for line in cgroup_base(entry).joinpath("cgroup.events").read_text(encoding="ascii").splitlines():
        key, _, value = line.partition(" ")
        values[key] = value
    if values.get("frozen") not in {"0", "1"}:
        raise SystemExit("sealed cgroup has no exact frozen state")
    return int(values["frozen"])

def cgroup_local_freeze(entry):
    value = cgroup_base(entry).joinpath("cgroup.freeze").read_text(encoding="ascii").strip()
    if value not in {"0", "1"}:
        raise SystemExit("sealed cgroup has no exact local freezer state")
    return int(value)

def subtree_pids(entry):
    base = cgroup_base(entry)
    result = set()
    for current, dirs, _files in os.walk(base, followlinks=False):
        dirs.sort()
        current_path = pathlib.Path(current)
        if current_path.is_symlink():
            raise SystemExit("sealed cgroup subtree contains a symlink")
        procs = current_path / "cgroup.procs"
        if procs.is_symlink() or not procs.is_file():
            raise SystemExit("sealed cgroup process inventory is unsafe")
        result.update(int(value) for value in procs.read_text(encoding="ascii").splitlines())
    return sorted(result)

def matching_processes(executable_path, executable_sha, argv_sha):
    matches = []
    for entry in pathlib.Path("/proc").iterdir():
        if not entry.name.isdigit():
            continue
        try:
            if (os.readlink(entry / "exe") == executable_path
                    and hashlib.sha256(entry.joinpath("cmdline").read_bytes()).hexdigest() == argv_sha
                    and digest(entry / "exe") == executable_sha):
                matches.append(int(entry.name))
        except (FileNotFoundError, PermissionError, ProcessLookupError):
            pass
    return sorted(matches)

def arc_pids():
    result = []
    for entry in pathlib.Path("/proc").iterdir():
        if not entry.name.isdigit():
            continue
        try:
            if entry.joinpath("comm").read_text().strip() == "arc-node":
                result.append(int(entry.name))
        except (FileNotFoundError, PermissionError, ProcessLookupError):
            pass
    return sorted(result)

reconciliation_outcomes = {
    "already-exited",
    "exited-after-indeterminate-TERM-intent",
    "original-exited; numeric-pid-reused; no signal sent to reused PID",
}

def reconcile_exit(prefix, outcome):
    if outcome not in reconciliation_outcomes:
        raise SystemExit("unsupported frozen-stop reconciliation outcome")
    path = root / f"{prefix}-reconciled-exited.json"
    if path.exists():
        value, _ = load_json(path, "arc.recovery.pidfd-event.v1")
        if (set(value) != {"schema", "target", "outcome"}
                or value.get("target") != prefix
                or value.get("outcome") not in reconciliation_outcomes):
            raise SystemExit(f"durable reconciliation differs: {path.name}")
        return
    event(path.name, {
        "schema": "arc.recovery.pidfd-event.v1",
        "target": prefix,
        "outcome": outcome,
    })

contract, contract_raw = load_json(
    root / "evidence" / "writer-contract.json", "arc.recovery.exact-writer.v3",
)
fast_intent, fast_intent_raw = load_json(
    root / "02-fast-cgroup-freeze-intent.json", "arc.recovery.fast-cgroup-freeze-intent.v1",
)
stop_intent, _ = load_json(root / "stop.intent.json", "arc.recovery.stop-intent.v1")
frozen_context, frozen_raw = load_json(
    root / "05-cgroups-frozen.json", "arc.recovery.cgroups-frozen.v1",
)
if (stop_intent.get("writer_contract_sha256") != hashlib.sha256(contract_raw).hexdigest()
        or fast_intent.get("freeze_plan_sha256") != contract.get("freeze_plan_sha256")
        or stop_intent.get("frozen_context_sha256") != hashlib.sha256(frozen_raw).hexdigest()
        or frozen_context.get("boot_id") != boot_id
        or frozen_context.get("all_cgroups_frozen") is not True):
    raise SystemExit("frozen cgroup controller is not bound to the durable stop intent")
cgroups = frozen_context.get("cgroups")
if (not isinstance(cgroups, list) or not cgroups
        or [entry.get("role") for entry in cgroups] not in (["supervisor"], ["supervisor", "writer"])):
    raise SystemExit("frozen cgroup role inventory differs")
for entry in cgroups:
    if (not isinstance(entry, dict)
            or set(entry) != {"role", "path", "device", "inode"}
            or not isinstance(entry["device"], int)
            or not isinstance(entry["inode"], int)):
        raise SystemExit("frozen cgroup identity fields differ")
role_entries = {entry["role"]: entry for entry in cgroups}
writer_parent_scope_cgroup = frozen_context.get("writer_parent_scope_cgroup")
writer_recovery_leaf = frozen_context.get("writer_recovery_leaf")
fast_writer_contract = fast_intent.get("writer", {})
if "writer" in role_entries:
    if (writer_recovery_leaf != role_entries["writer"]
            or writer_parent_scope_cgroup != fast_writer_contract.get("parent_scope_cgroup")
            or fast_writer_contract.get("recovery_leaf_path") != writer_recovery_leaf.get("path")):
        raise SystemExit("detached writer parent/leaf provenance differs in frozen context")
elif writer_parent_scope_cgroup is not None or writer_recovery_leaf is not None:
    raise SystemExit("systemd writer has detached parent/leaf provenance")
writer_cgroup = frozen_context["post_freeze_processes"]["writer"]["cgroup"]
supervisor_cgroup = frozen_context["post_freeze_processes"]["supervisor"]["cgroup"]
writer_entry = role_entries["writer"] if "writer" in role_entries else role_entries["supervisor"]
if writer_entry["path"] != writer_cgroup or role_entries["supervisor"]["path"] != supervisor_cgroup:
    raise SystemExit("frozen target-to-cgroup mapping differs")

def verify_detached_leaf():
    if "writer" not in role_entries: return
    leaf = cgroup_base(role_entries["writer"])
    events = dict(line.split(" ", 1) for line in leaf.joinpath("cgroup.events").read_text().splitlines())
    if (leaf.joinpath("cgroup.freeze").read_text(encoding="ascii").strip() != "1"
            or events.get("frozen") != "1" or events.get("populated") != "1"
            or subtree_pids(role_entries["writer"]) != [writer_pid]
            or unified_cgroup(writer_pid) != role_entries["writer"]["path"]):
        raise SystemExit("detached writer leaf lost local freeze/sole exact membership")

targets = [{
    "prefix": "30-writer", "pid": writer_pid, "start": writer_start,
    "executable_path": writer_executable_path,
    "executable_sha": writer_executable_sha, "argv_sha": writer_argv_sha,
    "cgroup": writer_cgroup,
}]
if supervisor_pid != writer_pid:
    targets.append({
        "prefix": "20-supervisor", "pid": supervisor_pid, "start": supervisor_start,
        "executable_path": supervisor_executable_path,
        "executable_sha": supervisor_executable_sha, "argv_sha": supervisor_argv_sha,
        "cgroup": supervisor_cgroup,
    })
writer_target = next(target for target in targets if target["prefix"] == "30-writer")
supervisor_targets = [target for target in targets if target["prefix"] == "20-supervisor"]
if "writer" in role_entries and len(supervisor_targets) != 1:
    raise SystemExit("detached writer requires one distinct sealed supervisor target")

captured_members = frozen_context.get("post_freeze_members")
captured_subtrees = frozen_context.get("post_freeze_subtrees")
signal_baseline = frozen_context.get("signal_baseline")
expected_roles = set(role_entries)
if (
    not isinstance(captured_members, dict)
    or set(captured_members) != expected_roles
    or not isinstance(captured_subtrees, dict)
    or set(captured_subtrees) != expected_roles
    or not isinstance(signal_baseline, dict)
    or set(signal_baseline) != {"supervisor", "writer"}
    or frozen_context.get("helper_and_ancestors_outside") is not True
):
    raise SystemExit("frozen cgroup member/signal inventory differs")
for role, members in captured_members.items():
    if not isinstance(members, list) or not members:
        raise SystemExit(f"frozen cgroup has no captured member inventory: {role}")
    seen = set()
    for member in members:
        if (
            not isinstance(member, dict)
            or set(member) != {
                "pid", "start_ticks", "ppid", "executable_path",
                "executable_sha256", "argv_sha256", "cgroup",
            }
            or not all(isinstance(member[key], int) and not isinstance(member[key], bool) for key in ("pid", "start_ticks", "ppid"))
            or member["pid"] in seen
            or member["cgroup"] != role_entries[role]["path"]
            or not all(re.fullmatch(r"[0-9a-f]{64}", member[key]) for key in ("executable_sha256", "argv_sha256"))
            or not isinstance(member["executable_path"], str)
            or not member["executable_path"].startswith("/")
        ):
            raise SystemExit(f"frozen cgroup member identity is malformed: {role}")
        seen.add(member["pid"])
    subtree_pids_from_rows = sorted({
        pid for row in captured_subtrees[role]
        for pid in row.get("pids", [])
    }) if isinstance(captured_subtrees[role], list) else []
    if subtree_pids_from_rows != sorted(seen):
        raise SystemExit(f"frozen cgroup subtree/member inventories differ: {role}")

term_bit = 1 << (signal.SIGTERM - 1)
for target_name, tasks in signal_baseline.items():
    if not isinstance(tasks, list) or not tasks:
        raise SystemExit(f"frozen signal baseline is empty: {target_name}")
    unblocked = False
    tids = set()
    for task in tasks:
        if (
            not isinstance(task, dict)
            or set(task) != {"tid", "SigIgn", "SigPnd", "ShdPnd", "SigBlk", "SigCgt", "TracerPid", "NSpid"}
            or not isinstance(task["tid"], int)
            or task["tid"] in tids
            or task["TracerPid"] != "0"
            or not all(re.fullmatch(r"[0-9a-f]+", task[key]) for key in ("SigIgn", "SigPnd", "ShdPnd", "SigBlk", "SigCgt"))
            or not re.fullmatch(r"[0-9]+(?: [0-9]+)*", task["NSpid"])
        ):
            raise SystemExit(f"frozen signal baseline is malformed: {target_name}")
        tids.add(task["tid"])
        if any(int(task[key], 16) & term_bit for key in ("SigIgn", "SigPnd", "ShdPnd")):
            raise SystemExit(f"frozen signal baseline already consumed/ignored SIGTERM: {target_name}")
        if not int(task["SigBlk"], 16) & term_bit:
            unblocked = True
        if task["NSpid"].split()[-1] == "1" and not int(task["SigCgt"], 16) & term_bit:
            raise SystemExit(f"frozen namespace-init baseline has default SIGTERM: {target_name}")
    if not unblocked:
        raise SystemExit(f"SIGTERM was blocked in every frozen baseline task: {target_name}")

def member_absent(member):
    if sealed_pid_state(member["pid"], member["start_ticks"]) == "same":
        return False
    sealed_target_pids = {writer_pid, supervisor_pid}
    if member["pid"] not in sealed_target_pids:
        # A reviewed transient `sleep <=60` is identified by its captured
        # PID/start/ancestry. Global sleep argv matching would confuse an
        # unrelated host sleep with a restart of this cgroup member.
        return True
    return not matching_processes(
        member["executable_path"], member["executable_sha256"], member["argv_sha256"],
    )

def disappeared_path(role):
    return root / f"06-{role}-cgroup-disappeared.json"

def reused_after_reboot_path(role):
    return root / f"06-{role}-cgroup-path-reused.json"

def existing_cgroup_disposition(entry):
    """Return the first durable terminal pathname disposition, if any.

    A cgroup pathname is boot-local.  After reboot it may alternate between
    absent and reused as login sessions come and go.  Once either exact
    disposition is durable, never traverse, thaw, or publish the opposite
    disposition for that historical target.
    """
    role = entry["role"]
    disappeared = disappeared_path(role)
    reused = reused_after_reboot_path(role)
    has_disappeared = disappeared.exists() or disappeared.is_symlink()
    has_reused = reused.exists() or reused.is_symlink()
    if has_disappeared and has_reused:
        raise SystemExit(f"cgroup has contradictory durable dispositions: {role}")
    if not has_disappeared and not has_reused:
        return None
    role_matches = (
        matching_processes(writer_executable_path, writer_executable_sha, writer_argv_sha)
        if role == "writer" else
        matching_processes(supervisor_executable_path, supervisor_executable_sha, supervisor_argv_sha)
    )
    if role == "supervisor" and "writer" not in role_entries:
        role_matches += matching_processes(writer_executable_path, writer_executable_sha, writer_argv_sha)
    if (any(not member_absent(member) for member in captured_members[role]) or role_matches):
        raise SystemExit(f"sealed process identity remains after durable cgroup disposition: {role}")
    observed_boot = pathlib.Path("/proc/sys/kernel/random/boot_id").read_text().strip()
    if has_disappeared:
        value, _ = load_json(disappeared, "arc.recovery.cgroup-disappeared.v1")
        expected = {
            "schema": "arc.recovery.cgroup-disappeared.v1",
            "role": role,
            "frozen_context_sha256": hashlib.sha256(frozen_raw).hexdigest(),
            "cgroup": entry,
            "outcome": "path-absent; captured-members-and-exact-matches-absent",
            "recovery_sigkill_sent": False,
        }
        if value != expected:
            raise SystemExit(f"durable disappeared-cgroup marker differs: {role}")
        path = pathlib.Path("/sys/fs/cgroup") / entry["path"].lstrip("/")
        if observed_boot == boot_id and (path.exists() or path.is_symlink()):
            raise SystemExit("same-boot cgroup reappeared after durable disappearance")
        return "disappeared"
    value, _ = load_json(reused, "arc.recovery.cgroup-reused-after-reboot.v1")
    if (set(value) != {
            "schema", "role", "frozen_context_sha256", "sealed_boot_id",
            "observed_boot_id", "sealed_cgroup", "observed_path",
            "observed_device", "observed_inode", "outcome",
            "recovery_sigkill_sent",
        }
            or value["role"] != role
            or value["frozen_context_sha256"] != hashlib.sha256(frozen_raw).hexdigest()
            or value["sealed_boot_id"] != boot_id
            or value["observed_boot_id"] == boot_id
            or not re.fullmatch(
                r"[0-9a-f]{8}-[0-9a-f]{4}-[0-9a-f]{4}-[0-9a-f]{4}-[0-9a-f]{12}",
                value["observed_boot_id"],
            )
            or value["sealed_cgroup"] != entry
            or value["observed_path"] != entry["path"]
            or any(isinstance(value[name], bool) or not isinstance(value[name], int)
                   or value[name] < 0 for name in ("observed_device", "observed_inode"))
            or value["outcome"] != "sealed-instance-gone; pathname-present-after-reboot; no signal-or-thaw sent"
            or value["recovery_sigkill_sent"] is not False
            or observed_boot == boot_id):
        raise SystemExit(f"durable rebooted cgroup-reuse marker differs: {role}")
    return "reused-after-reboot"

def verify_or_record_disappeared(entry):
    role = entry["role"]
    path = pathlib.Path("/sys/fs/cgroup") / entry["path"].lstrip("/")
    if path.exists() or path.is_symlink():
        raise SystemExit("sealed disappeared cgroup path reappeared")
    if any(not member_absent(member) for member in captured_members[role]):
        raise SystemExit(f"sealed cgroup disappeared while a captured member/match remains: {role}")
    if role == "supervisor" and (
        systemctl_value(supervisor_unit, "MainPID") != "0"
        or systemctl_value(supervisor_unit, "ActiveState") not in {"inactive", "failed"}
        or not no_job(supervisor_unit)
    ):
        raise SystemExit("supervisor cgroup disappeared but its unit is not stably inactive")
    value = {
        "schema": "arc.recovery.cgroup-disappeared.v1",
        "role": role,
        "frozen_context_sha256": hashlib.sha256(frozen_raw).hexdigest(),
        "cgroup": entry,
        "outcome": "path-absent; captured-members-and-exact-matches-absent",
        "recovery_sigkill_sent": False,
    }
    marker = disappeared_path(role)
    if marker.exists():
        observed, _ = load_json(marker, "arc.recovery.cgroup-disappeared.v1")
        if observed != value:
            raise SystemExit(f"durable disappeared-cgroup marker differs: {role}")
    else:
        event(marker.name, value)
    return "disappeared"

def verify_or_record_reused_after_reboot(entry, details):
    role = entry["role"]
    if pathlib.Path("/proc/sys/kernel/random/boot_id").read_text().strip() == boot_id:
        raise SystemExit("same-boot cgroup path/inode replacement is forbidden")
    if any(not member_absent(member) for member in captured_members[role]):
        raise SystemExit(f"sealed cgroup pathname was reused while a captured member/match remains: {role}")
    role_pid_same = (
        sealed_pid_state(writer_pid, writer_start) == "same" if role == "writer"
        else sealed_pid_state(supervisor_pid, supervisor_start) == "same"
    )
    role_matches = (
        matching_processes(writer_executable_path, writer_executable_sha, writer_argv_sha)
        if role == "writer" else
        matching_processes(supervisor_executable_path, supervisor_executable_sha, supervisor_argv_sha)
    )
    if role == "supervisor" and "writer" not in role_entries:
        role_pid_same = role_pid_same or sealed_pid_state(writer_pid, writer_start) == "same"
        role_matches += matching_processes(writer_executable_path, writer_executable_sha, writer_argv_sha)
    if role_pid_same or role_matches:
        raise SystemExit("sealed process identity remains after rebooted cgroup pathname reuse")
    value = {
        "schema": "arc.recovery.cgroup-reused-after-reboot.v1",
        "role": role, "frozen_context_sha256": hashlib.sha256(frozen_raw).hexdigest(),
        "sealed_boot_id": boot_id,
        "observed_boot_id": pathlib.Path("/proc/sys/kernel/random/boot_id").read_text().strip(),
        "sealed_cgroup": entry,
        "observed_path": entry["path"],
        "observed_device": details.st_dev, "observed_inode": details.st_ino,
        "outcome": "sealed-instance-gone; pathname-present-after-reboot; no signal-or-thaw sent",
        "recovery_sigkill_sent": False,
    }
    marker = reused_after_reboot_path(role)
    if marker.exists() or marker.is_symlink():
        observed, _ = load_json(marker, "arc.recovery.cgroup-reused-after-reboot.v1")
        if observed != value: raise SystemExit(f"durable rebooted cgroup-reuse marker differs: {role}")
    else:
        event(marker.name, value)
    return "reused-after-reboot"

def cgroup_state(entry):
    disposition = existing_cgroup_disposition(entry)
    if disposition is not None:
        return disposition
    path = pathlib.Path("/sys/fs/cgroup") / entry["path"].lstrip("/")
    try:
        details = path.lstat()
    except FileNotFoundError:
        return verify_or_record_disappeared(entry)
    if pathlib.Path("/proc/sys/kernel/random/boot_id").read_text().strip() != boot_id:
        return verify_or_record_reused_after_reboot(entry, details)
    if path.is_symlink() or not path.is_dir():
        raise SystemExit("sealed cgroup path became unsafe")
    if details.st_dev != entry["device"] or details.st_ino != entry["inode"]:
        raise SystemExit("sealed cgroup path/inode was replaced")
    marker = disappeared_path(entry["role"])
    if marker.exists() or marker.is_symlink():
        raise SystemExit("sealed cgroup reappeared after durable disappearance")
    return "frozen" if cgroup_frozen(entry) == 1 else "thawed"

def thaw_progress_path(entry):
    return root / f"51-{entry['role']}-cgroup-thaw-complete.json"

def thaw(entry, intent_path):
    check_network_fence()
    marker = thaw_progress_path(entry)
    state = cgroup_state(entry)
    thaw_intent_raw = intent_path.read_bytes()
    base_value = {
        "schema": "arc.recovery.cgroup-thaw-complete.v1",
        "role": entry["role"],
        "cgroup": entry,
        "frozen_context_sha256": hashlib.sha256(frozen_raw).hexdigest(),
        "thaw_intent_sha256": hashlib.sha256(thaw_intent_raw).hexdigest(),
        "recovery_sigkill_sent": False,
    }
    if marker.exists():
        observed, _ = load_json(marker, "arc.recovery.cgroup-thaw-complete.v1")
        outcome = observed.get("outcome")
        if outcome not in {"thawed-by-direct-inode-checked-controller", "already-thawed-after-durable-intent", "disappeared-empty", "sealed-instance-gone-path-reused"} \
                or observed != {**base_value, "outcome": outcome}:
            raise SystemExit(f"durable per-cgroup thaw marker differs: {entry['role']}")
        if state == "frozen":
            raise SystemExit("cgroup was refrozen after durable thaw completion")
        return
    if state == "disappeared":
        outcome = "disappeared-empty"
    elif state == "thawed":
        outcome = "already-thawed-after-durable-intent"
    elif state == "reused-after-reboot":
        outcome = "sealed-instance-gone-path-reused"
    else:
        directory_path = cgroup_base(entry)
        if cgroup_state(entry) == "frozen":
            # All legacy names are control-masked. The durable thaw intent
            # authorizes only this exact dirfd/dev/inode kernel transition;
            # name-based ThawUnit is forbidden because a unit name can be
            # rebound while the sealed cgroup inode remains our authority.
            directory = os.open(
                directory_path,
                os.O_RDONLY | getattr(os, "O_DIRECTORY", 0) | getattr(os, "O_NOFOLLOW", 0),
            )
            try:
                details = os.fstat(directory)
                if details.st_dev != entry["device"] or details.st_ino != entry["inode"]:
                    raise SystemExit("opened cgroup differs from sealed frozen identity")
                check_network_fence()
                freezer = os.open(
                    "cgroup.freeze", os.O_WRONLY | getattr(os, "O_NOFOLLOW", 0),
                    dir_fd=directory,
                )
                try:
                    os.write(freezer, b"0")
                finally:
                    os.close(freezer)
                if pathlib.Path(directory_path, "cgroup.freeze").read_text(encoding="ascii").strip() != "0":
                    raise SystemExit("direct-thaw cgroup local freeze did not clear")
            finally:
                os.close(directory)
        deadline = time.monotonic() + 10
        while True:
            state = cgroup_state(entry)
            if state in {"thawed", "disappeared"}:
                break
            if time.monotonic() >= deadline:
                raise SystemExit(f"cgroup did not thaw: {entry['path']}")
            time.sleep(0.02)
        outcome = "disappeared-empty" if state == "disappeared" else "thawed-by-direct-inode-checked-controller"
    event(marker.name, {**base_value, "outcome": outcome})

def term_progress(prefix):
    paths = {
        "intent": root / f"{prefix}-term-intent.json",
        "sent": root / f"{prefix}-term-sent.json",
        "pending": root / f"{prefix}-term-pending-observed.json",
        "replay": root / f"{prefix}-term-replay-safe.json",
        "exited": root / f"{prefix}-exited.json",
        "reconciled": root / f"{prefix}-reconciled-exited.json",
    }
    if paths["exited"].exists() or paths["reconciled"].exists():
        return "terminal"
    if paths["sent"].exists():
        return "confirmed"
    if paths["intent"].exists() or paths["pending"].exists():
        return "indeterminate"
    return "missing"

def ensure_term(target):
    prefix, pid, start = target["prefix"], target["pid"], target["start"]
    intent_path = root / f"{prefix}-term-intent.json"
    sent_path = root / f"{prefix}-term-sent.json"
    pending_path = root / f"{prefix}-term-pending-observed.json"
    replay_path = root / f"{prefix}-term-replay-safe.json"
    exited_path = root / f"{prefix}-exited.json"
    reconciled_path = root / f"{prefix}-reconciled-exited.json"
    state = sealed_pid_state(pid, start)
    if exited_path.exists() and reconciled_path.exists():
        raise SystemExit(f"{prefix} has contradictory terminal evidence")
    if exited_path.exists() or reconciled_path.exists():
        if state == "same":
            raise SystemExit(f"{prefix} has terminal evidence but the sealed process remains")
        return
    if state != "same":
        if sent_path.exists() or pending_path.exists():
            event(exited_path.name, {
                "schema": "arc.recovery.pidfd-event.v1",
                "target": prefix,
                "outcome": "exit-observed-after-durable-TERM-progress; recovery-sigkill-sent=false",
            })
        elif intent_path.exists():
            reconcile_exit(prefix, "exited-after-indeterminate-TERM-intent")
        elif state == "reused":
            reconcile_exit(prefix, "original-exited; numeric-pid-reused; no signal sent to reused PID")
        else:
            reconcile_exit(prefix, "already-exited")
        return
    verify_identity(
        pid, start, target["executable_path"], target["executable_sha"],
        target["argv_sha"], target["cgroup"],
    )
    entry = next(item for item in cgroups if item["path"] == target["cgroup"])
    if cgroup_state(entry) != "frozen":
        raise SystemExit(f"{prefix} cgroup thawed before durable TERM progress")
    expected_intent = {
        "schema": "arc.recovery.pidfd-term-intent.v1",
        "target": prefix, "pid": pid, "start_ticks": start,
        "signal": "SIGTERM", "sigkill_allowed": False,
    }
    exact_outcomes = {
        sent_path: "SIGTERM-sent-via-pidfd-while-cgroup-frozen",
        pending_path: "SIGTERM-pending-observed-after-indeterminate-intent",
        replay_path: "frozen-baseline-had-no-SIGTERM; current-pending=false; one-send-safe",
    }
    if intent_path.exists():
        observed, _ = load_json(intent_path, "arc.recovery.pidfd-term-intent.v1")
        if observed != expected_intent:
            raise SystemExit(f"durable TERM intent differs: {prefix}")
    for path, outcome in exact_outcomes.items():
        if path.exists():
            observed, _ = load_json(path, "arc.recovery.pidfd-event.v1")
            if observed != {
                "schema": "arc.recovery.pidfd-event.v1", "target": prefix, "outcome": outcome,
            }:
                raise SystemExit(f"durable TERM event differs: {path.name}")
    if sent_path.exists() or pending_path.exists():
        if not intent_path.exists() or (sent_path.exists() and pending_path.exists()):
            raise SystemExit(f"durable TERM progress is contradictory: {prefix}")
        if not term_pending(pid):
            raise SystemExit(f"{prefix} surviving frozen target lost its process-directed pending SIGTERM")
        return
    descriptor = os.pidfd_open(pid, 0)
    poller = select.poll(); poller.register(descriptor, select.POLLIN)
    try:
        if poller.poll(0):
            reconcile_exit(prefix, "already-exited")
            return
        preexisting_intent = intent_path.exists()
        pending_before_intent = term_pending(pid)
        if not preexisting_intent and pending_before_intent:
            raise SystemExit("SIGTERM became pending before its durable recovery intent")
        event(intent_path.name, expected_intent)
        verify_identity(
            pid, start, target["executable_path"], target["executable_sha"],
            target["argv_sha"], target["cgroup"],
        )
        if preexisting_intent and term_pending(pid):
            event(pending_path.name, {
                "schema": "arc.recovery.pidfd-event.v1",
                "target": prefix,
                "outcome": "SIGTERM-pending-observed-after-indeterminate-intent",
            })
            return
        if preexisting_intent:
            event(replay_path.name, {
                "schema": "arc.recovery.pidfd-event.v1",
                "target": prefix,
                "outcome": "frozen-baseline-had-no-SIGTERM; current-pending=false; one-send-safe",
            })
        check_fence()
        signal.pidfd_send_signal(descriptor, signal.SIGTERM, None, 0)
        event(sent_path.name, {
            "schema": "arc.recovery.pidfd-event.v1",
            "target": prefix, "outcome": "SIGTERM-sent-via-pidfd-while-cgroup-frozen",
        })
    finally:
        os.close(descriptor)

def validate_frozen_for_signals():
    check_fence()
    verify_detached_leaf()
    for entry in cgroups:
        state = cgroup_state(entry)
        if state == "thawed":
            raise SystemExit("sealed cgroup unexpectedly thawed before TERM/thaw intent")
        if entry["role"] == "supervisor" and state == "frozen" and systemctl_value(
            supervisor_unit, "FreezerState",
        ) not in {"running", "frozen"}:
            raise SystemExit("supervisor has an invalid advisory PID1 freezer state before TERM")

def progress_receipt(stage_targets):
    result = {}
    for target in stage_targets:
        prefix = target["prefix"]
        files = {}
        for suffix in (
            "term-intent", "term-sent", "term-pending-observed",
            "term-replay-safe", "exited", "reconciled-exited",
        ):
            path = root / f"{prefix}-{suffix}.json"
            if path.exists():
                files[suffix] = hashlib.sha256(path.read_bytes()).hexdigest()
        result[prefix] = {"state": term_progress(prefix), "files": files}
    return result

def validate_progress_receipt(progress, stage_targets):
    expected_prefixes = {target["prefix"] for target in stage_targets}
    if not isinstance(progress, dict) or set(progress) != expected_prefixes:
        raise SystemExit("durable thaw target progress has unexpected targets")
    allowed_suffixes = {
        "term-intent", "term-sent", "term-pending-observed",
        "term-replay-safe", "exited", "reconciled-exited",
    }
    for prefix, row in progress.items():
        current_state = term_progress(prefix)
        allowed_current = {
            "confirmed": {"confirmed", "terminal"},
            "indeterminate": {"indeterminate", "terminal"},
            "terminal": {"terminal"},
        }
        if (not isinstance(row, dict) or set(row) != {"state", "files"}
                or row["state"] not in {"confirmed", "indeterminate", "terminal"}
                or current_state not in allowed_current.get(row["state"], set())
                or not isinstance(row["files"], dict)
                or not set(row["files"]).issubset(allowed_suffixes)):
            raise SystemExit(f"durable thaw target progress is malformed: {prefix}")
        present = set(row["files"])
        if ({"term-sent", "term-pending-observed"} <= present
                or {"exited", "reconciled-exited"} <= present
                or (present & {"term-sent", "term-pending-observed", "term-replay-safe"}
                    and "term-intent" not in present)):
            raise SystemExit(f"durable thaw target progress is contradictory: {prefix}")
        if row["state"] == "confirmed":
            if (not {"term-intent", "term-sent"} <= present
                    or present & {"term-pending-observed", "exited", "reconciled-exited"}
                    or not present <= {"term-intent", "term-sent", "term-replay-safe"}):
                raise SystemExit(f"durable confirmed TERM progress differs: {prefix}")
        elif row["state"] == "indeterminate":
            if (not {"term-intent", "term-pending-observed"} <= present
                    or present & {"term-sent", "exited", "reconciled-exited"}
                    or not present <= {"term-intent", "term-pending-observed", "term-replay-safe"}):
                raise SystemExit(f"durable indeterminate TERM progress differs: {prefix}")
        elif "exited" in present:
            if ("term-intent" not in present
                    or len(present & {"term-sent", "term-pending-observed"}) != 1
                    or not present <= {
                        "term-intent", "term-sent", "term-pending-observed", "term-replay-safe", "exited",
                    }):
                raise SystemExit(f"durable terminal TERM progress differs: {prefix}")
        elif "reconciled-exited" in present:
            if (present & {"term-sent", "term-pending-observed", "exited"}
                    or not present <= {"term-intent", "term-replay-safe", "reconciled-exited"}):
                raise SystemExit(f"durable terminal reconciliation differs: {prefix}")
        else:
            raise SystemExit(f"durable terminal TERM progress has no terminal event: {prefix}")
        for suffix, expected_sha in row["files"].items():
            path = root / f"{prefix}-{suffix}.json"
            if (not isinstance(expected_sha, str)
                    or not re.fullmatch(r"[0-9a-f]{64}", expected_sha)
                    or path.is_symlink() or not path.is_file()
                    or hashlib.sha256(path.read_bytes()).hexdigest() != expected_sha):
                raise SystemExit(f"durable thaw receipt hash differs: {path.name}")

def write_or_verify_thaw_intent(path, mode, stage_cgroups, stage_targets, predecessor_sha256=None):
    if path.exists():
        value, _ = load_json(path, "arc.recovery.cgroup-thaw-intent.v2")
        if set(value) != {
            "schema", "mode", "frozen_context_sha256", "cgroups",
            "target_term_progress", "predecessor_sha256", "recovery_sigkill_allowed",
        } or (value.get("mode") != mode
                or value.get("frozen_context_sha256") != hashlib.sha256(frozen_raw).hexdigest()
                or value.get("cgroups") != stage_cgroups
                or value.get("predecessor_sha256") != predecessor_sha256
                or value.get("recovery_sigkill_allowed") is not False):
            raise SystemExit("durable cgroup thaw intent differs")
        validate_progress_receipt(value.get("target_term_progress"), stage_targets)
        return
    progress = progress_receipt(stage_targets)
    if any(row["state"] not in {"confirmed", "indeterminate", "terminal"} for row in progress.values()):
        raise SystemExit("target TERM progress is malformed before thaw")
    validate_progress_receipt(progress, stage_targets)
    event(path.name, {
        "schema": "arc.recovery.cgroup-thaw-intent.v2", "mode": mode,
        "frozen_context_sha256": hashlib.sha256(frozen_raw).hexdigest(),
        "cgroups": stage_cgroups,
        "target_term_progress": progress,
        "predecessor_sha256": predecessor_sha256,
        "recovery_sigkill_allowed": False,
    })

def load_writer_terminal(required=False):
    path = root / "52-writer-terminal-before-supervisor-thaw.json"
    if not path.exists() and not path.is_symlink():
        if required: raise SystemExit("detached writer terminal proof is missing")
        return None, None
    value, raw = load_json(path, "arc.recovery.detached-writer-terminal.v2")
    writer_intent_path = root / "50-cgroups-thaw-intent.json"
    writer_receipt_path = thaw_progress_path(role_entries["writer"])
    containment = value.get("supervisor_containment")
    if (set(value) != {
            "schema", "writer_pid", "writer_start_ticks", "writer_cgroup",
            "writer_thaw_intent_sha256", "writer_thaw_complete_sha256",
            "stable_absence_checks", "supervisor_containment", "recovery_sigkill_sent",
        }
            or value.get("writer_pid") != writer_pid
            or value.get("writer_start_ticks") != writer_start
            or value.get("writer_cgroup") != role_entries["writer"]
            or value.get("writer_thaw_intent_sha256")
            != hashlib.sha256(writer_intent_path.read_bytes()).hexdigest()
            or value.get("writer_thaw_complete_sha256")
            != hashlib.sha256(writer_receipt_path.read_bytes()).hexdigest()
            or value.get("stable_absence_checks") != 2
            or value.get("recovery_sigkill_sent") is not False
            or not isinstance(containment, dict)
            or set(containment) != {
                "supervisor_pid", "supervisor_start_ticks", "supervisor_cgroup",
                "outcome", "cgroup_disposition_sha256", "term_progress_before_writer_terminal",
            }
            or containment.get("supervisor_pid") != supervisor_pid
            or containment.get("supervisor_start_ticks") != supervisor_start
            or containment.get("supervisor_cgroup") != role_entries["supervisor"]
            or containment.get("outcome") not in {
                "sealed-supervisor-live-and-frozen",
                "sealed-supervisor-terminal-cgroup-frozen",
            }
            or containment.get("cgroup_disposition_sha256") is not None
            or containment.get("term_progress_before_writer_terminal") != "missing"):
        raise SystemExit("detached writer terminal/containment proof differs")
    return value, raw

def record_target_exits(stage_targets=targets):
    for target in stage_targets:
        prefix, pid, start = target["prefix"], target["pid"], target["start"]
        state = sealed_pid_state(pid, start)
        if state == "same":
            continue
        exited_path = root / f"{prefix}-exited.json"
        reconciled_path = root / f"{prefix}-reconciled-exited.json"
        if exited_path.exists() and reconciled_path.exists():
            raise SystemExit(f"{prefix} has contradictory terminal evidence")
        if exited_path.exists() or reconciled_path.exists():
            continue
        if (root / f"{prefix}-term-sent.json").exists() or (root / f"{prefix}-term-pending-observed.json").exists():
            event(f"{prefix}-exited.json", {
                "schema": "arc.recovery.pidfd-event.v1",
                "target": prefix,
                "outcome": "exit-observed-after-durable-TERM-progress; recovery-sigkill-sent=false",
            })
        elif (root / f"{prefix}-term-intent.json").exists():
            reconcile_exit(prefix, "exited-after-indeterminate-TERM-intent")
        elif state == "reused":
            reconcile_exit(prefix, "original-exited; numeric-pid-reused; no signal sent to reused PID")
        else:
            reconcile_exit(prefix, "already-exited")

def require_absent_snapshot():
    check_fence()
    if sealed_pid_state(writer_pid, writer_start) == "same":
        raise SystemExit("sealed writer PID/start remains after cgroup thaw")
    if sealed_pid_state(supervisor_pid, supervisor_start) == "same":
        raise SystemExit("sealed supervisor PID/start remains after cgroup thaw")
    if arc_pids():
        raise SystemExit(f"arc-node process remains after frozen stop: {arc_pids()}")
    if matching_processes(writer_executable_path, writer_executable_sha, writer_argv_sha):
        raise SystemExit("matching sealed writer process remains after frozen stop")
    if matching_processes(supervisor_executable_path, supervisor_executable_sha, supervisor_argv_sha):
        raise SystemExit("matching sealed supervisor process remains after frozen stop")
    for entry in cgroups:
        state = cgroup_state(entry)
        if state == "frozen" or (state == "thawed" and subtree_pids(entry)):
            raise SystemExit("reviewed cgroup is frozen or non-empty after thaw")
        if state == "thawed":
            thaw_receipt, _ = load_json(
                thaw_progress_path(entry), "arc.recovery.cgroup-thaw-complete.v1",
            )
            if thaw_receipt.get("outcome") not in {
                "thawed-by-direct-inode-checked-controller",
                "already-thawed-after-durable-intent",
            }:
                raise SystemExit("kernel-thawed cgroup has no authoritative direct-thaw receipt")
    for unit in (supervisor_unit, other_unit, "arc-node-update.service", "arc-node-update.timer"):
        if ((unit.endswith(".service") and systemctl_value(unit, "MainPID") != "0")
                or systemctl_value(unit, "ActiveState") not in {"inactive", "failed"}
                or not no_job(unit)):
            raise SystemExit(f"legacy unit is not stably inactive: {unit}")

def check_fence():
    check_network_fence()
    import stat as stat_module
    marker = pathlib.Path("/etc/arc-recovery/legacy-start-allowed")
    barrier_bytes = b"[Unit]\nConditionPathExists=/etc/arc-recovery/legacy-start-allowed\n"
    service_bytes = b"[Unit]\nRefuseManualStart=yes\nRefuseManualStop=yes\nIgnoreOnIsolate=yes\nStopWhenUnneeded=no\nBindsTo=\nPartOf=\nPropagatesStopTo=\nOnFailure=\nOnSuccess=\nFailureAction=none\nSuccessAction=none\nJobTimeoutAction=none\n\n[Service]\nExecReload=\nExecStop=\nExecStopPost=\nRestart=no\nKillMode=process\nSendSIGKILL=no\nSendSIGHUP=no\nOOMPolicy=continue\nWatchdogSec=0\nRuntimeMaxSec=infinity\n"
    timer_bytes = b"[Unit]\nRefuseManualStart=yes\nRefuseManualStop=yes\nIgnoreOnIsolate=yes\nStopWhenUnneeded=no\nBindsTo=\nPartOf=\nPropagatesStopTo=\nOnFailure=\nOnSuccess=\nFailureAction=none\nSuccessAction=none\nJobTimeoutAction=none\n"
    if marker.exists() or marker.is_symlink():
        raise SystemExit("legacy start authorization marker appeared after commit")
    units = (supervisor_unit, other_unit, "arc-node-update.service", "arc-node-update.timer")
    for unit in units:
        for override in (
            pathlib.Path(f"/etc/systemd/system.control/{unit}"),
            pathlib.Path(f"/etc/systemd/system.control/{unit}.d"),
        ):
            if override.exists() or override.is_symlink():
                raise SystemExit(f"persistent systemd control override appeared: {unit}")
        path = pathlib.Path(f"/etc/systemd/system/{unit}.d/zzzz-arc-recovery-freeze.conf")
        details = path.lstat()
        if (path.is_symlink() or not stat_module.S_ISREG(details.st_mode)
                or details.st_uid != 0 or details.st_gid != 0
                or details.st_mode & 0o222 or path.read_bytes() != barrier_bytes):
            raise SystemExit(f"persistent condition-only start barrier differs: {unit}")
    arm, arm_raw = load_json(root / "06-restart-barrier-armed.json", "arc.recovery.restart-barrier-arm.v1")
    commit, _ = load_json(root / "07-restart-barrier-committed.json", "arc.recovery.restart-barrier-committed.v2")
    gate, gate_raw = load_json(root / "06-pre-mask-activation-gate.json", "arc.recovery.pre-mask-activation-gate.v1")
    expected_masks = {unit: "/dev/null" for unit in units}
    if (commit.get("barrier_arm_sha256") != hashlib.sha256(arm_raw).hexdigest()
            or commit.get("allow_marker_absent") is not True
            or commit.get("unlink_parent_fsynced") is not True
            or arm.get("selected_unit") != supervisor_unit
            or arm.get("selected_main_pid") != supervisor_pid
            or arm.get("all_cgroups_frozen") is not True
            or arm.get("cgroups") != cgroups
            or arm.get("writer_parent_scope_cgroup") != writer_parent_scope_cgroup
            or arm.get("writer_recovery_leaf") != writer_recovery_leaf
            or arm.get("control_masks") != expected_masks
            or arm.get("pre_mask_activation_gate_sha256") != hashlib.sha256(gate_raw).hexdigest()
            or arm.get("source_sha256", {}).get("06-pre-mask-activation-gate.json")
            != hashlib.sha256(gate_raw).hexdigest()
            or gate.get("cgroups") != cgroups
            or gate.get("writer_parent_scope_cgroup") != writer_parent_scope_cgroup
            or gate.get("writer_recovery_leaf") != writer_recovery_leaf
            or gate.get("all_cgroups_frozen") is not True):
        raise SystemExit("restart-barrier arm/commit chain differs")

    def verify_same_boot_sources():
        manifests = gate.get("merged_unit_sources")
        unit_path = gate.get("unit_path")
        if (not isinstance(manifests, dict) or set(manifests) != set(units)
                or not isinstance(unit_path, list) or not unit_path):
            raise SystemExit("pre-mask merged source/UnitPath contract differs")
        for unit in units:
            rows = manifests[unit]
            if not isinstance(rows, list) or not rows: raise SystemExit(f"empty sealed source manifest: {unit}")
            expected_paths = set()
            for row in rows:
                if not isinstance(row, dict) or set(row) != {"path", "sha256"}:
                    raise SystemExit(f"malformed sealed source manifest: {unit}")
                source = pathlib.Path(row["path"]); details = source.lstat()
                if (source.is_symlink() or not stat_module.S_ISREG(details.st_mode)
                        or details.st_uid != 0 or details.st_gid != 0 or details.st_mode & 0o022
                        or hashlib.sha256(source.read_bytes()).hexdigest() != row["sha256"]):
                    raise SystemExit(f"sealed systemd source changed: {source}")
                if row["path"] in expected_paths: raise SystemExit(f"duplicate sealed source: {source}")
                expected_paths.add(row["path"])
            actual_dropins = set()
            for unit_root in unit_path:
                if not isinstance(unit_root, str) or unit_root.endswith("/system.control"): continue
                directory = pathlib.Path(unit_root) / f"{unit}.d"
                if not directory.exists(): continue
                if directory.is_symlink() or not directory.is_dir():
                    raise SystemExit(f"systemd drop-in directory changed: {directory}")
                for entry in directory.glob("*.conf"):
                    if entry.is_symlink() or not entry.is_file():
                        raise SystemExit(f"systemd drop-in changed: {entry}")
                    actual_dropins.add(str(entry))
            expected_dropins = {path for path in expected_paths if pathlib.PurePosixPath(path).parent.name == f"{unit}.d"}
            if actual_dropins != expected_dropins:
                raise SystemExit(f"systemd drop-in set changed after barrier commit: {unit}")
            control_dropin = pathlib.Path(f"/run/systemd/system.control/{unit}.d")
            if control_dropin.exists() or control_dropin.is_symlink():
                raise SystemExit(f"unreviewed high-priority control drop-in appeared: {unit}")

    def verify_post_reboot_condition(unit):
        if systemctl_value(unit, "LoadState") == "not-found": return
        merged = subprocess.check_output(["systemctl", "cat", unit], text=True)
        conditions = []; section = None
        for raw_line in merged.splitlines():
            line = raw_line.strip()
            if not line or line.startswith(("#", ";")): continue
            if line.startswith("[") and line.endswith("]"):
                section = line[1:-1].strip(); continue
            if section != "Unit" or "=" not in line: continue
            key, value = (item.strip() for item in line.split("=", 1))
            if key == "ConditionPathExists":
                if value == "": conditions.clear()
                else: conditions.append(value)
        if "/etc/arc-recovery/legacy-start-allowed" not in conditions:
            raise SystemExit(f"post-reboot persistent condition was reset: {unit}")

    observed_boot = pathlib.Path("/proc/sys/kernel/random/boot_id").read_text().strip()
    if observed_boot == boot_id:
        detached_mode = "writer" in role_entries
        writer_intent_path = root / "50-cgroups-thaw-intent.json"
        writer_terminal_path = root / "52-writer-terminal-before-supervisor-thaw.json"
        supervisor_intent_path = root / "53-supervisor-cgroup-thaw-intent.json"
        writer_thaw_started = writer_intent_path.exists() or writer_intent_path.is_symlink()
        writer_terminal_exists = writer_terminal_path.exists() or writer_terminal_path.is_symlink()
        supervisor_thaw_started = supervisor_intent_path.exists() or supervisor_intent_path.is_symlink()
        writer_terminal_raw = None
        if detached_mode:
            if writer_thaw_started:
                write_or_verify_thaw_intent(
                    writer_intent_path, "detached-writer-first",
                    [role_entries["writer"]], [writer_target],
                )
            if writer_terminal_exists:
                if not writer_thaw_started:
                    raise SystemExit("writer terminal proof appeared before writer thaw intent")
                _writer_terminal, writer_terminal_raw = load_writer_terminal(required=True)
            if supervisor_thaw_started:
                if writer_terminal_raw is None:
                    raise SystemExit("supervisor thaw intent appeared before writer terminal proof")
                write_or_verify_thaw_intent(
                    supervisor_intent_path, "detached-supervisor-after-writer-terminal",
                    [role_entries["supervisor"]], supervisor_targets,
                    hashlib.sha256(writer_terminal_raw).hexdigest(),
                )
        else:
            if writer_terminal_exists or supervisor_thaw_started:
                raise SystemExit("systemd-shared recovery has detached thaw artifacts")
            if writer_thaw_started:
                write_or_verify_thaw_intent(
                    writer_intent_path, "systemd-shared", cgroups, targets,
                )
        verify_same_boot_sources()
        for unit in units:
            mask = pathlib.Path(f"/run/systemd/system.control/{unit}")
            if (not mask.is_symlink() or os.readlink(mask) != "/dev/null"
                    or systemctl_value(unit, "LoadState") != "masked"
                    or systemctl_value(unit, "FragmentPath") != f"/run/systemd/system.control/{unit}"
                    or systemctl_value(unit, "UnitFileState") != "masked-runtime"
                    or not no_job(unit)):
                raise SystemExit(f"same-boot four-unit control mask differs: {unit}")
        runtime_path = pathlib.Path(f"/run/systemd/system/{supervisor_unit}.d/zzzy-arc-recovery-prefreeze-safety.conf")
        if runtime_path.is_symlink() or runtime_path.read_bytes() != service_bytes:
            raise SystemExit("selected runtime lifecycle safety differs")
        expected = {
            "Restart": "no", "KillMode": "process", "SendSIGKILL": "no", "SendSIGHUP": "no",
            "IgnoreOnIsolate": "yes",
            "OOMPolicy": "continue", "WatchdogUSec": "0", "RuntimeMaxUSec": "infinity",
            "CanReload": "no", "ExecReload": "", "ExecStop": "", "ExecStopPost": "",
            "OnFailure": "", "OnSuccess": "", "SuccessAction": "none",
            "FailureAction": "none", "JobTimeoutAction": "none",
        }
        supervisor_state = sealed_pid_state(supervisor_pid, supervisor_start)
        supervisor_cgroup_state = cgroup_state(role_entries["supervisor"])
        supervisor_thaw_authorized = writer_thaw_started if not detached_mode else supervisor_thaw_started
        if supervisor_cgroup_state == "frozen":
            if cgroup_local_freeze(role_entries["supervisor"]) != 1:
                raise SystemExit("selected supervisor cgroup lost its local freeze request")
        elif supervisor_cgroup_state == "thawed":
            if (not supervisor_thaw_authorized
                    or cgroup_local_freeze(role_entries["supervisor"]) != 0):
                raise SystemExit("selected supervisor thaw is not journal-authorized")
        elif supervisor_cgroup_state == "disappeared":
            if supervisor_state == "same":
                raise SystemExit("live selected supervisor lost its sealed cgroup")
            supervisor_progress = term_progress("20-supervisor") if supervisor_targets else term_progress("30-writer")
            if (supervisor_progress not in {"confirmed", "terminal"}
                    and not supervisor_thaw_authorized):
                raise SystemExit("selected supervisor disappeared without durable TERM/thaw progress")
        elif supervisor_cgroup_state != "reused-after-reboot":
            raise SystemExit("selected supervisor cgroup has an unsupported phase state")
        if supervisor_state == "same":
            if (any(systemctl_value(supervisor_unit, name) != wanted for name, wanted in expected.items())
                    or systemctl_value(supervisor_unit, "MainPID") != str(supervisor_pid)
                    or systemctl_value(supervisor_unit, "ActiveState") != "active"
                    or systemctl_value(supervisor_unit, "InvocationID")
                    != contract["supervisor_context"]["invocation_id"]
                    or systemctl_value(supervisor_unit, "ControlGroup") != supervisor_cgroup
                    or supervisor_cgroup_state not in ({"frozen", "thawed"} if supervisor_thaw_authorized else {"frozen"})):
                raise SystemExit("live selected supervisor lifecycle/phase identity changed")
        elif (systemctl_value(supervisor_unit, "MainPID") != "0"
                or systemctl_value(supervisor_unit, "ActiveState") not in {"inactive", "failed"}
                or not no_job(supervisor_unit)):
            raise SystemExit("terminal selected supervisor has inconsistent unit state")
        for unit in units:
            expected_runtime = timer_bytes if unit.endswith(".timer") else service_bytes
            path = pathlib.Path(f"/run/systemd/system/{unit}.d/zzzy-arc-recovery-prefreeze-safety.conf")
            if path.is_symlink() or path.read_bytes() != expected_runtime:
                raise SystemExit(f"same-boot runtime safety changed: {unit}")
            if unit != supervisor_unit and (
                    systemctl_value(unit, "ActiveState") not in {"inactive", "failed"}
                    or (unit.endswith(".service") and systemctl_value(unit, "MainPID") != "0")):
                raise SystemExit(f"masked alternative legacy unit activated: {unit}")
        if detached_mode:
            scope = fast_intent.get("writer", {}); scope_unit = scope.get("scope_unit")
            properties = scope.get("scope_properties"); sources = scope.get("scope_runtime_sources")
            safety_path = pathlib.Path(scope.get("scope_runtime_safety_path", ""))
            arm_scope = arm.get("detached_scope_safety")
            gate_scope = gate.get("detached_scope")
            arm_parent_state = arm_scope.get("parent_state") if isinstance(arm_scope, dict) else None
            expected_arm_scope = {
                "unit": scope_unit, "properties": properties, "sources": sources,
                "parent_state": arm_parent_state,
                "runtime_safety_sha256": scope.get("scope_runtime_safety_sha256"),
                "parent_scope_cgroup": writer_parent_scope_cgroup,
                "recovery_leaf": writer_recovery_leaf,
            }
            try: safety_details = safety_path.lstat()
            except FileNotFoundError: raise SystemExit("detached scope runtime safety disappeared")
            if (not re.fullmatch(r"session-[1-9][0-9]*\.scope", scope_unit or "")
                    or not isinstance(properties, dict) or not isinstance(sources, list) or not sources
                    or arm.get("detached_scope_safety") != expected_arm_scope
                    or arm_parent_state not in {"active-sealed", "terminal-after-leaf-seal"}
                    or not isinstance(gate_scope, dict)
                    or gate_scope.get("parent_state") not in {"active-sealed", "terminal-after-leaf-seal"}
                    or (gate_scope.get("parent_state") == "terminal-after-leaf-seal"
                        and arm_parent_state != "terminal-after-leaf-seal")
                    or safety_path != pathlib.Path(
                        f"/run/systemd/system.control/{scope_unit}.d/zzzy-arc-recovery-writer-scope-safety.conf"
                    ) or safety_path.is_symlink() or not stat_module.S_ISREG(safety_details.st_mode)
                    or safety_details.st_uid != 0 or safety_details.st_gid != 0
                    or safety_details.st_mode & 0o022
                    or hashlib.sha256(safety_path.read_bytes()).hexdigest()
                    != scope.get("scope_runtime_safety_sha256")):
                raise SystemExit("live detached scope safety contract differs")
            writer_state = sealed_pid_state(writer_pid, writer_start)
            writer_leaf_state = cgroup_state(role_entries["writer"])
            if writer_leaf_state == "frozen":
                if cgroup_local_freeze(role_entries["writer"]) != 1:
                    raise SystemExit("detached writer leaf lost its local freeze request")
            elif writer_leaf_state == "thawed":
                if not writer_thaw_started or cgroup_local_freeze(role_entries["writer"]) != 0:
                    raise SystemExit("detached writer leaf thaw is not journal-authorized")
            elif writer_leaf_state == "disappeared":
                if writer_state == "same" or term_progress("30-writer") not in {"confirmed", "terminal"}:
                    raise SystemExit("detached writer leaf disappeared without exact terminal progress")
            else:
                raise SystemExit("detached writer leaf has an unsupported phase state")
            if writer_state == "same":
                if (writer_leaf_state not in ({"frozen", "thawed"} if writer_thaw_started else {"frozen"})
                        or subtree_pids(role_entries["writer"]) != [writer_pid]
                        or unified_cgroup(writer_pid) != role_entries["writer"]["path"]):
                    raise SystemExit("live detached writer leaf membership/phase changed")
                parent_path = pathlib.Path("/sys/fs/cgroup") / writer_parent_scope_cgroup["path"].lstrip("/")
                parent_details = parent_path.lstat()
                if (parent_path.is_symlink() or not parent_path.is_dir()
                        or parent_details.st_dev != writer_parent_scope_cgroup["device"]
                        or parent_details.st_ino != writer_parent_scope_cgroup["inode"]):
                    raise SystemExit("live detached parent scope inode changed")
            scope_active = systemctl_value(scope_unit, "ActiveState") == "active"
            if scope_active:
                if arm_parent_state == "terminal-after-leaf-seal":
                    raise SystemExit("detached parent scope reactivated after durable terminal state")
                for name, wanted in properties.items():
                    observed = systemctl_value(scope_unit, name) or ("0" if name == "Job" else "")
                    if observed != wanted: raise SystemExit(f"live detached scope property changed: {name}")
                current_sources = []
                for header in re.findall(rb"(?m)^# (/[^\n]+)$", subprocess.check_output(["systemctl", "cat", scope_unit])):
                    source = pathlib.Path(header.decode("utf-8")); details = source.lstat()
                    if (source.is_symlink() or not stat_module.S_ISREG(details.st_mode)
                            or details.st_uid != 0 or details.st_gid != 0 or details.st_mode & 0o022):
                        raise SystemExit("live detached scope source is unsafe")
                    current_sources.append({"path": str(source), "sha256": digest(source)})
                if current_sources != sources:
                    raise SystemExit("live detached scope source manifest changed")
            elif (systemctl_value(scope_unit, "ActiveState") not in {"inactive", "failed"}
                    or systemctl_value(scope_unit, "MainPID") not in {"", "0"}
                    or not no_job(scope_unit)
                    or systemctl_value(scope_unit, "InvocationID") not in {"", scope["scope_invocation_id"]}
                    or systemctl_value(scope_unit, "ControlGroup") not in {"", writer_parent_scope_cgroup["path"]}):
                raise SystemExit("terminal detached parent scope lost sealed provenance")
    else:
        for unit in units:
            mask = pathlib.Path(f"/run/systemd/system.control/{unit}")
            if mask.exists() or mask.is_symlink():
                raise SystemExit(f"sealed-boot volatile control mask survived reboot: {unit}")
            if ((unit.endswith(".service") and systemctl_value(unit, "MainPID") != "0")
                    or systemctl_value(unit, "ActiveState") not in {"inactive", "failed"}
                    or not no_job(unit)):
                raise SystemExit(f"post-reboot legacy unit is not condition-fenced: {unit}")
            verify_post_reboot_condition(unit)

check_fence()
current_boot_id = pathlib.Path("/proc/sys/kernel/random/boot_id").read_text().strip()
stable_path = root / "40-stable-inactive.json"
thaw_intent_path = root / "50-cgroups-thaw-intent.json"
thawed_path = root / "50-cgroups-thawed.json"

if stable_path.exists():
    require_absent_snapshot()
    time.sleep(0.5)
    require_absent_snapshot()
    if current_boot_id != boot_id:
        event("16-post-stop-reboot-revalidated.json", {
            "schema": "arc.recovery.pidfd-event.v1", "target": "legacy-writer",
            "outcome": "post-stop reboot observed; durable stable-inactive proof revalidated; no stale PID signaled",
        })
    raise SystemExit(0)

if current_boot_id != boot_id:
    require_absent_snapshot()
    time.sleep(0.5)
    require_absent_snapshot()
    event("15-reboot-reconciled.json", {
        "schema": "arc.recovery.pidfd-event.v1", "target": "legacy-writer",
        "outcome": "sealed-boot-ended; no stale PID signaled; fenced services and writer absent",
    })
    event("40-stable-inactive.json", {
        "schema": "arc.recovery.pidfd-event.v1", "target": "legacy-writer",
        "outcome": "two-stable-inactive-checks",
    })
    raise SystemExit(0)

if "writer" in role_entries:
    # Detached mode is deliberately two-stage.  The supervisor remains frozen
    # and unsignaled until the writer is durably terminal, so even a default
    # SIGTERM disposition cannot remove containment while the writer can run.
    supervisor_intent_path = root / "53-supervisor-cgroup-thaw-intent.json"
    writer_terminal_path = root / "52-writer-terminal-before-supervisor-thaw.json"
    if not thaw_intent_path.exists():
        ensure_term(writer_target)
        check_fence()
        write_or_verify_thaw_intent(
            thaw_intent_path, "detached-writer-first",
            [role_entries["writer"]], [writer_target],
        )
    write_or_verify_thaw_intent(
        thaw_intent_path, "detached-writer-first",
        [role_entries["writer"]], [writer_target],
    )
    thaw(role_entries["writer"], thaw_intent_path)
    writer_deadline = time.monotonic() + 120
    while True:
        record_target_exits([writer_target])
        writer_state = sealed_pid_state(writer_pid, writer_start)
        writer_matches = matching_processes(
            writer_executable_path, writer_executable_sha, writer_argv_sha,
        )
        cgroup_status = cgroup_state(role_entries["writer"])
        writer_members = [] if cgroup_status == "disappeared" else subtree_pids(role_entries["writer"])
        supervisor_status = cgroup_state(role_entries["supervisor"])
        # An unsignaled supervisor disappearance has no durable causal/ordering
        # proof tying it to the still-contained writer.  Do not pretend that is
        # a valid v2 containment outcome: fail closed before publishing 52.
        if (supervisor_status != "frozen"
                or cgroup_local_freeze(role_entries["supervisor"]) != 1
                or term_progress("20-supervisor") != "missing"):
            raise SystemExit("supervisor lost frozen/unsignaled containment before writer terminal proof")
        if writer_state != "same" and not writer_matches and not writer_members:
            time.sleep(0.5)
            if (sealed_pid_state(writer_pid, writer_start) != "same"
                    and not matching_processes(writer_executable_path, writer_executable_sha, writer_argv_sha)
                    and (cgroup_state(role_entries["writer"]) == "disappeared"
                         or not subtree_pids(role_entries["writer"]))):
                break
        if time.monotonic() >= writer_deadline:
            raise SystemExit("detached writer did not exit before supervisor thaw; recovery SIGKILL is forbidden")
        time.sleep(0.25)
    if not writer_terminal_path.exists():
        supervisor_state = sealed_pid_state(supervisor_pid, supervisor_start)
        supervisor_status = cgroup_state(role_entries["supervisor"])
        if (supervisor_status != "frozen"
                or cgroup_local_freeze(role_entries["supervisor"]) != 1
                or term_progress("20-supervisor") != "missing"):
            raise SystemExit("supervisor containment changed before writer terminal publication")
        containment_outcome = (
            "sealed-supervisor-live-and-frozen" if supervisor_state == "same"
            else "sealed-supervisor-terminal-cgroup-frozen"
        )
        event(writer_terminal_path.name, {
            "schema": "arc.recovery.detached-writer-terminal.v2",
            "writer_pid": writer_pid, "writer_start_ticks": writer_start,
            "writer_cgroup": role_entries["writer"],
            "writer_thaw_intent_sha256": hashlib.sha256(thaw_intent_path.read_bytes()).hexdigest(),
            "writer_thaw_complete_sha256": hashlib.sha256(
                thaw_progress_path(role_entries["writer"]).read_bytes()
            ).hexdigest(),
            "stable_absence_checks": 2,
            "supervisor_containment": {
                "supervisor_pid": supervisor_pid, "supervisor_start_ticks": supervisor_start,
                "supervisor_cgroup": role_entries["supervisor"],
                "outcome": containment_outcome, "cgroup_disposition_sha256": None,
                "term_progress_before_writer_terminal": "missing",
            },
            "recovery_sigkill_sent": False,
        })
    _writer_terminal, writer_terminal_raw = load_writer_terminal(required=True)
    if not supervisor_intent_path.exists():
        ensure_term(supervisor_targets[0])
        check_fence()
        write_or_verify_thaw_intent(
            supervisor_intent_path, "detached-supervisor-after-writer-terminal",
            [role_entries["supervisor"]], supervisor_targets,
            hashlib.sha256(writer_terminal_raw).hexdigest(),
        )
    write_or_verify_thaw_intent(
        supervisor_intent_path, "detached-supervisor-after-writer-terminal",
        [role_entries["supervisor"]], supervisor_targets,
        hashlib.sha256(writer_terminal_raw).hexdigest(),
    )
    thaw(role_entries["supervisor"], supervisor_intent_path)
else:
    if not thaw_intent_path.exists():
        for target in targets:
            ensure_term(target)
        check_fence()
        write_or_verify_thaw_intent(
            thaw_intent_path, "systemd-shared", cgroups, targets,
        )
    write_or_verify_thaw_intent(
        thaw_intent_path, "systemd-shared", cgroups, targets,
    )
    thaw(role_entries["supervisor"], thaw_intent_path)
per_cgroup_progress = {
    entry["role"]: hashlib.sha256(thaw_progress_path(entry).read_bytes()).hexdigest()
    for entry in cgroups
}
event("50-cgroups-thawed.json", {
    "schema": "arc.recovery.cgroups-thawed.v1",
    "frozen_context_sha256": hashlib.sha256(frozen_raw).hexdigest(),
    "cgroups": cgroups,
    "per_cgroup_progress_sha256": per_cgroup_progress,
    "all_cgroups_thawed": True,
    "no_signal_replayed_after_own_stage_thaw_intent": True,
})

deadline = time.monotonic() + 120
while True:
    record_target_exits()
    try:
        require_absent_snapshot()
    except SystemExit:
        pass
    else:
        time.sleep(0.5)
        record_target_exits()
        require_absent_snapshot()
        break
    if time.monotonic() >= deadline:
        raise SystemExit("legacy targets did not exit after cgroup thaw; recovery SIGKILL is forbidden")
    time.sleep(0.25)
os.sync()
event("40-stable-inactive.json", {
    "schema": "arc.recovery.pidfd-event.v1", "target": "legacy-writer",
    "outcome": "two-stable-inactive-checks",
})
PY
}

reconcile_known_stop_partials() {
    local root="$1"
    python3 - "$root" <<'PY'
import os
import pathlib
import stat
import sys

root = pathlib.Path(sys.argv[1])
names = {
    ".01-prefreeze-runtime-safety-intent.json.partial",
    ".02-fast-cgroup-freeze-intent.json.partial",
    ".02-supervisor-cgroup-frozen.json.partial",
    ".02-writer-parent-cgroup-frozen.json.partial",
    ".02-writer-leaf-move-intent.json.partial",
    ".02-writer-cgroup-frozen.json.partial",
    ".02-writer-parent-released.json.partial",
    ".03-fast-cgroups-frozen.json.partial",
    ".04-pre-fence-quiesce-intent.json.partial",
    ".05-cgroups-frozen.json.partial",
    ".06-pre-mask-activation-gate.json.partial",
    ".06-restart-barrier-armed.json.partial",
    ".06-supervisor-cgroup-disappeared.json.partial",
    ".06-supervisor-cgroup-path-reused.json.partial",
    ".06-writer-cgroup-disappeared.json.partial",
    ".06-writer-cgroup-path-reused.json.partial",
    ".07-restart-barrier-committed.json.partial",
    ".10-fence-verified.json.partial",
    ".15-reboot-reconciled.json.partial",
    ".16-post-stop-reboot-revalidated.json.partial",
    ".40-stable-inactive.json.partial",
    ".50-cgroups-thaw-intent.json.partial",
    ".50-cgroups-thawed.json.partial",
    ".51-supervisor-cgroup-thaw-complete.json.partial",
    ".51-writer-cgroup-thaw-complete.json.partial",
    ".52-writer-terminal-before-supervisor-thaw.json.partial",
    ".53-supervisor-cgroup-thaw-intent.json.partial",
    ".stop.context.partial",
    ".stop.armed.partial",
}
for prefix in ("20-supervisor", "30-writer"):
    for suffix in (
        "term-intent", "term-sent", "term-pending-observed",
        "term-replay-safe", "exited", "reconciled-exited",
    ):
        names.add(f".{prefix}-{suffix}.json.partial")
removed = False
for name in sorted(names):
    path = root / name
    if not path.exists() and not path.is_symlink():
        continue
    mode = path.lstat().st_mode
    if path.is_symlink() or not stat.S_ISREG(mode):
        raise SystemExit(f"unsafe stop-publication partial: {name}")
    path.unlink()
    removed = True
if removed:
    descriptor = os.open(root, os.O_RDONLY | getattr(os, "O_DIRECTORY", 0))
    try:
        os.fsync(descriptor)
    finally:
        os.close(descriptor)
PY
}

write_or_verify_stop_context() {
    local root="$1" capture_id="$2" node="$3" freeze_sha="$4" validator="$5"
    local stake="$6" data_dir="$7"
    python3 - "$root/stop.context" "$capture_id" "$node" "$freeze_sha" \
        "$validator" "$stake" "$data_dir" <<'PY'
import datetime
import json
import os
import pathlib
import re
import sys

output = pathlib.Path(sys.argv[1])
capture_id, node, freeze_sha, validator, stake, data_dir = sys.argv[2:]
root = output.parent
contract = json.loads((root / "evidence" / "writer-contract.json").read_text(encoding="utf-8"))
reboot_reconciled = (root / "15-reboot-reconciled.json").is_file()
def signal_state(prefix):
    if (root / f"{prefix}-term-sent.json").is_file():
        return "confirmed"
    if (root / f"{prefix}-term-intent.json").is_file():
        return "indeterminate"
    return "none"
supervisor_state = (
    "shared-with-writer"
    if contract.get("supervisor_main_pid") == contract.get("writer_pid")
    else signal_state("20-supervisor")
)
writer_state = signal_state("30-writer")
fixed = {
    "schema": "arc.recovery.offline-stop.v4",
    "capture_id": capture_id,
    "node": node,
    "persistent_restart_fence": "true",
    "stop_reconciliation": "reboot-fenced" if reboot_reconciled else "same-boot-frozen-cgroup-controller",
    "quiescence": "cgroup-v2-freeze",
    "supervisor_pidfd_sigterm_state": supervisor_state,
    "writer_pidfd_sigterm_state": writer_state,
    "recovery_sigkill_sent": "false",
    "exit_cause": "unknown",
    "freeze_plan_sha256": freeze_sha,
    "validator_address": validator,
    "stake": stake,
    "data_dir": data_dir,
}
order = [
    "schema", "capture_id", "node", "stopped_at", "persistent_restart_fence",
    "stop_reconciliation", "quiescence", "supervisor_pidfd_sigterm_state", "writer_pidfd_sigterm_state", "recovery_sigkill_sent",
    "exit_cause", "freeze_plan_sha256",
    "validator_address", "stake", "data_dir",
]
if output.exists():
    if output.is_symlink():
        raise SystemExit("stop context is a symlink")
    observed = {}
    for line in output.read_text(encoding="utf-8").splitlines():
        key, separator, value = line.partition("=")
        if not separator or key in observed:
            raise SystemExit("stop context is malformed")
        observed[key] = value
    if set(observed) != set(order) or any(observed.get(key) != value for key, value in fixed.items()):
        raise SystemExit("existing stop context differs from durable stop journal")
    if not re.fullmatch(r"[0-9]{4}-[0-9]{2}-[0-9]{2}T[0-9]{2}:[0-9]{2}:[0-9]{2}Z", observed["stopped_at"]):
        raise SystemExit("existing stop context timestamp is malformed")
    expected = "".join(f"{key}={observed[key]}\n" for key in order)
    if output.read_text(encoding="utf-8") != expected:
        raise SystemExit("existing stop context is not canonical")
else:
    fixed["stopped_at"] = datetime.datetime.now(datetime.timezone.utc).replace(microsecond=0).isoformat().replace("+00:00", "Z")
    payload = "".join(f"{key}={fixed[key]}\n" for key in order).encode()
    temporary = output.with_name(f".{output.name}.partial")
    if temporary.exists() or temporary.is_symlink():
        if temporary.is_symlink() or not temporary.is_file():
            raise SystemExit("unsafe stop context partial")
        temporary.unlink()
    descriptor = os.open(temporary, os.O_WRONLY | os.O_CREAT | os.O_EXCL | getattr(os, "O_NOFOLLOW", 0), 0o400)
    with os.fdopen(descriptor, "wb") as handle:
        handle.write(payload); handle.flush(); os.fsync(handle.fileno())
    os.rename(temporary, output)
directory = os.open(output.parent, os.O_RDONLY | getattr(os, "O_DIRECTORY", 0))
try:
    os.fsync(directory)
finally:
    os.close(directory)
PY
}

fsync_recovery_tree() {
    local root="$1"
    python3 - "$root" <<'PY'
import os
import pathlib
import stat
import sys

root = pathlib.Path(sys.argv[1])
directories = []
for base, dirs, files in os.walk(root, followlinks=False):
    base_path = pathlib.Path(base)
    directories.append(base_path)
    for name in dirs:
        if (base_path / name).is_symlink():
            raise SystemExit("recovery journal contains a symlink directory")
    for name in files:
        path = base_path / name
        mode = path.lstat().st_mode
        if not stat.S_ISREG(mode) or path.is_symlink():
            raise SystemExit("recovery journal contains a non-regular file")
        descriptor = os.open(path, os.O_RDONLY | getattr(os, "O_NOFOLLOW", 0))
        try:
            os.fsync(descriptor)
        finally:
            os.close(descriptor)
for path in sorted(directories, key=lambda item: len(item.parts), reverse=True):
    descriptor = os.open(path, os.O_RDONLY | getattr(os, "O_DIRECTORY", 0))
    try:
        os.fsync(descriptor)
    finally:
        os.close(descriptor)
parent = os.open(root.parent, os.O_RDONLY | getattr(os, "O_DIRECTORY", 0))
try:
    os.fsync(parent)
finally:
    os.close(parent)
PY
}

# Canonical verifier for the direct-cgroup-v2 offline-stop.v4 journal.
verify_stop_journal_semantics() {
    local root="$1" capture_id="$2" node="$3" freeze_sha="$4"
    python3 - "$root" "$capture_id" "$node" "$freeze_sha" <<'PY'
import hashlib
import json
import pathlib
import re
import stat
import sys

root = pathlib.Path(sys.argv[1]); capture_id, node, freeze_sha = sys.argv[2:]
hex64 = re.compile(r"[0-9a-f]{64}")

def canonical(value):
    return (json.dumps(value, sort_keys=True, separators=(",", ":")) + "\n").encode()

def regular(path, required=True):
    try: mode = path.lstat().st_mode
    except FileNotFoundError:
        if required: raise SystemExit(f"required durable stop evidence is missing: {path.name}")
        return False
    if path.is_symlink() or not stat.S_ISREG(mode):
        raise SystemExit(f"durable stop evidence is unsafe: {path.name}")
    return True

def load(name, schema, required=True):
    path = root / name
    if not regular(path, required): return None, None
    raw = path.read_bytes(); value = json.loads(raw)
    if not isinstance(value, dict) or value.get("schema") != schema or raw != canonical(value):
        raise SystemExit(f"durable stop evidence differs: {name}")
    return value, raw

def sha(raw): return hashlib.sha256(raw).hexdigest()

contract, contract_raw = load("evidence/writer-contract.json", "arc.recovery.exact-writer.v3")
if contract.get("freeze_plan_sha256") != freeze_sha:
    raise SystemExit("writer contract does not bind the freeze plan")
context_object = contract.get("supervisor_context")
if not isinstance(context_object, dict) or context_object.get("schema") != "arc.recovery.supervisor-context.v1":
    raise SystemExit("writer contract supervisor context is malformed")
if sha(canonical(context_object)) != contract.get("supervisor_context_sha256"):
    raise SystemExit("writer contract supervisor context hash differs")

safety, safety_raw = load("01-prefreeze-runtime-safety-intent.json", "arc.recovery.prefreeze-runtime-safety-intent.v1")
expected_safety_keys = {
    "schema", "capture_id", "node", "freeze_plan_sha256", "boot_id",
    "supervisor_unit", "supervisor_main_pid", "supervisor_context_sha256",
    "runtime_dropin_path", "runtime_dropin_sha256", "pre_recovery_oom_policy",
    "pre_recovery_unit_configuration_sha256", "pre_recovery_unit_sources",
}
if (set(safety) != expected_safety_keys or safety["capture_id"] != capture_id
        or safety["node"] != node or safety["freeze_plan_sha256"] != freeze_sha
        or safety["boot_id"] != contract["boot_id"]
        or safety["supervisor_unit"] != contract["supervisor_unit"]
        or safety["supervisor_main_pid"] != contract["supervisor_main_pid"]
        or safety["supervisor_context_sha256"] != contract["supervisor_context_sha256"]
        or safety["runtime_dropin_path"] != (
            f"/run/systemd/system/{contract['supervisor_unit']}.d/"
            "zzzy-arc-recovery-prefreeze-safety.conf"
        )
        or safety["runtime_dropin_sha256"] != sha(
            b"[Unit]\nRefuseManualStart=yes\nRefuseManualStop=yes\nIgnoreOnIsolate=yes\nStopWhenUnneeded=no\n"
            b"BindsTo=\nPartOf=\nPropagatesStopTo=\nOnFailure=\nOnSuccess=\n"
            b"FailureAction=none\nSuccessAction=none\nJobTimeoutAction=none\n\n"
            b"[Service]\nExecReload=\nExecStop=\nExecStopPost=\nRestart=no\nKillMode=process\n"
            b"SendSIGKILL=no\nSendSIGHUP=no\nOOMPolicy=continue\nWatchdogSec=0\nRuntimeMaxSec=infinity\n"
        )
        or safety["pre_recovery_oom_policy"] not in {"stop", "continue"}
        or context_object.get("unit_configuration_sha256") != safety["pre_recovery_unit_configuration_sha256"]
        or context_object.get("automatic_lifecycle", {}).get("OOMPolicy") != safety["pre_recovery_oom_policy"]
        or not all(hex64.fullmatch(safety[key]) for key in (
            "runtime_dropin_sha256", "pre_recovery_unit_configuration_sha256",
        ))
        or not isinstance(context_object.get("automatic_lifecycle"), dict)
        or not isinstance(safety["pre_recovery_unit_sources"], list)
        or not safety["pre_recovery_unit_sources"]):
    raise SystemExit("prefreeze runtime safety intent fields differ")
source_paths = set()
for row in safety["pre_recovery_unit_sources"]:
    if (not isinstance(row, dict) or set(row) != {"path", "sha256"}
            or not isinstance(row["path"], str) or not row["path"].startswith("/")
            or not hex64.fullmatch(row["sha256"])):
        raise SystemExit("prefreeze unit source manifest is malformed")
    if row["path"] in source_paths:
        raise SystemExit("prefreeze unit source manifest contains a duplicate")
    source_paths.add(row["path"])

fast, fast_raw = load("02-fast-cgroup-freeze-intent.json", "arc.recovery.fast-cgroup-freeze-intent.v1")
expected_fast_keys = {
    "schema", "capture_id", "node", "freeze_plan_sha256", "boot_id",
    "supervisor_unit", "cgroups", "pre_recovery_unit_configuration_sha256",
    "prefreeze_runtime_safety_intent_sha256", "supervisor", "writer",
}
if (set(fast) != expected_fast_keys or fast["capture_id"] != capture_id
        or fast["node"] != node or fast["freeze_plan_sha256"] != freeze_sha
        or fast["boot_id"] != contract["boot_id"]
        or fast["supervisor_unit"] != contract["supervisor_unit"]
        or fast["pre_recovery_unit_configuration_sha256"] != safety["pre_recovery_unit_configuration_sha256"]
        or fast["prefreeze_runtime_safety_intent_sha256"] != sha(safety_raw)):
    raise SystemExit("fast cgroup-freeze intent fields differ")

expected_supervisor = {
    "pid": contract["supervisor_main_pid"], "start_ticks": contract["supervisor_start_ticks"],
    "executable_path": contract["supervisor_executable_path"],
    "executable_sha256": contract["supervisor_executable_sha256"],
    "argv_sha256": contract["supervisor_argv_sha256"],
    "context_sha256": contract["supervisor_context_sha256"],
}
expected_writer = {
    "pid": contract["writer_pid"], "start_ticks": contract["writer_start_ticks"],
    "cgroup_sha256": contract["writer_cgroup_sha256"],
    "supervision_mode": contract["writer_supervision_mode"],
    "executable_path": contract["executable_path"],
    "executable_sha256": contract["executable_sha256"],
    "argv_sha256": contract["argv_sha256"],
}
fast_targets = fast.get("cgroups")
fast_roles = [entry.get("role") for entry in fast_targets] if isinstance(fast_targets, list) else []
if fast_roles not in (["supervisor"], ["supervisor", "writer-parent"]):
    raise SystemExit("fast cgroup role set differs")
for entry in fast_targets:
    if (not isinstance(entry, dict) or set(entry) != {"role", "path", "device", "inode"}
            or not isinstance(entry["path"], str) or not entry["path"].startswith("/")
            or entry["path"] == "/"
            or not all(isinstance(entry[key], int) and not isinstance(entry[key], bool) for key in ("device", "inode"))):
        raise SystemExit("fast cgroup identity is malformed")
if ((contract["writer_supervision_mode"] == "systemd-unit" and fast_roles != ["supervisor"])
        or (contract["writer_supervision_mode"] == "detached-root-session"
            and fast_roles != ["supervisor", "writer-parent"])):
    raise SystemExit("fast cgroup role set differs from writer supervision mode")

scope_fields = {
    "scope_unit", "scope_invocation_id", "scope_runtime_safety_path",
    "scope_runtime_safety_sha256", "scope_runtime_sources", "scope_properties",
    "parent_scope_cgroup", "recovery_leaf_path",
}
fast_writer = fast.get("writer")
if (fast["supervisor"] != expected_supervisor or not isinstance(fast_writer, dict)
        or set(fast_writer) != set(expected_writer) | scope_fields
        or any(fast_writer.get(key) != value for key, value in expected_writer.items())):
    raise SystemExit("fast cgroup-freeze process identities differ")
if fast_roles == ["supervisor"]:
    if any(fast_writer[key] is not None for key in scope_fields):
        raise SystemExit("systemd-unit writer has a detached-scope contract")
else:
    scope_unit = fast_writer["scope_unit"]
    scope_path = fast_writer["scope_runtime_safety_path"]
    scope_sha = fast_writer["scope_runtime_safety_sha256"]
    scope_sources = fast_writer["scope_runtime_sources"]
    scope_properties = fast_writer["scope_properties"]
    scope_property_names = {
        "Names", "Id", "Following", "LoadState", "Transient", "FragmentPath",
        "ActiveState", "SubState", "Job", "InvocationID", "ControlGroup", "Controller",
        "DefaultDependencies",
        "RefuseManualStop", "IgnoreOnIsolate", "StopWhenUnneeded", "Slice", "Requires",
        "Wants", "RequiresMountsFor", "Conflicts", "Before", "After", "BindsTo", "PartOf",
        "Upholds", "PropagatesStopTo", "StopPropagatedFrom", "BoundBy", "ConflictedBy",
        "UpheldBy", "OnFailure", "OnSuccess", "OnFailureOf", "OnSuccessOf", "SuccessAction",
        "FailureAction", "JobTimeoutAction", "KillMode", "SendSIGKILL", "SendSIGHUP",
        "FinalKillSignal", "OOMPolicy", "RuntimeMaxUSec", "TimeoutStopUSec",
    }
    scope_safety = (
        b"[Unit]\nDefaultDependencies=no\nRefuseManualStop=yes\nIgnoreOnIsolate=yes\nStopWhenUnneeded=no\n"
        b"BindsTo=\nPartOf=\nPropagatesStopTo=\nConflicts=\nUpholds=\nOnFailure=\nOnSuccess=\n"
        b"FailureAction=none\nSuccessAction=none\nJobTimeoutAction=none\n\n"
        b"[Scope]\nKillMode=process\nSendSIGKILL=no\nSendSIGHUP=no\nOOMPolicy=continue\n"
        b"RuntimeMaxSec=infinity\nTimeoutStopSec=infinity\n"
    )
    if (not re.fullmatch(r"session-[1-9][0-9]*\.scope", scope_unit or "")
            or not re.fullmatch(r"[0-9a-f]{32}", fast_writer["scope_invocation_id"] or "")
            or scope_path != f"/run/systemd/system.control/{scope_unit}.d/zzzy-arc-recovery-writer-scope-safety.conf"
            or scope_sha != sha(scope_safety) or not isinstance(scope_sources, list) or not scope_sources
            or not isinstance(scope_properties, dict) or set(scope_properties) != scope_property_names
            or any(not isinstance(value, str) for value in scope_properties.values())):
        raise SystemExit("detached writer scope contract is malformed")
    source_paths = set()
    for row in scope_sources:
        if (not isinstance(row, dict) or set(row) != {"path", "sha256"}
                or not isinstance(row["path"], str) or not row["path"].startswith("/")
                or not hex64.fullmatch(row["sha256"]) or row["path"] in source_paths):
            raise SystemExit("detached writer scope source manifest is malformed")
        source_paths.add(row["path"])
    requires = set(scope_properties["Requires"].split())
    after = set(scope_properties["After"].split())
    fixed_after = {
        "user-runtime-dir@0.service", "user@0.service", "user-0.slice",
        "systemd-logind.service",
    }
    cleared_lifecycle = {
        "BindsTo", "PartOf", "Upholds", "PropagatesStopTo", "StopPropagatedFrom",
        "BoundBy", "ConflictedBy", "UpheldBy", "OnFailure", "OnSuccess",
        "OnFailureOf", "OnSuccessOf",
    }
    if (scope_path not in source_paths
            or next(row["sha256"] for row in scope_sources if row["path"] == scope_path) != scope_sha
            or scope_properties["Names"] != scope_unit or scope_properties["Id"] != scope_unit
            or scope_properties["Following"] or scope_properties["LoadState"] != "loaded"
            or scope_properties["Transient"] != "yes"
            or scope_properties["FragmentPath"] != f"/run/systemd/transient/{scope_unit}"
            or scope_properties["ActiveState"] != "active"
            or scope_properties["SubState"] not in {"running", "abandoned"}
            or scope_properties["Job"] != "0"
            or scope_properties["InvocationID"] != fast_writer["scope_invocation_id"]
            or scope_properties["ControlGroup"] != fast_writer["parent_scope_cgroup"]["path"]
            or fast_writer["parent_scope_cgroup"] != next(
                entry for entry in fast_targets if entry["role"] == "writer-parent"
            )
            or fast_writer["recovery_leaf_path"] != (
                fast_writer["parent_scope_cgroup"]["path"].rstrip("/") + "/arc-recovery-writer"
            ) or scope_properties["Controller"]
            or scope_properties["DefaultDependencies"] != "no"
            or scope_properties["RefuseManualStop"] != "yes"
            or scope_properties["IgnoreOnIsolate"] != "yes"
            or scope_properties["StopWhenUnneeded"] != "no"
            or scope_properties["Slice"] != "user-0.slice"
            or "user-0.slice" not in requires
            or any(not unit.endswith(".mount") for unit in requires - {"user-0.slice"})
            or set(scope_properties["Wants"].split())
               != {"user-runtime-dir@0.service", "user@0.service"}
            or set(scope_properties["RequiresMountsFor"].split()) != {"/root"}
            or scope_properties["Conflicts"] or scope_properties["Before"]
            or not fixed_after.issubset(after)
            or any(not unit.endswith(".mount") for unit in after - fixed_after)
            or any(scope_properties[name] for name in cleared_lifecycle)
            or scope_properties["SuccessAction"] != "none"
            or scope_properties["FailureAction"] != "none"
            or scope_properties["JobTimeoutAction"] != "none"
            or scope_properties["KillMode"] != "process"
            or scope_properties["SendSIGKILL"] != "no"
            or scope_properties["SendSIGHUP"] != "no"
            or scope_properties["FinalKillSignal"] != "9"
            or scope_properties["OOMPolicy"] != "continue"
            or scope_properties["RuntimeMaxUSec"] != "infinity"
            or scope_properties["TimeoutStopUSec"] != "infinity"):
        raise SystemExit("detached writer scope identity/lifecycle contract differs")

fast_frozen, fast_frozen_raw = load("03-fast-cgroups-frozen.json", "arc.recovery.fast-cgroups-frozen.v1")
supervisor_target = fast_targets[0]
supervisor_progress, supervisor_progress_raw = load(
    "02-supervisor-cgroup-frozen.json", "arc.recovery.fast-cgroup-progress.v1",
)
if supervisor_progress != {
    "schema": "arc.recovery.fast-cgroup-progress.v1",
    "freeze_intent_sha256": sha(fast_raw), "role": "supervisor", "cgroup": supervisor_target,
    "freeze_order": fast_roles, "observed_frozen": True,
}:
    raise SystemExit("fast supervisor cgroup progress receipt differs")
if fast_roles == ["supervisor"]:
    cgroups = [supervisor_target]; roles = ["supervisor"]
    writer_parent_scope_cgroup = None; writer_recovery_leaf = None
    progress_sources_by_name = {"02-supervisor-cgroup-frozen.json": sha(supervisor_progress_raw)}
    progress_hashes = {"supervisor": sha(supervisor_progress_raw)}
else:
    writer_parent_scope_cgroup = fast_writer["parent_scope_cgroup"]
    parent_progress, parent_progress_raw = load(
        "02-writer-parent-cgroup-frozen.json", "arc.recovery.fast-cgroup-progress.v1",
    )
    if parent_progress != {
        "schema": "arc.recovery.fast-cgroup-progress.v1",
        "freeze_intent_sha256": sha(fast_raw), "role": "writer-parent",
        "cgroup": writer_parent_scope_cgroup, "freeze_order": fast_roles,
        "observed_frozen": True,
    }:
        raise SystemExit("fast writer-parent cgroup progress receipt differs")
    move, move_raw = load(
        "02-writer-leaf-move-intent.json", "arc.recovery.detached-writer-leaf-move-intent.v1",
    )
    if move != {
        "schema": "arc.recovery.detached-writer-leaf-move-intent.v1",
        "freeze_plan_sha256": freeze_sha, "boot_id": contract["boot_id"],
        "freeze_intent_sha256": sha(fast_raw),
        "writer_parent_frozen_receipt_sha256": sha(parent_progress_raw),
        "writer_pid": contract["writer_pid"], "writer_start_ticks": contract["writer_start_ticks"],
        "writer_executable_sha256": contract["executable_sha256"],
        "writer_argv_sha256": contract["argv_sha256"],
        "parent_scope_cgroup": writer_parent_scope_cgroup,
        "recovery_leaf_path": fast_writer["recovery_leaf_path"],
        "parent_observed_frozen": True,
    }:
        raise SystemExit("detached writer leaf-move intent differs")
    leaf, leaf_raw = load("02-writer-cgroup-frozen.json", "arc.recovery.fast-cgroup-progress.v1")
    writer_recovery_leaf = leaf.get("cgroup")
    if (not isinstance(writer_recovery_leaf, dict)
            or set(writer_recovery_leaf) != {"role", "path", "device", "inode"}
            or writer_recovery_leaf.get("role") != "writer"
            or leaf != {
                "schema": "arc.recovery.fast-cgroup-progress.v1",
                "freeze_intent_sha256": sha(fast_raw),
                "leaf_move_intent_sha256": sha(move_raw),
                "role": "writer", "cgroup": writer_recovery_leaf,
                "parent_scope_cgroup": writer_parent_scope_cgroup,
                "recovery_leaf_path": fast_writer["recovery_leaf_path"],
                "writer_pid": contract["writer_pid"],
                "writer_start_ticks": contract["writer_start_ticks"],
                "freeze_order": ["supervisor", "writer-parent", "writer"],
                "observed_local_freeze": 1, "observed_frozen": True,
                "observed_populated": True,
            }):
        raise SystemExit("detached writer recovery-leaf receipt differs")
    release, release_raw = load(
        "02-writer-parent-released.json", "arc.recovery.detached-writer-parent-release.v1",
    )
    if release != {
        "schema": "arc.recovery.detached-writer-parent-release.v1",
        "freeze_intent_sha256": sha(fast_raw), "leaf_sealed_receipt_sha256": sha(leaf_raw),
        "parent_scope_cgroup": writer_parent_scope_cgroup, "recovery_leaf": writer_recovery_leaf,
        "parent_local_freeze": 0, "leaf_local_freeze": 1, "leaf_observed_frozen": True,
    }:
        raise SystemExit("detached writer parent-release receipt differs")
    cgroups = [supervisor_target, writer_recovery_leaf]; roles = ["supervisor", "writer"]
    progress_sources_by_name = {
        "02-supervisor-cgroup-frozen.json": sha(supervisor_progress_raw),
        "02-writer-parent-cgroup-frozen.json": sha(parent_progress_raw),
        "02-writer-leaf-move-intent.json": sha(move_raw),
        "02-writer-cgroup-frozen.json": sha(leaf_raw),
        "02-writer-parent-released.json": sha(release_raw),
    }
    progress_hashes = {
        "supervisor": sha(supervisor_progress_raw), "writer-parent": sha(parent_progress_raw),
        "writer-leaf-move-intent": sha(move_raw), "writer": sha(leaf_raw),
        "writer-parent-release": sha(release_raw),
    }
if fast_frozen != {
    "schema": "arc.recovery.fast-cgroups-frozen.v1",
    "freeze_intent_sha256": sha(fast_raw), "cgroups": cgroups,
    "writer_parent_scope_cgroup": writer_parent_scope_cgroup,
    "writer_recovery_leaf": writer_recovery_leaf,
    "freeze_order": list(progress_hashes), "per_cgroup_progress_sha256": progress_hashes,
    "all_cgroups_frozen": True,
}:
    raise SystemExit("fast frozen-cgroup receipt differs")

prefence, prefence_raw = load("04-pre-fence-quiesce-intent.json", "arc.recovery.pre-fence-quiesce-intent.v1")
required_prefence = {
    "schema", "capture_id", "node", "freeze_plan_sha256", "boot_id",
    "supervisor_unit", "supervisor_invocation_id", "cgroups", "supervisor",
    "writer", "fast_freeze_intent_sha256", "fast_frozen_context_sha256",
    "writer_parent_scope_cgroup", "writer_recovery_leaf", "pre_freeze_subtrees",
}
expected_prefence_writer = {
    **expected_writer, "parent_scope_cgroup": writer_parent_scope_cgroup,
    "recovery_leaf": writer_recovery_leaf,
}
if (set(prefence) != required_prefence or prefence["capture_id"] != capture_id
        or prefence["node"] != node or prefence["freeze_plan_sha256"] != freeze_sha
        or prefence["boot_id"] != contract["boot_id"] or prefence["cgroups"] != cgroups
        or prefence["supervisor"] != expected_supervisor or prefence["writer"] != expected_prefence_writer
        or prefence["writer_parent_scope_cgroup"] != writer_parent_scope_cgroup
        or prefence["writer_recovery_leaf"] != writer_recovery_leaf
        or prefence["fast_freeze_intent_sha256"] != sha(fast_raw)
        or prefence["fast_frozen_context_sha256"] != sha(fast_frozen_raw)
        or not re.fullmatch(r"[0-9a-f]{32}", prefence["supervisor_invocation_id"])):
    raise SystemExit("pre-fence quiesce intent differs")

frozen, frozen_raw = load("05-cgroups-frozen.json", "arc.recovery.cgroups-frozen.v1")
required_frozen = {
    "schema", "pre_fence_intent_sha256", "capture_id", "node",
    "freeze_plan_sha256", "boot_id", "cgroups", "post_freeze_subtrees",
    "post_freeze_members", "post_freeze_processes", "signal_baseline",
    "writer_parent_scope_cgroup", "writer_recovery_leaf",
    "all_cgroups_frozen", "helper_and_ancestors_outside",
}
if (set(frozen) != required_frozen or frozen["pre_fence_intent_sha256"] != sha(prefence_raw)
        or frozen["capture_id"] != capture_id or frozen["node"] != node
        or frozen["freeze_plan_sha256"] != freeze_sha or frozen["boot_id"] != contract["boot_id"]
        or frozen["cgroups"] != cgroups or frozen["all_cgroups_frozen"] is not True
        or frozen["writer_parent_scope_cgroup"] != writer_parent_scope_cgroup
        or frozen["writer_recovery_leaf"] != writer_recovery_leaf
        or frozen["helper_and_ancestors_outside"] is not True):
    raise SystemExit("detailed frozen-cgroup receipt differs")
role_set = set(roles)
if set(frozen.get("post_freeze_subtrees", {})) != role_set or set(frozen.get("post_freeze_members", {})) != role_set:
    raise SystemExit("detailed frozen-cgroup role inventories differ")

processes = frozen.get("post_freeze_processes")
if not isinstance(processes, dict) or set(processes) != {"supervisor", "writer"}:
    raise SystemExit("post-freeze target inventory differs")
for name, expected, cgroup_role in (
    ("supervisor", expected_supervisor, "supervisor"),
    ("writer", expected_writer, "writer" if "writer" in role_set else "supervisor"),
):
    value = processes[name]
    exact = {
        "pid", "start_ticks", "ppid", "executable_path", "executable_sha256", "argv_sha256", "cgroup",
    }
    if (not isinstance(value, dict) or set(value) != exact
            or any(value.get(key) != expected[key] for key in (
                "pid", "start_ticks", "executable_path", "executable_sha256", "argv_sha256",
            ))
            or value["cgroup"] != next(entry["path"] for entry in cgroups if entry["role"] == cgroup_role)):
        raise SystemExit(f"post-freeze {name} identity differs")

for role in roles:
    members = frozen["post_freeze_members"][role]
    subtrees = frozen["post_freeze_subtrees"][role]
    if not isinstance(members, list) or not members or not isinstance(subtrees, list) or not subtrees:
        raise SystemExit("post-freeze member/subtree inventory is empty")
    member_pids = set()
    for member in members:
        if (not isinstance(member, dict) or set(member) != {
                "pid", "start_ticks", "ppid", "executable_path", "executable_sha256", "argv_sha256", "cgroup",
            } or member["pid"] in member_pids or member["cgroup"] != next(entry["path"] for entry in cgroups if entry["role"] == role)
                or not all(hex64.fullmatch(member[key]) for key in ("executable_sha256", "argv_sha256"))):
            raise SystemExit("post-freeze member identity is malformed")
        member_pids.add(member["pid"])
    subtree_pids = set()
    for row in subtrees:
        if (not isinstance(row, dict) or set(row) != {"relative", "device", "inode", "pids"}
                or not isinstance(row["pids"], list)):
            raise SystemExit("post-freeze subtree identity is malformed")
        subtree_pids.update(row["pids"])
    if subtree_pids != member_pids:
        raise SystemExit("post-freeze member/subtree PID sets differ")

term_bit = 1 << 14
baselines = frozen.get("signal_baseline")
if not isinstance(baselines, dict) or set(baselines) != {"supervisor", "writer"}:
    raise SystemExit("frozen signal baseline target set differs")
for name, tasks in baselines.items():
    if not isinstance(tasks, list) or not tasks: raise SystemExit("frozen task baseline is empty")
    tids = set(); unblocked = False
    for task in tasks:
        if (not isinstance(task, dict) or set(task) != {
                "tid", "SigIgn", "SigPnd", "ShdPnd", "SigBlk", "SigCgt", "TracerPid", "NSpid",
            } or not isinstance(task["tid"], int) or task["tid"] in tids
                or task["TracerPid"] != "0"
                or not all(re.fullmatch(r"[0-9a-f]+", task[key]) for key in ("SigIgn", "SigPnd", "ShdPnd", "SigBlk", "SigCgt"))
                or not re.fullmatch(r"[0-9]+(?: [0-9]+)*", task["NSpid"])):
            raise SystemExit("frozen task signal baseline is malformed")
        tids.add(task["tid"])
        if any(int(task[key], 16) & term_bit for key in ("SigIgn", "SigPnd", "ShdPnd")):
            raise SystemExit("frozen baseline had ignored or pending SIGTERM")
        unblocked = unblocked or not bool(int(task["SigBlk"], 16) & term_bit)
        if task["NSpid"].split()[-1] == "1" and not int(task["SigCgt"], 16) & term_bit:
            raise SystemExit("namespace-init target had default SIGTERM disposition")
    if not unblocked: raise SystemExit("SIGTERM was blocked in every frozen task")

network_quarantine, network_quarantine_raw = load(
    "08-network-quarantine.json", "arc.recovery.legacy-network-quarantine.v1",
)
if ((network_quarantine.get("capture_id"), network_quarantine.get("node"),
     network_quarantine.get("freeze_plan_sha256")) != (capture_id, node, freeze_sha)
        or network_quarantine.get("writer", {}).get("pid") != contract.get("writer_pid")
        or network_quarantine.get("writer", {}).get("start_ticks") != contract.get("writer_start_ticks")):
    raise SystemExit("network-quarantine receipt differs in sealed stop")
intent, intent_raw = load("stop.intent.json", "arc.recovery.stop-intent.v1")
if (set(intent) != {
        "schema", "capture_id", "node", "freeze_plan_sha256", "writer_contract_sha256",
        "pre_fence_intent_sha256", "frozen_context_sha256", "network_quarantine_sha256", "intent_at",
    } or intent["capture_id"] != capture_id or intent["node"] != node
        or intent["freeze_plan_sha256"] != freeze_sha
        or intent["writer_contract_sha256"] != sha(contract_raw)
        or intent["pre_fence_intent_sha256"] != sha(prefence_raw)
        or intent["frozen_context_sha256"] != sha(frozen_raw)
        or intent["network_quarantine_sha256"] != sha(network_quarantine_raw)
        or not re.fullmatch(r"[0-9]{4}-[0-9]{2}-[0-9]{2}T[0-9]{2}:[0-9]{2}:[0-9]{2}Z", intent["intent_at"])):
    raise SystemExit("durable stop intent differs")
regular(root / "stop.armed")
if (root / "stop.armed").read_text(encoding="ascii") != (
    "schema=arc.recovery.stop-armed.v1\n" f"intent_sha256={sha(intent_raw)}\n"
):
    raise SystemExit("stop armed marker differs")

units = (
    "arc-self-heal.service", "arc-node.service",
    "arc-node-update.service", "arc-node-update.timer",
)
barrier_bytes = b"[Unit]\nConditionPathExists=/etc/arc-recovery/legacy-start-allowed\n"
service_runtime_bytes = (
    b"[Unit]\nRefuseManualStart=yes\nRefuseManualStop=yes\nIgnoreOnIsolate=yes\nStopWhenUnneeded=no\n"
    b"BindsTo=\nPartOf=\nPropagatesStopTo=\nOnFailure=\nOnSuccess=\n"
    b"FailureAction=none\nSuccessAction=none\nJobTimeoutAction=none\n\n"
    b"[Service]\nExecReload=\nExecStop=\nExecStopPost=\nRestart=no\nKillMode=process\n"
    b"SendSIGKILL=no\nSendSIGHUP=no\nOOMPolicy=continue\nWatchdogSec=0\nRuntimeMaxSec=infinity\n"
)
timer_runtime_bytes = (
    b"[Unit]\nRefuseManualStart=yes\nRefuseManualStop=yes\nIgnoreOnIsolate=yes\nStopWhenUnneeded=no\n"
    b"BindsTo=\nPartOf=\nPropagatesStopTo=\nOnFailure=\nOnSuccess=\n"
    b"FailureAction=none\nSuccessAction=none\nJobTimeoutAction=none\n"
)
runtime_hashes = {
    unit: sha(timer_runtime_bytes if unit.endswith(".timer") else service_runtime_bytes)
    for unit in units
}
progress_sources = progress_sources_by_name
expected_gate_sources = {
    "02-fast-cgroup-freeze-intent.json": sha(fast_raw),
    "03-fast-cgroups-frozen.json": sha(fast_frozen_raw),
    "04-pre-fence-quiesce-intent.json": sha(prefence_raw),
    "05-cgroups-frozen.json": sha(frozen_raw),
    "08-network-quarantine.json": sha(network_quarantine_raw),
    "stop.intent.json": sha(intent_raw),
    "evidence/writer-contract.json": sha(contract_raw),
    f"/root/.arc-recovery-plans/{freeze_sha}/freeze.lock.json": freeze_sha,
    **progress_sources,
}
live_arm_path = root / "09-quarantine-restart-arm.json"
live_commit_path = root / "09-quarantine-restart-committed.json"
if (live_arm_path.exists() or live_arm_path.is_symlink()
        or live_commit_path.exists() or live_commit_path.is_symlink()):
    live_arm, live_arm_raw = load(
        "09-quarantine-restart-arm.json", "arc.recovery.quarantine-live-restart-arm.v1",
    )
    live_commit, live_commit_raw = load(
        "09-quarantine-restart-committed.json",
        "arc.recovery.quarantine-live-restart-committed.v1",
    )
    if (live_commit.get("quarantine_restart_arm_sha256") != sha(live_arm_raw)
            or live_commit.get("allow_marker_absent") is not True):
        raise SystemExit("live quarantine restart source chain differs")
    expected_gate_sources["09-quarantine-restart-arm.json"] = sha(live_arm_raw)
    expected_gate_sources["09-quarantine-restart-committed.json"] = sha(live_commit_raw)
gate, gate_raw = load("06-pre-mask-activation-gate.json", "arc.recovery.pre-mask-activation-gate.v1")
expected_gate_keys = {
    "schema", "freeze_plan_sha256", "sealed_boot_id", "selected_unit", "selected_main_pid",
    "source_sha256", "prepare_barrier_sha256", "allow_marker", "run_mount", "unit_path",
    "unit_states", "selected_lifecycle", "merged_unit_sources", "runtime_safety_sha256",
    "alternative_activation_closure", "detached_scope", "cgroups",
    "writer_parent_scope_cgroup", "writer_recovery_leaf", "all_cgroups_frozen",
    "volatile_control_masks_absent_when_published", "persistent_control_masks_absent",
}
if (set(gate) != expected_gate_keys or gate["freeze_plan_sha256"] != freeze_sha
        or gate["sealed_boot_id"] != contract["boot_id"]
        or gate["selected_unit"] != contract["supervisor_unit"]
        or gate["selected_main_pid"] != contract["supervisor_main_pid"]
        or gate["source_sha256"] != expected_gate_sources
        or not hex64.fullmatch(gate["prepare_barrier_sha256"])
        or gate["runtime_safety_sha256"] != runtime_hashes
        or gate["cgroups"] != cgroups
        or gate["writer_parent_scope_cgroup"] != writer_parent_scope_cgroup
        or gate["writer_recovery_leaf"] != writer_recovery_leaf
        or gate["all_cgroups_frozen"] is not True
        or gate["volatile_control_masks_absent_when_published"] is not True
        or gate["persistent_control_masks_absent"] is not True):
    raise SystemExit("pre-mask activation gate identity/hash chain differs")
marker = gate.get("allow_marker")
marker_payload = b"schema=arc.recovery.legacy-start-allow.v1\n"
if (not isinstance(marker, dict) or set(marker) != {
        "path", "sha256", "device", "inode", "uid", "gid", "mode", "size",
    } or marker["path"] != "/etc/arc-recovery/legacy-start-allowed"
        or marker["sha256"] != sha(marker_payload) or marker["uid"] != 0 or marker["gid"] != 0
        or marker["mode"] != 0o400 or marker["size"] != len(marker_payload)
        or any(isinstance(marker[key], bool) or not isinstance(marker[key], int)
               for key in ("device", "inode"))):
    raise SystemExit("pre-mask allow-marker identity differs")
run_mount = gate.get("run_mount")
if (not isinstance(run_mount, dict) or set(run_mount) != {
        "id", "major_minor", "mountpoint", "fstype", "source",
    } or run_mount["mountpoint"] != "/run" or run_mount["fstype"] != "tmpfs"
        or any(not isinstance(value, str) or not value for value in run_mount.values())):
    raise SystemExit("pre-mask /run mount projection differs")
unit_path = gate.get("unit_path")
required_unit_paths = ("/etc/systemd/system.control", "/run/systemd/system.control", "/etc/systemd/system")
if (not isinstance(unit_path, list) or not unit_path or len(unit_path) != len(set(unit_path))
        or any(path not in unit_path for path in required_unit_paths)
        or not (unit_path.index(required_unit_paths[0]) < unit_path.index(required_unit_paths[1])
                < unit_path.index(required_unit_paths[2]))):
    raise SystemExit("pre-mask systemd UnitPath projection differs")
state_keys = {
    "Names", "Id", "Following", "ActiveState", "SubState", "MainPID", "Job",
    "ControlGroup", "FreezerState", "InvocationID", "WantedBy", "enablement",
}
unit_states = gate.get("unit_states")
if not isinstance(unit_states, dict) or set(unit_states) != set(units):
    raise SystemExit("pre-mask unit-state inventory differs")
for unit, state in unit_states.items():
    if (not isinstance(state, dict) or set(state) != state_keys
            or state["Names"] != unit or state["Id"] != unit or state["Following"]
            or state["Job"] != "0"):
        raise SystemExit(f"pre-mask unit alias/job state differs: {unit}")
    if unit == contract["supervisor_unit"]:
        if (state["ActiveState"] != "active" or state["SubState"] != "running"
                or state["MainPID"] != str(contract["supervisor_main_pid"])
                or state["FreezerState"] not in {"running", "frozen"}
                or state["InvocationID"] != prefence["supervisor_invocation_id"]
                or state["enablement"] != "enabled"
                or "multi-user.target" not in state["WantedBy"].split()):
            raise SystemExit("pre-mask selected unit state differs")
    elif (state["ActiveState"] not in {"inactive", "failed"}
            or (unit.endswith(".service") and state["MainPID"] != "0")):
        raise SystemExit(f"pre-mask alternative unit state differs: {unit}")
alternative_closure = gate.get("alternative_activation_closure")
alternative_units = set(units) - {contract["supervisor_unit"]}
reverse_fields = {
    "Names", "Id", "Following", "RequiredBy", "WantedBy", "BoundBy",
    "UpheldBy", "TriggeredBy", "OnFailureOf", "OnSuccessOf",
}
if not isinstance(alternative_closure, dict) or set(alternative_closure) != alternative_units:
    raise SystemExit("pre-mask alternative activation closure differs")
for unit, row in alternative_closure.items():
    if (not isinstance(row, dict) or set(row) != {"enablement", "reverse_activation"}
            or row["enablement"] not in {
                "disabled", "masked", "masked-runtime", "static", "indirect",
                "generated", "transient", "not-found",
            }
            or not isinstance(row["reverse_activation"], dict)
            or set(row["reverse_activation"]) != reverse_fields
            or row["reverse_activation"]["Names"] != unit
            or row["reverse_activation"]["Id"] != unit
            or row["reverse_activation"]["Following"]):
        raise SystemExit(f"pre-mask alternative activation closure is malformed: {unit}")
expected_lifecycle = {
    "Restart": "no", "KillMode": "process", "SendSIGKILL": "no", "SendSIGHUP": "no",
    "IgnoreOnIsolate": "yes", "OOMPolicy": "continue",
    "WatchdogUSec": "0", "RuntimeMaxUSec": "infinity", "CanReload": "no",
    "ExecReload": "", "ExecStop": "", "ExecStopPost": "", "RefuseManualStart": "yes",
    "RefuseManualStop": "yes", "StopWhenUnneeded": "no", "BindsTo": "", "PartOf": "",
    "PropagatesStopTo": "", "StopPropagatedFrom": "", "ReloadPropagatedFrom": "",
    "OnFailure": "", "OnSuccess": "", "SuccessAction": "none", "FailureAction": "none",
    "JobTimeoutAction": "none",
}
if gate.get("selected_lifecycle") != expected_lifecycle:
    raise SystemExit("pre-mask selected lifecycle contract differs")
manifests = gate.get("merged_unit_sources")
if not isinstance(manifests, dict) or set(manifests) != set(units):
    raise SystemExit("pre-mask merged unit-source inventory differs")
for unit, rows in manifests.items():
    if not isinstance(rows, list) or not rows:
        raise SystemExit(f"pre-mask merged unit-source inventory is empty: {unit}")
    paths = set()
    for row in rows:
        if (not isinstance(row, dict) or set(row) != {"path", "sha256"}
                or not isinstance(row["path"], str) or not row["path"].startswith("/")
                or not hex64.fullmatch(row["sha256"]) or row["path"] in paths):
            raise SystemExit(f"pre-mask merged unit-source row differs: {unit}")
        paths.add(row["path"])
    runtime_path = f"/run/systemd/system/{unit}.d/zzzy-arc-recovery-prefreeze-safety.conf"
    if (runtime_path not in paths
            or next(row["sha256"] for row in rows if row["path"] == runtime_path) != runtime_hashes[unit]):
        raise SystemExit(f"pre-mask runtime source binding differs: {unit}")
gate_scope = gate.get("detached_scope")
gate_parent_state = None
expected_gate_scope = None
if roles != ["supervisor"]:
    gate_parent_state = gate_scope.get("parent_state") if isinstance(gate_scope, dict) else None
    expected_gate_scope = {
        "unit": fast_writer["scope_unit"], "properties": fast_writer["scope_properties"],
        "parent_state": gate_parent_state,
        "runtime_safety_path": fast_writer["scope_runtime_safety_path"],
        "runtime_safety_sha256": fast_writer["scope_runtime_safety_sha256"],
        "sources": fast_writer["scope_runtime_sources"],
    }
if (gate_scope != expected_gate_scope
        or (roles != ["supervisor"]
            and gate_parent_state not in {"active-sealed", "terminal-after-leaf-seal"})):
    raise SystemExit("pre-mask detached scope binding differs")

arm, arm_raw = load("06-restart-barrier-armed.json", "arc.recovery.restart-barrier-arm.v1")
arm_sources = {
    "02-fast-cgroup-freeze-intent.json": sha(fast_raw),
    "03-fast-cgroups-frozen.json": sha(fast_frozen_raw),
    "04-pre-fence-quiesce-intent.json": sha(prefence_raw),
    "05-cgroups-frozen.json": sha(frozen_raw),
    "06-pre-mask-activation-gate.json": sha(gate_raw),
    "stop.intent.json": sha(intent_raw),
    "evidence/writer-contract.json": sha(contract_raw),
    **progress_sources,
}
if live_arm_path.exists() or live_commit_path.exists():
    arm_sources["09-quarantine-restart-arm.json"] = sha(live_arm_raw)
    arm_sources["09-quarantine-restart-committed.json"] = sha(live_commit_raw)
expected_arm_keys = {
    "schema", "freeze_plan_sha256", "sealed_boot_id", "selected_unit", "selected_main_pid",
    "allow_marker_path", "allow_marker_sha256", "allow_marker_identity",
    "allow_marker_observed_present", "source_sha256", "persistent_start_barrier_sha256",
    "runtime_safety_sha256", "control_masks", "effective_control_masks",
    "pre_mask_activation_gate_sha256", "prepare_barrier_sha256", "detached_scope_safety",
    "cgroups", "writer_parent_scope_cgroup", "writer_recovery_leaf", "all_cgroups_frozen",
}
expected_marker_identity = {
    key: marker[key] for key in ("device", "inode", "uid", "gid", "mode", "size")
}
arm_scope = arm.get("detached_scope_safety")
arm_parent_state = None
expected_scope_arm = None
if expected_gate_scope is not None:
    arm_parent_state = arm_scope.get("parent_state") if isinstance(arm_scope, dict) else None
    expected_scope_arm = {
        "unit": fast_writer["scope_unit"], "properties": fast_writer["scope_properties"],
        "parent_state": arm_parent_state,
        "sources": fast_writer["scope_runtime_sources"],
        "runtime_safety_sha256": fast_writer["scope_runtime_safety_sha256"],
        "parent_scope_cgroup": writer_parent_scope_cgroup,
        "recovery_leaf": writer_recovery_leaf,
    }
if (set(arm) != expected_arm_keys or arm["freeze_plan_sha256"] != freeze_sha
        or arm["sealed_boot_id"] != contract["boot_id"]
        or arm["selected_unit"] != contract["supervisor_unit"]
        or arm["selected_main_pid"] != contract["supervisor_main_pid"]
        or arm["allow_marker_path"] != marker["path"]
        or arm["allow_marker_sha256"] != marker["sha256"]
        or arm["allow_marker_identity"] != expected_marker_identity
        or arm["allow_marker_observed_present"] is not True
        or arm["source_sha256"] != arm_sources
        or arm["persistent_start_barrier_sha256"] != {unit: sha(barrier_bytes) for unit in units}
        or arm["runtime_safety_sha256"] != runtime_hashes
        or arm["control_masks"] != {unit: "/dev/null" for unit in units}
        or arm["effective_control_masks"] is not True
        or arm["pre_mask_activation_gate_sha256"] != sha(gate_raw)
        or arm["prepare_barrier_sha256"] != gate["prepare_barrier_sha256"]
        or arm["detached_scope_safety"] != expected_scope_arm
        or (expected_scope_arm is not None and (
            arm_parent_state not in {"active-sealed", "terminal-after-leaf-seal"}
            or (gate_parent_state == "terminal-after-leaf-seal"
                and arm_parent_state != "terminal-after-leaf-seal")
        ))
        or arm["cgroups"] != cgroups
        or arm["writer_parent_scope_cgroup"] != writer_parent_scope_cgroup
        or arm["writer_recovery_leaf"] != writer_recovery_leaf
        or arm["all_cgroups_frozen"] is not True):
    raise SystemExit("four-unit restart-barrier arm differs")

barrier, barrier_raw = load("07-restart-barrier-committed.json", "arc.recovery.restart-barrier-committed.v2")
expected_barrier_keys = {
    "schema", "barrier_arm_sha256", "sealed_boot_id", "observed_boot_id",
    "allow_marker_path", "allow_marker_absent", "unlink_parent_fsynced", "durability_basis",
    "selected_unit", "selected_main_pid_on_sealed_boot", "reboot_requires_zero_pid_signals",
}
same_commit_boot = barrier.get("observed_boot_id") == contract["boot_id"]
if (set(barrier) != expected_barrier_keys or barrier["barrier_arm_sha256"] != sha(arm_raw)
        or barrier["sealed_boot_id"] != contract["boot_id"]
        or not isinstance(barrier["observed_boot_id"], str)
        or barrier["allow_marker_path"] != marker["path"]
        or barrier["allow_marker_absent"] is not True or barrier["unlink_parent_fsynced"] is not True
        or barrier["selected_unit"] != contract["supervisor_unit"]
        or barrier["selected_main_pid_on_sealed_boot"] != contract["supervisor_main_pid"]
        or (same_commit_boot and (
            barrier["durability_basis"] not in {
                "same-boot-unlink-parent-fsynced", "same-boot-reconciled-parent-fsync",
                "same-boot-preproof-unlink-parent-fsynced",
            } or barrier["reboot_requires_zero_pid_signals"] is not False
        )) or (not same_commit_boot and (
            barrier["durability_basis"] != "post-reboot-marker-absence-parent-fsync"
            or barrier["reboot_requires_zero_pid_signals"] is not True
        ))):
    raise SystemExit("durable restart-barrier v2 receipt differs")

legacy, legacy_raw = load("evidence/legacy-service-fence.json", "arc.recovery.legacy-service-fence.v5")
if legacy != {
    "schema": "arc.recovery.legacy-service-fence.v5", "freeze_plan_sha256": freeze_sha,
    "barrier_arm_sha256": sha(arm_raw), "barrier_commit_sha256": sha(barrier_raw),
    "selected_unit": contract["supervisor_unit"],
    "selected_main_pid_on_sealed_boot": contract["supervisor_main_pid"],
    "persistent_condition_only_start_barriers": True, "allow_marker_absent": True,
    "all_cgroups_frozen_at_commit": True,
    "effective_four_unit_control_masks_on_sealed_boot": True,
    "recovery_sigkill_allowed": False,
}:
    raise SystemExit("legacy-service fence v5 receipt differs")
fence, _ = load("10-fence-verified.json", "arc.recovery.fence-verified.v3")
if fence != {
    "schema": "arc.recovery.fence-verified.v3", "freeze_plan_sha256": freeze_sha,
    "barrier_arm_sha256": sha(arm_raw), "barrier_commit_sha256": sha(barrier_raw),
    "legacy_service_fence_sha256": sha(legacy_raw),
    "persistent_condition_only_start_barriers": True, "allow_marker_absent": True,
    "recovery_sigkill_allowed": False,
}:
    raise SystemExit("effective fence-verified v3 receipt differs")

reboot, _ = load("15-reboot-reconciled.json", "arc.recovery.pidfd-event.v1", required=False)
post_reboot, _ = load("16-post-stop-reboot-revalidated.json", "arc.recovery.pidfd-event.v1", required=False)
if reboot is not None and reboot != {
    "schema": "arc.recovery.pidfd-event.v1", "target": "legacy-writer",
    "outcome": "sealed-boot-ended; no stale PID signaled; fenced services and writer absent",
}: raise SystemExit("reboot reconciliation receipt differs")
if post_reboot is not None and post_reboot != {
    "schema": "arc.recovery.pidfd-event.v1", "target": "legacy-writer",
    "outcome": "post-stop reboot observed; durable stable-inactive proof revalidated; no stale PID signaled",
}: raise SystemExit("post-stop reboot receipt differs")

def load_event(path, schema="arc.recovery.pidfd-event.v1"):
    value, raw = load(path.name, schema, required=False)
    return value, raw

targets = ["30-writer"] + ([] if contract["supervisor_main_pid"] == contract["writer_pid"] else ["20-supervisor"])
signal_states = {"20-supervisor": "none", "30-writer": "none"}
signal_events = {}
for prefix in ("20-supervisor", "30-writer"):
    is_target = prefix in targets
    expected_pid = contract["supervisor_main_pid"] if prefix == "20-supervisor" else contract["writer_pid"]
    expected_start = contract["supervisor_start_ticks"] if prefix == "20-supervisor" else contract["writer_start_ticks"]
    paths = {
        suffix: root / f"{prefix}-{suffix}.json" for suffix in (
            "term-intent", "term-sent", "term-pending-observed", "term-replay-safe",
            "exited", "reconciled-exited",
        )
    }
    if not is_target and any(path.exists() or path.is_symlink() for path in paths.values()):
        raise SystemExit("shared supervisor has a second signal chain")
    if not is_target: continue
    values = {}
    for suffix, path in paths.items():
        schema = "arc.recovery.pidfd-term-intent.v1" if suffix == "term-intent" else "arc.recovery.pidfd-event.v1"
        values[suffix], _ = load_event(path, schema)
    signal_events[prefix] = values
    if values["term-intent"] is not None and values["term-intent"] != {
        "schema": "arc.recovery.pidfd-term-intent.v1", "target": prefix,
        "pid": expected_pid, "start_ticks": expected_start, "signal": "SIGTERM",
        "sigkill_allowed": False,
    }: raise SystemExit(f"{prefix} TERM intent differs")
    outcomes = {
        "term-sent": "SIGTERM-sent-via-pidfd-while-cgroup-frozen",
        "term-pending-observed": "SIGTERM-pending-observed-after-indeterminate-intent",
        "term-replay-safe": "frozen-baseline-had-no-SIGTERM; current-pending=false; one-send-safe",
        "exited": "exit-observed-after-durable-TERM-progress; recovery-sigkill-sent=false",
    }
    for suffix, outcome in outcomes.items():
        if values[suffix] is not None and values[suffix] != {
            "schema": "arc.recovery.pidfd-event.v1", "target": prefix, "outcome": outcome,
        }: raise SystemExit(f"{prefix} event differs: {suffix}")
    reconciled_outcomes = {
        "already-exited", "exited-after-indeterminate-TERM-intent",
        "original-exited; numeric-pid-reused; no signal sent to reused PID",
    }
    if values["reconciled-exited"] is not None and (
        values["reconciled-exited"].get("outcome") not in reconciled_outcomes
        or values["reconciled-exited"] != {
            "schema": "arc.recovery.pidfd-event.v1", "target": prefix,
            "outcome": values["reconciled-exited"]["outcome"],
        }
    ): raise SystemExit(f"{prefix} reconciliation differs")
    if any(values[suffix] is not None for suffix in ("term-sent", "term-pending-observed", "term-replay-safe")) and values["term-intent"] is None:
        raise SystemExit(f"{prefix} signal progress has no intent")
    if values["term-sent"] is not None and values["term-pending-observed"] is not None:
        raise SystemExit(f"{prefix} has contradictory TERM delivery receipts")
    if (values["term-replay-safe"] is not None and values["term-sent"] is None
            and values["term-pending-observed"] is None
            and values["reconciled-exited"] is None and reboot is None):
        raise SystemExit(f"{prefix} replay-safe receipt has no later durable resolution")
    if values["exited"] is not None and values["term-sent"] is None and values["term-pending-observed"] is None:
        raise SystemExit(f"{prefix} exit has no durable TERM delivery progress")
    if values["exited"] is not None and values["reconciled-exited"] is not None:
        raise SystemExit(f"{prefix} has two terminal outcomes")
    if reboot is None and values["exited"] is None and values["reconciled-exited"] is None:
        raise SystemExit(f"{prefix} has no terminal outcome")
    signal_states[prefix] = "confirmed" if values["term-sent"] is not None else (
        "indeterminate" if values["term-intent"] is not None else "none"
    )

postboot_cgroup_receipts = {}
for role in roles:
    entry = next(entry for entry in cgroups if entry["role"] == role)
    disappeared, _ = load(
        f"06-{role}-cgroup-disappeared.json", "arc.recovery.cgroup-disappeared.v1", required=False,
    )
    reused, _ = load(
        f"06-{role}-cgroup-path-reused.json",
        "arc.recovery.cgroup-reused-after-reboot.v1", required=False,
    )
    if disappeared is not None and reused is not None:
        raise SystemExit(f"cgroup has both disappeared and path-reused receipts: {role}")
    if disappeared is not None and disappeared != {
        "schema": "arc.recovery.cgroup-disappeared.v1", "role": role,
        "frozen_context_sha256": sha(frozen_raw),
        "cgroup": entry,
        "outcome": "path-absent; captured-members-and-exact-matches-absent",
        "recovery_sigkill_sent": False,
    }: raise SystemExit(f"disappeared cgroup receipt differs: {role}")
    if reused is not None:
        if (set(reused) != {
                "schema", "role", "frozen_context_sha256", "sealed_boot_id",
                "observed_boot_id", "sealed_cgroup", "observed_path",
                "observed_device", "observed_inode", "outcome", "recovery_sigkill_sent",
            } or reused["role"] != role or reused["frozen_context_sha256"] != sha(frozen_raw)
                or reused["sealed_boot_id"] != contract["boot_id"]
                or reused["observed_boot_id"] == contract["boot_id"]
                or not isinstance(reused["observed_boot_id"], str)
                or reused["sealed_cgroup"] != entry or reused["observed_path"] != entry["path"]
                or any(isinstance(reused[key], bool) or not isinstance(reused[key], int)
                       for key in ("observed_device", "observed_inode"))
                or reused["outcome"]
                   != "sealed-instance-gone; pathname-present-after-reboot; no signal-or-thaw sent"
                or reused["recovery_sigkill_sent"] is not False):
            raise SystemExit(f"rebooted cgroup path-reuse receipt differs: {role}")
        postboot_cgroup_receipts[role] = "reused"
    elif disappeared is not None:
        postboot_cgroup_receipts[role] = "disappeared"
if any(kind == "reused" for kind in postboot_cgroup_receipts.values()) and reboot is None and post_reboot is None:
    raise SystemExit("cgroup pathname reuse was recorded without reboot reconciliation")
if (reboot is not None or post_reboot is not None) and set(postboot_cgroup_receipts) != role_set:
    raise SystemExit("reboot reconciliation lacks a receipt for each sealed cgroup path")

thaw_intent, thaw_intent_raw = load("50-cgroups-thaw-intent.json", "arc.recovery.cgroup-thaw-intent.v2", required=False)
supervisor_thaw_intent, supervisor_thaw_intent_raw = load(
    "53-supervisor-cgroup-thaw-intent.json", "arc.recovery.cgroup-thaw-intent.v2", required=False,
)
thawed, _ = load("50-cgroups-thawed.json", "arc.recovery.cgroups-thawed.v1", required=False)
writer_terminal, writer_terminal_raw = load(
    "52-writer-terminal-before-supervisor-thaw.json",
    "arc.recovery.detached-writer-terminal.v2", required=False,
)
allowed_progress_suffixes = {
    "term-intent", "term-sent", "term-pending-observed",
    "term-replay-safe", "exited", "reconciled-exited",
}

def validate_recorded_progress(prefix, row):
    if (not isinstance(row, dict) or set(row) != {"state", "files"}
            or row["state"] not in {"confirmed", "indeterminate", "terminal"}
            or not isinstance(row["files"], dict)
            or not set(row["files"]).issubset(allowed_progress_suffixes)):
        raise SystemExit("cgroup thaw TERM receipt is malformed")
    present = set(row["files"])
    for suffix, expected_sha in row["files"].items():
        path = root / f"{prefix}-{suffix}.json"
        if (not isinstance(expected_sha, str) or not regular(path)
                or not hex64.fullmatch(expected_sha) or sha(path.read_bytes()) != expected_sha):
            raise SystemExit("cgroup thaw TERM receipt hash differs")
    if ({"term-sent", "term-pending-observed"} <= present
            or {"exited", "reconciled-exited"} <= present
            or (present & {"term-sent", "term-pending-observed", "term-replay-safe"}
                and "term-intent" not in present)):
        raise SystemExit("cgroup thaw TERM receipt is contradictory")
    if row["state"] == "confirmed":
        if (not {"term-intent", "term-sent"} <= present
                or present & {"term-pending-observed", "exited", "reconciled-exited"}
                or not present <= {"term-intent", "term-sent", "term-replay-safe"}):
            raise SystemExit("confirmed cgroup thaw TERM receipt differs")
    elif row["state"] == "indeterminate":
        if (not {"term-intent", "term-pending-observed"} <= present
                or present & {"term-sent", "exited", "reconciled-exited"}
                or not present <= {"term-intent", "term-pending-observed", "term-replay-safe"}):
            raise SystemExit("indeterminate cgroup thaw TERM receipt differs")
    elif "exited" in present:
        if ("term-intent" not in present
                or len(present & {"term-sent", "term-pending-observed"}) != 1
                or not present <= {
                    "term-intent", "term-sent", "term-pending-observed", "term-replay-safe", "exited",
                }):
            raise SystemExit("terminal cgroup thaw TERM delivery receipt differs")
    elif "reconciled-exited" in present:
        if (present & {"term-sent", "term-pending-observed", "exited"}
                or not present <= {"term-intent", "term-replay-safe", "reconciled-exited"}):
            raise SystemExit("terminal cgroup thaw reconciliation receipt differs")
    else:
        raise SystemExit("terminal cgroup thaw TERM receipt has no terminal event")
    current = signal_events[prefix]
    current_state = "terminal" if (
        current["exited"] is not None or current["reconciled-exited"] is not None
    ) else ("confirmed" if current["term-sent"] is not None else "indeterminate")
    allowed_current = {
        "confirmed": {"confirmed", "terminal"},
        "indeterminate": {"indeterminate", "terminal"},
        "terminal": {"terminal"},
    }
    if current_state not in allowed_current[row["state"]]:
        raise SystemExit("cgroup thaw TERM state regressed")

def validate_thaw_chain(require_complete):
    if thaw_intent is None:
        if (thawed is not None or writer_terminal is not None or supervisor_thaw_intent is not None
                or any((root / f"51-{role}-cgroup-thaw-complete.json").exists() for role in roles)):
            raise SystemExit("cgroup thaw progress exists without a durable intent")
        if require_complete:
            raise SystemExit("same-boot stop has no durable thaw intent")
        return
    detached = "writer" in role_set
    writer_entry = next((entry for entry in cgroups if entry["role"] == "writer"), None)
    supervisor_entry = next(entry for entry in cgroups if entry["role"] == "supervisor")
    expected_initial_mode = "detached-writer-first" if detached else "systemd-shared"
    expected_initial_cgroups = [writer_entry] if detached else cgroups
    expected_initial_targets = {"30-writer"} if detached else set(targets)
    if (set(thaw_intent) != {
            "schema", "mode", "frozen_context_sha256", "cgroups",
            "target_term_progress", "predecessor_sha256", "recovery_sigkill_allowed",
        }
            or thaw_intent["mode"] != expected_initial_mode
            or thaw_intent["frozen_context_sha256"] != sha(frozen_raw)
            or thaw_intent["cgroups"] != expected_initial_cgroups
            or thaw_intent["predecessor_sha256"] is not None
            or thaw_intent["recovery_sigkill_allowed"] is not False
            or set(thaw_intent["target_term_progress"]) != expected_initial_targets):
        raise SystemExit("cgroup thaw intent differs")
    for prefix, row in thaw_intent["target_term_progress"].items():
        validate_recorded_progress(prefix, row)
    progress_hashes = {}
    thaw_order = ["writer", "supervisor"] if "writer" in role_set else ["supervisor"]
    progress_values = {}
    progress_raws = {}
    for role in thaw_order:
        value, raw = load(
            f"51-{role}-cgroup-thaw-complete.json",
            "arc.recovery.cgroup-thaw-complete.v1", required=False,
        )
        if value is None: continue
        expected_intent_raw = (
            supervisor_thaw_intent_raw if detached and role == "supervisor" else thaw_intent_raw
        )
        if expected_intent_raw is None:
            raise SystemExit(f"per-cgroup thaw receipt has no stage intent: {role}")
        outcome = value.get("outcome")
        if (outcome not in {
                "thawed-by-direct-inode-checked-controller",
                "already-thawed-after-durable-intent", "disappeared-empty",
                "sealed-instance-gone-path-reused",
            }
                or value != {
                    "schema": "arc.recovery.cgroup-thaw-complete.v1", "role": role,
                    "cgroup": next(entry for entry in cgroups if entry["role"] == role),
                    "frozen_context_sha256": sha(frozen_raw),
                    "thaw_intent_sha256": sha(expected_intent_raw),
                    "recovery_sigkill_sent": False, "outcome": outcome,
                }): raise SystemExit(f"per-cgroup thaw receipt differs: {role}")
        if (outcome == "disappeared-empty"
                and postboot_cgroup_receipts.get(role) != "disappeared"):
            raise SystemExit(f"disappeared thaw outcome has no cgroup receipt: {role}")
        if (outcome == "sealed-instance-gone-path-reused"
                and postboot_cgroup_receipts.get(role) != "reused"):
            raise SystemExit(f"path-reused thaw outcome has no cgroup receipt: {role}")
        progress_hashes[role] = sha(raw)
        progress_values[role] = value
        progress_raws[role] = raw
    if detached and "supervisor" in progress_values and "writer" not in progress_values:
        raise SystemExit("detached supervisor thaw receipt precedes writer thaw receipt")
    if writer_terminal is not None:
        # Canonical v2 52 evidence admits only an exact locally/effectively
        # frozen supervisor cgroup.  Pre-52 disappearance is intentionally not
        # reconstructible from later absence and therefore remains fail-closed.
        containment = writer_terminal.get("supervisor_containment")
        if (not detached or "writer" not in progress_values
                or (signal_events["30-writer"]["exited"] is None
                    and signal_events["30-writer"]["reconciled-exited"] is None)
                or set(writer_terminal) != {
                    "schema", "writer_pid", "writer_start_ticks", "writer_cgroup",
                    "writer_thaw_intent_sha256", "writer_thaw_complete_sha256",
                    "stable_absence_checks", "supervisor_containment", "recovery_sigkill_sent",
                }
                or writer_terminal["writer_pid"] != contract["writer_pid"]
                or writer_terminal["writer_start_ticks"] != contract["writer_start_ticks"]
                or writer_terminal["writer_cgroup"] != writer_entry
                or writer_terminal["writer_thaw_intent_sha256"] != sha(thaw_intent_raw)
                or writer_terminal["writer_thaw_complete_sha256"] != sha(progress_raws["writer"])
                or writer_terminal["stable_absence_checks"] != 2
                or writer_terminal["recovery_sigkill_sent"] is not False
                or not isinstance(containment, dict)
                or set(containment) != {
                    "supervisor_pid", "supervisor_start_ticks", "supervisor_cgroup",
                    "outcome", "cgroup_disposition_sha256", "term_progress_before_writer_terminal",
                }
                or containment["supervisor_pid"] != contract["supervisor_main_pid"]
                or containment["supervisor_start_ticks"] != contract["supervisor_start_ticks"]
                or containment["supervisor_cgroup"] != supervisor_entry
                or containment["outcome"] not in {
                    "sealed-supervisor-live-and-frozen",
                    "sealed-supervisor-terminal-cgroup-frozen",
                }
                or containment["cgroup_disposition_sha256"] is not None
                or containment["term_progress_before_writer_terminal"] != "missing"):
            raise SystemExit("detached writer terminal-before-supervisor-thaw receipt differs")
    if supervisor_thaw_intent is not None:
        if (not detached or writer_terminal_raw is None
                or set(supervisor_thaw_intent) != {
                    "schema", "mode", "frozen_context_sha256", "cgroups",
                    "target_term_progress", "predecessor_sha256", "recovery_sigkill_allowed",
                }
                or supervisor_thaw_intent["mode"] != "detached-supervisor-after-writer-terminal"
                or supervisor_thaw_intent["frozen_context_sha256"] != sha(frozen_raw)
                or supervisor_thaw_intent["cgroups"] != [supervisor_entry]
                or set(supervisor_thaw_intent["target_term_progress"]) != {"20-supervisor"}
                or supervisor_thaw_intent["predecessor_sha256"] != sha(writer_terminal_raw)
                or supervisor_thaw_intent["recovery_sigkill_allowed"] is not False):
            raise SystemExit("detached supervisor-stage thaw intent differs")
        for prefix, row in supervisor_thaw_intent["target_term_progress"].items():
            validate_recorded_progress(prefix, row)
    elif detached and ("supervisor" in progress_values or thawed is not None):
        raise SystemExit("detached supervisor thaw progress has no stage intent")
    if thawed is not None:
        if len(progress_hashes) != len(roles) or thawed != {
            "schema": "arc.recovery.cgroups-thawed.v1", "frozen_context_sha256": sha(frozen_raw),
            "cgroups": cgroups, "per_cgroup_progress_sha256": progress_hashes,
            "all_cgroups_thawed": True, "no_signal_replayed_after_own_stage_thaw_intent": True,
        }: raise SystemExit("final cgroup thaw receipt differs")
    if require_complete and (len(progress_hashes) != len(roles) or thawed is None):
        raise SystemExit("same-boot stop has an incomplete durable thaw chain")
    if require_complete and "writer" in role_set and writer_terminal is None:
        raise SystemExit("same-boot detached stop lacks writer-terminal-before-supervisor-thaw proof")
    if "writer" in role_set and ("supervisor" in progress_values or thawed is not None) and writer_terminal is None:
        raise SystemExit("supervisor thaw progress lacks prior detached-writer terminal proof")

if reboot is None:
    validate_thaw_chain(require_complete=True)
else:
    validate_thaw_chain(require_complete=False)

stable, _ = load("40-stable-inactive.json", "arc.recovery.pidfd-event.v1")
if stable != {
    "schema": "arc.recovery.pidfd-event.v1", "target": "legacy-writer",
    "outcome": "two-stable-inactive-checks",
}: raise SystemExit("stable-inactive receipt differs")

context_path = root / "stop.context"; regular(context_path)
context = {}
for line in context_path.read_text(encoding="utf-8").splitlines():
    key, separator, value = line.partition("=")
    if not separator or key in context: raise SystemExit("stop context is malformed")
    context[key] = value
expected_context_keys = {
    "schema", "capture_id", "node", "stopped_at", "persistent_restart_fence",
    "freeze_plan_sha256", "stop_reconciliation", "quiescence",
    "supervisor_pidfd_sigterm_state", "writer_pidfd_sigterm_state",
    "recovery_sigkill_sent", "exit_cause", "validator_address", "stake", "data_dir",
}
expected_supervisor_state = "shared-with-writer" if contract["supervisor_main_pid"] == contract["writer_pid"] else signal_states["20-supervisor"]
if (set(context) != expected_context_keys or context["schema"] != "arc.recovery.offline-stop.v4"
        or context["capture_id"] != capture_id or context["node"] != node
        or context["freeze_plan_sha256"] != freeze_sha
        or context["persistent_restart_fence"] != "true"
        or context["stop_reconciliation"] != ("reboot-fenced" if reboot is not None else "same-boot-frozen-cgroup-controller")
        or context["quiescence"] != "cgroup-v2-freeze"
        or context["supervisor_pidfd_sigterm_state"] != expected_supervisor_state
        or context["writer_pidfd_sigterm_state"] != signal_states["30-writer"]
        or context["recovery_sigkill_sent"] != "false" or context["exit_cause"] != "unknown"
        or context["validator_address"] != contract["validator_address"]
        or context["stake"] != str(contract["stake"]) or context["data_dir"] != contract["data_dir"]
        or not re.fullmatch(r"[0-9]{4}-[0-9]{2}-[0-9]{2}T[0-9]{2}:[0-9]{2}:[0-9]{2}Z", context["stopped_at"])):
    raise SystemExit("offline-stop context differs from durable cgroup journal")

allowed = re.compile(
    r"(?:01-prefreeze-runtime-safety-intent|02-fast-cgroup-freeze-intent|"
    r"02-(?:supervisor|writer|writer-parent)-cgroup-frozen|"
    r"02-writer-leaf-move-intent|02-writer-parent-released|03-fast-cgroups-frozen|"
    r"04-pre-fence-quiesce-intent|05-cgroups-frozen|"
    r"06-pre-mask-activation-gate|06-restart-barrier-armed|"
    r"06-(?:supervisor|writer)-cgroup-(?:disappeared|path-reused)|"
    r"07-restart-barrier-committed|08-network-quarantine|08-network-quarantine-monitor|"
    r"09-quarantine-restart-arm|09-quarantine-restart-supervisor-frozen|"
    r"09-quarantine-restart-committed|10-fence-verified|"
    r"15-reboot-reconciled|16-post-stop-reboot-revalidated|"
    r"(?:20-supervisor|30-writer)-(?:term-intent|term-sent|term-pending-observed|term-replay-safe|exited|reconciled-exited)|"
    r"40-stable-inactive|50-cgroups-thaw-intent|50-cgroups-thawed|"
    r"51-(?:supervisor|writer)-cgroup-thaw-complete|"
    r"52-writer-terminal-before-supervisor-thaw|53-supervisor-cgroup-thaw-intent)\.json"
)
for path in root.glob("[0-9][0-9]-*.json"):
    if not allowed.fullmatch(path.name): raise SystemExit(f"unknown durable stop event: {path.name}")
for path in root.rglob(".*.partial"):
    raise SystemExit(f"unreconciled stop-publication partial: {path.name}")
PY
}

verify_stop_identity() {
    local root="$1" capture_id="$2" node="$3"
    grep -Fxq "schema=arc.recovery.offline-stop.v4" "$root/stop.complete" || \
        die "stop evidence schema is not arc.recovery.offline-stop.v4"
    grep -Fxq "capture_id=$capture_id" "$root/stop.complete" || \
        die "stop evidence id differs from its immutable path"
    grep -Fxq "node=$node" "$root/stop.complete" || \
        die "stop evidence node differs from its immutable path"
    grep -Fxq "stopped=true" "$root/stop.complete" || \
        die "stop evidence does not attest a completed stop"
}

verify_sealed_stop_contract() {
    local root="$1" freeze_sha="$2" validator="$3" stake="$4" writer_pid="$5"
    local start_ticks="$6" boot_id="$7" writer_cgroup_sha="$8" writer_supervision_mode="$9"
    local unit="${10}" unit_main_pid="${11}" supervisor_start_ticks="${12}"
    local supervisor_executable_path="${13}" supervisor_executable_sha="${14}"
    local supervisor_argv_sha="${15}" supervisor_context_sha="${16}"
    local executable_path="${17}" executable_sha="${18}" argv_sha="${19}" data_dir="${20}"
    python3 - "$root/evidence/writer-contract.json" "$root/stop.context" \
        "$freeze_sha" "$validator" "$stake" "$writer_pid" "$start_ticks" \
        "$boot_id" "$writer_cgroup_sha" "$writer_supervision_mode" \
        "$unit" "$unit_main_pid" "$supervisor_start_ticks" \
        "$supervisor_executable_path" "$supervisor_executable_sha" \
        "$supervisor_argv_sha" "$supervisor_context_sha" "$executable_path" \
        "$executable_sha" "$argv_sha" "$data_dir" <<'PY'
import hashlib
import json
import pathlib
import sys

(contract_raw, context_raw, freeze_sha, validator, stake_raw, pid_raw,
 start_raw, boot_id, writer_cgroup_sha, writer_supervision_mode, unit, main_raw,
 supervisor_start_raw,
 supervisor_executable_path, supervisor_executable_sha, supervisor_argv_sha, supervisor_context_sha,
 executable_path, executable_sha, argv_sha, data_dir) = sys.argv[1:]
contract_path = pathlib.Path(contract_raw)
context_path = pathlib.Path(context_raw)
contract = json.loads(contract_path.read_text(encoding="utf-8"))
expected = {
    "schema": "arc.recovery.exact-writer.v3",
    "freeze_plan_sha256": freeze_sha,
    "validator_address": validator,
    "stake": int(stake_raw),
    "writer_pid": int(pid_raw),
    "writer_start_ticks": int(start_raw),
    "writer_cgroup_sha256": writer_cgroup_sha,
    "writer_supervision_mode": writer_supervision_mode,
    "boot_id": boot_id,
    "supervisor_unit": unit,
    "supervisor_main_pid": int(main_raw),
    "supervisor_start_ticks": int(supervisor_start_raw),
    "supervisor_executable_path": supervisor_executable_path,
    "supervisor_executable_sha256": supervisor_executable_sha,
    "supervisor_argv_sha256": supervisor_argv_sha,
    "supervisor_context_sha256": supervisor_context_sha,
    "executable_path": executable_path,
    "executable_sha256": executable_sha,
    "argv_sha256": argv_sha,
    "data_dir": data_dir,
}
supervisor_context = contract.get("supervisor_context")
if not isinstance(supervisor_context, dict) or supervisor_context.get("schema") != "arc.recovery.supervisor-context.v1":
    raise SystemExit("persisted supervisor context is malformed")
supervisor_context_payload = (json.dumps(supervisor_context, sort_keys=True, separators=(",", ":")) + "\n").encode()
if hashlib.sha256(supervisor_context_payload).hexdigest() != supervisor_context_sha:
    raise SystemExit("persisted supervisor context hash differs")
expected["supervisor_context"] = supervisor_context
if contract != expected:
    raise SystemExit("persisted stopped-writer contract differs from the sealed freeze plan")
if contract_path.read_text(encoding="utf-8") != json.dumps(
    expected, sort_keys=True, separators=(",", ":")
) + "\n":
    raise SystemExit("persisted stopped-writer contract is not canonical JSON")
context = {}
for line in context_path.read_text(encoding="utf-8").splitlines():
    key, separator, value = line.partition("=")
    if not separator or key in context:
        raise SystemExit("stop context is malformed")
    context[key] = value
if set(context) != {
    "schema", "capture_id", "node", "stopped_at",
    "persistent_restart_fence", "freeze_plan_sha256",
    "stop_reconciliation", "quiescence", "supervisor_pidfd_sigterm_state", "writer_pidfd_sigterm_state", "recovery_sigkill_sent",
    "exit_cause", "validator_address", "stake", "data_dir",
}:
    raise SystemExit("stop context fields are not exact")
if (
    context["schema"] != "arc.recovery.offline-stop.v4"
    or context["persistent_restart_fence"] != "true"
    or context["stop_reconciliation"] not in {"reboot-fenced", "same-boot-frozen-cgroup-controller"}
    or context["quiescence"] != "cgroup-v2-freeze"
    or context["supervisor_pidfd_sigterm_state"] not in {"none", "indeterminate", "confirmed", "shared-with-writer"}
    or context["writer_pidfd_sigterm_state"] not in {"none", "indeterminate", "confirmed"}
    or context["recovery_sigkill_sent"] != "false"
    or context["exit_cause"] != "unknown"
    or context["freeze_plan_sha256"] != freeze_sha
    or context["validator_address"] != validator
    or context["stake"] != stake_raw
    or context["data_dir"] != data_dir
):
    raise SystemExit("stop context differs from the sealed freeze plan")
PY
}

fence_stop() {
    local capture_id="$1" node="$2" freeze_sha="$3" validator="$4" stake="$5"
    local writer_pid="$6" start_ticks="$7" boot_id="$8" writer_cgroup_sha="$9"
    local writer_supervision_mode="${10}" unit="${11}" unit_main_pid="${12}"
    local supervisor_start_ticks="${13}" supervisor_executable_path="${14}"
    local supervisor_executable_sha="${15}" supervisor_argv_sha="${16}"
    local supervisor_context_sha="${17}" executable_path="${18}"
    local executable_sha="${19}" argv_sha="${20}" data_dir="${21}"
    require_hash "$capture_id" "capture id"
    require_node "$node"
    require_hash "$freeze_sha" "freeze plan hash"
    require_hash "$validator" "validator address"
    require_uint "$stake" "writer stake"
    require_uint "$writer_pid" "writer pid"
    require_uint "$start_ticks" "writer start ticks"
    require_hash "$writer_cgroup_sha" "writer cgroup hash"
    case "$writer_supervision_mode" in
        systemd-unit|detached-root-session) ;;
        *) die "writer supervision mode is not reviewed" ;;
    esac
    printf '%s\n' "$boot_id" | grep -Eq '^[0-9a-f-]{36}$' || die "boot id is malformed"
    case "$unit" in arc-node.service|arc-self-heal.service) ;; *) die "supervisor unit is not reviewed" ;; esac
    require_uint "$unit_main_pid" "supervisor MainPID"
    require_uint "$supervisor_start_ticks" "supervisor start ticks"
    require_safe_absolute_path "$supervisor_executable_path" "supervisor executable path"
    require_hash "$supervisor_executable_sha" "supervisor executable hash"
    require_hash "$supervisor_argv_sha" "supervisor argv hash"
    require_hash "$supervisor_context_sha" "supervisor context hash"
    require_safe_absolute_path "$executable_path" "writer executable path"
    require_hash "$executable_sha" "writer executable hash"
    require_hash "$argv_sha" "writer argv hash"
    require_safe_absolute_path "$data_dir" "writer data directory"
    require_commands curl python3 mktemp mv chmod date find pgrep flock sync
    mkdir -p -- "$STOP_BASE"
    [ -d "$STOP_BASE" ] && [ ! -L "$STOP_BASE" ] || die "global stop-journal root is unsafe"
    local global_lock="$STOP_BASE/.host-writer-recovery.lock"
    [ ! -L "$global_lock" ] || die "global writer-recovery lock is a symlink"
    : >> "$global_lock"
    chmod 600 -- "$global_lock"
    exec 7>> "$global_lock"
    flock -x 7
    local parent="$STOP_BASE/$capture_id" stop_root="$STOP_BASE/$capture_id/$node"
    mkdir -p -- "$parent"
    [ -d "$parent" ] && [ ! -L "$parent" ] || die "stop journal parent is unsafe"
    local lock_path="$parent/.${node}.stop.lock"
    [ ! -L "$lock_path" ] || die "stop journal lock is a symlink"
    : >> "$lock_path"
    chmod 600 -- "$lock_path"
    exec 8>> "$lock_path"
    flock -x 8
    python3 - "$freeze_sha" "$node" "$validator" "$stake" "$writer_pid" \
        "$start_ticks" "$boot_id" "$writer_cgroup_sha" "$writer_supervision_mode" \
        "$unit" "$unit_main_pid" "$supervisor_start_ticks" \
        "$supervisor_executable_path" "$supervisor_executable_sha" \
        "$supervisor_argv_sha" "$supervisor_context_sha" "$executable_path" \
        "$executable_sha" "$argv_sha" "$data_dir" "$parent/.${node}.stop.partial" \
        "$stop_root" <<'PY'
import hashlib
import json
import os
import pathlib
import re
import stat
import sys

(freeze_sha, node, validator, stake_raw, writer_pid_raw, writer_start_raw, boot_id,
 writer_cgroup_sha, writer_mode, unit, supervisor_pid_raw, supervisor_start_raw,
 supervisor_executable, supervisor_executable_sha, supervisor_argv_sha,
 supervisor_context_sha, writer_executable, writer_executable_sha, writer_argv_sha,
 data_dir, partial_raw, final_raw) = sys.argv[1:]
plan = pathlib.Path(f"/root/.arc-recovery-plans/{freeze_sha}/freeze.lock.json")
sidecar = pathlib.Path(f"{plan}.sha256")
details = plan.lstat()
if (plan.is_symlink() or not stat.S_ISREG(details.st_mode) or details.st_uid != 0
        or details.st_gid != 0 or details.st_mode & 0o222):
    raise SystemExit("pinned remote freeze plan is unsafe")
raw = plan.read_bytes()
if hashlib.sha256(raw).hexdigest() != freeze_sha:
    raise SystemExit("pinned remote freeze plan hash differs")
value = json.loads(raw)
canonical = (json.dumps(value, sort_keys=True, separators=(",", ":")) + "\n").encode()
if raw != canonical or value.get("schema") != "arc.recovery.freeze-plan.v5":
    raise SystemExit("pinned remote freeze plan schema/canonical bytes differ")
if (sidecar.is_symlink() or not sidecar.is_file()
        or sidecar.read_text(encoding="ascii") != f"{freeze_sha}  freeze.lock.json\n"):
    raise SystemExit("pinned remote freeze plan checksum sidecar differs")
rows = [row for row in value.get("nodes", []) if row.get("name") == node]
if len(rows) != 1: raise SystemExit("pinned remote freeze plan node is not unique")
row = rows[0]
expected = {
    "validator_address": validator, "stake": int(stake_raw),
    "writer_pid": int(writer_pid_raw), "writer_start_ticks": int(writer_start_raw),
    "boot_id": boot_id, "writer_cgroup_sha256": writer_cgroup_sha,
    "writer_supervision_mode": writer_mode, "supervisor_unit": unit,
    "supervisor_main_pid": int(supervisor_pid_raw),
    "supervisor_start_ticks": int(supervisor_start_raw),
    "supervisor_executable_path": supervisor_executable,
    "supervisor_executable_sha256": supervisor_executable_sha,
    "supervisor_argv_sha256": supervisor_argv_sha,
    "supervisor_context_sha256": supervisor_context_sha,
    "executable_path": writer_executable, "executable_sha256": writer_executable_sha,
    "argv_sha256": writer_argv_sha, "data_dir": data_dir,
}
if any(row.get(key) != expected_value for key, expected_value in expected.items()):
    raise SystemExit("remote helper arguments differ from the pinned node contract")
path = row.get("writer_cgroup_path"); device = row.get("writer_cgroup_device"); inode = row.get("writer_cgroup_inode")
if (not isinstance(path, str) or not re.fullmatch(r"/[A-Za-z0-9._@/-]+", path)
        or isinstance(device, bool) or not isinstance(device, int)
        or isinstance(inode, bool) or not isinstance(inode, int)):
    raise SystemExit("pinned writer cgroup identity is malformed")
proc = pathlib.Path(f"/proc/{writer_pid_raw}")
current_boot = pathlib.Path("/proc/sys/kernel/random/boot_id").read_text().strip()
roots = [pathlib.Path(partial_raw), pathlib.Path(final_raw)]
live_exact = False
if current_boot == boot_id and proc.is_dir():
    try:
        stat_fields = proc.joinpath("stat").read_text(encoding="ascii").split()
        if len(stat_fields) < 22: raise SystemExit("live writer stat is truncated")
        if int(stat_fields[21]) == int(writer_start_raw):
            if (os.readlink(proc / "exe") != writer_executable
                    or hashlib.sha256(proc.joinpath("exe").read_bytes()).hexdigest() != writer_executable_sha
                    or hashlib.sha256(proc.joinpath("cmdline").read_bytes()).hexdigest() != writer_argv_sha):
                raise SystemExit("live sealed writer executable/argv differs from pinned plan")
            cgroup_raw = proc.joinpath("cgroup").read_bytes()
            unified = [line.split(":", 2)[2] for line in cgroup_raw.decode("utf-8").splitlines()
                       if line.startswith("0::")]
            if len(unified) != 1: raise SystemExit("live writer unified cgroup is ambiguous")
            writer_path = unified[0]
            entry_path = pathlib.Path("/sys/fs/cgroup") / path.lstrip("/")
            current = entry_path.stat()
            if entry_path.is_symlink() or current.st_dev != device or current.st_ino != inode:
                raise SystemExit("live sealed writer cgroup path/inode differs from pinned plan")
            if writer_path == path:
                if hashlib.sha256(cgroup_raw).hexdigest() != writer_cgroup_sha:
                    raise SystemExit("live sealed writer parent cgroup bytes differ from pinned plan")
                live_exact = True
            elif writer_mode == "detached-root-session" and writer_path == path.rstrip("/") + "/arc-recovery-writer":
                move_paths = [root / "02-writer-leaf-move-intent.json" for root in roots
                              if (root / "02-writer-leaf-move-intent.json").exists()
                              or (root / "02-writer-leaf-move-intent.json").is_symlink()]
                if len(move_paths) != 1 or move_paths[0].is_symlink() or not move_paths[0].is_file():
                    raise SystemExit("live detached writer moved without one durable leaf intent")
                move_raw = move_paths[0].read_bytes(); move = json.loads(move_raw)
                parent_identity = {"role": "writer-parent", "path": path, "device": device, "inode": inode}
                if (move_raw != (json.dumps(move, sort_keys=True, separators=(",", ":")) + "\n").encode()
                        or move.get("schema") != "arc.recovery.detached-writer-leaf-move-intent.v1"
                        or move.get("freeze_plan_sha256") != freeze_sha or move.get("boot_id") != boot_id
                        or move.get("writer_pid") != int(writer_pid_raw)
                        or move.get("writer_start_ticks") != int(writer_start_raw)
                        or move.get("parent_scope_cgroup") != parent_identity
                        or move.get("recovery_leaf_path") != writer_path):
                    raise SystemExit("live detached writer leaf intent differs")
                leaf = pathlib.Path("/sys/fs/cgroup") / writer_path.lstrip("/")
                leaf_details = leaf.lstat()
                if (leaf.is_symlink() or not leaf.is_dir() or leaf_details.st_dev != device
                        or leaf.joinpath("cgroup.freeze").read_text(encoding="ascii").strip() != "1"):
                    raise SystemExit("live detached writer leaf is not independently frozen")
                members = set()
                for current_root, directories, _files in os.walk(leaf, followlinks=False):
                    if pathlib.Path(current_root).is_symlink(): raise SystemExit("writer leaf subtree is unsafe")
                    directories.sort()
                    members.update(int(value) for value in pathlib.Path(current_root).joinpath(
                        "cgroup.procs"
                    ).read_text(encoding="ascii").splitlines())
                if members != {int(writer_pid_raw)}:
                    raise SystemExit("live detached writer leaf membership differs")
                receipt_path = move_paths[0].parent / "02-writer-cgroup-frozen.json"
                if receipt_path.exists() or receipt_path.is_symlink():
                    if receipt_path.is_symlink() or not receipt_path.is_file():
                        raise SystemExit("live detached writer leaf receipt is unsafe")
                    receipt_raw = receipt_path.read_bytes(); receipt = json.loads(receipt_raw)
                    expected_leaf = {"role": "writer", "path": writer_path,
                                     "device": leaf_details.st_dev, "inode": leaf_details.st_ino}
                    if (receipt_raw != (json.dumps(receipt, sort_keys=True, separators=(",", ":")) + "\n").encode()
                            or receipt.get("schema") != "arc.recovery.fast-cgroup-progress.v1"
                            or receipt.get("cgroup") != expected_leaf
                            or receipt.get("parent_scope_cgroup") != parent_identity
                            or receipt.get("observed_local_freeze") != 1):
                        raise SystemExit("live detached writer leaf receipt differs")
                live_exact = True
            else:
                raise SystemExit("live sealed writer cgroup differs from pinned plan/recovery leaf")
    except (FileNotFoundError, ProcessLookupError):
        live_exact = False
if live_exact:
    marker = pathlib.Path("/etc/arc-recovery/legacy-start-allowed")
    if not marker.exists() and not marker.is_symlink():
        evidence_roots = [value for value in roots if value.exists() and not value.is_symlink()]
        live_pairs = [
            value for value in evidence_roots
            if (value / "09-quarantine-restart-arm.json").is_file()
            and not (value / "09-quarantine-restart-arm.json").is_symlink()
            and (value / "09-quarantine-restart-committed.json").is_file()
            and not (value / "09-quarantine-restart-committed.json").is_symlink()
        ]
        if len(live_pairs) != 1:
            raise SystemExit("live writer has marker absence without one quarantine restart arm")
        evidence_root = live_pairs[0]
        arm_raw = (evidence_root / "09-quarantine-restart-arm.json").read_bytes()
        commit_raw = (evidence_root / "09-quarantine-restart-committed.json").read_bytes()
        arm_value = json.loads(arm_raw); commit_value = json.loads(commit_raw)
        canonical = lambda value: (json.dumps(value, sort_keys=True, separators=(",", ":")) + "\n").encode()
        if (arm_raw != canonical(arm_value) or commit_raw != canonical(commit_value)
                or arm_value.get("schema") != "arc.recovery.quarantine-live-restart-arm.v1"
                or commit_value.get("schema") != "arc.recovery.quarantine-live-restart-committed.v1"
                or commit_value.get("quarantine_restart_arm_sha256") != hashlib.sha256(arm_raw).hexdigest()
                or arm_value.get("freeze_plan_sha256") != freeze_sha
                or arm_value.get("selected_unit") != unit
                or arm_value.get("selected_main_pid") != int(supervisor_pid_raw)
                or arm_value.get("writer", {}).get("pid") != int(writer_pid_raw)
                or commit_value.get("allow_marker_absent") is not True
                or commit_value.get("restart_prevented") is not True):
            raise SystemExit("live quarantine restart arm differs from the pinned writer")
if not live_exact:
    marker = pathlib.Path("/etc/arc-recovery/legacy-start-allowed")
    arms = []
    for evidence_root in roots:
        if evidence_root.exists() or evidence_root.is_symlink():
            if evidence_root.is_symlink() or not evidence_root.is_dir():
                raise SystemExit("restart-barrier evidence root is unsafe")
        arm_path = evidence_root / "06-restart-barrier-armed.json"
        if arm_path.exists() or arm_path.is_symlink():
            arms.append(arm_path)
    if marker.exists() or marker.is_symlink() or len(arms) != 1:
        raise SystemExit("sealed live writer is absent without a committed barrier arm")
    arm_path = arms[0]
    if arm_path.is_symlink() or not arm_path.is_file():
        raise SystemExit("committed barrier arm is unsafe")
    arm_raw = arm_path.read_bytes(); arm_value = json.loads(arm_raw)
    if (arm_raw != (json.dumps(arm_value, sort_keys=True, separators=(",", ":")) + "\n").encode()
            or arm_value.get("schema") != "arc.recovery.restart-barrier-arm.v1"
            or arm_value.get("freeze_plan_sha256") != freeze_sha
            or arm_value.get("sealed_boot_id") != boot_id
            or arm_value.get("selected_unit") != unit
            or arm_value.get("selected_main_pid") != int(supervisor_pid_raw)
            or arm_value.get("allow_marker_path") != "/etc/arc-recovery/legacy-start-allowed"
            or arm_value.get("all_cgroups_frozen") is not True
            or arm_value.get("control_masks") != {
                name: "/dev/null" for name in (
                    "arc-self-heal.service", "arc-node.service",
                    "arc-node-update.service", "arc-node-update.timer",
                )
            }
            or (writer_mode == "detached-root-session" and (
                arm_value.get("writer_parent_scope_cgroup") != {
                    "role": "writer-parent", "path": path, "device": device, "inode": inode,
                }
                or arm_value.get("writer_recovery_leaf", {}).get("path")
                != path.rstrip("/") + "/arc-recovery-writer"
            ))
            or (writer_mode == "systemd-unit" and (
                arm_value.get("writer_parent_scope_cgroup") is not None
                or arm_value.get("writer_recovery_leaf") is not None
                or not any(entry.get("path") == path and entry.get("device") == device
                           and entry.get("inode") == inode for entry in arm_value.get("cgroups", []))
            ))):
        raise SystemExit("committed barrier arm is not bound to the pinned writer cgroup")
PY
    if [ -e "$stop_root" ]; then
        verify_tree_index "$stop_root" stop.files.sha256 stop.complete
        verify_stop_identity "$stop_root" "$capture_id" "$node"
        stopped_status "$capture_id" "$node" "$freeze_sha" "$validator" "$stake" \
            "$writer_pid" "$start_ticks" "$boot_id" "$writer_cgroup_sha" \
            "$writer_supervision_mode" "$unit" "$unit_main_pid" \
            "$supervisor_start_ticks" "$supervisor_executable_path" \
            "$supervisor_executable_sha" "$supervisor_argv_sha" "$supervisor_context_sha" \
            "$executable_path" "$executable_sha" "$argv_sha" "$data_dir"
        return 0
    fi
    local temporary="$parent/.${node}.stop.partial"
    local partial_owner="schema=arc.recovery.partial.v1 capture=$capture_id node=$node phase=stop"
    if [ -e "$temporary" ] || [ -L "$temporary" ]; then
        [ -d "$temporary" ] && [ ! -L "$temporary" ] || die "stop journal partial path is unsafe"
        if [ -f "$temporary/stop.complete" ] && [ ! -L "$temporary/stop.complete" ]; then
            verify_tree_index "$temporary" stop.files.sha256 stop.complete
            verify_stop_identity "$temporary" "$capture_id" "$node"
            verify_stop_journal_semantics "$temporary" "$capture_id" "$node" "$freeze_sha"
            verify_sealed_stop_contract "$temporary" "$freeze_sha" "$validator" "$stake" \
                "$writer_pid" "$start_ticks" "$boot_id" "$writer_cgroup_sha" \
                "$writer_supervision_mode" "$unit" "$unit_main_pid" \
                "$supervisor_start_ticks" "$supervisor_executable_path" \
                "$supervisor_executable_sha" "$supervisor_argv_sha" "$supervisor_context_sha" \
                "$executable_path" "$executable_sha" "$argv_sha" "$data_dir"
            verify_legacy_restart_fence "$temporary"
            [ ! -e "$stop_root" ] || die "completed stop root appeared while lock was held"
            mv -T -- "$temporary" "$stop_root"
            sync -f "$parent"
            stopped_status "$capture_id" "$node" "$freeze_sha" "$validator" "$stake" \
                "$writer_pid" "$start_ticks" "$boot_id" "$writer_cgroup_sha" \
                "$writer_supervision_mode" "$unit" "$unit_main_pid" \
                "$supervisor_start_ticks" "$supervisor_executable_path" \
                "$supervisor_executable_sha" "$supervisor_argv_sha" "$supervisor_context_sha" \
                "$executable_path" "$executable_sha" "$argv_sha" "$data_dir"
            return 0
        fi
        if { [ -f "$temporary/01-prefreeze-runtime-safety-intent.json" ] \
                && [ ! -L "$temporary/01-prefreeze-runtime-safety-intent.json" ]; } \
            || { [ -f "$temporary/02-fast-cgroup-freeze-intent.json" ] \
                && [ ! -L "$temporary/02-fast-cgroup-freeze-intent.json" ]; } \
            || { [ -f "$temporary/stop.intent.json" ] && [ ! -L "$temporary/stop.intent.json" ]; } \
            || { [ -f "$temporary/04-pre-fence-quiesce-intent.json" ] \
                && [ ! -L "$temporary/04-pre-fence-quiesce-intent.json" ]; }; then
            [ -f "$temporary/.arc-recovery-partial-owner" ] && \
                [ ! -L "$temporary/.arc-recovery-partial-owner" ] || \
                die "armed stop journal has no ownership marker"
            [ "$(cat "$temporary/.arc-recovery-partial-owner")" = "$partial_owner" ] || \
                die "armed stop journal ownership differs"
            # These are derived only after every pidfd action. If completion
            # was not durable, regenerate them from the preserved source log.
            rm -f -- "$temporary/stop.files.sha256"
        else
            prepare_owned_partial_directory "$temporary" "$partial_owner"
        fi
    else
        prepare_owned_partial_directory "$temporary" "$partial_owner"
    fi
    ARCHIVE_NODE_TEMP_PATH="$temporary"
    pre_fence_quiesce "$temporary" "$capture_id" "$node" "$freeze_sha" "$boot_id" \
        "$writer_supervision_mode" "$unit" "$unit_main_pid" "$supervisor_start_ticks" \
        "$supervisor_executable_path" "$supervisor_executable_sha" "$supervisor_argv_sha" \
        "$supervisor_context_sha" "$writer_pid" "$start_ticks" "$writer_cgroup_sha" \
        "$executable_path" "$executable_sha" "$argv_sha"
    if [ "${ARC_RECOVERY_QUARANTINE_ONLY:-false}" = true ]; then
        verify_legacy_network_quarantine "$temporary" "$capture_id" "$node" "$freeze_sha"
        ARCHIVE_NODE_TEMP_PATH=""
        quarantine_status "$capture_id" "$node" "$freeze_sha"
        return 0
    fi
    if [ ! -f "$temporary/stop.intent.json" ]; then
        if [ -e "$temporary/evidence" ]; then
            [ -d "$temporary/evidence" ] && [ ! -L "$temporary/evidence" ] || \
                die "pre-stop evidence retry path is unsafe"
            find "$temporary/evidence" -depth -delete
        fi
        mkdir -- "$temporary/evidence"
        {
            printf 'schema=arc.recovery.rpc-evidence-reference.v1\n'
            printf 'freeze_plan_sha256=%s\n' "$freeze_sha"
            printf 'source=content-sealed-live-writer-audit\n'
            printf 'post_freeze_rpc_attempted=false\n'
        } > "$temporary/evidence/rpc-evidence-reference.txt"
        verify_exact_writer "$temporary/evidence" "$freeze_sha" "$validator" "$stake" \
            "$writer_pid" "$start_ticks" "$boot_id" "$writer_cgroup_sha" \
            "$writer_supervision_mode" "$unit" "$unit_main_pid" \
            "$supervisor_start_ticks" "$supervisor_executable_path" \
            "$supervisor_executable_sha" "$supervisor_argv_sha" "$supervisor_context_sha" \
            "$executable_path" "$executable_sha" "$argv_sha" "$data_dir"
    fi
    verify_or_arm_stop_journal "$temporary" "$capture_id" "$node" "$freeze_sha" \
        "$validator" "$stake" "$writer_pid" "$start_ticks" "$boot_id" \
        "$writer_cgroup_sha" "$writer_supervision_mode" "$unit" "$unit_main_pid" \
        "$supervisor_start_ticks" "$supervisor_executable_path" \
        "$supervisor_executable_sha" "$supervisor_argv_sha" "$supervisor_context_sha" \
        "$executable_path" "$executable_sha" "$argv_sha" "$data_dir"
    stop_node_cleanly "$temporary" "$writer_pid" "$start_ticks" "$boot_id" \
        "$writer_cgroup_sha" "$executable_path" "$executable_sha" "$argv_sha" \
        "$unit_main_pid" "$supervisor_start_ticks" "$unit" \
        "$supervisor_executable_path" "$supervisor_executable_sha" "$supervisor_argv_sha" \
        "$capture_id" "$node" "$freeze_sha"
    reconcile_known_stop_partials "$temporary"
    verify_legacy_restart_fence "$temporary"
    pgrep -x arc-node >/dev/null 2>&1 && die "legacy arc-node restarted after the persistent fence"
    write_or_verify_stop_context "$temporary" "$capture_id" "$node" "$freeze_sha" \
        "$validator" "$stake" "$data_dir"
    verify_stop_journal_semantics "$temporary" "$capture_id" "$node" "$freeze_sha"
    write_tree_index "$temporary" stop.files.sha256 stop.complete
    write_complete_marker "$temporary" stop.files.sha256 stop.complete arc.recovery.offline-stop.v4 \
        "capture_id=$capture_id" "node=$node" "stopped=true"
    fsync_recovery_tree "$temporary"
    chmod -R a-w,go-rwx -- "$temporary"
    fsync_recovery_tree "$temporary"
    [ ! -e "$stop_root" ] || die "stop root appeared while lock was held"
    mv -T -- "$temporary" "$stop_root"
    sync -f "$parent"
    ARCHIVE_NODE_TEMP_PATH=""
    verify_tree_index "$stop_root" stop.files.sha256 stop.complete
    verify_stop_identity "$stop_root" "$capture_id" "$node"
    stopped_status "$capture_id" "$node" "$freeze_sha" "$validator" "$stake" \
        "$writer_pid" "$start_ticks" "$boot_id" "$writer_cgroup_sha" \
        "$writer_supervision_mode" "$unit" "$unit_main_pid" \
        "$supervisor_start_ticks" "$supervisor_executable_path" \
        "$supervisor_executable_sha" "$supervisor_argv_sha" "$supervisor_context_sha" \
        "$executable_path" "$executable_sha" "$argv_sha" "$data_dir" >/dev/null
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
    if [ "$#" -eq 21 ]; then
        verify_sealed_stop_contract "$stop_root" "$3" "$4" "$5" "$6" "$7" \
            "$8" "$9" "${10}" "${11}" "${12}" "${13}" "${14}" "${15}" \
            "${16}" "${17}" "${18}" "${19}" "${20}" "${21}"
    elif [ "$#" -ne 2 ]; then
        die "stopped-status exact contract arguments are incomplete"
    fi
    local journal_freeze_sha
    journal_freeze_sha="$(python3 -c 'import json,sys; print(json.load(open(sys.argv[1], encoding="utf-8"))["freeze_plan_sha256"])' \
        "$stop_root/evidence/writer-contract.json")"
    require_hash "$journal_freeze_sha" "persisted stop freeze plan hash"
    verify_stop_journal_semantics "$stop_root" "$capture_id" "$node" "$journal_freeze_sha"
    verify_legacy_restart_fence "$stop_root"
    pgrep -x arc-node >/dev/null 2>&1 && die "legacy writer is running after freeze"
    local stop_complete_sha stop_files_sha
    stop_complete_sha="$(hash_file "$stop_root/stop.complete")"
    stop_files_sha="$(hash_file "$stop_root/stop.files.sha256")"
    python3 - "$stop_root/evidence/writer-contract.json" "$capture_id" "$node" \
        "$journal_freeze_sha" "$stop_complete_sha" "$stop_files_sha" <<'PY'
import hashlib
import json
import pathlib
import re
import sys

contract_path = pathlib.Path(sys.argv[1])
capture_id, node, freeze_sha, complete_sha, files_sha = sys.argv[2:]
contract_raw = contract_path.read_bytes()
contract = json.loads(contract_raw)
canonical_contract = (json.dumps(contract, sort_keys=True, separators=(",", ":")) + "\n").encode()
if contract_raw != canonical_contract:
    raise SystemExit("stopped writer contract is not canonical JSON")
hash_re = re.compile(r"[0-9a-f]{64}")
if any(hash_re.fullmatch(value) is None for value in (capture_id, freeze_sha, complete_sha, files_sha)):
    raise SystemExit("stopped-status hash binding is malformed")
validator = contract.get("validator_address")
stake = contract.get("stake")
if (contract.get("schema") != "arc.recovery.exact-writer.v3"
        or contract.get("freeze_plan_sha256") != freeze_sha
        or not isinstance(validator, str) or hash_re.fullmatch(validator) is None
        or isinstance(stake, bool) or not isinstance(stake, int) or stake <= 0):
    raise SystemExit("stopped writer identity is malformed")
value = {
    "capture_id": capture_id,
    "freeze_plan_sha256": freeze_sha,
    "node": node,
    "restart_fenced": True,
    "schema": "arc.recovery.offline-stop-status.v1",
    "stake": stake,
    "stop_complete_sha256": complete_sha,
    "stop_files_sha256": files_sha,
    "stop_schema": "arc.recovery.offline-stop.v4",
    "stopped": True,
    "validator_address": validator,
}

print(json.dumps(value, sort_keys=True, separators=(",", ":")))
PY
}

stopped_status_challenged() {
    [ "$#" -eq 23 ] || die "stopped-status-challenged requires the exact writer, host, and challenge contract"
    local node="$2" host="${22}" challenge="${23}" base
    require_hash "$challenge" "stopped-status challenge"
    case "$node:$host" in
        nyc:149.28.32.76|lax:140.82.16.112|ams:136.244.109.1|lhr:104.238.171.11|nrt:202.182.107.41|sgp:149.28.153.31) ;;
        *) die "stopped-status challenge host differs from the fixed production node mapping" ;;
    esac
    base="$(stopped_status "${@:1:21}")"
    python3 - "$host" "$challenge" "$base" <<'PY'
import json
import re
import sys

host, challenge, raw = sys.argv[1:]
value = json.loads(raw)
if not re.fullmatch(r"[0-9a-f]{64}", challenge):
    raise SystemExit("stopped-status challenge is malformed")
if set(value) != {
    "schema", "capture_id", "node", "freeze_plan_sha256", "validator_address",
    "stake", "stopped", "restart_fenced", "stop_schema",
    "stop_complete_sha256", "stop_files_sha256",
} or value.get("schema") != "arc.recovery.offline-stop-status.v1":
    raise SystemExit("base stopped-status output is malformed")
value["schema"] = "arc.recovery.offline-stop-challenged-status.v1"
value["host"] = host
value["challenge"] = challenge
print(json.dumps(value, sort_keys=True, separators=(",", ":")))
PY
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
root_device = root.stat().st_dev
rows = []
for base, dirs, files in os.walk(root, followlinks=False):
    dirs.sort()
    files.sort()
    for name in dirs:
        path = pathlib.Path(base) / name
        if path.is_symlink():
            raise SystemExit(f"symlink directory is forbidden in offline data: {path}")
        if path.stat().st_dev != root_device:
            raise SystemExit(f"cross-device directory is forbidden in offline data: {path}")
    for name in files:
        path = pathlib.Path(base) / name
        mode = path.lstat().st_mode
        if not stat.S_ISREG(mode):
            raise SystemExit(f"non-regular member is forbidden in offline data: {path}")
        if path.stat().st_dev != root_device:
            raise SystemExit(f"cross-device file is forbidden in offline data: {path}")
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

capture_source_data_dir() {
    python3 - "$1/capture-source.json" <<'PY'
import json
import sys
value = json.load(open(sys.argv[1], encoding="utf-8"))
path = value.get("data_dir")
if not isinstance(path, str) or not path.startswith("/"):
    raise SystemExit("capture source has no safe absolute data directory")
print(path)
PY
}

verify_capture_source() {
    local capture_root="$1" capture_id="$2" node="$3"
    verify_tree_index "$capture_root" capture.files.sha256 capture.complete
    verify_capture_identity "$capture_root" "$capture_id" "$node"
    local freeze_sha live_root live_root_sha live_receipt_sha data_dir temporary inventory
    freeze_sha="$(sed -n 's/^freeze_plan_sha256=//p' "$capture_root/capture.inventory")"
    require_hash "$freeze_sha" "capture live-observation freeze-plan hash"
    live_root="$LIVE_OBSERVATION_BASE/$capture_id/$node"
    verify_live_observation_receipt "$live_root" "$capture_id" "$node" "$freeze_sha"
    live_root_sha="$(hash_file "$live_root/live-observations.files.sha256")"
    live_receipt_sha="$(hash_file "$live_root/receipt.json")"
    grep -Fxq "legacy_live_observations_schema=arc.recovery.legacy-live-observations.v1" \
        "$capture_root/capture.inventory" || die "capture inventory omits live-observation schema"
    grep -Fxq "legacy_live_observations_root_sha256=$live_root_sha" \
        "$capture_root/capture.inventory" || die "capture inventory live-observation root differs"
    grep -Fxq "legacy_live_observations_receipt_sha256=$live_receipt_sha" \
        "$capture_root/capture.inventory" || die "capture inventory live-observation receipt differs"
    grep -Fxq "legacy_live_observations_labels=diagnostic,noncanonical,nonreward" \
        "$capture_root/capture.inventory" || die "capture inventory live-observation labels differ"
    data_dir="$(capture_source_data_dir "$capture_root")"
    require_safe_absolute_path "$data_dir" "sealed capture source data directory"
    [ -d "$data_dir" ] && [ ! -L "$data_dir" ] || \
        die "sealed capture source data directory is missing or a symlink"
    temporary="$(mktemp -d)"
    inventory="$temporary/source-data.files.sha256"
    ARCHIVE_NODE_TEMP_PATH="$temporary"
    write_regular_tree_inventory "$data_dir" "$inventory"
    cmp --silent "$capture_root/source-data.files.sha256" "$inventory" || \
        die "fenced legacy source tree changed after capture"
    find "$temporary" -depth -delete
    ARCHIVE_NODE_TEMP_PATH=""
    python3 - "$capture_root/capture-source.json" "$data_dir" <<'PY'
import hashlib
import json
import os
import pathlib
import stat
import sys

source_path = pathlib.Path(sys.argv[1])
data_dir = pathlib.Path(sys.argv[2])
value = json.loads(source_path.read_text(encoding="utf-8"))
expected = {
    "schema", "data_dir", "data_device", "data_inode", "data_bytes",
    "data_files", "state_wal_bytes", "state_wal_sha256", "external_snapshots",
}
if set(value) != expected or value["schema"] != "arc.recovery.capture-source.v1":
    raise SystemExit("capture source identity has missing, unknown, or unsupported fields")
details = data_dir.stat()
root_device = details.st_dev
if value["data_dir"] != str(data_dir) or value["data_device"] != details.st_dev or value["data_inode"] != details.st_ino:
    raise SystemExit("capture source path/device/inode identity changed")

def digest(path):
    result = hashlib.sha256()
    with path.open("rb") as handle:
        for chunk in iter(lambda: handle.read(1024 * 1024), b""):
            result.update(chunk)
    return result.hexdigest()

wal = data_dir / "state.wal"
if wal.is_symlink() or not stat.S_ISREG(wal.lstat().st_mode):
    raise SystemExit("capture source WAL is unsafe")
if wal.stat().st_size != value["state_wal_bytes"] or digest(wal) != value["state_wal_sha256"]:
    raise SystemExit("capture source WAL changed")
snapshots = value["external_snapshots"]
if not isinstance(snapshots, list):
    raise SystemExit("capture external snapshot inventory is malformed")
for row in snapshots:
    if not isinstance(row, dict) or set(row) != {"path", "size", "sha256"}:
        raise SystemExit("capture external snapshot fields are not exact")
    path = pathlib.Path(row["path"])
    if not path.is_absolute() or path.is_symlink() or not stat.S_ISREG(path.lstat().st_mode):
        raise SystemExit("capture external snapshot is missing or unsafe")
    if path.stat().st_size != row["size"] or digest(path) != row["sha256"]:
        raise SystemExit("capture external snapshot changed")
PY
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
        verify_capture_source "$capture_root" "$capture_id" "$node"
        printf 'archive node: existing offline capture verified capture=%s node=%s\n' "$capture_id" "$node"
        return 0
    fi
    local data_dir executable_path freeze_sha live_root live_root_sha live_receipt_sha
    data_dir="$(python3 -c 'import json,sys; print(json.load(open(sys.argv[1], encoding="utf-8"))["data_dir"])' \
        "$stop_root/evidence/writer-contract.json")"
    executable_path="$(python3 -c 'import json,sys; print(json.load(open(sys.argv[1], encoding="utf-8"))["executable_path"])' \
        "$stop_root/evidence/writer-contract.json")"
    freeze_sha="$(python3 -c 'import json,sys; print(json.load(open(sys.argv[1], encoding="utf-8"))["freeze_plan_sha256"])' \
        "$stop_root/evidence/writer-contract.json")"
    require_hash "$freeze_sha" "stopped writer freeze-plan hash"
    live_root="$LIVE_OBSERVATION_BASE/$capture_id/$node"
    verify_live_observation_receipt "$live_root" "$capture_id" "$node" "$freeze_sha"
    live_root_sha="$(hash_file "$live_root/live-observations.files.sha256")"
    live_receipt_sha="$(hash_file "$live_root/receipt.json")"
    require_safe_absolute_path "$data_dir" "stopped writer data directory"
    require_safe_absolute_path "$executable_path" "stopped writer executable"
    [ -d "$data_dir" ] && [ ! -L "$data_dir" ] || \
        die "exact stopped legacy data directory is missing or a symlink"
    pgrep -x arc-node >/dev/null 2>&1 && die "refusing content capture while arc-node is running"

    local temporary="$parent/.${node}.capture.partial"
    prepare_owned_partial_directory "$temporary" \
        "schema=arc.recovery.partial.v1 capture=$capture_id node=$node phase=capture"
    ARCHIVE_NODE_TEMP_PATH="$temporary"
    mkdir -- "$temporary/evidence" "$temporary/evidence/freeze" "$temporary/legacy-public"
    cp --archive -- "$stop_root/." "$temporary/evidence/freeze/"
    verify_tree_index "$temporary/evidence/freeze" stop.files.sha256 stop.complete
    write_regular_tree_inventory "$data_dir" "$temporary/source-data.files.sha256"

    python3 - "$data_dir" "$temporary/capture-source.json" <<'PY'
import hashlib
import json
import os
import pathlib
import stat
import sys

data_dir = pathlib.Path(sys.argv[1])
output = pathlib.Path(sys.argv[2])

def digest(path):
    value = hashlib.sha256()
    with path.open("rb") as handle:
        for chunk in iter(lambda: handle.read(1024 * 1024), b""):
            value.update(chunk)
    return value.hexdigest()

wal = data_dir / "state.wal"
if wal.is_symlink() or not stat.S_ISREG(wal.lstat().st_mode):
    raise SystemExit("fenced source has no regular non-symlink state.wal")
external = []
for candidate in (pathlib.Path(str(data_dir) + ".snapshot.lz4"),):
    if not candidate.exists() and not candidate.is_symlink():
        continue
    if candidate.is_symlink() or not stat.S_ISREG(candidate.lstat().st_mode):
        raise SystemExit(f"external snapshot is unsafe: {candidate}")
    external.append({"path": str(candidate), "size": candidate.stat().st_size, "sha256": digest(candidate)})
details = data_dir.stat()
root_device = details.st_dev
data_files = 0
data_bytes = 0
for base, dirs, files in os.walk(data_dir, followlinks=False):
    for name in dirs:
        directory = pathlib.Path(base) / name
        if directory.is_symlink():
            raise SystemExit("fenced source contains a symlink directory")
        if directory.stat().st_dev != root_device:
            raise SystemExit("fenced source contains a cross-device directory")
    for name in files:
        path = pathlib.Path(base) / name
        if path.is_symlink() or not stat.S_ISREG(path.lstat().st_mode):
            raise SystemExit("fenced source contains a non-regular file")
        if path.stat().st_dev != root_device:
            raise SystemExit("fenced source contains a cross-device file")
        data_files += 1
        data_bytes += path.stat().st_size
value = {
    "schema": "arc.recovery.capture-source.v1",
    "data_dir": str(data_dir),
    "data_device": root_device,
    "data_inode": details.st_ino,
    "data_bytes": data_bytes,
    "data_files": data_files,
    "state_wal_bytes": wal.stat().st_size,
    "state_wal_sha256": digest(wal),
    "external_snapshots": external,
}
output.write_text(json.dumps(value, sort_keys=True, separators=(",", ":")) + "\n", encoding="utf-8")
PY

    local source destination
    for source in \
        "${data_dir%/*}/genesis.toml" \
        "${data_dir%/*}/deploy/config/genesis.toml" \
        "${data_dir%/*}/testnet-seeds.txt" \
        "$executable_path"; do
        [ -f "$source" ] || continue
        case "$source" in
            */deploy/config/genesis.toml) destination="$temporary/legacy-public/deploy-genesis.toml" ;;
            "$executable_path") destination="$temporary/legacy-public/arc-node" ;;
            *) destination="$temporary/legacy-public/${source##*/}" ;;
        esac
        copy_stable_file "$source" "$destination"
    done

    {
        printf 'capture_id=%s\n' "$capture_id"
        printf 'node=%s\n' "$node"
        printf 'freeze_plan_sha256=%s\n' "$freeze_sha"
        printf 'hostname=%s\n' "$(hostname)"
        printf 'captured_offline_at=%s\n' "$(date -u +%Y-%m-%dT%H:%M:%SZ)"
        printf 'kernel=%s\n' "$(uname -srmo)"
        printf 'archive_scope=complete-content-indexed-stopped-legacy-source-v4\n'
        printf 'source_tree_content_sealed_by_index=true\n'
        printf 'source_tree_os_read_only=false\n'
        printf 'source_data_dir=%s\n' "$data_dir"
        printf 'source_index_sha256=%s\n' "$(hash_file "$temporary/source-data.files.sha256")"
        printf 'legacy_live_observations_schema=arc.recovery.legacy-live-observations.v1\n'
        printf 'legacy_live_observations_root_sha256=%s\n' "$live_root_sha"
        printf 'legacy_live_observations_receipt_sha256=%s\n' "$live_receipt_sha"
        printf 'legacy_live_observations_labels=diagnostic,noncanonical,nonreward\n'
        printf 'excluded_outside_data_dir_private_material=true\n'
        printf 'excluded_service_environments=true\n'
        printf 'excluded_build_models_and_git=true\n'
    } > "$temporary/capture.inventory"

    [ -s "$data_dir/state.wal" ] || die "fenced content source has no final state.wal"
    rm -f -- "$temporary/.arc-recovery-partial-owner"
    write_tree_index "$temporary" capture.files.sha256 capture.complete
    write_complete_marker "$temporary" capture.files.sha256 capture.complete arc.recovery.capture.v4 \
        "capture_id=$capture_id" "node=$node" "stopped=true" \
        "source_tree_content_sealed_by_index=true" "source_tree_os_read_only=false" \
        "legacy_live_observations_root_sha256=$live_root_sha" \
        "legacy_live_observations_receipt_sha256=$live_receipt_sha"
    chmod -R a-w,go-rwx -- "$temporary"
    mv -- "$temporary" "$capture_root"
    ARCHIVE_NODE_TEMP_PATH=""
    verify_capture_source "$capture_root" "$capture_id" "$node"
    printf 'archive node: OFFLINE CONTENT CAPTURE COMPLETE capture=%s node=%s\n' "$capture_id" "$node"
}

capture_status() {
    local capture_id="$1"
    local node="$2"
    require_hash "$capture_id" "capture id"
    require_node "$node"
    require_commands pgrep python3
    local capture_root="$CAPTURE_BASE/$capture_id/$node"
    local stop_root="$STOP_BASE/$capture_id/$node"
    verify_capture_source "$capture_root" "$capture_id" "$node"
    if pgrep -x arc-node >/dev/null 2>&1; then
        die "capture is complete but arc-node is running"
    fi
    verify_legacy_restart_fence "$stop_root"
    printf '{"capture_id":"%s","node":"%s","capture_complete":true,"stopped":true}\n' \
        "$capture_id" "$node"
}

sealed_source_status() {
    local capture_id="$1" node="$2"
    local capture_root="$CAPTURE_BASE/$capture_id/$node"
    local stop_root="$STOP_BASE/$capture_id/$node"
    require_hash "$capture_id" "capture id"
    require_node "$node"
    verify_tree_index "$stop_root" stop.files.sha256 stop.complete
    verify_stop_identity "$stop_root" "$capture_id" "$node"
    [ "$#" -eq 21 ] || die "sealed-source-status requires the exact freeze writer contract"
    verify_sealed_stop_contract "$stop_root" "$3" "$4" "$5" "$6" "$7" \
        "$8" "$9" "${10}" "${11}" "${12}" "${13}" "${14}" "${15}" \
        "${16}" "${17}" "${18}" "${19}" "${20}" "${21}"
    verify_legacy_restart_fence "$stop_root"
    verify_capture_source "$capture_root" "$capture_id" "$node"
    printf '{"capture_id":"%s","node":"%s","content_sealed":true,"restart_fenced":true}\n' \
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

persisted_head() {
    local capture_id="$1" node="$2" freeze_sha="$3" binary_sha="$4"
    local genesis_sha="$5" validators_sha="$6" legacy_validators_sha="$7" boot_id="$8"
    require_hash "$capture_id" "capture id"; require_node "$node"; require_hash "$freeze_sha" "freeze plan hash"
    require_hash "$binary_sha" "persisted-head exporter binary hash"
    require_hash "$genesis_sha" "persisted-head genesis hash"
    require_hash "$validators_sha" "persisted-head validator public-key hash"
    require_hash "$legacy_validators_sha" "persisted-head legacy validator-set hash"
    printf '%s\n' "$boot_id" | grep -Eq '^[0-9a-f]{8}-[0-9a-f]{4}-[0-9a-f]{4}-[0-9a-f]{4}-[0-9a-f]{12}$' || \
        die "persisted-head sealed boot id is malformed"
    require_commands python3 mktemp chmod pgrep find mkdir stat
    local capture_root="$CAPTURE_BASE/$capture_id/$node"
    local stop_root="$STOP_BASE/$capture_id/$node"
    local stage_root="$SEAL_BASE/$freeze_sha/$node"
    local parent="$PERSISTED_HEAD_BASE/$capture_id" output="$PERSISTED_HEAD_BASE/$capture_id/$node.json"
    verify_tree_index "$stop_root" stop.files.sha256 stop.complete
    verify_stop_identity "$stop_root" "$capture_id" "$node"
    verify_stop_journal_semantics "$stop_root" "$capture_id" "$node" "$freeze_sha"
    verify_legacy_restart_fence "$stop_root"
    verify_legacy_network_quarantine "$stop_root" "$capture_id" "$node" "$freeze_sha"
    verify_tree_index "$capture_root" capture.files.sha256 capture.complete
    verify_capture_source "$capture_root" "$capture_id" "$node"
    pgrep -x arc-node >/dev/null 2>&1 && die "persisted-head exporter requires the exact legacy writer to remain stopped"
    local data_dir snapshot
    data_dir="$(capture_source_data_dir "$capture_root")"
    snapshot="$(python3 - "$capture_root/capture-source.json" <<'PY'
import hashlib,json,pathlib,stat,sys
source=json.loads(pathlib.Path(sys.argv[1]).read_text(encoding="utf-8"))
data=pathlib.Path(source["data_dir"])
candidates=[data/"state.snapshot.lz4",pathlib.Path(f"{data}.snapshot.lz4")]
candidates += [pathlib.Path(row["path"]) for row in source.get("external_snapshots",[])]
rows=[]
for path in candidates:
    if not path.exists() and not path.is_symlink(): continue
    details=path.lstat()
    if path.is_symlink() or not stat.S_ISREG(details.st_mode):
        raise SystemExit(f"persisted-head snapshot candidate is unsafe: {path}")
    value=hashlib.sha256()
    with path.open("rb") as handle:
        for chunk in iter(lambda:handle.read(1024*1024),b""): value.update(chunk)
    digest=value.hexdigest()
    rows.append((str(path),digest))
if not rows: raise SystemExit("persisted-head capture has no stopped snapshot")
if len({digest for _path,digest in rows})!=1:
    raise SystemExit("persisted-head capture snapshot candidates disagree")
print(rows[0][0])
PY
)" || die "cannot select exact stopped capture snapshot"
    require_safe_absolute_path "$data_dir" "persisted-head stopped data directory"
    require_safe_absolute_path "$snapshot" "persisted-head stopped snapshot"
    local binary="$stage_root/arc-node" genesis="$stage_root/genesis.toml"
    local validators="$stage_root/validator-public-keys.json"
    local legacy_validators="$stage_root/legacy-validator-set-40m.json"
    python3 - "$binary" "$binary_sha" 500 "$genesis" "$genesis_sha" 400 \
        "$validators" "$validators_sha" 400 "$legacy_validators" "$legacy_validators_sha" 400 \
        "$snapshot" "$data_dir" "$boot_id" "$stop_root" "$capture_root" <<'PY'
import hashlib,os,pathlib,stat,sys
args=sys.argv[1:]
for offset in range(0,12,3):
    path=pathlib.Path(args[offset]); expected=args[offset+1]; mode=int(args[offset+2])
    details=path.lstat()
    if (path.is_symlink() or not stat.S_ISREG(details.st_mode) or details.st_uid!=0
            or details.st_gid!=0 or details.st_nlink!=1 or stat.S_IMODE(details.st_mode)!=mode
            or hashlib.sha256(path.read_bytes()).hexdigest()!=expected):
        raise SystemExit(f"persisted-head staged input is unsafe or differs: {path}")
snapshot=pathlib.Path(args[12]); data=pathlib.Path(args[13]); boot=args[14]
stop=pathlib.Path(args[15]); capture=pathlib.Path(args[16])
for path in (snapshot,data,stop,capture):
    details=path.lstat()
    if path.is_symlink() or details.st_uid!=0 or details.st_gid!=0:
        raise SystemExit(f"persisted-head source/root is unsafe: {path}")
if not stat.S_ISREG(snapshot.lstat().st_mode) or not stat.S_ISDIR(data.lstat().st_mode):
    raise SystemExit("persisted-head snapshot/data types differ")
contract_path=stop/"evidence/writer-contract.json"; raw=contract_path.read_bytes()
contract=__import__("json").loads(raw)
if contract.get("schema")!="arc.recovery.exact-writer.v3" or contract.get("boot_id")!=boot:
    raise SystemExit("persisted-head sealed boot differs from stopped writer contract")
PY
    python3 - "$PERSISTED_HEAD_BASE" "$parent" <<'PY'
import os,pathlib,stat,sys
base,parent=map(pathlib.Path,sys.argv[1:])
root=os.open("/root",os.O_RDONLY|getattr(os,"O_DIRECTORY",0)|getattr(os,"O_NOFOLLOW",0))
try:
    for name in (base.name,parent.name):
        directory=base if name==base.name else parent
        container=root if directory==base else os.open(base,os.O_RDONLY|getattr(os,"O_DIRECTORY",0)|getattr(os,"O_NOFOLLOW",0))
        try:
            try: details=os.stat(name,dir_fd=container,follow_symlinks=False)
            except FileNotFoundError:
                os.mkdir(name,0o700,dir_fd=container);details=os.stat(name,dir_fd=container,follow_symlinks=False)
            if (not stat.S_ISDIR(details.st_mode) or details.st_uid!=0 or details.st_gid!=0
                    or stat.S_IMODE(details.st_mode)!=0o700):
                raise SystemExit("persisted-head create-only directory is unsafe")
            os.fsync(container)
        finally:
            if container!=root: os.close(container)
finally: os.close(root)
PY
    local temporary
    temporary="$(mktemp -d "$parent/.${node}.persisted-head.XXXXXX")"
    ARCHIVE_NODE_TEMP_PATH="$temporary"
    exec 8<"$binary" 9<"$genesis" 10<"$validators" 11<"$legacy_validators" \
        12<"$snapshot" 13<"$data_dir" 14<"$data_dir/state.wal"
    local wal_before snapshot_before wal_size snapshot_size wal_identity snapshot_identity export_exit=0
    wal_before="$(hash_file /proc/self/fd/14)"
    snapshot_before="$(hash_file /proc/self/fd/12)"
    wal_size="$(stat -Lc %s /proc/self/fd/14)"
    snapshot_size="$(stat -Lc %s /proc/self/fd/12)"
    wal_identity="$(stat -Lc %d:%i:%s:%f /proc/self/fd/14)"
    snapshot_identity="$(stat -Lc %d:%i:%s:%f /proc/self/fd/12)"
    python3 - "$wal_identity" "$snapshot_identity" "$temporary" "$snapshot" <<'PY'
import hashlib,os,pathlib,stat,sys
directory=os.dup(13)
try:
    wal=os.open("state.wal",os.O_RDONLY|getattr(os,"O_NOFOLLOW",0),dir_fd=directory)
    held=os.fstat(14)
    opened=os.fstat(wal)
    if (held.st_dev,held.st_ino)!=(opened.st_dev,opened.st_ino):
        raise SystemExit("persisted-head held WAL differs from openat source")
finally: os.close(directory)
snapshot=os.dup(12)
try:
    held_snapshot=os.fstat(12)
    opened_snapshot=os.fstat(snapshot)
    if (held_snapshot.st_dev,held_snapshot.st_ino)!=(opened_snapshot.st_dev,opened_snapshot.st_ino):
        raise SystemExit("persisted-head held snapshot differs from O_NOFOLLOW source")
    snapshot_path=pathlib.Path(sys.argv[4])
    snapshot_parent=os.open(snapshot_path.parent,
        os.O_RDONLY|getattr(os,"O_DIRECTORY",0)|getattr(os,"O_NOFOLLOW",0))
    try:
        fresh_snapshot=os.open(snapshot_path.name,os.O_RDONLY|getattr(os,"O_NOFOLLOW",0),dir_fd=snapshot_parent)
        try:
            fresh_details=os.fstat(fresh_snapshot)
            if (held_snapshot.st_dev,held_snapshot.st_ino)!=(fresh_details.st_dev,fresh_details.st_ino):
                raise SystemExit("persisted-head snapshot pathname changed after held-FD open")
        finally: os.close(fresh_snapshot)
    finally: os.close(snapshot_parent)
    for descriptor,expected in ((wal,sys.argv[1]),(snapshot,sys.argv[2])):
        details=os.fstat(descriptor)
        observed=f"{details.st_dev}:{details.st_ino}:{details.st_size}:{details.st_mode:x}"
        if not stat.S_ISREG(details.st_mode) or observed!=expected:
            raise SystemExit("persisted-head openat/O_NOFOLLOW FD identity differs")
    root=pathlib.Path(sys.argv[3]); staged=root/"export-source"
    os.mkdir(staged,0o700)
    staged_dir=os.open(staged,os.O_RDONLY|getattr(os,"O_DIRECTORY",0)|getattr(os,"O_NOFOLLOW",0))
    try:
        for descriptor,name in ((wal,"state.wal"),(snapshot,"state.snapshot.lz4")):
            os.lseek(descriptor,0,os.SEEK_SET)
            target=os.open(name,os.O_WRONLY|os.O_CREAT|os.O_EXCL|getattr(os,"O_NOFOLLOW",0),0o400,dir_fd=staged_dir)
            try:
                while True:
                    chunk=os.read(descriptor,1024*1024)
                    if not chunk: break
                    os.write(target,chunk)
                os.fsync(target)
            finally: os.close(target)
        os.fsync(staged_dir)
    finally: os.close(staged_dir)
finally:
    os.close(wal);os.close(snapshot)
PY
    [ "$(hash_file "$temporary/export-source/state.wal")" = "$wal_before" ] || die "staged persisted-head WAL differs"
    [ "$(hash_file "$temporary/export-source/state.snapshot.lz4")" = "$snapshot_before" ] || die "staged persisted-head snapshot differs"
    [ "$(stat -c %U:%G:%a:%h "$temporary/export-source/state.wal")" = root:root:400:1 ] || die "staged persisted-head WAL inode is unsafe"
    [ "$(stat -c %U:%G:%a:%h "$temporary/export-source/state.snapshot.lz4")" = root:root:400:1 ] || die "staged persisted-head snapshot inode is unsafe"
    local staged_wal_identity staged_snapshot_identity
    staged_wal_identity="$(stat -Lc %d:%i:%s:%f:%u:%g:%h "$temporary/export-source/state.wal")"
    staged_snapshot_identity="$(stat -Lc %d:%i:%s:%f:%u:%g:%h "$temporary/export-source/state.snapshot.lz4")"
    if /usr/bin/env -i HOME=/root PATH=/usr/bin:/bin LANG=C LC_ALL=C \
        /proc/self/fd/8 recovery export \
        --data-dir "$temporary/export-source" \
        --snapshot "$temporary/export-source/state.snapshot.lz4" \
        --genesis /proc/self/fd/9 --validator-public-keys /proc/self/fd/10 \
        --legacy-validator-set /proc/self/fd/11 --output "$temporary/candidate.arcchkpt" \
        --source-consensus-round 0 --created-at-unix-ms 0 --recovery-epoch 1 \
        --validator-set-id 1 --allow-unbound-legacy-wal \
        > "$temporary/export-summary.json" 2> "$temporary/export.stderr"; then
        export_exit=0
    else
        export_exit="$?"
    fi
    [ "$export_exit" -eq 0 ] || die "persisted-head exact recovery export failed: exit=$export_exit"
    python3 - "$temporary" <<'PY'
import pathlib,stat,sys
root=pathlib.Path(sys.argv[1]); candidate=root/"candidate.arcchkpt"
root_details=root.lstat(); details=candidate.lstat()
if (root.is_symlink() or not stat.S_ISDIR(root_details.st_mode) or root_details.st_uid!=0
        or root_details.st_gid!=0 or stat.S_IMODE(root_details.st_mode)!=0o700):
    raise SystemExit("persisted-head private export root is unsafe")
if (candidate.is_symlink() or not stat.S_ISREG(details.st_mode) or details.st_uid!=0
        or details.st_gid!=0 or details.st_nlink!=1 or details.st_mode&0o022 or details.st_size<=0):
    raise SystemExit("persisted-head candidate checkpoint inode is unsafe")
PY
    write_offline_wal_evidence "$temporary/export-source/state.wal" \
        "$temporary/export-summary.json" "$temporary/offline-wal-boundary.json" "$temporary" || \
        die "persisted-head export summary rejected the exact held WAL boundary"
    /usr/bin/env -i HOME=/root PATH=/usr/bin:/bin LANG=C LC_ALL=C \
        /proc/self/fd/8 recovery inspect --checkpoint "$temporary/candidate.arcchkpt" \
        > "$temporary/candidate.inspect.json" 2> "$temporary/candidate.inspect.stderr" || \
        die "persisted-head held inspector rejected its candidate checkpoint"
    [ "$(hash_file /proc/self/fd/14)" = "$wal_before" ] || die "persisted-head WAL changed during export"
    [ "$(hash_file /proc/self/fd/12)" = "$snapshot_before" ] || die "persisted-head snapshot changed during export"
    [ "$(stat -Lc %d:%i:%s:%f /proc/self/fd/14)" = "$wal_identity" ] || die "persisted-head WAL inode changed during export"
    [ "$(stat -Lc %d:%i:%s:%f /proc/self/fd/12)" = "$snapshot_identity" ] || die "persisted-head snapshot inode changed during export"
    [ "$(hash_file /proc/self/fd/8)" = "$binary_sha" ] || die "persisted-head exporter changed during execution"
    [ "$(hash_file /proc/self/fd/9)" = "$genesis_sha" ] || die "persisted-head genesis changed during execution"
    [ "$(hash_file /proc/self/fd/10)" = "$validators_sha" ] || die "persisted-head validators changed during execution"
    [ "$(hash_file /proc/self/fd/11)" = "$legacy_validators_sha" ] || die "persisted-head legacy validators changed during execution"
    verify_capture_source "$capture_root" "$capture_id" "$node"
    verify_legacy_network_quarantine "$stop_root" "$capture_id" "$node" "$freeze_sha"
    pgrep -x arc-node >/dev/null 2>&1 && die "legacy writer appeared during persisted-head export"
    python3 - "$temporary/export-summary.json" "$temporary/candidate.inspect.json" \
        "$temporary/offline-wal-boundary.json" "$temporary/candidate.arcchkpt" "$output" \
        "$capture_id" "$node" "$freeze_sha" "$boot_id" "$binary_sha" "$genesis_sha" \
        "$validators_sha" "$legacy_validators_sha" "$snapshot" "$snapshot_before" \
        "$snapshot_size" "$data_dir/state.wal" "$wal_before" "$wal_size" \
        "$capture_root" "$stop_root" "$wal_identity" "$snapshot_identity" \
        "$staged_wal_identity" "$staged_snapshot_identity" <<'PY'
import datetime,hashlib,json,os,pathlib,re,stat,sys
(summary_raw,inspect_raw,boundary_raw,candidate_raw,output_raw,capture,node,freeze,boot,binary_sha,genesis_sha,
 validators_sha,legacy_sha,snapshot_raw,snapshot_sha,snapshot_size_raw,wal_raw,wal_sha,
 wal_size_raw,capture_raw,stop_raw,wal_identity_raw,snapshot_identity_raw,
 staged_wal_identity_raw,staged_snapshot_identity_raw)=sys.argv[1:]
summary_path=pathlib.Path(summary_raw); inspect_path=pathlib.Path(inspect_raw)
boundary_path=pathlib.Path(boundary_raw); candidate=pathlib.Path(candidate_raw); output=pathlib.Path(output_raw)
capture_root=pathlib.Path(capture_raw); stop_root=pathlib.Path(stop_raw)
canonical=lambda value:(json.dumps(value,sort_keys=True,separators=(",",":"))+"\n").encode()
digest=lambda path:hashlib.sha256(path.read_bytes()).hexdigest()
bare=lambda value:value.removeprefix("0x") if isinstance(value,str) else ""
def source_identity(raw):
    device,inode,size,mode=raw.split(":")
    return {"device":int(device),"inode":int(inode),"size":int(size),"mode":int(mode,16)}
def staged_identity(raw):
    device,inode,size,mode,uid,gid,nlink=raw.split(":")
    return {"device":int(device),"inode":int(inode),"size":int(size),"mode":int(mode,16),
            "uid":int(uid),"gid":int(gid),"nlink":int(nlink)}
summary_bytes=summary_path.read_bytes(); summary=json.loads(summary_bytes)
inspect_bytes=inspect_path.read_bytes(); inspect=json.loads(inspect_bytes)
boundary_bytes=boundary_path.read_bytes(); boundary=json.loads(boundary_bytes)
source_wal_identity=source_identity(wal_identity_raw)
source_snapshot_identity=source_identity(snapshot_identity_raw)
staged_wal_identity=staged_identity(staged_wal_identity_raw)
staged_snapshot_identity=staged_identity(staged_snapshot_identity_raw)
if (source_wal_identity["size"]!=int(wal_size_raw)
        or source_snapshot_identity["size"]!=int(snapshot_size_raw)
        or any(value["uid"]!=0 or value["gid"]!=0 or value["nlink"]!=1
               or value["mode"]!=0o100400 for value in (staged_wal_identity,staged_snapshot_identity))
        or staged_wal_identity["size"]!=int(wal_size_raw)
        or staged_snapshot_identity["size"]!=int(snapshot_size_raw)):
    raise SystemExit("persisted-head source/staged FD identity contract differs")
summary_keys={
 "status","manifest_hash","payload_hash","full_state_root","chain_id","genesis_hash",
 "source_height","source_block_hash","source_state_root","source_consensus_round",
 "created_at_unix_ms","transition_height","transition_block_hash","recovery_domain",
 "recovery_epoch","validator_set_id","protocol_version","validator_count","signature_count",
 "source_validator_count","source_validator_stake","source_validator_set_hash",
 "community_reward_issuance_policy","community_reward_issuance_policy_hash",
 "source_wal_original_bytes","source_wal_accepted_prefix_bytes",
 "source_wal_quarantined_tail_bytes","source_wal_tail_reason",
}
inspect_keys=summary_keys-{
 "source_wal_original_bytes","source_wal_accepted_prefix_bytes",
 "source_wal_quarantined_tail_bytes","source_wal_tail_reason",
}
if not isinstance(summary,dict) or set(summary)!=summary_keys:
    raise SystemExit("persisted-head export summary exact key set differs")
if not isinstance(inspect,dict) or set(inspect)!=inspect_keys:
    raise SystemExit("persisted-head inspect summary exact key set differs")
height=summary.get("source_height"); block_hash=bare(summary.get("source_block_hash")); state_root=bare(summary.get("source_state_root"))
if (summary.get("status")!="EXPORTED_UNSIGNED" or isinstance(height,bool) or not isinstance(height,int) or height<0
        or not re.fullmatch(r"[0-9a-f]{64}",block_hash) or not re.fullmatch(r"[0-9a-f]{64}",state_root)):
    raise SystemExit("persisted-head exporter summary has no exact source tuple")
for field in ("source_height","source_block_hash","source_state_root","full_state_root",
              "manifest_hash","payload_hash","source_consensus_round","created_at_unix_ms",
              "recovery_epoch","validator_set_id"):
    if field not in summary: raise SystemExit(f"persisted-head export summary omitted {field}")
uint_fields=("source_height","source_consensus_round","created_at_unix_ms","transition_height",
             "recovery_epoch","validator_set_id","validator_count","signature_count",
             "source_validator_count","source_validator_stake","source_wal_original_bytes",
             "source_wal_accepted_prefix_bytes","source_wal_quarantined_tail_bytes")
if any(isinstance(summary.get(field),bool) or not isinstance(summary.get(field),int)
       or summary[field]<0 for field in uint_fields):
    raise SystemExit("persisted-head export summary integer field differs")
if any(isinstance(inspect.get(field),bool) or not isinstance(inspect.get(field),int)
       or inspect[field]<0 for field in uint_fields if field in inspect):
    raise SystemExit("persisted-head inspect summary integer field differs")
hash_fields=("manifest_hash","payload_hash","full_state_root","genesis_hash","source_block_hash",
             "source_state_root","transition_block_hash","recovery_domain",
             "source_validator_set_hash","community_reward_issuance_policy_hash")
if any(not isinstance(summary.get(field),str) or
       re.fullmatch(r"0x[0-9a-f]{64}",summary[field]) is None for field in hash_fields):
    raise SystemExit("persisted-head export summary hash field differs")
if any(inspect.get(field)!=summary[field] for field in hash_fields):
    raise SystemExit("persisted-head inspect/export hash fields differ")
if (not isinstance(summary.get("chain_id"),str) or not summary["chain_id"]
        or not isinstance(summary.get("protocol_version"),str)
        or re.fullmatch(r"3\.[0-9]+\.[0-9]+",summary["protocol_version"]) is None
        or not isinstance(summary.get("community_reward_issuance_policy"),dict)
        or set(summary["community_reward_issuance_policy"])!={"reward_amount","epoch_blocks","max_per_block",
             "max_per_epoch","max_per_worker_epoch","max_per_coordinator_epoch"}
        or any(isinstance(value,bool) or not isinstance(value,int) or value<0
               for value in summary["community_reward_issuance_policy"].values())
        or summary.get("signature_count")!=0
        or summary.get("transition_height")!=height+1):
    raise SystemExit("persisted-head export summary semantic contract differs")
if any(inspect.get(field)!=summary[field] for field in inspect_keys-{"status"}):
    raise SystemExit("persisted-head inspect/export exact summary differs")
if (inspect.get("status")!="UNTRUSTED_INSPECTION"
        or any(inspect.get(field)!=summary.get(field) for field in (
            "source_height","source_block_hash","source_state_root","full_state_root",
            "manifest_hash","payload_hash","source_consensus_round","created_at_unix_ms",
            "recovery_epoch","validator_set_id","transition_height","transition_block_hash"))):
    raise SystemExit("persisted-head inspect/export candidate cross-check differs")
boundary_keys={"schema","capture_wal_sha256","capture_wal_bytes","accepted_prefix_bytes",
               "accepted_prefix_sha256","quarantined_tail_bytes","quarantined_tail_sha256",
               "tail_reason","prefix_plus_tail_sha256","prefix_plus_tail_reconstructs_capture"}
if (not isinstance(boundary,dict) or set(boundary)!=boundary_keys
        or boundary.get("schema")!="arc.recovery.offline-wal-recovery.v2"
        or boundary.get("capture_wal_sha256")!=wal_sha
        or boundary.get("capture_wal_bytes")!=int(wal_size_raw)
        or boundary.get("accepted_prefix_bytes",0)+boundary.get("quarantined_tail_bytes",0)!=int(wal_size_raw)
        or boundary.get("prefix_plus_tail_sha256")!=wal_sha
        or boundary.get("prefix_plus_tail_reconstructs_capture") is not True
        or summary.get("source_wal_original_bytes")!=boundary.get("capture_wal_bytes")
        or summary.get("source_wal_accepted_prefix_bytes")!=boundary.get("accepted_prefix_bytes")
        or summary.get("source_wal_quarantined_tail_bytes")!=boundary.get("quarantined_tail_bytes")
        or summary.get("source_wal_tail_reason")!=boundary.get("tail_reason")):
    raise SystemExit("persisted-head exact WAL boundary receipt differs")
tail=boundary["quarantined_tail_bytes"]
reason=boundary["tail_reason"]
if ((tail==0 and (reason not in (None,"","none")
                  or boundary["accepted_prefix_sha256"] is not None
                  or boundary["quarantined_tail_sha256"] is not None))
        or (tail>0 and (not isinstance(reason,str) or not reason.strip()
                       or not re.fullmatch(r"[0-9a-f]{64}",boundary.get("accepted_prefix_sha256", ""))
                       or not re.fullmatch(r"[0-9a-f]{64}",boundary.get("quarantined_tail_sha256", ""))))):
    raise SystemExit("persisted-head WAL tail reason/hash policy differs")
quarantine=stop_root/"08-network-quarantine.json"
plan_path=pathlib.Path(f"/root/.arc-recovery-plans/{freeze}/freeze.lock.json")
plan_raw=plan_path.read_bytes(); plan=json.loads(plan_raw)
if (hashlib.sha256(plan_raw).hexdigest()!=freeze or plan_raw!=canonical(plan)
        or plan.get("schema")!="arc.recovery.freeze-plan.v5"
        or not re.fullmatch(r"[0-9a-f]{40}",plan.get("source_commit", ""))):
    raise SystemExit("persisted-head pinned freeze/source commit differs")
partial=output.with_name(f".{output.name}.partial")
prior_source=None
if output.exists() and not output.is_symlink(): prior_source=output
elif partial.exists() and not partial.is_symlink():
    details=partial.lstat()
    if (not stat.S_ISREG(details.st_mode) or details.st_uid!=0 or details.st_gid!=0
            or details.st_nlink!=1 or stat.S_IMODE(details.st_mode)!=0o400):
        raise SystemExit("persisted-head publication partial inode is unsafe")
    try:
        parsed=json.loads(partial.read_text(encoding="utf-8"))
        if isinstance(parsed,dict) and parsed.get("schema")=="arc.recovery.persisted-legacy-head.v1":
            prior_source=partial
    except (UnicodeError,json.JSONDecodeError): pass
if prior_source is not None:
    prior=json.loads(prior_source.read_text(encoding="utf-8")); completed_at=prior.get("completed_at")
else: completed_at=datetime.datetime.now(datetime.timezone.utc).replace(microsecond=0).isoformat().replace("+00:00","Z")
if not isinstance(completed_at,str) or not re.fullmatch(r"[0-9]{4}-[0-9]{2}-[0-9]{2}T[0-9]{2}:[0-9]{2}:[0-9]{2}Z",completed_at):
    raise SystemExit("persisted-head completion timestamp is malformed")
receipt={
 "schema":"arc.recovery.persisted-legacy-head.v1","source_main_commit":plan["source_commit"],
 "capture_id":capture,"node":node,"freeze_plan_sha256":freeze,"boot_id":boot,
 "inspector_binary_sha256":binary_sha,"genesis_sha256":genesis_sha,
 "validator_public_keys_sha256":validators_sha,"legacy_validator_set_sha256":legacy_sha,
 "network_quarantine_receipt_sha256":digest(quarantine),
 "stop_complete_sha256":digest(stop_root/"stop.complete"),
 "stop_files_sha256":digest(stop_root/"stop.files.sha256"),
 "capture_complete_sha256":digest(capture_root/"capture.complete"),
 "capture_files_sha256":digest(capture_root/"capture.files.sha256"),
 "capture_source_sha256":digest(capture_root/"capture-source.json"),
 "source_data_index_sha256":digest(capture_root/"source-data.files.sha256"),
 "state_wal_sha256":wal_sha,"state_wal_size":int(wal_size_raw),
 "snapshot_sha256":snapshot_sha,"snapshot_size":int(snapshot_size_raw),
 "source_file_identity":{
   "state_wal":source_wal_identity,"snapshot":source_snapshot_identity,
 },
 "staged_file_contract":{
   "state_wal":{"sha256":wal_sha,"size":int(wal_size_raw),"mode":0o100400,"uid":0,"gid":0,"nlink":1},
   "snapshot":{"sha256":snapshot_sha,"size":int(snapshot_size_raw),"mode":0o100400,"uid":0,"gid":0,"nlink":1},
   "ephemeral_inode_receipted":False,
 },
 "export_summary_sha256":hashlib.sha256(summary_bytes).hexdigest(),
 "inspect_summary_sha256":hashlib.sha256(inspect_bytes).hexdigest(),
 "wal_boundary_sha256":hashlib.sha256(boundary_bytes).hexdigest(),
 "export_status":"EXPORTED_UNSIGNED",
 "head":{"height":height,"block_hash":block_hash,"state_root":state_root},
 "candidate_checkpoint_sha256":digest(candidate),"candidate_checkpoint_size":candidate.stat().st_size,
 "snapshot_path":snapshot_raw,"state_wal_path":wal_raw,
 "export_contract":{"binary_path":"/proc/self/fd/8","exit_code":0,
                    "source_consensus_round":0,"created_at_unix_ms":0,
                    "recovery_epoch":1,"validator_set_id":1,
                    "allow_unbound_legacy_wal":True,"read_only":True},
 "completed_at":completed_at,"rerun_reexecutes_export":True,
 "writer_stopped":True,"restart_barrier_active":True,"network_quarantine_active":True,
 "global_absence_claimed":False,
}
payload=canonical(receipt)
if output.exists() or output.is_symlink():
    details=output.lstat()
    if (output.is_symlink() or not stat.S_ISREG(details.st_mode) or details.st_uid!=0 or details.st_gid!=0
            or details.st_nlink!=1 or stat.S_IMODE(details.st_mode)!=0o400 or output.read_bytes()!=payload):
        raise SystemExit("persisted-head rerun differs from its create-only receipt")
else:
    temporary=partial
    if temporary.is_symlink(): raise SystemExit("persisted-head publication partial is unsafe")
    if temporary.exists():
        details=temporary.lstat(); existing=temporary.read_bytes()
        if (not stat.S_ISREG(details.st_mode) or details.st_uid!=0 or details.st_gid!=0
                or details.st_nlink!=1 or stat.S_IMODE(details.st_mode)!=0o400):
            raise SystemExit("persisted-head publication partial inode is unsafe")
        if existing==payload:
            os.replace(temporary,output)
            descriptor=os.open(output.parent,os.O_RDONLY|getattr(os,"O_DIRECTORY",0)|getattr(os,"O_NOFOLLOW",0))
            try: os.fsync(descriptor)
            finally: os.close(descriptor)
            print(json.dumps(receipt,sort_keys=True,separators=(",",":")))
            raise SystemExit(0)
        try: parsed_existing=json.loads(existing)
        except (UnicodeError,json.JSONDecodeError): parsed_existing=None
        if parsed_existing is not None and canonical(parsed_existing)==existing:
            raise SystemExit("persisted-head complete publication partial differs from re-executed receipt")
        # A crash can leave any byte prefix, including one after completed_at.
        # The exact 0700 ancestry plus root-owned fixed-name 0400/nlink1 inode
        # identifies this as our incomplete publication; never interpret its
        # bytes and never promote it.
        os.unlink(temporary)
        descriptor=os.open(output.parent,os.O_RDONLY|getattr(os,"O_DIRECTORY",0)|getattr(os,"O_NOFOLLOW",0))
        try: os.fsync(descriptor)
        finally: os.close(descriptor)
    descriptor=os.open(temporary,os.O_WRONLY|os.O_CREAT|os.O_EXCL|getattr(os,"O_NOFOLLOW",0),0o400)
    try:
        offset=0
        while offset<len(payload): offset+=os.write(descriptor,payload[offset:])
        if os.environ.get("ARC_RECOVERY_PERSISTED_HEAD_FAIL_AT")=="after-write":
            raise SystemExit("injected persisted-head failure after write")
        os.fsync(descriptor)
    finally: os.close(descriptor)
    if os.environ.get("ARC_RECOVERY_PERSISTED_HEAD_FAIL_AT")=="after-file-fsync":
        raise SystemExit("injected persisted-head failure after file fsync")
    os.replace(temporary,output)
    if os.environ.get("ARC_RECOVERY_PERSISTED_HEAD_FAIL_AT")=="after-rename":
        raise SystemExit("injected persisted-head failure after rename")
    descriptor=os.open(output.parent,os.O_RDONLY|getattr(os,"O_DIRECTORY",0)|getattr(os,"O_NOFOLLOW",0))
    try: os.fsync(descriptor)
    finally: os.close(descriptor)
print(json.dumps(receipt,sort_keys=True,separators=(",",":")))
PY
    find "$temporary" -depth -delete
    ARCHIVE_NODE_TEMP_PATH=""
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
        cli) filename=arc-cli; mode=500 ;;
        genesis) filename=genesis.toml; mode=400 ;;
        validators) filename='validator-public-keys.json'; mode=400 ;;
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

validator_key_identity() {
    local manifest="$1" node="$2" cli_sha="$3" key_sha="$4" expected_address="$5"
    require_hash "$manifest" "rollout manifest hash"
    require_node "$node"
    require_hash "$cli_sha" "validator identity CLI hash"
    require_hash "$key_sha" "validator keyfile hash"
    expected_address="$(normalize_hash "$expected_address" "validator identity address")"
    require_commands sha256sum stat
    local stage_root="$SEAL_BASE/$manifest/$node"
    local cli="$stage_root/arc-cli"
    local key=/etc/arc-v3/validator-key.json
    [ -f "$cli" ] && [ ! -L "$cli" ] || die "staged validator identity CLI is unavailable"
    [ "$(stat -c %U:%G:%a:%h "$cli")" = root:root:500:1 ] || \
        die "staged validator identity CLI owner/mode/link contract differs"
    [ -f "$key" ] && [ ! -L "$key" ] || die "validator keyfile is unavailable"
    [ "$(stat -c %U:%G:%a:%h "$key")" = root:root:600:1 ] || \
        die "validator keyfile owner/mode/link contract differs"
    exec 8<"$cli"
    [ -f /proc/self/fd/8 ] || die "validator identity CLI descriptor is unavailable"
    [ "$(sha256sum /proc/self/fd/8 | cut -d' ' -f1)" = "$cli_sha" ] || \
        die "validator identity CLI differs from its sealed hash"
    [ "$(sha256sum "$key" | cut -d' ' -f1)" = "$key_sha" ] || \
        die "validator keyfile differs from its authenticated install receipt"
    local derived
    derived="$(/usr/bin/env -i HOME=/root PATH=/usr/bin:/bin LANG=C LC_ALL=C \
        /proc/self/fd/8 keygen --verify-keyfile "$key")" || \
        die "sealed CLI rejected the validator keyfile"
    [ "$derived" = "$expected_address" ] || \
        die "sealed CLI derived a validator address outside the reviewed fleet mapping"
    [ "$(sha256sum /proc/self/fd/8 | cut -d' ' -f1)" = "$cli_sha" ] || \
        die "validator identity CLI changed during execution"
    [ "$(sha256sum "$key" | cut -d' ' -f1)" = "$key_sha" ] || \
        die "validator keyfile changed during identity verification"
    printf '{"address":"%s","cli_sha256":"%s","keyfile_sha256":"%s","node":"%s","schema":"arc.recovery.validator-key-identity.v1"}\n' \
        "$expected_address" "$cli_sha" "$key_sha" "$node"
}

validator_key_identity_transient() {
    local node="$1" cli_sha="$2" key_sha="$3" expected_address="$4" challenge="$5"
    require_node "$node"
    require_hash "$cli_sha" "validator identity CLI hash"
    require_hash "$key_sha" "validator keyfile hash"
    expected_address="$(normalize_hash "$expected_address" "validator identity address")"
    require_hash "$challenge" "validator identity challenge"
    require_commands cat chmod mktemp rm sha256sum stat
    local key=/etc/arc-v3/validator-key.json temporary derived
    [ -f "$key" ] && [ ! -L "$key" ] || die "validator keyfile is unavailable"
    [ "$(stat -c %U:%G:%a:%h "$key")" = root:root:600:1 ] || \
        die "validator keyfile owner/mode/link contract differs"
    temporary="$(mktemp /root/.arc-validator-key-proof.XXXXXX)"
    ARCHIVE_NODE_TEMP_PATH="$temporary"
    [ -f "$temporary" ] && [ ! -L "$temporary" ] || \
        die "transient validator identity CLI is unavailable"
    [ "$(stat -c %U:%G:%a:%h "$temporary")" = root:root:600:1 ] || \
        die "transient validator identity CLI creation contract differs"
    cat > "$temporary"
    [ "$(sha256sum "$temporary" | cut -d' ' -f1)" = "$cli_sha" ] || \
        die "transient validator identity CLI differs from its sealed hash"
    chmod 500 -- "$temporary"
    exec 8<"$temporary"
    rm -f -- "$temporary"
    ARCHIVE_NODE_TEMP_PATH=""
    [ -f /proc/self/fd/8 ] || die "transient validator identity CLI descriptor is unavailable"
    [ "$(stat -Lc %U:%G:%a:%h /proc/self/fd/8)" = root:root:500:0 ] || \
        die "transient validator identity CLI was not unlinked before execution"
    [ "$(sha256sum /proc/self/fd/8 | cut -d' ' -f1)" = "$cli_sha" ] || \
        die "transient validator identity CLI descriptor differs"
    [ "$(sha256sum "$key" | cut -d' ' -f1)" = "$key_sha" ] || \
        die "validator keyfile differs from its authenticated install receipt"
    derived="$(/usr/bin/env -i HOME=/root PATH=/usr/bin:/bin LANG=C LC_ALL=C \
        /proc/self/fd/8 keygen --verify-keyfile "$key")" || \
        die "sealed transient CLI rejected the validator keyfile"
    [ "$derived" = "$expected_address" ] || \
        die "sealed transient CLI derived a validator address outside the reviewed fleet mapping"
    [ "$(sha256sum /proc/self/fd/8 | cut -d' ' -f1)" = "$cli_sha" ] || \
        die "transient validator identity CLI changed during execution"
    [ "$(sha256sum "$key" | cut -d' ' -f1)" = "$key_sha" ] || \
        die "validator keyfile changed during identity verification"
    printf '{"address":"%s","challenge":"%s","cli_sha256":"%s","keyfile_sha256":"%s","node":"%s","schema":"arc.recovery.validator-key-identity-challenged.v1"}\n' \
        "$expected_address" "$challenge" "$cli_sha" "$key_sha" "$node"
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
    verify_capture_source "$capture_root" "$capture_id" "$node"
    local capture_data_dir
    capture_data_dir="$(capture_source_data_dir "$capture_root")"
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

    local temporary="$binding_parent/.${node}.binding.partial"
    prepare_owned_partial_directory "$temporary" \
        "schema=arc.recovery.partial.v1 capture=$capture_id node=$node manifest=$manifest phase=binding"
    ARCHIVE_NODE_TEMP_PATH="$temporary"
    "$stage_root/arc-node" recovery inspect \
        --checkpoint "$stage_root/recovery.arcchkpt" \
        > "$temporary/final-checkpoint.inspect.json" 2> "$temporary/final-checkpoint.inspect.stderr"

    # A final validator capture is classified only from that validator's own
    # stopped data directory and its own on-disk snapshot.  The independently
    # sealed canonical source snapshot is reference evidence; substituting it
    # here would turn a real fork (or an unpaired capture) into invented state.
    python3 - "$capture_root/capture-source.json" \
        "$temporary/capture-snapshot.selection.json" <<'PY'
import hashlib
import json
import pathlib
import stat
import sys

source = json.loads(pathlib.Path(sys.argv[1]).read_text(encoding="utf-8"))
selection_path = pathlib.Path(sys.argv[2])
data_dir = pathlib.Path(source["data_dir"])
candidates = [data_dir / "state.snapshot.lz4"] + [
    pathlib.Path(row["path"]) for row in source["external_snapshots"]
]
rows = []
for path in candidates:
    if not path.exists() and not path.is_symlink():
        continue
    row = {"path": str(path)}
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
        local capture_wal_before selected_snapshot
        capture_wal_before="$(hash_file "$capture_data_dir/state.wal")"
        selected_snapshot="$(python3 -c 'import json,sys; print(json.load(open(sys.argv[1], encoding="utf-8"))["selected"]["path"])' \
            "$temporary/capture-snapshot.selection.json")"
        require_safe_absolute_path "$selected_snapshot" "selected capture snapshot"

        local export_command=(
            "$stage_root/arc-node" recovery export
            --data-dir "$capture_data_dir"
            --snapshot "$selected_snapshot"
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

        if [ "$export_exit_code" -eq 0 ] && write_offline_wal_evidence \
            "$capture_data_dir/state.wal" "$temporary/export-summary.json" \
            "$temporary/offline-wal-recovery.json" "$temporary" \
            2> "$temporary/wal-evidence.stderr"; then
            :
        elif [ "$export_exit_code" -eq 0 ]; then
            export_exit_code=126
            printf 'offline WAL evidence rejected: ' >> "$temporary/export.stderr"
            cat "$temporary/wal-evidence.stderr" >> "$temporary/export.stderr"
        fi
        [ "$(hash_file "$capture_data_dir/state.wal")" = "$capture_wal_before" ] || \
            die "content-sealed capture WAL changed during offline export"
        verify_capture_source "$capture_root" "$capture_id" "$node"
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

    rm -f -- "$temporary/.arc-recovery-partial-owner"
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
    verify_capture_source "$capture_root" "$capture_id" "$node"
    verify_tree_index "$binding_root" binding.files.sha256 binding.complete
    verify_binding_identity "$binding_root" "$capture_id" "$node" "$manifest"

    local archive_root="$ARCHIVE_BASE/$manifest"
    local bundle_root="$archive_root/$node"
    local archive="$bundle_root/legacy-$node.tar.zst"
    local checksum="$archive.sha256"
    local inventory="$bundle_root/legacy-$node.inventory"
    local inventory_checksum="$inventory.sha256"
    mkdir -p -- "$ARCHIVE_BASE" "$archive_root"
    chmod 700 -- "$ARCHIVE_BASE" "$archive_root"
    if [ -e "$bundle_root" ]; then
        [ -d "$bundle_root" ] && [ ! -L "$bundle_root" ] || \
            die "existing bundle root is not a real directory"
        [ -s "$archive" ] && [ -s "$checksum" ] && [ -s "$inventory" ] && [ -s "$inventory_checksum" ] || \
            die "existing immutable bundle root is incomplete"
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

    local temporary="$archive_root/.${node}.bundle.partial"
    prepare_owned_partial_directory "$temporary" \
        "schema=arc.recovery.partial.v1 capture=$capture_id node=$node manifest=$manifest phase=bundle"
    ARCHIVE_NODE_TEMP_PATH="$temporary"
    archive="$temporary/legacy-$node.tar.zst"
    checksum="$archive.sha256"
    inventory="$temporary/legacy-$node.inventory"
    inventory_checksum="$inventory.sha256"

    local canonical classification freeze_sha live_root live_root_sha live_receipt_sha
    canonical="$(python3 -c 'import json,sys; print(str(json.load(open(sys.argv[1]))["canonical_match"]).lower())' "$binding_root/binding.json")"
    classification="$(python3 -c 'import json,sys; print(json.load(open(sys.argv[1]))["classification"])' "$binding_root/binding.json")"
    freeze_sha="$(sed -n 's/^freeze_plan_sha256=//p' "$capture_root/capture.inventory")"
    live_root="$LIVE_OBSERVATION_BASE/$capture_id/$node"
    verify_live_observation_receipt "$live_root" "$capture_id" "$node" "$freeze_sha"
    live_root_sha="$(hash_file "$live_root/live-observations.files.sha256")"
    live_receipt_sha="$(hash_file "$live_root/receipt.json")"
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

    tar --create --zstd --numeric-owner --acls --xattrs --sparse --one-file-system \
        --file "$archive" --directory /root \
        "arc-recovery-captures/$capture_id/$node" \
        "arc-recovery-bindings/$manifest/$node" \
        "arc-recovery-live-observations/$capture_id/$node"
    printf '%s  %s\n' "$(hash_file "$archive")" "${archive##*/}" > "$checksum"
    printf '%s  %s\n' "$(hash_file "$inventory")" "${inventory##*/}" > "$inventory_checksum"
    chmod 400 -- "$archive" "$checksum" "$inventory" "$inventory_checksum"
    zstd --test --quiet "$archive"
    tar --list --zstd --file "$archive" >/dev/null
    rm -f -- "$temporary/.arc-recovery-partial-owner"
    sync "$archive" "$checksum" "$inventory" "$inventory_checksum"
    mv -- "$temporary" "$bundle_root"
    ARCHIVE_NODE_TEMP_PATH=""
    archive="$bundle_root/legacy-$node.tar.zst"
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
    local bundle_root="$archive_root/$node"
    local archive="$bundle_root/legacy-$node.tar.zst"
    local checksum="$archive.sha256"
    local inventory="$bundle_root/legacy-$node.inventory"
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

stream_bundle() {
    local capture_id="$1" node="$2" manifest="$3"
    require_hash "$capture_id" "capture id"
    require_node "$node"
    require_hash "$manifest" "manifest hash"
    require_commands python3 tar zstd pgrep
    local capture_root="$CAPTURE_BASE/$capture_id/$node"
    local stop_root="$STOP_BASE/$capture_id/$node"
    local binding_root="$BINDING_BASE/$manifest/$node"
    local freeze_sha live_root
    verify_capture_source "$capture_root" "$capture_id" "$node"
    verify_tree_index "$binding_root" binding.files.sha256 binding.complete
    verify_binding_identity "$binding_root" "$capture_id" "$node" "$manifest"
    pgrep -x arc-node >/dev/null 2>&1 && die "refusing archive stream while arc-node is running"
    verify_legacy_restart_fence "$stop_root"
    freeze_sha="$(sed -n 's/^freeze_plan_sha256=//p' "$capture_root/capture.inventory")"
    live_root="$LIVE_OBSERVATION_BASE/$capture_id/$node"
    verify_live_observation_receipt "$live_root" "$capture_id" "$node" "$freeze_sha"

    local data_dir
    data_dir="$(capture_source_data_dir "$capture_root")"
    require_safe_absolute_path "$data_dir" "archive stream data directory"
    local members=(
        "${data_dir#/}"
        "${capture_root#/}"
        "${binding_root#/}"
        "${live_root#/}"
    )
    local snapshot
    while IFS= read -r snapshot; do
        [ -n "$snapshot" ] || continue
        require_safe_absolute_path "$snapshot" "archive stream external snapshot"
        members+=("${snapshot#/}")
    done < <(python3 - "$capture_root/capture-source.json" <<'PY'
import json
import sys
value = json.load(open(sys.argv[1], encoding="utf-8"))
for row in value["external_snapshots"]:
    print(row["path"])
PY
    )

    # No model/build/git path is a member: the resolved model bytes are sealed
    # independently in the rollout inventory and remain at their absolute path.
    tar --create --zstd --sort=name --numeric-owner --acls --xattrs --sparse \
        --one-file-system --file - --directory / "${members[@]}"
    verify_capture_source "$capture_root" "$capture_id" "$node"
}

stream_inventory() {
    local capture_id="$1" node="$2" manifest="$3"
    require_hash "$capture_id" "capture id"
    require_node "$node"
    require_hash "$manifest" "manifest hash"
    local capture_root="$CAPTURE_BASE/$capture_id/$node"
    local binding_root="$BINDING_BASE/$manifest/$node"
    local freeze_sha live_root live_root_sha live_receipt_sha
    verify_capture_source "$capture_root" "$capture_id" "$node"
    verify_tree_index "$binding_root" binding.files.sha256 binding.complete
    verify_binding_identity "$binding_root" "$capture_id" "$node" "$manifest"
    freeze_sha="$(sed -n 's/^freeze_plan_sha256=//p' "$capture_root/capture.inventory")"
    live_root="$LIVE_OBSERVATION_BASE/$capture_id/$node"
    verify_live_observation_receipt "$live_root" "$capture_id" "$node" "$freeze_sha"
    live_root_sha="$(hash_file "$live_root/live-observations.files.sha256")"
    live_receipt_sha="$(hash_file "$live_root/receipt.json")"
    local canonical classification
    canonical="$(python3 -c 'import json,sys; print(str(json.load(open(sys.argv[1]))["canonical_match"]).lower())' "$binding_root/binding.json")"
    classification="$(python3 -c 'import json,sys; print(json.load(open(sys.argv[1]))["classification"])' "$binding_root/binding.json")"
    case "$classification" in
        valid_canonical|valid_noncanonical_fork|preserved_unclassified) ;;
        *) die "stream inventory classification is invalid" ;;
    esac
    {
        printf 'manifest_sha256=%s\n' "$manifest"
        printf 'capture_id=%s\n' "$capture_id"
        printf 'node=%s\n' "$node"
        printf 'classification=%s\n' "$classification"
        printf 'canonical_match=%s\n' "$canonical"
        printf 'archive_scope=complete-content-indexed-stopped-legacy-source-v4\n'
        printf 'source_tree_retained_locally=true\n'
        printf 'model_excluded_and_bound_by_rollout=true\n'
        printf 'capture_index_sha256=%s\n' "$(hash_file "$capture_root/capture.files.sha256")"
        printf 'source_index_sha256=%s\n' "$(hash_file "$capture_root/source-data.files.sha256")"
        printf 'binding_index_sha256=%s\n' "$(hash_file "$binding_root/binding.files.sha256")"
    }
}

quarantine_retire() {
    [ "$#" -eq 7 ] || die "quarantine-retire requires the exact capture/rollout/archive boundary roots"
    local capture_id="$1" node="$2" freeze_sha="$3" rollout_sha="$4"
    local archive_sha="$5" boundary_sha="$6" bundle_sha="$7"
    require_hash "$capture_id" "capture id"; require_node "$node"
    require_hash "$freeze_sha" "freeze plan hash"
    require_hash "$rollout_sha" "rollout manifest hash"
    require_hash "$archive_sha" "archive manifest hash"
    require_hash "$boundary_sha" "legacy maintenance boundary hash"
    require_hash "$bundle_sha" "legacy maintenance evidence bundle hash"
    local stop_root="$STOP_BASE/$capture_id/$node"
    local retirement_root="$NETWORK_FENCE_STATE/retirements/$rollout_sha/$node"
    [ -d "$stop_root" ] && [ ! -L "$stop_root" ] || die "final stopped journal is unavailable for quarantine retirement"
    # The first irreversible byte is published only after the complete stopped,
    # restart-fenced, fully monitored state has been re-proved.  A retry after
    # INTENT must not call the pre-retirement verifier because the table/unit
    # are deliberately absent in later phases.
    if [ ! -e "$retirement_root/INTENT.json" ] && [ ! -L "$retirement_root/INTENT.json" ]; then
        stopped_status "$capture_id" "$node" >/dev/null
        quarantine_restart_status "$capture_id" "$node" "$freeze_sha" >/dev/null
        verify_legacy_network_quarantine "$stop_root" "$capture_id" "$node" "$freeze_sha"
    fi
    python3 - "$stop_root" "$retirement_root" "$capture_id" "$node" "$freeze_sha" \
        "$rollout_sha" "$archive_sha" "$boundary_sha" "$bundle_sha" \
        "$NETWORK_FENCE_STATE" "$NETWORK_FENCE_UNIT" \
        "${ARC_RECOVERY_RETIRE_MODE:-execute}" <<'PY'
import datetime, hashlib, json, os, pathlib, re, stat, subprocess, sys

(stop_raw, journal_raw, capture, node, freeze, rollout, archive, boundary,
 bundle, state_raw, unit_raw, mode) = sys.argv[1:]
stop = pathlib.Path(stop_raw); journal = pathlib.Path(journal_raw)
state = pathlib.Path(state_raw); unit = pathlib.Path(unit_raw)
hash_re = re.compile(r"[0-9a-f]{64}")
units = ("arc-self-heal.service", "arc-node.service",
         "arc-node-update.service", "arc-node-update.timer")
table = "arc_legacy_maintenance_v1"

def canonical(value):
    return (json.dumps(value, sort_keys=True, separators=(",", ":")) + "\n").encode()

def sha(raw): return hashlib.sha256(raw).hexdigest()
def now():
    return datetime.datetime.now(datetime.timezone.utc).replace(microsecond=0).strftime("%Y-%m-%dT%H:%M:%SZ")

def fsync_dir(path):
    descriptor = os.open(path, os.O_RDONLY | getattr(os, "O_DIRECTORY", 0) | getattr(os, "O_NOFOLLOW", 0))
    try: os.fsync(descriptor)
    finally: os.close(descriptor)

def secure_dir(path, mode, create=False):
    if create and not path.exists() and not path.is_symlink():
        os.mkdir(path, mode); fsync_dir(path.parent)
    details = path.lstat()
    if (path.is_symlink() or not stat.S_ISDIR(details.st_mode) or details.st_uid != 0
            or details.st_gid != 0 or stat.S_IMODE(details.st_mode) != mode):
        raise SystemExit(f"unsafe quarantine-retirement directory: {path}")

def secure(path, mode):
    details = path.lstat()
    if (path.is_symlink() or not stat.S_ISREG(details.st_mode) or details.st_uid != 0
            or details.st_gid != 0 or details.st_nlink != 1
            or stat.S_IMODE(details.st_mode) != mode):
        raise SystemExit(f"unsafe quarantine-retirement file: {path}")
    return path.read_bytes()

def create_exact(path, payload, mode):
    if path.exists() or path.is_symlink():
        if secure(path, mode) != payload:
            raise SystemExit(f"quarantine-retirement create-only file differs: {path}")
        return payload
    partial = path.with_name(f".{path.name}.partial")
    if partial.exists() or partial.is_symlink():
        details = partial.lstat()
        if (partial.is_symlink() or not stat.S_ISREG(details.st_mode)
                or details.st_uid != 0 or details.st_gid != 0 or details.st_nlink != 1
                or stat.S_IMODE(details.st_mode) != mode):
            raise SystemExit(f"unsafe quarantine-retirement partial: {partial}")
        if partial.read_bytes() == payload:
            os.replace(partial, path); fsync_dir(path.parent); return payload
        partial.unlink(); fsync_dir(path.parent)
    descriptor = os.open(
        partial,
        os.O_WRONLY | os.O_CREAT | os.O_EXCL | getattr(os, "O_NOFOLLOW", 0),
        mode,
    )
    with os.fdopen(descriptor, "wb") as handle:
        handle.write(payload); handle.flush(); os.fsync(handle.fileno())
    os.replace(partial, path); fsync_dir(path.parent)
    return payload

def load_json(path, mode, schema):
    raw = secure(path, mode); value = json.loads(raw)
    if raw != canonical(value) or value.get("schema") != schema:
        raise SystemExit(f"quarantine-retirement evidence differs: {path.name}")
    return value, raw

def stable_publish(path, schema, fixed):
    if path.exists() or path.is_symlink():
        value, raw = load_json(path, 0o400, schema)
        if any(value.get(key) != wanted for key, wanted in fixed.items()):
            raise SystemExit(f"quarantine-retirement phase differs: {path.name}")
        return value, raw
    value = {"schema": schema, **fixed, "recorded_at": now()}
    raw = canonical(value); create_exact(path, raw, 0o400)
    return value, raw

def ensure_parent(path, mode):
    if path.exists() or path.is_symlink(): secure_dir(path, mode)
    else: secure_dir(path, mode, create=True)

def systemctl(*args, check=True):
    return subprocess.run(["/usr/bin/systemctl", *args], check=check,
                          stdout=subprocess.PIPE, stderr=subprocess.PIPE, text=True)

def service_active(name):
    return systemctl("is-active", name, check=False).stdout.strip() == "active"

def service_enabled(name):
    return systemctl("is-enabled", name, check=False).stdout.strip() in {
        "enabled", "enabled-runtime", "linked", "linked-runtime", "alias"
    }

def unlink_exact(path, expected_sha, mode):
    if not path.exists() and not path.is_symlink():
        return
    raw = secure(path, mode)
    if sha(raw) != expected_sha:
        raise SystemExit(f"quarantine-retirement dependency changed: {path}")
    parent = os.open(path.parent, os.O_RDONLY | getattr(os, "O_DIRECTORY", 0) | getattr(os, "O_NOFOLLOW", 0))
    try:
        opened = os.open(path.name, os.O_RDONLY | getattr(os, "O_NOFOLLOW", 0), dir_fd=parent)
        try:
            details = os.fstat(opened)
            if (details.st_uid, details.st_gid, details.st_nlink,
                    stat.S_IMODE(details.st_mode)) != (0, 0, 1, mode):
                raise SystemExit(f"quarantine-retirement dependency inode changed: {path}")
            digest = hashlib.sha256()
            while True:
                chunk = os.read(opened, 1024 * 1024)
                if not chunk: break
                digest.update(chunk)
            if digest.hexdigest() != expected_sha:
                raise SystemExit(f"quarantine-retirement dependency content changed: {path}")
        finally: os.close(opened)
        os.unlink(path.name, dir_fd=parent); os.fsync(parent)
    finally: os.close(parent)

def normalize_nonowned(value):
    def scrub(item):
        if isinstance(item, dict):
            return {key: scrub(val) for key, val in sorted(item.items())
                    if key not in {"handle", "position", "index", "packets", "bytes"}}
        if isinstance(item, list): return [scrub(row) for row in item]
        return item
    result = []
    for entry in value.get("nftables", []):
        if "metainfo" in entry: continue
        obj = next(iter(entry.values())) if isinstance(entry, dict) and len(entry) == 1 else None
        if isinstance(obj, dict) and (
                (entry.get("table", {}).get("family"), entry.get("table", {}).get("name")) == ("inet", table)
                or (obj.get("family"), obj.get("table")) == ("inet", table)):
            continue
        result.append(scrub(entry))
    return result

for value in (capture, freeze, rollout, archive, boundary, bundle):
    if hash_re.fullmatch(value) is None: raise SystemExit("quarantine-retirement hash is malformed")
if node not in {"nyc", "lax", "ams", "lhr", "nrt", "sgp"}:
    raise SystemExit("quarantine-retirement node is unsupported")
if mode not in {"execute", "status"}:
    raise SystemExit("quarantine-retirement mode is unsupported")
secure_dir(state, 0o700)
retirements = state / "retirements"; ensure_parent(retirements, 0o700)
rollout_dir = retirements / rollout; ensure_parent(rollout_dir, 0o700)
ensure_parent(journal, 0o700)
base, base_raw = load_json(stop / "08-network-quarantine.json", 0o400,
                           "arc.recovery.legacy-network-quarantine.v1")
monitor, monitor_raw = load_json(stop / "08-network-quarantine-monitor.json", 0o400,
                                 "arc.recovery.legacy-network-quarantine-monitor.v1")
arm, arm_raw = load_json(stop / "09-quarantine-restart-arm.json", 0o400,
                         "arc.recovery.quarantine-live-restart-arm.v1")
commit, commit_raw = load_json(stop / "09-quarantine-restart-committed.json", 0o400,
                              "arc.recovery.quarantine-live-restart-committed.v1")
for value in (base, monitor, arm):
    if (value.get("capture_id"), value.get("node"), value.get("freeze_plan_sha256")) != (capture, node, freeze):
        raise SystemExit("quarantine-retirement source identity differs")
if (monitor.get("network_quarantine_receipt_sha256") != sha(base_raw)
        or arm.get("network_quarantine_receipt_sha256") != sha(base_raw)
        or arm.get("network_quarantine_monitor_sha256") != sha(monitor_raw)
        or commit.get("quarantine_restart_arm_sha256") != sha(arm_raw)
        or commit.get("restart_prevented") is not True
        or commit.get("allow_marker_absent") is not True):
    raise SystemExit("quarantine-retirement source receipt chain differs")

# Status is deliberately read-only.  It accepts only a previously committed
# terminal receipt and freshly re-proves the exact live postconditions without
# daemon-reload, stop/disable, unlink, nft mutation, or journal publication.
if mode == "status":
    receipt_path = journal / "RECEIPT.json"
    receipt, receipt_raw = load_json(
        receipt_path, 0o400, "arc.recovery.legacy-network-quarantine-retirement.v1"
    )
    expected_keys = {
        "schema", "capture_id", "node", "freeze_plan_sha256",
        "rollout_manifest_sha256", "archive_manifest_sha256",
        "legacy_maintenance_boundary_sha256",
        "legacy_maintenance_evidence_bundle_sha256",
        "network_quarantine_receipt_sha256",
        "network_quarantine_monitor_sha256",
        "quarantine_restart_arm_sha256",
        "quarantine_restart_commit_sha256", "intent_sha256",
        "preexisting_firewall_structural_sha256",
        "owned_ruleset_stateless_sha256", "pinned_nft_sha256",
        "legacy_start_barriers_sha256", "quarantine_arm_barriers_sha256",
        "nginx_retirement_barrier_sha256", "phases", "table_absent",
        "fence_service_active", "fence_service_enabled",
        "fence_dependencies_removed", "legacy_start_barrier_active",
        "nginx_retired", "automatic_legacy_restart", "rollback_policy",
        "completed_at",
    }
    if set(receipt) != expected_keys:
        raise SystemExit("quarantine-retirement receipt fields differ")
    identity = {
        "capture_id": capture, "node": node, "freeze_plan_sha256": freeze,
        "rollout_manifest_sha256": rollout,
        "archive_manifest_sha256": archive,
        "legacy_maintenance_boundary_sha256": boundary,
        "legacy_maintenance_evidence_bundle_sha256": bundle,
        "network_quarantine_receipt_sha256": sha(base_raw),
        "network_quarantine_monitor_sha256": sha(monitor_raw),
        "quarantine_restart_arm_sha256": sha(arm_raw),
        "quarantine_restart_commit_sha256": sha(commit_raw),
        "table_absent": True, "fence_service_active": False,
        "fence_service_enabled": False, "fence_dependencies_removed": True,
        "legacy_start_barrier_active": True, "nginx_retired": True,
        "automatic_legacy_restart": False,
        "rollback_policy": "maintenance-only-no-legacy-restart",
    }
    if any(receipt.get(key) != wanted for key, wanted in identity.items()):
        raise SystemExit("quarantine-retirement receipt identity/policy differs")

    intent, intent_raw = load_json(
        journal / "INTENT.json", 0o400,
        "arc.recovery.legacy-network-quarantine-retirement-intent.v1",
    )
    if receipt.get("intent_sha256") != sha(intent_raw):
        raise SystemExit("quarantine-retirement intent root differs")
    expected_phase_names = (
        "legacy-public-retired", "fence-service-retired",
        "fence-dependencies-removed", "owned-table-removed",
    )
    phase_files = (
        "PHASE-01-LEGACY-PUBLIC-RETIRED.json",
        "PHASE-02-FENCE-SERVICE-RETIRED.json",
        "PHASE-03-FENCE-DEPENDENCIES-REMOVED.json",
        "PHASE-04-OWNED-TABLE-REMOVED.json",
    )
    phase_rows = []
    for phase_name, file_name in zip(expected_phase_names, phase_files):
        phase, phase_raw = load_json(
            journal / file_name, 0o400,
            "arc.recovery.legacy-network-quarantine-retirement-phase.v1",
        )
        if (phase.get("intent_sha256") != sha(intent_raw)
                or phase.get("phase") != phase_name
                or phase.get("complete") is not True):
            raise SystemExit(f"quarantine-retirement phase differs: {file_name}")
        phase_rows.append({"phase": phase_name, "receipt_sha256": sha(phase_raw)})
    if receipt.get("phases") != phase_rows:
        raise SystemExit("quarantine-retirement phase ledger differs")

    legacy_barriers = arm.get("persistent_start_barrier_sha256")
    arm_barriers = arm.get("quarantine_arm_start_barrier_sha256")
    if (receipt.get("legacy_start_barriers_sha256") != legacy_barriers
            or receipt.get("quarantine_arm_barriers_sha256") != arm_barriers
            or not isinstance(legacy_barriers, dict) or set(legacy_barriers) != set(units)
            or not isinstance(arm_barriers, dict) or set(arm_barriers) != set(units)):
        raise SystemExit("quarantine-retirement retained barrier roots differ")
    for legacy in units:
        for name, wanted in (
            ("zzzz-arc-recovery-freeze.conf", legacy_barriers[legacy]),
            ("zzzx-arc-recovery-quarantine-arm.conf", arm_barriers[legacy]),
        ):
            if sha(secure(pathlib.Path(
                    f"/etc/systemd/system/{legacy}.d/{name}"), 0o444)) != wanted:
                raise SystemExit(f"retained legacy start barrier changed: {legacy}/{name}")
        dependency = pathlib.Path(
            f"/etc/systemd/system/{legacy}.d/zzzy-arc-recovery-network-fence.conf"
        )
        if dependency.exists() or dependency.is_symlink():
            raise SystemExit(f"network-quarantine dependency survived retirement: {legacy}")
    marker = pathlib.Path("/etc/arc-recovery/legacy-start-allowed")
    if marker.exists() or marker.is_symlink():
        raise SystemExit("legacy start marker reappeared after quarantine retirement")

    nginx_barrier_raw = b"[Unit]\nConditionPathExists=/etc/arc-recovery/legacy-start-allowed\n"
    nginx_barrier = pathlib.Path(
        "/etc/systemd/system/nginx.service.d/zzzz-arc-recovery-retired.conf"
    )
    if (sha(secure(nginx_barrier, 0o444))
            != receipt.get("nginx_retirement_barrier_sha256")
            or receipt.get("nginx_retirement_barrier_sha256") != sha(nginx_barrier_raw)):
        raise SystemExit("legacy nginx retirement barrier differs")
    retirement_dropin = unit.parent / f"{unit.name}.d/zzzz-arc-recovery-retired.conf"
    retirement_dropin_raw = b"[Unit]\nRefuseManualStop=no\n\n[Service]\nRestart=no\n"
    if secure(retirement_dropin, 0o444) != retirement_dropin_raw:
        raise SystemExit("network-quarantine retirement drop-in differs")
    if (service_active(unit.name) or service_enabled(unit.name)
            or service_active("nginx.service") or service_enabled("nginx.service")):
        raise SystemExit("retired public/quarantine service reactivated")

    pinned_nft = state / "nft"
    pinned_raw = secure(pinned_nft, 0o500)
    pinned_sha = base.get("file_sha256", {}).get("nft")
    if (sha(pinned_raw) != pinned_sha
            or pinned_sha != base.get("tool_sha256", {}).get("/usr/sbin/nft")
            or receipt.get("pinned_nft_sha256") != pinned_sha):
        raise SystemExit("quarantine-retirement pinned nft differs")
    table_present = subprocess.run(
        [str(pinned_nft), "list", "table", "inet", table],
        stdout=subprocess.DEVNULL, stderr=subprocess.DEVNULL,
    ).returncode == 0
    if table_present:
        raise SystemExit("owned network-quarantine table reappeared")
    live_ruleset = json.loads(subprocess.check_output(
        [str(pinned_nft), "--json", "list", "ruleset"]
    ))
    baseline = json.loads(secure(state / "preexisting-ruleset.json", 0o400))
    baseline_sha = sha(canonical(normalize_nonowned(baseline)))
    if (sha(canonical(normalize_nonowned(live_ruleset))) != baseline_sha
            or receipt.get("preexisting_firewall_structural_sha256") != baseline_sha
            or baseline_sha != base.get("preexisting_firewall_structural_sha256")):
        raise SystemExit("nonowned firewall changed after quarantine retirement")
    sys.stdout.buffer.write(receipt_raw)
    raise SystemExit(0)

intent_fixed = {
    "capture_id": capture, "node": node, "freeze_plan_sha256": freeze,
    "rollout_manifest_sha256": rollout, "archive_manifest_sha256": archive,
    "legacy_maintenance_boundary_sha256": boundary,
    "legacy_maintenance_evidence_bundle_sha256": bundle,
    "network_quarantine_receipt_sha256": sha(base_raw),
    "network_quarantine_monitor_sha256": sha(monitor_raw),
    "quarantine_restart_arm_sha256": sha(arm_raw),
    "quarantine_restart_commit_sha256": sha(commit_raw),
    "transition": "remove-only-capture-owned-network-quarantine",
    "legacy_restart_allowed": False,
    "rollback_policy": "maintenance-only-no-legacy-restart",
}
intent, intent_raw = stable_publish(
    journal / "INTENT.json", "arc.recovery.legacy-network-quarantine-retirement-intent.v1", intent_fixed,
)
intent_sha = sha(intent_raw)

# Phase 1: permanently retire the old public nginx path before opening any
# network traffic.  The retained absent-marker condition prevents a reboot or
# manual start from restoring it.
nginx_dir = pathlib.Path("/etc/systemd/system/nginx.service.d")
ensure_parent(nginx_dir, 0o755)
nginx_barrier = nginx_dir / "zzzz-arc-recovery-retired.conf"
nginx_barrier_raw = b"[Unit]\nConditionPathExists=/etc/arc-recovery/legacy-start-allowed\n"
create_exact(nginx_barrier, nginx_barrier_raw, 0o444)
systemctl("daemon-reload")
systemctl("stop", "nginx.service", check=False); systemctl("disable", "nginx.service", check=False)
if service_active("nginx.service") or service_enabled("nginx.service"):
    raise SystemExit("legacy nginx remained active/enabled during quarantine retirement")
phase1, phase1_raw = stable_publish(
    journal / "PHASE-01-LEGACY-PUBLIC-RETIRED.json",
    "arc.recovery.legacy-network-quarantine-retirement-phase.v1",
    {"intent_sha256": intent_sha, "phase": "legacy-public-retired",
     "nginx_retirement_barrier_sha256": sha(nginx_barrier_raw), "complete": True},
)

# Phase 2: make the monitor stoppable without altering its sealed base unit,
# then stop+disable it while its nft table is still the active safety barrier.
unit_dir = unit.parent / f"{unit.name}.d"; ensure_parent(unit_dir, 0o755)
retirement_dropin = unit_dir / "zzzz-arc-recovery-retired.conf"
retirement_dropin_raw = b"[Unit]\nRefuseManualStop=no\n\n[Service]\nRestart=no\n"
create_exact(retirement_dropin, retirement_dropin_raw, 0o444)
systemctl("daemon-reload")
systemctl("stop", unit.name, check=False); systemctl("disable", unit.name, check=False)
if service_active(unit.name) or service_enabled(unit.name):
    raise SystemExit("network-quarantine monitor remained active/enabled during retirement")
phase2, phase2_raw = stable_publish(
    journal / "PHASE-02-FENCE-SERVICE-RETIRED.json",
    "arc.recovery.legacy-network-quarantine-retirement-phase.v1",
    {"intent_sha256": intent_sha, "phase": "fence-service-retired",
     "retirement_dropin_sha256": sha(retirement_dropin_raw), "complete": True},
)

# Phase 3: remove only the exact capture-owned dependency drop-ins.  Both
# independent legacy start-barrier sets are retained and re-hashed below.
monitor_files = monitor.get("file_sha256")
if not isinstance(monitor_files, dict): raise SystemExit("quarantine monitor file inventory is malformed")
for legacy in units:
    dependency = pathlib.Path(f"/etc/systemd/system/{legacy}.d/zzzy-arc-recovery-network-fence.conf")
    wanted = monitor_files.get(str(dependency))
    if not isinstance(wanted, str) or hash_re.fullmatch(wanted) is None:
        raise SystemExit(f"quarantine dependency receipt is missing: {legacy}")
    unlink_exact(dependency, wanted, 0o400)
systemctl("daemon-reload")
phase3, phase3_raw = stable_publish(
    journal / "PHASE-03-FENCE-DEPENDENCIES-REMOVED.json",
    "arc.recovery.legacy-network-quarantine-retirement-phase.v1",
    {"intent_sha256": intent_sha, "phase": "fence-dependencies-removed",
     "dependencies": list(units), "complete": True},
)

legacy_barriers = arm.get("persistent_start_barrier_sha256")
arm_barriers = arm.get("quarantine_arm_start_barrier_sha256")
if (not isinstance(legacy_barriers, dict) or set(legacy_barriers) != set(units)
        or not isinstance(arm_barriers, dict) or set(arm_barriers) != set(units)):
    raise SystemExit("quarantine-retirement start-barrier inventory differs")
for legacy in units:
    for name, wanted, mode in (
        ("zzzz-arc-recovery-freeze.conf", legacy_barriers[legacy], 0o444),
        ("zzzx-arc-recovery-quarantine-arm.conf", arm_barriers[legacy], 0o444),
    ):
        path = pathlib.Path(f"/etc/systemd/system/{legacy}.d/{name}")
        if sha(secure(path, mode)) != wanted:
            raise SystemExit(f"retained legacy start barrier changed: {legacy}/{name}")
if pathlib.Path("/etc/arc-recovery/legacy-start-allowed").exists() or pathlib.Path(
        "/etc/arc-recovery/legacy-start-allowed").is_symlink():
    raise SystemExit("legacy start marker reappeared during quarantine retirement")

# Phase 4: exact-match the owned table and the nonowned baseline, then delete
# only that table with the sealed private nft copy.  A crash after deletion is
# reconciled by the preceding durable phases and the retained start barriers.
pinned_nft = state / "nft"
pinned_raw = secure(pinned_nft, 0o500)
pinned_sha = base.get("file_sha256", {}).get("nft")
if sha(pinned_raw) != pinned_sha or pinned_sha != base.get("tool_sha256", {}).get("/usr/sbin/nft"):
    raise SystemExit("quarantine-retirement pinned nft differs")

def table_exists():
    return subprocess.run([str(pinned_nft), "list", "table", "inet", table],
                          stdout=subprocess.DEVNULL, stderr=subprocess.DEVNULL).returncode == 0

def ruleset():
    return json.loads(subprocess.check_output([str(pinned_nft), "--json", "list", "ruleset"]))

baseline = json.loads(secure(state / "preexisting-ruleset.json", 0o400))
baseline_sha = sha(canonical(normalize_nonowned(baseline)))
if baseline_sha != base.get("preexisting_firewall_structural_sha256"):
    raise SystemExit("quarantine-retirement nonowned baseline root differs")
if table_exists():
    stateless = subprocess.check_output(
        [str(pinned_nft), "--stateless", "list", "table", "inet", table]
    )
    if sha(stateless) != base.get("owned_ruleset_stateless_sha256"):
        raise SystemExit("quarantine-retirement owned nft table differs")
    if sha(canonical(normalize_nonowned(ruleset()))) != baseline_sha:
        raise SystemExit("nonowned firewall changed before quarantine retirement")
    subprocess.run([str(pinned_nft), "delete", "table", "inet", table], check=True)
if table_exists(): raise SystemExit("owned network-quarantine table survived retirement")
if sha(canonical(normalize_nonowned(ruleset()))) != baseline_sha:
    raise SystemExit("nonowned firewall changed after quarantine retirement")
phase4, phase4_raw = stable_publish(
    journal / "PHASE-04-OWNED-TABLE-REMOVED.json",
    "arc.recovery.legacy-network-quarantine-retirement-phase.v1",
    {"intent_sha256": intent_sha, "phase": "owned-table-removed",
     "preexisting_firewall_structural_sha256": baseline_sha, "complete": True},
)

if service_active(unit.name) or service_enabled(unit.name):
    raise SystemExit("network-quarantine service reactivated after retirement")
for legacy in units:
    dependency = pathlib.Path(f"/etc/systemd/system/{legacy}.d/zzzy-arc-recovery-network-fence.conf")
    if dependency.exists() or dependency.is_symlink():
        raise SystemExit(f"network-quarantine dependency survived retirement: {legacy}")

phase_rows = [
    {"phase": name, "receipt_sha256": sha(raw)}
    for name, raw in (
        ("legacy-public-retired", phase1_raw),
        ("fence-service-retired", phase2_raw),
        ("fence-dependencies-removed", phase3_raw),
        ("owned-table-removed", phase4_raw),
    )
]
fixed = {
    "capture_id": capture, "node": node, "freeze_plan_sha256": freeze,
    "rollout_manifest_sha256": rollout, "archive_manifest_sha256": archive,
    "legacy_maintenance_boundary_sha256": boundary,
    "legacy_maintenance_evidence_bundle_sha256": bundle,
    "network_quarantine_receipt_sha256": sha(base_raw),
    "network_quarantine_monitor_sha256": sha(monitor_raw),
    "quarantine_restart_arm_sha256": sha(arm_raw),
    "quarantine_restart_commit_sha256": sha(commit_raw),
    "intent_sha256": intent_sha,
    "preexisting_firewall_structural_sha256": baseline_sha,
    "owned_ruleset_stateless_sha256": base["owned_ruleset_stateless_sha256"],
    "pinned_nft_sha256": pinned_sha,
    "legacy_start_barriers_sha256": legacy_barriers,
    "quarantine_arm_barriers_sha256": arm_barriers,
    "nginx_retirement_barrier_sha256": sha(nginx_barrier_raw),
    "phases": phase_rows,
    "table_absent": True, "fence_service_active": False,
    "fence_service_enabled": False, "fence_dependencies_removed": True,
    "legacy_start_barrier_active": True, "nginx_retired": True,
    "automatic_legacy_restart": False,
    "rollback_policy": "maintenance-only-no-legacy-restart",
}
receipt_path = journal / "RECEIPT.json"
if receipt_path.exists() or receipt_path.is_symlink():
    receipt, receipt_raw = load_json(
        receipt_path, 0o400, "arc.recovery.legacy-network-quarantine-retirement.v1"
    )
    if any(receipt.get(key) != wanted for key, wanted in fixed.items()):
        raise SystemExit("existing quarantine-retirement receipt differs")
else:
    receipt = {"schema": "arc.recovery.legacy-network-quarantine-retirement.v1",
               **fixed, "completed_at": now()}
    receipt_raw = canonical(receipt); create_exact(receipt_path, receipt_raw, 0o400)
sys.stdout.buffer.write(receipt_raw)
PY
}

quarantine_retire_status() {
    [ "$#" -eq 7 ] || die "quarantine-retire-status requires the exact capture/rollout/archive boundary roots"
    ARC_RECOVERY_RETIRE_MODE=status quarantine_retire "$@"
}

ACTION="${1:-}"
case "$ACTION" in
    stage-recovery-barrier)
        [ "$#" -eq 2 ] || { usage >&2; exit 2; }
        stage_recovery_barrier "$2"
        ;;
    capture-live-observations)
        [ "$#" -eq 11 ] || { usage >&2; exit 2; }
        capture_live_observations "$2" "$3" "$4" "$5" "$6" "$7" "$8" "$9" "${10}" "${11}"
        ;;
    live-observations-status)
        [ "$#" -eq 4 ] || { usage >&2; exit 2; }
        live_observations_status "$2" "$3" "$4"
        ;;
    live-observations-eligible)
        [ "$#" -eq 7 ] || { usage >&2; exit 2; }
        live_observations_eligible "$2" "$3" "$4" "$5" "$6" "$7"
        ;;
    legacy-height-bracket)
        [ "$#" -eq 16 ] || { usage >&2; exit 2; }
        legacy_height_bracket "${@:2}"
        ;;
    fence-stop)
        [ "$#" -eq 22 ] || { usage >&2; exit 2; }
        fence_stop "$2" "$3" "$4" "$5" "$6" "$7" "$8" "$9" \
            "${10}" "${11}" "${12}" "${13}" "${14}" "${15}" "${16}" \
            "${17}" "${18}" "${19}" "${20}" "${21}" "${22}"
        ;;
    quarantine)
        [ "$#" -eq 22 ] || { usage >&2; exit 2; }
        ARC_RECOVERY_QUARANTINE_ONLY=true fence_stop \
            "$2" "$3" "$4" "$5" "$6" "$7" "$8" "$9" \
            "${10}" "${11}" "${12}" "${13}" "${14}" "${15}" "${16}" \
            "${17}" "${18}" "${19}" "${20}" "${21}" "${22}"
        ;;
    quarantine-status)
        [ "$#" -eq 4 ] || { usage >&2; exit 2; }
        quarantine_status "$2" "$3" "$4"
        ;;
    quarantine-restart-arm)
        [ "$#" -eq 4 ] || { usage >&2; exit 2; }
        quarantine_restart_arm "$2" "$3" "$4"
        ;;
    quarantine-restart-status)
        [ "$#" -eq 4 ] || { usage >&2; exit 2; }
        quarantine_restart_status "$2" "$3" "$4"
        ;;
    quarantine-monitor-receipt)
        [ "$#" -eq 4 ] || { usage >&2; exit 2; }
        quarantine_monitor_receipt "$2" "$3" "$4"
        ;;
    quarantine-retire)
        [ "$#" -eq 8 ] || { usage >&2; exit 2; }
        quarantine_retire "$2" "$3" "$4" "$5" "$6" "$7" "$8"
        ;;
    quarantine-retire-status)
        [ "$#" -eq 8 ] || { usage >&2; exit 2; }
        quarantine_retire_status "$2" "$3" "$4" "$5" "$6" "$7" "$8"
        ;;
    quarantine-public-cross-proof)
        [ "$#" -eq 8 ] || { usage >&2; exit 2; }
        quarantine_public_cross_proof "$2" "$3" "$4" "$5" "$6" "$7" "$8"
        ;;
    quarantine-stability-sample)
        [ "$#" -eq 6 ] || { usage >&2; exit 2; }
        quarantine_stability_sample "$2" "$3" "$4" "$5" "$6"
        ;;
    persisted-head)
        [ "$#" -eq 9 ] || { usage >&2; exit 2; }
        persisted_head "$2" "$3" "$4" "$5" "$6" "$7" "$8" "$9"
        ;;
    stopped-status)
        { [ "$#" -eq 3 ] || [ "$#" -eq 22 ]; } || { usage >&2; exit 2; }
        stopped_status "$2" "$3" "${@:4}"
        ;;
    stopped-status-challenged)
        [ "$#" -eq 24 ] || { usage >&2; exit 2; }
        stopped_status_challenged "${@:2}"
        ;;
    capture-offline)
        [ "$#" -eq 3 ] || { usage >&2; exit 2; }
        capture_offline "$2" "$3"
        ;;
    status)
        [ "$#" -eq 3 ] || { usage >&2; exit 2; }
        capture_status "$2" "$3"
        ;;
    sealed-source-status)
        [ "$#" -eq 22 ] || { usage >&2; exit 2; }
        sealed_source_status "$2" "$3" "${@:4}"
        ;;
    stage-input)
        [ "$#" -eq 5 ] || { usage >&2; exit 2; }
        stage_input "$2" "$3" "$4" "$5"
        ;;
    validator-key-identity)
        [ "$#" -eq 6 ] || { usage >&2; exit 2; }
        validator_key_identity "$2" "$3" "$4" "$5" "$6"
        ;;
    validator-key-identity-transient)
        [ "$#" -eq 6 ] || { usage >&2; exit 2; }
        validator_key_identity_transient "$2" "$3" "$4" "$5" "$6"
        ;;
    bind)
        [ "$#" -eq 21 ] || { usage >&2; exit 2; }
        bind_capture "$2" "$3" "$4" "$5" "$6" "$7" "$8" "$9" \
            "${10}" "${11}" "${12}" "${13}" "${14}" "${15}" "${16}" \
            "${17}" "${18}" "${19}" "${20}" "${21}"
        ;;
    stream-bundle)
        [ "$#" -eq 4 ] || { usage >&2; exit 2; }
        stream_bundle "$2" "$3" "$4"
        ;;
    stream-inventory)
        [ "$#" -eq 4 ] || { usage >&2; exit 2; }
        stream_inventory "$2" "$3" "$4"
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
