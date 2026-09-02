#!/usr/bin/env bash
set -uo pipefail

TEST_DIR="$(CDPATH='' cd -- "$(dirname -- "$0")" && pwd)"
REPO_ROOT="$(CDPATH='' cd -- "$TEST_DIR/../.." && pwd)"
# shellcheck source=/dev/null
. "$TEST_DIR/helpers/testlib.sh"
GATE="$REPO_ROOT/scripts/recovery/verify-drive-prefreeze.sh"
IDENTITY_HELPER="$REPO_ROOT/scripts/recovery/drive-account-identity.py"
IDENTITY_UNIT_TEST="$TEST_DIR/test_drive_account_identity.py"

# Ubuntu exposes /usr/bin/python3 as a symlink, while the production gate
# deliberately requires the reviewed interpreter itself to be a normalized,
# executable regular non-symlink file. Resolve the system entrypoint once,
# validate the exact production-shaped path and file metadata, then hash and
# pin those resolved bytes below.
SYSTEM_PYTHON3="$(python3 -I - <<'PY'
import os
import pathlib
import re
import stat

entrypoint = pathlib.Path("/usr/bin/python3")
candidate = pathlib.Path(os.path.realpath(entrypoint))
try:
    metadata = candidate.lstat()
except OSError as error:
    raise SystemExit(f"cannot resolve the system Python fixture: {error}")
if re.fullmatch(r"/usr/bin/python3(?:\.[0-9]+)?", str(candidate)) is None:
    raise SystemExit(f"system Python resolved outside /usr/bin/python3[.VERSION]: {candidate}")
if (
    stat.S_ISLNK(metadata.st_mode)
    or not stat.S_ISREG(metadata.st_mode)
    or not os.access(candidate, os.X_OK)
    or metadata.st_uid != 0
    or stat.S_IMODE(metadata.st_mode) & 0o022
):
    raise SystemExit(f"system Python fixture is not a protected executable regular file: {candidate}")
print(candidate)
PY
)" || exit 1
readonly SYSTEM_PYTHON3

hash_text() {
    printf '%s' "$1" | python3 -c \
        'import hashlib,sys; print(hashlib.sha256(sys.stdin.buffer.read()).hexdigest())'
}

hash_file() {
    python3 - "$1" <<'PY'
import hashlib
import pathlib
import sys
print(hashlib.sha256(pathlib.Path(sys.argv[1]).read_bytes()).hexdigest())
PY
}

capture_id() {
    python3 - "$1" <<'PY'
import hashlib
import sys
print(hashlib.sha256(b"ARC recovery capture v2\0" + bytes.fromhex(sys.argv[1])).hexdigest())
PY
}

write_freeze_fixture() {
    local output="$1" size="${2:-1048576}"
    python3 - "$output" "$size" <<'PY'
import json
import pathlib
import sys
path = pathlib.Path(sys.argv[1]); size = int(sys.argv[2])
value = {"schema":"arc.recovery.freeze-plan.v3","nodes":[
    {"name":name,"data_bytes":size}
    for name in ("nyc","lax","ams","lhr","nrt","sgp")
]}
path.write_text(json.dumps(value,sort_keys=True,separators=(",",":"))+"\n",encoding="utf-8")
PY
}

