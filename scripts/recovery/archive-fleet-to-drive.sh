#!/usr/bin/env bash
# Two-phase six-validator freeze, checkpoint binding, and content-verified archive.
# Dry-run is the default for both mutating phases.
set -Eeuo pipefail
umask 077

SCRIPT_DIR="$(CDPATH='' cd -- "$(dirname -- "${BASH_SOURCE[0]}")" && pwd)"
REPO_ROOT="$(CDPATH='' cd -- "$SCRIPT_DIR/../.." && pwd)"
ORCHESTRATOR="$SCRIPT_DIR/archive-fleet-to-drive.sh"
REMOTE_HELPER="$SCRIPT_DIR/archive-node.sh"
ROLLOUT_TOOL="$SCRIPT_DIR/recovery_rollout.py"
ROLLOUT_SCHEMA="$SCRIPT_DIR/recovery-manifest.schema.json"
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
  archive-fleet-to-drive.sh audit-writers --legacy-validator-set /absolute/legacy-validators.json \
    --output /absolute/writers.lock.json

  archive-fleet-to-drive.sh seal-freeze-plan --window ID \
    --legacy-validator-set /absolute/legacy-validators.json \
    --writer-contracts /absolute/writers.lock.json \
    --output /absolute/freeze.lock.json

  archive-fleet-to-drive.sh capture --freeze-plan /absolute/freeze.lock.json [--plan]
  ARC_RECOVERY_FREEZE_GO='FREEZE PLAN_SHA256 CAPTURE CAPTURE_SHA256' archive-fleet-to-drive.sh capture \
    --freeze-plan /absolute/freeze.lock.json --execute

  archive-fleet-to-drive.sh seal --freeze-plan /absolute/freeze.lock.json \
    --manifest /absolute/rollout.lock.json \
    --validator-public-keys /absolute/validators.json \
    [--allow-unbound-legacy-wal] [--plan]
  ARC_RECOVERY_GO='GO ROLLOUT_SHA256 FREEZE PLAN_SHA256 CAPTURE CAPTURE_SHA256 DEST DRIVE_SHA256 LEGACY_WAL BOUND_OR_UNBOUND' archive-fleet-to-drive.sh seal \
    --freeze-plan /absolute/freeze.lock.json \
    --manifest /absolute/rollout.lock.json \
    --validator-public-keys /absolute/validators.json \
    --allow-unbound-legacy-wal --execute

  archive-fleet-to-drive.sh verify-complete --destination 'REMOTE:path/captures/CAPTURE_SHA256' \
    [--expected-complete-sha256 HASH --expected-archive-manifest-sha256 HASH \
     --expected-sha256sums-sha256 HASH --expected-prearchive-rollout-sha256 HASH] \
    [--new-node-paths NODE REMOTE_ROOT DATA_DIR]... [--verify-live-captures]

The freeze plan is sealed before the final checkpoint exists. It binds a
read-only audit of each exact writer PID/start-time/boot/unit/argv/executable,
validator identity/stake, and real data directory to the audited eight-member
legacy source set. `capture` persistently fences and cleanly stops all six
controlled writers before content-indexing any chain directory. Their exact 30M source
stake is more than one third of the sealed 40M set, so that sealed source set
cannot make quorum. Dynamically admitted external legacy identities are
recorded as untrusted forks; this tool never claims the vulnerable old network
globally halted. No legacy byte is deleted.

`seal` runs only after the final 5-of-6 checkpoint and the canonical prearchive
rollout manifest exist. That prearchive has four all-zero archive-finalization
roots; the final manifest may replace only those roots and must project exactly
back to the archived prearchive digest.
The exact recovery exporter verifies each stopped WAL only with that capture's
own on-disk snapshot. A derivable pair is labelled canonical or a fork; a
missing, ambiguous, or torn pair is preserved_unclassified and is never
combined with the canonical reference snapshot. Every exact stopped source is
streamed without a second full local tree, and all six streams plus the sealed
public inputs are uploaded to the exact capture-scoped Drive destination. A
canonical archive manifest and checksum are uploaded only after every object
check passes. COMPLETE.json is the last create-only mutation in this execution;
Drive is not represented as WORM, so consumers must cryptographically reverify
every object and reject a destination without a valid COMPLETE.
EOF
}

require_hash() {
    printf '%s\n' "$1" | grep -Eq '^[0-9a-f]{64}$' || \
        die "$2 must be exactly 64 lowercase hexadecimal characters"
}

require_uint() {
    printf '%s\n' "$1" | grep -Eq '^(0|[1-9][0-9]*)$' || \
        die "$2 must be an unsigned integer"
}

