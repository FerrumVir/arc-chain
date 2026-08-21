#!/usr/bin/env bash
# ─────────────────────────────────────────────────────────────────────────────
# Relaunch arc-node ON THIS HOST, preserving exactly what was running.
#
# Piped to a seed over ssh by arc-rolling-restart.sh. Runs nowhere else.
#
# WHY NOT `systemctl restart arc-node`
#
# Verified on AMS, 2026-08-21: arc-node is NOT managed by systemd on these
# seeds. `systemctl is-active arc-node` reports **inactive** with MainPID 0,
# while the real process runs from /root/arc-chain, started by hand and
# re-parented to PID 1.
#
# The unit file on disk is also materially different from the live process:
#
#   unit ExecStart : --validator-seed "arc-node-ams" --seeds-file /etc/arc/seeds.txt
#                    --data-dir /var/lib/arc/data --archive --stake 5000000
#   live argv      : --rpc 0.0.0.0:9090 --p2p-port 9091 --validator-seed AMS
#                    --seeds-file testnet-seeds.txt --genesis genesis.toml
#                    --stake 5000000 --eth-rpc-port 0 --model llama2-7b.gguf
#                    --shard-range 0:6 --shard-range 6:12 --shard-range 12:17
#
# The unit has no --genesis (so seed_genesis_validators would be skipped and the
# validator divergence would return), no --model or --shard-range (so the node
# would stop serving its three shard ranges and break the 3x-replicated
# pipeline), and a different --data-dir. Running `systemctl restart` would start
# that second, wrongly-configured node ALONGSIDE the live one, both contending
# for :9090.
#
# So this mirrors what arc-self-heal.sh already does successfully on these
# hosts: capture argv + environment + cwd from /proc, hard-kill, re-exec.
#
# Exits non-zero without touching anything if the live argv lacks --genesis.
# ─────────────────────────────────────────────────────────────────────────────

set -u

pid=$(pgrep -x arc-node | head -1)
if [ -z "$pid" ]; then
    echo "RELAUNCH_FAIL: no live arc-node process to capture argv from"
    exit 1
fi

cwd=$(readlink -f "/proc/$pid/cwd" 2>/dev/null)
cmdline=$(tr '\0' ' ' < "/proc/$pid/cmdline" 2>/dev/null)
env_lines=$(tr '\0' '\n' < "/proc/$pid/environ" 2>/dev/null \
    | grep -E '^(ARC_|PATH=|HOME=|USER=|LANG=)')

if [ -z "$cmdline" ] || [ -z "$cwd" ]; then
    echo "RELAUNCH_FAIL: could not read cmdline/cwd for pid $pid"
    exit 1
fi

# The whole point of the restart is that seed_genesis_validators() runs at boot
# and reseeds the validator set from genesis. Without --genesis it is skipped
# and the divergence returns within minutes. Refuse rather than waste the roll.
case "$cmdline" in
    *--genesis*) ;;
    *)
        echo "RELAUNCH_FAIL: live argv has no --genesis; restarting would reproduce the divergence"
        echo "ARGV: $cmdline"
        exit 1
        ;;
esac

echo "CAPTURED_CWD=$cwd"
echo "CAPTURED_ARGV=$cmdline"

# -9 because a clean shutdown would hang on the same path that stalled block
# production. -x matches comm="arc-node" exactly, never an argv substring, so
# this cannot kill our own ssh command or the self-heal daemon.
pkill -9 -x arc-node 2>/dev/null || true
sleep 3

if pgrep -x arc-node >/dev/null 2>&1; then
    echo "RELAUNCH_FAIL: arc-node still alive after SIGKILL"
    exit 1
fi

boot_log="${cwd}/rolling-restart-boot.log"
(
    while IFS= read -r kv; do
        [ -z "$kv" ] && continue
        export "${kv?}"
    done <<< "$env_lines"
    cd "$cwd" || exit 1
    # setsid + nohup + disown: screen-based launches drop silently from a
    # non-tty ssh session.
    setsid nohup bash -c "exec $cmdline" </dev/null >>"$boot_log" 2>&1 &
    disown
)

sleep 2
newpid=$(pgrep -x arc-node | head -1)
if [ -z "$newpid" ]; then
    echo "RELAUNCH_FAIL: no arc-node process after relaunch; see $boot_log"
    exit 1
fi

echo "RELAUNCH_OK newpid=$newpid"
