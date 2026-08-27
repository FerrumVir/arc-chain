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
  archive-node.sh fence-stop CAPTURE_SHA256 NODE FREEZE_SHA256 VALIDATOR_ADDRESS STAKE \
    WRITER_PID WRITER_START_TICKS BOOT_ID SUPERVISOR_UNIT SUPERVISOR_MAIN_PID \
    EXECUTABLE_PATH EXECUTABLE_SHA256 ARGV_SHA256 DATA_DIR
  archive-node.sh stopped-status CAPTURE_SHA256 NODE [FREEZE_SHA256 VALIDATOR_ADDRESS STAKE \
    WRITER_PID WRITER_START_TICKS BOOT_ID SUPERVISOR_UNIT SUPERVISOR_MAIN_PID \
    EXECUTABLE_PATH EXECUTABLE_SHA256 ARGV_SHA256 DATA_DIR]
  archive-node.sh capture-offline CAPTURE_SHA256 NODE
  archive-node.sh status CAPTURE_SHA256 NODE
  archive-node.sh sealed-source-status CAPTURE_SHA256 NODE FREEZE_SHA256 \
    VALIDATOR_ADDRESS STAKE WRITER_PID WRITER_START_TICKS BOOT_ID SUPERVISOR_UNIT \
    SUPERVISOR_MAIN_PID EXECUTABLE_PATH EXECUTABLE_SHA256 ARGV_SHA256 DATA_DIR
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
    chmod 400 "$root/$complete_name"
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
    [ ! -e /root/.arc-recovery-legacy-start-allowed ] || \
        die "legacy-start authorization marker exists; persistent freeze is not fail-closed"
    for service in arc-self-heal.service arc-node.service arc-node-update.service; do
        fence="/etc/systemd/system/$service.d/arc-recovery-freeze.conf"
        [ -f "$fence" ] && [ ! -L "$fence" ] || die "persistent legacy restart fence is missing: $service"
        grep -Fxq 'RefuseManualStart=yes' "$fence" || die "legacy fence does not refuse manual starts: $service"
        grep -Fxq 'ConditionPathExists=/root/.arc-recovery-legacy-start-allowed' "$fence" || \
            die "legacy fence does not refuse indirect activation: $service"
        grep -Fxq 'Restart=no' "$fence" || die "legacy fence does not disable restarts: $service"
        systemctl is-active --quiet "$service" && die "legacy service remains active: $service"
        systemctl is-enabled --quiet "$service" && die "legacy service remains enabled: $service"
    done
    for service in arc-node-update.timer arc-node-update.service; do
        systemctl is-active --quiet "$service" && die "legacy updater remains active: $service"
        systemctl is-enabled --quiet "$service" && die "legacy updater remains enabled: $service"
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
    for unit in arc-self-heal.service arc-node.service arc-node-update.service; do
        fence="/etc/systemd/system/$unit.d/arc-recovery-freeze.conf"
        mkdir -p -- "${fence%/*}"
        if [ -e "$fence" ]; then
            [ -f "$fence" ] && [ ! -L "$fence" ] || die "legacy restart fence is not a regular file: $fence"
            grep -Fxq 'RefuseManualStart=yes' "$fence" || die "existing legacy restart fence differs: $fence"
            grep -Fxq 'ConditionPathExists=/root/.arc-recovery-legacy-start-allowed' "$fence" || \
                die "existing legacy restart fence differs: $fence"
            grep -Fxq 'Restart=no' "$fence" || die "existing legacy restart fence differs: $fence"
        else
            temporary="$(mktemp "${fence}.partial.XXXXXX")"
            {
                printf '[Unit]\nRefuseManualStart=yes\n'
                printf 'ConditionPathExists=/root/.arc-recovery-legacy-start-allowed\n\n'
                printf '[Service]\nRestart=no\n'
            } > "$temporary"
            chmod 0644 -- "$temporary"
            mv -- "$temporary" "$fence"
        fi
    done
    systemctl daemon-reload
    [ ! -e /root/.arc-recovery-legacy-start-allowed ] || \
        die "legacy-start authorization marker exists; refusing freeze"
    # Never ask systemd to stop the audited writer: an unsealed ExecStop,
    # KillMode, or SendSIGKILL policy could bypass the exact TERM-only contract.
    # Disable future activation first, stop only the process-free timer, and
    # require the updater service already inactive. stop_node_cleanly() then
    # signals only the exact sealed writer PID and refuses escalation.
    systemctl disable arc-node-update.timer arc-node-update.service \
        arc-self-heal.service arc-node.service 2>/dev/null || true
    systemctl stop arc-node-update.timer 2>/dev/null || true
    systemctl is-active --quiet arc-node-update.service && \
        die "legacy updater service is active; refusing to race a writer mutation"

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
            "indirect_activation_condition_is_absent": True,
            "disabled_updater_units": ["arc-node-update.timer", "arc-node-update.service"],
    },
}
with output.open("x", encoding="utf-8", newline="\n") as handle:
    json.dump(value, handle, sort_keys=True, separators=(",", ":"))
    handle.write("\n")