require_absolute_file() {
    case "$1" in /*) ;; *) die "$2 must be an absolute path" ;; esac
    [ -f "$1" ] && [ ! -L "$1" ] || die "$2 is missing, non-regular, or a symlink: $1"
}

validate_drive_remote() {
    python3 - "$1" <<'PY'
import re
import sys

value = sys.argv[1]
if ("\x00" in value or "\n" in value or "\r" in value or value.startswith("-")
        or ":" not in value or value.endswith("/")):
    raise SystemExit("unsafe Drive remote")
remote, path = value.split(":", 1)
if not re.fullmatch(r"[A-Za-z0-9][A-Za-z0-9_-]{0,63}", remote):
    raise SystemExit("unsafe Drive remote name")
if not re.fullmatch(r"[A-Za-z0-9][A-Za-z0-9 ._/@%+=,-]{0,511}", path):
    raise SystemExit("unsafe Drive remote path")
if ".." in path.split("/"):
    raise SystemExit("Drive remote traversal is forbidden")
PY
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

tracked_source_hash() {
    local path="$1" relative
    relative="${path#"$REPO_ROOT"/}"
    [ "$relative" != "$path" ] || die "tracked source is outside the repository: $path"
    git -C "$REPO_ROOT" diff --quiet HEAD -- "$relative" || \
        die "tracked recovery source differs from HEAD: $relative"
    local disk_sha blob_sha
    disk_sha="$(hash_file "$path")"
    blob_sha="$(git -C "$REPO_ROOT" show "HEAD:$relative" | python3 -c 'import hashlib,sys; print(hashlib.sha256(sys.stdin.buffer.read()).hexdigest())')"
    [ "$disk_sha" = "$blob_sha" ] || die "tracked recovery source blob differs from HEAD: $relative"
    printf '%s\n' "$disk_sha"
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

audit_writers() {
    local legacy_validators="" output=""
    while [ "$#" -gt 0 ]; do
        case "$1" in
            --legacy-validator-set) [ "$#" -ge 2 ] || die "--legacy-validator-set needs a value"; legacy_validators="$2"; shift 2 ;;
            --output) [ "$#" -ge 2 ] || die "--output needs a value"; output="$2"; shift 2 ;;
            -h|--help) usage; return 0 ;;
            *) die "unknown audit-writers option: $1" ;;
        esac
    done
    require_absolute_file "$legacy_validators" "legacy validator set"
    case "$output" in /*.json) ;; *) die "--output must be an absolute .json path" ;; esac
    [ "$SSH_USER" = root ] || die "writer audit requires the sealed root SSH user"
    require_commands python3 ssh git
    local legacy_sha temporary node host
    legacy_sha="$(hash_file "$legacy_validators")"
    temporary="$(mktemp -d)"
    ARCHIVE_FLEET_TEMP_ROOT="$temporary"
    trap cleanup_temporary_root EXIT
    for node in nyc lax ams lhr nrt sgp; do
        host="$(host_for "$node")"
        ssh "${SSH_OPTIONS[@]}" "$SSH_USER@$host" -- python3 - "$node" "$host" \
            > "$temporary/$node.json" <<'PY'
import hashlib
import json
import os
import pathlib
import re
import subprocess
import sys
import urllib.request

name, host = sys.argv[1:]

def fail(message):
    raise SystemExit(f"writer audit {name}: {message}")

def sha256(path):
    digest = hashlib.sha256()
    with open(path, "rb") as handle:
        for chunk in iter(lambda: handle.read(1024 * 1024), b""):
            digest.update(chunk)
    return digest.hexdigest()

def uint(value, field):
    if isinstance(value, bool):
        fail(f"{field} is boolean")
    try:
        value = int(value)
    except (TypeError, ValueError):
        fail(f"{field} is not an integer")
    if value < 0:
        fail(f"{field} is negative")
    return value

def address(value, field):
    if not isinstance(value, str):
        fail(f"{field} is not a string")
    value = value.removeprefix("0x")
    if not re.fullmatch(r"[0-9a-f]{64}", value):
        fail(f"{field} is not a lowercase 32-byte address")
    return value

pids = []
for entry in pathlib.Path("/proc").iterdir():
    if not entry.name.isdigit():
        continue
    try:
        if (entry / "comm").read_text(encoding="utf-8").strip() == "arc-node":
            pids.append(int(entry.name))
    except (FileNotFoundError, PermissionError, ProcessLookupError):
        pass
if len(pids) != 1:
    fail(f"expected exactly one arc-node writer, found {pids}")
pid = pids[0]
proc = pathlib.Path("/proc") / str(pid)
boot_id = pathlib.Path("/proc/sys/kernel/random/boot_id").read_text(encoding="ascii").strip()
if not re.fullmatch(r"[0-9a-f-]{36}", boot_id):
    fail("kernel boot id is malformed")
stat_fields = (proc / "stat").read_text(encoding="ascii").split()
if len(stat_fields) < 22:
    fail("writer /proc stat is truncated")
start_ticks = uint(stat_fields[21], "writer start ticks")
argv_raw = (proc / "cmdline").read_bytes()
argv = [item.decode("utf-8") for item in argv_raw.rstrip(b"\0").split(b"\0")]
if not argv or not argv[0]:
    fail("writer argv is empty")
cwd = pathlib.Path(os.readlink(proc / "cwd"))

def option_values(option):
    values = []
    for index, item in enumerate(argv):
        if item == option:
            if index + 1 >= len(argv) or argv[index + 1].startswith("--"):
                fail(f"{option} has no value")
            values.append(argv[index + 1])
        elif item.startswith(option + "="):
            values.append(item.split("=", 1)[1])
    return values

data_raw = None
for index, item in enumerate(argv):
    if item == "--data-dir":
        if index + 1 >= len(argv):
            fail("--data-dir has no value")
        data_raw = argv[index + 1]
    elif item.startswith("--data-dir="):
        data_raw = item.split("=", 1)[1]
if data_raw is None:
    data_raw = "arc-data"
data_candidate = pathlib.Path(data_raw)
if not data_candidate.is_absolute():
    data_candidate = cwd / data_candidate
data_dir = pathlib.Path(os.path.realpath(data_candidate))
if not data_dir.is_dir() or data_dir.is_symlink():
    fail(f"real writer data directory is unavailable or a symlink: {data_dir}")
if not (data_dir / "state.wal").is_file() or (data_dir / "state.wal").is_symlink():
    fail("real writer data directory has no regular state.wal")

model_values = option_values("--model")
if len(model_values) != 1:
    fail(f"expected exactly one --model argument, found {model_values}")
model_candidate = pathlib.Path(model_values[0])
if not model_candidate.is_absolute():
    model_candidate = cwd / model_candidate
model_path = pathlib.Path(os.path.realpath(model_candidate))
if not model_path.is_file() or model_path.is_symlink():
    fail(f"resolved model is unavailable, non-regular, or a symlink: {model_path}")
model_size_bytes = model_path.stat().st_size
model_sha256 = sha256(model_path)
if model_size_bytes != 4_081_004_224:
    fail(f"model size differs from reviewed Llama-2-7B bytes: {model_size_bytes}")
if model_sha256 != "08a5566d61d7cb6b420c3e4387a39e0078e1f2fe5f055f3a03887385304d4bfa":
    fail(f"model SHA-256 differs from reviewed Llama-2-7B bytes: {model_sha256}")

expected_shards = {
    "nyc": [[0, 6], [22, 27], [27, 32]],
    "lax": [[0, 6], [6, 12], [27, 32]],
    "ams": [[0, 6], [6, 12], [12, 17]],
    "lhr": [[6, 12], [12, 17], [17, 22]],
    "nrt": [[12, 17], [17, 22], [22, 27]],
    "sgp": [[17, 22], [22, 27], [27, 32]],
}
shard_ranges = []
for value in option_values("--shard-range"):
    if not re.fullmatch(r"(?:0|[1-9][0-9]*):(?:0|[1-9][0-9]*)", value):
        fail(f"malformed --shard-range argument: {value!r}")
    start, end = map(int, value.split(":", 1))
    shard_ranges.append([start, end])
if shard_ranges != expected_shards[name]:
    fail(f"live shard arguments differ from the reviewed {name} assignment: {shard_ranges}")

exe_path = os.readlink(proc / "exe")
if not os.path.isabs(exe_path):
    fail("writer executable is not absolute")
cgroup = (proc / "cgroup").read_text(encoding="utf-8")
units = [unit for unit in ("arc-node.service", "arc-self-heal.service") if unit in cgroup]
if len(units) != 1:
    fail(f"writer does not belong to exactly one reviewed systemd unit: {units}")
unit = units[0]
unit_main_pid = uint(
    subprocess.check_output(
        ["systemctl", "show", unit, "--property=MainPID", "--value"], text=True
    ).strip(),
    "unit MainPID",
)
if unit_main_pid <= 0 or not pathlib.Path(f"/proc/{unit_main_pid}").exists():
    fail(f"reviewed supervisor unit is not active: {unit}")
if unit_main_pid != pid:
    fail("reviewed supervisor MainPID is not the exact writer PID")

node_info = None
rpc_origin = None
for port in (9090, 9944):
    try:
        with urllib.request.urlopen(f"http://127.0.0.1:{port}/node/info", timeout=10) as response:
            node_info = json.loads(response.read(1024 * 1024 + 1))
        rpc_origin = f"http://127.0.0.1:{port}"
        break
    except Exception:
        pass
if not isinstance(node_info, dict) or rpc_origin is None:
    fail("writer /node/info identity endpoint is unavailable")
validator_address = address(node_info.get("validator"), "node/info validator")
stake = uint(node_info.get("stake"), "node/info stake")
if stake <= 0:
    fail("controlled writer has no positive source stake")

observed_positive = []
observed_error = None
try:
    with urllib.request.urlopen(f"{rpc_origin}/validators", timeout=10) as response:
        body = json.loads(response.read(8 * 1024 * 1024 + 1))
    rows = body.get("validators") if isinstance(body, dict) else body
    if not isinstance(rows, list):
        raise ValueError("validators response has no array")
    seen = set()
    for row in rows:
        if not isinstance(row, dict):
            raise ValueError("validator row is not an object")
        row_stake = uint(row.get("stake"), "observed validator stake")
        if row_stake == 0:
            continue
        row_address = address(row.get("address"), "observed validator address")
        key = (row_address, row_stake)
        if key not in seen:
            observed_positive.append({"address": row_address, "stake": row_stake})
            seen.add(key)
    observed_positive.sort(key=lambda row: (row["address"], row["stake"]))
except Exception as error:
    observed_error = str(error)

data_bytes = uint(
    subprocess.check_output(["du", "-s", "-B1", str(data_dir)], text=True).split()[0],
    "data directory bytes",
)
data_files = 0
data_device = data_dir.stat().st_dev
for base, dirs, files in os.walk(data_dir, followlinks=False):
    for item in dirs:
        directory = pathlib.Path(base) / item
        if directory.is_symlink():
            fail("writer data directory contains a symlink directory")
        if directory.stat().st_dev != data_device:
            fail("writer data directory contains a cross-device directory")
    for item in files:
        candidate = pathlib.Path(base) / item
        if candidate.is_symlink() or not candidate.is_file():
            fail("writer data directory contains a symlink or non-regular file")
        if candidate.stat().st_dev != data_device:
            fail("writer data directory contains a cross-device file")
    data_files += len(files)
target_stat = os.statvfs("/root")
available_bytes = target_stat.f_bavail * target_stat.f_frsize
available_inodes = target_stat.f_favail
wal_bytes = (data_dir / "state.wal").stat().st_size
snapshot_bytes = sum(
    candidate.stat().st_size
    for candidate in (data_dir / "state.snapshot.lz4", pathlib.Path(str(data_dir) + ".snapshot.lz4"))
    if candidate.is_file() and not candidate.is_symlink()
)
new_v3_headroom_bytes = data_bytes
max_binding_temporary_bytes = max(data_bytes, wal_bytes + snapshot_bytes) + 2 * 1024 * 1024 * 1024
archive_stream_temporary_bytes = 0
required_free_bytes = new_v3_headroom_bytes + max_binding_temporary_bytes
required_free_inodes = data_files + 10_000

print(json.dumps({
    "name": name,
    "host": host,
    "boot_id": boot_id,
    "writer_pid": pid,
    "writer_start_ticks": start_ticks,
    "supervisor_unit": unit,
    "supervisor_main_pid": unit_main_pid,
    "executable_path": exe_path,
    "executable_sha256": sha256(f"/proc/{pid}/exe"),
    "argv_sha256": hashlib.sha256(argv_raw).hexdigest(),
    "data_dir": str(data_dir),
    "model_path": str(model_path),
    "model_sha256": model_sha256,
    "model_size_bytes": model_size_bytes,
    "shard_ranges": shard_ranges,
    "data_device": data_device,
    "data_bytes": data_bytes,
    "data_files": data_files,
    "capture_device": os.stat("/root").st_dev,
    "available_bytes": available_bytes,
    "available_inodes": available_inodes,
    "required_free_bytes": required_free_bytes,
    "required_free_inodes": required_free_inodes,
    "new_v3_headroom_bytes": new_v3_headroom_bytes,
    "max_binding_temporary_bytes": max_binding_temporary_bytes,
    "archive_stream_temporary_bytes": archive_stream_temporary_bytes,
    "validator_address": validator_address,
    "stake": stake,
    "rpc_origin": rpc_origin,
    "observed_positive_validators": observed_positive,
    "observed_validator_error": observed_error,
}, sort_keys=True, separators=(",", ":")))
PY
        printf '  audited writer: %s %s\n' "$node" "$host"
    done

    python3 - "$output" "$legacy_validators" "$legacy_sha" "$temporary" "${NODES[@]}" <<'PY'
import datetime
import hashlib
import json
import os
import pathlib
import re
import stat
import sys

output = pathlib.Path(sys.argv[1])
legacy_path = pathlib.Path(sys.argv[2])
legacy_sha = sys.argv[3]
audit_root = pathlib.Path(sys.argv[4])
expected = [entry.split("=", 1) for entry in sys.argv[5:]]

def canonical(value):
    return (json.dumps(value, sort_keys=True, separators=(",", ":")) + "\n").encode()

def normalize_address(value):
    if not isinstance(value, str):
        raise SystemExit("legacy validator address is not a string")
    value = value.removeprefix("0x")
    if not re.fullmatch(r"[0-9a-f]{64}", value):
        raise SystemExit("legacy validator address is malformed")
    return value

legacy_raw = json.loads(legacy_path.read_text(encoding="utf-8"))
if not isinstance(legacy_raw, list) or len(legacy_raw) != 8:
    raise SystemExit("legacy source set must contain exactly eight validators")
legacy = []
for row in legacy_raw:
    if not isinstance(row, dict) or set(row) != {"address", "stake"}:
        raise SystemExit("legacy validator rows must contain only address/stake")
    stake = row["stake"]
    if isinstance(stake, bool) or not isinstance(stake, int) or stake <= 0:
        raise SystemExit("legacy validator stake must be positive")
    legacy.append({"address": normalize_address(row["address"]), "stake": stake})
legacy.sort(key=lambda row: row["address"])
if len({row["address"] for row in legacy}) != 8 or sum(row["stake"] for row in legacy) != 40_000_000:
    raise SystemExit("legacy source set must be eight unique validators totalling 40M")
legacy_by_address = {row["address"]: row["stake"] for row in legacy}

nodes = []
expected_shards = {
    "nyc": [[0, 6], [22, 27], [27, 32]],
    "lax": [[0, 6], [6, 12], [27, 32]],
    "ams": [[0, 6], [6, 12], [12, 17]],
    "lhr": [[6, 12], [12, 17], [17, 22]],
    "nrt": [[12, 17], [17, 22], [22, 27]],
    "sgp": [[17, 22], [22, 27], [27, 32]],
}
for name, host in expected:
    row = json.loads((audit_root / f"{name}.json").read_text(encoding="utf-8"))
    if row.get("name") != name or row.get("host") != host:
        raise SystemExit("writer audit host/name differs from reviewed fleet")
    address = row.get("validator_address")
    if address not in legacy_by_address or legacy_by_address[address] != row.get("stake"):
        raise SystemExit(f"controlled writer {name} is not an exact member of the sealed legacy set")
    if row["available_bytes"] < row["required_free_bytes"]:
        raise SystemExit(f"controlled writer {name} lacks safe archive free space")
    if row["available_inodes"] < row["required_free_inodes"]:
        raise SystemExit(f"controlled writer {name} lacks safe archive free inodes")
    if (row.get("model_sha256") != "08a5566d61d7cb6b420c3e4387a39e0078e1f2fe5f055f3a03887385304d4bfa"
            or row.get("model_size_bytes") != 4_081_004_224
            or not isinstance(row.get("model_path"), str)
            or not row["model_path"].startswith("/")
            or row.get("shard_ranges") != expected_shards[name]):
        raise SystemExit(f"controlled writer {name} model bytes/path or shard assignment differs")
    nodes.append(row)
if len({row["validator_address"] for row in nodes}) != 6:
    raise SystemExit("controlled writer identities are not unique")
controlled_stake = sum(row["stake"] for row in nodes)
total_stake = sum(row["stake"] for row in legacy)
quorum_stake = total_stake * 2 // 3 + 1
remaining_stake = total_stake - controlled_stake
if controlled_stake * 3 <= total_stake or remaining_stake >= quorum_stake:
    raise SystemExit("stopping all controlled writers does not provably remove sealed-source quorum")
controlled = {row["validator_address"] for row in nodes}
external_source = [row for row in legacy if row["address"] not in controlled]
observed_sets = []
external_observations = {}
for row in nodes:
    observed = row["observed_positive_validators"]
    observed_sets.append(tuple((item["address"], item["stake"]) for item in observed))
    for item in observed:
        if item["address"] not in controlled:
            external_observations[(item["address"], item["stake"])] = item

value = {
    "schema": "arc.recovery.writer-contracts.v1",
    "created_at": datetime.datetime.now(datetime.timezone.utc).replace(microsecond=0).isoformat().replace("+00:00", "Z"),
    "legacy_validator_set_sha256": legacy_sha,
    "legacy_validators": legacy,
    "source_total_stake": total_stake,
    "source_quorum_stake": quorum_stake,
    "controlled_writer_stake": controlled_stake,
    "maximum_source_stake_after_controlled_stop": remaining_stake,
    "controlled_quorum_unavailable_after_all_stops": True,
    "global_legacy_halt_claimed": False,
    "external_source_validators": external_source,
    "untrusted_external_observations": sorted(external_observations.values(), key=lambda row: (row["address"], row["stake"])),
    "dynamic_membership_disagrees": len(set(observed_sets)) > 1,
    "nodes": nodes,
}
payload = canonical(value)
digest = hashlib.sha256(payload).hexdigest()
sidecar = output.with_name(output.name + ".sha256")
if output.exists() or sidecar.exists():
    raise SystemExit("writer contract or sidecar already exists")
output.parent.mkdir(parents=True, exist_ok=True)
created = []
try:
    fd = os.open(output, os.O_WRONLY | os.O_CREAT | os.O_EXCL | getattr(os, "O_NOFOLLOW", 0), 0o444)
    with os.fdopen(fd, "wb") as handle:
        handle.write(payload); handle.flush(); os.fsync(handle.fileno())
    created.append(output)
    fd = os.open(sidecar, os.O_WRONLY | os.O_CREAT | os.O_EXCL | getattr(os, "O_NOFOLLOW", 0), 0o444)
    with os.fdopen(fd, "w", encoding="ascii", newline="\n") as handle:
        handle.write(f"{digest}  {output.name}\n"); handle.flush(); os.fsync(handle.fileno())
    created.append(sidecar)
    directory_fd = os.open(output.parent, os.O_RDONLY)
    try: os.fsync(directory_fd)
    finally: os.close(directory_fd)
except Exception:
    for path in reversed(created):
        path.chmod(0o600); path.unlink()
    raise
print(digest)
PY
    local digest
    digest="$(hash_file "$output")"
    printf 'archive fleet: sealed exact live writer contracts %s\n' "$output"
    printf 'archive fleet: writer contracts sha256 %s\n' "$digest"
}

seal_freeze_plan() {
    local window="" output="" legacy_validators="" writer_contracts=""
    while [ "$#" -gt 0 ]; do
        case "$1" in
            --window) [ "$#" -ge 2 ] || die "--window needs a value"; window="$2"; shift 2 ;;
            --legacy-validator-set) [ "$#" -ge 2 ] || die "--legacy-validator-set needs a value"; legacy_validators="$2"; shift 2 ;;
            --writer-contracts) [ "$#" -ge 2 ] || die "--writer-contracts needs a value"; writer_contracts="$2"; shift 2 ;;
            --output) [ "$#" -ge 2 ] || die "--output needs a value"; output="$2"; shift 2 ;;
            -h|--help) usage; return 0 ;;
            *) die "unknown seal-freeze-plan option: $1" ;;
        esac
    done
    printf '%s\n' "$window" | grep -Eq '^[A-Za-z0-9][A-Za-z0-9._:@+-]{0,127}$' || \
        die "--window must be a short reviewable change/window identifier"
    case "$output" in /*.json) ;; *) die "--output must be an absolute .json path" ;; esac
    require_absolute_file "$legacy_validators" "legacy validator set"
    require_absolute_file "$writer_contracts" "writer contracts"
    require_absolute_file "${writer_contracts}.sha256" "writer-contract checksum"
    require_commands python3 git
    require_absolute_file "$ORCHESTRATOR" "archive orchestrator"
    require_absolute_file "$REMOTE_HELPER" "remote archive helper"
    require_absolute_file "$ROLLOUT_TOOL" "rollout verifier"
    require_absolute_file "$ROLLOUT_SCHEMA" "rollout schema"
    local helper_sha orchestrator_sha rollout_tool_sha schema_sha source_commit legacy_sha contracts_sha
    helper_sha="$(tracked_source_hash "$REMOTE_HELPER")"
    orchestrator_sha="$(tracked_source_hash "$ORCHESTRATOR")"
    rollout_tool_sha="$(tracked_source_hash "$ROLLOUT_TOOL")"
    schema_sha="$(tracked_source_hash "$ROLLOUT_SCHEMA")"
    source_commit="$(current_source_commit)"
    legacy_sha="$(hash_file "$legacy_validators")"
    contracts_sha="$(hash_file "$writer_contracts")"
    python3 - "$output" "$window" "$helper_sha" "$orchestrator_sha" \
        "$rollout_tool_sha" "$schema_sha" "$source_commit" "$legacy_validators" \
        "$legacy_sha" "$writer_contracts" "$contracts_sha" "${NODES[@]}" <<'PY'
import datetime
import hashlib
import json
import os
import pathlib
import stat
import sys

output = pathlib.Path(sys.argv[1])
(window, helper_sha, orchestrator_sha, rollout_tool_sha, schema_sha,
 source_commit, legacy_path_raw, legacy_sha, contracts_path_raw,
 contracts_sha) = sys.argv[2:12]
expected_nodes = [entry.split("=", 1) for entry in sys.argv[12:]]
legacy_path = pathlib.Path(legacy_path_raw)
contracts_path = pathlib.Path(contracts_path_raw)

def canonical(value):
    return (json.dumps(value, sort_keys=True, separators=(",", ":")) + "\n").encode()

def verify_locked(path, expected_sha):
    sidecar = path.with_name(path.name + ".sha256")
    for candidate in (path, sidecar):
        details = candidate.lstat()
        if stat.S_ISLNK(details.st_mode) or not stat.S_ISREG(details.st_mode) or details.st_mode & 0o222:
            raise SystemExit(f"writer contract input is mutable or unsafe: {candidate}")
    payload = path.read_bytes()
    if hashlib.sha256(payload).hexdigest() != expected_sha:
        raise SystemExit("writer contract changed while freeze plan was sealed")
    if sidecar.read_text(encoding="ascii") != f"{expected_sha}  {path.name}\n":
        raise SystemExit("writer contract sidecar differs")
    value = json.loads(payload)
    if payload != canonical(value):
        raise SystemExit("writer contract is not canonical JSON")
    return value

contracts = verify_locked(contracts_path, contracts_sha)
if contracts.get("schema") != "arc.recovery.writer-contracts.v1":
    raise SystemExit("writer contract schema is unsupported")
if contracts.get("legacy_validator_set_sha256") != legacy_sha:
    raise SystemExit("writer contract legacy-set hash differs")
if hashlib.sha256(legacy_path.read_bytes()).hexdigest() != legacy_sha:
    raise SystemExit("legacy validator set changed while freeze plan was sealed")
nodes = contracts.get("nodes")
if not isinstance(nodes, list) or [(row.get("name"), row.get("host")) for row in nodes] != [tuple(row) for row in expected_nodes]:
    raise SystemExit("writer contract fleet/order differs from reviewed topology")
if (contracts.get("source_total_stake") != 40_000_000
        or contracts.get("controlled_writer_stake", 0) * 3 <= contracts.get("source_total_stake", 1)
        or contracts.get("maximum_source_stake_after_controlled_stop", 40_000_000) >= contracts.get("source_quorum_stake", 0)
        or contracts.get("controlled_quorum_unavailable_after_all_stops") is not True
        or contracts.get("global_legacy_halt_claimed") is not False):
    raise SystemExit("writer contract does not prove controlled sealed-source quorum removal")
plan = {
    "schema": "arc.recovery.freeze-plan.v2",
    "window": window,
    "created_at": datetime.datetime.now(datetime.timezone.utc).replace(microsecond=0).isoformat().replace("+00:00", "Z"),
    "sentinels": ["nyc", "lax"],
    "nodes": nodes,
    "remote_helper_sha256": helper_sha,
    "orchestrator_sha256": orchestrator_sha,
    "rollout_tool_sha256": rollout_tool_sha,
    "rollout_schema_sha256": schema_sha,
    "source_commit": source_commit,
    "legacy_validator_set_sha256": legacy_sha,
    "writer_contracts_sha256": contracts_sha,
    "quorum_proof": {
        "source_total_stake": contracts["source_total_stake"],
        "source_quorum_stake": contracts["source_quorum_stake"],
        "controlled_writer_stake": contracts["controlled_writer_stake"],
        "maximum_source_stake_after_controlled_stop": contracts["maximum_source_stake_after_controlled_stop"],
        "controlled_quorum_unavailable_after_all_stops": True,
        "global_legacy_halt_claimed": False,
        "external_source_validators": contracts["external_source_validators"],
        "untrusted_external_observations": contracts["untrusted_external_observations"],
        "dynamic_membership_disagrees": contracts["dynamic_membership_disagrees"],
    },
}
payload = (json.dumps(plan, sort_keys=True, separators=(",", ":")) + "\n").encode()
digest = hashlib.sha256(payload).hexdigest()
sidecar = output.with_name(output.name + ".sha256")
if output.exists() or sidecar.exists():
    raise SystemExit("freeze plan or sidecar already exists; refusing replacement")
output.parent.mkdir(parents=True, exist_ok=True)
created = []
try:
    fd = os.open(output, os.O_WRONLY | os.O_CREAT | os.O_EXCL | getattr(os, "O_NOFOLLOW", 0), 0o444)
    with os.fdopen(fd, "wb") as handle:
        handle.write(payload)
        handle.flush()
        os.fsync(handle.fileno())
    created.append(output)
    fd = os.open(sidecar, os.O_WRONLY | os.O_CREAT | os.O_EXCL | getattr(os, "O_NOFOLLOW", 0), 0o444)
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
    require_absolute_file "$ROLLOUT_TOOL" "rollout verifier"
    require_absolute_file "$ROLLOUT_SCHEMA" "rollout schema"
    local helper_sha orchestrator_sha rollout_tool_sha schema_sha source_commit
    helper_sha="$(tracked_source_hash "$REMOTE_HELPER")"
    orchestrator_sha="$(tracked_source_hash "$ORCHESTRATOR")"
    rollout_tool_sha="$(tracked_source_hash "$ROLLOUT_TOOL")"
    schema_sha="$(tracked_source_hash "$ROLLOUT_SCHEMA")"
    source_commit="$(current_source_commit)"
    python3 - "$plan" "$helper_sha" "$orchestrator_sha" "$rollout_tool_sha" \
        "$schema_sha" "$source_commit" "${NODES[@]}" <<'PY'
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
helper_sha, orchestrator_sha, rollout_tool_sha, schema_sha, source_commit = sys.argv[2:7]
expected_nodes = []
for entry in sys.argv[7:]:
    name, host = entry.split("=", 1)
    expected_nodes.append({"name": name, "host": host})
value = json.loads(path.read_text(encoding="utf-8"))
if set(value) != {
    "schema", "window", "created_at", "sentinels", "nodes",
    "remote_helper_sha256", "orchestrator_sha256", "rollout_tool_sha256",
    "rollout_schema_sha256", "source_commit", "legacy_validator_set_sha256",
    "writer_contracts_sha256", "quorum_proof",
}:
    raise SystemExit("freeze plan has missing or unknown fields")
if value["schema"] != "arc.recovery.freeze-plan.v2":
    raise SystemExit("unsupported freeze plan schema")
if not isinstance(value["window"], str) or not re.fullmatch(r"[A-Za-z0-9][A-Za-z0-9._:@+-]{0,127}", value["window"]):
    raise SystemExit("freeze plan window is invalid")
if not isinstance(value["created_at"], str) or not re.fullmatch(r"[0-9]{4}-[0-9]{2}-[0-9]{2}T[0-9]{2}:[0-9]{2}:[0-9]{2}Z", value["created_at"]):
    raise SystemExit("freeze plan timestamp is invalid")
if value["sentinels"] != ["nyc", "lax"]:
    raise SystemExit("freeze plan sentinel order differs")
nodes = value["nodes"]
if not isinstance(nodes, list) or [(row.get("name"), row.get("host")) for row in nodes] != [
    (row["name"], row["host"]) for row in expected_nodes
]:
    raise SystemExit("freeze plan fleet or sentinel order differs from the reviewed six-node topology")
expected_shards = {
    "nyc": [[0, 6], [22, 27], [27, 32]],
    "lax": [[0, 6], [6, 12], [27, 32]],
    "ams": [[0, 6], [6, 12], [12, 17]],
    "lhr": [[6, 12], [12, 17], [17, 22]],
    "nrt": [[12, 17], [17, 22], [22, 27]],
    "sgp": [[17, 22], [22, 27], [27, 32]],
}
for row in nodes:
    if row.get("supervisor_main_pid") != row.get("writer_pid"):
        raise SystemExit(f"freeze plan supervisor MainPID is not the exact writer for {row['name']}")
    if (row.get("model_sha256") != "08a5566d61d7cb6b420c3e4387a39e0078e1f2fe5f055f3a03887385304d4bfa"
            or row.get("model_size_bytes") != 4_081_004_224
            or not isinstance(row.get("model_path"), str)
            or not row["model_path"].startswith("/")
            or row.get("shard_ranges") != expected_shards[row["name"]]):
        raise SystemExit(f"freeze plan model bytes/path or shard assignment differs for {row['name']}")
if value["remote_helper_sha256"] != helper_sha:
    raise SystemExit("remote helper bytes differ from the sealed freeze plan")
if value["orchestrator_sha256"] != orchestrator_sha:
    raise SystemExit("orchestrator bytes differ from the sealed freeze plan")
if value["rollout_tool_sha256"] != rollout_tool_sha:
    raise SystemExit("rollout verifier bytes differ from the sealed freeze plan")
if value["rollout_schema_sha256"] != schema_sha:
    raise SystemExit("rollout schema bytes differ from the sealed freeze plan")
if value["source_commit"] != source_commit:
    raise SystemExit("source commit differs from the sealed freeze plan")
hash_re = re.compile(r"[0-9a-f]{64}")
for field in ("legacy_validator_set_sha256", "writer_contracts_sha256"):
    if not isinstance(value[field], str) or not hash_re.fullmatch(value[field]):
        raise SystemExit(f"freeze plan {field} is malformed")
proof = value["quorum_proof"]
if set(proof) != {
    "source_total_stake", "source_quorum_stake", "controlled_writer_stake",
    "maximum_source_stake_after_controlled_stop",
    "controlled_quorum_unavailable_after_all_stops", "global_legacy_halt_claimed",
    "external_source_validators", "untrusted_external_observations",
    "dynamic_membership_disagrees",
}:
    raise SystemExit("freeze plan quorum proof fields are not exact")
if (proof["source_total_stake"] != 40_000_000
        or proof["controlled_writer_stake"] * 3 <= proof["source_total_stake"]
        or proof["maximum_source_stake_after_controlled_stop"] >= proof["source_quorum_stake"]
        or proof["controlled_quorum_unavailable_after_all_stops"] is not True
        or proof["global_legacy_halt_claimed"] is not False):
    raise SystemExit("freeze plan does not prove controlled sealed-source quorum removal")
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
    local expected_sha="$1" node host remote_temporary
    require_hash "$expected_sha" "sealed remote helper hash"
    REMOTE_HELPER_SHA="$(hash_file "$REMOTE_HELPER")"
    [ "$REMOTE_HELPER_SHA" = "$expected_sha" ] || \
        die "remote helper bytes changed after freeze-plan verification"
    REMOTE_HELPER_PATH="/root/.arc-recovery-helpers/$REMOTE_HELPER_SHA/archive-node.sh"
    for node in nyc lax ams lhr nrt sgp; do
        host="$(host_for "$node")"
        remote_temporary="$(ssh "${SSH_OPTIONS[@]}" "$SSH_USER@$host" -- sh -c \
            'umask 077; root=/root/.arc-recovery-helper-uploads; if test -e "$root"; then test -d "$root" && test ! -L "$root"; else mkdir -m 700 -- "$root"; fi; mktemp "$root/upload.XXXXXX"' sh)"
        case "$remote_temporary" in /root/.arc-recovery-helper-uploads/upload.*) ;; *) die "unsafe remote helper temporary path" ;; esac
        scp -q "${SSH_OPTIONS[@]}" "$REMOTE_HELPER" "$SSH_USER@$host:$remote_temporary"
        ssh "${SSH_OPTIONS[@]}" "$SSH_USER@$host" -- sh -c \
            'set -eu; temporary=$1 target=$2 expected=$3; trap '\''rm -f -- "$temporary"'\'' EXIT; test -f "$temporary" && test ! -L "$temporary"; actual=$(sha256sum "$temporary" | cut -d" " -f1); test "$actual" = "$expected"; parent=${target%/*}; grand=${parent%/*}; if test -e "$grand"; then test -d "$grand" && test ! -L "$grand"; else mkdir -m 700 -- "$grand"; fi; if test -e "$parent"; then test -d "$parent" && test ! -L "$parent"; else mkdir -m 700 -- "$parent"; fi; chmod 500 -- "$temporary"; if ln -- "$temporary" "$target" 2>/dev/null; then :; else test -f "$target" && test ! -L "$target" && test "$(sha256sum "$target" | cut -d" " -f1)" = "$expected"; fi; chmod 500 -- "$target"; test "$(sha256sum "$target" | cut -d" " -f1)" = "$expected"' \
            sh "$remote_temporary" "$REMOTE_HELPER_PATH" "$REMOTE_HELPER_SHA"
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
        'helper=$1 expected=$2; shift 2; exec 9<"$helper"; test -f /proc/self/fd/9; actual=$(sha256sum /proc/self/fd/9 | cut -d" " -f1); test "$actual" = "$expected" || { printf "remote helper hash mismatch\n" >&2; exit 1; }; exec /proc/self/fd/9 "$@"' \
        sh "$REMOTE_HELPER_PATH" "$REMOTE_HELPER_SHA" "$@"
}

freeze_node_field() {
    local plan="$1" node="$2" field="$3"
    python3 - "$plan" "$node" "$field" <<'PY'
import json
import sys
value = json.load(open(sys.argv[1], encoding="utf-8"))
rows = [row for row in value["nodes"] if row.get("name") == sys.argv[2]]
if len(rows) != 1 or sys.argv[3] not in rows[0]:
    raise SystemExit("sealed writer field is missing or ambiguous")
answer = rows[0][sys.argv[3]]
if isinstance(answer, bool):
    print(str(answer).lower())
elif isinstance(answer, (str, int)):
    print(answer)
else:
    raise SystemExit("sealed writer field is not scalar")
PY
}

run_stopped_status_exact() {
    local freeze_plan="$1" freeze_sha="$2" capture_id="$3" node="$4"
    run_remote "$node" stopped-status "$capture_id" "$node" \
        "$freeze_sha" \
        "$(freeze_node_field "$freeze_plan" "$node" validator_address)" \
        "$(freeze_node_field "$freeze_plan" "$node" stake)" \
        "$(freeze_node_field "$freeze_plan" "$node" writer_pid)" \
        "$(freeze_node_field "$freeze_plan" "$node" writer_start_ticks)" \
        "$(freeze_node_field "$freeze_plan" "$node" boot_id)" \
        "$(freeze_node_field "$freeze_plan" "$node" supervisor_unit)" \
        "$(freeze_node_field "$freeze_plan" "$node" supervisor_main_pid)" \
        "$(freeze_node_field "$freeze_plan" "$node" executable_path)" \
        "$(freeze_node_field "$freeze_plan" "$node" executable_sha256)" \
        "$(freeze_node_field "$freeze_plan" "$node" argv_sha256)" \
        "$(freeze_node_field "$freeze_plan" "$node" data_dir)"
}

run_sealed_source_status_exact() {
    local freeze_plan="$1" freeze_sha="$2" capture_id="$3" node="$4"
    run_remote "$node" sealed-source-status "$capture_id" "$node" "$freeze_sha" \
        "$(freeze_node_field "$freeze_plan" "$node" validator_address)" \
        "$(freeze_node_field "$freeze_plan" "$node" stake)" \
        "$(freeze_node_field "$freeze_plan" "$node" writer_pid)" \
        "$(freeze_node_field "$freeze_plan" "$node" writer_start_ticks)" \
        "$(freeze_node_field "$freeze_plan" "$node" boot_id)" \
        "$(freeze_node_field "$freeze_plan" "$node" supervisor_unit)" \
        "$(freeze_node_field "$freeze_plan" "$node" supervisor_main_pid)" \
        "$(freeze_node_field "$freeze_plan" "$node" executable_path)" \
        "$(freeze_node_field "$freeze_plan" "$node" executable_sha256)" \
        "$(freeze_node_field "$freeze_plan" "$node" argv_sha256)" \
        "$(freeze_node_field "$freeze_plan" "$node" data_dir)"
}

remote_readiness() {
    local capture_id="$1" freeze_sha="$2" freeze_plan="$3"
    local node host pid start_ticks boot_id unit unit_main_pid exe_sha argv_sha data_dir
    local model_path model_sha model_size
    for node in nyc lax ams lhr nrt sgp; do
        host="$(host_for "$node")"
        pid="$(freeze_node_field "$freeze_plan" "$node" writer_pid)"
        start_ticks="$(freeze_node_field "$freeze_plan" "$node" writer_start_ticks)"
        boot_id="$(freeze_node_field "$freeze_plan" "$node" boot_id)"
        unit="$(freeze_node_field "$freeze_plan" "$node" supervisor_unit)"
        unit_main_pid="$(freeze_node_field "$freeze_plan" "$node" supervisor_main_pid)"
        exe_sha="$(freeze_node_field "$freeze_plan" "$node" executable_sha256)"
        argv_sha="$(freeze_node_field "$freeze_plan" "$node" argv_sha256)"
        data_dir="$(freeze_node_field "$freeze_plan" "$node" data_dir)"
        model_path="$(freeze_node_field "$freeze_plan" "$node" model_path)"
        model_sha="$(freeze_node_field "$freeze_plan" "$node" model_sha256)"
        model_size="$(freeze_node_field "$freeze_plan" "$node" model_size_bytes)"
        if ssh "${SSH_OPTIONS[@]}" "$SSH_USER@$host" -- sh -c \
            'set -eu; capture=$1 pid=$2 start=$3 boot=$4 unit=$5 main=$6 exe_sha=$7 argv_sha=$8 data=$9 model=${10} model_sha=${11} model_size=${12}; test "$(cat /proc/sys/kernel/random/boot_id)" = "$boot"; test -d "/proc/$pid"; test "$(awk '\''{print $22}'\'' "/proc/$pid/stat")" = "$start"; test "$(cat "/proc/$pid/comm")" = arc-node; test "$(pgrep -x arc-node)" = "$pid"; test "$(systemctl show "$unit" --property=MainPID --value)" = "$main"; test "$(sha256sum "/proc/$pid/exe" | cut -d" " -f1)" = "$exe_sha"; test "$(sha256sum "/proc/$pid/cmdline" | cut -d" " -f1)" = "$argv_sha"; test -d "$data" && test ! -L "$data" && test -s "$data/state.wal"; test -f "$model" && test ! -L "$model"; test "$(stat -c %s "$model")" = "$model_size"; test "$(sha256sum "$model" | cut -d" " -f1)" = "$model_sha"; command -v curl >/dev/null; command -v python3 >/dev/null; command -v sha256sum >/dev/null; command -v zstd >/dev/null; command -v tar >/dev/null; command -v systemctl >/dev/null; test ! -e /root/arc-recovery-captures || { test -d /root/arc-recovery-captures && test ! -L /root/arc-recovery-captures; }; { test ! -e "$capture" || { test -d "$capture" && test ! -L "$capture"; }; }; bytes=$(du -s -B1 "$data" | cut -f1); files=$(find "$data" -type f | wc -l); wal_bytes=$(stat -c %s "$data/state.wal"); snapshot_bytes=0; for snapshot in "$data/state.snapshot.lz4" "$data.snapshot.lz4"; do if test -f "$snapshot" && test ! -L "$snapshot"; then snapshot_bytes=$((snapshot_bytes + $(stat -c %s "$snapshot"))); fi; done; binding_bytes=$((wal_bytes + snapshot_bytes)); test "$binding_bytes" -ge "$bytes" || binding_bytes=$bytes; binding_bytes=$((binding_bytes + 2147483648)); required_bytes=$((bytes + binding_bytes)); required_inodes=$((files + 10000)); free_bytes=$(df -PB1 /root | awk '\''NR==2 {print $4}'\''); free_inodes=$(df -Pi /root | awk '\''NR==2 {print $4}'\''); test "$free_bytes" -ge "$required_bytes" || { printf "insufficient recovery bytes including v3 headroom: need=%s free=%s\n" "$required_bytes" "$free_bytes" >&2; exit 1; }; test "$free_inodes" -ge "$required_inodes" || { printf "insufficient recovery inodes including v3 headroom: need=%s free=%s\n" "$required_inodes" "$free_inodes" >&2; exit 1; }' \
            sh "/root/arc-recovery-captures/$capture_id/$node" "$pid" "$start_ticks" \
            "$boot_id" "$unit" "$unit_main_pid" "$exe_sha" "$argv_sha" "$data_dir" \
            "$model_path" "$model_sha" "$model_size" >/dev/null 2>&1; then
            printf '  exact live writer/disk ready: %s %s pid=%s data=%s\n' "$node" "$host" "$pid" "$data_dir"
            continue
        fi
        run_stopped_status_exact "$freeze_plan" "$freeze_sha" "$capture_id" "$node" >/dev/null || \
            die "$node is neither the exact sealed live writer nor an exact persistently fenced stop"
        local readiness_state=stopped
        if run_remote "$node" status "$capture_id" "$node" >/dev/null 2>&1; then
            readiness_state=captured
        fi
        ssh "${SSH_OPTIONS[@]}" "$SSH_USER@$host" -- sh -c \
            'set -eu; data=$1 model=$2 model_sha=$3 model_size=$4; ! pgrep -x arc-node >/dev/null 2>&1; test -d "$data" && test ! -L "$data" && test -s "$data/state.wal"; test -f "$model" && test ! -L "$model"; test "$(stat -c %s "$model")" = "$model_size"; test "$(sha256sum "$model" | cut -d" " -f1)" = "$model_sha"; bytes=$(du -s -B1 "$data" | cut -f1); files=$(find "$data" -type f | wc -l); wal_bytes=$(stat -c %s "$data/state.wal"); snapshot_bytes=0; for snapshot in "$data/state.snapshot.lz4" "$data.snapshot.lz4"; do if test -f "$snapshot" && test ! -L "$snapshot"; then snapshot_bytes=$((snapshot_bytes + $(stat -c %s "$snapshot"))); fi; done; binding_bytes=$((wal_bytes + snapshot_bytes)); test "$binding_bytes" -ge "$bytes" || binding_bytes=$bytes; binding_bytes=$((binding_bytes + 2147483648)); required_bytes=$((bytes + binding_bytes)); required_inodes=$((files + 10000)); free_bytes=$(df -PB1 /root | awk '\''NR==2 {print $4}'\''); free_inodes=$(df -Pi /root | awk '\''NR==2 {print $4}'\''); test "$free_bytes" -ge "$required_bytes"; test "$free_inodes" -ge "$required_inodes"' \
            sh "$data_dir" "$model_path" "$model_sha" "$model_size"
        printf '  exact %s stop/content and disk ready: %s %s data=%s\n' \
            "$readiness_state" "$node" "$host" "$data_dir"
    done
}

ensure_stopped() {
    local freeze_plan="$1" freeze_sha="$2" capture_id="$3" node="$4"
    if run_stopped_status_exact "$freeze_plan" "$freeze_sha" "$capture_id" "$node" >/dev/null 2>&1; then
        run_stopped_status_exact "$freeze_plan" "$freeze_sha" "$capture_id" "$node"
        return 0
    fi
    run_remote "$node" fence-stop "$capture_id" "$node" "$freeze_sha" \
        "$(freeze_node_field "$freeze_plan" "$node" validator_address)" \
        "$(freeze_node_field "$freeze_plan" "$node" stake)" \
        "$(freeze_node_field "$freeze_plan" "$node" writer_pid)" \
        "$(freeze_node_field "$freeze_plan" "$node" writer_start_ticks)" \
        "$(freeze_node_field "$freeze_plan" "$node" boot_id)" \
        "$(freeze_node_field "$freeze_plan" "$node" supervisor_unit)" \
        "$(freeze_node_field "$freeze_plan" "$node" supervisor_main_pid)" \
        "$(freeze_node_field "$freeze_plan" "$node" executable_path)" \
        "$(freeze_node_field "$freeze_plan" "$node" executable_sha256)" \
        "$(freeze_node_field "$freeze_plan" "$node" argv_sha256)" \
        "$(freeze_node_field "$freeze_plan" "$node" data_dir)"
    run_stopped_status_exact "$freeze_plan" "$freeze_sha" "$capture_id" "$node"
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
    printf '  first stops: %s (no global halt claim)\n' "${SENTINELS[*]}"
    printf '  remaining: AMS LHR NRT SGP; sealed-source quorum is unavailable only after all six exact writer stops\n'
    REMOTE_HELPER_SHA="$(manifest_field "$freeze_plan" remote_helper_sha256)"
    require_hash "$REMOTE_HELPER_SHA" "sealed remote helper hash"
    REMOTE_HELPER_PATH="/root/.arc-recovery-helpers/$REMOTE_HELPER_SHA/archive-node.sh"
    remote_readiness "$capture_id" "$freeze_sha" "$freeze_plan"
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
    ensure_stopped "$freeze_plan" "$freeze_sha" "$capture_id" nyc
    printf 'archive fleet: persistently fencing and stopping second sentinel LAX\n'
    ensure_stopped "$freeze_plan" "$freeze_sha" "$capture_id" lax
    run_stopped_status_exact "$freeze_plan" "$freeze_sha" "$capture_id" nyc >/dev/null
    run_stopped_status_exact "$freeze_plan" "$freeze_sha" "$capture_id" lax >/dev/null
    printf 'archive fleet: first two controlled writers stopped; external vulnerable legacy forks remain untrusted and are not claimed halted\n'

    local log_root node
    log_root="$(mktemp -d)"
    ARCHIVE_FLEET_TEMP_ROOT="$log_root"
    trap cleanup_temporary_root EXIT
    local pids=() names=()
    for node in "${REMAINING[@]}"; do
        (
            ensure_stopped "$freeze_plan" "$freeze_sha" "$capture_id" "$node"
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
        run_stopped_status_exact "$freeze_plan" "$freeze_sha" "$capture_id" "$node" >/dev/null
    done
    [ "$(manifest_field "$freeze_plan" quorum_proof.controlled_quorum_unavailable_after_all_stops)" = true ] || \
        die "sealed freeze proof does not remove controlled source quorum"
    [ "$(manifest_field "$freeze_plan" quorum_proof.global_legacy_halt_claimed)" = false ] || \
        die "freeze plan impermissibly claims a global legacy halt"
    printf 'archive fleet: ALL SIX CONTROLLED WRITERS HALTED; sealed 40M source set has at most %s unstopped stake (< quorum %s). External dynamic identities remain untrusted forks; no global halt is claimed.\n' \
        "$(manifest_field "$freeze_plan" quorum_proof.maximum_source_stake_after_controlled_stop)" \
        "$(manifest_field "$freeze_plan" quorum_proof.source_quorum_stake)"
    printf 'archive fleet: beginning offline all-six exact data-directory copies\n'

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
rr.require_prearchive_manifest(manifest)
rr.verify_artifacts(manifest)
rr.RecoveryRollout(manifest, digest).verify_checkpoint()
freeze = json.loads(pathlib.Path(sys.argv[2]).read_text(encoding="utf-8"))
freeze_sha, capture_id = sys.argv[3:5]
captured = sorted((entry["name"], entry["host"]) for entry in freeze["nodes"])
rollout = sorted((entry["name"], entry["host"]) for entry in manifest["validators"])
if captured != rollout:
    rr.fail("rollout validator names/hosts differ from the sealed freeze plan")
archive = manifest["archive"]
if (archive["freeze_plan_sha256"] != freeze_sha
        or archive["capture_id"] != capture_id):
    rr.fail("rollout archive binding differs from the exact freeze plan and capture id")
for archive_field, freeze_field in (
    ("archive_orchestrator_sha256", "orchestrator_sha256"),
    ("remote_helper_sha256", "remote_helper_sha256"),
    ("rollout_tool_sha256", "rollout_tool_sha256"),
    ("rollout_schema_sha256", "rollout_schema_sha256"),
):
    if archive[archive_field] != freeze[freeze_field]:
        rr.fail(f"rollout {archive_field} differs from the sealed freeze provenance")
captured_runtime = {
    entry["name"]: {
        "model_path": entry["model_path"],
        "model_sha256": entry["model_sha256"],
        "model_size_bytes": entry["model_size_bytes"],
        "shard_ranges": entry["shard_ranges"],
    }
    for entry in freeze["nodes"]
}
rollout_runtime = {
    entry["name"]: {
        "model_path": entry["model_path"],
        "model_sha256": entry["model_sha256"],
        "model_size_bytes": entry["model_size_bytes"],
        "shard_ranges": entry["shard_ranges"],
    }
    for entry in manifest["validators"]
}
if captured_runtime != rollout_runtime:
    rr.fail("rollout model bytes/path or per-node shard arguments differ from the sealed live inventory")
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

hash_size_stream() {
    python3 -c '
import hashlib, sys
digest = hashlib.sha256(); size = 0
for chunk in iter(lambda: sys.stdin.buffer.read(1024 * 1024), b""):
    digest.update(chunk); size += len(chunk)
print(digest.hexdigest(), size)
'
}

forward_hash_size_stream() {
    local output="$1"
    python3 -c '
import hashlib, pathlib, sys
output = pathlib.Path(sys.argv[1]); digest = hashlib.sha256(); size = 0
for chunk in iter(lambda: sys.stdin.buffer.read(1024 * 1024), b""):
    digest.update(chunk); size += len(chunk); sys.stdout.buffer.write(chunk)
sys.stdout.buffer.flush()
output.write_text(f"{digest.hexdigest()} {size}\n", encoding="ascii")
' "$output"
}

stream_bundle_to_drive() {
    local node="$1" capture_id="$2" manifest_sha="$3" destination="$4" work_root="$5"
    local archive_name="legacy-$node.tar.zst"
    local archive_remote="$destination/$archive_name"
    local inventory="$work_root/legacy-$node.inventory"
    local inventory_sidecar="$inventory.sha256"
    local archive_sidecar="$work_root/$archive_name.sha256"
    local status="$work_root/$node-bundle-status.json"
    run_remote "$node" stream-inventory "$capture_id" "$node" "$manifest_sha" > "$inventory"
    chmod 400 -- "$inventory"

    local classification
    classification="$(sed -n 's/^classification=//p' "$inventory")"
    case "$classification" in
        valid_canonical|valid_noncanonical_fork|preserved_unclassified) ;;
        *) die "remote stream inventory classification is invalid for $node" ;;
    esac

    # Staging is a sibling of the exact capture destination. Interrupted,
    # unpredictable objects are never accepted as archive members and are not
    # guessed at or deleted by a retry (which could race another authorized run).
    local partial_root="${destination%/*}/.arc-recovery-partials/$capture_id/$manifest_sha"

    local source_hash_size remote_hash_size
    if rclone cat "$archive_remote" 2>/dev/null | hash_size_stream > "$work_root/$node-existing.hash-size" && \
            [ -s "$work_root/$node-existing.hash-size" ]; then
        remote_hash_size="$(cat "$work_root/$node-existing.hash-size")"
        source_hash_size="$(run_remote "$node" stream-bundle "$capture_id" "$node" "$manifest_sha" | hash_size_stream)"
        [ "$remote_hash_size" = "$source_hash_size" ] || \
            die "existing Drive bundle differs from the exact deterministic fenced source stream: $node"
    else
        local token partial_remote pipeline_status
        token="$(python3 -c 'import secrets; print(secrets.token_hex(32))')"
        partial_remote="$partial_root/legacy-$node.$token.tar.zst"
        set +e
        run_remote "$node" stream-bundle "$capture_id" "$node" "$manifest_sha" | \
            forward_hash_size_stream "$work_root/$node-upload.hash-size" | \
            rclone rcat "$partial_remote" --metadata --streaming-upload-cutoff 1M
        pipeline_status=("${PIPESTATUS[@]}")
        set -e
        if [ "${pipeline_status[0]}" -ne 0 ] || [ "${pipeline_status[1]}" -ne 0 ] || \
                [ "${pipeline_status[2]}" -ne 0 ] || [ ! -s "$work_root/$node-upload.hash-size" ]; then
            rclone deletefile "$partial_remote" >/dev/null 2>&1 || true
            die "streaming archive upload failed before immutable publication: $node"
        fi
        source_hash_size="$(cat "$work_root/$node-upload.hash-size")"
        remote_hash_size="$(rclone cat "$partial_remote" | hash_size_stream)"
        [ "$remote_hash_size" = "$source_hash_size" ] || {
            rclone deletefile "$partial_remote" >/dev/null 2>&1 || true
            die "Drive partial differs from exact streamed bytes: $node"
        }
        rclone moveto "$partial_remote" "$archive_remote" --immutable --checksum --metadata
        remote_hash_size="$(rclone cat "$archive_remote" | hash_size_stream)"
        [ "$remote_hash_size" = "$source_hash_size" ] || \
            die "published Drive bundle differs after server-side move: $node"
    fi

    local archive_sha archive_size inventory_sha
    read -r archive_sha archive_size <<< "$remote_hash_size"
    require_hash "$archive_sha" "streamed bundle hash"
    require_uint "$archive_size" "streamed bundle size"
    [ "$archive_size" -gt 0 ] || die "streamed bundle is empty: $node"
    printf '%s  %s\n' "$archive_sha" "$archive_name" > "$archive_sidecar"
    inventory_sha="$(hash_file "$inventory")"
    printf '%s  %s\n' "$inventory_sha" "${inventory##*/}" > "$inventory_sidecar"
    chmod 400 -- "$archive_sidecar" "$inventory_sidecar"
    upload_immutable "$archive_sidecar" "$destination/${archive_sidecar##*/}"
    upload_immutable "$inventory" "$destination/${inventory##*/}"
    upload_immutable "$inventory_sidecar" "$destination/${inventory_sidecar##*/}"

    python3 - "$status" "$capture_id" "$node" "$manifest_sha" "$classification" \
        "$archive_name" "$archive_size" "$archive_sha" "${archive_sidecar##*/}" \
        "$(hash_file "$archive_sidecar")" "${inventory##*/}" "$(stat -f %z "$inventory" 2>/dev/null || stat -c %s "$inventory")" \
        "$inventory_sha" "${inventory_sidecar##*/}" "$(hash_file "$inventory_sidecar")" <<'PY'