write_fake_rclone() {
    local output="$1"
    cat > "$output" <<'SH'
#!/usr/bin/env bash
set -u
command_name="${1:-}"
shift || true
printf '%s %s\n' "$command_name" "$*" >> "$FAKE_RCLONE_LOG"
if [ "${FAKE_WARN_COMMAND:-}" = "$command_name" ]; then
    printf 'shared client warning\n' >&2
fi
case "$command_name" in
    version)
        printf 'rclone %s\n' "${FAKE_RCLONE_VERSION:-v1.75.0}"
        printf '%s\n' '- os/version: test' '- os/type: test' '- os/arch: test'
        ;;
    config)
        subcommand="${1:-}"
        case "$subcommand" in
            redacted)
                remote="${2:-arc-recovery-drive}"
                printf '[%s]\n' "$remote"
                printf 'type = drive\n'
                if [ "${FAKE_NO_CLIENT:-false}" != true ]; then
                    # Real rclone v1.75.0 redacts both OAuth client fields.
                    printf 'client_id = XXX\n'
                    printf 'client_secret = XXX\n'
                fi
                printf 'token = XXX\n'
                ;;
            show)
                if [ "${2:-}" = --help ]; then
                    printf '%s\n' \
                        'Print (decrypted) config file, or the config for a single remote.' \
                        'Usage:' \
                        '  rclone config show [<remote>] [flags]'
                    exit 0
                fi
                remote="${2:-}"
                [ "$remote" = arc-recovery-drive ] || exit 12
                calls_path="$FAKE_FIXTURE_ROOT/config-show-calls"
                previous_calls=0
                [ ! -f "$calls_path" ] || read -r previous_calls < "$calls_path"
                calls="$((previous_calls + 1))"
                printf '%s\n' "$calls" > "$calls_path"
                access_token="$(/bin/cat "$FAKE_TOKEN_STATE")"
                client_id=arc-recovery-client.apps.googleusercontent.com
                client_secret=private-client-secret
                if [ -e "$FAKE_FIXTURE_ROOT/no-client" ]; then
                    client_id=''
                elif [ -e "$FAKE_FIXTURE_ROOT/redacted-client" ]; then
                    client_id=XXX
                elif [ "$calls" -gt 1 ] && [ -e "$FAKE_FIXTURE_ROOT/client-switch" ]; then
                    client_id=switched-client.apps.googleusercontent.com
                fi
                if [ -e "$FAKE_FIXTURE_ROOT/no-client-secret" ]; then
                    client_secret=''
                elif [ -e "$FAKE_FIXTURE_ROOT/redacted-client-secret" ]; then
                    client_secret=XXX
                fi
                printf '[%s]\n' "$remote"
                printf 'type = drive\n'
                [ -z "$client_id" ] || printf 'client_id = %s\n' "$client_id"
                [ -z "$client_secret" ] || printf 'client_secret = %s\n' "$client_secret"
                printf 'token = {"access_token":"%s","token_type":"Bearer","refresh_token":"private-refresh-token"}\n' "$access_token"
                ;;
            *) exit 2 ;;
        esac
        ;;
    about)
        if [ "${FAKE_REFRESH_DISABLED:-false}" != true ]; then
            printf '%s\n' 'fresh-access-token' > "$FAKE_TOKEN_STATE"
        fi
        if [ "${FAKE_ABOUT_WITHOUT_FREE:-false}" = true ]; then
            printf '{"used":1}\n'
        else
            printf '{"free":%s,"used":1}\n' "${FAKE_FREE_BYTES:-1000000000000}"
        fi
        ;;
    copyto)
        [ "${FAKE_UPLOAD_FAIL:-false}" != true ] || exit 9
        cp -- "$1" "$FAKE_RCLONE_STORE"
        ;;
    cat)
        if [ "${FAKE_CORRUPT_CAT:-false}" = true ]; then
            printf corrupt
        else
            /bin/cat -- "$FAKE_RCLONE_STORE"
        fi
        ;;
    deletefile)
        [ "${FAKE_DELETE_FAIL:-false}" != true ] || exit 10
        rm -f -- "$FAKE_RCLONE_STORE"
        ;;
    lsf)
        [ ! -e "$FAKE_RCLONE_STORE" ] || printf 'canary-still-present\n'
        ;;
    *) exit 2 ;;
esac
SH
    chmod 700 "$output"
}

