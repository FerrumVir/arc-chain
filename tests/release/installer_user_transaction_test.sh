#!/usr/bin/env bash
set -uo pipefail

TEST_DIR="$(CDPATH='' cd -- "$(dirname -- "$0")" && pwd)"
REPO_ROOT="$(CDPATH='' cd -- "$TEST_DIR/../.." && pwd)"
# shellcheck source=/dev/null
. "$TEST_DIR/helpers/testlib.sh"

root_staging_preserves_target_privileges_and_rollback() {
    python3 - "$REPO_ROOT/install.sh" <<'PY'
import os
from pathlib import Path
import re
import shutil
import subprocess
import sys

if sys.platform != "linux":
    print("Linux root-to-user transaction test runs in the Linux release-contract gate")
    raise SystemExit(0)

source = Path(sys.argv[1]).read_text()
names = (
    "as_root", "as_target", "install_root_as_owner",
    "transaction_crosses_user_boundary", "copy_to_install_owner", "atomic_copy", "transaction_as_owner",
    "run_transaction_executor", "snapshot_transaction_path", "restore_transaction_path",
)
functions = []
for name in names:
    match = re.search(rf"^{name}\(\) \{{\n.*?^\}}\n", source, re.MULTILINE | re.DOTALL)
    if match is None:
        raise SystemExit(f"Missing installer transaction function: {name}")
    functions.append(match.group(0))

script = "set -Eeuo pipefail\numask 077\n" + "\n".join(functions) + r'''
die() { printf '%s\n' "$*" >&2; exit 1; }
CURRENT_UID="$(id -u)"
CURRENT_USER="$(id -un)"
[ "$CURRENT_UID" -eq 0 ]
TARGET_USER=nobody
TARGET_UID="$(id -u "$TARGET_USER")"
TARGET_GROUP="$(id -gn "$TARGET_USER")"
[ "$TARGET_UID" -ne 0 ]
work="$(mktemp -d /tmp/arc-user-transaction.XXXXXX)"
trap 'rm -rf -- "$work"' EXIT
chmod 711 "$work"
TMP_DIR="$work/root-private"
mkdir -m 700 "$TMP_DIR"
printf 'original\n' > "$TMP_DIR/payload"
printf 'root-private-sentinel\n' > "$TMP_DIR/locked"
for SERVICE_SCOPE in none system-user; do
    ARC_DIR="$work/$SERVICE_SCOPE"
    mkdir -m 700 "$ARC_DIR"
    chown "$TARGET_USER:$TARGET_GROUP" "$ARC_DIR"
    USER_TRANSACTION_TMP_DIR=""
    TRANSACTION_PATHS=()
    TRANSACTION_BACKUPS=()
    TRANSACTION_EXISTED=()
    TRANSACTION_EXECUTORS=()
    if as_target test -r "$TMP_DIR/payload"; then
        die 'The target user unexpectedly reads private installer staging'
    fi
    reserved="$(as_target mktemp "$ARC_DIR/.reserved.XXXXXX")"
    copy_to_install_owner "$TMP_DIR/payload" "$reserved"
    [ "$(cat "$reserved")" = original ]
    [ "$(stat -c %u "$reserved")" = "$TARGET_UID" ]
    atomic_copy "$TMP_DIR/payload" "$ARC_DIR/program" 700
    [ "$(stat -c %u "$ARC_DIR/program")" = "$TARGET_UID" ]
    [ "$(stat -c %a "$ARC_DIR/program")" = 700 ]
    as_target touch -t 200001010101.01 "$ARC_DIR/program"
    original_mtime="$(stat -c %Y "$ARC_DIR/program")"
    snapshot_transaction_path "$ARC_DIR/program"
    [ "$(stat -c %u "$USER_TRANSACTION_TMP_DIR")" = "$TARGET_UID" ]
    [ "$(stat -c %a "$USER_TRANSACTION_TMP_DIR")" = 700 ]
    printf 'replacement\n' > "$TMP_DIR/replacement"
    atomic_copy "$TMP_DIR/replacement" "$ARC_DIR/program" 600
    [ "$(cat "$ARC_DIR/program")" = replacement ]
    restore_transaction_path 0
    [ "$(cat "$ARC_DIR/program")" = original ]
    [ "$(stat -c %u "$ARC_DIR/program")" = "$TARGET_UID" ]
    [ "$(stat -c %a "$ARC_DIR/program")" = 700 ]
    [ "$(stat -c %Y "$ARC_DIR/program")" = "$original_mtime" ]
    as_target ln -s "$TMP_DIR/locked" "$ARC_DIR/program.new.$$"
    if atomic_copy "$TMP_DIR/replacement" "$ARC_DIR/program" 600; then
        die 'A pre-existing staged symlink must refuse replacement'
    fi
    [ "$(cat "$TMP_DIR/locked")" = root-private-sentinel ]
    [ "$(cat "$ARC_DIR/program")" = original ]
    [ "$(stat -c %a "$TMP_DIR")" = 700 ]
    as_target rm -rf -- "$USER_TRANSACTION_TMP_DIR"
done
'''
command = ["/bin/bash", "-c", script]
if os.geteuid() != 0:
    sudo = shutil.which("sudo")
    if sudo is None:
        raise SystemExit("Run the Linux root-to-user transaction check in CI with passwordless sudo")
    command = [sudo, "-n", *command]
subprocess.run(command, check=True, timeout=60)
PY
}

run_test 'real Linux root staging preserves target ownership, rollback and private directories' root_staging_preserves_target_privileges_and_rollback
finish_tests
