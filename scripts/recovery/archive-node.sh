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
  archive-node.sh fence-stop CAPTURE_SHA256 NODE FREEZE_SHA256 VALIDATOR_ADDRESS STAKE \
    WRITER_PID WRITER_START_TICKS BOOT_ID WRITER_CGROUP_SHA256 WRITER_SUPERVISION_MODE \
    SUPERVISOR_UNIT SUPERVISOR_MAIN_PID \
    SUPERVISOR_START_TICKS SUPERVISOR_EXECUTABLE_PATH SUPERVISOR_EXECUTABLE_SHA256 \
    SUPERVISOR_ARGV_SHA256 SUPERVISOR_CONTEXT_SHA256 \
    EXECUTABLE_PATH EXECUTABLE_SHA256 ARGV_SHA256 DATA_DIR
  archive-node.sh stopped-status CAPTURE_SHA256 NODE [FREEZE_SHA256 VALIDATOR_ADDRESS STAKE \
    WRITER_PID WRITER_START_TICKS BOOT_ID WRITER_CGROUP_SHA256 WRITER_SUPERVISION_MODE \
    SUPERVISOR_UNIT SUPERVISOR_MAIN_PID \
    SUPERVISOR_START_TICKS SUPERVISOR_EXECUTABLE_PATH SUPERVISOR_EXECUTABLE_SHA256 \
    SUPERVISOR_ARGV_SHA256 SUPERVISOR_CONTEXT_SHA256 \
    EXECUTABLE_PATH EXECUTABLE_SHA256 ARGV_SHA256 DATA_DIR]
  archive-node.sh capture-offline CAPTURE_SHA256 NODE
  archive-node.sh status CAPTURE_SHA256 NODE
  archive-node.sh sealed-source-status CAPTURE_SHA256 NODE FREEZE_SHA256 \
    VALIDATOR_ADDRESS STAKE WRITER_PID WRITER_START_TICKS BOOT_ID WRITER_CGROUP_SHA256 \
    WRITER_SUPERVISION_MODE SUPERVISOR_UNIT \
    SUPERVISOR_MAIN_PID SUPERVISOR_START_TICKS SUPERVISOR_EXECUTABLE_PATH \
    SUPERVISOR_EXECUTABLE_SHA256 SUPERVISOR_ARGV_SHA256 SUPERVISOR_CONTEXT_SHA256 EXECUTABLE_PATH \
    EXECUTABLE_SHA256 ARGV_SHA256 DATA_DIR
  archive-node.sh stage-input MANIFEST_SHA256 NODE ROLE EXPECTED_SHA256 < FILE
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

    # Marker absence plus a durable arm is the commit point even when a crash
    # interrupted publication of the commit receipt. Re-fsync the parent and
    # publish the inferred receipt without touching a stale PID or cgroup.
    if [ ! -e "$marker" ] && [ ! -L "$marker" ]; then
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
schemas = {
    "02-fast-cgroup-freeze-intent.json": "arc.recovery.fast-cgroup-freeze-intent.v1",
    "03-fast-cgroups-frozen.json": "arc.recovery.fast-cgroups-frozen.v1",
    "04-pre-fence-quiesce-intent.json": "arc.recovery.pre-fence-quiesce-intent.v1",
    "05-cgroups-frozen.json": "arc.recovery.cgroups-frozen.v1",
    "stop.intent.json": "arc.recovery.stop-intent.v1",
}
values = {}; sources = {}
for name, schema in schemas.items():
    values[name], raw = load(root / name, schema); sources[name] = hashlib.sha256(raw).hexdigest()
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
marker_details = marker.lstat()
if (marker.is_symlink() or not stat.S_ISREG(marker_details.st_mode) or marker_details.st_uid != 0
        or marker_details.st_gid != 0 or stat.S_IMODE(marker_details.st_mode) != 0o400
        or marker.read_bytes() != marker_payload): raise SystemExit("allow marker differs before arm")
sources = {}
schemas = {
    "02-fast-cgroup-freeze-intent.json": "arc.recovery.fast-cgroup-freeze-intent.v1",
    "03-fast-cgroups-frozen.json": "arc.recovery.fast-cgroups-frozen.v1",
    "04-pre-fence-quiesce-intent.json": "arc.recovery.pre-fence-quiesce-intent.v1",
    "05-cgroups-frozen.json": "arc.recovery.cgroups-frozen.v1",
    "06-pre-mask-activation-gate.json": "arc.recovery.pre-mask-activation-gate.v1",
    "stop.intent.json": "arc.recovery.stop-intent.v1",
}
values = {}
for name, schema in schemas.items():
    values[name], raw = load(root / name, schema); sources[name] = hashlib.sha256(raw).hexdigest()
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
    "allow_marker_identity": {
        "device": marker_details.st_dev, "inode": marker_details.st_ino, "uid": marker_details.st_uid,
        "gid": marker_details.st_gid, "mode": stat.S_IMODE(marker_details.st_mode), "size": marker_details.st_size,
    },
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
    "unlink_parent_fsynced": True, "durability_basis": "same-boot-unlink-parent-fsynced",
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
    require_commands pgrep systemctl sync python3 find sort tail
    commit_restart_barrier "$root" "$supervisor_unit" "$supervisor_pid" "$boot_id"
    record_committed_restart_barrier "$root" "$supervisor_unit" "$supervisor_pid"
    arm_stop_journal "$root"
    python3 - "$root" "$writer_pid" "$writer_start_ticks" "$boot_id" \
        "$writer_cgroup_sha" "$writer_executable_path" "$writer_executable_sha" \
        "$writer_argv_sha" "$supervisor_pid" "$supervisor_start_ticks" \
        "$supervisor_unit" "$supervisor_executable_path" \
        "$supervisor_executable_sha" "$supervisor_argv_sha" <<'PY'