PY
}

verify_exact_writer() {
    local evidence_root="$1" freeze_sha="$2" validator="$3" stake="$4" writer_pid="$5"
    local start_ticks="$6" boot_id="$7" unit="$8" unit_main_pid="$9"
    local executable_path="${10}" executable_sha="${11}" argv_sha="${12}" data_dir="${13}"
    python3 - "$evidence_root/writer-contract.json" "$freeze_sha" "$validator" "$stake" \
        "$writer_pid" "$start_ticks" "$boot_id" "$unit" "$unit_main_pid" \
        "$executable_path" "$executable_sha" "$argv_sha" "$data_dir" <<'PY'
import hashlib
import json
import os
import pathlib
import re
import subprocess
import sys
import urllib.request

(output_raw, freeze_sha, validator, stake_raw, pid_raw, start_raw, boot_id,
 unit, main_raw, executable_path, executable_sha, argv_sha, data_dir_raw) = sys.argv[1:]
output = pathlib.Path(output_raw)
pid, stake, start_ticks, unit_main_pid = map(int, (pid_raw, stake_raw, start_raw, main_raw))
if unit not in {"arc-node.service", "arc-self-heal.service"}:
    raise SystemExit("sealed writer supervisor unit is not reviewed")
if not re.fullmatch(r"[0-9a-f]{64}", validator):
    raise SystemExit("sealed writer validator address is malformed")
if not all(re.fullmatch(r"[0-9a-f]{64}", value) for value in (freeze_sha, executable_sha, argv_sha)):
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
if unit not in cgroup:
    raise SystemExit("writer is no longer owned by the sealed supervisor unit")
actual_main = int(subprocess.check_output(
    ["systemctl", "show", unit, "--property=MainPID", "--value"], text=True
).strip())
if actual_main != unit_main_pid:
    raise SystemExit("supervisor MainPID differs from sealed audit")
if unit_main_pid != pid:
    raise SystemExit("reviewed supervisor MainPID is not the exact writer")
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
node_info = None
for port in (9090, 9944):
    try:
        with urllib.request.urlopen(f"http://127.0.0.1:{port}/node/info", timeout=10) as response:
            node_info = json.loads(response.read(1024 * 1024 + 1))
        break
    except Exception:
        pass
if not isinstance(node_info, dict):
    raise SystemExit("writer identity endpoint is unavailable at freeze")
actual_validator = str(node_info.get("validator", "")).removeprefix("0x")
if actual_validator != validator or node_info.get("stake") != stake:
    raise SystemExit("writer validator identity/stake differs from sealed audit")
value = {
    "schema": "arc.recovery.exact-writer.v1",
    "freeze_plan_sha256": freeze_sha,
    "validator_address": validator,
    "stake": stake,
    "writer_pid": pid,
    "writer_start_ticks": start_ticks,
    "boot_id": boot_id,
    "supervisor_unit": unit,
    "supervisor_main_pid": unit_main_pid,
    "executable_path": executable_path,
    "executable_sha256": executable_sha,
    "argv_sha256": argv_sha,
    "data_dir": str(data_dir),
}
with output.open("x", encoding="utf-8", newline="\n") as handle:
    json.dump(value, handle, sort_keys=True, separators=(",", ":")); handle.write("\n")
PY
}

