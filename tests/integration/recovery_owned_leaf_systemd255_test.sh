#!/usr/bin/env bash
# Opt-in destructive integration test for the recovery-owned cgroup v2 leaf.
#
# This test starts and stops disposable systemd units, freezes cgroups, and
# sends SIGKILL during cleanup. Run it only in a disposable Linux VM/container:
#
#   sudo ARC_RUN_DISPOSABLE_SYSTEMD255_TEST=I_UNDERSTAND_THIS_MUTATES_A_DISPOSABLE_HOST \
#     tests/integration/recovery_owned_leaf_systemd255_test.sh

set -Eeuo pipefail

readonly opt_in_value="I_UNDERSTAND_THIS_MUTATES_A_DISPOSABLE_HOST"
if [[ "${ARC_RUN_DISPOSABLE_SYSTEMD255_TEST:-}" != "$opt_in_value" ]]; then
    echo "SKIP: destructive systemd/cgroup integration test requires explicit disposable-host opt-in" >&2
    echo "Set ARC_RUN_DISPOSABLE_SYSTEMD255_TEST=$opt_in_value only inside a disposable VM/container." >&2
    exit 77
fi

die() {
    echo "not ok - $*" >&2
    exit 1
}

[[ "$(uname -s)" == "Linux" ]] || die "Linux is required"
[[ "$EUID" -eq 0 ]] || die "root is required"
for command in systemctl systemd-run stat awk python3; do
    command -v "$command" >/dev/null 2>&1 || die "required command is missing: $command"
done

systemd_version="$(systemctl --version | awk 'NR == 1 { print $2 }')"
[[ "$systemd_version" =~ ^[0-9]+$ ]] || die "could not parse the systemd version"
(( systemd_version >= 255 )) || die "systemd >=255 is required (found $systemd_version)"
[[ "$(stat -fc '%T' /sys/fs/cgroup)" == "cgroup2fs" ]] || die "unified cgroup v2 is required"
[[ -w /sys/fs/cgroup && -r /sys/fs/cgroup/cgroup.controllers ]] || die "writable cgroup v2 is required"
systemctl show --property=Version --value >/dev/null 2>&1 || die "a running system systemd manager is required"

token="arcownedleaf${BASHPID}${RANDOM}"
[[ "$token" =~ ^[a-z0-9]+$ ]] || die "generated unsafe unit token"
readonly scope_unit="$token.scope"
readonly parent_slice="$token.slice"
readonly dropin_dir="/run/systemd/system.control/$scope_unit.d"
readonly dropin_path="$dropin_dir/zzzy-arc-recovery-writer-scope-safety.conf"

runner_pid=""
worker_pid=""
worker_start=""
base=""
leaf=""
leaf_device=""
leaf_inode=""

