#!/usr/bin/env bash
# ARC Chain headless/server installer for Linux and macOS.
#
# Security and upgrade contract:
#   - resolve one immutable vX.Y.Z release (never walk back to an older tag)
#   - require the node, CLI, installer, and network config in that release
#   - verify every downloaded file against that release's SHA256SUMS
#   - keep validator identity out of argv and preserve it across upgrades
#   - refuse downgrades and unknown installed versions
#   - run stake-0 community mode only
set -Eeuo pipefail
umask 077

REPOSITORY="${ARC_REPOSITORY:-FerrumVir/arc-chain}"
API_ROOT="${ARC_GITHUB_API_ROOT:-https://api.github.com/repos/${REPOSITORY}}"
DOWNLOAD_ROOT="${ARC_GITHUB_DOWNLOAD_ROOT:-https://github.com/${REPOSITORY}/releases/download}"

# Community/reward RPC is explicit HTTPS configuration, separate from QUIC
# P2P discovery. Every managed node receives the same reviewed six origins;
# raw remote HTTP is deliberately not configurable through the installer.
COMMUNITY_RPC_ORIGINS=(
    https://149-28-32-76.nip.io
    https://140-82-16-112.nip.io
    https://136-244-109-1.nip.io
    https://104-238-171-11.nip.io
    https://202-182-107-41.nip.io
    https://149-28-153-31.nip.io
)

REQUESTED_VERSION="${ARC_NODE_VERSION:-}"
ARG_INSTALL_DIR=""
ARG_DATA_DIR=""
ARG_MODEL_PATH=""
ARG_RPC_PORT=""
ARG_P2P_PORT=""
ARG_SERVICE_SCOPE=""
MODEL_WAS_SET=false
RPC_WAS_SET=false
P2P_WAS_SET=false
DATA_WAS_SET=false
SERVICE_WAS_SET=false
INSTALL_SERVICE=true
INSTALL_UPDATER=true
UPDATE_ONLY=false
UNINSTALL=false
PURGE=false

if [ "${ARC_MODEL_PATH+x}" = x ]; then MODEL_WAS_SET=true; fi
if [ "${ARC_RPC_PORT+x}" = x ]; then RPC_WAS_SET=true; fi
if [ "${ARC_P2P_PORT+x}" = x ]; then P2P_WAS_SET=true; fi
if [ "${ARC_NODE_DATA_DIR+x}" = x ]; then DATA_WAS_SET=true; fi
if [ "${ARC_INSTALL_SCOPE+x}" = x ]; then SERVICE_WAS_SET=true; fi

if [ -n "${NO_COLOR:-}" ]; then
    BOLD='' RED='' GREEN='' YELLOW='' CYAN='' RESET=''
else
    BOLD='\033[1m' RED='\033[31m' GREEN='\033[32m'
    YELLOW='\033[33m' CYAN='\033[36m' RESET='\033[0m'
fi

info() { printf '%b[INFO]%b %s\n' "$CYAN" "$RESET" "$*"; }
ok() { printf '%b[ OK ]%b %s\n' "$GREEN" "$RESET" "$*"; }
warn() { printf '%b[WARN]%b %s\n' "$YELLOW" "$RESET" "$*" >&2; }
die() { printf '%b[FAIL]%b %s\n' "$RED" "$RESET" "$*" >&2; exit 1; }

usage() {
    cat <<'EOF'
ARC Chain headless/server installer

Usage:
  bash install.sh [options]

Options:
  --version X.Y.Z        Install an exact release (or ARC_NODE_VERSION).
  --install-dir PATH     Program/config root (default: ~/.arc).
  --data-dir PATH        Chain data directory (default: INSTALL_DIR/data).
  --model PATH           Optional local GGUF model. Omitted means observer/router.
  --port PORT            RPC port (default: 9944).
  --p2p-port PORT        QUIC P2P port (default: RPC port + 1).
  --system-service       Linux system service (sudo/root required).
  --user-service         Linux per-user systemd service.
  --no-service           Install only; do not start or health-check a node.
  --no-auto-update       Do not install the daily checksummed updater.
  --update-only          Update an existing install; used by the timer.
  --uninstall            Remove services and programs; keep identity and data.
  --purge                With --uninstall, also remove identity and chain data.
  -h, --help             Show this help.

Environment equivalents:
  ARC_NODE_VERSION, ARC_DIR, ARC_NODE_DATA_DIR, ARC_MODEL_PATH,
  ARC_RPC_PORT, ARC_P2P_PORT, ARC_INSTALL_SCOPE (auto/system/user).

Windows Server is supported by release binaries but not by this shell script.
Download arc-node-windows-x86_64.exe, arc-cli-windows-x86_64.exe, and
SHA256SUMS from the same release and verify them manually.
EOF
}

need_value() {
    [ "$#" -ge 2 ] && [ -n "$2" ] || die "$1 requires a value"
}

while [ "$#" -gt 0 ]; do
    case "$1" in
        --version)
            need_value "$@"; REQUESTED_VERSION="$2"; shift 2 ;;
        --install-dir)
            need_value "$@"; ARG_INSTALL_DIR="$2"; shift 2 ;;
        --data-dir)
            need_value "$@"; ARG_DATA_DIR="$2"; DATA_WAS_SET=true; shift 2 ;;
        --model)
            need_value "$@"; ARG_MODEL_PATH="$2"; MODEL_WAS_SET=true; shift 2 ;;
        --port)
            need_value "$@"; ARG_RPC_PORT="$2"; RPC_WAS_SET=true; shift 2 ;;
        --p2p-port)
            need_value "$@"; ARG_P2P_PORT="$2"; P2P_WAS_SET=true; shift 2 ;;
        --system-service)
            ARG_SERVICE_SCOPE=system; SERVICE_WAS_SET=true; shift ;;
        --user-service)
            ARG_SERVICE_SCOPE=user; SERVICE_WAS_SET=true; shift ;;
        --service-scope)
            need_value "$@"; ARG_SERVICE_SCOPE="$2"; SERVICE_WAS_SET=true; shift 2 ;;
        --no-service)
            INSTALL_SERVICE=false; shift ;;
        --no-auto-update)
            INSTALL_UPDATER=false; shift ;;
        --update-only)
            UPDATE_ONLY=true; shift ;;
        --uninstall)
            UNINSTALL=true; shift ;;
        --purge)
            PURGE=true; shift ;;
        -h|--help)
            usage; exit 0 ;;
        *)
            die "Unknown option: $1 (run with --help)" ;;
    esac
done

for command_name in curl awk sed grep mktemp uname id chmod mkdir cp mv dirname stat; do
    command -v "$command_name" >/dev/null 2>&1 \
        || die "Required command is missing: $command_name"
done

OS="$(uname -s)"
ARCH="$(uname -m)"
case "$OS:$ARCH" in
    Linux:x86_64|Linux:amd64)
        PLATFORM=linux-x86_64 ;;
    Linux:aarch64|Linux:arm64)
        PLATFORM=linux-arm64 ;;
    Darwin:arm64)
        PLATFORM=macos-arm64 ;;
    Darwin:x86_64)
        PLATFORM=macos-x86_64 ;;
    Darwin:*)
        die "Unsupported macOS architecture '$ARCH'. Releases support arm64 and x86_64." ;;
    Linux:*)
        die "Unsupported Linux architecture '$ARCH'. Releases support x86_64/amd64 and arm64/aarch64." ;;
    *)
        die "Unsupported operating system '$OS'. This installer supports Linux and macOS." ;;
esac

CURRENT_UID="$(id -u)"
CURRENT_USER="$(id -un)"
TARGET_USER="${ARC_TARGET_USER:-}"
if [ -z "$TARGET_USER" ]; then
    if [ "$CURRENT_UID" -eq 0 ] && [ -n "${SUDO_USER:-}" ] && [ "${SUDO_USER:-}" != root ]; then
        TARGET_USER="$SUDO_USER"
    else
        TARGET_USER="$CURRENT_USER"
    fi
fi
id "$TARGET_USER" >/dev/null 2>&1 || die "Install user does not exist: $TARGET_USER"
TARGET_UID="$(id -u "$TARGET_USER")"
TARGET_GROUP="$(id -gn "$TARGET_USER")"