stop_node_cleanly() {
    local evidence_root="$1" writer_pid="$2"
    require_commands pgrep kill systemctl sync sleep python3 grep mkdir mktemp mv chmod
    install_legacy_restart_fence "$evidence_root"
    if [ -d "/proc/$writer_pid" ]; then
        [ "$(cat "/proc/$writer_pid/comm")" = arc-node ] || \
            die "audited writer PID changed identity before TERM"
        kill -TERM "$writer_pid"
    fi
    local shutdown_wait=0
    while [ -d "/proc/$writer_pid" ] && [ "$shutdown_wait" -lt 120 ]; do
        sleep 0.5
        shutdown_wait=$((shutdown_wait + 1))
    done
    if [ -d "/proc/$writer_pid" ]; then
        die "exact arc-node writer did not complete a clean shutdown; refusing SIGKILL and freeze"
    fi
    pgrep -x arc-node >/dev/null 2>&1 && die "an unreviewed arc-node process appeared during freeze"
    sync
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
    local start_ticks="$6" boot_id="$7" unit="$8" unit_main_pid="$9"
    local executable_path="${10}" executable_sha="${11}" argv_sha="${12}" data_dir="${13}"
    python3 - "$root/evidence/writer-contract.json" "$root/stop.context" \
        "$freeze_sha" "$validator" "$stake" "$writer_pid" "$start_ticks" \
        "$boot_id" "$unit" "$unit_main_pid" "$executable_path" \
        "$executable_sha" "$argv_sha" "$data_dir" <<'PY'
import json
import pathlib
import sys

(contract_raw, context_raw, freeze_sha, validator, stake_raw, pid_raw,
 start_raw, boot_id, unit, main_raw, executable_path, executable_sha,
 argv_sha, data_dir) = sys.argv[1:]
contract_path = pathlib.Path(contract_raw)
context_path = pathlib.Path(context_raw)
contract = json.loads(contract_path.read_text(encoding="utf-8"))
expected = {
    "schema": "arc.recovery.exact-writer.v1",
    "freeze_plan_sha256": freeze_sha,
    "validator_address": validator,
    "stake": int(stake_raw),
    "writer_pid": int(pid_raw),
    "writer_start_ticks": int(start_raw),
    "boot_id": boot_id,
    "supervisor_unit": unit,
    "supervisor_main_pid": int(main_raw),
    "executable_path": executable_path,
    "executable_sha256": executable_sha,
    "argv_sha256": argv_sha,
    "data_dir": data_dir,
}
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
    "validator_address", "stake", "data_dir",
}:
    raise SystemExit("stop context fields are not exact")
