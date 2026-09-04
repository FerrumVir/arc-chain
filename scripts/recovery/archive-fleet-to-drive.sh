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
DRIVE_PREFREEZE_GATE="$SCRIPT_DIR/verify-drive-prefreeze.sh"
LEGACY_HEIGHT_TOOL="$SCRIPT_DIR/legacy-public-height.py"
RECOVERY_FREEZE_MODULE="$SCRIPT_DIR/recovery_freeze.py"
QUARANTINE_ROUND_DRIVER="$SCRIPT_DIR/quarantine_round_driver.py"
QUARANTINE_ROUND_MODULE="$SCRIPT_DIR/quarantine_rounds.py"
LATE_FORK_INTERLOCK_TOOL="$SCRIPT_DIR/legacy-late-fork-interlock.py"
DRIVE_REMOTE="${ARC_RECOVERY_DRIVE_REMOTE:-arc-drive-arc:ARC Chain Recovery v0.8}"
SSH_USER="${ARC_RECOVERY_SSH_USER:-root}"

NODES=(
    'nyc=149.28.32.76'
    'lax=140.82.16.112'
    'ams=136.244.109.1'
    'lhr=104.238.171.11'
    'nrt=202.182.107.41'
    'sgp=149.28.153.31'
)
PRETAG_ARTIFACT_KEYS=(
    pretag_raw_headless_linux_x86_64
    pretag_raw_headless_linux_arm64
    pretag_raw_headless_macos_arm64
    pretag_raw_headless_macos_x86_64
    pretag_raw_headless_windows_x86_64
    pretag_raw_desktop_linux_x86_64
    pretag_raw_desktop_macos_arm64
    pretag_raw_desktop_macos_x86_64
    pretag_raw_desktop_windows_x86_64
)
PRETAG_ARCHIVE_NAMES=(
    pretag-headless-linux-x86_64.actions.zip
    pretag-headless-linux-arm64.actions.zip
    pretag-headless-macos-arm64.actions.zip
    pretag-headless-macos-x86_64.actions.zip
    pretag-headless-windows-x86_64.actions.zip
    pretag-desktop-linux-x86_64.actions.zip
    pretag-desktop-macos-arm64.actions.zip
    pretag-desktop-macos-x86_64.actions.zip
    pretag-desktop-windows-x86_64.actions.zip
)
SSH_OPTIONS=(-o BatchMode=yes -o ConnectTimeout=10 -o StrictHostKeyChecking=yes)
ARCHIVE_FLEET_TEMP_ROOT=""
ARCHIVE_FLEET_PINNED_ROOT=""
ARCHIVE_FLEET_PINNED_TRANSPORT_ROOT=""
ARCHIVE_FLEET_PINNED_PYTHON_ROOT=""
OPERATOR_FREEZE_PLAN=""
ARC_OPERATOR_TRANSPORT_READY=false
ARC_OPERATOR_TRANSPORT_RCLONE=false
ARC_OPERATOR_PYTHON_READY=false
ARC_OPERATOR_PYTHON_SHA256=""
ARC_OPERATOR_PYTHON_SOURCE=""
ARC_OPERATOR_SSH_SHA256=""
ARC_OPERATOR_SCP_SHA256=""
ARC_OPERATOR_RCLONE_SHA256=""
ARC_OPERATOR_SSH_KNOWN_HOSTS_SHA256=""
ARC_OPERATOR_SSH_IDENTITY_SHA256=""
ARC_OPERATOR_SSH_BIN=/usr/bin/ssh
ARC_OPERATOR_SCP_BIN=/usr/bin/scp
ARC_OPERATOR_RCLONE_BIN=""
ARC_OPERATOR_KNOWN_HOSTS=""
ARC_OPERATOR_IDENTITY=""
ARC_OPERATOR_RCLONE_CONFIG=""
ARC_OPERATOR_PYTHON_BIN=""
ARC_OPERATOR_GH_READY=false
ARC_OPERATOR_GH_BIN=""
ARC_OPERATOR_GH_SHA256=""
ARC_OPERATOR_GH_LOGIN=""
ARC_OPERATOR_GH_TOKEN=""
ARC_OPERATOR_GH_HOME=""

cleanup_temporary_root() {
    # Only ever installed as the invocation EXIT trap. Every rm was previously
    # unchecked, so a partial sweep -- the pinned SSH identity or the rclone
    # config surviving on a read-only mount, a busy file, a chmod'd parent --
    # was indistinguishable from a clean one. Report each surviving root by
    # exact path so an operator can remove it before retrying.
    local cleanup_status=0 root
    ARC_OPERATOR_GH_TOKEN=""
    for root in "$ARCHIVE_FLEET_TEMP_ROOT" "$ARCHIVE_FLEET_PINNED_ROOT" \
        "$ARCHIVE_FLEET_PINNED_TRANSPORT_ROOT" "$ARCHIVE_FLEET_PINNED_PYTHON_ROOT"; do
        [ -n "$root" ] || continue
        rm -rf -- "$root" || cleanup_status=1
        if [ -e "$root" ] || [ -L "$root" ]; then
            printf 'archive fleet: FATAL credential sweep incomplete: %s\n' "$root" >&2
            cleanup_status=1
        fi
    done
    return "$cleanup_status"
}

begin_temporary_scope() {
    # Each dispatched command runs as the leader of its supervised process
    # group. Never inherit ownership of a caller's temporary roots or
    # initialized wrappers: every invocation allocates and cleans only its own
    # private runtime state.
    ARCHIVE_FLEET_TEMP_ROOT=""
    ARCHIVE_FLEET_PINNED_ROOT=""
    ARCHIVE_FLEET_PINNED_TRANSPORT_ROOT=""
    ARCHIVE_FLEET_PINNED_PYTHON_ROOT=""
    ARC_OPERATOR_TRANSPORT_READY=false
    ARC_OPERATOR_TRANSPORT_RCLONE=false
    ARC_OPERATOR_PYTHON_READY=false
    ARC_OPERATOR_GH_READY=false
    ARC_OPERATOR_GH_TOKEN=""
    trap cleanup_temporary_root EXIT
    trap 'exit 129' HUP
    trap 'exit 130' INT
    trap 'exit 143' TERM
}

die() {
    printf 'archive fleet: %s\n' "$*" >&2
    exit 1
}

bootstrap_hash_file() {
    local output digest
    if [ -x /usr/bin/sha256sum ]; then
        output="$(/usr/bin/sha256sum -- "$1")" || die "cannot hash protected file: $1"
    elif [ -x /usr/bin/shasum ]; then
        output="$(/usr/bin/shasum -a 256 -- "$1")" || die "cannot hash protected file: $1"
    else
        die "absolute system SHA-256 utility is unavailable"
    fi
    digest="${output%% *}"
    require_hash "$digest" "protected file hash"
    printf '%s\n' "$digest"
}

configure_operator_python() {
    if [ "$ARC_OPERATOR_PYTHON_READY" = true ]; then
        [ "$(bootstrap_hash_file "$ARC_OPERATOR_PYTHON_BIN")" = "$ARC_OPERATOR_PYTHON_SHA256" ] || \
            die "pinned Python executable changed during the operator transaction"
        return 0
    fi
    local python_path="${ARC_RECOVERY_PYTHON_PATH:-}"
    local python_sha="${ARC_RECOVERY_PYTHON_SHA256:-}"
    case "$python_path" in
        /usr/bin/python3|/usr/bin/python3.[0-9]*) ;;
        *) die "ARC_RECOVERY_PYTHON_PATH must be one normalized /usr/bin/python3[.VERSION] path" ;;
    esac
    require_hash "$python_sha" "operator Python executable hash"
    require_absolute_file "$python_path" "operator Python executable"
    [ "$(bootstrap_hash_file "$python_path")" = "$python_sha" ] || \
        die "operator Python differs from its reviewed SHA-256"
    local runtime
    runtime="$(mktemp -d)"
    ARCHIVE_FLEET_PINNED_PYTHON_ROOT="$runtime"
    chmod 700 "$runtime"
    /usr/bin/env -i HOME=/var/empty PATH=/usr/bin:/bin LANG=C LC_ALL=C \
        "$python_path" -I - "$python_path" "$python_sha" <<'PY'
import hashlib, os, pathlib, stat, sys
source = pathlib.Path(sys.argv[1]); expected = sys.argv[2]
if os.fspath(source) not in {"/usr/bin/python3"} and not os.fspath(source).startswith("/usr/bin/python3."):
    raise SystemExit("operator Python path escaped the reviewed system boundary")
fd = os.open(source, os.O_RDONLY | getattr(os, "O_NOFOLLOW", 0) | getattr(os, "O_CLOEXEC", 0))
try:
    before = os.fstat(fd); visible = os.lstat(source)
    identity = lambda value: (value.st_dev, value.st_ino, value.st_mode, value.st_uid,
                              value.st_gid, value.st_nlink, value.st_size,
                              value.st_mtime_ns, value.st_ctime_ns)
    if (not stat.S_ISREG(before.st_mode) or stat.S_ISLNK(visible.st_mode)
            or identity(before) != identity(visible) or before.st_uid != 0
            or stat.S_IMODE(before.st_mode) & 0o022 or before.st_nlink < 1
            or before.st_size <= 0 or before.st_size > 128 * 1024 * 1024):
        raise SystemExit("operator Python owner/mode/type/link/size contract differs")
    chunks = []
    while True:
        chunk = os.read(fd, 1024 * 1024)
        if not chunk: break
        chunks.append(chunk)
    payload = b"".join(chunks); after = os.fstat(fd)
    if identity(before) != identity(after) or hashlib.sha256(payload).hexdigest() != expected:
        raise SystemExit("operator Python changed or differs from its reviewed SHA-256")
finally:
    os.close(fd)
PY
    ARC_OPERATOR_PYTHON_BIN="$python_path"
    ARC_OPERATOR_PYTHON_SHA256="$python_sha"
    ARC_OPERATOR_PYTHON_SOURCE="$python_path"
    ARC_OPERATOR_PYTHON_READY=true
    [ "$(bootstrap_hash_file "$ARC_OPERATOR_PYTHON_BIN")" = "$ARC_OPERATOR_PYTHON_SHA256" ] || \
        die "reviewed Python changed after validation"
}

python3() {
    if [ "$ARC_OPERATOR_PYTHON_READY" != true ]; then
        command python3 "$@"
        return
    fi
    [ "$(bootstrap_hash_file "$ARC_OPERATOR_PYTHON_BIN")" = "$ARC_OPERATOR_PYTHON_SHA256" ] || \
        die "pinned Python executable changed during the operator transaction"
    /usr/bin/env -i HOME="$ARCHIVE_FLEET_PINNED_PYTHON_ROOT" PATH=/usr/bin:/bin \
        LANG=C LC_ALL=C "$ARC_OPERATOR_PYTHON_BIN" -I "$@"
}

assert_github_anchor_tool() {
    [ "$ARC_OPERATOR_GH_READY" = true ] || die "GitHub Gist anchor transport is not initialized"
    [ "$(bootstrap_hash_file "$ARC_OPERATOR_GH_BIN")" = "$ARC_OPERATOR_GH_SHA256" ] || \
        die "reviewed GitHub CLI changed during the operator transaction"
}

gh_api() {
    assert_github_anchor_tool
    local result
    if /usr/bin/env -i HOME="$ARC_OPERATOR_GH_HOME" PATH=/usr/bin:/bin \
        LANG=C LC_ALL=C GH_HOST=github.com GH_TOKEN="$ARC_OPERATOR_GH_TOKEN" \
        GH_PROMPT_DISABLED=1 GH_PAGER=cat NO_COLOR=1 \
        "$ARC_OPERATOR_GH_BIN" api "$@"; then
        result=0
    else
        result=$?
    fi
    assert_github_anchor_tool
    return "$result"
}

configure_github_anchor_transport() {
    if [ "$ARC_OPERATOR_GH_READY" = true ]; then
        assert_github_anchor_tool
        return 0
    fi
    local gh_path="${ARC_RECOVERY_GH_PATH:-}"
    local gh_sha="${ARC_RECOVERY_GH_SHA256:-}"
    local login="${ARC_RECOVERY_GITHUB_LOGIN:-}"
    local operator_home="${HOME:-}"
    case "$login" in
        ''|*[!A-Za-z0-9-]*|-*|*-) die "ARC_RECOVERY_GITHUB_LOGIN is malformed" ;;
    esac
    [ "${#login}" -le 39 ] || die "ARC_RECOVERY_GITHUB_LOGIN is malformed"
    case "$operator_home" in /*) ;; *) die "operator HOME must be absolute for GitHub keychain authentication" ;; esac
    require_hash "$gh_sha" "GitHub CLI executable hash"
    require_absolute_file "$gh_path" "GitHub CLI executable"
    python3 - "$gh_path" "$gh_sha" <<'PY'
import hashlib, os, pathlib, stat, sys
path = pathlib.Path(sys.argv[1]); expected = sys.argv[2]
fd = os.open(path, os.O_RDONLY | getattr(os, "O_NOFOLLOW", 0) | getattr(os, "O_CLOEXEC", 0))
try:
    before = os.fstat(fd); visible = os.lstat(path)
    identity = lambda value: (value.st_dev, value.st_ino, value.st_mode, value.st_uid,
                              value.st_gid, value.st_nlink, value.st_size,
                              value.st_mtime_ns, value.st_ctime_ns)
    if (not stat.S_ISREG(before.st_mode) or stat.S_ISLNK(visible.st_mode)
            or identity(before) != identity(visible) or before.st_uid not in {0, os.getuid()}
            or before.st_mode & 0o022 or before.st_nlink != 1
            or before.st_size <= 0 or before.st_size > 256 * 1024 * 1024):
        raise SystemExit("GitHub CLI owner/mode/type/link/size contract differs")
    digest = hashlib.sha256()
    while True:
        chunk = os.read(fd, 1024 * 1024)
        if not chunk: break
        digest.update(chunk)
    after = os.fstat(fd)
    if identity(before) != identity(after) or digest.hexdigest() != expected:
        raise SystemExit("GitHub CLI changed or differs from its reviewed SHA-256")
finally:
    os.close(fd)
PY
    local token actual_login
    token="$(/usr/bin/env -i HOME="$operator_home" PATH=/usr/bin:/bin \
        LANG=C LC_ALL=C GH_HOST=github.com GH_PROMPT_DISABLED=1 NO_COLOR=1 \
        "$gh_path" auth token --hostname github.com --user "$login")" || \
        die "cannot obtain the authenticated GitHub token for the exact anchor owner"
    [ -n "$token" ] || die "GitHub CLI returned an empty authentication token"
    ARC_OPERATOR_GH_BIN="$gh_path"
    ARC_OPERATOR_GH_SHA256="$gh_sha"
    ARC_OPERATOR_GH_LOGIN="$login"
    ARC_OPERATOR_GH_TOKEN="$token"
    ARC_OPERATOR_GH_HOME="$operator_home"
    ARC_OPERATOR_GH_READY=true
    actual_login="$(gh_api /user --jq .login)" || die "cannot authenticate the GitHub Gist anchor owner"
    [ "$actual_login" = "$login" ] || die "authenticated GitHub account differs from ARC_RECOVERY_GITHUB_LOGIN"
    assert_github_anchor_tool
}

run_github_gist_anchor_canary() (
    local freeze_sha="$1" capture_id="$2" output="$3"
    require_hash "$freeze_sha" "Gist canary freeze-plan hash"
    require_hash "$capture_id" "Gist canary capture id"
    configure_github_anchor_transport
    local temporary challenge filename description content created_id="" created_revision=""
    temporary="$(mktemp -d)"
    challenge="$(python3 -c 'import secrets; print(secrets.token_hex(32))')"
    require_hash "$challenge" "Gist canary challenge"
    filename="arc-recovery-gist-canary-$challenge.txt"
    description="ARC recovery Gist permission canary $challenge"
    content="freeze_plan_sha256=$freeze_sha
capture_id=$capture_id
challenge=$challenge
"
    python3 - "$temporary/create.json" "$description" "$filename" "$content" <<'PY'
import json, pathlib, sys
path=pathlib.Path(sys.argv[1]);request={"description":sys.argv[2],"public":False,"files":{sys.argv[3]:{"content":sys.argv[4]}}}
path.write_text(json.dumps(request,sort_keys=True,separators=(",",":"))+"\n",encoding="utf-8")
PY
    cleanup_canary() {
        if [ -n "$created_id" ]; then
            gh_api --method DELETE "/gists/$created_id" >/dev/null 2>&1 || true
        fi
        if [ -n "$temporary" ] && [ -d "$temporary" ] && [ ! -L "$temporary" ]; then
            find "$temporary" -depth -delete 2>/dev/null || true
        fi
    }
    trap cleanup_canary EXIT
    gh_api --method POST /gists --input "$temporary/create.json" > "$temporary/created.json" || \
        die "GitHub Gist write canary create failed"
    local created_tuple
    created_tuple="$(python3 - "$temporary/created.json" \
        "$ARC_OPERATOR_GH_LOGIN" "$description" "$filename" "$content" <<'PY'
import json,pathlib,re,sys
value=json.loads(pathlib.Path(sys.argv[1]).read_text(encoding="utf-8"));login,description,filename,content=sys.argv[2:]
history=value.get("history");revision=history[0].get("version","") if isinstance(history,list) and history and isinstance(history[0],dict) else ""
if (re.fullmatch(r"[0-9a-f]{20,64}",str(value.get("id",""))) is None
        or re.fullmatch(r"[0-9a-f]{40}",str(revision)) is None
        or value.get("public") is not False or value.get("description") != description
        or not isinstance(value.get("owner"),dict) or value["owner"].get("login") != login
        or not isinstance(value.get("files"),dict) or set(value["files"]) != {filename}
        or value["files"][filename].get("content") != content):
    raise SystemExit("created Gist canary identity/content differs")
print(value["id"],revision)
PY
)"
    created_id="${created_tuple%% *}"; created_revision="${created_tuple#* }"
    gh_api "/gists/$created_id/$created_revision" > "$temporary/revision.json" || \
        die "GitHub Gist write canary immutable revision read failed"
    python3 - "$temporary/revision.json" "$created_id" "$created_revision" \
        "$ARC_OPERATOR_GH_LOGIN" "$description" "$filename" "$content" <<'PY'
import json,pathlib,sys
v=json.loads(pathlib.Path(sys.argv[1]).read_text(encoding="utf-8"));gid,revision,login,description,filename,content=sys.argv[2:]
if (v.get("id") != gid or v.get("public") is not False or v.get("description") != description
        or not isinstance(v.get("owner"),dict) or v["owner"].get("login") != login
        or not isinstance(v.get("history"),list) or not v["history"] or v["history"][0].get("version") != revision
        or not isinstance(v.get("files"),dict) or set(v["files"]) != {filename}
        or v["files"][filename].get("truncated") is not False
        or v["files"][filename].get("content") != content):
    raise SystemExit("Gist canary immutable revision differs")
PY
    gh_api --method DELETE "/gists/$created_id" > "$temporary/delete.out" || \
        die "GitHub Gist write canary delete failed"
    local delete_status=0
    gh_api --include "/gists/$created_id" > "$temporary/deleted-check.out" 2>&1 || delete_status=$?
    [ "$delete_status" -ne 0 ] || die "deleted GitHub Gist canary remains readable"
    python3 - "$temporary/deleted-check.out" <<'PY'
import pathlib,re,sys
raw=pathlib.Path(sys.argv[1]).read_text(encoding="utf-8",errors="replace")
if re.search(r"(?m)^HTTP/(?:1\.1|2(?:\.0)?) 404(?: |$)",raw) is None:
    raise SystemExit("GitHub Gist canary deletion did not return an authenticated 404")
PY
    created_id_for_receipt="$created_id"
    created_id=""
    python3 - "$output" "$freeze_sha" "$capture_id" "$challenge" "$filename" \
        "$content" "$created_id_for_receipt" "$created_revision" "$ARC_OPERATOR_GH_LOGIN" \
        "$ARC_OPERATOR_GH_BIN" "$ARC_OPERATOR_GH_SHA256" <<'PY'
import datetime,hashlib,json,os,pathlib,stat,sys
(output_raw,freeze_sha,capture_id,challenge,filename,content,gist_id,revision,login,
 gh_path,gh_sha)=sys.argv[1:]
output=pathlib.Path(output_raw);canonical=lambda v:(json.dumps(v,sort_keys=True,separators=(",",":"))+"\n").encode()
value={"schema":"arc.recovery.github-gist-write-canary.v1","provider":"github.com","owner_login":login,
       "freeze_plan_sha256":freeze_sha,"capture_id":capture_id,"challenge":challenge,
       "gist_id":gist_id,"gist_revision":revision,"gist_filename":filename,
       "gist_content_sha256":hashlib.sha256(content.encode()).hexdigest(),
       "github_cli_path":gh_path,"github_cli_sha256":gh_sha,
       "create_verified":True,"revision_read_verified":True,"delete_verified":True,
       "completed_at":datetime.datetime.now(datetime.timezone.utc).strftime("%Y-%m-%dT%H:%M:%SZ")}
payload=canonical(value);parent=output.parent;details=parent.lstat()
if (not output.is_absolute() or os.path.normpath(os.fspath(output)) != os.fspath(output)
        or os.path.realpath(output) != os.fspath(output) or parent.is_symlink()
        or not stat.S_ISDIR(details.st_mode) or details.st_uid != os.geteuid() or details.st_mode & 0o022):
    raise SystemExit("Gist canary receipt path is unsafe")
dfd=os.open(parent,os.O_RDONLY|getattr(os,"O_DIRECTORY",0)|getattr(os,"O_NOFOLLOW",0))
try:
    fd=os.open(output.name,os.O_WRONLY|os.O_CREAT|os.O_EXCL|getattr(os,"O_NOFOLLOW",0),0o400,dir_fd=dfd)
    with os.fdopen(fd,"wb") as handle:handle.write(payload);handle.flush();os.fsync(handle.fileno());os.fchmod(handle.fileno(),0o400)
    os.fsync(dfd)
finally:os.close(dfd)
PY
    cleanup_canary
    trap - EXIT
    printf '%s\n' "$output"
)

transport_hash_file() {
    python3 - "$1" <<'PY'
import hashlib, os, sys
fd = os.open(sys.argv[1], os.O_RDONLY | getattr(os, "O_NOFOLLOW", 0))
try:
    digest = hashlib.sha256()
    while True:
        chunk = os.read(fd, 1024 * 1024)
        if not chunk:
            break
        digest.update(chunk)
    print(digest.hexdigest())
finally:
    os.close(fd)
PY
}

assert_operator_transport_tools() {
    [ "$ARC_OPERATOR_TRANSPORT_READY" = true ] || die "operator transport is not initialized"
    [ "$(transport_hash_file "$ARC_OPERATOR_SSH_BIN")" = "$ARC_OPERATOR_SSH_SHA256" ] || \
        die "reviewed SSH executable changed during the operator transaction"
    [ "$(transport_hash_file "$ARC_OPERATOR_SCP_BIN")" = "$ARC_OPERATOR_SCP_SHA256" ] || \
        die "reviewed SCP executable changed during the operator transaction"
    if [ "$ARC_OPERATOR_TRANSPORT_RCLONE" = true ]; then
        [ "$(transport_hash_file "$ARC_OPERATOR_RCLONE_BIN")" = "$ARC_OPERATOR_RCLONE_SHA256" ] || \
            die "pinned rclone executable changed during the operator transaction"
    fi
}

configure_operator_transport() {
    local require_rclone="${1:-false}"
    configure_operator_python
    if [ "$ARC_OPERATOR_TRANSPORT_READY" = true ]; then
        if [ "$require_rclone" = true ] && [ "$ARC_OPERATOR_TRANSPORT_RCLONE" != true ]; then
            die "operator transport was initialized without the required Drive channel"
        fi
        assert_operator_transport_tools
        return 0
    fi
    local known_hosts="${ARC_RECOVERY_SSH_KNOWN_HOSTS:-}"
    local known_hosts_sha="${ARC_RECOVERY_SSH_KNOWN_HOSTS_SHA256:-}"
    local identity="${ARC_RECOVERY_SSH_IDENTITY:-}"
    local identity_sha="${ARC_RECOVERY_SSH_IDENTITY_SHA256:-}"
    local ssh_sha="${ARC_RECOVERY_SSH_SHA256:-}"
    local scp_sha="${ARC_RECOVERY_SCP_SHA256:-}"
    local rclone_path="${ARC_RECOVERY_RCLONE_PATH:-}"
    local rclone_sha="${ARC_RECOVERY_RCLONE_SHA256:-}"
    local rclone_config="${ARC_RECOVERY_RCLONE_CONFIG:-}"
    require_hash "$known_hosts_sha" "operator known-hosts hash"
    require_hash "$identity_sha" "operator SSH identity hash"
    require_hash "$ssh_sha" "operator SSH executable hash"
    require_hash "$scp_sha" "operator SCP executable hash"
    require_absolute_file "$known_hosts" "operator known-hosts file"
    require_absolute_file "$identity" "operator SSH identity"
    [ "$SSH_USER" = root ] || die "operator transport requires the fixed root SSH user"
    if [ "$require_rclone" = true ]; then
        require_hash "$rclone_sha" "operator rclone executable hash"
        require_absolute_file "$rclone_path" "operator rclone executable"
        require_absolute_file "$rclone_config" "operator rclone config"
    fi
    local runtime
    runtime="$(mktemp -d)"
    ARCHIVE_FLEET_PINNED_TRANSPORT_ROOT="$runtime"
    chmod 700 "$runtime"
    python3 - "$runtime" "$known_hosts" "$known_hosts_sha" "$identity" "$identity_sha" \
        "$ARC_OPERATOR_SSH_BIN" "$ssh_sha" "$ARC_OPERATOR_SCP_BIN" "$scp_sha" \
        "$require_rclone" "$rclone_path" "$rclone_sha" "$rclone_config" "${NODES[@]}" <<'PY'
import base64
import binascii
import hashlib
import os
import pathlib
import re
import stat
import struct
import sys

(runtime_raw, known_raw, known_sha, identity_raw, identity_sha, ssh_raw, ssh_sha,
 scp_raw, scp_sha, require_rclone_raw, rclone_raw, rclone_sha,
 config_raw, *fleet_raw) = sys.argv[1:]
runtime = pathlib.Path(runtime_raw)
operator_uid = os.getuid()
hash_re = re.compile(r"[0-9a-f]{64}")

def fail(message):
    raise SystemExit(f"operator transport: {message}")

def read_locked(path, label, maximum, modes, owners, nlink=None):
    path = pathlib.Path(path)
    if not path.is_absolute() or os.path.normpath(os.fspath(path)) != os.fspath(path):
        fail(f"{label} path is not normalized and absolute")
    fd = os.open(path, os.O_RDONLY | getattr(os, "O_NOFOLLOW", 0) | getattr(os, "O_CLOEXEC", 0))
    try:
        before = os.fstat(fd); visible = os.lstat(path)
        identity = lambda value: (value.st_dev, value.st_ino, value.st_mode, value.st_uid,
                                  value.st_gid, value.st_nlink, value.st_size,
                                  value.st_mtime_ns, value.st_ctime_ns)
        if (not stat.S_ISREG(before.st_mode) or stat.S_ISLNK(visible.st_mode)
                or identity(before) != identity(visible) or before.st_uid not in owners
                or stat.S_IMODE(before.st_mode) not in modes or before.st_size <= 0
                or before.st_size > maximum or (nlink is not None and before.st_nlink != nlink)):
            fail(f"{label} owner/mode/type/link/size contract differs")
        chunks = []
        while True:
            chunk = os.read(fd, 1024 * 1024)
            if not chunk: break
            chunks.append(chunk)
        payload = b"".join(chunks); after = os.fstat(fd)
        if len(payload) != before.st_size or identity(before) != identity(after):
            fail(f"{label} changed while read")
        return payload
    finally:
        os.close(fd)

def checked_digest(payload, expected, label):
    if hash_re.fullmatch(expected or "") is None or hashlib.sha256(payload).hexdigest() != expected:
        fail(f"{label} differs from its reviewed SHA-256")

def create(name, payload, mode):
    target = runtime / name
    fd = os.open(target, os.O_WRONLY | os.O_CREAT | os.O_EXCL | getattr(os, "O_NOFOLLOW", 0), mode)
    with os.fdopen(fd, "wb") as handle:
        handle.write(payload); handle.flush(); os.fsync(handle.fileno()); os.fchmod(handle.fileno(), mode)
    return target

ssh_payload = read_locked(ssh_raw, "SSH executable", 64 * 1024 * 1024, {0o555, 0o755}, {0})
scp_payload = read_locked(scp_raw, "SCP executable", 64 * 1024 * 1024, {0o555, 0o755}, {0})
checked_digest(ssh_payload, ssh_sha, "SSH executable")
checked_digest(scp_payload, scp_sha, "SCP executable")
known_payload = read_locked(known_raw, "known-hosts", 64 * 1024, {0o400}, {operator_uid}, 1)
checked_digest(known_payload, known_sha, "known-hosts")
identity_payload = read_locked(identity_raw, "SSH identity", 64 * 1024, {0o400}, {operator_uid}, 1)
checked_digest(identity_payload, identity_sha, "SSH identity")

fleet = [entry.split("=", 1) for entry in fleet_raw]
lines = known_payload.decode("ascii").splitlines()
if len(lines) != len(fleet):
    fail("known-hosts must contain exactly the fixed six rows")
for line, (node, host) in zip(lines, fleet):
    fields = line.split()
    if len(fields) != 3 or fields[:2] != [host, "ssh-ed25519"]:
        fail(f"known-hosts topology/key type differs for {node}")
    try: blob = base64.b64decode(fields[2], validate=True)
    except binascii.Error: fail(f"known-hosts key is invalid base64 for {node}")
    prefix = struct.pack(">I", 11) + b"ssh-ed25519" + struct.pack(">I", 32)
    if len(blob) != len(prefix) + 32 or not blob.startswith(prefix):
        fail(f"known-hosts key blob is not one Ed25519 key for {node}")

create("known_hosts", known_payload, 0o400)
create("id_ed25519", identity_payload, 0o400)
if require_rclone_raw == "true":
    rclone_payload = read_locked(
        rclone_raw, "rclone executable", 256 * 1024 * 1024,
        {0o500, 0o555, 0o700, 0o755}, {0, operator_uid}, 1,
    )
    checked_digest(rclone_payload, rclone_sha, "rclone executable")
    config_payload = read_locked(config_raw, "rclone config", 1024 * 1024, {0o600}, {operator_uid}, 1)
    create("rclone", rclone_payload, 0o500)
    create("rclone.conf", config_payload, 0o600)
directory = os.open(runtime, os.O_RDONLY | getattr(os, "O_DIRECTORY", 0))
try: os.fsync(directory)
finally: os.close(directory)
PY
    ARC_OPERATOR_SSH_SHA256="$ssh_sha"
    ARC_OPERATOR_SCP_SHA256="$scp_sha"
    ARC_OPERATOR_SSH_KNOWN_HOSTS_SHA256="$known_hosts_sha"
    ARC_OPERATOR_SSH_IDENTITY_SHA256="$identity_sha"
    ARC_OPERATOR_KNOWN_HOSTS="$runtime/known_hosts"
    ARC_OPERATOR_IDENTITY="$runtime/id_ed25519"
    if [ "$require_rclone" = true ]; then
        ARC_OPERATOR_RCLONE_SHA256="$rclone_sha"
        ARC_OPERATOR_RCLONE_BIN="$runtime/rclone"
        ARC_OPERATOR_RCLONE_CONFIG="$runtime/rclone.conf"
        ARC_OPERATOR_TRANSPORT_RCLONE=true
    fi
    ARC_OPERATOR_TRANSPORT_READY=true
    assert_operator_transport_tools
}

ssh() {
    if [ "$ARC_OPERATOR_TRANSPORT_READY" != true ]; then
        command ssh "$@"
        return
    fi
    assert_operator_transport_tools
    local result
    if /usr/bin/env -i HOME="$ARCHIVE_FLEET_PINNED_TRANSPORT_ROOT" \
        PATH=/usr/bin:/bin:/usr/sbin:/sbin LANG=C LC_ALL=C \
        "$ARC_OPERATOR_SSH_BIN" -F /dev/null -i "$ARC_OPERATOR_IDENTITY" \
        -o "UserKnownHostsFile=$ARC_OPERATOR_KNOWN_HOSTS" \
        -o GlobalKnownHostsFile=/dev/null -o HostKeyAlgorithms=ssh-ed25519 \
        -o PubkeyAcceptedAlgorithms=ssh-ed25519 -o IdentityAgent=none \
        -o UpdateHostKeys=no -o CanonicalizeHostname=no -o CheckHostIP=yes \
        -o IdentitiesOnly=yes -o ProxyCommand=none -o ProxyJump=none \
        -o PasswordAuthentication=no -o KbdInteractiveAuthentication=no \
        -o PreferredAuthentications=publickey -o NumberOfPasswordPrompts=0 \
        -o ChallengeResponseAuthentication=no -o GSSAPIAuthentication=no \
        -o ForwardAgent=no -o ForwardX11=no -o ClearAllForwardings=yes \
        -o PermitLocalCommand=no -o RequestTTY=no "$@"; then
        result=0
    else
        result=$?
    fi
    assert_operator_transport_tools
    return "$result"
}

ssh_remote_exact() {
    [ "$#" -ge 2 ] || die "ssh_remote_exact requires a host and command vector"
    local host="$1" remote_command="" remote_argument quoted_argument
    shift
    for remote_argument in "$@"; do
        printf -v quoted_argument '%q' "$remote_argument"
        if [ -n "$remote_command" ]; then
            remote_command+=" $quoted_argument"
        else
            remote_command="$quoted_argument"
        fi
    done
    # OpenSSH does not transmit an argv array.  Exactly one shell-quoted
    # command string after the destination is the only safe representation.
    ssh "${SSH_OPTIONS[@]}" "$SSH_USER@$host" "$remote_command"
}

scp() {
    if [ "$ARC_OPERATOR_TRANSPORT_READY" != true ]; then
        command scp "$@"
        return
    fi
    assert_operator_transport_tools
    local result
    if /usr/bin/env -i HOME="$ARCHIVE_FLEET_PINNED_TRANSPORT_ROOT" \
        PATH=/usr/bin:/bin:/usr/sbin:/sbin LANG=C LC_ALL=C \
        "$ARC_OPERATOR_SCP_BIN" -S "$ARC_OPERATOR_SSH_BIN" -F /dev/null \
        -i "$ARC_OPERATOR_IDENTITY" -o "UserKnownHostsFile=$ARC_OPERATOR_KNOWN_HOSTS" \
        -o GlobalKnownHostsFile=/dev/null -o HostKeyAlgorithms=ssh-ed25519 \
        -o PubkeyAcceptedAlgorithms=ssh-ed25519 -o IdentityAgent=none \
        -o UpdateHostKeys=no -o CanonicalizeHostname=no -o CheckHostIP=yes \
        -o IdentitiesOnly=yes -o ProxyCommand=none -o ProxyJump=none \
        -o PasswordAuthentication=no -o KbdInteractiveAuthentication=no \
        -o PreferredAuthentications=publickey -o NumberOfPasswordPrompts=0 \
        -o ChallengeResponseAuthentication=no -o GSSAPIAuthentication=no \
        -o ForwardAgent=no -o ForwardX11=no -o ClearAllForwardings=yes \
        -o PermitLocalCommand=no -o RequestTTY=no "$@"; then
        result=0
    else
        result=$?
    fi
    assert_operator_transport_tools
    return "$result"
}

rclone() {
    if [ "$ARC_OPERATOR_TRANSPORT_READY" != true ]; then
        command rclone "$@"
        return
    fi
    [ "$ARC_OPERATOR_TRANSPORT_RCLONE" = true ] || die "Drive transport is not initialized"
    assert_operator_transport_tools
    local result
    if /usr/bin/env -i HOME="$ARCHIVE_FLEET_PINNED_TRANSPORT_ROOT" \
        PATH=/usr/bin:/bin:/usr/sbin:/sbin LANG=C LC_ALL=C \
        "$ARC_OPERATOR_RCLONE_BIN" --config "$ARC_OPERATOR_RCLONE_CONFIG" "$@"; then
        result=0
    else
        result=$?
    fi
    assert_operator_transport_tools
    return "$result"
}

usage() {
    cat <<'EOF'
Usage:
  # Pin once in the operator shell. Replace only the three /absolute paths;
  # known_hosts/id_ed25519 must be the exact sealed production-stage files.
  arc_sha256() {
    if [ -x /usr/bin/sha256sum ]; then /usr/bin/sha256sum "$1";
    else /usr/bin/shasum -a 256 "$1"; fi | /usr/bin/awk '{print $1}'
  }
  export ARC_RECOVERY_SSH_USER=root
  # Copy the exact normalized non-symlink operator_python_path recorded in
  # freeze.lock.json (Ubuntu example shown; never use the /usr/bin/python3 symlink).
  export ARC_RECOVERY_PYTHON_PATH=/usr/bin/python3.12
  test -f "$ARC_RECOVERY_PYTHON_PATH" && test ! -L "$ARC_RECOVERY_PYTHON_PATH"
  export ARC_RECOVERY_PYTHON_SHA256="$(arc_sha256 "$ARC_RECOVERY_PYTHON_PATH")"
  export ARC_RECOVERY_SSH_KNOWN_HOSTS=/absolute/production-input-stage/private/known_hosts
  export ARC_RECOVERY_SSH_KNOWN_HOSTS_SHA256="$(arc_sha256 "$ARC_RECOVERY_SSH_KNOWN_HOSTS")"
  export ARC_RECOVERY_SSH_IDENTITY=/absolute/production-input-stage/private/id_ed25519
  export ARC_RECOVERY_SSH_IDENTITY_SHA256="$(arc_sha256 "$ARC_RECOVERY_SSH_IDENTITY")"
  export ARC_RECOVERY_SSH_SHA256="$(arc_sha256 /usr/bin/ssh)"
  export ARC_RECOVERY_SCP_SHA256="$(arc_sha256 /usr/bin/scp)"
  export ARC_RECOVERY_RCLONE_PATH=/absolute/reviewed/non-symlink/rclone
  export ARC_RECOVERY_RCLONE_SHA256="$(arc_sha256 "$ARC_RECOVERY_RCLONE_PATH")"
  export ARC_RECOVERY_RCLONE_CONFIG=/absolute/operator-owned-single-link-rclone.conf
  # Resolve Homebrew's symlink before pinning. The authenticated account must
  # have Gist scope; the token itself is never written to evidence or output.
  export ARC_RECOVERY_GH_PATH="$(/bin/realpath "$(command -v gh)")"
  test -f "$ARC_RECOVERY_GH_PATH" && test ! -L "$ARC_RECOVERY_GH_PATH"
  export ARC_RECOVERY_GH_SHA256="$(arc_sha256 "$ARC_RECOVERY_GH_PATH")"
  export ARC_RECOVERY_GITHUB_LOGIN=FerrumVir

  archive-fleet-to-drive.sh prepare-writers \
    --legacy-validator-set /absolute/legacy-validators.json \
    --output /absolute/writers.lock.json [--plan]
  ARC_RECOVERY_PREPARE_GO='STAGE-BARRIERS ORCHESTRATOR_SHA256 HELPER HELPER_SHA256' \
    archive-fleet-to-drive.sh prepare-writers \
    --legacy-validator-set /absolute/legacy-validators.json \
    --output /absolute/writers.lock.json --execute

  archive-fleet-to-drive.sh audit-writers --legacy-validator-set /absolute/legacy-validators.json \
    --output /absolute/writers.lock.json

  archive-fleet-to-drive.sh seal-freeze-plan --window ID \
    --legacy-validator-set /absolute/legacy-validators.json \
    --writer-contracts /absolute/writers.lock.json \
    --drive-remote-root 'arc-drive-arc:ARC Chain Recovery v0.8' \
    --drive-client-id-sha256 HASH --drive-account-sha256 HASH \
    --drive-daily-upload-budget-bytes BYTES --attest-dedicated-drive-uploader \
    --output /absolute/freeze.lock.json

  archive-fleet-to-drive.sh capture --freeze-plan /absolute/freeze.lock.json \
    --sample-legacy-public-height-output /absolute/unique-legacy-public-height.json \
    --inspector-binary /absolute/pretag-linux-x86_64/arc-node \
    --inspector-binary-sha256 HASH --genesis /absolute/genesis.toml \
    --genesis-sha256 HASH --validator-public-keys /absolute/validator-public-keys.json \
    --validator-public-keys-sha256 HASH \
    --legacy-validator-set /absolute/legacy-validator-set-40m.json \
    --legacy-validator-set-sha256 HASH \
    [--offline-stop-evidence-output /absolute/offline-stop-evidence.json] [--plan]
  ARC_RECOVERY_FREEZE_GO='FREEZE PLAN_SHA256 CAPTURE CAPTURE_SHA256' archive-fleet-to-drive.sh capture \
    --freeze-plan /absolute/freeze.lock.json \
    --sample-legacy-public-height-output /absolute/unique-legacy-public-height.json \
    --inspector-binary /absolute/pretag-linux-x86_64/arc-node \
    --inspector-binary-sha256 HASH --genesis /absolute/genesis.toml \
    --genesis-sha256 HASH --validator-public-keys /absolute/validator-public-keys.json \
    --validator-public-keys-sha256 HASH \
    --legacy-validator-set /absolute/legacy-validator-set-40m.json \
    --legacy-validator-set-sha256 HASH \
    --offline-stop-evidence-output /absolute/offline-stop-evidence.json --execute

  archive-fleet-to-drive.sh verify-offline-stop --freeze-plan /absolute/freeze.lock.json \
    --offline-stop-evidence /absolute/offline-stop-evidence.json \
    --offline-stop-evidence-sha256 HASH --ssh-known-hosts /absolute/known_hosts \
    --ssh-known-hosts-sha256 HASH --ssh-identity /absolute/id_ed25519 \
    --python-path /usr/bin/python3[.VERSION] --python-sha256 HASH --ssh-sha256 HASH \
    --challenge RANDOM_SHA256

  archive-fleet-to-drive.sh verify-installed-keys \
    --freeze-plan /absolute/production-input-stage/private/freeze.lock.json \
    --manifest /absolute/provisional-sealed-rollout.lock.json \
    --cli /absolute/production-input-stage/arc-cli --cli-sha256 HASH \
    --validator-public-keys /absolute/production-input-stage/validator-public-keys.json \
    --validator-public-keys-sha256 HASH \
    --validator-install-receipt /absolute/production-input-stage/private/VALIDATOR-KEY-INSTALL-RECEIPT.json \
    --validator-install-receipt-sha256 HASH \
    --vault-restore-receipt /absolute/production-input-stage/private/VALIDATOR-VAULT-RESTORE-RECEIPT.json \
    --vault-restore-receipt-sha256 HASH --challenge RANDOM_SHA256 \
    [--output /absolute/create-only-installed-key-proof.json]

  archive-fleet-to-drive.sh seal --freeze-plan /absolute/freeze.lock.json \
    --manifest /absolute/rollout.lock.json \
    --validator-public-keys /absolute/validators.json \
    --validator-install-receipt /absolute/production-input-stage/private/VALIDATOR-KEY-INSTALL-RECEIPT.json \
    --vault-restore-receipt /absolute/production-input-stage/private/VALIDATOR-VAULT-RESTORE-RECEIPT.json \
    --finalization-intent /absolute/archive-finalization-intent.json \
    --work-root /absolute/protected-large-archive-work-volume \
    [--allow-unbound-legacy-wal] [--plan]
  ARC_RECOVERY_GO='GO ROLLOUT_SHA256 FREEZE PLAN_SHA256 CAPTURE CAPTURE_SHA256 DEST DRIVE_SHA256 LEGACY_WAL BOUND_OR_UNBOUND' archive-fleet-to-drive.sh seal \
    --freeze-plan /absolute/freeze.lock.json \
    --manifest /absolute/rollout.lock.json \
    --validator-public-keys /absolute/validators.json \
    --validator-install-receipt /absolute/production-input-stage/private/VALIDATOR-KEY-INSTALL-RECEIPT.json \
    --vault-restore-receipt /absolute/production-input-stage/private/VALIDATOR-VAULT-RESTORE-RECEIPT.json \
    --finalization-intent /absolute/archive-finalization-intent.json \
    --work-root /absolute/protected-large-archive-work-volume \
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

`capture` fences the non-atomic fleet through immutable mixed-state rounds.
Every attempt freshly samples exactly the still-live targets, re-proves every
previously secured node in the same observation bracket, and gives each target
the same exact all-target authorization/readiness bytes. A target may cross its
kernel nft gate only during the 300-second authorization window. An attempt
that secures no node is preserved outside the transition ledger and is never a
lease for a later retry; its still-live targets are sampled again. A positive
partial round is sealed only after the old helpers expire, and its remaining
targets move to a fresh round. Thus at most six positive rounds secure all six
nodes without unquarantining or restarting a node that already transitioned.

A reboot after the persistent restart barrier but before the nft applied
commit is represented by a distinct persistently-stopped-precommit transition:
the exact round intent/barrier must have been armed in-window, the old writer
must remain absent behind the fail-closed restart dependency, and the offline
captured head and ancestry must bind the same authorization. The boundary is
derived only after an actual secured transition and the final ledger carries
every node exactly once. Post-stop construction verifies this sealed history;
it never substitutes current wall-clock age or re-queries retired origins.

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
check passes. The nonsecret finalization intent is first preserved as an exact
secret GitHub Gist and re-fetched by immutable revision. COMPLETE.json v2 binds
that intent, Gist id/revision/file hash, and is the last create-only mutation;
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

pin_freeze_plan() {
    local source="$1" destination_root="$2"
    python3 - "$source" "$destination_root" <<'PY'
import hashlib
import os
import pathlib
import re
import stat
import sys

source = pathlib.Path(sys.argv[1])
source_sidecar = source.with_name(source.name + ".sha256")
root = pathlib.Path(sys.argv[2])
if not source.is_absolute() or not root.is_absolute() or not root.is_dir() or root.is_symlink():
    raise SystemExit("freeze-plan pin paths are unsafe")

def read_locked(path):
    descriptor = os.open(path, os.O_RDONLY | getattr(os, "O_NOFOLLOW", 0))
    try:
        details = os.fstat(descriptor)
        if not stat.S_ISREG(details.st_mode) or details.st_mode & 0o222:
            raise SystemExit(f"sealed freeze input is mutable or non-regular: {path}")
        chunks = []
        while True:
            chunk = os.read(descriptor, 1024 * 1024)
            if not chunk:
                break
            chunks.append(chunk)
        return b"".join(chunks)
    finally:
        os.close(descriptor)

payload = read_locked(source)
sidecar = read_locked(source_sidecar)
digest = hashlib.sha256(payload).hexdigest()
if sidecar != f"{digest}  {source.name}\n".encode("ascii"):
    raise SystemExit("freeze-plan sidecar does not bind the exact source bytes")
destination = root / source.name
destination_sidecar = root / source_sidecar.name
for path, value in ((destination, payload), (destination_sidecar, sidecar)):
    descriptor = os.open(path, os.O_WRONLY | os.O_CREAT | os.O_EXCL | getattr(os, "O_NOFOLLOW", 0), 0o400)
    with os.fdopen(descriptor, "wb") as handle:
        handle.write(value); handle.flush(); os.fsync(handle.fileno())
directory = os.open(root, os.O_RDONLY | getattr(os, "O_DIRECTORY", 0))
try:
    os.fsync(directory)
finally:
    os.close(directory)
print(destination)
PY
}

assert_pinned_freeze_bytes() {
    local plan="$1" expected_sha="$2"
    case "$plan" in "$ARCHIVE_FLEET_PINNED_ROOT"/*) ;; *) die "destructive freeze input is not the private pinned snapshot" ;; esac
    [ "$(hash_file "$plan")" = "$expected_sha" ] || \
        die "private pinned freeze-plan bytes changed before destructive call"
    [ "$(cat "${plan}.sha256")" = "$expected_sha  ${plan##*/}" ] || \
        die "private pinned freeze-plan sidecar changed before destructive call"
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

validate_legacy_public_height_sample_output() {
    local output="$1"
    python3 - "$output" <<'PY'
import os
import pathlib
import stat
import sys

path = pathlib.Path(sys.argv[1])
if (
    not path.is_absolute()
    or path.suffix != ".json"
    or os.path.normpath(os.fspath(path)) != os.fspath(path)
    or path.name in {"", ".", ".."}
    or any(part in {".", ".."} for part in path.parts[1:])
):
    raise SystemExit("late legacy public-height output path is unsafe")
flags = os.O_RDONLY | getattr(os, "O_DIRECTORY", 0) | getattr(os, "O_NOFOLLOW", 0)
directory = os.open("/", flags)
try:
    root_details = os.fstat(directory)
    if (
        not stat.S_ISDIR(root_details.st_mode)
        or root_details.st_uid != 0
        or root_details.st_mode & 0o022
    ):
        raise SystemExit("late legacy public-height filesystem root is unsafe")
    for component in path.parent.parts[1:]:
        next_directory = os.open(component, flags, dir_fd=directory)
        os.close(directory)
        directory = next_directory
        details = os.fstat(directory)
        if (
            not stat.S_ISDIR(details.st_mode)
            or details.st_uid not in {0, os.geteuid()}
            or details.st_mode & 0o022
        ):
            raise SystemExit("late legacy public-height output ancestry is unsafe")
    try:
        details = os.stat(path.name, dir_fd=directory, follow_symlinks=False)
    except FileNotFoundError:
        print("absent")
        raise SystemExit(0)
    if (
        not stat.S_ISREG(details.st_mode)
        or details.st_uid != os.geteuid()
        or details.st_nlink != 1
        or stat.S_IMODE(details.st_mode) != 0o400
        or not 0 < details.st_size <= 16 * 1024 * 1024
    ):
        raise SystemExit("late legacy public-height output identity is unsafe")
    print("sealed")
finally:
    os.close(directory)
PY
}

sealed_legacy_public_height_receipt_sha() {
    local path="$1"
    python3 - "$path" <<'PY'
import hashlib
import json
import os
import pathlib
import stat
import sys

path = pathlib.Path(sys.argv[1])
descriptor = os.open(path, os.O_RDONLY | getattr(os, "O_NOFOLLOW", 0))
try:
    before = os.fstat(descriptor)
    stable = lambda value: (
        value.st_dev, value.st_ino, value.st_mode, value.st_uid, value.st_gid,
        value.st_nlink, value.st_size, value.st_mtime_ns, value.st_ctime_ns,
    )
    if (
        not stat.S_ISREG(before.st_mode)
        or before.st_uid != os.geteuid()
        or before.st_nlink != 1
        or stat.S_IMODE(before.st_mode) != 0o400
        or not 0 < before.st_size <= 16 * 1024 * 1024
    ):
        raise SystemExit("sealed legacy public-height receipt identity is unsafe")
    chunks = []
    while True:
        chunk = os.read(descriptor, 1024 * 1024)
        if not chunk:
            break
        chunks.append(chunk)
    payload = b"".join(chunks)
    if len(payload) != before.st_size or stable(os.fstat(descriptor)) != stable(before):
        raise SystemExit("sealed legacy public-height receipt changed while read")
finally:
    os.close(descriptor)
value = json.loads(payload)
canonical = (json.dumps(value, sort_keys=True, separators=(",", ":")) + "\n").encode()
if payload != canonical:
    raise SystemExit("sealed legacy public-height receipt is noncanonical")
print(hashlib.sha256(payload).hexdigest())
PY
}

validate_intrinsic_legacy_public_height_receipt() {
    local receipt="$1" receipt_sha="$2" freeze_plan="$3" freeze_sha="$4"
    local source_main
    source_main="$(manifest_field "$freeze_plan" source_commit)"
    python3 -B -I - "$LEGACY_HEIGHT_TOOL" "$receipt" "$receipt_sha" \
        "$source_main" "$freeze_sha" <<'PY'
import hashlib
import importlib.util
import json
import os
import pathlib
import stat
import sys

tool = pathlib.Path(sys.argv[1])
receipt = pathlib.Path(sys.argv[2])
expected, source_main, freeze_sha = sys.argv[3:]
spec = importlib.util.spec_from_file_location("arc_pinned_legacy_public_height", tool)
if spec is None or spec.loader is None:
    raise SystemExit("cannot load pinned legacy public-height validator")
module = importlib.util.module_from_spec(spec)
sys.modules[spec.name] = module
spec.loader.exec_module(module)
descriptor = os.open(receipt, os.O_RDONLY | getattr(os, "O_NOFOLLOW", 0))
try:
    before = os.fstat(descriptor)
    stable = lambda value: (
        value.st_dev, value.st_ino, value.st_mode, value.st_uid, value.st_gid,
        value.st_nlink, value.st_size, value.st_mtime_ns, value.st_ctime_ns,
    )
    if (
        not stat.S_ISREG(before.st_mode)
        or before.st_uid != os.geteuid()
        or before.st_nlink != 1
        or stat.S_IMODE(before.st_mode) != 0o400
        or not 0 < before.st_size <= 16 * 1024 * 1024
    ):
        raise SystemExit("legacy public-height intrinsic receipt identity is unsafe")
    chunks = []
    while True:
        chunk = os.read(descriptor, 1024 * 1024)
        if not chunk:
            break
        chunks.append(chunk)
    payload = b"".join(chunks)
    if len(payload) != before.st_size or stable(os.fstat(descriptor)) != stable(before):
        raise SystemExit("legacy public-height intrinsic receipt changed while read")
finally:
    os.close(descriptor)
if hashlib.sha256(payload).hexdigest() != expected:
    raise SystemExit("legacy public-height intrinsic receipt hash differs")
value = json.loads(payload)
if payload != module.canonical_bytes(value):
    raise SystemExit("legacy public-height intrinsic receipt is noncanonical")
completed = module.parse_utc(value.get("completed_at"), "completed_at")
module.validate_receipt(
    value,
    source_main=source_main,
    freeze_sha=freeze_sha,
    now=completed,
    max_age_seconds=module.MAX_RECEIPT_AGE_SECONDS,
)
PY
}

pin_legacy_public_height_toolchain() {
    local destination="$1" tool_sha freeze_sha rounds_sha
    tool_sha="$(tracked_source_hash "$LEGACY_HEIGHT_TOOL")"
    freeze_sha="$(tracked_source_hash "$RECOVERY_FREEZE_MODULE")"
    rounds_sha="$(tracked_source_hash "$QUARANTINE_ROUND_MODULE")"
    python3 - "$destination" \
        "$LEGACY_HEIGHT_TOOL" "$tool_sha" \
        "$RECOVERY_FREEZE_MODULE" "$freeze_sha" \
        "$QUARANTINE_ROUND_MODULE" "$rounds_sha" <<'PY'
import hashlib
import os
import pathlib
import stat
import sys

root = pathlib.Path(sys.argv[1])
pairs = [(pathlib.Path(sys.argv[index]), sys.argv[index + 1]) for index in range(2, 8, 2)]
output_names = (
    "legacy-public-height.py",
    "recovery_freeze.py",
    "quarantine_rounds.py",
)
parent = root.parent
parent_details = parent.lstat()
if (
    not root.is_absolute()
    or root.name in {"", ".", ".."}
    or parent.is_symlink()
    or not stat.S_ISDIR(parent_details.st_mode)
    or parent_details.st_uid != os.geteuid()
    or stat.S_IMODE(parent_details.st_mode) != 0o700
):
    raise SystemExit("legacy public-height pin root parent is unsafe")
parent_fd = os.open(parent, os.O_RDONLY | getattr(os, "O_DIRECTORY", 0) | getattr(os, "O_NOFOLLOW", 0))
try:
    os.mkdir(root.name, 0o700, dir_fd=parent_fd)
    os.fsync(parent_fd)
finally:
    os.close(parent_fd)
root_fd = os.open(root, os.O_RDONLY | getattr(os, "O_DIRECTORY", 0) | getattr(os, "O_NOFOLLOW", 0))
try:
    for index, (source, expected) in enumerate(pairs):
        descriptor = os.open(source, os.O_RDONLY | getattr(os, "O_NOFOLLOW", 0))
        try:
            before = os.fstat(descriptor)
            stable = lambda value: (
                value.st_dev, value.st_ino, value.st_mode, value.st_uid, value.st_gid,
                value.st_nlink, value.st_size, value.st_mtime_ns, value.st_ctime_ns,
            )
            if (
                source.is_symlink()
                or not stat.S_ISREG(before.st_mode)
                or before.st_uid not in {0, os.geteuid()}
                or before.st_nlink < 1
                or before.st_mode & 0o022
                or not 0 < before.st_size <= 16 * 1024 * 1024
            ):
                raise SystemExit(f"legacy public-height source identity is unsafe: {source}")
            chunks = []
            while True:
                chunk = os.read(descriptor, 1024 * 1024)
                if not chunk:
                    break
                chunks.append(chunk)
            payload = b"".join(chunks)
            if len(payload) != before.st_size or stable(os.fstat(descriptor)) != stable(before):
                raise SystemExit(f"legacy public-height source changed while read: {source}")
        finally:
            os.close(descriptor)
        if hashlib.sha256(payload).hexdigest() != expected:
            raise SystemExit(f"legacy public-height source hash differs: {source}")
        output = os.open(
            output_names[index],
            os.O_WRONLY | os.O_CREAT | os.O_EXCL | getattr(os, "O_NOFOLLOW", 0),
            0o400,
            dir_fd=root_fd,
        )
        with os.fdopen(output, "wb") as handle:
            handle.write(payload)
            handle.flush()
            os.fsync(handle.fileno())
            os.fchmod(handle.fileno(), 0o400)
    os.fsync(root_fd)
finally:
    os.close(root_fd)
os.chmod(root, 0o700, follow_symlinks=False)
print(root / "legacy-public-height.py")
PY
}

validate_durable_legacy_height_cross_proof() {
    local path="$1" freeze_sha="$2" capture_id="$3" receipt_sha="$4"
    require_absolute_file "$path" "durable authenticated legacy-height cross-proof"
    require_hash "$receipt_sha" "legacy public-height receipt hash"
    python3 - "$path" "$freeze_sha" "$capture_id" "$receipt_sha" <<'PY'
import json
import os
import pathlib
import stat
import sys

path = pathlib.Path(sys.argv[1])
descriptor = os.open(path, os.O_RDONLY | getattr(os, "O_NOFOLLOW", 0))
try:
    before = os.fstat(descriptor)
    stable = lambda value: (
        value.st_dev, value.st_ino, value.st_mode, value.st_uid, value.st_gid,
        value.st_nlink, value.st_size, value.st_mtime_ns, value.st_ctime_ns,
    )
    if (
        not stat.S_ISREG(before.st_mode)
        or before.st_uid != os.geteuid()
        or before.st_nlink != 1
        or stat.S_IMODE(before.st_mode) != 0o400
        or not 0 < before.st_size <= 16 * 1024 * 1024
    ):
        raise SystemExit("durable authenticated legacy-height cross-proof is unsafe")
    chunks = []
    while True:
        chunk = os.read(descriptor, 1024 * 1024)
        if not chunk:
            break
        chunks.append(chunk)
    payload = b"".join(chunks)
    if len(payload) != before.st_size or stable(os.fstat(descriptor)) != stable(before):
        raise SystemExit("durable authenticated legacy-height cross-proof changed while read")
finally:
    os.close(descriptor)
value = json.loads(payload)
canonical = (json.dumps(value, sort_keys=True, separators=(",", ":")) + "\n").encode()
if (
    payload != canonical
    or value.get("schema") != "arc.recovery.authenticated-legacy-height-fleet.v1"
    or value.get("freeze_plan_sha256") != sys.argv[2]
    or value.get("capture_id") != sys.argv[3]
    or value.get("legacy_public_height_receipt_sha256") != sys.argv[4]
):
    raise SystemExit("durable authenticated legacy-height cross-proof differs")
PY
}

sample_legacy_public_height_late() {
    local freeze_plan="$1" freeze_sha="$2" output="$3"
    local source_main result receipt_sha state
    state="$(validate_legacy_public_height_sample_output "$output")"
    [ "$state" = absent ] || die \
        "late legacy public-height output already exists; preserve it and choose a new unique output"
    source_main="$(manifest_field "$freeze_plan" source_commit)"
    result="$(python3 -B -I "$LEGACY_HEIGHT_TOOL" sample \
        --source-main "$source_main" --freeze-plan "$freeze_plan" \
        --freeze-plan-sha256 "$freeze_sha" --output "$output" \
        --timeout-seconds 10)"
    receipt_sha="$(python3 - "$result" <<'PY'
import json
import re
import sys

value = json.loads(sys.argv[1])
if (
    not isinstance(value, dict)
    or set(value) != {"legacy_public_max_height", "receipt_sha256"}
    or isinstance(value.get("legacy_public_max_height"), bool)
    or not isinstance(value.get("legacy_public_max_height"), int)
    or value["legacy_public_max_height"] < 0
    or re.fullmatch(r"[0-9a-f]{64}", str(value.get("receipt_sha256"))) is None
):
    raise SystemExit("late legacy public-height sampler output differs")
print(value["receipt_sha256"])
PY
)"
    require_hash "$receipt_sha" "late legacy public-height receipt hash"
    state="$(validate_legacy_public_height_sample_output "$output")"
    [ "$state" = sealed ] || die "late legacy public-height receipt was not sealed"
    [ "$(sealed_legacy_public_height_receipt_sha "$output")" = "$receipt_sha" ] || \
        die "late legacy public-height receipt differs from sampler output"
    printf '%s\n' "$receipt_sha"
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
    # This command pins the SSH identity into a private temporary transport
    # root.  Install cleanup before argument validation or transport setup so
    # both successful plans and every fail-closed exit remove that copy.
    begin_temporary_scope
    local legacy_validators="" output=""
    while [ "$#" -gt 0 ]; do
        case "$1" in
            --legacy-validator-set) [ "$#" -ge 2 ] || die "--legacy-validator-set needs a value"; legacy_validators="$2"; shift 2 ;;
            --output) [ "$#" -ge 2 ] || die "--output needs a value"; output="$2"; shift 2 ;;
            -h|--help) usage; return 0 ;;
            *) die "unknown audit-writers option: $1" ;;
        esac
    done
    configure_operator_transport false
    require_absolute_file "$legacy_validators" "legacy validator set"
    case "$output" in /*.json) ;; *) die "--output must be an absolute .json path" ;; esac
    [ "$SSH_USER" = root ] || die "writer audit requires the sealed root SSH user"
    require_commands python3 ssh git
    local legacy_sha temporary node host
    legacy_sha="$(hash_file "$legacy_validators")"
    temporary="$(mktemp -d)"
    ARCHIVE_FLEET_TEMP_ROOT="$temporary"
    for node in nyc lax ams lhr nrt sgp; do
        host="$(host_for "$node")"
        ssh_remote_exact "$host" /usr/bin/env -i HOME=/root \
            PATH=/usr/bin:/bin LANG=C LC_ALL=C /usr/bin/python3 -I - "$node" "$host" \
            > "$temporary/$node.json" <<'PY'
import hashlib
import json
import os
import pathlib
import re
import shutil
import stat
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
unified_rows = []
for line in cgroup.splitlines():
    hierarchy, controllers, path = line.split(":", 2)
    if hierarchy == "0" and controllers == "": unified_rows.append(path)
if (len(unified_rows) != 1 or not re.fullmatch(r"/[A-Za-z0-9._@/-]+", unified_rows[0])
        or ".." in unified_rows[0] or unified_rows[0] == "/"):
    fail("writer unified cgroup is missing or unsafe")
writer_cgroup_path = unified_rows[0]
writer_cgroup_root = pathlib.Path("/sys/fs/cgroup") / writer_cgroup_path.lstrip("/")
if writer_cgroup_root.is_symlink() or not writer_cgroup_root.is_dir():
    fail("writer cgroup directory is missing or unsafe")
writer_cgroup_details = writer_cgroup_root.stat()
active_units = []
for candidate_unit in ("arc-node.service", "arc-self-heal.service"):
    candidate_main = uint(
        subprocess.check_output(
            ["systemctl", "show", candidate_unit, "--property=MainPID", "--value"],
            text=True,
        ).strip(),
        f"{candidate_unit} MainPID",
    )
    if candidate_main > 0 and pathlib.Path(f"/proc/{candidate_main}").exists():
        active_units.append((candidate_unit, candidate_main))
if len(active_units) != 1:
    fail(f"expected one reviewed active supervisor unit, found {active_units}")
unit, observed_unit_main_pid = active_units[0]
if unit in cgroup:
    writer_supervision_mode = "systemd-unit"
else:
    if (not re.fullmatch(r"/user\.slice/user-0\.slice/session-[1-9][0-9]*\.scope", writer_cgroup_path)
            or int(stat_fields[3]) != 1):
        fail("detached writer is outside the reviewed root-session relationship")
    observed_members = set()
    for current, directories, _files in os.walk(writer_cgroup_root, followlinks=False):
        directories.sort()
        current_path = pathlib.Path(current)
        if current_path.is_symlink(): fail("detached writer cgroup subtree contains a symlink")
        procs = current_path / "cgroup.procs"
        if procs.is_symlink() or not procs.is_file(): fail("detached writer cgroup inventory is unsafe")
        observed_members.update(int(value) for value in procs.read_text(encoding="ascii").splitlines())
    if observed_members != {pid}:
        fail(f"detached writer is not the sole recursive cgroup member: {sorted(observed_members)}")
    writer_supervision_mode = "detached-root-session"
writer_cgroup_sha256 = hashlib.sha256(cgroup.encode("utf-8")).hexdigest()
unit_main_pid = uint(
    subprocess.check_output(
        ["systemctl", "show", unit, "--property=MainPID", "--value"], text=True
    ).strip(),
    "unit MainPID",
)
if unit_main_pid != observed_unit_main_pid:
    fail("reviewed supervisor MainPID changed during audit")
if unit_main_pid <= 0 or not pathlib.Path(f"/proc/{unit_main_pid}").exists():
    fail(f"reviewed supervisor unit is not active: {unit}")
supervisor_proc = pathlib.Path("/proc") / str(unit_main_pid)
supervisor_stat = supervisor_proc.joinpath("stat").read_text(encoding="ascii").split()
if len(supervisor_stat) < 22:
    fail("supervisor /proc stat is truncated")
supervisor_start_ticks = uint(supervisor_stat[21], "supervisor start ticks")
supervisor_argv_raw = supervisor_proc.joinpath("cmdline").read_bytes()
if not supervisor_argv_raw:
    fail("supervisor argv is empty")
supervisor_executable_path = os.readlink(supervisor_proc / "exe")
if not os.path.isabs(supervisor_executable_path):
    fail("supervisor executable is not absolute")
if unit not in supervisor_proc.joinpath("cgroup").read_text(encoding="utf-8"):
    fail("supervisor MainPID is outside the reviewed systemd unit")

def signal_ignored(process, signal_number):
    for line in process.joinpath("status").read_text(encoding="ascii").splitlines():
        if line.startswith("SigIgn:"):
            return bool(int(line.split(":", 1)[1].strip(), 16) & (1 << (signal_number - 1)))
    fail("process status has no SigIgn mask")

if signal_ignored(proc, 15) or signal_ignored(supervisor_proc, 15):
    fail("writer or supervisor ignores SIGTERM; deterministic recovery shutdown is unsupported")

try:
    supervisor_argv = [item.decode("utf-8") for item in supervisor_argv_raw.rstrip(b"\0").split(b"\0")]
except UnicodeDecodeError:
    fail("supervisor argv is not UTF-8")
payloads = []
if pathlib.Path(supervisor_executable_path).name in {"bash", "sh", "dash"}:
    if len(supervisor_argv) < 2:
        fail("interpreted supervisor has no script payload")
    payload_path = pathlib.Path(os.path.realpath(supervisor_argv[1]))
    if not payload_path.is_absolute() or not payload_path.is_file() or payload_path.is_symlink():
        fail("interpreted supervisor payload is missing, non-regular, or a symlink")
    payload_text = payload_path.read_text(encoding="utf-8")
    if re.search(r"(?:^|[;\s])trap(?:\s|$)", payload_text):
        fail("interpreted supervisor has a signal/exit trap; TERM quiescence is unreviewed")
    payloads.append({"path": str(payload_path), "sha256": sha256(payload_path)})
unit_configuration = subprocess.check_output(["systemctl", "cat", unit])
unit_hooks = {}
for hook in ("ExecReload", "ExecStop", "ExecStopPost", "OnFailure", "OnSuccess", "SuccessAction", "FailureAction", "JobTimeoutAction"):
    unit_hooks[hook] = subprocess.check_output(
        ["systemctl", "show", unit, f"--property={hook}", "--value"], text=True
    ).strip()
if any(value not in {"", "none"} for value in unit_hooks.values()):
    fail(f"reviewed supervisor has an unsealed lifecycle hook: {unit_hooks}")
automatic_lifecycle = {
    prop: subprocess.check_output(
        ["systemctl", "show", unit, f"--property={prop}", "--value"], text=True
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
    or automatic_lifecycle["OOMPolicy"] not in {"continue", "stop"}
):
    fail(f"reviewed supervisor has an automatic stop/kill source: {automatic_lifecycle}")
invocation_id = subprocess.check_output(
    ["systemctl", "show", unit, "--property=InvocationID", "--value"], text=True
).strip()
if not re.fullmatch(r"[0-9a-f]{32}", invocation_id):
    fail("reviewed supervisor InvocationID is malformed")
control_group = subprocess.check_output(
    ["systemctl", "show", unit, "--property=ControlGroup", "--value"], text=True
).strip()
if not re.fullmatch(r"/[A-Za-z0-9._@/-]+", control_group):
    fail("reviewed supervisor ControlGroup is malformed")
supervisor_cgroup_root = pathlib.Path("/sys/fs/cgroup") / control_group.lstrip("/")
if supervisor_cgroup_root.is_symlink() or not supervisor_cgroup_root.is_dir():
    fail("reviewed supervisor cgroup directory is unsafe")
if (writer_supervision_mode == "detached-root-session"
        and (writer_cgroup_path == control_group
             or writer_cgroup_path.startswith(control_group.rstrip("/") + "/")
             or control_group.startswith(writer_cgroup_path.rstrip("/") + "/"))):
    fail("detached writer and supervisor cgroups are not disjoint")
sleep_identity = None
if unit == "arc-self-heal.service":
    sleep_candidate = shutil.which("sleep")
    if not sleep_candidate:
        fail("self-heal supervisor has no reviewed sleep executable")
    sleep_path = pathlib.Path(os.path.realpath(sleep_candidate))
    sleep_identity = {"path": str(sleep_path), "sha256": sha256(sleep_path), "argv_policy": "sleep-duration-max-60s-v1", "max_seconds": 60}
cgroup_procs = pathlib.Path("/sys/fs/cgroup") / control_group.lstrip("/") / "cgroup.procs"
if not cgroup_procs.is_file():
    fail("reviewed supervisor cgroup process inventory is unavailable")
for member_raw in cgroup_procs.read_text(encoding="ascii").splitlines():
    member = int(member_raw)
    if member in {unit_main_pid, pid}:
        continue
    member_proc = pathlib.Path("/proc") / str(member)
    try:
        member_exe = os.readlink(member_proc / "exe")
        member_argv = [item.decode("utf-8") for item in member_proc.joinpath("cmdline").read_bytes().rstrip(b"\0").split(b"\0")]
    except (FileNotFoundError, ProcessLookupError, UnicodeDecodeError):
        fail("supervisor cgroup membership changed during audit")
    duration_match = re.fullmatch(r"([0-9]+(?:\.[0-9]+)?)([smhd]?)", member_argv[1]) if len(member_argv) == 2 else None
    duration_seconds = None if duration_match is None else float(duration_match.group(1)) * {"": 1, "s": 1, "m": 60, "h": 3600, "d": 86400}[duration_match.group(2)]
    if (not sleep_identity or member_exe != sleep_identity["path"]
            or duration_seconds is None or duration_seconds > sleep_identity["max_seconds"]):
        fail(f"unreviewed process exists in supervisor cgroup: pid={member}")
supervisor_context = {
    "schema": "arc.recovery.supervisor-context.v1",
    "unit": unit,
    "unit_configuration_sha256": hashlib.sha256(unit_configuration).hexdigest(),
    "lifecycle_hooks": unit_hooks,
    "automatic_lifecycle": automatic_lifecycle,
    "invocation_id": invocation_id,
    "control_group": control_group,
    "interpreter_payloads": payloads,
    "allowed_transient_sleep": sleep_identity,
    "term_traps_rejected": True,
}
supervisor_context_sha256 = hashlib.sha256(
    (json.dumps(supervisor_context, sort_keys=True, separators=(",", ":")) + "\n").encode()
).hexdigest()

# Flush the helper's enablement-link removals again from this independent audit
# process before sealing the precommit boot projection.
os.sync()
allow_marker = pathlib.Path("/etc/arc-recovery/legacy-start-allowed")
allow_payload = b"schema=arc.recovery.legacy-start-allow.v1\n"
if (allow_marker.is_symlink() or not allow_marker.is_file()
        or allow_marker.read_bytes() != allow_payload):
    fail("prepare allow marker is absent or differs")
allow_details = allow_marker.lstat()
if (allow_details.st_uid != 0 or allow_details.st_gid != 0
        or stat.S_IMODE(allow_details.st_mode) != 0o400
        or allow_details.st_dev != pathlib.Path("/etc/systemd/system").stat().st_dev):
    fail("prepare allow marker ownership/mode/filesystem differs")
start_barrier_bytes = b"[Unit]\nConditionPathExists=/etc/arc-recovery/legacy-start-allowed\n"
prepare_units = (
    "arc-self-heal.service", "arc-node.service",
    "arc-node-update.service", "arc-node-update.timer",
)
barriers = {}
merged_sources = {}
unit_states = {}
activation_closure = {}
closure_properties = (
    "Names", "Id", "Following",
    "ActiveState", "SubState", "MainPID", "Job", "ControlGroup", "FreezerState",
    "Restart", "KillMode", "SendSIGKILL", "OOMPolicy", "WatchdogUSec",
    "RuntimeMaxUSec", "RuntimeRandomizedExtraUSec", "CanReload",
    "StopWhenUnneeded", "BindsTo", "PartOf", "PropagatesStopTo",
    "StopPropagatedFrom", "ReloadPropagatedFrom", "Upholds", "UpheldBy",
    "TriggeredBy", "RequiredBy", "BoundBy", "ConflictedBy",
    "WantedBy", "OnFailureOf", "OnSuccessOf",
)
for prepare_unit in prepare_units:
    barrier = pathlib.Path(
        f"/etc/systemd/system/{prepare_unit}.d/zzzz-arc-recovery-freeze.conf"
    )
    details = barrier.lstat()
    if (barrier.is_symlink() or not stat.S_ISREG(details.st_mode)
            or barrier.read_bytes() != start_barrier_bytes
            or details.st_uid != 0 or details.st_gid != 0
            or details.st_mode & 0o222):
        fail(f"prepared persistent start barrier differs: {prepare_unit}")
    barriers[prepare_unit] = {
        "path": str(barrier), "sha256": sha256(barrier),
        "mode": stat.S_IMODE(details.st_mode), "uid": details.st_uid, "gid": details.st_gid,
    }
    merged = subprocess.check_output(["systemctl", "cat", prepare_unit])
    headers = re.findall(rb"(?m)^# (/[^\n]+)$", merged)
    if not headers: fail(f"prepared unit has no merged source manifest: {prepare_unit}")
    source_rows = []
    for header in headers:
        source = pathlib.Path(header.decode("utf-8")); source_details = source.lstat()
        if source.is_symlink() or not stat.S_ISREG(source_details.st_mode):
            fail(f"prepared unit source is unsafe: {source}")
        source_rows.append({"path": str(source), "sha256": sha256(source)})
    if len({row["path"] for row in source_rows}) != len(source_rows):
        fail(f"prepared unit source manifest is duplicated: {prepare_unit}")
    merged_sources[prepare_unit] = source_rows
    def prepare_prop(name):
        return subprocess.check_output(
            ["systemctl", "show", prepare_unit, f"--property={name}", "--value"], text=True,
        ).strip()
    enabled_result = subprocess.run(
        ["systemctl", "is-enabled", prepare_unit], text=True,
        stdout=subprocess.PIPE, stderr=subprocess.DEVNULL, check=False,
    )
    unit_states[prepare_unit] = {
        "active_state": prepare_prop("ActiveState"),
        "sub_state": prepare_prop("SubState"),
        # MainPID is not a property of timer units, so systemd returns an
        # empty value for arc-node-update.timer.  Normalize that service-only
        # property to the same quiescent zero represented by inactive services.
        "main_pid": int(prepare_prop("MainPID") or "0"),
        "job": prepare_prop("Job") or "0",
        "enablement": enabled_result.stdout.strip(),
    }
    activation_closure[prepare_unit] = {
        name: prepare_prop(name) for name in closure_properties
    }
    # MainPID is not defined for timer units.  systemd may render that
    # service-only property as either an empty string or "0", depending on
    # whether the timer came from a full vendor unit or the inert recovery
    # anchor.  Seal one canonical quiescent representation so the state row
    # and activation-closure row cannot disagree semantically.
    if (prepare_unit.endswith(".timer")
            and not activation_closure[prepare_unit]["MainPID"]):
        activation_closure[prepare_unit]["MainPID"] = "0"
    if (activation_closure[prepare_unit]["Names"] != prepare_unit
            or activation_closure[prepare_unit]["Id"] != prepare_unit
            or activation_closure[prepare_unit]["Following"]):
        fail(f"prepared unit alias closure differs: {prepare_unit}")
if (unit_states[unit]["active_state"] != "active"
        or unit_states[unit]["sub_state"] != "running"
        or unit_states[unit]["main_pid"] != unit_main_pid
        or unit_states[unit]["job"] != "0"
        or unit_states[unit]["enablement"] != "enabled"
        or "multi-user.target" not in activation_closure[unit]["WantedBy"].split()):
    fail("prepared selected supervisor state differs")
for prepare_unit in prepare_units:
    if prepare_unit == unit: continue
    row = unit_states[prepare_unit]
    if (row["active_state"] not in {"inactive", "failed"} or row["job"] != "0"
            or (prepare_unit.endswith(".service") and row["main_pid"] != 0)):
        fail(f"prepared alternative activation source is not quiescent: {prepare_unit}")
    for reverse_start in (
        "RequiredBy", "WantedBy", "BoundBy", "UpheldBy", "TriggeredBy",
        "OnFailureOf", "OnSuccessOf",
    ):
        observed_edge = activation_closure[prepare_unit].get(reverse_start)
        internal_timer_edge = (
            prepare_unit == "arc-node-update.service"
            and reverse_start == "TriggeredBy"
            and set(observed_edge.split()) == {"arc-node-update.timer"}
        )
        if observed_edge and not internal_timer_edge:
            fail(f"prepared alternative has a reverse activation edge: {prepare_unit} {reverse_start}")
default_target = subprocess.check_output(["systemctl", "get-default"], text=True).strip()
if default_target not in {"multi-user.target", "graphical.target"}:
    fail(f"prepared boot default target is unsupported: {default_target}")
def target_prop(target, name):
    return subprocess.check_output(
        ["systemctl", "show", target, f"--property={name}", "--value"], text=True,
    ).strip()
default_projection = {
    name: target_prop(default_target, name)
    for name in ("Names", "Id", "Following", "LoadState", "FragmentPath", "Requires", "Wants")
}
if (default_projection["Id"] != default_target or default_target not in default_projection["Names"].split()
        or default_projection["Following"] or default_projection["LoadState"] != "loaded"
        or (default_target == "graphical.target"
            and "multi-user.target" not in default_projection["Requires"].split())):
    fail(f"prepared default target does not durably reach multi-user: {default_projection}")
default_link = next((candidate for candidate in (
    pathlib.Path("/etc/systemd/system/default.target"),
    pathlib.Path("/usr/local/lib/systemd/system/default.target"),
    pathlib.Path("/usr/lib/systemd/system/default.target"),
) if candidate.exists() or candidate.is_symlink()), None)
if default_link is None:
    fail("prepared default target has no durable unit-file symlink")
default_details = default_link.lstat()
default_target_raw = os.readlink(default_link) if default_link.is_symlink() else None
if (not stat.S_ISLNK(default_details.st_mode) or default_details.st_uid != 0
        or default_details.st_gid != 0 or pathlib.Path(os.path.realpath(default_link)).name != default_target):
    fail("prepared default-target symlink identity differs")
enablement_link = pathlib.Path(f"/etc/systemd/system/multi-user.target.wants/{unit}")
enablement_details = enablement_link.lstat()
enablement_target_raw = os.readlink(enablement_link) if enablement_link.is_symlink() else None
enablement_target = pathlib.Path(os.path.realpath(enablement_link))
if (not stat.S_ISLNK(enablement_details.st_mode) or enablement_details.st_uid != 0
        or enablement_details.st_gid != 0 or not enablement_target.is_file()
        or enablement_target.is_symlink()):
    fail("prepared selected enablement symlink is not durable and exact")
boot_activation = {
    "default_target": default_target,
    "default_target_projection": default_projection,
    "default_target_symlink": {
        "path": str(default_link), "target": default_target_raw,
        "device": default_details.st_dev, "inode": default_details.st_ino,
        "uid": default_details.st_uid, "gid": default_details.st_gid,
    },
    "selected_enablement_symlink": {
        "path": str(enablement_link), "target": enablement_target_raw,
        "device": enablement_details.st_dev, "inode": enablement_details.st_ino,
        "uid": enablement_details.st_uid, "gid": enablement_details.st_gid,
        "resolved_path": str(enablement_target), "resolved_sha256": sha256(enablement_target),
    },
    "selected_reached_from_multi_user": True,
    "precommit_reboot_fail_open": True,
}
prepare_barrier = {
    "schema": "arc.recovery.prepare-barrier.v1",
    "allow_marker": {
        "path": str(allow_marker), "sha256": hashlib.sha256(allow_payload).hexdigest(),
        "mode": stat.S_IMODE(allow_details.st_mode), "uid": allow_details.st_uid,
        "gid": allow_details.st_gid, "device": allow_details.st_dev,
    },
    "persistent_start_barriers": barriers,
    "merged_unit_sources": merged_sources,
    "unit_states": unit_states,
    "activation_closure": activation_closure,
    "boot_activation": boot_activation,
    "selected_unit": unit,
    "selected_main_pid": unit_main_pid,
    "alternatives_inactive_no_jobs": True,
    "alternative_enablement_sync_completed": True,
    "writer_cgroup_relationship_sealed": True,
}

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
    "writer_cgroup_sha256": writer_cgroup_sha256,
    "writer_cgroup_path": writer_cgroup_path,
    "writer_cgroup_device": writer_cgroup_details.st_dev,
    "writer_cgroup_inode": writer_cgroup_details.st_ino,
    "writer_supervision_mode": writer_supervision_mode,
    "supervisor_unit": unit,
    "supervisor_main_pid": unit_main_pid,
    "supervisor_start_ticks": supervisor_start_ticks,
    "supervisor_executable_path": supervisor_executable_path,
    "supervisor_executable_sha256": sha256(f"/proc/{unit_main_pid}/exe"),
    "supervisor_argv_sha256": hashlib.sha256(supervisor_argv_raw).hexdigest(),
    "supervisor_context": supervisor_context,
    "supervisor_context_sha256": supervisor_context_sha256,
    "prepare_barrier": prepare_barrier,
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
import fcntl
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
    for field in (
        "writer_pid", "writer_start_ticks", "supervisor_main_pid",
        "supervisor_start_ticks",
    ):
        if isinstance(row.get(field), bool) or not isinstance(row.get(field), int) or row[field] <= 0:
            raise SystemExit(f"controlled writer {name} has an invalid {field}")
    for field in (
        "executable_sha256", "argv_sha256", "writer_cgroup_sha256",
        "supervisor_executable_sha256",
        "supervisor_argv_sha256",
        "supervisor_context_sha256",
    ):
        if not isinstance(row.get(field), str) or not re.fullmatch(r"[0-9a-f]{64}", row[field]):
            raise SystemExit(f"controlled writer {name} has an invalid {field}")
    context = row.get("supervisor_context")
    if not isinstance(context, dict) or context.get("schema") != "arc.recovery.supervisor-context.v1":
        raise SystemExit(f"controlled writer {name} has an invalid supervisor context")
    context_payload = (json.dumps(context, sort_keys=True, separators=(",", ":")) + "\n").encode()
    if hashlib.sha256(context_payload).hexdigest() != row["supervisor_context_sha256"]:
        raise SystemExit(f"controlled writer {name} supervisor context hash differs")
    prepare = row.get("prepare_barrier")
    expected_prepare_units = {
        "arc-self-heal.service", "arc-node.service",
        "arc-node-update.service", "arc-node-update.timer",
    }
    if (not isinstance(prepare, dict) or prepare.get("schema") != "arc.recovery.prepare-barrier.v1"
            or prepare.get("selected_unit") != row.get("supervisor_unit")
            or prepare.get("selected_main_pid") != row.get("supervisor_main_pid")
            or prepare.get("alternatives_inactive_no_jobs") is not True
            or prepare.get("alternative_enablement_sync_completed") is not True
            or prepare.get("writer_cgroup_relationship_sealed") is not True
            or set(prepare.get("persistent_start_barriers", {})) != expected_prepare_units
            or set(prepare.get("merged_unit_sources", {})) != expected_prepare_units
            or set(prepare.get("unit_states", {})) != expected_prepare_units
            or set(prepare.get("activation_closure", {})) != expected_prepare_units):
        raise SystemExit(f"controlled writer {name} prepare barrier is incomplete")
    marker = prepare.get("allow_marker", {})
    if (marker.get("path") != "/etc/arc-recovery/legacy-start-allowed"
            or marker.get("sha256") != hashlib.sha256(
                b"schema=arc.recovery.legacy-start-allow.v1\n"
            ).hexdigest() or marker.get("mode") != 0o400
            or marker.get("uid") != 0 or marker.get("gid") != 0):
        raise SystemExit(f"controlled writer {name} prepare allow marker differs")
    boot = prepare.get("boot_activation", {})
    if (boot.get("default_target") not in {"multi-user.target", "graphical.target"}
            or boot.get("selected_reached_from_multi_user") is not True
            or boot.get("precommit_reboot_fail_open") is not True
            or boot.get("selected_enablement_symlink", {}).get("path")
            != f"/etc/systemd/system/multi-user.target.wants/{row.get('supervisor_unit')}"):
        raise SystemExit(f"controlled writer {name} boot activation proof differs")
    for field in ("executable_path", "supervisor_executable_path"):
        if not isinstance(row.get(field), str) or not row[field].startswith("/"):
            raise SystemExit(f"controlled writer {name} has an invalid {field}")
    if row.get("writer_supervision_mode") not in {"systemd-unit", "detached-root-session"}:
        raise SystemExit(f"controlled writer {name} has an invalid supervision mode")
    if (not isinstance(row.get("writer_cgroup_path"), str)
            or not re.fullmatch(r"/[A-Za-z0-9._@/-]+", row["writer_cgroup_path"])
            or ".." in row["writer_cgroup_path"]
            or row["writer_cgroup_path"] == "/"
            or isinstance(row.get("writer_cgroup_device"), bool)
            or not isinstance(row.get("writer_cgroup_device"), int)
            or row["writer_cgroup_device"] <= 0
            or isinstance(row.get("writer_cgroup_inode"), bool)
            or not isinstance(row.get("writer_cgroup_inode"), int)
            or row["writer_cgroup_inode"] <= 0):
        raise SystemExit(f"controlled writer {name} has an invalid cgroup identity")
    if row["supervisor_main_pid"] == row["writer_pid"] and (
        row["supervisor_start_ticks"] != row["writer_start_ticks"]
        or row["supervisor_executable_path"] != row["executable_path"]
        or row["supervisor_executable_sha256"] != row["executable_sha256"]
        or row["supervisor_argv_sha256"] != row["argv_sha256"]
    ):
        raise SystemExit(f"directly supervised writer {name} has conflicting process identities")
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
    "schema": "arc.recovery.writer-contracts.v3",
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
    # Python receives a private HOME even for this local-only command.  Keep
    # that runtime root invocation-scoped on success and failure.
    begin_temporary_scope
    local window="" output="" legacy_validators="" writer_contracts=""
    local drive_remote_root="$DRIVE_REMOTE" drive_client_sha="" drive_account_sha=""
    local drive_daily_budget="" dedicated_drive_uploader=false
    while [ "$#" -gt 0 ]; do
        case "$1" in
            --window) [ "$#" -ge 2 ] || die "--window needs a value"; window="$2"; shift 2 ;;
            --legacy-validator-set) [ "$#" -ge 2 ] || die "--legacy-validator-set needs a value"; legacy_validators="$2"; shift 2 ;;
            --writer-contracts) [ "$#" -ge 2 ] || die "--writer-contracts needs a value"; writer_contracts="$2"; shift 2 ;;
            --drive-remote-root) [ "$#" -ge 2 ] || die "--drive-remote-root needs a value"; drive_remote_root="$2"; shift 2 ;;
            --drive-client-id-sha256) [ "$#" -ge 2 ] || die "--drive-client-id-sha256 needs a value"; drive_client_sha="$2"; shift 2 ;;
            --drive-account-sha256) [ "$#" -ge 2 ] || die "--drive-account-sha256 needs a value"; drive_account_sha="$2"; shift 2 ;;
            --drive-daily-upload-budget-bytes) [ "$#" -ge 2 ] || die "--drive-daily-upload-budget-bytes needs a value"; drive_daily_budget="$2"; shift 2 ;;
            --attest-dedicated-drive-uploader) dedicated_drive_uploader=true; shift ;;
            --output) [ "$#" -ge 2 ] || die "--output needs a value"; output="$2"; shift 2 ;;
            -h|--help) usage; return 0 ;;
            *) die "unknown seal-freeze-plan option: $1" ;;
        esac
    done
    configure_operator_python
    printf '%s\n' "$window" | grep -Eq '^[A-Za-z0-9][A-Za-z0-9._:@+-]{0,127}$' || \
        die "--window must be a short reviewable change/window identifier"
    case "$output" in /*.json) ;; *) die "--output must be an absolute .json path" ;; esac
    require_absolute_file "$legacy_validators" "legacy validator set"
    require_absolute_file "$writer_contracts" "writer contracts"
    require_absolute_file "${writer_contracts}.sha256" "writer-contract checksum"
    validate_drive_remote "$drive_remote_root"
    [ "${drive_remote_root%%:*}" != arc-drive ] || die "legacy arc-drive remote cannot authorize a production freeze"
    require_hash "$drive_client_sha" "Drive OAuth client-id hash"
    require_hash "$drive_account_sha" "Drive account hash"
    require_uint "$drive_daily_budget" "Drive daily upload budget"
    [ "$drive_daily_budget" -le 700000000000 ] || \
        die "Drive operational upload budget exceeds the reviewed decimal 700 GB ceiling"
    [ "$dedicated_drive_uploader" = true ] || \
        die "freeze sealing requires --attest-dedicated-drive-uploader"
    require_commands python3 git
    require_absolute_file "$ORCHESTRATOR" "archive orchestrator"
    require_absolute_file "$REMOTE_HELPER" "remote archive helper"
    require_absolute_file "$ROLLOUT_TOOL" "rollout verifier"
    require_absolute_file "$ROLLOUT_SCHEMA" "rollout schema"
    require_absolute_file "$DRIVE_PREFREEZE_GATE" "Drive prefreeze gate"
    local helper_sha orchestrator_sha rollout_tool_sha schema_sha drive_gate_sha
    local source_commit legacy_sha contracts_sha drive_root_sha
    helper_sha="$(tracked_source_hash "$REMOTE_HELPER")"
    orchestrator_sha="$(tracked_source_hash "$ORCHESTRATOR")"
    rollout_tool_sha="$(tracked_source_hash "$ROLLOUT_TOOL")"
    schema_sha="$(tracked_source_hash "$ROLLOUT_SCHEMA")"
    drive_gate_sha="$(tracked_source_hash "$DRIVE_PREFREEZE_GATE")"
    source_commit="$(current_source_commit)"
    legacy_sha="$(hash_file "$legacy_validators")"
    contracts_sha="$(hash_file "$writer_contracts")"
    drive_root_sha="$(printf '%s' "$drive_remote_root" | python3 -c 'import hashlib,sys; print(hashlib.sha256(sys.stdin.buffer.read()).hexdigest())')"
    python3 - "$output" "$window" "$helper_sha" "$orchestrator_sha" \
        "$rollout_tool_sha" "$schema_sha" "$drive_gate_sha" "$source_commit" \
        "$legacy_validators" "$legacy_sha" "$writer_contracts" "$contracts_sha" \
        "$drive_remote_root" "$drive_root_sha" "$drive_client_sha" "$drive_account_sha" \
        "$drive_daily_budget" "$ARC_OPERATOR_PYTHON_SOURCE" "$ARC_OPERATOR_PYTHON_SHA256" "${NODES[@]}" <<'PY'
import datetime
import hashlib
import json
import os
import pathlib
import stat
import sys

output = pathlib.Path(sys.argv[1])
(window, helper_sha, orchestrator_sha, rollout_tool_sha, schema_sha,
 drive_gate_sha, source_commit, legacy_path_raw, legacy_sha, contracts_path_raw,
 contracts_sha, drive_remote_root, drive_root_sha, drive_client_sha,
 drive_account_sha, drive_daily_budget_raw, operator_python_path, operator_python_sha) = sys.argv[2:20]
expected_nodes = [entry.split("=", 1) for entry in sys.argv[20:]]
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
if contracts.get("schema") != "arc.recovery.writer-contracts.v3":
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
    "schema": "arc.recovery.freeze-plan.v5",
    "window": window,
    "created_at": datetime.datetime.now(datetime.timezone.utc).replace(microsecond=0).isoformat().replace("+00:00", "Z"),
    "sentinels": ["nyc", "lax"],
    "nodes": nodes,
    "remote_helper_sha256": helper_sha,
    "orchestrator_sha256": orchestrator_sha,
    "rollout_tool_sha256": rollout_tool_sha,
    "rollout_schema_sha256": schema_sha,
    "operator_python_path": operator_python_path,
    "operator_python_sha256": operator_python_sha,
    "source_commit": source_commit,
    "legacy_validator_set_sha256": legacy_sha,
    "writer_contracts_sha256": contracts_sha,
    "drive_prefreeze": {
        "gate_sha256": drive_gate_sha,
        "remote_root": drive_remote_root,
        "remote_root_sha256": drive_root_sha,
        "oauth_client_id_sha256": drive_client_sha,
        "account_sha256": drive_account_sha,
        "daily_upload_budget_bytes": int(drive_daily_budget_raw),
        "dedicated_no_other_upload_writers_attested": True,
    },
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
    require_absolute_file "$DRIVE_PREFREEZE_GATE" "Drive prefreeze gate"
    local helper_sha orchestrator_sha rollout_tool_sha schema_sha drive_gate_sha source_commit
    helper_sha="$(tracked_source_hash "$REMOTE_HELPER")"
    orchestrator_sha="$(tracked_source_hash "$ORCHESTRATOR")"
    rollout_tool_sha="$(tracked_source_hash "$ROLLOUT_TOOL")"
    schema_sha="$(tracked_source_hash "$ROLLOUT_SCHEMA")"
    drive_gate_sha="$(tracked_source_hash "$DRIVE_PREFREEZE_GATE")"
    source_commit="$(current_source_commit)"
    python3 - "$plan" "$helper_sha" "$orchestrator_sha" "$rollout_tool_sha" \
        "$schema_sha" "$drive_gate_sha" "$source_commit" "$ARC_OPERATOR_PYTHON_SOURCE" \
        "$ARC_OPERATOR_PYTHON_SHA256" "${NODES[@]}" <<'PY'
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
helper_sha, orchestrator_sha, rollout_tool_sha, schema_sha, drive_gate_sha, source_commit, python_path, python_sha = sys.argv[2:10]
expected_nodes = []
for entry in sys.argv[10:]:
    name, host = entry.split("=", 1)
    expected_nodes.append({"name": name, "host": host})
value = json.loads(path.read_text(encoding="utf-8"))
if set(value) != {
    "schema", "window", "created_at", "sentinels", "nodes",
    "remote_helper_sha256", "orchestrator_sha256", "rollout_tool_sha256",
    "rollout_schema_sha256", "operator_python_path", "operator_python_sha256",
    "source_commit", "legacy_validator_set_sha256",
    "writer_contracts_sha256", "drive_prefreeze", "quorum_proof",
}:
    raise SystemExit("freeze plan has missing or unknown fields")
if value["schema"] != "arc.recovery.freeze-plan.v5":
    raise SystemExit("unsupported freeze plan schema")
if value["operator_python_path"] != python_path or value["operator_python_sha256"] != python_sha:
    raise SystemExit("freeze plan operator Python identity differs from this transaction")
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
    for field in (
        "writer_pid", "writer_start_ticks", "supervisor_main_pid",
        "supervisor_start_ticks",
    ):
        if isinstance(row.get(field), bool) or not isinstance(row.get(field), int) or row[field] <= 0:
            raise SystemExit(f"freeze plan {field} is invalid for {row['name']}")
    for field in (
        "executable_sha256", "argv_sha256", "writer_cgroup_sha256",
        "supervisor_executable_sha256",
        "supervisor_argv_sha256",
        "supervisor_context_sha256",
    ):
        if not isinstance(row.get(field), str) or not re.fullmatch(r"[0-9a-f]{64}", row[field]):
            raise SystemExit(f"freeze plan {field} is invalid for {row['name']}")
    context = row.get("supervisor_context")
    if not isinstance(context, dict) or context.get("schema") != "arc.recovery.supervisor-context.v1":
        raise SystemExit(f"freeze plan supervisor context is invalid for {row['name']}")
    context_payload = (json.dumps(context, sort_keys=True, separators=(",", ":")) + "\n").encode()
    if hashlib.sha256(context_payload).hexdigest() != row["supervisor_context_sha256"]:
        raise SystemExit(f"freeze plan supervisor context hash differs for {row['name']}")
    prepare = row.get("prepare_barrier")
    expected_prepare_units = {
        "arc-self-heal.service", "arc-node.service",
        "arc-node-update.service", "arc-node-update.timer",
    }
    if (not isinstance(prepare, dict) or prepare.get("schema") != "arc.recovery.prepare-barrier.v1"
            or prepare.get("selected_unit") != row.get("supervisor_unit")
            or prepare.get("selected_main_pid") != row.get("supervisor_main_pid")
            or prepare.get("alternatives_inactive_no_jobs") is not True
            or prepare.get("alternative_enablement_sync_completed") is not True
            or prepare.get("writer_cgroup_relationship_sealed") is not True
            or set(prepare.get("persistent_start_barriers", {})) != expected_prepare_units
            or set(prepare.get("merged_unit_sources", {})) != expected_prepare_units
            or set(prepare.get("unit_states", {})) != expected_prepare_units
            or set(prepare.get("activation_closure", {})) != expected_prepare_units):
        raise SystemExit(f"freeze plan prepare barrier differs for {row['name']}")
    boot = prepare.get("boot_activation", {})
    if (boot.get("default_target") not in {"multi-user.target", "graphical.target"}
            or boot.get("selected_reached_from_multi_user") is not True
            or boot.get("precommit_reboot_fail_open") is not True
            or boot.get("selected_enablement_symlink", {}).get("path")
            != f"/etc/systemd/system/multi-user.target.wants/{row.get('supervisor_unit')}"):
        raise SystemExit(f"freeze plan boot activation differs for {row['name']}")
    for field in ("executable_path", "supervisor_executable_path", "data_dir", "model_path"):
        if not isinstance(row.get(field), str) or not re.fullmatch(r"/[A-Za-z0-9._/@%+=,-]+", row[field]) or ".." in row[field]:
            raise SystemExit(f"freeze plan {field} is invalid for {row['name']}")
    if row.get("writer_supervision_mode") not in {"systemd-unit", "detached-root-session"}:
        raise SystemExit(f"freeze plan supervision mode is invalid for {row['name']}")
    if (not isinstance(row.get("writer_cgroup_path"), str)
            or not re.fullmatch(r"/[A-Za-z0-9._@/-]+", row["writer_cgroup_path"])
            or ".." in row["writer_cgroup_path"]
            or row["writer_cgroup_path"] == "/"
            or isinstance(row.get("writer_cgroup_device"), bool)
            or not isinstance(row.get("writer_cgroup_device"), int)
            or row["writer_cgroup_device"] <= 0
            or isinstance(row.get("writer_cgroup_inode"), bool)
            or not isinstance(row.get("writer_cgroup_inode"), int)
            or row["writer_cgroup_inode"] <= 0):
        raise SystemExit(f"freeze plan cgroup identity is invalid for {row['name']}")
    if row.get("supervisor_unit") not in {"arc-node.service", "arc-self-heal.service"}:
        raise SystemExit(f"freeze plan supervisor unit is invalid for {row['name']}")
    if not isinstance(row.get("boot_id"), str) or not re.fullmatch(r"[0-9a-f]{8}-[0-9a-f]{4}-[0-9a-f]{4}-[0-9a-f]{4}-[0-9a-f]{12}", row["boot_id"]):
        raise SystemExit(f"freeze plan boot ID is invalid for {row['name']}")
    if not isinstance(row.get("validator_address"), str) or not re.fullmatch(r"[0-9a-f]{64}", row["validator_address"]):
        raise SystemExit(f"freeze plan validator address is invalid for {row['name']}")
    if isinstance(row.get("stake"), bool) or not isinstance(row.get("stake"), int) or row["stake"] <= 0:
        raise SystemExit(f"freeze plan stake is invalid for {row['name']}")
    if row["supervisor_main_pid"] == row["writer_pid"] and (
        row["supervisor_start_ticks"] != row["writer_start_ticks"]
        or row["supervisor_executable_path"] != row["executable_path"]
        or row["supervisor_executable_sha256"] != row["executable_sha256"]
        or row["supervisor_argv_sha256"] != row["argv_sha256"]
    ):
        raise SystemExit(f"freeze plan direct supervisor identity conflicts for {row['name']}")
    if row.get("supervisor_unit") == "arc-node.service" and (
        row.get("writer_supervision_mode") != "systemd-unit"
        or row["supervisor_main_pid"] != row["writer_pid"]
    ):
        raise SystemExit(f"freeze plan direct arc-node service relationship differs for {row['name']}")
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
drive = value["drive_prefreeze"]
if not isinstance(drive, dict) or set(drive) != {
    "gate_sha256", "remote_root", "remote_root_sha256", "oauth_client_id_sha256",
    "account_sha256", "daily_upload_budget_bytes",
    "dedicated_no_other_upload_writers_attested",
}:
    raise SystemExit("freeze plan Drive prefreeze binding fields differ")
if drive.get("gate_sha256") != drive_gate_sha:
    raise SystemExit("Drive prefreeze gate bytes differ from the sealed freeze plan")
for field in ("remote_root_sha256", "oauth_client_id_sha256", "account_sha256"):
    if not isinstance(drive.get(field), str) or not re.fullmatch(r"[0-9a-f]{64}", drive[field]):
        raise SystemExit(f"freeze plan Drive {field} is malformed")
remote_root = drive.get("remote_root")
if not isinstance(remote_root, str) or remote_root.startswith("arc-drive:") or ":" not in remote_root:
    raise SystemExit("freeze plan uses an unsafe or legacy Drive remote")
if hashlib.sha256(remote_root.encode("utf-8")).hexdigest() != drive["remote_root_sha256"]:
    raise SystemExit("freeze plan Drive remote root hash differs")
budget = drive.get("daily_upload_budget_bytes")
if isinstance(budget, bool) or not isinstance(budget, int) or not 0 < budget <= 700_000_000_000:
    raise SystemExit("freeze plan Drive upload budget is outside the reviewed ceiling")
if drive.get("dedicated_no_other_upload_writers_attested") is not True:
    raise SystemExit("freeze plan lacks the dedicated ARC Drive uploader attestation")
source_sizes = [row.get("data_bytes") for row in nodes]
if any(isinstance(size, bool) or not isinstance(size, int) or size <= 0 for size in source_sizes):
    raise SystemExit("freeze plan source byte reservations are malformed")
if 3 * sum(source_sizes) + 32 * 1024**3 > budget:
    raise SystemExit("freeze plan archive reservation exceeds the reviewed Drive budget")
if 3 * max(source_sizes) + 4 * 1024**3 > 5_000_000_000_000:
    raise SystemExit("freeze plan largest object reservation exceeds Google Drive's limit")
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
REMOTE_FREEZE_PLAN_PATH=""

install_helpers() {
    local expected_sha="$1" node host remote_temporary
    require_hash "$expected_sha" "sealed remote helper hash"
    REMOTE_HELPER_SHA="$(hash_file "$REMOTE_HELPER")"
    [ "$REMOTE_HELPER_SHA" = "$expected_sha" ] || \
        die "remote helper bytes changed after freeze-plan verification"
    REMOTE_HELPER_PATH="/root/.arc-recovery-helpers/$REMOTE_HELPER_SHA/archive-node.sh"
    for node in nyc lax ams lhr nrt sgp; do
        host="$(host_for "$node")"
        remote_temporary="$(ssh_remote_exact "$host" /bin/sh -c \
            'umask 077; root=/root/.arc-recovery-helper-uploads; if test -e "$root"; then test -d "$root" && test ! -L "$root"; else mkdir -m 700 -- "$root"; fi; mktemp "$root/upload.XXXXXX"' /bin/sh)"
        case "$remote_temporary" in /root/.arc-recovery-helper-uploads/upload.*) ;; *) die "unsafe remote helper temporary path" ;; esac
        scp -q "${SSH_OPTIONS[@]}" "$REMOTE_HELPER" "$SSH_USER@$host:$remote_temporary"
        ssh_remote_exact "$host" /bin/sh -c \
            'set -eu; temporary=$1 target=$2 expected=$3; trap '\''rm -f -- "$temporary"'\'' EXIT; test -f "$temporary" && test ! -L "$temporary"; actual=$(sha256sum "$temporary" | cut -d" " -f1); test "$actual" = "$expected"; parent=${target%/*}; grand=${parent%/*}; if test -e "$grand"; then test -d "$grand" && test ! -L "$grand"; else mkdir -m 700 -- "$grand"; fi; if test -e "$parent"; then test -d "$parent" && test ! -L "$parent"; else mkdir -m 700 -- "$parent"; fi; chmod 500 -- "$temporary"; if ln -- "$temporary" "$target" 2>/dev/null; then :; else test -f "$target" && test ! -L "$target" && test "$(sha256sum "$target" | cut -d" " -f1)" = "$expected"; fi; chmod 500 -- "$target"; test "$(sha256sum "$target" | cut -d" " -f1)" = "$expected"' \
            /bin/sh "$remote_temporary" "$REMOTE_HELPER_PATH" "$REMOTE_HELPER_SHA"
    done
}

install_freeze_plan() {
    local plan="$1" expected_sha="$2" node host remote_temporary
    require_absolute_file "$plan" "pinned freeze plan"
    require_hash "$expected_sha" "pinned freeze plan hash"
    [ "$(hash_file "$plan")" = "$expected_sha" ] || die "pinned freeze plan bytes changed before remote staging"
    REMOTE_FREEZE_PLAN_PATH="/root/.arc-recovery-plans/$expected_sha/freeze.lock.json"
    for node in nyc lax ams lhr nrt sgp; do
        host="$(host_for "$node")"
        remote_temporary="$(ssh_remote_exact "$host" /bin/sh -c \
            'umask 077; root=/root/.arc-recovery-plan-uploads; if test -e "$root"; then test -d "$root" && test ! -L "$root"; else mkdir -m 700 -- "$root"; fi; mktemp "$root/upload.XXXXXX"' /bin/sh)"
        case "$remote_temporary" in /root/.arc-recovery-plan-uploads/upload.*) ;; *) die "unsafe remote freeze-plan temporary path" ;; esac
        scp -q "${SSH_OPTIONS[@]}" "$plan" "$SSH_USER@$host:$remote_temporary"
        ssh_remote_exact "$host" /bin/sh -c \
            'set -eu; temporary=$1 target=$2 expected=$3; trap '\''rm -f -- "$temporary"'\'' EXIT; test -f "$temporary" && test ! -L "$temporary"; test "$(sha256sum "$temporary" | cut -d" " -f1)" = "$expected"; parent=${target%/*}; grand=${parent%/*}; top=${grand%/*}; for directory in "$top" "$grand" "$parent"; do if test -e "$directory"; then test -d "$directory" && test ! -L "$directory"; else mkdir -m 700 -- "$directory"; fi; done; chmod 400 -- "$temporary"; if ln -- "$temporary" "$target" 2>/dev/null; then :; else test -f "$target" && test ! -L "$target" && test "$(sha256sum "$target" | cut -d" " -f1)" = "$expected" && test "$(/usr/bin/stat -c %u:%g:%a -- "$target")" = 0:0:400; fi; sidecar="$target.sha256"; expected_line="$expected  ${target##*/}"; if test -e "$sidecar"; then test -f "$sidecar" && test ! -L "$sidecar" && test "$(cat "$sidecar")" = "$expected_line" && test "$(/usr/bin/stat -c %u:%g:%a -- "$sidecar")" = 0:0:400; else side_tmp="$sidecar.partial.$$"; (umask 077; printf "%s\n" "$expected_line" > "$side_tmp"); chmod 400 "$side_tmp"; ln "$side_tmp" "$sidecar"; rm -f "$side_tmp"; fi; python3 - "$target" "$sidecar" <<'\''PY'\''
import os, pathlib, stat, sys
for raw in sys.argv[1:]:
    path = pathlib.Path(raw); details = path.lstat()
    if path.is_symlink() or not stat.S_ISREG(details.st_mode) or details.st_mode & 0o222 or details.st_uid != 0 or details.st_gid != 0:
        raise SystemExit("pinned freeze-plan artifact is unsafe")
    descriptor = os.open(path, os.O_RDONLY | getattr(os, "O_NOFOLLOW", 0))
    try: os.fsync(descriptor)
    finally: os.close(descriptor)
for path in {pathlib.Path(sys.argv[1]).parent, pathlib.Path(sys.argv[1]).parent.parent, pathlib.Path(sys.argv[1]).parent.parent.parent}:
    descriptor = os.open(path, os.O_RDONLY | getattr(os, "O_DIRECTORY", 0) | getattr(os, "O_NOFOLLOW", 0))
    try: os.fsync(descriptor)
    finally: os.close(descriptor)
PY' \
            /bin/sh "$remote_temporary" "$REMOTE_FREEZE_PLAN_PATH" "$expected_sha"
    done
}

prepare_writers() {
    # Plan mode still authenticates the transport contract.  Its private SSH
    # identity copy must disappear before the plan command returns.
    begin_temporary_scope
    local legacy_validators="" output="" execute=false
    while [ "$#" -gt 0 ]; do
        case "$1" in
            --legacy-validator-set) [ "$#" -ge 2 ] || die "--legacy-validator-set needs a value"; legacy_validators="$2"; shift 2 ;;
            --output) [ "$#" -ge 2 ] || die "--output needs a value"; output="$2"; shift 2 ;;
            --plan) execute=false; shift ;;
            --execute) execute=true; shift ;;
            -h|--help) usage; return 0 ;;
            *) die "unknown prepare-writers option: $1" ;;
        esac
    done
    configure_operator_transport false
    require_absolute_file "$legacy_validators" "legacy validator set"
    case "$output" in /*.json) ;; *) die "--output must be an absolute .json path" ;; esac
    [ "$SSH_USER" = root ] || die "writer preparation requires the sealed root SSH user"
    require_commands git ssh scp python3
    local orchestrator_sha helper_sha expected_go
    orchestrator_sha="$(tracked_source_hash "$ORCHESTRATOR")"
    helper_sha="$(tracked_source_hash "$REMOTE_HELPER")"
    expected_go="STAGE-BARRIERS $orchestrator_sha HELPER $helper_sha"
    printf 'archive fleet: PREPARE-WRITERS authorization=%s\n' "$expected_go"
    printf 'archive fleet: preparation stages only fail-open persistent start barriers, stops/disables process-free alternatives, and seals either a systemd-owned writer or an exact detached root-session writer relationship; the shared allow marker remains present and no writer is stopped\n'
    if [ "$execute" != true ]; then
        printf 'archive fleet: PLAN ONLY; no persistent host file, unit, cgroup, or local audit was changed\n'
        return 0
    fi
    [ "${ARC_RECOVERY_PREPARE_GO:-}" = "$expected_go" ] || \
        die "execution requires ARC_RECOVERY_PREPARE_GO='$expected_go'"
    install_helpers "$helper_sha"
    local log_root node failed=0 pid index
    local pids=() names=()
    log_root="$(mktemp -d)"
    ARCHIVE_FLEET_TEMP_ROOT="$log_root"
    for node in nyc lax ams lhr nrt sgp; do
        run_remote "$node" stage-recovery-barrier "$node" > "$log_root/$node.log" 2>&1 &
        pids+=("$!"); names+=("$node")
    done
    for index in "${!pids[@]}"; do
        if wait "${pids[$index]}"; then
            sed -n '1,40p' "$log_root/${names[$index]}.log"
        else
            printf 'archive fleet: writer preparation failed: %s\n' "${names[$index]}" >&2
            sed -n '1,120p' "$log_root/${names[$index]}.log" >&2
            failed=1
        fi
    done
    [ "$failed" -eq 0 ] || \
        die "preparation is safely resumable while each present allow marker keeps its staged start barrier fail-open"
    ( audit_writers --legacy-validator-set "$legacy_validators" --output "$output" )
}

run_remote() {
    local node="$1"
    shift
    local host remote_command remote_argument quoted_argument
    local remote_wrapper
    host="$(host_for "$node")"
    [ -n "$REMOTE_HELPER_PATH" ] && [ -n "$REMOTE_HELPER_SHA" ] || \
        die "remote helper is not installed for this sealed execution"
    remote_wrapper='PATH=/usr/bin:/bin:/usr/sbin:/sbin; export PATH; unset BASH_ENV ENV CDPATH GLOBIGNORE LD_PRELOAD LD_LIBRARY_PATH PYTHONPATH PYTHONHOME RUSTC_WRAPPER; umask 077; helper=$1 expected=$2; shift 2; exec 9<"$helper"; test -f /proc/self/fd/9; actual=$(sha256sum /proc/self/fd/9 | cut -d" " -f1); test "$actual" = "$expected" || { printf "remote helper hash mismatch\n" >&2; exit 1; }; exec /usr/bin/env -i HOME=/root PATH=/usr/bin:/bin:/usr/sbin:/sbin LANG=C LC_ALL=C /proc/self/fd/9 "$@"'
    remote_command=''
    for remote_argument in /bin/bash -c "$remote_wrapper" /bin/bash \
        "$REMOTE_HELPER_PATH" "$REMOTE_HELPER_SHA" "$@"; do
        printf -v quoted_argument '%q' "$remote_argument"
        if [ -n "$remote_command" ]; then
            remote_command+=" $quoted_argument"
        else
            remote_command="$quoted_argument"
        fi
    done
    # OpenSSH concatenates every argument after the destination without
    # preserving argv boundaries.  Pass one fully quoted command string so the
    # remote login shell reconstructs the exact /bin/bash -c vector.
    ssh "${SSH_OPTIONS[@]}" "$SSH_USER@$host" "$remote_command"
}

run_remote_canonical_input() {
    [ "$#" -ge 3 ] || die "run_remote_canonical_input requires node, input, and mode"
    local node="$1" input="$2"
    shift 2
    require_absolute_file "$input" "remote canonical JSON input"
    python3 - "$input" <<'PY'
import os,pathlib,stat,sys
path=pathlib.Path(sys.argv[1]);details=path.lstat()
if (path.is_symlink() or not stat.S_ISREG(details.st_mode)
        or stat.S_IMODE(details.st_mode)!=0o400 or details.st_nlink!=1
        or details.st_uid not in {0,os.geteuid()}):
    raise SystemExit("remote canonical JSON input must be a protected single-link mode-0400 file")
PY
    run_remote "$node" "$@" < "$input"
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
        "$(freeze_node_field "$freeze_plan" "$node" writer_cgroup_sha256)" \
        "$(freeze_node_field "$freeze_plan" "$node" writer_supervision_mode)" \
        "$(freeze_node_field "$freeze_plan" "$node" supervisor_unit)" \
        "$(freeze_node_field "$freeze_plan" "$node" supervisor_main_pid)" \
        "$(freeze_node_field "$freeze_plan" "$node" supervisor_start_ticks)" \
        "$(freeze_node_field "$freeze_plan" "$node" supervisor_executable_path)" \
        "$(freeze_node_field "$freeze_plan" "$node" supervisor_executable_sha256)" \
        "$(freeze_node_field "$freeze_plan" "$node" supervisor_argv_sha256)" \
        "$(freeze_node_field "$freeze_plan" "$node" supervisor_context_sha256)" \
        "$(freeze_node_field "$freeze_plan" "$node" executable_path)" \
        "$(freeze_node_field "$freeze_plan" "$node" executable_sha256)" \
        "$(freeze_node_field "$freeze_plan" "$node" argv_sha256)" \
        "$(freeze_node_field "$freeze_plan" "$node" data_dir)"
}

run_stopped_status_challenged_exact() {
    local freeze_plan="$1" freeze_sha="$2" capture_id="$3" node="$4"
    local host="$5" challenge="$6" known_hosts="$7" identity="$8"
    local transition_kind="${9:-network-quarantine-active}" transition_sha="${10:--}"
    local remote_command remote_argument quoted_argument
    local remote_arguments
    if [ "$transition_kind" = persistently-stopped-precommit ]; then
        require_hash "$transition_sha" "challenged persistently-stopped transition root"
        remote_arguments=(
            "$REMOTE_HELPER_PATH" "$REMOTE_HELPER_SHA"
            quarantine-round-stopped-status-challenged "$capture_id" "$node"
            "$freeze_sha" "$transition_sha" "$host" "$challenge"
        )
    elif [ "$transition_kind" = network-quarantine-active ]; then
        [ "$transition_sha" = - ] || \
            die "active challenged stopped-status unexpectedly supplied a stopped transition root"
        remote_arguments=(
            "$REMOTE_HELPER_PATH" "$REMOTE_HELPER_SHA"
            stopped-status-challenged "$capture_id" "$node" "$freeze_sha"
            "$(freeze_node_field "$freeze_plan" "$node" validator_address)"
            "$(freeze_node_field "$freeze_plan" "$node" stake)"
            "$(freeze_node_field "$freeze_plan" "$node" writer_pid)"
            "$(freeze_node_field "$freeze_plan" "$node" writer_start_ticks)"
            "$(freeze_node_field "$freeze_plan" "$node" boot_id)"
            "$(freeze_node_field "$freeze_plan" "$node" writer_cgroup_sha256)"
            "$(freeze_node_field "$freeze_plan" "$node" writer_supervision_mode)"
            "$(freeze_node_field "$freeze_plan" "$node" supervisor_unit)"
            "$(freeze_node_field "$freeze_plan" "$node" supervisor_main_pid)"
            "$(freeze_node_field "$freeze_plan" "$node" supervisor_start_ticks)"
            "$(freeze_node_field "$freeze_plan" "$node" supervisor_executable_path)"
            "$(freeze_node_field "$freeze_plan" "$node" supervisor_executable_sha256)"
            "$(freeze_node_field "$freeze_plan" "$node" supervisor_argv_sha256)"
            "$(freeze_node_field "$freeze_plan" "$node" supervisor_context_sha256)"
            "$(freeze_node_field "$freeze_plan" "$node" executable_path)"
            "$(freeze_node_field "$freeze_plan" "$node" executable_sha256)"
            "$(freeze_node_field "$freeze_plan" "$node" argv_sha256)"
            "$(freeze_node_field "$freeze_plan" "$node" data_dir)"
            "$host" "$challenge"
        )
    else
        die "offline-stop challenged transition kind differs for $node"
    fi
    remote_command='/bin/bash -s --'
    for remote_argument in "${remote_arguments[@]}"; do
        printf -v quoted_argument '%q' "$remote_argument"
        remote_command+=" $quoted_argument"
    done
    /usr/bin/env -i PATH=/usr/bin:/bin LANG=C LC_ALL=C TZ=UTC HOME=/var/empty \
        /usr/bin/ssh -F /dev/null \
        -o BatchMode=yes -o ConnectTimeout=10 -o ConnectionAttempts=1 \
        -o ServerAliveInterval=5 -o ServerAliveCountMax=2 \
        -o StrictHostKeyChecking=yes -o "UserKnownHostsFile=$known_hosts" \
        -o GlobalKnownHostsFile=/dev/null -o HostKeyAlgorithms=ssh-ed25519 \
        -o CheckHostIP=yes -o UpdateHostKeys=no -o CanonicalizeHostname=no \
        -o AddressFamily=inet -o ProxyCommand=none -o ProxyJump=none \
        -o ClearAllForwardings=yes -o ForwardAgent=no -o PermitLocalCommand=no \
        -o IdentityAgent=none -o IdentitiesOnly=yes -o PreferredAuthentications=publickey \
        -o PasswordAuthentication=no -o KbdInteractiveAuthentication=no \
        -o ChallengeResponseAuthentication=no -o GSSAPIAuthentication=no \
        -o NumberOfPasswordPrompts=0 -o LogLevel=ERROR -i "$identity" \
        "root@$host" "$remote_command" <<'REMOTE'
set -Eeuo pipefail
PATH=/usr/bin:/bin
export PATH
helper="$1"
expected="$2"
shift 2
exec 9<"$helper"
test -f /proc/self/fd/9
actual="$(/usr/bin/sha256sum /proc/self/fd/9)"
actual="${actual%% *}"
test "$actual" = "$expected" || {
    printf 'remote helper hash mismatch\n' >&2
    exit 1
}
exec /bin/bash /proc/self/fd/9 "$@"
REMOTE
}

offline_stop_node_transition_ref() {
    local evidence="$1" node="$2"
    python3 - "$evidence" "$node" <<'PY'
import json,pathlib,re,sys
value=json.loads(pathlib.Path(sys.argv[1]).read_text(encoding="utf-8"))
rows=[row for row in value.get("nodes",[]) if row.get("node")==sys.argv[2]]
if len(rows)!=1:raise SystemExit("offline-stop node row is missing or ambiguous")
row=rows[0]
active={"node","host","validator_address","stake","stop_complete_sha256",
        "stop_files_sha256","stopped_status_argv_sha256","stopped_status_sha256"}
stopped={"node","host","transition_kind","transition_receipt_sha256",
         "current_status_sha256","persisted_head_sha256"}
if set(row)==active:
    print("network-quarantine-active -")
elif (set(row)==stopped and row.get("transition_kind")=="persistently-stopped-precommit"
      and re.fullmatch(r"[0-9a-f]{64}",str(row.get("transition_receipt_sha256")))):
    print("persistently-stopped-precommit",row["transition_receipt_sha256"])
else:raise SystemExit("offline-stop node transition row differs")
PY
}

run_offline_stop_status_exact() {
    local freeze_plan="$1" freeze_sha="$2" capture_id="$3" evidence="$4" node="$5"
    local transition_kind transition_sha
    read -r transition_kind transition_sha < <(
        offline_stop_node_transition_ref "$evidence" "$node"
    ) || die "cannot resolve offline-stop transition kind for $node"
    if [ "$transition_kind" = network-quarantine-active ]; then
        run_stopped_status_exact "$freeze_plan" "$freeze_sha" "$capture_id" "$node"
    else
        run_remote "$node" quarantine-round-stopped-status "$capture_id" "$node" \
            "$freeze_sha" "$transition_sha"
    fi
}

verify_offline_stop_inputs() {
    local freeze_plan="$1" freeze_sha="$2" evidence="$3" evidence_sha="$4"
    local known_hosts="$5" known_hosts_sha="$6" identity="$7"
    python3 - "$freeze_plan" "${freeze_plan}.sha256" "$freeze_sha" \
        "$evidence" "${evidence}.sha256" "$evidence_sha" \
        "$known_hosts" "$known_hosts_sha" "$identity" <<'PY'
import base64
import datetime
import hashlib
import json
import os
import pathlib
import re
import stat
import struct
import sys

(freeze_raw, freeze_sidecar_raw, freeze_sha, evidence_raw, evidence_sidecar_raw,
 evidence_sha, known_raw, known_sha, identity_raw) = sys.argv[1:]
freeze = pathlib.Path(freeze_raw); freeze_sidecar = pathlib.Path(freeze_sidecar_raw)
evidence = pathlib.Path(evidence_raw); evidence_sidecar = pathlib.Path(evidence_sidecar_raw)
known = pathlib.Path(known_raw); identity = pathlib.Path(identity_raw)
fleet = (
    ("nyc", "149.28.32.76"), ("lax", "140.82.16.112"),
    ("ams", "136.244.109.1"), ("lhr", "104.238.171.11"),
    ("nrt", "202.182.107.41"), ("sgp", "149.28.153.31"),
)
canonical = lambda value: (json.dumps(value, sort_keys=True, separators=(",", ":")) + "\n").encode()
digest = lambda value: hashlib.sha256(value).hexdigest()

def locked(path, mode, label):
    if not path.is_absolute() or os.fspath(path) != os.path.normpath(os.fspath(path)):
        raise SystemExit(f"{label} path is unsafe")
    details = path.lstat()
    if path.is_symlink() or not stat.S_ISREG(details.st_mode) or stat.S_IMODE(details.st_mode) != mode:
        raise SystemExit(f"{label} must be a regular non-symlink mode-{mode:04o} file")
    if details.st_nlink != 1:
        raise SystemExit(f"{label} must be single-linked")
    return path.read_bytes()

freeze_payload = locked(freeze, 0o400, "freeze plan")
freeze_sidecar_payload = locked(freeze_sidecar, 0o400, "freeze sidecar")
plan = json.loads(freeze_payload)
if freeze_payload != canonical(plan) or digest(freeze_payload) != freeze_sha:
    raise SystemExit("freeze plan is not the exact canonical hash")
if freeze_sidecar_payload != f"{freeze_sha}  {freeze.name}\n".encode("ascii"):
    raise SystemExit("freeze sidecar differs")
evidence_payload = locked(evidence, 0o400, "offline-stop evidence")
evidence_sidecar_payload = locked(evidence_sidecar, 0o400, "offline-stop evidence sidecar")
receipt = json.loads(evidence_payload)
if evidence_payload != canonical(receipt) or digest(evidence_payload) != evidence_sha:
    raise SystemExit("offline-stop evidence is not the exact canonical hash")
if evidence_sidecar_payload != f"{evidence_sha}  {evidence.name}\n".encode("ascii"):
    raise SystemExit("offline-stop evidence sidecar differs")
known_payload = locked(known, 0o400, "SSH known-hosts trust anchor")
if digest(known_payload) != known_sha:
    raise SystemExit("SSH known-hosts trust anchor hash differs")
lines = known_payload.decode("ascii").splitlines(keepends=True)
if len(lines) != len(fleet) or any(not line.endswith("\n") for line in lines):
    raise SystemExit("SSH known-hosts trust anchor must contain exactly six lines")
seen_blobs = set()
for (node, host), line in zip(fleet, lines):
    fields = line[:-1].split(" ")
    if len(fields) != 3 or fields[:2] != [host, "ssh-ed25519"]:
        raise SystemExit(f"SSH known-hosts mapping differs for {node}")
    blob = base64.b64decode(fields[2], validate=True)
    prefix = struct.pack(">I", 11) + b"ssh-ed25519" + struct.pack(">I", 32)
    if len(blob) != len(prefix) + 32 or not blob.startswith(prefix) or base64.b64encode(blob).decode() != fields[2]:
        raise SystemExit(f"SSH known-hosts key is malformed for {node}")
    if blob in seen_blobs:
        raise SystemExit("SSH known-hosts trust anchor repeats a host key")
    seen_blobs.add(blob)
locked(identity, 0o400, "SSH private identity")
if identity.stat().st_uid != os.geteuid():
    raise SystemExit("SSH private identity is not operator-owned")
if (plan.get("schema") != "arc.recovery.freeze-plan.v5"
        or [(row.get("name"), row.get("host")) for row in plan.get("nodes", [])] != list(fleet)):
    raise SystemExit("freeze topology differs from the fixed production fleet")
capture = hashlib.sha256(b"ARC recovery capture v2\0" + bytes.fromhex(freeze_sha)).hexdigest()
expected_top = {
    "schema": "arc.validator-vault.offline-stop-evidence.v2",
    "source_main_commit": plan.get("source_commit"),
    "freeze_plan_sha256": freeze_sha,
    "freeze_plan_sidecar_sha256": digest(freeze_sidecar_payload),
    "capture_id": capture,
    "remote_helper_sha256": plan.get("remote_helper_sha256"),
    "remote_helper_path": f"/root/.arc-recovery-helpers/{plan.get('remote_helper_sha256')}/archive-node.sh",
}
if not isinstance(receipt, dict) or set(receipt) != set(expected_top) | {
    "first_quarantine_started_at", "all_controlled_stopped_at",
    "legacy_height_cross_proof", "legacy_maintenance_boundary",
    "legacy_maintenance_boundary_sha256",
    "legacy_maintenance_evidence_bundle_sha256",
    "legacy_live_observation_selection_sha256",
    "legacy_live_observation_generation",
    "observation_generation_receipt_sha256",
    "drive_prefreeze_receipt_sha256",
    "quarantine_generation_ledger_sha256", "nodes"
}:
    raise SystemExit("offline-stop evidence fields differ")
if any(receipt.get(key) != value for key, value in expected_top.items()):
    raise SystemExit("offline-stop evidence source/freeze/helper binding differs")
if not isinstance(receipt.get("nodes"), list) or len(receipt["nodes"]) != len(fleet):
    raise SystemExit("offline-stop evidence is not a complete six-node receipt")
hash_re = re.compile(r"[0-9a-f]{64}")
active_fields = {
    "node", "host", "validator_address", "stake", "stop_complete_sha256",
    "stop_files_sha256", "stopped_status_argv_sha256", "stopped_status_sha256",
}
stopped_fields = {
    "node", "host", "transition_kind", "transition_receipt_sha256",
    "current_status_sha256", "persisted_head_sha256",
}
for (node, host), frozen, row in zip(fleet, plan["nodes"], receipt["nodes"]):
    if (row.get("node"), row.get("host")) != (node, host):
        raise SystemExit(f"offline-stop evidence topology differs for {node}")
    if set(row) == active_fields:
        if ((row.get("validator_address"), row.get("stake"))
                != (frozen.get("validator_address"), frozen.get("stake"))):
            raise SystemExit(f"offline-stop active writer identity differs for {node}")
        roots = [row.get(field) for field in (
            "stop_complete_sha256", "stop_files_sha256",
            "stopped_status_argv_sha256", "stopped_status_sha256",
        )]
    elif set(row) == stopped_fields:
        if row.get("transition_kind") != "persistently-stopped-precommit":
            raise SystemExit(f"offline-stop stopped transition kind differs for {node}")
        roots = [row.get(field) for field in (
            "transition_receipt_sha256", "current_status_sha256", "persisted_head_sha256",
        )]
    else:
        raise SystemExit(f"offline-stop evidence node fields differ for {node}")
    if any(hash_re.fullmatch(str(root)) is None for root in roots):
        raise SystemExit(f"offline-stop evidence node root is malformed for {node}")
try:
    first_quarantine = datetime.datetime.strptime(
        receipt["first_quarantine_started_at"], "%Y-%m-%dT%H:%M:%SZ"
    ).replace(tzinfo=datetime.timezone.utc)
    all_stopped = datetime.datetime.strptime(
        receipt["all_controlled_stopped_at"], "%Y-%m-%dT%H:%M:%SZ"
    ).replace(tzinfo=datetime.timezone.utc)
except (TypeError, ValueError):
    raise SystemExit("offline-stop maintenance boundary timestamps are not canonical UTC")
if first_quarantine > all_stopped:
    raise SystemExit("offline-stop maintenance boundary timestamps are reversed")
cross = receipt["legacy_height_cross_proof"]
if (not isinstance(cross, dict)
        or cross.get("schema") != "arc.recovery.authenticated-legacy-height-fleet.v1"
        or cross.get("source_main_commit") != plan.get("source_commit")
        or cross.get("freeze_plan_sha256") != freeze_sha
        or cross.get("capture_id") != capture
        or [(row.get("node"), row.get("host")) for row in cross.get("nodes", [])] != list(fleet)):
    raise SystemExit("offline-stop authenticated legacy-height cross-proof differs")
boundary = receipt["legacy_maintenance_boundary"]
if (not isinstance(boundary, dict)
        or boundary.get("schema") != "arc.recovery.legacy-maintenance-boundary.v1"
        or digest(canonical(boundary)) != receipt.get("legacy_maintenance_boundary_sha256")
        or boundary.get("legacy_maintenance_evidence_bundle_sha256")
            != receipt.get("legacy_maintenance_evidence_bundle_sha256")
        or boundary.get("quarantine_generation_ledger_sha256")
            != receipt.get("quarantine_generation_ledger_sha256")
        or boundary.get("legacy_live_observation_selection_sha256")
            != receipt.get("legacy_live_observation_selection_sha256")
        or boundary.get("legacy_live_observation_generation")
            != receipt.get("legacy_live_observation_generation")
        or boundary.get("observation_generation_receipt_sha256")
            != receipt.get("observation_generation_receipt_sha256")
        or boundary.get("drive_prefreeze_receipt_sha256")
            != receipt.get("drive_prefreeze_receipt_sha256")
        or boundary.get("source_main_commit") != plan.get("source_commit")
        or boundary.get("freeze_plan_sha256") != freeze_sha
        or boundary.get("capture_id") != capture
        or boundary.get("first_quarantine_started_at") != receipt["first_quarantine_started_at"]
        or boundary.get("all_controlled_stopped_at") != receipt["all_controlled_stopped_at"]
        or boundary.get("continuity_safety_margin") != 128
        or boundary.get("legacy_public_max_height")
            != boundary.get("observed_cutoff_height", -129) + 128
        or boundary.get("global_absence_claimed") is not False
        or [(row.get("node"), row.get("host")) for row in boundary.get("nodes", [])] != list(fleet)):
    raise SystemExit("offline-stop legacy maintenance boundary differs")
PY
}

build_offline_stop_remote_verification() {
    local freeze_plan="$1" freeze_sha="$2" evidence="$3" evidence_sha="$4"
    local known_sha="$5" challenge="$6" started_at="$7" completed_at="$8"
    local duration_ms="$9" status_root="${10}" ssh_sha="${11}"
    python3 - "$freeze_plan" "$freeze_sha" "$evidence" "$evidence_sha" \
        "$known_sha" "$challenge" "$started_at" "$completed_at" "$duration_ms" \
        "$status_root" "$ssh_sha" "$QUARANTINE_ROUND_MODULE" <<'PY'
import hashlib
import json
import pathlib
import re
import stat
import sys

(freeze_raw, freeze_sha, evidence_raw, evidence_sha, known_sha, challenge,
 started_at, completed_at, duration_raw, status_root_raw, ssh_sha,
 rounds_module_raw) = sys.argv[1:]
plan = json.loads(pathlib.Path(freeze_raw).read_text(encoding="utf-8"))
receipt = json.loads(pathlib.Path(evidence_raw).read_text(encoding="utf-8"))
status_root = pathlib.Path(status_root_raw)
fleet = (
    ("nyc", "149.28.32.76"), ("lax", "140.82.16.112"),
    ("ams", "136.244.109.1"), ("lhr", "104.238.171.11"),
    ("nrt", "202.182.107.41"), ("sgp", "149.28.153.31"),
)
canonical = lambda value: (json.dumps(value, sort_keys=True, separators=(",", ":")) + "\n").encode()
digest = lambda value: hashlib.sha256(value).hexdigest()
hash_re = re.compile(r"[0-9a-f]{64}")
active_status_fields = {
    "schema", "capture_id", "node", "host", "freeze_plan_sha256", "validator_address",
    "stake", "stopped", "restart_fenced", "stop_schema", "stop_complete_sha256",
    "stop_files_sha256", "challenge",
}
active_receipt_fields = {
    "node", "host", "validator_address", "stake", "stop_complete_sha256",
    "stop_files_sha256", "stopped_status_argv_sha256", "stopped_status_sha256",
}
stopped_receipt_fields = {
    "node", "host", "transition_kind", "transition_receipt_sha256",
    "current_status_sha256", "persisted_head_sha256",
}
import importlib.util
spec=importlib.util.spec_from_file_location("arc_quarantine_rounds",rounds_module_raw)
if spec is None or spec.loader is None:
    raise SystemExit("remote stop verifier cannot load quarantine-round validator")
rounds=importlib.util.module_from_spec(spec);spec.loader.exec_module(rounds)

def unwrap(wrapper,label):
    if not isinstance(wrapper,dict) or set(wrapper)!={"value","sha256"}:
        raise SystemExit(f"challenged {label} wrapper fields differ")
    raw=canonical(wrapper["value"])
    if digest(raw)!=wrapper["sha256"]:
        raise SystemExit(f"challenged {label} wrapper root differs")
    return wrapper["value"],wrapper["sha256"]

rows = []
for (node, host), frozen, local in zip(fleet, plan["nodes"], receipt["nodes"]):
    path = status_root / f"{node}-challenged-status.json"
    raw = path.read_bytes(); status = json.loads(raw)
    if raw != canonical(status):
        raise SystemExit(f"challenged stopped-status is noncanonical: {node}")
    if set(local)==active_receipt_fields:
        if set(status)!=active_status_fields:
            raise SystemExit(f"challenged active stopped-status fields differ: {node}")
        expected = {
            "schema": "arc.recovery.offline-stop-challenged-status.v1",
            "capture_id": receipt["capture_id"], "node": node, "host": host,
            "freeze_plan_sha256": freeze_sha, "validator_address": frozen["validator_address"],
            "stake": frozen["stake"], "stopped": True, "restart_fenced": True,
            "stop_schema": "arc.recovery.offline-stop.v4",
            "stop_complete_sha256": local["stop_complete_sha256"],
            "stop_files_sha256": local["stop_files_sha256"], "challenge": challenge,
        }
        if status != expected:
            raise SystemExit(f"challenged active stopped-status differs from sealed stop tree: {node}")
    elif set(local)==stopped_receipt_fields:
        stopped_status_fields={
            "schema","capture_id","freeze_plan_sha256","node","host","transition_kind",
            "transition_receipt","current_status","challenge",
        }
        if (set(status)!=stopped_status_fields
                or status.get("schema")
                    !="arc.recovery.quarantine-persistently-stopped-challenged-status.v1"
                or (status.get("capture_id"),status.get("freeze_plan_sha256"),
                    status.get("node"),status.get("host"),status.get("transition_kind"),
                    status.get("challenge"))!=(receipt["capture_id"],freeze_sha,node,host,
                        "persistently-stopped-precommit",challenge)
                or local.get("transition_kind")!="persistently-stopped-precommit"):
            raise SystemExit(f"challenged persistently-stopped identity differs: {node}")
        transition,transition_sha=unwrap(status["transition_receipt"],f"{node} transition")
        current,_current_sha=unwrap(status["current_status"],f"{node} current status")
        try:
            projection=rounds.validate_node_transition(transition)
            rounds.validate_prior_fenced_status(
                current,transition=transition,transition_sha256=transition_sha
            )
        except rounds.QuarantineRoundError as error:
            raise SystemExit(f"challenged persistently-stopped proof differs: {node}: {error}") from error
        if (projection.get("kind")!=rounds.STOPPED_PRECOMMIT_TRANSITION_KIND
                or transition_sha!=local.get("transition_receipt_sha256")
                or transition.get("node")!=node or transition.get("host")!=host
                or any(hash_re.fullmatch(str(local.get(field))) is None for field in (
                    "current_status_sha256","persisted_head_sha256"))):
            raise SystemExit(f"challenged persistently-stopped ancestry differs: {node}")
    else:
        raise SystemExit(f"offline-stop challenged node receipt fields differ: {node}")
    rows.append({"node": node, "host": host, "status": status, "status_sha256": digest(raw)})
helper_sha = plan["remote_helper_sha256"]
value = {
    "schema": "arc.recovery.offline-stop-remote-verification.v1",
    "source_main_commit": plan["source_commit"], "freeze_plan_sha256": freeze_sha,
    "capture_id": receipt["capture_id"], "remote_helper_sha256": helper_sha,
    "remote_helper_path": f"/root/.arc-recovery-helpers/{helper_sha}/archive-node.sh",
    "offline_stop_evidence_sha256": evidence_sha, "ssh_known_hosts_sha256": known_sha,
    "ssh_path": "/usr/bin/ssh", "ssh_sha256": ssh_sha, "challenge": challenge,
    "started_at": started_at, "completed_at": completed_at,
    "duration_ms": int(duration_raw), "nodes": rows,
}
sys.stdout.buffer.write(canonical(value))
PY
}

verify_offline_stop_transport_tools() {
    local python_path="$1" python_sha="$2" ssh_sha="$3"
    case "$python_path" in
        /usr/bin/python3|/usr/bin/python3.[0-9]*) ;;
        *) die "reviewed Python path is outside the fixed /usr/bin family" ;;
    esac
    require_hash "$python_sha" "reviewed Python hash"
    require_hash "$ssh_sha" "reviewed OpenSSH hash"
    [ -x "$python_path" ] && [ -f "$python_path" ] && [ ! -L "$python_path" ] || \
        die "reviewed absolute Python target is unavailable or unsafe"
    [ -x /usr/bin/ssh ] && [ -f /usr/bin/ssh ] && [ ! -L /usr/bin/ssh ] || \
        die "reviewed absolute OpenSSH client is unavailable or unsafe"
    /usr/bin/env -i HOME=/var/empty PATH=/usr/bin:/bin LANG=C LC_ALL=C \
        "$python_path" -I - "$python_path" "$python_sha" "$ssh_sha" <<'PY'
import hashlib
import os
import pathlib
import re
import stat
import sys

python_raw, python_sha, ssh_sha = sys.argv[1:]
hash_re = re.compile(r"[0-9a-f]{64}")
if hash_re.fullmatch(python_sha) is None or hash_re.fullmatch(ssh_sha) is None:
    raise SystemExit("reviewed transport-tool hash is malformed")

def protected_ancestry(path, label):
    current = pathlib.Path("/")
    ancestry = [current]
    for component in path.parent.parts[1:]:
        current /= component
        ancestry.append(current)
    for ancestor in ancestry:
        details = os.lstat(ancestor)
        if (stat.S_ISLNK(details.st_mode) or not stat.S_ISDIR(details.st_mode)
                or details.st_uid != 0 or details.st_mode & 0o022):
            raise SystemExit(f"{label} has an unprotected system ancestor: {ancestor}")

def reviewed_python_entrypoint():
    candidate = pathlib.Path("/usr/bin/python3")
    protected_ancestry(candidate, "reviewed Python")
    seen = set()
    for _ in range(8):
        if candidate in seen:
            raise SystemExit("reviewed Python entry point contains a symlink cycle")
        seen.add(candidate)
        details = os.lstat(candidate)
        if not stat.S_ISLNK(details.st_mode):
            if (candidate.parent != pathlib.Path("/usr/bin")
                    or re.fullmatch(r"python3(?:\.[0-9]+)?", candidate.name) is None):
                raise SystemExit("reviewed Python resolved outside /usr/bin")
            return candidate
        if details.st_uid != 0:
            raise SystemExit("reviewed Python entry-point symlink is not root-owned")
        target = pathlib.Path(os.readlink(candidate))
        candidate = target if target.is_absolute() else candidate.parent / target
        candidate = pathlib.Path(os.path.normpath(os.fspath(candidate)))
        if candidate.parent != pathlib.Path("/usr/bin"):
            raise SystemExit("reviewed Python symlink resolves outside /usr/bin")
    raise SystemExit("reviewed Python entry point exceeds symlink-depth bound")

def secure_hash(path, expected, label, allow_multiple_hardlinks):
    protected_ancestry(path, label)
    before_path = os.lstat(path)
    if (stat.S_ISLNK(before_path.st_mode) or not stat.S_ISREG(before_path.st_mode)
            or before_path.st_uid != 0 or before_path.st_mode & 0o022
            or before_path.st_mode & 0o111 == 0 or before_path.st_nlink < 1):
        raise SystemExit(f"{label} is not a protected executable regular file")
    if not allow_multiple_hardlinks and before_path.st_nlink != 1:
        raise SystemExit(f"{label} has an unreviewed hard-link count")
    flags = os.O_RDONLY | getattr(os, "O_NOFOLLOW", 0) | getattr(os, "O_CLOEXEC", 0)
    descriptor = os.open(path, flags)
    try:
        before = os.fstat(descriptor)
        identity = lambda value: (
            value.st_dev, value.st_ino, value.st_mode, value.st_uid, value.st_nlink,
            value.st_size, value.st_mtime_ns, value.st_ctime_ns,
        )
        if identity(before) != identity(before_path):
            raise SystemExit(f"{label} changed before it was opened")
        digest = hashlib.sha256()
        size = 0
        while True:
            chunk = os.read(descriptor, 1024 * 1024)
            if not chunk:
                break
            digest.update(chunk); size += len(chunk)
        after = os.fstat(descriptor)
        after_path = os.lstat(path)
        if size <= 0 or identity(before) != identity(after) or identity(before) != identity(after_path):
            raise SystemExit(f"{label} changed while it was hashed")
    finally:
        os.close(descriptor)
    if digest.hexdigest() != expected:
        raise SystemExit(f"{label} bytes differ from the builder-reviewed hash")

python_path = pathlib.Path(python_raw)
if python_path != reviewed_python_entrypoint():
    raise SystemExit("reviewed Python path differs from /usr/bin/python3 resolution")
secure_hash(python_path, python_sha, "reviewed Python", True)
secure_hash(pathlib.Path("/usr/bin/ssh"), ssh_sha, "reviewed OpenSSH", False)
PY
}

verify_offline_stop_phase() {
    # The challenged verifier allocates a private Python HOME before its
    # evidence scratch directory.  Own both from the first instruction.
    begin_temporary_scope
    local freeze_plan="" evidence="" evidence_sha="" known_hosts="" known_sha=""
    local identity="" challenge="" python_path="" python_sha="" ssh_sha=""
    while [ "$#" -gt 0 ]; do
        case "$1" in
            --freeze-plan) [ "$#" -ge 2 ] || die "--freeze-plan needs a value"; freeze_plan="$2"; shift 2 ;;
            --offline-stop-evidence) [ "$#" -ge 2 ] || die "--offline-stop-evidence needs a value"; evidence="$2"; shift 2 ;;
            --offline-stop-evidence-sha256) [ "$#" -ge 2 ] || die "--offline-stop-evidence-sha256 needs a value"; evidence_sha="$2"; shift 2 ;;
            --ssh-known-hosts) [ "$#" -ge 2 ] || die "--ssh-known-hosts needs a value"; known_hosts="$2"; shift 2 ;;
            --ssh-known-hosts-sha256) [ "$#" -ge 2 ] || die "--ssh-known-hosts-sha256 needs a value"; known_sha="$2"; shift 2 ;;
            --ssh-identity) [ "$#" -ge 2 ] || die "--ssh-identity needs a value"; identity="$2"; shift 2 ;;
            --python-path) [ "$#" -ge 2 ] || die "--python-path needs a value"; python_path="$2"; shift 2 ;;
            --python-sha256) [ "$#" -ge 2 ] || die "--python-sha256 needs a value"; python_sha="$2"; shift 2 ;;
            --ssh-sha256) [ "$#" -ge 2 ] || die "--ssh-sha256 needs a value"; ssh_sha="$2"; shift 2 ;;
            --challenge) [ "$#" -ge 2 ] || die "--challenge needs a value"; challenge="$2"; shift 2 ;;
            *) die "unknown verify-offline-stop option: $1" ;;
        esac
    done
    require_hash "$evidence_sha" "offline-stop evidence hash"
    require_hash "$known_sha" "SSH known-hosts hash"
    require_hash "$challenge" "offline-stop challenge"
    require_absolute_file "$freeze_plan" "sealed freeze plan"
    require_absolute_file "$evidence" "offline-stop evidence"
    require_absolute_file "$known_hosts" "SSH known-hosts trust anchor"
    require_absolute_file "$identity" "SSH private identity"
    ARC_RECOVERY_PYTHON_PATH="$python_path"
    ARC_RECOVERY_PYTHON_SHA256="$python_sha"
    configure_operator_python
    verify_offline_stop_transport_tools "$python_path" "$python_sha" "$ssh_sha"
    local freeze_sha capture_id
    freeze_sha="$(freeze_plan_hash "$freeze_plan")"
    capture_id="$(capture_id_for_freeze_plan_hash "$freeze_sha")"
    verify_offline_stop_inputs "$freeze_plan" "$freeze_sha" "$evidence" "$evidence_sha" \
        "$known_hosts" "$known_sha" "$identity"
    REMOTE_HELPER_SHA="$(manifest_field "$freeze_plan" remote_helper_sha256)"
    require_hash "$REMOTE_HELPER_SHA" "sealed remote helper hash"
    REMOTE_HELPER_PATH="/root/.arc-recovery-helpers/$REMOTE_HELPER_SHA/archive-node.sh"
    local temporary started started_at completed completed_at duration_ms node host failed=0 index
    local transition_kind transition_sha
    local pids=() names=()
    temporary="$(/usr/bin/mktemp -d)"
    ARCHIVE_FLEET_TEMP_ROOT="$temporary"
    read -r started started_at < <(python3 - <<'PY'
import datetime, time
print(time.monotonic_ns(), datetime.datetime.now(datetime.timezone.utc).strftime("%Y-%m-%dT%H:%M:%SZ"))
PY
)
    for node in nyc lax ams lhr nrt sgp; do
        host="$(host_for "$node")"
        read -r transition_kind transition_sha < <(
            offline_stop_node_transition_ref "$evidence" "$node"
        ) || die "cannot resolve challenged offline-stop transition for $node"
        ( ulimit -f 128
          run_stopped_status_challenged_exact "$freeze_plan" "$freeze_sha" "$capture_id" \
            "$node" "$host" "$challenge" "$known_hosts" "$identity" \
            "$transition_kind" "$transition_sha" ) \
            > "$temporary/$node-challenged-status.json" 2> "$temporary/$node.stderr" &
        pids+=("$!"); names+=("$node")
    done
    for index in "${!pids[@]}"; do
        if ! wait "${pids[$index]}"; then
            /usr/bin/sed -n '1,40p' "$temporary/${names[$index]}.stderr" >&2
            failed=1
        fi
    done
    [ "$failed" -eq 0 ] || die "fresh challenged stopped-status failed on one or more fixed hosts"
    read -r completed completed_at < <(python3 - <<'PY'
import datetime, time
print(time.monotonic_ns(), datetime.datetime.now(datetime.timezone.utc).strftime("%Y-%m-%dT%H:%M:%SZ"))
PY
)
    duration_ms="$(( (completed - started) / 1000000 ))"
    [ "$duration_ms" -le 120000 ] || die "fresh six-host verification exceeded 120 seconds"
    build_offline_stop_remote_verification "$freeze_plan" "$freeze_sha" "$evidence" \
        "$evidence_sha" "$known_sha" "$challenge" "$started_at" "$completed_at" \
        "$duration_ms" "$temporary" "$ssh_sha"
}

create_offline_stop_evidence() {
    local freeze_plan="$1" freeze_sha="$2" capture_id="$3" status_root="$4" output="$5"
    local first_quarantine_started_at="$6" all_controlled_stopped_at="$7"
    local legacy_height_cross_proof="$8" maintenance_boundary="$9"
    local maintenance_evidence_bundle="${10}"
    local helper_sha helper_path
    helper_sha="$(manifest_field "$freeze_plan" remote_helper_sha256)"
    helper_path="/root/.arc-recovery-helpers/$helper_sha/archive-node.sh"
    python3 - "$freeze_plan" "${freeze_plan}.sha256" "$freeze_sha" "$capture_id" \
        "$helper_sha" "$helper_path" "$status_root" "$output" \
        "$first_quarantine_started_at" "$all_controlled_stopped_at" \
        "$legacy_height_cross_proof" "$maintenance_boundary" \
        "$maintenance_evidence_bundle" "$QUARANTINE_ROUND_MODULE" <<'PY'
import datetime
import hashlib
import json
import os
import pathlib
import re
import stat
import sys

(plan_raw, sidecar_raw, freeze_sha, capture_id, helper_sha, helper_path,
 status_root_raw, output_raw, first_quarantine_started_at,
 all_controlled_stopped_at, cross_proof_raw, maintenance_boundary_raw,
 maintenance_evidence_bundle_raw, rounds_module_raw) = sys.argv[1:]
plan_path = pathlib.Path(plan_raw)
sidecar_path = pathlib.Path(sidecar_raw)
status_root = pathlib.Path(status_root_raw)
output = pathlib.Path(output_raw)
cross_proof_path = pathlib.Path(cross_proof_raw)
maintenance_boundary_path = pathlib.Path(maintenance_boundary_raw)
maintenance_evidence_bundle_path = pathlib.Path(maintenance_evidence_bundle_raw)
sidecar_output = output.with_name(output.name + ".sha256")
nodes = (
    ("nyc", "149.28.32.76"),
    ("lax", "140.82.16.112"),
    ("ams", "136.244.109.1"),
    ("lhr", "104.238.171.11"),
    ("nrt", "202.182.107.41"),
    ("sgp", "149.28.153.31"),
)
hash_re = re.compile(r"[0-9a-f]{64}")
commit_re = re.compile(r"[0-9a-f]{40}")
utc_re = re.compile(r"\d{4}-\d{2}-\d{2}T\d{2}:\d{2}:\d{2}Z")

def canonical(value):
    return (json.dumps(value, sort_keys=True, separators=(",", ":")) + "\n").encode()

def digest(value):
    return hashlib.sha256(value).hexdigest()

if (not output.is_absolute() or output.suffix != ".json"
        or os.fspath(output) != os.path.normpath(os.fspath(output))):
    raise SystemExit("offline-stop evidence output must be a normalized absolute .json path")
for parent in (output.parent, *output.parents):
    if parent == pathlib.Path("/"):
        break
    details = parent.lstat()
    if parent.is_symlink() or not stat.S_ISDIR(details.st_mode):
        raise SystemExit("offline-stop evidence output has an unsafe parent")
terminal_exists = (output.exists() or output.is_symlink(),
                   sidecar_output.exists() or sidecar_output.is_symlink())
if terminal_exists[1] and not terminal_exists[0]:
    raise SystemExit("offline-stop evidence sidecar exists without its ordered primary file")

plan_payload = plan_path.read_bytes()
plan = json.loads(plan_payload)
if plan_payload != canonical(plan) or digest(plan_payload) != freeze_sha:
    raise SystemExit("offline-stop evidence freeze plan differs from pinned canonical bytes")
freeze_sidecar = sidecar_path.read_bytes()
if freeze_sidecar != f"{freeze_sha}  {plan_path.name}\n".encode("ascii"):
    raise SystemExit("offline-stop evidence freeze sidecar differs")
if (plan.get("schema") != "arc.recovery.freeze-plan.v5"
        or plan.get("remote_helper_sha256") != helper_sha
        or hash_re.fullmatch(helper_sha) is None
        or helper_path != f"/root/.arc-recovery-helpers/{helper_sha}/archive-node.sh"):
    raise SystemExit("offline-stop evidence helper binding differs from the freeze plan")
source_commit = plan.get("source_commit")
if not isinstance(source_commit, str) or commit_re.fullmatch(source_commit) is None:
    raise SystemExit("offline-stop evidence requires a protected-main 40-character source commit")
if (utc_re.fullmatch(first_quarantine_started_at) is None
        or utc_re.fullmatch(all_controlled_stopped_at) is None):
    raise SystemExit("offline-stop maintenance boundary timestamps are not canonical UTC")
first_time = datetime.datetime.strptime(
    first_quarantine_started_at, "%Y-%m-%dT%H:%M:%SZ"
).replace(tzinfo=datetime.timezone.utc)
all_time = datetime.datetime.strptime(
    all_controlled_stopped_at, "%Y-%m-%dT%H:%M:%SZ"
).replace(tzinfo=datetime.timezone.utc)
if first_time > all_time:
    raise SystemExit("offline-stop maintenance boundary timestamps are reversed")
plan_nodes = plan.get("nodes")
if (not isinstance(plan_nodes, list)
        or [(row.get("name"), row.get("host")) for row in plan_nodes] != list(nodes)):
    raise SystemExit("offline-stop evidence freeze topology differs from the fixed production fleet")

bundle_details = maintenance_evidence_bundle_path.lstat()
bundle_payload = maintenance_evidence_bundle_path.read_bytes()
bundle = json.loads(bundle_payload)
if (maintenance_evidence_bundle_path.is_symlink()
        or not stat.S_ISREG(bundle_details.st_mode)
        or bundle_payload != canonical(bundle)
        or bundle.get("schema") != "arc.recovery.legacy-maintenance-evidence-bundle.v1"
        or (bundle.get("source_main_commit"), bundle.get("freeze_plan_sha256"),
            bundle.get("capture_id")) != (source_commit, freeze_sha, capture_id)):
    raise SystemExit("offline-stop maintenance evidence bundle identity differs")
bundle_rows = bundle.get("nodes")
if (not isinstance(bundle_rows, list)
        or [(row.get("node"), row.get("host")) for row in bundle_rows] != list(nodes)):
    raise SystemExit("offline-stop maintenance evidence bundle topology differs")
import importlib.util
spec=importlib.util.spec_from_file_location("arc_quarantine_rounds",rounds_module_raw)
if spec is None or spec.loader is None:
    raise SystemExit("offline-stop evidence cannot load quarantine-round validator")
rounds=importlib.util.module_from_spec(spec);spec.loader.exec_module(rounds)

def unwrap(wrapper,label):
    if not isinstance(wrapper,dict) or set(wrapper)!={"value","sha256"}:
        raise SystemExit(f"offline-stop {label} wrapper differs")
    raw=canonical(wrapper["value"])
    if digest(raw)!=wrapper["sha256"]:
        raise SystemExit(f"offline-stop {label} wrapper root differs")
    return wrapper["value"],wrapper["sha256"]

status_fields = {
    "schema", "capture_id", "node", "freeze_plan_sha256", "validator_address",
    "stake", "stopped", "restart_fenced", "stop_schema",
    "stop_complete_sha256", "stop_files_sha256",
}
argv_fields = (
    "validator_address", "stake", "writer_pid", "writer_start_ticks", "boot_id",
    "writer_cgroup_sha256", "writer_supervision_mode", "supervisor_unit",
    "supervisor_main_pid", "supervisor_start_ticks", "supervisor_executable_path",
    "supervisor_executable_sha256", "supervisor_argv_sha256",
    "supervisor_context_sha256", "executable_path", "executable_sha256",
    "argv_sha256", "data_dir",
)
receipt_nodes = []
active_completion_roots = []
for (name, host), frozen, bundle_row in zip(nodes, plan_nodes, bundle_rows):
    status_path = status_root / f"{name}-stopped-status.json"
    details = status_path.lstat()
    if status_path.is_symlink() or not stat.S_ISREG(details.st_mode):
        raise SystemExit(f"offline-stop status is unsafe: {name}")
    status_payload = status_path.read_bytes()
    status = json.loads(status_payload)
    if status_payload != canonical(status):
        raise SystemExit(f"offline-stop status is noncanonical: {name}")
    if bundle_row.get("transition_kind") == rounds.STOPPED_PRECOMMIT_TRANSITION_KIND:
        stopped_fields={"node","host","transition_kind","transition_receipt",
                        "current_status","persisted_head"}
        if set(bundle_row)!=stopped_fields or (bundle_row.get("node"),bundle_row.get("host"))!=(name,host):
            raise SystemExit(f"offline-stop stopped maintenance row fields differ: {name}")
        transition,transition_sha=unwrap(bundle_row["transition_receipt"],f"{name} transition")
        historical_status,historical_status_sha=unwrap(
            bundle_row["current_status"],f"{name} historical current status"
        )
        persisted,persisted_sha=unwrap(bundle_row["persisted_head"],f"{name} persisted head")
        try:
            projection=rounds.validate_node_transition(transition)
            rounds.validate_prior_fenced_status(
                status,transition=transition,transition_sha256=transition_sha
            )
            rounds.validate_prior_fenced_status(
                historical_status,transition=transition,transition_sha256=transition_sha
            )
        except rounds.QuarantineRoundError as error:
            raise SystemExit(f"offline-stop stopped transition/status differs: {name}: {error}") from error
        if (projection.get("kind")!=rounds.STOPPED_PRECOMMIT_TRANSITION_KIND
                or transition.get("node")!=name or transition.get("host")!=host
                or persisted.get("source_pair_role")!="preauthorization-boundary"
                or persisted.get("head")!=transition.get("stable_head")
                or transition.get("persisted_head",{}).get("sha256")!=persisted_sha
                or transition.get("persisted_head",{}).get("value")!=persisted):
            raise SystemExit(f"offline-stop stopped transition ancestry differs: {name}")
        receipt_nodes.append({
            "host":host,"node":name,
            "transition_kind":"persistently-stopped-precommit",
            "transition_receipt_sha256":transition_sha,
            "current_status_sha256":historical_status_sha,
            "persisted_head_sha256":persisted_sha,
        })
        continue
    if set(status) != status_fields:
        raise SystemExit(f"offline-stop active status has inexact fields: {name}")
    expected = {
        "schema": "arc.recovery.offline-stop-status.v1",
        "capture_id": capture_id,
        "node": name,
        "freeze_plan_sha256": freeze_sha,
        "validator_address": frozen["validator_address"],
        "stake": frozen["stake"],
        "stopped": True,
        "restart_fenced": True,
        "stop_schema": "arc.recovery.offline-stop.v4",
    }
    if any(status.get(field) != value for field, value in expected.items()):
        raise SystemExit(f"offline-stop status identity differs: {name}")
    for field in ("validator_address", "stop_complete_sha256", "stop_files_sha256"):
        if hash_re.fullmatch(status.get(field, "")) is None:
            raise SystemExit(f"offline-stop status {field} is malformed: {name}")
    argv = ["stopped-status", capture_id, name, freeze_sha]
    argv.extend(str(frozen[field]) for field in argv_fields)
    receipt_nodes.append({
        "host": host,
        "node": name,
        "stake": frozen["stake"],
        "stop_complete_sha256": status["stop_complete_sha256"],
        "stop_files_sha256": status["stop_files_sha256"],
        "stopped_status_argv_sha256": digest(canonical(argv)),
        "stopped_status_sha256": digest(status_payload),
        "validator_address": frozen["validator_address"],
    })
    active_completion_roots.append(status["stop_complete_sha256"])
if len(set(active_completion_roots)) != len(active_completion_roots):
    raise SystemExit("offline-stop active completion roots are not unique per validator")

cross_details = cross_proof_path.lstat()
cross_payload = cross_proof_path.read_bytes()
cross_proof = json.loads(cross_payload)
if (cross_proof_path.is_symlink() or not stat.S_ISREG(cross_details.st_mode)
        or cross_payload != canonical(cross_proof)
        or cross_proof.get("schema") != "arc.recovery.authenticated-legacy-height-fleet.v1"
        or cross_proof.get("source_main_commit") != source_commit
        or cross_proof.get("freeze_plan_sha256") != freeze_sha
        or cross_proof.get("capture_id") != capture_id
        or not isinstance(cross_proof.get("conservative_height_floor"), int)
        or isinstance(cross_proof.get("conservative_height_floor"), bool)
        or cross_proof["conservative_height_floor"] < 0):
    raise SystemExit("offline-stop authenticated legacy-height cross-proof differs")
cross_nodes = cross_proof.get("nodes")
if (not isinstance(cross_nodes, list) or len(cross_nodes) != len(nodes)
        or [(row.get("node"), row.get("host")) for row in cross_nodes] != list(nodes)):
    raise SystemExit("offline-stop authenticated legacy-height topology differs")
for row in cross_nodes:
    if (set(row) != {"node", "host", "proof", "proof_sha256"}
            or hash_re.fullmatch(row.get("proof_sha256", "")) is None
            or digest(canonical(row.get("proof"))) != row["proof_sha256"]):
        raise SystemExit("offline-stop authenticated legacy-height proof row differs")

boundary_details = maintenance_boundary_path.lstat()
boundary_payload = maintenance_boundary_path.read_bytes()
boundary = json.loads(boundary_payload)
boundary_fields = {
    "schema", "source_main_commit", "freeze_plan_sha256", "capture_id",
    "first_quarantine_started_at", "all_controlled_stopped_at", "created_at",
    "official_origin_scope", "legacy_public_height_receipt",
    "authenticated_prefence_height_cross_proof_sha256",
    "legacy_live_observation_selection_sha256", "legacy_live_observation_generation",
    "observation_generation_receipt_sha256", "drive_prefreeze_receipt_sha256",
    "quarantine_generation_ledger_sha256",
    "legacy_maintenance_evidence_bundle_sha256", "network_quarantine_challenge",
    "network_quarantine_stability_proof_sha256",
    "tools", "nodes", "evidence_heights", "observed_cutoff_height", "continuity_safety_margin",
    "continuity_safety_margin_policy", "legacy_public_max_height",
    "global_absence_claimed", "reopening_policy", "late_fork_circuit",
    "threat_model",
}
if (maintenance_boundary_path.is_symlink() or not stat.S_ISREG(boundary_details.st_mode)
        or boundary_details.st_nlink != 1 or boundary_details.st_uid not in {0, os.geteuid()}
        or boundary_details.st_mode & 0o022 or boundary_payload != canonical(boundary)
        or set(boundary) != boundary_fields
        or boundary.get("schema") != "arc.recovery.legacy-maintenance-boundary.v1"
        or (boundary.get("source_main_commit"), boundary.get("freeze_plan_sha256"),
            boundary.get("capture_id")) != (source_commit, freeze_sha, capture_id)
        or boundary.get("first_quarantine_started_at") != first_quarantine_started_at
        or boundary.get("all_controlled_stopped_at") != all_controlled_stopped_at
        or boundary.get("global_absence_claimed") is not False
        or boundary.get("continuity_safety_margin") != 128
        or boundary.get("legacy_public_max_height")
            != boundary.get("observed_cutoff_height", -129) + 128):
    raise SystemExit("offline-stop legacy maintenance boundary differs")
boundary_nodes = boundary.get("nodes")
if (not isinstance(boundary_nodes, list)
        or [(row.get("node"), row.get("host")) for row in boundary_nodes] != list(nodes)):
    raise SystemExit("offline-stop maintenance-boundary topology differs")
boundary_sidecar = maintenance_boundary_path.with_name(maintenance_boundary_path.name + ".sha256")
boundary_sidecar_details = boundary_sidecar.lstat()
boundary_sidecar_payload = boundary_sidecar.read_bytes()
boundary_sha = digest(boundary_payload)
if (boundary_sidecar.is_symlink() or not stat.S_ISREG(boundary_sidecar_details.st_mode)
        or boundary_sidecar_details.st_nlink != 1
        or boundary_sidecar_details.st_uid not in {0, os.geteuid()}
        or stat.S_IMODE(boundary_sidecar_details.st_mode) != 0o400
        or boundary_sidecar_payload != f"{boundary_sha}  {maintenance_boundary_path.name}\n".encode("ascii")):
    raise SystemExit("offline-stop maintenance-boundary sidecar differs")

bundle_sha = digest(bundle_payload)
selection_sealed = bundle.get("live_observation_selection")
if (not isinstance(selection_sealed, dict) or set(selection_sealed) != {"value", "sha256"}
        or not isinstance(selection_sealed.get("value"), dict)
        or digest(canonical(selection_sealed["value"])) != selection_sealed.get("sha256")
        or boundary.get("legacy_live_observation_selection_sha256") != selection_sealed["sha256"]
        or boundary.get("legacy_live_observation_generation")
            != selection_sealed["value"].get("observation_generation")
        or boundary.get("observation_generation_receipt_sha256")
            != selection_sealed["value"].get("observation_generation_receipt_sha256")
        or boundary.get("drive_prefreeze_receipt_sha256")
            != selection_sealed["value"].get("drive_prefreeze_receipt_sha256")):
    raise SystemExit("offline-stop live-observation selection differs")
bundle_sidecar = maintenance_evidence_bundle_path.with_name(
    maintenance_evidence_bundle_path.name + ".sha256"
)
bundle_sidecar_details = bundle_sidecar.lstat()
if (maintenance_evidence_bundle_path.is_symlink() or not stat.S_ISREG(bundle_details.st_mode)
        or bundle_details.st_nlink != 1 or bundle_details.st_uid not in {0, os.geteuid()}
        or stat.S_IMODE(bundle_details.st_mode) != 0o400
        or bundle_payload != canonical(bundle)
        or bundle.get("schema") != "arc.recovery.legacy-maintenance-evidence-bundle.v1"
        or (bundle.get("source_main_commit"), bundle.get("freeze_plan_sha256"),
            bundle.get("capture_id")) != (source_commit, freeze_sha, capture_id)
        or bundle.get("first_quarantine_started_at") != first_quarantine_started_at
        or bundle.get("all_controlled_stopped_at") != all_controlled_stopped_at
        or boundary.get("legacy_maintenance_evidence_bundle_sha256") != bundle_sha
        or boundary.get("network_quarantine_stability_proof_sha256")
            != bundle.get("quarantine_stability_proof", {}).get("sha256")
        or boundary.get("quarantine_generation_ledger_sha256")
            != bundle.get("quarantine_generation_ledger", {}).get("sha256")
        or bundle_sidecar.is_symlink() or not stat.S_ISREG(bundle_sidecar_details.st_mode)
        or bundle_sidecar_details.st_nlink != 1
        or bundle_sidecar_details.st_uid not in {0, os.geteuid()}
        or stat.S_IMODE(bundle_sidecar_details.st_mode) != 0o400
        or bundle_sidecar.read_bytes()
            != f"{bundle_sha}  {maintenance_evidence_bundle_path.name}\n".encode("ascii")):
    raise SystemExit("offline-stop maintenance evidence bundle differs")

value = {
    "all_controlled_stopped_at": all_controlled_stopped_at,
    "capture_id": capture_id,
    "first_quarantine_started_at": first_quarantine_started_at,
    "freeze_plan_sha256": freeze_sha,
    "freeze_plan_sidecar_sha256": digest(freeze_sidecar),
    "legacy_height_cross_proof": cross_proof,
    "legacy_maintenance_boundary": boundary,
    "legacy_maintenance_boundary_sha256": boundary_sha,
    "legacy_maintenance_evidence_bundle_sha256": bundle_sha,
    "legacy_live_observation_selection_sha256": selection_sealed["sha256"],
    "legacy_live_observation_generation": selection_sealed["value"]["observation_generation"],
    "observation_generation_receipt_sha256":
        selection_sealed["value"]["observation_generation_receipt_sha256"],
    "drive_prefreeze_receipt_sha256":
        selection_sealed["value"]["drive_prefreeze_receipt_sha256"],
    "quarantine_generation_ledger_sha256":
        bundle["quarantine_generation_ledger"]["sha256"],
    "nodes": receipt_nodes,
    "remote_helper_path": helper_path,
    "remote_helper_sha256": helper_sha,
    "schema": "arc.validator-vault.offline-stop-evidence.v2",
    "source_main_commit": source_commit,
}
payload = canonical(value)
receipt_sha = digest(payload)
sidecar_payload = f"{receipt_sha}  {output.name}\n".encode("ascii")
dfd=os.open(output.parent,os.O_RDONLY|getattr(os,"O_DIRECTORY",0)|getattr(os,"O_NOFOLLOW",0))
def publish(path,data,label):
    partial=path.with_name(path.name+".partial")
    def read_name(name,modes):
        fd=os.open(name,os.O_RDONLY|getattr(os,"O_NOFOLLOW",0),dir_fd=dfd)
        try:
            details=os.fstat(fd)
            if (not stat.S_ISREG(details.st_mode) or details.st_uid not in {0,os.geteuid()}
                    or details.st_nlink!=1 or stat.S_IMODE(details.st_mode) not in modes
                    or details.st_size<=0 or details.st_size>32*1024*1024):
                raise SystemExit(f"{label} identity differs")
            chunks=[]
            while True:
                chunk=os.read(fd,1024*1024)
                if not chunk:break
                chunks.append(chunk)
            raw=b"".join(chunks)
            if len(raw)!=details.st_size:raise SystemExit(f"{label} changed while read")
            return raw
        finally:os.close(fd)
    if path.exists() or path.is_symlink():
        if read_name(path.name,{0o400})!=data:raise SystemExit(f"existing {label} differs")
        if partial.exists() or partial.is_symlink():
            read_name(partial.name,{0o400,0o600});os.unlink(partial.name,dir_fd=dfd);os.fsync(dfd)
        return
    promote=False
    if partial.exists() or partial.is_symlink():
        if read_name(partial.name,{0o400,0o600})==data:
            os.chmod(partial.name,0o400,dir_fd=dfd,follow_symlinks=False);promote=True
        else:
            os.unlink(partial.name,dir_fd=dfd);os.fsync(dfd)
    if not promote:
        fd=os.open(partial.name,os.O_WRONLY|os.O_CREAT|os.O_EXCL|getattr(os,"O_NOFOLLOW",0),
                   0o600,dir_fd=dfd)
        with os.fdopen(fd,"wb") as handle:
            handle.write(data);handle.flush();os.fsync(handle.fileno());os.fchmod(handle.fileno(),0o400)
    os.rename(partial.name,path.name,src_dir_fd=dfd,dst_dir_fd=dfd);os.fsync(dfd)
try:
    publish(output,payload,"offline-stop evidence")
    publish(sidecar_output,sidecar_payload,"offline-stop evidence sidecar")
finally:os.close(dfd)
print(receipt_sha)
PY
}

verify_offline_stop_evidence_remote() (
    local freeze_plan="$1" freeze_sha="$2" capture_id="$3" evidence="$4" expected_sha="$5"
    local maintenance_evidence_bundle="$6"
    local temporary node derived derived_sha first_quarantine_started_at all_controlled_stopped_at
    require_hash "$expected_sha" "offline-stop evidence hash"
    require_absolute_file "$maintenance_evidence_bundle" "legacy maintenance evidence bundle"
    temporary="$(mktemp -d)"
    trap 'chmod -R u+w "$temporary" 2>/dev/null || true; rm -rf -- "$temporary"' EXIT
    for node in nyc lax ams lhr nrt sgp; do
        run_offline_stop_status_exact "$freeze_plan" "$freeze_sha" "$capture_id" \
            "$evidence" "$node" \
            > "$temporary/$node-stopped-status.json"
    done
    python3 - "$evidence" "$maintenance_evidence_bundle" "$freeze_sha" "$capture_id" <<'PY'
import hashlib,json,pathlib,sys
canonical=lambda value:(json.dumps(value,sort_keys=True,separators=(",",":"))+"\n").encode()
evidence=json.loads(pathlib.Path(sys.argv[1]).read_text(encoding="utf-8"))
path=pathlib.Path(sys.argv[2]);raw=path.read_bytes();bundle=json.loads(raw)
if (raw!=canonical(bundle) or bundle.get("schema")!="arc.recovery.legacy-maintenance-evidence-bundle.v1"
        or bundle.get("freeze_plan_sha256")!=sys.argv[3] or bundle.get("capture_id")!=sys.argv[4]
        or hashlib.sha256(raw).hexdigest()!=evidence.get("legacy_maintenance_evidence_bundle_sha256")
        or evidence.get("legacy_maintenance_boundary",{}).get("legacy_maintenance_evidence_bundle_sha256")
            !=evidence.get("legacy_maintenance_evidence_bundle_sha256")
        or bundle.get("quarantine_generation_ledger",{}).get("sha256")
            !=evidence.get("quarantine_generation_ledger_sha256")
        or bundle.get("live_observation_selection",{}).get("sha256")
            !=evidence.get("legacy_live_observation_selection_sha256")
        or bundle.get("live_observation_selection",{}).get("value",{}).get("observation_generation")
            !=evidence.get("legacy_live_observation_generation")
        or bundle.get("live_observation_selection",{}).get("value",{}).get("observation_generation_receipt_sha256")
            !=evidence.get("observation_generation_receipt_sha256")
        or bundle.get("live_observation_selection",{}).get("value",{}).get("drive_prefreeze_receipt_sha256")
            !=evidence.get("drive_prefreeze_receipt_sha256")):
    raise SystemExit("fresh remote verification maintenance evidence bundle differs")
PY
    local persisted_binary_sha persisted_genesis_sha persisted_validators_sha persisted_legacy_sha
    read -r persisted_binary_sha persisted_genesis_sha persisted_validators_sha persisted_legacy_sha < <(
        python3 - "$maintenance_evidence_bundle" <<'PY'
import json,pathlib,sys
value=json.loads(pathlib.Path(sys.argv[1]).read_text(encoding="utf-8"));rows=value["nodes"]
first=rows[0]["persisted_head"]["value"]
keys=("inspector_binary_sha256","genesis_sha256","validator_public_keys_sha256","legacy_validator_set_sha256")
wanted=tuple(first[key] for key in keys)
if any(tuple(row["persisted_head"]["value"][key] for key in keys)!=wanted for row in rows):
    raise SystemExit("maintenance evidence persisted inspector inputs differ across nodes")
print(*wanted)
PY
    )
    local transition_kind transition_sha
    for node in nyc lax ams lhr nrt sgp; do
        read -r transition_kind transition_sha < <(
            offline_stop_node_transition_ref "$evidence" "$node"
        ) || die "cannot resolve persisted-head transition for $node"
        if [ "$transition_kind" = network-quarantine-active ]; then
            run_persisted_head_exact "$freeze_plan" "$freeze_sha" "$capture_id" "$node" \
                "$persisted_binary_sha" "$persisted_genesis_sha" "$persisted_validators_sha" \
                "$persisted_legacy_sha" > "$temporary/$node-persisted-head.json"
            python3 - "$maintenance_evidence_bundle" \
                "$temporary/$node-persisted-head.json" "$node" <<'PY'
import json,pathlib,sys
canonical=lambda value:(json.dumps(value,sort_keys=True,separators=(",",":"))+"\n").encode()
bundle=json.loads(pathlib.Path(sys.argv[1]).read_text(encoding="utf-8"));raw=pathlib.Path(sys.argv[2]).read_bytes()
rows=[row for row in bundle["nodes"] if row.get("node")==sys.argv[3]]
if len(rows)!=1 or raw!=canonical(rows[0]["persisted_head"]["value"]):
    raise SystemExit(f"fresh persisted-head export differs: {sys.argv[3]}")
PY
        fi
    done
    local challenge
    challenge="$(offline_cross_field "$evidence" fleet challenge)"
    for node in nyc lax ams lhr nrt sgp; do
        run_remote "$node" legacy-height-bracket "$capture_id" "$node" "$freeze_sha" \
            "$(freeze_node_field "$freeze_plan" "$node" writer_pid)" \
            "$(freeze_node_field "$freeze_plan" "$node" writer_start_ticks)" \
            "$(freeze_node_field "$freeze_plan" "$node" boot_id)" \
            "$(freeze_node_field "$freeze_plan" "$node" executable_path)" \
            "$(freeze_node_field "$freeze_plan" "$node" executable_sha256)" \
            "$(freeze_node_field "$freeze_plan" "$node" argv_sha256)" \
            "$(freeze_node_field "$freeze_plan" "$node" rpc_origin)" \
            "$(offline_cross_field "$evidence" "$node" public_info_before_height)" \
            "$(offline_cross_field "$evidence" "$node" public_latest_block_height)" \
            "$(offline_cross_field "$evidence" "$node" public_info_after_height)" \
            "$(offline_cross_field "$evidence" "$node" public_latest_block_hash)" \
            "$challenge" > "$temporary/$node-legacy-height-bracket.json"
    done
    local legacy_height_cross_proof="$temporary/authenticated-legacy-height-cross-proof.json"
    python3 - "$evidence" "$temporary" "$legacy_height_cross_proof" <<'PY'
import json, os, pathlib, sys
evidence = json.loads(pathlib.Path(sys.argv[1]).read_text(encoding="utf-8"))
root = pathlib.Path(sys.argv[2]); output = pathlib.Path(sys.argv[3])
canonical = lambda value: (json.dumps(value, sort_keys=True, separators=(",", ":")) + "\n").encode()
cross = evidence["legacy_height_cross_proof"]
for row in cross["nodes"]:
    observed = (root / f"{row['node']}-legacy-height-bracket.json").read_bytes()
    if observed != canonical(row["proof"]):
        raise SystemExit(f"fresh remote legacy-height proof differs: {row['node']}")
payload = canonical(cross)
descriptor = os.open(output, os.O_WRONLY | os.O_CREAT | os.O_EXCL | getattr(os, "O_NOFOLLOW", 0), 0o400)
with os.fdopen(descriptor, "wb") as handle:
    handle.write(payload); handle.flush(); os.fsync(handle.fileno())
PY
    local maintenance_boundary="$temporary/legacy-maintenance-boundary.json"
    read -r first_quarantine_started_at all_controlled_stopped_at < <(python3 - \
            "$evidence" "$maintenance_boundary" <<'PY'
import hashlib
import json
import os
import pathlib
import sys
value = json.loads(pathlib.Path(sys.argv[1]).read_text(encoding="utf-8"))
boundary=value.get("legacy_maintenance_boundary")
payload=(json.dumps(boundary,sort_keys=True,separators=(",",":"))+"\n").encode()
expected=value.get("legacy_maintenance_boundary_sha256")
if hashlib.sha256(payload).hexdigest()!=expected:
    raise SystemExit("offline-stop embedded maintenance boundary hash differs")
output=pathlib.Path(sys.argv[2])
fd=os.open(output,os.O_WRONLY|os.O_CREAT|os.O_EXCL|getattr(os,"O_NOFOLLOW",0),0o400)
with os.fdopen(fd,"wb") as handle:
    handle.write(payload);handle.flush();os.fsync(handle.fileno())
sidecar=output.with_name(output.name+".sha256")
fd=os.open(sidecar,os.O_WRONLY|os.O_CREAT|os.O_EXCL|getattr(os,"O_NOFOLLOW",0),0o400)
with os.fdopen(fd,"wb") as handle:
    handle.write(f"{expected}  {output.name}\n".encode("ascii"));handle.flush();os.fsync(handle.fileno())
print(value.get("first_quarantine_started_at", ""), value.get("all_controlled_stopped_at", ""))
PY
)
    derived="$temporary/derived-offline-stop-evidence.json"
    derived_sha="$(create_offline_stop_evidence "$freeze_plan" "$freeze_sha" \
        "$capture_id" "$temporary" "$derived" "$first_quarantine_started_at" \
        "$all_controlled_stopped_at" "$legacy_height_cross_proof" \
        "$maintenance_boundary" "$maintenance_evidence_bundle")"
    [ "$derived_sha" = "$expected_sha" ] || \
        die "fresh remote offline-stop roots differ from the sealed rollout receipt"
    python3 - "$evidence" "$derived" "$expected_sha" <<'PY'
import hashlib
import pathlib
import stat
import sys

evidence, derived = map(pathlib.Path, sys.argv[1:3])
expected = sys.argv[3]
sidecar = evidence.with_name(evidence.name + ".sha256")
for path in (evidence, sidecar):
    details = path.lstat()
    if path.is_symlink() or not stat.S_ISREG(details.st_mode) or stat.S_IMODE(details.st_mode) != 0o400:
        raise SystemExit(f"sealed offline-stop evidence is unsafe or not mode 0400: {path}")
payload = evidence.read_bytes()
if hashlib.sha256(payload).hexdigest() != expected or payload != derived.read_bytes():
    raise SystemExit("sealed offline-stop evidence differs from fresh remote stop roots")
if sidecar.read_bytes() != f"{expected}  {evidence.name}\n".encode("ascii"):
    raise SystemExit("sealed offline-stop evidence sidecar differs")
PY
    printf 'archive fleet: PASS six exact tagged remote stop roots match sealed evidence %s\n' \
        "$expected_sha"
)

reserve_live_observation_generation() {
    local root="$1" selected="$2" drive_receipt="$3" freeze_plan="$4"
    local freeze_sha="$5" capture_id="$6" resume_state="$7"
    python3 - "$root" "$selected" "$drive_receipt" "$freeze_plan" \
        "$freeze_sha" "$capture_id" "$resume_state" <<'PY'
import datetime, fcntl, hashlib, json, os, pathlib, re, secrets, stat, sys

root, selected, drive_path, plan_path = map(pathlib.Path, sys.argv[1:5])
freeze_sha, capture_id, resume_state = sys.argv[5:]
canonical = lambda value: (json.dumps(value, sort_keys=True, separators=(",", ":")) + "\n").encode()
digest = lambda raw: hashlib.sha256(raw).hexdigest()
hash_re = re.compile(r"[0-9a-f]{64}")
utc_format = "%Y-%m-%dT%H:%M:%S.%fZ"
maximum_age = 300

def locked(path, label, modes={0o400}, maximum=4 * 1024 * 1024, links={1}):
    fd = os.open(path, os.O_RDONLY | getattr(os, "O_NOFOLLOW", 0))
    try:
        before = os.fstat(fd)
        if (not stat.S_ISREG(before.st_mode) or pathlib.Path(path).is_symlink()
                or before.st_uid != os.geteuid() or before.st_nlink not in links
                or stat.S_IMODE(before.st_mode) not in modes
                or not 0 < before.st_size <= maximum):
            raise SystemExit(f"live-observation {label} is unsafe")
        raw = os.read(fd, maximum + 1)
        if len(raw) != before.st_size:
            raise SystemExit(f"live-observation {label} changed while read")
        return raw
    finally:
        os.close(fd)

if (any(hash_re.fullmatch(value) is None for value in (freeze_sha, capture_id))
        or resume_state not in {"bound", "unbound"}):
    raise SystemExit("live-observation generation capture identity is malformed")
plan_raw = locked(plan_path, "freeze plan", {0o400, 0o600}, 16 * 1024 * 1024)
plan = json.loads(plan_raw)
if (digest(plan_raw) != freeze_sha or canonical(plan) != plan_raw
        or plan.get("schema") != "arc.recovery.freeze-plan.v5"):
    raise SystemExit("live-observation generation freeze plan differs")
source_commit = plan.get("source_commit")
if not isinstance(source_commit, str) or re.fullmatch(r"[0-9a-f]{40}", source_commit) is None:
    raise SystemExit("live-observation generation source commit differs")

parent = root.parent
details = parent.lstat()
if (parent.is_symlink() or not stat.S_ISDIR(details.st_mode)
        or details.st_uid != os.geteuid() or details.st_mode & 0o022):
    raise SystemExit("live-observation generation parent is unsafe")
if root.exists() or root.is_symlink():
    details = root.lstat()
    if root.is_symlink() or not stat.S_ISDIR(details.st_mode) \
            or details.st_uid != os.geteuid() or stat.S_IMODE(details.st_mode) != 0o700:
        raise SystemExit("live-observation generation root is unsafe")
else:
    os.mkdir(root, 0o700)
    dfd = os.open(parent, os.O_RDONLY | getattr(os, "O_DIRECTORY", 0))
    try: os.fsync(dfd)
    finally: os.close(dfd)

lock_path = root / ".generation.lock"
lock_fd = os.open(lock_path, os.O_RDWR | os.O_CREAT | getattr(os, "O_NOFOLLOW", 0), 0o600)
fcntl.flock(lock_fd, fcntl.LOCK_EX)

def validate_generation(path):
    partial_match = re.fullmatch(r"\.([0-9a-f]{64})\.json\.partial", path.name)
    raw = locked(path, "generation receipt", {0o400, 0o600}, links={1, 2})
    value = json.loads(raw)
    expected = {
        "schema", "source_main_commit", "freeze_plan_sha256", "capture_id",
        "observation_generation", "created_at", "max_selection_age_seconds",
        "drive_prefreeze_receipt",
    }
    if (raw != canonical(value) or set(value) != expected
            or value.get("schema") != "arc.recovery.legacy-live-observation-generation.v1"
            or (value.get("source_main_commit"), value.get("freeze_plan_sha256"), value.get("capture_id"))
                != (source_commit, freeze_sha, capture_id)
            or hash_re.fullmatch(str(value.get("observation_generation"))) is None
            or value.get("max_selection_age_seconds") != maximum_age
            or path.name not in {value["observation_generation"] + ".json",
                                 "." + value["observation_generation"] + ".json.partial"}
            or (partial_match is not None
                and partial_match.group(1) != value["observation_generation"])):
        raise SystemExit("live-observation generation receipt identity differs")
    datetime.datetime.strptime(value["created_at"], utc_format)
    drive = value.get("drive_prefreeze_receipt")
    if (not isinstance(drive, dict) or set(drive) != {"path", "sha256", "value"}
            or not isinstance(drive.get("path"), str) or not drive["path"].startswith("/")
            or hash_re.fullmatch(str(drive.get("sha256"))) is None
            or digest(canonical(drive.get("value"))) != drive["sha256"]):
        raise SystemExit("live-observation generation Drive binding differs")
    return value, raw

# Complete an interrupted durable partial -> no-replace hard-link publish.
# A crash after link but before unlink leaves one inode with nlink=2; that is
# an explicit recoverable intermediate, never a malformed final receipt.
for partial in sorted(root.glob(".*.json.partial")):
    try:
        partial_value, partial_raw = validate_generation(partial)
    except (OSError, ValueError, json.JSONDecodeError, SystemExit):
        try: details = partial.lstat()
        except OSError: continue
        if (not partial.is_symlink() and stat.S_ISREG(details.st_mode)
                and details.st_uid == os.geteuid() and details.st_nlink == 1
                and stat.S_IMODE(details.st_mode) == 0o600):
            os.unlink(partial)
            dfd = os.open(root, os.O_RDONLY | getattr(os, "O_DIRECTORY", 0))
            try: os.fsync(dfd)
            finally: os.close(dfd)
        continue
    final = root / f"{partial_value['observation_generation']}.json"
    if final.exists() or final.is_symlink():
        final_value, final_raw = validate_generation(final)
        if final_value != partial_value or final_raw != partial_raw:
            raise SystemExit("live-observation generation interrupted publication differs")
    else:
        os.chmod(partial, 0o400, follow_symlinks=False)
        os.link(partial, final, follow_symlinks=False)
        dfd = os.open(root, os.O_RDONLY | getattr(os, "O_DIRECTORY", 0))
        try: os.fsync(dfd)
        finally: os.close(dfd)
    os.unlink(partial)
    dfd = os.open(root, os.O_RDONLY | getattr(os, "O_DIRECTORY", 0))
    try: os.fsync(dfd)
    finally: os.close(dfd)

drive_raw = locked(drive_path, "Drive execute receipt")
drive = json.loads(drive_raw)
if (drive_raw != canonical(drive) or drive.get("schema") != "arc.recovery.drive-prefreeze.v1"
        or drive.get("mode") != "execute" or drive.get("freeze_plan_sha256") != freeze_sha
        or drive.get("capture_id") != capture_id or drive.get("canary_verified") is not True
        or drive.get("canary_deleted") is not True):
    raise SystemExit("live-observation generation Drive execute receipt differs")
drive_sha = digest(drive_raw)

if selected.exists() or selected.is_symlink():
    selected_raw = locked(selected, "selected generation", {0o400}, 16 * 1024 * 1024)
    selected_value = json.loads(selected_raw)
    if (selected_raw != canonical(selected_value)
            or selected_value.get("schema") != "arc.recovery.legacy-live-observation-selection.v1"
            or (selected_value.get("source_main_commit"), selected_value.get("freeze_plan_sha256"),
                selected_value.get("capture_id")) != (source_commit, freeze_sha, capture_id)):
        raise SystemExit("selected live-observation generation differs")
    generation = selected_value.get("observation_generation")
    generation_path = root / f"{generation}.json"
    generation_value, generation_raw = validate_generation(generation_path)
    if (selected_value.get("observation_generation_receipt") != generation_value
            or selected_value.get("observation_generation_receipt_sha256") != digest(generation_raw)
            or selected_value.get("drive_prefreeze_receipt_sha256")
                != generation_value["drive_prefreeze_receipt"]["sha256"]):
        raise SystemExit("selected live-observation generation root differs")
    selected_at = datetime.datetime.strptime(
        selected_value["selected_at"], utc_format
    ).replace(tzinfo=datetime.timezone.utc)
    now = datetime.datetime.now(datetime.timezone.utc)
    if resume_state == "unbound" and (
        generation_value["drive_prefreeze_receipt"]["sha256"] != drive_sha
        or selected_at > now
        or (now - selected_at).total_seconds() > maximum_age
    ):
        raise SystemExit("unbound selected live-observation generation is stale or uses another Drive canary")
    print(generation, generation_path, digest(generation_raw),
          generation_value["drive_prefreeze_receipt"]["sha256"])
    raise SystemExit(0)

now = datetime.datetime.now(datetime.timezone.utc)
# No unselected generation crosses a capture invocation boundary.  A crash
# before selection is powerless and the next invocation gets a new nonce even
# when the capacity receipt bytes happen to be identical.
generation = secrets.token_hex(32)
created_at = now.strftime(utc_format)
value = {
    "schema": "arc.recovery.legacy-live-observation-generation.v1",
    "source_main_commit": source_commit,
    "freeze_plan_sha256": freeze_sha,
    "capture_id": capture_id,
    "observation_generation": generation,
    "created_at": created_at,
    "max_selection_age_seconds": maximum_age,
    "drive_prefreeze_receipt": {"path": str(drive_path), "sha256": drive_sha, "value": drive},
}
raw = canonical(value)
path = root / f"{generation}.json"
partial = root / f".{generation}.json.partial"
fd = os.open(partial, os.O_WRONLY | os.O_CREAT | os.O_EXCL | getattr(os, "O_NOFOLLOW", 0), 0o600)
with os.fdopen(fd, "wb") as handle:
    handle.write(raw); handle.flush(); os.fsync(handle.fileno()); os.fchmod(handle.fileno(), 0o400)
os.link(partial, path, follow_symlinks=False)
dfd = os.open(root, os.O_RDONLY | getattr(os, "O_DIRECTORY", 0))
try:
    os.fsync(dfd)
    os.unlink(partial)
    os.fsync(dfd)
finally: os.close(dfd)
print(generation, path, digest(raw), drive_sha)
PY
}

reconcile_local_create_only_resume_links() {
    local selection="$1" maintenance_root="$2"
    # The capture-wide lock is held by the caller.  Every local create-only
    # publisher links `name.partial` to `name` before unlinking the partial.
    # Heal the complete protected maintenance-input tree before any resume
    # reader or dynamic re-probe opens it.  Otherwise a sealed final-absent
    # partial in a sibling evidence directory could conflict with newly
    # sampled timestamp/counter bytes and wedge an already-quarantined capture.
    python3 - "$selection" "$maintenance_root" <<'PY'
import os,pathlib,stat,sys
selection=pathlib.Path(sys.argv[1]);maintenance_root=pathlib.Path(sys.argv[2])
candidates=[selection.with_name(selection.name+".partial")]
if maintenance_root.exists() or maintenance_root.is_symlink():
    root_details=maintenance_root.lstat()
    if (maintenance_root.is_symlink() or not stat.S_ISDIR(root_details.st_mode)
            or root_details.st_uid!=os.geteuid()
            or stat.S_IMODE(root_details.st_mode)!=0o700):
        raise SystemExit("create-only resume maintenance root is unsafe")
    for directory,names,files in os.walk(maintenance_root,followlinks=False):
        directory_path=pathlib.Path(directory);details=directory_path.lstat()
        if (directory_path.is_symlink() or not stat.S_ISDIR(details.st_mode)
                or details.st_uid!=os.geteuid()
                or stat.S_IMODE(details.st_mode)!=0o700):
            raise SystemExit("create-only resume directory is unsafe")
        for name in names:
            child=directory_path/name
            if child.is_symlink():
                raise SystemExit("create-only resume directory symlink is unsafe")
        candidates.extend(directory_path/name for name in files if name.endswith(".partial"))
for partial in candidates:
    if not (partial.exists() or partial.is_symlink()):continue
    terminal=partial.with_name(partial.name[:-len(".partial")])
    partial_details=partial.lstat()
    if not (terminal.exists() or terminal.is_symlink()):
        if (partial.is_symlink() or not stat.S_ISREG(partial_details.st_mode)
                or partial_details.st_uid!=os.geteuid() or partial_details.st_nlink!=1
                or stat.S_IMODE(partial_details.st_mode) not in {0o400,0o600}):
            raise SystemExit("create-only unlinked partial identity differs")
        mode=stat.S_IMODE(partial_details.st_mode)
        if terminal.suffix!=".json" or not 0<=partial_details.st_size<=32*1024*1024:
            raise SystemExit("create-only sealed partial destination differs")
        fd=os.open(partial,os.O_RDONLY|getattr(os,"O_NOFOLLOW",0))
        try:
            raw=os.read(fd,32*1024*1024+1)
            if len(raw)!=partial_details.st_size:
                raise SystemExit("create-only sealed partial changed")
            try:value=__import__("json").loads(raw)
            except (UnicodeDecodeError,__import__("json").JSONDecodeError):value=None
            canonical=(__import__("json").dumps(
                value,sort_keys=True,separators=(",",":"))+"\n").encode() \
                if isinstance(value,dict) else None
        finally:os.close(fd)
        if not isinstance(value,dict) or raw!=canonical:
            if mode!=0o600:
                raise SystemExit("create-only sealed partial is noncanonical")
            os.unlink(partial)
            descriptor=os.open(partial.parent,os.O_RDONLY|getattr(os,"O_DIRECTORY",0))
            try:os.fsync(descriptor)
            finally:os.close(descriptor)
            continue
        if mode==0o600:
            os.chmod(partial,0o400,follow_symlinks=False)
            fd=os.open(partial,os.O_RDONLY|getattr(os,"O_NOFOLLOW",0))
            try:os.fsync(fd)
            finally:os.close(fd)
        try:os.link(partial,terminal,follow_symlinks=False)
        except FileExistsError:
            raise SystemExit("create-only sealed partial raced with resume")
        descriptor=os.open(partial.parent,os.O_RDONLY|getattr(os,"O_DIRECTORY",0))
        try:
            os.fsync(descriptor);os.unlink(partial);os.fsync(descriptor)
        finally:os.close(descriptor)
        continue
    terminal_details=terminal.lstat()
    same=(partial_details.st_dev,partial_details.st_ino)==(
        terminal_details.st_dev,terminal_details.st_ino)
    if not same:
        # A separate nlink=1 partial is producer-owned recovery state and does
        # not make the already-terminal artifact unsafe for a resume reader.
        if terminal_details.st_nlink!=1:
            raise SystemExit("create-only terminal has an unexplained link")
        continue
    if (partial.is_symlink() or terminal.is_symlink()
            or not stat.S_ISREG(partial_details.st_mode)
            or not stat.S_ISREG(terminal_details.st_mode)
            or partial_details.st_uid!=os.geteuid()
            or terminal_details.st_uid!=os.geteuid()
            or stat.S_IMODE(partial_details.st_mode)!=0o400
            or stat.S_IMODE(terminal_details.st_mode)!=0o400
            or partial_details.st_nlink!=2 or terminal_details.st_nlink!=2):
        raise SystemExit("create-only linked publication identity differs")
    os.unlink(partial)
    descriptor=os.open(partial.parent,os.O_RDONLY|getattr(os,"O_DIRECTORY",0))
    try:os.fsync(descriptor)
    finally:os.close(descriptor)
PY
}

live_observation_selection_resume_state() {
    local selection="$1" round_root="$2" current_drive_sha="$3"
    local freeze_sha="$4" capture_id="$5"
    python3 - "$selection" "$round_root" "$current_drive_sha" "$freeze_sha" "$capture_id" <<'PY'
import datetime,hashlib,json,os,pathlib,re,stat,sys
selection=pathlib.Path(sys.argv[1]);round_root=pathlib.Path(sys.argv[2])
drive_sha,freeze,capture=sys.argv[3:]
canonical=lambda value:(json.dumps(value,sort_keys=True,separators=(",",":"))+"\n").encode()
digest=lambda raw:hashlib.sha256(raw).hexdigest()
hash_re=re.compile(r"[0-9a-f]{64}");utc_format="%Y-%m-%dT%H:%M:%S.%fZ"
if any(hash_re.fullmatch(value) is None for value in (drive_sha,freeze,capture)):
    raise SystemExit("live-observation resume identity is malformed")
def locked(path,label,maximum=32*1024*1024,links={1},modes={0o400}):
    fd=os.open(path,os.O_RDONLY|getattr(os,"O_NOFOLLOW",0))
    try:
        details=os.fstat(fd)
        if (not stat.S_ISREG(details.st_mode) or path.is_symlink()
                or details.st_uid!=os.geteuid() or details.st_nlink not in links
                or stat.S_IMODE(details.st_mode) not in modes
                or not 0<details.st_size<=maximum):
            raise SystemExit(f"live-observation resume {label} is unsafe")
        raw=os.read(fd,maximum+1)
        if len(raw)!=details.st_size:raise SystemExit(f"live-observation resume {label} changed")
        value=json.loads(raw)
        if raw!=canonical(value):raise SystemExit(f"live-observation resume {label} is noncanonical")
        return value,raw
    finally:os.close(fd)
def fsync_parent():
    descriptor=os.open(selection.parent,os.O_RDONLY|getattr(os,"O_DIRECTORY",0))
    try:os.fsync(descriptor)
    finally:os.close(descriptor)
partial=selection.with_name(selection.name+".partial")
selection_present=selection.exists() or selection.is_symlink()
partial_present=partial.exists() or partial.is_symlink()
if selection_present and partial_present:
    final_details=selection.lstat();partial_details=partial.lstat()
    if (selection.is_symlink() or partial.is_symlink()
            or not stat.S_ISREG(final_details.st_mode)
            or not stat.S_ISREG(partial_details.st_mode)
            or final_details.st_uid!=os.geteuid() or partial_details.st_uid!=os.geteuid()
            or stat.S_IMODE(final_details.st_mode)!=0o400
            or stat.S_IMODE(partial_details.st_mode)!=0o400
            or (final_details.st_dev,final_details.st_ino)
                !=(partial_details.st_dev,partial_details.st_ino)
            or final_details.st_nlink!=2 or partial_details.st_nlink!=2):
        raise SystemExit("live-observation resume linked selection publication differs")
    _final_value,final_raw=locked(selection,"linked selection",links={2})
    _partial_value,partial_raw=locked(partial,"linked selection partial",links={2})
    if final_raw!=partial_raw:
        raise SystemExit("live-observation resume linked selection bytes differ")
    os.unlink(partial);fsync_parent()
elif not selection_present and partial_present:
    details=partial.lstat()
    if (partial.is_symlink() or not stat.S_ISREG(details.st_mode)
            or details.st_uid!=os.geteuid() or details.st_nlink!=1
            or stat.S_IMODE(details.st_mode) not in {0o400,0o600}):
        raise SystemExit("live-observation resume selection partial is unsafe")
    try:
        _partial_value,partial_raw=locked(
            partial,"selection partial",links={1},modes={0o400,0o600})
    except (ValueError,json.JSONDecodeError,UnicodeDecodeError):
        if stat.S_IMODE(details.st_mode)!=0o600:
            raise SystemExit("live-observation resume sealed selection partial is malformed")
        os.unlink(partial);fsync_parent()
    else:
        os.chmod(partial,0o400,follow_symlinks=False)
        try:os.link(partial,selection,follow_symlinks=False)
        except FileExistsError:
            _racing_value,racing_raw=locked(selection,"racing selection",links={1,2})
            if racing_raw!=partial_raw:
                raise SystemExit("live-observation resume racing selection differs")
        fsync_parent()
        os.unlink(partial);fsync_parent()
if not (selection.exists() or selection.is_symlink()):
    print("absent -");raise SystemExit(0)
def zero_progress_released(attempt,authorization_raw,readiness_raw,dispatch_raw):
    path=attempt/"zero-progress-release.json"
    if not (path.exists() or path.is_symlink()):return False
    release,_release_raw=locked(path,"zero-progress release")
    fields={"schema","capture_id","freeze_plan_sha256","round_number",
        "round_authorization_sha256","round_readiness_sha256",
        "mutation_dispatch_sha256","live_observation_selection_sha256",
        "live_observation_generation","observation_generation_receipt_sha256",
        "drive_prefreeze_receipt_sha256","challenge","released_at","nodes"}
    nodes=release.get("nodes");challenge=release.get("challenge")
    authorization=json.loads(authorization_raw)
    targets=authorization.get("targets")
    try:datetime.datetime.strptime(release.get("released_at"),"%Y-%m-%dT%H:%M:%SZ")
    except (TypeError,ValueError):
        raise SystemExit("live-observation zero-progress release time differs")
    if (set(release)!=fields
            or release.get("schema")!="arc.recovery.quarantine-round-zero-progress-release.v1"
            or release.get("round_number")!=1
            or (release.get("capture_id"),release.get("freeze_plan_sha256"),
                release.get("round_authorization_sha256"),release.get("round_readiness_sha256"),
                release.get("mutation_dispatch_sha256"),
                release.get("live_observation_selection_sha256"),
                release.get("live_observation_generation"),
                release.get("observation_generation_receipt_sha256"),
                release.get("drive_prefreeze_receipt_sha256"))
                !=(capture,freeze,digest(authorization_raw),digest(readiness_raw),
                   digest(dispatch_raw),selection_sha,generation,generation_sha,
                   selection_drive_sha)
            or hash_re.fullmatch(str(challenge)) is None or not isinstance(nodes,list)
            or not isinstance(targets,list)
            or [row.get("node") for row in targets]
                !=["nyc","lax","ams","lhr","nrt","sgp"]
            or [row.get("value",{}).get("node") for row in nodes]
                !=["nyc","lax","ams","lhr","nrt","sgp"]):
        raise SystemExit("live-observation zero-progress release differs")
    target_by_node={row["node"]:row for row in targets}
    proof_fields={"schema","capture_id","freeze_plan_sha256","observation_generation",
        "round_number","round_authorization_sha256","round_readiness_sha256",
        "mutation_dispatch_sha256","challenge","node","boot_id","writer_live_unfenced",
        "apply_state_present","restart_effective_mutation_absent","active_selector_absent",
        "quarantine_nft_absent","authorization_accepted","readiness_present",
        "accepted_boottime_ns","elapsed_since_acceptance_ns","observed_boottime_ns","observed_at"}
    for wrapper in nodes:
        proof=wrapper.get("value") if isinstance(wrapper,dict) else None
        if isinstance(proof,dict):
            accepted_ns=proof.get("accepted_boottime_ns")
            observed_ns=proof.get("observed_boottime_ns")
            elapsed_ns=proof.get("elapsed_since_acceptance_ns")
            try:datetime.datetime.strptime(proof.get("observed_at"),"%Y-%m-%dT%H:%M:%SZ")
            except (TypeError,ValueError):observed_at_valid=False
            else:observed_at_valid=True
        else:
            accepted_ns=observed_ns=elapsed_ns=None;observed_at_valid=False
        if (not isinstance(wrapper,dict) or set(wrapper)!={"value","sha256"}
                or not isinstance(wrapper.get("value"),dict)
                or set(proof)!=proof_fields
                or digest(canonical(proof))!=wrapper.get("sha256")
                or proof.get("schema")!="arc.recovery.quarantine-round-zero-progress-node-proof.v1"
                or (proof.get("capture_id"),proof.get("freeze_plan_sha256"),
                    proof.get("observation_generation"),proof.get("round_number"),
                    proof.get("round_authorization_sha256"),proof.get("round_readiness_sha256"),
                    proof.get("mutation_dispatch_sha256"),proof.get("challenge"))
                    !=(capture,freeze,generation,1,digest(authorization_raw),
                       digest(readiness_raw),digest(dispatch_raw),challenge)
                or proof.get("boot_id")!=target_by_node.get(proof.get("node"),{}).get("boot_id")
                or proof.get("writer_live_unfenced") is not True
                or proof.get("restart_effective_mutation_absent") is not True
                or proof.get("active_selector_absent") is not True
                or proof.get("quarantine_nft_absent") is not True
                or proof.get("authorization_accepted") is not True
                or not isinstance(proof.get("apply_state_present"),bool)
                or not isinstance(proof.get("readiness_present"),bool)
                or any(isinstance(number,bool) or not isinstance(number,int) or number<=0
                       for number in (accepted_ns,elapsed_ns,observed_ns))
                or observed_ns<=accepted_ns+300_000_000_000
                or elapsed_ns!=observed_ns-accepted_ns or not observed_at_valid):
            raise SystemExit("live-observation zero-progress node proof differs")
    return True
value,raw=locked(selection,"selection")
fields={"schema","source_main_commit","freeze_plan_sha256","capture_id",
    "observation_generation","observation_generation_receipt",
    "observation_generation_receipt_path","observation_generation_receipt_sha256",
    "drive_prefreeze_receipt_path","drive_prefreeze_receipt_sha256",
    "generation_created_at","selected_at","max_selection_age_seconds","labels","nodes"}
generation=value.get("observation_generation");selection_sha=digest(raw)
generation_sha=value.get("observation_generation_receipt_sha256")
selection_drive_sha=value.get("drive_prefreeze_receipt_sha256")
if (set(value)!=fields or value.get("schema")!="arc.recovery.legacy-live-observation-selection.v1"
        or (value.get("freeze_plan_sha256"),value.get("capture_id"))!=(freeze,capture)
        or hash_re.fullmatch(str(generation)) is None
        or value.get("max_selection_age_seconds")!=300):
    raise SystemExit("live-observation resume selection differs")
bound=False
if round_root.exists() or round_root.is_symlink():
    details=round_root.lstat()
    if (round_root.is_symlink() or not stat.S_ISDIR(details.st_mode)
            or details.st_uid!=os.geteuid() or stat.S_IMODE(details.st_mode)!=0o700):
        raise SystemExit("live-observation resume quarantine-round root is unsafe")
    dispatches=sorted(round_root.glob("round-*/attempt.*/mutation-dispatch.json"))
    readinesses=sorted(round_root.glob("round-*/attempt.*/readiness.json"))
    for path in readinesses:
        authorization,authorization_raw=locked(
            path.with_name("authorization.json"),"readiness authorization")
        if ((authorization.get("capture_id"),authorization.get("freeze_plan_sha256"),
             authorization.get("live_observation_selection_sha256"),
             authorization.get("live_observation_generation"),
             authorization.get("observation_generation_receipt_sha256"),
             authorization.get("drive_prefreeze_receipt_sha256"))
                !=(capture,freeze,selection_sha,generation,generation_sha,
                   selection_drive_sha)):
            raise SystemExit("live-observation resume readiness authorization differs")
        _readiness,readiness_raw=locked(path,"quarantine readiness")
        dispatch_path=path.with_name("mutation-dispatch.json")
        if not (dispatch_path.exists() or dispatch_path.is_symlink()):
            # Local readiness is built before dispatch publication and before
            # any remote readiness send; alone it is a powerless crash prefix.
            continue
        _dispatch,dispatch_raw=locked(dispatch_path,"mutation dispatch")
        if zero_progress_released(path.parent,authorization_raw,readiness_raw,dispatch_raw):
            continue
        bound=True
    for path in dispatches:
        dispatch,dispatch_raw=locked(path,"mutation dispatch")
        if (dispatch.get("schema")!="arc.recovery.quarantine-mutation-dispatch.v1"
                or (dispatch.get("capture_id"),dispatch.get("freeze_plan_sha256"),
                    dispatch.get("live_observation_selection_sha256"),
                    dispatch.get("live_observation_generation"),
                    dispatch.get("observation_generation_receipt_sha256"),
                    dispatch.get("drive_prefreeze_receipt_sha256"))
                    !=(capture,freeze,selection_sha,generation,generation_sha,
                       selection_drive_sha)):
            raise SystemExit("live-observation resume mutation dispatch differs")
        authorization,authorization_raw=locked(path.with_name("authorization.json"),
                                                "dispatch authorization")
        _readiness,readiness_raw=locked(path.with_name("readiness.json"),
                                        "dispatch readiness")
        if zero_progress_released(path.parent,authorization_raw,readiness_raw,dispatch_raw):
            continue
        bound=True
    for result_path in sorted(round_root.glob("round-*/result.json")):
        result,_result_raw=locked(result_path,"quarantine result")
        if result.get("transitions"):
            authorization,_authorization_raw=locked(
                result_path.with_name("authorization.json"),"quarantine authorization")
            if ((authorization.get("live_observation_selection_sha256"),
                 authorization.get("live_observation_generation"),
                 authorization.get("observation_generation_receipt_sha256"),
                 authorization.get("drive_prefreeze_receipt_sha256"))
                    !=(selection_sha,generation,generation_sha,selection_drive_sha)):
                raise SystemExit("live-observation resume quarantine result differs")
            bound=True
if bound:
    print("bound",generation);raise SystemExit(0)
# Every unbound selection is invocation-local.  Even a wall-fresh selection is
# rotated after the exact six-writer live/unfenced proof on a new invocation;
# only a dispatch-bound selection may resume across an operator crash.
print("rotate",generation)
PY
}

release_stale_zero_progress_dispatches() {
    local freeze_plan="$1" selection="$2" round_root="$3" current_drive_sha="$4"
    local freeze_sha="$5" capture_id="$6" log_root="$7"
    [ -f "$selection" ] && [ ! -L "$selection" ] || return 0
    [ -d "$round_root" ] && [ ! -L "$round_root" ] || return 0
    local attempt authorization readiness dispatch auth_sha readiness_sha dispatch_sha
    local round generation challenge proof_root node temporary release_temporary proof_failed
    while IFS= read -r attempt; do
        [ -n "$attempt" ] || continue
        authorization="$attempt/authorization.json"
        readiness="$attempt/readiness.json"
        dispatch="$attempt/mutation-dispatch.json"
        auth_sha="$(round_artifact_sha "$authorization")"
        readiness_sha="$(round_artifact_sha "$readiness")"
        dispatch_sha="$(round_artifact_sha "$dispatch")"
        read -r round generation < <(python3 - "$authorization" "$selection" <<'PY'
import json,pathlib,sys
authorization=json.loads(pathlib.Path(sys.argv[1]).read_text())
selection=json.loads(pathlib.Path(sys.argv[2]).read_text())
print(authorization["round_number"],selection["observation_generation"])
PY
        )
        challenge="$(python3 - <<'PY'
import secrets
print(secrets.token_hex(32))
PY
        )"
        proof_root="$(prepare_protected_maintenance_directory \
            "$attempt/zero-progress-proofs-$challenge")"
        proof_failed=0
        for node in nyc lax ams lhr nrt sgp; do
            temporary="$log_root/$node-zero-progress-$challenge.new.json"
            if ! run_remote "$node" quarantine-round-zero-progress-proof \
                "$capture_id" "$generation" "$node" "$freeze_sha" "$round" \
                "$auth_sha" "$readiness_sha" "$dispatch_sha" "$challenge" \
                "$(freeze_node_field "$freeze_plan" "$node" writer_pid)" \
                "$(freeze_node_field "$freeze_plan" "$node" writer_start_ticks)" \
                "$(freeze_node_field "$freeze_plan" "$node" boot_id)" > "$temporary"; then
                proof_failed=1
                break
            fi
            chmod 400 "$temporary"
            publish_canonical_maintenance_input "$temporary" "$proof_root/$node.json"
        done
        if [ "$proof_failed" -ne 0 ]; then
            printf 'archive fleet: quarantine dispatch is not yet eligible for exact zero-progress release; resuming its node BOOTTIME lease\n'
            continue
        fi
        release_temporary="$log_root/zero-progress-release-$challenge.new.json"
        python3 - "$authorization" "$readiness" "$dispatch" "$selection" \
            "$proof_root" "$challenge" "$release_temporary" <<'PY'
import datetime,hashlib,json,os,pathlib,stat,sys
authorization_path,readiness_path,dispatch_path,selection_path,proof_root,challenge,output=sys.argv[1:]
authorization_path=pathlib.Path(authorization_path);readiness_path=pathlib.Path(readiness_path)
dispatch_path=pathlib.Path(dispatch_path);selection_path=pathlib.Path(selection_path)
proof_root=pathlib.Path(proof_root);output=pathlib.Path(output)
canonical=lambda value:(json.dumps(value,sort_keys=True,separators=(",",":"))+"\n").encode()
digest=lambda raw:hashlib.sha256(raw).hexdigest()
def locked(path,label):
    fd=os.open(path,os.O_RDONLY|getattr(os,"O_NOFOLLOW",0))
    try:
        details=os.fstat(fd)
        if (not stat.S_ISREG(details.st_mode) or path.is_symlink()
                or details.st_uid!=os.geteuid() or details.st_nlink!=1
                or stat.S_IMODE(details.st_mode)!=0o400
                or not 0<details.st_size<=32*1024*1024):
            raise SystemExit(f"zero-progress release {label} is unsafe")
        raw=os.read(fd,32*1024*1024+1);value=json.loads(raw)
        if len(raw)!=details.st_size or raw!=canonical(value):
            raise SystemExit(f"zero-progress release {label} differs")
        return value,raw
    finally:os.close(fd)
authorization,authorization_raw=locked(authorization_path,"authorization")
readiness,readiness_raw=locked(readiness_path,"readiness")
dispatch,dispatch_raw=locked(dispatch_path,"dispatch")
selection,selection_raw=locked(selection_path,"selection")
identity=(authorization["capture_id"],authorization["freeze_plan_sha256"],
          authorization["round_number"],digest(authorization_raw),digest(readiness_raw),
          digest(dispatch_raw),digest(selection_raw),selection["observation_generation"],
          selection["observation_generation_receipt_sha256"],
          selection["drive_prefreeze_receipt_sha256"])
if identity[2]!=1:
    raise SystemExit("zero-progress release is only valid for the first all-live round")
if ((readiness.get("round_authorization_sha256"),dispatch.get("round_authorization_sha256"),
     dispatch.get("round_readiness_sha256"))!=(identity[3],identity[3],identity[4])
        or (authorization.get("live_observation_selection_sha256"),
            authorization.get("live_observation_generation"),
            authorization.get("observation_generation_receipt_sha256"),
            authorization.get("drive_prefreeze_receipt_sha256"))!=identity[6:]):
    raise SystemExit("zero-progress release attempt/selection binding differs")
nodes=[]
targets=authorization.get("targets")
if (not isinstance(targets,list) or [row.get("node") for row in targets]
        !=["nyc","lax","ams","lhr","nrt","sgp"]):
    raise SystemExit("zero-progress release authorization topology differs")
target_by_node={row["node"]:row for row in targets}
proof_fields={"schema","capture_id","freeze_plan_sha256","observation_generation",
 "round_number","round_authorization_sha256","round_readiness_sha256",
 "mutation_dispatch_sha256","challenge","node","boot_id","writer_live_unfenced",
 "apply_state_present","restart_effective_mutation_absent","active_selector_absent",
 "quarantine_nft_absent","authorization_accepted","readiness_present",
 "accepted_boottime_ns","elapsed_since_acceptance_ns","observed_boottime_ns","observed_at"}
for node in ("nyc","lax","ams","lhr","nrt","sgp"):
    value,raw=locked(proof_root/f"{node}.json",f"{node} proof")
    accepted_ns=value.get("accepted_boottime_ns");observed_ns=value.get("observed_boottime_ns")
    elapsed_ns=value.get("elapsed_since_acceptance_ns")
    try:
        observed_at=datetime.datetime.strptime(value.get("observed_at"),"%Y-%m-%dT%H:%M:%SZ")
    except (TypeError,ValueError):
        raise SystemExit(f"zero-progress release node proof time differs: {node}")
    if (set(value)!=proof_fields
            or value.get("schema")!="arc.recovery.quarantine-round-zero-progress-node-proof.v1"
            or (value.get("capture_id"),value.get("freeze_plan_sha256"),
                value.get("observation_generation"),value.get("round_number"),
                value.get("round_authorization_sha256"),value.get("round_readiness_sha256"),
                value.get("mutation_dispatch_sha256"),value.get("challenge"),value.get("node"))
                !=(identity[0],identity[1],identity[7],identity[2],identity[3],identity[4],
                   identity[5],challenge,node)
            or value.get("boot_id")!=target_by_node[node].get("boot_id")
            or any(value.get(field) is not True for field in
                   ("writer_live_unfenced","restart_effective_mutation_absent","active_selector_absent",
                    "quarantine_nft_absent","authorization_accepted"))
            or not isinstance(value.get("apply_state_present"),bool)
            or not isinstance(value.get("readiness_present"),bool)
            or any(isinstance(number,bool) or not isinstance(number,int) or number<=0
                   for number in (accepted_ns,elapsed_ns,observed_ns))
            or observed_ns<=accepted_ns+300_000_000_000
            or elapsed_ns!=observed_ns-accepted_ns):
        raise SystemExit(f"zero-progress release node proof differs: {node}")
    nodes.append({"value":value,"sha256":digest(raw)})
release={"schema":"arc.recovery.quarantine-round-zero-progress-release.v1",
 "capture_id":identity[0],"freeze_plan_sha256":identity[1],"round_number":identity[2],
 "round_authorization_sha256":identity[3],"round_readiness_sha256":identity[4],
 "mutation_dispatch_sha256":identity[5],"live_observation_selection_sha256":identity[6],
 "live_observation_generation":identity[7],"observation_generation_receipt_sha256":identity[8],
 "drive_prefreeze_receipt_sha256":identity[9],"challenge":challenge,
 "released_at":datetime.datetime.now(datetime.timezone.utc).strftime("%Y-%m-%dT%H:%M:%SZ"),
 "nodes":nodes}
raw=canonical(release)
fd=os.open(output,os.O_WRONLY|os.O_CREAT|os.O_EXCL|getattr(os,"O_NOFOLLOW",0),0o600)
with os.fdopen(fd,"wb") as handle:
    handle.write(raw);handle.flush();os.fsync(handle.fileno());os.fchmod(handle.fileno(),0o400)
PY
        publish_canonical_maintenance_input "$release_temporary" \
            "$attempt/zero-progress-release.json"
        printf 'archive fleet: released expired zero-progress quarantine dispatch after six challenged live/unfenced proofs\n'
    done < <(python3 - "$selection" "$round_root" "$current_drive_sha" <<'PY'
import hashlib,json,pathlib,sys
selection_path=pathlib.Path(sys.argv[1]);root=pathlib.Path(sys.argv[2]);_drive=sys.argv[3]
selection=json.loads(selection_path.read_text())
selection_sha=hashlib.sha256((json.dumps(
    selection,sort_keys=True,separators=(",",":"))+"\n").encode()).hexdigest()
for readiness in sorted(root.glob("round-*/attempt.*/readiness.json")):
    attempt=readiness.parent;dispatch=attempt/"mutation-dispatch.json"
    if not dispatch.is_file() or dispatch.is_symlink() or (attempt/"zero-progress-release.json").exists():continue
    authorization=json.loads((attempt/"authorization.json").read_text())
    if ((authorization.get("live_observation_selection_sha256"),
         authorization.get("live_observation_generation"),
         authorization.get("observation_generation_receipt_sha256"),
         authorization.get("drive_prefreeze_receipt_sha256"))
            !=(selection_sha,selection.get("observation_generation"),
               selection.get("observation_generation_receipt_sha256"),
               selection.get("drive_prefreeze_receipt_sha256"))):continue
    if (authorization.get("round_number")!=1 or authorization.get("prior_fenced")
            or authorization.get("prior_round_result_sha256s")):continue
    result=attempt/"result.json"
    if result.exists():
        value=json.loads(result.read_text())
        if value.get("transitions"):continue
    transitions=attempt/"node-transitions"
    if transitions.exists() and any(path.is_file() for path in transitions.iterdir()):continue
    print(attempt)
PY
    )
}

capture_remaining_target_inert_proofs() {
    local freeze_plan="$1" attempt_root="$2" targets_csv="$3" log_root="$4"
    local authorization="$attempt_root/authorization.json"
    local readiness="$attempt_root/readiness.json"
    local dispatch="$attempt_root/mutation-dispatch.json"
    local auth_sha readiness_sha dispatch_sha round generation capture freeze challenge proof_root
    local node temporary failed=0
    local names=()
    [ -n "$targets_csv" ] || die "remaining-target inert proof set is empty"
    auth_sha="$(round_artifact_sha "$authorization")"
    readiness_sha="$(round_artifact_sha "$readiness")"
    dispatch_sha="$(round_artifact_sha "$dispatch")"
    read -r round generation capture freeze < <(python3 - "$authorization" <<'PY'
import json,pathlib,sys
value=json.loads(pathlib.Path(sys.argv[1]).read_text(encoding="utf-8"))
print(value["round_number"],value["live_observation_generation"],
      value["capture_id"],value["freeze_plan_sha256"])
PY
    )
    require_uint "$round" "remaining-target inert proof round"
    require_hash "$generation" "remaining-target live-observation generation"
    require_hash "$capture" "remaining-target capture"
    require_hash "$freeze" "remaining-target freeze plan"
    challenge="$(python3 -I - <<'PY'
import secrets
print(secrets.token_hex(32))
PY
    )"
    require_hash "$challenge" "remaining-target inert proof challenge"
    proof_root="$(prepare_protected_maintenance_directory \
        "$attempt_root/remaining-target-inert-proofs-$challenge")"
    IFS=',' read -r -a names <<< "$targets_csv"
    [ "${#names[@]}" -gt 0 ] || die "remaining-target inert proof topology is empty"
    for node in "${names[@]}"; do
        require_node "$node"
        temporary="$log_root/$node-remaining-inert-$challenge.new.json"
        if ! run_remote "$node" quarantine-round-zero-progress-proof \
                "$capture" "$generation" "$node" "$freeze" \
                "$round" "$auth_sha" "$readiness_sha" "$dispatch_sha" "$challenge" \
                "$(freeze_node_field "$freeze_plan" "$node" writer_pid)" \
                "$(freeze_node_field "$freeze_plan" "$node" writer_start_ticks)" \
                "$(freeze_node_field "$freeze_plan" "$node" boot_id)" \
                > "$temporary" 2> "$temporary.stderr"; then
            sed -n '1,80p' "$temporary.stderr" >&2
            failed=1
            break
        fi
        chmod 400 "$temporary"
        publish_canonical_maintenance_input "$temporary" "$proof_root/$node.json"
    done
    [ "$failed" -eq 0 ] || return 1
    printf '%s\n' "$proof_root"
}

archive_stale_live_observation_selection() {
    local selection="$1" archive_root="$2" generation="$3"
    python3 - "$selection" "$archive_root" "$generation" <<'PY'
import os,pathlib,re,stat,sys
selection=pathlib.Path(sys.argv[1]);root=pathlib.Path(sys.argv[2]);generation=sys.argv[3]
if re.fullmatch(r"[0-9a-f]{64}",generation) is None:
    raise SystemExit("stale live-observation generation is malformed")
parent=root.parent;details=parent.lstat()
if (parent.is_symlink() or not stat.S_ISDIR(details.st_mode)
        or details.st_uid!=os.geteuid() or details.st_mode&0o022):
    raise SystemExit("live-observation selection archive parent is unsafe")
if root.exists() or root.is_symlink():
    details=root.lstat()
    if (root.is_symlink() or not stat.S_ISDIR(details.st_mode)
            or details.st_uid!=os.geteuid() or stat.S_IMODE(details.st_mode)!=0o700):
        raise SystemExit("live-observation selection archive is unsafe")
else:
    os.mkdir(root,0o700)
    dfd=os.open(parent,os.O_RDONLY|getattr(os,"O_DIRECTORY",0))
    try:os.fsync(dfd)
    finally:os.close(dfd)
def read_regular(path,label,modes={0o400},links={1}):
    fd=os.open(path,os.O_RDONLY|getattr(os,"O_NOFOLLOW",0))
    try:
        details=os.fstat(fd)
        if (not stat.S_ISREG(details.st_mode) or path.is_symlink()
                or details.st_uid!=os.geteuid() or details.st_nlink not in links
                or stat.S_IMODE(details.st_mode) not in modes
                or not 0<details.st_size<=16*1024*1024):
            raise SystemExit(f"{label} is unsafe")
        source=os.read(fd,16*1024*1024+1)
        if len(source)!=details.st_size:raise SystemExit(f"{label} changed while read")
        return source
    finally:os.close(fd)
target=root/f"{generation}.json"
partial=root/f".{generation}.json.partial"
source=read_regular(selection,"stale live-observation selection")
if (target.exists() or target.is_symlink()) and (partial.exists() or partial.is_symlink()):
    target_details=target.lstat();partial_details=partial.lstat()
    if (target.is_symlink() or partial.is_symlink()
            or (target_details.st_dev,target_details.st_ino)
                !=(partial_details.st_dev,partial_details.st_ino)
            or target_details.st_nlink!=2 or partial_details.st_nlink!=2
            or read_regular(target,"linked archived selection",links={2})!=source
            or read_regular(partial,"linked archived selection partial",
                            {0o400,0o600},{2})!=source):
        raise SystemExit("archived live-observation selection interrupted publication differs")
    os.unlink(partial)
    dfd=os.open(root,os.O_RDONLY|getattr(os,"O_DIRECTORY",0))
    try:os.fsync(dfd)
    finally:os.close(dfd)
if target.exists() or target.is_symlink():
    if read_regular(target,"archived live-observation selection")!=source:
        raise SystemExit("archived live-observation selection differs")
else:
    if partial.exists() or partial.is_symlink():
        details=partial.lstat()
        if (partial.is_symlink() or not stat.S_ISREG(details.st_mode)
                or details.st_uid!=os.geteuid() or details.st_nlink!=1
                or stat.S_IMODE(details.st_mode) not in {0o400,0o600}):
            raise SystemExit("archived live-observation selection partial is unsafe")
        if stat.S_IMODE(details.st_mode)==0o600:
            try:partial_source=read_regular(
                partial,"archived live-observation selection partial",{0o600})
            except SystemExit:partial_source=None
            if partial_source!=source:
                os.unlink(partial)
                dfd=os.open(root,os.O_RDONLY|getattr(os,"O_DIRECTORY",0))
                try:os.fsync(dfd)
                finally:os.close(dfd)
        elif read_regular(partial,"archived live-observation selection partial")!=source:
            raise SystemExit("archived live-observation selection partial differs")
    if not (partial.exists() or partial.is_symlink()):
        fd=os.open(partial,os.O_WRONLY|os.O_CREAT|os.O_EXCL|getattr(os,"O_NOFOLLOW",0),0o600)
        with os.fdopen(fd,"wb") as handle:
            handle.write(source);handle.flush();os.fsync(handle.fileno());os.fchmod(handle.fileno(),0o400)
    else:
        os.chmod(partial,0o400,follow_symlinks=False)
    os.link(partial,target,follow_symlinks=False)
    dfd=os.open(root,os.O_RDONLY|getattr(os,"O_DIRECTORY",0))
    try:
        os.fsync(dfd)
        os.unlink(partial)
        os.fsync(dfd)
    finally:os.close(dfd)
os.unlink(selection)
dfd=os.open(parent,os.O_RDONLY|getattr(os,"O_DIRECTORY",0))
try:os.fsync(dfd)
finally:os.close(dfd)
details=target.lstat()
if (target.is_symlink() or not stat.S_ISREG(details.st_mode)
        or details.st_uid!=os.geteuid() or details.st_nlink!=1
        or stat.S_IMODE(details.st_mode)!=0o400):
    raise SystemExit("archived live-observation selection identity differs")
PY
}

seal_live_observation_selection() {
    local output="$1" generation_receipt="$2" statuses="$3" freeze_sha="$4" capture_id="$5"
    python3 - "$output" "$generation_receipt" "$statuses" "$freeze_sha" "$capture_id" <<'PY'
import datetime, hashlib, json, os, pathlib, re, stat, sys
output, generation_path, statuses_path = map(pathlib.Path, sys.argv[1:4])
freeze_sha, capture_id = sys.argv[4:]
canonical = lambda value: (json.dumps(value, sort_keys=True, separators=(",", ":")) + "\n").encode()
digest = lambda raw: hashlib.sha256(raw).hexdigest()
hash_re = re.compile(r"[0-9a-f]{64}")
utc_format = "%Y-%m-%dT%H:%M:%S.%fZ"
nodes = ("nyc", "lax", "ams", "lhr", "nrt", "sgp")

def read_locked(path, label, maximum=16 * 1024 * 1024, links={1}):
    fd = os.open(path, os.O_RDONLY | getattr(os, "O_NOFOLLOW", 0))
    try:
        details = os.fstat(fd)
        if (not stat.S_ISREG(details.st_mode) or path.is_symlink()
                or details.st_uid != os.geteuid() or details.st_nlink not in links
                or stat.S_IMODE(details.st_mode) not in {0o400, 0o600}
                or not 0 < details.st_size <= maximum):
            raise SystemExit(f"live-observation selection {label} is unsafe")
        raw = os.read(fd, maximum + 1)
        if len(raw) != details.st_size:
            raise SystemExit(f"live-observation selection {label} changed")
        return raw
    finally: os.close(fd)

generation_raw = read_locked(generation_path, "generation receipt")
generation_value = json.loads(generation_raw)
generation = generation_value.get("observation_generation")
drive = generation_value.get("drive_prefreeze_receipt", {})
if (generation_raw != canonical(generation_value)
        or generation_value.get("schema") != "arc.recovery.legacy-live-observation-generation.v1"
        or (generation_value.get("freeze_plan_sha256"), generation_value.get("capture_id"))
            != (freeze_sha, capture_id)
        or hash_re.fullmatch(str(generation)) is None
        or generation_path.name != f"{generation}.json"
        or not isinstance(drive, dict) or hash_re.fullmatch(str(drive.get("sha256"))) is None):
    raise SystemExit("live-observation selection generation receipt differs")
generation_sha = digest(generation_raw)
statuses_raw = read_locked(statuses_path, "status set")
rows = [json.loads(line) for line in statuses_raw.decode("utf-8").splitlines() if line]
if len(rows) != 6 or [row.get("node") for row in rows] != list(nodes):
    raise SystemExit("live-observation selection status set is not the ordered fleet")
status_fields = {"schema", "capture_id", "observation_generation",
    "observation_generation_receipt_sha256", "drive_prefreeze_receipt_sha256",
    "node", "freeze_plan_sha256", "created_at", "completed_at", "root_sha256",
    "receipt_sha256", "labels"}
created_at = datetime.datetime.strptime(generation_value["created_at"], utc_format).replace(tzinfo=datetime.timezone.utc)
selected_at = None
existing = None
partial = output.with_name(output.name + ".partial")
if (output.exists() or output.is_symlink()) and (partial.exists() or partial.is_symlink()):
    output_details=output.lstat();partial_details=partial.lstat()
    if (output.is_symlink() or partial.is_symlink()
            or (output_details.st_dev,output_details.st_ino)!=(partial_details.st_dev,partial_details.st_ino)
            or output_details.st_nlink!=2 or partial_details.st_nlink!=2
            or read_locked(output,"linked selection",links={2})
                !=read_locked(partial,"linked selection partial",links={2})):
        raise SystemExit("live-observation selection interrupted publication differs")
    os.unlink(partial)
    dfd=os.open(output.parent,os.O_RDONLY|getattr(os,"O_DIRECTORY",0))
    try:os.fsync(dfd)
    finally:os.close(dfd)
if output.exists() or output.is_symlink():
    existing_raw = read_locked(output, "existing selection")
    existing = json.loads(existing_raw)
    selected_at = datetime.datetime.strptime(existing["selected_at"], utc_format).replace(tzinfo=datetime.timezone.utc)
else:
    if partial.exists() or partial.is_symlink():
        details=partial.lstat()
        if (partial.is_symlink() or not stat.S_ISREG(details.st_mode)
                or details.st_uid!=os.geteuid() or details.st_nlink!=1
                or stat.S_IMODE(details.st_mode) not in {0o400,0o600}):
            raise SystemExit("live-observation selection partial identity differs")
        try:
            partial_raw=read_locked(partial,"selection partial")
            partial_value=json.loads(partial_raw)
            if partial_raw!=canonical(partial_value):raise ValueError("noncanonical")
        except (SystemExit,ValueError,json.JSONDecodeError,UnicodeDecodeError):
            if stat.S_IMODE(details.st_mode)!=0o600:
                raise SystemExit("sealed live-observation selection partial is malformed")
            os.unlink(partial)
            dfd=os.open(output.parent,os.O_RDONLY|getattr(os,"O_DIRECTORY",0))
            try:os.fsync(dfd)
            finally:os.close(dfd)
            selected_at=datetime.datetime.now(datetime.timezone.utc)
        else:
            selected_at=datetime.datetime.strptime(
                partial_value["selected_at"],utc_format
            ).replace(tzinfo=datetime.timezone.utc)
    else:
        selected_at = datetime.datetime.now(datetime.timezone.utc)
normalized = []
for node, row in zip(nodes, rows):
    if (set(row) != status_fields or row.get("schema") != "arc.recovery.legacy-live-observations-status.v1"
            or (row.get("capture_id"), row.get("observation_generation"),
                row.get("observation_generation_receipt_sha256"),
                row.get("drive_prefreeze_receipt_sha256"), row.get("node"),
                row.get("freeze_plan_sha256")) != (
                capture_id, generation, generation_sha, drive["sha256"], node, freeze_sha)
            or row.get("labels") != ["diagnostic", "noncanonical", "nonreward"]
            or any(hash_re.fullmatch(str(row.get(key))) is None for key in ("root_sha256", "receipt_sha256"))):
        raise SystemExit(f"live-observation selection status differs for {node}")
    started = datetime.datetime.strptime(row["created_at"], utc_format).replace(tzinfo=datetime.timezone.utc)
    completed = datetime.datetime.strptime(row["completed_at"], utc_format).replace(tzinfo=datetime.timezone.utc)
    # Node UTC is audit-only; exact status roots and the generation nonce bind
    # causality.  Only a node's own start/completion pair must not regress.
    if started > completed:
        raise SystemExit(f"live-observation selection node timeline regressed for {node}")
    normalized.append({key: row[key] for key in (
        "node", "created_at", "completed_at", "root_sha256", "receipt_sha256")})
if (selected_at - created_at).total_seconds() > generation_value["max_selection_age_seconds"]:
    raise SystemExit("live-observation generation exceeded its selection freshness window")
value = {
    "schema": "arc.recovery.legacy-live-observation-selection.v1",
    "source_main_commit": generation_value["source_main_commit"],
    "freeze_plan_sha256": freeze_sha,
    "capture_id": capture_id,
    "observation_generation": generation,
    "observation_generation_receipt": generation_value,
    "observation_generation_receipt_path": str(generation_path),
    "observation_generation_receipt_sha256": generation_sha,
    "drive_prefreeze_receipt_path": drive["path"],
    "drive_prefreeze_receipt_sha256": drive["sha256"],
    "generation_created_at": generation_value["created_at"],
    "selected_at": selected_at.strftime(utc_format),
    "max_selection_age_seconds": generation_value["max_selection_age_seconds"],
    "labels": ["diagnostic", "noncanonical", "nonreward"],
    "nodes": normalized,
}
payload = canonical(value)
if existing is not None:
    if canonical(existing) != payload:
        raise SystemExit("existing live-observation selection differs")
else:
    if partial.exists() or partial.is_symlink():
        partial_raw = read_locked(partial, "selection partial")
        if partial_raw != payload:
            raise SystemExit("live-observation selection partial differs")
        os.chmod(partial, 0o400, follow_symlinks=False)
    else:
        fd = os.open(partial, os.O_WRONLY | os.O_CREAT | os.O_EXCL | getattr(os, "O_NOFOLLOW", 0), 0o600)
        with os.fdopen(fd, "wb") as handle:
            handle.write(payload); handle.flush(); os.fsync(handle.fileno()); os.fchmod(handle.fileno(), 0o400)
    try:
        os.link(partial,output,follow_symlinks=False)
    except FileExistsError:
        if read_locked(output,"racing selection",links={1,2})!=payload:
            raise SystemExit("racing live-observation selection differs")
    dfd = os.open(output.parent, os.O_RDONLY | getattr(os, "O_DIRECTORY", 0))
    try:
        os.fsync(dfd)
        os.unlink(partial)
        os.fsync(dfd)
    finally: os.close(dfd)
    if read_locked(output,"published selection")!=payload:
        raise SystemExit("published live-observation selection differs")
print(digest(payload))
PY
}

verify_live_observation_generation_receipt_exact() {
    local path="$1" observation_generation="$2" expected_sha="$3"
    local drive_receipt_sha="$4" freeze_sha="$5" capture_id="$6"
    python3 - "$path" "$observation_generation" "$expected_sha" \
        "$drive_receipt_sha" "$freeze_sha" "$capture_id" <<'PY'
import datetime,hashlib,json,os,pathlib,re,stat,sys
path=pathlib.Path(sys.argv[1]);generation,expected,drive_sha,freeze_sha,capture_id=sys.argv[2:]
hash_re=re.compile(r"[0-9a-f]{64}")
if any(hash_re.fullmatch(value) is None for value in (generation,expected,drive_sha,freeze_sha,capture_id)):
    raise SystemExit("live-observation generation verification identity is malformed")
fd=os.open(path,os.O_RDONLY|getattr(os,"O_NOFOLLOW",0))
try:
    details=os.fstat(fd)
    if (not stat.S_ISREG(details.st_mode) or path.is_symlink() or details.st_uid!=os.geteuid()
            or details.st_nlink!=1 or stat.S_IMODE(details.st_mode)!=0o400
            or not 0<details.st_size<=4*1024*1024):
        raise SystemExit("live-observation generation receipt is unsafe")
    raw=os.read(fd,4*1024*1024+1)
    if len(raw)!=details.st_size:raise SystemExit("live-observation generation receipt changed")
finally:os.close(fd)
value=json.loads(raw);canonical=(json.dumps(value,sort_keys=True,separators=(",",":"))+"\n").encode()
drive=value.get("drive_prefreeze_receipt")
generation_fields={"schema","source_main_commit","freeze_plan_sha256","capture_id",
    "observation_generation","created_at","max_selection_age_seconds","drive_prefreeze_receipt"}
drive_wrapper_fields={"path","sha256","value"}
drive_fields={"schema","mode","freeze_plan_sha256","capture_id","remote_root_sha256",
    "client_id_sha256","account_sha256","permission_id_sha256","rclone_version",
    "source_bytes","archive_reservation_bytes","largest_object_reservation_bytes",
    "daily_upload_budget_bytes","daily_upload_budget_basis","available_bytes_before",
    "available_bytes_after","canary_bytes","canary_verified","canary_deleted"}
drive_value=drive.get("value") if isinstance(drive,dict) else None
try:
    created=datetime.datetime.strptime(value.get("created_at"),"%Y-%m-%dT%H:%M:%S.%fZ")
except (TypeError,ValueError):
    raise SystemExit("live-observation generation timestamp differs")
drive_path=drive.get("path") if isinstance(drive,dict) else None
numbers=(drive_value.get(field) for field in ("source_bytes","archive_reservation_bytes",
    "largest_object_reservation_bytes","daily_upload_budget_bytes","available_bytes_before",
    "available_bytes_after","canary_bytes")) if isinstance(drive_value,dict) else ()
if (set(value)!=generation_fields or raw!=canonical or hashlib.sha256(raw).hexdigest()!=expected
        or value.get("schema")!="arc.recovery.legacy-live-observation-generation.v1"
        or re.fullmatch(r"[0-9a-f]{40}",str(value.get("source_main_commit"))) is None
        or (value.get("observation_generation"),value.get("freeze_plan_sha256"),value.get("capture_id"))
            !=(generation,freeze_sha,capture_id)
        or value.get("max_selection_age_seconds")!=300
        or path.name!=generation+".json" or not isinstance(drive,dict)
        or set(drive)!=drive_wrapper_fields or not isinstance(drive_path,str)
        or not pathlib.Path(drive_path).is_absolute() or os.path.normpath(drive_path)!=drive_path
        or drive.get("sha256")!=drive_sha
        or not isinstance(drive_value,dict) or set(drive_value)!=drive_fields
        or hashlib.sha256((json.dumps(drive_value,sort_keys=True,separators=(",",":"))+"\n").encode()).hexdigest()!=drive_sha
        or (drive_value.get("schema"),drive_value.get("mode"),drive_value.get("freeze_plan_sha256"),
            drive_value.get("capture_id"),drive_value.get("canary_verified"),
            drive_value.get("canary_deleted"))
            !=("arc.recovery.drive-prefreeze.v1","execute",freeze_sha,capture_id,True,True)
        or drive_value.get("rclone_version")!="v1.75.0"
        or drive_value.get("daily_upload_budget_basis")
            !="operator-reviewed-remaining-dedicated-account"
        or drive_value.get("canary_bytes")!=8*1024*1024
        or any(hash_re.fullmatch(str(drive_value.get(field))) is None
               or drive_value.get(field)=="0"*64 for field in
               ("remote_root_sha256","client_id_sha256","account_sha256","permission_id_sha256"))
        or any(isinstance(number,bool) or not isinstance(number,int) or number<=0 for number in numbers)
        or drive_value.get("available_bytes_before")
            <drive_value.get("archive_reservation_bytes")+drive_value.get("canary_bytes")
        or drive_value.get("available_bytes_after")<drive_value.get("archive_reservation_bytes")
        or drive_value.get("daily_upload_budget_bytes")<drive_value.get("archive_reservation_bytes")):
    raise SystemExit("live-observation generation receipt bytes or bindings differ")
PY
}

verify_live_observation_selection_exact() {
    local path="$1" expected_sha="$2" generation_receipt="$3"
    local generation="$4" generation_sha="$5" drive_sha="$6"
    local freeze_sha="$7" capture_id="$8"
    verify_live_observation_generation_receipt_exact "$generation_receipt" \
        "$generation" "$generation_sha" "$drive_sha" "$freeze_sha" "$capture_id"
    python3 - "$path" "$expected_sha" "$generation_receipt" "$generation" \
        "$generation_sha" "$drive_sha" "$freeze_sha" "$capture_id" <<'PY'
import datetime,hashlib,json,os,pathlib,re,stat,sys
path=pathlib.Path(sys.argv[1]);expected,gen_path_raw,generation,gen_sha,drive_sha,freeze,capture=sys.argv[2:]
gen_path=pathlib.Path(gen_path_raw);hash_re=re.compile(r"[0-9a-f]{64}")
canonical=lambda value:(json.dumps(value,sort_keys=True,separators=(",",":"))+"\n").encode()
def locked(item,label):
    fd=os.open(item,os.O_RDONLY|getattr(os,"O_NOFOLLOW",0))
    try:
        details=os.fstat(fd)
        if (not stat.S_ISREG(details.st_mode) or item.is_symlink()
                or details.st_uid!=os.geteuid() or details.st_nlink!=1
                or stat.S_IMODE(details.st_mode)!=0o400
                or not 0<details.st_size<=16*1024*1024):
            raise SystemExit(f"live-observation {label} is unsafe")
        raw=os.read(fd,16*1024*1024+1)
        if len(raw)!=details.st_size:raise SystemExit(f"live-observation {label} changed")
        value=json.loads(raw)
        if raw!=canonical(value):raise SystemExit(f"live-observation {label} is noncanonical")
        return value,raw
    finally:os.close(fd)
if any(hash_re.fullmatch(value) is None for value in
       (expected,generation,gen_sha,drive_sha,freeze,capture)):
    raise SystemExit("live-observation selection verification identity is malformed")
value,raw=locked(path,"selection");gen_value,gen_raw=locked(gen_path,"generation receipt")
fields={"schema","source_main_commit","freeze_plan_sha256","capture_id",
    "observation_generation","observation_generation_receipt",
    "observation_generation_receipt_path","observation_generation_receipt_sha256",
    "drive_prefreeze_receipt_path","drive_prefreeze_receipt_sha256",
    "generation_created_at","selected_at","max_selection_age_seconds","labels","nodes"}
try:
    created=datetime.datetime.strptime(value.get("generation_created_at"),"%Y-%m-%dT%H:%M:%S.%fZ")
    selected=datetime.datetime.strptime(value.get("selected_at"),"%Y-%m-%dT%H:%M:%S.%fZ")
except (TypeError,ValueError):
    raise SystemExit("live-observation selection timestamp differs")
drive_wrapper=gen_value.get("drive_prefreeze_receipt",{})
nodes=value.get("nodes")
if (set(value)!=fields or hashlib.sha256(raw).hexdigest()!=expected
        or value.get("schema")!="arc.recovery.legacy-live-observation-selection.v1"
        or value.get("source_main_commit")!=gen_value.get("source_main_commit")
        or (value.get("observation_generation"),value.get("freeze_plan_sha256"),value.get("capture_id"))
            !=(generation,freeze,capture)
        or value.get("observation_generation_receipt")!=gen_value
        or value.get("observation_generation_receipt_path")!=str(gen_path)
        or value.get("observation_generation_receipt_sha256")!=gen_sha
        or hashlib.sha256(gen_raw).hexdigest()!=gen_sha
        or value.get("generation_created_at")!=gen_value.get("created_at")
        or value.get("max_selection_age_seconds")!=300
        or not 0<=(selected-created).total_seconds()<=300
        or value.get("drive_prefreeze_receipt_path")!=drive_wrapper.get("path")
        or value.get("drive_prefreeze_receipt_sha256")!=drive_sha
        or value.get("labels")!=["diagnostic","noncanonical","nonreward"]
        or not isinstance(nodes,list) or len(nodes)!=6
        or [row.get("node") for row in nodes]!=["nyc","lax","ams","lhr","nrt","sgp"]):
    raise SystemExit("live-observation selection bytes or bindings differ")
node_fields={"node","created_at","completed_at","root_sha256","receipt_sha256"}
for row in nodes:
    try:
        started=datetime.datetime.strptime(row.get("created_at"),"%Y-%m-%dT%H:%M:%S.%fZ")
        completed=datetime.datetime.strptime(row.get("completed_at"),"%Y-%m-%dT%H:%M:%S.%fZ")
    except (AttributeError,TypeError,ValueError):
        raise SystemExit("live-observation selection node timestamp differs")
    if (set(row)!=node_fields or started>completed
            or hash_re.fullmatch(str(row.get("root_sha256"))) is None
            or hash_re.fullmatch(str(row.get("receipt_sha256"))) is None):
        raise SystemExit("live-observation selection node row differs")
PY
}

run_live_observations_exact() {
    local freeze_plan="$1" freeze_sha="$2" capture_id="$3" observation_generation="$4"
    local generation_receipt_sha="$5" drive_receipt_sha="$6" node="$7"
    run_remote "$node" capture-live-observations "$capture_id" "$observation_generation" \
        "$generation_receipt_sha" "$drive_receipt_sha" "$node" "$freeze_sha" \
        "$(freeze_node_field "$freeze_plan" "$node" writer_pid)" \
        "$(freeze_node_field "$freeze_plan" "$node" writer_start_ticks)" \
        "$(freeze_node_field "$freeze_plan" "$node" boot_id)" \
        "$(freeze_node_field "$freeze_plan" "$node" executable_path)" \
        "$(freeze_node_field "$freeze_plan" "$node" executable_sha256)" \
        "$(freeze_node_field "$freeze_plan" "$node" argv_sha256)" \
        "$(freeze_node_field "$freeze_plan" "$node" rpc_origin)"
}

run_live_observations_eligibility_exact() {
    local freeze_plan="$1" freeze_sha="$2" capture_id="$3" observation_generation="$4" node="$5"
    run_remote "$node" live-observations-eligible "$capture_id" "$observation_generation" "$node" "$freeze_sha" \
        "$(freeze_node_field "$freeze_plan" "$node" writer_pid)" \
        "$(freeze_node_field "$freeze_plan" "$node" writer_start_ticks)" \
        "$(freeze_node_field "$freeze_plan" "$node" boot_id)"
}

capture_all_live_observations() {
    local freeze_plan="$1" freeze_sha="$2" capture_id="$3" observation_generation="$4"
    local generation_receipt="$5" generation_receipt_sha="$6" drive_receipt_sha="$7"
    local log_root="$8" statuses="$9"
    local node index failed=0 complete_count=0
    local pids=() names=()
    assert_pinned_freeze_bytes "$freeze_plan" "$freeze_sha"
    verify_live_observation_generation_receipt_exact "$generation_receipt" \
        "$observation_generation" "$generation_receipt_sha" "$drive_receipt_sha" \
        "$freeze_sha" "$capture_id"
    for node in nyc lax ams lhr nrt sgp; do
        if run_remote "$node" live-observations-status "$capture_id" "$observation_generation" \
                "$generation_receipt_sha" "$drive_receipt_sha" "$node" "$freeze_sha" \
                >/dev/null 2>&1; then
            complete_count=$((complete_count + 1))
        fi
    done
    if [ "$complete_count" -ne 6 ]; then
        # A partial retry may capture only while every sealed writer is still
        # live and unfenced. This fleet-wide read-only barrier prevents filling
        # a missing receipt after any other writer has crossed its stop fence.
        for node in nyc lax ams lhr nrt sgp; do
            (
                run_live_observations_eligibility_exact \
                    "$freeze_plan" "$freeze_sha" "$capture_id" "$observation_generation" "$node"
            ) > "$log_root/$node-live-observations-eligibility.log" 2>&1 &
            pids+=("$!")
            names+=("$node")
        done
        for index in "${!pids[@]}"; do
            if ! wait "${pids[$index]}"; then
                printf 'archive fleet: pre-freeze live-observation fleet eligibility failed: %s\n' \
                    "${names[$index]}" >&2
                sed -n '1,100p' "$log_root/${names[$index]}-live-observations-eligibility.log" >&2
                failed=1
            fi
        done
        [ "$failed" -eq 0 ] || \
            die "a receipt is missing after at least one writer became stopped/fenced; recapture is forbidden"
    fi
    pids=()
    names=()
    failed=0
    for node in nyc lax ams lhr nrt sgp; do
        (
            run_live_observations_exact "$freeze_plan" "$freeze_sha" "$capture_id" \
                "$observation_generation" "$generation_receipt_sha" "$drive_receipt_sha" "$node"
        ) > "$log_root/$node-live-observations.log" 2>&1 &
        pids+=("$!")
        names+=("$node")
    done
    for index in "${!pids[@]}"; do
        if wait "${pids[$index]}"; then
            sed -n '1,20p' "$log_root/${names[$index]}-live-observations.log"
        else
            printf 'archive fleet: bounded pre-freeze live-observation receipt failed: %s\n' \
                "${names[$index]}" >&2
            sed -n '1,100p' "$log_root/${names[$index]}-live-observations.log" >&2
            failed=1
        fi
    done
    [ "$failed" -eq 0 ] || \
        die "live-observation receipt set is incomplete; this execution sent no writer freeze/stop signal"
    : > "$statuses"
    chmod 600 "$statuses"
    for node in nyc lax ams lhr nrt sgp; do
        run_remote "$node" live-observations-status "$capture_id" "$observation_generation" \
            "$generation_receipt_sha" "$drive_receipt_sha" "$node" "$freeze_sha" >> "$statuses"
    done
    printf 'archive fleet: all six durable diagnostic/noncanonical/nonreward live-observation receipts verified\n'
}

run_sealed_source_status_exact() {
    local freeze_plan="$1" freeze_sha="$2" capture_id="$3" node="$4"
    run_remote "$node" sealed-source-status "$capture_id" "$node" "$freeze_sha" \
        "$(freeze_node_field "$freeze_plan" "$node" validator_address)" \
        "$(freeze_node_field "$freeze_plan" "$node" stake)" \
        "$(freeze_node_field "$freeze_plan" "$node" writer_pid)" \
        "$(freeze_node_field "$freeze_plan" "$node" writer_start_ticks)" \
        "$(freeze_node_field "$freeze_plan" "$node" boot_id)" \
        "$(freeze_node_field "$freeze_plan" "$node" writer_cgroup_sha256)" \
        "$(freeze_node_field "$freeze_plan" "$node" writer_supervision_mode)" \
        "$(freeze_node_field "$freeze_plan" "$node" supervisor_unit)" \
        "$(freeze_node_field "$freeze_plan" "$node" supervisor_main_pid)" \
        "$(freeze_node_field "$freeze_plan" "$node" supervisor_start_ticks)" \
        "$(freeze_node_field "$freeze_plan" "$node" supervisor_executable_path)" \
        "$(freeze_node_field "$freeze_plan" "$node" supervisor_executable_sha256)" \
        "$(freeze_node_field "$freeze_plan" "$node" supervisor_argv_sha256)" \
        "$(freeze_node_field "$freeze_plan" "$node" supervisor_context_sha256)" \
        "$(freeze_node_field "$freeze_plan" "$node" executable_path)" \
        "$(freeze_node_field "$freeze_plan" "$node" executable_sha256)" \
        "$(freeze_node_field "$freeze_plan" "$node" argv_sha256)" \
        "$(freeze_node_field "$freeze_plan" "$node" data_dir)"
}

remote_readiness() {
    local capture_id="$1" freeze_sha="$2" freeze_plan="$3"
    local node host pid start_ticks boot_id writer_cgroup_sha writer_supervision_mode
    local unit unit_main_pid supervisor_start_ticks
    local supervisor_executable_path supervisor_executable_sha supervisor_argv_sha
    local executable_path exe_sha argv_sha data_dir
    local model_path model_sha model_size
    for node in nyc lax ams lhr nrt sgp; do
        host="$(host_for "$node")"
        pid="$(freeze_node_field "$freeze_plan" "$node" writer_pid)"
        start_ticks="$(freeze_node_field "$freeze_plan" "$node" writer_start_ticks)"
        boot_id="$(freeze_node_field "$freeze_plan" "$node" boot_id)"
        writer_cgroup_sha="$(freeze_node_field "$freeze_plan" "$node" writer_cgroup_sha256)"
        writer_supervision_mode="$(freeze_node_field "$freeze_plan" "$node" writer_supervision_mode)"
        unit="$(freeze_node_field "$freeze_plan" "$node" supervisor_unit)"
        unit_main_pid="$(freeze_node_field "$freeze_plan" "$node" supervisor_main_pid)"
        supervisor_start_ticks="$(freeze_node_field "$freeze_plan" "$node" supervisor_start_ticks)"
        supervisor_executable_path="$(freeze_node_field "$freeze_plan" "$node" supervisor_executable_path)"
        supervisor_executable_sha="$(freeze_node_field "$freeze_plan" "$node" supervisor_executable_sha256)"
        supervisor_argv_sha="$(freeze_node_field "$freeze_plan" "$node" supervisor_argv_sha256)"
        executable_path="$(freeze_node_field "$freeze_plan" "$node" executable_path)"
        exe_sha="$(freeze_node_field "$freeze_plan" "$node" executable_sha256)"
        argv_sha="$(freeze_node_field "$freeze_plan" "$node" argv_sha256)"
        data_dir="$(freeze_node_field "$freeze_plan" "$node" data_dir)"
        model_path="$(freeze_node_field "$freeze_plan" "$node" model_path)"
        model_sha="$(freeze_node_field "$freeze_plan" "$node" model_sha256)"
        model_size="$(freeze_node_field "$freeze_plan" "$node" model_size_bytes)"
        if ssh_remote_exact "$host" /bin/sh -c \
            'set -eu; capture=$1 pid=$2 start=$3 boot=$4 writer_cgroup_sha=$5 writer_mode=$6 unit=$7 main=$8 supervisor_start=$9 supervisor_executable=${10} supervisor_exe_sha=${11} supervisor_argv_sha=${12} executable=${13} exe_sha=${14} argv_sha=${15} data=${16} model=${17} model_sha=${18} model_size=${19}; test "$(cat /proc/sys/kernel/random/boot_id)" = "$boot"; test -d "/proc/$pid"; test "$(awk '\''{print $22}'\'' "/proc/$pid/stat")" = "$start"; test "$(cat "/proc/$pid/comm")" = arc-node; test "$(pgrep -x arc-node)" = "$pid"; test "$(sha256sum "/proc/$pid/cgroup" | cut -d" " -f1)" = "$writer_cgroup_sha"; case "$writer_mode" in systemd-unit) grep -Fq "$unit" "/proc/$pid/cgroup";; detached-root-session) ! grep -Fq "$unit" "/proc/$pid/cgroup" && test "$(awk '\''{print $4}'\'' "/proc/$pid/stat")" = 1;; *) exit 1;; esac; test "$(systemctl show "$unit" --property=MainPID --value)" = "$main"; test -d "/proc/$main"; test "$(awk '\''{print $22}'\'' "/proc/$main/stat")" = "$supervisor_start"; test "$(readlink "/proc/$main/exe")" = "$supervisor_executable"; test "$(sha256sum "/proc/$main/exe" | cut -d" " -f1)" = "$supervisor_exe_sha"; test "$(sha256sum "/proc/$main/cmdline" | cut -d" " -f1)" = "$supervisor_argv_sha"; grep -Fq "$unit" "/proc/$main/cgroup"; test "$(readlink "/proc/$pid/exe")" = "$executable"; test "$(sha256sum "/proc/$pid/exe" | cut -d" " -f1)" = "$exe_sha"; test "$(sha256sum "/proc/$pid/cmdline" | cut -d" " -f1)" = "$argv_sha"; test -d "$data" && test ! -L "$data" && test -s "$data/state.wal"; test -f "$model" && test ! -L "$model"; test "$(stat -c %s "$model")" = "$model_size"; test "$(sha256sum "$model" | cut -d" " -f1)" = "$model_sha"; command -v curl >/dev/null; command -v python3 >/dev/null; command -v sha256sum >/dev/null; command -v zstd >/dev/null; command -v tar >/dev/null; command -v systemctl >/dev/null; test ! -e /root/arc-recovery-captures || { test -d /root/arc-recovery-captures && test ! -L /root/arc-recovery-captures; }; { test ! -e "$capture" || { test -d "$capture" && test ! -L "$capture"; }; }; bytes=$(du -s -B1 "$data" | cut -f1); files=$(find "$data" -type f | wc -l); wal_bytes=$(stat -c %s "$data/state.wal"); snapshot_bytes=0; for snapshot in "$data/state.snapshot.lz4" "$data.snapshot.lz4"; do if test -f "$snapshot" && test ! -L "$snapshot"; then snapshot_bytes=$((snapshot_bytes + $(stat -c %s "$snapshot"))); fi; done; binding_bytes=$((wal_bytes + snapshot_bytes)); test "$binding_bytes" -ge "$bytes" || binding_bytes=$bytes; binding_bytes=$((binding_bytes + 2147483648)); required_bytes=$((bytes + binding_bytes)); required_inodes=$((files + 10000)); free_bytes=$(df -PB1 /root | awk '\''NR==2 {print $4}'\''); free_inodes=$(df -Pi /root | awk '\''NR==2 {print $4}'\''); test "$free_bytes" -ge "$required_bytes" || { printf "insufficient recovery bytes including v3 headroom: need=%s free=%s\n" "$required_bytes" "$free_bytes" >&2; exit 1; }; test "$free_inodes" -ge "$required_inodes" || { printf "insufficient recovery inodes including v3 headroom: need=%s free=%s\n" "$required_inodes" "$free_inodes" >&2; exit 1; }' \
            /bin/sh "/root/arc-recovery-captures/$capture_id/$node" "$pid" "$start_ticks" \
            "$boot_id" "$writer_cgroup_sha" "$writer_supervision_mode" \
            "$unit" "$unit_main_pid" "$supervisor_start_ticks" \
            "$supervisor_executable_path" "$supervisor_executable_sha" "$supervisor_argv_sha" \
            "$executable_path" "$exe_sha" "$argv_sha" "$data_dir" \
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
        ssh_remote_exact "$host" /bin/sh -c \
            'set -eu; data=$1 model=$2 model_sha=$3 model_size=$4; ! pgrep -x arc-node >/dev/null 2>&1; test -d "$data" && test ! -L "$data" && test -s "$data/state.wal"; test -f "$model" && test ! -L "$model"; test "$(stat -c %s "$model")" = "$model_size"; test "$(sha256sum "$model" | cut -d" " -f1)" = "$model_sha"; bytes=$(du -s -B1 "$data" | cut -f1); files=$(find "$data" -type f | wc -l); wal_bytes=$(stat -c %s "$data/state.wal"); snapshot_bytes=0; for snapshot in "$data/state.snapshot.lz4" "$data.snapshot.lz4"; do if test -f "$snapshot" && test ! -L "$snapshot"; then snapshot_bytes=$((snapshot_bytes + $(stat -c %s "$snapshot"))); fi; done; binding_bytes=$((wal_bytes + snapshot_bytes)); test "$binding_bytes" -ge "$bytes" || binding_bytes=$bytes; binding_bytes=$((binding_bytes + 2147483648)); required_bytes=$((bytes + binding_bytes)); required_inodes=$((files + 10000)); free_bytes=$(df -PB1 /root | awk '\''NR==2 {print $4}'\''); free_inodes=$(df -Pi /root | awk '\''NR==2 {print $4}'\''); test "$free_bytes" -ge "$required_bytes"; test "$free_inodes" -ge "$required_inodes"' \
            /bin/sh "$data_dir" "$model_path" "$model_sha" "$model_size"
        printf '  exact %s stop/content and disk ready: %s %s data=%s\n' \
            "$readiness_state" "$node" "$host" "$data_dir"
    done
}

stop_after_quarantine_round_exact() {
    local capture_id="$1" freeze_sha="$2" node="$3" round="$4"
    local authorization_sha="$5" readiness_sha="$6" transition_sha="$7"
    local final_source_capture_sha="$8"
    require_uint "$round" "quarantine transition round"
    require_hash "$authorization_sha" "quarantine round authorization root"
    require_hash "$readiness_sha" "quarantine round readiness root"
    require_hash "$transition_sha" "quarantine node transition root"
    require_hash "$final_source_capture_sha" "post-quarantine final source capture root"
    run_remote "$node" stop-after-quarantine-round "$capture_id" "$node" \
        "$freeze_sha" "$round" "$authorization_sha" "$readiness_sha" \
        "$transition_sha" "$final_source_capture_sha"
}

run_quarantine_status_exact() {
    local freeze_sha="$2" capture_id="$3" node="$4"
    run_remote "$node" quarantine-status "$capture_id" "$node" "$freeze_sha"
}

run_quarantine_monitor_receipt_exact() {
    local freeze_sha="$2" capture_id="$3" node="$4"
    run_remote "$node" quarantine-monitor-receipt "$capture_id" "$node" "$freeze_sha"
}

run_quarantine_public_cross_proof_exact() {
    local freeze_sha="$2" capture_id="$3" node="$4" receipt="$5" challenge="$6"
    run_remote "$node" quarantine-public-cross-proof "$capture_id" "$node" "$freeze_sha" \
        "$(legacy_height_row_field "$receipt" "$node" info_after_height)" \
        "$(legacy_height_row_field "$receipt" "$node" latest_block_height)" \
        "$(legacy_height_row_field "$receipt" "$node" latest_block_hash)" \
        "$challenge"
}

run_quarantine_stability_sample_exact() {
    local freeze_sha="$2" capture_id="$3" node="$4" challenge="$5" sample_index="$6"
    run_remote "$node" quarantine-stability-sample "$capture_id" "$node" "$freeze_sha" \
        "$challenge" "$sample_index"
}

create_network_quarantine_stability_proof() {
    local freeze_plan="$1" freeze_sha="$2" capture_id="$3" challenge="$4"
    local sample_root="$5" output="$6" started_at="$7" completed_at="$8"
    local elapsed_ns="$9"
    local generation_ledger="${10}"
    python3 - "$freeze_plan" "$freeze_sha" "$capture_id" "$challenge" \
        "$sample_root" "$output" "$started_at" "$completed_at" "$elapsed_ns" \
        "$generation_ledger" \
        "${NODES[@]}" <<'PY'
import datetime,hashlib,json,os,pathlib,re,stat,sys
(plan_raw,freeze,capture,challenge,root_raw,output_raw,started,completed,elapsed_raw,ledger_raw,
 *fleet_raw)=sys.argv[1:]
root=pathlib.Path(root_raw);output=pathlib.Path(output_raw);fleet=[tuple(row.split("=",1)) for row in fleet_raw]
canonical=lambda value:(json.dumps(value,sort_keys=True,separators=(",",":"))+"\n").encode()
digest=lambda raw:hashlib.sha256(raw).hexdigest();hash_re=re.compile(r"[0-9a-f]{64}")
expected=[("nyc","149.28.32.76"),("lax","140.82.16.112"),("ams","136.244.109.1"),
          ("lhr","104.238.171.11"),("nrt","202.182.107.41"),("sgp","149.28.153.31")]
if fleet!=expected or hash_re.fullmatch(freeze) is None or hash_re.fullmatch(capture) is None or hash_re.fullmatch(challenge) is None:
    raise SystemExit("quarantine stability fleet/hash identity differs")
for timestamp in (started,completed):datetime.datetime.strptime(timestamp,"%Y-%m-%dT%H:%M:%SZ")
elapsed=int(elapsed_raw)
plan=json.loads(pathlib.Path(plan_raw).read_text(encoding="utf-8"));source=plan.get("source_commit")
if plan.get("schema")!="arc.recovery.freeze-plan.v5" or not isinstance(source,str):
    raise SystemExit("quarantine stability freeze plan differs")
ledger_path=pathlib.Path(ledger_raw);ledger_bytes=ledger_path.read_bytes();ledger=json.loads(ledger_bytes)
if (ledger_bytes!=canonical(ledger) or ledger.get("schema")!="arc.recovery.quarantine-generation-ledger.v2"
        or (ledger.get("freeze_plan_sha256"),ledger.get("capture_id"))!=(freeze,capture)
        or [(row.get("node"),row.get("host")) for row in ledger.get("fleet",[])]!=fleet):
    raise SystemExit("quarantine stability generation ledger differs")
transitions=[]
for round_wrapper in ledger.get("rounds",[]):
    for wrapper in round_wrapper.get("result",{}).get("value",{}).get("transitions",[]):
        value=wrapper.get("value",{})
        transitions.append((value.get("node"),value.get("schema"),wrapper.get("sha256")))
if len(transitions)!=len(fleet) or {row[0] for row in transitions}!={row[0] for row in fleet}:
    raise SystemExit("quarantine stability transition partition differs")
active_schema="arc.recovery.quarantine-node-nft-applied.v1"
active_names={node for node,schema,_root in transitions if schema==active_schema}
active_fleet=[row for row in fleet if row[0] in active_names]
active_roots=[{"node":node,"sha256":next(root for name,schema,root in transitions
        if name==node and schema==active_schema)} for node,_host in active_fleet]
if any(hash_re.fullmatch(str(row["sha256"])) is None for row in active_roots):
    raise SystemExit("quarantine stability active transition roots differ")
if active_fleet and elapsed<120_000_000_000:
    raise SystemExit("quarantine stability monotonic interval is below 120 seconds")
if not active_fleet and elapsed!=0:
    raise SystemExit("all-stopped stability proof must have zero elapsed time")
sample_fields={"schema","capture_id","node","freeze_plan_sha256","challenge","sample_index",
    "started_at","completed_at","quarantine_status_before","quarantine_status_before_sha256",
    "quarantine_status_after","quarantine_status_after_sha256","writer","listener_ownership",
    "head","output_deny_packets","ss_sha256","global_absence_claimed"}
status_fields={"schema","capture_id","node","freeze_plan_sha256","receipt_sha256","table",
    "rule_counters","counter_snapshot_sha256","owned_ruleset_stateless_sha256",
    "listener_inventory","loopback_head","quarantine_policy","active","enabled"}
rows=[];fleet_heads=[]
for node,host in active_fleet:
    samples=[]
    for index in (0,1):
        path=root/f"{node}-{index}.json";details=path.lstat();raw=path.read_bytes();value=json.loads(raw)
        if (path.is_symlink() or not stat.S_ISREG(details.st_mode) or raw!=canonical(value)
                or set(value)!=sample_fields or value.get("schema")!="arc.recovery.legacy-network-quarantine-stability-sample.v1"
                or (value.get("capture_id"),value.get("node"),value.get("freeze_plan_sha256"),
                    value.get("challenge"),value.get("sample_index"))!=(capture,node,freeze,challenge,index)
                or value.get("global_absence_claimed") is not False):
            raise SystemExit(f"quarantine stability sample differs: {node}/{index}")
        for side in ("before","after"):
            status=value[f"quarantine_status_{side}"]
            if (not isinstance(status,dict) or set(status)!=status_fields
                    or digest(canonical(status))!=value[f"quarantine_status_{side}_sha256"]
                    or status.get("active") is not True or status.get("enabled") is not True):
                raise SystemExit(f"quarantine stability status differs: {node}/{index}/{side}")
        head=value.get("head")
        if (not isinstance(head,dict) or set(head)!={"height","block_hash","state_root","response_sha256","stable_attempt"}
                or isinstance(head.get("height"),bool) or not isinstance(head.get("height"),int) or head["height"]<1
                or hash_re.fullmatch(str(head.get("block_hash"))) is None
                or hash_re.fullmatch(str(head.get("state_root"))) is None):
            raise SystemExit(f"quarantine stability head differs: {node}/{index}")
        response_sha=head.get("response_sha256")
        if (not isinstance(response_sha,dict)
                or set(response_sha)!={"info_before","latest","exact","info_after"}
                or any(hash_re.fullmatch(str(value)) is None for value in response_sha.values())
                or isinstance(head.get("stable_attempt"),bool)
                or not isinstance(head.get("stable_attempt"),int)
                or not 1<=head["stable_attempt"]<=10):
            raise SystemExit(f"quarantine stability response roots differ: {node}/{index}")
        samples.append({"value":value,"sha256":digest(raw)})
    sample_heads=[{key:sample["value"]["head"][key]
                   for key in ("height","block_hash","state_root")} for sample in samples]
    if sample_heads[0]!=sample_heads[1]:
        raise SystemExit(f"quarantine stability host head changed: {node}")
    counters=[sample["value"].get("output_deny_packets") for sample in samples]
    if (any(isinstance(value,bool) or not isinstance(value,int) or value<0 for value in counters)
            or counters[1]<counters[0]):
        raise SystemExit(f"quarantine stability output deny counter regressed: {node}")
    if samples[0]["value"]["writer"]!=samples[1]["value"]["writer"]:
        raise SystemExit(f"quarantine stability writer changed: {node}")
    fleet_heads.append({"node":node,"host":host,"head":sample_heads[0]})
    rows.append({"node":node,"host":host,"samples":samples,
                 "output_deny_packets":{"sample_0":counters[0],"sample_1":counters[1]}})
value={"schema":"arc.recovery.legacy-network-quarantine-stability.v1",
       "source_main_commit":source,"freeze_plan_sha256":freeze,"capture_id":capture,
       "quarantine_generation_ledger_sha256":digest(ledger_bytes),
       "active_transition_sha256s":active_roots,
       "challenge":challenge,"interval_seconds":120 if active_fleet else 0,
       "sample_count":2 if active_fleet else 0,
       "started_at":started,"completed_at":completed,"monotonic_elapsed_ns":elapsed,
       "fleet_heads":fleet_heads,"nodes":rows,"global_absence_claimed":False}
payload=canonical(value)
fd=os.open(output,os.O_WRONLY|os.O_CREAT|os.O_EXCL|getattr(os,"O_NOFOLLOW",0),0o400)
with os.fdopen(fd,"wb") as handle:handle.write(payload);handle.flush();os.fsync(handle.fileno());os.fchmod(handle.fileno(),0o400)
PY
}

verify_network_quarantine_stability_proof() {
    local proof="$1" freeze_sha="$2" capture_id="$3" challenge="$4"
    local generation_ledger="$5"
    python3 - "$proof" "$freeze_sha" "$capture_id" "$challenge" \
        "$generation_ledger" "${NODES[@]}" <<'PY'
import datetime,hashlib,json,os,pathlib,re,stat,sys
path=pathlib.Path(sys.argv[1]);freeze,capture,challenge,ledger_raw=sys.argv[2:6]
fleet=[tuple(row.split("=",1)) for row in sys.argv[6:]]
canonical=lambda value:(json.dumps(value,sort_keys=True,separators=(",",":"))+"\n").encode()
digest=lambda raw:hashlib.sha256(raw).hexdigest();hash_re=re.compile(r"[0-9a-f]{64}")
details=path.lstat();raw=path.read_bytes();value=json.loads(raw)
if (path.is_symlink() or not stat.S_ISREG(details.st_mode) or details.st_uid!=os.geteuid()
        or details.st_nlink!=1 or stat.S_IMODE(details.st_mode)!=0o400 or raw!=canonical(value)):
    raise SystemExit("network-quarantine stability proof is unsafe")
fields={"schema","source_main_commit","freeze_plan_sha256","capture_id","challenge",
        "interval_seconds","sample_count","started_at","completed_at","monotonic_elapsed_ns",
        "fleet_heads","nodes","global_absence_claimed",
        "quarantine_generation_ledger_sha256","active_transition_sha256s"}
ledger_path=pathlib.Path(ledger_raw);ledger_bytes=ledger_path.read_bytes();ledger=json.loads(ledger_bytes)
if (ledger_bytes!=canonical(ledger) or ledger.get("schema")!="arc.recovery.quarantine-generation-ledger.v2"
        or (ledger.get("freeze_plan_sha256"),ledger.get("capture_id"))!=(freeze,capture)):
    raise SystemExit("network-quarantine stability ledger differs")
transitions=[]
for round_wrapper in ledger.get("rounds",[]):
    for wrapper in round_wrapper.get("result",{}).get("value",{}).get("transitions",[]):
        item=wrapper.get("value",{});transitions.append((item.get("node"),item.get("schema"),wrapper.get("sha256")))
active_schema="arc.recovery.quarantine-node-nft-applied.v1"
active_fleet=[row for row in fleet if any(name==row[0] and schema==active_schema for name,schema,_ in transitions)]
active_roots=[{"node":node,"sha256":next(root for name,schema,root in transitions
    if name==node and schema==active_schema)} for node,_host in active_fleet]
if (set(value)!=fields or value.get("schema")!="arc.recovery.legacy-network-quarantine-stability.v1"
        or (value.get("freeze_plan_sha256"),value.get("capture_id"),value.get("challenge"))
            !=(freeze,capture,challenge)
        or value.get("quarantine_generation_ledger_sha256")!=digest(ledger_bytes)
        or value.get("active_transition_sha256s")!=active_roots
        or value.get("interval_seconds")!=(120 if active_fleet else 0)
        or value.get("sample_count")!=(2 if active_fleet else 0)
        or value.get("global_absence_claimed") is not False
        or isinstance(value.get("monotonic_elapsed_ns"),bool)
        or not isinstance(value.get("monotonic_elapsed_ns"),int)
        or (active_fleet and value["monotonic_elapsed_ns"]<120_000_000_000)
        or (not active_fleet and value["monotonic_elapsed_ns"]!=0)):
    raise SystemExit("network-quarantine stability proof identity differs")
for timestamp in (value.get("started_at"),value.get("completed_at")):
    datetime.datetime.strptime(timestamp,"%Y-%m-%dT%H:%M:%SZ")
rows=value.get("nodes");heads=value.get("fleet_heads")
if (not isinstance(rows,list) or not isinstance(heads,list)
        or [(row.get("node"),row.get("host")) for row in rows]!=active_fleet
        or [(row.get("node"),row.get("host")) for row in heads]!=active_fleet):
    raise SystemExit("network-quarantine stability topology differs")
sample_fields={"schema","capture_id","node","freeze_plan_sha256","challenge","sample_index",
    "started_at","completed_at","quarantine_status_before","quarantine_status_before_sha256",
    "quarantine_status_after","quarantine_status_after_sha256","writer","listener_ownership",
    "head","output_deny_packets","ss_sha256","global_absence_claimed"}
for row,head_row,(node,host) in zip(rows,heads,active_fleet):
    if set(row)!={"node","host","samples","output_deny_packets"}:
        raise SystemExit(f"network-quarantine stability row fields differ: {node}")
    samples=row.get("samples")
    if not isinstance(samples,list) or len(samples)!=2:raise SystemExit(f"stability samples differ: {node}")
    projected=[];counters=[];writer=None
    for index,sealed in enumerate(samples):
        if not isinstance(sealed,dict) or set(sealed)!={"value","sha256"}:
            raise SystemExit(f"stability sealed sample differs: {node}/{index}")
        sample=sealed.get("value")
        if (not isinstance(sample,dict) or set(sample)!=sample_fields
                or digest(canonical(sample))!=sealed.get("sha256")
                or (sample.get("capture_id"),sample.get("node"),sample.get("freeze_plan_sha256"),
                    sample.get("challenge"),sample.get("sample_index"))!=(capture,node,freeze,challenge,index)
                or sample.get("global_absence_claimed") is not False):
            raise SystemExit(f"stability sample identity differs: {node}/{index}")
        current={key:sample.get("head",{}).get(key) for key in ("height","block_hash","state_root")}
        if (isinstance(current["height"],bool) or not isinstance(current["height"],int)
                or current["height"]<1 or hash_re.fullmatch(str(current["block_hash"])) is None
                or hash_re.fullmatch(str(current["state_root"])) is None):
            raise SystemExit(f"stability sample head differs: {node}/{index}")
        projected.append(current);counters.append(sample.get("output_deny_packets"))
        if writer is None:writer=sample.get("writer")
        elif sample.get("writer")!=writer:raise SystemExit(f"stability writer changed: {node}")
    if projected[0]!=projected[1] or head_row!={"node":node,"host":host,"head":projected[0]}:
        raise SystemExit(f"stability per-host head changed: {node}")
    if (any(isinstance(counter,bool) or not isinstance(counter,int) or counter<0 for counter in counters)
            or counters[1]<counters[0]
            or row.get("output_deny_packets")!={"sample_0":counters[0],"sample_1":counters[1]}):
        raise SystemExit(f"stability deny counter regressed: {node}")
print(hashlib.sha256(raw).hexdigest())
PY
}

probe_quarantine_external_exact() (
    local freeze_plan="$1" freeze_sha="$2" capture_id="$3" node="$4"
    local before="$5" output="$6" challenge="$7" host temporary after probes
    host="$(host_for "$node")"
    temporary="$(mktemp -d)"
    trap 'find "$temporary" -depth -delete 2>/dev/null || true' EXIT
    after="$temporary/after.json"
    probes="$temporary/probes.json"
    python3 - "$before" "$host" "$node" "$freeze_sha" "$capture_id" \
        "$challenge" "$probes" <<'PY'
import datetime,hashlib,json,os,pathlib,socket,stat,sys
before_raw,host,node,freeze,capture,challenge,output_raw=sys.argv[1:]
before_path=pathlib.Path(before_raw);output=pathlib.Path(output_raw)
value=json.loads(before_path.read_text(encoding="utf-8"))
if (value.get("schema")!="arc.recovery.legacy-network-quarantine-status.v1"
        or value.get("node")!=node or value.get("freeze_plan_sha256")!=freeze
        or value.get("capture_id")!=capture or value.get("active") is not True
        or value.get("enabled") is not True):
    raise SystemExit("pre-challenge network-quarantine status differs")
inventory=value.get("listener_inventory")
if not isinstance(inventory,list): raise SystemExit("network-quarantine status omits listener inventory")
tcp={443,9090};udp={443,9091}
for row in inventory:
    if not isinstance(row,dict) or row.get("quarantine_coverage") not in {
            "explicit-ssh-allow","nonloopback-deny-before-conntrack"}:
        raise SystemExit("network-quarantine listener inventory is malformed")
    port=row.get("port");protocol=row.get("protocol")
    if isinstance(port,bool) or not isinstance(port,int) or not 0<port<65536:
        raise SystemExit("network-quarantine listener port is malformed")
    if protocol=="tcp" and port!=22: tcp.add(port)
    elif protocol=="udp": udp.add(port)
    elif protocol!="tcp": raise SystemExit("network-quarantine listener protocol differs")
started=datetime.datetime.now(datetime.timezone.utc).strftime("%Y-%m-%dT%H:%M:%SZ")
source=None;results=[];payload=bytes.fromhex(challenge)
route=socket.socket(socket.AF_INET,socket.SOCK_DGRAM)
try:
    route.connect((host,22));source=route.getsockname()[0]
finally: route.close()
for port in sorted(tcp):
    sock=socket.socket(socket.AF_INET,socket.SOCK_STREAM);sock.settimeout(1.0)
    try:
        code=sock.connect_ex((host,port))
        results.append({"protocol":"tcp","port":port,"connect_succeeded":code==0,
                        "connect_errno":code})
    finally: sock.close()
for port in sorted(udp):
    sock=socket.socket(socket.AF_INET,socket.SOCK_DGRAM);sock.settimeout(1.0)
    try:
        sent=sock.sendto(payload,(host,port))
        results.append({"protocol":"udp","port":port,
                        "payload_sha256":hashlib.sha256(payload).hexdigest(),
                        "bytes_sent":sent})
    finally: sock.close()
if any(row.get("connect_succeeded") is True for row in results):
    raise SystemExit("a quarantined external TCP listener remained reachable")
completed=datetime.datetime.now(datetime.timezone.utc).strftime("%Y-%m-%dT%H:%M:%SZ")
body={"started_at":started,"completed_at":completed,"operator_source_address":source,
      "challenge":challenge,"targets":{"tcp":sorted(tcp),"udp":sorted(udp)},
      "results":results}
payload_raw=(json.dumps(body,sort_keys=True,separators=(",",":"))+"\n").encode()
fd=os.open(output,os.O_WRONLY|os.O_CREAT|os.O_EXCL|getattr(os,"O_NOFOLLOW",0),0o400)
with os.fdopen(fd,"wb") as handle:handle.write(payload_raw);handle.flush();os.fsync(handle.fileno())
PY
    run_quarantine_status_exact "$freeze_plan" "$freeze_sha" "$capture_id" "$node" > "$after"
    python3 - "$before" "$after" "$probes" "$output" "$host" "$node" \
        "$freeze_sha" "$capture_id" "$challenge" <<'PY'
import hashlib,json,os,pathlib,stat,sys
before_raw,after_raw,probes_raw,output_raw,host,node,freeze,capture,challenge=sys.argv[1:]
canonical=lambda value:(json.dumps(value,sort_keys=True,separators=(",",":"))+"\n").encode()
def locked(path):
    path=pathlib.Path(path);details=path.lstat();raw=path.read_bytes();value=json.loads(raw)
    if path.is_symlink() or not stat.S_ISREG(details.st_mode) or raw!=canonical(value):
        raise SystemExit("external-quarantine proof input is unsafe/noncanonical")
    return value,raw
before,before_bytes=locked(before_raw);after,after_bytes=locked(after_raw);probes,probe_bytes=locked(probes_raw)
for value in (before,after):
    if (value.get("schema")!="arc.recovery.legacy-network-quarantine-status.v1"
            or (value.get("capture_id"),value.get("node"),value.get("freeze_plan_sha256"))
            !=(capture,node,freeze) or value.get("active") is not True or value.get("enabled") is not True):
        raise SystemExit("external-quarantine status identity differs")
if before.get("receipt_sha256")!=after.get("receipt_sha256"):
    raise SystemExit("network-quarantine receipt changed during external challenge")
comment="arc-recovery:prerouting:iifname:deny"
def packets(value):
    counter=value.get("rule_counters",{}).get(comment,{})
    answer=counter.get("packets")
    if isinstance(answer,bool) or not isinstance(answer,int) or answer<0:
        raise SystemExit("network-quarantine deny counter is malformed")
    return answer
before_packets=packets(before);after_packets=packets(after)
attempts=len(probes.get("results",[]))
if attempts<2 or after_packets-before_packets<attempts:
    raise SystemExit("external TCP/UDP challenges did not reach the quarantine deny rule")
if probes.get("challenge")!=challenge:
    raise SystemExit("external-quarantine challenge differs")
try:
    challenge_payload=bytes.fromhex(challenge)
    expected_payload_sha256=hashlib.sha256(challenge_payload).hexdigest()
except ValueError as error:
    raise SystemExit("external-quarantine challenge is not exact hexadecimal") from error
targets=probes.get("targets",{});results=probes.get("results")
if (not isinstance(targets,dict) or set(targets)!={"tcp","udp"}
        or not isinstance(results,list)
        or [(row.get("protocol"),row.get("port")) for row in results]
        != ([('tcp',port) for port in targets["tcp"]]
            + [('udp',port) for port in targets["udp"]])):
    raise SystemExit("external-quarantine challenged target/result inventory differs")
for row in results:
    if row.get("protocol")=="tcp":
        if (set(row)!={"protocol","port","connect_succeeded","connect_errno"}
                or row.get("connect_succeeded") is not False
                or isinstance(row.get("connect_errno"),bool)
                or not isinstance(row.get("connect_errno"),int)
                or row["connect_errno"] in {0,61,111}):
            raise SystemExit("external-quarantine TCP failure evidence differs")
    elif (set(row)!={"protocol","port","payload_sha256","bytes_sent"}
            or row.get("payload_sha256")!=expected_payload_sha256
            or row.get("bytes_sent")!=len(challenge_payload)):
        raise SystemExit("external-quarantine UDP payload evidence differs")
value={"schema":"arc.recovery.legacy-network-quarantine-external-proof.v1",
       "capture_id":capture,"node":node,"host":host,"freeze_plan_sha256":freeze,
       "challenge":challenge,"started_at":probes["started_at"],"completed_at":probes["completed_at"],
       "operator_source_address":probes["operator_source_address"],
       "listener_inventory":before["listener_inventory"],"targets":probes["targets"],
       "results":probes["results"],"network_quarantine_receipt_sha256":before["receipt_sha256"],
       "before_status_sha256":hashlib.sha256(before_bytes).hexdigest(),
       "after_status_sha256":hashlib.sha256(after_bytes).hexdigest(),
       "after_status":after,
       "deny_counter":{"comment":comment,"before_packets":before_packets,
                       "after_packets":after_packets,"minimum_delta":attempts},
       "ssh_status_reproved":True,"global_absence_claimed":False}
payload=canonical(value);output=pathlib.Path(output_raw)
fd=os.open(output,os.O_WRONLY|os.O_CREAT|os.O_EXCL|getattr(os,"O_NOFOLLOW",0),0o400)
with os.fdopen(fd,"wb") as handle:handle.write(payload);handle.flush();os.fsync(handle.fileno())
directory=os.open(output.parent,os.O_RDONLY|getattr(os,"O_DIRECTORY",0)|getattr(os,"O_NOFOLLOW",0))
try:os.fsync(directory)
finally:os.close(directory)
PY
    find "$temporary" -depth -delete
    trap - EXIT
)

run_drive_prefreeze_gate() {
    local mode="$1" freeze_plan="$2" freeze_sha="$3" capture_id="$4"
    local persist_phase="${5:-}"
    assert_pinned_freeze_bytes "$freeze_plan" "$freeze_sha"
    local receipt attempt_started_ns="" attempt_nonce=""
    if [ "$persist_phase" = archive-seal ]; then
        attempt_started_ns="$(python3 -c 'import time; print(time.time_ns())')"
        attempt_nonce="$(python3 -c 'import secrets; print(secrets.token_hex(32))')"
        require_uint "$attempt_started_ns" "Drive archive-seal attempt start"
        require_hash "$attempt_nonce" "Drive archive-seal attempt nonce"
    fi
    receipt="$(/usr/bin/env -i HOME="$ARCHIVE_FLEET_PINNED_TRANSPORT_ROOT" \
        PATH="$ARCHIVE_FLEET_PINNED_PYTHON_ROOT:$ARCHIVE_FLEET_PINNED_TRANSPORT_ROOT:/usr/bin:/bin:/usr/sbin:/sbin" \
        LANG=C LC_ALL=C RCLONE_CONFIG="$ARC_OPERATOR_RCLONE_CONFIG" \
        ARC_RECOVERY_PYTHON_PATH="$ARC_OPERATOR_PYTHON_SOURCE" \
        ARC_RECOVERY_PYTHON_SHA256="$ARC_OPERATOR_PYTHON_SHA256" \
        /bin/bash "$DRIVE_PREFREEZE_GATE" "$mode" \
        --freeze-plan "$freeze_plan" \
        --expected-freeze-plan-sha256 "$freeze_sha" \
        --capture-id "$capture_id" \
        --remote-root "$(manifest_field "$freeze_plan" drive_prefreeze.remote_root)" \
        --expected-root-sha256 "$(manifest_field "$freeze_plan" drive_prefreeze.remote_root_sha256)" \
        --expected-client-id-sha256 "$(manifest_field "$freeze_plan" drive_prefreeze.oauth_client_id_sha256)" \
        --expected-account-sha256 "$(manifest_field "$freeze_plan" drive_prefreeze.account_sha256)" \
        --daily-upload-budget-bytes "$(manifest_field "$freeze_plan" drive_prefreeze.daily_upload_budget_bytes)")"
    python3 - "$receipt" "$mode" "$freeze_sha" "$capture_id" <<'PY'
import json
import sys
value = json.loads(sys.argv[1])
if value.get("schema") != "arc.recovery.drive-prefreeze.v1" or value.get("mode") != sys.argv[2]:
    raise SystemExit("Drive prefreeze receipt schema/mode differs")
if value.get("freeze_plan_sha256") != sys.argv[3] or value.get("capture_id") != sys.argv[4]:
    raise SystemExit("Drive prefreeze receipt identity differs")
if sys.argv[2] == "execute" and (value.get("canary_verified") is not True or value.get("canary_deleted") is not True):
    raise SystemExit("Drive prefreeze execute receipt lacks verified/deleted canary")
print(json.dumps(value, sort_keys=True, separators=(",", ":")))
PY
    if [ "$mode" = execute ] || [ "$persist_phase" = archive-seal ]; then
        local receipt_root="${OPERATOR_FREEZE_PLAN}.drive-prefreeze-receipts/$capture_id"
        python3 - "$receipt_root" "$receipt" "$persist_phase" \
            "$attempt_started_ns" "$attempt_nonce" "$ARC_OPERATOR_RCLONE_BIN" \
            "$ARC_OPERATOR_RCLONE_SHA256" "$(hash_file "$ARC_OPERATOR_RCLONE_CONFIG")" <<'PY'
import datetime
import hashlib
import json
import os
import pathlib
import re
import stat
import sys
root = pathlib.Path(sys.argv[1])
value = json.loads(sys.argv[2])
phase = sys.argv[3]
(started_raw, nonce, rclone_path, rclone_sha, config_sha) = sys.argv[4:]
payload = (json.dumps(value, sort_keys=True, separators=(",", ":")) + "\n").encode()
digest = hashlib.sha256(payload).hexdigest()
hash_re=re.compile(r"[0-9a-f]{64}")
base=root.parent;operator_parent=base.parent;details=operator_parent.lstat()
if (operator_parent.is_symlink() or not stat.S_ISDIR(details.st_mode)
        or details.st_uid!=os.geteuid() or details.st_mode&0o022):
    raise SystemExit("Drive prefreeze receipt parent is unsafe")
parent_fd=os.open(operator_parent,os.O_RDONLY|getattr(os,"O_DIRECTORY",0)|getattr(os,"O_NOFOLLOW",0))
try:
    try:os.mkdir(base.name,0o700,dir_fd=parent_fd);os.fsync(parent_fd)
    except FileExistsError:pass
finally:
    os.close(parent_fd)
base_details=base.lstat()
if base.is_symlink() or not stat.S_ISDIR(base_details.st_mode) or base_details.st_uid!=os.geteuid() or stat.S_IMODE(base_details.st_mode)!=0o700:
    raise SystemExit("Drive prefreeze receipt base is unsafe")
base_fd=os.open(base,os.O_RDONLY|getattr(os,"O_DIRECTORY",0)|getattr(os,"O_NOFOLLOW",0))
try:
    try:os.mkdir(root.name,0o700,dir_fd=base_fd);os.fsync(base_fd)
    except FileExistsError:pass
finally:os.close(base_fd)
root_details=root.lstat()
if root.is_symlink() or not stat.S_ISDIR(root_details.st_mode) or root_details.st_uid!=os.geteuid() or stat.S_IMODE(root_details.st_mode)!=0o700:
    raise SystemExit("Drive prefreeze capture receipt root is unsafe")
if phase=="archive-seal":
    if (not started_raw.isdigit() or int(started_raw)<=0 or hash_re.fullmatch(nonce) is None
            or hash_re.fullmatch(rclone_sha) is None or hash_re.fullmatch(config_sha) is None):
        raise SystemExit("Drive archive-seal attempt identity is malformed")
    output=root/f"{started_raw}-{nonce}.json"
    completed_ns=max(int(started_raw),__import__('time').time_ns())
    attempt={"schema":"arc.recovery.drive-archive-seal-attempt.v1","phase":"archive-seal",
        "freeze_plan_sha256":value["freeze_plan_sha256"],"capture_id":value["capture_id"],
        "attempt_nonce":nonce,"started_at_unix_ns":int(started_raw),"completed_at_unix_ns":completed_ns,
        "completed_at":datetime.datetime.now(datetime.timezone.utc).strftime("%Y-%m-%dT%H:%M:%SZ"),
        "drive_prefreeze_receipt":value,"drive_prefreeze_receipt_sha256":digest,
        "rclone_path":rclone_path,"rclone_sha256":rclone_sha,"rclone_config_sha256":config_sha,
        "selected_immediately_before_first_archive_upload":True}
    attempt_payload=(json.dumps(attempt,sort_keys=True,separators=(",",":"))+"\n").encode()
    attempt_output=output.with_name(output.name+".attempt.json")
else:
    output=root/f"{digest}.json";attempt_output=None;attempt_payload=None
dfd=os.open(root,os.O_RDONLY|getattr(os,"O_DIRECTORY",0)|getattr(os,"O_NOFOLLOW",0))
def publish(path,data):
    partial=path.with_name(path.name+".partial")
    def read_name(name,modes):
        fd=os.open(name,os.O_RDONLY|getattr(os,"O_NOFOLLOW",0),dir_fd=dfd)
        try:
            info=os.fstat(fd)
            if (not stat.S_ISREG(info.st_mode) or info.st_uid!=os.geteuid() or info.st_nlink!=1
                    or stat.S_IMODE(info.st_mode) not in modes or info.st_size<=0 or info.st_size>4*1024*1024):
                raise SystemExit("Drive prefreeze receipt identity differs")
            raw=os.read(fd,4*1024*1024+1)
            if len(raw)!=info.st_size:raise SystemExit("Drive prefreeze receipt changed while read")
            return raw
        finally:os.close(fd)
    if path.exists() or path.is_symlink():
        if read_name(path.name,{0o400})!=data:raise SystemExit("existing Drive prefreeze receipt differs")
        return
    if partial.exists() or partial.is_symlink():
        current=read_name(partial.name,{0o400,0o600})
        if current!=data:os.unlink(partial.name,dir_fd=dfd);os.fsync(dfd)
        else:os.chmod(partial.name,0o400,dir_fd=dfd,follow_symlinks=False)
    if not partial.exists():
        fd=os.open(partial.name,os.O_WRONLY|os.O_CREAT|os.O_EXCL|getattr(os,"O_NOFOLLOW",0),0o600,dir_fd=dfd)
        with os.fdopen(fd,"wb") as handle:
            handle.write(data);handle.flush();os.fsync(handle.fileno());os.fchmod(handle.fileno(),0o400)
    os.rename(partial.name,path.name,src_dir_fd=dfd,dst_dir_fd=dfd);os.fsync(dfd)
try:
    publish(output,payload)
    if attempt_output is not None:publish(attempt_output,attempt_payload)
finally:os.close(dfd)
print(output)
PY
    fi
}

ensure_offline_capture() {
    local capture_id="$1" node="$2" observation_generation="$3"
    local generation_receipt_sha="$4" drive_receipt_sha="$5"
    run_remote "$node" capture-offline "$capture_id" "$node" "$observation_generation" \
        "$generation_receipt_sha" "$drive_receipt_sha"
    run_remote "$node" status "$capture_id" "$node"
}

run_persisted_head_exact() {
    local freeze_plan="$1" freeze_sha="$2" capture_id="$3" node="$4"
    local binary_sha="$5" genesis_sha="$6" validators_sha="$7" legacy_validators_sha="$8"
    run_remote "$node" persisted-head "$capture_id" "$node" "$freeze_sha" \
        "$binary_sha" "$genesis_sha" "$validators_sha" "$legacy_validators_sha" \
        "$(freeze_node_field "$freeze_plan" "$node" boot_id)"
}

create_legacy_maintenance_evidence_bundle() {
    local freeze_plan="$1" freeze_sha="$2" capture_id="$3" status_root="$4"
    local authenticated_cross="$5" quarantine_root="$6" persisted_root="$7"
    local stability_proof="$8" output="$9"
    local first_quarantine_started_at="${10}" all_controlled_stopped_at="${11}"
    local quarantine_generation_ledger="${12}"
    local live_observation_selection="${13}"
    python3 - "$freeze_plan" "$freeze_sha" "$capture_id" "$status_root" \
        "$authenticated_cross" "$quarantine_root" "$persisted_root" "$stability_proof" "$output" \
        "$first_quarantine_started_at" "$all_controlled_stopped_at" \
        "$quarantine_generation_ledger" "$live_observation_selection" \
        "$QUARANTINE_ROUND_MODULE" "${NODES[@]}" <<'PY'
import datetime,hashlib,json,os,pathlib,re,stat,sys
(plan_raw,freeze_sha,capture_id,status_root_raw,authenticated_raw,quarantine_raw,
 persisted_raw,stability_raw,output_raw,first_started,all_stopped,ledger_raw,
 observation_selection_raw,rounds_module_raw,*fleet_raw)=sys.argv[1:]
plan_path=pathlib.Path(plan_raw);status_root=pathlib.Path(status_root_raw)
authenticated_path=pathlib.Path(authenticated_raw);quarantine_root=pathlib.Path(quarantine_raw)
persisted_root=pathlib.Path(persisted_raw);stability_path=pathlib.Path(stability_raw)
ledger_path=pathlib.Path(ledger_raw)
observation_selection_path=pathlib.Path(observation_selection_raw)
output=pathlib.Path(output_raw)
sidecar=output.with_name(output.name+".sha256")
fleet=[tuple(row.split("=",1)) for row in fleet_raw]
expected=[("nyc","149.28.32.76"),("lax","140.82.16.112"),("ams","136.244.109.1"),
          ("lhr","104.238.171.11"),("nrt","202.182.107.41"),("sgp","149.28.153.31")]
canonical=lambda value:(json.dumps(value,sort_keys=True,separators=(",",":"))+"\n").encode()
digest=lambda raw:hashlib.sha256(raw).hexdigest()
hash_re=re.compile(r"[0-9a-f]{64}");utc_re=re.compile(r"\d{4}-\d{2}-\d{2}T\d{2}:\d{2}:\d{2}Z")
if fleet!=expected or hash_re.fullmatch(freeze_sha) is None or hash_re.fullmatch(capture_id) is None:
    raise SystemExit("maintenance evidence bundle fleet/hash identity differs")
if utc_re.fullmatch(first_started) is None or utc_re.fullmatch(all_stopped) is None:
    raise SystemExit("maintenance evidence bundle timestamps are not canonical UTC")
if datetime.datetime.strptime(first_started,"%Y-%m-%dT%H:%M:%SZ")>datetime.datetime.strptime(all_stopped,"%Y-%m-%dT%H:%M:%SZ"):
    raise SystemExit("maintenance evidence bundle timestamps are reversed")

def locked(path,label,maximum=32*1024*1024):
    path=pathlib.Path(path);fd=os.open(path,os.O_RDONLY|getattr(os,"O_NOFOLLOW",0)|getattr(os,"O_CLOEXEC",0))
    try:
        before=os.fstat(fd);visible=os.lstat(path)
        identity=lambda value:(value.st_dev,value.st_ino,value.st_mode,value.st_uid,value.st_gid,
                               value.st_nlink,value.st_size,value.st_mtime_ns,value.st_ctime_ns)
        if (not stat.S_ISREG(before.st_mode) or stat.S_ISLNK(visible.st_mode)
                or identity(before)!=identity(visible) or before.st_uid not in {0,os.geteuid()}
                or before.st_nlink!=1 or before.st_mode&0o022 or before.st_size<=0
                or before.st_size>maximum):
            raise SystemExit(f"maintenance evidence bundle {label} is unsafe")
        chunks=[]
        while True:
            chunk=os.read(fd,1024*1024)
            if not chunk:break
            chunks.append(chunk)
        raw=b"".join(chunks)
        if len(raw)!=before.st_size or identity(before)!=identity(os.fstat(fd)):
            raise SystemExit(f"maintenance evidence bundle {label} changed while read")
    finally:os.close(fd)
    try:value=json.loads(raw)
    except (UnicodeDecodeError,json.JSONDecodeError) as error:
        raise SystemExit(f"maintenance evidence bundle {label} is invalid JSON") from error
    if not isinstance(value,dict) or raw!=canonical(value):
        raise SystemExit(f"maintenance evidence bundle {label} is noncanonical")
    return value,raw

plan,plan_raw_bytes=locked(plan_path,"freeze plan")
source_commit=plan.get("source_commit")
if (plan.get("schema")!="arc.recovery.freeze-plan.v5" or digest(plan_raw_bytes)!=freeze_sha
        or not isinstance(source_commit,str) or re.fullmatch(r"[0-9a-f]{40}",source_commit) is None
        or [(row.get("name"),row.get("host")) for row in plan.get("nodes",[])]!=fleet):
    raise SystemExit("maintenance evidence bundle freeze plan differs")
authenticated,authenticated_bytes=locked(authenticated_path,"authenticated pre-fence proof")
if (authenticated.get("schema")!="arc.recovery.authenticated-legacy-height-fleet.v1"
        or (authenticated.get("source_main_commit"),authenticated.get("freeze_plan_sha256"),
            authenticated.get("capture_id"))!=(source_commit,freeze_sha,capture_id)):
    raise SystemExit("maintenance evidence bundle authenticated proof differs")
observation_selection,observation_selection_bytes=locked(
    observation_selection_path,"live-observation selection")
selection_fields={"schema","source_main_commit","freeze_plan_sha256","capture_id",
    "observation_generation","observation_generation_receipt",
    "observation_generation_receipt_path","observation_generation_receipt_sha256",
    "drive_prefreeze_receipt_path","drive_prefreeze_receipt_sha256",
    "generation_created_at","selected_at","max_selection_age_seconds","labels","nodes"}
generation_receipt=observation_selection.get("observation_generation_receipt")
if (set(observation_selection)!=selection_fields
        or observation_selection.get("schema")!="arc.recovery.legacy-live-observation-selection.v1"
        or (observation_selection.get("source_main_commit"),
            observation_selection.get("freeze_plan_sha256"),observation_selection.get("capture_id"))
            !=(source_commit,freeze_sha,capture_id)
        or not isinstance(generation_receipt,dict)
        or digest(canonical(generation_receipt))
            !=observation_selection.get("observation_generation_receipt_sha256")
        or generation_receipt.get("observation_generation")
            !=observation_selection.get("observation_generation")
        or generation_receipt.get("drive_prefreeze_receipt",{}).get("sha256")
            !=observation_selection.get("drive_prefreeze_receipt_sha256")
        or observation_selection.get("labels")!=["diagnostic","noncanonical","nonreward"]):
    raise SystemExit("maintenance evidence bundle live-observation selection differs")
datetime.datetime.strptime(observation_selection["selected_at"],"%Y-%m-%dT%H:%M:%S.%fZ")
# Selection UTC and node transition UTC are audit-only across hosts.  The
# authorization/readiness/dispatch/ledger hash chain is the causal boundary.
ledger,ledger_bytes=locked(ledger_path,"quarantine generation ledger")
import importlib.util
spec=importlib.util.spec_from_file_location("arc_quarantine_rounds",rounds_module_raw)
if spec is None or spec.loader is None:
    raise SystemExit("maintenance evidence bundle cannot load quarantine-round validator")
rounds=importlib.util.module_from_spec(spec);spec.loader.exec_module(rounds)
try:ledger_state=rounds.validate_generation_ledger(ledger)
except rounds.QuarantineRoundError as error:
    raise SystemExit(f"maintenance evidence bundle generation ledger differs: {error}") from error
if ((ledger_state["freeze_plan_sha256"],ledger_state["capture_id"])
        !=(freeze_sha,capture_id)
        or (ledger_state["live_observation_selection_sha256"],
            ledger_state["live_observation_generation"],
            ledger_state["observation_generation_receipt_sha256"],
            ledger_state["drive_prefreeze_receipt_sha256"])
            !=(digest(observation_selection_bytes),
               observation_selection["observation_generation"],
               observation_selection["observation_generation_receipt_sha256"],
               observation_selection["drive_prefreeze_receipt_sha256"])
        or ledger.get("first_secured_at")!=first_started
        or ledger_state["all_nodes_secured_at"]
            >datetime.datetime.strptime(all_stopped,"%Y-%m-%dT%H:%M:%SZ").replace(tzinfo=datetime.timezone.utc)):
    raise SystemExit("maintenance evidence bundle generation ledger identity/timeline differs")
transition_wrappers={}
transition_projections={}
for round_wrapper in ledger["rounds"]:
    for wrapper in round_wrapper["result"]["value"]["transitions"]:
        transition=wrapper["value"];name=transition["node"]
        if name in transition_wrappers:raise SystemExit("maintenance evidence repeats a node transition")
        transition_wrappers[name]=wrapper
        transition_projections[name]=rounds.validate_node_transition(transition)
if set(transition_wrappers)!={name for name,_host in fleet}:
    raise SystemExit("maintenance evidence transition partition differs")
active_fleet=[row for row in fleet if transition_projections[row[0]]["kind"]
              ==rounds.ACTIVE_TRANSITION_KIND]
active_roots=[{"node":node,"sha256":transition_wrappers[node]["sha256"]}
              for node,_host in active_fleet]
challenge_path=quarantine_root/"quarantine-challenge.json"
challenge_value,challenge_bytes=locked(challenge_path,"quarantine challenge")
challenge=challenge_value.get("challenge")
if (challenge_value.get("schema")!="arc.recovery.legacy-network-quarantine-challenge.v1"
        or (challenge_value.get("freeze_plan_sha256"),challenge_value.get("capture_id"))!=(freeze_sha,capture_id)
        or not isinstance(challenge,str) or hash_re.fullmatch(challenge) is None):
    raise SystemExit("maintenance evidence bundle challenge differs")
stability,stability_bytes=locked(stability_path,"quarantine stability proof")
stability_fields={"schema","source_main_commit","freeze_plan_sha256","capture_id","challenge",
    "interval_seconds","sample_count","started_at","completed_at","monotonic_elapsed_ns",
    "fleet_heads","nodes","global_absence_claimed",
    "quarantine_generation_ledger_sha256","active_transition_sha256s"}
if (set(stability)!=stability_fields
        or stability.get("schema")!="arc.recovery.legacy-network-quarantine-stability.v1"
        or (stability.get("source_main_commit"),stability.get("freeze_plan_sha256"),
            stability.get("capture_id"),stability.get("challenge"))
            !=(source_commit,freeze_sha,capture_id,challenge)
        or stability.get("quarantine_generation_ledger_sha256")!=digest(ledger_bytes)
        or stability.get("active_transition_sha256s")!=active_roots
        or stability.get("interval_seconds")!=(120 if active_fleet else 0)
        or stability.get("sample_count")!=(2 if active_fleet else 0)
        or isinstance(stability.get("monotonic_elapsed_ns"),bool)
        or not isinstance(stability.get("monotonic_elapsed_ns"),int)
        or (active_fleet and stability["monotonic_elapsed_ns"]<120_000_000_000)
        or (not active_fleet and stability["monotonic_elapsed_ns"]!=0)
        or stability.get("global_absence_claimed") is not False):
    raise SystemExit("maintenance evidence bundle stability proof differs")
stability_nodes=stability.get("nodes");stability_heads=stability.get("fleet_heads")
if (not isinstance(stability_nodes,list) or not isinstance(stability_heads,list)
        or [(row.get("node"),row.get("host")) for row in stability_nodes]!=active_fleet
        or [(row.get("node"),row.get("host")) for row in stability_heads]!=active_fleet):
    raise SystemExit("maintenance evidence bundle stability topology differs")
for row,head_row,(node,host) in zip(stability_nodes,stability_heads,active_fleet):
    samples=row.get("samples")
    if not isinstance(samples,list) or len(samples)!=2:
        raise SystemExit(f"maintenance evidence bundle stability samples differ: {node}")
    heads=[];counters=[];writer=None
    for index,sealed_sample in enumerate(samples):
        if (not isinstance(sealed_sample,dict) or set(sealed_sample)!={"value","sha256"}
                or not isinstance(sealed_sample.get("value"),dict)
                or digest(canonical(sealed_sample["value"]))!=sealed_sample.get("sha256")):
            raise SystemExit(f"maintenance evidence bundle sealed stability sample differs: {node}/{index}")
        sample=sealed_sample["value"]
        if ((sample.get("capture_id"),sample.get("node"),sample.get("freeze_plan_sha256"),
             sample.get("challenge"),sample.get("sample_index"))!=(capture_id,node,freeze_sha,challenge,index)
                or sample.get("global_absence_claimed") is not False):
            raise SystemExit(f"maintenance evidence bundle stability sample identity differs: {node}/{index}")
        head=sample.get("head",{});projected={key:head.get(key) for key in ("height","block_hash","state_root")}
        if (isinstance(projected["height"],bool) or not isinstance(projected["height"],int)
                or projected["height"]<1 or hash_re.fullmatch(str(projected["block_hash"])) is None
                or hash_re.fullmatch(str(projected["state_root"])) is None):
            raise SystemExit(f"maintenance evidence bundle stability head differs: {node}/{index}")
        heads.append(projected);counters.append(sample.get("output_deny_packets"))
        if writer is None:writer=sample.get("writer")
        elif sample.get("writer")!=writer:
            raise SystemExit(f"maintenance evidence bundle stability writer changed: {node}")
    if (heads[0]!=heads[1] or head_row!={"node":node,"host":host,"head":heads[0]}
            or any(isinstance(value,bool) or not isinstance(value,int) or value<0 for value in counters)
            or counters[1]<counters[0]
            or row.get("output_deny_packets")!={"sample_0":counters[0],"sample_1":counters[1]}):
        raise SystemExit(f"maintenance evidence bundle stability contract differs: {node}")

inventory=[]
def sealed(value,raw,node,role):
    root=digest(raw);inventory.append({"node":node,"role":role,"sha256":root,"size":len(raw)})
    return {"value":value,"sha256":root}
authenticated_sealed=sealed(authenticated,authenticated_bytes,"fleet","authenticated-prefence-height-cross-proof")
observation_selection_sealed=sealed(
    observation_selection,observation_selection_bytes,"fleet","live-observation-selection")
ledger_sealed=sealed(ledger,ledger_bytes,"fleet","quarantine-generation-ledger")
challenge_sealed=sealed(challenge_value,challenge_bytes,"fleet","network-quarantine-challenge")
stability_sealed=sealed(stability,stability_bytes,"fleet","network-quarantine-stability-proof")
nodes=[]
for node,host in fleet:
    transition_wrapper=transition_wrappers[node]
    transition=transition_wrapper["value"]
    projection=transition_projections[node]
    if projection["kind"]==rounds.STOPPED_PRECOMMIT_TRANSITION_KIND:
        current,current_bytes=locked(
            status_root/f"{node}-stopped-status.json",f"{node} stopped-round current status"
        )
        persisted,persisted_bytes=locked(
            persisted_root/f"{node}-persisted-head.json",f"{node} stopped-round persisted head"
        )
        embedded=transition.get("persisted_head",{})
        transition_bytes=canonical(transition)
        current_fields={"schema","capture_id","freeze_plan_sha256","node","host",
            "node_transition_receipt_sha256","transition_schema","transitioned_at",
            "observed_at","writer_state","current_boot_id","stable_head",
            "persistent_restart_fence_sha256","precommit_status_sha256","source_inputs",
            "nft_table_absent","applied_commit_absent","active_selector_absent",
            "fence_unit_enabled","fence_unit_active","automatic_legacy_restart"}
        if (set(current)!=current_fields
                or current.get("schema")
                    !="arc.recovery.quarantine-prior-persistently-stopped-status.v1"
                or (current.get("capture_id"),current.get("freeze_plan_sha256"),
                    current.get("node"),current.get("host"))
                    !=(capture_id,freeze_sha,node,host)
                or current.get("node_transition_receipt_sha256")
                    !=transition_wrapper["sha256"]
                or current.get("transition_schema")!=projection["schema"]
                or current.get("stable_head")!=projection["stable_head"]
                or current.get("writer_state")!="persistently-stopped"
                or current.get("nft_table_absent") is not True
                or current.get("active_selector_absent") is not True
                or current.get("fence_unit_enabled") is not True
                or current.get("fence_unit_active") is not False
                or current.get("automatic_legacy_restart") is not False):
            raise SystemExit(f"maintenance evidence stopped-round current status differs: {node}")
        if (not isinstance(embedded,dict) or set(embedded)!={"value","sha256"}
                or persisted_bytes!=canonical(embedded.get("value"))
                or digest(persisted_bytes)!=embedded.get("sha256")
                or persisted.get("source_pair_role")!="preauthorization-boundary"
                or persisted.get("live_source_capture_sha256")
                    !=persisted.get("source_inputs",{}).get("live_source_capture_sha256")
                or persisted.get("head")!=projection["stable_head"]):
            raise SystemExit(f"maintenance evidence stopped-round persisted head differs: {node}")
        transition_sealed=sealed(
            transition,transition_bytes,node,"persistently-stopped-transition"
        )
        if transition_sealed!=transition_wrapper:
            raise SystemExit(f"maintenance evidence stopped transition root differs: {node}")
        nodes.append({"node":node,"host":host,
            "transition_kind":"persistently-stopped-precommit",
            "transition_receipt":transition_sealed,
            "current_status":sealed(current,current_bytes,node,"stopped-current-status"),
            "persisted_head":sealed(persisted,persisted_bytes,node,"persisted-head")})
        continue
    stopped,stopped_bytes=locked(status_root/f"{node}-stopped-status.json",f"{node} stopped status")
    network,network_bytes=locked(quarantine_root/f"{node}-network-quarantine-receipt.json",
                                 f"{node} network quarantine receipt")
    status,status_bytes=locked(quarantine_root/f"{node}-status.json",f"{node} quarantine status")
    monitor,monitor_bytes=locked(quarantine_root/f"{node}-monitor.json",f"{node} quarantine monitor")
    post,post_bytes=locked(quarantine_root/f"{node}-post-proof-status.json",f"{node} post-proof status")
    external,external_bytes=locked(quarantine_root/f"{node}-external-proof.json",f"{node} external proof")
    cross,cross_bytes=locked(quarantine_root/f"{node}-public-cross-proof.json",f"{node} public cross proof")
    persisted,persisted_bytes=locked(persisted_root/f"{node}-persisted-head.json",f"{node} persisted head")
    identity=(capture_id,node,freeze_sha)
    if (stopped.get("schema")!="arc.recovery.offline-stop-status.v1"
            or (stopped.get("capture_id"),stopped.get("node"),stopped.get("freeze_plan_sha256"))!=identity
            or stopped.get("stopped") is not True or stopped.get("restart_fenced") is not True):
        raise SystemExit(f"maintenance evidence bundle stopped status differs: {node}")
    for value,label in ((status,"status"),(post,"post status")):
        if (value.get("schema")!="arc.recovery.legacy-network-quarantine-status.v1"
                or (value.get("capture_id"),value.get("node"),value.get("freeze_plan_sha256"))!=identity
                or value.get("active") is not True or value.get("enabled") is not True):
            raise SystemExit(f"maintenance evidence bundle quarantine {label} differs: {node}")
    if (network.get("schema")!="arc.recovery.legacy-network-quarantine.v1"
            or (network.get("capture_id"),network.get("node"),network.get("freeze_plan_sha256"))!=identity
            or digest(network_bytes)!=status.get("receipt_sha256")):
        raise SystemExit(f"maintenance evidence bundle network quarantine receipt differs: {node}")
    monitor_fields={"schema","capture_id","node","freeze_plan_sha256",
        "network_quarantine_receipt_sha256","monitor_contract_sha256",
        "semantic_interpreter","firewall_loader_inventory","file_sha256","unit",
        "legacy_exec_start_pre","incident_latched","continuous_fail_closed",
        "automatic_unfence","global_absence_claimed"}
    interpreter=monitor.get("semantic_interpreter")
    if (set(monitor)!=monitor_fields
            or monitor.get("schema")!="arc.recovery.legacy-network-quarantine-monitor.v1"
            or (monitor.get("capture_id"),monitor.get("node"),monitor.get("freeze_plan_sha256"))!=identity
            or monitor.get("network_quarantine_receipt_sha256")!=status.get("receipt_sha256")
            or monitor.get("incident_latched") is not False
            or monitor.get("continuous_fail_closed") is not True
            or monitor.get("automatic_unfence") is not False
            or monitor.get("global_absence_claimed") is not False
            or not isinstance(interpreter,dict)
            or set(interpreter)!={"normalized_path","sha256","device","inode","uid","gid",
                                  "mode","nlink","isolated","environment"}
            or interpreter.get("uid")!=0 or interpreter.get("gid")!=0
            or interpreter.get("mode")!=0o755 or interpreter.get("nlink")!=1
            or interpreter.get("isolated") is not True
            or interpreter.get("environment")!={"PATH":"/usr/bin:/bin","LC_ALL":"C",
                                                  "TZ":"UTC","PYTHONHASHSEED":"0"}
            or not isinstance(interpreter.get("normalized_path"),str)
            or re.fullmatch(r"/usr/bin/python3(?:\.[0-9]+)?",interpreter["normalized_path"]) is None
            or hash_re.fullmatch(str(interpreter.get("sha256"))) is None
            or any(isinstance(interpreter.get(field),bool) or not isinstance(interpreter.get(field),int)
                   or interpreter[field]<=0 for field in ("device","inode"))):
        raise SystemExit(f"maintenance evidence bundle quarantine monitor differs: {node}")
    if (external.get("schema")!="arc.recovery.legacy-network-quarantine-external-proof.v1"
            or (external.get("capture_id"),external.get("node"),external.get("freeze_plan_sha256"))!=identity
            or external.get("host")!=host or external.get("challenge")!=challenge
            or external.get("before_status_sha256")!=digest(status_bytes)
            or external.get("after_status_sha256")!=digest(canonical(external.get("after_status")))):
        raise SystemExit(f"maintenance evidence bundle external proof differs: {node}")
    if (cross.get("schema")!="arc.recovery.legacy-network-quarantine-public-cross-proof.v1"
            or (cross.get("capture_id"),cross.get("node"),cross.get("freeze_plan_sha256"))!=identity
            or cross.get("challenge")!=challenge
            or cross.get("quarantine_status_sha256")!=digest(canonical(cross.get("quarantine_status")))):
        raise SystemExit(f"maintenance evidence bundle public cross proof differs: {node}")
    if (persisted.get("schema")!="arc.recovery.persisted-legacy-head.v1"
            or (persisted.get("capture_id"),persisted.get("node"),persisted.get("freeze_plan_sha256"))!=identity
            or persisted.get("source_main_commit")!=source_commit
            or persisted.get("writer_stopped") is not True
            or persisted.get("restart_barrier_active") is not True
            or persisted.get("network_quarantine_active") is not True
            or persisted.get("global_absence_claimed") is not False):
        raise SystemExit(f"maintenance evidence bundle persisted head differs: {node}")
    nodes.append({"node":node,"host":host,
        "stopped_status":sealed(stopped,stopped_bytes,node,"stopped-status"),
        "network_quarantine_receipt":sealed(network,network_bytes,node,"network-quarantine-receipt"),
        "quarantine_status":sealed(status,status_bytes,node,"quarantine-status"),
        "quarantine_monitor":sealed(monitor,monitor_bytes,node,"network-quarantine-monitor"),
        "post_proof_quarantine_status":sealed(post,post_bytes,node,"post-proof-quarantine-status"),
        "external_quarantine_proof":sealed(external,external_bytes,node,"external-quarantine-proof"),
        "public_cross_proof":sealed(cross,cross_bytes,node,"public-cross-proof"),
        "persisted_head":sealed(persisted,persisted_bytes,node,"persisted-head")})
inventory_root=digest(canonical({"schema":"arc.recovery.legacy-maintenance-evidence-inventory.v1",
                                 "objects":inventory}))
value={"schema":"arc.recovery.legacy-maintenance-evidence-bundle.v1",
       "source_main_commit":source_commit,"freeze_plan_sha256":freeze_sha,"capture_id":capture_id,
       "first_quarantine_started_at":first_started,"all_controlled_stopped_at":all_stopped,
       "challenge":challenge,
       "live_observation_selection":observation_selection_sealed,
       "authenticated_prefence_height_cross_proof":authenticated_sealed,
       "quarantine_generation_ledger":ledger_sealed,
       "network_quarantine_challenge":challenge_sealed,
       "quarantine_stability_proof":stability_sealed,"nodes":nodes,
       "object_inventory":inventory,"aggregate_root_sha256":inventory_root}
payload=canonical(value);bundle_sha=digest(payload);sidecar_payload=f"{bundle_sha}  {output.name}\n".encode("ascii")
if (not output.is_absolute() or output.suffix!=".json" or os.path.normpath(os.fspath(output))!=os.fspath(output)
        or os.path.realpath(output)!=os.fspath(output)):
    raise SystemExit("maintenance evidence bundle output path is unsafe")
parent=output.parent;parent_details=parent.lstat()
if (parent.is_symlink() or not stat.S_ISDIR(parent_details.st_mode)
        or parent_details.st_uid not in {0,os.geteuid()} or parent_details.st_mode&0o022):
    raise SystemExit("maintenance evidence bundle output parent is unsafe")
dfd=os.open(parent,os.O_RDONLY|getattr(os,"O_DIRECTORY",0)|getattr(os,"O_NOFOLLOW",0))
def publish(path,data,label):
    partial=path.with_name(path.name+".partial")
    def read_name(name,modes):
        fd=os.open(name,os.O_RDONLY|getattr(os,"O_NOFOLLOW",0),dir_fd=dfd)
        try:
            details=os.fstat(fd)
            if (not stat.S_ISREG(details.st_mode) or details.st_uid not in {0,os.geteuid()}
                    or details.st_nlink!=1 or stat.S_IMODE(details.st_mode) not in modes
                    or details.st_size<=0 or details.st_size>32*1024*1024):
                raise SystemExit(f"{label} identity differs")
            chunks=[]
            while True:
                chunk=os.read(fd,1024*1024)
                if not chunk:break
                chunks.append(chunk)
            raw=b"".join(chunks)
            if len(raw)!=details.st_size:raise SystemExit(f"{label} changed while read")
            return raw
        finally:os.close(fd)
    if path.exists() or path.is_symlink():
        if read_name(path.name,{0o400})!=data:raise SystemExit(f"existing {label} differs")
        if partial.exists() or partial.is_symlink():
            read_name(partial.name,{0o400,0o600});os.unlink(partial.name,dir_fd=dfd);os.fsync(dfd)
        return
    promote=False
    if partial.exists() or partial.is_symlink():
        if read_name(partial.name,{0o400,0o600})==data:
            os.chmod(partial.name,0o400,dir_fd=dfd,follow_symlinks=False);promote=True
        else:os.unlink(partial.name,dir_fd=dfd);os.fsync(dfd)
    if not promote:
        fd=os.open(partial.name,os.O_WRONLY|os.O_CREAT|os.O_EXCL|getattr(os,"O_NOFOLLOW",0),0o600,dir_fd=dfd)
        with os.fdopen(fd,"wb") as handle:
            handle.write(data);handle.flush();os.fsync(handle.fileno());os.fchmod(handle.fileno(),0o400)
    os.rename(partial.name,path.name,src_dir_fd=dfd,dst_dir_fd=dfd);os.fsync(dfd)
try:
    publish(output,payload,"maintenance evidence bundle")
    publish(sidecar,sidecar_payload,"maintenance evidence bundle sidecar")
finally:os.close(dfd)
print(bundle_sha)
PY
}

create_legacy_maintenance_boundary() {
    local freeze_plan="$1" freeze_sha="$2" capture_id="$3"
    local public_receipt="$4" public_receipt_sha="$5" authenticated_cross="$6"
    local quarantine_root="$7" persisted_root="$8" output="$9"
    local first_quarantine_started_at="${10}" all_controlled_stopped_at="${11}"
    local inspector_binary_sha="${12}" genesis_sha="${13}"
    local validators_sha="${14}" legacy_validators_sha="${15}"
    local evidence_bundle="${16}"
    local helper_sha
    helper_sha="$(manifest_field "$freeze_plan" remote_helper_sha256)"
    python3 - "$freeze_plan" "$freeze_sha" "$capture_id" \
        "$public_receipt" "$public_receipt_sha" "$authenticated_cross" \
        "$quarantine_root" "$persisted_root" "$output" \
        "$first_quarantine_started_at" "$all_controlled_stopped_at" \
        "$helper_sha" "$inspector_binary_sha" "$genesis_sha" \
        "$validators_sha" "$legacy_validators_sha" "$evidence_bundle" \
        "$QUARANTINE_ROUND_MODULE" "${NODES[@]}" <<'PY'
import datetime
import hashlib
import json
import os
import pathlib
import re
import stat
import sys

(plan_raw, freeze_sha, capture_id, public_raw, public_sha, authenticated_raw,
 quarantine_root_raw, persisted_root_raw, output_raw, first_quarantine_started_at,
 all_controlled_stopped_at, helper_sha, inspector_sha, genesis_sha, validators_sha,
 legacy_validators_sha, evidence_bundle_raw, rounds_module_raw, *fleet_raw) = sys.argv[1:]
plan_path=pathlib.Path(plan_raw);public_path=pathlib.Path(public_raw)
authenticated_path=pathlib.Path(authenticated_raw);quarantine_root=pathlib.Path(quarantine_root_raw)
persisted_root=pathlib.Path(persisted_root_raw);output=pathlib.Path(output_raw)
evidence_bundle_path=pathlib.Path(evidence_bundle_raw)
sidecar=output.with_name(output.name+".sha256")
fleet=[tuple(row.split("=",1)) for row in fleet_raw]
expected_nodes=[("nyc","149.28.32.76"),("lax","140.82.16.112"),
                ("ams","136.244.109.1"),("lhr","104.238.171.11"),
                ("nrt","202.182.107.41"),("sgp","149.28.153.31")]
hash_re=re.compile(r"[0-9a-f]{64}");commit_re=re.compile(r"[0-9a-f]{40}")
utc_re=re.compile(r"\d{4}-\d{2}-\d{2}T\d{2}:\d{2}:\d{2}Z")
canonical=lambda value:(json.dumps(value,sort_keys=True,separators=(",",":"))+"\n").encode()
digest=lambda raw:hashlib.sha256(raw).hexdigest()

if fleet!=expected_nodes:
    raise SystemExit("maintenance-boundary fleet differs from the fixed official six")
for value,label in ((freeze_sha,"freeze"),(capture_id,"capture"),(public_sha,"public receipt"),
                    (helper_sha,"helper"),(inspector_sha,"inspector"),(genesis_sha,"genesis"),
                    (validators_sha,"validators"),(legacy_validators_sha,"legacy validators")):
    if hash_re.fullmatch(value) is None: raise SystemExit(f"maintenance-boundary {label} hash is malformed")
if any(utc_re.fullmatch(value) is None for value in
       (first_quarantine_started_at,all_controlled_stopped_at)):
    raise SystemExit("maintenance-boundary timestamps are not canonical UTC")
first=datetime.datetime.strptime(first_quarantine_started_at,"%Y-%m-%dT%H:%M:%SZ")
stopped=datetime.datetime.strptime(all_controlled_stopped_at,"%Y-%m-%dT%H:%M:%SZ")
if first>stopped: raise SystemExit("maintenance-boundary timestamps are reversed")

def locked(path,label,maximum=32*1024*1024):
    path=pathlib.Path(path);flags=os.O_RDONLY|getattr(os,"O_NOFOLLOW",0)|getattr(os,"O_CLOEXEC",0)
    fd=os.open(path,flags)
    try:
        before=os.fstat(fd);visible=os.lstat(path)
        identity=lambda value:(value.st_dev,value.st_ino,value.st_mode,value.st_uid,value.st_gid,
                               value.st_nlink,value.st_size,value.st_mtime_ns,value.st_ctime_ns)
        if (not stat.S_ISREG(before.st_mode) or stat.S_ISLNK(visible.st_mode)
                or identity(before)!=identity(visible) or before.st_nlink!=1
                or before.st_uid not in {0,os.geteuid()} or before.st_mode&0o022
                or before.st_size<=0 or before.st_size>maximum):
            raise SystemExit(f"maintenance-boundary {label} is unsafe")
        chunks=[];remaining=maximum+1
        while remaining:
            chunk=os.read(fd,min(1024*1024,remaining))
            if not chunk: break
            chunks.append(chunk);remaining-=len(chunk)
        raw=b"".join(chunks);after=os.fstat(fd)
        if identity(before)!=identity(after) or len(raw)!=before.st_size:
            raise SystemExit(f"maintenance-boundary {label} changed while read")
    finally: os.close(fd)
    try:value=json.loads(raw)
    except (UnicodeDecodeError,json.JSONDecodeError) as error:
        raise SystemExit(f"maintenance-boundary {label} is invalid JSON") from error
    if raw!=canonical(value): raise SystemExit(f"maintenance-boundary {label} is noncanonical")
    return value,raw

plan,plan_bytes=locked(plan_path,"freeze plan")
public,public_bytes=locked(public_path,"public height receipt")
authenticated,authenticated_bytes=locked(authenticated_path,"authenticated height cross-proof")
evidence_bundle,evidence_bundle_bytes=locked(evidence_bundle_path,"maintenance evidence bundle")
source_commit=plan.get("source_commit")
if (digest(plan_bytes)!=freeze_sha or plan.get("schema")!="arc.recovery.freeze-plan.v5"
        or commit_re.fullmatch(str(source_commit)) is None or plan.get("remote_helper_sha256")!=helper_sha):
    raise SystemExit("maintenance-boundary freeze plan identity differs")
for field in ("orchestrator_sha256","rollout_tool_sha256","rollout_schema_sha256"):
    if hash_re.fullmatch(str(plan.get(field))) is None:
        raise SystemExit(f"maintenance-boundary freeze plan {field} is malformed")
expected_capture=hashlib.sha256(b"ARC recovery capture v2\0"+bytes.fromhex(freeze_sha)).hexdigest()
if capture_id!=expected_capture: raise SystemExit("maintenance-boundary capture derivation differs")
public_fields={"schema","source_main_commit","freeze_plan_sha256","capture_id","started_at",
               "completed_at","duration_ms","request_policy","origins","legacy_public_max_height"}
if (set(public)!=public_fields or digest(public_bytes)!=public_sha
        or public.get("schema")!="arc.recovery.legacy-public-height.v1"
        or (public.get("source_main_commit"),public.get("freeze_plan_sha256"),public.get("capture_id"))
            !=(source_commit,freeze_sha,capture_id)):
    raise SystemExit("maintenance-boundary public receipt identity differs")
origins=public.get("origins")
expected_origins=[(node,f"http://{host}:9090") for node,host in fleet]
if (not isinstance(origins,list)
        or [(row.get("name"),row.get("origin")) for row in origins]!=expected_origins):
    raise SystemExit("maintenance-boundary official public origin set differs")
if (not isinstance(public.get("legacy_public_max_height"),int)
        or isinstance(public["legacy_public_max_height"],bool)
        or public["legacy_public_max_height"]!=max(row.get("info_after_height",-1) for row in origins)):
    raise SystemExit("maintenance-boundary public maximum differs")
if (authenticated.get("schema")!="arc.recovery.authenticated-legacy-height-fleet.v1"
        or (authenticated.get("source_main_commit"),authenticated.get("freeze_plan_sha256"),
            authenticated.get("capture_id"),authenticated.get("legacy_public_height_receipt_sha256"))
            !=(source_commit,freeze_sha,capture_id,public_sha)):
    raise SystemExit("maintenance-boundary authenticated cross-proof differs")
if (evidence_bundle.get("schema")!="arc.recovery.legacy-maintenance-evidence-bundle.v1"
        or (evidence_bundle.get("source_main_commit"),evidence_bundle.get("freeze_plan_sha256"),
            evidence_bundle.get("capture_id"))!=(source_commit,freeze_sha,capture_id)
        or evidence_bundle.get("first_quarantine_started_at")!=first_quarantine_started_at
        or evidence_bundle.get("all_controlled_stopped_at")!=all_controlled_stopped_at
        or evidence_bundle.get("authenticated_prefence_height_cross_proof",{}).get("value")!=authenticated
        or evidence_bundle.get("authenticated_prefence_height_cross_proof",{}).get("sha256")!=digest(authenticated_bytes)):
    raise SystemExit("maintenance-boundary evidence bundle identity differs")
observation_selection_sealed=evidence_bundle.get("live_observation_selection")
if (not isinstance(observation_selection_sealed,dict)
        or set(observation_selection_sealed)!={"value","sha256"}
        or not isinstance(observation_selection_sealed.get("value"),dict)
        or digest(canonical(observation_selection_sealed["value"]))
            !=observation_selection_sealed.get("sha256")
        or observation_selection_sealed["value"].get("schema")
            !="arc.recovery.legacy-live-observation-selection.v1"
        or (observation_selection_sealed["value"].get("source_main_commit"),
            observation_selection_sealed["value"].get("freeze_plan_sha256"),
            observation_selection_sealed["value"].get("capture_id"))
            !=(source_commit,freeze_sha,capture_id)):
    raise SystemExit("maintenance-boundary live-observation selection seal differs")
stability_sealed=evidence_bundle.get("quarantine_stability_proof")
if (not isinstance(stability_sealed,dict) or set(stability_sealed)!={"value","sha256"}
        or not isinstance(stability_sealed.get("value"),dict)
        or digest(canonical(stability_sealed["value"]))!=stability_sealed.get("sha256")):
    raise SystemExit("maintenance-boundary stability proof seal differs")
ledger_sealed=evidence_bundle.get("quarantine_generation_ledger")
if (not isinstance(ledger_sealed,dict) or set(ledger_sealed)!={"value","sha256"}
        or not isinstance(ledger_sealed.get("value"),dict)
        or digest(canonical(ledger_sealed["value"]))!=ledger_sealed.get("sha256")):
    raise SystemExit("maintenance-boundary generation ledger seal differs")
import importlib.util
spec=importlib.util.spec_from_file_location("arc_quarantine_rounds",rounds_module_raw)
if spec is None or spec.loader is None:
    raise SystemExit("maintenance-boundary cannot load quarantine-round validator")
rounds=importlib.util.module_from_spec(spec);spec.loader.exec_module(rounds)
try:ledger_state=rounds.validate_generation_ledger(ledger_sealed["value"])
except rounds.QuarantineRoundError as error:
    raise SystemExit(f"maintenance-boundary generation ledger differs: {error}") from error
if ((ledger_state["freeze_plan_sha256"],ledger_state["capture_id"])
        !=(freeze_sha,capture_id)
        or (ledger_state["live_observation_selection_sha256"],
            ledger_state["live_observation_generation"],
            ledger_state["observation_generation_receipt_sha256"],
            ledger_state["drive_prefreeze_receipt_sha256"])
            !=(observation_selection_sealed["sha256"],
               observation_selection_sealed["value"].get("observation_generation"),
               observation_selection_sealed["value"].get(
                   "observation_generation_receipt_sha256"),
               observation_selection_sealed["value"].get(
                   "drive_prefreeze_receipt_sha256"))
        or ledger_sealed["value"].get("first_secured_at")
            !=first_quarantine_started_at):
    raise SystemExit("maintenance-boundary generation ledger identity differs")
transition_wrappers={}
transition_projections={}
for round_wrapper in ledger_sealed["value"]["rounds"]:
    for wrapper in round_wrapper["result"]["value"]["transitions"]:
        transition=wrapper["value"];name=transition["node"]
        if name in transition_wrappers:raise SystemExit("maintenance-boundary repeats a transition")
        transition_wrappers[name]=wrapper
        transition_projections[name]=rounds.validate_node_transition(transition)
if set(transition_wrappers)!={name for name,_host in fleet}:
    raise SystemExit("maintenance-boundary transition partition differs")
active_fleet=[row for row in fleet if transition_projections[row[0]]["kind"]
              ==rounds.ACTIVE_TRANSITION_KIND]
active_roots=[{"node":node,"sha256":transition_wrappers[node]["sha256"]}
              for node,_host in active_fleet]
stability=stability_sealed["value"]
if (stability.get("schema")!="arc.recovery.legacy-network-quarantine-stability.v1"
        or (stability.get("source_main_commit"),stability.get("freeze_plan_sha256"),
            stability.get("capture_id"))!=(source_commit,freeze_sha,capture_id)
        or stability.get("quarantine_generation_ledger_sha256")!=ledger_sealed["sha256"]
        or stability.get("active_transition_sha256s")!=active_roots
        or stability.get("interval_seconds")!=(120 if active_fleet else 0)
        or stability.get("sample_count")!=(2 if active_fleet else 0)
        or isinstance(stability.get("monotonic_elapsed_ns"),bool)
        or not isinstance(stability.get("monotonic_elapsed_ns"),int)
        or (active_fleet and stability["monotonic_elapsed_ns"]<120_000_000_000)
        or (not active_fleet and stability["monotonic_elapsed_ns"]!=0)
        or stability.get("global_absence_claimed") is not False):
    raise SystemExit("maintenance-boundary stability proof differs")
stability_rows=stability.get("nodes")
if (not isinstance(stability_rows,list)
        or [(row.get("node"),row.get("host")) for row in stability_rows]!=active_fleet):
    raise SystemExit("maintenance-boundary stability topology differs")
bundle_rows=evidence_bundle.get("nodes")
if (not isinstance(bundle_rows,list)
        or [(row.get("node"),row.get("host")) for row in bundle_rows]!=fleet):
    raise SystemExit("maintenance-boundary evidence bundle topology differs")
authenticated_rows=authenticated.get("nodes")
if (not isinstance(authenticated_rows,list)
        or [(row.get("node"),row.get("host")) for row in authenticated_rows]!=fleet):
    raise SystemExit("maintenance-boundary authenticated topology differs")

def exact_tuple(value,label):
    if (not isinstance(value,dict)
            or set(value) not in ({"height","block_hash","state_root"},
                                  {"height","block_hash","state_root","response_sha256"})
            or isinstance(value.get("height"),bool) or not isinstance(value.get("height"),int)
            or value["height"]<0 or hash_re.fullmatch(str(value.get("block_hash"))) is None
            or hash_re.fullmatch(str(value.get("state_root"))) is None
            or ("response_sha256" in value
                and hash_re.fullmatch(str(value.get("response_sha256"))) is None)):
        raise SystemExit(f"maintenance-boundary {label} tuple is malformed")
    return {key:value[key] for key in ("height","block_hash","state_root")}

rows=[];evidence_heights=[];challenge=stability.get("challenge")
if hash_re.fullmatch(str(challenge)) is None:
    raise SystemExit("maintenance-boundary quarantine challenge is malformed")
stability_by={row["node"]:row for row in stability_rows}
for (node,host),origin,authenticated_row in zip(fleet,origins,authenticated_rows):
    projection=transition_projections[node]
    transition_wrapper=transition_wrappers[node]
    bundle_row=next(row for row in bundle_rows if row.get("node")==node)
    persisted,persisted_bytes=locked(
        persisted_root/f"{node}-persisted-head.json",f"{node} persisted head"
    )
    if projection["kind"]==rounds.STOPPED_PRECOMMIT_TRANSITION_KIND:
        if set(bundle_row)!={"node","host","transition_kind","transition_receipt",
                            "current_status","persisted_head"}:
            raise SystemExit(f"maintenance-boundary stopped evidence fields differ: {node}")
        if (bundle_row.get("transition_kind")!="persistently-stopped-precommit"
                or bundle_row.get("transition_receipt")!=transition_wrapper
                or bundle_row.get("persisted_head",{}).get("value")!=persisted
                or bundle_row.get("persisted_head",{}).get("sha256")!=digest(persisted_bytes)
                or persisted.get("source_pair_role")!="preauthorization-boundary"
                or persisted.get("head")!=projection["stable_head"]):
            raise SystemExit(f"maintenance-boundary stopped transition binding differs: {node}")
        current=bundle_row.get("current_status",{})
        if (not isinstance(current,dict) or set(current)!={"value","sha256"}
                or digest(canonical(current.get("value")))!=current.get("sha256")
                or current.get("value",{}).get("node_transition_receipt_sha256")
                    !=transition_wrapper["sha256"]):
            raise SystemExit(f"maintenance-boundary stopped current status differs: {node}")
        persisted_tuple=exact_tuple(persisted.get("head"),f"{node} persisted stopped")
        auth_proof=authenticated_row.get("proof");auth_sha=authenticated_row.get("proof_sha256")
        if (not isinstance(auth_proof,dict) or digest(canonical(auth_proof))!=auth_sha
                or auth_proof.get("node")!=node
                or auth_proof.get("public_info_after_height")!=origin["info_after_height"]):
            raise SystemExit(f"maintenance-boundary stopped authenticated row differs: {node}")
        height_sources=(
            ("public_info_before",origin["info_before_height"],public_sha),
            ("public_latest",origin["latest_block_height"],public_sha),
            ("public_info_after",origin["info_after_height"],public_sha),
            ("authenticated_info_before",auth_proof["authenticated_info_before_height"],auth_sha),
            ("authenticated_latest",auth_proof["authenticated_latest_block_height"],auth_sha),
            ("authenticated_info_after",auth_proof["authenticated_info_after_height"],auth_sha),
            ("stopped_transition_head",persisted_tuple["height"],transition_wrapper["sha256"]),
            ("final_persisted_head",persisted_tuple["height"],digest(persisted_bytes)),
        )
        for label,height,evidence_sha in height_sources:
            if (isinstance(height,bool) or not isinstance(height,int) or height<0
                    or hash_re.fullmatch(str(evidence_sha)) is None):
                raise SystemExit(f"maintenance-boundary stopped evidence height differs: {node}/{label}")
            evidence_heights.append({"node":node,"label":label,"height":height,
                                     "evidence_sha256":evidence_sha})
        rows.append({"node":node,"host":host,"origin":origin["origin"],
            "transition_kind":"persistently-stopped-precommit",
            "transition_receipt_sha256":transition_wrapper["sha256"],
            "final_persisted_head":{"tuple":persisted_tuple,
                                      "evidence_sha256":digest(persisted_bytes)}})
        continue
    stability_row=stability_by[node]
    status,status_bytes=locked(quarantine_root/f"{node}-status.json",f"{node} quarantine status")
    post_status,post_status_bytes=locked(
        quarantine_root/f"{node}-post-proof-status.json",f"{node} post-proof quarantine status")
    external,external_bytes=locked(quarantine_root/f"{node}-external-proof.json",f"{node} external proof")
    cross,cross_bytes=locked(quarantine_root/f"{node}-public-cross-proof.json",f"{node} public cross-proof")
    for field,value,raw in (
        ("quarantine_status",status,status_bytes),
        ("post_proof_quarantine_status",post_status,post_status_bytes),
        ("external_quarantine_proof",external,external_bytes),
        ("public_cross_proof",cross,cross_bytes),
        ("persisted_head",persisted,persisted_bytes),
    ):
        sealed=bundle_row.get(field)
        if (not isinstance(sealed,dict) or set(sealed)!={"value","sha256"}
                or sealed.get("value")!=value or sealed.get("sha256")!=digest(raw)):
            raise SystemExit(f"maintenance-boundary evidence bundle object differs: {node}/{field}")
    if challenge is None:challenge=cross.get("challenge")
    if hash_re.fullmatch(str(challenge)) is None:
        raise SystemExit("maintenance-boundary quarantine challenge is malformed")
    if stability.get("challenge")!=challenge:
        raise SystemExit("maintenance-boundary stability challenge differs")
    identity=(capture_id,node,freeze_sha)
    status_fields={"schema","capture_id","node","freeze_plan_sha256","receipt_sha256",
        "table","rule_counters","counter_snapshot_sha256","owned_ruleset_stateless_sha256",
        "listener_inventory","loopback_head","quarantine_policy","active","enabled"}
    if (set(status)!=status_fields
            or status.get("schema")!="arc.recovery.legacy-network-quarantine-status.v1"
            or (status.get("capture_id"),status.get("node"),status.get("freeze_plan_sha256"))!=identity
            or status.get("active") is not True or status.get("enabled") is not True):
        raise SystemExit(f"maintenance-boundary quarantine status differs: {node}")
    if (set(post_status)!=status_fields
            or post_status.get("schema")!="arc.recovery.legacy-network-quarantine-status.v1"
            or (post_status.get("capture_id"),post_status.get("node"),
                post_status.get("freeze_plan_sha256"))!=identity
            or post_status.get("receipt_sha256")!=status.get("receipt_sha256")
            or post_status.get("active") is not True or post_status.get("enabled") is not True):
        raise SystemExit(f"maintenance-boundary post-proof quarantine status differs: {node}")
    external_fields={"schema","capture_id","node","host","freeze_plan_sha256","challenge",
        "started_at","completed_at","operator_source_address","listener_inventory","targets",
        "results","network_quarantine_receipt_sha256","before_status_sha256",
        "after_status_sha256","after_status","deny_counter","ssh_status_reproved",
        "global_absence_claimed"}
    if (set(external)!=external_fields
            or external.get("schema")!="arc.recovery.legacy-network-quarantine-external-proof.v1"
            or (external.get("capture_id"),external.get("node"),external.get("freeze_plan_sha256"))!=identity
            or external.get("host")!=host or external.get("challenge")!=challenge
            or external.get("network_quarantine_receipt_sha256")!=status.get("receipt_sha256")
            or external.get("before_status_sha256")!=digest(status_bytes)
            or external.get("ssh_status_reproved") is not True
            or external.get("global_absence_claimed") is not False):
        raise SystemExit(f"maintenance-boundary external quarantine proof differs: {node}")
    external_after=external.get("after_status")
    if (not isinstance(external_after,dict) or set(external_after)!=status_fields
            or digest(canonical(external_after))!=external.get("after_status_sha256")
            or external_after.get("receipt_sha256")!=status.get("receipt_sha256")
            or (external_after.get("capture_id"),external_after.get("node"),
                external_after.get("freeze_plan_sha256"))!=identity
            or external_after.get("active") is not True or external_after.get("enabled") is not True):
        raise SystemExit(f"maintenance-boundary external after-status differs: {node}")
    if (cross.get("schema")!="arc.recovery.legacy-network-quarantine-public-cross-proof.v1"
            or set(cross)!={"schema","capture_id","node","freeze_plan_sha256","challenge",
                "network_quarantine_receipt_sha256","quarantine_status_sha256",
                "quarantine_status","rule_counters","public_info_after_block",
                "public_latest_block","fenced_head","fenced_head_covers_public_info_after",
                "public_latest_hash_matches","global_absence_claimed"}
            or (cross.get("capture_id"),cross.get("node"),cross.get("freeze_plan_sha256"))!=identity
            or cross.get("challenge")!=challenge
            or cross.get("network_quarantine_receipt_sha256")!=status.get("receipt_sha256")
            or cross.get("quarantine_status_sha256")!=digest(canonical(cross.get("quarantine_status")))
            or cross.get("rule_counters")!=cross.get("quarantine_status",{}).get("rule_counters")
            or cross.get("fenced_head_covers_public_info_after") is not True
            or cross.get("public_latest_hash_matches") is not True
            or cross.get("global_absence_claimed") is not False):
        raise SystemExit(f"maintenance-boundary public cross-proof differs: {node}")
    public_tuple=exact_tuple(cross.get("public_info_after_block"),f"{node} public")
    public_latest=exact_tuple(cross.get("public_latest_block"),f"{node} public latest")
    fenced_tuple=exact_tuple(cross.get("fenced_head"),f"{node} post-quarantine")
    initial_head=status.get("loopback_head",{})
    initial_fenced_tuple=exact_tuple({"height":initial_head.get("latest_height"),
        "block_hash":initial_head.get("block_hash"),"state_root":initial_head.get("state_root")},
        f"{node} initial post-quarantine")
    if (public_tuple["height"]!=origin.get("info_after_height")
            or public_latest["height"]!=origin.get("latest_block_height")
            or public_latest["block_hash"]!=origin.get("latest_block_hash")
            or fenced_tuple["height"]<public_tuple["height"]):
        raise SystemExit(f"maintenance-boundary public/post-quarantine tuple differs: {node}")
    if (persisted.get("schema")!="arc.recovery.persisted-legacy-head.v1"
            or (persisted.get("capture_id"),persisted.get("node"),persisted.get("freeze_plan_sha256"))!=identity
            or persisted.get("source_main_commit")!=source_commit
            or persisted.get("boot_id")!=next(row["boot_id"] for row in plan["nodes"] if row["name"]==node)
            or persisted.get("inspector_binary_sha256")!=inspector_sha
            or persisted.get("genesis_sha256")!=genesis_sha
            or persisted.get("validator_public_keys_sha256")!=validators_sha
            or persisted.get("legacy_validator_set_sha256")!=legacy_validators_sha
            or persisted.get("network_quarantine_receipt_sha256")!=status.get("receipt_sha256")
            or persisted.get("source_pair_role")!="post-quarantine-final-export"
            or persisted.get("selected_source_head")!=persisted.get("head")
            or hash_re.fullmatch(str(persisted.get("final_source_capture_sha256"))) is None
            or persisted.get("export_status")!="EXPORTED_UNSIGNED"):
        raise SystemExit(f"maintenance-boundary persisted head identity differs: {node}")
    persisted_tuple=exact_tuple(persisted.get("head"),f"{node} persisted")
    if persisted_tuple["height"]<fenced_tuple["height"]:
        raise SystemExit(f"maintenance-boundary persisted head precedes post-quarantine head: {node}")
    if persisted_tuple["height"]==fenced_tuple["height"] and persisted_tuple!=fenced_tuple:
        raise SystemExit(f"maintenance-boundary same-height persisted/fenced tuple differs: {node}")
    auth_proof=authenticated_row.get("proof");auth_sha=authenticated_row.get("proof_sha256")
    auth_fields={"schema","capture_id","node","freeze_plan_sha256","challenge","rpc_origin",
        "writer_pid","writer_start_ticks","boot_id","executable_sha256","argv_sha256",
        "started_at","completed_at","public_info_before_height","public_latest_block_height",
        "public_info_after_height","public_latest_block_hash","authenticated_info_before_height",
        "authenticated_latest_block_height","authenticated_info_after_height",
        "authenticated_latest_block_hash","authenticated_info_before_body_sha256",
        "authenticated_latest_block_body_sha256","authenticated_info_after_body_sha256",
        "conservative_height_floor"}
    if (not isinstance(auth_proof,dict) or set(auth_proof)!=auth_fields
            or auth_proof.get("schema")!="arc.recovery.authenticated-legacy-height-bracket.v1"
            or digest(canonical(auth_proof))!=auth_sha
            or auth_proof.get("node")!=node
            or auth_proof.get("challenge")!=authenticated.get("challenge")
            or auth_proof.get("public_info_after_height")!=origin["info_after_height"]):
        raise SystemExit(f"maintenance-boundary authenticated row differs: {node}")
    stability_samples=stability_row.get("samples")
    if not isinstance(stability_samples,list) or len(stability_samples)!=2:
        raise SystemExit(f"maintenance-boundary stability samples differ: {node}")
    stability_tuples=[]
    for stability_index,sealed_sample in enumerate(stability_samples):
        if (not isinstance(sealed_sample,dict) or set(sealed_sample)!={"value","sha256"}
                or not isinstance(sealed_sample.get("value"),dict)
                or digest(canonical(sealed_sample["value"]))!=sealed_sample.get("sha256")):
            raise SystemExit(f"maintenance-boundary sealed stability sample differs: {node}/{stability_index}")
        sample=sealed_sample["value"]
        if ((sample.get("capture_id"),sample.get("node"),sample.get("freeze_plan_sha256"),
             sample.get("challenge"),sample.get("sample_index"))
                !=(capture_id,node,freeze_sha,stability.get("challenge"),stability_index)):
            raise SystemExit(f"maintenance-boundary stability sample identity differs: {node}/{stability_index}")
        sample_head=sample.get("head",{})
        stability_tuples.append(exact_tuple(
            {key:sample_head.get(key) for key in ("height","block_hash","state_root")},
            f"{node} quarantine stability sample {stability_index}"))
    if stability_tuples[0]!=stability_tuples[1]:
        raise SystemExit(f"maintenance-boundary quarantine head changed over drain: {node}")
    height_sources=(
        ("public_info_before",origin["info_before_height"],public_sha),
        ("public_latest",origin["latest_block_height"],public_sha),
        ("public_info_after",origin["info_after_height"],public_sha),
        ("authenticated_info_before",auth_proof["authenticated_info_before_height"],auth_sha),
        ("authenticated_latest",auth_proof["authenticated_latest_block_height"],auth_sha),
        ("authenticated_info_after",auth_proof["authenticated_info_after_height"],auth_sha),
        ("authenticated_conservative_floor",auth_proof["conservative_height_floor"],auth_sha),
        ("initial_post_quarantine_head",initial_fenced_tuple["height"],digest(status_bytes)),
        ("public_cross_info_after",public_tuple["height"],digest(cross_bytes)),
        ("post_quarantine_head",fenced_tuple["height"],digest(cross_bytes)),
        ("quarantine_stability_sample_0",stability_tuples[0]["height"],
         stability_samples[0]["sha256"]),
        ("quarantine_stability_sample_1",stability_tuples[1]["height"],
         stability_samples[1]["sha256"]),
        ("final_persisted_head",persisted_tuple["height"],digest(persisted_bytes)),
    )
    for label,height,evidence_sha in height_sources:
        if isinstance(height,bool) or not isinstance(height,int) or height<0 or hash_re.fullmatch(evidence_sha) is None:
            raise SystemExit(f"maintenance-boundary evidence height differs: {node}/{label}")
        evidence_heights.append({"node":node,"label":label,"height":height,
                                 "evidence_sha256":evidence_sha})
    rows.append({
        "node":node,"host":host,"origin":origin["origin"],
        "public_observation":{"tuple":public_tuple,"evidence_sha256":digest(cross_bytes)},
        "authenticated_prefence_proof_sha256":auth_sha,
        "network_quarantine_receipt_sha256":status["receipt_sha256"],
        "quarantine_status_sha256":digest(status_bytes),
        "post_proof_quarantine_status_sha256":digest(post_status_bytes),
        "external_quarantine_proof_sha256":digest(external_bytes),
        "public_cross_proof_sha256":digest(cross_bytes),
        "initial_post_quarantine_head":{"tuple":initial_fenced_tuple,
            "evidence_sha256":digest(status_bytes)},
        "post_quarantine_head":{"tuple":fenced_tuple,"evidence_sha256":digest(cross_bytes)},
        "final_persisted_head":{"tuple":persisted_tuple,"evidence_sha256":digest(persisted_bytes)},
    })

observed_cutoff=max(row["height"] for row in evidence_heights)
margin=128
existing_boundary=None
if output.exists() or output.is_symlink() or sidecar.exists() or sidecar.is_symlink():
    if not output.exists():
        raise SystemExit("maintenance-boundary sidecar exists without its ordered primary file")
    existing_boundary,existing_payload=locked(output,"existing maintenance boundary")
    created_at=existing_boundary.get("created_at")
    if utc_re.fullmatch(str(created_at)) is None:
        raise SystemExit("existing maintenance-boundary timestamp is malformed")
else:
    created_at=datetime.datetime.now(datetime.timezone.utc).strftime("%Y-%m-%dT%H:%M:%SZ")
value={
    "schema":"arc.recovery.legacy-maintenance-boundary.v1",
    "source_main_commit":source_commit,"freeze_plan_sha256":freeze_sha,"capture_id":capture_id,
    "first_quarantine_started_at":first_quarantine_started_at,
    "all_controlled_stopped_at":all_controlled_stopped_at,
    "created_at":created_at,
    "official_origin_scope":{"global_absence_claimed":False,
        "origins":[{"node":node,"host":host,"origin":origin["origin"]}
                   for (node,host),origin in zip(fleet,origins)]},
    "legacy_public_height_receipt":{"schema":public["schema"],"sha256":public_sha,
        "completed_at":public["completed_at"],"observed_max_height":public["legacy_public_max_height"]},
    "authenticated_prefence_height_cross_proof_sha256":digest(authenticated_bytes),
    "legacy_live_observation_selection_sha256":observation_selection_sealed["sha256"],
    "legacy_live_observation_generation":observation_selection_sealed["value"]["observation_generation"],
    "observation_generation_receipt_sha256":
        observation_selection_sealed["value"]["observation_generation_receipt_sha256"],
    "drive_prefreeze_receipt_sha256":
        observation_selection_sealed["value"]["drive_prefreeze_receipt_sha256"],
    "quarantine_generation_ledger_sha256":ledger_sealed["sha256"],
    "legacy_maintenance_evidence_bundle_sha256":digest(evidence_bundle_bytes),
    "network_quarantine_stability_proof_sha256":stability_sealed["sha256"],
    "network_quarantine_challenge":challenge,
    "tools":{"remote_helper_sha256":helper_sha,"inspector_binary_sha256":inspector_sha,
        "genesis_sha256":genesis_sha,"validator_public_keys_sha256":validators_sha,
        "legacy_validator_set_sha256":legacy_validators_sha,
        "orchestrator_sha256":plan["orchestrator_sha256"],
        "rollout_tool_sha256":plan["rollout_tool_sha256"],
        "rollout_schema_sha256":plan["rollout_schema_sha256"]},
    "nodes":rows,"evidence_heights":evidence_heights,"observed_cutoff_height":observed_cutoff,
    "continuity_safety_margin":margin,
    "continuity_safety_margin_policy":{"prune_depth":100,"commit_rule_rounds":2,
        "operational_headroom":26,"cryptographic_global_absence_proof":False},
    "legacy_public_max_height":observed_cutoff+margin,"global_absence_claimed":False,
    "reopening_policy":{"required_validator_count":6,
        "height_relation":"strictly-greater-than-legacy_public_max_height",
        "required_equal_fields":["block_hash","state_root"]},
    "late_fork_circuit":{"monitor_scope":"retired-and-community-legacy-sources",
        "trigger":"self-consistent-legacy-fork-candidate-above-observed-cutoff-height",
        "action":"enter-maintenance-preserve-and-offline-validate",
        "rewrite_v3_history_allowed":False},
    "threat_model":{"trusted_host_root_required":True,
        "sealed_reviewed_legacy_binary_non_adversarial":True,
        "quarantine_purpose":"operational-network-isolation",
        "hostile_root_containment_claimed":False},
}
if (value["continuity_safety_margin_policy"]["prune_depth"]
        +value["continuity_safety_margin_policy"]["commit_rule_rounds"]
        +value["continuity_safety_margin_policy"]["operational_headroom"]!=margin):
    raise SystemExit("maintenance-boundary safety margin arithmetic differs")
payload=canonical(value);receipt_sha=digest(payload)
if (not output.is_absolute() or output.suffix!=".json"
        or os.path.normpath(os.fspath(output))!=os.fspath(output)
        or os.path.realpath(output)!=os.fspath(output)):
    raise SystemExit("maintenance-boundary output path is unsafe")
for parent in (output.parent,*output.parents):
    if parent==pathlib.Path("/"):break
    details=parent.lstat()
    if (parent.is_symlink() or not stat.S_ISDIR(details.st_mode)
            or details.st_uid not in {0,os.geteuid()} or details.st_mode&0o022):
        raise SystemExit("maintenance-boundary output ancestry is unsafe")
if existing_boundary is not None:
    if existing_payload!=payload:
        raise SystemExit("existing maintenance-boundary terminal files differ")
dfd=os.open(output.parent,os.O_RDONLY|getattr(os,"O_DIRECTORY",0)|getattr(os,"O_NOFOLLOW",0))
def publish(path,data,label):
    partial=path.with_name(path.name+".partial")
    def read_name(name,modes):
        fd=os.open(name,os.O_RDONLY|getattr(os,"O_NOFOLLOW",0),dir_fd=dfd)
        try:
            details=os.fstat(fd)
            if (not stat.S_ISREG(details.st_mode) or details.st_nlink!=1
                    or details.st_uid not in {0,os.geteuid()}
                    or stat.S_IMODE(details.st_mode) not in modes
                    or details.st_size<=0 or details.st_size>32*1024*1024):
                raise SystemExit(f"{label} identity differs")
            chunks=[]
            while True:
                chunk=os.read(fd,1024*1024)
                if not chunk:break
                chunks.append(chunk)
            raw=b"".join(chunks)
            if len(raw)!=details.st_size:raise SystemExit(f"{label} changed while read")
            return raw
        finally:os.close(fd)
    if path.exists() or path.is_symlink():
        if read_name(path.name,{0o400})!=data:raise SystemExit(f"existing {label} differs")
        if partial.exists() or partial.is_symlink():
            read_name(partial.name,{0o400,0o600});os.unlink(partial.name,dir_fd=dfd);os.fsync(dfd)
        return
    promote=False
    if partial.exists() or partial.is_symlink():
        if read_name(partial.name,{0o400,0o600})==data:
            os.chmod(partial.name,0o400,dir_fd=dfd,follow_symlinks=False);promote=True
        else:
            os.unlink(partial.name,dir_fd=dfd);os.fsync(dfd)
    if not promote:
        fd=os.open(partial.name,os.O_WRONLY|os.O_CREAT|os.O_EXCL|getattr(os,"O_NOFOLLOW",0),
                   0o600,dir_fd=dfd)
        with os.fdopen(fd,"wb") as handle:
            handle.write(data);handle.flush();os.fsync(handle.fileno());os.fchmod(handle.fileno(),0o400)
    os.rename(partial.name,path.name,src_dir_fd=dfd,dst_dir_fd=dfd);os.fsync(dfd)
try:
    publish(output,payload,"maintenance-boundary primary")
    publish(sidecar,f"{receipt_sha}  {output.name}\n".encode("ascii"),
            "maintenance-boundary sidecar")
finally:os.close(dfd)
print(receipt_sha)
PY
}

legacy_height_row_field() {
    local receipt="$1" node="$2" field="$3"
    python3 - "$receipt" "$node" "$field" <<'PY'
import json
import pathlib
import sys
value = json.loads(pathlib.Path(sys.argv[1]).read_text(encoding="utf-8"))
rows = [row for row in value.get("origins", []) if row.get("name") == sys.argv[2]]
if len(rows) != 1 or sys.argv[3] not in rows[0]:
    raise SystemExit("legacy-height receipt row field is missing or ambiguous")
answer = rows[0][sys.argv[3]]
if isinstance(answer, bool) or not isinstance(answer, (str, int)):
    raise SystemExit("legacy-height receipt row field is not scalar")
print(answer)
PY
}

offline_cross_field() {
    local evidence="$1" node="$2" field="$3"
    python3 - "$evidence" "$node" "$field" <<'PY'
import json, pathlib, sys
value = json.loads(pathlib.Path(sys.argv[1]).read_text(encoding="utf-8"))
cross = value["legacy_height_cross_proof"]
if sys.argv[2] == "fleet":
    answer = cross.get(sys.argv[3])
else:
    rows = [row for row in cross["nodes"] if row.get("node") == sys.argv[2]]
    if len(rows) != 1: raise SystemExit("offline cross-proof node is ambiguous")
    answer = rows[0]["proof"].get(sys.argv[3])
if isinstance(answer, bool) or not isinstance(answer, (str, int)):
    raise SystemExit("offline cross-proof field is not scalar")
print(answer)
PY
}

build_authenticated_legacy_height_cross_proof() {
    local freeze_plan="$1" freeze_sha="$2" capture_id="$3" receipt="$4"
    local receipt_sha="$5" challenge="$6" proof_root="$7" output="$8"
    python3 - "$freeze_plan" "$freeze_sha" "$capture_id" "$receipt" "$receipt_sha" \
        "$challenge" "$proof_root" "$output" "${NODES[@]}" <<'PY'
import datetime
import hashlib
import json
import os
import pathlib
import re
import stat
import sys

(freeze_raw, freeze_sha, capture_id, receipt_raw, receipt_sha, challenge,
 proof_root_raw, output_raw, *fleet_raw) = sys.argv[1:]
freeze_path = pathlib.Path(freeze_raw); receipt_path = pathlib.Path(receipt_raw)
proof_root = pathlib.Path(proof_root_raw); output = pathlib.Path(output_raw)
fleet = [tuple(value.split("=", 1)) for value in fleet_raw]
canonical = lambda value: (json.dumps(value, sort_keys=True, separators=(",", ":")) + "\n").encode()
digest = lambda value: hashlib.sha256(value).hexdigest()
hash_re = re.compile(r"[0-9a-f]{64}")
freeze_payload = freeze_path.read_bytes(); freeze = json.loads(freeze_payload)
receipt_payload = receipt_path.read_bytes(); receipt = json.loads(receipt_payload)
if (freeze_payload != canonical(freeze) or digest(freeze_payload) != freeze_sha
        or receipt_payload != canonical(receipt) or digest(receipt_payload) != receipt_sha
        or hash_re.fullmatch(challenge) is None):
    raise SystemExit("authenticated legacy-height cross-proof input differs")
public_rows = receipt.get("origins")
if (not isinstance(public_rows, list)
        or [row.get("name") for row in public_rows] != [node for node, _ in fleet]):
    raise SystemExit("authenticated legacy-height public topology differs")
proof_rows = []
for (node, host), public in zip(fleet, public_rows):
    path = proof_root / f"{node}-legacy-height-bracket.json"
    details = path.lstat(); raw = path.read_bytes(); proof = json.loads(raw)
    if (path.is_symlink() or not stat.S_ISREG(details.st_mode) or raw != canonical(proof)):
        raise SystemExit(f"authenticated legacy-height proof is unsafe: {node}")
    expected = {
        "schema": "arc.recovery.authenticated-legacy-height-bracket.v1",
        "capture_id": capture_id, "node": node,
        "freeze_plan_sha256": freeze_sha, "challenge": challenge,
        "rpc_origin": next(row["rpc_origin"] for row in freeze["nodes"] if row["name"] == node),
        "public_info_before_height": public["info_before_height"],
        "public_latest_block_height": public["latest_block_height"],
        "public_info_after_height": public["info_after_height"],
        "public_latest_block_hash": public["latest_block_hash"],
    }
    if any(proof.get(field) != wanted for field, wanted in expected.items()):
        raise SystemExit(f"authenticated legacy-height proof binding differs: {node}")
    if proof.get("conservative_height_floor") != max(
        public["info_after_height"], proof.get("authenticated_info_after_height", -1)
    ):
        raise SystemExit(f"authenticated legacy-height conservative floor differs: {node}")
    if (proof["public_latest_block_height"] == proof["authenticated_latest_block_height"]
            and proof["public_latest_block_hash"] != proof["authenticated_latest_block_hash"]):
        raise SystemExit(f"authenticated/public same-height hash disagreement: {node}")
    proof_rows.append({
        "node": node, "host": host, "proof": proof,
        "proof_sha256": digest(raw),
    })
started = min(row["proof"]["started_at"] for row in proof_rows)
completed = max(row["proof"]["completed_at"] for row in proof_rows)
for value in (started, completed):
    datetime.datetime.strptime(value, "%Y-%m-%dT%H:%M:%SZ")
result = {
    "schema": "arc.recovery.authenticated-legacy-height-fleet.v1",
    "source_main_commit": freeze["source_commit"],
    "freeze_plan_sha256": freeze_sha, "capture_id": capture_id,
    "legacy_public_height_receipt_sha256": receipt_sha,
    "challenge": challenge, "started_at": started, "completed_at": completed,
    "conservative_height_floor": max(row["proof"]["conservative_height_floor"] for row in proof_rows),
    "nodes": proof_rows,
}
payload = canonical(result)
partial = output.with_name(output.name + ".partial")
directory = os.open(output.parent, os.O_RDONLY | getattr(os, "O_DIRECTORY", 0)
                    | getattr(os, "O_NOFOLLOW", 0))
def locked(name, modes):
    descriptor = os.open(name, os.O_RDONLY | getattr(os, "O_NOFOLLOW", 0), dir_fd=directory)
    try:
        details = os.fstat(descriptor)
        if (not stat.S_ISREG(details.st_mode) or details.st_uid != os.geteuid()
                or details.st_nlink != 1 or stat.S_IMODE(details.st_mode) not in modes
                or details.st_size <= 0 or details.st_size > 16 * 1024 * 1024):
            raise SystemExit("authenticated legacy-height output identity differs")
        chunks = []
        while True:
            chunk = os.read(descriptor, 1024 * 1024)
            if not chunk: break
            chunks.append(chunk)
        raw = b"".join(chunks)
        if len(raw) != details.st_size:
            raise SystemExit("authenticated legacy-height output changed while read")
        return raw
    finally: os.close(descriptor)
try:
    if output.exists() or output.is_symlink():
        if locked(output.name, {0o400}) != payload:
            raise SystemExit("existing authenticated legacy-height output differs")
        if partial.exists() or partial.is_symlink():
            locked(partial.name, {0o400, 0o600})
            os.unlink(partial.name, dir_fd=directory); os.fsync(directory)
    else:
        promote = False
        if partial.exists() or partial.is_symlink():
            if locked(partial.name, {0o400, 0o600}) == payload:
                os.chmod(partial.name, 0o400, dir_fd=directory, follow_symlinks=False)
                promote = True
            else:
                os.unlink(partial.name, dir_fd=directory); os.fsync(directory)
        if not promote:
            descriptor = os.open(
                partial.name,
                os.O_WRONLY | os.O_CREAT | os.O_EXCL | getattr(os, "O_NOFOLLOW", 0),
                0o600,
                dir_fd=directory,
            )
            with os.fdopen(descriptor, "wb") as handle:
                handle.write(payload); handle.flush(); os.fsync(handle.fileno())
                os.fchmod(handle.fileno(), 0o400)
        os.rename(partial.name, output.name, src_dir_fd=directory, dst_dir_fd=directory)
        os.fsync(directory)
finally: os.close(directory)
print(digest(payload))
PY
}

capture_authenticated_legacy_height_cross_proof() {
    local freeze_plan="$1" freeze_sha="$2" capture_id="$3" receipt="$4"
    local receipt_sha="$5" output="$6" proof_root="$7"
    require_absolute_file "$receipt" "legacy public-height receipt"
    require_hash "$receipt_sha" "legacy public-height receipt hash"
    [ "$(sealed_legacy_public_height_receipt_sha "$receipt")" = "$receipt_sha" ] || \
        die "legacy public-height receipt differs from its explicit hash"
    local source_main challenge verify_result
    source_main="$(manifest_field "$freeze_plan" source_commit)"
    verify_result="$(python3 -B -I "$LEGACY_HEIGHT_TOOL" verify \
        --source-main "$source_main" --freeze-plan "$freeze_plan" \
        --freeze-plan-sha256 "$freeze_sha" --receipt "$receipt" --max-age-seconds 300)"
    python3 - "$verify_result" "$receipt_sha" <<'PY'
import json, sys
value = json.loads(sys.argv[1])
if value.get("receipt_sha256") != sys.argv[2]:
    raise SystemExit("legacy public-height verifier returned a different receipt hash")
PY
    challenge="$(python3 -I - <<'PY'
import secrets
print(secrets.token_hex(32))
PY
)"
    local node
    local pids=() names=()
    for node in nyc lax ams lhr nrt sgp; do
        (
            run_remote "$node" legacy-height-bracket "$capture_id" "$node" "$freeze_sha" \
                "$(freeze_node_field "$freeze_plan" "$node" writer_pid)" \
                "$(freeze_node_field "$freeze_plan" "$node" writer_start_ticks)" \
                "$(freeze_node_field "$freeze_plan" "$node" boot_id)" \
                "$(freeze_node_field "$freeze_plan" "$node" executable_path)" \
                "$(freeze_node_field "$freeze_plan" "$node" executable_sha256)" \
                "$(freeze_node_field "$freeze_plan" "$node" argv_sha256)" \
                "$(freeze_node_field "$freeze_plan" "$node" rpc_origin)" \
                "$(legacy_height_row_field "$receipt" "$node" info_before_height)" \
                "$(legacy_height_row_field "$receipt" "$node" latest_block_height)" \
                "$(legacy_height_row_field "$receipt" "$node" info_after_height)" \
                "$(legacy_height_row_field "$receipt" "$node" latest_block_hash)" \
                "$challenge"
        ) > "$proof_root/$node-legacy-height-bracket.json" 2> "$proof_root/$node-legacy-height.stderr" &
        pids+=("$!"); names+=("$node")
    done
    local failed=0 index
    for index in "${!pids[@]}"; do
        if ! wait "${pids[$index]}"; then
            sed -n '1,40p' "$proof_root/${names[$index]}-legacy-height.stderr" >&2
            failed=1
        fi
    done
    [ "$failed" -eq 0 ] || die "authenticated legacy-height bracket failed on one or more fixed writers"
    build_authenticated_legacy_height_cross_proof "$freeze_plan" "$freeze_sha" \
        "$capture_id" "$receipt" "$receipt_sha" "$challenge" "$proof_root" "$output" >/dev/null
}

reserve_stop_boundary_timestamp() {
    local evidence_output="$1" boundary="$2" freeze_sha="$3" capture_id="$4"
    local public_receipt="${5:-}" public_receipt_sha="${6:-}" authenticated_cross="${7:-}"
    python3 - "$evidence_output" "$boundary" "$freeze_sha" "$capture_id" \
        "$public_receipt" "$public_receipt_sha" "$authenticated_cross" <<'PY'
import datetime
import fcntl
import hashlib
import json
import os
import pathlib
import re
import stat
import sys

output = pathlib.Path(sys.argv[1])
boundary, freeze_sha, capture_id, public_raw, public_sha, authenticated_raw = sys.argv[2:]
if boundary != "all-controlled-stopped":
    raise SystemExit("unsupported stop boundary")
if any((public_raw, public_sha, authenticated_raw)):
    raise SystemExit("unexpected public-height inputs for terminal stop boundary")
if (not output.is_absolute() or output.suffix != ".json"
        or os.fspath(output) != os.path.normpath(os.fspath(output))):
    raise SystemExit("stop boundary output is unsafe")
parent = output.parent
details = parent.lstat()
if (parent.is_symlink() or not stat.S_ISDIR(details.st_mode)
        or details.st_uid != os.geteuid() or details.st_mode & 0o022):
    raise SystemExit("stop boundary parent must be a real protected operator directory")
path = output.with_name(output.name + f".{boundary}.json")
partial = path.with_name(path.name + ".partial")
canonical = lambda value: (json.dumps(value, sort_keys=True, separators=(",", ":")) + "\n").encode()
expected = {
    "schema": "arc.recovery.stop-boundary-timestamp.v1",
    "boundary": boundary,
    "freeze_plan_sha256": freeze_sha,
    "capture_id": capture_id,
}

def locked_bytes(name, *, modes, links={1}, allow_empty=False):
    fd = os.open(name, os.O_RDONLY | getattr(os, "O_NOFOLLOW", 0), dir_fd=dfd)
    try:
        details = os.fstat(fd)
        stable = lambda item: (
            item.st_dev, item.st_ino, item.st_mode, item.st_uid, item.st_gid,
            item.st_nlink, item.st_size, item.st_mtime_ns, item.st_ctime_ns,
        )
        if (not stat.S_ISREG(details.st_mode) or details.st_uid != os.geteuid()
                or details.st_nlink not in links or stat.S_IMODE(details.st_mode) not in modes
                or details.st_size < (0 if allow_empty else 1) or details.st_size > 4096):
            raise SystemExit("stop boundary timestamp file identity is unsafe")
        raw = b""
        while len(raw) <= 4096:
            chunk = os.read(fd, 4097 - len(raw))
            if not chunk:
                break
            raw += chunk
        if len(raw) != details.st_size or stable(os.fstat(fd)) != stable(details):
            raise SystemExit("stop boundary timestamp changed while read")
        return raw
    finally:
        os.close(fd)

dfd = os.open(parent, os.O_RDONLY | getattr(os, "O_DIRECTORY", 0) | getattr(os, "O_NOFOLLOW", 0))
try:
    lock_name = path.name + ".lock"
    lock_fd = os.open(
        lock_name,
        os.O_RDWR | os.O_CREAT | getattr(os, "O_NOFOLLOW", 0),
        0o600,
        dir_fd=dfd,
    )
    lock_details = os.fstat(lock_fd)
    if (not stat.S_ISREG(lock_details.st_mode) or lock_details.st_uid != os.geteuid()
            or lock_details.st_gid != os.getegid() or lock_details.st_nlink != 1
            or stat.S_IMODE(lock_details.st_mode) != 0o600):
        raise SystemExit("stop boundary timestamp lock is unsafe")
    fcntl.flock(lock_fd, fcntl.LOCK_EX)
    if path.exists() or path.is_symlink():
        same_inode = bool(
            (partial.exists() or partial.is_symlink()) and os.path.samefile(path, partial)
        )
        payload = locked_bytes(path.name, modes={0o400}, links={2} if same_inode else {1})
        value = json.loads(payload)
        if payload != canonical(value) or set(value) != set(expected) | {"timestamp"}:
            raise SystemExit("existing stop boundary timestamp is noncanonical")
        if any(value.get(key) != wanted for key, wanted in expected.items()):
            raise SystemExit("existing stop boundary timestamp belongs to another capture")
        if partial.exists() or partial.is_symlink():
            fragment = locked_bytes(
                partial.name, modes={0o400, 0o600}, links={1, 2}, allow_empty=True
            )
            if same_inode:
                if fragment != payload:
                    raise SystemExit("stop boundary committed final/partial bytes differ")
            elif fragment:
                try:
                    fragment_value = json.loads(fragment)
                except (UnicodeDecodeError, json.JSONDecodeError):
                    fragment_value = None
                if isinstance(fragment_value, dict) and fragment == canonical(fragment_value):
                    if fragment != payload:
                        raise SystemExit("stop boundary canonical partial conflicts with terminal")
            os.unlink(partial.name, dir_fd=dfd)
            os.fsync(dfd)
    else:
        value = None
        if partial.exists() or partial.is_symlink():
            raw = locked_bytes(
                partial.name, modes={0o400, 0o600}, allow_empty=True
            )
            try:
                candidate = json.loads(raw)
            except (UnicodeDecodeError, json.JSONDecodeError):
                candidate = None
            if (isinstance(candidate, dict) and raw == canonical(candidate)
                    and set(candidate) == set(expected) | {"timestamp"}
                    and all(candidate.get(key) == wanted for key, wanted in expected.items())
                    and isinstance(candidate.get("timestamp"), str)
                    and re.fullmatch(r"\d{4}-\d{2}-\d{2}T\d{2}:\d{2}:\d{2}Z", candidate["timestamp"])):
                os.chmod(partial.name, 0o400, dir_fd=dfd, follow_symlinks=False)
                descriptor = os.open(
                    partial.name, os.O_RDONLY | getattr(os, "O_NOFOLLOW", 0), dir_fd=dfd
                )
                try:
                    os.fsync(descriptor)
                finally:
                    os.close(descriptor)
                os.link(
                    partial.name, path.name, src_dir_fd=dfd, dst_dir_fd=dfd,
                    follow_symlinks=False,
                )
                os.unlink(partial.name, dir_fd=dfd)
                os.fsync(dfd)
                value = candidate
            else:
                if isinstance(candidate, dict) and raw == canonical(candidate):
                    raise SystemExit("stop boundary canonical partial has conflicting identity")
                os.unlink(partial.name, dir_fd=dfd)
                os.fsync(dfd)
        if value is None:
            value = {
                **expected,
                "timestamp": datetime.datetime.now(datetime.timezone.utc).strftime("%Y-%m-%dT%H:%M:%SZ"),
            }
            payload = canonical(value)
            descriptor = os.open(
                partial.name,
                os.O_WRONLY | os.O_CREAT | os.O_EXCL | getattr(os, "O_NOFOLLOW", 0),
                0o600,
                dir_fd=dfd,
            )
            with os.fdopen(descriptor, "wb") as handle:
                handle.write(payload); handle.flush(); os.fchmod(handle.fileno(), 0o400)
                os.fsync(handle.fileno())
            try:
                os.link(
                    partial.name, path.name, src_dir_fd=dfd, dst_dir_fd=dfd,
                    follow_symlinks=False,
                )
            except FileExistsError:
                terminal = locked_bytes(path.name, modes={0o400}, links={1, 2})
                if terminal != payload:
                    raise SystemExit("concurrent stop boundary terminal differs")
            os.unlink(partial.name, dir_fd=dfd)
            os.fsync(dfd)
    fcntl.flock(lock_fd, fcntl.LOCK_UN)
    os.close(lock_fd)
finally:
    os.close(dfd)
timestamp = value["timestamp"]
if not isinstance(timestamp, str) or re.fullmatch(r"\d{4}-\d{2}-\d{2}T\d{2}:\d{2}:\d{2}Z", timestamp) is None:
    raise SystemExit("stop boundary timestamp is not canonical UTC")
print(timestamp)
PY
}

prepare_protected_maintenance_directory() {
    local path="$1"
    python3 - "$path" <<'PY'
import os,pathlib,stat,sys
path=pathlib.Path(sys.argv[1])
if (not path.is_absolute() or os.path.normpath(os.fspath(path))!=os.fspath(path)
        or os.path.realpath(path.parent)!=os.fspath(path.parent)):
    raise SystemExit("maintenance evidence directory path is unsafe")
parent=path.parent;details=parent.lstat()
if (parent.is_symlink() or not stat.S_ISDIR(details.st_mode)
        or details.st_uid not in {0,os.geteuid()} or details.st_mode&0o022):
    raise SystemExit("maintenance evidence directory parent is unsafe")
dfd=os.open(parent,os.O_RDONLY|getattr(os,"O_DIRECTORY",0)|getattr(os,"O_NOFOLLOW",0))
try:
    try:os.mkdir(path.name,0o700,dir_fd=dfd);os.fsync(dfd)
    except FileExistsError:pass
finally:os.close(dfd)
details=path.lstat()
if (path.is_symlink() or not stat.S_ISDIR(details.st_mode)
        or details.st_uid!=os.geteuid() or stat.S_IMODE(details.st_mode)!=0o700):
    raise SystemExit("maintenance evidence directory is unsafe")
print(path)
PY
}

publish_canonical_maintenance_input() {
    local source="$1" destination="$2"
    python3 - "$source" "$destination" <<'PY'
import fcntl,json,os,pathlib,stat,sys
source=pathlib.Path(sys.argv[1]);destination=pathlib.Path(sys.argv[2])
raw=source.read_bytes();value=json.loads(raw)
canonical=(json.dumps(value,sort_keys=True,separators=(",",":"))+"\n").encode()
if raw!=canonical:raise SystemExit("maintenance evidence input is noncanonical")
parent=destination.parent;details=parent.lstat()
if (parent.is_symlink() or not stat.S_ISDIR(details.st_mode)
        or details.st_uid!=os.geteuid() or stat.S_IMODE(details.st_mode)!=0o700):
    raise SystemExit("maintenance evidence destination directory is unsafe")
dfd=os.open(parent,os.O_RDONLY|getattr(os,"O_DIRECTORY",0)|getattr(os,"O_NOFOLLOW",0))
partial=destination.with_name(destination.name+".partial")
def read_locked(name,modes,links={1},empty=False):
    fd=os.open(name,os.O_RDONLY|getattr(os,"O_NOFOLLOW",0),dir_fd=dfd)
    try:
        before=os.fstat(fd)
        stable=lambda value:(value.st_dev,value.st_ino,value.st_mode,value.st_uid,value.st_gid,
                             value.st_nlink,value.st_size,value.st_mtime_ns,value.st_ctime_ns)
        if (not stat.S_ISREG(before.st_mode) or before.st_uid!=os.geteuid()
                or before.st_nlink not in links or stat.S_IMODE(before.st_mode) not in modes
                or before.st_size<0 or (before.st_size==0 and not empty)
                or before.st_size>32*1024*1024):
            raise SystemExit("maintenance evidence input identity differs")
        chunks=[];remaining=32*1024*1024+1
        while remaining:
            chunk=os.read(fd,min(1024*1024,remaining))
            if not chunk:break
            chunks.append(chunk);remaining-=len(chunk)
        current=b"".join(chunks)
        if len(current)!=before.st_size or stable(os.fstat(fd))!=stable(before):
            raise SystemExit("maintenance evidence input changed while read")
        return current
    finally:os.close(fd)
try:
    lock_name=destination.name+".lock"
    lock_fd=os.open(lock_name,os.O_RDWR|os.O_CREAT|getattr(os,"O_NOFOLLOW",0),0o600,dir_fd=dfd)
    lock_details=os.fstat(lock_fd)
    if (not stat.S_ISREG(lock_details.st_mode) or lock_details.st_uid!=os.geteuid()
            or lock_details.st_gid!=os.getegid() or lock_details.st_nlink!=1
            or stat.S_IMODE(lock_details.st_mode)!=0o600):
        raise SystemExit("maintenance evidence lock identity differs")
    fcntl.flock(lock_fd,fcntl.LOCK_EX)
    if destination.exists() or destination.is_symlink():
        same=(partial.exists() or partial.is_symlink()) and os.path.samefile(destination,partial)
        if read_locked(destination.name,{0o400},{2} if same else {1})!=raw:
            raise SystemExit("existing maintenance evidence input differs")
        if partial.exists() or partial.is_symlink():
            fragment=read_locked(partial.name,{0o400,0o600},{1,2},True)
            if fragment and fragment!=raw:
                try:fragment_value=json.loads(fragment)
                except (UnicodeDecodeError,json.JSONDecodeError):fragment_value=None
                if isinstance(fragment_value,dict) and fragment==(json.dumps(fragment_value,sort_keys=True,separators=(",",":"))+"\n").encode():
                    raise SystemExit("canonical maintenance partial conflicts with terminal")
            os.unlink(partial.name,dir_fd=dfd);os.fsync(dfd)
    else:
        promote=False
        if partial.exists() or partial.is_symlink():
            current=read_locked(partial.name,{0o400,0o600},{1},True)
            if current==raw:
                os.chmod(partial.name,0o400,dir_fd=dfd,follow_symlinks=False)
                fd=os.open(partial.name,os.O_RDONLY|getattr(os,"O_NOFOLLOW",0),dir_fd=dfd)
                try:os.fsync(fd)
                finally:os.close(fd)
                promote=True
            else:
                try:partial_value=json.loads(current)
                except (UnicodeDecodeError,json.JSONDecodeError):partial_value=None
                if isinstance(partial_value,dict) and current==(json.dumps(partial_value,sort_keys=True,separators=(",",":"))+"\n").encode():
                    raise SystemExit("canonical maintenance partial conflicts with source")
                os.unlink(partial.name,dir_fd=dfd);os.fsync(dfd)
        if not promote:
            fd=os.open(partial.name,os.O_WRONLY|os.O_CREAT|os.O_EXCL|getattr(os,"O_NOFOLLOW",0),0o600,dir_fd=dfd)
            with os.fdopen(fd,"wb") as handle:
                handle.write(raw);handle.flush();os.fchmod(handle.fileno(),0o400);os.fsync(handle.fileno())
        try:
            os.link(partial.name,destination.name,src_dir_fd=dfd,dst_dir_fd=dfd,follow_symlinks=False)
        except FileExistsError:
            if read_locked(destination.name,{0o400},{1,2})!=raw:
                raise SystemExit("concurrent maintenance evidence terminal differs")
        os.unlink(partial.name,dir_fd=dfd)
        os.fsync(dfd)
    fcntl.flock(lock_fd,fcntl.LOCK_UN);os.close(lock_fd)
finally:os.close(dfd)
PY
}

round_artifact_sha() {
    local path="$1"
    require_absolute_file "$path" "quarantine round artifact"
    hash_file "$path"
}

round_authorization_is_live() {
    local authorization="$1"
    python3 - "$authorization" <<'PY'
import datetime,json,pathlib,sys
value=json.loads(pathlib.Path(sys.argv[1]).read_text(encoding="utf-8"))
deadline=datetime.datetime.strptime(value["authorization_deadline"],"%Y-%m-%dT%H:%M:%SZ").replace(tzinfo=datetime.timezone.utc)
raise SystemExit(0 if datetime.datetime.now(datetime.timezone.utc)<=deadline else 1)
PY
}

operator_selection_window_is_live() {
    local selected_monotonic_ns="$1" selected_realtime_ns="$2"
    python3 - "$selected_monotonic_ns" "$selected_realtime_ns" \
        "$QUARANTINE_ROUND_DRIVER" <<'PY'
import importlib.util,sys
try:selected_monotonic=int(sys.argv[1]);selected_realtime=int(sys.argv[2])
except ValueError:raise SystemExit(1)
spec=importlib.util.spec_from_file_location("arc_quarantine_round_driver",sys.argv[3])
if spec is None or spec.loader is None:raise SystemExit(1)
module=importlib.util.module_from_spec(spec);sys.modules[spec.name]=module;spec.loader.exec_module(module)
try:module.operator_selection_remaining_ns(selected_monotonic,selected_realtime)
except (ValueError,OSError):raise SystemExit(1)
PY
}

quarantine_round_prefix_ref() {
    local round_root="$1" through="$2" node="$3"
    python3 -I "$QUARANTINE_ROUND_DRIVER" prefix-ref \
        --round-root "$round_root" --through "$through" --node "$node"
}

quarantine_round_remaining_targets() {
    local round_root="$1" through="$2"
    python3 - "$round_root" "$through" "$QUARANTINE_ROUND_MODULE" <<'PY'
import importlib.util,json,pathlib,sys
root=pathlib.Path(sys.argv[1]);through=int(sys.argv[2]);module_path=sys.argv[3]
spec=importlib.util.spec_from_file_location("arc_quarantine_rounds",module_path)
if spec is None or spec.loader is None:raise SystemExit("cannot load quarantine-round validator")
module=importlib.util.module_from_spec(spec);spec.loader.exec_module(module)
secured=set();prior=[]
for number in range(1,through+1):
    auth=json.loads((root/f"round-{number}"/"authorization.json").read_text(encoding="utf-8"))
    result=json.loads((root/f"round-{number}"/"result.json").read_text(encoding="utf-8"))
    transitions=[module.validate_wrapper(row,"prefix transition")[0] for row in result["transitions"]]
    module.validate_round_result(
        result,authorization=auth,prior_results=prior,transition_receipts=transitions
    )
    secured.update(row["node"] for row in transitions);prior.append(result)
print(",".join(name for name,_host in module.FLEET if name not in secured))
PY
}

capture_quarantine_round_prior_statuses() {
    local round_root="$1" through="$2" freeze_sha="$3" capture_id="$4"
    local output_root="$5" log_root="$6" node round auth_sha readiness_sha applied_sha
    for node in nyc lax ams lhr nrt sgp; do
        if [ "$through" -eq 0 ]; then
            break
        fi
        if ! read -r round auth_sha readiness_sha applied_sha < <(
            quarantine_round_prefix_ref "$round_root" "$through" "$node" 2>/dev/null
        ); then
            continue
        fi
        require_uint "$round" "prior-fenced quarantine round"
        require_hash "$auth_sha" "prior-fenced authorization root"
        require_hash "$readiness_sha" "prior-fenced readiness root"
        require_hash "$applied_sha" "prior-fenced applied root"
        run_remote "$node" quarantine-round-status "$capture_id" "$node" "$freeze_sha" \
            "$round" "$auth_sha" "$readiness_sha" "$applied_sha" \
            > "$log_root/$node-prior-fenced-status.new.json"
        chmod 400 "$log_root/$node-prior-fenced-status.new.json"
        publish_canonical_maintenance_input \
            "$log_root/$node-prior-fenced-status.new.json" "$output_root/$node.json"
    done
}

capture_quarantine_round_target_cross() {
    local freeze_plan="$1" freeze_sha="$2" capture_id="$3" targets="$4"
    local public_receipt="$5" attempt_root="$6"
    local challenge bracket_root cross node index failed=0
    local pids=() names=()
    challenge="$(python3 -I - <<'PY'
import secrets
print(secrets.token_hex(32))
PY
)"
    require_hash "$challenge" "quarantine-round authenticated-height challenge"
    bracket_root="$attempt_root/authenticated-brackets"
    mkdir -m 700 -- "$bracket_root"
    cross="$attempt_root/authenticated-target-cross.json"
    IFS=',' read -r -a names <<< "$targets"
    for node in "${names[@]}"; do
        (
            run_remote "$node" legacy-height-bracket "$capture_id" "$node" "$freeze_sha" \
                "$(freeze_node_field "$freeze_plan" "$node" writer_pid)" \
                "$(freeze_node_field "$freeze_plan" "$node" writer_start_ticks)" \
                "$(freeze_node_field "$freeze_plan" "$node" boot_id)" \
                "$(freeze_node_field "$freeze_plan" "$node" executable_path)" \
                "$(freeze_node_field "$freeze_plan" "$node" executable_sha256)" \
                "$(freeze_node_field "$freeze_plan" "$node" argv_sha256)" \
                "$(freeze_node_field "$freeze_plan" "$node" rpc_origin)" \
                "$(legacy_height_row_field "$public_receipt" "$node" info_before_height)" \
                "$(legacy_height_row_field "$public_receipt" "$node" latest_block_height)" \
                "$(legacy_height_row_field "$public_receipt" "$node" info_after_height)" \
                "$(legacy_height_row_field "$public_receipt" "$node" latest_block_hash)" \
                "$challenge" > "$bracket_root/$node.new.json"
            chmod 400 "$bracket_root/$node.new.json"
            publish_canonical_maintenance_input \
                "$bracket_root/$node.new.json" "$bracket_root/$node.json"
        ) > "$attempt_root/$node-authenticated-bracket.log" 2>&1 &
        pids+=("$!")
    done
    for index in "${!pids[@]}"; do
        if ! wait "${pids[$index]}"; then
            sed -n '1,80p' "$attempt_root/${names[$index]}-authenticated-bracket.log" >&2
            failed=1
        fi
    done
    [ "$failed" -eq 0 ] || die \
        "one or more still-live targets failed the authenticated quarantine-round bracket"
    python3 -I "$QUARANTINE_ROUND_DRIVER" build-cross \
        --freeze-plan "$freeze_plan" --freeze-plan-sha256 "$freeze_sha" \
        --capture-id "$capture_id" --targets "$targets" --public "$public_receipt" \
        --bracket-root "$bracket_root" --output "$cross" >/dev/null
}

quarantine_cross_node_field() {
    local cross="$1" node="$2" field="$3"
    python3 - "$cross" "$node" "$field" <<'PY'
import json,pathlib,sys
value=json.loads(pathlib.Path(sys.argv[1]).read_text(encoding="utf-8"))
rows=[row for row in value.get("nodes",[]) if row.get("node")==sys.argv[2]]
if len(rows)!=1 or sys.argv[3] not in rows[0]:raise SystemExit("quarantine cross field is absent or ambiguous")
item=rows[0][sys.argv[3]]
if isinstance(item,(dict,list,bool)) or item is None:raise SystemExit("quarantine cross field is not scalar")
print(item)
PY
}

capture_quarantine_round_live_sources() {
    local freeze_plan="$1" freeze_sha="$2" capture_id="$3" round_number="$4"
    local targets="$5" public_receipt="$6" cross="$7" attempt_root="$8"
    local inspector_sha="$9" genesis_sha="${10}" legacy_sha="${11}" allow_unbound="${12}"
    local output_root="$attempt_root/live-source-captures" node index failed=0
    local public_sha cross_sha temporary
    local pids=() names=()
    mkdir -m 700 -- "$output_root"
    public_sha="$(round_artifact_sha "$public_receipt")"
    cross_sha="$(round_artifact_sha "$cross")"
    IFS=',' read -r -a names <<< "$targets"
    for node in "${names[@]}"; do
        (
            temporary="$attempt_root/$node-live-source.new.json"
            local public_after authenticated_after minimum_height
            public_after="$(legacy_height_row_field "$public_receipt" "$node" info_after_height)"
            authenticated_after="$(quarantine_cross_node_field "$cross" "$node" loopback_info_after_height)"
            if [ "$public_after" -ge "$authenticated_after" ]; then
                minimum_height="$public_after"
            else
                minimum_height="$authenticated_after"
            fi
            run_remote "$node" capture-live-source "$capture_id" "$node" "$freeze_sha" \
                "$round_number" \
                "$(freeze_node_field "$freeze_plan" "$node" writer_pid)" \
                "$(freeze_node_field "$freeze_plan" "$node" writer_start_ticks)" \
                "$(freeze_node_field "$freeze_plan" "$node" boot_id)" \
                "$(freeze_node_field "$freeze_plan" "$node" writer_cgroup_sha256)" \
                "$(freeze_node_field "$freeze_plan" "$node" executable_path)" \
                "$(freeze_node_field "$freeze_plan" "$node" executable_sha256)" \
                "$(freeze_node_field "$freeze_plan" "$node" argv_sha256)" \
                "$(freeze_node_field "$freeze_plan" "$node" data_dir)" \
                "$(freeze_node_field "$freeze_plan" "$node" rpc_origin)" \
                "$inspector_sha" "$genesis_sha" "$legacy_sha" "$allow_unbound" \
                "$(legacy_height_row_field "$public_receipt" "$node" latest_block_height)" \
                "$(legacy_height_row_field "$public_receipt" "$node" latest_block_hash)" \
                "$(quarantine_cross_node_field "$cross" "$node" loopback_latest_height)" \
                "$(quarantine_cross_node_field "$cross" "$node" loopback_latest_block_hash)" \
                "$public_sha" "$cross_sha" preauthorization-boundary \
                "$minimum_height" - - - "$cross_sha" - - > "$temporary"
            chmod 400 "$temporary"
            publish_canonical_maintenance_input "$temporary" "$output_root/$node.json"
        ) > "$attempt_root/$node-live-source.log" 2>&1 &
        pids+=("$!")
    done
    for index in "${!pids[@]}"; do
        if ! wait "${pids[$index]}"; then
            sed -n '1,120p' "$attempt_root/${names[$index]}-live-source.log" >&2
            failed=1
        fi
    done
    [ "$failed" -eq 0 ] || die \
        "one or more live targets failed exact snapshot/WAL-prefix capture"
}

seal_quarantine_mutation_dispatch() {
    local authorization="$1" readiness="$2" output="$3"
    python3 - "$authorization" "$readiness" "$output" <<'PY'
import datetime,hashlib,json,os,pathlib,stat,sys
authorization_path,readiness_path,output=map(pathlib.Path,sys.argv[1:])
canonical=lambda value:(json.dumps(value,sort_keys=True,separators=(",",":"))+"\n").encode()
digest=lambda raw:hashlib.sha256(raw).hexdigest()
def locked(path,label,modes={0o400},links={1}):
    fd=os.open(path,os.O_RDONLY|getattr(os,"O_NOFOLLOW",0))
    try:
        details=os.fstat(fd)
        if (not stat.S_ISREG(details.st_mode) or path.is_symlink()
                or details.st_uid!=os.geteuid() or details.st_nlink not in links
                or stat.S_IMODE(details.st_mode) not in modes
                or not 0<details.st_size<=32*1024*1024):
            raise SystemExit(f"quarantine mutation dispatch {label} is unsafe")
        raw=os.read(fd,32*1024*1024+1)
        if len(raw)!=details.st_size:raise SystemExit(f"quarantine mutation dispatch {label} changed")
        value=json.loads(raw)
        if raw!=canonical(value):raise SystemExit(f"quarantine mutation dispatch {label} is noncanonical")
        return value,raw
    finally:os.close(fd)
authorization,authorization_raw=locked(authorization_path,"authorization")
readiness,readiness_raw=locked(readiness_path,"readiness")
identity={
    "schema":"arc.recovery.quarantine-mutation-dispatch.v1",
    "capture_id":authorization.get("capture_id"),
    "freeze_plan_sha256":authorization.get("freeze_plan_sha256"),
    "round_number":authorization.get("round_number"),
    "round_authorization_sha256":digest(authorization_raw),
    "round_readiness_sha256":digest(readiness_raw),
    "live_observation_selection_sha256":authorization.get("live_observation_selection_sha256"),
    "live_observation_generation":authorization.get("live_observation_generation"),
    "observation_generation_receipt_sha256":authorization.get("observation_generation_receipt_sha256"),
    "drive_prefreeze_receipt_sha256":authorization.get("drive_prefreeze_receipt_sha256"),
    "targets":[{"node":row.get("node"),"host":row.get("host")} for row in authorization.get("targets",[])],
}
if (readiness.get("round_authorization_sha256")!=identity["round_authorization_sha256"]
        or readiness.get("round_number")!=identity["round_number"]):
    raise SystemExit("quarantine mutation dispatch readiness differs")
partial=output.with_name(output.name+".partial")
if (output.exists() or output.is_symlink()) and (partial.exists() or partial.is_symlink()):
    output_details=output.lstat();partial_details=partial.lstat()
    if (output.is_symlink() or partial.is_symlink()
            or (output_details.st_dev,output_details.st_ino)!=(partial_details.st_dev,partial_details.st_ino)
            or output_details.st_nlink!=2 or partial_details.st_nlink!=2
            or locked(output,"linked receipt",links={2})[1]
                !=locked(partial,"linked partial receipt",{0o400,0o600},{2})[1]):
        raise SystemExit("quarantine mutation dispatch interrupted publication differs")
    os.unlink(partial)
    dfd=os.open(output.parent,os.O_RDONLY|getattr(os,"O_DIRECTORY",0))
    try:os.fsync(dfd)
    finally:os.close(dfd)
if output.exists() or output.is_symlink():
    existing,_raw=locked(output,"existing receipt")
    dispatched_at=existing.get("dispatched_at")
else:
    if partial.exists() or partial.is_symlink():
        details=partial.lstat()
        if (partial.is_symlink() or not stat.S_ISREG(details.st_mode)
                or details.st_uid!=os.geteuid() or details.st_nlink!=1
                or stat.S_IMODE(details.st_mode) not in {0o400,0o600}):
            raise SystemExit("quarantine mutation dispatch partial identity differs")
        try:
            partial_value,partial_raw=locked(partial,"partial receipt",{0o400,0o600})
            dispatched_at=partial_value["dispatched_at"]
            datetime.datetime.strptime(dispatched_at,"%Y-%m-%dT%H:%M:%SZ")
        except (SystemExit,KeyError,TypeError,ValueError,json.JSONDecodeError,UnicodeDecodeError):
            if stat.S_IMODE(details.st_mode)!=0o600:
                raise SystemExit("sealed quarantine mutation dispatch partial is malformed")
            os.unlink(partial)
            dfd=os.open(output.parent,os.O_RDONLY|getattr(os,"O_DIRECTORY",0))
            try:os.fsync(dfd)
            finally:os.close(dfd)
            dispatched_at=datetime.datetime.now(datetime.timezone.utc).strftime("%Y-%m-%dT%H:%M:%SZ")
    else:
        dispatched_at=datetime.datetime.now(datetime.timezone.utc).strftime("%Y-%m-%dT%H:%M:%SZ")
value={**identity,"dispatched_at":dispatched_at}
payload=canonical(value)
if output.exists() or output.is_symlink():
    if existing!=value:raise SystemExit("existing quarantine mutation dispatch differs")
else:
    if partial.exists() or partial.is_symlink():
        partial_value,partial_raw=locked(partial,"partial receipt",{0o400,0o600})
        if partial_value!=value or partial_raw!=payload:
            raise SystemExit("partial quarantine mutation dispatch differs")
        os.chmod(partial,0o400,follow_symlinks=False)
    else:
        fd=os.open(partial,os.O_WRONLY|os.O_CREAT|os.O_EXCL|getattr(os,"O_NOFOLLOW",0),0o600)
        with os.fdopen(fd,"wb") as handle:
            handle.write(payload);handle.flush();os.fsync(handle.fileno());os.fchmod(handle.fileno(),0o400)
    try:
        os.link(partial,output,follow_symlinks=False)
    except FileExistsError:
        if locked(output,"racing receipt",links={1,2})[1]!=payload:
            raise SystemExit("racing quarantine mutation dispatch differs")
    dfd=os.open(output.parent,os.O_RDONLY|getattr(os,"O_DIRECTORY",0))
    try:
        os.fsync(dfd)
        os.unlink(partial)
        os.fsync(dfd)
    finally:os.close(dfd)
    if locked(output,"published receipt")[1]!=payload:
        raise SystemExit("published quarantine mutation dispatch differs")
print(digest(payload))
PY
}

capture_post_quarantine_final_sources() {
    local freeze_plan="$1" freeze_sha="$2" capture_id="$3" ledger="$4"
    local stability_proof="$5" status_root="$6" output_root="$7" log_root="$8"
    local inspector_sha="$9" genesis_sha="${10}" legacy_sha="${11}"
    local allow_unbound="${12}"
    shift 12
    local nodes=("$@") node index failed=0 stability_sha temporary
    local pids=() names=()
    stability_sha="$(round_artifact_sha "$stability_proof")"
    mkdir -m 700 -- "$output_root"
    for node in "${nodes[@]}"; do
        (
            local round public_height public_hash authenticated_height authenticated_hash
            local public_sha cross_sha minimum_height expected_height expected_hash expected_state
            local network_receipt_sha owned_ruleset_sha
            read -r round public_height public_hash authenticated_height authenticated_hash \
                public_sha cross_sha minimum_height expected_height expected_hash expected_state \
                network_receipt_sha owned_ruleset_sha < <(
                python3 - "$ledger" "$stability_proof" "$status_root/$node-pre-stop-status.json" \
                    "$node" "$stability_sha" <<'PY'
import hashlib,json,pathlib,sys
ledger_path,stability_path,status_path,node,stability_sha=sys.argv[1:]
canonical=lambda value:(json.dumps(value,sort_keys=True,separators=(",",":"))+"\n").encode()
ledger_raw=pathlib.Path(ledger_path).read_bytes();ledger=json.loads(ledger_raw)
stability_raw=pathlib.Path(stability_path).read_bytes();stability=json.loads(stability_raw)
status_raw=pathlib.Path(status_path).read_bytes();status=json.loads(status_raw)
if (ledger_raw!=canonical(ledger) or stability_raw!=canonical(stability)
        or status_raw!=canonical(status)
        or hashlib.sha256(stability_raw).hexdigest()!=stability_sha):
    raise SystemExit("final source capture inputs are noncanonical")
found=[]
for round_wrapper in ledger.get("rounds",[]):
    authorization=round_wrapper.get("authorization",{}).get("value",{})
    for wrapper in round_wrapper.get("result",{}).get("value",{}).get("transitions",[]):
        transition=wrapper.get("value",{})
        if transition.get("node")==node:found.append((authorization,transition))
if len(found)!=1 or found[0][1].get("schema")!="arc.recovery.quarantine-node-nft-applied.v1":
    raise SystemExit("final source capture node is not an exact active transition")
authorization,transition=found[0]
public_wrapper=authorization["public_height_receipt"]
cross_wrapper=authorization["authenticated_height_cross_proof"]
public=public_wrapper["value"];cross=cross_wrapper["value"]
public_rows=[row for row in public["origins"] if row.get("name")==node]
cross_rows=[row for row in cross["nodes"] if row.get("node")==node]
head_rows=[row for row in stability.get("fleet_heads",[]) if row.get("node")==node]
if len(public_rows)!=1 or len(cross_rows)!=1 or len(head_rows)!=1:
    raise SystemExit("final source capture boundary row is missing or ambiguous")
public_row=public_rows[0];cross_row=cross_rows[0];head=head_rows[0]["head"]
if (status.get("schema")!="arc.recovery.legacy-network-quarantine-status.v1"
        or status.get("node")!=node or status.get("active") is not True
        or status.get("enabled") is not True
        or {"height":status.get("loopback_head",{}).get("latest_height"),
            "block_hash":status.get("loopback_head",{}).get("block_hash"),
            "state_root":status.get("loopback_head",{}).get("state_root")}!=head):
    raise SystemExit("final source capture status/head differs from stability proof")
values=(authorization["round_number"],public_row["latest_block_height"],
    public_row["latest_block_hash"],cross_row["loopback_latest_height"],
    cross_row["loopback_latest_block_hash"],public_wrapper["sha256"],cross_wrapper["sha256"],
    max(public_row["info_after_height"],cross_row["loopback_info_after_height"]),
    head["height"],head["block_hash"],head["state_root"],status["receipt_sha256"],
    status["owned_ruleset_stateless_sha256"])
print(" ".join(map(str,values)))
PY
            )
            temporary="$log_root/$node-final-source.new.json"
            run_remote "$node" capture-live-source "$capture_id" "$node" "$freeze_sha" \
                "$round" \
                "$(freeze_node_field "$freeze_plan" "$node" writer_pid)" \
                "$(freeze_node_field "$freeze_plan" "$node" writer_start_ticks)" \
                "$(freeze_node_field "$freeze_plan" "$node" boot_id)" \
                "$(freeze_node_field "$freeze_plan" "$node" writer_cgroup_sha256)" \
                "$(freeze_node_field "$freeze_plan" "$node" executable_path)" \
                "$(freeze_node_field "$freeze_plan" "$node" executable_sha256)" \
                "$(freeze_node_field "$freeze_plan" "$node" argv_sha256)" \
                "$(freeze_node_field "$freeze_plan" "$node" data_dir)" \
                "$(freeze_node_field "$freeze_plan" "$node" rpc_origin)" \
                "$inspector_sha" "$genesis_sha" "$legacy_sha" "$allow_unbound" \
                "$public_height" "$public_hash" "$authenticated_height" \
                "$authenticated_hash" "$public_sha" "$cross_sha" \
                post-quarantine-final-export "$minimum_height" "$expected_height" \
                "$expected_hash" "$expected_state" "$stability_sha" \
                "$network_receipt_sha" "$owned_ruleset_sha" > "$temporary"
            chmod 400 "$temporary"
            python3 - "$temporary" "$node" "$expected_height" "$expected_hash" \
                "$expected_state" "$stability_sha" <<'PY'
import hashlib,json,pathlib,sys
path=pathlib.Path(sys.argv[1]);node=sys.argv[2];height=int(sys.argv[3])
expected={"height":height,"block_hash":sys.argv[4],"state_root":sys.argv[5]}
value=json.loads(path.read_text(encoding="utf-8"))
raw=(json.dumps(value,sort_keys=True,separators=(",",":"))+"\n").encode()
if (path.read_bytes()!=raw or value.get("node")!=node
        or value.get("source_pair_role")!="post-quarantine-final-export"
        or value.get("expected_head")!=expected or value.get("head")!=expected
        or value.get("boundary_proof_sha256")!=sys.argv[6]
        or value.get("content_sealed") is not True
        or value.get("strict_offline_replay") is not True):
    raise SystemExit("post-quarantine final source capture differs")
PY
            publish_canonical_maintenance_input "$temporary" "$output_root/$node.json"
        ) > "$log_root/$node-final-source.log" 2>&1 &
        pids+=("$!"); names+=("$node")
    done
    for index in "${!pids[@]}"; do
        if ! wait "${pids[$index]}"; then
            sed -n '1,120p' "$log_root/${names[$index]}-final-source.log" >&2
            failed=1
        fi
    done
    [ "$failed" -eq 0 ] || die \
        "one or more active nodes lack an exact post-stability source pair; no writer stop is authorized"
}

quarantine_authorization_matches_live_observation() {
    local authorization="$1" selection="$2" selection_sha="$3"
    local generation_receipt="$4" generation="$5" generation_sha="$6"
    local drive_sha="$7" freeze_sha="$8" capture_id="$9"
    verify_live_observation_selection_exact "$selection" "$selection_sha" \
        "$generation_receipt" "$generation" "$generation_sha" "$drive_sha" \
        "$freeze_sha" "$capture_id" || return 1
    python3 - "$authorization" "$selection" "$selection_sha" "$generation" \
        "$generation_sha" "$drive_sha" "$freeze_sha" "$capture_id" <<'PY'
import hashlib,json,os,pathlib,stat,sys
authorization_path,selection_path=map(pathlib.Path,sys.argv[1:3])
selection_sha,generation,generation_sha,drive_sha,freeze,capture=sys.argv[3:]
canonical=lambda value:(json.dumps(value,sort_keys=True,separators=(",",":"))+"\n").encode()
def locked(path,label):
    fd=os.open(path,os.O_RDONLY|getattr(os,"O_NOFOLLOW",0))
    try:
        details=os.fstat(fd)
        if (not stat.S_ISREG(details.st_mode) or path.is_symlink()
                or details.st_uid!=os.geteuid() or details.st_nlink!=1
                or stat.S_IMODE(details.st_mode)!=0o400
                or not 0<details.st_size<=32*1024*1024):
            raise SystemExit(f"quarantine observation {label} is unsafe")
        raw=os.read(fd,32*1024*1024+1)
        if len(raw)!=details.st_size:raise SystemExit(f"quarantine observation {label} changed")
        value=json.loads(raw)
        if raw!=canonical(value):raise SystemExit(f"quarantine observation {label} is noncanonical")
        return value,raw
    finally:os.close(fd)
authorization,_authorization_raw=locked(authorization_path,"authorization")
selection,selection_raw=locked(selection_path,"selection")
expected=(selection_sha,generation,generation_sha,drive_sha,selection.get("selected_at"))
actual=(authorization.get("live_observation_selection_sha256"),
        authorization.get("live_observation_generation"),
        authorization.get("observation_generation_receipt_sha256"),
        authorization.get("drive_prefreeze_receipt_sha256"),
        authorization.get("live_observation_selected_at"))
if (hashlib.sha256(selection_raw).hexdigest()!=selection_sha
        or (authorization.get("capture_id"),authorization.get("freeze_plan_sha256"))
            !=(capture,freeze) or actual!=expected):
    raise SystemExit("quarantine authorization uses another live-observation selection")
PY
}

quarantine_attempt_has_valid_zero_progress_release() {
    local attempt_root="$1" freeze_sha="$2" capture_id="$3"
    local release="$attempt_root/zero-progress-release.json"
    [ -f "$release" ] && [ ! -L "$release" ] || return 1
    python3 -I - "$attempt_root" "$freeze_sha" "$capture_id" \
        "$QUARANTINE_ROUND_MODULE" <<'PY'
import datetime,hashlib,importlib.util,json,os,pathlib,re,stat,sys
attempt=pathlib.Path(sys.argv[1]);freeze,capture,module_path=sys.argv[2:]
canonical=lambda value:(json.dumps(value,sort_keys=True,separators=(",",":"))+"\n").encode()
digest=lambda raw:hashlib.sha256(raw).hexdigest()
hash_re=re.compile(r"[0-9a-f]{64}")
if any(hash_re.fullmatch(value) is None for value in (freeze,capture)):
    raise SystemExit("zero-progress resume identity is malformed")
spec=importlib.util.spec_from_file_location("arc_zero_progress_rounds",module_path)
if spec is None or spec.loader is None:raise SystemExit("cannot load quarantine-round validator")
rounds=importlib.util.module_from_spec(spec);sys.modules[spec.name]=rounds;spec.loader.exec_module(rounds)
def locked(path,label):
    fd=os.open(path,os.O_RDONLY|getattr(os,"O_NOFOLLOW",0))
    try:
        details=os.fstat(fd)
        if (not stat.S_ISREG(details.st_mode) or path.is_symlink()
                or details.st_uid!=os.geteuid() or details.st_nlink!=1
                or stat.S_IMODE(details.st_mode)!=0o400
                or not 0<details.st_size<=32*1024*1024):
            raise SystemExit(f"zero-progress resume {label} is unsafe")
        raw=os.read(fd,32*1024*1024+1)
        if len(raw)!=details.st_size:raise SystemExit(f"zero-progress resume {label} changed")
        value=json.loads(raw)
        if raw!=canonical(value):raise SystemExit(f"zero-progress resume {label} is noncanonical")
        return value,raw
    finally:os.close(fd)
authorization,authorization_raw=locked(attempt/"authorization.json","authorization")
readiness,readiness_raw=locked(attempt/"readiness.json","readiness")
dispatch,dispatch_raw=locked(attempt/"mutation-dispatch.json","dispatch")
release,_release_raw=locked(attempt/"zero-progress-release.json","release")
state=rounds.validate_round_authorization(authorization,prior_results=[])
auth_sha=digest(authorization_raw);readiness_sha=digest(readiness_raw);dispatch_sha=digest(dispatch_raw)
targets=authorization.get("targets")
names=[row.get("node") for row in targets] if isinstance(targets,list) else []
if (state.get("round_number")!=1 or state.get("capture_id")!=capture
        or state.get("freeze_plan_sha256")!=freeze
        or names!=["nyc","lax","ams","lhr","nrt","sgp"]):
    raise SystemExit("zero-progress resume authorization differs")
probe={"schema":rounds.ROUND_RESULT_SCHEMA,"capture_id":capture,
    "freeze_plan_sha256":freeze,"round_number":1,
    "round_authorization_sha256":auth_sha,"target_readiness":rounds.wrap(readiness),
    "transitions":[],"mutation_dispatch":rounds.wrap(dispatch),
    "remaining_target_inert_proofs":[],"remaining_targets":names,
    "completed_at":authorization["authorization_deadline"]}
rounds.validate_round_result(
    probe,authorization=authorization,prior_results=[],transition_receipts=[]
)
dispatch_fields={"schema","capture_id","freeze_plan_sha256","round_number",
    "round_authorization_sha256","round_readiness_sha256",
    "live_observation_selection_sha256","live_observation_generation",
    "observation_generation_receipt_sha256","drive_prefreeze_receipt_sha256",
    "targets","dispatched_at"}
try:
    dispatched=datetime.datetime.strptime(dispatch.get("dispatched_at"),"%Y-%m-%dT%H:%M:%SZ")
except (TypeError,ValueError):
    raise SystemExit("zero-progress resume dispatch time differs")
del dispatched
observation_identity=(authorization.get("live_observation_selection_sha256"),
    authorization.get("live_observation_generation"),
    authorization.get("observation_generation_receipt_sha256"),
    authorization.get("drive_prefreeze_receipt_sha256"))
if (set(dispatch)!=dispatch_fields
        or dispatch.get("schema")!="arc.recovery.quarantine-mutation-dispatch.v1"
        or (dispatch.get("capture_id"),dispatch.get("freeze_plan_sha256"),
            dispatch.get("round_number"),dispatch.get("round_authorization_sha256"),
            dispatch.get("round_readiness_sha256"))
            !=(capture,freeze,1,auth_sha,readiness_sha)
        or (dispatch.get("live_observation_selection_sha256"),
            dispatch.get("live_observation_generation"),
            dispatch.get("observation_generation_receipt_sha256"),
            dispatch.get("drive_prefreeze_receipt_sha256"))!=observation_identity
        or dispatch.get("targets")!=[
            {"node":row.get("node"),"host":row.get("host")} for row in targets]):
    raise SystemExit("zero-progress resume dispatch differs")
release_fields={"schema","capture_id","freeze_plan_sha256","round_number",
    "round_authorization_sha256","round_readiness_sha256","mutation_dispatch_sha256",
    "live_observation_selection_sha256","live_observation_generation",
    "observation_generation_receipt_sha256","drive_prefreeze_receipt_sha256",
    "challenge","released_at","nodes"}
challenge=release.get("challenge");nodes=release.get("nodes")
try:
    released=datetime.datetime.strptime(release.get("released_at"),"%Y-%m-%dT%H:%M:%SZ")
except (TypeError,ValueError):
    raise SystemExit("zero-progress resume release time differs")
del released
if (set(release)!=release_fields
        or release.get("schema")!="arc.recovery.quarantine-round-zero-progress-release.v1"
        or (release.get("capture_id"),release.get("freeze_plan_sha256"),
            release.get("round_number"),release.get("round_authorization_sha256"),
            release.get("round_readiness_sha256"),release.get("mutation_dispatch_sha256"))
            !=(capture,freeze,1,auth_sha,readiness_sha,dispatch_sha)
        or (release.get("live_observation_selection_sha256"),
            release.get("live_observation_generation"),
            release.get("observation_generation_receipt_sha256"),
            release.get("drive_prefreeze_receipt_sha256"))!=observation_identity
        or hash_re.fullmatch(str(challenge)) is None
        or not isinstance(nodes,list) or len(nodes)!=6):
    raise SystemExit("zero-progress resume release differs")
target_by_node={row["node"]:row for row in targets}
proof_fields={"schema","capture_id","freeze_plan_sha256","observation_generation",
    "round_number","round_authorization_sha256","round_readiness_sha256",
    "mutation_dispatch_sha256","challenge","node","boot_id","writer_live_unfenced",
    "apply_state_present","restart_effective_mutation_absent","active_selector_absent",
    "quarantine_nft_absent","authorization_accepted","readiness_present",
    "accepted_boottime_ns","elapsed_since_acceptance_ns","observed_boottime_ns","observed_at"}
for expected_node,wrapper in zip(names,nodes):
    proof=wrapper.get("value") if isinstance(wrapper,dict) else None
    if isinstance(proof,dict):
        accepted=proof.get("accepted_boottime_ns");elapsed=proof.get("elapsed_since_acceptance_ns")
        observed=proof.get("observed_boottime_ns")
        try:seen=datetime.datetime.strptime(proof.get("observed_at"),"%Y-%m-%dT%H:%M:%SZ")
        except (TypeError,ValueError):seen=None
    else:accepted=elapsed=observed=seen=None
    if (not isinstance(wrapper,dict) or set(wrapper)!={"value","sha256"}
            or not isinstance(proof,dict) or set(proof)!=proof_fields
            or wrapper.get("sha256")!=digest(canonical(proof))
            or proof.get("schema")!="arc.recovery.quarantine-round-zero-progress-node-proof.v1"
            or (proof.get("capture_id"),proof.get("freeze_plan_sha256"),
                proof.get("observation_generation"),proof.get("round_number"),
                proof.get("round_authorization_sha256"),proof.get("round_readiness_sha256"),
                proof.get("mutation_dispatch_sha256"),proof.get("challenge"),proof.get("node"))
                !=(capture,freeze,observation_identity[1],1,auth_sha,readiness_sha,
                   dispatch_sha,challenge,expected_node)
            or proof.get("boot_id")!=target_by_node[expected_node].get("boot_id")
            or any(proof.get(field) is not True for field in
                   ("writer_live_unfenced","restart_effective_mutation_absent",
                    "active_selector_absent","quarantine_nft_absent","authorization_accepted"))
            or not isinstance(proof.get("apply_state_present"),bool)
            or not isinstance(proof.get("readiness_present"),bool)
            or any(isinstance(number,bool) or not isinstance(number,int) or number<=0
                   for number in (accepted,elapsed,observed))
            or observed<=accepted+300_000_000_000 or elapsed!=observed-accepted
            or seen is None):
        raise SystemExit(f"zero-progress resume node proof differs: {expected_node}")
transitions=attempt/"node-transitions"
if transitions.exists() or transitions.is_symlink():
    details=transitions.lstat()
    if transitions.is_symlink() or not stat.S_ISDIR(details.st_mode):
        raise SystemExit("zero-progress resume transition root is unsafe")
    if any(transitions.iterdir()):raise SystemExit("zero-progress resume has a node transition")
result_path=attempt/"result.json"
if result_path.exists() or result_path.is_symlink():
    result,_result_raw=locked(result_path,"result")
    if result.get("transitions")!=[]:raise SystemExit("zero-progress resume result transitioned")
    rounds.validate_round_result(
        result,authorization=authorization,prior_results=[],transition_receipts=[]
    )
PY
}

quarantine_attempt_binds_live_observation_selection() {
    local attempt_root="$1" freeze_sha="$2" capture_id="$3"
    # Authorization, acceptances, and a locally sealed readiness are all
    # powerless crash prefixes: dispatch is published before readiness is sent
    # to any node.  Only dispatch/result/transition evidence can bind a stale
    # attempt to its observation selection.  A complete exact six-node
    # zero-progress release proves an otherwise-binding dispatch was inert.
    if quarantine_attempt_has_valid_zero_progress_release "$attempt_root" \
            "$freeze_sha" "$capture_id"; then
        return 1
    fi
    if [ -e "$attempt_root/mutation-dispatch.json" ] || \
            [ -L "$attempt_root/mutation-dispatch.json" ] || \
            [ -e "$attempt_root/result.json" ] || \
            [ -L "$attempt_root/result.json" ] || \
            find "$attempt_root/node-transitions" \
                \( -type f -o -type l \) -print -quit 2>/dev/null | grep -q .; then
        return 0
    fi
    return 1
}

complete_quarantine_round_attempt() {
    local freeze_plan="$1" freeze_sha="$2" capture_id="$3" round_number="$4"
    local round_root="$5" attempt_root="$6" log_root="$7"
    local inspector_binary_sha="$8" inspector_genesis_sha="$9"
    local inspector_validators_sha="${10}" inspector_legacy_validators_sha="${11}"
    local allow_unbound_legacy_wal="${12}"
    local observation_selection="${13}" observation_selection_sha="${14}"
    local observation_generation_receipt="${15}" observation_generation="${16}"
    local observation_generation_receipt_sha="${17}" drive_prefreeze_receipt_sha="${18}"
    local operator_selection_monotonic_ns="${19}" operator_selection_realtime_ns="${20}"
    local authorization="$attempt_root/authorization.json"
    local acceptance_root="$attempt_root/authorization-acceptances"
    local readiness="$attempt_root/readiness.json"
    local applied_root="$attempt_root/node-transitions"
    local mutation_dispatch="$attempt_root/mutation-dispatch.json"
    local result="$attempt_root/result.json"
    local auth_sha readiness_sha temporary targets node index failed applied_count
    local remaining_targets remaining_proof_root final_round_root
    local pids=() names=() remaining_names=() result_args=()
    quarantine_authorization_matches_live_observation "$authorization" \
        "$observation_selection" "$observation_selection_sha" \
        "$observation_generation_receipt" "$observation_generation" \
        "$observation_generation_receipt_sha" "$drive_prefreeze_receipt_sha" \
        "$freeze_sha" "$capture_id" || return 3
    auth_sha="$(round_artifact_sha "$authorization")"
    targets="$(python3 - "$authorization" <<'PY'
import json,pathlib,sys
value=json.loads(pathlib.Path(sys.argv[1]).read_text(encoding="utf-8"))
names=[row.get("node") for row in value.get("targets",[])]
if not names:raise SystemExit("round authorization target set is empty")
print(",".join(names))
PY
)"
    IFS=',' read -r -a names <<< "$targets"
    # A durable positive result is the terminal closure for this attempt.  Its
    # embedded BOOTTIME-expired proofs make an old nft apply impossible, but a
    # remaining live writer may legitimately stop later.  Never run another
    # old-attempt applied/stopped status probe after the result is sealed: that
    # could append a stopped transition and make the immutable result disagree
    # with its own crash-resume closure.  Validate the exact existing bytes,
    # finish the prefix copy, and let a later round own every remaining node.
    if [ -f "$result" ] && [ ! -L "$result" ]; then
        python3 -I "$QUARANTINE_ROUND_DRIVER" build-result \
            --round-number "$round_number" --round-root "$round_root" \
            --authorization "$authorization" --readiness "$readiness" \
            --dispatch "$mutation_dispatch" --applied-root "$applied_root" \
            --output "$result" >/dev/null
        if ! python3 - "$result" <<'PY'
import json,pathlib,sys
value=json.loads(pathlib.Path(sys.argv[1]).read_text(encoding="utf-8"))
raise SystemExit(0 if isinstance(value.get("transitions"),list) and value["transitions"] else 1)
PY
        then
            # Historical/diagnostic zero-progress results remain attempt-local.
            # They are never transition-ledger members and cannot authorize an
            # immutable positive prefix, even before their challenged release.
            return 2
        fi
        final_round_root="$round_root/round-$round_number"
        if [ ! -e "$final_round_root" ]; then
            mkdir -m 700 -- "$final_round_root"
        fi
        publish_canonical_maintenance_input "$authorization" \
            "$final_round_root/authorization.json"
        publish_canonical_maintenance_input "$result" \
            "$final_round_root/result.json"
        return 0
    fi
    [ ! -e "$result" ] && [ ! -L "$result" ] || \
        die "quarantine round result is unsafe"
    [ -e "$acceptance_root" ] || mkdir -m 700 -- "$acceptance_root"
    [ -e "$applied_root" ] || mkdir -m 700 -- "$applied_root"
    if [ -f "$readiness" ] && [ ! -L "$readiness" ]; then
        readiness_sha="$(round_artifact_sha "$readiness")"
        if [ -f "$mutation_dispatch" ] && [ ! -L "$mutation_dispatch" ]; then
            seal_quarantine_mutation_dispatch "$authorization" "$readiness" \
                "$mutation_dispatch" >/dev/null
        elif round_authorization_is_live "$authorization" && \
                { [ "$round_number" -ne 1 ] || \
                  operator_selection_window_is_live \
                    "$operator_selection_monotonic_ns" \
                    "$operator_selection_realtime_ns"; }; then
            seal_quarantine_mutation_dispatch "$authorization" "$readiness" \
                "$mutation_dispatch" >/dev/null
        fi
        for node in "${names[@]}"; do
            [ ! -f "$applied_root/$node.json" ] || continue
            temporary="$log_root/$node-round-$round_number-applied-status.new.json"
            if run_remote "$node" quarantine-round-applied-status "$capture_id" "$node" \
                    "$freeze_sha" "$round_number" "$auth_sha" "$readiness_sha" \
                    > "$temporary" 2> "$temporary.stderr"; then
                chmod 400 "$temporary"
                publish_canonical_maintenance_input "$temporary" "$applied_root/$node.json"
            else
                temporary="$log_root/$node-round-$round_number-stopped-precommit.new.json"
                if [ -f "$mutation_dispatch" ] && \
                        run_remote "$node" quarantine-round-stopped-precommit \
                        "$capture_id" "$node" "$freeze_sha" "$round_number" \
                        "$auth_sha" "$readiness_sha" "$inspector_binary_sha" \
                        "$inspector_genesis_sha" "$inspector_validators_sha" \
                        "$inspector_legacy_validators_sha" "$allow_unbound_legacy_wal" \
                        > "$temporary" 2> "$temporary.stderr"; then
                    chmod 400 "$temporary"
                    publish_canonical_maintenance_input "$temporary" \
                        "$applied_root/$node.json"
                fi
            fi
        done
    fi
    applied_count=0
    for node in "${names[@]}"; do [ ! -f "$applied_root/$node.json" ] || applied_count=$((applied_count + 1)); done
    if [ "$applied_count" -lt "${#names[@]}" ]; then
        if ! round_authorization_is_live "$authorization" || \
                { [ ! -f "$mutation_dispatch" ] && [ "$round_number" -eq 1 ] && \
                  ! operator_selection_window_is_live \
                    "$operator_selection_monotonic_ns" \
                    "$operator_selection_realtime_ns"; }; then
            if [ "$applied_count" -eq 0 ]; then
                return 2
            fi
        else
            pids=(); failed=0
            for node in "${names[@]}"; do
                [ -f "$acceptance_root/$node.json" ] && continue
                (
                    temporary="$log_root/$node-round-$round_number-acceptance.new.json"
                    if ! run_remote "$node" quarantine-round-authorization-status \
                            "$capture_id" "$node" "$freeze_sha" "$round_number" "$auth_sha" \
                            > "$temporary" 2> "$temporary.stderr"; then
                        run_remote_canonical_input "$node" "$authorization" \
                            quarantine-round-authorize "$capture_id" "$node" "$freeze_sha" \
                            "$round_number" "$auth_sha" > "$temporary"
                    fi
                    chmod 400 "$temporary"
                    publish_canonical_maintenance_input "$temporary" \
                        "$acceptance_root/$node.json"
                ) > "$log_root/$node-round-$round_number-authorize.log" 2>&1 &
                pids+=("$!")
            done
            for index in "${!pids[@]}"; do
                if ! wait "${pids[$index]}"; then failed=1; fi
            done
            [ "$failed" -eq 0 ] || die \
                "not every still-live target accepted the exact quarantine-round authorization"
            if [ ! -f "$readiness" ]; then
                if [ "$round_number" -eq 1 ]; then
                    python3 -I "$QUARANTINE_ROUND_DRIVER" build-readiness \
                        --round-number "$round_number" --round-root "$round_root" \
                        --authorization "$authorization" --acceptance-root "$acceptance_root" \
                        --operator-selection-monotonic-ns \
                            "$operator_selection_monotonic_ns" \
                        --operator-selection-realtime-ns \
                            "$operator_selection_realtime_ns" \
                        --output "$readiness" >/dev/null
                else
                    python3 -I "$QUARANTINE_ROUND_DRIVER" build-readiness \
                        --round-number "$round_number" --round-root "$round_root" \
                        --authorization "$authorization" --acceptance-root "$acceptance_root" \
                        --output "$readiness" >/dev/null
                fi
            fi
            readiness_sha="$(round_artifact_sha "$readiness")"
            seal_quarantine_mutation_dispatch "$authorization" "$readiness" \
                "$mutation_dispatch" >/dev/null
            pids=(); failed=0
            for node in "${names[@]}"; do
                (
                    temporary="$log_root/$node-round-$round_number-readiness.new.json"
                    run_remote_canonical_input "$node" "$readiness" quarantine-round-ready \
                        "$capture_id" "$node" "$freeze_sha" "$round_number" "$auth_sha" \
                        "$readiness_sha" > "$temporary"
                    chmod 400 "$temporary"
                    cmp --silent "$readiness" "$temporary" || \
                        die "remote quarantine-round readiness bytes differ for $node"
                ) > "$log_root/$node-round-$round_number-ready.log" 2>&1 &
                pids+=("$!")
            done
            for index in "${!pids[@]}"; do
                if ! wait "${pids[$index]}"; then failed=1; fi
            done
            [ "$failed" -eq 0 ] || die \
                "the exact all-target quarantine-round readiness was not durable everywhere"
            pids=()
            for node in "${names[@]}"; do
                [ ! -f "$applied_root/$node.json" ] || continue
                (
                    temporary="$log_root/$node-round-$round_number-applied.new.json"
                    run_remote "$node" quarantine-round-apply "$capture_id" "$node" \
                        "$freeze_sha" "$round_number" "$auth_sha" "$readiness_sha" \
                        > "$temporary"
                    chmod 400 "$temporary"
                    publish_canonical_maintenance_input "$temporary" "$applied_root/$node.json"
                ) > "$log_root/$node-round-$round_number-apply.log" 2>&1 &
                pids+=("$!")
            done
            # Individual failures are an expected partial-round state.  Do not
            # roll back a successful fence and do not authorize a late retry.
            for index in "${!pids[@]}"; do wait "${pids[$index]}" || true; done
        fi
    fi
    readiness_sha="$(round_artifact_sha "$readiness")"
    applied_count=0
    for node in "${names[@]}"; do [ ! -f "$applied_root/$node.json" ] || applied_count=$((applied_count + 1)); done
    if [ "$applied_count" -lt "${#names[@]}" ]; then
        # Do not sleep against the operator wall-clock deadline: a backward
        # step could turn a bounded node BOOTTIME lease into an unbounded local
        # wait.  Query immediately; an incomplete attempt is re-challenged for
        # zero progress on the next capture invocation.
        for node in "${names[@]}"; do
            [ ! -f "$applied_root/$node.json" ] || continue
            temporary="$log_root/$node-round-$round_number-final-applied-status.new.json"
            if run_remote "$node" quarantine-round-applied-status "$capture_id" "$node" \
                    "$freeze_sha" "$round_number" "$auth_sha" "$readiness_sha" \
                    > "$temporary" 2> "$temporary.stderr"; then
                chmod 400 "$temporary"
                publish_canonical_maintenance_input "$temporary" "$applied_root/$node.json"
            else
                temporary="$log_root/$node-round-$round_number-final-stopped-precommit.new.json"
                if run_remote "$node" quarantine-round-stopped-precommit \
                        "$capture_id" "$node" "$freeze_sha" "$round_number" \
                        "$auth_sha" "$readiness_sha" "$inspector_binary_sha" \
                        "$inspector_genesis_sha" "$inspector_validators_sha" \
                        "$inspector_legacy_validators_sha" "$allow_unbound_legacy_wal" \
                        > "$temporary" 2> "$temporary.stderr"; then
                    chmod 400 "$temporary"
                    publish_canonical_maintenance_input "$temporary" \
                        "$applied_root/$node.json"
                fi
            fi
        done
    fi
    applied_count=0
    remaining_names=()
    for node in "${names[@]}"; do
        if [ -f "$applied_root/$node.json" ] && [ ! -L "$applied_root/$node.json" ]; then
            applied_count=$((applied_count + 1))
        else
            remaining_names+=("$node")
        fi
    done
    if [ "$applied_count" -eq 0 ]; then
        return 2
    fi
    remaining_proof_root=""
    if [ "$applied_count" -lt "${#names[@]}" ] \
            && [ ! -f "$result" ] && [ ! -L "$result" ]; then
        remaining_targets="$(IFS=,; printf '%s' "${remaining_names[*]}")"
        if ! remaining_proof_root="$(capture_remaining_target_inert_proofs \
                "$freeze_plan" "$attempt_root" "$remaining_targets" "$log_root")"; then
            printf 'archive fleet: positive quarantine round %s remains open; at least one old-dispatch target is live, ambiguous, or inside its BOOTTIME lease\n' \
                "$round_number" >&2
            return 4
        fi
    fi
    result_args=(
        build-result --round-number "$round_number" --round-root "$round_root"
        --authorization "$authorization" --readiness "$readiness"
        --dispatch "$mutation_dispatch" --applied-root "$applied_root"
        --output "$result"
    )
    if [ -n "$remaining_proof_root" ]; then
        result_args+=(--remaining-proof-root "$remaining_proof_root")
    fi
    python3 -I "$QUARANTINE_ROUND_DRIVER" "${result_args[@]}" >/dev/null
    final_round_root="$round_root/round-$round_number"
    if [ ! -e "$final_round_root" ]; then
        mkdir -m 700 -- "$final_round_root"
    fi
    publish_canonical_maintenance_input "$authorization" \
        "$final_round_root/authorization.json"
    publish_canonical_maintenance_input "$result" "$final_round_root/result.json"
    return 0
}

run_quarantine_generation_rounds() {
    local freeze_plan="$1" freeze_sha="$2" capture_id="$3"
    local maintenance_input_root="$4" log_root="$5" output="$6"
    local inspector_binary_sha="$7" inspector_genesis_sha="$8"
    local inspector_validators_sha="$9" inspector_legacy_validators_sha="${10}"
    local allow_unbound_legacy_wal="${11}"
    local observation_selection="${12}" observation_selection_sha="${13}"
    local observation_generation_receipt="${14}" observation_generation="${15}"
    local observation_generation_receipt_sha="${16}" drive_prefreeze_receipt_sha="${17}"
    local operator_selection_monotonic_ns="${18}" operator_selection_realtime_ns="${19}"
    local source_main round_number round_dir attempt_root authorization
    local prior_status_root public_receipt status targets
    source_main="$(manifest_field "$freeze_plan" source_commit)"
    [ -f "$QUARANTINE_ROUND_DRIVER" ] && [ ! -L "$QUARANTINE_ROUND_DRIVER" ] || \
        die "quarantine round driver is missing or unsafe"
    [ -f "$QUARANTINE_ROUND_MODULE" ] && [ ! -L "$QUARANTINE_ROUND_MODULE" ] || \
        die "quarantine round validator is missing or unsafe"
    tracked_source_hash "$QUARANTINE_ROUND_DRIVER" >/dev/null
    tracked_source_hash "$QUARANTINE_ROUND_MODULE" >/dev/null
    # Re-read the immutable operator receipts immediately before the first
    # command that can quarantine a writer.  Supplied hashes alone are not an
    # observation provenance boundary.
    verify_live_observation_selection_exact "$observation_selection" \
        "$observation_selection_sha" "$observation_generation_receipt" \
        "$observation_generation" "$observation_generation_receipt_sha" \
        "$drive_prefreeze_receipt_sha" "$freeze_sha" "$capture_id"
    local round_root
    round_root="$(prepare_protected_maintenance_directory \
        "$maintenance_input_root/quarantine-rounds")"
    round_number=1
    while [ "$round_number" -le 6 ]; do
        round_dir="$round_root/round-$round_number"
        if [ -f "$round_dir/result.json" ] && [ ! -L "$round_dir/result.json" ]; then
            quarantine_round_remaining_targets "$round_root" "$round_number" >/dev/null
            round_number=$((round_number + 1))
            continue
        fi
        targets="$(quarantine_round_remaining_targets "$round_root" "$((round_number - 1))")"
        [ -n "$targets" ] || break
        for authorization in "$round_dir"/attempt.*/authorization.json; do
            [ -f "$authorization" ] && [ ! -L "$authorization" ] || continue
            attempt_root="${authorization%/authorization.json}"
            if ! quarantine_authorization_matches_live_observation "$authorization" \
                    "$observation_selection" "$observation_selection_sha" \
                    "$observation_generation_receipt" "$observation_generation" \
                    "$observation_generation_receipt_sha" "$drive_prefreeze_receipt_sha" \
                    "$freeze_sha" "$capture_id"; then
                if quarantine_attempt_binds_live_observation_selection \
                        "$attempt_root" "$freeze_sha" "$capture_id"; then
                    die "quarantine attempt progressed under another live-observation selection"
                fi
                printf 'archive fleet: ignoring unissued quarantine authorization from another live-observation selection\n'
                continue
            fi
            if complete_quarantine_round_attempt "$freeze_plan" "$freeze_sha" "$capture_id" \
                    "$round_number" "$round_root" "$attempt_root" "$log_root" \
                    "$inspector_binary_sha" "$inspector_genesis_sha" \
                    "$inspector_validators_sha" "$inspector_legacy_validators_sha" \
                    "$allow_unbound_legacy_wal" "$observation_selection" \
                    "$observation_selection_sha" "$observation_generation_receipt" \
                    "$observation_generation" "$observation_generation_receipt_sha" \
                    "$drive_prefreeze_receipt_sha" \
                    "$operator_selection_monotonic_ns" \
                    "$operator_selection_realtime_ns"; then
                break
            else
                status=$?
                if [ "$status" -eq 4 ]; then
                    die "positive quarantine round $round_number is awaiting exact remaining-target BOOTTIME closure"
                fi
                [ "$status" -eq 2 ] || die "quarantine round $round_number recovery failed"
            fi
        done
        if [ -f "$round_dir/result.json" ] && [ ! -L "$round_dir/result.json" ]; then
            printf 'archive fleet: recovered positive quarantine round %s\n' "$round_number"
            round_number=$((round_number + 1))
            continue
        fi
        if [ ! -e "$round_dir" ]; then
            mkdir -m 700 -- "$round_dir"
        fi
        attempt_root="$(mktemp -d "$round_dir/attempt.XXXXXX")"
        chmod 700 "$attempt_root"
        public_receipt="$attempt_root/target-public-height.json"
        python3 -B -I "$LEGACY_HEIGHT_TOOL" sample-targets \
            --source-main "$source_main" --freeze-plan "$freeze_plan" \
            --freeze-plan-sha256 "$freeze_sha" --targets "$targets" \
            --output "$public_receipt" --timeout-seconds 10 >/dev/null
        prior_status_root="$attempt_root/prior-fenced-status"
        mkdir -m 700 -- "$prior_status_root"
        capture_quarantine_round_prior_statuses "$round_root" "$((round_number - 1))" \
            "$freeze_sha" "$capture_id" "$prior_status_root" "$log_root"
        capture_quarantine_round_target_cross "$freeze_plan" "$freeze_sha" \
            "$capture_id" "$targets" "$public_receipt" "$attempt_root"
        capture_quarantine_round_live_sources "$freeze_plan" "$freeze_sha" \
            "$capture_id" "$round_number" "$targets" "$public_receipt" \
            "$attempt_root/authenticated-target-cross.json" "$attempt_root" \
            "$inspector_binary_sha" "$inspector_genesis_sha" \
            "$inspector_legacy_validators_sha" "$allow_unbound_legacy_wal"
        python3 -I "$QUARANTINE_ROUND_DRIVER" build-authorization \
            --freeze-plan "$freeze_plan" --freeze-plan-sha256 "$freeze_sha" \
            --capture-id "$capture_id" --round-number "$round_number" \
            --round-root "$round_root" --public "$public_receipt" \
            --cross "$attempt_root/authenticated-target-cross.json" \
            --prior-status-root "$prior_status_root" \
            --source-capture-root "$attempt_root/live-source-captures" \
            --live-observation-selection "$observation_selection" \
            --live-observation-selection-sha256 "$observation_selection_sha" \
            --output "$attempt_root/authorization.json" >/dev/null
        if complete_quarantine_round_attempt "$freeze_plan" "$freeze_sha" "$capture_id" \
                "$round_number" "$round_root" "$attempt_root" "$log_root" \
                "$inspector_binary_sha" "$inspector_genesis_sha" \
                "$inspector_validators_sha" "$inspector_legacy_validators_sha" \
                "$allow_unbound_legacy_wal" "$observation_selection" \
                "$observation_selection_sha" "$observation_generation_receipt" \
                "$observation_generation" "$observation_generation_receipt_sha" \
                "$drive_prefreeze_receipt_sha" \
                "$operator_selection_monotonic_ns" \
                "$operator_selection_realtime_ns"; then
            printf 'archive fleet: sealed positive quarantine transition round %s\n' \
                "$round_number"
            round_number=$((round_number + 1))
        else
            status=$?
            if [ "$status" -eq 4 ]; then
                die "positive quarantine round $round_number is awaiting exact remaining-target BOOTTIME closure"
            fi
            [ "$status" -eq 2 ] || die "fresh quarantine round failed"
            printf 'archive fleet: preserved zero-progress round attempt %s; resampling still-live targets\n' \
                "$round_number"
        fi
    done
    python3 -I "$QUARANTINE_ROUND_DRIVER" build-ledger \
        --round-root "$round_root" --freeze-plan-sha256 "$freeze_sha" \
        --capture-id "$capture_id" --output "$output" >/dev/null
    hash_file "$output"
}

reserve_quarantine_challenge() {
    local root="$1" freeze_sha="$2" capture_id="$3"
    python3 - "$root/quarantine-challenge.json" "$freeze_sha" "$capture_id" <<'PY'
import hashlib,json,os,pathlib,secrets,stat,sys
path=pathlib.Path(sys.argv[1]);freeze,capture=sys.argv[2:]
partial=path.with_name(path.name+".partial")
canonical=lambda value:(json.dumps(value,sort_keys=True,separators=(",",":"))+"\n").encode()
dfd=os.open(path.parent,os.O_RDONLY|getattr(os,"O_DIRECTORY",0)|getattr(os,"O_NOFOLLOW",0))
def load(name,modes):
    fd=os.open(name,os.O_RDONLY|getattr(os,"O_NOFOLLOW",0),dir_fd=dfd)
    try:
        details=os.fstat(fd)
        if (not stat.S_ISREG(details.st_mode) or details.st_uid!=os.geteuid()
                or details.st_nlink!=1 or stat.S_IMODE(details.st_mode) not in modes
                or details.st_size<=0 or details.st_size>4096):
            raise SystemExit("quarantine challenge file identity differs")
        raw=os.read(fd,4097)
        if len(raw)!=details.st_size:raise SystemExit("quarantine challenge changed while read")
        try:value=json.loads(raw)
        except (UnicodeDecodeError,json.JSONDecodeError):return None,raw
        return value,raw
    finally:os.close(fd)
def valid(value,raw):
    return (isinstance(value,dict) and set(value)=={"schema","freeze_plan_sha256","capture_id","challenge"}
        and raw==canonical(value) and value.get("schema")=="arc.recovery.legacy-network-quarantine-challenge.v1"
        and (value.get("freeze_plan_sha256"),value.get("capture_id"))==(freeze,capture)
        and isinstance(value.get("challenge"),str) and len(value["challenge"])==64
        and all(c in "0123456789abcdef" for c in value["challenge"]))
try:
    if path.exists() or path.is_symlink():
        value,raw=load(path.name,{0o400})
        if not valid(value,raw):raise SystemExit("existing quarantine challenge differs")
        if partial.exists() or partial.is_symlink():
            load(partial.name,{0o400,0o600});os.unlink(partial.name,dir_fd=dfd);os.fsync(dfd)
    else:
        value=None
        if partial.exists() or partial.is_symlink():
            candidate,candidate_raw=load(partial.name,{0o400,0o600})
            if valid(candidate,candidate_raw):
                os.chmod(partial.name,0o400,dir_fd=dfd,follow_symlinks=False)
                os.rename(partial.name,path.name,src_dir_fd=dfd,dst_dir_fd=dfd);os.fsync(dfd)
                value=candidate
            else:
                os.unlink(partial.name,dir_fd=dfd);os.fsync(dfd)
        if value is None:
            value={"schema":"arc.recovery.legacy-network-quarantine-challenge.v1",
                   "freeze_plan_sha256":freeze,"capture_id":capture,"challenge":secrets.token_hex(32)}
            raw=canonical(value)
            fd=os.open(partial.name,os.O_WRONLY|os.O_CREAT|os.O_EXCL|getattr(os,"O_NOFOLLOW",0),0o600,dir_fd=dfd)
            with os.fdopen(fd,"wb") as handle:
                handle.write(raw);handle.flush();os.fsync(handle.fileno());os.fchmod(handle.fileno(),0o400)
            os.rename(partial.name,path.name,src_dir_fd=dfd,dst_dir_fd=dfd);os.fsync(dfd)
finally:os.close(dfd)
challenge=value.get("challenge")
if not isinstance(challenge,str) or len(challenge)!=64 or any(c not in "0123456789abcdef" for c in challenge):
    raise SystemExit("quarantine challenge is malformed")
print(challenge)
PY
}

verify_quarantine_maintenance_inputs() {
    local root="$1" freeze_sha="$2" capture_id="$3" public_receipt="$4" challenge="$5"
    local stability_proof="$6"
    local generation_ledger="$7"
    verify_network_quarantine_stability_proof "$stability_proof" "$freeze_sha" \
        "$capture_id" "$challenge" "$generation_ledger" >/dev/null
    python3 - "$root" "$freeze_sha" "$capture_id" "$public_receipt" "$challenge" \
        "$generation_ledger" "$QUARANTINE_ROUND_MODULE" "${NODES[@]}" <<'PY'
import hashlib,json,os,pathlib,re,stat,sys
root=pathlib.Path(sys.argv[1]);freeze,capture,public_raw,challenge=sys.argv[2:6]
ledger_raw,rounds_module_raw=sys.argv[6:8]
fleet=[tuple(row.split("=",1)) for row in sys.argv[8:]]
canonical=lambda value:(json.dumps(value,sort_keys=True,separators=(",",":"))+"\n").encode()
hash_re=re.compile(r"[0-9a-f]{64}")
def locked(path,label):
    path=pathlib.Path(path);details=path.lstat();raw=path.read_bytes();value=json.loads(raw)
    if (path.is_symlink() or not stat.S_ISREG(details.st_mode) or details.st_uid!=os.geteuid()
            or details.st_nlink!=1 or stat.S_IMODE(details.st_mode)!=0o400 or raw!=canonical(value)):
        raise SystemExit(f"maintenance quarantine input is unsafe: {label}")
    return value,raw
challenge_receipt,_=locked(root/"quarantine-challenge.json","challenge")
if (challenge_receipt!={"schema":"arc.recovery.legacy-network-quarantine-challenge.v1",
        "freeze_plan_sha256":freeze,"capture_id":capture,"challenge":challenge}
        or hash_re.fullmatch(challenge) is None):
    raise SystemExit("maintenance quarantine challenge receipt differs")
public=json.loads(pathlib.Path(public_raw).read_bytes())
origins=public.get("origins")
if not isinstance(origins,list) or [row.get("name") for row in origins]!=[row[0] for row in fleet]:
    raise SystemExit("maintenance quarantine public topology differs")
ledger_path=pathlib.Path(ledger_raw);ledger_bytes=ledger_path.read_bytes();ledger=json.loads(ledger_bytes)
if (ledger_bytes!=canonical(ledger) or ledger.get("schema")!="arc.recovery.quarantine-generation-ledger.v2"
        or (ledger.get("freeze_plan_sha256"),ledger.get("capture_id"))!=(freeze,capture)):
    raise SystemExit("maintenance quarantine generation ledger differs")
transitions=[]
for round_wrapper in ledger.get("rounds",[]):
    for wrapper in round_wrapper.get("result",{}).get("value",{}).get("transitions",[]):
        item=wrapper.get("value",{});transitions.append((item.get("node"),item.get("schema")))
if len(transitions)!=len(fleet) or {row[0] for row in transitions}!={row[0] for row in fleet}:
    raise SystemExit("maintenance quarantine transition partition differs")
active_schema="arc.recovery.quarantine-node-nft-applied.v1"
stopped_schema="arc.recovery.quarantine-node-persistently-stopped-precommit.v1"
if any(schema not in {active_schema,stopped_schema} for _node,schema in transitions):
    raise SystemExit("maintenance quarantine transition kind differs")
active_names={node for node,schema in transitions if schema==active_schema}
for (node,host),origin in zip(fleet,origins):
    if node not in active_names:
        forbidden=[root/f"{node}-{suffix}.json" for suffix in (
            "status","post-proof-status","external-proof","public-cross-proof",
            "network-quarantine-receipt")]
        if any(path.exists() or path.is_symlink() for path in forbidden):
            raise SystemExit(f"stopped transition has active-quarantine evidence: {node}")
        continue
    status,status_raw=locked(root/f"{node}-status.json",f"{node} status")
    post,post_raw=locked(root/f"{node}-post-proof-status.json",f"{node} post status")
    external,external_raw=locked(root/f"{node}-external-proof.json",f"{node} external proof")
    cross,cross_raw=locked(root/f"{node}-public-cross-proof.json",f"{node} public proof")
    identity=(capture,node,freeze)
    status_fields={"schema","capture_id","node","freeze_plan_sha256","receipt_sha256",
        "table","rule_counters","counter_snapshot_sha256","owned_ruleset_stateless_sha256",
        "listener_inventory","loopback_head","quarantine_policy","active","enabled"}
    for value,label in ((status,"status"),(post,"post status")):
        if (set(value)!=status_fields
                or value.get("schema")!="arc.recovery.legacy-network-quarantine-status.v1"
                or (value.get("capture_id"),value.get("node"),value.get("freeze_plan_sha256"))!=identity
                or value.get("active") is not True or value.get("enabled") is not True):
            raise SystemExit(f"maintenance quarantine {label} identity differs: {node}")
    if status.get("receipt_sha256")!=post.get("receipt_sha256"):
        raise SystemExit(f"maintenance quarantine receipt changed: {node}")
    external_fields={"schema","capture_id","node","host","freeze_plan_sha256","challenge",
        "started_at","completed_at","operator_source_address","listener_inventory","targets",
        "results","network_quarantine_receipt_sha256","before_status_sha256",
        "after_status_sha256","after_status","deny_counter","ssh_status_reproved",
        "global_absence_claimed"}
    if (set(external)!=external_fields
            or external.get("schema")!="arc.recovery.legacy-network-quarantine-external-proof.v1"
            or (external.get("capture_id"),external.get("node"),external.get("freeze_plan_sha256"))!=identity
            or external.get("host")!=host or external.get("challenge")!=challenge
            or external.get("before_status_sha256")!=hashlib.sha256(status_raw).hexdigest()
            or external.get("network_quarantine_receipt_sha256")!=status.get("receipt_sha256")
            or external.get("ssh_status_reproved") is not True
            or external.get("global_absence_claimed") is not False):
        raise SystemExit(f"maintenance quarantine external proof differs: {node}")
    embedded_after=external.get("after_status")
    if (not isinstance(embedded_after,dict) or set(embedded_after)!=status_fields
            or hashlib.sha256(canonical(embedded_after)).hexdigest()!=external.get("after_status_sha256")
            or embedded_after.get("receipt_sha256")!=status.get("receipt_sha256")
            or (embedded_after.get("capture_id"),embedded_after.get("node"),
                embedded_after.get("freeze_plan_sha256"))!=identity
            or embedded_after.get("active") is not True or embedded_after.get("enabled") is not True):
        raise SystemExit(f"maintenance quarantine embedded after-status differs: {node}")
    targets=external.get("targets");results=external.get("results")
    if (not isinstance(targets,dict) or set(targets)!={"tcp","udp"}
            or not isinstance(results,list)
            or [(row.get("protocol"),row.get("port")) for row in results]
                !=([("tcp",port) for port in targets["tcp"]]
                   +[("udp",port) for port in targets["udp"]])):
        raise SystemExit(f"maintenance quarantine external targets differ: {node}")
    payload_sha=hashlib.sha256(bytes.fromhex(challenge)).hexdigest()
    for result in results:
        if result["protocol"]=="tcp":
            if (set(result)!={"protocol","port","connect_succeeded","connect_errno"}
                    or result.get("connect_succeeded") is not False
                    or not isinstance(result.get("connect_errno"),int)
                    or isinstance(result.get("connect_errno"),bool)
                    or result["connect_errno"] in {0,61,111}):
                raise SystemExit(f"maintenance quarantine TCP drop proof differs: {node}")
        elif (set(result)!={"protocol","port","payload_sha256","bytes_sent"}
                or result.get("payload_sha256")!=payload_sha or result.get("bytes_sent")!=32):
            raise SystemExit(f"maintenance quarantine UDP payload proof differs: {node}")
    cross_fields={"schema","capture_id","node","freeze_plan_sha256","challenge",
        "network_quarantine_receipt_sha256","quarantine_status_sha256","quarantine_status",
        "rule_counters","public_info_after_block","public_latest_block","fenced_head",
        "fenced_head_covers_public_info_after","public_latest_hash_matches","global_absence_claimed"}
    if (set(cross)!=cross_fields
            or cross.get("schema")!="arc.recovery.legacy-network-quarantine-public-cross-proof.v1"
            or (cross.get("capture_id"),cross.get("node"),cross.get("freeze_plan_sha256"))!=identity
            or cross.get("challenge")!=challenge
            or cross.get("network_quarantine_receipt_sha256")!=status.get("receipt_sha256")
            or cross.get("quarantine_status_sha256")
                !=hashlib.sha256(canonical(cross.get("quarantine_status"))).hexdigest()
            or cross.get("rule_counters")!=cross.get("quarantine_status",{}).get("rule_counters")
            or cross.get("fenced_head_covers_public_info_after") is not True
            or cross.get("public_latest_hash_matches") is not True
            or cross.get("global_absence_claimed") is not False):
        raise SystemExit(f"maintenance quarantine public cross-proof differs: {node}")
    public_after=cross.get("public_info_after_block",{});public_latest=cross.get("public_latest_block",{})
    for value,label,allowed in ((public_after,"public after",{"height","block_hash","state_root","response_sha256"}),
                        (public_latest,"public latest",{"height","block_hash","state_root","response_sha256"}),
                        (cross.get("fenced_head",{}),"fenced head",{"height","block_hash","state_root"})):
        if (set(value)!=allowed
                or isinstance(value.get("height"),bool) or not isinstance(value.get("height"),int)
                or value["height"]<0 or hash_re.fullmatch(str(value.get("block_hash"))) is None
                or hash_re.fullmatch(str(value.get("state_root"))) is None
                or ("response_sha256" in allowed
                    and hash_re.fullmatch(str(value.get("response_sha256"))) is None)):
            raise SystemExit(f"maintenance quarantine {label} tuple differs: {node}")
    if (public_after["height"]!=origin.get("info_after_height")
            or public_latest["height"]!=origin.get("latest_block_height")
            or public_latest["block_hash"]!=origin.get("latest_block_hash")):
        raise SystemExit(f"maintenance quarantine public receipt cross-binding differs: {node}")
PY
}

capture_phase() {
    # Plan mode initializes both SSH and Drive transports.  Register cleanup
    # before parsing or validating anything so OAuth/identity copies never
    # survive a successful plan or a fail-closed exit.
    begin_temporary_scope
    local freeze_plan="" offline_stop_output="" legacy_height_receipt=""
    local legacy_height_receipt_sha="" legacy_height_sample_output=""
    local inspector_binary="" inspector_binary_sha=""
    local inspector_genesis="" inspector_genesis_sha="" inspector_validators=""
    local inspector_validators_sha="" inspector_legacy_validators=""
    local inspector_legacy_validators_sha="" execute=false
    local allow_unbound_legacy_wal=false
    while [ "$#" -gt 0 ]; do
        case "$1" in
            --freeze-plan) [ "$#" -ge 2 ] || die "--freeze-plan needs a value"; freeze_plan="$2"; shift 2 ;;
            --offline-stop-evidence-output) [ "$#" -ge 2 ] || die "--offline-stop-evidence-output needs a value"; offline_stop_output="$2"; shift 2 ;;
            --legacy-public-height-receipt) [ "$#" -ge 2 ] || die "--legacy-public-height-receipt needs a value"; legacy_height_receipt="$2"; shift 2 ;;
            --legacy-public-height-receipt-sha256) [ "$#" -ge 2 ] || die "--legacy-public-height-receipt-sha256 needs a value"; legacy_height_receipt_sha="$2"; shift 2 ;;
            --sample-legacy-public-height-output) [ "$#" -ge 2 ] || die "--sample-legacy-public-height-output needs a value"; legacy_height_sample_output="$2"; shift 2 ;;
            --inspector-binary) [ "$#" -ge 2 ] || die "--inspector-binary needs a value"; inspector_binary="$2"; shift 2 ;;
            --inspector-binary-sha256) [ "$#" -ge 2 ] || die "--inspector-binary-sha256 needs a value"; inspector_binary_sha="$2"; shift 2 ;;
            --genesis) [ "$#" -ge 2 ] || die "--genesis needs a value"; inspector_genesis="$2"; shift 2 ;;
            --genesis-sha256) [ "$#" -ge 2 ] || die "--genesis-sha256 needs a value"; inspector_genesis_sha="$2"; shift 2 ;;
            --validator-public-keys) [ "$#" -ge 2 ] || die "--validator-public-keys needs a value"; inspector_validators="$2"; shift 2 ;;
            --validator-public-keys-sha256) [ "$#" -ge 2 ] || die "--validator-public-keys-sha256 needs a value"; inspector_validators_sha="$2"; shift 2 ;;
            --legacy-validator-set) [ "$#" -ge 2 ] || die "--legacy-validator-set needs a value"; inspector_legacy_validators="$2"; shift 2 ;;
            --legacy-validator-set-sha256) [ "$#" -ge 2 ] || die "--legacy-validator-set-sha256 needs a value"; inspector_legacy_validators_sha="$2"; shift 2 ;;
            --allow-unbound-legacy-wal) allow_unbound_legacy_wal=true; shift ;;
            --execute) execute=true; shift ;;
            --plan) execute=false; shift ;;
            -h|--help) usage; return 0 ;;
            *) die "unknown capture option: $1" ;;
        esac
    done
    [ -n "$freeze_plan" ] || die "--freeze-plan is required"
    if [ -n "$legacy_height_sample_output" ]; then
        [ -z "$legacy_height_receipt" ] && [ -z "$legacy_height_receipt_sha" ] || die \
            "--sample-legacy-public-height-output is mutually exclusive with an existing receipt/hash"
        legacy_height_receipt="$legacy_height_sample_output"
    else
        [ -n "$legacy_height_receipt" ] || die \
            "capture requires --sample-legacy-public-height-output or --legacy-public-height-receipt"
        require_hash "$legacy_height_receipt_sha" "legacy public-height receipt hash"
        require_absolute_file "$legacy_height_receipt" "legacy public-height receipt"
    fi
    local inspector_path inspector_expected
    for inspector_path in "$inspector_binary" "$inspector_genesis" \
        "$inspector_validators" "$inspector_legacy_validators"; do
        [ -n "$inspector_path" ] || die \
            "capture requires --inspector-binary, --genesis, --validator-public-keys, and --legacy-validator-set"
        require_absolute_file "$inspector_path" "capture recovery-export input"
    done
    for inspector_expected in "$inspector_binary_sha" "$inspector_genesis_sha" \
        "$inspector_validators_sha" "$inspector_legacy_validators_sha"; do
        require_hash "$inspector_expected" "capture recovery-export input hash"
    done
    [ "$(hash_file "$inspector_binary")" = "$inspector_binary_sha" ] || \
        die "capture inspector binary differs from its explicit hash"
    [ "$(hash_file "$inspector_genesis")" = "$inspector_genesis_sha" ] || \
        die "capture inspector genesis differs from its explicit hash"
    [ "$(hash_file "$inspector_validators")" = "$inspector_validators_sha" ] || \
        die "capture inspector validator-public-keys differs from its explicit hash"
    [ "$(hash_file "$inspector_legacy_validators")" = "$inspector_legacy_validators_sha" ] || \
        die "capture inspector legacy-validator-set differs from its explicit hash"
    [ -n "$offline_stop_output" ] || offline_stop_output="${freeze_plan}.offline-stop-evidence.json"
    case "$offline_stop_output" in /*.json) ;; *) die "offline-stop evidence output must be an absolute .json path" ;; esac
    if [ -n "$legacy_height_sample_output" ]; then
        case "$legacy_height_receipt" in
            "$offline_stop_output"|"$offline_stop_output".*) die \
                "late legacy public-height output collides with the offline-stop evidence namespace" ;;
        esac
    fi
    local legacy_height_cross_proof="${offline_stop_output}.authenticated-height-cross-proof.json"
    local legacy_height_cross_partial="${legacy_height_cross_proof}.partial"
    configure_operator_transport true
    if [ -n "$legacy_height_sample_output" ]; then
        validate_legacy_public_height_sample_output "$legacy_height_receipt" >/dev/null
    fi
    require_commands python3 ssh scp grep git mktemp
    [ -x "$REMOTE_HELPER" ] || die "remote helper is missing or not executable"
    [ -x "$DRIVE_PREFREEZE_GATE" ] || die "Drive prefreeze gate is missing or not executable"
    OPERATOR_FREEZE_PLAN="$freeze_plan"
    ARCHIVE_FLEET_PINNED_ROOT="$(mktemp -d "${TMPDIR:-/tmp}/arc-freeze-plan.XXXXXX")"
    [ -d "$ARCHIVE_FLEET_PINNED_ROOT" ] && [ ! -L "$ARCHIVE_FLEET_PINNED_ROOT" ] || \
        die "cannot create private freeze-plan snapshot root"
    chmod 700 "$ARCHIVE_FLEET_PINNED_ROOT"
    freeze_plan="$(pin_freeze_plan "$OPERATOR_FREEZE_PLAN" "$ARCHIVE_FLEET_PINNED_ROOT")"
    local freeze_sha capture_id
    freeze_sha="$(freeze_plan_hash "$freeze_plan")"
    capture_id="$(capture_id_for_freeze_plan_hash "$freeze_sha")"
    if [ -n "$legacy_height_sample_output" ]; then
        local late_height_output_state
        printf '%s\n' "${legacy_height_receipt##*/}" | grep -Eq \
            "^legacy-public-height\\.${capture_id}\\.[0-9a-f]{32}\\.json$" || die \
            "late legacy public-height output must be capture-scoped with a unique 32-hex nonce"
        late_height_output_state="$(validate_legacy_public_height_sample_output \
            "$legacy_height_receipt")"
        if [ -e "$legacy_height_cross_proof" ] || [ -L "$legacy_height_cross_proof" ]; then
            [ "$late_height_output_state" = sealed ] || die \
                "authenticated legacy-height cross-proof exists without its selected receipt"
            legacy_height_receipt_sha="$(sealed_legacy_public_height_receipt_sha \
                "$legacy_height_receipt")"
            validate_durable_legacy_height_cross_proof "$legacy_height_cross_proof" \
                "$freeze_sha" "$capture_id" "$legacy_height_receipt_sha"
        else
            [ "$late_height_output_state" = absent ] || die \
                "unselected late legacy public-height receipt exists; preserve it and choose a new unique output"
            [ ! -e "$legacy_height_cross_partial" ] && \
                [ ! -L "$legacy_height_cross_partial" ] || die \
                "partial authenticated legacy-height proof exists; preserve this namespace and choose a new offline-stop output"
        fi
    fi
    printf 'ARC staged legacy freeze plan\n'
    printf '  freeze:   %s\n' "$freeze_sha"
    printf '  capture:  %s\n' "$capture_id"
    printf '  quarantine: crash-safe fresh mixed-state rounds before any TERM/thaw\n'
    printf '  stops:      all six quarantined writers concurrently; no global halt/absence claim\n'
    REMOTE_HELPER_SHA="$(manifest_field "$freeze_plan" remote_helper_sha256)"
    require_hash "$REMOTE_HELPER_SHA" "sealed remote helper hash"
    REMOTE_HELPER_PATH="/root/.arc-recovery-helpers/$REMOTE_HELPER_SHA/archive-node.sh"
    run_drive_prefreeze_gate preflight "$freeze_plan" "$freeze_sha" "$capture_id"
    if [ "$execute" != true ]; then
        printf 'archive fleet: PLAN ONLY; no persistent service or recovery-managed remote/local file was changed\n'
        return 0
    fi
    local expected_go="FREEZE $freeze_sha CAPTURE $capture_id"
    [ "${ARC_RECOVERY_FREEZE_GO:-}" = "$expected_go" ] || \
        die "execution requires ARC_RECOVERY_FREEZE_GO='$expected_go'"

    [ "$(freeze_plan_hash "$freeze_plan")" = "$freeze_sha" ] || \
        die "freeze plan or source bindings changed before execution"
    # Pin the sampler and its local imports into this invocation's private
    # root. Both sampling and verification execute these immutable bytes with
    # bytecode generation disabled.
    LEGACY_HEIGHT_TOOL="$(pin_legacy_public_height_toolchain \
        "$ARCHIVE_FLEET_PINNED_ROOT/legacy-height-toolchain")"
    if [ -f "$legacy_height_receipt" ] && [ ! -L "$legacy_height_receipt" ]; then
        local actual_legacy_height_receipt_sha
        actual_legacy_height_receipt_sha="$(sealed_legacy_public_height_receipt_sha \
            "$legacy_height_receipt")"
        if [ -n "$legacy_height_sample_output" ]; then
            legacy_height_receipt_sha="$actual_legacy_height_receipt_sha"
        else
            [ "$actual_legacy_height_receipt_sha" = "$legacy_height_receipt_sha" ] || die \
                "legacy public-height receipt differs from its explicit hash"
        fi
        validate_intrinsic_legacy_public_height_receipt "$legacy_height_receipt" \
            "$legacy_height_receipt_sha" "$freeze_plan" "$freeze_sha"
    fi
    install_helpers "$(manifest_field "$freeze_plan" remote_helper_sha256)"
    assert_pinned_freeze_bytes "$freeze_plan" "$freeze_sha"
    install_freeze_plan "$freeze_plan" "$freeze_sha"
    local inspector_stage_root
    inspector_stage_root="$(mktemp -d)"
    ARCHIVE_FLEET_TEMP_ROOT="$inspector_stage_root"
    local inspector_stage_pids=() inspector_stage_nodes=() inspector_stage_failed=0
    local inspector_stage_index inspector_stage_node
    for inspector_stage_node in nyc lax ams lhr nrt sgp; do
        (
            stage_capture_inspector_inputs "$inspector_stage_node" "$freeze_sha" \
                "$inspector_binary" "$inspector_binary_sha" \
                "$inspector_genesis" "$inspector_genesis_sha" \
                "$inspector_validators" "$inspector_validators_sha" \
                "$inspector_legacy_validators" "$inspector_legacy_validators_sha"
        ) > "$inspector_stage_root/$inspector_stage_node.log" 2>&1 &
        inspector_stage_pids+=("$!")
        inspector_stage_nodes+=("$inspector_stage_node")
    done
    for inspector_stage_index in "${!inspector_stage_pids[@]}"; do
        if ! wait "${inspector_stage_pids[$inspector_stage_index]}"; then
            sed -n '1,80p' \
                "$inspector_stage_root/${inspector_stage_nodes[$inspector_stage_index]}.log" >&2
            inspector_stage_failed=1
        fi
    done
    [ "$inspector_stage_failed" -eq 0 ] || \
        die "exact capture recovery-export inputs were not staged on all six writers"
    find "$inspector_stage_root" -depth -delete
    ARCHIVE_FLEET_TEMP_ROOT=""
    printf 'archive fleet: staged exact hash-bound v0.8 recovery exporter inputs on all six writers\n'
    printf 'archive fleet: running exact ARC Drive identity/capacity/write-read-delete gate\n'
    local drive_execute_output drive_prefreeze_receipt
    drive_execute_output="$(run_drive_prefreeze_gate execute "$freeze_plan" "$freeze_sha" "$capture_id")"
    printf '%s\n' "$drive_execute_output"
    drive_prefreeze_receipt="$(printf '%s\n' "$drive_execute_output" | tail -n 1)"
    require_absolute_file "$drive_prefreeze_receipt" "Drive prefreeze execute receipt"
    local log_root
    log_root="$(mktemp -d)"
    ARCHIVE_FLEET_TEMP_ROOT="$log_root"
    local maintenance_input_root
    maintenance_input_root="$(prepare_protected_maintenance_directory \
        "${offline_stop_output}.maintenance-inputs")"
    # Serialize the complete capture-wide selection -> authorization boundary
    # independently of caller-selected output paths.  The fixed, uid-scoped
    # /tmp root is owner-only beneath the root-owned sticky directory; the
    # capture-id child is the lock inode on both Darwin and Linux.
    local capture_state_lock_dir
    capture_state_lock_dir="$(python3 - "$capture_id" <<'PY'
import os,pathlib,re,stat,sys
capture=sys.argv[1]
if re.fullmatch(r"[0-9a-f]{64}",capture) is None:raise SystemExit("capture lock id is malformed")
tmp_entry=pathlib.Path("/tmp")
tmp=pathlib.Path(os.path.realpath(tmp_entry));details=tmp.lstat()
if (not tmp.is_absolute() or tmp.is_symlink() or not stat.S_ISDIR(details.st_mode) or details.st_uid!=0
        or not details.st_mode&stat.S_ISVTX):
    raise SystemExit("fixed capture-lock parent is unsafe")
base=tmp/f"arc-recovery-capture-locks-{os.geteuid()}"
for path in (base,base/capture):
    try:os.mkdir(path,0o700)
    except FileExistsError:pass
    details=path.lstat()
    if (path.is_symlink() or not stat.S_ISDIR(details.st_mode)
            or details.st_uid!=os.geteuid() or stat.S_IMODE(details.st_mode)!=0o700):
        raise SystemExit("capture-lock directory identity differs")
print(base/capture)
PY
)"
    # FD 8 is unused by the orchestrator (FD 9 is reserved inside the remote
    # helper wrapper) and is compatible with the operator's macOS Bash 3.2.
    exec 8<"$capture_state_lock_dir"
    python3 - 8 <<'PY'
import errno,fcntl,sys
descriptor=int(sys.argv[1])
try:fcntl.flock(descriptor,fcntl.LOCK_EX|fcntl.LOCK_NB)
except OSError as error:
    if error.errno in {errno.EACCES,errno.EAGAIN}:
        raise SystemExit("another capture/quarantine process owns this recovery boundary")
    raise
PY
    local observation_generation_root="${offline_stop_output}.live-observation-generations"
    local observation_selection="${offline_stop_output}.live-observation-selection.json"
    local observation_selection_archive="${offline_stop_output}.live-observation-selections"
    local quarantine_round_root="$maintenance_input_root/quarantine-rounds"
    local observation_generation observation_generation_receipt
    local observation_generation_receipt_sha drive_prefreeze_receipt_sha
    local observation_resume_state prior_observation_generation selection_state
    drive_prefreeze_receipt_sha="$(hash_file "$drive_prefreeze_receipt")"
    reconcile_local_create_only_resume_links "$observation_selection" \
        "$maintenance_input_root"
    release_stale_zero_progress_dispatches "$freeze_plan" "$observation_selection" \
        "$quarantine_round_root" "$drive_prefreeze_receipt_sha" "$freeze_sha" \
        "$capture_id" "$log_root"
    read -r selection_state prior_observation_generation < <(
        live_observation_selection_resume_state "$observation_selection" \
            "$quarantine_round_root" "$drive_prefreeze_receipt_sha" \
            "$freeze_sha" "$capture_id"
    )
    case "$selection_state" in
        bound)
            observation_resume_state=bound
            printf 'archive fleet: resuming immutable live-observation selection already bound to quarantine dispatch\n'
            ;;
        absent)
            observation_resume_state=unbound
            ;;
        rotate)
            # Selection replacement is allowed only before any mutation intent
            # and only while every exact sealed writer remains live/unfenced.
            for observation_node in nyc lax ams lhr nrt sgp; do
                run_live_observations_eligibility_exact "$freeze_plan" "$freeze_sha" \
                    "$capture_id" "$prior_observation_generation" "$observation_node" \
                    >/dev/null || die \
                    "stale live-observation selection cannot rotate after a writer stopped/fenced"
            done
            archive_stale_live_observation_selection "$observation_selection" \
                "$observation_selection_archive" "$prior_observation_generation"
            observation_resume_state=unbound
            printf 'archive fleet: versioned stale unbound live-observation selection; all writers re-proved live/unfenced\n'
            ;;
        *) die "unknown live-observation selection resume state: $selection_state" ;;
    esac
    read -r observation_generation observation_generation_receipt \
        observation_generation_receipt_sha drive_prefreeze_receipt_sha < <(
        reserve_live_observation_generation "$observation_generation_root" \
            "$observation_selection" "$drive_prefreeze_receipt" "$freeze_plan" \
            "$freeze_sha" "$capture_id" "$observation_resume_state"
    )
    require_hash "$observation_generation" "live-observation generation"
    require_absolute_file "$observation_generation_receipt" "live-observation generation receipt"
    require_hash "$observation_generation_receipt_sha" "live-observation generation receipt hash"
    require_hash "$drive_prefreeze_receipt_sha" "Drive prefreeze receipt hash"
    [ "$(hash_file "$observation_generation_receipt")" = "$observation_generation_receipt_sha" ] || \
        die "live-observation generation receipt changed before capture"
    local live_observation_statuses="$log_root/live-observation-statuses.jsonl"
    printf 'archive fleet: capturing exactly three bounded loopback live-observation endpoints on every legacy writer generation=%s\n' \
        "$observation_generation"
    capture_all_live_observations "$freeze_plan" "$freeze_sha" "$capture_id" \
        "$observation_generation" "$observation_generation_receipt" \
        "$observation_generation_receipt_sha" \
        "$drive_prefreeze_receipt_sha" "$log_root" "$live_observation_statuses"
    assert_pinned_freeze_bytes "$freeze_plan" "$freeze_sha"
    local first_quarantine_started_at all_controlled_stopped_at
    local first_boundary_path="${offline_stop_output}.first-quarantine-started.json"
    if [ -e "$legacy_height_cross_proof" ] || [ -L "$legacy_height_cross_proof" ]; then
        if [ -n "$legacy_height_sample_output" ]; then
            [ "$(validate_legacy_public_height_sample_output "$legacy_height_receipt")" = sealed ] || \
                die "selected late legacy public-height receipt is missing or unsafe"
            legacy_height_receipt_sha="$(sealed_legacy_public_height_receipt_sha \
                "$legacy_height_receipt")"
        fi
        validate_durable_legacy_height_cross_proof "$legacy_height_cross_proof" \
            "$freeze_sha" "$capture_id" "$legacy_height_receipt_sha"
        printf 'archive fleet: reusing the durable authenticated pre-quarantine height proof\n'
    else
        [ ! -e "$first_boundary_path" ] && [ ! -L "$first_boundary_path" ] || die \
            "first quarantine boundary exists without its durable authenticated height proof"
        [ ! -e "$legacy_height_cross_partial" ] && \
            [ ! -L "$legacy_height_cross_partial" ] || die \
            "partial authenticated legacy-height proof must remain in an abandoned offline-output namespace"
        if [ -n "$legacy_height_sample_output" ]; then
            printf 'archive fleet: sampling all six public heights after expensive pre-freeze staging\n'
            legacy_height_receipt_sha="$(sample_legacy_public_height_late \
                "$freeze_plan" "$freeze_sha" "$legacy_height_receipt")"
            printf 'archive fleet: sealed late public-height receipt path=%s sha256=%s\n' \
                "$legacy_height_receipt" "$legacy_height_receipt_sha"
        fi
        printf 'archive fleet: cross-proving public height samples against every exact SSH-authenticated loopback writer\n'
        capture_authenticated_legacy_height_cross_proof "$freeze_plan" "$freeze_sha" \
            "$capture_id" "$legacy_height_receipt" "$legacy_height_receipt_sha" \
            "$legacy_height_cross_proof" "$log_root"
    fi
    local quarantine_root
    quarantine_root="$(prepare_protected_maintenance_directory \
        "$maintenance_input_root/network-quarantine")"
    local quarantine_generation_ledger="$maintenance_input_root/quarantine-generation-ledger.json"
    local quarantine_generation_ledger_sha
    local observation_selection_sha operator_selection_monotonic_ns
    local operator_selection_realtime_ns
    observation_selection_sha="$(seal_live_observation_selection "$observation_selection" \
        "$observation_generation_receipt" "$live_observation_statuses" \
        "$freeze_sha" "$capture_id")"
    require_hash "$observation_selection_sha" "live-observation selection hash"
    if [ "$observation_resume_state" = unbound ]; then
        read -r operator_selection_monotonic_ns operator_selection_realtime_ns < <(
            python3 - <<'PY'
import time
print(time.monotonic_ns(),time.time_ns())
PY
        )
        require_uint "$operator_selection_monotonic_ns" \
            "operator live-observation monotonic marker"
        require_uint "$operator_selection_realtime_ns" \
            "operator live-observation realtime marker"
    else
        operator_selection_monotonic_ns=-
        operator_selection_realtime_ns=-
    fi
    printf 'archive fleet: selected fresh canary-bound live-observation generation %s root=%s\n' \
        "$observation_generation" "$observation_selection_sha"
    quarantine_generation_ledger_sha="$(run_quarantine_generation_rounds \
        "$freeze_plan" "$freeze_sha" "$capture_id" "$maintenance_input_root" \
        "$log_root" "$quarantine_generation_ledger" "$inspector_binary_sha" \
        "$inspector_genesis_sha" "$inspector_validators_sha" \
        "$inspector_legacy_validators_sha" "$allow_unbound_legacy_wal" \
        "$observation_selection" "$observation_selection_sha" \
        "$observation_generation_receipt" "$observation_generation" \
        "$observation_generation_receipt_sha" "$drive_prefreeze_receipt_sha" \
        "$operator_selection_monotonic_ns" \
        "$operator_selection_realtime_ns")"
    require_hash "$quarantine_generation_ledger_sha" "quarantine generation ledger root"
    local active_quarantine_nodes=() stopped_quarantine_nodes=() transition_kind
    for quarantine_node in nyc lax ams lhr nrt sgp; do
        transition_kind="$(python3 -I "$QUARANTINE_ROUND_DRIVER" extract \
            --ledger "$quarantine_generation_ledger" --node "$quarantine_node" \
            --kind transition-kind)"
        if [ "$transition_kind" = network-quarantine-active ]; then
            active_quarantine_nodes+=("$quarantine_node")
        elif [ "$transition_kind" = persistently-stopped-precommit ]; then
            stopped_quarantine_nodes+=("$quarantine_node")
        else
            die "unknown quarantine transition kind for $quarantine_node: $transition_kind"
        fi
    done
    python3 -I "$QUARANTINE_ROUND_DRIVER" build-first-boundary \
        --ledger "$quarantine_generation_ledger" \
        --live-observation-selection "$observation_selection" \
        --live-observation-selection-sha256 "$observation_selection_sha" \
        --output "$first_boundary_path" >/dev/null
    first_quarantine_started_at="$(python3 - "$first_boundary_path" <<'PY'
import json,pathlib,sys
print(json.loads(pathlib.Path(sys.argv[1]).read_text(encoding="utf-8"))["timestamp"])
PY
)"
    local quarantine_node quarantine_index quarantine_failed=0
    local quarantine_pids=() quarantine_nodes=()
    for quarantine_node in "${active_quarantine_nodes[@]}"; do
        python3 -I "$QUARANTINE_ROUND_DRIVER" extract \
            --ledger "$quarantine_generation_ledger" --node "$quarantine_node" \
            --kind network > "$log_root/$quarantine_node-network-quarantine-receipt.new.json"
        chmod 400 "$log_root/$quarantine_node-network-quarantine-receipt.new.json"
        publish_canonical_maintenance_input \
            "$log_root/$quarantine_node-network-quarantine-receipt.new.json" \
            "$quarantine_root/$quarantine_node-network-quarantine-receipt.json"
        if [ ! -e "$quarantine_root/$quarantine_node-status.json" ] \
                && [ ! -L "$quarantine_root/$quarantine_node-status.json" ]; then
            run_quarantine_status_exact "$freeze_plan" "$freeze_sha" "$capture_id" \
                "$quarantine_node" > "$log_root/$quarantine_node-status.new.json"
            publish_canonical_maintenance_input \
                "$log_root/$quarantine_node-status.new.json" \
                "$quarantine_root/$quarantine_node-status.json"
        fi
    done
    printf 'archive fleet: all six nodes are fenced by the sealed mixed-state quarantine generation ledger\n'
    local quarantine_challenge
    quarantine_challenge="$(reserve_quarantine_challenge "$quarantine_root" \
        "$freeze_sha" "$capture_id")"
    require_hash "$quarantine_challenge" "legacy quarantine public cross-proof challenge"
    quarantine_pids=()
    quarantine_nodes=()
    quarantine_failed=0
    for quarantine_node in "${active_quarantine_nodes[@]}"; do
        if [ -e "$quarantine_root/$quarantine_node-external-proof.json" ] \
                || [ -L "$quarantine_root/$quarantine_node-external-proof.json" ]; then
            continue
        fi
        (
            probe_quarantine_external_exact "$freeze_plan" "$freeze_sha" \
                "$capture_id" "$quarantine_node" \
                "$quarantine_root/$quarantine_node-status.json" \
                "$log_root/$quarantine_node-external-proof.new.json" \
                "$quarantine_challenge"
            publish_canonical_maintenance_input \
                "$log_root/$quarantine_node-external-proof.new.json" \
                "$quarantine_root/$quarantine_node-external-proof.json"
        ) > "$log_root/$quarantine_node-external-proof.log" 2>&1 &
        quarantine_pids+=("$!")
        quarantine_nodes+=("$quarantine_node")
    done
    for quarantine_index in "${!quarantine_pids[@]}"; do
        if ! wait "${quarantine_pids[$quarantine_index]}"; then
            sed -n '1,120p' \
                "$log_root/${quarantine_nodes[$quarantine_index]}-external-proof.log" >&2
            quarantine_failed=1
        fi
    done
    [ "$quarantine_failed" -eq 0 ] || die \
        "external TCP/UDP challenges did not prove all six full-host quarantines"
    quarantine_pids=()
    quarantine_nodes=()
    quarantine_failed=0
    for quarantine_node in "${active_quarantine_nodes[@]}"; do
        if [ -e "$quarantine_root/$quarantine_node-public-cross-proof.json" ] \
                || [ -L "$quarantine_root/$quarantine_node-public-cross-proof.json" ]; then
            continue
        fi
        (
            run_quarantine_public_cross_proof_exact "$freeze_plan" "$freeze_sha" \
                "$capture_id" "$quarantine_node" "$legacy_height_receipt" \
                "$quarantine_challenge"
        ) > "$log_root/$quarantine_node-public-cross-proof.new.json" \
            2> "$log_root/$quarantine_node-public-cross-proof.stderr" &
        quarantine_pids+=("$!")
        quarantine_nodes+=("$quarantine_node")
    done
    for quarantine_index in "${!quarantine_pids[@]}"; do
        if ! wait "${quarantine_pids[$quarantine_index]}"; then
            sed -n '1,100p' \
                "$log_root/${quarantine_nodes[$quarantine_index]}-public-cross-proof.stderr" >&2
            quarantine_failed=1
        fi
    done
    [ "$quarantine_failed" -eq 0 ] || die \
        "quarantined loopback heads did not cryptographically cover every public observation"
    for quarantine_node in "${active_quarantine_nodes[@]}"; do
        if [ -e "$log_root/$quarantine_node-public-cross-proof.new.json" ]; then
            publish_canonical_maintenance_input \
                "$log_root/$quarantine_node-public-cross-proof.new.json" \
                "$quarantine_root/$quarantine_node-public-cross-proof.json"
        fi
        run_quarantine_status_exact "$freeze_plan" "$freeze_sha" "$capture_id" \
            "$quarantine_node" > "$log_root/$quarantine_node-post-proof-status.new.json"
        python3 - "$log_root/$quarantine_node-post-proof-status.new.json" \
            "$quarantine_root/$quarantine_node-status.json" "$freeze_sha" \
            "$capture_id" "$quarantine_node" <<'PY'
import json,pathlib,sys
fresh=json.loads(pathlib.Path(sys.argv[1]).read_text(encoding="utf-8"))
initial=json.loads(pathlib.Path(sys.argv[2]).read_text(encoding="utf-8"))
expected=(sys.argv[4],sys.argv[5],sys.argv[3])
if ((fresh.get("capture_id"),fresh.get("node"),fresh.get("freeze_plan_sha256"))!=expected
        or fresh.get("schema")!="arc.recovery.legacy-network-quarantine-status.v1"
        or fresh.get("receipt_sha256")!=initial.get("receipt_sha256")
        or fresh.get("active") is not True or fresh.get("enabled") is not True):
    raise SystemExit("fresh post-proof network-quarantine status differs")
PY
        if [ ! -e "$quarantine_root/$quarantine_node-post-proof-status.json" ] \
                && [ ! -L "$quarantine_root/$quarantine_node-post-proof-status.json" ]; then
            publish_canonical_maintenance_input \
                "$log_root/$quarantine_node-post-proof-status.new.json" \
                "$quarantine_root/$quarantine_node-post-proof-status.json"
        fi
    done
    local quarantine_stability_proof="$quarantine_root/fleet-stability-proof.json"
    if [ -e "$quarantine_stability_proof" ] || [ -L "$quarantine_stability_proof" ]; then
        verify_network_quarantine_stability_proof "$quarantine_stability_proof" \
            "$freeze_sha" "$capture_id" "$quarantine_challenge" \
            "$quarantine_generation_ledger" >/dev/null
        printf 'archive fleet: reusing the canonical active-subset quarantine stability proof\n'
    else
        local stability_sample_root="$log_root/quarantine-stability-samples"
        mkdir -m 700 -- "$stability_sample_root"
        local stability_started_at stability_started_ns stability_completed_at stability_completed_ns
        read -r stability_started_at stability_started_ns < <(python3 - <<'PY'
import datetime,time
print(datetime.datetime.now(datetime.timezone.utc).strftime("%Y-%m-%dT%H:%M:%SZ"),time.monotonic_ns())
PY
        )
        local stability_index stability_failed stability_elapsed_ns
        local stability_pids stability_nodes
        for stability_index in 0 1; do
            stability_pids=(); stability_nodes=(); stability_failed=0
            for quarantine_node in "${active_quarantine_nodes[@]}"; do
                (
                    run_quarantine_stability_sample_exact "$freeze_plan" "$freeze_sha" \
                        "$capture_id" "$quarantine_node" "$quarantine_challenge" \
                        "$stability_index"
                ) > "$stability_sample_root/$quarantine_node-$stability_index.json" \
                    2> "$stability_sample_root/$quarantine_node-$stability_index.stderr" &
                stability_pids+=("$!"); stability_nodes+=("$quarantine_node")
            done
            for quarantine_index in "${!stability_pids[@]}"; do
                if ! wait "${stability_pids[$quarantine_index]}"; then
                    sed -n '1,100p' \
                        "$stability_sample_root/${stability_nodes[$quarantine_index]}-$stability_index.stderr" >&2
                    stability_failed=1
                fi
            done
            [ "$stability_failed" -eq 0 ] || die \
                "a full live-fence/head stability sample failed; no writer stop is authorized"
            if [ "$stability_index" -eq 0 ] \
                    && [ "${#active_quarantine_nodes[@]}" -gt 0 ]; then
                printf 'archive fleet: first exact active-subset quarantine/head sample complete; observing a full 120-second drain interval\n'
                /bin/sleep 120
            fi
        done
        read -r stability_completed_at stability_completed_ns < <(python3 - <<'PY'
import datetime,time
print(datetime.datetime.now(datetime.timezone.utc).strftime("%Y-%m-%dT%H:%M:%SZ"),time.monotonic_ns())
PY
        )
        stability_elapsed_ns="$(python3 - "$stability_started_ns" "$stability_completed_ns" <<'PY'
import sys
start,end=map(int,sys.argv[1:])
if end<start:raise SystemExit("quarantine stability monotonic clock regressed")
print(end-start)
PY
        )"
        if [ "${#active_quarantine_nodes[@]}" -eq 0 ]; then
            stability_elapsed_ns=0
        fi
        create_network_quarantine_stability_proof "$freeze_plan" "$freeze_sha" \
            "$capture_id" "$quarantine_challenge" "$stability_sample_root" \
            "$log_root/fleet-stability-proof.new.json" "$stability_started_at" \
            "$stability_completed_at" "$stability_elapsed_ns" \
            "$quarantine_generation_ledger"
        publish_canonical_maintenance_input "$log_root/fleet-stability-proof.new.json" \
            "$quarantine_stability_proof"
        verify_network_quarantine_stability_proof "$quarantine_stability_proof" \
            "$freeze_sha" "$capture_id" "$quarantine_challenge" \
            "$quarantine_generation_ledger" >/dev/null
    fi
    verify_quarantine_maintenance_inputs "$quarantine_root" "$freeze_sha" \
        "$capture_id" "$legacy_height_receipt" "$quarantine_challenge" \
        "$quarantine_stability_proof" "$quarantine_generation_ledger"
    # A historical stability proof is never substituted for the live boundary.
    # Re-run the full remote AST/tool/unit/status proof on every host immediately
    # before the stop transaction; fence drift leaves the writer untouched.
    for quarantine_node in "${active_quarantine_nodes[@]}"; do
        run_quarantine_status_exact "$freeze_plan" "$freeze_sha" "$capture_id" \
            "$quarantine_node" > "$log_root/$quarantine_node-pre-stop-status.json"
    done
    local final_source_capture_root
    final_source_capture_root="$(prepare_protected_maintenance_directory \
        "$maintenance_input_root/post-quarantine-final-source-captures")"
    capture_post_quarantine_final_sources "$freeze_plan" "$freeze_sha" "$capture_id" \
        "$quarantine_generation_ledger" "$quarantine_stability_proof" "$log_root" \
        "$final_source_capture_root" "$log_root" "$inspector_binary_sha" \
        "$inspector_genesis_sha" "$inspector_legacy_validators_sha" \
        "$allow_unbound_legacy_wal" "${active_quarantine_nodes[@]}"
    printf 'archive fleet: all active legacy writers are behind exact round-bound persistent full-host quarantines; pidfd TERM may now begin\n'
    local node
    local pids=() names=()
    printf 'archive fleet: stopping the active quarantined writers concurrently; no host may thaw outside its proved quarantine\n'
    for node in "${active_quarantine_nodes[@]}"; do
        (
            local node_round node_authorization_sha node_readiness_sha node_transition_sha
            read -r node_round node_authorization_sha node_readiness_sha node_transition_sha < <(
                python3 -I "$QUARANTINE_ROUND_DRIVER" extract \
                    --ledger "$quarantine_generation_ledger" --node "$node" --kind refs
            )
            stop_after_quarantine_round_exact "$capture_id" "$freeze_sha" "$node" \
                "$node_round" "$node_authorization_sha" "$node_readiness_sha" \
                "$node_transition_sha" \
                "$(hash_file "$final_source_capture_root/$node.json")"
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
    [ "$failed" -eq 0 ] || die \
        "at least one quarantined writer stop failed; completed nodes remain restart-fenced and incomplete nodes remain network-quarantined"
    for node in "${active_quarantine_nodes[@]}"; do
        run_stopped_status_exact "$freeze_plan" "$freeze_sha" "$capture_id" "$node" \
            > "$log_root/$node-stopped-status.json"
    done
    for node in "${stopped_quarantine_nodes[@]}"; do
        read -r node_round node_authorization_sha node_readiness_sha node_transition_sha < <(
            python3 -I "$QUARANTINE_ROUND_DRIVER" extract \
                --ledger "$quarantine_generation_ledger" --node "$node" --kind refs
        )
        run_remote "$node" quarantine-round-status "$capture_id" "$node" "$freeze_sha" \
            "$node_round" "$node_authorization_sha" "$node_readiness_sha" \
            "$node_transition_sha" > "$log_root/$node-stopped-status.json"
    done
    # Only seal the all-stopped boundary after all six exact final remote
    # stopped-status proofs have completed successfully.
    all_controlled_stopped_at="$(reserve_stop_boundary_timestamp \
        "$offline_stop_output" all-controlled-stopped "$freeze_sha" "$capture_id")"
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
        ensure_offline_capture "$capture_id" "$node" "$observation_generation" \
            "$observation_generation_receipt_sha" "$drive_prefreeze_receipt_sha" \
            > "$log_root/$node-capture.log" 2>&1 &
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
    local persisted_root
    persisted_root="$(prepare_protected_maintenance_directory \
        "$maintenance_input_root/persisted-heads")"
    pids=() names=() failed=0
    for node in nyc lax ams lhr nrt sgp; do
        if [ -e "$persisted_root/$node-persisted-head.json" ] \
                || [ -L "$persisted_root/$node-persisted-head.json" ]; then
            continue
        fi
        (
            local node_kind
            node_kind="$(python3 -I "$QUARANTINE_ROUND_DRIVER" extract \
                --ledger "$quarantine_generation_ledger" --node "$node" \
                --kind transition-kind)"
            if [ "$node_kind" = network-quarantine-active ]; then
                run_persisted_head_exact "$freeze_plan" "$freeze_sha" "$capture_id" "$node" \
                    "$inspector_binary_sha" "$inspector_genesis_sha" \
                    "$inspector_validators_sha" "$inspector_legacy_validators_sha" \
                    > "$log_root/$node-persisted-head.new.json"
            elif [ "$node_kind" = persistently-stopped-precommit ]; then
                python3 -I "$QUARANTINE_ROUND_DRIVER" extract \
                    --ledger "$quarantine_generation_ledger" --node "$node" \
                    --kind transition | python3 -c \
                    'import hashlib,json,sys; value=json.load(sys.stdin); persisted=value["persisted_head"]; raw=(json.dumps(persisted["value"],sort_keys=True,separators=(",",":"))+"\n").encode(); assert hashlib.sha256(raw).hexdigest()==persisted["sha256"] and persisted["value"]["source_pair_role"]=="preauthorization-boundary"; sys.stdout.buffer.write(raw)' \
                    > "$log_root/$node-persisted-head.new.json"
            else
                die "persisted-head transition kind differs for $node"
            fi
            publish_canonical_maintenance_input "$log_root/$node-persisted-head.new.json" \
                "$persisted_root/$node-persisted-head.json"
        ) > "$log_root/$node-persisted-head.log" 2>&1 &
        pids+=("$!"); names+=("$node")
    done
    for index in "${!pids[@]}"; do
        if ! wait "${pids[$index]}"; then
            printf 'archive fleet: persisted legacy head export failed: %s\n' "${names[$index]}" >&2
            sed -n '1,120p' "$log_root/${names[$index]}-persisted-head.log" >&2
            failed=1
        fi
    done
    [ "$failed" -eq 0 ] || die \
        "at least one final stopped snapshot/WAL pair lacks an exact recovery-export head"
    # Retain the full, freshly revalidated monitor receipt for every active
    # network-quarantine transition.  Its
    # semantic-interpreter identity is required later to run the fail-closed
    # public late-fork interlock without trusting an ambient host Python.
    pids=(); names=(); failed=0
    for node in "${active_quarantine_nodes[@]}"; do
        if [ -e "$quarantine_root/$node-monitor.json" ] \
                || [ -L "$quarantine_root/$node-monitor.json" ]; then
            continue
        fi
        (
            run_quarantine_monitor_receipt_exact "$freeze_plan" "$freeze_sha" \
                "$capture_id" "$node" > "$log_root/$node-monitor.new.json"
            publish_canonical_maintenance_input "$log_root/$node-monitor.new.json" \
                "$quarantine_root/$node-monitor.json"
        ) > "$log_root/$node-monitor.log" 2>&1 &
        pids+=("$!"); names+=("$node")
    done
    for index in "${!pids[@]}"; do
        if ! wait "${pids[$index]}"; then
            printf 'archive fleet: quarantine monitor receipt failed: %s\n' "${names[$index]}" >&2
            sed -n '1,120p' "$log_root/${names[$index]}-monitor.log" >&2
            failed=1
        fi
    done
    [ "$failed" -eq 0 ] || die \
        "at least one stopped host lacks a freshly verified network-quarantine monitor receipt"
    local maintenance_evidence_bundle="${offline_stop_output}.legacy-maintenance-evidence-bundle.json"
    local maintenance_evidence_bundle_sha
    maintenance_evidence_bundle_sha="$(create_legacy_maintenance_evidence_bundle \
        "$freeze_plan" "$freeze_sha" "$capture_id" "$log_root" \
        "$legacy_height_cross_proof" "$quarantine_root" "$persisted_root" \
        "$quarantine_stability_proof" "$maintenance_evidence_bundle" \
        "$first_quarantine_started_at" \
        "$all_controlled_stopped_at" "$quarantine_generation_ledger" \
        "$observation_selection")"
    local maintenance_boundary="${offline_stop_output}.legacy-maintenance-boundary.json"
    local maintenance_boundary_sha
    maintenance_boundary_sha="$(create_legacy_maintenance_boundary \
        "$freeze_plan" "$freeze_sha" "$capture_id" "$legacy_height_receipt" \
        "$legacy_height_receipt_sha" "$legacy_height_cross_proof" "$quarantine_root" \
        "$persisted_root" "$maintenance_boundary" "$first_quarantine_started_at" \
        "$all_controlled_stopped_at" "$inspector_binary_sha" "$inspector_genesis_sha" \
        "$inspector_validators_sha" "$inspector_legacy_validators_sha" \
        "$maintenance_evidence_bundle")"
    [ -f "$LATE_FORK_INTERLOCK_TOOL" ] && [ ! -L "$LATE_FORK_INTERLOCK_TOOL" ] || \
        die "legacy late-fork interlock tool is missing or unsafe"
    local late_fork_source_set="${offline_stop_output}.legacy-late-fork-source-set.json"
    local late_fork_source_set_result late_fork_source_set_sha
    late_fork_source_set_result="$(python3 "$LATE_FORK_INTERLOCK_TOOL" build-source-set \
        --boundary "$maintenance_boundary" \
        --boundary-sha256 "$maintenance_boundary_sha" \
        --output "$late_fork_source_set")"
    late_fork_source_set_sha="$(python3 - "$late_fork_source_set_result" \
        "$late_fork_source_set" <<'PY'
import hashlib,json,pathlib,sys
value=json.loads(sys.argv[1]);path=pathlib.Path(sys.argv[2]);raw=path.read_bytes()
if (set(value)!={"schema","source_set_sha256","output"}
        or value.get("schema")!="arc.recovery.legacy-late-fork-source-set-build.v1"
        or value.get("output")!=str(path)
        or value.get("source_set_sha256")!=hashlib.sha256(raw).hexdigest()):
    raise SystemExit("legacy late-fork source-set build receipt differs")
print(value["source_set_sha256"])
PY
    )"
    local offline_stop_sha
    offline_stop_sha="$(create_offline_stop_evidence "$freeze_plan" "$freeze_sha" \
        "$capture_id" "$log_root" "$offline_stop_output" \
        "$first_quarantine_started_at" "$all_controlled_stopped_at" \
        "$legacy_height_cross_proof" "$maintenance_boundary" \
        "$maintenance_evidence_bundle")"
    printf 'archive fleet: LEGACY-MAINTENANCE-EVIDENCE-BUNDLE path=%s sha256=%s schema=arc.recovery.legacy-maintenance-evidence-bundle.v1\n' \
        "$maintenance_evidence_bundle" "$maintenance_evidence_bundle_sha"
    printf 'archive fleet: LEGACY-MAINTENANCE-BOUNDARY path=%s sha256=%s schema=arc.recovery.legacy-maintenance-boundary.v1\n' \
        "$maintenance_boundary" "$maintenance_boundary_sha"
    printf 'archive fleet: LEGACY-LATE-FORK-SOURCE-SET path=%s sha256=%s schema=arc.recovery.legacy-late-fork-source-set.v1\n' \
        "$late_fork_source_set" "$late_fork_source_set_sha"
    printf 'archive fleet: OFFLINE-STOP-EVIDENCE path=%s sha256=%s schema=arc.validator-vault.offline-stop-evidence.v2\n' \
        "$offline_stop_output" "$offline_stop_sha"
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

verify_validator_receipt_chain() {
    local install_receipt="$1" restore_receipt="$2" manifest="$3"
    local source_commit="$4" cli_sha="$5" genesis_sha="$6" known_sha="$7"
    local ssh_sha="$8" scp_sha="$9" freeze_sha="${10}" offline_sha="${11}"
    python3 - "$ROLLOUT_TOOL" "$install_receipt" "$restore_receipt" "$manifest" "$source_commit" \
        "$cli_sha" "$genesis_sha" "$known_sha" "$ssh_sha" "$scp_sha" \
        "$freeze_sha" "$offline_sha" <<'PY'
import importlib.util, pathlib, re, sys
(tool_raw, install_raw, restore_raw, manifest_raw, source_commit, cli_sha, genesis_sha,
 known_sha, ssh_sha, scp_sha, freeze_sha, offline_sha) = sys.argv[1:]
spec = importlib.util.spec_from_file_location("arc_recovery_receipt_gate", tool_raw)
if spec is None or spec.loader is None: raise SystemExit("cannot load the hash-bound rollout receipt gate")
module = importlib.util.module_from_spec(spec); spec.loader.exec_module(module)
manifest, _digest = module.load_sealed_manifest(pathlib.Path(manifest_raw))
rows, payloads = module.verify_production_input_stage(manifest)
sanitized = module.validate_validator_receipt_chain(manifest, payloads)
expected = {
    "source_main_commit": source_commit, "arc_cli_sha256": cli_sha,
    "genesis_sha256": genesis_sha, "known_hosts_sha256": known_sha,
    "ssh_sha256": ssh_sha, "scp_sha256": scp_sha,
    "freeze_plan_sha256": freeze_sha,
    "offline_stop_evidence_sha256": offline_sha,
}
if any(sanitized.get(field) != value for field, value in expected.items()):
    raise SystemExit("validator receipt chain differs from the operator/artifact/freeze tuple")
artifacts = manifest["artifacts"]
if (pathlib.Path(install_raw) != pathlib.Path(artifacts["validator_key_install_receipt"]["path"])
        or pathlib.Path(restore_raw) != pathlib.Path(artifacts["validator_vault_restore_receipt"]["path"])):
    raise SystemExit("validator receipt arguments differ from manifest-staged paths")
for row in sanitized["validators"]:
    print(row["node"], row["address"], row["keyfile_sha256"])
PY
}

verify_operator_transport_matches_stage() {
    local manifest="$1"
    python3 - "$manifest" "$ARC_OPERATOR_KNOWN_HOSTS" \
        "$ARC_OPERATOR_SSH_KNOWN_HOSTS_SHA256" "$ARC_OPERATOR_IDENTITY" \
        "$ARC_OPERATOR_SSH_IDENTITY_SHA256" <<'PY'
import hashlib, json, os, pathlib, stat, sys
manifest_path, pinned_known_raw, known_sha, pinned_identity_raw, identity_sha = sys.argv[1:]
canonical = lambda value: (json.dumps(value, sort_keys=True, separators=(",", ":")) + "\n").encode()
def fail(message): raise SystemExit(f"operator staged transport: {message}")
def read(path, label, mode, maximum):
    path = pathlib.Path(path); fd = os.open(path, os.O_RDONLY | getattr(os, "O_NOFOLLOW", 0))
    try:
        before = os.fstat(fd); visible = os.lstat(path)
        ident = lambda value: (value.st_dev, value.st_ino, value.st_mode, value.st_uid,
                               value.st_nlink, value.st_size, value.st_mtime_ns, value.st_ctime_ns)
        if (not stat.S_ISREG(before.st_mode) or stat.S_ISLNK(visible.st_mode)
                or ident(before) != ident(visible) or before.st_uid != os.getuid()
                or stat.S_IMODE(before.st_mode) != mode or before.st_nlink != 1
                or before.st_size <= 0 or before.st_size > maximum):
            fail(f"{label} identity differs")
        body = b""
        while True:
            chunk = os.read(fd, 1024 * 1024)
            if not chunk: break
            body += chunk
        if len(body) != before.st_size or ident(before) != ident(os.fstat(fd)):
            fail(f"{label} changed while read")
        return body
    finally: os.close(fd)
manifest_raw = read(manifest_path, "rollout manifest", 0o400, 32 * 1024 * 1024)
manifest = json.loads(manifest_raw)
if manifest_raw != canonical(manifest): fail("rollout manifest is noncanonical")
artifact_paths = [pathlib.Path(row["path"]) for row in manifest.get("artifacts", {}).values()]
if not artifact_paths: fail("rollout manifest has no artifacts")
stage_root = pathlib.Path(os.path.commonpath([os.fspath(path) for path in artifact_paths]))
stage_path = stage_root / "STAGE-MANIFEST.json"
stage_artifact = manifest.get("artifacts", {}).get("production_input_stage_manifest")
stage_provenance_sha = manifest.get("provenance", {}).get("production_input_stage_manifest_sha256")
if (not isinstance(stage_artifact, dict)
        or pathlib.Path(stage_artifact.get("path", "")) != stage_path
        or stage_artifact.get("sha256") != stage_provenance_sha):
    fail("stage manifest path/hash is not sealed by rollout provenance")
stage_raw = read(stage_path, "stage manifest", 0o400, 1024 * 1024)
if hashlib.sha256(stage_raw).hexdigest() != stage_provenance_sha:
    fail("stage manifest bytes differ from sealed rollout provenance")
stage = json.loads(stage_raw)
if stage_raw != canonical(stage) or stage.get("schema") != "arc.recovery.production-input-stage.v1":
    fail("stage manifest is noncanonical or unsupported")
rows = stage.get("files")
if not isinstance(rows, list): fail("stage manifest files are unavailable")
by_name = {row.get("name"): row for row in rows if isinstance(row, dict)}
if len(by_name) != len(rows): fail("stage manifest file names are duplicate/malformed")
known_row, identity_row = by_name.get("ssh_known_hosts"), by_name.get("ssh_identity")
if not isinstance(known_row, dict) or not isinstance(identity_row, dict): fail("stage manifest omits SSH trust inputs")
if known_row.get("path") != "private/known_hosts" or identity_row.get("path") != "private/id_ed25519":
    fail("stage manifest SSH paths differ")
known = read(stage_root / known_row["path"], "staged known-hosts", 0o400, 64 * 1024)
identity = read(stage_root / identity_row["path"], "staged SSH identity", 0o400, 128 * 1024)
if hashlib.sha256(known).hexdigest() != known_sha or known_row.get("sha256") != known_sha:
    fail("staged known-hosts differs from the operator transport")
if hashlib.sha256(identity).hexdigest() != identity_sha or identity_row.get("sha256") != identity_sha:
    fail("staged SSH identity differs from the operator transport")
if known != read(pinned_known_raw, "pinned known-hosts", 0o400, 64 * 1024): fail("pinned and staged known-hosts bytes differ")
if identity != read(pinned_identity_raw, "pinned SSH identity", 0o400, 128 * 1024): fail("pinned and staged SSH identity bytes differ")
PY
}

verify_rollout_and_capture_topology() {
    local manifest="$1" freeze_plan="$2" freeze_sha="$3" capture_id="$4"
    local allow_provisional="${5:-false}"
    case "$allow_provisional" in true|false) ;; *) die "invalid provisional installed-key proof policy" ;; esac
    python3 - "$ROLLOUT_TOOL" "$manifest" "$freeze_plan" "$freeze_sha" "$capture_id" \
        "$allow_provisional" <<'PY'
import importlib.util
import json
import pathlib
import sys

tool_path = pathlib.Path(sys.argv[1])
spec = importlib.util.spec_from_file_location("arc_recovery_archive_gate", tool_path)
if spec is None or spec.loader is None: raise SystemExit("cannot load hash-bound rollout verifier")
rr = importlib.util.module_from_spec(spec); spec.loader.exec_module(rr)
manifest_path = pathlib.Path(sys.argv[2])
allow_provisional = sys.argv[6] == "true"
manifest, digest = rr.load_sealed_manifest(
    manifest_path,
    allow_provisional_installed_key_proof=allow_provisional,
)
if manifest["mode"] != "production":
    rr.fail("fleet archive sealing requires a production rollout manifest")
rr.require_prearchive_manifest(manifest)
rr.verify_artifacts(manifest)
rr.RecoveryRollout(manifest, digest).verify_checkpoint()
freeze = json.loads(pathlib.Path(sys.argv[3]).read_text(encoding="utf-8"))
freeze_sha, capture_id = sys.argv[4:6]
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

stage_capture_inspector_inputs() {
    local node="$1" freeze_sha="$2" binary="$3" binary_sha="$4"
    local genesis="$5" genesis_sha="$6" validators="$7" validators_sha="$8"
    local legacy_validators="$9" legacy_validators_sha="${10}"
    stage_file "$node" "$freeze_sha" binary "$binary" "$binary_sha"
    stage_file "$node" "$freeze_sha" genesis "$genesis" "$genesis_sha"
    stage_file "$node" "$freeze_sha" validators "$validators" "$validators_sha"
    stage_file "$node" "$freeze_sha" legacy-validators \
        "$legacy_validators" "$legacy_validators_sha"
}

verify_remote_validator_key_identity() {
    local node="$1" manifest_sha="$2" cli_sha="$3" key_sha="$4" address="$5"
    local output
    output="$(run_remote "$node" validator-key-identity \
        "$manifest_sha" "$node" "$cli_sha" "$key_sha" "$address")"
    python3 - "$output" "$node" "$cli_sha" "$key_sha" "$address" <<'PY'
import json, sys
raw, node, cli_sha, key_sha, address = sys.argv[1:]
expected = {
    "schema": "arc.recovery.validator-key-identity.v1", "node": node,
    "cli_sha256": cli_sha, "keyfile_sha256": key_sha, "address": address,
}
canonical = json.dumps(expected, sort_keys=True, separators=(",", ":"))
if raw != canonical or json.loads(raw) != expected:
    raise SystemExit(f"fresh validator identity proof differs for {node}")
PY
}

verify_remote_validator_key_identity_transient() {
    local node="$1" cli="$2" cli_sha="$3" key_sha="$4" address="$5" challenge="$6"
    local output
    output="$(run_remote "$node" validator-key-identity-transient \
        "$node" "$cli_sha" "$key_sha" "$address" "$challenge" < "$cli")"
    python3 - "$output" "$node" "$cli_sha" "$key_sha" "$address" "$challenge" <<'PY'
import json, sys
raw, node, cli_sha, key_sha, address, challenge = sys.argv[1:]
expected = {
    "schema": "arc.recovery.validator-key-identity-challenged.v1",
    "node": node, "cli_sha256": cli_sha, "keyfile_sha256": key_sha,
    "address": address, "challenge": challenge,
}
canonical = json.dumps(expected, sort_keys=True, separators=(",", ":"))
if raw != canonical or json.loads(raw) != expected:
    raise SystemExit(f"fresh challenged validator identity proof differs for {node}")
print(raw)
PY
}

verify_installed_keys_phase() {
    # The identity verifier pins Python and SSH state before its proof scratch
    # root; keep every copy inside one invocation-scoped cleanup boundary.
    begin_temporary_scope
    local freeze_plan="" manifest="" cli="" cli_sha="" validators="" validators_sha=""
    local install_receipt="" install_sha="" restore_receipt="" restore_sha=""
    local challenge="" output=""
    while [ "$#" -gt 0 ]; do
        case "$1" in
            --freeze-plan) [ "$#" -ge 2 ] || die "--freeze-plan needs a value"; freeze_plan="$2"; shift 2 ;;
            --manifest) [ "$#" -ge 2 ] || die "--manifest needs a value"; manifest="$2"; shift 2 ;;
            --cli) [ "$#" -ge 2 ] || die "--cli needs a value"; cli="$2"; shift 2 ;;
            --cli-sha256) [ "$#" -ge 2 ] || die "--cli-sha256 needs a value"; cli_sha="$2"; shift 2 ;;
            --validator-public-keys) [ "$#" -ge 2 ] || die "--validator-public-keys needs a value"; validators="$2"; shift 2 ;;
            --validator-public-keys-sha256) [ "$#" -ge 2 ] || die "--validator-public-keys-sha256 needs a value"; validators_sha="$2"; shift 2 ;;
            --validator-install-receipt) [ "$#" -ge 2 ] || die "--validator-install-receipt needs a value"; install_receipt="$2"; shift 2 ;;
            --validator-install-receipt-sha256) [ "$#" -ge 2 ] || die "--validator-install-receipt-sha256 needs a value"; install_sha="$2"; shift 2 ;;
            --vault-restore-receipt) [ "$#" -ge 2 ] || die "--vault-restore-receipt needs a value"; restore_receipt="$2"; shift 2 ;;
            --vault-restore-receipt-sha256) [ "$#" -ge 2 ] || die "--vault-restore-receipt-sha256 needs a value"; restore_sha="$2"; shift 2 ;;
            --challenge) [ "$#" -ge 2 ] || die "--challenge needs a value"; challenge="$2"; shift 2 ;;
            --output) [ "$#" -ge 2 ] || die "--output needs a value"; output="$2"; shift 2 ;;
            -h|--help) usage; return 0 ;;
            *) die "unknown verify-installed-keys option: $1" ;;
        esac
    done
    configure_operator_transport false
    require_hash "$cli_sha" "validator identity CLI hash"
    require_hash "$validators_sha" "validator public-key manifest hash"
    require_hash "$install_sha" "validator install receipt hash"
    require_hash "$restore_sha" "validator vault restore receipt hash"
    require_hash "$challenge" "validator identity challenge"
    require_absolute_file "$freeze_plan" "validator identity freeze plan"
    require_absolute_file "$manifest" "provisional sealed rollout manifest"
    require_absolute_file "$cli" "validator identity CLI"
    require_absolute_file "$validators" "validator public-key manifest"
    require_absolute_file "$install_receipt" "validator install receipt"
    require_absolute_file "$restore_receipt" "validator vault restore receipt"
    if [ -n "$output" ]; then
        case "$output" in /*.json) ;; *) die "validator installed-key proof output must be an absolute .json path" ;; esac
        [ "$(python3 -c 'import os,sys; print(os.path.normpath(sys.argv[1]))' "$output")" = "$output" ] || \
            die "validator installed-key proof output must be lexically normalized"
        [ ! -e "$output" ] && [ ! -L "$output" ] || \
            die "validator installed-key proof output already exists"
    fi

    OPERATOR_FREEZE_PLAN="$freeze_plan"
    ARCHIVE_FLEET_PINNED_ROOT="$(mktemp -d "${TMPDIR:-/tmp}/arc-key-proof.XXXXXX")"
    [ -d "$ARCHIVE_FLEET_PINNED_ROOT" ] && [ ! -L "$ARCHIVE_FLEET_PINNED_ROOT" ] || \
        die "cannot allocate the private validator identity proof root"
    freeze_plan="$(pin_freeze_plan "$freeze_plan" "$ARCHIVE_FLEET_PINNED_ROOT")"
    local freeze_sha capture_id manifest_sha verification_output
    freeze_sha="$(freeze_plan_hash "$freeze_plan")"
    capture_id="$(capture_id_for_freeze_plan_hash "$freeze_sha")"
    verification_output="$(verify_rollout_and_capture_topology \
        "$manifest" "$freeze_plan" "$freeze_sha" "$capture_id" true)"
    manifest_sha="$(printf '%s\n' "$verification_output" | tail -n 1)"
    require_hash "$manifest_sha" "provisional sealed rollout manifest hash"
    verify_operator_transport_matches_stage "$manifest"

    local manifest_cli manifest_cli_sha manifest_validators manifest_validators_sha
    local manifest_install manifest_install_sha manifest_restore manifest_restore_sha
    local offline_evidence offline_sha maintenance_evidence_bundle source_commit sealed_ssh_sha receipt_rows
    manifest_cli="$(manifest_field "$manifest" artifacts.cli.path)"
    manifest_cli_sha="$(manifest_field "$manifest" artifacts.cli.sha256)"
    manifest_validators="$(manifest_field "$manifest" artifacts.validator_public_keys.path)"
    manifest_validators_sha="$(manifest_field "$manifest" artifacts.validator_public_keys.sha256)"
    manifest_install="$(manifest_field "$manifest" artifacts.validator_key_install_receipt.path)"
    manifest_install_sha="$(manifest_field "$manifest" artifacts.validator_key_install_receipt.sha256)"
    manifest_restore="$(manifest_field "$manifest" artifacts.validator_vault_restore_receipt.path)"
    manifest_restore_sha="$(manifest_field "$manifest" artifacts.validator_vault_restore_receipt.sha256)"
    [ "$cli" = "$manifest_cli" ] && [ "$cli_sha" = "$manifest_cli_sha" ] || \
        die "validator identity CLI path/hash differs from the manifest-staged artifact"
    [ "$validators" = "$manifest_validators" ] && [ "$validators_sha" = "$manifest_validators_sha" ] || \
        die "validator public-key path/hash differs from the manifest-staged artifact"
    [ "$install_receipt" = "$manifest_install" ] && [ "$install_sha" = "$manifest_install_sha" ] || \
        die "validator install receipt path/hash differs from the manifest-staged artifact"
    [ "$restore_receipt" = "$manifest_restore" ] && [ "$restore_sha" = "$manifest_restore_sha" ] || \
        die "validator vault restore receipt path/hash differs from the manifest-staged artifact"
    [ "$(hash_file "$cli")" = "$cli_sha" ] || die "validator identity CLI changed"
    [ "$(hash_file "$validators")" = "$validators_sha" ] || die "validator public-key manifest changed"
    [ "$(hash_file "$install_receipt")" = "$install_sha" ] || die "validator install receipt changed"
    [ "$(hash_file "$restore_receipt")" = "$restore_sha" ] || die "validator vault restore receipt changed"

    offline_evidence="$(manifest_field "$manifest" artifacts.offline_stop_evidence.path)"
    offline_sha="$(manifest_field "$manifest" artifacts.offline_stop_evidence.sha256)"
    maintenance_evidence_bundle="$(manifest_field "$manifest" artifacts.legacy_maintenance_evidence_bundle.path)"
    source_commit="$(manifest_field "$manifest" provenance.source_main_commit)"
    sealed_ssh_sha="$(manifest_field "$manifest" provenance.offline_stop_verification.ssh_sha256)"
    [ "$sealed_ssh_sha" = "$ARC_OPERATOR_SSH_SHA256" ] || \
        die "operator SSH executable differs from the sealed stop transport"
    [ "$(current_source_commit)" = "$source_commit" ] || \
        die "validator identity verifier worktree differs from protected main"
    receipt_rows="$ARCHIVE_FLEET_PINNED_ROOT/validator-key-rows.tsv"
    verify_validator_receipt_chain "$install_receipt" "$restore_receipt" \
        "$manifest" "$source_commit" "$cli_sha" \
        "$(manifest_field "$manifest" artifacts.genesis.sha256)" \
        "$ARC_OPERATOR_SSH_KNOWN_HOSTS_SHA256" "$sealed_ssh_sha" \
        "$ARC_OPERATOR_SCP_SHA256" "$freeze_sha" "$offline_sha" > "$receipt_rows"
    chmod 400 "$receipt_rows"

    REMOTE_HELPER_SHA="$(manifest_field "$freeze_plan" remote_helper_sha256)"
    require_hash "$REMOTE_HELPER_SHA" "sealed remote helper hash"
    [ "$(hash_file "$REMOTE_HELPER")" = "$REMOTE_HELPER_SHA" ] || \
        die "remote helper bytes differ from the sealed freeze plan"
    REMOTE_HELPER_PATH="/root/.arc-recovery-helpers/$REMOTE_HELPER_SHA/archive-node.sh"
    verify_offline_stop_evidence_remote "$freeze_plan" "$freeze_sha" "$capture_id" \
        "$offline_evidence" "$offline_sha" "$maintenance_evidence_bundle" >&2

    local started_at completed_at log_root node address key_sha row_count=0
    started_at="$(python3 -c 'import time; print(time.time_ns() // 1_000_000)')"
    log_root="$ARCHIVE_FLEET_PINNED_ROOT/validator-key-responses"
    mkdir -m 700 -- "$log_root"
    while IFS=' ' read -r node address key_sha; do
        [ "$node" = "${NODES[$row_count]%%=*}" ] || \
            die "validator receipt rows differ from the fixed fleet order"
        verify_remote_validator_key_identity_transient \
            "$node" "$cli" "$cli_sha" "$key_sha" "$address" "$challenge" \
            > "$log_root/$node.json"
        chmod 400 "$log_root/$node.json"
        row_count=$((row_count + 1))
    done < "$receipt_rows"
    [ "$row_count" -eq 6 ] || die "validator receipt chain did not prove exactly six identities"
    verify_offline_stop_evidence_remote "$freeze_plan" "$freeze_sha" "$capture_id" \
        "$offline_evidence" "$offline_sha" "$maintenance_evidence_bundle" >&2
    completed_at="$(python3 -c 'import time; print(time.time_ns() // 1_000_000)')"

    local stage_sha proof_path
    stage_sha="$(manifest_field "$manifest" provenance.production_input_stage_manifest_sha256)"
    proof_path="$ARCHIVE_FLEET_PINNED_ROOT/validator-installed-key-proof.json"
    python3 - "$proof_path" "$log_root" "$source_commit" "$stage_sha" "$freeze_sha" \
        "$offline_sha" "$install_sha" "$validators_sha" "$cli_sha" \
        "$REMOTE_HELPER_SHA" "$REMOTE_HELPER_PATH" \
        "$ARC_OPERATOR_SSH_KNOWN_HOSTS_SHA256" "$ARC_OPERATOR_SSH_IDENTITY_SHA256" \
        "$ARC_OPERATOR_SSH_SHA256" "$ARC_OPERATOR_SCP_SHA256" "$challenge" \
        "$started_at" "$completed_at" "${NODES[@]}" <<'PY'
import hashlib, json, os, pathlib, re, sys
(output_raw, responses_raw, source_commit, stage_sha, freeze_sha, offline_sha,
 install_sha, validators_sha, cli_sha, helper_sha, helper_path, known_sha,
 identity_sha, ssh_sha, scp_sha, challenge, started_raw, completed_raw,
 *fleet_raw) = sys.argv[1:]
output = pathlib.Path(output_raw); responses = pathlib.Path(responses_raw)
canonical = lambda value: (json.dumps(value, sort_keys=True, separators=(",", ":")) + "\n").encode()
hash_re = re.compile(r"[0-9a-f]{64}")
for value in (stage_sha, freeze_sha, offline_sha, install_sha, validators_sha, cli_sha,
              helper_sha, known_sha, identity_sha, ssh_sha, scp_sha, challenge):
    if hash_re.fullmatch(value) is None: raise SystemExit("validator identity proof hash is malformed")
started = int(started_raw); completed = int(completed_raw)
if started <= 0 or completed < started: raise SystemExit("validator identity proof time window is invalid")
rows = []
addresses = set(); key_hashes = set(); response_hashes = set()
for item in fleet_raw:
    node, host = item.split("=", 1)
    path = responses / f"{node}.json"
    raw = path.read_bytes(); response = json.loads(raw)
    expected_fields = {"schema", "node", "cli_sha256", "keyfile_sha256", "address", "challenge"}
    if raw != canonical(response) or set(response) != expected_fields:
        raise SystemExit(f"validator identity response is noncanonical or inexact: {node}")
    if (response["schema"] != "arc.recovery.validator-key-identity-challenged.v1"
            or response["node"] != node or response["cli_sha256"] != cli_sha
            or response["challenge"] != challenge
            or hash_re.fullmatch(response["address"]) is None
            or hash_re.fullmatch(response["keyfile_sha256"]) is None):
        raise SystemExit(f"validator identity response tuple differs: {node}")
    response_sha = hashlib.sha256(raw).hexdigest()
    if (response["address"] in addresses or response["keyfile_sha256"] in key_hashes
            or response_sha in response_hashes):
        raise SystemExit("validator identity proof repeats an address, key hash, or response root")
    addresses.add(response["address"]); key_hashes.add(response["keyfile_sha256"])
    response_hashes.add(response_sha)
    rows.append({"node": node, "host": host, "key_path": "/etc/arc-v3/validator-key.json",
                 "address": response["address"], "keyfile_sha256": response["keyfile_sha256"],
                 "remote_response_sha256": response_sha, "state": "verified"})
value = {
    "schema": "arc.recovery.validator-installed-key-proof.v1",
    "source_main_commit": source_commit,
    "production_input_stage_manifest_sha256": stage_sha,
    "freeze_plan_sha256": freeze_sha,
    "offline_stop_evidence_sha256": offline_sha,
    "validator_install_receipt_sha256": install_sha,
    "validator_public_keys_sha256": validators_sha,
    "arc_cli_sha256": cli_sha,
    "remote_helper_sha256": helper_sha,
    "remote_helper_path": helper_path,
    "ssh_known_hosts_sha256": known_sha,
    "ssh_identity_sha256": identity_sha,
    "ssh_path": "/usr/bin/ssh", "ssh_sha256": ssh_sha,
    "scp_path": "/usr/bin/scp", "scp_sha256": scp_sha,
    "challenge": challenge, "started_at_unix_ms": started,
    "completed_at_unix_ms": completed, "validators": rows,
}
payload = canonical(value)
fd = os.open(output, os.O_WRONLY | os.O_CREAT | os.O_EXCL | getattr(os, "O_NOFOLLOW", 0), 0o400)
with os.fdopen(fd, "wb") as handle:
    handle.write(payload); handle.flush(); os.fsync(handle.fileno()); os.fchmod(handle.fileno(), 0o400)
directory = os.open(output.parent, os.O_RDONLY | getattr(os, "O_DIRECTORY", 0))
try: os.fsync(directory)
finally: os.close(directory)
PY
    if [ -n "$output" ]; then
        python3 - "$proof_path" "$output" <<'PY'
import os, pathlib, stat, sys
source, output = map(pathlib.Path, sys.argv[1:])
parent = output.parent; details = parent.lstat()
if (parent.is_symlink() or not parent.is_dir() or details.st_uid != os.getuid()
        or details.st_mode & 0o022):
    raise SystemExit("validator identity proof output parent is unsafe")
payload = source.read_bytes()
fd = os.open(output, os.O_WRONLY | os.O_CREAT | os.O_EXCL | getattr(os, "O_NOFOLLOW", 0), 0o400)
with os.fdopen(fd, "wb") as handle:
    handle.write(payload); handle.flush(); os.fsync(handle.fileno()); os.fchmod(handle.fileno(), 0o400)
directory = os.open(parent, os.O_RDONLY | getattr(os, "O_DIRECTORY", 0))
try: os.fsync(directory)
finally: os.close(directory)
PY
    fi
    cat "$proof_path"
}

upload_immutable() {
    local source="$1" destination="$2"
    rclone copyto "$source" "$destination" --immutable --checksum --metadata \
        --drive-stop-on-upload-limit \
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
    chmod 400 "$inventory"

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
            rclone rcat "$partial_remote" --metadata --streaming-upload-cutoff 1M \
                --drive-stop-on-upload-limit
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
    chmod 400 "$archive_sidecar" "$inventory_sidecar"
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

register_shared_input() {
    local source="$1" expected_sha="$2" catalog_root="$3" name="$4"
    require_hash "$expected_sha" "shared input hash"
    python3 - "$source" "$expected_sha" "$catalog_root" "$name" <<'PY'
import hashlib,json,os,pathlib,re,stat,sys
source=pathlib.Path(sys.argv[1]);expected=sys.argv[2]
root=pathlib.Path(sys.argv[3]);name=sys.argv[4]
canonical=lambda value:(json.dumps(value,sort_keys=True,separators=(",",":"))+"\n").encode()
if re.fullmatch(r"[A-Za-z0-9][A-Za-z0-9._-]{0,127}",name) is None:
    raise SystemExit("shared input archive name is unsafe")
root_details=root.lstat()
if (root.is_symlink() or not stat.S_ISDIR(root_details.st_mode)
        or root_details.st_uid!=os.geteuid() or stat.S_IMODE(root_details.st_mode)!=0o700):
    raise SystemExit("shared input catalog root is unsafe")
if not source.is_absolute() or os.path.normpath(os.fspath(source))!=os.fspath(source):
    raise SystemExit("shared input source path must be normalized and absolute")
flags=os.O_RDONLY|getattr(os,"O_NOFOLLOW",0)|getattr(os,"O_CLOEXEC",0)
descriptor=os.open(source,flags)
identity=lambda value:{
    "device":value.st_dev,"inode":value.st_ino,"mode":stat.S_IMODE(value.st_mode),
    "uid":value.st_uid,"gid":value.st_gid,"nlink":value.st_nlink,"size":value.st_size,
    "mtime_ns":value.st_mtime_ns,"ctime_ns":value.st_ctime_ns,
}
try:
    before=os.fstat(descriptor);visible=os.lstat(source)
    if (not stat.S_ISREG(before.st_mode) or stat.S_ISLNK(visible.st_mode)
            or identity(before)!=identity(visible) or before.st_size<=0):
        raise SystemExit("shared input source is missing, mutable, or non-regular")
    digest=hashlib.sha256();total=0
    while True:
        chunk=os.read(descriptor,1024*1024)
        if not chunk:break
        digest.update(chunk);total+=len(chunk)
    after=os.fstat(descriptor);final_visible=os.lstat(source)
    if (total!=before.st_size or identity(before)!=identity(after)
            or identity(after)!=identity(final_visible)):
        raise SystemExit("shared input source changed while registered")
finally:os.close(descriptor)
observed=digest.hexdigest()
if observed!=expected:raise SystemExit("shared input source hash differs")
value={"schema":"arc.recovery.shared-input-source.v1","archive_name":name,
       "source_path":os.fspath(source),"size":total,"sha256":observed,
       "source_identity":identity(before)}
payload=canonical(value);path=root/name
fd=os.open(path,os.O_WRONLY|os.O_CREAT|os.O_EXCL|getattr(os,"O_NOFOLLOW",0),0o400)
with os.fdopen(fd,"wb") as handle:
    handle.write(payload);handle.flush();os.fsync(handle.fileno());os.fchmod(handle.fileno(),0o400)
directory=os.open(root,os.O_RDONLY|getattr(os,"O_DIRECTORY",0)|getattr(os,"O_NOFOLLOW",0))
try:os.fsync(directory)
finally:os.close(directory)
PY
}

shared_input_descriptor_field() {
    local descriptor="$1" field="$2"
    python3 - "$descriptor" "$field" <<'PY'
import json,os,pathlib,re,stat,sys
path=pathlib.Path(sys.argv[1]);field=sys.argv[2]
visible=path.lstat();fd=os.open(path,os.O_RDONLY|getattr(os,"O_NOFOLLOW",0)|getattr(os,"O_CLOEXEC",0))
descriptor_identity=lambda item:(item.st_dev,item.st_ino,item.st_mode,item.st_uid,item.st_gid,
                                 item.st_nlink,item.st_size,item.st_mtime_ns,item.st_ctime_ns)
try:
    details=os.fstat(fd)
    if details.st_size<=0 or details.st_size>128*1024:raise SystemExit("shared input descriptor size differs")
    raw=b"".join(iter(lambda:os.read(fd,64*1024),b""));after=os.fstat(fd);final_visible=path.lstat()
    if (descriptor_identity(details)!=descriptor_identity(visible)
            or descriptor_identity(details)!=descriptor_identity(after)
            or descriptor_identity(after)!=descriptor_identity(final_visible)):
        raise SystemExit("shared input descriptor changed while read")
finally:os.close(fd)
value=json.loads(raw);canonical=(json.dumps(value,sort_keys=True,separators=(",",":"))+"\n").encode()
if (path.is_symlink() or not stat.S_ISREG(details.st_mode) or details.st_nlink!=1
        or details.st_uid!=os.geteuid() or stat.S_IMODE(details.st_mode)!=0o400 or raw!=canonical
        or value.get("schema")!="arc.recovery.shared-input-source.v1"
        or set(value)!={"schema","archive_name","source_path","size","sha256","source_identity"}
        or value.get("archive_name")!=path.name
        or re.fullmatch(r"[A-Za-z0-9][A-Za-z0-9._-]{0,127}",path.name) is None
        or re.fullmatch(r"[0-9a-f]{64}",value.get("sha256","")) is None
        or isinstance(value.get("size"),bool) or not isinstance(value.get("size"),int) or value["size"]<=0):
    raise SystemExit("shared input descriptor differs")
if field not in {"archive_name","size","sha256"}:raise SystemExit("unknown shared input descriptor field")
print(value[field])
PY
}

stream_shared_input_descriptor() {
    local descriptor="$1" status="$2"
    python3 - "$descriptor" "$status" <<'PY'
import hashlib,json,os,pathlib,re,stat,sys
path=pathlib.Path(sys.argv[1]);status_path=pathlib.Path(sys.argv[2])
canonical=lambda value:(json.dumps(value,sort_keys=True,separators=(",",":"))+"\n").encode()
visible=path.lstat();descriptor_fd=os.open(path,os.O_RDONLY|getattr(os,"O_NOFOLLOW",0)|getattr(os,"O_CLOEXEC",0))
descriptor_identity=lambda item:(item.st_dev,item.st_ino,item.st_mode,item.st_uid,item.st_gid,
                                 item.st_nlink,item.st_size,item.st_mtime_ns,item.st_ctime_ns)
try:
    details=os.fstat(descriptor_fd)
    if details.st_size<=0 or details.st_size>128*1024:raise SystemExit("shared input descriptor size differs")
    raw=b"".join(iter(lambda:os.read(descriptor_fd,64*1024),b""))
    after_descriptor=os.fstat(descriptor_fd);final_visible=path.lstat()
    if (descriptor_identity(details)!=descriptor_identity(visible)
            or descriptor_identity(details)!=descriptor_identity(after_descriptor)
            or descriptor_identity(after_descriptor)!=descriptor_identity(final_visible)):
        raise SystemExit("shared input descriptor changed while read")
finally:os.close(descriptor_fd)
value=json.loads(raw)
if (path.is_symlink() or not stat.S_ISREG(details.st_mode) or details.st_uid!=os.geteuid()
        or details.st_nlink!=1 or stat.S_IMODE(details.st_mode)!=0o400 or raw!=canonical(value)
        or value.get("schema")!="arc.recovery.shared-input-source.v1"
        or set(value)!={"schema","archive_name","source_path","size","sha256","source_identity"}
        or value.get("archive_name")!=path.name
        or re.fullmatch(r"[A-Za-z0-9][A-Za-z0-9._-]{0,127}",path.name) is None
        or re.fullmatch(r"[0-9a-f]{64}",value.get("sha256","")) is None
        or isinstance(value.get("size"),bool) or not isinstance(value.get("size"),int) or value["size"]<=0):
    raise SystemExit("shared input descriptor is unsafe")
source=pathlib.Path(value["source_path"]);expected_identity=value["source_identity"]
identity=lambda item:{
    "device":item.st_dev,"inode":item.st_ino,"mode":stat.S_IMODE(item.st_mode),
    "uid":item.st_uid,"gid":item.st_gid,"nlink":item.st_nlink,"size":item.st_size,
    "mtime_ns":item.st_mtime_ns,"ctime_ns":item.st_ctime_ns,
}
fd=os.open(source,os.O_RDONLY|getattr(os,"O_NOFOLLOW",0)|getattr(os,"O_CLOEXEC",0))
try:
    before=os.fstat(fd);visible=os.lstat(source)
    if (not stat.S_ISREG(before.st_mode) or stat.S_ISLNK(visible.st_mode)
            or identity(before)!=expected_identity or identity(visible)!=expected_identity):
        raise SystemExit("shared input identity changed before stream")
    digest=hashlib.sha256();total=0
    while True:
        chunk=os.read(fd,1024*1024)
        if not chunk:break
        digest.update(chunk);total+=len(chunk);sys.stdout.buffer.write(chunk)
    sys.stdout.buffer.flush();after=os.fstat(fd);final_visible=os.lstat(source)
    if (identity(after)!=expected_identity or identity(final_visible)!=expected_identity
            or total!=value["size"] or digest.hexdigest()!=value["sha256"]):
        raise SystemExit("shared input changed while streamed")
finally:os.close(fd)
status=f"{value['sha256']} {value['size']}\n".encode();temporary=status_path.with_name(f".{status_path.name}.partial")
if status_path.exists() or status_path.is_symlink():
    if status_path.is_symlink() or status_path.read_bytes()!=status:raise SystemExit("shared stream status differs")
else:
    if temporary.exists() or temporary.is_symlink():raise SystemExit("shared stream status partial exists")
    out=os.open(temporary,os.O_WRONLY|os.O_CREAT|os.O_EXCL|getattr(os,"O_NOFOLLOW",0),0o400)
    with os.fdopen(out,"wb") as handle:handle.write(status);handle.flush();os.fsync(handle.fileno())
    os.replace(temporary,status_path)
    parent=os.open(status_path.parent,os.O_RDONLY|getattr(os,"O_DIRECTORY",0)|getattr(os,"O_NOFOLLOW",0))
    try:os.fsync(parent)
    finally:os.close(parent)
PY
}

stream_shared_input_to_drive() {
    local descriptor="$1" destination="$2" work_root="$3"
    local name expected_sha expected_size remote final_status
    name="$(shared_input_descriptor_field "$descriptor" archive_name)"
    expected_sha="$(shared_input_descriptor_field "$descriptor" sha256)"
    expected_size="$(shared_input_descriptor_field "$descriptor" size)"
    remote="$destination/$name"
    final_status="$work_root/shared-$expected_sha.hash-size"
    if rclone cat "$remote" 2>/dev/null | hash_size_stream > "$work_root/shared-$expected_sha.remote" && \
            [ -s "$work_root/shared-$expected_sha.remote" ]; then
        [ "$(cat "$work_root/shared-$expected_sha.remote")" = "$expected_sha $expected_size" ] || \
            die "existing Drive shared input differs: $name"
        stream_shared_input_descriptor "$descriptor" "$final_status" >/dev/null
    else
        local token partial partial_root pipeline_status
        token="$(python3 -c 'import secrets;print(secrets.token_hex(32))')"
        partial_root="${destination%/*}/.arc-recovery-partials/shared"
        partial="$partial_root/$name.$token"
        set +e
        stream_shared_input_descriptor "$descriptor" "$final_status" | \
            rclone rcat "$partial" --metadata --streaming-upload-cutoff 1M --drive-stop-on-upload-limit
        pipeline_status=("${PIPESTATUS[@]}")
        set -e
        if [ "${pipeline_status[0]}" -ne 0 ] || [ "${pipeline_status[1]}" -ne 0 ] || \
                [ ! -s "$final_status" ] || [ "$(cat "$final_status")" != "$expected_sha $expected_size" ]; then
            rclone deletefile "$partial" >/dev/null 2>&1 || true
            die "streaming shared input upload failed: $name"
        fi
        [ "$(rclone cat "$partial" | hash_size_stream)" = "$expected_sha $expected_size" ] || {
            rclone deletefile "$partial" >/dev/null 2>&1 || true
            die "Drive shared-input partial differs: $name"
        }
        rclone moveto "$partial" "$remote" --immutable --checksum --metadata
        [ "$(rclone cat "$remote" | hash_size_stream)" = "$expected_sha $expected_size" ] || \
            die "published Drive shared input differs: $name"
    fi
}

verify_remote_shared_inputs() {
    local catalog_root="$1" destination="$2" work_root="$3" descriptor name sha size status
    for descriptor in "$catalog_root"/*; do
        [ -f "$descriptor" ] && [ ! -L "$descriptor" ] || die "shared input descriptor inventory is unsafe"
        name="$(shared_input_descriptor_field "$descriptor" archive_name)"
        sha="$(shared_input_descriptor_field "$descriptor" sha256)"
        size="$(shared_input_descriptor_field "$descriptor" size)"
        status="$work_root/shared-$sha.hash-size"
        stream_shared_input_descriptor "$descriptor" "$status" >/dev/null
        [ "$(rclone cat "$destination/$name" | hash_size_stream)" = "$sha $size" ] || \
            die "remote shared input verification differs: $name"
    done
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

create_live_observation_fleet_binding() {
    local output="$1" capture_id="$2" freeze_sha="$3" observation_generation="$4"
    local generation_receipt_sha="$5" drive_receipt_sha="$6" selection_sha="$7"
    local statuses="$8"
    local node
    : > "$statuses"
    chmod 600 "$statuses"
    for node in nyc lax ams lhr nrt sgp; do
        run_remote "$node" live-observations-status "$capture_id" "$observation_generation" \
            "$generation_receipt_sha" "$drive_receipt_sha" "$node" "$freeze_sha" >> "$statuses"
    done
    python3 - "$output" "$statuses" "$capture_id" "$freeze_sha" \
        "$observation_generation" "$generation_receipt_sha" "$drive_receipt_sha" \
        "$selection_sha" <<'PY'
import json
import os
import pathlib
import re
import stat
import sys

output, statuses = map(pathlib.Path, sys.argv[1:3])
capture_id, freeze_sha, generation, generation_receipt_sha, drive_receipt_sha, selection_sha = sys.argv[3:]
nodes = ("nyc", "lax", "ams", "lhr", "nrt", "sgp")
hash_re = re.compile(r"[0-9a-f]{64}")
rows = [json.loads(line) for line in statuses.read_text(encoding="utf-8").splitlines() if line]
if len(rows) != 6 or [row.get("node") for row in rows] != list(nodes):
    raise SystemExit("live-observation status set does not contain the ordered six validators")
normalized = []
for node, row in zip(nodes, rows):
    if (not isinstance(row, dict) or set(row) != {
            "schema", "capture_id", "observation_generation",
            "observation_generation_receipt_sha256", "drive_prefreeze_receipt_sha256",
            "node", "freeze_plan_sha256", "created_at", "completed_at",
            "root_sha256", "receipt_sha256", "labels",
        } or row.get("schema") != "arc.recovery.legacy-live-observations-status.v1"
            or row.get("capture_id") != capture_id or row.get("node") != node
            or row.get("observation_generation") != generation
            or row.get("observation_generation_receipt_sha256") != generation_receipt_sha
            or row.get("drive_prefreeze_receipt_sha256") != drive_receipt_sha
            or row.get("freeze_plan_sha256") != freeze_sha
            or row.get("labels") != ["diagnostic", "noncanonical", "nonreward"]
            or not hash_re.fullmatch(row.get("root_sha256", ""))
            or not hash_re.fullmatch(row.get("receipt_sha256", ""))):
        raise SystemExit(f"live-observation status is malformed for {node}")
    normalized.append({
        "node": node,
        "root_sha256": row["root_sha256"],
        "receipt_sha256": row["receipt_sha256"],
    })
value = {
    "schema": "arc.recovery.legacy-live-observations-fleet.v1",
    "capture_id": capture_id,
    "freeze_plan_sha256": freeze_sha,
    "observation_generation": generation,
    "observation_generation_receipt_sha256": generation_receipt_sha,
    "drive_prefreeze_receipt_sha256": drive_receipt_sha,
    "live_observation_selection_sha256": selection_sha,
    "receipt_schema": "arc.recovery.legacy-live-observations.v1",
    "labels": ["diagnostic", "noncanonical", "nonreward"],
    "nodes": normalized,
}
payload = (json.dumps(value, sort_keys=True, separators=(",", ":")) + "\n").encode()
descriptor = os.open(output, os.O_WRONLY | os.O_CREAT | os.O_EXCL | getattr(os, "O_NOFOLLOW", 0), 0o400)
with os.fdopen(descriptor, "wb") as handle:
    handle.write(payload); handle.flush(); os.fsync(handle.fileno())
directory = os.open(output.parent, os.O_RDONLY | getattr(os, "O_DIRECTORY", 0))
try: os.fsync(directory)
finally: os.close(directory)
PY
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
import stat
import sys

(output_raw, shared_raw, allow_unbound_raw, source_height_raw, source_hash_raw,
 source_state_root_raw, transition_state_root_raw, checkpoint_manifest_raw,
 source_round_raw, created_at_raw, recovery_epoch_raw, validator_set_id_raw,
 binary_sha, genesis_sha, validators_sha, legacy_validators_sha,
 snapshot_sha, wal_sha, checkpoint_sha) = sys.argv[1:]
output = pathlib.Path(output_raw)
catalog = pathlib.Path(shared_raw)
hash_re = re.compile(r"[0-9a-f]{64}")

def artifact(name, expected):
    descriptor_path=catalog/name;visible=descriptor_path.lstat()
    descriptor_fd=os.open(descriptor_path,os.O_RDONLY|getattr(os,"O_NOFOLLOW",0)|getattr(os,"O_CLOEXEC",0))
    descriptor_identity=lambda value:(value.st_dev,value.st_ino,value.st_mode,value.st_uid,
        value.st_gid,value.st_nlink,value.st_size,value.st_mtime_ns,value.st_ctime_ns)
    try:
        details=os.fstat(descriptor_fd)
        if details.st_size<=0 or details.st_size>128*1024:
            raise SystemExit(f"canonical reference descriptor size differs: {name}")
        raw=b"".join(iter(lambda:os.read(descriptor_fd,64*1024),b""))
        after_descriptor=os.fstat(descriptor_fd);final_visible=descriptor_path.lstat()
        if (descriptor_identity(details)!=descriptor_identity(visible)
                or descriptor_identity(details)!=descriptor_identity(after_descriptor)
                or descriptor_identity(after_descriptor)!=descriptor_identity(final_visible)):
            raise SystemExit(f"canonical reference descriptor changed while read: {name}")
    finally:os.close(descriptor_fd)
    descriptor=json.loads(raw);canonical=(json.dumps(descriptor,sort_keys=True,separators=(",",":"))+"\n").encode()
    if (descriptor_path.is_symlink() or not stat.S_ISREG(details.st_mode)
            or details.st_uid!=os.geteuid() or details.st_nlink!=1
            or stat.S_IMODE(details.st_mode)!=0o400
            or raw!=canonical or descriptor.get("schema")!="arc.recovery.shared-input-source.v1"
            or set(descriptor)!={"schema","archive_name","source_path","size","sha256","source_identity"}
            or descriptor.get("archive_name")!=name
            or not isinstance(descriptor.get("source_path"),str)
            or isinstance(descriptor.get("size"),bool) or not isinstance(descriptor.get("size"),int)
            or descriptor["size"]<=0 or hash_re.fullmatch(descriptor.get("sha256","")) is None):
        raise SystemExit(f"canonical reference descriptor is missing or unsafe: {name}")
    path=pathlib.Path(descriptor["source_path"]);wanted=descriptor["source_identity"]
    identity=lambda value:{"device":value.st_dev,"inode":value.st_ino,"mode":stat.S_IMODE(value.st_mode),
        "uid":value.st_uid,"gid":value.st_gid,"nlink":value.st_nlink,"size":value.st_size,
        "mtime_ns":value.st_mtime_ns,"ctime_ns":value.st_ctime_ns}
    fd=os.open(path,os.O_RDONLY|getattr(os,"O_NOFOLLOW",0))
    try:
        before=os.fstat(fd);visible=os.lstat(path);value=hashlib.sha256();total=0
        if not stat.S_ISREG(before.st_mode) or stat.S_ISLNK(visible.st_mode) or identity(before)!=wanted or identity(visible)!=wanted:
            raise SystemExit(f"canonical reference input identity differs: {name}")
        while True:
            chunk=os.read(fd,1024*1024)
            if not chunk:break
            value.update(chunk);total+=len(chunk)
        after=os.fstat(fd);final_visible=os.lstat(path)
    finally:os.close(fd)
    actual=value.hexdigest()
    if (identity(after)!=wanted or identity(final_visible)!=wanted or total!=descriptor["size"]
            or not hash_re.fullmatch(expected) or actual!=expected or actual!=descriptor["sha256"]):
        raise SystemExit(f"canonical reference input hash differs: {name}")
    return {"name":name,"size":total,"sha256":actual}

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
    local shared_root="$1" statuses="$2" metadata_root="$3"
    local freeze_sha="$4" capture_id="$5" manifest_sha="$6" source_commit="$7"
    local orchestrator_sha="$8" helper_sha="$9"
    local rollout_tool_sha="${10}" schema_sha="${11}"
    local canonical_count="${12}" fork_count="${13}" unclassified_count="${14}"
    mkdir -p -- "$metadata_root"
    python3 - "$shared_root" "$statuses" "$metadata_root/SHA256SUMS" \
        "$metadata_root/ARCHIVE-MANIFEST.json" \
        "$metadata_root/ARCHIVE-MANIFEST.json.sha256" \
        "$freeze_sha" "$capture_id" "$manifest_sha" "$source_commit" \
        "$orchestrator_sha" "$helper_sha" \
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
 freeze_sha, capture_id, rollout_sha, source_commit,
 orchestrator_sha, helper_sha, rollout_tool_sha, schema_sha, canonical_raw, fork_raw,
 unclassified_raw) = sys.argv[1:]
catalog_root = pathlib.Path(shared_root_raw)
statuses_path = pathlib.Path(statuses_raw)
sums_path = pathlib.Path(sums_raw)
manifest_path = pathlib.Path(manifest_raw)
manifest_sidecar_path = pathlib.Path(manifest_sidecar_raw)
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

catalog_details=catalog_root.lstat()
if (catalog_root.is_symlink() or not stat.S_ISDIR(catalog_details.st_mode)
        or catalog_details.st_uid!=os.geteuid() or stat.S_IMODE(catalog_details.st_mode)!=0o700):
    raise SystemExit("shared input catalog root is unsafe")
identity=lambda value:{"device":value.st_dev,"inode":value.st_ino,"mode":stat.S_IMODE(value.st_mode),
    "uid":value.st_uid,"gid":value.st_gid,"nlink":value.st_nlink,"size":value.st_size,
    "mtime_ns":value.st_mtime_ns,"ctime_ns":value.st_ctime_ns}
def read_descriptor(path):
    visible=path.lstat();fd=os.open(path,os.O_RDONLY|getattr(os,"O_NOFOLLOW",0)|getattr(os,"O_CLOEXEC",0))
    try:
        before=os.fstat(fd)
        if before.st_size<=0 or before.st_size>128*1024:raise SystemExit("shared input descriptor size differs")
        raw=b"".join(iter(lambda:os.read(fd,64*1024),b""));after=os.fstat(fd);final_visible=path.lstat()
        if identity(before)!=identity(visible) or identity(before)!=identity(after) or identity(after)!=identity(final_visible):
            raise SystemExit("shared input descriptor changed while read")
        return before,raw
    finally:os.close(fd)
semantic_names={"legacy-live-observations.json","canonical-reference.json","offline-stop-evidence.json"}
semantic_payloads={};shared_inputs=[]
for descriptor_path in sorted(catalog_root.iterdir(),key=lambda item:item.name):
    details,raw=read_descriptor(descriptor_path);descriptor=json.loads(raw)
    if (descriptor_path.is_symlink() or not stat.S_ISREG(details.st_mode)
            or details.st_uid!=os.geteuid() or details.st_nlink!=1
            or stat.S_IMODE(details.st_mode)!=0o400 or raw!=canonical(descriptor)
            or descriptor.get("schema")!="arc.recovery.shared-input-source.v1"
            or descriptor.get("archive_name")!=descriptor_path.name
            or re.fullmatch(r"[A-Za-z0-9][A-Za-z0-9._-]{0,127}",descriptor_path.name) is None):
        raise SystemExit(f"shared input descriptor is unsafe: {descriptor_path}")
    source=pathlib.Path(descriptor["source_path"]);wanted=descriptor["source_identity"]
    fd=os.open(source,os.O_RDONLY|getattr(os,"O_NOFOLLOW",0)|getattr(os,"O_CLOEXEC",0))
    try:
        before=os.fstat(fd);visible=os.lstat(source);hasher=hashlib.sha256();total=0
        if not stat.S_ISREG(before.st_mode) or stat.S_ISLNK(visible.st_mode) or identity(before)!=wanted or identity(visible)!=wanted:
            raise SystemExit(f"shared input identity differs: {descriptor_path.name}")
        semantic=bytearray() if descriptor_path.name in semantic_names else None
        if semantic is not None and before.st_size>4*1024*1024:
            raise SystemExit(f"shared semantic input exceeds its bounded size: {descriptor_path.name}")
        while True:
            chunk=os.read(fd,1024*1024)
            if not chunk:break
            hasher.update(chunk);total+=len(chunk)
            if semantic is not None:semantic.extend(chunk)
        after=os.fstat(fd);final_visible=os.lstat(source)
    finally:os.close(fd)
    observed=hasher.hexdigest()
    if (identity(after)!=wanted or identity(final_visible)!=wanted or total!=descriptor.get("size")
            or observed!=descriptor.get("sha256")):
        raise SystemExit(f"shared input changed while metadata was built: {descriptor_path.name}")
    if descriptor_path.name in sums:raise SystemExit("shared archive input collides with a bundle object")
    sums[descriptor_path.name]=observed
    shared_inputs.append({"name":descriptor_path.name,"size":total,"sha256":observed})
    if semantic is not None:semantic_payloads[descriptor_path.name]=bytes(semantic)

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
observations_payload = semantic_payloads.get("legacy-live-observations.json")
if not isinstance(observations_payload, bytes):
    raise SystemExit("fleet live-observation binding is missing")
try:
    live_observations = json.loads(observations_payload.decode("utf-8"))
except (UnicodeDecodeError, json.JSONDecodeError) as error:
    raise SystemExit(f"fleet live-observation binding is unreadable: {error}")
if observations_payload != canonical(live_observations):
    raise SystemExit("fleet live-observation binding is not canonical JSON")
if (not isinstance(live_observations, dict) or set(live_observations) != {
        "schema", "capture_id", "freeze_plan_sha256", "observation_generation",
        "observation_generation_receipt_sha256", "drive_prefreeze_receipt_sha256",
        "live_observation_selection_sha256", "receipt_schema", "labels", "nodes",
    } or live_observations.get("schema") != "arc.recovery.legacy-live-observations-fleet.v1"
        or live_observations.get("capture_id") != capture_id
        or live_observations.get("freeze_plan_sha256") != freeze_sha
        or live_observations.get("receipt_schema") != "arc.recovery.legacy-live-observations.v1"
        or live_observations.get("labels") != ["diagnostic", "noncanonical", "nonreward"]):
    raise SystemExit("fleet live-observation binding fields/identity differ")
offline_payload=semantic_payloads.get("offline-stop-evidence.json")
try:offline=json.loads(offline_payload.decode("utf-8"))
except (AttributeError,UnicodeDecodeError,json.JSONDecodeError) as error:
    raise SystemExit(f"offline-stop observation provenance is unreadable: {error}")
if (offline_payload!=canonical(offline)
        or (live_observations.get("live_observation_selection_sha256"),
            live_observations.get("observation_generation"),
            live_observations.get("observation_generation_receipt_sha256"),
            live_observations.get("drive_prefreeze_receipt_sha256"))
           !=(offline.get("legacy_live_observation_selection_sha256"),
              offline.get("legacy_live_observation_generation"),
              offline.get("observation_generation_receipt_sha256"),
              offline.get("drive_prefreeze_receipt_sha256"))):
    raise SystemExit("archive live-observation binding differs from offline-stop provenance")
live_rows = live_observations.get("nodes")
if not isinstance(live_rows, list) or [row.get("node") for row in live_rows] != list(nodes):
    raise SystemExit("fleet live-observation binding does not contain the ordered six validators")
for node, row in zip(nodes, live_rows):
    if (not isinstance(row, dict) or set(row) != {"node", "root_sha256", "receipt_sha256"}
            or row.get("node") != node
            or not hash_re.fullmatch(row.get("root_sha256", ""))
            or not hash_re.fullmatch(row.get("receipt_sha256", ""))):
        raise SystemExit(f"fleet live-observation node binding is malformed: {node}")
if shared_by_name.get("legacy-live-observations.json") != {
    "name": "legacy-live-observations.json",
    "size": len(observations_payload),
    "sha256": hashlib.sha256(observations_payload).hexdigest(),
}:
    raise SystemExit("archive shared inputs do not bind the fleet live-observation receipt roots")
reference_payload = semantic_payloads.get("canonical-reference.json")
if not isinstance(reference_payload, bytes):
    raise SystemExit("canonical reference evidence is missing")
try:
    canonical_reference = json.loads(reference_payload.decode("utf-8"))
except (UnicodeDecodeError, json.JSONDecodeError) as error:
    raise SystemExit(f"canonical reference evidence is unreadable: {error}")
if reference_payload != canonical(canonical_reference):
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
for directory in {sums_path.parent, manifest_path.parent}:
    directory_fd = os.open(directory, os.O_RDONLY)
    try:
        os.fsync(directory_fd)
    finally:
        os.close(directory_fd)
print(archive_manifest_sha)
PY
}

seal_archive_finalization_intent() {
    local intent="$1" shared_root="$2" statuses="$3" sums="$4"
    local manifest="$5" sidecar="$6" freeze_sha="$7"
    local capture_id="$8" rollout_sha="$9" source_commit="${10}"
    local destination="${11}" github_login="${12}"
    python3 - "$intent" "$shared_root" "$statuses" "$sums" "$manifest" \
        "$sidecar" "$freeze_sha" "$capture_id" "$rollout_sha" \
        "$source_commit" "$destination" "$github_login" <<'PY'
import hashlib
import json
import os
import pathlib
import re
import stat
import sys

(intent_raw, shared_raw, statuses_raw, sums_raw, manifest_raw, sidecar_raw,
 freeze_sha, capture_id, rollout_sha, source_commit,
 destination, github_login) = sys.argv[1:]
intent = pathlib.Path(intent_raw)
intent_sidecar = intent.with_name(intent.name + ".sha256")
catalog_root = pathlib.Path(shared_raw)
statuses_path = pathlib.Path(statuses_raw)
sums_path = pathlib.Path(sums_raw)
manifest_path = pathlib.Path(manifest_raw)
manifest_sidecar_path = pathlib.Path(sidecar_raw)
hash_re = re.compile(r"[0-9a-f]{64}")
commit_re = re.compile(r"[0-9a-f]{40}(?:[0-9a-f]{24})?")
name_re = re.compile(r"[A-Za-z0-9][A-Za-z0-9._-]{0,127}")
nodes = ("nyc", "lax", "ams", "lhr", "nrt", "sgp")
classifications = {"valid_canonical", "valid_noncanonical_fork", "preserved_unclassified"}
canonical = lambda value: (json.dumps(value, sort_keys=True, separators=(",", ":")) + "\n").encode()
digest = lambda payload: hashlib.sha256(payload).hexdigest()

if (not intent.is_absolute() or intent.suffix != ".json"
        or os.fspath(intent) != os.path.normpath(os.fspath(intent))
        or os.path.realpath(intent) != os.fspath(intent)):
    raise SystemExit("archive finalization intent path must be normalized absolute .json")
parent_details = intent.parent.lstat()
if (intent.parent.is_symlink() or not stat.S_ISDIR(parent_details.st_mode)
        or parent_details.st_uid != os.geteuid() or parent_details.st_mode & 0o022):
    raise SystemExit("archive finalization intent parent must be a real protected operator directory")
parent_fd = os.open(
    intent.parent,
    os.O_RDONLY | getattr(os, "O_DIRECTORY", 0) | getattr(os, "O_NOFOLLOW", 0),
)
opened_parent = os.fstat(parent_fd)
if (opened_parent.st_dev, opened_parent.st_ino) != (parent_details.st_dev, parent_details.st_ino):
    os.close(parent_fd)
    raise SystemExit("archive finalization intent parent changed while opened")
if (not hash_re.fullmatch(freeze_sha) or not hash_re.fullmatch(capture_id)
        or not hash_re.fullmatch(rollout_sha) or not commit_re.fullmatch(source_commit)):
    raise SystemExit("archive finalization intent scalar identity is malformed")
if not destination or "\x00" in destination or "\n" in destination or "\r" in destination:
    raise SystemExit("archive finalization destination is unsafe")
if (re.fullmatch(r"[A-Za-z0-9](?:[A-Za-z0-9-]{0,37}[A-Za-z0-9])?", github_login) is None
        or "--" in github_login):
    raise SystemExit("archive finalization GitHub owner is malformed")

def read_regular(path, label, maximum=16 * 1024 * 1024, exact_mode=None, nlink1=False,
                 materialize=True):
    flags = os.O_RDONLY | getattr(os, "O_NOFOLLOW", 0) | getattr(os, "O_CLOEXEC", 0)
    descriptor = os.open(path, flags)
    try:
        before = os.fstat(descriptor)
        visible = os.lstat(path)
        identity = lambda value: (
            value.st_dev, value.st_ino, value.st_mode, value.st_uid, value.st_nlink,
            value.st_size, value.st_mtime_ns, value.st_ctime_ns,
        )
        if (not stat.S_ISREG(before.st_mode) or stat.S_ISLNK(visible.st_mode)
                or identity(before) != identity(visible) or before.st_size <= 0
                or before.st_size > maximum or before.st_uid != os.geteuid()):
            raise SystemExit(f"{label} is missing, mutable, oversized, or non-regular")
        if exact_mode is not None and stat.S_IMODE(before.st_mode) != exact_mode:
            raise SystemExit(f"{label} mode differs from {exact_mode:04o}")
        if nlink1 and before.st_nlink != 1:
            raise SystemExit(f"{label} must be single-linked")
        hasher = hashlib.sha256()
        payload = bytearray() if materialize else None
        total = 0
        remaining = maximum + 1
        while remaining:
            chunk = os.read(descriptor, min(1024 * 1024, remaining))
            if not chunk:
                break
            total += len(chunk); hasher.update(chunk); remaining -= len(chunk)
            if payload is not None: payload.extend(chunk)
        after = os.fstat(descriptor); final_visible = os.lstat(path)
        if (total != before.st_size or identity(before) != identity(after)
                or identity(after) != identity(final_visible)):
            raise SystemExit(f"{label} changed while read")
        return (bytes(payload) if payload is not None else None), before, hasher.hexdigest()
    finally:
        os.close(descriptor)

def canonical_object(path, label):
    raw, details, _observed_sha = read_regular(path, label)
    assert raw is not None
    try:
        value = json.loads(raw)
    except json.JSONDecodeError as error:
        raise SystemExit(f"{label} is invalid JSON: {error}")
    if not isinstance(value, dict) or raw != canonical(value):
        raise SystemExit(f"{label} is not one canonical JSON object")
    return value, raw, details

manifest, manifest_payload, manifest_details = canonical_object(manifest_path, "archive manifest")
sidecar_payload, sidecar_details, sidecar_sha = read_regular(manifest_sidecar_path, "archive manifest sidecar", 512)
sums_payload, sums_details, sums_sha = read_regular(sums_path, "archive SHA256SUMS", 4 * 1024 * 1024)
assert sidecar_payload is not None and sums_payload is not None
manifest_sha = digest(manifest_payload)
if sidecar_payload != f"{manifest_sha}  ARCHIVE-MANIFEST.json\n".encode("ascii"):
    raise SystemExit("archive manifest sidecar differs from exact manifest bytes")
if (manifest.get("freeze_plan_sha256"), manifest.get("capture_id"),
        manifest.get("rollout_manifest_sha256"), manifest.get("source_commit")) != (
            freeze_sha, capture_id, rollout_sha, source_commit):
    raise SystemExit("archive manifest scalar identity differs from finalization intent")
if manifest.get("sha256sums") != {
    "name": "SHA256SUMS", "size": sums_details.st_size, "sha256": sums_sha,
}:
    raise SystemExit("archive manifest SHA256SUMS root differs from local bytes")

shared=[]
shared_details=os.lstat(catalog_root)
if (stat.S_ISLNK(shared_details.st_mode) or not stat.S_ISDIR(shared_details.st_mode)
        or shared_details.st_uid!=os.geteuid() or stat.S_IMODE(shared_details.st_mode)!=0o700):
    raise SystemExit("archive shared-input catalog is unsafe")
source_identity=lambda item:{"device":item.st_dev,"inode":item.st_ino,"mode":stat.S_IMODE(item.st_mode),
    "uid":item.st_uid,"gid":item.st_gid,"nlink":item.st_nlink,"size":item.st_size,
    "mtime_ns":item.st_mtime_ns,"ctime_ns":item.st_ctime_ns}
for descriptor_path in sorted(catalog_root.iterdir(),key=lambda item:item.name):
    if name_re.fullmatch(descriptor_path.name) is None:raise SystemExit("archive shared input name is unsafe")
    descriptor_raw,_descriptor_details,_descriptor_sha=read_regular(
        descriptor_path,f"shared descriptor {descriptor_path.name}",128*1024,0o400,True
    )
    descriptor=json.loads(descriptor_raw)
    if (descriptor_raw!=canonical(descriptor) or descriptor.get("schema")!="arc.recovery.shared-input-source.v1"
            or descriptor.get("archive_name")!=descriptor_path.name):
        raise SystemExit("archive shared input descriptor differs")
    source=pathlib.Path(descriptor["source_path"]);wanted=descriptor["source_identity"]
    fd=os.open(source,os.O_RDONLY|getattr(os,"O_NOFOLLOW",0)|getattr(os,"O_CLOEXEC",0))
    try:
        before=os.fstat(fd);visible=os.lstat(source);hasher=hashlib.sha256();total=0
        if (not stat.S_ISREG(before.st_mode) or stat.S_ISLNK(visible.st_mode)
                or source_identity(before)!=wanted or source_identity(visible)!=wanted):
            raise SystemExit(f"archive shared input identity differs: {descriptor_path.name}")
        while True:
            chunk=os.read(fd,1024*1024)
            if not chunk:break
            hasher.update(chunk);total+=len(chunk)
        after=os.fstat(fd);final_visible=os.lstat(source)
    finally:os.close(fd)
    observed_sha=hasher.hexdigest()
    if (source_identity(after)!=wanted or source_identity(final_visible)!=wanted
            or total!=descriptor.get("size") or observed_sha!=descriptor.get("sha256")):
        raise SystemExit(f"archive shared input changed during finalization: {descriptor_path.name}")
    shared.append({"name":descriptor_path.name,"size":total,"sha256":observed_sha})
if manifest.get("shared_inputs") != shared:
    raise SystemExit("archive manifest shared roots differ from current sealed local inputs")
shared_by_name = {row["name"]: row for row in shared}
for name, expected_payload in (
    ("source-commit.txt", f"{source_commit}\n".encode("ascii")),
    ("capture-id.txt", f"{capture_id}\n".encode("ascii")),
    ("freeze-plan.json.sha256", f"{freeze_sha}  freeze-plan.json\n".encode("ascii")),
    ("rollout-manifest.json.sha256", f"{rollout_sha}  rollout-manifest.json\n".encode("ascii")),
):
    item = shared_by_name.get(name)
    if item != {"name": name, "size": len(expected_payload), "sha256": digest(expected_payload)}:
        raise SystemExit(f"archive shared scalar link differs: {name}")
if shared_by_name.get("freeze-plan.json", {}).get("sha256") != freeze_sha:
    raise SystemExit("archive shared freeze plan differs from freeze root")
if shared_by_name.get("rollout-manifest.json", {}).get("sha256") != rollout_sha:
    raise SystemExit("archive shared rollout manifest differs from prearchive root")

try:
    status_rows = [json.loads(line) for line in statuses_path.read_text(encoding="utf-8").splitlines() if line]
except (OSError, json.JSONDecodeError) as error:
    raise SystemExit(f"archive bundle statuses are unreadable: {error}")
if [row.get("node") for row in status_rows] != list(nodes):
    raise SystemExit("archive bundle statuses omit or reorder the fixed six nodes")
bundles = []
for node, row in zip(nodes, status_rows):
    if (not isinstance(row, dict) or set(row) != {
            "schema", "capture_id", "node", "rollout_manifest_sha256",
            "classification", "bundle", "inventory",
        } or row.get("schema") != "arc.recovery.bundle-status.v1"
            or row.get("capture_id") != capture_id or row.get("node") != node
            or row.get("rollout_manifest_sha256") != rollout_sha
            or row.get("classification") not in classifications):
        raise SystemExit(f"archive bundle status differs for {node}")
    bundles.append({
        "node": node, "classification": row["classification"],
        "bundle": row["bundle"], "inventory": row["inventory"],
    })
if manifest.get("validator_bundles") != bundles:
    raise SystemExit("archive manifest bundle/classification roots differ from local statuses")

value = {
    "schema": "arc.recovery.archive-finalization-intent.v2",
    "source_commit": source_commit,
    "freeze_plan_sha256": freeze_sha,
    "capture_id": capture_id,
    "prearchive_rollout_sha256": rollout_sha,
    "destination": destination,
    "destination_sha256": digest(destination.encode("utf-8")),
    "archive_manifest": {"name": "ARCHIVE-MANIFEST.json", "size": manifest_details.st_size, "sha256": manifest_sha},
    "archive_manifest_sidecar": {"name": "ARCHIVE-MANIFEST.json.sha256", "size": sidecar_details.st_size, "sha256": sidecar_sha},
    "sha256sums": {"name": "SHA256SUMS", "size": sums_details.st_size, "sha256": sums_sha},
    "shared_inputs": shared,
    "validator_bundles": bundles,
    "capture_classification_counts": manifest.get("capture_classification_counts"),
    "github_anchor_policy": {
        "provider": "github.com",
        "owner_login": github_login,
        "visibility": "secret",
        "filename": f"arc-recovery-{capture_id}.finalization-intent.json",
    },
}
payload = canonical(value)
intent_sha = digest(payload)
sidecar = f"{intent_sha}  {intent.name}\n".encode("ascii")
def publish(path, body, label):
    partial = path.with_name(path.name + ".partial")
    if path.exists() or path.is_symlink():
        existing, _details, _sha = read_regular(
            path, f"existing {label}", 32 * 1024 * 1024, 0o400, True
        )
        if existing != body:
            raise SystemExit(f"existing archive finalization {label} differs")
        if partial.exists() or partial.is_symlink():
            _raw, details, _sha = read_regular(
                partial, f"uncommitted {label}", 32 * 1024 * 1024, None, True
            )
            if stat.S_IMODE(details.st_mode) not in {0o400, 0o600}:
                raise SystemExit(f"archive finalization {label} partial mode differs")
            os.unlink(partial.name, dir_fd=parent_fd); os.fsync(parent_fd)
        return
    promote = False
    if partial.exists() or partial.is_symlink():
        existing, details, _sha = read_regular(
            partial, f"uncommitted {label}", 32 * 1024 * 1024, None, True
        )
        if stat.S_IMODE(details.st_mode) not in {0o400, 0o600}:
            raise SystemExit(f"archive finalization {label} partial mode differs")
        if existing == body:
            os.chmod(partial.name, 0o400, dir_fd=parent_fd, follow_symlinks=False)
            promote = True
        else:
            os.unlink(partial.name, dir_fd=parent_fd); os.fsync(parent_fd)
    if not promote:
        descriptor = os.open(
            partial.name,
            os.O_WRONLY | os.O_CREAT | os.O_EXCL | getattr(os, "O_NOFOLLOW", 0),
            0o600,
            dir_fd=parent_fd,
        )
        with os.fdopen(descriptor, "wb") as handle:
            handle.write(body); handle.flush(); os.fsync(handle.fileno())
            os.fchmod(handle.fileno(), 0o400)
    os.rename(partial.name, path.name, src_dir_fd=parent_fd, dst_dir_fd=parent_fd)
    os.fsync(parent_fd)
try:
    publish(intent, payload, "intent")
    publish(intent_sidecar, sidecar, "intent sidecar")
finally:
    os.close(parent_fd)
print(intent_sha)
PY
}

archive_finalization_intent_roots() {
    local intent="$1" shared_root="$2" freeze_sha="$3" capture_id="$4"
    local rollout_sha="$5" source_commit="$6" destination="$7"
    python3 - "$intent" "$shared_root" "$freeze_sha" "$capture_id" \
        "$rollout_sha" "$source_commit" "$destination" <<'PY'
import hashlib, json, os, pathlib, re, stat, sys
intent_raw, shared_raw, freeze_sha, capture_id, rollout_sha, source_commit, destination = sys.argv[1:]
intent = pathlib.Path(intent_raw); sidecar = intent.with_name(intent.name + ".sha256")
catalog_root = pathlib.Path(shared_raw)
canonical = lambda value: (json.dumps(value, sort_keys=True, separators=(",", ":")) + "\n").encode()
digest = lambda payload: hashlib.sha256(payload).hexdigest()
fields = {
    "schema", "source_commit", "freeze_plan_sha256", "capture_id",
    "prearchive_rollout_sha256", "destination", "destination_sha256",
    "archive_manifest", "archive_manifest_sidecar", "sha256sums",
    "shared_inputs", "validator_bundles", "capture_classification_counts",
    "github_anchor_policy",
}
if (not intent.is_absolute() or os.path.normpath(os.fspath(intent)) != os.fspath(intent)
        or os.path.realpath(intent) != os.fspath(intent)):
    raise SystemExit("archive finalization intent path/ancestry is unsafe")
parent_details = intent.parent.lstat()
if (intent.parent.is_symlink() or not stat.S_ISDIR(parent_details.st_mode)
        or parent_details.st_uid != os.geteuid() or parent_details.st_mode & 0o022):
    raise SystemExit("archive finalization intent parent is not protected")
parent_fd = os.open(
    intent.parent,
    os.O_RDONLY | getattr(os, "O_DIRECTORY", 0) | getattr(os, "O_NOFOLLOW", 0),
)
opened_parent = os.fstat(parent_fd)
if (opened_parent.st_dev, opened_parent.st_ino) != (parent_details.st_dev, parent_details.st_ino):
    os.close(parent_fd)
    raise SystemExit("archive finalization intent parent changed while opened")
def locked(path, maximum, materialize=True):
    fd = os.open(path, os.O_RDONLY | getattr(os, "O_NOFOLLOW", 0))
    try:
        before = os.fstat(fd); visible = os.lstat(path)
        identity = lambda value: (
            value.st_dev, value.st_ino, value.st_mode, value.st_uid, value.st_gid,
            value.st_nlink, value.st_size, value.st_mtime_ns, value.st_ctime_ns,
        )
        if (not stat.S_ISREG(before.st_mode) or stat.S_ISLNK(visible.st_mode)
                or identity(before) != identity(visible)
                or stat.S_IMODE(before.st_mode) != 0o400 or before.st_nlink != 1
                or before.st_uid != os.geteuid()
                or before.st_size <= 0 or before.st_size > maximum):
            raise SystemExit(f"archive finalization seal is unsafe: {path}")
        hasher = hashlib.sha256(); raw = bytearray() if materialize else None; total = 0
        while total <= maximum:
            chunk = os.read(fd, min(1024 * 1024, maximum + 1 - total))
            if not chunk: break
            total += len(chunk); hasher.update(chunk)
            if raw is not None: raw.extend(chunk)
        after = os.fstat(fd); final_visible = os.lstat(path)
        if (total != before.st_size or identity(before) != identity(after)
                or identity(after) != identity(final_visible)):
            raise SystemExit("archive finalization seal changed while read")
        return (bytes(raw) if raw is not None else None), before.st_size, hasher.hexdigest()
    finally: os.close(fd)
try:
    raw, _intent_size, intent_sha = locked(intent, 32 * 1024 * 1024)
    assert raw is not None
    value = json.loads(raw)
    if raw != canonical(value) or not isinstance(value, dict) or set(value) != fields:
        raise SystemExit("archive finalization intent is noncanonical or inexact")
    sidecar_raw, _sidecar_size, _sidecar_sha = locked(sidecar, 512)
    if sidecar_raw != f"{intent_sha}  {intent.name}\n".encode("ascii"):
        raise SystemExit("archive finalization intent sidecar differs")
finally:
    os.close(parent_fd)
expected_scalars = {
    "schema": "arc.recovery.archive-finalization-intent.v2",
    "source_commit": source_commit, "freeze_plan_sha256": freeze_sha,
    "capture_id": capture_id, "prearchive_rollout_sha256": rollout_sha,
    "destination": destination, "destination_sha256": digest(destination.encode()),
}
if any(value.get(key) != expected for key, expected in expected_scalars.items()):
    raise SystemExit("archive finalization intent scalar binding differs")
nodes = ("nyc", "lax", "ams", "lhr", "nrt", "sgp")
bundles = value.get("validator_bundles")
if not isinstance(bundles, list) or [row.get("node") for row in bundles] != list(nodes):
    raise SystemExit("archive finalization intent omits/reorders validator bundles")
policy = value.get("github_anchor_policy")
if (not isinstance(policy, dict) or set(policy) != {
        "provider", "owner_login", "visibility", "filename",
    } or policy.get("provider") != "github.com" or policy.get("visibility") != "secret"
        or re.fullmatch(r"[A-Za-z0-9](?:[A-Za-z0-9-]{0,37}[A-Za-z0-9])?",
                        policy.get("owner_login", "")) is None
        or policy.get("filename") != f"arc-recovery-{capture_id}.finalization-intent.json"):
    raise SystemExit("archive finalization intent GitHub anchor policy differs")
current_shared = []
catalog_details=catalog_root.lstat()
if (catalog_root.is_symlink() or not stat.S_ISDIR(catalog_details.st_mode)
        or catalog_details.st_uid!=os.geteuid() or stat.S_IMODE(catalog_details.st_mode)!=0o700):
    raise SystemExit("archive finalization catalog root is unsafe")
identity=lambda item:{"device":item.st_dev,"inode":item.st_ino,"mode":stat.S_IMODE(item.st_mode),
    "uid":item.st_uid,"gid":item.st_gid,"nlink":item.st_nlink,"size":item.st_size,
    "mtime_ns":item.st_mtime_ns,"ctime_ns":item.st_ctime_ns}
for descriptor_path in sorted(catalog_root.iterdir(),key=lambda item:item.name):
    descriptor_raw,_size,_sha=locked(descriptor_path,128*1024)
    descriptor=json.loads(descriptor_raw)
    if (descriptor_raw!=canonical(descriptor)
            or descriptor.get("schema")!="arc.recovery.shared-input-source.v1"
            or descriptor.get("archive_name")!=descriptor_path.name):
        raise SystemExit("archive finalization shared descriptor differs")
    source=pathlib.Path(descriptor["source_path"]);wanted=descriptor["source_identity"]
    fd=os.open(source,os.O_RDONLY|getattr(os,"O_NOFOLLOW",0)|getattr(os,"O_CLOEXEC",0))
    try:
        before=os.fstat(fd);visible=os.lstat(source);hasher=hashlib.sha256();total=0
        if (not stat.S_ISREG(before.st_mode) or stat.S_ISLNK(visible.st_mode)
                or identity(before)!=wanted or identity(visible)!=wanted):
            raise SystemExit(f"archive finalization shared identity differs: {descriptor_path.name}")
        while True:
            chunk=os.read(fd,1024*1024)
            if not chunk:break
            hasher.update(chunk);total+=len(chunk)
        after=os.fstat(fd);final_visible=os.lstat(source)
    finally:os.close(fd)
    observed_sha=hasher.hexdigest()
    if (identity(after)!=wanted or identity(final_visible)!=wanted
            or total!=descriptor.get("size") or observed_sha!=descriptor.get("sha256")):
        raise SystemExit(f"archive finalization shared input changed: {descriptor_path.name}")
    current_shared.append({"name":descriptor_path.name,"size":total,"sha256":observed_sha})
if value.get("shared_inputs") != current_shared:
    raise SystemExit("archive finalization intent differs from current shared inputs")
for field in ("archive_manifest", "archive_manifest_sidecar", "sha256sums"):
    item = value.get(field)
    if (not isinstance(item, dict) or set(item) != {"name", "size", "sha256"}
            or not isinstance(item["size"], int) or item["size"] <= 0
            or re.fullmatch(r"[0-9a-f]{64}", item.get("sha256", "")) is None):
        raise SystemExit(f"archive finalization intent {field} root is malformed")
print(intent_sha, value["archive_manifest"]["sha256"], value["sha256sums"]["sha256"],
      value["archive_manifest_sidecar"]["sha256"], value["prearchive_rollout_sha256"])
PY
}

gist_response_receipt() {
    local response="$1" intent="$2"
    python3 - "$response" "$intent" "$ARC_OPERATOR_GH_LOGIN" <<'PY'
import hashlib, json, os, pathlib, re, stat, sys
response_path, intent_path = map(pathlib.Path, sys.argv[1:3]); expected_login = sys.argv[3]
canonical = lambda value: (json.dumps(value, sort_keys=True, separators=(",", ":")) + "\n").encode()
def locked(path, label, maximum):
    fd = os.open(path, os.O_RDONLY | getattr(os, "O_NOFOLLOW", 0) | getattr(os, "O_CLOEXEC", 0))
    try:
        before = os.fstat(fd); visible = os.lstat(path)
        identity = lambda value: (value.st_dev, value.st_ino, value.st_mode, value.st_uid,
                                  value.st_gid, value.st_nlink, value.st_size,
                                  value.st_mtime_ns, value.st_ctime_ns)
        if (not stat.S_ISREG(before.st_mode) or stat.S_ISLNK(visible.st_mode)
                or identity(before) != identity(visible) or before.st_size <= 0
                or before.st_size > maximum):
            raise SystemExit(f"{label} is unsafe")
        chunks = []
        while True:
            chunk = os.read(fd, 1024 * 1024)
            if not chunk: break
            chunks.append(chunk)
        raw = b"".join(chunks); after = os.fstat(fd)
        if identity(before) != identity(after) or len(raw) != before.st_size:
            raise SystemExit(f"{label} changed while read")
        return raw
    finally: os.close(fd)
intent_raw = locked(intent_path, "archive finalization intent", 1024 * 1024)
try: intent = json.loads(intent_raw)
except (UnicodeDecodeError, json.JSONDecodeError): raise SystemExit("archive finalization intent is invalid")
if intent_raw != canonical(intent) or intent.get("schema") != "arc.recovery.archive-finalization-intent.v2":
    raise SystemExit("archive finalization intent is noncanonical or unsupported")
policy = intent.get("github_anchor_policy")
if (not isinstance(policy, dict) or set(policy) != {
        "provider", "owner_login", "visibility", "filename",
    } or policy.get("provider") != "github.com" or policy.get("visibility") != "secret"
        or policy.get("owner_login") != expected_login):
    raise SystemExit("archive finalization intent GitHub policy differs")
response_raw = locked(response_path, "GitHub Gist API response", 4 * 1024 * 1024)
try: response = json.loads(response_raw)
except (UnicodeDecodeError, json.JSONDecodeError): raise SystemExit("GitHub Gist API response is invalid")
gist_id = response.get("id"); owner = response.get("owner"); files = response.get("files")
history = response.get("history")
if (not isinstance(gist_id, str) or re.fullmatch(r"[0-9a-f]{20,64}", gist_id) is None
        or response.get("public") is not False or not isinstance(owner, dict)
        or owner.get("login") != expected_login or not isinstance(files, dict)
        or set(files) != {policy["filename"]} or not isinstance(history, list) or not history):
    raise SystemExit("GitHub Gist identity/owner/visibility/file set differs")
file_row = files[policy["filename"]]; first_history = history[0]
if (not isinstance(file_row, dict) or file_row.get("truncated") is not False
        or file_row.get("content") != intent_raw.decode("utf-8")
        or not isinstance(first_history, dict)
        or re.fullmatch(r"[0-9a-f]{40}", str(first_history.get("version", ""))) is None):
    raise SystemExit("GitHub Gist content/revision differs from the finalization intent")
intent_sha = hashlib.sha256(intent_raw).hexdigest()
receipt = {
    "schema": "arc.recovery.archive-finalization-gist-anchor.v1",
    "provider": "github.com",
    "owner_login": expected_login,
    "visibility": "secret",
    "gist_id": gist_id,
    "gist_revision": first_history["version"],
    "gist_filename": policy["filename"],
    "gist_file_sha256": intent_sha,
    "intent_sha256": intent_sha,
    "created_at": response.get("created_at"),
}
if re.fullmatch(r"[0-9]{4}-[0-9]{2}-[0-9]{2}T[0-9]{2}:[0-9]{2}:[0-9]{2}Z", str(receipt["created_at"])) is None:
    raise SystemExit("GitHub Gist creation timestamp is malformed")
sys.stdout.buffer.write(canonical(receipt))
PY
}

write_gist_anchor_receipt() {
    local receipt="$1" payload="$2"
    python3 - "$receipt" "$payload" <<'PY'
import json, os, pathlib, stat, sys
target = pathlib.Path(sys.argv[1]); payload = (sys.argv[2] + "\n").encode()
canonical = lambda value: (json.dumps(value, sort_keys=True, separators=(",", ":")) + "\n").encode()
try: value = json.loads(payload)
except (UnicodeDecodeError, json.JSONDecodeError): raise SystemExit("Gist anchor receipt is invalid")
if payload != canonical(value): raise SystemExit("Gist anchor receipt is noncanonical")
if (not target.is_absolute() or os.path.normpath(os.fspath(target)) != os.fspath(target)
        or os.path.realpath(target) != os.fspath(target)):
    raise SystemExit("Gist anchor receipt path/ancestry is unsafe")
parent = target.parent; details = parent.lstat()
if (parent.is_symlink() or not stat.S_ISDIR(details.st_mode)
        or details.st_uid != os.geteuid() or details.st_mode & 0o022):
    raise SystemExit("Gist anchor receipt parent is not protected")
parent_fd = os.open(parent, os.O_RDONLY | getattr(os, "O_DIRECTORY", 0) | getattr(os, "O_NOFOLLOW", 0))
partial = target.with_name(target.name + ".partial")
def read_name(name, modes):
    fd = os.open(name, os.O_RDONLY | getattr(os, "O_NOFOLLOW", 0), dir_fd=parent_fd)
    try:
        details = os.fstat(fd)
        if (not stat.S_ISREG(details.st_mode) or details.st_uid != os.geteuid()
                or details.st_nlink != 1 or stat.S_IMODE(details.st_mode) not in modes
                or details.st_size <= 0 or details.st_size > 1024 * 1024):
            raise SystemExit("Gist anchor receipt identity differs")
        chunks = []
        while True:
            chunk = os.read(fd, 1024 * 1024)
            if not chunk: break
            chunks.append(chunk)
        raw = b"".join(chunks)
        if len(raw) != details.st_size:
            raise SystemExit("Gist anchor receipt changed while read")
        return raw
    finally: os.close(fd)
try:
    opened = os.fstat(parent_fd)
    if (opened.st_dev, opened.st_ino) != (details.st_dev, details.st_ino):
        raise SystemExit("Gist anchor receipt parent changed while opened")
    if target.exists() or target.is_symlink():
        if read_name(target.name, {0o400}) != payload:
            raise SystemExit("existing Gist anchor receipt differs")
        if partial.exists() or partial.is_symlink():
            read_name(partial.name, {0o400, 0o600})
            os.unlink(partial.name, dir_fd=parent_fd); os.fsync(parent_fd)
    else:
        promote = False
        if partial.exists() or partial.is_symlink():
            if read_name(partial.name, {0o400, 0o600}) == payload:
                os.chmod(partial.name, 0o400, dir_fd=parent_fd, follow_symlinks=False)
                promote = True
            else:
                os.unlink(partial.name, dir_fd=parent_fd); os.fsync(parent_fd)
        if not promote:
            fd = os.open(
                partial.name,
                os.O_WRONLY | os.O_CREAT | os.O_EXCL | getattr(os, "O_NOFOLLOW", 0),
                0o600,
                dir_fd=parent_fd,
            )
            with os.fdopen(fd, "wb") as handle:
                handle.write(payload); handle.flush(); os.fsync(handle.fileno())
                os.fchmod(handle.fileno(), 0o400)
        os.rename(partial.name, target.name, src_dir_fd=parent_fd, dst_dir_fd=parent_fd)
        os.fsync(parent_fd)
finally: os.close(parent_fd)
PY
}

create_or_verify_gist_anchor() {
    local intent="$1" receipt="$2" temporary response receipt_payload candidate ids_file
    configure_github_anchor_transport
    temporary="$(mktemp -d)"
    response="$temporary/gist.json"
    local cached_receipt=false
    if [ -e "$receipt" ] || [ -L "$receipt" ]; then
        if python3 - "$receipt" <<'PY'
import json,os,pathlib,re,stat,sys
path=pathlib.Path(sys.argv[1]);details=path.lstat();raw=path.read_bytes()
canonical=lambda value:(json.dumps(value,sort_keys=True,separators=(",",":"))+"\n").encode()
if (path.is_symlink() or not stat.S_ISREG(details.st_mode) or details.st_uid!=os.geteuid()
        or details.st_nlink!=1 or stat.S_IMODE(details.st_mode)!=0o400
        or details.st_size<=0 or details.st_size>1024*1024):
    raise SystemExit("cached Gist receipt identity differs")
try:value=json.loads(raw)
except (UnicodeDecodeError,json.JSONDecodeError):raise SystemExit(2)
if (raw!=canonical(value) or value.get("schema")!="arc.recovery.archive-finalization-gist-anchor.v1"
        or re.fullmatch(r"[0-9a-f]{20,64}",str(value.get("gist_id",""))) is None
        or re.fullmatch(r"[0-9a-f]{40}",str(value.get("gist_revision",""))) is None):
    raise SystemExit("cached Gist receipt is a complete but unreviewed object")
PY
        then
            cached_receipt=true
        else
            local receipt_status=$?
            [ "$receipt_status" -eq 2 ] || die "cached Gist anchor receipt is unsafe or semantically different"
            python3 - "$receipt" <<'PY'
import os,pathlib,stat,sys
path=pathlib.Path(sys.argv[1]);parent=path.parent;details=path.lstat()
if (path.is_symlink() or not stat.S_ISREG(details.st_mode) or details.st_uid!=os.geteuid()
        or details.st_nlink!=1 or stat.S_IMODE(details.st_mode)!=0o400):
    raise SystemExit("cannot reconcile unsafe truncated Gist receipt")
dfd=os.open(parent,os.O_RDONLY|getattr(os,"O_DIRECTORY",0)|getattr(os,"O_NOFOLLOW",0))
try:os.unlink(path.name,dir_fd=dfd);os.fsync(dfd)
finally:os.close(dfd)
PY
        fi
    fi
    if [ "$cached_receipt" = true ]; then
        local gist_id gist_revision gist_tuple
        gist_tuple="$(python3 - "$receipt" <<'PY'
import json, pathlib, re, sys
value=json.loads(pathlib.Path(sys.argv[1]).read_text(encoding="utf-8")); gist_id=value.get("gist_id", ""); revision=value.get("gist_revision", "")
if re.fullmatch(r"[0-9a-f]{20,64}", gist_id) is None or re.fullmatch(r"[0-9a-f]{40}", revision) is None: raise SystemExit("cached Gist identity is malformed")
print(gist_id, revision)
PY
)"
        gist_id="${gist_tuple%% *}"; gist_revision="${gist_tuple#* }"
        gh_api "/gists/$gist_id/$gist_revision" > "$response" || die "cannot refetch the sealed GitHub Gist revision"
        receipt_payload="$(gist_response_receipt "$response" "$intent")" || die "GitHub Gist anchor differs from the sealed intent"
        write_gist_anchor_receipt "$receipt" "$receipt_payload" || die "cached GitHub Gist anchor receipt differs"
    else
        ids_file="$temporary/candidate-ids"
        gh_api --paginate --slurp '/gists?per_page=100' > "$temporary/gists.json" || \
            die "cannot enumerate private GitHub Gists for crash recovery"
        python3 - "$temporary/gists.json" "$intent" "$ARC_OPERATOR_GH_LOGIN" > "$ids_file" <<'PY'
import json, pathlib, sys
pages=json.loads(pathlib.Path(sys.argv[1]).read_text(encoding="utf-8"))
intent=json.loads(pathlib.Path(sys.argv[2]).read_text(encoding="utf-8")); login=sys.argv[3]
description=f"ARC recovery finalization intent {intent['capture_id']}"
seen=set()
for page in pages:
    if not isinstance(page, list): raise SystemExit("GitHub Gist pagination response is malformed")
    for row in page:
        if (isinstance(row, dict) and row.get("description") == description
                and row.get("public") is False and isinstance(row.get("owner"), dict)
                and row["owner"].get("login") == login and isinstance(row.get("id"), str)
                and row["id"] not in seen):
            seen.add(row["id"]); print(row["id"])
PY
        local match_count=0 matched_payload=""
        while IFS= read -r candidate; do
            [ -n "$candidate" ] || continue
            gh_api "/gists/$candidate" > "$response" || die "cannot inspect candidate GitHub Gist anchor"
            if receipt_payload="$(gist_response_receipt "$response" "$intent" 2>/dev/null)"; then
                match_count=$((match_count + 1)); matched_payload="$receipt_payload"
            fi
        done < "$ids_file"
        [ "$match_count" -le 1 ] || die "multiple exact GitHub Gist anchors exist; refusing ambiguous recovery"
        if [ "$match_count" -eq 1 ]; then
            receipt_payload="$matched_payload"
        else
            python3 - "$intent" > "$temporary/create.json" <<'PY'
import json, pathlib, sys
path=pathlib.Path(sys.argv[1]); raw=path.read_text(encoding="utf-8"); value=json.loads(raw)
request={"description":f"ARC recovery finalization intent {value['capture_id']}",
         "public":False,"files":{value["github_anchor_policy"]["filename"]:{"content":raw}}}
sys.stdout.write(json.dumps(request, sort_keys=True, separators=(",", ":")) + "\n")
PY
            gh_api --method POST /gists --input "$temporary/create.json" > "$temporary/created.json" || \
                die "cannot create the private GitHub Gist finalization anchor"
            local created_id created_revision created_tuple
            created_tuple="$(python3 - "$temporary/created.json" <<'PY'
import json,pathlib,re,sys
value=json.loads(pathlib.Path(sys.argv[1]).read_text(encoding="utf-8")); answer=value.get("id", ""); history=value.get("history")
revision=history[0].get("version", "") if isinstance(history,list) and history and isinstance(history[0],dict) else ""
if re.fullmatch(r"[0-9a-f]{20,64}", answer) is None or re.fullmatch(r"[0-9a-f]{40}", revision) is None: raise SystemExit("created Gist identity is malformed")
print(answer, revision)
PY
)"
            created_id="${created_tuple%% *}"; created_revision="${created_tuple#* }"
            gh_api "/gists/$created_id/$created_revision" > "$response" || die "cannot refetch the newly created GitHub Gist revision"
            receipt_payload="$(gist_response_receipt "$response" "$intent")" || \
                die "new GitHub Gist did not preserve the exact finalization intent"
        fi
        local verified_id verified_revision verified_tuple
        verified_tuple="$(python3 -c \
            'import json,sys; v=json.loads(sys.argv[1]); print(v["gist_id"],v["gist_revision"])' \
            "$receipt_payload")"
        verified_id="${verified_tuple%% *}"; verified_revision="${verified_tuple#* }"
        gh_api "/gists/$verified_id/$verified_revision" > "$response" || \
            die "cannot refetch the selected immutable GitHub Gist revision"
        receipt_payload="$(gist_response_receipt "$response" "$intent")" || \
            die "selected GitHub Gist revision differs from the sealed intent"
        write_gist_anchor_receipt "$receipt" "$receipt_payload"
    fi
    python3 - "$receipt" <<'PY'
import json,pathlib,sys
value=json.loads(pathlib.Path(sys.argv[1]).read_text(encoding="utf-8"))
print(value["intent_sha256"], value["gist_id"], value["gist_revision"], value["gist_file_sha256"])
PY
}

build_archive_complete() {
    local output="$1" intent="$2" anchor_receipt="$3"
    python3 - "$output" "$intent" "$anchor_receipt" <<'PY'
import hashlib,json,os,pathlib,re,stat,sys
output,intent_path,receipt_path=map(pathlib.Path,sys.argv[1:])
canonical=lambda value:(json.dumps(value,sort_keys=True,separators=(",",":"))+"\n").encode()
intent_raw=intent_path.read_bytes(); intent=json.loads(intent_raw)
receipt_raw=receipt_path.read_bytes(); receipt=json.loads(receipt_raw)
if intent_raw != canonical(intent) or receipt_raw != canonical(receipt): raise SystemExit("finalization inputs are noncanonical")
intent_sha=hashlib.sha256(intent_raw).hexdigest()
if (intent.get("schema") != "arc.recovery.archive-finalization-intent.v2"
        or receipt.get("schema") != "arc.recovery.archive-finalization-gist-anchor.v1"
        or receipt.get("intent_sha256") != intent_sha or receipt.get("gist_file_sha256") != intent_sha
        or receipt.get("owner_login") != intent["github_anchor_policy"]["owner_login"]
        or receipt.get("gist_filename") != intent["github_anchor_policy"]["filename"]):
    raise SystemExit("GitHub anchor receipt differs from finalization intent")
anchor={key:receipt[key] for key in ("intent_sha256","gist_id","gist_revision","gist_file_sha256")}
if (re.fullmatch(r"[0-9a-f]{20,64}", anchor["gist_id"]) is None
        or re.fullmatch(r"[0-9a-f]{40}", anchor["gist_revision"]) is None
        or any(re.fullmatch(r"[0-9a-f]{64}", anchor[key]) is None for key in ("intent_sha256","gist_file_sha256"))):
    raise SystemExit("GitHub anchor identity is malformed")
complete={
    "schema":"arc.recovery.archive-complete.v2",
    "freeze_plan_sha256":intent["freeze_plan_sha256"],
    "capture_id":intent["capture_id"],
    "rollout_manifest_sha256":intent["prearchive_rollout_sha256"],
    "source_commit":intent["source_commit"],
    "archive_manifest_sha256":intent["archive_manifest"]["sha256"],
    "object_count_before_complete":len(intent["shared_inputs"])+24+3,
    "validator_bundle_count":6,
    "finalization_anchor":anchor,
}
payload=canonical(complete)
output.parent.mkdir(parents=True,exist_ok=True)
fd=os.open(output,os.O_WRONLY|os.O_CREAT|os.O_EXCL|getattr(os,"O_NOFOLLOW",0),0o444)
with os.fdopen(fd,"wb") as handle:
    handle.write(payload);handle.flush();os.fsync(handle.fileno());os.fchmod(handle.fileno(),0o444)
directory=os.open(output.parent,os.O_RDONLY|getattr(os,"O_DIRECTORY",0))
try:os.fsync(directory)
finally:os.close(directory)
PY
}

fetch_verify_or_recover_complete_gist_anchor() {
    local complete="$1" recovered_intent="${2:-}" recovered_receipt="${3:-}"
    configure_github_anchor_transport
    local temporary gist_id gist_revision response
    temporary="$(mktemp -d)"; response="$temporary/gist.json"
    local gist_tuple
    gist_tuple="$(python3 - "$complete" <<'PY'
import json,pathlib,re,sys
value=json.loads(pathlib.Path(sys.argv[1]).read_text(encoding="utf-8")); anchor=value.get("finalization_anchor")
if (value.get("schema") != "arc.recovery.archive-complete.v2" or not isinstance(anchor,dict)
        or set(anchor) != {"intent_sha256","gist_id","gist_revision","gist_file_sha256"}
        or re.fullmatch(r"[0-9a-f]{20,64}",str(anchor.get("gist_id",""))) is None
        or re.fullmatch(r"[0-9a-f]{40}",str(anchor.get("gist_revision",""))) is None):
    raise SystemExit("COMPLETE GitHub anchor is malformed")
print(anchor["gist_id"], anchor["gist_revision"])
PY
)"
    gist_id="${gist_tuple%% *}"; gist_revision="${gist_tuple#* }"
    gh_api "/gists/$gist_id/$gist_revision" > "$response" || \
        die "cannot refetch COMPLETE's immutable GitHub Gist revision"
    python3 - "$complete" "$response" "$ARC_OPERATOR_GH_LOGIN" "$recovered_intent" "$recovered_receipt" <<'PY'
import hashlib,json,os,pathlib,re,stat,sys
complete_path,response_path=map(pathlib.Path,sys.argv[1:3]);login=sys.argv[3]
intent_target=pathlib.Path(sys.argv[4]) if sys.argv[4] else None
receipt_target=pathlib.Path(sys.argv[5]) if sys.argv[5] else None
canonical=lambda value:(json.dumps(value,sort_keys=True,separators=(",",":"))+"\n").encode()
complete=json.loads(complete_path.read_text(encoding="utf-8"));response=json.loads(response_path.read_text(encoding="utf-8"))
anchor=complete["finalization_anchor"];filename=f"arc-recovery-{complete['capture_id']}.finalization-intent.json"
if (response.get("id") != anchor["gist_id"] or response.get("public") is not False
        or not isinstance(response.get("owner"),dict) or response["owner"].get("login") != login
        or not isinstance(response.get("files"),dict) or set(response["files"]) != {filename}
        or not isinstance(response.get("history"),list) or not response["history"]
        or response["history"][0].get("version") != anchor["gist_revision"]):
    raise SystemExit("COMPLETE GitHub Gist identity/owner/revision differs")
row=response["files"][filename];content=row.get("content") if isinstance(row,dict) else None
if row.get("truncated") is not False or not isinstance(content,str): raise SystemExit("COMPLETE GitHub Gist content is unavailable")
raw=content.encode();intent=json.loads(raw);intent_sha=hashlib.sha256(raw).hexdigest()
if (raw != canonical(intent) or intent_sha != anchor["intent_sha256"]
        or intent_sha != anchor["gist_file_sha256"]
        or intent.get("schema") != "arc.recovery.archive-finalization-intent.v2"
        or intent.get("freeze_plan_sha256") != complete["freeze_plan_sha256"]
        or intent.get("capture_id") != complete["capture_id"]
        or intent.get("prearchive_rollout_sha256") != complete["rollout_manifest_sha256"]
        or intent.get("source_commit") != complete["source_commit"]
        or intent.get("archive_manifest",{}).get("sha256") != complete["archive_manifest_sha256"]
        or intent.get("github_anchor_policy") != {"provider":"github.com","owner_login":login,
            "visibility":"secret","filename":filename}):
    raise SystemExit("COMPLETE GitHub Gist content differs from archive roots")
receipt={"schema":"arc.recovery.archive-finalization-gist-anchor.v1","provider":"github.com",
         "owner_login":login,"visibility":"secret","gist_id":anchor["gist_id"],
         "gist_revision":anchor["gist_revision"],"gist_filename":filename,
         "gist_file_sha256":intent_sha,"intent_sha256":intent_sha,"created_at":response.get("created_at")}
def protected_write(target,payload,mode):
    if (not target.is_absolute() or os.path.normpath(os.fspath(target)) != os.fspath(target)
            or os.path.realpath(target) != os.fspath(target)):
        raise SystemExit("recovered finalization path/ancestry is unsafe")
    parent=target.parent;details=parent.lstat()
    if parent.is_symlink() or not stat.S_ISDIR(details.st_mode) or details.st_uid != os.geteuid() or details.st_mode & 0o022:
        raise SystemExit("recovered finalization parent is not protected")
    dfd=os.open(parent,os.O_RDONLY|getattr(os,"O_DIRECTORY",0)|getattr(os,"O_NOFOLLOW",0))
    try:
        try:fd=os.open(target.name,os.O_WRONLY|os.O_CREAT|os.O_EXCL|getattr(os,"O_NOFOLLOW",0),mode,dir_fd=dfd)
        except FileExistsError:
            fd=os.open(target.name,os.O_RDONLY|getattr(os,"O_NOFOLLOW",0),dir_fd=dfd)
            try:
                current=os.read(fd,len(payload)+1);st=os.fstat(fd)
                if current != payload or stat.S_IMODE(st.st_mode) != mode or st.st_uid != os.geteuid() or st.st_nlink != 1:
                    raise SystemExit("existing recovered finalization file differs or is unsafe")
            finally:os.close(fd)
        else:
            with os.fdopen(fd,"wb") as handle:handle.write(payload);handle.flush();os.fsync(handle.fileno());os.fchmod(handle.fileno(),mode)
        os.fsync(dfd)
    finally:os.close(dfd)
if intent_target:
    protected_write(intent_target,raw,0o400)
    protected_write(intent_target.with_name(intent_target.name+".sha256"),f"{intent_sha}  {intent_target.name}\n".encode(),0o400)
if receipt_target:protected_write(receipt_target,canonical(receipt),0o400)
PY
}

verify_remote_complete() (
    local destination="$1" expected_complete="${2:-}" expected_manifest="${3:-}" expected_sidecar="${4:-}"
    local expected_complete_sha="${5:-}" expected_manifest_sha="${6:-}" expected_sums_sha="${7:-}"
    local expected_prearchive_sha="${8:-}"
    local expected_sidecar_sha="${9:-}"
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
        "$expected_prearchive_sha" "$expected_sidecar_sha" <<'PY'
import hashlib
import json
import pathlib
import re
import sys

complete_path, manifest_path, sidecar_path, sums_path, objects_path, names_path, manifest_sha_path = map(pathlib.Path, sys.argv[1:8])
expected_complete_sha, expected_manifest_sha, expected_sums_sha, expected_prearchive_sha, expected_sidecar_sha = sys.argv[8:13]
complete = json.loads(complete_path.read_text(encoding="utf-8"))
manifest = json.loads(manifest_path.read_text(encoding="utf-8"))
canonical = lambda value: (json.dumps(value, sort_keys=True, separators=(",", ":")) + "\n").encode()
if complete_path.read_bytes() != canonical(complete) or manifest_path.read_bytes() != canonical(manifest):
    raise SystemExit("archive completion evidence is not canonical JSON")
complete_keys = {
    "schema", "freeze_plan_sha256", "capture_id", "rollout_manifest_sha256",
    "source_commit", "archive_manifest_sha256", "object_count_before_complete",
    "validator_bundle_count", "finalization_anchor",
}
if set(complete) != complete_keys or complete["schema"] != "arc.recovery.archive-complete.v2":
    raise SystemExit("COMPLETE.json has missing, unknown, or unsupported fields")
anchor = complete["finalization_anchor"]
if (not isinstance(anchor, dict) or set(anchor) != {
        "intent_sha256", "gist_id", "gist_revision", "gist_file_sha256",
    } or re.fullmatch(r"[0-9a-f]{20,64}", str(anchor.get("gist_id", ""))) is None
        or re.fullmatch(r"[0-9a-f]{40}", str(anchor.get("gist_revision", ""))) is None
        or any(re.fullmatch(r"[0-9a-f]{64}", str(anchor.get(key, ""))) is None
               for key in ("intent_sha256", "gist_file_sha256"))
        or anchor["intent_sha256"] != anchor["gist_file_sha256"]):
    raise SystemExit("COMPLETE.json finalization anchor is malformed")
manifest_sha = hashlib.sha256(manifest_path.read_bytes()).hexdigest()
complete_sha = hashlib.sha256(complete_path.read_bytes()).hexdigest()
sums_sha = hashlib.sha256(sums_path.read_bytes()).hexdigest()
sidecar_sha = hashlib.sha256(sidecar_path.read_bytes()).hexdigest()
for label, expected, actual in (
    ("COMPLETE", expected_complete_sha, complete_sha),
    ("archive manifest", expected_manifest_sha, manifest_sha),
    ("SHA256SUMS", expected_sums_sha, sums_sha),
    ("archive manifest sidecar", expected_sidecar_sha, sidecar_sha),
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
    fetch_verify_or_recover_complete_gist_anchor "$temporary/COMPLETE.json"
    local name expected_sha expected_size actual
    while IFS=$'\t' read -r name expected_sha expected_size; do
        actual="$(rclone cat "$destination/$name" | python3 -c 'import hashlib,sys; data=sys.stdin.buffer; digest=hashlib.sha256(); size=0
for chunk in iter(lambda: data.read(1024*1024), b""):
 digest.update(chunk); size += len(chunk)
print(digest.hexdigest(), size)')" || die "cannot hash remote archive object: $name"
        [ "$actual" = "$expected_sha $expected_size" ] || \
            die "remote archive object differs from SHA256SUMS/manifest: $name"
    done < "$temporary/objects.tsv"
    rclone cat "$destination/legacy-live-observations.json" \
        > "$temporary/legacy-live-observations.json" || \
        die "cannot read remote fleet live-observation binding"
    rclone cat "$destination/offline-stop-evidence.json" \
        > "$temporary/offline-stop-evidence.json" || \
        die "cannot read remote offline-stop observation provenance"
    python3 - "$temporary/legacy-live-observations.json" \
        "$temporary/ARCHIVE-MANIFEST.json" "$temporary/live-observation-bindings.tsv" \
        "$temporary/offline-stop-evidence.json" <<'PY'
import hashlib, json, pathlib, re, sys
path, manifest_path, output, offline_path = map(pathlib.Path, sys.argv[1:])
value = json.loads(path.read_text(encoding="utf-8"))
manifest = json.loads(manifest_path.read_text(encoding="utf-8"))
canonical = (json.dumps(value, sort_keys=True, separators=(",", ":")) + "\n").encode()
if path.read_bytes() != canonical:
    raise SystemExit("remote fleet live-observation binding is not canonical JSON")
if (not isinstance(value, dict) or set(value) != {
        "schema", "capture_id", "freeze_plan_sha256", "observation_generation",
        "observation_generation_receipt_sha256", "drive_prefreeze_receipt_sha256",
        "live_observation_selection_sha256", "receipt_schema", "labels", "nodes",
    } or value.get("schema") != "arc.recovery.legacy-live-observations-fleet.v1"
        or value.get("capture_id") != manifest.get("capture_id")
        or value.get("freeze_plan_sha256") != manifest.get("freeze_plan_sha256")
        or value.get("receipt_schema") != "arc.recovery.legacy-live-observations.v1"
        or value.get("labels") != ["diagnostic", "noncanonical", "nonreward"]):
    raise SystemExit("remote fleet live-observation binding identity/labels differ")
offline=json.loads(offline_path.read_text(encoding="utf-8"))
offline_canonical=(json.dumps(offline,sort_keys=True,separators=(",",":"))+"\n").encode()
if (offline_path.read_bytes()!=offline_canonical
        or (value.get("live_observation_selection_sha256"),
            value.get("observation_generation"),
            value.get("observation_generation_receipt_sha256"),
            value.get("drive_prefreeze_receipt_sha256"))
           !=(offline.get("legacy_live_observation_selection_sha256"),
              offline.get("legacy_live_observation_generation"),
              offline.get("observation_generation_receipt_sha256"),
              offline.get("drive_prefreeze_receipt_sha256"))):
    raise SystemExit("remote archive observation provenance differs from offline-stop evidence")
nodes = ("nyc", "lax", "ams", "lhr", "nrt", "sgp")
rows = value.get("nodes")
if not isinstance(rows, list) or [row.get("node") for row in rows] != list(nodes):
    raise SystemExit("remote fleet live-observation binding omits/reorders validators")
hash_re = re.compile(r"[0-9a-f]{64}")
for node, row in zip(nodes, rows):
    if (not isinstance(row, dict) or set(row) != {"node", "root_sha256", "receipt_sha256"}
            or row.get("node") != node
            or not hash_re.fullmatch(row.get("root_sha256", ""))
            or not hash_re.fullmatch(row.get("receipt_sha256", ""))):
        raise SystemExit(f"remote fleet live-observation node binding is malformed: {node}")
shared = {item.get("name"): item for item in manifest.get("shared_inputs", []) if isinstance(item, dict)}
expected = {"name": path.name, "size": len(canonical), "sha256": hashlib.sha256(canonical).hexdigest()}
if shared.get(path.name) != expected:
    raise SystemExit("archive manifest does not bind the remote fleet live-observation object")
output.write_text("".join(
    f"{row['node']}\t{row['root_sha256']}\t{row['receipt_sha256']}\n" for row in rows
), encoding="ascii")
PY
    rclone lsf --files-only -R "$destination" | LC_ALL=C sort > "$temporary/actual-names"
    LC_ALL=C sort "$temporary/expected-names" -o "$temporary/expected-names"
    cmp --silent "$temporary/expected-names" "$temporary/actual-names" || \
        die "remote destination contains missing, duplicate, or unexpected objects"
    cat "$temporary/manifest-sha"
)

verify_complete_phase() {
    # ``verify-complete`` is also the production rollout's pre-GO archive
    # preflight.  Keep every pinned transport/config copy and metadata scratch
    # inside this function's subprocess, and remove it on both success and any
    # fail-closed exit.
    begin_temporary_scope
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
    configure_operator_transport true
    configure_github_anchor_transport
    require_commands python3 rclone mktemp cmp
    local archive_manifest_sha
    archive_manifest_sha="$(verify_remote_complete "$destination" "" "" "" \
        "$expected_complete_sha" "$expected_manifest_sha" "$expected_sums_sha" \
        "$expected_prearchive_sha")"
    require_hash "$archive_manifest_sha" "verified archive manifest hash"
    if [ "${#new_node_paths[@]}" -gt 0 ] || [ "$verify_live_captures" = true ]; then
        local temporary freeze_plan freeze_sha capture_id
        temporary="$(mktemp -d)"
        ARCHIVE_FLEET_TEMP_ROOT="$temporary"
        freeze_plan="$temporary/freeze-plan.json"
        rclone cat "$destination/freeze-plan.json" > "$freeze_plan"
        rclone cat "$destination/freeze-plan.json.sha256" > "${freeze_plan}.sha256"
        chmod 444 "$freeze_plan" "${freeze_plan}.sha256"
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

verify_archive_work_root_capacity() {
    local root="$1" manifest="$2"
    python3 - "$root" "$manifest" "$ORCHESTRATOR" "$REMOTE_HELPER" \
        "$ROLLOUT_TOOL" "$ROLLOUT_SCHEMA" <<'PY'
import json, os, pathlib, stat, sys
root, manifest_path, *tools = map(pathlib.Path, sys.argv[1:])
if (not root.is_absolute() or os.path.normpath(os.fspath(root)) != os.fspath(root)):
    raise SystemExit("archive work root must be normalized and absolute")
details = root.lstat()
if (root.is_symlink() or not root.is_dir() or details.st_uid != os.getuid()
        or stat.S_IMODE(details.st_mode) != 0o700):
    raise SystemExit("archive work root must be a real operator-owned mode-0700 directory")
manifest = json.loads(manifest_path.read_bytes())
paths = {pathlib.Path(row["path"]) for row in manifest["artifacts"].values()}
paths.update(tools)
paths.update((manifest_path, manifest_path.with_name(manifest_path.name + ".sha256")))
identity=lambda value:(value.st_dev,value.st_ino,value.st_mode,value.st_uid,value.st_gid,
                       value.st_nlink,value.st_size,value.st_mtime_ns,value.st_ctime_ns)
for path in paths:
    descriptor=os.open(path,os.O_RDONLY|getattr(os,"O_NOFOLLOW",0)|getattr(os,"O_CLOEXEC",0))
    try:
        before=os.fstat(descriptor);visible=os.lstat(path);after=os.fstat(descriptor)
    finally:os.close(descriptor)
    if (not stat.S_ISREG(before.st_mode) or stat.S_ISLNK(visible.st_mode)
            or before.st_size<=0 or identity(before)!=identity(visible)
            or identity(before)!=identity(after)):
        raise SystemExit(f"archive work reservation input is unsafe: {path}")
# Shared inputs are streamed from their sealed source descriptors. Work-root
# demand is therefore bounded metadata, inventories, receipts, and pipeline
# buffers rather than the sum of snapshot/WAL/release artifact sizes.
required = 1024 * 1024**2 + len(paths) * 1024**2
filesystem = os.statvfs(root)
available = filesystem.f_bavail * filesystem.f_frsize
available_inodes = filesystem.f_favail
required_inodes = 1024 + len(paths) * 4
if available < required:
    raise SystemExit(
        f"archive work root has {available} bytes available; {required} are required"
    )
if available_inodes and available_inodes < required_inodes:
    raise SystemExit("archive work root has insufficient free inodes")
print(required)
PY
}

seal_phase() {
    # Seal plans authenticate SSH and Drive before any GO check.  Install the
    # invocation cleanup boundary first so all private transport copies are
    # removed on plan, success, and every ordinary error path.
    begin_temporary_scope
    local freeze_plan="" manifest="" validators="" finalization_intent="" work_root=""
    local validator_install_receipt="" vault_restore_receipt=""
    local execute=false allow_unbound=false
    while [ "$#" -gt 0 ]; do
        case "$1" in
            --freeze-plan) [ "$#" -ge 2 ] || die "--freeze-plan needs a value"; freeze_plan="$2"; shift 2 ;;
            --manifest) [ "$#" -ge 2 ] || die "--manifest needs a value"; manifest="$2"; shift 2 ;;
            --validator-public-keys) [ "$#" -ge 2 ] || die "--validator-public-keys needs a value"; validators="$2"; shift 2 ;;
            --validator-install-receipt) [ "$#" -ge 2 ] || die "--validator-install-receipt needs a value"; validator_install_receipt="$2"; shift 2 ;;
            --vault-restore-receipt) [ "$#" -ge 2 ] || die "--vault-restore-receipt needs a value"; vault_restore_receipt="$2"; shift 2 ;;
            --finalization-intent) [ "$#" -ge 2 ] || die "--finalization-intent needs a value"; finalization_intent="$2"; shift 2 ;;
            --work-root) [ "$#" -ge 2 ] || die "--work-root needs a value"; work_root="$2"; shift 2 ;;
            --allow-unbound-legacy-wal) allow_unbound=true; shift ;;
            --execute) execute=true; shift ;;
            --plan) execute=false; shift ;;
            -h|--help) usage; return 0 ;;
            *) die "unknown seal option: $1" ;;
        esac
    done
    configure_operator_transport true
    require_commands python3 ssh scp rclone grep mktemp cp find git
    require_absolute_file "$manifest" "rollout manifest"
    require_absolute_file "$validators" "validator public-key file"
    require_absolute_file "$validator_install_receipt" "validator install receipt"
    require_absolute_file "$vault_restore_receipt" "validator vault restore receipt"
    case "$finalization_intent" in /*.json) ;; *) die "--finalization-intent must be one normalized absolute .json path" ;; esac
    [ "$(python3 -c 'import os,sys; print(os.path.normpath(sys.argv[1]))' "$finalization_intent")" = "$finalization_intent" ] || \
        die "--finalization-intent must be lexically normalized"
    case "$work_root" in /*) ;; *) die "--work-root must be an absolute protected directory" ;; esac
    [ "$(python3 -c 'import os,sys; print(os.path.normpath(sys.argv[1]))' "$work_root")" = "$work_root" ] || \
        die "--work-root must be lexically normalized"
    [ -x "$REMOTE_HELPER" ] || die "remote helper is missing or not executable"
    [ -f "$ROLLOUT_TOOL" ] || die "recovery rollout verifier is missing"
    OPERATOR_FREEZE_PLAN="$freeze_plan"
    ARCHIVE_FLEET_PINNED_ROOT="$(mktemp -d "${TMPDIR:-/tmp}/arc-seal-freeze-plan.XXXXXX")"
    [ -d "$ARCHIVE_FLEET_PINNED_ROOT" ] && [ ! -L "$ARCHIVE_FLEET_PINNED_ROOT" ] || \
        die "cannot allocate the private seal freeze-plan snapshot"
    freeze_plan="$(pin_freeze_plan "$freeze_plan" "$ARCHIVE_FLEET_PINNED_ROOT")"
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

    local binary cli build_metadata genesis validators_manifest height_receipt offline_stop_evidence ssh_known_hosts reward_probe
    local maintenance_evidence_bundle maintenance_evidence_bundle_sidecar
    local maintenance_boundary maintenance_boundary_sidecar offline_stop_evidence_sidecar
    local late_fork_source_set late_fork_source_set_sidecar late_fork_interlock_tool
    local checkpoint legacy_validator_set source_snapshot source_wal caddy
    local pretag_input_set pretag_initial_set production_stage_manifest
    local manifest_restore_receipt manifest_install_receipt
    local binary_sha cli_sha build_metadata_sha genesis_sha validators_manifest_sha
    local height_receipt_sha offline_stop_evidence_sha ssh_known_hosts_sha reward_probe_sha checkpoint_sha legacy_validator_set_sha
    local maintenance_evidence_bundle_sha maintenance_evidence_bundle_sidecar_sha
    local maintenance_boundary_sha maintenance_boundary_sidecar_sha offline_stop_evidence_sidecar_sha
    local late_fork_source_set_sha late_fork_source_set_sidecar_sha late_fork_interlock_tool_sha
    local source_snapshot_sha source_wal_sha caddy_sha
    local pretag_input_set_sha pretag_initial_set_sha production_stage_manifest_sha
    local manifest_restore_receipt_sha manifest_install_receipt_sha
    local pretag_paths=() pretag_hashes=() pretag_index pretag_key
    local source_height source_hash source_state_root transition_state_root checkpoint_manifest
    local source_round created_at_unix_ms recovery_epoch validator_set_id
    binary="$(manifest_field "$manifest" artifacts.binary.path)"
    binary_sha="$(manifest_field "$manifest" artifacts.binary.sha256)"
    cli="$(manifest_field "$manifest" artifacts.cli.path)"
    cli_sha="$(manifest_field "$manifest" artifacts.cli.sha256)"
    build_metadata="$(manifest_field "$manifest" artifacts.build_metadata.path)"
    build_metadata_sha="$(manifest_field "$manifest" artifacts.build_metadata.sha256)"
    pretag_input_set="$(manifest_field "$manifest" artifacts.pretag_artifact_input_set.path)"
    pretag_input_set_sha="$(manifest_field "$manifest" artifacts.pretag_artifact_input_set.sha256)"
    pretag_initial_set="$(manifest_field "$manifest" artifacts.pretag_initial_live_provenance_set.path)"
    pretag_initial_set_sha="$(manifest_field "$manifest" artifacts.pretag_initial_live_provenance_set.sha256)"
    production_stage_manifest="$(manifest_field "$manifest" artifacts.production_input_stage_manifest.path)"
    production_stage_manifest_sha="$(manifest_field "$manifest" artifacts.production_input_stage_manifest.sha256)"
    manifest_restore_receipt="$(manifest_field "$manifest" artifacts.validator_vault_restore_receipt.path)"
    manifest_restore_receipt_sha="$(manifest_field "$manifest" artifacts.validator_vault_restore_receipt.sha256)"
    manifest_install_receipt="$(manifest_field "$manifest" artifacts.validator_key_install_receipt.path)"
    manifest_install_receipt_sha="$(manifest_field "$manifest" artifacts.validator_key_install_receipt.sha256)"
    for pretag_key in "${PRETAG_ARTIFACT_KEYS[@]}"; do
        pretag_paths+=("$(manifest_field "$manifest" "artifacts.$pretag_key.path")")
        pretag_hashes+=("$(manifest_field "$manifest" "artifacts.$pretag_key.sha256")")
    done
    genesis="$(manifest_field "$manifest" artifacts.genesis.path)"
    genesis_sha="$(manifest_field "$manifest" artifacts.genesis.sha256)"
    validators_manifest="$(manifest_field "$manifest" artifacts.validator_public_keys.path)"
    validators_manifest_sha="$(manifest_field "$manifest" artifacts.validator_public_keys.sha256)"
    height_receipt="$(manifest_field "$manifest" artifacts.legacy_public_height_receipt.path)"
    height_receipt_sha="$(manifest_field "$manifest" artifacts.legacy_public_height_receipt.sha256)"
    offline_stop_evidence="$(manifest_field "$manifest" artifacts.offline_stop_evidence.path)"
    offline_stop_evidence_sha="$(manifest_field "$manifest" artifacts.offline_stop_evidence.sha256)"
    offline_stop_evidence_sidecar="$(manifest_field "$manifest" artifacts.offline_stop_evidence_sidecar.path)"
    offline_stop_evidence_sidecar_sha="$(manifest_field "$manifest" artifacts.offline_stop_evidence_sidecar.sha256)"
    maintenance_evidence_bundle="$(manifest_field "$manifest" artifacts.legacy_maintenance_evidence_bundle.path)"
    maintenance_evidence_bundle_sha="$(manifest_field "$manifest" artifacts.legacy_maintenance_evidence_bundle.sha256)"
    maintenance_evidence_bundle_sidecar="$(manifest_field "$manifest" artifacts.legacy_maintenance_evidence_bundle_sidecar.path)"
    maintenance_evidence_bundle_sidecar_sha="$(manifest_field "$manifest" artifacts.legacy_maintenance_evidence_bundle_sidecar.sha256)"
    maintenance_boundary="$(manifest_field "$manifest" artifacts.legacy_maintenance_boundary.path)"
    maintenance_boundary_sha="$(manifest_field "$manifest" artifacts.legacy_maintenance_boundary.sha256)"
    maintenance_boundary_sidecar="$(manifest_field "$manifest" artifacts.legacy_maintenance_boundary_sidecar.path)"
    maintenance_boundary_sidecar_sha="$(manifest_field "$manifest" artifacts.legacy_maintenance_boundary_sidecar.sha256)"
    late_fork_source_set="$(manifest_field "$manifest" artifacts.legacy_late_fork_source_set.path)"
    late_fork_source_set_sha="$(manifest_field "$manifest" artifacts.legacy_late_fork_source_set.sha256)"
    late_fork_source_set_sidecar="$(manifest_field "$manifest" artifacts.legacy_late_fork_source_set_sidecar.path)"
    late_fork_source_set_sidecar_sha="$(manifest_field "$manifest" artifacts.legacy_late_fork_source_set_sidecar.sha256)"
    late_fork_interlock_tool="$(manifest_field "$manifest" artifacts.legacy_late_fork_interlock_tool.path)"
    late_fork_interlock_tool_sha="$(manifest_field "$manifest" artifacts.legacy_late_fork_interlock_tool.sha256)"
    ssh_known_hosts="$(manifest_field "$manifest" artifacts.ssh_known_hosts.path)"
    ssh_known_hosts_sha="$(manifest_field "$manifest" artifacts.ssh_known_hosts.sha256)"
    [ "$ssh_known_hosts_sha" = "$ARC_OPERATOR_SSH_KNOWN_HOSTS_SHA256" ] || \
        die "operator SSH trust anchor differs from the sealed rollout artifact"
    reward_probe="$(manifest_field "$manifest" artifacts.reward_probe.path)"
    reward_probe_sha="$(manifest_field "$manifest" artifacts.reward_probe.sha256)"
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
    local source_commit sealed_ssh_sha validator_receipt_rows
    source_commit="$(manifest_field "$manifest" provenance.source_main_commit)"
    sealed_ssh_sha="$(manifest_field "$manifest" provenance.offline_stop_verification.ssh_sha256)"
    [ "$sealed_ssh_sha" = "$ARC_OPERATOR_SSH_SHA256" ] || \
        die "operator SSH executable differs from the sealed remote-stop transport"
    verify_operator_transport_matches_stage "$manifest"
    source_height="$(manifest_field "$manifest" chain.source_height)"
    source_hash="$(manifest_field "$manifest" chain.source_block_hash)"
    source_state_root="$(manifest_field "$manifest" chain.source_state_root)"
    transition_state_root="$(manifest_field "$manifest" chain.full_state_root)"
    checkpoint_manifest="$(manifest_field "$manifest" chain.approved_checkpoint_manifest_hash)"
    source_round="$(manifest_field "$manifest" chain.source_consensus_round)"
    created_at_unix_ms="$(manifest_field "$manifest" chain.created_at_unix_ms)"
    recovery_epoch="$(manifest_field "$manifest" chain.recovery_epoch)"
    validator_set_id="$(manifest_field "$manifest" chain.validator_set_id)"
    local observation_generation observation_generation_receipt_sha
    local observation_drive_receipt_sha observation_selection_sha
    read -r observation_generation observation_generation_receipt_sha \
        observation_drive_receipt_sha observation_selection_sha < <(
        python3 - "$offline_stop_evidence" <<'PY'
import json,pathlib,re,sys
value=json.loads(pathlib.Path(sys.argv[1]).read_text(encoding="utf-8"))
fields=("legacy_live_observation_generation","observation_generation_receipt_sha256",
        "drive_prefreeze_receipt_sha256","legacy_live_observation_selection_sha256")
items=[value.get(field) for field in fields]
if any(not isinstance(item,str) or re.fullmatch(r"[0-9a-f]{64}",item) is None for item in items):
    raise SystemExit("offline-stop live-observation archive provenance is malformed")
print(*items)
PY
    )

    [ "$validators" = "$validators_manifest" ] || \
        die "--validator-public-keys path differs from the sealed rollout artifact"
    [ "$validator_sha" = "$validators_manifest_sha" ] || \
        die "validator public-key bytes differ from the sealed rollout artifact"
    [ "$validator_install_receipt" = "$manifest_install_receipt" ] || \
        die "--validator-install-receipt must be the exact manifest-staged receipt path"
    [ "$vault_restore_receipt" = "$manifest_restore_receipt" ] || \
        die "--vault-restore-receipt must be the exact manifest-staged receipt path"
    [ "$(hash_file "$validator_install_receipt")" = "$manifest_install_receipt_sha" ] || \
        die "validator install receipt differs from its manifest artifact hash"
    [ "$(hash_file "$vault_restore_receipt")" = "$manifest_restore_receipt_sha" ] || \
        die "validator vault restore receipt differs from its manifest artifact hash"
    [ "$(current_source_commit)" = "$source_commit" ] || \
        die "archive worktree commit differs from the protected-main manifest provenance"
    validator_receipt_rows="$ARCHIVE_FLEET_PINNED_ROOT/validator-key-rows.tsv"
    verify_validator_receipt_chain "$validator_install_receipt" "$vault_restore_receipt" \
        "$manifest" "$source_commit" "$cli_sha" "$genesis_sha" \
        "$ssh_known_hosts_sha" "$sealed_ssh_sha" "$ARC_OPERATOR_SCP_SHA256" \
        "$freeze_sha" "$offline_stop_evidence_sha" > "$validator_receipt_rows"
    chmod 400 "$validator_receipt_rows"

    local archive_work_required
    archive_work_required="$(verify_archive_work_root_capacity "$work_root" "$manifest")"
    require_uint "$archive_work_required" "archive work-root reservation"
    printf 'archive fleet: PASS protected work root reserves %s bytes plus reviewed inode headroom\n' \
        "$archive_work_required"

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
    verify_offline_stop_evidence_remote "$freeze_plan" "$freeze_sha" "$capture_id" \
        "$offline_stop_evidence" "$offline_stop_evidence_sha" "$maintenance_evidence_bundle"
    for node in nyc lax ams lhr nrt sgp; do
        host="$(host_for "$node")"
        run_remote "$node" status "$capture_id" "$node" >/dev/null
        printf '  capture present/stopped: %s\n' "$node"
    done
    rclone lsd "$DRIVE_REMOTE" >/dev/null
    if [ "$execute" != true ]; then
        printf 'archive fleet: PLAN ONLY; no persistent remote, Drive, or source credential/config file was changed\n'
        return 0
    fi
    local expected_go="GO $manifest_sha FREEZE $freeze_sha CAPTURE $capture_id DEST $destination_sha LEGACY_WAL $policy"
    [ "${ARC_RECOVERY_GO:-}" = "$expected_go" ] || \
        die "execution requires ARC_RECOVERY_GO='$expected_go'"
    # Prove the independently authenticated GitHub anchor channel before the
    # first execute-mode remote mutation, so a missing token/scope/account
    # cannot strand an otherwise completed Drive upload.
    configure_github_anchor_transport

    [ "$(freeze_plan_hash "$freeze_plan")" = "$freeze_sha" ] || \
        die "freeze plan or source bindings changed before execution"
    local log_root github_gist_canary_receipt
    log_root="$(mktemp -d)"
    ARCHIVE_FLEET_TEMP_ROOT="$log_root"
    github_gist_canary_receipt="$log_root/github-gist-write-canary.json"
    run_github_gist_anchor_canary "$freeze_sha" "$capture_id" \
        "$github_gist_canary_receipt" >/dev/null
    printf 'archive fleet: PASS private GitHub Gist create/read-by-revision/delete canary before archive mutation\n'
    install_helpers "$(manifest_field "$freeze_plan" remote_helper_sha256)"
    local pids=() names=()
    for node in nyc lax ams lhr nrt sgp; do
        (
            stage_file "$node" "$manifest_sha" binary "$binary" "$binary_sha"
            stage_file "$node" "$manifest_sha" cli "$cli" "$cli_sha"
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

    local receipt_node receipt_address receipt_key_sha receipt_count=0
    while IFS=' ' read -r receipt_node receipt_address receipt_key_sha; do
        [ "$receipt_node" = "${NODES[$receipt_count]%%=*}" ] || \
            die "validator receipt rows differ from the fixed fleet order"
        verify_remote_validator_key_identity "$receipt_node" "$manifest_sha" \
            "$cli_sha" "$receipt_key_sha" "$receipt_address"
        receipt_count=$((receipt_count + 1))
    done < "$validator_receipt_rows"
    [ "$receipt_count" -eq 6 ] || die "validator receipt chain did not prove exactly six identities"
    printf 'archive fleet: PASS exact staged CLI derived all six installed validator addresses\n'

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

    local shared_root="$log_root/shared-input-catalog"
    local shared_generated="$log_root/shared-input-generated"
    local metadata_root="$log_root/archive-metadata"
    local complete_root="$log_root/archive-complete"
    mkdir -m 700 -- "$shared_root" "$shared_generated"
    local orchestrator_sha helper_sha rollout_tool_sha schema_sha
    [ "$(current_source_commit)" = "$source_commit" ] || \
        die "archive source commit changed during the seal transaction"
    orchestrator_sha="$(hash_file "$ORCHESTRATOR")"
    helper_sha="$(hash_file "$REMOTE_HELPER")"
    rollout_tool_sha="$(hash_file "$ROLLOUT_TOOL")"
    schema_sha="$(hash_file "$SCRIPT_DIR/recovery-manifest.schema.json")"
    register_shared_input "$freeze_plan" "$freeze_sha" "$shared_root" freeze-plan.json
    register_shared_input "${freeze_plan}.sha256" "$(hash_file "${freeze_plan}.sha256")" \
        "$shared_root" freeze-plan.json.sha256
    register_shared_input "$ORCHESTRATOR" "$orchestrator_sha" "$shared_root" archive-fleet-to-drive.sh
    register_shared_input "$REMOTE_HELPER" "$helper_sha" "$shared_root" archive-node.sh
    register_shared_input "$ROLLOUT_TOOL" "$rollout_tool_sha" "$shared_root" recovery_rollout.py
    register_shared_input "$SCRIPT_DIR/recovery-manifest.schema.json" "$schema_sha" \
        "$shared_root" recovery-manifest.schema.json
    register_shared_input "$github_gist_canary_receipt" \
        "$(hash_file "$github_gist_canary_receipt")" \
        "$shared_root" github-gist-write-canary.json
    register_shared_input "$binary" "$binary_sha" "$shared_root" arc-node
    register_shared_input "$cli" "$cli_sha" "$shared_root" arc-cli
    register_shared_input "$build_metadata" "$build_metadata_sha" \
        "$shared_root" build-metadata.json
    register_shared_input "$pretag_input_set" "$pretag_input_set_sha" \
        "$shared_root" PRETAG-ARTIFACT-INPUT-SET.json
    register_shared_input "$pretag_initial_set" "$pretag_initial_set_sha" \
        "$shared_root" PRETAG-INITIAL-LIVE-PROVENANCE-SET.json
    register_shared_input "$production_stage_manifest" "$production_stage_manifest_sha" \
        "$shared_root" PRODUCTION-INPUT-STAGE-MANIFEST.json
    for pretag_index in "${!PRETAG_ARTIFACT_KEYS[@]}"; do
        register_shared_input "${pretag_paths[$pretag_index]}" "${pretag_hashes[$pretag_index]}" \
            "$shared_root" "${PRETAG_ARCHIVE_NAMES[$pretag_index]}"
    done
    register_shared_input "$genesis" "$genesis_sha" "$shared_root" genesis.toml
    register_shared_input "$validators" "$validator_sha" "$shared_root" validator-public-keys.json
    register_shared_input "$vault_restore_receipt" "$manifest_restore_receipt_sha" \
        "$shared_root" VALIDATOR-VAULT-RESTORE-RECEIPT.json
    register_shared_input "$validator_install_receipt" "$manifest_install_receipt_sha" \
        "$shared_root" VALIDATOR-KEY-INSTALL-RECEIPT.json
    python3 - "$shared_generated/VALIDATOR-KEY-RECEIPT-CHAIN.json" "$manifest" <<'PY'
import json, os, pathlib, sys
path, manifest_path = map(pathlib.Path, sys.argv[1:])
with manifest_path.open("rb") as handle:
    manifest = json.load(handle)
value = manifest["provenance"]["validator_key_receipt_chain"]
payload = (json.dumps(value, sort_keys=True, separators=(",", ":")) + "\n").encode()
fd = os.open(path, os.O_WRONLY | os.O_CREAT | os.O_EXCL | getattr(os, "O_NOFOLLOW", 0), 0o400)
with os.fdopen(fd, "wb") as handle:
    handle.write(payload); handle.flush(); os.fsync(handle.fileno()); os.fchmod(handle.fileno(), 0o400)
PY
    register_shared_input "$shared_generated/VALIDATOR-KEY-RECEIPT-CHAIN.json" \
        "$(hash_file "$shared_generated/VALIDATOR-KEY-RECEIPT-CHAIN.json")" \
        "$shared_root" VALIDATOR-KEY-RECEIPT-CHAIN.json
    register_shared_input "$height_receipt" "$height_receipt_sha" \
        "$shared_root" legacy-public-height.json
    register_shared_input "$offline_stop_evidence" "$offline_stop_evidence_sha" \
        "$shared_root" offline-stop-evidence.json
    register_shared_input "$offline_stop_evidence_sidecar" "$offline_stop_evidence_sidecar_sha" \
        "$shared_root" offline-stop-evidence.json.sha256
    register_shared_input "$maintenance_evidence_bundle" "$maintenance_evidence_bundle_sha" \
        "$shared_root" legacy-maintenance-evidence-bundle.json
    register_shared_input "$maintenance_evidence_bundle_sidecar" \
        "$maintenance_evidence_bundle_sidecar_sha" \
        "$shared_root" legacy-maintenance-evidence-bundle.json.sha256
    register_shared_input "$maintenance_boundary" "$maintenance_boundary_sha" \
        "$shared_root" legacy-maintenance-boundary.json
    register_shared_input "$maintenance_boundary_sidecar" "$maintenance_boundary_sidecar_sha" \
        "$shared_root" legacy-maintenance-boundary.json.sha256
    register_shared_input "$late_fork_source_set" "$late_fork_source_set_sha" \
        "$shared_root" legacy-late-fork-source-set.json
    register_shared_input "$late_fork_source_set_sidecar" "$late_fork_source_set_sidecar_sha" \
        "$shared_root" legacy-late-fork-source-set.json.sha256
    register_shared_input "$late_fork_interlock_tool" "$late_fork_interlock_tool_sha" \
        "$shared_root" legacy-late-fork-interlock.py
    register_shared_input "$ssh_known_hosts" "$ssh_known_hosts_sha" \
        "$shared_root" ssh-known-hosts
    register_shared_input "$reward_probe" "$reward_probe_sha" \
        "$shared_root" community-reward-probe.py
    register_shared_input "$legacy_validator_set" "$legacy_validator_set_sha" \
        "$shared_root" legacy-validator-set-40m.json
    register_shared_input "$source_snapshot" "$source_snapshot_sha" "$shared_root" source.snapshot.lz4
    register_shared_input "$source_wal" "$source_wal_sha" "$shared_root" source.state.wal
    register_shared_input "$checkpoint" "$checkpoint_sha" "$shared_root" recovery.arcchkpt
    register_shared_input "$caddy" "$caddy_sha" "$shared_root" caddy
    register_shared_input "$manifest" "$manifest_sha" "$shared_root" rollout-manifest.json
    register_shared_input "${manifest}.sha256" "$(hash_file "${manifest}.sha256")" \
        "$shared_root" rollout-manifest.json.sha256
    printf '%s\n' "$source_commit" > "$shared_generated/source-commit.txt"
    printf '%s\n' "$capture_id" > "$shared_generated/capture-id.txt"
    python3 - "$shared_generated/archive-seal-options.json" "$allow_unbound" <<'PY'
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
    chmod 400 "$shared_generated/source-commit.txt" "$shared_generated/capture-id.txt"
    register_shared_input "$shared_generated/source-commit.txt" \
        "$(hash_file "$shared_generated/source-commit.txt")" "$shared_root" source-commit.txt
    register_shared_input "$shared_generated/capture-id.txt" \
        "$(hash_file "$shared_generated/capture-id.txt")" "$shared_root" capture-id.txt
    register_shared_input "$shared_generated/archive-seal-options.json" \
        "$(hash_file "$shared_generated/archive-seal-options.json")" \
        "$shared_root" archive-seal-options.json
    create_canonical_reference \
        "$shared_generated/canonical-reference.json" "$shared_root" "$allow_unbound" \
        "$source_height" "$source_hash" "$source_state_root" "$transition_state_root" \
        "$checkpoint_manifest" "$source_round" "$created_at_unix_ms" \
        "$recovery_epoch" "$validator_set_id" "$binary_sha" "$genesis_sha" \
        "$validator_sha" "$legacy_validator_set_sha" "$source_snapshot_sha" \
        "$source_wal_sha" "$checkpoint_sha"
    register_shared_input "$shared_generated/canonical-reference.json" \
        "$(hash_file "$shared_generated/canonical-reference.json")" \
        "$shared_root" canonical-reference.json
    create_live_observation_fleet_binding \
        "$shared_generated/legacy-live-observations.json" "$capture_id" "$freeze_sha" \
        "$observation_generation" "$observation_generation_receipt_sha" \
        "$observation_drive_receipt_sha" "$observation_selection_sha" \
        "$log_root/live-observation-statuses.jsonl"
    register_shared_input "$shared_generated/legacy-live-observations.json" \
        "$(hash_file "$shared_generated/legacy-live-observations.json")" \
        "$shared_root" legacy-live-observations.json

    local drive_seal_log="$log_root/drive-seal-preflight.log"
    # Re-prove write/read/hash/delete at the final upload boundary.  A stale
    # read-only account/capacity check cannot prove that the pinned OAuth
    # principal still has the mutation permissions required to finish seal.
    run_drive_prefreeze_gate execute "$freeze_plan" "$freeze_sha" "$capture_id" \
        archive-seal > "$drive_seal_log"
    local drive_seal_receipt drive_seal_attempt_receipt
    drive_seal_receipt="$(tail -n 1 "$drive_seal_log")"
    require_absolute_file "$drive_seal_receipt" "archive-seal Drive execute receipt"
    drive_seal_attempt_receipt="${drive_seal_receipt}.attempt.json"
    require_absolute_file "$drive_seal_attempt_receipt" "archive-seal Drive attempt receipt"
    register_shared_input "$drive_seal_receipt" "$(hash_file "$drive_seal_receipt")" \
        "$shared_root" drive-archive-seal-prefreeze.json
    register_shared_input "$drive_seal_attempt_receipt" "$(hash_file "$drive_seal_attempt_receipt")" \
        "$shared_root" drive-archive-seal-attempt.json
    printf 'archive fleet: PASS fresh pinned Drive client/account/capacity and write/read/delete canary immediately before upload\n'

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
        local gist_anchor_receipt="${finalization_intent}.gist-anchor.json"
        # The independent provider is the recovery source when the operator
        # disk copy was lost.  It is re-fetched and exact-matched even when a
        # local cache still exists.
        fetch_verify_or_recover_complete_gist_anchor \
            "$log_root/existing-COMPLETE.json" "$finalization_intent" "$gist_anchor_receipt"
        local expected_complete_sha expected_intent_sha expected_manifest_sha expected_sums_sha
        local expected_sidecar_sha expected_prearchive_sha
        read -r expected_intent_sha expected_manifest_sha expected_sums_sha \
            expected_sidecar_sha expected_prearchive_sha < <(
                archive_finalization_intent_roots "$finalization_intent" "$shared_root" \
                    "$freeze_sha" "$capture_id" "$manifest_sha" "$source_commit" "$destination"
            )
        [ "$expected_prearchive_sha" = "$manifest_sha" ] || \
            die "archive finalization intent prearchive root differs"
        mkdir -p -- "$complete_root"
        build_archive_complete "$complete_root/COMPLETE.json" \
            "$finalization_intent" "$gist_anchor_receipt"
        expected_complete_sha="$(hash_file "$complete_root/COMPLETE.json")"
        [ "$expected_intent_sha" = "$(hash_file "$finalization_intent")" ] || \
            die "recovered finalization intent root differs"
        archive_manifest_sha="$(verify_remote_complete "$destination" "" "" "" \
            "$expected_complete_sha" "$expected_manifest_sha" "$expected_sums_sha" \
            "$expected_prearchive_sha" "$expected_sidecar_sha")"
        require_hash "$archive_manifest_sha" "existing archive manifest hash"
        [ "$archive_manifest_sha" = "$expected_manifest_sha" ] || \
            die "existing archive manifest differs from the local finalization intent"
        printf 'archive fleet: existing COMPLETE.json fully verified; verification-only resume\n'
        printf 'archive fleet: FINAL-ROLLOUT-ROOTS destination=%s complete_sha256=%s archive_manifest_sha256=%s sha256sums_sha256=%s prearchive_rollout_sha256=%s\n' \
            "$destination" "$expected_complete_sha" "$expected_manifest_sha" \
            "$expected_sums_sha" "$expected_prearchive_sha"
        return 0
    fi
    rclone mkdir "$destination"
    local shared_descriptor
    for shared_descriptor in "$shared_root"/*; do
        stream_shared_input_to_drive "$shared_descriptor" "$destination" "$log_root"
    done

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
        "$shared_root" "$log_root/bundle-statuses.jsonl" "$metadata_root" \
        "$freeze_sha" "$capture_id" "$manifest_sha" "$source_commit" \
        "$orchestrator_sha" "$helper_sha" "$rollout_tool_sha" "$schema_sha" \
        "$canonical_count" "$fork_count" "$unclassified_count")"
    require_hash "$archive_manifest_sha" "archive manifest hash"
    local finalization_intent_sha intent_root_sha intent_manifest_sha
    local intent_sums_sha intent_sidecar_sha intent_prearchive_sha
    finalization_intent_sha="$(seal_archive_finalization_intent \
        "$finalization_intent" "$shared_root" "$log_root/bundle-statuses.jsonl" \
        "$metadata_root/SHA256SUMS" "$metadata_root/ARCHIVE-MANIFEST.json" \
        "$metadata_root/ARCHIVE-MANIFEST.json.sha256" "$freeze_sha" "$capture_id" \
        "$manifest_sha" "$source_commit" "$destination" "$ARC_OPERATOR_GH_LOGIN")"
    require_hash "$finalization_intent_sha" "archive finalization intent hash"
    read -r intent_root_sha intent_manifest_sha intent_sums_sha \
        intent_sidecar_sha intent_prearchive_sha < <(
            archive_finalization_intent_roots "$finalization_intent" "$shared_root" \
                "$freeze_sha" "$capture_id" "$manifest_sha" "$source_commit" "$destination"
        )
    [ "$intent_root_sha" = "$finalization_intent_sha" ] || \
        die "archive finalization intent root changed before independent anchoring"
    [ "$intent_manifest_sha" = "$archive_manifest_sha" ] || \
        die "archive finalization intent manifest root differs before publication"
    [ "$intent_sums_sha" = "$(hash_file "$metadata_root/SHA256SUMS")" ] || \
        die "archive finalization intent checksum root differs before publication"
    [ "$intent_sidecar_sha" = "$(hash_file "$metadata_root/ARCHIVE-MANIFEST.json.sha256")" ] || \
        die "archive finalization intent sidecar root differs before publication"
    [ "$intent_prearchive_sha" = "$manifest_sha" ] || \
        die "archive finalization intent prearchive root differs before publication"
    local gist_anchor_receipt="${finalization_intent}.gist-anchor.json"
    local anchored_intent_sha anchored_gist_id anchored_gist_revision anchored_gist_file_sha
    read -r anchored_intent_sha anchored_gist_id anchored_gist_revision anchored_gist_file_sha < <(
        create_or_verify_gist_anchor "$finalization_intent" "$gist_anchor_receipt"
    )
    [ "$anchored_intent_sha" = "$finalization_intent_sha" ] || \
        die "GitHub Gist anchor intent root differs"
    [ "$anchored_gist_file_sha" = "$finalization_intent_sha" ] || \
        die "GitHub Gist file bytes differ from the sealed intent"
    mkdir -p -- "$complete_root"
    build_archive_complete "$complete_root/COMPLETE.json" \
        "$finalization_intent" "$gist_anchor_receipt"
    fetch_verify_or_recover_complete_gist_anchor "$complete_root/COMPLETE.json"
    printf 'archive fleet: PASS independent GitHub Gist anchor id=%s revision=%s intent_sha256=%s\n' \
        "$anchored_gist_id" "$anchored_gist_revision" "$anchored_intent_sha"
    verify_remote_shared_inputs "$shared_root" "$destination" "$log_root"
    rclone copy "$metadata_root" "$destination" --immutable --checksum --metadata \
        --drive-stop-on-upload-limit \
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

archive_write_current_process_id() {
    # Bash 3.2 keeps $$ fixed in subshells.  A short-lived /bin/sh reports the
    # actual PID of the Bash process that spawned it through its PPID. Write it
    # directly: command substitution would insert another Bash process.
    /bin/sh -c 'printf "%s\n" "$PPID"' > "$1"
}

archive_process_field() {
    local field="$1" pid="$2" value
    case "$field" in ppid|pgid) ;; *) return 2 ;; esac
    if value="$(/bin/ps -o "$field=" -p "$pid" 2>/dev/null)"; then
        value="${value//[[:space:]]/}"
    else
        return 1
    fi
    case "$value" in ''|*[!0-9]*) return 1 ;; esac
    printf '%s\n' "$value"
}

archive_process_exists() {
    local state
    builtin kill -0 -- "$1" 2>/dev/null || return 1
    # A transient ps failure is not proof that a killable process exited.
    state="$(/bin/ps -o stat= -p "$1" 2>/dev/null)" || return 0
    state="${state//[[:space:]]/}"
    case "$state" in Z*) return 1 ;; *) return 0 ;; esac
}

archive_process_in_group() {
    local process_pid="$1" wanted_pgid="$2" observed_pgid
    archive_process_exists "$process_pid" || return 1
    observed_pgid="$(archive_process_field pgid "$process_pid")" || return 1
    [ "$observed_pgid" = "$wanted_pgid" ]
}

archive_process_group_has_members_except() {
    local wanted="$1"
    shift
    local process_pid process_pgid process_state excluded skip snapshot
    snapshot="$(LC_ALL=C /bin/ps -ax -o pid= -o pgid= -o stat= 2>/dev/null)" || return 0
    while read -r process_pid process_pgid process_state; do
        [ "$process_pgid" = "$wanted" ] || continue
        case "$process_state" in ''|Z*) continue ;; esac
        skip=false
        for excluded in "$@"; do
            if [ -n "$excluded" ] && [ "$process_pid" = "$excluded" ]; then
                skip=true
                break
            fi
        done
        [ "$skip" = true ] || return 0
    done <<< "$snapshot"
    return 1
}

archive_stop_and_kill_group_members_except() {
    local wanted="$1"
    shift
    local process_pid process_pgid process_state excluded skip snapshot attempt
    local all_stopped=false
    local -a targets=()
    # SIGSTOP is uncatchable. While the sentinel anchors the PGID, freezing the
    # complete group prevents a target from exiting/reusing its PID between the
    # identity snapshot and exact KILL. Always CONT the excluded owners before
    # returning so a pending phase trap can run.
    builtin kill -s STOP -- "-$wanted" 2>/dev/null || return 1
    for ((attempt = 0; attempt < 250; attempt += 1)); do
        all_stopped=true
        snapshot="$(LC_ALL=C /bin/ps -ax -o pid= -o pgid= -o stat= 2>/dev/null)" || {
            all_stopped=false
            break
        }
        while read -r process_pid process_pgid process_state; do
            [ "$process_pgid" = "$wanted" ] || continue
            case "$process_state" in ''|Z*|T*|t*) ;; *) all_stopped=false; break ;; esac
        done <<< "$snapshot"
        [ "$all_stopped" = true ] && break
        /bin/sleep 0.02
    done
    if [ "$all_stopped" != true ]; then
        builtin kill -s CONT -- "-$wanted" 2>/dev/null || true
        return 1
    fi
    # Re-snapshot only while every member is stopped. No member can spawn or
    # voluntarily exit during target selection.
    snapshot="$(LC_ALL=C /bin/ps -ax -o pid= -o pgid= -o stat= 2>/dev/null)" || {
        builtin kill -s CONT -- "-$wanted" 2>/dev/null || true
        return 1
    }
    while read -r process_pid process_pgid process_state; do
        [ "$process_pgid" = "$wanted" ] || continue
        case "$process_state" in ''|Z*) continue ;; T*|t*) ;; *)
            builtin kill -s CONT -- "-$wanted" 2>/dev/null || true
            return 1
            ;;
        esac
        skip=false
        for excluded in "$@"; do
            if [ -n "$excluded" ] && [ "$process_pid" = "$excluded" ]; then
                skip=true
                break
            fi
        done
        [ "$skip" = true ] || targets+=("$process_pid")
    done <<< "$snapshot"
    # Bash 3.2 treats "${targets[@]}" on an empty array as an unbound-variable
    # fatal under set -u, which no caller-side "|| true" can suppress. Dying
    # here leaves the entire group SIGSTOPped by the kill above with the CONT
    # below never reached. The ${x[@]+...} guard keeps the expansion legal.
    for process_pid in ${targets[@]+"${targets[@]}"}; do
        builtin kill -s KILL -- "$process_pid" 2>/dev/null || true
    done
    builtin kill -s CONT -- "-$wanted" 2>/dev/null || true
    return 0
}

archive_send_sentinel_command() {
    local gate="$1" stop_token="$2" verb="$3" argument="${4:-}"
    local fifo="$gate/sentinel.fifo"
    case "$verb" in ARM|FINALIZE) [ -z "$argument" ] || return 2 ;; SIGNAL)
        case "$argument" in HUP|INT|TERM) ;; *) return 2 ;; esac
        ;; *) return 2 ;;
    esac
    # Never let a missing FIFO turn the redirection into a regular file.  The
    # gate is private, but its disappearance is also the completion signal.
    [ -p "$fifo" ] && [ ! -L "$fifo" ] || return 1
    if exec 8<> "$fifo"; then
        printf '%s\t%s\t%s\n' "$stop_token" "$verb" "$argument" >&8 || {
            exec 8>&-
            exec 8<&-
            return 1
        }
        exec 8>&-
        exec 8<&-
        return 0
    fi
    return 1
}

archive_guardian_publish_heartbeat() {
    local gate="$1" watchdog_pid="$2" watchdog_pgid="$3" stop_token="$4" sequence="$5"
    (umask 077; printf '%s\t%s\t%s\t%s\n' \
        "$watchdog_pid" "$watchdog_pgid" "$stop_token" "$sequence" \
        > "$gate/guardian.heartbeat.partial") && \
        /bin/mv -f "$gate/guardian.heartbeat.partial" "$gate/guardian.heartbeat"
}

archive_supervisor_publish_heartbeat() {
    local gate="$1" supervisor_pid="$2" stop_token="$3" sequence="$4"
    (umask 077; printf '%s\t%s\t%s\n' "$supervisor_pid" "$stop_token" "$sequence" \
        > "$gate/supervisor.heartbeat.partial") && \
        /bin/mv -f "$gate/supervisor.heartbeat.partial" "$gate/supervisor.heartbeat"
}

archive_guardian_anchor_valid() {
    local watchdog_pid="$1" sentinel_pid="$2" phase_pgid="$3"
    local observed_parent observed_sentinel_pgid
    observed_parent="$(archive_process_field ppid "$watchdog_pid")" || return 1
    [ "$observed_parent" = "$sentinel_pid" ] || return 1
    observed_sentinel_pgid="$(archive_process_field pgid "$sentinel_pid")" || return 1
    [ "$observed_sentinel_pgid" = "$phase_pgid" ]
}

archive_guardian_job_is_active() {
    local expected_pid="$1" job_pid=""
    job_pid="$(jobs -p '%+' 2>/dev/null)" || job_pid=""
    [ "$job_pid" = "$expected_pid" ]
}

archive_terminate_guardian_job() {
    local watchdog_pid="$1" watchdog_pgid="$2" sentinel_pid="$3"
    local observed_parent observed_pgid process_pid process_pgid process_state snapshot attempt
    local all_stopped=false
    archive_guardian_job_is_active "$watchdog_pid" || {
        wait "$watchdog_pid" 2>/dev/null || true
        return 0
    }
    observed_parent="$(archive_process_field ppid "$watchdog_pid")" || return 1
    observed_pgid="$(archive_process_field pgid "$watchdog_pid")" || return 1
    [ "$observed_parent" = "$sentinel_pid" ] || return 1
    [ "$observed_pgid" = "$watchdog_pgid" ] || return 1
    [ "$watchdog_pid" = "$watchdog_pgid" ] || return 1
    # The guardian is this sentinel's sole job. A job-spec resolves through
    # Bash's unreaped child table and therefore cannot target a recycled PID.
    builtin kill -s STOP -- '%+' 2>/dev/null || return 1
    for ((attempt = 0; attempt < 250; attempt += 1)); do
        all_stopped=true
        snapshot="$(LC_ALL=C /bin/ps -ax -o pid= -o pgid= -o stat= 2>/dev/null)" || {
            all_stopped=false
            break
        }
        while read -r process_pid process_pgid process_state; do
            [ "$process_pgid" = "$watchdog_pgid" ] || continue
            case "$process_state" in ''|Z*|T*|t*) ;; *) all_stopped=false; break ;; esac
        done <<< "$snapshot"
        [ "$all_stopped" = true ] && break
        /bin/sleep 0.02
    done
    if [ "$all_stopped" != true ]; then
        builtin kill -s CONT -- '%+' 2>/dev/null || true
        return 1
    fi
    builtin kill -s KILL -- '%+' 2>/dev/null || true
    wait "$watchdog_pid" 2>/dev/null || true
    return 0
}

archive_dispatch_sentinel() {
    local supervisor_pid="$1" phase_pid="$2" gate="$3" phase_pgid="$4" stop_token="$5"
    local fifo="$gate/sentinel.fifo" request_token request_verb request_argument
    local sentinel_pid sentinel_pgid observed_parent attempt
    local watchdog_pid="" watchdog_pgid="" last_watchdog_sequence=""
    local supervisor_sequence="" last_supervisor_sequence=""
    local watchdog_started_at=0 watchdog_last_seen_at=0 supervisor_last_seen_at=0
    local guardian_requested=false guardian_finalizing=false guardian_finalized=false armed=false
    local guardian_failed=false completion_ack_pid="" completion_ack_token=""
    local guardian_initial_mode=monitor ready_pid ready_pgid ready_token ready_sequence
    trap '' HUP INT TERM
    archive_write_current_process_id "$gate/sentinel.pid.partial" || exit 125
    IFS= read -r sentinel_pid < "$gate/sentinel.pid.partial" || exit 125
    sentinel_pgid="$(archive_process_field pgid "$sentinel_pid")" || exit 125
    [ "$sentinel_pgid" = "$phase_pgid" ] || exit 125
    # Open the endpoint before publishing readiness. Otherwise a writer could
    # successfully open its own RDWR descriptor, lose a command when it closes,
    # and mistake that unacknowledged write for delivery.
    exec 9<> "$fifo"
    (umask 077; printf '%s\t%s\t%s\n' "$sentinel_pid" "$sentinel_pgid" "$stop_token" \
        > "$gate/sentinel.ready.partial")
    /bin/mv -f "$gate/sentinel.ready.partial" "$gate/sentinel.ready"
    supervisor_last_seen_at="$SECONDS"
    if [ -n "${ARC_ARCHIVE_DISPATCH_TEST_OVERRIDE_NAMES:-}" ] && \
        [ "${ARC_ARCHIVE_DISPATCH_TEST_STOP_PHASE_AFTER_SENTINEL_READY:-false}" = true ] && \
        mkdir "$gate/test-phase-stopped-once" 2>/dev/null; then
        builtin kill -s STOP -- "$phase_pid"
    fi
    # Read/write keeps bootstrap nonblocking. Only this sentinel interprets
    # authenticated control messages and it is itself a member of the phase
    # group, so it can never signal a recycled numeric PGID.
    while :; do
        request_token=""; request_verb=""; request_argument=""
        # Bash 3.2 accepts only integral read timeouts. A control write wakes
        # this immediately; one second also gives portable lease pacing.
        IFS=$'\t' read -r -t 1 -u 9 request_token request_verb request_argument || true
        if [ "$request_token" = "$stop_token" ]; then
            case "$request_verb:$request_argument" in
                SIGNAL:HUP|SIGNAL:INT|SIGNAL:TERM)
                    if builtin kill -s "$request_argument" -- "-$sentinel_pgid" 2>/dev/null; then
                        (umask 077; printf '%s\n' "$stop_token" \
                            > "$gate/signal.$request_argument.ack.partial") && \
                            /bin/mv -f "$gate/signal.$request_argument.ack.partial" \
                                "$gate/signal.$request_argument.ack"
                    fi
                    ;;
                ARM:)
                    if [ -n "$watchdog_pid" ] && archive_process_exists "$watchdog_pid" && \
                        [ -f "$gate/watchdog.ready" ]; then
                        armed=true
                        (umask 077; printf '%s\t%s\n' "$sentinel_pid" "$stop_token" \
                            > "$gate/sentinel.armed.partial") && \
                            /bin/mv -f "$gate/sentinel.armed.partial" "$gate/sentinel.armed"
                    fi
                    ;;
                FINALIZE:)
                    guardian_requested=true
                    guardian_finalizing=true
                    ;;
            esac
            if [ "$request_verb" = FINALIZE ]; then
                (umask 077; printf '%s\n' "$stop_token" \
                    > "$gate/sentinel.finalize.ack.partial") && \
                    /bin/mv -f "$gate/sentinel.finalize.ack.partial" \
                        "$gate/sentinel.finalize.ack"
            fi
        fi

        if [ "$guardian_requested" = false ] && [ -f "$gate/guardian.start" ] && \
            [ ! -L "$gate/guardian.start" ]; then
            ready_token=""
            IFS= read -r ready_token < "$gate/guardian.start" || ready_token=""
            [ "$ready_token" = "$stop_token" ] && guardian_requested=true
        fi

        if [ "$guardian_requested" = true ] && [ -z "$watchdog_pid" ]; then
            rm -f -- "$gate/watchdog.ready" "$gate/guardian.heartbeat" \
                "$gate/guardian.finalized"
            if [ "$guardian_finalizing" = true ]; then
                guardian_initial_mode=finalize
            else
                guardian_initial_mode=monitor
            fi
            set -m
            archive_dispatch_parent_watchdog "$sentinel_pid" "$phase_pid" "$phase_pgid" \
                "$sentinel_pid" "$stop_token" "$gate" "$guardian_initial_mode" &
            watchdog_pid="$!"
            set +m
            watchdog_pgid="$watchdog_pid"
            last_watchdog_sequence=""
            watchdog_started_at="$SECONDS"; watchdog_last_seen_at="$SECONDS"
        fi

        if [ -n "$watchdog_pid" ]; then
            if ! archive_guardian_job_is_active "$watchdog_pid"; then
                wait "$watchdog_pid" 2>/dev/null || true
                guardian_finalized=false
                if [ -f "$gate/guardian.finalized" ] && [ ! -L "$gate/guardian.finalized" ]; then
                    ready_pid=""; ready_token=""
                    IFS=$'\t' read -r ready_pid ready_token < "$gate/guardian.finalized" || true
                    [ "$ready_pid" = "$watchdog_pid" ] && [ "$ready_token" = "$stop_token" ] && \
                        guardian_finalized=true
                fi
                watchdog_pid=""; watchdog_pgid=""
                # Gate the sweep on the guardian receipt ALONE. The sentinel
                # runs inside phase_pgid (set +m before the fork; asserted at
                # sentinel start), so its own /bin/ps command-substitution
                # children are members of the group being counted and are not
                # in the exclusion list -- the predicate is structurally always
                # true here, which made this sweep unreachable and stranded the
                # 0700 gate (the phase TMPDIR holding id_ed25519, known_hosts
                # and rclone.conf) on every guardian-kill path.
                # It is also redundant: the guardian leads its own PGID
                # (watchdog_pid == watchdog_pgid, :13329), so its identical
                # calls are sound, and it writes guardian.finalized only after
                # its anchor-validated drain loop has emptied the group.
                if [ "$guardian_finalized" = true ]; then
                    (umask 077; printf '%s\t%s\t%s\n' "$sentinel_pid" "$stop_token" \
                        "$guardian_failed" > "$gate/sentinel.complete.partial") && \
                        /bin/mv -f "$gate/sentinel.complete.partial" "$gate/sentinel.complete"
                    # Preserve the terminal receipt until the live supervisor
                    # acknowledges it. If that lease is gone, sweep anyway;
                    # a resumed supervisor treats missing receipt as failure.
                    for ((attempt = 0; attempt < 250; attempt += 1)); do
                        if [ -f "$gate/supervisor.complete.ack" ] && \
                            [ ! -L "$gate/supervisor.complete.ack" ]; then
                            completion_ack_pid=""; completion_ack_token=""
                            IFS=$'\t' read -r completion_ack_pid completion_ack_token \
                                < "$gate/supervisor.complete.ack" || true
                            if [ "$completion_ack_pid" = "$supervisor_pid" ] && \
                                [ "$completion_ack_token" = "$stop_token" ]; then
                                break
                            fi
                        fi
                        /bin/sleep 0.02
                    done
                    archive_remove_dispatch_gate_until_absent "$gate"
                    return 0
                fi
                # Any unplanned guardian exit closes the mutation boundary.
                guardian_failed=true
                (umask 077; printf '%s\n' "$stop_token" \
                    > "$gate/guardian.failed.partial") && \
                    /bin/mv -f "$gate/guardian.failed.partial" "$gate/guardian.failed"
                guardian_finalizing=true
                guardian_requested=true
                continue
            fi
            if [ -f "$gate/guardian.heartbeat" ] && [ ! -L "$gate/guardian.heartbeat" ]; then
                ready_pid=""; ready_pgid=""; ready_token=""; ready_sequence=""
                IFS=$'\t' read -r ready_pid ready_pgid ready_token ready_sequence \
                    < "$gate/guardian.heartbeat" || true
                if [ "$ready_pid" = "$watchdog_pid" ] && [ "$ready_pgid" = "$watchdog_pgid" ] && \
                    [ "$ready_token" = "$stop_token" ] && [ -n "$ready_sequence" ] && \
                    [ "$ready_sequence" != "$last_watchdog_sequence" ]; then
                    last_watchdog_sequence="$ready_sequence"
                    watchdog_last_seen_at="$SECONDS"
                fi
            fi
            if [ $((SECONDS - watchdog_last_seen_at)) -ge 3 ] || \
                { [ ! -f "$gate/watchdog.ready" ] && \
                    [ $((SECONDS - watchdog_started_at)) -ge 3 ]; }; then
                # The guardian is an unreaped direct child and its separate
                # process group is the sentinel's sole job. Stop/kill/reap that
                # exact job before any replacement; if it cannot be stopped,
                # retain the sentinel and gate indefinitely.
                if archive_terminate_guardian_job "$watchdog_pid" "$watchdog_pgid" "$sentinel_pid"; then
                    guardian_failed=true
                    (umask 077; printf '%s\n' "$stop_token" \
                        > "$gate/guardian.failed.partial") && \
                        /bin/mv -f "$gate/guardian.failed.partial" "$gate/guardian.failed"
                    watchdog_pid=""; watchdog_pgid=""
                    guardian_finalizing=true
                    guardian_requested=true
                    continue
                fi
            fi
        fi

        if [ "$guardian_requested" = true ] && [ "$guardian_finalizing" = false ]; then
            if [ -f "$gate/supervisor.heartbeat" ] && [ ! -L "$gate/supervisor.heartbeat" ]; then
                ready_pid=""; ready_token=""; supervisor_sequence=""
                IFS=$'\t' read -r ready_pid ready_token supervisor_sequence \
                    < "$gate/supervisor.heartbeat" || true
                if [ "$ready_pid" = "$supervisor_pid" ] && [ "$ready_token" = "$stop_token" ] && \
                    [ -n "$supervisor_sequence" ] && \
                    [ "$supervisor_sequence" != "$last_supervisor_sequence" ]; then
                    last_supervisor_sequence="$supervisor_sequence"
                    supervisor_last_seen_at="$SECONDS"
                fi
            fi
            # A stopped, killed, or wedged supervisor loses its mutation lease.
            # It has no numeric PGID capability, so a later resume cannot hit a
            # released/reused group after this fail-closed finalization.
            [ $((SECONDS - supervisor_last_seen_at)) -lt 3 ] || guardian_finalizing=true
        elif [ "$armed" = false ]; then
            observed_parent="$(archive_process_field ppid "$phase_pid")" || observed_parent=""
            if [ "$observed_parent" != "$supervisor_pid" ]; then
                guardian_requested=true
                guardian_finalizing=true
            fi
        fi

        if [ "$guardian_finalizing" = true ] && [ -n "$watchdog_pid" ] && \
            [ -f "$gate/watchdog.ready" ] && [ ! -L "$gate/watchdog.ready" ]; then
            (umask 077; printf '%s\t%s\n' "$watchdog_pid" "$stop_token" \
                > "$gate/guardian.finalize.partial") && \
                /bin/mv -f "$gate/guardian.finalize.partial" "$gate/guardian.finalize"
        fi
    done
}

archive_stop_dispatch_sentinel() {
    local sentinel_pid="$1" phase_pgid="$2" fifo="$3" stop_token="$4"
    local observed_pgid
    while archive_process_exists "$sentinel_pid"; do
        observed_pgid="$(archive_process_field pgid "$sentinel_pid")" || {
            /bin/sleep 0.02
            continue
        }
        # A different group proves that the anchored process exited and its PID
        # was reused. Never signal that unrelated replacement.
        [ "$observed_pgid" = "$phase_pgid" ] || return 0
        # Opening the private FIFO read/write never blocks even if the sentinel
        # exits between identity validation and this cooperative exact-token
        # stop. Do not numeric-kill across that PID-reuse window.
        if [ -p "$fifo" ] && [ ! -L "$fifo" ] && exec 8<> "$fifo"; then
            printf '%s\tFINALIZE\t\n' "$stop_token" >&8 || true
            exec 8>&-
            exec 8<&-
        fi
        /bin/sleep 0.02
    done
    return 0
}

archive_remove_dispatch_gate() {
    local gate="$1" attempt
    for ((attempt = 0; attempt < 3; attempt += 1)); do
        rm -rf -- "$gate" 2>/dev/null || true
        if [ ! -e "$gate" ] && [ ! -L "$gate" ]; then
            return 0
        fi
        /bin/sleep 0.02
    done
    return 1
}

archive_write_phase_override_snapshot() {
    local gate="$1" partial="$1/phase-overrides.sh.partial"
    local function_name override_names="${ARC_ARCHIVE_DISPATCH_TEST_OVERRIDE_NAMES:-}"
    local IFS=$' \t\n'
    # The executable production CLI rejects overrides below. Sourced contract
    # tests may name only the few test doubles that the fresh phase interpreter
    # must retain; never serialize the whole shell function table (Bash 3.2
    # cannot round-trip some heredoc definitions emitted by `declare -f`).
    (umask 077; : > "$partial") || return 1
    for function_name in $override_names; do
        case "$function_name" in
            ''|*[!A-Za-z0-9_]*) return 1 ;;
        esac
        case "$function_name" in [0-9]*) return 1 ;; esac
        declare -F "$function_name" >/dev/null || return 1
        declare -f "$function_name" >> "$partial" || return 1
    done
    /bin/bash -n "$partial" || return 1
    chmod 400 "$partial" || return 1
    /bin/mv -f "$partial" "$gate/phase-overrides.sh"
}

archive_remove_dispatch_gate_until_absent() {
    local gate="$1" attempts=0
    while ! archive_remove_dispatch_gate "$gate"; do
        attempts=$((attempts + 1))
        if [ "$attempts" -eq 1 ] || [ $((attempts % 60)) -eq 0 ]; then
            printf 'archive fleet: FATAL guardian retaining and retrying private dispatch gate: %s\n' \
                "$gate" >&2
        fi
        /bin/sleep 1
    done
    return 0
}

ARC_ARCHIVE_DISPATCH_GATE=""
ARC_ARCHIVE_DISPATCH_STOP_TOKEN=""
ARC_ARCHIVE_DISPATCH_GROUP_VALIDATED=false
ARC_ARCHIVE_DISPATCH_PHASE_JOB_ACTIVE=false
ARC_ARCHIVE_DISPATCH_SIGNAL=""
ARC_ARCHIVE_DISPATCH_SIGNAL_STATUS=0
ARC_ARCHIVE_DISPATCH_SIGNAL_FORWARDED=false

archive_dispatch_forward_signal() {
    local signal="$1" status="$2"
    if [ "$ARC_ARCHIVE_DISPATCH_SIGNAL_STATUS" -eq 0 ]; then
        ARC_ARCHIVE_DISPATCH_SIGNAL="$signal"
        ARC_ARCHIVE_DISPATCH_SIGNAL_STATUS="$status"
    fi
    if [ "$ARC_ARCHIVE_DISPATCH_SIGNAL_FORWARDED" = false ]; then
        if [ "$ARC_ARCHIVE_DISPATCH_GROUP_VALIDATED" = true ]; then
            # After the arm handshake, only the in-group sentinel may interpret
            # the numeric phase PGID. This parent writes an authenticated FIFO
            # command and later requires the sentinel's acknowledgement.
            if archive_send_sentinel_command "$ARC_ARCHIVE_DISPATCH_GATE" \
                "$ARC_ARCHIVE_DISPATCH_STOP_TOKEN" SIGNAL \
                "$ARC_ARCHIVE_DISPATCH_SIGNAL"; then
                ARC_ARCHIVE_DISPATCH_SIGNAL_FORWARDED=true
            fi
        else
            if [ "$ARC_ARCHIVE_DISPATCH_PHASE_JOB_ACTIVE" = true ]; then
                # The shell job table cannot resolve a completed job to a
                # recycled numeric PID. The phase is the sole parent job until
                # validated sentinel-anchored group signaling takes over.
                if builtin kill -s "$ARC_ARCHIVE_DISPATCH_SIGNAL" -- '%%' 2>/dev/null; then
                    ARC_ARCHIVE_DISPATCH_SIGNAL_FORWARDED=true
                fi
            fi
        fi
    fi
}

archive_restore_signal_trap() {
    local saved="$1" signal="$2"
    trap - "$signal"
    if [ -n "$saved" ]; then
        # `trap -p` emits shell-quoted Bash syntax; evaluating only that
        # interpreter-generated value restores the caller's exact handler.
        # shellcheck disable=SC2294
        eval "$saved"
    fi
}

archive_dispatch_phase() {
    local supervisor_pid="$1" gate="$2" command_name="$3"
    shift 3
    local phase_pid="" phase_pgid="" sentinel_pid="" sentinel_pgid=""
    local observed_parent="" stop_token="" ready_sentinel_pid="" ready_stop_token=""
    local bootstrap_exit_status=0 bootstrap_attempt
    local bootstrap_signal_status=0 sentinel_handed_off=false
    set +m
    trap 'bootstrap_exit_status=$?; trap - EXIT; \
        if [ "$sentinel_handed_off" = false ] && [ -n "$sentinel_pid" ] && \
            [ -n "$phase_pgid" ] && [ -n "$stop_token" ]; then \
            archive_stop_dispatch_sentinel "$sentinel_pid" "$phase_pgid" \
                "$gate/sentinel.fifo" "$stop_token"; \
        fi; \
        exit "$bootstrap_exit_status"' EXIT
    # Bootstrap signals are recorded, not allowed to orphan a half-published
    # sentinel. They become exact exits immediately after readiness is durable.
    trap 'bootstrap_signal_status=129' HUP
    trap 'bootstrap_signal_status=130' INT
    trap 'bootstrap_signal_status=143' TERM
    archive_write_current_process_id "$gate/phase.pid.partial" || exit 125
    IFS= read -r phase_pid < "$gate/phase.pid.partial" || exit 125
    phase_pgid="$(archive_process_field pgid "$phase_pid")" || exit 125
    mkfifo "$gate/sentinel.fifo" || exit 125
    chmod 600 "$gate/sentinel.fifo" || exit 125
    stop_token="ARC-ARCHIVE-STOP:$supervisor_pid:$phase_pid:$RANDOM:$RANDOM"
    archive_dispatch_sentinel "$supervisor_pid" "$phase_pid" "$gate" "$phase_pgid" \
        "$stop_token" &
    sentinel_pid="$!"
    for ((bootstrap_attempt = 0; bootstrap_attempt < 250; bootstrap_attempt += 1)); do
        [ -f "$gate/sentinel.ready" ] && break
        archive_process_exists "$sentinel_pid" || break
        /bin/sleep 0.02
    done
    if [ ! -f "$gate/sentinel.ready" ] || [ -L "$gate/sentinel.ready" ]; then
        exit 125
    fi
    IFS=$'\t' read -r ready_sentinel_pid sentinel_pgid ready_stop_token \
        < "$gate/sentinel.ready" || exit 125
    if [ "$ready_sentinel_pid" != "$sentinel_pid" ] || \
        [ "$sentinel_pgid" != "$phase_pgid" ] || \
        [ "$ready_stop_token" != "$stop_token" ] || [ "$sentinel_pid" = "$phase_pid" ]; then
        exit 125
    fi
    TMPDIR="$gate/runtime"
    export TMPDIR
    (umask 077; printf '%s\t%s\t%s\t%s\t%s\n' \
        "$phase_pid" "$phase_pgid" "$sentinel_pid" "$sentinel_pgid" "$stop_token" \
        > "$gate/phase.ready.partial")
    /bin/mv -f "$gate/phase.ready.partial" "$gate/phase.ready"
    sentinel_handed_off=true
    trap - EXIT
    trap 'exit 129' HUP
    trap 'exit 130' INT
    trap 'exit 143' TERM
    case "$bootstrap_signal_status" in
        0) ;;
        129|130|143) exit "$bootstrap_signal_status" ;;
        *) exit 125 ;;
    esac
    while [ ! -f "$gate/go" ]; do
        observed_parent="$(archive_process_field ppid "$phase_pid")" || observed_parent=""
        if [ "$observed_parent" != "$supervisor_pid" ]; then
            archive_stop_dispatch_sentinel "$sentinel_pid" "$phase_pgid" \
                "$gate/sentinel.fifo" "$stop_token"
            archive_remove_dispatch_gate_until_absent "$gate"
            exit 143
        fi
        /bin/sleep 0.02
    done
    # Keep this as a direct simple command. Placing a phase function in an
    # if/!/&&/|| condition disables errexit throughout that function and could
    # let a failed precondition continue into a later mutation.
    "$command_name" "$@"
}

archive_dispatch_parent_watchdog() {
    local sentinel_parent_pid="$1" phase_pid="$2" phase_pgid="$3" sentinel_pid="$4"
    local stop_token="$5" gate="$6" guardian_mode="${7:-monitor}"
    local watchdog_pid watchdog_pgid observed_parent attempt requested_pid requested_token
    local heartbeat_sequence=0
    set +m
    trap '' HUP INT TERM
    archive_write_current_process_id "$gate/watchdog.pid.partial" || exit 125
    IFS= read -r watchdog_pid < "$gate/watchdog.pid.partial" || exit 125
    watchdog_pgid="$(archive_process_field pgid "$watchdog_pid")" || exit 125
    [ "$watchdog_pid" = "$watchdog_pgid" ] || exit 125
    observed_parent="$(archive_process_field ppid "$watchdog_pid")" || exit 125
    [ "$observed_parent" = "$sentinel_parent_pid" ] || exit 125
    if [ -n "${ARC_ARCHIVE_DISPATCH_TEST_OVERRIDE_NAMES:-}" ] && \
        [ "${ARC_ARCHIVE_DISPATCH_TEST_STOP_WATCHDOG_BEFORE_READY:-false}" = true ] && \
        mkdir "$gate/test-watchdog-stopped-once" 2>/dev/null; then
        builtin kill -s STOP -- "$watchdog_pid"
    fi
    (umask 077; printf '%s\t%s\t%s\n' "$watchdog_pid" "$watchdog_pgid" "$stop_token" \
        > "$gate/watchdog.ready.partial")
    /bin/mv -f "$gate/watchdog.ready.partial" "$gate/watchdog.ready"
    if [ "$guardian_mode" = monitor ]; then
        while :; do
            heartbeat_sequence=$((heartbeat_sequence + 1))
            archive_guardian_publish_heartbeat "$gate" "$watchdog_pid" "$watchdog_pgid" \
                "$stop_token" "$heartbeat_sequence" || exit 125
            if [ -f "$gate/guardian.finalize" ] && [ ! -L "$gate/guardian.finalize" ]; then
                requested_pid=""; requested_token=""
                IFS=$'\t' read -r requested_pid requested_token \
                    < "$gate/guardian.finalize" || true
                if [ "$requested_pid" = "$watchdog_pid" ] && \
                    [ "$requested_token" = "$stop_token" ]; then
                    guardian_mode=finalize
                    break
                fi
            fi
            observed_parent="$(archive_process_field ppid "$watchdog_pid")" || observed_parent=""
            # A guardian never acts on a numeric phase PGID after losing the
            # sentinel that anchors it. Retain the gate and exit harmlessly;
            # normal teardown always has the sentinel reap this child first.
            [ "$observed_parent" = "$sentinel_parent_pid" ] || return 125
            /bin/sleep 0.02
        done
    fi

    [ "$guardian_mode" = finalize ] || return 125
    archive_guardian_anchor_valid "$watchdog_pid" "$sentinel_pid" "$phase_pgid" || return 125
    # The in-group sentinel anchors phase_pgid throughout this drain. Give the
    # phase a bounded cooperative path before exact stopped-member KILL.
    builtin kill -s TERM -- "-$phase_pgid" 2>/dev/null || true
    for ((attempt = 0; attempt < 250; attempt += 1)); do
        heartbeat_sequence=$((heartbeat_sequence + 1))
        archive_guardian_publish_heartbeat "$gate" "$watchdog_pid" "$watchdog_pgid" \
            "$stop_token" "$heartbeat_sequence" || return 125
        archive_guardian_anchor_valid "$watchdog_pid" "$sentinel_pid" "$phase_pgid" || return 125
        archive_process_exists "$phase_pid" || break
        /bin/sleep 0.02
    done
    if archive_process_exists "$phase_pid"; then
        # A foreground child may ignore TERM while Bash defers its trap. Kill
        # only the other group members. Releasing Bash's already-pending TERM
        # lets its EXIT handler run without a second signal interrupting it.
        archive_stop_and_kill_group_members_except "$phase_pgid" \
            "$phase_pid" "$sentinel_pid" || true
        for ((attempt = 0; attempt < 250; attempt += 1)); do
            heartbeat_sequence=$((heartbeat_sequence + 1))
            archive_guardian_publish_heartbeat "$gate" "$watchdog_pid" "$watchdog_pgid" \
                "$stop_token" "$heartbeat_sequence" || return 125
            archive_guardian_anchor_valid "$watchdog_pid" "$sentinel_pid" "$phase_pgid" || return 125
            archive_process_exists "$phase_pid" || break
            /bin/sleep 0.02
        done
    fi
    if archive_process_exists "$phase_pid"; then
        # No software can make an EXIT trap run in a permanently wedged Bash.
        # Bound the guardian instead of permitting post-parent mutations.
        archive_stop_and_kill_group_members_except "$phase_pgid" "$sentinel_pid" || true
    fi
    if archive_process_group_has_members_except "$phase_pgid" "$sentinel_pid"; then
        archive_guardian_anchor_valid "$watchdog_pid" "$sentinel_pid" "$phase_pgid" || return 125
        builtin kill -s TERM -- "-$phase_pgid" 2>/dev/null || true
        for ((attempt = 0; attempt < 250; attempt += 1)); do
            heartbeat_sequence=$((heartbeat_sequence + 1))
            archive_guardian_publish_heartbeat "$gate" "$watchdog_pid" "$watchdog_pgid" \
                "$stop_token" "$heartbeat_sequence" || return 125
            archive_guardian_anchor_valid "$watchdog_pid" "$sentinel_pid" "$phase_pgid" || return 125
            archive_process_group_has_members_except "$phase_pgid" "$sentinel_pid" || break
            /bin/sleep 0.02
        done
    fi
    if archive_process_group_has_members_except "$phase_pgid" "$sentinel_pid"; then
        # The cleanup owner is already gone, so only signal-ignoring leaked
        # descendants remain and can be force-killed without interrupting it.
        archive_stop_and_kill_group_members_except "$phase_pgid" "$sentinel_pid" || true
        for ((attempt = 0; attempt < 250; attempt += 1)); do
            archive_process_group_has_members_except "$phase_pgid" "$sentinel_pid" || break
            /bin/sleep 0.02
        done
    fi
    # A KILL-pending task can remain temporarily visible while uninterruptible.
    # Retain the sentinel/gate and retry exact member kills until every mutable
    # member is truly gone; only then release the PGID anchor by exact PID.
    while archive_process_group_has_members_except "$phase_pgid" "$sentinel_pid"; do
        heartbeat_sequence=$((heartbeat_sequence + 1))
        archive_guardian_publish_heartbeat "$gate" "$watchdog_pid" "$watchdog_pgid" \
            "$stop_token" "$heartbeat_sequence" || return 125
        archive_guardian_anchor_valid "$watchdog_pid" "$sentinel_pid" "$phase_pgid" || return 125
        archive_stop_and_kill_group_members_except "$phase_pgid" "$sentinel_pid" || true
        /bin/sleep 0.1
    done
    archive_guardian_anchor_valid "$watchdog_pid" "$sentinel_pid" "$phase_pgid" || return 125
    (umask 077; printf '%s\t%s\n' "$watchdog_pid" "$stop_token" \
        > "$gate/guardian.finalized.partial") || return 125
    /bin/mv -f "$gate/guardian.finalized.partial" "$gate/guardian.finalized"
    return 0
}

dispatch_archive_command() {
    local command_name="$1"
    shift
    case "$command_name" in
        prepare_writers|audit_writers|seal_freeze_plan|capture_phase|\
        verify_offline_stop_phase|verify_installed_keys_phase|seal_phase|verify_complete_phase) ;;
        *) printf 'archive fleet: internal dispatcher rejected command: %s\n' "$command_name" >&2; return 2 ;;
    esac
    if [ -n "${ARC_ARCHIVE_DISPATCH_TEST_OVERRIDE_NAMES:-}" ] && \
        [ "${BASH_SOURCE[0]}" = "$0" ]; then
        printf 'archive fleet: FATAL phase test overrides are forbidden in the executable operator CLI\n' >&2
        return 125
    fi

    local supervisor_pid gate="" gate_parent="${TMPDIR:-/tmp}"
    local requested_work_root="" resolved_work_root=""
    local phase_pid="" phase_pgid="" sentinel_pid="" sentinel_pgid="" stop_token=""
    local candidate_phase_pid="" candidate_phase_pgid="" candidate_sentinel_pid=""
    local candidate_sentinel_pgid="" candidate_stop_token=""
    local watchdog_pid="" watchdog_pgid="" ready_pid="" ready_pgid="" ready_token=""
    local ready_sequence="" last_guardian_sequence="" guardian_last_seen=0
    local supervisor_sequence=0 loop_count=0 closing_ticks=0
    local phase_status=125 setup_status=0 attempt
    local guardian_ready=false sentinel_adopted=false sentinel_membership_valid=false
    local sentinel_armed=false finalize_acknowledged=false gate_removed=false
    local terminal_receipt=false terminal_guardian_failed=""
    local ready_fields_valid=true signal_acknowledged=false
    local monitor_enabled=false saved_hup saved_int saved_term
    saved_hup="$(trap -p HUP)"
    saved_int="$(trap -p INT)"
    saved_term="$(trap -p TERM)"
    case $- in *m*) monitor_enabled=true ;; esac
    ARC_ARCHIVE_DISPATCH_GATE=""
    ARC_ARCHIVE_DISPATCH_STOP_TOKEN=""
    ARC_ARCHIVE_DISPATCH_GROUP_VALIDATED=false
    ARC_ARCHIVE_DISPATCH_PHASE_JOB_ACTIVE=false
    ARC_ARCHIVE_DISPATCH_SIGNAL=""
    ARC_ARCHIVE_DISPATCH_SIGNAL_STATUS=0
    ARC_ARCHIVE_DISPATCH_SIGNAL_FORWARDED=false
    trap 'archive_dispatch_forward_signal HUP 129' HUP
    trap 'archive_dispatch_forward_signal INT 130' INT
    trap 'archive_dispatch_forward_signal TERM 143' TERM

    if [ "$command_name" = seal_phase ]; then
        local argument_index argument_value
        argument_index=1
        while [ "$argument_index" -le "$#" ]; do
            argument_value="${!argument_index}"
            if [ "$argument_value" = --work-root ] && [ "$argument_index" -lt "$#" ]; then
                argument_index=$((argument_index + 1))
                requested_work_root="${!argument_index}"
            fi
            argument_index=$((argument_index + 1))
        done
        case "$requested_work_root" in
            /*)
                if [ -d "$requested_work_root" ] && [ ! -L "$requested_work_root" ] && \
                    [ -O "$requested_work_root" ] && \
                    resolved_work_root="$(CDPATH='' cd -- "$requested_work_root" 2>/dev/null && pwd -P)" && \
                    [ "$resolved_work_root" = "$requested_work_root" ]; then
                    gate_parent="$requested_work_root"
                fi
                ;;
        esac
    fi
    if gate="$(mktemp -d "$gate_parent/arc-archive-dispatch.XXXXXX")"; then
        chmod 700 "$gate" || setup_status=125
        mkdir -m 700 "$gate/runtime" || setup_status=125
        archive_write_current_process_id "$gate/supervisor.pid" || setup_status=125
        archive_write_phase_override_snapshot "$gate" || setup_status=125
        if [ "$setup_status" -eq 0 ]; then
            IFS= read -r supervisor_pid < "$gate/supervisor.pid" || setup_status=125
            case "$supervisor_pid" in ''|*[!0-9]*) setup_status=125 ;; esac
        fi
    else
        setup_status=125
    fi

    if [ "$setup_status" -eq 0 ] && [ "$ARC_ARCHIVE_DISPATCH_SIGNAL_STATUS" -eq 0 ]; then
        set -m
        # A fresh Bash boundary restores errexit even when the caller invokes
        # this dispatcher under if/!/&&/||. Remove every environment-imported
        # function before sourcing the exact orchestrator and narrow test file.
        BASH_ENV=/dev/null ENV=/dev/null /bin/bash --noprofile --norc -Eeuo pipefail -c '
for imported_function in $(builtin compgen -A function); do
    builtin unset -f "$imported_function"
done
unset imported_function
phase_orchestrator=$1
phase_overrides=$2
shift 2
phase_arguments=("$@")
set --
. "$phase_orchestrator" >/dev/null
. "$phase_overrides"
archive_dispatch_phase "${phase_arguments[@]}"
' arc-archive-dispatch-phase "$ORCHESTRATOR" "$gate/phase-overrides.sh" \
            "$supervisor_pid" "$gate" "$command_name" "$@" &
        phase_pid="$!"
        ARC_ARCHIVE_DISPATCH_PHASE_JOB_ACTIVE=true
        for ((attempt = 0; attempt < 250; attempt += 1)); do
            [ -f "$gate/phase.ready" ] && break
            archive_process_exists "$phase_pid" || break
            /bin/sleep 0.02
        done
        if [ ! -f "$gate/phase.ready" ]; then
            for ((attempt = 0; attempt < 250; attempt += 1)); do
                [ -f "$gate/sentinel.ready" ] && break
                archive_process_exists "$phase_pid" || break
                /bin/sleep 0.02
            done
        fi
        if [ -f "$gate/phase.ready" ] && [ ! -L "$gate/phase.ready" ]; then
            IFS=$'\t' read -r candidate_phase_pid candidate_phase_pgid \
                candidate_sentinel_pid candidate_sentinel_pgid candidate_stop_token \
                < "$gate/phase.ready" || setup_status=125
            case "$candidate_phase_pid:$candidate_phase_pgid:$candidate_sentinel_pid:$candidate_sentinel_pgid" in
                *[!0-9:]*) ready_fields_valid=false ;;
            esac
            if [ "$ready_fields_valid" = false ] || \
                [ "$candidate_phase_pid" != "$phase_pid" ] || \
                [ "$candidate_phase_pgid" != "$phase_pid" ] || \
                [ "$candidate_sentinel_pid" = "$candidate_phase_pid" ] || \
                [ "$candidate_sentinel_pgid" != "$candidate_phase_pgid" ] || \
                [ -z "$candidate_stop_token" ]; then
                setup_status=125
            else
                phase_pgid="$candidate_phase_pgid"
                sentinel_pid="$candidate_sentinel_pid"
                sentinel_pgid="$candidate_sentinel_pgid"
                stop_token="$candidate_stop_token"
                sentinel_adopted=true
            fi
        else
            setup_status=125
        fi
        if [ "$sentinel_adopted" = false ] && [ -f "$gate/sentinel.ready" ] && \
            [ ! -L "$gate/sentinel.ready" ]; then
            candidate_sentinel_pid=""; candidate_sentinel_pgid=""; candidate_stop_token=""
            IFS=$'\t' read -r candidate_sentinel_pid candidate_sentinel_pgid candidate_stop_token \
                < "$gate/sentinel.ready" || true
            case "$candidate_sentinel_pid:$candidate_sentinel_pgid" in
                *[!0-9:]*) ;;
                *)
                    if [ "$candidate_sentinel_pgid" = "$phase_pid" ] && \
                        [ -n "$candidate_stop_token" ]; then
                        phase_pgid="$candidate_sentinel_pgid"
                        sentinel_pid="$candidate_sentinel_pid"
                        sentinel_pgid="$candidate_sentinel_pgid"
                        stop_token="$candidate_stop_token"
                        sentinel_adopted=true
                    fi
                    ;;
            esac
        fi
        if [ "$sentinel_adopted" = true ]; then
            for ((attempt = 0; attempt < 250; attempt += 1)); do
                if archive_process_in_group "$sentinel_pid" "$phase_pgid"; then
                    sentinel_membership_valid=true
                    break
                fi
                archive_process_exists "$sentinel_pid" || break
                /bin/sleep 0.02
            done
        fi
        [ "$sentinel_membership_valid" = true ] || setup_status=125
    fi

    if [ "$sentinel_membership_valid" = true ] && \
        [ "$ARC_ARCHIVE_DISPATCH_SIGNAL_STATUS" -eq 0 ]; then
        supervisor_sequence=$((supervisor_sequence + 1))
        archive_supervisor_publish_heartbeat "$gate" "$supervisor_pid" "$stop_token" \
            "$supervisor_sequence" || setup_status=125
        if ! (umask 077; printf '%s\n' "$stop_token" > "$gate/guardian.start.partial") || \
            ! /bin/mv -f "$gate/guardian.start.partial" "$gate/guardian.start"; then
            setup_status=125
        fi
        for ((attempt = 0; attempt < 500; attempt += 1)); do
            [ -f "$gate/watchdog.ready" ] && break
            [ -e "$gate" ] || break
            supervisor_sequence=$((supervisor_sequence + 1))
            archive_supervisor_publish_heartbeat "$gate" "$supervisor_pid" "$stop_token" \
                "$supervisor_sequence" || { setup_status=125; break; }
            /bin/sleep 0.02
        done
        if [ -f "$gate/watchdog.ready" ] && [ ! -L "$gate/watchdog.ready" ]; then
            ready_pid=""; ready_pgid=""; ready_token=""
            IFS=$'\t' read -r ready_pid ready_pgid ready_token \
                < "$gate/watchdog.ready" || setup_status=125
            if [ "$ready_pid" = "$ready_pgid" ] && [ "$ready_pgid" != "$phase_pgid" ] && \
                [ "$ready_token" = "$stop_token" ]; then
                watchdog_pid="$ready_pid"
                watchdog_pgid="$ready_pgid"
                for ((attempt = 0; attempt < 250; attempt += 1)); do
                    ready_pid="$(archive_process_field ppid "$watchdog_pid")" || ready_pid=""
                    if [ "$ready_pid" = "$sentinel_pid" ] && \
                        archive_process_in_group "$watchdog_pid" "$watchdog_pgid"; then
                        guardian_ready=true
                        break
                    fi
                    /bin/sleep 0.02
                done
            fi
        fi
        [ "$guardian_ready" = true ] || setup_status=125
        if [ "$guardian_ready" = true ]; then
            guardian_ready=false
            for ((attempt = 0; attempt < 250; attempt += 1)); do
                if [ -f "$gate/guardian.heartbeat" ] && \
                    [ ! -L "$gate/guardian.heartbeat" ]; then
                    ready_pid=""; ready_pgid=""; ready_token=""; ready_sequence=""
                    IFS=$'\t' read -r ready_pid ready_pgid ready_token ready_sequence \
                        < "$gate/guardian.heartbeat" || true
                    if [ "$ready_pid" = "$watchdog_pid" ] && \
                        [ "$ready_pgid" = "$watchdog_pgid" ] && \
                        [ "$ready_token" = "$stop_token" ] && [ -n "$ready_sequence" ]; then
                        last_guardian_sequence="$ready_sequence"
                        guardian_last_seen="$SECONDS"
                        guardian_ready=true
                        break
                    fi
                fi
                /bin/sleep 0.02
            done
            [ "$guardian_ready" = true ] || setup_status=125
        fi
        [ ! -f "$gate/guardian.failed" ] || setup_status=125
        if [ "$guardian_ready" = true ]; then
            for ((attempt = 0; attempt < 250; attempt += 1)); do
                supervisor_sequence=$((supervisor_sequence + 1))
                archive_supervisor_publish_heartbeat "$gate" "$supervisor_pid" "$stop_token" \
                    "$supervisor_sequence" || { setup_status=125; break; }
                archive_send_sentinel_command "$gate" "$stop_token" ARM || true
                if [ -f "$gate/sentinel.armed" ] && [ ! -L "$gate/sentinel.armed" ]; then
                    ready_pid=""; ready_token=""
                    IFS=$'\t' read -r ready_pid ready_token < "$gate/sentinel.armed" || true
                    if [ "$ready_pid" = "$sentinel_pid" ] && [ "$ready_token" = "$stop_token" ]; then
                        sentinel_armed=true
                        break
                    fi
                fi
                [ -e "$gate" ] || break
                /bin/sleep 0.02
            done
        fi
        [ "$sentinel_armed" = true ] || setup_status=125
        if [ "$sentinel_armed" = true ]; then
            ARC_ARCHIVE_DISPATCH_GATE="$gate"
            ARC_ARCHIVE_DISPATCH_STOP_TOKEN="$stop_token"
            ARC_ARCHIVE_DISPATCH_GROUP_VALIDATED=true
            ARC_ARCHIVE_DISPATCH_PHASE_JOB_ACTIVE=false
        fi
    fi

    if [ "$setup_status" -eq 0 ] && [ "$ARC_ARCHIVE_DISPATCH_SIGNAL_STATUS" -eq 0 ]; then
        if ! (umask 077; printf 'go\n' > "$gate/go.partial") || \
            ! /bin/mv -f "$gate/go.partial" "$gate/go"; then
            setup_status=125
        fi
    fi
    if [ "$setup_status" -ne 0 ] || [ "$ARC_ARCHIVE_DISPATCH_SIGNAL_STATUS" -ne 0 ]; then
        if [ "$ARC_ARCHIVE_DISPATCH_SIGNAL_STATUS" -ne 0 ]; then
            archive_dispatch_forward_signal "$ARC_ARCHIVE_DISPATCH_SIGNAL" \
                "$ARC_ARCHIVE_DISPATCH_SIGNAL_STATUS"
        fi
        if [ "$sentinel_membership_valid" = true ] && [ "$setup_status" -ne 0 ]; then
            archive_send_sentinel_command "$gate" "$stop_token" FINALIZE || true
        elif [ "$sentinel_membership_valid" = false ] && [ -n "$phase_pid" ]; then
            builtin kill -s TERM -- '%%' 2>/dev/null || true
            builtin kill -s CONT -- '%%' 2>/dev/null || true
        fi
    fi

    if [ -n "$phase_pid" ]; then
        while archive_process_exists "$phase_pid"; do
            loop_count=$((loop_count + 1))
            if [ "$sentinel_membership_valid" = true ] && [ -e "$gate" ] && \
                [ $((loop_count % 5)) -eq 0 ]; then
                supervisor_sequence=$((supervisor_sequence + 1))
                archive_supervisor_publish_heartbeat "$gate" "$supervisor_pid" "$stop_token" \
                    "$supervisor_sequence" || setup_status=125
            fi
            if [ "$ARC_ARCHIVE_DISPATCH_SIGNAL_STATUS" -ne 0 ]; then
                archive_dispatch_forward_signal "$ARC_ARCHIVE_DISPATCH_SIGNAL" \
                    "$ARC_ARCHIVE_DISPATCH_SIGNAL_STATUS"
                if [ -f "$gate/signal.$ARC_ARCHIVE_DISPATCH_SIGNAL.ack" ] && \
                    [ ! -L "$gate/signal.$ARC_ARCHIVE_DISPATCH_SIGNAL.ack" ]; then
                    ready_token=""
                    IFS= read -r ready_token \
                        < "$gate/signal.$ARC_ARCHIVE_DISPATCH_SIGNAL.ack" || true
                    [ "$ready_token" = "$stop_token" ] && signal_acknowledged=true
                fi
            fi
            if [ "$guardian_ready" = true ]; then
                if [ -f "$gate/guardian.failed" ] || [ ! -f "$gate/guardian.heartbeat" ]; then
                    setup_status=125
                else
                    ready_pid=""; ready_pgid=""; ready_token=""; ready_sequence=""
                    IFS=$'\t' read -r ready_pid ready_pgid ready_token ready_sequence \
                        < "$gate/guardian.heartbeat" || true
                    if [ "$ready_pid" = "$watchdog_pid" ] && \
                        [ "$ready_pgid" = "$watchdog_pgid" ] && \
                        [ "$ready_token" = "$stop_token" ] && \
                        [ -n "$ready_sequence" ] && \
                        [ "$ready_sequence" != "$last_guardian_sequence" ]; then
                        last_guardian_sequence="$ready_sequence"
                        guardian_last_seen="$SECONDS"
                    elif [ $((SECONDS - guardian_last_seen)) -ge 5 ]; then
                        setup_status=125
                    fi
                fi
            fi
            if [ "$setup_status" -ne 0 ]; then
                archive_send_sentinel_command "$gate" "$stop_token" FINALIZE || true
                closing_ticks=$((closing_ticks + 1))
            elif [ "$ARC_ARCHIVE_DISPATCH_SIGNAL_STATUS" -ne 0 ]; then
                closing_ticks=$((closing_ticks + 1))
            fi
            [ "$closing_ticks" -lt 1000 ] || break
            /bin/sleep 0.02
        done
        if ! archive_process_exists "$phase_pid"; then
            if wait "$phase_pid" 2>/dev/null; then phase_status=0; else phase_status=$?; fi
        elif [ "$sentinel_membership_valid" = false ]; then
            builtin kill -s STOP -- '%%' 2>/dev/null || true
            builtin kill -s KILL -- '%%' 2>/dev/null || true
            wait "$phase_pid" 2>/dev/null || true
            setup_status=125
        else
            setup_status=125
        fi
    fi

    if [ "$sentinel_membership_valid" = true ] && [ -e "$gate" ]; then
        for ((attempt = 0; attempt < 500; attempt += 1)); do
            archive_send_sentinel_command "$gate" "$stop_token" FINALIZE || true
            if [ -f "$gate/sentinel.finalize.ack" ] && \
                [ ! -L "$gate/sentinel.finalize.ack" ]; then
                ready_token=""
                IFS= read -r ready_token < "$gate/sentinel.finalize.ack" || true
                [ "$ready_token" = "$stop_token" ] && finalize_acknowledged=true
            fi
            [ "$finalize_acknowledged" = true ] && break
            [ -e "$gate" ] || break
            supervisor_sequence=$((supervisor_sequence + 1))
            archive_supervisor_publish_heartbeat "$gate" "$supervisor_pid" "$stop_token" \
                "$supervisor_sequence" || true
            /bin/sleep 0.02
        done
        if [ "$finalize_acknowledged" = false ] && [ -e "$gate" ]; then
            setup_status=125
        fi
        for ((attempt = 0; attempt < 1000; attempt += 1)); do
            if [ -f "$gate/sentinel.complete" ] && [ ! -L "$gate/sentinel.complete" ]; then
                ready_pid=""; ready_token=""; terminal_guardian_failed=""
                IFS=$'\t' read -r ready_pid ready_token terminal_guardian_failed \
                    < "$gate/sentinel.complete" || true
                if [ "$ready_pid" = "$sentinel_pid" ] && [ "$ready_token" = "$stop_token" ]; then
                    case "$terminal_guardian_failed" in true|false) terminal_receipt=true ;; esac
                    break
                fi
            fi
            [ -e "$gate" ] || break
            /bin/sleep 0.02
        done
        if [ "$terminal_receipt" = true ]; then
            [ "$terminal_guardian_failed" = false ] || setup_status=125
            (umask 077; printf '%s\t%s\n' "$supervisor_pid" "$stop_token" \
                > "$gate/supervisor.complete.ack.partial") && \
                /bin/mv -f "$gate/supervisor.complete.ack.partial" \
                    "$gate/supervisor.complete.ack" || setup_status=125
        else
            # The sentinel may have failed closed while this supervisor was
            # stopped. Absence without the sticky terminal receipt is not a
            # normal success proof.
            setup_status=125
        fi
        ARC_ARCHIVE_DISPATCH_GROUP_VALIDATED=false
        ARC_ARCHIVE_DISPATCH_PHASE_JOB_ACTIVE=false
        ARC_ARCHIVE_DISPATCH_GATE=""
        ARC_ARCHIVE_DISPATCH_STOP_TOKEN=""
        for ((attempt = 0; attempt < 1000; attempt += 1)); do
            if [ ! -e "$gate" ] && [ ! -L "$gate" ]; then
                gate_removed=true
                break
            fi
            /bin/sleep 0.02
        done
        if [ "$gate_removed" = false ]; then
            printf 'archive fleet: FATAL containment continues asynchronously in private gate: %s\n' \
                "$gate" >&2
            setup_status=125
        fi
    elif [ -n "$gate" ]; then
        if [ -n "$phase_pid" ] && archive_process_exists "$phase_pid"; then
            setup_status=125
        elif archive_remove_dispatch_gate "$gate"; then
            gate_removed=true
        else
            archive_remove_dispatch_gate_until_absent "$gate"
            gate_removed=true
            setup_status=125
        fi
    fi

    if [ -n "$phase_pid" ] && ! archive_process_exists "$phase_pid"; then
        wait "$phase_pid" 2>/dev/null || true
    fi
    ARC_ARCHIVE_DISPATCH_GROUP_VALIDATED=false
    ARC_ARCHIVE_DISPATCH_PHASE_JOB_ACTIVE=false
    ARC_ARCHIVE_DISPATCH_GATE=""
    ARC_ARCHIVE_DISPATCH_STOP_TOKEN=""
    archive_restore_signal_trap "$saved_hup" HUP
    archive_restore_signal_trap "$saved_int" INT
    archive_restore_signal_trap "$saved_term" TERM
    if [ "$monitor_enabled" = true ]; then set -m; else set +m; fi

    if [ "$ARC_ARCHIVE_DISPATCH_SIGNAL_STATUS" -ne 0 ]; then
        if [ "$sentinel_armed" = true ] && [ "$signal_acknowledged" = false ] && \
            [ "$gate_removed" = false ]; then
            printf 'archive fleet: FATAL signal delivery unacknowledged; containment retained: %s\n' \
                "$gate" >&2
        fi
        return "$ARC_ARCHIVE_DISPATCH_SIGNAL_STATUS"
    fi
    [ "$setup_status" -eq 0 ] || return "$setup_status"
    return "$phase_status"
}

COMMAND="${1:-}"
if [ -n "$COMMAND" ]; then
    shift
fi
case "$COMMAND" in
    prepare-writers) dispatch_archive_command prepare_writers "$@" ;;
    audit-writers) dispatch_archive_command audit_writers "$@" ;;
    seal-freeze-plan) dispatch_archive_command seal_freeze_plan "$@" ;;
    capture) dispatch_archive_command capture_phase "$@" ;;
    verify-offline-stop) dispatch_archive_command verify_offline_stop_phase "$@" ;;
    verify-installed-keys) dispatch_archive_command verify_installed_keys_phase "$@" ;;
    seal) dispatch_archive_command seal_phase "$@" ;;
    verify-complete) dispatch_archive_command verify_complete_phase "$@" ;;
    -h|--help|help|'') usage ;;
    *) usage >&2; exit 2 ;;
esac
