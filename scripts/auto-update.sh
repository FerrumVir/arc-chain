#!/usr/bin/env bash
# ─────────────────────────────────────────────────────────────────────────────
# ARC Chain - Auto-Update Daemon
#
# Checks GitHub releases every 10 minutes. When a new version is available,
# downloads the pre-built binary and gracefully restarts the node.
#
# Usage:
#   ./scripts/auto-update.sh                    # Foreground daemon
#   ./scripts/auto-update.sh --once             # Check once and exit
#   nohup ./scripts/auto-update.sh &            # Background daemon
#
# The daemon only restarts the node if you originally started it via
# join-inference.sh or install-node.sh - otherwise it just logs the
# available update and lets you restart manually.
# ─────────────────────────────────────────────────────────────────────────────
set -euo pipefail

REPO="FerrumVir/arc-chain"
CHECK_INTERVAL="${ARC_UPDATE_INTERVAL:-600}"  # 10 minutes
LOG_FILE="${HOME}/.arc-chain/auto-update.log"
ARC_HOME="${HOME}/.arc-chain"
REPO_DIR="${ARC_HOME}/arc-chain"

mkdir -p "$ARC_HOME"

log() {
    local msg="[$(date '+%Y-%m-%d %H:%M:%S')] $*"
    # Write to stderr (not stdout) so function return values via echo are clean
    echo "$msg" >&2
    echo "$msg" >> "$LOG_FILE"
}

detect_platform() {
    local os arch
    os="$(uname -s)"
    arch="$(uname -m)"
    case "$os" in
        Linux)
            case "$arch" in
                x86_64|amd64) echo "linux-x86_64" ;;
                aarch64|arm64) echo "linux-aarch64" ;;
                *) echo "unsupported"; return 1 ;;
            esac
            ;;
        Darwin)
            case "$arch" in
                arm64) echo "macos-arm64" ;;
                x86_64) echo "macos-x86_64" ;;
                *) echo "unsupported"; return 1 ;;
            esac
            ;;
        *) echo "unsupported"; return 1 ;;
    esac
}

get_installed_version() {
    # Try common binary locations
    local bin=""
    for candidate in "$REPO_DIR/target/release/arc-node" "$ARC_HOME/bin/arc-node" "$(command -v arc-node 2>/dev/null)"; do
        if [ -x "$candidate" ]; then
            bin="$candidate"
            break
        fi
    done
    if [ -z "$bin" ]; then
        echo "none"
        return
    fi
    "$bin" --version 2>/dev/null | grep -oE '[0-9]+\.[0-9]+\.[0-9]+' | head -1 || echo "unknown"
}

get_latest_version() {
    # GitHub releases API - no auth needed for public repos
    curl -sf "https://api.github.com/repos/${REPO}/releases/latest" 2>/dev/null \
        | grep -m1 '"tag_name"' \
        | sed -E 's/.*"v?([0-9]+\.[0-9]+\.[0-9]+)".*/\1/'
}

version_gt() {
    # Returns 0 if $1 > $2
    [ "$1" != "$2" ] && [ "$(printf '%s\n' "$1" "$2" | sort -V | tail -1)" = "$1" ]
}

download_binary() {
    local version="$1"
    local platform="$2"
    local asset="arc-node-${platform}"
    local url="https://github.com/${REPO}/releases/download/v${version}/${asset}"
    local tmpfile="/tmp/arc-node-update-$$"
    log "Downloading $asset from v$version..."
    if curl -sfL -o "$tmpfile" "$url"; then
        chmod +x "$tmpfile"
        echo "$tmpfile"
        return 0
    else
        rm -f "$tmpfile"
        log "  Download failed: $url"
        return 1
    fi
}

find_running_node_pid() {
    # Find arc-node process started by the user
    pgrep -f 'arc-node.*(--validator-seed|--seeds-file)' 2>/dev/null | head -1
}

restart_node() {
    local new_bin="$1"
    local pid
    pid="$(find_running_node_pid)"
    if [ -z "$pid" ]; then
        log "  No running arc-node process found - skipping restart"
        return 1
    fi

    # Capture the running command line
    local cmdline
    if [ "$(uname -s)" = "Linux" ]; then
        cmdline="$(tr '\0' ' ' < /proc/$pid/cmdline 2>/dev/null)"
    else
        cmdline="$(ps -p "$pid" -o command= 2>/dev/null)"
    fi

    if [ -z "$cmdline" ]; then
        log "  Couldn't read cmdline for pid $pid - skipping restart"
        return 1
    fi

    # Extract the binary path (first word)
    local old_bin
    old_bin="$(echo "$cmdline" | awk '{print $1}')"
    local args
    args="$(echo "$cmdline" | cut -d' ' -f2-)"

    log "  Stopping old node (pid $pid)..."
    kill "$pid" 2>/dev/null || true

    # Wait up to 10s for graceful shutdown
    for _ in 1 2 3 4 5 6 7 8 9 10; do
        kill -0 "$pid" 2>/dev/null || break
        sleep 1
    done
    kill -9 "$pid" 2>/dev/null || true

    # Replace binary
    log "  Installing new binary to $old_bin..."
    cp "$new_bin" "$old_bin"
    chmod +x "$old_bin"

    # Restart
    log "  Starting new node..."
    nohup "$old_bin" $args > "${ARC_HOME}/node.log" 2>&1 &
    local new_pid=$!
    log "  New node started (pid $new_pid)"
    return 0
}

check_and_update() {
    local platform
    platform="$(detect_platform)" || { log "Unsupported platform"; return 1; }

    local current latest
    current="$(get_installed_version)"
    latest="$(get_latest_version)"

    if [ -z "$latest" ]; then
        log "Could not fetch latest version from GitHub"
        return 1
    fi

    log "Installed: v$current  | Latest: v$latest"

    if [ "$current" = "$latest" ]; then
        log "Already up to date."
        return 0
    fi

    if [ "$current" != "none" ] && [ "$current" != "unknown" ]; then
        if ! version_gt "$latest" "$current"; then
            log "Installed version is newer or equal - skipping"
            return 0
        fi
    fi

    log "New version available: v$latest (have v$current)"

    local new_bin
    if ! new_bin="$(download_binary "$latest" "$platform")"; then
        return 1
    fi

    if restart_node "$new_bin"; then
        log "Updated to v$latest successfully"
        rm -f "$new_bin"
    else
        log "Update downloaded to $new_bin but node was not restarted (not running or unknown cmdline)"
    fi
}

# ── Main ────────────────────────────────────────────────────────────────────
ONCE=false
for arg in "$@"; do
    case "$arg" in
        --once) ONCE=true ;;
    esac
done

log "ARC auto-update daemon starting (interval: ${CHECK_INTERVAL}s, repo: $REPO)"

if $ONCE; then
    check_and_update
    exit 0
fi

while true; do
    check_and_update || true
    sleep "$CHECK_INTERVAL"
done
