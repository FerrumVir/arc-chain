#!/usr/bin/env bash
# Fail-closed Google Drive readiness gate for the destructive fleet-freeze edge.
set -Eeuo pipefail
umask 077

SCRIPT_DIR="$(CDPATH='' cd -- "$(dirname -- "$0")" && pwd)"
HARD_DAILY_UPLOAD_LIMIT_BYTES=750000000000
GOOGLE_DRIVE_OBJECT_LIMIT_BYTES=5000000000000
CANARY_BYTES=$((8 * 1024 * 1024))
REVIEWED_RCLONE_VERSION="v1.75.0"
DRIVE_ACCOUNT_HELPER="$SCRIPT_DIR/drive-account-identity.py"
DRIVE_ACCOUNT_HELPER_SHA256="04d79584c48909407c0c841cac9a81ef9857030a09085837ef41074c7adc5bb4"
TEMP_ROOT=""
CANARY_REMOTE=""
CANARY_PRESENT=false
RCLONE_CALL_INDEX=0
RCLONE_BIN=""
PINNED_PYTHON_PATH=""
PINNED_PYTHON_SHA256=""

die() {
    printf 'drive prefreeze: %s\n' "$*" >&2
    exit 1
}

cleanup() {
    if [ "$CANARY_PRESENT" = true ] && [ -n "$CANARY_REMOTE" ]; then
        # This is a create-unique canary owned by this invocation. Cleanup is
        # best effort on an already-failing path and never targets a directory.
        "$RCLONE_BIN" deletefile "$CANARY_REMOTE" --drive-use-trash=false \
            >/dev/null 2>&1 || true
    fi
    if [ -n "$TEMP_ROOT" ] && [ -d "$TEMP_ROOT" ] && [ ! -L "$TEMP_ROOT" ]; then
        rm -rf -- "$TEMP_ROOT"
    fi
}

trap cleanup EXIT

usage() {
    cat <<'EOF'
Usage:
  verify-drive-prefreeze.sh preflight|execute \
    --freeze-plan /absolute/freeze.lock.json \
    --expected-freeze-plan-sha256 HASH \
    --capture-id HASH \
    --remote-root 'ARC_REMOTE:ARC Chain Recovery' \
    --expected-root-sha256 HASH \
    --expected-client-id-sha256 HASH \
    --expected-account-sha256 HASH \
    --daily-upload-budget-bytes BYTES

`preflight` is read-only. `execute` repeats the same checks, then uploads,
downloads, SHA-256 verifies, and permanently deletes one unique 8 MiB canary.
Run `execute` after validating the exact FREEZE authorization and immediately
before the first validator SIGTERM.

The expected client hash is SHA256(exact OAuth client_id UTF-8 bytes). The
expected account hash is SHA256(lowercase stripped account email UTF-8 bytes).
The expected root hash is SHA256(exact remote-root UTF-8 bytes). These values
are identifiers, not OAuth client secrets or tokens.

The gate requires the reviewed rclone v1.75.0. It refreshes the selected
remote with a read-only `rclone about`, then pipes one decrypted selected-remote
`rclone config show` stream directly to a hash-pinned local helper. From that
single in-memory stream the helper hashes the custom OAuth client ID, consumes
the bearer token, and calls the fixed Google Drive v3
`about?fields=user(emailAddress,permissionId,me)` endpoint with verified TLS.
OAuth material and raw client/account/permission fields are never written to a
receipt, temporary file, argv, environment variable, or log.

`--daily-upload-budget-bytes` is the independently reviewed *remaining* budget
for this dedicated ARC uploader in its current Google quota window, capped here
at 750 GB (750,000,000,000 bytes, decimal). Google Drive does not expose the
remaining daily-upload counter; do not assert a fresh budget unless this
account has no other upload writers.
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

bootstrap_python_hash() {
    local output digest
    if [ -x /usr/bin/sha256sum ]; then
        output="$(/usr/bin/env -i HOME=/var/empty PATH=/usr/bin:/bin LANG=C LC_ALL=C \
            /usr/bin/sha256sum -- "$1")" || die "cannot hash pinned Python"
    elif [ -x /usr/bin/shasum ]; then
        output="$(/usr/bin/env -i HOME=/var/empty PATH=/usr/bin:/bin LANG=C LC_ALL=C \
            /usr/bin/shasum -a 256 -- "$1")" || die "cannot hash pinned Python"
    else
        die "absolute system SHA-256 utility is unavailable"
    fi
    digest="${output%% *}"
    require_hash "$digest" "pinned Python observed hash"
    printf '%s\n' "$digest"
}

configure_pinned_python() {
    PINNED_PYTHON_PATH="${ARC_RECOVERY_PYTHON_PATH:-}"
    PINNED_PYTHON_SHA256="${ARC_RECOVERY_PYTHON_SHA256:-}"
    case "$PINNED_PYTHON_PATH" in
        /usr/bin/python3|/usr/bin/python3.[0-9]*) ;;
        *) die "ARC_RECOVERY_PYTHON_PATH must be /usr/bin/python3[.VERSION]" ;;
    esac
    require_hash "$PINNED_PYTHON_SHA256" "pinned Python expected hash"
    [ -f "$PINNED_PYTHON_PATH" ] && [ ! -L "$PINNED_PYTHON_PATH" ] && [ -x "$PINNED_PYTHON_PATH" ] || \
        die "pinned Python must be one executable regular non-symlink file"
    [ "$(bootstrap_python_hash "$PINNED_PYTHON_PATH")" = "$PINNED_PYTHON_SHA256" ] || \
        die "pinned Python differs from its reviewed hash"
    /usr/bin/env -i HOME=/var/empty PATH=/usr/bin:/bin LANG=C LC_ALL=C \
        "$PINNED_PYTHON_PATH" -I - "$PINNED_PYTHON_PATH" "$PINNED_PYTHON_SHA256" <<'PY'