if [ "$CURRENT_UID" -ne 0 ] && [ "$TARGET_USER" != "$CURRENT_USER" ]; then
    die "A non-root installer cannot install files for another user ($TARGET_USER)."
fi
if [ "$OS" = Darwin ] && [ "$CURRENT_UID" -eq 0 ]; then
    die "Do not run the macOS installer with sudo. Run it as the login user so launchd and file ownership are correct."
fi

if [ "$TARGET_USER" = "$CURRENT_USER" ]; then
    TARGET_HOME="$HOME"
elif command -v getent >/dev/null 2>&1; then
    TARGET_HOME="$(getent passwd "$TARGET_USER" | awk -F: '{print $6}')"
else
    TARGET_HOME=""
fi
[ -n "$TARGET_HOME" ] && [ -d "$TARGET_HOME" ] \
    || die "Could not determine a home directory for $TARGET_USER"

DEFAULT_ARC_DIR="$TARGET_HOME/.arc"
SCOPE_HINT="${ARG_SERVICE_SCOPE:-${ARC_INSTALL_SCOPE:-}}"
if [ "$INSTALL_SERVICE" = false ]; then
    SCOPE_HINT=none
elif [ -z "$SCOPE_HINT" ] || [ "$SCOPE_HINT" = auto ]; then
    if [ "$OS" = Darwin ]; then SCOPE_HINT=launchd
    elif [ "$CURRENT_UID" -eq 0 ]; then SCOPE_HINT=system
    else SCOPE_HINT=user
    fi
fi
if [ "$OS" = Linux ] && [ "$SCOPE_HINT" = system ]; then
    # A root updater must not execute binaries from a user-renamable home
    # directory. System installs therefore default beneath a root-owned parent.
    DEFAULT_ARC_DIR=/var/lib/arc-chain
fi

if [ -n "$ARG_INSTALL_DIR" ]; then
    ARC_DIR="$ARG_INSTALL_DIR"
else
    ARC_DIR="${ARC_DIR:-$DEFAULT_ARC_DIR}"