proc_values() {
    local pid="$1" raw rest
    local -a fields
    raw="$(<"/proc/$pid/stat")" || return 1
    rest="${raw##*) }"
    read -r -a fields <<<"$rest"
    (( ${#fields[@]} >= 20 )) || return 1
    printf '%s %s\n' "${fields[19]}" "$((fields[11] + fields[12]))"
}

event_value() {
    local events="$1" key="$2"
    awk -v wanted="$key" '$1 == wanted { print $2 }' "$events"
}

wait_event() {
    local events="$1" key="$2" expected="$3"
    local _
    for _ in {1..500}; do
        [[ -r "$events" ]] && [[ "$(event_value "$events" "$key")" == "$expected" ]] && return 0
        sleep 0.01
    done
    return 1
}

same_worker() {
    local observed_start _
    [[ -n "$worker_pid" && -n "$worker_start" && -r "/proc/$worker_pid/stat" ]] || return 1
    read -r observed_start _ < <(proc_values "$worker_pid") || return 1
    [[ "$observed_start" == "$worker_start" ]]
}

cleanup() {
    local status=$? current_device current_inode
    trap - EXIT INT TERM

    # Thaw only the inode captured by this test. Never follow a reused path.
    if [[ -n "$leaf" && -d "$leaf" && -n "$leaf_device" && -n "$leaf_inode" ]]; then
        read -r current_device current_inode < <(stat -Lc '%d %i' "$leaf") || true
        if [[ "$current_device" == "$leaf_device" && "$current_inode" == "$leaf_inode" ]]; then
            printf '0' >"$leaf/cgroup.freeze" 2>/dev/null || true
        fi
    fi
    if [[ -n "$base" && -d "$base" ]]; then
        printf '0' >"$base/cgroup.freeze" 2>/dev/null || true
    fi

    # The worker catches TERM by design; SIGKILL is test cleanup only and is
    # sent solely to the PID/start pair created and captured by this script.
    if same_worker; then
        kill -KILL "$worker_pid" 2>/dev/null || true
    fi
    if [[ -n "$runner_pid" ]]; then
        wait "$runner_pid" 2>/dev/null || true
    fi

    if [[ "$dropin_path" == /run/systemd/system.control/arcownedleaf*.scope.d/zzzy-arc-recovery-writer-scope-safety.conf ]]; then
        rm -f -- "$dropin_path"
        rmdir -- "$dropin_dir" 2>/dev/null || true
    fi
    systemctl daemon-reload >/dev/null 2>&1 || true
    systemctl stop "$parent_slice" >/dev/null 2>&1 || true
    systemctl reset-failed "$scope_unit" "$parent_slice" >/dev/null 2>&1 || true

    if (( status != 0 )); then
        echo "owned-leaf integration test failed; disposable resources were cleaned up" >&2
    fi
    exit "$status"
}
trap cleanup EXIT INT TERM

# A scope, not a service, models the detached root-login writer. systemd-run
# waits outside the scope while the caught-TERM busy worker is its sole member.
systemd-run --quiet --scope --unit="$scope_unit" --slice="$parent_slice" \
    /bin/bash -c 'trap : TERM HUP; while :; do :; done' &
runner_pid=$!

for _ in {1..500}; do
    [[ "$(systemctl show "$scope_unit" --property=ActiveState --value 2>/dev/null || true)" == "active" ]] && break
    sleep 0.01
done
[[ "$(systemctl show "$scope_unit" --property=ActiveState --value)" == "active" ]] || die "scope did not become active"
[[ "$(systemctl show "$scope_unit" --property=Job --value)" =~ ^(0)?$ ]] || die "scope has a pending job"

control_group="$(systemctl show "$scope_unit" --property=ControlGroup --value)"
[[ "$control_group" == /* && "$control_group" != "/" && "$control_group" != *".."* ]] || die "unsafe scope cgroup path"
base="/sys/fs/cgroup${control_group}"
[[ -d "$base" && ! -L "$base" ]] || die "scope cgroup is missing or unsafe"
[[ -w "$base/cgroup.freeze" ]] || die "writable cgroup v2 freezer is required"
mapfile -t initial_members <"$base/cgroup.procs"
(( ${#initial_members[@]} == 1 )) || die "scope is not initially single-member"
worker_pid="${initial_members[0]}"
[[ "$worker_pid" =~ ^[1-9][0-9]*$ ]] || die "worker PID is malformed"
read -r worker_start _ < <(proc_values "$worker_pid") || die "could not capture worker identity"

mkdir -p -- "$dropin_dir"
cat >"$dropin_path" <<'EOF'
[Unit]
DefaultDependencies=no
RefuseManualStop=yes
IgnoreOnIsolate=yes
StopWhenUnneeded=no
BindsTo=
PartOf=
PropagatesStopTo=
Conflicts=
Upholds=
OnFailure=
OnSuccess=
FailureAction=none
SuccessAction=none
JobTimeoutAction=none

[Scope]
KillMode=process
SendSIGKILL=no
SendSIGHUP=no
OOMPolicy=continue
RuntimeMaxSec=infinity
TimeoutStopSec=infinity
EOF
chmod 0444 "$dropin_path"
systemctl daemon-reload
[[ "$(systemctl show "$scope_unit" --property=DefaultDependencies --value)" == "no" ]] || die "scope DefaultDependencies safety was not applied"
[[ "$(systemctl show "$scope_unit" --property=RefuseManualStop --value)" == "yes" ]] || die "scope manual-stop safety was not applied"
[[ "$(systemctl show "$scope_unit" --property=IgnoreOnIsolate --value)" == "yes" ]] || die "scope isolate safety was not applied"
[[ "$(systemctl show "$scope_unit" --property=KillMode --value)" == "process" ]] || die "scope KillMode safety was not applied"

# Freeze the parent first. Arm the child's *local* freeze request before moving
# the exact worker, then release the parent. This distinguishes an independent
# child barrier from an inherited frozen state.
printf '1' >"$base/cgroup.freeze"
wait_event "$base/cgroup.events" frozen 1 || die "parent cgroup did not freeze"
leaf="$base/arc-recovery-writer"
[[ ! -e "$leaf" && ! -L "$leaf" ]] || die "owned leaf path already exists"
mkdir -- "$leaf"
read -r leaf_device leaf_inode < <(stat -Lc '%d %i' "$leaf")
[[ "$leaf_device" =~ ^[0-9]+$ && "$leaf_inode" =~ ^[1-9][0-9]*$ ]] || die "owned leaf identity is malformed"
printf '1' >"$leaf/cgroup.freeze"
[[ "$(<"$leaf/cgroup.freeze")" == "1" ]] || die "owned leaf local freezer bit did not arm"
printf '%s' "$worker_pid" >"$leaf/cgroup.procs"
wait_event "$leaf/cgroup.events" frozen 1 || die "owned leaf did not become effectively frozen"
[[ "$(event_value "$leaf/cgroup.events" populated)" == "1" ]] || die "owned leaf is not populated"
mapfile -t leaf_members <"$leaf/cgroup.procs"
if (( ${#leaf_members[@]} != 1 )) || [[ "${leaf_members[0]}" != "$worker_pid" ]]; then
    die "owned leaf membership differs"
fi

printf '0' >"$base/cgroup.freeze"
wait_event "$base/cgroup.events" frozen 0 || die "parent cgroup did not release"
[[ "$(<"$leaf/cgroup.freeze")" == "1" ]] || die "owned leaf lost its local freezer bit"
wait_event "$leaf/cgroup.events" frozen 1 || die "owned leaf thawed with its parent"

read -r before_start before_ticks < <(proc_values "$worker_pid") || die "worker disappeared while frozen"
sleep 0.2
read -r stable_start stable_ticks < <(proc_values "$worker_pid") || die "worker disappeared while frozen"
[[ "$before_start" == "$worker_start" && "$stable_start" == "$worker_start" ]] || die "worker PID/start identity changed"
[[ "$before_ticks" == "$stable_ticks" ]] || die "worker CPU advanced while owned leaf was frozen"
read -r before_sigpnd before_shdpnd < <(awk '/^SigPnd:/ { p=$2 } /^ShdPnd:/ { s=$2 } END { print p, s }' "/proc/$worker_pid/status")

# Stopping the implicit Slice dependency makes PID 1 stop/thaw the scope. The
# parent is allowed to transition; the independently frozen owned leaf is not.
systemctl stop "$parent_slice"
[[ "$(systemctl show "$scope_unit" --property=ActiveState --value)" == "inactive" ]] || die "scope did not take the adversarial dependency stop"
[[ "$(systemctl show "$scope_unit" --property=SubState --value)" == "dead" ]] || die "scope did not reach dead substate"
wait_event "$base/cgroup.events" frozen 0 || die "scope parent is not thawed after dependency stop"
[[ "$(<"$leaf/cgroup.freeze")" == "1" ]] || die "owned leaf local freezer bit changed after dependency stop"
wait_event "$leaf/cgroup.events" frozen 1 || die "owned leaf thawed after dependency stop"
[[ "$(event_value "$leaf/cgroup.events" populated)" == "1" ]] || die "owned leaf emptied after dependency stop"
read -r current_device current_inode < <(stat -Lc '%d %i' "$leaf")
[[ "$current_device" == "$leaf_device" && "$current_inode" == "$leaf_inode" ]] || die "owned leaf inode changed"
mapfile -t leaf_members <"$leaf/cgroup.procs"
if (( ${#leaf_members[@]} != 1 )) || [[ "${leaf_members[0]}" != "$worker_pid" ]]; then
    die "owned leaf sole membership changed"
fi

read -r stopped_start stopped_ticks < <(proc_values "$worker_pid") || die "worker exited after dependency stop"
sleep 0.2
read -r stopped_start_2 stopped_ticks_2 < <(proc_values "$worker_pid") || die "worker exited after dependency stop"
[[ "$stopped_start" == "$worker_start" && "$stopped_start_2" == "$worker_start" ]] || die "worker identity changed after dependency stop"
[[ "$stopped_ticks" == "$stopped_ticks_2" ]] || die "worker CPU advanced after dependency stop"
read -r after_sigpnd after_shdpnd < <(awk '/^SigPnd:/ { p=$2 } /^ShdPnd:/ { s=$2 } END { print p, s }' "/proc/$worker_pid/status")
[[ "$after_sigpnd" == "$before_sigpnd" && "$after_shdpnd" == "$before_shdpnd" ]] || die "scope dependency stop injected a signal"

# Match the recovery controller: open the exact directory without following a
# symlink, re-check dev/inode on the fd, then openat/write the freezer file.
python3 - "$leaf" "$leaf_device" "$leaf_inode" <<'PY'
import os
import sys

path, expected_device, expected_inode = sys.argv[1], int(sys.argv[2]), int(sys.argv[3])
flags = os.O_RDONLY | getattr(os, "O_DIRECTORY", 0) | getattr(os, "O_NOFOLLOW", 0)
directory = os.open(path, flags)
try:
    details = os.fstat(directory)
    if (details.st_dev, details.st_ino) != (expected_device, expected_inode):
        raise SystemExit("opened owned leaf differs from captured dev/inode")
    freezer = os.open("cgroup.freeze", os.O_WRONLY | getattr(os, "O_NOFOLLOW", 0), dir_fd=directory)
    try:
        os.write(freezer, b"0")
    finally:
        os.close(freezer)
finally:
    os.close(directory)
PY
wait_event "$leaf/cgroup.events" frozen 0 || die "owned leaf did not directly thaw"
[[ "$(<"$leaf/cgroup.freeze")" == "0" ]] || die "owned leaf local freezer bit did not clear"

read -r resumed_start resumed_ticks < <(proc_values "$worker_pid") || die "worker exited during direct thaw"
sleep 0.2
read -r resumed_start_2 resumed_ticks_2 < <(proc_values "$worker_pid") || die "worker exited after direct thaw"
[[ "$resumed_start" == "$worker_start" && "$resumed_start_2" == "$worker_start" ]] || die "worker identity changed after direct thaw"
(( resumed_ticks_2 > resumed_ticks )) || die "worker CPU did not resume after direct thaw"

printf 'ok - systemd %s owned leaf survived parent stop; signals %s/%s; frozen ticks %s/%s; resumed ticks %s/%s\n' \
    "$systemd_version" "$after_sigpnd" "$after_shdpnd" \
    "$stopped_ticks" "$stopped_ticks_2" "$resumed_ticks" "$resumed_ticks_2"