write_fake_identity_helper() {
    local output="$1"
    cat > "$output" <<'SH'
#!/usr/bin/python3
import hashlib
import pathlib
import sys

root = pathlib.Path(__file__).resolve().parent.parent
remote = sys.argv[1] if len(sys.argv) == 2 else ""
(root / "python-argv.log").open("a", encoding="utf-8").write(remote + "\n")
if remote != "arc-recovery-drive":
    raise SystemExit(20)
selected_config = sys.stdin.read()
expected_token = (root / "selected-token").read_text(encoding="utf-8").strip()
if expected_token == "expired-access-token":
    print("drive-account-identity: selected token was not refreshed", file=sys.stderr)
    raise SystemExit(21)
if f'"access_token":"{expected_token}"' not in selected_config:
    print("drive-account-identity: selected token does not match refreshed config", file=sys.stderr)
    raise SystemExit(21)
if "client_secret = private-client-secret" not in selected_config:
    print("drive-account-identity: selected config was not decrypted in memory", file=sys.stderr)
    raise SystemExit(22)
client_lines = [
    line.split("=", 1)[1].strip()
    for line in selected_config.splitlines()
    if line.startswith("client_id =")
]
if len(client_lines) != 1 or not client_lines[0] or client_lines[0] == "XXX":
    print("drive-account-identity: selected custom client is invalid", file=sys.stderr)
    raise SystemExit(22)
calls_path = root / "identity-calls"
calls = int(calls_path.read_text(encoding="utf-8")) + 1 if calls_path.exists() else 1
calls_path.write_text(f"{calls}\n", encoding="utf-8")
mode_path = root / "identity-mode"
mode = mode_path.read_text(encoding="utf-8").strip() if mode_path.exists() else "success"
if mode == "403":
    print("drive-account-identity: drive-about-http-403", file=sys.stderr)
    raise SystemExit(23)
if mode == "malformed":
    print("not-a-sanitized-identity")
    raise SystemExit(0)
if mode != "success":
    raise SystemExit(24)
account = "recovery@arc.example"
permission = "permission_0123456789"
if calls > 1 and (root / "account-switch").exists():
    account = "switched@arc.example"
if calls > 1 and (root / "permission-switch").exists():
    permission = "permission_switched"
print(
    hashlib.sha256(client_lines[0].encode("utf-8")).hexdigest(),
    hashlib.sha256(account.encode("utf-8")).hexdigest(),
    hashlib.sha256(permission.encode("utf-8")).hexdigest(),
)
SH
    chmod 700 "$output"
}

setup_fixture() {
    FIXTURE="$(mktemp -d)"
    mkdir -p "$FIXTURE/bin" "$FIXTURE/tmp"
    FREEZE_PLAN="$FIXTURE/freeze.json"
    write_freeze_fixture "$FREEZE_PLAN" 1048576
    FREEZE_SHA="$(hash_file "$FREEZE_PLAN")"
    CAPTURE="$(capture_id "$FREEZE_SHA")"
    REMOTE_ROOT='arc-recovery-drive:ARC Chain Recovery'
    ROOT_SHA="$(hash_text "$REMOTE_ROOT")"
    CLIENT_SHA="$(hash_text 'arc-recovery-client.apps.googleusercontent.com')"
    ACCOUNT_SHA="$(hash_text 'recovery@arc.example')"
    FAKE_RCLONE_LOG="$FIXTURE/rclone.log"
    FAKE_RCLONE_STORE="$FIXTURE/remote-canary"
    FAKE_TOKEN_STATE="$FIXTURE/selected-token"
    FAKE_IDENTITY_CALLS="$FIXTURE/identity-calls"
    FAKE_PYTHON_ARGV_LOG="$FIXTURE/python-argv.log"
    TEST_GATE="$FIXTURE/bin/verify-drive-prefreeze.sh"
    FAKE_IDENTITY_HELPER="$FIXTURE/bin/drive-account-identity.py"
    : > "$FAKE_RCLONE_LOG"
    : > "$FAKE_PYTHON_ARGV_LOG"
    printf '%s\n' 'expired-access-token' > "$FAKE_TOKEN_STATE"
    write_fake_rclone "$FIXTURE/bin/rclone"
    write_fake_identity_helper "$FAKE_IDENTITY_HELPER"
    cp -- "$GATE" "$TEST_GATE"
    local fake_helper_sha
    fake_helper_sha="$(hash_file "$FAKE_IDENTITY_HELPER")"
    python3 - "$TEST_GATE" "$fake_helper_sha" <<'PY'
import pathlib
import sys
path = pathlib.Path(sys.argv[1])
text = path.read_text(encoding="utf-8")
prefix = 'DRIVE_ACCOUNT_HELPER_SHA256="'
start = text.index(prefix) + len(prefix)
end = text.index('"', start)
path.write_text(text[:start] + sys.argv[2] + text[end:], encoding="utf-8")
PY
    chmod 700 "$TEST_GATE"
    PINNED_PYTHON_SHA="$(hash_file "$SYSTEM_PYTHON3")"
    export FAKE_RCLONE_LOG FAKE_RCLONE_STORE FAKE_TOKEN_STATE
    export FAKE_IDENTITY_CALLS FAKE_PYTHON_ARGV_LOG
    FAKE_FIXTURE_ROOT="$FIXTURE"
    export FAKE_FIXTURE_ROOT
}