fi
case "$ARC_DIR" in
    /*) ;;
    *) die "--install-dir/ARC_DIR must be an absolute path (got: $ARC_DIR)" ;;
esac
case "$ARC_DIR" in
    /|/bin|/etc|/usr|/var|/opt|"$TARGET_HOME")
        die "Refusing unsafe install directory: $ARC_DIR" ;;
esac
case "$ARC_DIR" in *$'\n'*|*$'\r'*) die "Install paths may not contain newlines" ;; esac

# Read only a strict key/value data file. Never source user-writable config:
# the system updater runs as root and sourcing it would be privilege escalation.
CONFIG_FILE="$ARC_DIR/install.conf"
SAVED_RPC_PORT=""
SAVED_P2P_PORT=""
SAVED_DATA_DIR=""
SAVED_MODEL_PATH=""
SAVED_SERVICE_SCOPE=""
if [ -f "$CONFIG_FILE" ]; then
    [ ! -L "$CONFIG_FILE" ] || die "Refusing symlinked installer config: $CONFIG_FILE"
    while IFS='=' read -r key value; do
        case "$key" in
            version) : ;; # Informational; the executable is authoritative.
            rpc_port) SAVED_RPC_PORT="$value" ;;
            p2p_port) SAVED_P2P_PORT="$value" ;;
            data_dir) SAVED_DATA_DIR="$value" ;;
            model_path) SAVED_MODEL_PATH="$value" ;;
            service_scope) SAVED_SERVICE_SCOPE="$value" ;;
            ''|'#'*) ;;
            *) die "Unknown key '$key' in $CONFIG_FILE" ;;
        esac
    done < "$CONFIG_FILE"
fi

if [ "$RPC_WAS_SET" = true ]; then
    RPC_PORT="${ARG_RPC_PORT:-${ARC_RPC_PORT:-}}"
else
    RPC_PORT="${SAVED_RPC_PORT:-9944}"
fi
if [ "$P2P_WAS_SET" = true ]; then
    P2P_PORT="${ARG_P2P_PORT:-${ARC_P2P_PORT:-}}"
else
    P2P_PORT="${SAVED_P2P_PORT:-}"
fi
if [ -z "$P2P_PORT" ]; then
    [ "$RPC_PORT" -lt 65535 ] 2>/dev/null \
        || die "RPC port 65535 requires an explicit --p2p-port"
    P2P_PORT=$((RPC_PORT + 1))
fi

if [ "$DATA_WAS_SET" = true ]; then
    NODE_DATA_DIR="${ARG_DATA_DIR:-${ARC_NODE_DATA_DIR:-}}"
else
    NODE_DATA_DIR="${SAVED_DATA_DIR:-$ARC_DIR/data}"
fi
if [ "$MODEL_WAS_SET" = true ]; then
    MODEL_PATH="${ARG_MODEL_PATH:-${ARC_MODEL_PATH:-}}"
else
    MODEL_PATH="$SAVED_MODEL_PATH"
fi

if [ "$SERVICE_WAS_SET" = true ]; then
    SERVICE_SCOPE="${ARG_SERVICE_SCOPE:-${ARC_INSTALL_SCOPE:-}}"
elif [ -n "$SAVED_SERVICE_SCOPE" ]; then
    SERVICE_SCOPE="$SAVED_SERVICE_SCOPE"
elif [ "$OS" = Darwin ]; then
    SERVICE_SCOPE=launchd
elif [ "$CURRENT_UID" -eq 0 ]; then
    SERVICE_SCOPE=system
else
    SERVICE_SCOPE=user
fi

if [ "$INSTALL_SERVICE" = false ]; then
    SERVICE_SCOPE=none
    if [ "$INSTALL_UPDATER" = true ]; then
        warn "--no-service also disables scheduled auto-update; use the installed arc-installer manually."
        INSTALL_UPDATER=false
    fi
fi

case "$SERVICE_SCOPE" in
    auto)
        if [ "$OS" = Darwin ]; then SERVICE_SCOPE=launchd
        elif [ "$CURRENT_UID" -eq 0 ]; then SERVICE_SCOPE=system
        else SERVICE_SCOPE=user
        fi ;;
    system)
        [ "$OS" = Linux ] || die "--system-service is supported only on Linux" ;;
    user)
        [ "$OS" = Linux ] || die "--user-service is supported only on Linux"
        [ "$CURRENT_UID" -ne 0 ] \
            || die "For a Linux user service, rerun without sudo as $TARGET_USER" ;;
    launchd)
        [ "$OS" = Darwin ] || die "launchd service scope is supported only on macOS" ;;
    none) ;;
    *) die "Service scope must be auto, system, user, or none (got: $SERVICE_SCOPE)" ;;
esac

if [ "$SERVICE_SCOPE" = system ]; then
    case "$ARC_DIR" in
        "$TARGET_HOME"|"$TARGET_HOME"/*|/tmp|/tmp/*|/var/tmp|/var/tmp/*)
            die "A root auto-updater cannot safely execute from a user-renamable path ($ARC_DIR). Use the default /var/lib/arc-chain or another root-owned parent." ;;
    esac
fi

valid_port() {
    case "$1" in ''|*[!0-9]*) return 1 ;; esac
    [ "$1" -ge 1 ] && [ "$1" -le 65535 ]
}
valid_port "$RPC_PORT" || die "Invalid RPC port: $RPC_PORT"
valid_port "$P2P_PORT" || die "Invalid P2P port: $P2P_PORT"
[ "$RPC_PORT" != "$P2P_PORT" ] || die "RPC and P2P ports must be different"

for path_value in "$NODE_DATA_DIR" "$MODEL_PATH"; do
    [ -z "$path_value" ] && continue
    case "$path_value" in
        /*) ;;
        *) die "Data/model paths must be absolute (got: $path_value)" ;;
    esac
    case "$path_value" in *$'\n'*|*$'\r'*) die "Data/model paths may not contain newlines" ;; esac
done
[ -z "$MODEL_PATH" ] || [ -f "$MODEL_PATH" ] \
    || die "Model file does not exist: $MODEL_PATH"

as_root() {
    if [ "$CURRENT_UID" -eq 0 ]; then
        "$@"
    elif command -v sudo >/dev/null 2>&1; then
        sudo "$@"
    else
        die "System service installation requires root or sudo. Use --user-service instead."
    fi
}

as_target() {
    if [ "$TARGET_USER" = "$CURRENT_USER" ]; then
        "$@"
    elif [ "$CURRENT_UID" -eq 0 ] && command -v runuser >/dev/null 2>&1; then
        runuser -u "$TARGET_USER" -- "$@"
    elif [ "$CURRENT_UID" -eq 0 ] && command -v sudo >/dev/null 2>&1; then
        sudo -u "$TARGET_USER" -- "$@"
    else
        die "Cannot validate file access as install user $TARGET_USER (runuser/sudo is unavailable)."
    fi
}

set_target_owner() {
    if [ "$CURRENT_UID" -eq 0 ]; then
        chown "$TARGET_USER:$TARGET_GROUP" "$@"
    fi
}

ensure_private_dir() {
    mkdir -p -- "$1"
    chmod 700 "$1"
    set_target_owner "$1"
}

case "$SERVICE_SCOPE" in
    system)
        command -v systemctl >/dev/null 2>&1 \
            || die "systemctl is unavailable; use --no-service for install-only mode"
        if [ "$CURRENT_UID" -ne 0 ]; then
            command -v sudo >/dev/null 2>&1 \
                || die "A system service requires sudo/root; use --user-service instead"
            sudo -v || die "sudo authorization failed; use --user-service or --no-service"
        fi ;;
    user)
        command -v systemctl >/dev/null 2>&1 \
            || die "systemctl is unavailable; use --no-service for install-only mode"
        systemctl --user show-environment >/dev/null 2>&1 \
            || die "No systemd user manager is reachable. Use sudo --system-service, or --no-service." ;;
    launchd)
        command -v launchctl >/dev/null 2>&1 || die "launchctl is unavailable" ;;
esac

if [ "$PURGE" = true ] && [ "$UNINSTALL" = false ]; then
    die "--purge is valid only with --uninstall"
fi

uninstall_arc() {
    info "Removing ARC services and installed programs"
    case "$SERVICE_SCOPE" in
        system)
            as_root systemctl disable --now arc-node.timer arc-node-update.timer arc-node.service 2>/dev/null || true
            as_root rm -f -- \
                /etc/systemd/system/arc-node.service \
                /etc/systemd/system/arc-node-update.service \
                /etc/systemd/system/arc-node-update.timer
            as_root systemctl daemon-reload ;;
        user)
            systemctl --user disable --now arc-node-update.timer arc-node.service 2>/dev/null || true
            rm -f -- \
                "$TARGET_HOME/.config/systemd/user/arc-node.service" \
                "$TARGET_HOME/.config/systemd/user/arc-node-update.service" \
                "$TARGET_HOME/.config/systemd/user/arc-node-update.timer"
            systemctl --user daemon-reload ;;
        launchd)
            local domain="user/$TARGET_UID"
            if launchctl print "gui/$TARGET_UID" >/dev/null 2>&1; then domain="gui/$TARGET_UID"; fi
            launchctl bootout "$domain/network.arc.node" 2>/dev/null || true
            launchctl bootout "$domain/network.arc.update" 2>/dev/null || true
            rm -f -- \
                "$TARGET_HOME/Library/LaunchAgents/network.arc.node.plist" \
                "$TARGET_HOME/Library/LaunchAgents/network.arc.update.plist" ;;
    esac
    if [ "$SERVICE_SCOPE" = system ]; then
        as_root rm -f -- \
            "$ARC_DIR/bin/arc-node" "$ARC_DIR/bin/arc-cli" \
            "$ARC_DIR/bin/run-arc-node" "$ARC_DIR/bin/arc-installer" \
            "$ARC_DIR/node.env"
    else
        rm -f -- \
            "$ARC_DIR/bin/arc-node" "$ARC_DIR/bin/arc-cli" \
            "$ARC_DIR/bin/run-arc-node" "$ARC_DIR/bin/arc-installer" \
            "$ARC_DIR/node.env"
    fi
    if [ "$PURGE" = true ]; then
        case "$ARC_DIR" in /|"$TARGET_HOME") die "Refusing to purge unsafe path: $ARC_DIR" ;; esac
        if [ "$SERVICE_SCOPE" = system ]; then as_root rm -rf -- "$ARC_DIR"
        else rm -rf -- "$ARC_DIR"
        fi
        ok "Programs, identity, and chain data removed from $ARC_DIR"
    else
        ok "Programs removed. Identity and chain data remain in $ARC_DIR"
    fi
}

if [ "$UNINSTALL" = true ]; then
    uninstall_arc
    exit 0
fi

if [ "$SERVICE_SCOPE" = system ]; then
    # The daily updater is a root system service. Its executable and the node
    # binary must therefore never be writable by the unprivileged node user.
    INSTALL_PARENT="$(dirname -- "$ARC_DIR")"
    as_root mkdir -p -- "$INSTALL_PARENT"
    PARENT_UID="$(stat -c %u "$INSTALL_PARENT")"
    PARENT_MODE="$(stat -c %a "$INSTALL_PARENT")"
    PARENT_MODE_DECIMAL=$((10#$PARENT_MODE))
    [ "$PARENT_UID" -eq 0 ] \
        || die "System install parent must be owned by root: $INSTALL_PARENT"
    [ $(((PARENT_MODE_DECIMAL / 10) % 10 & 2)) -eq 0 ] \
        && [ $((PARENT_MODE_DECIMAL % 10 & 2)) -eq 0 ] \
        || die "System install parent must not be group/world writable: $INSTALL_PARENT"
    as_root mkdir -p -- "$ARC_DIR" "$ARC_DIR/bin" "$ARC_DIR/identity" "$NODE_DATA_DIR"
    as_root chown root:root "$ARC_DIR" "$ARC_DIR/bin"
    as_root chmod 755 "$ARC_DIR" "$ARC_DIR/bin"
    as_root chown "$TARGET_USER:$TARGET_GROUP" "$ARC_DIR/identity" "$NODE_DATA_DIR"
    as_root chmod 700 "$ARC_DIR/identity" "$NODE_DATA_DIR"
else
    ensure_private_dir "$ARC_DIR"
    ensure_private_dir "$ARC_DIR/bin"
    ensure_private_dir "$ARC_DIR/identity"
    ensure_private_dir "$NODE_DATA_DIR"
fi
if [ "$SERVICE_SCOPE" != system ]; then
    as_target test -w "$ARC_DIR" \
        || die "Install user $TARGET_USER cannot write $ARC_DIR"
fi
as_target test -w "$NODE_DATA_DIR" \
    || die "Install user $TARGET_USER cannot write data directory $NODE_DATA_DIR"
if [ -n "$MODEL_PATH" ]; then
    as_target test -r "$MODEL_PATH" \
        || die "Install user $TARGET_USER cannot read model $MODEL_PATH"
fi

TRANSACTION_ACTIVE=false
TRANSACTION_COMMITTED=false
TRANSACTION_ROLLING_BACK=false

LOCK_DIR="$ARC_DIR/.install.lock"
if [ "$SERVICE_SCOPE" = system ]; then
    if ! as_root mkdir "$LOCK_DIR" 2>/dev/null; then
        die "Another ARC install/update appears to be running ($LOCK_DIR exists)."
    fi
else
    if ! mkdir "$LOCK_DIR" 2>/dev/null; then
        die "Another ARC install/update appears to be running ($LOCK_DIR exists)."
    fi
fi
TMP_DIR="$(mktemp -d "${TMPDIR:-/tmp}/arc-install.XXXXXX")"
cleanup() {
    local status=$?
    trap - EXIT HUP INT TERM
    if [ "$TRANSACTION_ACTIVE" = true ] \
        && [ "$TRANSACTION_COMMITTED" = false ] \
        && [ "$TRANSACTION_ROLLING_BACK" = false ]; then
        rollback_install_transaction || true
    fi
    rm -rf -- "$TMP_DIR"
    if [ "$SERVICE_SCOPE" = system ]; then
        as_root rmdir "$LOCK_DIR" 2>/dev/null || true
    else
        rmdir "$LOCK_DIR" 2>/dev/null || true
    fi
    exit "$status"
}
trap cleanup EXIT
trap 'exit 129' HUP
trap 'exit 130' INT
trap 'exit 143' TERM

NODE_ASSET="arc-node-$PLATFORM"
CLI_ASSET="arc-cli-$PLATFORM"

strict_version() {
    printf '%s\n' "$1" | grep -Eq '^[0-9]+\.[0-9]+\.[0-9]+$'
}

extract_version() {
    sed -nE 's/.*[^0-9]([0-9]+\.[0-9]+\.[0-9]+).*/\1/p' | head -n 1
}