import hashlib
import json
import os
import pathlib
import re
import select
import signal
import subprocess
import sys
import time

(root_raw, writer_pid_raw, writer_start_raw, boot_id, writer_cgroup_sha,
 writer_executable_path, writer_executable_sha, writer_argv_sha,
 supervisor_pid_raw, supervisor_start_raw, supervisor_unit,
 supervisor_executable_path, supervisor_executable_sha,
 supervisor_argv_sha) = sys.argv[1:]
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

intent, intent_raw = load("stop.intent.json", "arc.recovery.stop-intent.v1")
if (set(intent) != {
        "schema", "capture_id", "node", "freeze_plan_sha256", "writer_contract_sha256",
        "pre_fence_intent_sha256", "frozen_context_sha256", "intent_at",
    } or intent["capture_id"] != capture_id or intent["node"] != node
        or intent["freeze_plan_sha256"] != freeze_sha
        or intent["writer_contract_sha256"] != sha(contract_raw)
        or intent["pre_fence_intent_sha256"] != sha(prefence_raw)
        or intent["frozen_context_sha256"] != sha(frozen_raw)
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
    "stop.intent.json": sha(intent_raw),
    "evidence/writer-contract.json": sha(contract_raw),
    f"/root/.arc-recovery-plans/{freeze_sha}/freeze.lock.json": freeze_sha,
    **progress_sources,
}
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
    r"07-restart-barrier-committed|10-fence-verified|"
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
    grep -Fxq "capture_id=$capture_id" "$root/stop.complete" || \
        die "stop evidence id differs from its immutable path"
    grep -Fxq "node=$node" "$root/stop.complete" || \
        die "stop evidence node differs from its immutable path"
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
        "$supervisor_executable_path" "$supervisor_executable_sha" "$supervisor_argv_sha"
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
    local data_dir temporary inventory
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
    local data_dir executable_path
    data_dir="$(python3 -c 'import json,sys; print(json.load(open(sys.argv[1], encoding="utf-8"))["data_dir"])' \
        "$stop_root/evidence/writer-contract.json")"
    executable_path="$(python3 -c 'import json,sys; print(json.load(open(sys.argv[1], encoding="utf-8"))["executable_path"])' \
        "$stop_root/evidence/writer-contract.json")"
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
        printf 'hostname=%s\n' "$(hostname)"
        printf 'captured_offline_at=%s\n' "$(date -u +%Y-%m-%dT%H:%M:%SZ)"
        printf 'kernel=%s\n' "$(uname -srmo)"
        printf 'archive_scope=complete-content-indexed-stopped-legacy-source-v4\n'
        printf 'source_tree_content_sealed_by_index=true\n'
        printf 'source_tree_os_read_only=false\n'
        printf 'source_data_dir=%s\n' "$data_dir"
        printf 'source_index_sha256=%s\n' "$(hash_file "$temporary/source-data.files.sha256")"
        printf 'excluded_outside_data_dir_private_material=true\n'
        printf 'excluded_service_environments=true\n'
        printf 'excluded_build_models_and_git=true\n'
    } > "$temporary/capture.inventory"

    [ -s "$data_dir/state.wal" ] || die "fenced content source has no final state.wal"
    rm -f -- "$temporary/.arc-recovery-partial-owner"
    write_tree_index "$temporary" capture.files.sha256 capture.complete
    write_complete_marker "$temporary" capture.files.sha256 capture.complete arc.recovery.capture.v4 \
        "capture_id=$capture_id" "node=$node" "stopped=true" \
        "source_tree_content_sealed_by_index=true" "source_tree_os_read_only=false"
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
    verify_tree_index "$capture_root" capture.files.sha256 capture.complete
    verify_capture_identity "$capture_root" "$capture_id" "$node"
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

    tar --create --zstd --numeric-owner --acls --xattrs --sparse --one-file-system \
        --file "$archive" --directory /root \
        "arc-recovery-captures/$capture_id/$node" \
        "arc-recovery-bindings/$manifest/$node"
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
    verify_capture_source "$capture_root" "$capture_id" "$node"
    verify_tree_index "$binding_root" binding.files.sha256 binding.complete
    verify_binding_identity "$binding_root" "$capture_id" "$node" "$manifest"
    pgrep -x arc-node >/dev/null 2>&1 && die "refusing archive stream while arc-node is running"
    verify_legacy_restart_fence "$stop_root"

    local data_dir
    data_dir="$(capture_source_data_dir "$capture_root")"
    require_safe_absolute_path "$data_dir" "archive stream data directory"
    local members=(
        "${data_dir#/}"
        "${capture_root#/}"
        "${binding_root#/}"
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
    verify_capture_source "$capture_root" "$capture_id" "$node"
    verify_tree_index "$binding_root" binding.files.sha256 binding.complete
    verify_binding_identity "$binding_root" "$capture_id" "$node" "$manifest"
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

ACTION="${1:-}"
case "$ACTION" in
    stage-recovery-barrier)
        [ "$#" -eq 2 ] || { usage >&2; exit 2; }
        stage_recovery_barrier "$2"
        ;;
    fence-stop)
        [ "$#" -eq 22 ] || { usage >&2; exit 2; }
        fence_stop "$2" "$3" "$4" "$5" "$6" "$7" "$8" "$9" \
            "${10}" "${11}" "${12}" "${13}" "${14}" "${15}" "${16}" \
            "${17}" "${18}" "${19}" "${20}" "${21}" "${22}"
        ;;
    stopped-status)
        { [ "$#" -eq 3 ] || [ "$#" -eq 22 ]; } || { usage >&2; exit 2; }
        stopped_status "$2" "$3" "${@:4}"
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