gate() {
    local mode="$1"
    shift
    printf '%s\n' "${FAKE_IDENTITY_MODE:-success}" > "$FIXTURE/identity-mode"
    if [ "${FAKE_NO_CLIENT:-false}" = true ]; then
        : > "$FIXTURE/no-client"
    else
        rm -f -- "$FIXTURE/no-client"
    fi
    if [ "${FAKE_REDACTED_CLIENT:-false}" = true ]; then
        : > "$FIXTURE/redacted-client"
    else
        rm -f -- "$FIXTURE/redacted-client"
    fi
    if [ "${FAKE_NO_CLIENT_SECRET:-false}" = true ]; then
        : > "$FIXTURE/no-client-secret"
    else
        rm -f -- "$FIXTURE/no-client-secret"
    fi
    if [ "${FAKE_REDACTED_CLIENT_SECRET:-false}" = true ]; then
        : > "$FIXTURE/redacted-client-secret"
    else
        rm -f -- "$FIXTURE/redacted-client-secret"
    fi
    if [ "${FAKE_ACCOUNT_SWITCH:-false}" = true ]; then
        : > "$FIXTURE/account-switch"
    else
        rm -f -- "$FIXTURE/account-switch"
    fi
    if [ "${FAKE_PERMISSION_SWITCH:-false}" = true ]; then
        : > "$FIXTURE/permission-switch"
    else
        rm -f -- "$FIXTURE/permission-switch"
    fi
    if [ "${FAKE_CLIENT_SWITCH:-false}" = true ]; then
        : > "$FIXTURE/client-switch"
    else
        rm -f -- "$FIXTURE/client-switch"
    fi
    ARC_RECOVERY_PYTHON_PATH="$SYSTEM_PYTHON3" \
    ARC_RECOVERY_PYTHON_SHA256="$PINNED_PYTHON_SHA" \
    TMPDIR="$FIXTURE/tmp" PATH="$FIXTURE/bin:$PATH" "$TEST_GATE" "$mode" \
        --freeze-plan "$FREEZE_PLAN" \
        --expected-freeze-plan-sha256 "$FREEZE_SHA" \
        --capture-id "$CAPTURE" \
        --remote-root "$REMOTE_ROOT" \
        --expected-root-sha256 "$ROOT_SHA" \
        --expected-client-id-sha256 "$CLIENT_SHA" \
        --expected-account-sha256 "$ACCOUNT_SHA" \
        --daily-upload-budget-bytes 700000000000 "$@"
}

preflight_is_read_only_and_binds_receipt() (
    setup_fixture
    trap 'rm -rf -- "$FIXTURE"' EXIT
    local receipt
    receipt="$(gate preflight)" || return 1
    python3 - "$receipt" "$FREEZE_SHA" "$CAPTURE" "$ROOT_SHA" "$CLIENT_SHA" "$ACCOUNT_SHA" <<'PY' || return 1
import json,sys
value=json.loads(sys.argv[1])
assert value["schema"]=="arc.recovery.drive-prefreeze.v1"
assert value["mode"]=="preflight"
assert value["freeze_plan_sha256"]==sys.argv[2]
assert value["capture_id"]==sys.argv[3]
assert value["remote_root_sha256"]==sys.argv[4]
assert value["client_id_sha256"]==sys.argv[5]
assert value["account_sha256"]==sys.argv[6]
assert value["rclone_version"]=="v1.75.0"
assert not value["canary_verified"] and not value["canary_deleted"]
assert value["archive_reservation_bytes"]==3*(6*1048576)+32*1024**3
assert value["largest_object_reservation_bytes"]==3*1048576+4*1024**3
PY
    ! grep -Eq '^(copyto|cat|deletefile|lsf) ' "$FAKE_RCLONE_LOG" || return 1
    grep -Fq 'config show arc-recovery-drive' "$FAKE_RCLONE_LOG" || return 1
    ! grep -Fq 'userinfo' "$FAKE_RCLONE_LOG"
)