version_compare() {
    awk -v left="$1" -v right="$2" 'BEGIN {
        split(left, a, "."); split(right, b, ".");
        for (i = 1; i <= 3; i++) {
            if ((a[i] + 0) > (b[i] + 0)) { print 1; exit }
            if ((a[i] + 0) < (b[i] + 0)) { print -1; exit }
        }
        print 0
    }'
}

github_curl() {
    local url="$1"
    local args=(--fail --silent --show-error --location --retry 3 \
        --proto '=https' --proto-redir '=https' --tlsv1.2 \
        --connect-timeout 15 --header 'Accept: application/vnd.github+json' \
        --header 'User-Agent: arc-chain-installer')
    if [ -n "${GITHUB_TOKEN:-}" ]; then
        args+=(--header "Authorization: Bearer $GITHUB_TOKEN")
    fi
    curl "${args[@]}" "$url"
}

if [ -n "$REQUESTED_VERSION" ]; then
    REQUESTED_VERSION="${REQUESTED_VERSION#v}"
    strict_version "$REQUESTED_VERSION" \
        || die "Version must be strict X.Y.Z (got: $REQUESTED_VERSION)"
    RELEASE_METADATA_URL="$API_ROOT/releases/tags/v$REQUESTED_VERSION"
else
    RELEASE_METADATA_URL="$API_ROOT/releases/latest"
fi

info "Resolving a complete release for $PLATFORM"
if ! github_curl "$RELEASE_METADATA_URL" > "$TMP_DIR/release.json"; then
    die "Could not read GitHub release metadata: $RELEASE_METADATA_URL"
fi

# SHA256SUMS and the payloads it authenticates live in the same GitHub
# release. A mutable release could replace both together, so a checksum alone
# is not an authenticity boundary. The sole publisher enables GitHub immutable
# releases before creation; require that server-side property here as well and
# reject drafts/prereleases from both pinned installs and unattended updates.
require_release_boolean() {
    local key="$1"
    local expected="$2"
    local rejected=true
    if [ "$expected" = true ]; then rejected=false; fi
    grep -Eq "\"$key\"[[:space:]]*:[[:space:]]*$expected([[:space:]]*[,}])" \
        "$TMP_DIR/release.json" \
        || die "Release metadata must declare $key=$expected"
    if grep -Eq "\"$key\"[[:space:]]*:[[:space:]]*$rejected([[:space:]]*[,}])" \
        "$TMP_DIR/release.json"; then
        die "Release metadata contains conflicting $key values"
    fi
}
require_release_boolean immutable true
require_release_boolean draft false
require_release_boolean prerelease false

RESOLVED_TAG="$(sed -n 's/.*"tag_name"[[:space:]]*:[[:space:]]*"\([^"]*\)".*/\1/p' "$TMP_DIR/release.json" | head -n 1)"
case "$RESOLVED_TAG" in v*) VERSION="${RESOLVED_TAG#v}" ;; *) die "Release metadata has no valid vX.Y.Z tag" ;; esac
strict_version "$VERSION" || die "Release tag is not strict vX.Y.Z: $RESOLVED_TAG"
if [ -n "$REQUESTED_VERSION" ] && [ "$VERSION" != "$REQUESTED_VERSION" ]; then
    die "GitHub returned $RESOLVED_TAG when v$REQUESTED_VERSION was requested"
fi

# Do not scrape the release asset array with a greedy line-oriented regex.
# GitHub serves API JSON minified on one line, where that approach captures
# only the final `name` value. The exact immutable tag is resolved above; each
# required asset is downloaded from that tag below with fail-on-404 and then
# matched to exactly one SHA256SUMS entry. A missing asset therefore fails
# closed without release walking or fallback.

INSTALLED_VERSION=""
if [ -x "$ARC_DIR/bin/arc-node" ]; then
    INSTALLED_VERSION="$("$ARC_DIR/bin/arc-node" --version 2>/dev/null | extract_version || true)"
    if [ -z "$INSTALLED_VERSION" ] || ! strict_version "$INSTALLED_VERSION"; then
        die "Existing arc-node has an unknown version; refusing to overwrite it: $ARC_DIR/bin/arc-node"
    fi
fi
if [ "$UPDATE_ONLY" = true ] && [ -z "$INSTALLED_VERSION" ]; then
    die "--update-only requires an existing installation in $ARC_DIR"
fi
if [ -n "$INSTALLED_VERSION" ]; then
    COMPARISON="$(version_compare "$VERSION" "$INSTALLED_VERSION")"
    if [ "$COMPARISON" -lt 0 ]; then
        die "Refusing downgrade from v$INSTALLED_VERSION to v$VERSION"
    fi
    if [ "$UPDATE_ONLY" = true ] && [ "$COMPARISON" -eq 0 ]; then
        ok "Already up to date at v$INSTALLED_VERSION"
        exit 0
    fi
fi

RELEASE_URL="$DOWNLOAD_ROOT/$RESOLVED_TAG"
info "Downloading checksums for $RESOLVED_TAG"
github_curl "$RELEASE_URL/SHA256SUMS" > "$TMP_DIR/SHA256SUMS" \
    || die "Could not download $RELEASE_URL/SHA256SUMS"

sha256_file() {
    if command -v sha256sum >/dev/null 2>&1; then
        sha256sum "$1" | awk '{print $1}'
    elif command -v shasum >/dev/null 2>&1; then
        shasum -a 256 "$1" | awk '{print $1}'
    else
        die "sha256sum (Linux) or shasum (macOS) is required"
    fi
}

verify_file() {
    local file="$1"
    local asset="$2"
    local expected
    local count
    local actual
    expected="$(awk -v name="$asset" '$2 == name || $2 == "*" name { print $1 }' "$TMP_DIR/SHA256SUMS")"
    count="$(printf '%s\n' "$expected" | awk 'NF { n += 1 } END { print n + 0 }')"
    [ "$count" -eq 1 ] || die "SHA256SUMS must contain exactly one entry for $asset"
    printf '%s\n' "$expected" | grep -Eq '^[0-9a-fA-F]{64}$' \
        || die "Invalid SHA-256 entry for $asset"
    actual="$(sha256_file "$file")"
    [ "$actual" = "$expected" ] \
        || die "Checksum verification failed for $asset (expected $expected, got $actual)"
}

download_checked() {
    local asset="$1"
    local destination="$TMP_DIR/$asset"
    info "Downloading $asset"
    github_curl "$RELEASE_URL/$asset" > "$destination" \
        || die "Download failed: $RELEASE_URL/$asset"
    verify_file "$destination" "$asset"
}

download_checked "$NODE_ASSET"
download_checked "$CLI_ASSET"
download_checked testnet-seeds.txt
download_checked genesis.toml
if [ "$INSTALL_UPDATER" = true ]; then download_checked install.sh; fi
chmod 700 "$TMP_DIR/$NODE_ASSET" "$TMP_DIR/$CLI_ASSET"
DOWNLOADED_NODE_VERSION="$("$TMP_DIR/$NODE_ASSET" --version 2>/dev/null | extract_version || true)"
DOWNLOADED_CLI_VERSION="$("$TMP_DIR/$CLI_ASSET" --version 2>/dev/null | extract_version || true)"
[ "$DOWNLOADED_NODE_VERSION" = "$VERSION" ] \
    || die "$NODE_ASSET reports v${DOWNLOADED_NODE_VERSION:-unknown}, expected v$VERSION"
