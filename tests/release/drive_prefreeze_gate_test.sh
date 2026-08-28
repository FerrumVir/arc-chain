#!/usr/bin/env bash
set -uo pipefail

TEST_DIR="$(CDPATH='' cd -- "$(dirname -- "$0")" && pwd)"
REPO_ROOT="$(CDPATH='' cd -- "$TEST_DIR/../.." && pwd)"
# shellcheck source=/dev/null
. "$TEST_DIR/helpers/testlib.sh"
GATE="$REPO_ROOT/scripts/recovery/verify-drive-prefreeze.sh"

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
    config)
        subcommand="${1:-}"
        case "$subcommand" in
            redacted)
                remote="${2:-arc-recovery-drive}"
                printf '[%s]\n' "$remote"
                printf 'type = drive\n'
                if [ "${FAKE_NO_CLIENT:-false}" != true ]; then
                    printf 'client_id = arc-recovery-client.apps.googleusercontent.com\n'
                    printf 'client_secret = XXX\n'
                fi
                printf 'token = XXX\n'
                ;;
            userinfo)
                printf '{"Email":"%s"}\n' "${FAKE_ACCOUNT:-recovery@arc.example}"
                ;;
            *) exit 2 ;;
        esac
        ;;
    about)
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

setup_fixture() {
    FIXTURE="$(mktemp -d)"
    mkdir -p "$FIXTURE/bin"
    FREEZE_PLAN="$FIXTURE/freeze.json"
    write_freeze_fixture "$FREEZE_PLAN" "${1:-1048576}"
    FREEZE_SHA="$(hash_file "$FREEZE_PLAN")"
    CAPTURE="$(capture_id "$FREEZE_SHA")"
    REMOTE_ROOT='arc-recovery-drive:ARC Chain Recovery'
    ROOT_SHA="$(hash_text "$REMOTE_ROOT")"
    CLIENT_SHA="$(hash_text 'arc-recovery-client.apps.googleusercontent.com')"
    ACCOUNT_SHA="$(hash_text 'recovery@arc.example')"
    FAKE_RCLONE_LOG="$FIXTURE/rclone.log"
    FAKE_RCLONE_STORE="$FIXTURE/remote-canary"
    : > "$FAKE_RCLONE_LOG"
    write_fake_rclone "$FIXTURE/bin/rclone"
    export FAKE_RCLONE_LOG FAKE_RCLONE_STORE
}

gate() {
    local mode="$1"
    shift
    PATH="$FIXTURE/bin:$PATH" "$GATE" "$mode" \
        --freeze-plan "$FREEZE_PLAN" \
        --expected-freeze-plan-sha256 "$FREEZE_SHA" \
        --capture-id "$CAPTURE" \
        --remote-root "$REMOTE_ROOT" \
        --expected-root-sha256 "$ROOT_SHA" \
        --expected-client-id-sha256 "$CLIENT_SHA" \
        --expected-account-sha256 "$ACCOUNT_SHA" \
        --daily-upload-budget-bytes 751619276800 "$@"
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
assert not value["canary_verified"] and not value["canary_deleted"]
assert value["archive_reservation_bytes"]==3*(6*1048576)+32*1024**3
assert value["largest_object_reservation_bytes"]==3*1048576+4*1024**3
PY
    ! grep -Eq '^(copyto|cat|deletefile|lsf) ' "$FAKE_RCLONE_LOG"
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
    for command_name in config about; do
        FAKE_WARN_COMMAND="$command_name" gate preflight >/dev/null 2>&1 && return 1
    done
    FAKE_WARN_COMMAND='cat' gate execute >/dev/null 2>&1 && return 1
    return 0
)

capacity_budget_and_object_limits_fail_closed() (
    setup_fixture
    trap 'rm -rf -- "$FIXTURE"' EXIT
    FAKE_FREE_BYTES=1000 gate preflight >/dev/null 2>&1 && return 1
    FAKE_ABOUT_WITHOUT_FREE=true gate preflight >/dev/null 2>&1 && return 1

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
    bash -n "$GATE" "$0" && shellcheck -S warning "$GATE" "$0"
}

run_test 'prefreeze mode is read-only and emits bound receipt' preflight_is_read_only_and_binds_receipt
run_test 'execute mode verifies and deletes exact canary' execute_verifies_and_deletes_canary
run_test 'custom client account and root fail closed' custom_client_account_and_root_fail_closed
run_test 'any rclone stderr warning is fatal' every_stderr_warning_is_fatal
run_test 'capacity budget and object limits fail closed' capacity_budget_and_object_limits_fail_closed
run_test 'canary corruption and deletion failure are fatal' canary_corruption_or_delete_failure_is_fatal
run_test 'freeze and capture identities fail closed' freeze_and_capture_binding_fail_closed
run_test 'Drive prefreeze gate scripts pass syntax and lint' scripts_are_lintable
finish_tests