execute_verifies_and_deletes_canary() (
    setup_fixture
    trap 'rm -rf -- "$FIXTURE"' EXIT
    local receipt
    receipt="$(gate execute)" || return 1
    python3 - "$receipt" <<'PY' || return 1
import json,sys
value=json.loads(sys.argv[1])
assert value["mode"]=="execute"
assert value["canary_bytes"]==8*1024**2
assert value["canary_verified"] and value["canary_deleted"]
PY
    [ ! -e "$FAKE_RCLONE_STORE" ] || return 1
    grep -Fq 'copyto ' "$FAKE_RCLONE_LOG" || return 1
    grep -Fq -- '--drive-stop-on-upload-limit' "$FAKE_RCLONE_LOG" || return 1
    grep -Fq 'cat ' "$FAKE_RCLONE_LOG" || return 1
    grep -Fq 'deletefile ' "$FAKE_RCLONE_LOG" || return 1
    grep -Fq 'lsf ' "$FAKE_RCLONE_LOG"
)

custom_client_account_and_root_fail_closed() (
    setup_fixture
    trap 'rm -rf -- "$FIXTURE"' EXIT
    FAKE_NO_CLIENT=true gate preflight >/dev/null 2>&1 && return 1
    unset FAKE_NO_CLIENT
    FAKE_REDACTED_CLIENT=true gate preflight >/dev/null 2>&1 && return 1
    unset FAKE_REDACTED_CLIENT
    FAKE_NO_CLIENT_SECRET=true gate preflight >/dev/null 2>&1 && return 1
    unset FAKE_NO_CLIENT_SECRET
    FAKE_REDACTED_CLIENT_SECRET=true gate preflight >/dev/null 2>&1 && return 1
    unset FAKE_REDACTED_CLIENT_SECRET
    local original="$CLIENT_SHA"
    CLIENT_SHA="$(printf '0%.0s' {1..64})"
    gate preflight >/dev/null 2>&1 && return 1
    CLIENT_SHA="$original"
    ACCOUNT_SHA="$(printf '1%.0s' {1..64})"
    gate preflight >/dev/null 2>&1 && return 1
    ACCOUNT_SHA="$(hash_text 'recovery@arc.example')"
    ROOT_SHA="$(printf '2%.0s' {1..64})"
    gate preflight >/dev/null 2>&1 && return 1
    ROOT_SHA="$(hash_text "$REMOTE_ROOT")"
    REMOTE_ROOT='arc-drive:ARC Chain Recovery'
    ROOT_SHA="$(hash_text "$REMOTE_ROOT")"
    gate preflight >/dev/null 2>&1 && return 1
    return 0
)

every_stderr_warning_is_fatal() (
    setup_fixture
    trap 'rm -rf -- "$FIXTURE"' EXIT
    local command_name
    for command_name in version config about; do
        FAKE_WARN_COMMAND="$command_name" gate preflight >/dev/null 2>&1 && return 1
    done
    FAKE_WARN_COMMAND='cat' gate execute >/dev/null 2>&1 && return 1
    return 0
)

rclone_version_and_selected_config_capability_are_bound() (
    setup_fixture
    trap 'rm -rf -- "$FIXTURE"' EXIT
    FAKE_RCLONE_VERSION=v1.74.1 gate preflight >/dev/null 2>&1 && return 1
    FAKE_RCLONE_VERSION=v1.75.0 gate preflight >/dev/null || return 1
    grep -Fq 'config show --help' "$FAKE_RCLONE_LOG" || return 1
    grep -Fq 'config show arc-recovery-drive' "$FAKE_RCLONE_LOG" || return 1
    ! grep -Fq 'userinfo' "$GATE"
)

expired_token_refreshes_before_identity_and_api_failures_are_fatal() (
    setup_fixture
    trap 'rm -rf -- "$FIXTURE"' EXIT
    FAKE_REFRESH_DISABLED=true gate preflight >/dev/null 2>&1 && return 1
    rm -f -- "$FAKE_IDENTITY_CALLS"
    printf '%s\n' 'expired-access-token' > "$FAKE_TOKEN_STATE"
    gate preflight >/dev/null || return 1
    [ "$(/bin/cat "$FAKE_TOKEN_STATE")" = fresh-access-token ] || return 1
    local about_line identity_line
    about_line="$(grep -n '^about ' "$FAKE_RCLONE_LOG" | tail -1 | cut -d: -f1)"
    identity_line="$(grep -n '^config show arc-recovery-drive$' "$FAKE_RCLONE_LOG" | tail -1 | cut -d: -f1)"
    [ "$about_line" -lt "$identity_line" ] || return 1

    rm -f -- "$FAKE_IDENTITY_CALLS"
    FAKE_IDENTITY_MODE=403 gate preflight >/dev/null 2>&1 && return 1
    rm -f -- "$FAKE_IDENTITY_CALLS"
    FAKE_IDENTITY_MODE=malformed gate preflight >/dev/null 2>&1 && return 1
    return 0
)