[ "$DOWNLOADED_CLI_VERSION" = "$VERSION" ] \
    || die "$CLI_ASSET reports v${DOWNLOADED_CLI_VERSION:-unknown}, expected v$VERSION"

atomic_copy() {
    local source="$1"
    local destination="$2"
    local mode="$3"
    local ownership="${4:-target}"
    local staged="${destination}.new.$$"
    if [ "$SERVICE_SCOPE" = system ]; then
        as_root cp -- "$source" "$staged" || return 1
        as_root chmod "$mode" "$staged" || return 1
        if [ "$ownership" = root ]; then
            as_root chown root:root "$staged" || return 1
        else
            as_root chown "$TARGET_USER:$TARGET_GROUP" "$staged" || return 1
        fi
        as_root mv -f -- "$staged" "$destination" || return 1
    else
        cp -- "$source" "$staged" || return 1
        chmod "$mode" "$staged" || return 1
        set_target_owner "$staged" || return 1
        mv -f -- "$staged" "$destination" || return 1
    fi
}

TRANSACTION_PATHS=()
TRANSACTION_BACKUPS=()
TRANSACTION_EXISTED=()
TRANSACTION_COPY_COUNT=0
PRIOR_NODE_ACTIVE=false
PRIOR_NODE_ENABLED=false
PRIOR_UPDATER_ACTIVE=false
PRIOR_UPDATER_ENABLED=false
PRIOR_LAUNCHD_NODE_LOADED=false
PRIOR_LAUNCHD_NODE_DISABLED=false
PRIOR_LAUNCHD_UPDATER_LOADED=false
PRIOR_LAUNCHD_UPDATER_DISABLED=false
LAUNCHD_DOMAIN=""
USER_UNIT_DIR="$TARGET_HOME/.config/systemd/user"
LAUNCH_AGENT_DIR="$TARGET_HOME/Library/LaunchAgents"
NODE_PLIST="$LAUNCH_AGENT_DIR/network.arc.node.plist"
UPDATE_PLIST="$LAUNCH_AGENT_DIR/network.arc.update.plist"
SEED_FILE="$ARC_DIR/identity/validator-seed"

transaction_as_owner() {
    if [ "$SERVICE_SCOPE" = system ]; then
        as_root "$@"
    else
        "$@"
    fi
}

snapshot_transaction_path() {
    local path="$1"
    local index="${#TRANSACTION_PATHS[@]}"
    local backup="$TMP_DIR/transaction-backup-$index"

    if transaction_as_owner test -L "$path"; then
        die "Refusing symlinked managed install path: $path"
    fi
    TRANSACTION_PATHS[index]="$path"
    TRANSACTION_BACKUPS[index]="$backup"
    if transaction_as_owner test -e "$path"; then
        transaction_as_owner test -f "$path" \
            || die "Managed install path is not a regular file: $path"
        transaction_as_owner cp -p -- "$path" "$backup" \
            || die "Could not snapshot managed install path: $path"
        TRANSACTION_EXISTED[index]=true
    else
        TRANSACTION_EXISTED[index]=false
    fi
}

capture_service_state() {
    case "$SERVICE_SCOPE" in
        system)
            if as_root systemctl is-active --quiet arc-node.service; then
                PRIOR_NODE_ACTIVE=true
            fi
            if as_root systemctl is-enabled --quiet arc-node.service; then
                PRIOR_NODE_ENABLED=true
            fi
            if as_root systemctl is-active --quiet arc-node-update.timer; then
                PRIOR_UPDATER_ACTIVE=true
            fi
            if as_root systemctl is-enabled --quiet arc-node-update.timer; then
                PRIOR_UPDATER_ENABLED=true
            fi
            ;;
        user)
            if systemctl --user is-active --quiet arc-node.service; then
                PRIOR_NODE_ACTIVE=true
            fi
            if systemctl --user is-enabled --quiet arc-node.service; then
                PRIOR_NODE_ENABLED=true
            fi
            if systemctl --user is-active --quiet arc-node-update.timer; then
                PRIOR_UPDATER_ACTIVE=true
            fi
            if systemctl --user is-enabled --quiet arc-node-update.timer; then
                PRIOR_UPDATER_ENABLED=true
            fi
            ;;
        launchd)
            LAUNCHD_DOMAIN="user/$TARGET_UID"
            if launchctl print "gui/$TARGET_UID" >/dev/null 2>&1; then
                LAUNCHD_DOMAIN="gui/$TARGET_UID"
            fi
            if launchctl print "$LAUNCHD_DOMAIN/network.arc.node" >/dev/null 2>&1; then
                PRIOR_LAUNCHD_NODE_LOADED=true
            fi
            if launchctl print "$LAUNCHD_DOMAIN/network.arc.update" >/dev/null 2>&1; then
                PRIOR_LAUNCHD_UPDATER_LOADED=true
            fi
            local disabled_state
            disabled_state="$(launchctl print-disabled "$LAUNCHD_DOMAIN" 2>/dev/null)" \
                || die "Could not capture the prior launchd enablement state"
            case "$disabled_state" in
                *'"network.arc.node" => true'*) PRIOR_LAUNCHD_NODE_DISABLED=true ;;
            esac
            case "$disabled_state" in
                *'"network.arc.update" => true'*) PRIOR_LAUNCHD_UPDATER_DISABLED=true ;;
            esac
            ;;
    esac
}

begin_install_transaction() {
    case "${ARC_INSTALL_TEST_FAIL_AFTER_COPY:-}" in
        ''|*[!0-9]*)
            [ -z "${ARC_INSTALL_TEST_FAIL_AFTER_COPY:-}" ] \
                || die "ARC_INSTALL_TEST_FAIL_AFTER_COPY must be a positive integer"
            ;;
        0) die "ARC_INSTALL_TEST_FAIL_AFTER_COPY must be a positive integer" ;;
    esac

    snapshot_transaction_path "$ARC_DIR/bin/arc-node"
    snapshot_transaction_path "$ARC_DIR/bin/arc-cli"
    snapshot_transaction_path "$ARC_DIR/testnet-seeds.txt"
    snapshot_transaction_path "$ARC_DIR/genesis.toml"
    snapshot_transaction_path "$ARC_DIR/bin/arc-installer"
    snapshot_transaction_path "$SEED_FILE"
    snapshot_transaction_path "$ARC_DIR/node.env"
    snapshot_transaction_path "$ARC_DIR/bin/run-arc-node"
    snapshot_transaction_path "$CONFIG_FILE"

    case "$SERVICE_SCOPE" in
        system)
            snapshot_transaction_path /etc/systemd/system/arc-node.service
            snapshot_transaction_path /etc/systemd/system/arc-node-update.service
            snapshot_transaction_path /etc/systemd/system/arc-node-update.timer
            ;;
        user)
            snapshot_transaction_path "$USER_UNIT_DIR/arc-node.service"
            snapshot_transaction_path "$USER_UNIT_DIR/arc-node-update.service"
            snapshot_transaction_path "$USER_UNIT_DIR/arc-node-update.timer"
            ;;
        launchd)
            snapshot_transaction_path "$NODE_PLIST"
            snapshot_transaction_path "$UPDATE_PLIST"
            ;;
    esac
    capture_service_state
    TRANSACTION_ACTIVE=true
}

transactional_copy() {
    atomic_copy "$@" || return 1
    TRANSACTION_COPY_COUNT=$((TRANSACTION_COPY_COUNT + 1))
    if [ -n "${ARC_INSTALL_TEST_FAIL_AFTER_COPY:-}" ] \
        && [ "$TRANSACTION_COPY_COUNT" -eq "$ARC_INSTALL_TEST_FAIL_AFTER_COPY" ]; then
        warn "Injected installer failure after managed copy $TRANSACTION_COPY_COUNT"
        return 97
    fi
}