import hashlib, os, pathlib, stat, sys
path = pathlib.Path(sys.argv[1]); expected = sys.argv[2]
fd = os.open(path, os.O_RDONLY | getattr(os, "O_NOFOLLOW", 0) | getattr(os, "O_CLOEXEC", 0))
try:
    before = os.fstat(fd); visible = os.lstat(path)
    identity = lambda value: (value.st_dev, value.st_ino, value.st_mode, value.st_uid,
                              value.st_gid, value.st_nlink, value.st_size,
                              value.st_mtime_ns, value.st_ctime_ns)
    if (not stat.S_ISREG(before.st_mode) or stat.S_ISLNK(visible.st_mode)
            or identity(before) != identity(visible) or before.st_uid != 0
            or stat.S_IMODE(before.st_mode) & 0o022 or before.st_nlink < 1):
        raise SystemExit("pinned Python owner/mode/type/link identity differs")
    digest = hashlib.sha256()
    while chunk := os.read(fd, 1024 * 1024): digest.update(chunk)
    if identity(before) != identity(os.fstat(fd)) or digest.hexdigest() != expected:
        raise SystemExit("pinned Python changed or differs from its reviewed hash")
finally: os.close(fd)
PY
}

python3() {
    [ -n "$PINNED_PYTHON_PATH" ] || die "pinned Python is not initialized"
    [ "$(bootstrap_python_hash "$PINNED_PYTHON_PATH")" = "$PINNED_PYTHON_SHA256" ] || \
        die "pinned Python changed during Drive prefreeze"
    /usr/bin/env -i HOME="${TEMP_ROOT:-/var/empty}" PATH=/usr/bin:/bin LANG=C LC_ALL=C \
        "$PINNED_PYTHON_PATH" -I "$@"
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

hash_text() {
    printf '%s' "$1" | python3 -c \
        'import hashlib,sys; print(hashlib.sha256(sys.stdin.buffer.read()).hexdigest())'
}

capture_id_for_freeze() {
    python3 - "$1" <<'PY'
import hashlib
import sys

freeze = sys.argv[1]
print(hashlib.sha256(b"ARC recovery capture v2\0" + bytes.fromhex(freeze)).hexdigest())
PY
}

validate_remote_root() {
    python3 - "$1" <<'PY'
import re
import sys

value = sys.argv[1]
if ("\x00" in value or "\n" in value or "\r" in value or value.startswith("-")
        or ":" not in value or value.endswith("/")):
    raise SystemExit("remote root is unsafe")
remote, path = value.split(":", 1)
if not re.fullmatch(r"[A-Za-z0-9][A-Za-z0-9_-]{0,63}", remote):
    raise SystemExit("remote name is unsafe")
if not re.fullmatch(r"[A-Za-z0-9][A-Za-z0-9 ._/@%+=,-]{0,511}", path):
    raise SystemExit("remote root path is unsafe")
if ".." in path.split("/"):
    raise SystemExit("remote root traversal is forbidden")
if path == "captures" or path.endswith("/captures") or "/captures/" in path:
    raise SystemExit("prefreeze input must be an archive root, not a capture destination")
PY
}

reject_effective_client_overrides() {
    local remote_name="$1" remote_env variable
    remote_env="$(printf '%s' "$remote_name" | tr '[:lower:]-' '[:upper:]_')"
    while IFS= read -r variable; do
        case "$variable" in
            RCLONE_DRIVE_*|"RCLONE_CONFIG_${remote_env}_"*)
                die "ambient rclone Drive/selected-remote override is forbidden: $variable"
                ;;
        esac
    done < <(compgen -e)
    for variable in SSL_CERT_FILE SSL_CERT_DIR SSLKEYLOGFILE PYTHONHTTPSVERIFY; do
        if printenv "$variable" >/dev/null 2>&1; then
            die "ambient TLS behavior override is forbidden: $variable"
        fi
    done
}