account_and_permission_switch_after_canary_fail_closed() (
    setup_fixture
    trap 'rm -rf -- "$FIXTURE"' EXIT
    FAKE_ACCOUNT_SWITCH=true gate execute >/dev/null 2>&1 && return 1
    rm -f -- "$FAKE_RCLONE_STORE" "$FAKE_IDENTITY_CALLS"
    FAKE_PERMISSION_SWITCH=true gate execute >/dev/null 2>&1 && return 1
    rm -f -- "$FAKE_RCLONE_STORE" "$FAKE_IDENTITY_CALLS" \
        "$FIXTURE/config-show-calls"
    FAKE_CLIENT_SWITCH=true gate execute >/dev/null 2>&1 && return 1
    return 0
)

token_config_and_tls_overrides_do_not_leak_or_bypass() (
    setup_fixture
    trap 'rm -rf -- "$FIXTURE"' EXIT
    local receipt="$FIXTURE/receipt" error="$FIXTURE/error"
    gate preflight > "$receipt" 2> "$error" || return 1
    for secret in fresh-access-token private-refresh-token private-client-secret; do
        ! grep -Fq "$secret" "$receipt" || return 1
        ! grep -Fq "$secret" "$error" || return 1
        ! grep -Fq "$secret" "$FAKE_RCLONE_LOG" || return 1
        ! grep -Fq "$secret" "$FAKE_PYTHON_ARGV_LOG" || return 1
    done
    ! find "$FIXTURE/tmp" -name 'arc-drive-prefreeze.*' -print -quit | grep -q . || return 1

    RCLONE_DRIVE_TOKEN=forbidden gate preflight >/dev/null 2>&1 && return 1
    RCLONE_CONFIG_ARC_RECOVERY_DRIVE_TOKEN=forbidden \
        gate preflight >/dev/null 2>&1 && return 1
    SSLKEYLOGFILE="$FIXTURE/tls.keys" gate preflight >/dev/null 2>&1 && return 1
    [ ! -e "$FIXTURE/tls.keys" ]
)

identity_helper_unit_contract_and_real_rclone_capability() (
    "$SYSTEM_PYTHON3" -I "$IDENTITY_UNIT_TEST" || return 1
    ! grep -Fq 'config userinfo' "$GATE" || return 1
    ! grep -Fq 'config redacted' "$GATE" || return 1

    local real_rclone real_version config output
    real_rclone="$(command -v rclone || true)"
    [ -n "$real_rclone" ] || return 0
    real_version="$("$real_rclone" version 2>/dev/null | sed -n '1p')"
    [ "$real_version" = 'rclone v1.75.0' ] || return 0
    config="$(mktemp)"
    chmod 600 "$config"
    cat > "$config" <<'EOF'
[capability-drive]
type = drive
client_id = test.apps.googleusercontent.com
client_secret = fake-secret
token = {"access_token":"fake-access","token_type":"Bearer","refresh_token":"fake-refresh","expiry":"2099-01-01T00:00:00Z"}
EOF
    output="$($real_rclone --config "$config" config redacted capability-drive 2>/dev/null)" || {
        rm -f -- "$config"
        return 1
    }
    [ "$(printf '%s\n' "$output" | sed -n 's/^client_id = //p')" = XXX ] || {
        rm -f -- "$config"
        return 1
    }
    [ "$(printf '%s\n' "$output" | sed -n 's/^client_secret = //p')" = XXX ] || {
        rm -f -- "$config"
        return 1
    }
    output="$($real_rclone --config "$config" config show capability-drive 2>/dev/null)" || {
        rm -f -- "$config"
        return 1
    }
    printf '%s\n' "$output" | grep -Fq 'client_id = test.apps.googleusercontent.com' || {
        rm -f -- "$config"
        return 1
    }
    "$real_rclone" --config "$config" config userinfo capability-drive: --json \
        >/dev/null 2>&1 && {
            rm -f -- "$config"
            return 1
        }
    output=''
    rm -f -- "$config"
)