restore_transaction_path() {
    local index="$1"
    local path="${TRANSACTION_PATHS[$index]}"
    local backup="${TRANSACTION_BACKUPS[$index]}"
    local staged="${path}.rollback.$$"
    transaction_as_owner rm -f -- "${path}.new.$$" || return 1
    if [ "${TRANSACTION_EXISTED[$index]}" = true ]; then
        transaction_as_owner cp -p -- "$backup" "$staged" || return 1
        transaction_as_owner mv -f -- "$staged" "$path" || return 1
    else
        transaction_as_owner rm -f -- "$staged" "$path" || return 1
    fi
}

restore_systemd_state() {
    local status=0
    if [ "$SERVICE_SCOPE" = system ]; then
        as_root systemctl daemon-reload || status=1
        if [ "$PRIOR_NODE_ENABLED" = true ]; then
            as_root systemctl enable arc-node.service >/dev/null || status=1
        else
            as_root systemctl disable arc-node.service >/dev/null 2>&1 || true
        fi
        if [ "$PRIOR_UPDATER_ENABLED" = true ]; then
            as_root systemctl enable arc-node-update.timer >/dev/null || status=1
        else
            as_root systemctl disable arc-node-update.timer >/dev/null 2>&1 || true
        fi
        if [ "$PRIOR_UPDATER_ACTIVE" = true ]; then
            as_root systemctl restart arc-node-update.timer || status=1
        else
            as_root systemctl stop arc-node-update.timer >/dev/null 2>&1 || true
        fi
        if [ "$PRIOR_NODE_ACTIVE" = true ]; then
            as_root systemctl restart arc-node.service || status=1
        else
            as_root systemctl stop arc-node.service >/dev/null 2>&1 || true
        fi
    else
        systemctl --user daemon-reload || status=1
        if [ "$PRIOR_NODE_ENABLED" = true ]; then
            systemctl --user enable arc-node.service >/dev/null || status=1
        else
            systemctl --user disable arc-node.service >/dev/null 2>&1 || true
        fi
        if [ "$PRIOR_UPDATER_ENABLED" = true ]; then
            systemctl --user enable arc-node-update.timer >/dev/null || status=1
        else
            systemctl --user disable arc-node-update.timer >/dev/null 2>&1 || true
        fi
        if [ "$PRIOR_UPDATER_ACTIVE" = true ]; then
            systemctl --user restart arc-node-update.timer || status=1
        else
            systemctl --user stop arc-node-update.timer >/dev/null 2>&1 || true
        fi
        if [ "$PRIOR_NODE_ACTIVE" = true ]; then
            systemctl --user restart arc-node.service || status=1
        else
            systemctl --user stop arc-node.service >/dev/null 2>&1 || true
        fi
    fi
    return "$status"
}

restore_launchd_state() {
    local status=0
    launchctl bootout "$LAUNCHD_DOMAIN/network.arc.node" 2>/dev/null || true
    launchctl bootout "$LAUNCHD_DOMAIN/network.arc.update" 2>/dev/null || true
    if [ "$PRIOR_LAUNCHD_NODE_DISABLED" = true ]; then
        launchctl disable "$LAUNCHD_DOMAIN/network.arc.node" || status=1
    else
        launchctl enable "$LAUNCHD_DOMAIN/network.arc.node" || status=1
    fi
    if [ "$PRIOR_LAUNCHD_UPDATER_DISABLED" = true ]; then
        launchctl disable "$LAUNCHD_DOMAIN/network.arc.update" || status=1
    else
        launchctl enable "$LAUNCHD_DOMAIN/network.arc.update" || status=1
    fi
    if [ "$PRIOR_LAUNCHD_NODE_LOADED" = true ]; then
        launchctl bootstrap "$LAUNCHD_DOMAIN" "$NODE_PLIST" || status=1
    fi
    if [ "$PRIOR_LAUNCHD_UPDATER_LOADED" = true ]; then
        launchctl bootstrap "$LAUNCHD_DOMAIN" "$UPDATE_PLIST" || status=1
    fi
    return "$status"
}

rollback_install_transaction() {
    local status=0
    local index
    TRANSACTION_ROLLING_BACK=true
    warn "Install/update failed; restoring every previously managed file and service state"
    for ((index=${#TRANSACTION_PATHS[@]} - 1; index >= 0; index--)); do
        restore_transaction_path "$index" || status=1
    done
    case "$SERVICE_SCOPE" in
        system|user) restore_systemd_state || status=1 ;;
        launchd) restore_launchd_state || status=1 ;;
    esac
    TRANSACTION_ACTIVE=false
    TRANSACTION_ROLLING_BACK=false
    if [ "$status" -eq 0 ]; then
        ok "Previous ARC installation and service state restored"
    else
        warn "Rollback encountered an error; inspect all managed files and service state before restarting ARC"
    fi
    return "$status"
}

commit_install_transaction() {
    TRANSACTION_COMMITTED=true
    TRANSACTION_ACTIVE=false
}

if [ "$SERVICE_SCOPE" = system ]; then PROGRAM_MODE=755; CONFIG_MODE=644
else PROGRAM_MODE=700; CONFIG_MODE=600
fi
begin_install_transaction
transactional_copy "$TMP_DIR/$NODE_ASSET" "$ARC_DIR/bin/arc-node" "$PROGRAM_MODE" root
transactional_copy "$TMP_DIR/$CLI_ASSET" "$ARC_DIR/bin/arc-cli" "$PROGRAM_MODE" root
transactional_copy "$TMP_DIR/testnet-seeds.txt" "$ARC_DIR/testnet-seeds.txt" "$CONFIG_MODE" root
transactional_copy "$TMP_DIR/genesis.toml" "$ARC_DIR/genesis.toml" "$CONFIG_MODE" root
if [ "$INSTALL_UPDATER" = true ]; then
    transactional_copy "$TMP_DIR/install.sh" "$ARC_DIR/bin/arc-installer" "$PROGRAM_MODE" root
fi

if [ -e "$SEED_FILE" ]; then
    [ -f "$SEED_FILE" ] && [ ! -L "$SEED_FILE" ] && [ -s "$SEED_FILE" ] \
        || die "Identity seed is not a non-empty regular file: $SEED_FILE"
    chmod 600 "$SEED_FILE"
else
    if command -v openssl >/dev/null 2>&1; then
        GENERATED_SEED="arc-community-$(openssl rand -hex 32)"
    else
        GENERATED_SEED="arc-community-$(od -An -N32 -tx1 /dev/urandom | tr -d ' \n')"
    fi
    printf '%s\n' "$GENERATED_SEED" > "$TMP_DIR/validator-seed"
    transactional_copy "$TMP_DIR/validator-seed" "$SEED_FILE" 600
    unset GENERATED_SEED
fi
set_target_owner "$SEED_FILE"

VALIDATOR_SEED="$(sed -n '1p' "$SEED_FILE")"
case "$VALIDATOR_SEED" in *$'\n'*|*$'\r'*|'') die "Identity seed contains invalid data" ;; esac
printf 'ARC_VALIDATOR_SEED=%s\n' "$VALIDATOR_SEED" > "$TMP_DIR/node.env"
transactional_copy "$TMP_DIR/node.env" "$ARC_DIR/node.env" 600
unset VALIDATOR_SEED

NODE_ARGS=(
    "$ARC_DIR/bin/arc-node"
    --rpc "127.0.0.1:$RPC_PORT"
    --p2p-port "$P2P_PORT"
    --seeds-file "$ARC_DIR/testnet-seeds.txt"
    --genesis "$ARC_DIR/genesis.toml"
    --stake 0
    --min-stake 0
    --eth-rpc-port 0
    --data-dir "$NODE_DATA_DIR"
    --community-mode
)
for origin in "${COMMUNITY_RPC_ORIGINS[@]}"; do
    NODE_ARGS+=(--community-rpc-url "$origin")
done
if [ -n "$MODEL_PATH" ]; then
    NODE_ARGS+=(--model "$MODEL_PATH" --full-integer-worker)
fi

{
    printf '%s\n' '#!/usr/bin/env bash' 'set -Eeuo pipefail'
    printf '. %q\n' "$ARC_DIR/node.env"
    printf '%s\n' 'export ARC_VALIDATOR_SEED'
    printf 'exec'
    for argument in "${NODE_ARGS[@]}"; do printf ' %q' "$argument"; done
    printf ' "$@"\n'
} > "$TMP_DIR/run-arc-node"
transactional_copy "$TMP_DIR/run-arc-node" "$ARC_DIR/bin/run-arc-node" "$PROGRAM_MODE" root