run_rclone_clean() {
    local label="$1" output="$2" stderr_path
    shift 2
    RCLONE_CALL_INDEX=$((RCLONE_CALL_INDEX + 1))
    stderr_path="$TEMP_ROOT/rclone-stderr-$RCLONE_CALL_INDEX"
    : > "$stderr_path"
    if ! "$RCLONE_BIN" "$@" > "$output" 2> "$stderr_path"; then
        die "rclone $label failed"
    fi
    [ ! -s "$stderr_path" ] || \
        die "rclone $label emitted stderr; warnings are fatal before fleet freeze"
}

inspect_rclone_version() {
    local version_path="$1"
    python3 - "$version_path" "$REVIEWED_RCLONE_VERSION" <<'PY'
import pathlib
import sys

try:
    lines = pathlib.Path(sys.argv[1]).read_text(encoding="utf-8").splitlines()
except (OSError, UnicodeError):
    raise SystemExit("rclone version output is invalid")
expected = "rclone " + sys.argv[2]
if not lines or lines[0] != expected:
    raise SystemExit("rclone version is not the exact reviewed release")
print(sys.argv[2])
PY
}

inspect_config_show_capability() {
    local help_path="$1"
    python3 - "$help_path" <<'PY'
import pathlib
import sys

try:
    value = pathlib.Path(sys.argv[1]).read_text(encoding="utf-8")
except (OSError, UnicodeError):
    raise SystemExit("rclone config-show help is invalid")
if "Print (decrypted) config file, or the config for a single remote." not in value:
    raise SystemExit("rclone lacks the reviewed selected-remote config-show capability")
if "rclone config show [<remote>] [flags]" not in value:
    raise SystemExit("rclone config-show command shape differs from the reviewed release")
PY
}