import json
import pathlib
import sys
(output, capture, node, manifest, classification, bundle_name, bundle_size,
 bundle_sha, bundle_sidecar, bundle_sidecar_sha, inventory_name, inventory_size,
 inventory_sha, inventory_sidecar, inventory_sidecar_sha) = sys.argv[1:]
value = {
    "schema": "arc.recovery.bundle-status.v1",
    "capture_id": capture,
    "node": node,
    "rollout_manifest_sha256": manifest,
    "classification": classification,
    "bundle": {"name": bundle_name, "size": int(bundle_size), "sha256": bundle_sha,
               "sidecar_name": bundle_sidecar, "sidecar_sha256": bundle_sidecar_sha},
    "inventory": {"name": inventory_name, "size": int(inventory_size), "sha256": inventory_sha,
                  "sidecar_name": inventory_sidecar, "sidecar_sha256": inventory_sidecar_sha},
}
pathlib.Path(output).write_text(json.dumps(value, sort_keys=True, separators=(",", ":")) + "\n", encoding="utf-8")
PY
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

create_canonical_reference() {
    local output="$1" shared_root="$2" allow_unbound="$3"
    local source_height="$4" source_hash="$5" source_state_root="$6"
    local transition_state_root="$7" checkpoint_manifest="$8" source_round="$9"
    local created_at="${10}" recovery_epoch="${11}" validator_set_id="${12}"
    shift 12
    python3 - "$output" "$shared_root" "$allow_unbound" "$source_height" \
        "$source_hash" "$source_state_root" "$transition_state_root" \
        "$checkpoint_manifest" "$source_round" "$created_at" "$recovery_epoch" \
        "$validator_set_id" "$@" <<'PY'
import hashlib
import json
import os
import pathlib
import re
import sys

(output_raw, shared_raw, allow_unbound_raw, source_height_raw, source_hash_raw,
 source_state_root_raw, transition_state_root_raw, checkpoint_manifest_raw,
 source_round_raw, created_at_raw, recovery_epoch_raw, validator_set_id_raw,
 binary_sha, genesis_sha, validators_sha, legacy_validators_sha,
 snapshot_sha, wal_sha, checkpoint_sha) = sys.argv[1:]
output = pathlib.Path(output_raw)
shared = pathlib.Path(shared_raw)
hash_re = re.compile(r"[0-9a-f]{64}")

def digest(path):
    value = hashlib.sha256()
    with path.open("rb") as handle:
        for chunk in iter(lambda: handle.read(1024 * 1024), b""):
            value.update(chunk)
    return value.hexdigest()

def artifact(name, expected):
    path = shared / name
    if path.is_symlink() or not path.is_file():
        raise SystemExit(f"canonical reference input is missing or unsafe: {name}")
    actual = digest(path)
    if not hash_re.fullmatch(expected) or actual != expected:
        raise SystemExit(f"canonical reference input hash differs: {name}")
    return {"name": name, "size": path.stat().st_size, "sha256": actual}

def bare(value):
    value = value.removeprefix("0x")
    if not hash_re.fullmatch(value):
        raise SystemExit("canonical reference checkpoint hash is malformed")
    return value

if allow_unbound_raw not in {"true", "false"}:
    raise SystemExit("canonical reference legacy-WAL policy is malformed")
reference = {
    "schema": "arc.recovery.canonical-reference.v1",
    "independently_verified": True,
    "allow_unbound_legacy_wal": allow_unbound_raw == "true",
    "verifier_binary": artifact("arc-node", binary_sha),
    "genesis": artifact("genesis.toml", genesis_sha),
    "validator_public_keys": artifact("validator-public-keys.json", validators_sha),
    "legacy_validator_set": artifact("legacy-validator-set-40m.json", legacy_validators_sha),
    "source_snapshot": artifact("source.snapshot.lz4", snapshot_sha),
    "source_wal": artifact("source.state.wal", wal_sha),
    "selected_checkpoint": artifact("recovery.arcchkpt", checkpoint_sha),
    "source_height": int(source_height_raw),
    "source_block_hash": bare(source_hash_raw),
    "source_state_root": bare(source_state_root_raw),
    "transition_state_root": bare(transition_state_root_raw),
    "checkpoint_manifest_hash": bare(checkpoint_manifest_raw),
    "source_consensus_round": int(source_round_raw),
    "created_at_unix_ms": int(created_at_raw),
    "recovery_epoch": int(recovery_epoch_raw),
    "validator_set_id": int(validator_set_id_raw),
}
payload = (json.dumps(reference, sort_keys=True, separators=(",", ":")) + "\n").encode()
fd = os.open(output, os.O_WRONLY | os.O_CREAT | os.O_EXCL | getattr(os, "O_NOFOLLOW", 0), 0o400)
with os.fdopen(fd, "wb") as handle:
    handle.write(payload)
    handle.flush()
    os.fsync(handle.fileno())
directory_fd = os.open(output.parent, os.O_RDONLY)
try:
    os.fsync(directory_fd)
finally:
    os.close(directory_fd)
PY
}

