#!/usr/bin/env bash
# Test-only phase override for the archive dispatcher signal lifecycle contract.
# shellcheck disable=SC2034 # sourced cleanup consumes the four root variables

capture_phase() {
    begin_temporary_scope
    : "${ARC_SIGNAL_CASE_DIR:?}"
    : "${ARC_SIGNAL_PYTHON:?}"
    : "${ARC_SIGNAL_IDENTITY:?}"
    : "${ARC_SIGNAL_RCLONE_CONFIG:?}"
    : "${ARC_SIGNAL_FOREGROUND_IGNORES:=false}"
    : "${ARC_SIGNAL_BACKGROUND_RESURRECTS:=false}"
    : "${ARC_SIGNAL_CLEANUP_DELAY_SECONDS:=0}"

    local scratch_root pinned_root transport_root python_root
    local phase_pid phase_pgid background_pid
    scratch_root="$(mktemp -d "$TMPDIR/scratch.XXXXXX")"
    ARCHIVE_FLEET_TEMP_ROOT="$scratch_root"
    pinned_root="$(mktemp -d "$TMPDIR/pinned.XXXXXX")"
    ARCHIVE_FLEET_PINNED_ROOT="$pinned_root"
    transport_root="$(mktemp -d "$TMPDIR/transport.XXXXXX")"
    ARCHIVE_FLEET_PINNED_TRANSPORT_ROOT="$transport_root"
    python_root="$(mktemp -d "$TMPDIR/python-home.XXXXXX")"
    ARCHIVE_FLEET_PINNED_PYTHON_ROOT="$python_root"
    chmod 700 "$scratch_root" "$pinned_root" "$transport_root" "$python_root"
    cp "$ARC_SIGNAL_IDENTITY" "$transport_root/id_ed25519"
    cp "$ARC_SIGNAL_RCLONE_CONFIG" "$transport_root/rclone.conf"
    chmod 400 "$transport_root/id_ed25519"
    chmod 600 "$transport_root/rclone.conf"
    printf 'simulated-refresh\n' >> "$transport_root/rclone.conf"
    (umask 077; printf 'scratch\t%s\npinned\t%s\ntransport\t%s\npython\t%s\n' \
        "$scratch_root" "$pinned_root" "$transport_root" "$python_root" \
        > "$ARC_SIGNAL_CASE_DIR/roots.tsv.partial")
    /bin/mv -f "$ARC_SIGNAL_CASE_DIR/roots.tsv.partial" "$ARC_SIGNAL_CASE_DIR/roots.tsv"

    archive_signal_probe_cleanup() {
        local exit_status="$?" cleanup_failed=0 path
        (umask 077; printf 'cleanup-started exit=%s\n' "$exit_status" \
            > "$ARC_SIGNAL_CASE_DIR/cleanup.started.partial")
        /bin/mv -f "$ARC_SIGNAL_CASE_DIR/cleanup.started.partial" \
            "$ARC_SIGNAL_CASE_DIR/cleanup.started"
        if [ "$ARC_SIGNAL_CLEANUP_DELAY_SECONDS" != 0 ]; then
            /bin/sleep "$ARC_SIGNAL_CLEANUP_DELAY_SECONDS"
        fi
        cleanup_temporary_root || cleanup_failed=1
        if [ "$ARC_SIGNAL_BACKGROUND_RESURRECTS" = true ]; then
            /bin/sleep 0.5
        fi
        while IFS=$'\t' read -r _ path; do
            [ ! -e "$path" ] && [ ! -L "$path" ] || cleanup_failed=1
        done < "$ARC_SIGNAL_CASE_DIR/roots.tsv"
        if [ "$cleanup_failed" -eq 0 ]; then
            (umask 077; printf 'cleanup-complete exit=%s\n' "$exit_status" \
                > "$ARC_SIGNAL_CASE_DIR/cleanup.complete.partial")
            /bin/mv -f "$ARC_SIGNAL_CASE_DIR/cleanup.complete.partial" \
                "$ARC_SIGNAL_CASE_DIR/cleanup.complete"
        else
            (umask 077; printf 'cleanup-failed exit=%s\n' "$exit_status" \
                > "$ARC_SIGNAL_CASE_DIR/cleanup.failed")
        fi
        return "$exit_status"
    }
    trap archive_signal_probe_cleanup EXIT

    archive_write_current_process_id "$ARC_SIGNAL_CASE_DIR/phase.pid.partial"
    IFS= read -r phase_pid < "$ARC_SIGNAL_CASE_DIR/phase.pid.partial"
    phase_pgid="$(archive_process_field pgid "$phase_pid")"
    (umask 077; printf '%s\t%s\n' "$phase_pid" "$phase_pgid" \
        > "$ARC_SIGNAL_CASE_DIR/phase.info.partial")
    /bin/mv -f "$ARC_SIGNAL_CASE_DIR/phase.info.partial" "$ARC_SIGNAL_CASE_DIR/phase.info"

    "$ARC_SIGNAL_PYTHON" -c '
import os, pathlib, signal, sys, time
def stop(signum, _frame):
    raise SystemExit(128 + signum)
resurrect = sys.argv[2] == "true"
handler = signal.SIG_IGN if resurrect else stop
for item in (signal.SIGHUP, signal.SIGINT, signal.SIGTERM):
    signal.signal(item, handler)
path = pathlib.Path(sys.argv[1]); partial = path.with_name(path.name + ".partial")
partial.write_text(f"{os.getpid()}\t{os.getpgrp()}\n", encoding="utf-8")
os.chmod(partial, 0o600); os.replace(partial, path)
if resurrect:
    root = pathlib.Path(sys.argv[3]); marker = pathlib.Path(sys.argv[4])
    while root.exists() or root.is_symlink():
        time.sleep(0.001)
    root.mkdir(mode=0o700)
    (root / "resurrected-secret").write_text("must be swept\n", encoding="utf-8")
    marker.write_text("resurrection-attempted\n", encoding="utf-8")
    raise SystemExit(0)
while True:
    time.sleep(1)
' "$ARC_SIGNAL_CASE_DIR/background.info" "$ARC_SIGNAL_BACKGROUND_RESURRECTS" \
    "$scratch_root" "$ARC_SIGNAL_CASE_DIR/resurrection.attempted" &
    background_pid="$!"

    "$ARC_SIGNAL_PYTHON" -c '
import os, pathlib, signal, sys, time
def stop(signum, _frame):
    raise SystemExit(128 + signum)
handler = signal.SIG_IGN if sys.argv[2] == "true" else stop
for item in (signal.SIGHUP, signal.SIGINT, signal.SIGTERM):
    signal.signal(item, handler)
path = pathlib.Path(sys.argv[1]); partial = path.with_name(path.name + ".partial")
partial.write_text(f"{os.getpid()}\t{os.getpgrp()}\n", encoding="utf-8")
os.chmod(partial, 0o600); os.replace(partial, path)
while True:
    time.sleep(1)
' "$ARC_SIGNAL_CASE_DIR/foreground.info" "$ARC_SIGNAL_FOREGROUND_IGNORES"
    wait "$background_pid"
}