inspect_drive_identity() {
    local label="$1" result rclone_stderr helper_stderr
    [ -f "$DRIVE_ACCOUNT_HELPER" ] && [ ! -L "$DRIVE_ACCOUNT_HELPER" ] || \
        die "Drive account identity helper is missing, non-regular, or a symlink"
    [ "$(hash_file "$DRIVE_ACCOUNT_HELPER")" = "$DRIVE_ACCOUNT_HELPER_SHA256" ] || \
        die "Drive account identity helper differs from the reviewed bytes"
    RCLONE_CALL_INDEX=$((RCLONE_CALL_INDEX + 1))
    rclone_stderr="$TEMP_ROOT/rclone-stderr-$RCLONE_CALL_INDEX"
    helper_stderr="$TEMP_ROOT/drive-identity-stderr-$RCLONE_CALL_INDEX"
    : > "$rclone_stderr"
    : > "$helper_stderr"
    if ! result="$(
        set -o pipefail
        "$RCLONE_BIN" config show "$REMOTE_NAME" 2> "$rclone_stderr" | \
            python3 -I "$DRIVE_ACCOUNT_HELPER" "$REMOTE_NAME" 2> "$helper_stderr"
    )"; then
        die "Drive account identity $label failed"
    fi
    [ ! -s "$rclone_stderr" ] || \
        die "rclone account configuration stream emitted stderr; warnings are fatal before fleet freeze"
    [ ! -s "$helper_stderr" ] || \
        die "Drive account identity helper emitted stderr"
    printf '%s\n' "$result" | grep -Eq '^[0-9a-f]{64} [0-9a-f]{64} [0-9a-f]{64}$' || \
        die "Drive account identity helper returned an invalid result"
    printf '%s\n' "$result"
}

inspect_free_bytes() {
    local about_path="$1"
    python3 - "$about_path" <<'PY'
import json
import pathlib
import sys

try:
    value = json.loads(pathlib.Path(sys.argv[1]).read_text(encoding="utf-8"))
except (OSError, UnicodeError, json.JSONDecodeError) as error:
    raise SystemExit(f"rclone capacity response is invalid: {error}")
free = value.get("free") if isinstance(value, dict) else None
if isinstance(free, bool) or not isinstance(free, int) or free < 0:
    raise SystemExit("Google Drive did not return a finite integer free-byte capacity")
print(free)
PY
}

freeze_reservations() {
    local freeze_plan="$1"
    python3 - "$freeze_plan" <<'PY'
import json
import pathlib
import sys

GIB = 1024 ** 3
EXPECTED = {"nyc", "lax", "ams", "lhr", "nrt", "sgp"}
try:
    value = json.loads(pathlib.Path(sys.argv[1]).read_text(encoding="utf-8"))
except (OSError, UnicodeError, json.JSONDecodeError) as error:
    raise SystemExit(f"freeze plan is invalid JSON: {error}")
nodes = value.get("nodes") if isinstance(value, dict) else None
if not isinstance(nodes, list) or len(nodes) != 6:
    raise SystemExit("freeze plan must bind exactly six archive sources")
sizes = {}
for row in nodes:
    if not isinstance(row, dict):
        raise SystemExit("freeze plan node row is not an object")
    name, size = row.get("name"), row.get("data_bytes")
    if name in sizes or name not in EXPECTED:
        raise SystemExit("freeze plan archive source names are duplicated or unexpected")
    if isinstance(size, bool) or not isinstance(size, int) or size <= 0:
        raise SystemExit(f"freeze plan data_bytes is invalid for {name}")
    sizes[name] = size
if set(sizes) != EXPECTED:
    raise SystemExit("freeze plan does not bind the exact six archive source names")

# Three times the sealed source allocation plus 32 GiB covers compression/tar
# expansion, capture/binding evidence, and the shared recovery artifacts. The
# largest single-object reservation independently includes another 4 GiB.
total_source = sum(sizes.values())
archive_reservation = 3 * total_source + 32 * GIB
largest_object_reservation = 3 * max(sizes.values()) + 4 * GIB
print(total_source, archive_reservation, largest_object_reservation)
PY
}