build_archive_metadata() {
    local shared_root="$1" statuses="$2" metadata_root="$3" complete_root="$4"
    local freeze_sha="$5" capture_id="$6" manifest_sha="$7" source_commit="$8"
    local orchestrator_sha="$9" helper_sha="${10}"
    local rollout_tool_sha="${11}" schema_sha="${12}"
    local canonical_count="${13}" fork_count="${14}" unclassified_count="${15}"
    mkdir -p -- "$metadata_root" "$complete_root"
    python3 - "$shared_root" "$statuses" "$metadata_root/SHA256SUMS" \
        "$metadata_root/ARCHIVE-MANIFEST.json" \
        "$metadata_root/ARCHIVE-MANIFEST.json.sha256" \
        "$complete_root/COMPLETE.json" "$freeze_sha" "$capture_id" \
        "$manifest_sha" "$source_commit" "$orchestrator_sha" "$helper_sha" \
        "$rollout_tool_sha" "$schema_sha" \
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
 orchestrator_sha, helper_sha, rollout_tool_sha, schema_sha, canonical_raw, fork_raw,
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
    ("remote helper", helper_sha), ("rollout tool", rollout_tool_sha),
    ("rollout schema", schema_sha),
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

shared_by_name = {item["name"]: item for item in shared_inputs}
for expected, name in (
    (orchestrator_sha, "archive-fleet-to-drive.sh"),
    (helper_sha, "archive-node.sh"),
    (rollout_tool_sha, "recovery_rollout.py"),
    (schema_sha, "recovery-manifest.schema.json"),
):
    item = shared_by_name.get(name)
    if item is None or item["sha256"] != expected:
        raise SystemExit(f"archive provenance differs from shared object bytes: {name}")
reference_path = shared_root / "canonical-reference.json"
try:
    canonical_reference = json.loads(reference_path.read_text(encoding="utf-8"))
except (OSError, json.JSONDecodeError) as error:
    raise SystemExit(f"canonical reference evidence is unreadable: {error}")
if reference_path.read_bytes() != canonical(canonical_reference):
    raise SystemExit("canonical reference evidence is not canonical JSON")
reference_keys = {
    "schema", "independently_verified", "allow_unbound_legacy_wal",
    "verifier_binary", "genesis", "validator_public_keys",
    "legacy_validator_set", "source_snapshot", "source_wal",
    "selected_checkpoint", "source_height", "source_block_hash",
    "source_state_root", "transition_state_root", "checkpoint_manifest_hash",
    "source_consensus_round", "created_at_unix_ms", "recovery_epoch",
    "validator_set_id",
}
if (not isinstance(canonical_reference, dict)
        or set(canonical_reference) != reference_keys
        or canonical_reference.get("schema") != "arc.recovery.canonical-reference.v1"
        or canonical_reference.get("independently_verified") is not True
        or not isinstance(canonical_reference.get("allow_unbound_legacy_wal"), bool)):
    raise SystemExit("canonical reference evidence has missing, unknown, or unsupported fields")
reference_objects = {
    "verifier_binary": "arc-node",
    "genesis": "genesis.toml",
    "validator_public_keys": "validator-public-keys.json",
    "legacy_validator_set": "legacy-validator-set-40m.json",
    "source_snapshot": "source.snapshot.lz4",
    "source_wal": "source.state.wal",
    "selected_checkpoint": "recovery.arcchkpt",
}
for field, name in reference_objects.items():
    if canonical_reference[field] != shared_by_name.get(name):
        raise SystemExit(f"canonical reference {field} differs from the archived object bytes")
reference_payload = canonical(canonical_reference)
reference_entry = shared_by_name.get("canonical-reference.json")
if reference_entry != {
    "name": "canonical-reference.json",
    "size": len(reference_payload),
    "sha256": hashlib.sha256(reference_payload).hexdigest(),
}:
    raise SystemExit("canonical reference object does not bind its manifest projection")
options_payload = canonical({
    "allow_unbound_legacy_wal": canonical_reference["allow_unbound_legacy_wal"]
})
options_entry = shared_by_name.get("archive-seal-options.json")
if options_entry != {
    "name": "archive-seal-options.json",
    "size": len(options_payload),
    "sha256": hashlib.sha256(options_payload).hexdigest(),
}:
    raise SystemExit("canonical reference legacy-WAL policy differs from archive seal options")
for field in (
    "source_block_hash", "source_state_root", "transition_state_root",
    "checkpoint_manifest_hash",
):
    if not isinstance(canonical_reference[field], str) or not hash_re.fullmatch(canonical_reference[field]):
        raise SystemExit(f"canonical reference {field} is malformed")
for field in (
    "source_height", "source_consensus_round", "created_at_unix_ms",
    "recovery_epoch", "validator_set_id",
):
    value = canonical_reference[field]
    if isinstance(value, bool) or not isinstance(value, int) or value < 0:
        raise SystemExit(f"canonical reference {field} is malformed")

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
    "schema": "arc.recovery.archive-manifest.v2",
    "freeze_plan_sha256": freeze_sha,
    "capture_id": capture_id,
    "rollout_manifest_sha256": rollout_sha,
    "source_commit": source_commit,
    "orchestrator_sha256": orchestrator_sha,
    "remote_helper_sha256": helper_sha,
    "rollout_tool_sha256": rollout_tool_sha,
    "rollout_schema_sha256": schema_sha,
    "canonical_reference": canonical_reference,
    "capture_classification_counts": counts,
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
    local expected_complete_sha="${5:-}" expected_manifest_sha="${6:-}" expected_sums_sha="${7:-}"
    local expected_prearchive_sha="${8:-}"
    local temporary
    temporary="$(mktemp -d)"
    trap 'rm -rf -- "$temporary"' EXIT
    rclone cat "$destination/COMPLETE.json" > "$temporary/COMPLETE.json" || \
        die "archive destination has no readable COMPLETE.json"
    rclone cat "$destination/ARCHIVE-MANIFEST.json" > "$temporary/ARCHIVE-MANIFEST.json" || \
        die "archive destination has no readable archive manifest"
    rclone cat "$destination/ARCHIVE-MANIFEST.json.sha256" > "$temporary/ARCHIVE-MANIFEST.json.sha256" || \
        die "archive destination has no readable archive manifest sidecar"
    rclone cat "$destination/SHA256SUMS" > "$temporary/SHA256SUMS" || \
        die "archive destination has no readable SHA256SUMS"
    if [ -n "$expected_complete" ]; then
        cmp --silent "$expected_complete" "$temporary/COMPLETE.json" || \
            die "existing COMPLETE.json differs from this sealed archive"
        cmp --silent "$expected_manifest" "$temporary/ARCHIVE-MANIFEST.json" || \
            die "remote archive manifest differs from this sealed archive"
        cmp --silent "$expected_sidecar" "$temporary/ARCHIVE-MANIFEST.json.sha256" || \
            die "remote archive manifest sidecar differs from this sealed archive"
    fi
    python3 - "$temporary/COMPLETE.json" "$temporary/ARCHIVE-MANIFEST.json" \
        "$temporary/ARCHIVE-MANIFEST.json.sha256" "$temporary/SHA256SUMS" \
        "$temporary/objects.tsv" "$temporary/expected-names" "$temporary/manifest-sha" \
        "$expected_complete_sha" "$expected_manifest_sha" "$expected_sums_sha" \
        "$expected_prearchive_sha" <<'PY'
import hashlib
import json
import pathlib
import re
import sys

complete_path, manifest_path, sidecar_path, sums_path, objects_path, names_path, manifest_sha_path = map(pathlib.Path, sys.argv[1:8])
expected_complete_sha, expected_manifest_sha, expected_sums_sha, expected_prearchive_sha = sys.argv[8:12]
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
complete_sha = hashlib.sha256(complete_path.read_bytes()).hexdigest()
sums_sha = hashlib.sha256(sums_path.read_bytes()).hexdigest()
for label, expected, actual in (
    ("COMPLETE", expected_complete_sha, complete_sha),
    ("archive manifest", expected_manifest_sha, manifest_sha),
    ("SHA256SUMS", expected_sums_sha, sums_sha),
):
    if expected and expected != actual:
        raise SystemExit(f"{label} sha256 differs from the independently sealed rollout root")
if complete["archive_manifest_sha256"] != manifest_sha:
    raise SystemExit("COMPLETE.json does not bind the archive manifest bytes")
if sidecar_path.read_text(encoding="ascii") != f"{manifest_sha}  ARCHIVE-MANIFEST.json\n":
    raise SystemExit("archive manifest checksum sidecar differs")
manifest_keys = {
    "schema", "freeze_plan_sha256", "capture_id", "rollout_manifest_sha256",
    "source_commit", "orchestrator_sha256", "remote_helper_sha256",
    "rollout_tool_sha256", "rollout_schema_sha256",
    "canonical_reference", "capture_classification_counts", "shared_inputs",
    "validator_bundles", "sha256sums",
}
if set(manifest) != manifest_keys or manifest.get("schema") != "arc.recovery.archive-manifest.v2":
    raise SystemExit("archive manifest has missing, unknown, or unsupported fields")
for field in ("freeze_plan_sha256", "capture_id", "rollout_manifest_sha256", "source_commit"):
    if manifest.get(field) != complete[field]:
        raise SystemExit(f"COMPLETE.json {field} differs from archive manifest")
if expected_prearchive_sha and manifest.get("rollout_manifest_sha256") != expected_prearchive_sha:
    raise SystemExit("archive manifest differs from the sealed prearchive rollout digest")
bundles = manifest.get("validator_bundles")
nodes = ("nyc", "lax", "ams", "lhr", "nrt", "sgp")
if not isinstance(bundles, list) or [row.get("node") for row in bundles] != list(nodes):
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

hash_re = re.compile(r"[0-9a-f]{64}")
name_re = re.compile(r"[A-Za-z0-9][A-Za-z0-9._-]{0,127}")
objects = {}
shared = manifest["shared_inputs"]
if not isinstance(shared, list):
    raise SystemExit("archive shared_inputs is not an array")
for item in shared:
    if not isinstance(item, dict) or set(item) != {"name", "size", "sha256"}:
        raise SystemExit("shared archive item fields are not exact")
    name, size, digest = item["name"], item["size"], item["sha256"]
    if not isinstance(name, str) or not name_re.fullmatch(name):
        raise SystemExit("shared archive item name is unsafe")
    if isinstance(size, bool) or not isinstance(size, int) or size <= 0 or not isinstance(digest, str) or not hash_re.fullmatch(digest):
        raise SystemExit("shared archive item size/hash is malformed")
    if name in objects:
        raise SystemExit("duplicate archive object name")
    objects[name] = (digest, size)

for field, name in (
    ("orchestrator_sha256", "archive-fleet-to-drive.sh"),
    ("remote_helper_sha256", "archive-node.sh"),
    ("rollout_tool_sha256", "recovery_rollout.py"),
    ("rollout_schema_sha256", "recovery-manifest.schema.json"),
):
    item = objects.get(name)
    if item is None or manifest[field] != item[0]:
        raise SystemExit(f"archive provenance {field} differs from the shared object bytes")

reference = manifest["canonical_reference"]
reference_keys = {
    "schema", "independently_verified", "allow_unbound_legacy_wal",
    "verifier_binary", "genesis", "validator_public_keys",
    "legacy_validator_set", "source_snapshot", "source_wal",
    "selected_checkpoint", "source_height", "source_block_hash",
    "source_state_root", "transition_state_root", "checkpoint_manifest_hash",
    "source_consensus_round", "created_at_unix_ms", "recovery_epoch",
    "validator_set_id",
}
if (not isinstance(reference, dict)
        or set(reference) != reference_keys
        or reference.get("schema") != "arc.recovery.canonical-reference.v1"
        or reference.get("independently_verified") is not True
        or not isinstance(reference.get("allow_unbound_legacy_wal"), bool)):
    raise SystemExit("canonical reference has missing, unknown, or unsupported fields")
reference_objects = {
    "verifier_binary": "arc-node",
    "genesis": "genesis.toml",
    "validator_public_keys": "validator-public-keys.json",
    "legacy_validator_set": "legacy-validator-set-40m.json",
    "source_snapshot": "source.snapshot.lz4",
    "source_wal": "source.state.wal",
    "selected_checkpoint": "recovery.arcchkpt",
}
for field, name in reference_objects.items():
    item = reference[field]
    if (not isinstance(item, dict)
            or set(item) != {"name", "size", "sha256"}
            or item.get("name") != name
            or objects.get(name) != (item.get("sha256"), item.get("size"))):
        raise SystemExit(f"canonical reference {field} differs from the archived object bytes")
reference_payload = canonical(reference)
if objects.get("canonical-reference.json") != (
    hashlib.sha256(reference_payload).hexdigest(), len(reference_payload)
):
    raise SystemExit("canonical-reference.json differs from the archive-manifest projection")
options_payload = canonical({
    "allow_unbound_legacy_wal": reference["allow_unbound_legacy_wal"]
})
if objects.get("archive-seal-options.json") != (
    hashlib.sha256(options_payload).hexdigest(), len(options_payload)
):
    raise SystemExit("canonical reference legacy-WAL policy differs from archive seal options")
for field in (
    "source_block_hash", "source_state_root", "transition_state_root",
    "checkpoint_manifest_hash",
):
    if not isinstance(reference[field], str) or not hash_re.fullmatch(reference[field]):
        raise SystemExit(f"canonical reference {field} is malformed")
for field in (
    "source_height", "source_consensus_round", "created_at_unix_ms",
    "recovery_epoch", "validator_set_id",
):
    value = reference[field]
    if isinstance(value, bool) or not isinstance(value, int) or value < 0:
        raise SystemExit(f"canonical reference {field} is malformed")

allowed_classifications = {"valid_canonical", "valid_noncanonical_fork", "preserved_unclassified"}
observed_counts = {key: 0 for key in allowed_classifications}
for node, row in zip(nodes, bundles):
    if not isinstance(row, dict) or set(row) != {"node", "classification", "bundle", "inventory"}:
        raise SystemExit("validator bundle fields are not exact")
    if row["node"] != node or row["classification"] not in allowed_classifications:
        raise SystemExit("validator bundle identity/classification is invalid")
    observed_counts[row["classification"]] += 1
    for label, suffix in (("bundle", ".tar.zst"), ("inventory", ".inventory")):
        item = row[label]
        if not isinstance(item, dict) or set(item) != {"name", "size", "sha256", "sidecar_name", "sidecar_sha256"}:
            raise SystemExit("bundle object fields are not exact")
        expected_name = f"legacy-{node}{suffix}"
        if item["name"] != expected_name or item["sidecar_name"] != expected_name + ".sha256":
            raise SystemExit("bundle object name is noncanonical")
        if isinstance(item["size"], bool) or not isinstance(item["size"], int) or item["size"] <= 0:
            raise SystemExit("bundle object size is invalid")
        if not hash_re.fullmatch(item["sha256"]) or not hash_re.fullmatch(item["sidecar_sha256"]):
            raise SystemExit("bundle object hash is malformed")
        sidecar_size = len(f"{item['sha256']}  {item['name']}\n".encode())
        for name, digest, size in (
            (item["name"], item["sha256"], item["size"]),
            (item["sidecar_name"], item["sidecar_sha256"], sidecar_size),
        ):
            if name in objects:
                raise SystemExit("duplicate archive object name")
            objects[name] = (digest, size)
if manifest["capture_classification_counts"] != observed_counts:
    raise SystemExit("archive classification counts differ from bundle rows")

lines = sums_path.read_text(encoding="ascii").splitlines()
sums = {}
for line in lines:
    match = re.fullmatch(r"([0-9a-f]{64})  ([A-Za-z0-9][A-Za-z0-9._-]{0,127})", line)
    if not match or match.group(2) in sums:
        raise SystemExit("SHA256SUMS has a malformed or duplicate row")
    sums[match.group(2)] = match.group(1)
if sums != {name: value[0] for name, value in objects.items()}:
    raise SystemExit("SHA256SUMS does not exactly cover every shared/bundle object")
sums_entry = manifest["sha256sums"]
if sums_entry != {"name": "SHA256SUMS", "size": sums_path.stat().st_size, "sha256": sums_sha}:
    raise SystemExit("archive manifest does not exactly bind SHA256SUMS")

sidecar_sha = hashlib.sha256(sidecar_path.read_bytes()).hexdigest()
metadata = {
    "SHA256SUMS": (sums_sha, sums_path.stat().st_size),
    "ARCHIVE-MANIFEST.json": (manifest_sha, manifest_path.stat().st_size),
    "ARCHIVE-MANIFEST.json.sha256": (sidecar_sha, sidecar_path.stat().st_size),
    "COMPLETE.json": (complete_sha, complete_path.stat().st_size),
}
all_names = sorted(set(objects) | set(metadata))
if len(all_names) != complete["object_count_before_complete"] + 1:
    raise SystemExit("remote object cardinality differs from COMPLETE")
objects_path.write_text(
    "".join(f"{name}\t{digest}\t{size}\n" for name, (digest, size) in sorted(objects.items())),
    encoding="utf-8",
)
names_path.write_text("".join(f"{name}\n" for name in all_names), encoding="utf-8")
manifest_sha_path.write_text(manifest_sha + "\n", encoding="ascii")
PY
    local name expected_sha expected_size actual
    while IFS=$'\t' read -r name expected_sha expected_size; do
        actual="$(rclone cat "$destination/$name" | python3 -c 'import hashlib,sys; data=sys.stdin.buffer; digest=hashlib.sha256(); size=0
for chunk in iter(lambda: data.read(1024*1024), b""):
 digest.update(chunk); size += len(chunk)
print(digest.hexdigest(), size)')" || die "cannot hash remote archive object: $name"
        [ "$actual" = "$expected_sha $expected_size" ] || \
            die "remote archive object differs from SHA256SUMS/manifest: $name"
    done < "$temporary/objects.tsv"
    rclone lsf --files-only -R "$destination" | LC_ALL=C sort > "$temporary/actual-names"
    LC_ALL=C sort "$temporary/expected-names" -o "$temporary/expected-names"
    cmp --silent "$temporary/expected-names" "$temporary/actual-names" || \
        die "remote destination contains missing, duplicate, or unexpected objects"
    cat "$temporary/manifest-sha"
)