{
    printf '%s\n' '# ARC installer state v1'
    printf 'version=%s\n' "$VERSION"
    printf 'rpc_port=%s\n' "$RPC_PORT"
    printf 'p2p_port=%s\n' "$P2P_PORT"
    printf 'data_dir=%s\n' "$NODE_DATA_DIR"
    printf 'model_path=%s\n' "$MODEL_PATH"
    printf 'service_scope=%s\n' "$SERVICE_SCOPE"
} > "$TMP_DIR/install.conf"
transactional_copy "$TMP_DIR/install.conf" "$CONFIG_FILE" "$CONFIG_MODE" root

systemd_escape() {
    local value="$1"
    value="${value//\\/\\\\}"
    value="${value//\"/\\\"}"
    value="${value//%/%%}"
    printf '%s' "$value"
}

write_exec_start() {
    printf 'ExecStart='
    for argument in "$@"; do
        printf '"%s" ' "$(systemd_escape "$argument")"
    done
    printf '\n'
}

install_systemd_system() {
    command -v systemctl >/dev/null 2>&1 || {
        warn "systemctl is unavailable; use --no-service for an install-only setup"
        return 1
    }
    {
        printf '%s\n' \
            '[Unit]' \
            'Description=ARC Chain community node' \
            'Wants=network-online.target' \
            'After=network-online.target' \
            '' \
            '[Service]' \
            'Type=simple'
        printf 'User=%s\nGroup=%s\n' "$TARGET_USER" "$TARGET_GROUP"
        printf 'WorkingDirectory="%s"\n' "$(systemd_escape "$ARC_DIR")"
        write_exec_start "$ARC_DIR/bin/run-arc-node"
        printf '%s\n' \
            'Restart=on-failure' \
            'RestartSec=5' \
            'TimeoutStopSec=30' \
            'UMask=0077' \
            'NoNewPrivileges=true' \
            'PrivateTmp=true' \
            'ProtectSystem=full' \
            'ProtectKernelTunables=true' \
            'ProtectKernelModules=true' \
            'ProtectControlGroups=true' \
            'RestrictSUIDSGID=true' \
            '' \
            '[Install]' \
            'WantedBy=multi-user.target'
    } > "$TMP_DIR/arc-node.service" || return 1
    transactional_copy "$TMP_DIR/arc-node.service" /etc/systemd/system/arc-node.service 644 root || return 1
    as_root systemctl daemon-reload || return 1
    as_root systemctl enable arc-node.service >/dev/null || return 1
    as_root systemctl restart arc-node.service || return 1
}

install_systemd_user() {
    command -v systemctl >/dev/null 2>&1 || {
        warn "systemctl is unavailable; use --no-service for an install-only setup"
        return 1
    }
    if ! systemctl --user show-environment >/dev/null 2>&1; then
        warn "No per-user systemd manager is reachable from this session. Rerun with sudo --system-service, or use --no-service."
        return 1
    fi
    ensure_private_dir "$TARGET_HOME/.config" || return 1
    ensure_private_dir "$TARGET_HOME/.config/systemd" || return 1
    ensure_private_dir "$USER_UNIT_DIR" || return 1
    {
        printf '%s\n' \
            '[Unit]' \
            'Description=ARC Chain community node' \
            'Wants=network-online.target' \
            'After=network-online.target' \
            '' \
            '[Service]' \
            'Type=simple'
        printf 'WorkingDirectory="%s"\n' "$(systemd_escape "$ARC_DIR")"
        write_exec_start "$ARC_DIR/bin/run-arc-node"
        printf '%s\n' \
            'Restart=on-failure' \
            'RestartSec=5' \
            'TimeoutStopSec=30' \
            'UMask=0077' \
            'NoNewPrivileges=true' \
            'PrivateTmp=true' \
            'ProtectSystem=full' \
            'ProtectKernelTunables=true' \
            'ProtectKernelModules=true' \
            'ProtectControlGroups=true' \
            'RestrictSUIDSGID=true' \
            '' \
            '[Install]' \
            'WantedBy=default.target'
    } > "$TMP_DIR/arc-node.service" || return 1
    transactional_copy "$TMP_DIR/arc-node.service" "$USER_UNIT_DIR/arc-node.service" 600 || return 1
    systemctl --user daemon-reload || return 1
    systemctl --user enable arc-node.service >/dev/null || return 1
    systemctl --user restart arc-node.service || return 1
}

xml_escape() {
    local value="$1"
    value="${value//&/&amp;}"
    value="${value//</&lt;}"
    value="${value//>/&gt;}"
    value="${value//\"/&quot;}"
    value="${value//\'/&apos;}"
    printf '%s' "$value"
}

install_launchd() {
    command -v launchctl >/dev/null 2>&1 || return 1
    mkdir -p -- "$LAUNCH_AGENT_DIR" || return 1
    {
        printf '%s\n' '<?xml version="1.0" encoding="UTF-8"?>' \
            '<!DOCTYPE plist PUBLIC "-//Apple//DTD PLIST 1.0//EN" "http://www.apple.com/DTDs/PropertyList-1.0.dtd">' \
            '<plist version="1.0"><dict>' \
            '<key>Label</key><string>network.arc.node</string>' \
            '<key>ProgramArguments</key><array>'
        printf '<string>%s</string>\n' "$(xml_escape "$ARC_DIR/bin/run-arc-node")"
        printf '%s\n' \
            '</array>' \
            '<key>RunAtLoad</key><true/>' \
            '<key>KeepAlive</key><dict><key>SuccessfulExit</key><false/></dict>' \
            '<key>ThrottleInterval</key><integer>5</integer>'
        printf '<key>WorkingDirectory</key><string>%s</string>\n' "$(xml_escape "$ARC_DIR")"
        printf '<key>StandardOutPath</key><string>%s</string>\n' "$(xml_escape "$ARC_DIR/node.log")"
        printf '<key>StandardErrorPath</key><string>%s</string>\n' "$(xml_escape "$ARC_DIR/node.log")"
        printf '%s\n' '</dict></plist>'
    } > "$TMP_DIR/network.arc.node.plist" || return 1
    if command -v plutil >/dev/null 2>&1; then
        plutil -lint "$TMP_DIR/network.arc.node.plist" >/dev/null || return 1
    fi
    transactional_copy "$TMP_DIR/network.arc.node.plist" "$NODE_PLIST" 600 || return 1
    launchctl bootout "$LAUNCHD_DOMAIN/network.arc.node" 2>/dev/null || true
    launchctl bootstrap "$LAUNCHD_DOMAIN" "$NODE_PLIST" || return 1
    launchctl enable "$LAUNCHD_DOMAIN/network.arc.node" || return 1
    launchctl kickstart -k "$LAUNCHD_DOMAIN/network.arc.node" || return 1
}

install_systemd_updater_system() {
    {
        printf '%s\n' '[Unit]' 'Description=Update ARC Chain from a checksummed release' '' '[Service]' 'Type=oneshot'
        printf 'Environment="ARC_TARGET_USER=%s"\n' "$(systemd_escape "$TARGET_USER")"
        write_exec_start "$ARC_DIR/bin/arc-installer" --update-only --install-dir "$ARC_DIR" --service-scope system
    } > "$TMP_DIR/arc-node-update.service" || return 1
    {
        printf '%s\n' \
            '[Unit]' \
            'Description=Daily ARC Chain update check' \
            '' \
            '[Timer]' \
            'OnCalendar=*-*-* 04:17:00' \
            'RandomizedDelaySec=30m' \
            'Persistent=true' \
            '' \
            '[Install]' \
            'WantedBy=timers.target'
    } > "$TMP_DIR/arc-node-update.timer" || return 1
    transactional_copy "$TMP_DIR/arc-node-update.service" /etc/systemd/system/arc-node-update.service 644 root || return 1
    transactional_copy "$TMP_DIR/arc-node-update.timer" /etc/systemd/system/arc-node-update.timer 644 root || return 1
    as_root systemctl daemon-reload || return 1
    as_root systemctl enable --now arc-node-update.timer >/dev/null || return 1
}