if [ "${ARC_SIGNAL_GATE_REMOVE_FAIL_ONCE:-false}" = true ]; then
    # Force the dispatcher's first authoritative sweep to fail. The
    # cleanup-only guardian must acknowledge ownership before the parent may
    # return, then retry until the exact private gate is absent.
    archive_remove_dispatch_gate() {
        local gate="$1"
        if mkdir "$ARC_SIGNAL_CASE_DIR/gate-remove-failed-once" 2>/dev/null; then
            return 1
        fi
        rm -rf -- "$gate" 2>/dev/null || true
        [ ! -e "$gate" ] && [ ! -L "$gate" ]
    }
fi

if [ "${ARC_SIGNAL_SENTINEL_MOVE_FAIL_ONCE:-false}" = true ]; then
    # Model the guardian killing the sentinel's external mv child during
    # escalation. The sentinel shell must survive the 137 and retry through the
    # existing FINALIZE handshake instead of abandoning the credential gate.
    archive_sentinel_atomic_move() {
        local source="$1" destination="$2"
        if [ "${ARC_SIGNAL_SENTINEL_MOVE_HAS_FAILED:-false}" = false ] && \
            [ "${destination##*/}" = sentinel.finalize.ack ]; then
            ARC_SIGNAL_SENTINEL_MOVE_HAS_FAILED=true
            (umask 077; printf 'injected move exit 137\n' \
                > "$ARC_SIGNAL_CASE_DIR/sentinel-move-failed-once")
            return 137
        fi
        /bin/mv -f "$source" "$destination"
    }
fi