verify_complete_phase() {
    local destination="" expected_complete_sha="" expected_manifest_sha="" expected_sums_sha="" expected_prearchive_sha=""
    local verify_live_captures=false
    local new_node_paths=()
    while [ "$#" -gt 0 ]; do
        case "$1" in
            --destination) [ "$#" -ge 2 ] || die "--destination needs a value"; destination="$2"; shift 2 ;;
            --expected-complete-sha256) [ "$#" -ge 2 ] || die "--expected-complete-sha256 needs a value"; expected_complete_sha="$2"; shift 2 ;;
            --expected-archive-manifest-sha256) [ "$#" -ge 2 ] || die "--expected-archive-manifest-sha256 needs a value"; expected_manifest_sha="$2"; shift 2 ;;
            --expected-sha256sums-sha256) [ "$#" -ge 2 ] || die "--expected-sha256sums-sha256 needs a value"; expected_sums_sha="$2"; shift 2 ;;
            --expected-prearchive-rollout-sha256) [ "$#" -ge 2 ] || die "--expected-prearchive-rollout-sha256 needs a value"; expected_prearchive_sha="$2"; shift 2 ;;
            --new-node-paths) [ "$#" -ge 4 ] || die "--new-node-paths needs NODE REMOTE_ROOT DATA_DIR"; new_node_paths+=("$2" "$3" "$4"); shift 4 ;;
            --verify-live-captures) verify_live_captures=true; shift ;;
            -h|--help) usage; return 0 ;;
            *) die "unknown verify-complete option: $1" ;;
        esac
    done
    [ -n "$destination" ] || die "verify-complete requires --destination"
    validate_drive_remote "$destination" || die "verify-complete destination is unsafe"
    for value in "$expected_complete_sha" "$expected_manifest_sha" "$expected_sums_sha" "$expected_prearchive_sha"; do
        [ -z "$value" ] || require_hash "$value" "expected archive root"
    done
    require_commands python3 rclone mktemp cmp
    local archive_manifest_sha
    archive_manifest_sha="$(verify_remote_complete "$destination" "" "" "" \
        "$expected_complete_sha" "$expected_manifest_sha" "$expected_sums_sha" \
        "$expected_prearchive_sha")"
    require_hash "$archive_manifest_sha" "verified archive manifest hash"
    if [ "${#new_node_paths[@]}" -gt 0 ] || [ "$verify_live_captures" = true ]; then
        local temporary freeze_plan freeze_sha capture_id
        temporary="$(mktemp -d)"
        freeze_plan="$temporary/freeze-plan.json"
        rclone cat "$destination/freeze-plan.json" > "$freeze_plan"
        rclone cat "$destination/freeze-plan.json.sha256" > "${freeze_plan}.sha256"
        chmod 444 -- "$freeze_plan" "${freeze_plan}.sha256"
        freeze_sha="$(freeze_plan_hash "$freeze_plan")"
        capture_id="$(capture_id_for_freeze_plan_hash "$freeze_sha")"
        [ "$destination" = "$DRIVE_REMOTE/captures/$capture_id" ] || \
            die "verified archive destination differs from its frozen capture id"
        if [ "${#new_node_paths[@]}" -gt 0 ]; then
            python3 - "$freeze_plan" "${new_node_paths[@]}" <<'PY'