install_systemd_updater_user() {
    local unit_dir="$TARGET_HOME/.config/systemd/user"
    {
        printf '%s\n' '[Unit]' 'Description=Update ARC Chain from a checksummed release' '' '[Service]' 'Type=oneshot'
        write_exec_start "$ARC_DIR/bin/arc-installer" --update-only --install-dir "$ARC_DIR" --service-scope user
    } > "$TMP_DIR/arc-node-update.service" || return 1
    {
        printf '%s\n' \
            '[Unit]' \
            'Description=Daily ARC Chain update check' \
            '' \
            '[Timer]' \
            'OnCalendar=*-*-* 04:17:00' \
            'RandomizedDelaySec=30m' \
            'Persistent=true' \
            '' \
            '[Install]' \
            'WantedBy=timers.target'
    } > "$TMP_DIR/arc-node-update.timer" || return 1
    transactional_copy "$TMP_DIR/arc-node-update.service" "$unit_dir/arc-node-update.service" 600 || return 1
    transactional_copy "$TMP_DIR/arc-node-update.timer" "$unit_dir/arc-node-update.timer" 600 || return 1
    systemctl --user daemon-reload || return 1
    systemctl --user enable --now arc-node-update.timer >/dev/null || return 1
}

install_launchd_updater() {
    {
        printf '%s\n' '<?xml version="1.0" encoding="UTF-8"?>' \
            '<!DOCTYPE plist PUBLIC "-//Apple//DTD PLIST 1.0//EN" "http://www.apple.com/DTDs/PropertyList-1.0.dtd">' \
            '<plist version="1.0"><dict>' \
            '<key>Label</key><string>network.arc.update</string>' \
            '<key>ProgramArguments</key><array>'
        for argument in "$ARC_DIR/bin/arc-installer" --update-only --install-dir "$ARC_DIR" --service-scope launchd; do
            printf '<string>%s</string>\n' "$(xml_escape "$argument")"
        done
        printf '%s\n' \
            '</array>' \
            '<key>StartCalendarInterval</key><dict><key>Hour</key><integer>4</integer><key>Minute</key><integer>17</integer></dict>'
        printf '<key>StandardOutPath</key><string>%s</string>\n' "$(xml_escape "$ARC_DIR/update.log")"
        printf '<key>StandardErrorPath</key><string>%s</string>\n' "$(xml_escape "$ARC_DIR/update.log")"
        printf '%s\n' '</dict></plist>'
    } > "$TMP_DIR/network.arc.update.plist" || return 1
    if command -v plutil >/dev/null 2>&1; then
        plutil -lint "$TMP_DIR/network.arc.update.plist" >/dev/null || return 1
    fi
    transactional_copy "$TMP_DIR/network.arc.update.plist" "$UPDATE_PLIST" 600 || return 1
    launchctl bootout "$LAUNCHD_DOMAIN/network.arc.update" 2>/dev/null || true
    launchctl bootstrap "$LAUNCHD_DOMAIN" "$UPDATE_PLIST" || return 1
    launchctl enable "$LAUNCHD_DOMAIN/network.arc.update" || return 1
}

disable_updater() {
    case "$SERVICE_SCOPE" in
        system)
            as_root systemctl disable --now arc-node-update.timer 2>/dev/null || true
            as_root rm -f -- /etc/systemd/system/arc-node-update.service /etc/systemd/system/arc-node-update.timer || return 1
            as_root systemctl daemon-reload || return 1 ;;
        user)
            systemctl --user disable --now arc-node-update.timer 2>/dev/null || true
            rm -f -- "$TARGET_HOME/.config/systemd/user/arc-node-update.service" "$TARGET_HOME/.config/systemd/user/arc-node-update.timer" || return 1
            systemctl --user daemon-reload || return 1 ;;
        launchd)
            [ -n "$LAUNCHD_DOMAIN" ] || LAUNCHD_DOMAIN="user/$TARGET_UID"
            launchctl bootout "$LAUNCHD_DOMAIN/network.arc.update" 2>/dev/null || true
            rm -f -- "$UPDATE_PLIST" || return 1 ;;
    esac
}

rollback_and_die() {
    local reason="$1"
    rollback_install_transaction || true
    die "$reason"
}

if [ "$SERVICE_SCOPE" != none ]; then
    info "Installing $SERVICE_SCOPE service for $TARGET_USER"
    case "$SERVICE_SCOPE" in
        system) install_systemd_system || rollback_and_die "Could not install/start the system service" ;;
        user) install_systemd_user || rollback_and_die "Could not install/start the user service" ;;
        launchd) install_launchd || rollback_and_die "Could not install/start the launchd service" ;;
    esac

    if [ "$INSTALL_UPDATER" = true ]; then
        case "$SERVICE_SCOPE" in
            system) install_systemd_updater_system || rollback_and_die "Could not install the system update timer" ;;
            user) install_systemd_updater_user || rollback_and_die "Could not install the user update timer" ;;
            launchd) install_launchd_updater || rollback_and_die "Could not install the launchd updater" ;;
        esac
    else
        disable_updater || rollback_and_die "Could not disable the previous update schedule"
    fi

    HEALTH_TIMEOUT="${ARC_HEALTH_TIMEOUT:-180}"
    if [ -n "$MODEL_PATH" ] && [ "${ARC_HEALTH_TIMEOUT+x}" != x ]; then HEALTH_TIMEOUT=600; fi
    case "$HEALTH_TIMEOUT" in ''|*[!0-9]*) rollback_and_die "ARC_HEALTH_TIMEOUT must be an integer" ;; esac
    info "Waiting up to ${HEALTH_TIMEOUT}s for http://127.0.0.1:$RPC_PORT/health"
    health_ready=false
    health_status=""
    elapsed=0
    while [ "$elapsed" -lt "$HEALTH_TIMEOUT" ]; do
        if health_status="$("$ARC_DIR/bin/arc-cli" \
            --rpc "http://127.0.0.1:$RPC_PORT" health 2>/dev/null)"; then
            case "$health_status" in
                ok|degraded)
                    health_ready=true
                    break
                    ;;
            esac
        fi
        sleep 2
        elapsed=$((elapsed + 2))
    done
    [ "$health_ready" = true ] \
        || rollback_and_die "Node service did not return an ok/degraded health status on RPC port $RPC_PORT"
    if [ "$health_status" = ok ]; then
        ok "ARC node v$VERSION is reachable and reports status=ok on RPC port $RPC_PORT"
    else
        warn "ARC node v$VERSION is reachable but reports status=degraded on RPC port $RPC_PORT. Inspect /health before claiming chain or inference readiness."
    fi
else
    ok "ARC node v$VERSION and CLI installed without starting a service"
    printf 'Run it with: %q\n' "$ARC_DIR/bin/run-arc-node"
fi

commit_install_transaction

if [ "$SERVICE_SCOPE" = user ] && command -v loginctl >/dev/null 2>&1; then
    if [ "$(loginctl show-user "$TARGET_USER" -p Linger --value 2>/dev/null || true)" != yes ]; then
        warn "This user service starts at login. For boot-time start before login, an administrator may run: sudo loginctl enable-linger $TARGET_USER"
    fi
fi

printf '\n%bARC headless install complete%b\n' "$BOLD" "$RESET"
printf '  Version:     v%s\n' "$VERSION"
printf '  Node:        %s\n' "$ARC_DIR/bin/arc-node"
printf '  CLI:         %s\n' "$ARC_DIR/bin/arc-cli"
printf '  Data:        %s\n' "$NODE_DATA_DIR"
printf '  RPC:         http://127.0.0.1:%s\n' "$RPC_PORT"
printf '  Service:     %s\n' "$SERVICE_SCOPE"
printf '  Identity:    %s (preserved on upgrades; never printed)\n' "$SEED_FILE"
if [ -z "$MODEL_PATH" ]; then
    printf '  Inference:   observer/router only; rerun with --model /absolute/model.gguf to serve local inference\n'
else
    printf '  Model:       %s\n' "$MODEL_PATH"
fi