write_canary() {
    local output="$1"
    python3 - "$output" "$CANARY_BYTES" <<'PY'
import os
import pathlib
import sys

path = pathlib.Path(sys.argv[1])
remaining = int(sys.argv[2])
with path.open("xb") as handle:
    while remaining:
        chunk = os.urandom(min(1024 * 1024, remaining))
        handle.write(chunk)
        remaining -= len(chunk)
    handle.flush()
    os.fsync(handle.fileno())
path.chmod(0o400)
PY
}

emit_receipt() {
    python3 - "$@" <<'PY'
import json
import sys

(mode, freeze_sha, capture_id, root_sha, client_sha, account_sha, permission_sha, rclone_version,
 total_source, reservation, largest_object, budget, free_before, free_after,
 canary_verified, canary_deleted) = sys.argv[1:]
value = {
    "schema": "arc.recovery.drive-prefreeze.v1",
    "mode": mode,
    "freeze_plan_sha256": freeze_sha,
    "capture_id": capture_id,
    "remote_root_sha256": root_sha,
    "client_id_sha256": client_sha,
    "account_sha256": account_sha,
    "permission_id_sha256": permission_sha,
    "rclone_version": rclone_version,
    "source_bytes": int(total_source),
    "archive_reservation_bytes": int(reservation),
    "largest_object_reservation_bytes": int(largest_object),
    "daily_upload_budget_bytes": int(budget),
    "daily_upload_budget_basis": "operator-reviewed-remaining-dedicated-account",
    "available_bytes_before": int(free_before),
    "available_bytes_after": int(free_after),
    "canary_bytes": 8 * 1024 * 1024,
    "canary_verified": canary_verified == "true",
    "canary_deleted": canary_deleted == "true",
}
print(json.dumps(value, sort_keys=True, separators=(",", ":")))
PY
}

MODE="${1:-}"
case "$MODE" in
    preflight|execute) shift ;;
    -h|--help|help|'') usage; exit 0 ;;
    *) usage >&2; exit 2 ;;
esac

FREEZE_PLAN=""
EXPECTED_FREEZE_SHA=""
CAPTURE_ID=""
REMOTE_ROOT=""
EXPECTED_ROOT_SHA=""
EXPECTED_CLIENT_SHA=""
EXPECTED_ACCOUNT_SHA=""
DAILY_BUDGET=""
while [ "$#" -gt 0 ]; do
    case "$1" in
        --freeze-plan) [ "$#" -ge 2 ] || die "--freeze-plan needs a value"; FREEZE_PLAN="$2"; shift 2 ;;
        --expected-freeze-plan-sha256) [ "$#" -ge 2 ] || die "--expected-freeze-plan-sha256 needs a value"; EXPECTED_FREEZE_SHA="$2"; shift 2 ;;
        --capture-id) [ "$#" -ge 2 ] || die "--capture-id needs a value"; CAPTURE_ID="$2"; shift 2 ;;
        --remote-root) [ "$#" -ge 2 ] || die "--remote-root needs a value"; REMOTE_ROOT="$2"; shift 2 ;;
        --expected-root-sha256) [ "$#" -ge 2 ] || die "--expected-root-sha256 needs a value"; EXPECTED_ROOT_SHA="$2"; shift 2 ;;
        --expected-client-id-sha256) [ "$#" -ge 2 ] || die "--expected-client-id-sha256 needs a value"; EXPECTED_CLIENT_SHA="$2"; shift 2 ;;
        --expected-account-sha256) [ "$#" -ge 2 ] || die "--expected-account-sha256 needs a value"; EXPECTED_ACCOUNT_SHA="$2"; shift 2 ;;
        --daily-upload-budget-bytes) [ "$#" -ge 2 ] || die "--daily-upload-budget-bytes needs a value"; DAILY_BUDGET="$2"; shift 2 ;;
        -h|--help) usage; exit 0 ;;
        *) die "unknown option: $1" ;;
    esac
done