import json
import os
import pathlib
import sys

freeze = json.loads(pathlib.Path(sys.argv[1]).read_text(encoding="utf-8"))
raw = sys.argv[2:]
if len(raw) != 18:
    raise SystemExit("final rollout must provide exactly six new-node path triples")
provided = {}
for index in range(0, len(raw), 3):
    name, remote_root, data_dir = raw[index:index + 3]
    if name in provided or not all(path.startswith("/") and os.path.normpath(path) == path for path in (remote_root, data_dir)):
        raise SystemExit("final rollout path binding is duplicated, relative, or non-normalized")
    provided[name] = (remote_root, data_dir)
legacy = {row["name"]: row["data_dir"] for row in freeze["nodes"]}
if set(provided) != set(legacy):
    raise SystemExit("final rollout path binding differs from the six frozen nodes")
for name, old in legacy.items():
    for label, new in zip(("remote_root", "data_dir"), provided[name]):
        common = os.path.commonpath((old, new))
        if common in {old, new}:
            raise SystemExit(f"{name} new {label} overlaps frozen legacy data path")
PY
        fi
        if [ "$verify_live_captures" = true ]; then
            REMOTE_HELPER_SHA="$(manifest_field "$freeze_plan" remote_helper_sha256)"
            require_hash "$REMOTE_HELPER_SHA" "sealed remote helper hash"
            REMOTE_HELPER_PATH="/root/.arc-recovery-helpers/$REMOTE_HELPER_SHA/archive-node.sh"
            local node
            for node in nyc lax ams lhr nrt sgp; do
                run_sealed_source_status_exact "$freeze_plan" "$freeze_sha" "$capture_id" "$node" >/dev/null
            done
            printf 'archive fleet: PASS all six frozen legacy source indexes reverified after cutover\n'
        fi
        rm -rf -- "$temporary"
    fi
    printf 'archive fleet: VERIFIED COMPLETE destination=%s archive_manifest=%s\n' \
        "$destination" "$archive_manifest_sha"
}