capacity_budget_and_object_limits_fail_closed() (
    setup_fixture
    trap 'rm -rf -- "$FIXTURE"' EXIT
    FAKE_FREE_BYTES=1000 gate preflight >/dev/null 2>&1 && return 1
    FAKE_ABOUT_WITHOUT_FREE=true gate preflight >/dev/null 2>&1 && return 1

    gate preflight --daily-upload-budget-bytes 750000000001 \
        > /dev/null 2> "$FIXTURE/hard-cap.err" && return 1
    grep -Fq 'exceeds the 750 GB (750,000,000,000 byte)' \
        "$FIXTURE/hard-cap.err" || return 1

    write_freeze_fixture "$FREEZE_PLAN" $((50 * 1024 * 1024 * 1024))
    FREEZE_SHA="$(hash_file "$FREEZE_PLAN")"; CAPTURE="$(capture_id "$FREEZE_SHA")"
    gate preflight > /dev/null 2> "$FIXTURE/daily.err" && return 1
    grep -Fq 'archive reservation exceeds the reviewed daily upload budget' \
        "$FIXTURE/daily.err" || return 1

    write_freeze_fixture "$FREEZE_PLAN" 2000000000000
    FREEZE_SHA="$(hash_file "$FREEZE_PLAN")"; CAPTURE="$(capture_id "$FREEZE_SHA")"
    gate preflight > /dev/null 2> "$FIXTURE/object.err" && return 1
    grep -Fq 'largest archive object reservation exceeds the 5 TB' \
        "$FIXTURE/object.err" || return 1
    return 0
)

canary_corruption_or_delete_failure_is_fatal() (
    setup_fixture
    trap 'rm -rf -- "$FIXTURE"' EXIT
    FAKE_CORRUPT_CAT=true gate execute >/dev/null 2>&1 && return 1
    rm -f -- "$FAKE_RCLONE_STORE"
    FAKE_DELETE_FAIL=true gate execute >/dev/null 2>&1 && return 1
    return 0
)

freeze_and_capture_binding_fail_closed() (
    setup_fixture
    trap 'rm -rf -- "$FIXTURE"' EXIT
    FREEZE_SHA="$(printf '3%.0s' {1..64})"
    gate preflight >/dev/null 2>&1 && return 1
    FREEZE_SHA="$(hash_file "$FREEZE_PLAN")"
    CAPTURE="$(printf '4%.0s' {1..64})"
    gate preflight >/dev/null 2>&1 && return 1
    return 0
)

scripts_are_lintable() {
    bash -n "$GATE" "$0" && \
        "$SYSTEM_PYTHON3" -m py_compile "$IDENTITY_HELPER" "$IDENTITY_UNIT_TEST" && \
        shellcheck -S warning "$GATE" "$0"
}

run_test 'prefreeze mode is read-only and emits bound receipt' preflight_is_read_only_and_binds_receipt
run_test 'execute mode verifies and deletes exact canary' execute_verifies_and_deletes_canary
run_test 'custom client account and root fail closed' custom_client_account_and_root_fail_closed
run_test 'any rclone stderr warning is fatal' every_stderr_warning_is_fatal
run_test 'rclone version and selected-config capability are bound' rclone_version_and_selected_config_capability_are_bound
run_test 'expired token refresh and API failures fail closed' expired_token_refreshes_before_identity_and_api_failures_are_fatal
run_test 'post-canary client account and permission switches fail closed' account_and_permission_switch_after_canary_fail_closed
run_test 'token config and TLS overrides neither leak nor bypass' token_config_and_tls_overrides_do_not_leak_or_bypass
run_test 'identity helper and real rclone capability are regression tested' identity_helper_unit_contract_and_real_rclone_capability
run_test 'capacity budget and object limits fail closed' capacity_budget_and_object_limits_fail_closed
run_test 'canary corruption and deletion failure are fatal' canary_corruption_or_delete_failure_is_fatal
run_test 'freeze and capture identities fail closed' freeze_and_capture_binding_fail_closed
run_test 'Drive prefreeze gate scripts pass syntax and lint' scripts_are_lintable
finish_tests