configure_pinned_python
command -v python3 >/dev/null 2>&1 || die "required command is missing: python3"
command -v rclone >/dev/null 2>&1 || die "required command is missing: rclone"
command -v grep >/dev/null 2>&1 || die "required command is missing: grep"
command -v mktemp >/dev/null 2>&1 || die "required command is missing: mktemp"
command -v tr >/dev/null 2>&1 || die "required command is missing: tr"

RCLONE_COMMAND="$(command -v rclone)"
case "$RCLONE_COMMAND" in /*) ;; *) die "rclone must resolve to an absolute executable path" ;; esac
RCLONE_BIN="$(python3 - "$RCLONE_COMMAND" <<'PY'
import os
import pathlib
import sys

candidate = pathlib.Path(os.path.realpath(sys.argv[1]))
if not candidate.is_file() or not os.access(candidate, os.X_OK):
    raise SystemExit("rclone is not a regular executable")
print(candidate)
PY
)"
[ -n "$RCLONE_BIN" ] || die "cannot resolve the rclone executable"

case "$FREEZE_PLAN" in /*) ;; *) die "freeze plan must be an absolute path" ;; esac
[ -f "$FREEZE_PLAN" ] && [ ! -L "$FREEZE_PLAN" ] || \
    die "freeze plan is missing, non-regular, or a symlink"
require_hash "$EXPECTED_FREEZE_SHA" "expected freeze-plan hash"
require_hash "$CAPTURE_ID" "capture id"
require_hash "$EXPECTED_ROOT_SHA" "expected remote-root hash"
require_hash "$EXPECTED_CLIENT_SHA" "expected OAuth client-id hash"
require_hash "$EXPECTED_ACCOUNT_SHA" "expected account hash"
require_uint "$DAILY_BUDGET" "daily upload budget"
[ "$DAILY_BUDGET" -gt 0 ] || die "daily upload budget must be positive"
[ "$DAILY_BUDGET" -le "$HARD_DAILY_UPLOAD_LIMIT_BYTES" ] || \
    die "daily upload budget exceeds the 750 GB (750,000,000,000 byte) Google Drive hard policy ceiling"

validate_remote_root "$REMOTE_ROOT" || die "remote root is unsafe"
REMOTE_NAME="${REMOTE_ROOT%%:*}"
[ "$REMOTE_NAME" != arc-drive ] || \
    die "legacy arc-drive is forbidden; use the reviewed parallel ARC recovery remote"
[ "$(hash_text "$REMOTE_ROOT")" = "$EXPECTED_ROOT_SHA" ] || \
    die "remote root differs from the reviewed exact hash"
reject_effective_client_overrides "$REMOTE_NAME"

ACTUAL_FREEZE_SHA="$(hash_file "$FREEZE_PLAN")"
[ "$ACTUAL_FREEZE_SHA" = "$EXPECTED_FREEZE_SHA" ] || \
    die "freeze-plan bytes differ from the expected hash"
[ "$(capture_id_for_freeze "$ACTUAL_FREEZE_SHA")" = "$CAPTURE_ID" ] || \
    die "capture id is not derived from the exact freeze-plan hash"

read -r TOTAL_SOURCE_BYTES ARCHIVE_RESERVATION_BYTES LARGEST_OBJECT_RESERVATION_BYTES <<EOF
$(freeze_reservations "$FREEZE_PLAN")
EOF
require_uint "$TOTAL_SOURCE_BYTES" "source byte total"
require_uint "$ARCHIVE_RESERVATION_BYTES" "archive reservation"
require_uint "$LARGEST_OBJECT_RESERVATION_BYTES" "largest object reservation"
[ "$LARGEST_OBJECT_RESERVATION_BYTES" -le "$GOOGLE_DRIVE_OBJECT_LIMIT_BYTES" ] || \
    die "largest archive object reservation exceeds the 5 TB Google Drive object limit"
[ "$ARCHIVE_RESERVATION_BYTES" -le "$DAILY_BUDGET" ] || \
    die "archive reservation exceeds the reviewed daily upload budget"

TEMP_ROOT="$(mktemp -d "${TMPDIR:-/tmp}/arc-drive-prefreeze.XXXXXX")"
[ -d "$TEMP_ROOT" ] && [ ! -L "$TEMP_ROOT" ] || die "cannot create private temporary root"

run_rclone_clean "version inspection" "$TEMP_ROOT/rclone-version" version
ACTUAL_RCLONE_VERSION="$(inspect_rclone_version "$TEMP_ROOT/rclone-version")"
[ "$ACTUAL_RCLONE_VERSION" = "$REVIEWED_RCLONE_VERSION" ] || \
    die "rclone version differs from the reviewed release"
run_rclone_clean "config-show capability inspection" "$TEMP_ROOT/config-show-help" \
    config show --help
inspect_config_show_capability "$TEMP_ROOT/config-show-help"

# This benign read is deliberately first: rclone refreshes an expired OAuth
# access token and persists the refreshed selected-remote token before the
# credential bytes are streamed in memory to the fixed Drive API helper.
run_rclone_clean "capacity inspection" "$TEMP_ROOT/about-before.json" \
    about "$REMOTE_ROOT" --json --contimeout 10s --timeout 20s \
    --retries 2 --low-level-retries 2
FREE_BEFORE="$(inspect_free_bytes "$TEMP_ROOT/about-before.json")"
require_uint "$FREE_BEFORE" "Drive free-byte capacity"

ACTUAL_IDENTITY="$(inspect_drive_identity "inspection")"
ACTUAL_CLIENT_SHA="${ACTUAL_IDENTITY%% *}"
ACTUAL_IDENTITY_REST="${ACTUAL_IDENTITY#* }"
ACTUAL_ACCOUNT_SHA="${ACTUAL_IDENTITY_REST%% *}"
ACTUAL_PERMISSION_SHA="${ACTUAL_IDENTITY_REST#* }"
require_hash "$ACTUAL_CLIENT_SHA" "effective OAuth client-id hash"
require_hash "$ACTUAL_ACCOUNT_SHA" "effective Drive account hash"
require_hash "$ACTUAL_PERMISSION_SHA" "effective Drive permission-id hash"
[ "$ACTUAL_CLIENT_SHA" = "$EXPECTED_CLIENT_SHA" ] || \
    die "effective OAuth client differs from the reviewed ARC client"
[ "$ACTUAL_ACCOUNT_SHA" = "$EXPECTED_ACCOUNT_SHA" ] || \
    die "effective Drive account differs from the reviewed ARC account"

REQUIRED_WITH_CANARY=$((ARCHIVE_RESERVATION_BYTES + CANARY_BYTES))
[ "$FREE_BEFORE" -ge "$REQUIRED_WITH_CANARY" ] || \
    die "Drive free-byte capacity is below the archive reservation plus canary"

FREE_AFTER="$FREE_BEFORE"
CANARY_VERIFIED=false
CANARY_DELETED=false
if [ "$MODE" = execute ]; then
    CANARY_LOCAL="$TEMP_ROOT/canary.bin"
    CANARY_DOWNLOAD="$TEMP_ROOT/canary.download"
    write_canary "$CANARY_LOCAL"
    CANARY_SHA="$(hash_file "$CANARY_LOCAL")"
    require_hash "$CANARY_SHA" "canary hash"
    CANARY_TOKEN="$(python3 -c 'import secrets; print(secrets.token_hex(32))')"
    CANARY_NAME=".arc-prefreeze-${CAPTURE_ID}-${CANARY_TOKEN}.bin"
    CANARY_REMOTE="$REMOTE_ROOT/$CANARY_NAME"

    run_rclone_clean "canary upload" /dev/null \
        copyto "$CANARY_LOCAL" "$CANARY_REMOTE" --immutable --metadata \
        --drive-stop-on-upload-limit --retries 1 --low-level-retries 2
    # Only an upload that returned cleanly may authorize cleanup deletion. A
    # failed immutable create is never guessed to own a pre-existing object.
    CANARY_PRESENT=true
    run_rclone_clean "canary download" "$CANARY_DOWNLOAD" cat "$CANARY_REMOTE"
    [ "$(hash_file "$CANARY_DOWNLOAD")" = "$CANARY_SHA" ] || \
        die "Drive canary bytes differ after upload/download"
    [ "$(python3 -c 'import os,sys; print(os.path.getsize(sys.argv[1]))' "$CANARY_DOWNLOAD")" = "$CANARY_BYTES" ] || \
        die "Drive canary size differs after upload/download"
    CANARY_VERIFIED=true

    run_rclone_clean "canary deletion" /dev/null \
        deletefile "$CANARY_REMOTE" --drive-use-trash=false
    run_rclone_clean "canary deletion verification" "$TEMP_ROOT/canary-list" \
        lsf "$REMOTE_ROOT" --files-only --include "$CANARY_NAME"
    [ ! -s "$TEMP_ROOT/canary-list" ] || die "Drive canary still exists after deletion"
    CANARY_PRESENT=false
    CANARY_DELETED=true

    # Narrow the credential/configuration TOCTOU window: the exact effective
    # tool, OAuth client, account, and Drive permission identity must still
    # match after the mutating canary.
    run_rclone_clean "post-canary version inspection" \
        "$TEMP_ROOT/rclone-version-after" version
    [ "$(inspect_rclone_version "$TEMP_ROOT/rclone-version-after")" = "$ACTUAL_RCLONE_VERSION" ] || \
        die "rclone version changed during Drive prefreeze execution"
    run_rclone_clean "post-canary capacity inspection" "$TEMP_ROOT/about-after.json" \
        about "$REMOTE_ROOT" --json --contimeout 10s --timeout 20s \
        --retries 2 --low-level-retries 2
    FREE_AFTER="$(inspect_free_bytes "$TEMP_ROOT/about-after.json")"
    require_uint "$FREE_AFTER" "post-canary Drive free-byte capacity"
    [ "$FREE_AFTER" -ge "$ARCHIVE_RESERVATION_BYTES" ] || \
        die "Drive free-byte capacity fell below the archive reservation after canary"

    POST_IDENTITY="$(inspect_drive_identity "post-canary inspection")"
    POST_CLIENT_SHA="${POST_IDENTITY%% *}"
    POST_IDENTITY_REST="${POST_IDENTITY#* }"
    POST_ACCOUNT_SHA="${POST_IDENTITY_REST%% *}"
    POST_PERMISSION_SHA="${POST_IDENTITY_REST#* }"
    [ "$POST_CLIENT_SHA" = "$ACTUAL_CLIENT_SHA" ] || \
        die "effective OAuth client changed during Drive prefreeze execution"
    [ "$POST_ACCOUNT_SHA" = "$ACTUAL_ACCOUNT_SHA" ] || \
        die "effective Drive account changed during Drive prefreeze execution"
    [ "$POST_PERMISSION_SHA" = "$ACTUAL_PERMISSION_SHA" ] || \
        die "effective Drive permission identity changed during Drive prefreeze execution"
fi

emit_receipt "$MODE" "$ACTUAL_FREEZE_SHA" "$CAPTURE_ID" "$EXPECTED_ROOT_SHA" \
    "$ACTUAL_CLIENT_SHA" "$ACTUAL_ACCOUNT_SHA" "$ACTUAL_PERMISSION_SHA" \
    "$ACTUAL_RCLONE_VERSION" "$TOTAL_SOURCE_BYTES" \
    "$ARCHIVE_RESERVATION_BYTES" "$LARGEST_OBJECT_RESERVATION_BYTES" \
    "$DAILY_BUDGET" "$FREE_BEFORE" "$FREE_AFTER" \
    "$CANARY_VERIFIED" "$CANARY_DELETED"