verify_reference_pair() (
    local binary="$1" genesis="$2" validators="$3" legacy_validators="$4"
    local snapshot="$5" source_wal="$6" source_round="$7" created_at="$8"
    local recovery_epoch="$9" validator_set_id="${10}" source_height="${11}"
    local source_hash="${12}" source_state_root="${13}" transition_state_root="${14}"
    local checkpoint_manifest="${15}" allow_unbound="${16}"
    local temporary
    temporary="$(mktemp -d)"
    trap 'find "$temporary" -depth -delete 2>/dev/null || true' EXIT
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
    "${command[@]}" > "$temporary/summary.json" 2> "$temporary/export.stderr" || \
        die "sealed reference snapshot/WAL export command failed"
    [ -s "$temporary/reference.arcchkpt" ] && [ ! -L "$temporary/reference.arcchkpt" ] || \
        die "sealed reference snapshot/WAL did not produce a regular checkpoint artifact"
    python3 - "$temporary/summary.json" "$source_height" "$source_hash" \
        "$source_state_root" "$transition_state_root" "$checkpoint_manifest" \
        "$source_round" "$created_at" "$recovery_epoch" "$validator_set_id" <<'PY' || \
        die "sealed reference snapshot/WAL does not reproduce the selected checkpoint"
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
    printf 'archive fleet: PASS sealed source snapshot/WAL independently reproduces the selected checkpoint\n'
)

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
    local manifest_destination manifest_allow_unbound destination_sha policy
    manifest_destination="$(manifest_field "$manifest" archive.destination)"
    manifest_allow_unbound="$(manifest_field "$manifest" archive.allow_unbound_legacy_wal)"
    [ "$manifest_destination" = "$DRIVE_REMOTE/captures/$capture_id" ] || \
        die "rollout archive destination differs from the exact configured capture-scoped Drive path"
    validate_drive_remote "$manifest_destination" || die "sealed archive destination is unsafe"
    [ "$manifest_allow_unbound" = "$allow_unbound" ] || \
        die "--allow-unbound-legacy-wal differs from the sealed archive policy"
    destination_sha="$(printf '%s' "$manifest_destination" | python3 -c 'import hashlib,sys; print(hashlib.sha256(sys.stdin.buffer.read()).hexdigest())')"
    policy=BOUND
    [ "$allow_unbound" = true ] && policy=UNBOUND

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

    printf 'ARC content-verified legacy archive seal plan\n'
    printf '  freeze plan:          %s\n' "$freeze_sha"
    printf '  capture:              %s\n' "$capture_id"
    printf '  rollout manifest:     %s\n' "$manifest_sha"
    printf '  validator public set: %s\n' "$validator_sha"
    printf '  legacy source set:    %s\n' "$legacy_validator_set_sha"
    printf '  paired snapshot/WAL:  %s / %s\n' "$source_snapshot_sha" "$source_wal_sha"
    printf '  selected checkpoint:  H=%s hash=%s source_root=%s transition_root=%s\n' \
        "$source_height" "$source_hash" "$source_state_root" "$transition_state_root"
    printf '  unbound legacy WAL:   %s (explicitly persisted in binding evidence)\n' "$allow_unbound"
    printf '  destination:          %s (sha256=%s)\n' "$manifest_destination" "$destination_sha"
    local node host
    REMOTE_HELPER_SHA="$(manifest_field "$freeze_plan" remote_helper_sha256)"
    require_hash "$REMOTE_HELPER_SHA" "sealed remote helper hash"
    REMOTE_HELPER_PATH="/root/.arc-recovery-helpers/$REMOTE_HELPER_SHA/archive-node.sh"
    for node in nyc lax ams lhr nrt sgp; do
        host="$(host_for "$node")"
        run_remote "$node" status "$capture_id" "$node" >/dev/null
        printf '  capture present/stopped: %s\n' "$node"
    done
    rclone lsd "$DRIVE_REMOTE" >/dev/null
    if [ "$execute" != true ]; then
        printf 'archive fleet: PLAN ONLY; no remote or Drive file was changed\n'
        return 0
    fi
    local expected_go="GO $manifest_sha FREEZE $freeze_sha CAPTURE $capture_id DEST $destination_sha LEGACY_WAL $policy"
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
    [ "$failed" -eq 0 ] || die "staging failed; fenced source bytes remain in place and no upload was attempted"

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
    [ "$failed" -eq 0 ] || die "at least one capture could not produce content-sealed classification evidence; no bundle or upload was attempted"

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
    printf 'archive fleet: final-capture classification complete canonical=%s forks=%s preserved_unclassified=%s; all six remain labelled and retained; the independently verified shared reference pair is canonical\n' \
        "$canonical_count" "$fork_count" "$unclassified_count"

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
    create_canonical_reference \
        "$shared_root/canonical-reference.json" "$shared_root" "$allow_unbound" \
        "$source_height" "$source_hash" "$source_state_root" "$transition_state_root" \
        "$checkpoint_manifest" "$source_round" "$created_at_unix_ms" \
        "$recovery_epoch" "$validator_set_id" "$binary_sha" "$genesis_sha" \
        "$validator_sha" "$legacy_validator_set_sha" "$source_snapshot_sha" \
        "$source_wal_sha" "$checkpoint_sha"

    local destination="$manifest_destination"
    local existing_capture archive_manifest_sha
    if existing_capture="$(rclone cat "$destination/capture-id.txt" 2>/dev/null)"; then
        [ "$existing_capture" = "$capture_id" ] || \
            die "Drive destination is already bound to a different freeze capture"
    fi
    if rclone cat "$destination/COMPLETE.json" > "$log_root/existing-COMPLETE.json" 2>/dev/null; then
        python3 - "$log_root/existing-COMPLETE.json" "$freeze_sha" "$capture_id" "$manifest_sha" <<'PY'