if (
    context["schema"] != "arc.recovery.offline-stop.v1"
    or context["persistent_restart_fence"] != "true"
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
    local writer_pid="$6" start_ticks="$7" boot_id="$8" unit="$9"
    local unit_main_pid="${10}" executable_path="${11}" executable_sha="${12}"
    local argv_sha="${13}" data_dir="${14}"
    require_hash "$capture_id" "capture id"
    require_node "$node"
    require_hash "$freeze_sha" "freeze plan hash"
    require_hash "$validator" "validator address"
    require_uint "$stake" "writer stake"
    require_uint "$writer_pid" "writer pid"
    require_uint "$start_ticks" "writer start ticks"
    printf '%s\n' "$boot_id" | grep -Eq '^[0-9a-f-]{36}$' || die "boot id is malformed"
    case "$unit" in arc-node.service|arc-self-heal.service) ;; *) die "supervisor unit is not reviewed" ;; esac
    require_uint "$unit_main_pid" "supervisor MainPID"
    require_safe_absolute_path "$executable_path" "writer executable path"
    require_hash "$executable_sha" "writer executable hash"
    require_hash "$argv_sha" "writer argv hash"
    require_safe_absolute_path "$data_dir" "writer data directory"
    require_commands curl python3 mktemp mv chmod date find pgrep
    local parent="$STOP_BASE/$capture_id" stop_root="$STOP_BASE/$capture_id/$node"
    mkdir -p -- "$parent"
    if [ -e "$stop_root" ]; then
        verify_tree_index "$stop_root" stop.files.sha256 stop.complete
        verify_stop_identity "$stop_root" "$capture_id" "$node"
        stopped_status "$capture_id" "$node" "$freeze_sha" "$validator" "$stake" \
            "$writer_pid" "$start_ticks" "$boot_id" "$unit" "$unit_main_pid" \
            "$executable_path" "$executable_sha" "$argv_sha" "$data_dir"
        return 0
    fi
    local temporary="$parent/.${node}.stop.partial"
    prepare_owned_partial_directory "$temporary" \
        "schema=arc.recovery.partial.v1 capture=$capture_id node=$node phase=stop"
    ARCHIVE_NODE_TEMP_PATH="$temporary"
    mkdir -- "$temporary/evidence"
    capture_pre_stop_evidence "$temporary/evidence"
    verify_exact_writer "$temporary/evidence" "$freeze_sha" "$validator" "$stake" \
        "$writer_pid" "$start_ticks" "$boot_id" "$unit" "$unit_main_pid" \
        "$executable_path" "$executable_sha" "$argv_sha" "$data_dir"
    stop_node_cleanly "$temporary/evidence" "$writer_pid"
    verify_legacy_restart_fence
    pgrep -x arc-node >/dev/null 2>&1 && die "legacy arc-node restarted after the persistent fence"
    {
        printf 'schema=arc.recovery.offline-stop.v1\n'
        printf 'capture_id=%s\n' "$capture_id"
        printf 'node=%s\n' "$node"
        printf 'stopped_at=%s\n' "$(date -u +%Y-%m-%dT%H:%M:%SZ)"
        printf 'persistent_restart_fence=true\n'
        printf 'freeze_plan_sha256=%s\n' "$freeze_sha"
        printf 'validator_address=%s\n' "$validator"
        printf 'stake=%s\n' "$stake"
        printf 'data_dir=%s\n' "$data_dir"
    } > "$temporary/stop.context"
    rm -f -- "$temporary/.arc-recovery-partial-owner"
    write_tree_index "$temporary" stop.files.sha256 stop.complete
    write_complete_marker "$temporary" stop.files.sha256 stop.complete arc.recovery.offline-stop.v1 \
        "capture_id=$capture_id" "node=$node" "stopped=true"
    chmod -R a-w,go-rwx -- "$temporary"
    mv -- "$temporary" "$stop_root"
    ARCHIVE_NODE_TEMP_PATH=""
    verify_tree_index "$stop_root" stop.files.sha256 stop.complete
    verify_stop_identity "$stop_root" "$capture_id" "$node"
    stopped_status "$capture_id" "$node" "$freeze_sha" "$validator" "$stake" \
        "$writer_pid" "$start_ticks" "$boot_id" "$unit" "$unit_main_pid" \
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
    if [ "$#" -eq 14 ]; then
        verify_sealed_stop_contract "$stop_root" "$3" "$4" "$5" "$6" "$7" \
            "$8" "$9" "${10}" "${11}" "${12}" "${13}" "${14}"
    elif [ "$#" -ne 2 ]; then
        die "stopped-status exact contract arguments are incomplete"
    fi
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
    verify_capture_source "$capture_root" "$capture_id" "$node"
    if pgrep -x arc-node >/dev/null 2>&1; then
        die "capture is complete but arc-node is running"
    fi
    verify_legacy_restart_fence
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
    [ "$#" -eq 14 ] || die "sealed-source-status requires the exact freeze writer contract"
    verify_sealed_stop_contract "$stop_root" "$3" "$4" "$5" "$6" "$7" \
        "$8" "$9" "${10}" "${11}" "${12}" "${13}" "${14}"
    verify_legacy_restart_fence
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
    local binding_root="$BINDING_BASE/$manifest/$node"
    verify_capture_source "$capture_root" "$capture_id" "$node"
    verify_tree_index "$binding_root" binding.files.sha256 binding.complete
    verify_binding_identity "$binding_root" "$capture_id" "$node" "$manifest"
    pgrep -x arc-node >/dev/null 2>&1 && die "refusing archive stream while arc-node is running"
    verify_legacy_restart_fence

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
    fence-stop)
        [ "$#" -eq 15 ] || { usage >&2; exit 2; }
        fence_stop "$2" "$3" "$4" "$5" "$6" "$7" "$8" "$9" \
            "${10}" "${11}" "${12}" "${13}" "${14}" "${15}"
        ;;
    stopped-status)
        { [ "$#" -eq 3 ] || [ "$#" -eq 15 ]; } || { usage >&2; exit 2; }
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
        [ "$#" -eq 15 ] || { usage >&2; exit 2; }
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