import json
import sys
value = json.load(open(sys.argv[1], encoding="utf-8"))
if (value.get("freeze_plan_sha256"), value.get("capture_id"), value.get("rollout_manifest_sha256")) != tuple(sys.argv[2:]):
    raise SystemExit("existing COMPLETE belongs to a different freeze/capture/prearchive rollout")
PY
        archive_manifest_sha="$(verify_remote_complete "$destination" "" "" "" "" "" "" "$manifest_sha")"
        require_hash "$archive_manifest_sha" "existing archive manifest hash"
        printf 'archive fleet: existing COMPLETE.json fully verified; verification-only resume\n'
        printf 'archive fleet: FINAL-ROLLOUT-ROOTS destination=%s complete_sha256=%s archive_manifest_sha256=%s sha256sums_sha256=%s prearchive_rollout_sha256=%s\n' \
            "$destination" "$(rclone cat "$destination/COMPLETE.json" | hash_size_stream | cut -d' ' -f1)" \
            "$archive_manifest_sha" "$(rclone cat "$destination/SHA256SUMS" | hash_size_stream | cut -d' ' -f1)" "$manifest_sha"
        return 0
    fi
    rclone mkdir "$destination"
    rclone copy "$shared_root" "$destination" --immutable --checksum --metadata \
        --retries 5 --low-level-retries 20

    # Stream at most three exact fenced sources concurrently. No full capture,
    # working-data, or compressed-bundle copy is created on a validator.
    local upload_order=(nyc lax ams lhr nrt sgp)
    local upload_index
    failed=0
    for upload_index in 0 3; do
        pids=()
        names=()
        for node in "${upload_order[@]:upload_index:3}"; do
            (
                stream_bundle_to_drive "$node" "$capture_id" "$manifest_sha" "$destination" "$log_root"
                printf 'archive fleet: streamed and SHA-256-verified preserved classified capture %s\n' "$node"
            ) > "$log_root/$node-upload.log" 2>&1 &
            pids+=("$!")
            names+=("$node")
        done
        for index in "${!pids[@]}"; do
            if wait "${pids[$index]}"; then
                sed -n '1,80p' "$log_root/${names[$index]}-upload.log"
            else
                printf 'archive fleet: create-only streamed Drive upload/check failed: %s\n' \
                    "${names[$index]}" >&2
                sed -n '1,160p' "$log_root/${names[$index]}-upload.log" >&2
                failed=1
            fi
        done
    done
    [ "$failed" -eq 0 ] || \
        die "one or more preserved validator uploads failed; COMPLETE was not emitted"
    : > "$log_root/bundle-statuses.jsonl"
    for node in nyc lax ams lhr nrt sgp; do
        cat "$log_root/$node-bundle-status.json" >> "$log_root/bundle-statuses.jsonl"
    done

    archive_manifest_sha="$(build_archive_metadata \
        "$shared_root" "$log_root/bundle-statuses.jsonl" "$metadata_root" "$complete_root" \
        "$freeze_sha" "$capture_id" "$manifest_sha" "$source_commit" \
        "$orchestrator_sha" "$helper_sha" "$rollout_tool_sha" "$schema_sha" \
        "$canonical_count" "$fork_count" "$unclassified_count")"
    require_hash "$archive_manifest_sha" "archive manifest hash"
    rclone check "$shared_root" "$destination" --checksum --one-way --checkers 4
    rclone copy "$metadata_root" "$destination" --immutable --checksum --metadata \
        --retries 5 --low-level-retries 20
    rclone check "$metadata_root" "$destination" --checksum --one-way --checkers 4
    # This is deliberately the final remote mutation. A failed or partial run
    # remains resumable, but no consumer may accept it as complete.
    upload_immutable "$complete_root/COMPLETE.json" "$destination/COMPLETE.json"
    verify_remote_complete "$destination" "$complete_root/COMPLETE.json" \
        "$metadata_root/ARCHIVE-MANIFEST.json" \
        "$metadata_root/ARCHIVE-MANIFEST.json.sha256" >/dev/null
    printf 'archive fleet: COMPLETE capture=%s rollout=%s archive_manifest=%s capture_canonical=%s capture_forks=%s capture_preserved_unclassified=%s canonical_reference=verified destination=%s\n' \
        "$capture_id" "$manifest_sha" "$archive_manifest_sha" "$canonical_count" \
        "$fork_count" "$unclassified_count" "$destination"
    printf 'archive fleet: FINAL-ROLLOUT-ROOTS destination=%s complete_sha256=%s archive_manifest_sha256=%s sha256sums_sha256=%s prearchive_rollout_sha256=%s\n' \
        "$destination" "$(hash_file "$complete_root/COMPLETE.json")" "$archive_manifest_sha" \
        "$(hash_file "$metadata_root/SHA256SUMS")" "$manifest_sha"
}

COMMAND="${1:-}"
if [ -n "$COMMAND" ]; then
    shift
fi
case "$COMMAND" in
    audit-writers) audit_writers "$@" ;;
    seal-freeze-plan) seal_freeze_plan "$@" ;;
    capture) capture_phase "$@" ;;
    seal) seal_phase "$@" ;;
    verify-complete) verify_complete_phase "$@" ;;
    -h|--help|help|'') usage ;;
    *) usage >&2; exit 2 ;;
esac
