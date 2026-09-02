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
# v0.8 closes admission, joins every producer/task, and then fsyncs the WAL.
# Already-owned v0.8 community work retains the 4,000-second public window plus
# its 300-second late-submit grace; reserve another two minutes for joins/fsync.
# Released v0.7 binaries do not implement that barrier and are governed by the
# separate recovered-fleet retirement gate below.
GRACEFUL_STOP_TIMEOUT_SECS=4420
# The managed system-user bridge cannot ask systemd to restart its root-owned
# replacement unit:
# its updater deliberately runs as the unprivileged community user. It sends
# SIGTERM to its own node and relies on Restart=always instead. Cover the
# v0.8 quiescence budget plus RestartSec=5 and a bounded startup allowance.
SYSTEM_USER_RESTART_TIMEOUT_SECS=$((GRACEFUL_STOP_TIMEOUT_SECS + 30))

# Community/reward RPC is explicit HTTPS configuration, separate from QUIC
# P2P discovery. Every managed node receives the same reviewed six origins;
# raw remote HTTP is deliberately not configurable through the installer.
COMMUNITY_RPC_ORIGINS=(
    https://149.28.32.76
    https://140.82.16.112
    https://136.244.109.1
    https://104.238.171.11
    https://202.182.107.41
    https://149.28.153.31
)
COMMUNITY_VALIDATOR_ADDRESSES=(
    adf4ff16f997c871c16f3897e67881311d08f975f28ebdcf79e86ea9e3b99d0f
    44d20543df6e76696da2ebbbd79e4243cd41729fa5b890e2618991e489314780
    5772741c93d8a4b04ec39007cb568a31e13ffba0d3e786596d1900d30e529f21
    228787281308d6c1a560848c2c168814bde1b6153e9e65a286d7211f04628fdd
    f03cbab49cf553a05541ddebc09b32a4c5507efb157d354b6d7f8c6682c32f5f
    f521309b041da7aefc742548bdc002c31b47183aacfbbbf245ded09845d0415b
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
  --data-dir PATH        Dedicated absolute chain data dir (default: INSTALL_DIR/data).
  --model PATH           Optional local GGUF model. Omitted means observer/router.
  --port PORT            RPC port (default: 9944).
  --p2p-port PORT        QUIC P2P port (default: RPC port + 1).
  --system-service       Linux system service (sudo/root required).
  --user-service         Linux per-user systemd service.
  --no-service           Install only; do not start or health-check a node.
  --no-auto-update       Do not install the daily checksummed updater.
  --update-only          Update an existing install; used by the timer.
  --uninstall            Remove services and programs; keep identity and data.
  --purge                Remove a marked install root; external data is kept.
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
    if [ "$#" -lt 2 ] || [ -z "$2" ]; then
        die "$1 requires a value"
    fi
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

for command_name in curl awk sed grep cmp dd sync mktemp uname id chmod mkdir cp mv dirname stat ssh-keygen ps nohup; do
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
if [ -z "$TARGET_HOME" ] || [ ! -d "$TARGET_HOME" ]; then
    die "Could not determine a home directory for $TARGET_USER"
fi

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

# Directory arguments are later passed to mkdir/chown/chmod, sometimes as
# root.  Keep their spelling unambiguous so lexical checks cannot be bypassed
# with a trailing slash, `..`, or an implementation-defined leading `//`.
# This intentionally does not resolve symlinks: managed directory paths must
# not contain them at all.
NORMALIZED_DIRECTORY_PATH=""
normalize_managed_directory_path() {
    local path_value="$1" path_label="$2"
    case "$path_value" in
        /*) ;;
        *) die "$path_label must be an absolute path (got: $path_value)" ;;
    esac
    case "$path_value" in
        *$'\n'*|*$'\r'*) die "$path_label may not contain newlines" ;;
    esac
    while [ "$path_value" != / ] && [ "${path_value%/}" != "$path_value" ]; do
        path_value="${path_value%/}"
    done
    case "$path_value" in
        *//*) die "$path_label must not contain repeated '/' components (got: $path_value)" ;;
    esac
    case "/${path_value#/}/" in
        */./*|*/../*)
            die "$path_label must not contain '.' or '..' components (got: $path_value)" ;;
    esac
    NORMALIZED_DIRECTORY_PATH="$path_value"
}

validate_managed_directory_components() {
    local path_value="$1" path_label="$2"
    local remainder="${path_value#/}" component current_path=""
    while [ -n "$remainder" ]; do
        case "$remainder" in
            */*) component="${remainder%%/*}"; remainder="${remainder#*/}" ;;
            *) component="$remainder"; remainder="" ;;
        esac
        current_path="$current_path/$component"
        [ ! -L "$current_path" ] \
            || die "Refusing $path_label with a symlink component: $current_path"
        if [ -e "$current_path" ] && [ ! -d "$current_path" ]; then
            die "Refusing $path_label with a non-directory component: $current_path"
        fi
    done
}

if [ -n "$ARG_INSTALL_DIR" ]; then
    ARC_DIR="$ARG_INSTALL_DIR"
else
    ARC_DIR="${ARC_DIR:-$DEFAULT_ARC_DIR}"
fi
normalize_managed_directory_path "$ARC_DIR" "--install-dir/ARC_DIR"
ARC_DIR="$NORMALIZED_DIRECTORY_PATH"
case "$ARC_DIR" in
    /|/Applications|/Library|/Network|/System|/Users|/Volumes|/bin|/boot|/dev|/etc|/home|/lib|/lib32|/lib64|/lost+found|/media|/mnt|/opt|/private|/proc|/root|/run|/sbin|/srv|/sys|/tmp|/usr|/usr/local|/var|/var/cache|/var/lib|/var/log|/var/run|/var/spool|/var/tmp|"$TARGET_HOME")
        die "Refusing unsafe install directory: $ARC_DIR" ;;
esac
validate_managed_directory_components "$ARC_DIR" "install directory"
ARC_DIR_PREEXISTED=false
if [ -d "$ARC_DIR" ]; then ARC_DIR_PREEXISTED=true; fi
INSTALL_ROOT_MARKER="$ARC_DIR/.arc-chain-install-root"
LEGACY_ADOPTION_MARKER="$ARC_DIR/.arc-chain-legacy-adoption-pending"
LEGACY_USER_INSTALL_ROOT="$TARGET_HOME/.arc"
LEGACY_SYSTEM_INSTALL_ROOT=/var/lib/arc-chain
LEGACY_ADOPTION_CANDIDATE=false
LEGACY_ADOPTION_ACTIVE=false
LEGACY_ADOPTION_MARKER_NEEDS_CREATE=false
LEGACY_ADOPTION_EVIDENCE_NEEDS_VALIDATION=false
LEGACY_SOURCE_VERSION=""
LEGACY_PRESERVED_DIR="$ARC_DIR/legacy-v0.7-preserved"
LEGACY_RETIREMENT_DIR="$LEGACY_PRESERVED_DIR/retirement-v0.8"
LEGACY_RETIREMENT_RELEASE_DIR="$LEGACY_RETIREMENT_DIR/release"
LEGACY_RETIREMENT_SUPERVISOR_BINDING="$LEGACY_RETIREMENT_DIR/supervisor-binding.json"
LEGACY_RETIREMENT_INTENT="$LEGACY_RETIREMENT_DIR/retirement-intent.json"
LEGACY_RETIREMENT_STOP_EVIDENCE="$LEGACY_RETIREMENT_DIR/stop-evidence.json"
LEGACY_RETIREMENT_RECEIPT="$LEGACY_RETIREMENT_DIR/retirement-receipt.json"
LEGACY_RETIREMENT_INTENT_DURABLE=false
LEGACY_RETIREMENT_INTENT_SHA256=""
LEGACY_RETIREMENT_STOP_EVIDENCE_SHA256=""
LEGACY_RETIREMENT_RECEIPT_SHA256=""
LEGACY_RETIREMENT_SUPERVISOR_BINDING_SHA256=""
LEGACY_RETIREMENT_PROCESS_PID=""
LEGACY_RETIREMENT_PROCESS_BOOT_ID=""
LEGACY_RETIREMENT_PROCESS_START_TICKS=""
LEGACY_RETIREMENT_ALREADY_OFFLINE=false
LEGACY_RETIREMENT_OBSERVATION_STARTED_AT=""
LEGACY_RETIREMENT_SUPERVISOR_MECHANISM=""
SYSTEMD_UNIT_DIR=/etc/systemd/system
LEGACY_LINUX_NODE_UNIT="$SYSTEMD_UNIT_DIR/arc-node.service"
LEGACY_LINUX_UPDATER_SERVICE="$SYSTEMD_UNIT_DIR/arc-updater.service"
LEGACY_LINUX_UPDATER_TIMER="$SYSTEMD_UNIT_DIR/arc-updater.timer"
LEGACY_MAC_NODE_PLIST="$TARGET_HOME/Library/LaunchAgents/com.arc.inference.plist"
LEGACY_MAC_UPDATER_PLIST="$TARGET_HOME/Library/LaunchAgents/com.arc.updater.plist"
LEGACY_NODE_PID_FILE="$ARC_DIR/node.pid"
LEGACY_SUPERVISOR_KIND=none
LEGACY_MARKER_SUPERVISOR_KIND=""
LEGACY_MARKER_SERVICE_SCOPE=""
LEGACY_MARKER_RPC_PORT=""
LEGACY_MARKER_P2P_PORT=""
LEGACY_MARKER_MODEL_PATH=""
LEGACY_DETACHED_PID=""
LEGACY_DETACHED_WAS_RUNNING=false
LEGACY_DETACHED_HAD_PID_FILE=false
LEGACY_DETACHED_COMMUNITY_MODE=false
LEGACY_LINUX_NODE_ACTIVE=false
LEGACY_LINUX_NODE_ENABLED=false
LEGACY_MAC_NODE_LOADED=false
LEGACY_MAC_UPDATER_LOADED=false
LEGACY_PARTIAL_RESUME=false
LEGACY_DETACHED_RESTART_ARGS=()
SYSTEM_USER_BOOTSTRAP=false
if [ "$UNINSTALL" = false ] && [ "$ARC_DIR_PREEXISTED" = true ] \
    && [ ! -e "$INSTALL_ROOT_MARKER" ] && [ ! -L "$INSTALL_ROOT_MARKER" ]; then
    case "$ARC_DIR" in
        "$LEGACY_USER_INSTALL_ROOT"|"$LEGACY_SYSTEM_INSTALL_ROOT")
            LEGACY_ADOPTION_CANDIDATE=true ;;
    esac
fi

# Read only a strict key/value data file. Never source user-writable config:
# the system updater runs as root and sourcing it would be privilege escalation.
CONFIG_FILE="$ARC_DIR/install.conf"
SEED_FILE="$ARC_DIR/identity/validator-seed"
KEY_FILE="$ARC_DIR/identity/validator-key.json"
LEGACY_SEED_EVIDENCE="$ARC_DIR/identity/legacy-validator-seed.evidence"
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
    if [ -n "$SAVED_DATA_DIR" ]; then
        NODE_DATA_DIR="$SAVED_DATA_DIR"
    elif [ "$LEGACY_ADOPTION_CANDIDATE" = true ]; then
        # v0.7 data is preserved for forensics and never reused as v0.8 state.
        NODE_DATA_DIR="$ARC_DIR/data-v0.8"
    else
        NODE_DATA_DIR="$ARC_DIR/data"
    fi
fi
normalize_managed_directory_path "$NODE_DATA_DIR" "--data-dir/ARC_NODE_DATA_DIR"
NODE_DATA_DIR="$NORMALIZED_DIRECTORY_PATH"
if [ "$MODEL_WAS_SET" = true ]; then
    MODEL_PATH="${ARG_MODEL_PATH:-${ARC_MODEL_PATH:-}}"
else
    MODEL_PATH="$SAVED_MODEL_PATH"
fi

# A pending v0.7 adoption binds the manager choice as well as the data path.
# Read only the narrow scope hint here so a crash after retiring a legacy
# system supervisor resumes with the same privilege boundary. The complete
# marker is authenticated and compared byte-for-byte before any mutation.
PENDING_LEGACY_SCOPE=""
PENDING_LEGACY_RPC_PORT=""
PENDING_LEGACY_P2P_PORT=""
PENDING_LEGACY_MODEL_PATH=""
if [ "$LEGACY_ADOPTION_CANDIDATE" = true ] \
    && [ -f "$LEGACY_ADOPTION_MARKER" ] && [ ! -L "$LEGACY_ADOPTION_MARKER" ] \
    && [ "$(sed -n '1p' "$LEGACY_ADOPTION_MARKER")" = \
        'arc-chain-legacy-adoption-pending-v2' ]; then
    PENDING_LEGACY_SCOPE="$(sed -n '5s/^service_scope=//p' "$LEGACY_ADOPTION_MARKER")"
    case "$PENDING_LEGACY_SCOPE" in
        system-user|user|launchd|none) ;;
        *) PENDING_LEGACY_SCOPE="" ;;
    esac
    if [ -n "$PENDING_LEGACY_SCOPE" ]; then
        PENDING_LEGACY_RPC_PORT="$(sed -n '7s/^rpc_port=//p' "$LEGACY_ADOPTION_MARKER")"
        PENDING_LEGACY_P2P_PORT="$(sed -n '8s/^p2p_port=//p' "$LEGACY_ADOPTION_MARKER")"
        PENDING_LEGACY_MODEL_PATH="$(sed -n '9s/^model_path=//p' "$LEGACY_ADOPTION_MARKER")"
        if [ "$RPC_WAS_SET" = false ]; then RPC_PORT="$PENDING_LEGACY_RPC_PORT"; fi
        if [ "$P2P_WAS_SET" = false ]; then P2P_PORT="$PENDING_LEGACY_P2P_PORT"; fi
        if [ "$MODEL_WAS_SET" = false ]; then MODEL_PATH="$PENDING_LEGACY_MODEL_PATH"; fi
    fi
fi

if [ "$SERVICE_WAS_SET" = true ]; then
    SERVICE_SCOPE="${ARG_SERVICE_SCOPE:-${ARC_INSTALL_SCOPE:-}}"
elif [ -n "$PENDING_LEGACY_SCOPE" ]; then
    SERVICE_SCOPE="$PENDING_LEGACY_SCOPE"
elif [ -n "$SAVED_SERVICE_SCOPE" ]; then
    SERVICE_SCOPE="$SAVED_SERVICE_SCOPE"
elif [ "$OS" = Darwin ]; then
    SERVICE_SCOPE=launchd
elif [ "$CURRENT_UID" -eq 0 ]; then
    SERVICE_SCOPE=system
else
    SERVICE_SCOPE=user
fi

# The real v0.7 community installer put a user-owned ~/.arc behind a
# root-owned system service. Preserve that safe split automatically: the new
# system units stay root-owned, while both the node and updater execute as the
# community user and every file under ~/.arc remains user-owned. This avoids a
# root updater ever executing from a user-replaceable home directory.
if [ "$OS" = Linux ] && [ "$LEGACY_ADOPTION_CANDIDATE" = true ] \
    && [ "$ARC_DIR" = "$LEGACY_USER_INSTALL_ROOT" ] \
    && [ "$INSTALL_SERVICE" = true ] && [ "$SERVICE_WAS_SET" = false ] \
    && { [ -e "$LEGACY_LINUX_NODE_UNIT" ] || [ -L "$LEGACY_LINUX_NODE_UNIT" ]; }; then
    SERVICE_SCOPE=system-user
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
    system-user)
        [ "$OS" = Linux ] || die "system-user service scope is supported only on Linux"
        [ "$ARC_DIR" = "$LEGACY_USER_INSTALL_ROOT" ] \
            || die "system-user scope is reserved for the managed per-user ARC root" ;;
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

# Never chmod/chown a shared operating-system tree. Dedicated directories
# below /var/lib, /srv, /opt, mounted volumes, or the install user's home are
# supported; the roots themselves and executable/config/runtime namespaces are
# not. Reject descendants of namespaces where chain data never belongs too.
case "$NODE_DATA_DIR" in
    /|/bin|/bin/*|/boot|/boot/*|/dev|/dev/*|/etc|/etc/*|/lib|/lib/*|/lib32|/lib32/*|/lib64|/lib64/*|/proc|/proc/*|/root|/root/*|/run|/run/*|/sbin|/sbin/*|/sys|/sys/*|/tmp|/tmp/*|/usr|/usr/*|/var|/var/cache|/var/cache/*|/var/lib|/var/log|/var/log/*|/var/run|/var/run/*|/var/spool|/var/spool/*|/var/tmp|/var/tmp/*|/opt|/srv|/mnt|/media|/home|/Users|/Applications|/Applications/*|/Library|/Library/*|/System|/System/*|/private/etc|/private/etc/*|/private/tmp|/private/tmp/*|/private/usr|/private/usr/*|/private/var|/private/var/log|/private/var/log/*|/private/var/run|/private/var/run/*|/private/var/tmp|/private/var/tmp/*|"$TARGET_HOME")
        die "Refusing unsafe data directory; choose a dedicated ARC path: $NODE_DATA_DIR" ;;
esac
if [ "$NODE_DATA_DIR" = "$ARC_DIR" ]; then
    die "Data directory must be separate from the ARC program/config root: $NODE_DATA_DIR"
fi
case "$ARC_DIR/" in
    "$NODE_DATA_DIR"/*)
        die "Data directory must not contain the ARC program/config root: $NODE_DATA_DIR" ;;
esac
case "$NODE_DATA_DIR" in
    "$ARC_DIR/bin"|"$ARC_DIR/bin/"*|"$ARC_DIR/identity"|"$ARC_DIR/identity/"*)
        die "Data directory overlaps a managed ARC program/identity path: $NODE_DATA_DIR" ;;
esac
validate_managed_directory_components "$NODE_DATA_DIR" "data directory"

if [ -n "$MODEL_PATH" ]; then
    case "$MODEL_PATH" in
        /*) ;;
        *) die "Model path must be absolute (got: $MODEL_PATH)" ;;
    esac
    case "$MODEL_PATH" in *$'\n'*|*$'\r'*) die "Model path may not contain newlines" ;; esac
fi
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

# Execute a secret-file operation as the file's exact owner. The Rust CLI
# independently revalidates ownership through the opened handle, so a path
# swap between this lookup and open fails closed instead of changing trust.
as_private_file_owner() {
    local private_path="$1" owner_uid
    shift
    owner_uid="$(portable_file_uid "$private_path")" \
        || die "Could not inspect private identity file owner: $private_path"
    if [ "$owner_uid" -eq "$CURRENT_UID" ]; then
        "$@"
    elif [ "$owner_uid" -eq "$TARGET_UID" ]; then
        as_target "$@"
    else
        die "Private identity file is not owned by the installer or target user: $private_path"
    fi
}

install_root_marker_expected() {
    printf '%s\n' \
        'arc-chain-managed-install-root-v1' \
        "path=$ARC_DIR"
}

portable_file_uid() {
    stat -c %u -- "$1" 2>/dev/null || stat -f '%u' "$1"
}

portable_file_mode() {
    stat -c %a -- "$1" 2>/dev/null || stat -f '%Lp' "$1"
}

validate_install_root_marker() {
    local expected_uid marker_uid marker_mode marker_permissions
    [ ! -L "$INSTALL_ROOT_MARKER" ] \
        || die "Refusing symlinked ARC install-root marker: $INSTALL_ROOT_MARKER"
    [ -f "$INSTALL_ROOT_MARKER" ] \
        || die "Refusing unmarked install directory: $ARC_DIR"
    cmp -s "$INSTALL_ROOT_MARKER" <(install_root_marker_expected) \
        || die "ARC install-root marker is not bound to this exact directory: $ARC_DIR"

    if [ "$SERVICE_SCOPE" = system ]; then expected_uid=0
    else expected_uid="$TARGET_UID"
    fi
    marker_uid="$(portable_file_uid "$INSTALL_ROOT_MARKER")" \
        || die "Could not inspect ARC install-root marker owner: $INSTALL_ROOT_MARKER"
    [ "$marker_uid" -eq "$expected_uid" ] \
        || die "ARC install-root marker has the wrong owner: $INSTALL_ROOT_MARKER"
    marker_mode="$(portable_file_mode "$INSTALL_ROOT_MARKER")" \
        || die "Could not inspect ARC install-root marker permissions: $INSTALL_ROOT_MARKER"
    case "$marker_mode" in
        ''|*[!0-7]*) die "Could not parse ARC install-root marker permissions" ;;
    esac
    marker_permissions=$((8#$marker_mode))
    [ $((marker_permissions & 0022)) -eq 0 ] \
        || die "ARC install-root marker must not be group/world writable: $INSTALL_ROOT_MARKER"
}

install_root_as_owner() {
    if [ "$SERVICE_SCOPE" = system ]; then as_root "$@"
    else as_target "$@"
    fi
}

create_marked_install_root() {
    local install_parent marker_payload staged_root marker_mode root_mode
    install_parent="$(dirname -- "$ARC_DIR")"
    install_root_as_owner mkdir -p -- "$install_parent" \
        || die "Could not create ARC install parent: $install_parent"
    marker_payload="$(mktemp "${TMPDIR:-/tmp}/arc-install-root-marker.XXXXXX")" \
        || die "Could not stage the ARC install-root marker"
    install_root_marker_expected > "$marker_payload"
    chmod 644 "$marker_payload"
    staged_root="$(install_root_as_owner mktemp -d "$install_parent/.arc-install-root.new.XXXXXX")" \
        || {
            rm -f -- "$marker_payload"
            die "Could not reserve the ARC install root beneath $install_parent"
        }
    if [ "$SERVICE_SCOPE" = system ]; then marker_mode=444; root_mode=755
    else marker_mode=600; root_mode=700
    fi
    if ! install_root_as_owner cp -- "$marker_payload" "$staged_root/.arc-chain-install-root" \
        || ! install_root_as_owner chmod "$marker_mode" "$staged_root/.arc-chain-install-root" \
        || ! install_root_as_owner chmod "$root_mode" "$staged_root"; then
        rm -f -- "$marker_payload"
        install_root_as_owner rm -rf -- "$staged_root" || true
        die "Could not initialize the ARC install-root marker"
    fi
    rm -f -- "$marker_payload"
    if ! install_root_as_owner mv -n -- "$staged_root" "$ARC_DIR" \
        || [ -e "$staged_root" ]; then
        install_root_as_owner rm -rf -- "$staged_root" || true
        die "Install directory appeared while ARC was reserving it: $ARC_DIR"
    fi
    validate_install_root_marker
}

legacy_marker_expected() {
    printf '%s\n' \
        'arc-chain-legacy-adoption-pending-v2' \
        "path=$ARC_DIR" \
        "source_version=$LEGACY_SOURCE_VERSION" \
        "data_dir=$NODE_DATA_DIR" \
        "service_scope=$SERVICE_SCOPE" \
        "supervisor_kind=$LEGACY_SUPERVISOR_KIND" \
        "rpc_port=$RPC_PORT" \
        "p2p_port=$P2P_PORT" \
        "model_path=$MODEL_PATH"
}

legacy_expected_owner_uid() {
    if [ "$ARC_DIR" = "$LEGACY_SYSTEM_INSTALL_ROOT" ]; then printf '0\n'
    else printf '%s\n' "$TARGET_UID"
    fi
}

validate_owned_nonwritable_path() {
    local path="$1" path_label="$2" path_kind="$3" expected_uid="$4"
    local actual_uid actual_mode actual_permissions
    [ ! -L "$path" ] || die "Legacy $path_label must not be a symlink: $path"
    case "$path_kind" in
        directory) [ -d "$path" ] || die "Legacy $path_label is not a directory: $path" ;;
        executable)
            if [ ! -f "$path" ] || [ ! -x "$path" ]; then
                die "Legacy $path_label is not an executable regular file: $path"
            fi ;;
        file) [ -f "$path" ] || die "Legacy $path_label is not a regular file: $path" ;;
        *) die "Internal legacy path validation error: $path_kind" ;;
    esac
    actual_uid="$(portable_file_uid "$path")" \
        || die "Could not inspect legacy $path_label owner: $path"
    [ "$actual_uid" -eq "$expected_uid" ] \
        || die "Legacy $path_label has an unexpected owner: $path"
    actual_mode="$(portable_file_mode "$path")" \
        || die "Could not inspect legacy $path_label permissions: $path"
    case "$actual_mode" in
        ''|*[!0-7]*) die "Could not parse legacy $path_label permissions: $path" ;;
    esac
    actual_permissions=$((8#$actual_mode))
    [ $((actual_permissions & 0022)) -eq 0 ] \
        || die "Legacy $path_label must not be group/world writable: $path"
}

validate_legacy_default_ancestor_chain() {
    local expected_uid="$1" remainder component current_path=""
    local ancestor_uid ancestor_mode ancestor_permissions

    ancestor_uid="$(portable_file_uid /)" \
        || die "Could not inspect legacy ARC ancestor: /"
    [ "$ancestor_uid" -eq 0 ] \
        || die "Legacy ARC filesystem root must be root-owned"
    ancestor_mode="$(portable_file_mode /)" \
        || die "Could not inspect legacy ARC ancestor permissions: /"
    case "$ancestor_mode" in
        ''|*[!0-7]*) die "Could not parse legacy ARC ancestor permissions: /" ;;
    esac
    ancestor_permissions=$((8#$ancestor_mode))
    [ $((ancestor_permissions & 0022)) -eq 0 ] \
        || die "Legacy ARC filesystem root must not be group/world writable"

    remainder="${ARC_DIR#/}"
    while [ -n "$remainder" ]; do
        case "$remainder" in
            */*) component="${remainder%%/*}"; remainder="${remainder#*/}" ;;
            *) component="$remainder"; remainder="" ;;
        esac
        current_path="$current_path/$component"
        [ ! -L "$current_path" ] \
            || die "Legacy ARC install has a symlink component: $current_path"
        [ -d "$current_path" ] \
            || die "Legacy ARC install has a non-directory component: $current_path"
        ancestor_uid="$(portable_file_uid "$current_path")" \
            || die "Could not inspect legacy ARC ancestor: $current_path"
        if [ "$ARC_DIR" = "$LEGACY_SYSTEM_INSTALL_ROOT" ]; then
            [ "$ancestor_uid" -eq 0 ] \
                || die "Legacy system ARC ancestors must be root-owned: $current_path"
        else
            [ "$ancestor_uid" -eq 0 ] || [ "$ancestor_uid" -eq "$expected_uid" ] \
                || die "Legacy ARC ancestor has an unexpected owner: $current_path"
        fi
        ancestor_mode="$(portable_file_mode "$current_path")" \
            || die "Could not inspect legacy ARC ancestor permissions: $current_path"
        case "$ancestor_mode" in
            ''|*[!0-7]*) die "Could not parse legacy ARC ancestor permissions: $current_path" ;;
        esac
        ancestor_permissions=$((8#$ancestor_mode))
        [ $((ancestor_permissions & 0022)) -eq 0 ] \
            || die "Legacy ARC ancestor must not be group/world writable: $current_path"
    done
    validate_owned_nonwritable_path "$ARC_DIR" "install root" directory "$expected_uid"
}

read_managed_binary_version() {
    local binary_path="$1" version_output
    version_output="$(install_root_as_owner "$binary_path" --version 2>/dev/null)" \
        || die "Legacy ARC binary did not report a version: $binary_path"
    printf '%s\n' "$version_output" \
        | sed -nE 's/.*[^0-9]([0-9]+\.[0-9]+\.[0-9]+).*/\1/p' \
        | head -n 1
}

validate_legacy_v07_layout() {
    local expected_uid legacy_version_file legacy_seed line_count
    expected_uid="$(legacy_expected_owner_uid)"
    if [ "$ARC_DIR" = "$LEGACY_SYSTEM_INSTALL_ROOT" ]; then
        [ "$SERVICE_SCOPE" = system ] \
            || die "A legacy system-default install may be adopted only with --system-service"
    else
        [ "$SERVICE_SCOPE" != system ] \
            || die "A legacy user-default install may not be adopted by a root system service"
    fi
    validate_legacy_default_ancestor_chain "$expected_uid"
    validate_owned_nonwritable_path "$ARC_DIR/bin" "bin directory" directory "$expected_uid"
    validate_owned_nonwritable_path "$ARC_DIR/bin/arc-node" "node binary" executable "$expected_uid"
    validate_owned_nonwritable_path "$ARC_DIR/version.txt" "version file" file "$expected_uid"
    validate_owned_nonwritable_path "$ARC_DIR/seeds.txt" "seed configuration" file "$expected_uid"
    validate_owned_nonwritable_path "$ARC_DIR/genesis.toml" "genesis configuration" file "$expected_uid"
    validate_owned_nonwritable_path "$ARC_DIR/identity.seed" "identity seed" file "$expected_uid"
    validate_owned_nonwritable_path "$ARC_DIR/data" "data directory" directory "$expected_uid"

    LEGACY_SOURCE_VERSION="$(read_managed_binary_version "$ARC_DIR/bin/arc-node")"
    printf '%s\n' "$LEGACY_SOURCE_VERSION" | grep -Eq '^0\.7\.[0-9]+$' \
        || die "Only a recognized ARC v0.7.x default install can be adopted"
    legacy_version_file="$(sed -n '1p' "$ARC_DIR/version.txt")"
    legacy_version_file="${legacy_version_file#v}"
    [ "$legacy_version_file" = "$LEGACY_SOURCE_VERSION" ] \
        || die "Legacy ARC version.txt does not match the installed v0.7.x binary"
    [ "$(awk 'END { print NR + 0 }' "$ARC_DIR/version.txt")" -eq 1 ] \
        || die "Legacy ARC version.txt must contain exactly one version line"
    grep -Eq '^[[:space:]]*chain_id[[:space:]]*=[[:space:]]*"0x415243"' \
        "$ARC_DIR/genesis.toml" \
        || die "Legacy ARC genesis does not identify ARC chain 0x415243"
    grep -Eq '^[[:space:]]*[0-9]{1,3}(\.[0-9]{1,3}){3}:[0-9]{1,5}([[:space:]]|$)' \
        "$ARC_DIR/seeds.txt" \
        || die "Legacy ARC seed configuration has no recognized peer entry"
    legacy_seed="$(sed -n '1p' "$ARC_DIR/identity.seed")"
    line_count="$(awk 'END { print NR + 0 }' "$ARC_DIR/identity.seed")"
    if [ "$line_count" -ne 1 ] \
        || ! printf '%s\n' "$legacy_seed" \
            | grep -Eq '^community-[A-Za-z0-9][A-Za-z0-9-]{0,127}$'; then
        die "Legacy ARC identity seed does not match the v0.7 community-node format"
    fi
    if [ -e "$ARC_DIR/bin/arc-auto-update.sh" ] || [ -L "$ARC_DIR/bin/arc-auto-update.sh" ]; then
        validate_owned_nonwritable_path "$ARC_DIR/bin/arc-auto-update.sh" \
            "updater" executable "$expected_uid"
        grep -Fq 'ARC Chain auto-updater' "$ARC_DIR/bin/arc-auto-update.sh" \
            || die "Legacy ARC updater does not match the recognized community-node layout"
    fi
    if [ -n "$MODEL_PATH" ]; then
        case "$MODEL_PATH" in
            "$ARC_DIR/"*)
                case "$MODEL_PATH" in
                    *//*) die "Legacy ARC model path must not contain repeated '/' components" ;;
                esac
                case "/${MODEL_PATH#/}/" in
                    */./*|*/../*)
                        die "Legacy ARC model path must not contain '.' or '..' components" ;;
                esac
                validate_managed_directory_components "$(dirname -- "$MODEL_PATH")" \
                    "legacy model path"
                validate_owned_nonwritable_path "$MODEL_PATH" "model" file "$expected_uid" ;;
        esac
    fi
}

legacy_path_present() {
    [ -e "$1" ] || [ -L "$1" ]
}

LEGACY_ARGUMENT_VALUE=""
read_single_legacy_systemd_argument() {
    local unit_path="$1" argument_name="$2" required="${3:-true}"
    local extracted count
    extracted="$(awk -v name="$argument_name" '
        {
            for (i = 1; i <= NF; i++) {
                if ($i == name) {
                    count += 1
                    if (i == NF || $(i + 1) == "\\") invalid = 1
                    value = $(i + 1)
                }
            }
        }
        END {
            if (invalid) exit 2
            if (count > 1) exit 3
            if (count == 1) print value
        }
    ' "$unit_path")" \
        || die "Legacy Linux node unit has an ambiguous $argument_name argument"
    count="$(awk -v name="$argument_name" \
        '{ for (i = 1; i <= NF; i++) if ($i == name) count += 1 } END { print count + 0 }' \
        "$unit_path")"
    if [ "$required" = true ]; then
        [ "$count" -eq 1 ] \
            || die "Legacy Linux node unit must contain exactly one $argument_name argument"
    else
        [ "$count" -le 1 ] \
            || die "Legacy Linux node unit contains duplicate $argument_name arguments"
    fi
    LEGACY_ARGUMENT_VALUE="$extracted"
}

legacy_systemd_exec_tokens() {
    awk '
        /^ExecStart=/ {
            if (seen) exit 3
            seen = 1
            in_exec = 1
            first_line = 1
        }
        in_exec {
            line = $0
            if (first_line) {
                sub(/^ExecStart=/, "", line)
                first_line = 0
            }
            continued = (line ~ /\\[[:space:]]*$/)
            sub(/[[:space:]]*\\[[:space:]]*$/, "", line)
            field_count = split(line, fields, /[[:space:]]+/)
            for (i = 1; i <= field_count; i++) {
                if (fields[i] != "") print fields[i]
            }
            if (!continued) in_exec = 0
            next
        }
        END {
            if (seen != 1 || in_exec) exit 2
        }
    ' "$1"
}

plist_program_arguments() {
    awk '
        /<key>ProgramArguments<\/key>/ {
            key_count += 1
            if (key_count > 1) exit 3
            in_arguments = 1
            next
        }
        in_arguments && /<\/array>/ { exit }
        in_arguments {
            line = $0
            while (match(line, /<string>[^<]*<\/string>/)) {
                value = substr(line, RSTART + 8, RLENGTH - 17)
                print value
                line = substr(line, RSTART + RLENGTH)
            }
        }
        END { if (key_count != 1) exit 2 }
    ' "$1"
}

read_single_legacy_plist_argument() {
    local plist_path="$1" argument_name="$2" required="${3:-true}"
    local extracted count
    extracted="$(plist_program_arguments "$plist_path" \
        | awk -v name="$argument_name" '
            $0 == name {
                count += 1
                if ((getline value) <= 0) exit 2
                result = value
            }
            END {
                if (count > 1) exit 3
                if (count == 1) print result
            }
        ')" || die "Legacy macOS agent has an ambiguous $argument_name argument"
    count="$(plist_program_arguments "$plist_path" \
        | awk -v name="$argument_name" '$0 == name { count += 1 } END { print count + 0 }')" \
        || die "Could not parse legacy macOS ProgramArguments"
    if [ "$required" = true ]; then
        [ "$count" -eq 1 ] \
            || die "Legacy macOS agent must contain exactly one $argument_name argument"
    else
        [ "$count" -le 1 ] \
            || die "Legacy macOS agent contains duplicate $argument_name arguments"
    fi
    LEGACY_ARGUMENT_VALUE="$extracted"
}

read_single_legacy_command_argument() {
    local command_line="$1" argument_name="$2" required="${3:-true}"
    local extracted count
    extracted="$(printf '%s\n' "$command_line" \
        | awk -v name="$argument_name" '
            {
                for (i = 1; i <= NF; i++) {
                    if ($i == name) {
                        count += 1
                        if (i == NF) invalid = 1
                        value = $(i + 1)
                    }
                }
            }
            END {
                if (invalid || count > 1) exit 2
                if (count == 1) print value
            }
        ')" || die "Legacy detached node has an ambiguous $argument_name argument"
    count="$(printf '%s\n' "$command_line" \
        | awk -v name="$argument_name" '{ for (i = 1; i <= NF; i++) if ($i == name) count += 1 } END { print count + 0 }')"
    if [ "$required" = true ]; then
        [ "$count" -eq 1 ] \
            || die "Legacy detached node must contain exactly one $argument_name argument"
    else
        [ "$count" -le 1 ] \
            || die "Legacy detached node contains duplicate $argument_name arguments"
    fi
    LEGACY_ARGUMENT_VALUE="$extracted"
}

apply_legacy_runtime_configuration() {
    local legacy_rpc_argument="$1" legacy_p2p_port="$2" legacy_model_path="$3"
    local legacy_rpc_host legacy_rpc_port
    case "$legacy_rpc_argument" in
        *:*) legacy_rpc_host="${legacy_rpc_argument%:*}"; legacy_rpc_port="${legacy_rpc_argument##*:}" ;;
        *) die "Legacy supervisor has an invalid RPC argument" ;;
    esac
    [ "$legacy_rpc_host" = 0.0.0.0 ] \
        || die "Legacy supervisor uses an unexpected RPC bind address"
    valid_port "$legacy_rpc_port" || die "Legacy supervisor has an invalid RPC port"
    valid_port "$legacy_p2p_port" || die "Legacy supervisor has an invalid P2P port"
    [ "$legacy_rpc_port" != "$legacy_p2p_port" ] \
        || die "Legacy supervisor reuses one RPC/P2P port"
    case "$legacy_model_path" in
        '') ;;
        /*)
            case "$legacy_model_path" in *$'\n'*|*$'\r'*) die "Legacy model path contains a newline" ;; esac
            [ -f "$legacy_model_path" ] \
                || die "Legacy supervisor model path is not a regular file: $legacy_model_path" ;;
        *) die "Legacy supervisor model path is not absolute: $legacy_model_path" ;;
    esac
    if [ "$RPC_WAS_SET" = false ]; then RPC_PORT="$legacy_rpc_port"; fi
    if [ "$P2P_WAS_SET" = false ]; then P2P_PORT="$legacy_p2p_port"; fi
    if [ "$MODEL_WAS_SET" = false ]; then MODEL_PATH="$legacy_model_path"; fi
    valid_port "$RPC_PORT" || die "Invalid adopted RPC port: $RPC_PORT"
    valid_port "$P2P_PORT" || die "Invalid adopted P2P port: $P2P_PORT"
    [ "$RPC_PORT" != "$P2P_PORT" ] \
        || die "Adopted RPC and P2P ports must be different"
}

validate_legacy_unit_directive_names() {
    local unit_path="$1" allowed_names="$2" unit_label="$3"
    awk -F= -v allowed=" $allowed_names " '
        /^[A-Za-z][A-Za-z0-9]*=/ {
            if (index(allowed, " " $1 " ") == 0) exit 2
        }
    ' "$unit_path" \
        || die "Legacy $unit_label contains an unexpected systemd directive"
}

validate_legacy_plist_keys() {
    local plist_path="$1" allowed_keys="$2" plist_label="$3"
    awk -v allowed=" $allowed_keys " '
        {
            line = $0
            while (match(line, /<key>[^<]*<\/key>/)) {
                value = substr(line, RSTART + 5, RLENGTH - 11)
                if (index(allowed, " " value " ") == 0) exit 2
                line = substr(line, RSTART + RLENGTH)
            }
        }
    ' "$plist_path" \
        || die "Legacy $plist_label contains an unexpected launchd key"
}

validate_legacy_linux_units() {
    local updater_service_present=false updater_timer_present=false
    local legacy_rpc legacy_p2p legacy_model legacy_seed legacy_pair
    local legacy_exec_token_count expected_exec_token_count=20
    validate_owned_nonwritable_path "$LEGACY_LINUX_NODE_UNIT" \
        "Linux system node unit" file 0
    validate_legacy_unit_directive_names "$LEGACY_LINUX_NODE_UNIT" \
        'Description After Type User WorkingDirectory Environment ExecStart Restart RestartSec StandardOutput StandardError WantedBy' \
        'Linux node unit'
    [ "$(grep -Ec '^ExecStart=' "$LEGACY_LINUX_NODE_UNIT")" -eq 1 ] \
        || die "Legacy Linux node unit must contain exactly one ExecStart"
    if [ "$(grep -Ec '^User=' "$LEGACY_LINUX_NODE_UNIT")" -ne 1 ] \
        || ! grep -Fqx "User=$TARGET_USER" "$LEGACY_LINUX_NODE_UNIT"; then
        die "Legacy Linux node unit does not run as $TARGET_USER"
    fi
    if [ "$(grep -Ec '^WorkingDirectory=' "$LEGACY_LINUX_NODE_UNIT")" -ne 1 ] \
        || ! grep -Fqx "WorkingDirectory=$ARC_DIR" "$LEGACY_LINUX_NODE_UNIT"; then
        die "Legacy Linux node unit targets a different working directory"
    fi
    if [ "$(grep -Ec '^Environment=' "$LEGACY_LINUX_NODE_UNIT")" -ne 1 ] \
        || ! grep -Fqx "Environment=ARC_DIR=$ARC_DIR" "$LEGACY_LINUX_NODE_UNIT"; then
        die "Legacy Linux node unit targets a different ARC environment"
    fi
    grep -Fqx "ExecStart=$ARC_DIR/bin/arc-node \\" "$LEGACY_LINUX_NODE_UNIT" \
        || die "Legacy Linux node unit targets an unexpected executable"
    if grep -Eq '^(Exec(StartPre|StartPost|Stop|StopPost|Reload|Condition)|PermissionsStartOnly|RootDirectory|RootImage|DynamicUser|Group|SupplementaryGroups)=' \
        "$LEGACY_LINUX_NODE_UNIT"; then
        die "Legacy Linux node unit contains an unexpected privilege or lifecycle directive"
    fi

    read_single_legacy_systemd_argument "$LEGACY_LINUX_NODE_UNIT" --rpc
    legacy_rpc="$LEGACY_ARGUMENT_VALUE"
    read_single_legacy_systemd_argument "$LEGACY_LINUX_NODE_UNIT" --p2p-port
    legacy_p2p="$LEGACY_ARGUMENT_VALUE"
    read_single_legacy_systemd_argument "$LEGACY_LINUX_NODE_UNIT" --seeds-file
    [ "$LEGACY_ARGUMENT_VALUE" = "$ARC_DIR/seeds.txt" ] \
        || die "Legacy Linux node unit targets unexpected seeds"
    read_single_legacy_systemd_argument "$LEGACY_LINUX_NODE_UNIT" --genesis
    [ "$LEGACY_ARGUMENT_VALUE" = "$ARC_DIR/genesis.toml" ] \
        || die "Legacy Linux node unit targets unexpected genesis"
    read_single_legacy_systemd_argument "$LEGACY_LINUX_NODE_UNIT" --validator-seed
    legacy_seed="$(sed -n '1p' "$ARC_DIR/identity.seed")"
    [ "$LEGACY_ARGUMENT_VALUE" = "$legacy_seed" ] \
        || die "Legacy Linux node unit targets an unexpected identity"
    for legacy_pair in '--stake:0' '--min-stake:0' '--eth-rpc-port:0'; do
        read_single_legacy_systemd_argument "$LEGACY_LINUX_NODE_UNIT" "${legacy_pair%%:*}"
        [ "$LEGACY_ARGUMENT_VALUE" = "${legacy_pair#*:}" ] \
            || die "Legacy Linux node unit has an unexpected ${legacy_pair%%:*} value"
    done
    read_single_legacy_systemd_argument "$LEGACY_LINUX_NODE_UNIT" --data-dir
    [ "$LEGACY_ARGUMENT_VALUE" = "$ARC_DIR/data" ] \
        || die "Legacy Linux node unit targets unexpected chain data"
    read_single_legacy_systemd_argument "$LEGACY_LINUX_NODE_UNIT" --model false
    legacy_model="$LEGACY_ARGUMENT_VALUE"
    [ "$(awk '{ for (i = 1; i <= NF; i++) if ($i == "--community-mode") count += 1 } END { print count + 0 }' \
        "$LEGACY_LINUX_NODE_UNIT")" -eq 1 ] \
        || die "Legacy Linux node unit has an unexpected community-mode argument"
    [ -z "$legacy_model" ] || expected_exec_token_count=22
    legacy_exec_token_count="$(legacy_systemd_exec_tokens "$LEGACY_LINUX_NODE_UNIT" \
        | awk 'END { print NR + 0 }')" \
        || die "Could not parse the legacy Linux node command"
    [ "$legacy_exec_token_count" -eq "$expected_exec_token_count" ] \
        || die "Legacy Linux node unit contains unexpected executable arguments"
    apply_legacy_runtime_configuration "$legacy_rpc" "$legacy_p2p" "$legacy_model"

    legacy_path_present "$LEGACY_LINUX_UPDATER_SERVICE" \
        && updater_service_present=true
    legacy_path_present "$LEGACY_LINUX_UPDATER_TIMER" \
        && updater_timer_present=true
    [ "$updater_service_present" = "$updater_timer_present" ] \
        || die "Legacy Linux updater service/timer is incomplete; refusing partial retirement"
    if [ "$updater_service_present" = true ]; then
        validate_owned_nonwritable_path "$LEGACY_LINUX_UPDATER_SERVICE" \
            "Linux updater service" file 0
        validate_owned_nonwritable_path "$LEGACY_LINUX_UPDATER_TIMER" \
            "Linux updater timer" file 0
        validate_legacy_unit_directive_names "$LEGACY_LINUX_UPDATER_SERVICE" \
            'Description Type User Environment ExecStart' 'Linux updater service'
        validate_legacy_unit_directive_names "$LEGACY_LINUX_UPDATER_TIMER" \
            'Description OnCalendar Persistent WantedBy' 'Linux updater timer'
        [ "$(grep -Ec '^ExecStart=' "$LEGACY_LINUX_UPDATER_SERVICE")" -eq 1 ] \
            || die "Legacy Linux updater service must contain exactly one ExecStart"
        if [ "$(grep -Ec '^User=' "$LEGACY_LINUX_UPDATER_SERVICE")" -ne 1 ] \
            || ! grep -Fqx "User=$TARGET_USER" "$LEGACY_LINUX_UPDATER_SERVICE"; then
            die "Legacy Linux updater service does not run as $TARGET_USER"
        fi
        if [ "$(grep -Ec '^Environment=' "$LEGACY_LINUX_UPDATER_SERVICE")" -ne 1 ] \
            || ! grep -Fqx "Environment=ARC_DIR=$ARC_DIR" \
                "$LEGACY_LINUX_UPDATER_SERVICE"; then
            die "Legacy Linux updater service targets a different ARC root"
        fi
        grep -Fqx "ExecStart=$ARC_DIR/bin/arc-auto-update.sh" \
            "$LEGACY_LINUX_UPDATER_SERVICE" \
            || die "Legacy Linux updater service targets an unexpected executable"
        if grep -Eq '^(Exec(StartPre|StartPost|Stop|StopPost|Reload|Condition)|PermissionsStartOnly|RootDirectory|RootImage|DynamicUser|Group|SupplementaryGroups)=' \
            "$LEGACY_LINUX_UPDATER_SERVICE"; then
            die "Legacy Linux updater service contains an unexpected privilege or lifecycle directive"
        fi
        if [ "$(grep -Ec '^OnCalendar=' "$LEGACY_LINUX_UPDATER_TIMER")" -ne 1 ] \
            || ! grep -Fqx 'OnCalendar=*-*-* 04:17:00' "$LEGACY_LINUX_UPDATER_TIMER" \
            || [ "$(grep -Ec '^Persistent=' "$LEGACY_LINUX_UPDATER_TIMER")" -ne 1 ] \
            || ! grep -Fqx 'Persistent=true' "$LEGACY_LINUX_UPDATER_TIMER"; then
            die "Legacy Linux updater timer has an unexpected schedule layout"
        fi
    fi
}

validate_legacy_macos_plists() {
    local updater_present=false legacy_rpc legacy_p2p legacy_model legacy_seed legacy_pair
    local legacy_exec_token_count expected_exec_token_count=20
    validate_owned_nonwritable_path "$LEGACY_MAC_NODE_PLIST" \
        "macOS node LaunchAgent" file "$TARGET_UID"
    validate_legacy_plist_keys "$LEGACY_MAC_NODE_PLIST" \
        'Label ProgramArguments EnvironmentVariables ARC_DIR WorkingDirectory RunAtLoad KeepAlive ProcessType Nice LowPriorityBackgroundIO StandardOutPath StandardErrorPath' \
        'macOS node LaunchAgent'
    [ "$(grep -Fc '<key>Label</key>' "$LEGACY_MAC_NODE_PLIST")" -eq 1 ] \
        || die "Legacy macOS node LaunchAgent must contain exactly one label"
    [ "$(grep -Fc '<key>Label</key><string>com.arc.inference</string>' \
        "$LEGACY_MAC_NODE_PLIST")" -eq 1 ] \
        || die "Legacy macOS node LaunchAgent has an unexpected label"
    [ "$(plist_program_arguments "$LEGACY_MAC_NODE_PLIST" | sed -n '1p')" = \
        "$ARC_DIR/bin/arc-node" ] \
        || die "Legacy macOS node LaunchAgent targets an unexpected executable"
    grep -Fq "<key>ARC_DIR</key><string>$ARC_DIR</string>" "$LEGACY_MAC_NODE_PLIST" \
        || die "Legacy macOS node LaunchAgent targets a different ARC environment"
    grep -Fq "<key>WorkingDirectory</key><string>$ARC_DIR</string>" \
        "$LEGACY_MAC_NODE_PLIST" \
        || die "Legacy macOS node LaunchAgent targets a different working directory"
    if grep -Eq '<key>(Program|UserName|GroupName|RootDirectory|EnableTransactions)</key>' \
        "$LEGACY_MAC_NODE_PLIST"; then
        die "Legacy macOS node LaunchAgent contains an unexpected execution directive"
    fi
    read_single_legacy_plist_argument "$LEGACY_MAC_NODE_PLIST" --rpc
    legacy_rpc="$LEGACY_ARGUMENT_VALUE"
    read_single_legacy_plist_argument "$LEGACY_MAC_NODE_PLIST" --p2p-port
    legacy_p2p="$LEGACY_ARGUMENT_VALUE"
    read_single_legacy_plist_argument "$LEGACY_MAC_NODE_PLIST" --seeds-file
    [ "$LEGACY_ARGUMENT_VALUE" = "$ARC_DIR/seeds.txt" ] \
        || die "Legacy macOS node LaunchAgent targets unexpected seeds"
    read_single_legacy_plist_argument "$LEGACY_MAC_NODE_PLIST" --genesis
    [ "$LEGACY_ARGUMENT_VALUE" = "$ARC_DIR/genesis.toml" ] \
        || die "Legacy macOS node LaunchAgent targets unexpected genesis"
    read_single_legacy_plist_argument "$LEGACY_MAC_NODE_PLIST" --validator-seed
    legacy_seed="$(sed -n '1p' "$ARC_DIR/identity.seed")"
    [ "$LEGACY_ARGUMENT_VALUE" = "$legacy_seed" ] \
        || die "Legacy macOS node LaunchAgent targets an unexpected identity"
    for legacy_pair in '--stake:0' '--min-stake:0' '--eth-rpc-port:0'; do
        read_single_legacy_plist_argument "$LEGACY_MAC_NODE_PLIST" "${legacy_pair%%:*}"
        [ "$LEGACY_ARGUMENT_VALUE" = "${legacy_pair#*:}" ] \
            || die "Legacy macOS node LaunchAgent has an unexpected ${legacy_pair%%:*} value"
    done
    read_single_legacy_plist_argument "$LEGACY_MAC_NODE_PLIST" --data-dir
    [ "$LEGACY_ARGUMENT_VALUE" = "$ARC_DIR/data" ] \
        || die "Legacy macOS node LaunchAgent targets unexpected chain data"
    read_single_legacy_plist_argument "$LEGACY_MAC_NODE_PLIST" --model false
    legacy_model="$LEGACY_ARGUMENT_VALUE"
    [ "$(plist_program_arguments "$LEGACY_MAC_NODE_PLIST" \
        | awk '$0 == "--community-mode" { count += 1 } END { print count + 0 }')" -eq 1 ] \
        || die "Legacy macOS node LaunchAgent has an unexpected community-mode argument"
    [ -z "$legacy_model" ] || expected_exec_token_count=22
    legacy_exec_token_count="$(plist_program_arguments "$LEGACY_MAC_NODE_PLIST" \
        | awk 'END { print NR + 0 }')" \
        || die "Could not parse legacy macOS ProgramArguments"
    [ "$legacy_exec_token_count" -eq "$expected_exec_token_count" ] \
        || die "Legacy macOS node LaunchAgent contains unexpected executable arguments"
    apply_legacy_runtime_configuration "$legacy_rpc" "$legacy_p2p" "$legacy_model"

    legacy_path_present "$LEGACY_MAC_UPDATER_PLIST" && updater_present=true
    if [ "$updater_present" = true ]; then
        validate_owned_nonwritable_path "$LEGACY_MAC_UPDATER_PLIST" \
            "macOS updater LaunchAgent" file "$TARGET_UID"
        validate_legacy_plist_keys "$LEGACY_MAC_UPDATER_PLIST" \
            'Label ProgramArguments EnvironmentVariables ARC_DIR StartCalendarInterval Hour Minute StandardOutPath StandardErrorPath' \
            'macOS updater LaunchAgent'
        [ "$(grep -Fc '<key>Label</key>' "$LEGACY_MAC_UPDATER_PLIST")" -eq 1 ] \
            || die "Legacy macOS updater LaunchAgent must contain exactly one label"
        [ "$(grep -Fc '<key>Label</key><string>com.arc.updater</string>' \
            "$LEGACY_MAC_UPDATER_PLIST")" -eq 1 ] \
            || die "Legacy macOS updater LaunchAgent has an unexpected label"
        [ "$(plist_program_arguments "$LEGACY_MAC_UPDATER_PLIST" | sed -n '1p')" = \
            "$ARC_DIR/bin/arc-auto-update.sh" ] \
            || die "Legacy macOS updater LaunchAgent targets an unexpected executable"
        [ "$(plist_program_arguments "$LEGACY_MAC_UPDATER_PLIST" | awk 'END { print NR + 0 }')" -eq 1 ] \
            || die "Legacy macOS updater LaunchAgent has unexpected executable arguments"
        grep -Fq "<key>ARC_DIR</key><string>$ARC_DIR</string>" \
            "$LEGACY_MAC_UPDATER_PLIST" \
            || die "Legacy macOS updater LaunchAgent targets a different ARC environment"
        if grep -Eq '<key>(Program|UserName|GroupName|RootDirectory|EnableTransactions)</key>' \
            "$LEGACY_MAC_UPDATER_PLIST"; then
            die "Legacy macOS updater LaunchAgent contains an unexpected execution directive"
        fi
    fi
}

validate_legacy_detached_pid() {
    local pid_line="$1" source_label="$2"
    local expected_uid process_uid command_line command_line_count
    local legacy_rpc legacy_p2p legacy_model legacy_seed
    local legacy_pair community_count expected_token_count actual_token_count
    if [ "$CURRENT_UID" -eq 0 ] && [ "$TARGET_USER" != root ]; then
        die "Adopt a user-owned detached v0.7 node without sudo so rollback can restore its process safely"
    fi
    expected_uid="$(legacy_expected_owner_uid)"
    case "$pid_line" in ''|*[!0-9]*) die "Legacy detached node PID is invalid" ;; esac
    [ "$pid_line" -gt 1 ] \
        || die "Legacy detached node PID is invalid"
    kill -0 "$pid_line" 2>/dev/null \
        || die "Legacy detached node process disappeared during $source_label validation"
    process_uid="$(ps -ww -p "$pid_line" -o uid= 2>/dev/null \
        | awk 'NF { if (++count > 1 || $1 !~ /^[0-9]+$/ || NF != 1) exit 2; uid=$1 } END { if (count == 1) print uid; else exit 3 }')" \
        || die "Could not prove the owner of the legacy detached node process"
    [ "$process_uid" -eq "$expected_uid" ] \
        || die "Legacy detached node process is owned by an unexpected user"
    command_line="$(ps -ww -p "$pid_line" -o command= 2>/dev/null)" \
        || die "Could not inspect the legacy detached node process"
    command_line_count="$(printf '%s\n' "$command_line" | awk 'END { print NR + 0 }')"
    [ "$command_line_count" -eq 1 ] \
        || die "Legacy detached node command line is ambiguous"
    case "$command_line" in
        "$ARC_DIR/bin/arc-node"|"$ARC_DIR/bin/arc-node "*) ;;
        *) die "Legacy $source_label identifies a different executable" ;;
    esac
    LEGACY_DETACHED_COMMUNITY_MODE=false
    read_single_legacy_command_argument "$command_line" --rpc
    legacy_rpc="$LEGACY_ARGUMENT_VALUE"
    read_single_legacy_command_argument "$command_line" --p2p-port
    legacy_p2p="$LEGACY_ARGUMENT_VALUE"
    read_single_legacy_command_argument "$command_line" --seeds-file
    [ "$LEGACY_ARGUMENT_VALUE" = "$ARC_DIR/seeds.txt" ] \
        || die "Legacy detached node targets unexpected seeds"
    read_single_legacy_command_argument "$command_line" --genesis
    [ "$LEGACY_ARGUMENT_VALUE" = "$ARC_DIR/genesis.toml" ] \
        || die "Legacy detached node targets unexpected genesis"
    legacy_seed="$(sed -n '1p' "$ARC_DIR/identity.seed")"
    read_single_legacy_command_argument "$command_line" --validator-seed
    [ "$LEGACY_ARGUMENT_VALUE" = "$legacy_seed" ] \
        || die "Legacy detached node targets an unexpected identity"
    for legacy_pair in '--stake:0' '--min-stake:0' '--eth-rpc-port:0'; do
        read_single_legacy_command_argument "$command_line" "${legacy_pair%%:*}"
        [ "$LEGACY_ARGUMENT_VALUE" = "${legacy_pair#*:}" ] \
            || die "Legacy detached node has an unexpected ${legacy_pair%%:*} value"
    done
    read_single_legacy_command_argument "$command_line" --data-dir
    [ "$LEGACY_ARGUMENT_VALUE" = "$ARC_DIR/data" ] \
        || die "Legacy detached node targets unexpected chain data"
    read_single_legacy_command_argument "$command_line" --model false
    legacy_model="$LEGACY_ARGUMENT_VALUE"
    community_count="$(printf '%s\n' "$command_line" \
        | awk '{ for (i = 1; i <= NF; i++) if ($i == "--community-mode") count += 1 } END { print count + 0 }')"
    [ "$community_count" -le 1 ] \
        || die "Legacy detached node has duplicate community-mode arguments"
    [ "$community_count" -eq 0 ] || LEGACY_DETACHED_COMMUNITY_MODE=true
    expected_token_count=19
    [ -z "$legacy_model" ] || expected_token_count=$((expected_token_count + 2))
    [ "$LEGACY_DETACHED_COMMUNITY_MODE" = false ] \
        || expected_token_count=$((expected_token_count + 1))
    actual_token_count="$(printf '%s\n' "$command_line" | awk '{ print NF + 0 }')"
    [ "$actual_token_count" -eq "$expected_token_count" ] \
        || die "Legacy detached node contains unexpected or whitespace-ambiguous arguments"
    apply_legacy_runtime_configuration "$legacy_rpc" "$legacy_p2p" "$legacy_model"
    LEGACY_DETACHED_RESTART_ARGS=(
        "$ARC_DIR/bin/arc-node"
        --rpc "$legacy_rpc"
        --p2p-port "$legacy_p2p"
        --seeds-file "$ARC_DIR/seeds.txt"
        --genesis "$ARC_DIR/genesis.toml"
        --validator-seed "$legacy_seed"
        --stake 0 --min-stake 0 --eth-rpc-port 0
        --data-dir "$ARC_DIR/data"
    )
    [ -z "$legacy_model" ] || LEGACY_DETACHED_RESTART_ARGS+=(--model "$legacy_model")
    [ "$LEGACY_DETACHED_COMMUNITY_MODE" = false ] \
        || LEGACY_DETACHED_RESTART_ARGS+=(--community-mode)
    LEGACY_DETACHED_PID="$pid_line"
    LEGACY_DETACHED_WAS_RUNNING=true
}

validate_legacy_detached_process() {
    local expected_uid pid_line line_count
    expected_uid="$(legacy_expected_owner_uid)"
    validate_owned_nonwritable_path "$LEGACY_NODE_PID_FILE" \
        "detached node PID file" file "$expected_uid"
    pid_line="$(sed -n '1p' "$LEGACY_NODE_PID_FILE")"
    line_count="$(awk 'END { print NR + 0 }' "$LEGACY_NODE_PID_FILE")"
    case "$pid_line" in ''|*[!0-9]*) die "Legacy detached node PID is invalid" ;; esac
    if ! { [ "$line_count" -eq 1 ] && [ "$pid_line" -gt 1 ]; }; then
        die "Legacy detached node PID file is malformed"
    fi
    LEGACY_DETACHED_HAD_PID_FILE=true
    validate_legacy_detached_pid "$pid_line" node.pid
}

LEGACY_UNTRACKED_PROCESS_FOUND=false
discover_untracked_legacy_detached_process() {
    local validate_found="${1:-true}"
    local expected_uid process_rows pid process_uid command_line discovered_pid=""
    local discovered_count=0
    LEGACY_UNTRACKED_PROCESS_FOUND=false
    expected_uid="$(legacy_expected_owner_uid)"
    process_rows="$(ps -ww -axo pid=,uid=,command= 2>/dev/null)" \
        || die "Could not enumerate processes while proving legacy v0.7 quiescence"
    while read -r pid process_uid command_line; do
        [ -n "$pid" ] || continue
        case "$pid" in ''|*[!0-9]*)
            die "Process enumeration returned an ambiguous PID row" ;;
        esac
        case "$process_uid" in ''|*[!0-9]*)
            die "Process enumeration returned an ambiguous owner row" ;;
        esac
        case "$command_line" in
            "$ARC_DIR/bin/arc-node"|"$ARC_DIR/bin/arc-node "*)
                [ "$process_uid" -eq "$expected_uid" ] \
                    || die "A legacy ARC node under $ARC_DIR is owned by an unexpected user"
                discovered_count=$((discovered_count + 1))
                discovered_pid="$pid" ;;
        esac
    done <<< "$process_rows"
    [ "$discovered_count" -le 1 ] \
        || die "Multiple untracked legacy ARC node processes are running; refusing ambiguous adoption"
    if [ "$discovered_count" -eq 1 ]; then
        if [ "$validate_found" = true ]; then
            LEGACY_DETACHED_HAD_PID_FILE=false
            validate_legacy_detached_pid "$discovered_pid" "untracked process"
        fi
        LEGACY_UNTRACKED_PROCESS_FOUND=true
    fi
}

ensure_no_legacy_arc_processes() {
    discover_untracked_legacy_detached_process false
    [ "$LEGACY_UNTRACKED_PROCESS_FOUND" = false ] \
        || die "A legacy ARC node process still owns the v0.7 runtime after graceful retirement"
}

configure_legacy_detached_restart_from_bound_state() {
    local legacy_seed
    legacy_seed="$(sed -n '1p' "$ARC_DIR/identity.seed")"
    LEGACY_DETACHED_RESTART_ARGS=(
        "$ARC_DIR/bin/arc-node"
        --rpc "$RPC_PORT"
        --p2p-port "$P2P_PORT"
        --seeds-file "$ARC_DIR/seeds.txt"
        --genesis "$ARC_DIR/genesis.toml"
        --validator-seed "$legacy_seed"
        --stake 0 --min-stake 0 --eth-rpc-port 0
        --data-dir "$ARC_DIR/data"
    )
    [ -z "$MODEL_PATH" ] || LEGACY_DETACHED_RESTART_ARGS+=(--model "$MODEL_PATH")
    [ "$LEGACY_DETACHED_COMMUNITY_MODE" = false ] \
        || LEGACY_DETACHED_RESTART_ARGS+=(--community-mode)
}

legacy_detached_topology_expected() {
    local pid_file
    case "$LEGACY_SUPERVISOR_KIND" in
        detached) pid_file=true ;;
        detached-untracked) pid_file=false ;;
        *) die "Internal detached topology error: $LEGACY_SUPERVISOR_KIND" ;;
    esac
    printf '%s\n' \
        'arc-chain-legacy-detached-topology-v1' \
        "pid_file=$pid_file" \
        "community_mode=$LEGACY_DETACHED_COMMUNITY_MODE"
}

load_legacy_detached_topology_archive() {
    local topology_file="$LEGACY_PRESERVED_DIR/legacy-detached-topology"
    local expected_uid community_line observed_community=""
    expected_uid="$(legacy_expected_owner_uid)"
    validate_owned_nonwritable_path "$topology_file" \
        "preserved detached topology" file "$expected_uid"
    [ "$(sed -n '1p' "$topology_file")" = \
        'arc-chain-legacy-detached-topology-v1' ] \
        || die "Preserved detached topology has an unknown schema"
    community_line="$(sed -n '3p' "$topology_file")"
    case "$community_line" in
        community_mode=true) observed_community=true ;;
        community_mode=false) observed_community=false ;;
        *) die "Preserved detached topology has an invalid community-mode value" ;;
    esac
    if [ "$LEGACY_DETACHED_WAS_RUNNING" = true ] \
        && [ "$LEGACY_DETACHED_COMMUNITY_MODE" != "$observed_community" ]; then
        die "Live detached process no longer matches its preserved topology"
    fi
    LEGACY_DETACHED_COMMUNITY_MODE="$observed_community"
    cmp -s "$topology_file" <(legacy_detached_topology_expected) \
        || die "Preserved detached topology does not match the adoption marker"
    LEGACY_DETACHED_HAD_PID_FILE=false
    [ "$LEGACY_SUPERVISOR_KIND" != detached ] \
        || LEGACY_DETACHED_HAD_PID_FILE=true
    LEGACY_DETACHED_WAS_RUNNING=true
    configure_legacy_detached_restart_from_bound_state
}

prepare_legacy_supervisor() {
    local resume_mode="${1:-false}"
    local linux_node_present=false mac_node_present=false detached_present=false
    local linux_updater_artifact=false mac_updater_artifact=false detected_count=0

    legacy_path_present "$LEGACY_LINUX_NODE_UNIT" && linux_node_present=true
    if legacy_path_present "$LEGACY_LINUX_UPDATER_SERVICE" \
        || legacy_path_present "$LEGACY_LINUX_UPDATER_TIMER"; then
        linux_updater_artifact=true
    fi
    legacy_path_present "$LEGACY_MAC_NODE_PLIST" && mac_node_present=true
    legacy_path_present "$LEGACY_MAC_UPDATER_PLIST" && mac_updater_artifact=true
    legacy_path_present "$LEGACY_NODE_PID_FILE" && detached_present=true

    if [ "$resume_mode" = true ]; then
        LEGACY_SUPERVISOR_KIND="$LEGACY_MARKER_SUPERVISOR_KIND"
    else
        [ "$linux_node_present" = true ] && detected_count=$((detected_count + 1))
        [ "$mac_node_present" = true ] && detected_count=$((detected_count + 1))
        [ "$detached_present" = true ] && detected_count=$((detected_count + 1))
        if [ "$detected_count" -eq 0 ]; then
            discover_untracked_legacy_detached_process
            if [ "$LEGACY_UNTRACKED_PROCESS_FOUND" = true ]; then
                detected_count=1
                LEGACY_SUPERVISOR_KIND=detached-untracked
            fi
        fi
        [ "$detected_count" -le 1 ] \
            || die "Multiple legacy ARC supervisors are present; refusing an ambiguous adoption"
        if [ "$LEGACY_SUPERVISOR_KIND" = detached-untracked ]; then :
        elif [ "$linux_node_present" = true ]; then LEGACY_SUPERVISOR_KIND=linux-system
        elif [ "$mac_node_present" = true ]; then LEGACY_SUPERVISOR_KIND=macos-launchd
        elif [ "$detached_present" = true ]; then LEGACY_SUPERVISOR_KIND=detached
        else LEGACY_SUPERVISOR_KIND=none
        fi
    fi

    # Older installers published the pending marker before the legacy archive.
    # Continue to accept that recoverable crash state, but new migrations first
    # fence the unsigned updater beneath the install lock, publish the archive,
    # and only then publish the marker. Once an archive exists, later
    # transaction crash states may legitimately have retired the source.
    if [ "$resume_mode" = true ] \
        && ! legacy_path_present "$LEGACY_PRESERVED_DIR"; then
        case "$LEGACY_SUPERVISOR_KIND" in
            linux-system)
                [ "$linux_node_present" = true ] \
                    || die "Pending Linux adoption has neither its original supervisor nor its archive" ;;
            macos-launchd)
                [ "$mac_node_present" = true ] \
                    || die "Pending macOS adoption has neither its original supervisor nor its archive" ;;
            detached)
                [ "$detached_present" = true ] \
                    || die "Pending detached adoption has neither its original PID source nor its archive" ;;
            detached-untracked)
                discover_untracked_legacy_detached_process
                [ "$LEGACY_UNTRACKED_PROCESS_FOUND" = true ] \
                    || die "Pending untracked adoption has neither its original process nor its archive" ;;
        esac
    fi

    case "$LEGACY_SUPERVISOR_KIND" in
        linux-system)
            [ "$OS" = Linux ] \
                || die "Legacy marker binds a Linux supervisor on a non-Linux host"
            if [ "$linux_node_present" = true ] && [ "$LEGACY_PARTIAL_RESUME" = false ]; then
                validate_legacy_linux_units
            elif [ "$linux_node_present" = true ]; then
                if cmp -s "$LEGACY_LINUX_NODE_UNIT" \
                    "$LEGACY_PRESERVED_DIR/legacy-linux-arc-node.service"; then
                    validate_legacy_linux_units
                else
                    validate_partial_managed_system_user_node_unit
                fi
                if [ "$linux_updater_artifact" = true ]; then
                    if ! legacy_path_present "$LEGACY_LINUX_UPDATER_SERVICE" \
                        || ! legacy_path_present "$LEGACY_LINUX_UPDATER_TIMER"; then
                        die "Legacy Linux updater service/timer is incomplete after resume"
                    fi
                    validate_owned_nonwritable_path "$LEGACY_LINUX_UPDATER_SERVICE" \
                        "Linux updater service" file 0
                    validate_owned_nonwritable_path "$LEGACY_LINUX_UPDATER_TIMER" \
                        "Linux updater timer" file 0
                    if ! cmp -s "$LEGACY_LINUX_UPDATER_SERVICE" \
                        "$LEGACY_PRESERVED_DIR/legacy-linux-arc-updater.service" \
                        || ! cmp -s "$LEGACY_LINUX_UPDATER_TIMER" \
                            "$LEGACY_PRESERVED_DIR/legacy-linux-arc-updater.timer"; then
                        die "Legacy Linux updater units changed during pending adoption"
                    fi
                fi
            elif [ "$resume_mode" != true ]; then
                die "Legacy Linux supervisor disappeared before adoption was reserved"
            elif [ "$linux_updater_artifact" = true ]; then
                die "Partially retired legacy Linux units do not match the pending marker"
            fi
            if [ "$mac_node_present" != false ] \
                || [ "$mac_updater_artifact" != false ] \
                || [ "$detached_present" != false ]; then
                die "Pending Linux adoption acquired a conflicting legacy supervisor"
            fi
            command -v systemctl >/dev/null 2>&1 \
                || die "systemctl is required to retire the legacy Linux supervisor"
            if [ "$CURRENT_UID" -ne 0 ]; then
                command -v sudo >/dev/null 2>&1 \
                    || die "Retiring the legacy Linux system units requires sudo"
                sudo -v || die "sudo authorization failed before legacy retirement"
            fi ;;
        macos-launchd)
            [ "$OS" = Darwin ] \
                || die "Legacy marker binds a macOS supervisor on a non-macOS host"
            if [ "$mac_node_present" = true ]; then
                validate_legacy_macos_plists
            elif [ "$resume_mode" != true ]; then
                die "Legacy macOS supervisor disappeared before adoption was reserved"
            elif [ "$mac_updater_artifact" = true ]; then
                die "Partially retired legacy macOS agents do not match the pending marker"
            fi
            if [ "$linux_node_present" != false ] \
                || [ "$linux_updater_artifact" != false ] \
                || [ "$detached_present" != false ]; then
                die "Pending macOS adoption acquired a conflicting legacy supervisor"
            fi ;;
        detached)
            if [ "$detached_present" = true ]; then
                validate_legacy_detached_process
            elif [ "$resume_mode" != true ]; then
                die "Legacy detached process disappeared before adoption was reserved"
            else
                discover_untracked_legacy_detached_process
                [ "$LEGACY_UNTRACKED_PROCESS_FOUND" = false ] \
                    || die "Pending PID-managed adoption acquired an untracked ARC process"
            fi
            if [ "$linux_node_present" != false ] \
                || [ "$linux_updater_artifact" != false ] \
                || [ "$mac_node_present" != false ] \
                || [ "$mac_updater_artifact" != false ]; then
                die "Pending detached adoption acquired a conflicting legacy supervisor"
            fi ;;
        detached-untracked)
            [ "$detached_present" = false ] \
                || die "Pending untracked adoption acquired a conflicting node.pid"
            discover_untracked_legacy_detached_process
            if [ "$LEGACY_UNTRACKED_PROCESS_FOUND" = false ] \
                && [ "$resume_mode" != true ]; then
                die "Legacy untracked process disappeared before adoption was reserved"
            fi
            if [ "$linux_node_present" != false ] \
                || [ "$linux_updater_artifact" != false ] \
                || [ "$mac_node_present" != false ] \
                || [ "$mac_updater_artifact" != false ]; then
                die "Pending untracked adoption acquired a conflicting legacy supervisor"
            fi ;;
        none)
            if [ "$linux_node_present" != false ] \
                || [ "$linux_updater_artifact" != false ] \
                || [ "$mac_node_present" != false ] \
                || [ "$mac_updater_artifact" != false ] \
                || [ "$detached_present" != false ]; then
                die "Legacy supervisor artifacts do not match a supervisor-free adoption"
            fi
            discover_untracked_legacy_detached_process
            [ "$LEGACY_UNTRACKED_PROCESS_FOUND" = false ] \
                || die "Supervisor-free adoption acquired an untracked legacy ARC process" ;;
    esac

    if [ "$SERVICE_SCOPE" = system-user ]; then
        [ "$LEGACY_SUPERVISOR_KIND" = linux-system ] \
            || die "system-user scope requires a verified legacy Linux system supervisor"
        [ "$ARC_DIR" = "$LEGACY_USER_INSTALL_ROOT" ] \
            || die "system-user scope requires the exact legacy user-default root"
    fi
    if [ "$resume_mode" = true ]; then
        if [ "$RPC_PORT" != "$LEGACY_MARKER_RPC_PORT" ] \
            || [ "$P2P_PORT" != "$LEGACY_MARKER_P2P_PORT" ] \
            || [ "$MODEL_PATH" != "$LEGACY_MARKER_MODEL_PATH" ]; then
            die "Legacy supervisor configuration changed during pending adoption"
        fi
    fi
}

validate_resumable_legacy_evidence() {
    local expected_uid preserved_name preserved_version
    expected_uid="$(legacy_expected_owner_uid)"
    validate_legacy_default_ancestor_chain "$expected_uid"
    validate_owned_nonwritable_path "$ARC_DIR/identity.seed" \
        "identity seed" file "$expected_uid"
    validate_owned_nonwritable_path "$ARC_DIR/data" \
        "data directory" directory "$expected_uid"
    validate_owned_nonwritable_path "$LEGACY_PRESERVED_DIR" \
        "preserved configuration directory" directory "$expected_uid"
    for preserved_name in version.txt seeds.txt genesis.toml identity.seed; do
        validate_owned_nonwritable_path "$LEGACY_PRESERVED_DIR/$preserved_name" \
            "preserved $preserved_name" file "$expected_uid"
    done
    preserved_version="$(sed -n '1p' "$LEGACY_PRESERVED_DIR/version.txt")"
    preserved_version="${preserved_version#v}"
    [ "$preserved_version" = "$LEGACY_SOURCE_VERSION" ] \
        || die "Preserved legacy version does not match the pending marker"
    cmp -s "$ARC_DIR/identity.seed" "$LEGACY_PRESERVED_DIR/identity.seed" \
        || die "Legacy identity changed after adoption was reserved"
    [ "$NODE_DATA_DIR" != "$ARC_DIR/data" ] \
        || die "Pending legacy adoption may not reuse v0.7 chain data"
}

validate_partial_managed_system_user_node_unit() {
    validate_owned_nonwritable_path "$SYSTEMD_UNIT_DIR/arc-node.service" \
        "partial managed system-user node unit" file 0
    validate_legacy_unit_directive_names "$SYSTEMD_UNIT_DIR/arc-node.service" \
        'Description Wants After Type User Group WorkingDirectory ExecStart Restart RestartSec TimeoutStopSec SendSIGKILL UMask NoNewPrivileges PrivateTmp ProtectSystem ProtectKernelTunables ProtectKernelModules ProtectControlGroups RestrictSUIDSGID WantedBy' \
        'partial managed system-user node unit'
    [ "$(grep -Fc '# ARC managed system-user node unit v1' \
        "$SYSTEMD_UNIT_DIR/arc-node.service")" -eq 1 ] \
        || die "Partial system-user node unit lacks the ARC ownership contract"
    if [ "$(grep -Ec '^User=' "$SYSTEMD_UNIT_DIR/arc-node.service")" -ne 1 ] \
        || ! grep -Fqx "User=$TARGET_USER" "$SYSTEMD_UNIT_DIR/arc-node.service"; then
        die "Partial system-user node unit does not run as $TARGET_USER"
    fi
    if [ "$(grep -Ec '^Group=' "$SYSTEMD_UNIT_DIR/arc-node.service")" -ne 1 ] \
        || ! grep -Fqx "Group=$TARGET_GROUP" "$SYSTEMD_UNIT_DIR/arc-node.service"; then
        die "Partial system-user node unit has an unexpected group"
    fi
    if [ "$(grep -Ec '^WorkingDirectory=' "$SYSTEMD_UNIT_DIR/arc-node.service")" -ne 1 ] \
        || ! grep -Fqx "WorkingDirectory=\"$ARC_DIR\"" \
            "$SYSTEMD_UNIT_DIR/arc-node.service"; then
        die "Partial system-user node unit targets a different working directory"
    fi
    if [ "$(grep -Ec '^ExecStart=' "$SYSTEMD_UNIT_DIR/arc-node.service")" -ne 1 ] \
        || ! grep -Fqx "ExecStart=\"$ARC_DIR/bin/run-arc-node\" " \
            "$SYSTEMD_UNIT_DIR/arc-node.service"; then
        die "Partial system-user node unit targets an unexpected runner"
    fi
    if [ "$(grep -Ec '^Restart=' "$SYSTEMD_UNIT_DIR/arc-node.service")" -ne 1 ] \
        || ! grep -Fqx 'Restart=always' "$SYSTEMD_UNIT_DIR/arc-node.service"; then
        die "Partial system-user node unit lacks the managed restart bridge"
    fi
    if [ "$(grep -Ec '^TimeoutStopSec=' "$SYSTEMD_UNIT_DIR/arc-node.service")" -ne 1 ] \
        || ! grep -Fqx "TimeoutStopSec=$GRACEFUL_STOP_TIMEOUT_SECS" \
            "$SYSTEMD_UNIT_DIR/arc-node.service"; then
        die "Partial system-user node unit lacks the complete graceful-stop budget"
    fi
    if [ "$(grep -Ec '^SendSIGKILL=' "$SYSTEMD_UNIT_DIR/arc-node.service")" -ne 1 ] \
        || ! grep -Fqx 'SendSIGKILL=no' "$SYSTEMD_UNIT_DIR/arc-node.service"; then
        die "Partial system-user node unit may force-kill admitted work"
    fi
}

validate_managed_system_user_units() {
    local updater_service_present=false updater_timer_present=false
    validate_partial_managed_system_user_node_unit
    legacy_path_present "$SYSTEMD_UNIT_DIR/arc-node-update.service" \
        && updater_service_present=true
    legacy_path_present "$SYSTEMD_UNIT_DIR/arc-node-update.timer" \
        && updater_timer_present=true
    [ "$updater_service_present" = "$updater_timer_present" ] \
        || die "Managed system-user updater service/timer is incomplete"
    [ "$updater_service_present" = true ] || return 0
    validate_owned_nonwritable_path "$SYSTEMD_UNIT_DIR/arc-node-update.service" \
        "managed system-user updater service" file 0
    validate_owned_nonwritable_path "$SYSTEMD_UNIT_DIR/arc-node-update.timer" \
        "managed system-user updater timer" file 0
    validate_legacy_unit_directive_names "$SYSTEMD_UNIT_DIR/arc-node-update.service" \
        'Description Type User Group NoNewPrivileges UMask ExecStart' \
        'managed system-user updater unit'
    validate_legacy_unit_directive_names "$SYSTEMD_UNIT_DIR/arc-node-update.timer" \
        'Description OnCalendar RandomizedDelaySec Persistent WantedBy' \
        'managed system-user updater timer'
    [ "$(grep -Fc '# ARC managed system-user updater unit v1' \
        "$SYSTEMD_UNIT_DIR/arc-node-update.service")" -eq 1 ] \
        || die "System-user updater unit lacks the ARC ownership contract"
    if [ "$(grep -Ec '^User=' "$SYSTEMD_UNIT_DIR/arc-node-update.service")" -ne 1 ] \
        || ! grep -Fqx "User=$TARGET_USER" "$SYSTEMD_UNIT_DIR/arc-node-update.service"; then
        die "System-user updater does not run as $TARGET_USER"
    fi
    if [ "$(grep -Ec '^Group=' "$SYSTEMD_UNIT_DIR/arc-node-update.service")" -ne 1 ] \
        || ! grep -Fqx "Group=$TARGET_GROUP" "$SYSTEMD_UNIT_DIR/arc-node-update.service"; then
        die "System-user updater has an unexpected group"
    fi
    [ "$(grep -Ec '^ExecStart=' "$SYSTEMD_UNIT_DIR/arc-node-update.service")" -eq 1 ] \
        || die "System-user updater must contain exactly one ExecStart"
    grep -Fq "\"$ARC_DIR/bin/arc-installer\"" \
        "$SYSTEMD_UNIT_DIR/arc-node-update.service" \
        || die "System-user updater targets an unexpected installer"
    grep -Fq -- '--service-scope" "system-user"' \
        "$SYSTEMD_UNIT_DIR/arc-node-update.service" \
        || die "System-user updater does not preserve its bounded scope"
    [ "$(grep -Fc '# ARC managed system-user updater timer v1' \
        "$SYSTEMD_UNIT_DIR/arc-node-update.timer")" -eq 1 ] \
        || die "System-user updater timer lacks the ARC ownership contract"
}

validate_legacy_adoption_marker() {
    local source_line service_line supervisor_line rpc_line p2p_line model_line
    local expected_uid marker_uid marker_mode marker_permissions
    [ ! -L "$LEGACY_ADOPTION_MARKER" ] \
        || die "Refusing symlinked legacy-adoption marker: $LEGACY_ADOPTION_MARKER"
    [ -f "$LEGACY_ADOPTION_MARKER" ] \
        || die "Legacy adoption marker is missing: $LEGACY_ADOPTION_MARKER"
    source_line="$(sed -n '3p' "$LEGACY_ADOPTION_MARKER")"
    LEGACY_SOURCE_VERSION="${source_line#source_version=}"
    if [ "$source_line" != "source_version=$LEGACY_SOURCE_VERSION" ] \
        || ! printf '%s\n' "$LEGACY_SOURCE_VERSION" | grep -Eq '^0\.7\.[0-9]+$'; then
        die "Legacy-adoption marker has an invalid source version"
    fi
    service_line="$(sed -n '5p' "$LEGACY_ADOPTION_MARKER")"
    LEGACY_MARKER_SERVICE_SCOPE="${service_line#service_scope=}"
    case "$LEGACY_MARKER_SERVICE_SCOPE" in
        system-user|user|launchd|none) ;;
        *) die "Legacy-adoption marker has an invalid service scope" ;;
    esac
    [ "$SERVICE_SCOPE" = "$LEGACY_MARKER_SERVICE_SCOPE" ] \
        || die "Legacy adoption must resume with its bound service scope ($LEGACY_MARKER_SERVICE_SCOPE)"
    supervisor_line="$(sed -n '6p' "$LEGACY_ADOPTION_MARKER")"
    LEGACY_MARKER_SUPERVISOR_KIND="${supervisor_line#supervisor_kind=}"
    case "$LEGACY_MARKER_SUPERVISOR_KIND" in
        none|linux-system|macos-launchd|detached|detached-untracked) ;;
        *) die "Legacy-adoption marker has an invalid supervisor kind" ;;
    esac
    LEGACY_SUPERVISOR_KIND="$LEGACY_MARKER_SUPERVISOR_KIND"
    rpc_line="$(sed -n '7p' "$LEGACY_ADOPTION_MARKER")"
    p2p_line="$(sed -n '8p' "$LEGACY_ADOPTION_MARKER")"
    model_line="$(sed -n '9p' "$LEGACY_ADOPTION_MARKER")"
    LEGACY_MARKER_RPC_PORT="${rpc_line#rpc_port=}"
    LEGACY_MARKER_P2P_PORT="${p2p_line#p2p_port=}"
    LEGACY_MARKER_MODEL_PATH="${model_line#model_path=}"
    valid_port "$LEGACY_MARKER_RPC_PORT" \
        || die "Legacy-adoption marker has an invalid RPC port"
    valid_port "$LEGACY_MARKER_P2P_PORT" \
        || die "Legacy-adoption marker has an invalid P2P port"
    [ "$LEGACY_MARKER_RPC_PORT" != "$LEGACY_MARKER_P2P_PORT" ] \
        || die "Legacy-adoption marker reuses one RPC/P2P port"
    case "$LEGACY_MARKER_MODEL_PATH" in
        '') ;;
        /*) ;;
        *) die "Legacy-adoption marker has a non-absolute model path" ;;
    esac
    cmp -s "$LEGACY_ADOPTION_MARKER" <(legacy_marker_expected) \
        || die "Legacy-adoption marker is not bound to this exact directory"
    expected_uid="$(legacy_expected_owner_uid)"
    marker_uid="$(portable_file_uid "$LEGACY_ADOPTION_MARKER")" \
        || die "Could not inspect legacy-adoption marker owner"
    [ "$marker_uid" -eq "$expected_uid" ] \
        || die "Legacy-adoption marker has the wrong owner"
    marker_mode="$(portable_file_mode "$LEGACY_ADOPTION_MARKER")" \
        || die "Could not inspect legacy-adoption marker permissions"
    case "$marker_mode" in
        ''|*[!0-7]*) die "Could not parse legacy-adoption marker permissions" ;;
    esac
    marker_permissions=$((8#$marker_mode))
    [ $((marker_permissions & 0022)) -eq 0 ] \
        || die "Legacy-adoption marker must not be group/world writable"
}

create_legacy_adoption_marker() {
    local staged_marker marker_mode
    if [ -e "$LEGACY_ADOPTION_MARKER" ] || [ -L "$LEGACY_ADOPTION_MARKER" ]; then
        die "Refusing to overwrite an existing legacy-adoption marker"
    fi
    staged_marker="$(install_root_as_owner mktemp "$ARC_DIR/.legacy-adoption.new.XXXXXX")" \
        || die "Could not reserve the legacy-adoption marker"
    if ! legacy_marker_expected \
        | install_root_as_owner dd of="$staged_marker" conv=fsync 2>/dev/null; then
        install_root_as_owner rm -f -- "$staged_marker" || true
        die "Could not durably write the legacy-adoption marker"
    fi
    if [ "$ARC_DIR" = "$LEGACY_SYSTEM_INSTALL_ROOT" ]; then marker_mode=444
    else marker_mode=600
    fi
    if ! install_root_as_owner chmod "$marker_mode" "$staged_marker"; then
        install_root_as_owner rm -f -- "$staged_marker" || true
        die "Could not protect the legacy-adoption marker"
    fi
    if ! install_root_as_owner mv -n -- "$staged_marker" "$LEGACY_ADOPTION_MARKER" \
        || [ -e "$staged_marker" ]; then
        install_root_as_owner rm -f -- "$staged_marker" || true
        die "Legacy-adoption marker appeared concurrently"
    fi
    sync
    if [ "${ARC_INSTALL_TEST_FAIL_AFTER_LEGACY_MARKER_FSYNC:-}" = 1 ]; then
        die "Injected failure after durable legacy-adoption marker publication"
    fi
    validate_legacy_adoption_marker
}

preserve_legacy_v07_configuration() {
    local expected_uid staged_dir file_name updater_source source_path archive_name
    expected_uid="$(legacy_expected_owner_uid)"
    if [ -e "$LEGACY_PRESERVED_DIR" ] || [ -L "$LEGACY_PRESERVED_DIR" ]; then
        validate_owned_nonwritable_path "$LEGACY_PRESERVED_DIR" \
            "preserved configuration directory" directory "$expected_uid"
        for file_name in version.txt seeds.txt genesis.toml identity.seed; do
            validate_owned_nonwritable_path "$LEGACY_PRESERVED_DIR/$file_name" \
                "preserved $file_name" file "$expected_uid"
            cmp -s "$ARC_DIR/$file_name" "$LEGACY_PRESERVED_DIR/$file_name" \
                || die "Preserved legacy $file_name differs from its v0.7 source"
        done
        case "$LEGACY_SUPERVISOR_KIND" in
            linux-system)
                archive_name=legacy-linux-arc-node.service
                validate_owned_nonwritable_path "$LEGACY_PRESERVED_DIR/$archive_name" \
                    "preserved $archive_name" file "$expected_uid"
                if [ -f "$LEGACY_PRESERVED_DIR/legacy-linux-arc-updater.service" ] \
                    || [ -f "$LEGACY_PRESERVED_DIR/legacy-linux-arc-updater.timer" ]; then
                    for archive_name in legacy-linux-arc-updater.service legacy-linux-arc-updater.timer; do
                        validate_owned_nonwritable_path "$LEGACY_PRESERVED_DIR/$archive_name" \
                            "preserved $archive_name" file "$expected_uid"
                    done
                fi ;;
            macos-launchd)
                validate_owned_nonwritable_path \
                    "$LEGACY_PRESERVED_DIR/legacy-macos-com.arc.inference.plist" \
                    "preserved macOS node LaunchAgent" file "$expected_uid"
                if [ -f "$LEGACY_PRESERVED_DIR/legacy-macos-com.arc.updater.plist" ]; then
                    validate_owned_nonwritable_path \
                        "$LEGACY_PRESERVED_DIR/legacy-macos-com.arc.updater.plist" \
                        "preserved macOS updater LaunchAgent" file "$expected_uid"
                fi ;;
            detached|detached-untracked)
                load_legacy_detached_topology_archive
                if [ "$LEGACY_SUPERVISOR_KIND" = detached ]; then
                    validate_owned_nonwritable_path "$LEGACY_PRESERVED_DIR/legacy-node.pid" \
                        "preserved detached PID file" file "$expected_uid"
                fi ;;
        esac
        return
    fi
    staged_dir="$(install_root_as_owner mktemp -d "$ARC_DIR/.legacy-preserved.new.XXXXXX")" \
        || die "Could not reserve the legacy configuration archive"
    for file_name in version.txt seeds.txt genesis.toml identity.seed; do
        if ! install_root_as_owner cp -p -- "$ARC_DIR/$file_name" "$staged_dir/$file_name"; then
            install_root_as_owner rm -rf -- "$staged_dir" || true
            die "Could not preserve legacy $file_name"
        fi
        if ! install_root_as_owner chmod 600 "$staged_dir/$file_name"; then
            install_root_as_owner rm -rf -- "$staged_dir" || true
            die "Could not protect preserved legacy $file_name"
        fi
    done
    updater_source="$ARC_DIR/bin/arc-auto-update.sh"
    if [ -f "$updater_source" ] && [ ! -L "$updater_source" ]; then
        if ! install_root_as_owner cp -p -- "$updater_source" "$staged_dir/arc-auto-update.sh"; then
            install_root_as_owner rm -rf -- "$staged_dir" || true
            die "Could not preserve the legacy updater"
        fi
        if ! install_root_as_owner chmod 700 "$staged_dir/arc-auto-update.sh"; then
            install_root_as_owner rm -rf -- "$staged_dir" || true
            die "Could not protect the preserved legacy updater"
        fi
    fi
    case "$LEGACY_SUPERVISOR_KIND" in
        linux-system)
            for source_path in \
                "$LEGACY_LINUX_NODE_UNIT" \
                "$LEGACY_LINUX_UPDATER_SERVICE" \
                "$LEGACY_LINUX_UPDATER_TIMER"; do
                if [ ! -f "$source_path" ] || [ -L "$source_path" ]; then
                    continue
                fi
                case "$source_path" in
                    "$LEGACY_LINUX_NODE_UNIT") archive_name=legacy-linux-arc-node.service ;;
                    "$LEGACY_LINUX_UPDATER_SERVICE") archive_name=legacy-linux-arc-updater.service ;;
                    *) archive_name=legacy-linux-arc-updater.timer ;;
                esac
                if ! install_root_as_owner cp -p -- "$source_path" "$staged_dir/$archive_name" \
                    || ! install_root_as_owner chmod 600 "$staged_dir/$archive_name"; then
                    install_root_as_owner rm -rf -- "$staged_dir" || true
                    die "Could not preserve legacy Linux supervisor file: $source_path"
                fi
            done ;;
        macos-launchd)
            for source_path in "$LEGACY_MAC_NODE_PLIST" "$LEGACY_MAC_UPDATER_PLIST"; do
                if [ ! -f "$source_path" ] || [ -L "$source_path" ]; then
                    continue
                fi
                if [ "$source_path" = "$LEGACY_MAC_NODE_PLIST" ]; then
                    archive_name=legacy-macos-com.arc.inference.plist
                else
                    archive_name=legacy-macos-com.arc.updater.plist
                fi
                if ! install_root_as_owner cp -p -- "$source_path" "$staged_dir/$archive_name" \
                    || ! install_root_as_owner chmod 600 "$staged_dir/$archive_name"; then
                    install_root_as_owner rm -rf -- "$staged_dir" || true
                    die "Could not preserve legacy macOS supervisor file: $source_path"
                fi
            done ;;
        detached|detached-untracked)
            if ! legacy_detached_topology_expected \
                | install_root_as_owner dd \
                    of="$staged_dir/legacy-detached-topology" conv=fsync 2>/dev/null \
                || ! install_root_as_owner chmod 600 \
                    "$staged_dir/legacy-detached-topology"; then
                install_root_as_owner rm -rf -- "$staged_dir" || true
                die "Could not preserve the legacy detached process topology"
            fi
            if [ "$LEGACY_SUPERVISOR_KIND" = detached ] \
                && [ -f "$LEGACY_NODE_PID_FILE" ] && [ ! -L "$LEGACY_NODE_PID_FILE" ]; then
                if ! install_root_as_owner cp -p -- "$LEGACY_NODE_PID_FILE" \
                    "$staged_dir/legacy-node.pid" \
                    || ! install_root_as_owner chmod 600 "$staged_dir/legacy-node.pid"; then
                    install_root_as_owner rm -rf -- "$staged_dir" || true
                    die "Could not preserve the legacy detached PID file"
                fi
            fi ;;
    esac
    if ! install_root_as_owner chmod 700 "$staged_dir"; then
        install_root_as_owner rm -rf -- "$staged_dir" || true
        die "Could not protect the legacy configuration archive"
    fi
    sync
    if ! install_root_as_owner mv -n -- "$staged_dir" "$LEGACY_PRESERVED_DIR" \
        || [ -e "$staged_dir" ]; then
        install_root_as_owner rm -rf -- "$staged_dir" || true
        die "Legacy configuration archive appeared concurrently"
    fi
    sync
}

validate_legacy_supervisor_archive_binding() {
    [ "$LEGACY_ADOPTION_ACTIVE" = true ] || return 0
    if legacy_path_present "$ARC_DIR/bin/arc-auto-update.sh"; then
        validate_owned_nonwritable_path "$ARC_DIR/bin/arc-auto-update.sh" \
            "updater" executable "$(legacy_expected_owner_uid)"
        if [ ! -f "$LEGACY_PRESERVED_DIR/arc-auto-update.sh" ] \
            || ! cmp -s "$ARC_DIR/bin/arc-auto-update.sh" \
                "$LEGACY_PRESERVED_DIR/arc-auto-update.sh"; then
            die "Legacy updater changed after adoption was reserved"
        fi
    fi
    case "$LEGACY_SUPERVISOR_KIND" in
        linux-system)
            if legacy_path_present "$LEGACY_LINUX_NODE_UNIT"; then
                if cmp -s "$LEGACY_LINUX_NODE_UNIT" \
                    "$LEGACY_PRESERVED_DIR/legacy-linux-arc-node.service"; then
                    :
                elif [ "$LEGACY_PARTIAL_RESUME" = true ]; then
                    validate_partial_managed_system_user_node_unit
                else
                    die "Legacy Linux node unit changed after adoption was reserved"
                fi
            fi
            if legacy_path_present "$LEGACY_LINUX_UPDATER_SERVICE" \
                || legacy_path_present "$LEGACY_LINUX_UPDATER_TIMER"; then
                if [ ! -f "$LEGACY_LINUX_UPDATER_SERVICE" ] \
                    || [ ! -f "$LEGACY_LINUX_UPDATER_TIMER" ] \
                    || ! cmp -s "$LEGACY_LINUX_UPDATER_SERVICE" \
                        "$LEGACY_PRESERVED_DIR/legacy-linux-arc-updater.service" \
                    || ! cmp -s "$LEGACY_LINUX_UPDATER_TIMER" \
                        "$LEGACY_PRESERVED_DIR/legacy-linux-arc-updater.timer"; then
                    die "Legacy Linux updater units changed after adoption was reserved"
                fi
            fi ;;
        macos-launchd)
            if legacy_path_present "$LEGACY_MAC_NODE_PLIST"; then
                cmp -s "$LEGACY_MAC_NODE_PLIST" \
                    "$LEGACY_PRESERVED_DIR/legacy-macos-com.arc.inference.plist" \
                    || die "Legacy macOS node agent changed after adoption was reserved"
            fi
            if legacy_path_present "$LEGACY_MAC_UPDATER_PLIST"; then
                cmp -s "$LEGACY_MAC_UPDATER_PLIST" \
                    "$LEGACY_PRESERVED_DIR/legacy-macos-com.arc.updater.plist" \
                    || die "Legacy macOS updater agent changed after adoption was reserved"
            fi ;;
        detached|detached-untracked)
            load_legacy_detached_topology_archive
            if legacy_path_present "$LEGACY_NODE_PID_FILE"; then
                [ "$LEGACY_SUPERVISOR_KIND" = detached ] \
                    || die "Untracked legacy topology unexpectedly acquired node.pid"
                cmp -s "$LEGACY_NODE_PID_FILE" \
                    "$LEGACY_PRESERVED_DIR/legacy-node.pid" \
                    || die "Legacy detached PID changed after adoption was reserved"
            fi ;;
    esac
}

validate_completed_legacy_adoption() {
    local expected_uid node_version cli_version configured_version preserved_version preserved_name
    local active_address
    expected_uid="$(legacy_expected_owner_uid)"
    validate_legacy_default_ancestor_chain "$expected_uid"
    validate_owned_nonwritable_path "$ARC_DIR/bin" "bin directory" directory "$expected_uid"
    validate_owned_nonwritable_path "$ARC_DIR/bin/arc-node" "v0.8 node binary" executable "$expected_uid"
    validate_owned_nonwritable_path "$ARC_DIR/bin/arc-cli" "v0.8 CLI binary" executable "$expected_uid"
    validate_owned_nonwritable_path "$ARC_DIR/bin/run-arc-node" "v0.8 runner" executable "$expected_uid"
    validate_owned_nonwritable_path "$ARC_DIR/install.conf" "v0.8 installer config" file "$expected_uid"
    validate_owned_nonwritable_path "$KEY_FILE" \
        "migrated validator keyfile" file "$TARGET_UID"
    validate_owned_nonwritable_path "$LEGACY_PRESERVED_DIR" \
        "preserved configuration directory" directory "$expected_uid"
    for preserved_name in version.txt seeds.txt genesis.toml identity.seed; do
        validate_owned_nonwritable_path "$LEGACY_PRESERVED_DIR/$preserved_name" \
            "preserved $preserved_name" file "$expected_uid"
    done
    preserved_version="$(sed -n '1p' "$LEGACY_PRESERVED_DIR/version.txt")"
    preserved_version="${preserved_version#v}"
    [ "$preserved_version" = "$LEGACY_SOURCE_VERSION" ] \
        || die "Preserved legacy version does not match the pending adoption marker"
    if [ -e "$SEED_FILE" ] || [ -L "$SEED_FILE" ]; then
        die "Pending adoption still has an active legacy validator seed"
    fi
    if [ -e "$ARC_DIR/node.env" ] || [ -L "$ARC_DIR/node.env" ]; then
        die "Pending adoption still has a secret-bearing node.env"
    fi
    [ "$(sed -n '1p' "$ARC_DIR/install.conf")" = '# ARC installer state v1' ] \
        || die "Pending adoption has an unrecognized v0.8 installer config"
    configured_version="$(sed -n 's/^version=//p' "$ARC_DIR/install.conf")"
    printf '%s\n' "$configured_version" | grep -Eq '^[0-9]+\.[0-9]+\.[0-9]+$' \
        || die "Pending adoption has an invalid v0.8 configured version"
    node_version="$(read_managed_binary_version "$ARC_DIR/bin/arc-node")"
    cli_version="$(read_managed_binary_version "$ARC_DIR/bin/arc-cli")"
    if [ "$node_version" != "$configured_version" ] \
        || [ "$cli_version" != "$configured_version" ]; then
        die "Pending adoption binaries do not match install.conf"
    fi
    active_address="$(as_target "$ARC_DIR/bin/arc-cli" keygen --verify-keyfile "$KEY_FILE")" \
        || die "Pending adoption validator keyfile failed verification"
    printf '%s\n' "$active_address" | grep -Eq '^[0-9a-f]{64}$' \
        || die "Pending adoption validator keyfile produced an invalid public address"
    case "$configured_version" in
        0.0.*|0.1.*|0.2.*|0.3.*|0.4.*|0.5.*|0.6.*|0.7.*)
            die "Pending adoption did not reach a v0.8-or-newer managed install" ;;
    esac
}

promote_legacy_adoption_marker() {
    local staged_marker marker_mode
    validate_legacy_adoption_marker
    if [ -e "$INSTALL_ROOT_MARKER" ] || [ -L "$INSTALL_ROOT_MARKER" ]; then
        validate_install_root_marker
    else
        staged_marker="$(install_root_as_owner mktemp "$ARC_DIR/.install-root.new.XXXXXX")" \
            || die "Could not reserve the final ARC install-root marker"
        if ! install_root_marker_expected \
            | install_root_as_owner dd of="$staged_marker" conv=fsync 2>/dev/null; then
            install_root_as_owner rm -f -- "$staged_marker" || true
            die "Could not durably write the final ARC install-root marker"
        fi
        if [ "$SERVICE_SCOPE" = system ]; then marker_mode=444
        else marker_mode=600
        fi
        if ! install_root_as_owner chmod "$marker_mode" "$staged_marker"; then
            install_root_as_owner rm -f -- "$staged_marker" || true
            die "Could not protect the final ARC install-root marker"
        fi
        if ! install_root_as_owner mv -n -- "$staged_marker" "$INSTALL_ROOT_MARKER" \
            || [ -e "$staged_marker" ]; then
            install_root_as_owner rm -f -- "$staged_marker" || true
            die "Final ARC install-root marker appeared concurrently"
        fi
        sync
        validate_install_root_marker
    fi
    install_root_as_owner rm -f -- "$LEGACY_ADOPTION_MARKER"
    sync
    LEGACY_ADOPTION_ACTIVE=false
}

set_target_owner() {
    if [ "$CURRENT_UID" -eq 0 ]; then
        chown "$TARGET_USER:$TARGET_GROUP" "$@"
    fi
}

ensure_private_dir() {
    if [ "$CURRENT_UID" -eq 0 ] && [ "$TARGET_USER" != root ]; then
        # A sudo install without a system service uses paths owned by the
        # invoking user. Create them as that user instead of letting a root
        # mkdir/chmod follow a path the user can race.
        as_target mkdir -p -- "$1"
        as_target chmod 700 "$1"
    else
        mkdir -p -- "$1"
        chmod 700 "$1"
        set_target_owner "$1"
    fi
}

validate_root_managed_parent_chain() {
    local managed_path="$1" path_label="$2"
    local parent_path remainder component current_path=""
    local component_uid component_mode component_permissions
    parent_path="$(dirname -- "$managed_path")"
    remainder="${parent_path#/}"

    # `/` is part of every absolute path and must satisfy the same invariant.
    component_uid="$(stat -c %u -- /)" \
        || die "Could not inspect root while validating $path_label"
    component_mode="$(stat -c %a -- /)" \
        || die "Could not inspect root permissions while validating $path_label"
    [ "$component_uid" -eq 0 ] \
        || die "$path_label has a non-root-owned ancestor: /"
    case "$component_mode" in
        ''|*[!0-7]*) die "Could not parse permissions for $path_label ancestor: /" ;;
    esac
    component_permissions=$((8#$component_mode))
    [ $((component_permissions & 0022)) -eq 0 ] \
        || die "$path_label has a group/world-writable ancestor: /"

    while [ -n "$remainder" ]; do
        case "$remainder" in
            */*) component="${remainder%%/*}"; remainder="${remainder#*/}" ;;
            *) component="$remainder"; remainder="" ;;
        esac
        current_path="$current_path/$component"
        [ ! -L "$current_path" ] \
            || die "$path_label has a symlink ancestor: $current_path"
        [ -e "$current_path" ] || continue
        [ -d "$current_path" ] \
            || die "$path_label has a non-directory ancestor: $current_path"
        component_uid="$(stat -c %u -- "$current_path")" \
            || die "Could not inspect $path_label ancestor: $current_path"
        component_mode="$(stat -c %a -- "$current_path")" \
            || die "Could not inspect permissions for $path_label ancestor: $current_path"
        [ "$component_uid" -eq 0 ] \
            || die "$path_label parent chain must be root-owned: $current_path"
        case "$component_mode" in
            ''|*[!0-7]*)
                die "Could not parse permissions for $path_label ancestor: $current_path" ;;
        esac
        component_permissions=$((8#$component_mode))
        [ $((component_permissions & 0022)) -eq 0 ] \
            || die "$path_label parent chain must not be group/world writable: $current_path"
    done
}

if [ "$PURGE" = true ] && [ "$UNINSTALL" = false ]; then
    die "--purge is valid only with --uninstall"
fi

if [ "$UNINSTALL" = false ]; then
    if [ "$ARC_DIR_PREEXISTED" = true ]; then
        if [ -e "$INSTALL_ROOT_MARKER" ] || [ -L "$INSTALL_ROOT_MARKER" ]; then
            validate_install_root_marker
        elif [ "$LEGACY_ADOPTION_CANDIDATE" = true ]; then
            if [ -e "$LEGACY_ADOPTION_MARKER" ] || [ -L "$LEGACY_ADOPTION_MARKER" ]; then
                validate_legacy_adoption_marker
                pending_source_version="$LEGACY_SOURCE_VERSION"
                detected_installed_version="$(read_managed_binary_version "$ARC_DIR/bin/arc-node")"
                case "$detected_installed_version" in
                    0.7.*)
                        validate_legacy_v07_layout
                        [ "$LEGACY_SOURCE_VERSION" = "$pending_source_version" ] \
                            || die "Pending adoption source version changed after marker creation"
                        # Preserve compatibility with the old marker-before-
                        # archive crash shape, but defer every publication until
                        # the install lock is held and the unsigned updater is
                        # stopped. The locked phase revalidates the live source.
                        prepare_legacy_supervisor true
                        LEGACY_ADOPTION_EVIDENCE_NEEDS_VALIDATION=true ;;
                    0.8.*|0.9.*|[1-9][0-9]*.*)
                        LEGACY_SOURCE_VERSION="$pending_source_version"
                        LEGACY_PARTIAL_RESUME=true
                        # A newer binary proves the retirement/copy transaction
                        # already began, so its pre-transaction archive must be
                        # present before any partial supervisor is inspected.
                        validate_resumable_legacy_evidence
                        prepare_legacy_supervisor true
                        LEGACY_ADOPTION_EVIDENCE_NEEDS_VALIDATION=true ;;
                    *) die "Pending adoption contains an unknown partial node binary" ;;
                esac
                LEGACY_ADOPTION_ACTIVE=true
            else
                validate_legacy_v07_layout
                [ "$NODE_DATA_DIR" != "$ARC_DIR/data" ] \
                    || die "Legacy v0.7 data must be preserved; choose a fresh v0.8 data directory"
                if [ -e "$NODE_DATA_DIR" ] || [ -L "$NODE_DATA_DIR" ]; then
                    die "Legacy adoption requires a new, unused v0.8 data directory: $NODE_DATA_DIR"
                fi
                prepare_legacy_supervisor false
                LEGACY_ADOPTION_MARKER_NEEDS_CREATE=true
                LEGACY_ADOPTION_EVIDENCE_NEEDS_VALIDATION=true
                LEGACY_ADOPTION_ACTIVE=true
            fi
        else
            validate_install_root_marker
        fi
    elif [ "$UPDATE_ONLY" = true ]; then
        die "--update-only requires an existing marked ARC installation in $ARC_DIR"
    fi
else
    # Prove ownership before stopping services or removing even individually
    # named files. A pending legacy-adoption marker is intentionally not an
    # uninstall or purge capability. Purge checks the marker again immediately
    # before recursion.
    validate_install_root_marker
fi

if [ "$SERVICE_SCOPE" = system-user ]; then
    if [ "$LEGACY_ADOPTION_ACTIVE" = true ]; then
        SYSTEM_USER_BOOTSTRAP=true
    else
        validate_managed_system_user_units
    fi
fi

if [ "$SERVICE_SCOPE" = system ]; then
    # Both trees will be created and permissioned by root. Every existing
    # parent is verified before the first privileged filesystem mutation.
    validate_root_managed_parent_chain "$ARC_DIR" "System install directory"
    validate_root_managed_parent_chain "$NODE_DATA_DIR" "System data directory"
elif [ "$CURRENT_UID" -eq 0 ] && [ "$TARGET_USER" = root ]; then
    # A direct-root install has no less privilege merely because it omitted a
    # service. Keep root mkdir/chmod away from attacker-writable ancestors.
    validate_root_managed_parent_chain "$ARC_DIR" "Root install directory"
    validate_root_managed_parent_chain "$NODE_DATA_DIR" "Root data directory"
fi

case "$SERVICE_SCOPE" in
    system)
        command -v systemctl >/dev/null 2>&1 \
            || die "systemctl is unavailable; use --no-service for install-only mode"
        if [ "$CURRENT_UID" -ne 0 ]; then
            command -v sudo >/dev/null 2>&1 \
                || die "A system service requires sudo/root; use --user-service instead"
            sudo -v || die "sudo authorization failed; use --user-service or --no-service"
        fi ;;
    system-user)
        command -v systemctl >/dev/null 2>&1 \
            || die "systemctl is unavailable for the managed system-user service"
        if [ "$SYSTEM_USER_BOOTSTRAP" = true ] && [ "$CURRENT_UID" -ne 0 ]; then
            command -v sudo >/dev/null 2>&1 \
                || die "The one-time legacy system-unit migration requires sudo"
            sudo -v || die "sudo authorization failed before the one-time legacy migration"
        fi ;;
    user)
        command -v systemctl >/dev/null 2>&1 \
            || die "systemctl is unavailable; use --no-service for install-only mode"
        systemctl --user show-environment >/dev/null 2>&1 \
            || die "No systemd user manager is reachable. Use sudo --system-service, or --no-service." ;;
    launchd)
        command -v launchctl >/dev/null 2>&1 || die "launchctl is unavailable" ;;
esac

uninstall_arc() {
    info "Removing ARC services and installed programs"
    case "$SERVICE_SCOPE" in
        system|system-user)
            as_root systemctl disable --now arc-node.timer arc-node-update.timer arc-node.service 2>/dev/null || true
            as_root rm -f -- \
                "$SYSTEMD_UNIT_DIR/arc-node.service" \
                "$SYSTEMD_UNIT_DIR/arc-node-update.service" \
                "$SYSTEMD_UNIT_DIR/arc-node-update.timer"
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
        validate_managed_directory_components "$ARC_DIR" "install directory"
        validate_install_root_marker
        if [ "$SERVICE_SCOPE" = system ]; then
            as_root rm -rf -- "$ARC_DIR"
        elif [ "$CURRENT_UID" -eq 0 ] && [ "$TARGET_USER" != root ]; then
            # A sudo install-only tree belongs to the invoking user. Purging
            # as that user avoids turning a user-controlled rename race into
            # an arbitrary root deletion primitive.
            as_target rm -rf -- "$ARC_DIR"
        else
            rm -rf -- "$ARC_DIR"
        fi
        case "$NODE_DATA_DIR/" in
            "$ARC_DIR/"*) ;;
            *) warn "External data directory was preserved: $NODE_DATA_DIR" ;;
        esac
        if [ -e "$ARC_DIR" ] || [ -L "$ARC_DIR" ]; then
            die "ARC install root could not be completely purged: $ARC_DIR"
        fi
        ok "Marked ARC install root, identity, and contained chain data removed from $ARC_DIR"
    else
        ok "Programs removed. Identity and chain data remain in $ARC_DIR"
    fi
}

if [ "$UNINSTALL" = true ]; then
    uninstall_arc
    exit 0
fi

if [ "$ARC_DIR_PREEXISTED" = false ]; then
    create_marked_install_root
    ARC_DIR_PREEXISTED=true
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
    if [ $(((PARENT_MODE_DECIMAL / 10) % 10 & 2)) -ne 0 ] \
        || [ $((PARENT_MODE_DECIMAL % 10 & 2)) -ne 0 ]; then
        die "System install parent must not be group/world writable: $INSTALL_PARENT"
    fi
    as_root mkdir -p -- "$ARC_DIR" "$ARC_DIR/bin" "$ARC_DIR/identity"
    # A v0.7 adoption must prove that the v0.8 state path is still absent.
    # Create it only after the compiled retirement verifier has published the
    # matching offline receipt. Fresh installs and ordinary v0.8 updates keep
    # the normal eager directory preparation.
    if [ "$LEGACY_ADOPTION_ACTIVE" = false ]; then
        as_root mkdir -p -- "$NODE_DATA_DIR"
    fi
    as_root chown root:root "$ARC_DIR" "$ARC_DIR/bin"
    as_root chmod 755 "$ARC_DIR" "$ARC_DIR/bin"
    as_root chown "$TARGET_USER:$TARGET_GROUP" "$ARC_DIR/identity"
    as_root chmod 700 "$ARC_DIR/identity"
    if [ "$LEGACY_ADOPTION_ACTIVE" = false ]; then
        as_root chown "$TARGET_USER:$TARGET_GROUP" "$NODE_DATA_DIR"
        as_root chmod 700 "$NODE_DATA_DIR"
    fi
else
    ensure_private_dir "$ARC_DIR"
    ensure_private_dir "$ARC_DIR/bin"
    ensure_private_dir "$ARC_DIR/identity"
    if [ "$LEGACY_ADOPTION_ACTIVE" = false ]; then
        ensure_private_dir "$NODE_DATA_DIR"
    fi
fi
if [ "$SERVICE_SCOPE" != system ]; then
    as_target test -w "$ARC_DIR" \
        || die "Install user $TARGET_USER cannot write $ARC_DIR"
fi
if [ "$LEGACY_ADOPTION_ACTIVE" = false ]; then
    as_target test -w "$NODE_DATA_DIR" \
        || die "Install user $TARGET_USER cannot write data directory $NODE_DATA_DIR"
fi
if [ -n "$MODEL_PATH" ]; then
    as_target test -r "$MODEL_PATH" \
        || die "Install user $TARGET_USER cannot read model $MODEL_PATH"
fi

TRANSACTION_ACTIVE=false
TRANSACTION_COMMITTED=false
TRANSACTION_ROLLING_BACK=false
LEGACY_UPDATER_FENCE_LAUNCHD_DOMAIN=""

ensure_no_legacy_updater_processes() {
    local process_rows pid process_uid command_line updater_path
    [ "$LEGACY_ADOPTION_ACTIVE" = true ] || return 0
    updater_path="$ARC_DIR/bin/arc-auto-update.sh"
    process_rows="$(ps -ww -axo pid=,uid=,command= 2>/dev/null)" \
        || die "Could not enumerate processes while fencing the legacy updater"
    while read -r pid process_uid command_line; do
        [ -n "$pid" ] || continue
        case "$command_line" in
            "$updater_path"|"$updater_path "*|*" $updater_path"|*" $updater_path "*)
                die "A legacy unsigned updater process is still running after its schedule was fenced" ;;
        esac
    done <<< "$process_rows"
}

revalidate_legacy_sources_after_updater_fence() {
    local source_version_before="$LEGACY_SOURCE_VERSION"
    [ "$LEGACY_ADOPTION_ACTIVE" = true ] || return 0
    ensure_no_legacy_updater_processes
    if [ "$LEGACY_PARTIAL_RESUME" = false ]; then
        validate_legacy_v07_layout
        [ "$LEGACY_SOURCE_VERSION" = "$source_version_before" ] \
            || die "Legacy node version changed while the updater was being fenced"
        preserve_legacy_v07_configuration
    fi
    validate_legacy_supervisor_archive_binding
}

fence_legacy_updater_before_network() {
    [ "$LEGACY_ADOPTION_ACTIVE" = true ] || return 0
    case "$LEGACY_SUPERVISOR_KIND" in
        linux-system)
            if legacy_path_present "$LEGACY_LINUX_UPDATER_SERVICE"; then
                as_root systemctl disable --now arc-updater.timer >/dev/null 2>&1 \
                    || die "Could not fence the legacy updater timer"
                as_root systemctl disable --now arc-updater.service >/dev/null 2>&1 \
                    || die "Could not fence the running legacy updater"
                if as_root systemctl is-active --quiet arc-updater.timer \
                    || as_root systemctl is-enabled --quiet arc-updater.timer \
                    || as_root systemctl is-active --quiet arc-updater.service \
                    || as_root systemctl is-enabled --quiet arc-updater.service; then
                    die "Legacy updater schedule remained active after fencing"
                fi
            fi ;;
        macos-launchd)
            LEGACY_UPDATER_FENCE_LAUNCHD_DOMAIN="user/$TARGET_UID"
            if launchctl print "gui/$TARGET_UID" >/dev/null 2>&1; then
                LEGACY_UPDATER_FENCE_LAUNCHD_DOMAIN="gui/$TARGET_UID"
            fi
            if legacy_path_present "$LEGACY_MAC_UPDATER_PLIST"; then
                launchctl disable \
                    "$LEGACY_UPDATER_FENCE_LAUNCHD_DOMAIN/com.arc.updater" \
                    >/dev/null 2>&1 \
                    || die "Could not persistently fence the legacy macOS updater"
            fi
            if launchctl print \
                "$LEGACY_UPDATER_FENCE_LAUNCHD_DOMAIN/com.arc.updater" \
                >/dev/null 2>&1; then
                LEGACY_MAC_UPDATER_LOADED=true
                launchctl bootout \
                    "$LEGACY_UPDATER_FENCE_LAUNCHD_DOMAIN/com.arc.updater" \
                    >/dev/null 2>&1 \
                    || die "Could not fence the legacy macOS updater"
                if launchctl print \
                    "$LEGACY_UPDATER_FENCE_LAUNCHD_DOMAIN/com.arc.updater" \
                    >/dev/null 2>&1; then
                    die "Legacy macOS updater remained loaded after fencing"
                fi
            fi ;;
    esac
    # The v0.7 updater ignores this install lock. Re-read every source after
    # it is definitely absent so a mid-reservation replacement cannot cross
    # the transaction boundary unnoticed. Once fenced, it intentionally stays
    # fenced on every failure: durable migration evidence must never coexist
    # with an unsigned updater that can invalidate that evidence.
    revalidate_legacy_sources_after_updater_fence
}

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
IDENTITY_CHECK_FILE=""
cleanup() {
    local status=$?
    trap - EXIT HUP INT TERM
    if [ "$TRANSACTION_ACTIVE" = true ] \
        && [ "$TRANSACTION_COMMITTED" = false ] \
        && [ "$TRANSACTION_ROLLING_BACK" = false ]; then
        rollback_install_transaction || true
    fi
    if [ -n "$IDENTITY_CHECK_FILE" ]; then
        rm -f -- "$IDENTITY_CHECK_FILE"
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

fence_legacy_updater_before_network
if [ "$LEGACY_ADOPTION_ACTIVE" = true ]; then
    # No marker or archive is published until the install lock is held and the
    # unsigned v0.7 updater is absent. Archive-first publication means a crash
    # cannot pin stale source bytes behind a live updater schedule.
    if [ "$LEGACY_ADOPTION_MARKER_NEEDS_CREATE" = true ]; then
        create_legacy_adoption_marker
        LEGACY_ADOPTION_MARKER_NEEDS_CREATE=false
    fi
    if [ "$LEGACY_ADOPTION_EVIDENCE_NEEDS_VALIDATION" = true ]; then
        validate_resumable_legacy_evidence
        LEGACY_ADOPTION_EVIDENCE_NEEDS_VALIDATION=false
    fi
    validate_legacy_adoption_marker
    validate_legacy_supervisor_archive_binding
fi

NODE_ASSET="arc-node-$PLATFORM"
CLI_ASSET="arc-cli-$PLATFORM"
HEADLESS_CHANNEL_MINIMUM_VERSION=0.8.0

strict_version() {
    printf '%s\n' "$1" | grep -Eq '^[0-9]+\.[0-9]+\.[0-9]+$'
}

# Bash arithmetic is signed and accepts implementation-specific integer
# spellings.  Network evidence is compared arithmetically below, so accept
# only canonical decimal integers small enough to remain portable.
canonical_uint() {
    local value="$1"
    [ -n "$value" ] && [ "${#value}" -le 18 ] || return 1
    case "$value" in
        *[!0-9]*) return 1 ;;
        0|[1-9]*) return 0 ;;
        *) return 1 ;;
    esac
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

# GitHub's global `latest` pointer must remain on the v0.7-compatible release
# until every unsigned v0.7 updater has been retired.  New headless installs
# and the signed v0.8 updater therefore discover their own channel from the
# release collection, then resolve and authenticate one exact immutable tag.
#
# This small JSON scanner deliberately recognizes only a top-level release
# object's literal `tag_name` string.  A release body containing escaped JSON,
# or a nested asset object, cannot inject a candidate.  The selected tag is
# still treated only as discovery: the exact-tag response, protected source
# commit, owner-signed manifest, and every payload are verified below.
extract_top_level_release_tags() {
    awk '
        BEGIN {
            object_depth = 0
            array_depth = 0
            root_started = 0
            root_closed = 0
            in_string = 0
            escaped = 0
            token = ""
            token_had_escape = 0
            tag_key_pending = 0
            tag_value_pending = 0
            capture_tag_value = 0
        }
        {
            line = $0 "\n"
            for (i = 1; i <= length(line); i++) {
                ch = substr(line, i, 1)
                if (in_string) {
                    if (escaped) {
                        token_had_escape = 1
                        escaped = 0
                        continue
                    }
                    if (ch == "\\") {
                        escaped = 1
                        continue
                    }
                    if (ch == "\"") {
                        in_string = 0
                        if (capture_tag_value) {
                            if (!token_had_escape) print token
                            capture_tag_value = 0
                        } else if (object_depth == 1 && !token_had_escape && token == "tag_name") {
                            tag_key_pending = 1
                        }
                        token = ""
                        token_had_escape = 0
                        continue
                    }
                    token = token ch
                    continue
                }

                if (root_closed && ch !~ /[[:space:]]/) exit 2

                if (tag_key_pending) {
                    if (ch ~ /[[:space:]]/) continue
                    tag_key_pending = 0
                    if (ch == ":") {
                        tag_value_pending = 1
                        continue
                    }
                }
                if (tag_value_pending) {
                    if (ch ~ /[[:space:]]/) continue
                    tag_value_pending = 0
                    if (ch == "\"" && object_depth == 1) {
                        in_string = 1
                        token = ""
                        token_had_escape = 0
                        capture_tag_value = 1
                        continue
                    }
                }

                if (ch == "\"") {
                    if (!root_started || root_closed) exit 2
                    in_string = 1
                    token = ""
                    token_had_escape = 0
                } else if (ch == "{") {
                    if (!root_started) root_started = 1
                    object_depth += 1
                } else if (ch == "}") {
                    if (object_depth <= 0) exit 2
                    object_depth -= 1
                    tag_key_pending = 0
                    tag_value_pending = 0
                    if (object_depth == 0 && array_depth == 0) root_closed = 1
                } else if (ch == "[") {
                    if (!root_started) root_started = 1
                    array_depth += 1
                } else if (ch == "]") {
                    if (array_depth <= 0) exit 2
                    array_depth -= 1
                    if (object_depth == 0 && array_depth == 0) root_closed = 1
                } else if (!root_started && ch !~ /[[:space:]]/) {
                    exit 2
                }
            }
        }
        END {
            if (in_string || escaped || object_depth != 0 || array_depth != 0 \
                || !root_started || !root_closed) exit 2
        }
    ' "$1"
}

# Emit exactly one top-level JSON scalar as either `string<TAB>value` or
# `bare<TAB>value`. This keeps the installer dependency-free on minimal Linux
# and macOS hosts while avoiding regex matches inside nested objects, arrays,
# or quoted prose. Escaped top-level keys and escaped selected values fail
# closed because every field consumed below has a canonical ASCII spelling.
extract_unique_top_level_json_scalar() {
    local json_file="$1" wanted_key="$2"
    awk -v wanted="$wanted_key" '
        function fail() { failed = 1; exit 2 }
        function emit(kind, value) {
            if (found) fail()
            print kind "\t" value
            found = 1
        }
        BEGIN {
            mode = "before"
            object_depth = 0
            array_depth = 0
            in_string = 0
            escaped = 0
            string_kind = ""
            token = ""
            token_had_escape = 0
            key = ""
            root_closed = 0
            found = 0
            failed = 0
        }
        {
            line = $0 "\n"
            for (i = 1; i <= length(line); i++) {
                ch = substr(line, i, 1)
                if (root_closed) {
                    if (ch !~ /[[:space:]]/) fail()
                    continue
                }
                if (in_string) {
                    if (escaped) {
                        token_had_escape = 1
                        escaped = 0
                        token = token ch
                        continue
                    }
                    if (ch == "\\") {
                        escaped = 1
                        token_had_escape = 1
                        continue
                    }
                    if (ch == "\"") {
                        in_string = 0
                        if (string_kind == "key") {
                            if (token_had_escape) fail()
                            key = token
                            mode = "colon"
                        } else if (string_kind == "value") {
                            if (key == wanted) {
                                if (token_had_escape) fail()
                                emit("string", token)
                            }
                            mode = "comma"
                        }
                        string_kind = ""
                        token = ""
                        token_had_escape = 0
                        continue
                    }
                    token = token ch
                    continue
                }

                if (mode == "before") {
                    if (ch ~ /[[:space:]]/) continue
                    if (ch != "{") fail()
                    object_depth = 1
                    mode = "key"
                    continue
                }

                if (mode == "nested") {
                    if (ch == "\"") {
                        in_string = 1
                        string_kind = "nested"
                        token = ""
                        token_had_escape = 0
                    } else if (ch == "{") {
                        object_depth += 1
                    } else if (ch == "}") {
                        if (object_depth <= 1) fail()
                        object_depth -= 1
                        if (object_depth == 1 && array_depth == 0) mode = "comma"
                    } else if (ch == "[") {
                        array_depth += 1
                    } else if (ch == "]") {
                        if (array_depth <= 0) fail()
                        array_depth -= 1
                        if (object_depth == 1 && array_depth == 0) mode = "comma"
                    }
                    continue
                }

                if (mode == "key") {
                    if (ch ~ /[[:space:]]/) continue
                    if (ch == "}") {
                        object_depth = 0
                        root_closed = 1
                        continue
                    }
                    if (ch != "\"") fail()
                    in_string = 1
                    string_kind = "key"
                    token = ""
                    token_had_escape = 0
                    continue
                }
                if (mode == "colon") {
                    if (ch ~ /[[:space:]]/) continue
                    if (ch != ":") fail()
                    mode = "value"
                    continue
                }
                if (mode == "value") {
                    if (ch ~ /[[:space:]]/) continue
                    if (ch == "\"") {
                        in_string = 1
                        string_kind = "value"
                        token = ""
                        token_had_escape = 0
                    } else if (ch == "{") {
                        object_depth += 1
                        mode = "nested"
                    } else if (ch == "[") {
                        array_depth += 1
                        mode = "nested"
                    } else {
                        token = ch
                        mode = "bare"
                    }
                    continue
                }
                if (mode == "bare") {
                    if (ch == "," || ch == "}") {
                        sub(/^[[:space:]]+/, "", token)
                        sub(/[[:space:]]+$/, "", token)
                        if (token == "" || token ~ /[[:space:]\"{}\[\],:]/) fail()
                        if (key == wanted) emit("bare", token)
                        token = ""
                        if (ch == ",") {
                            mode = "key"
                        } else {
                            object_depth = 0
                            root_closed = 1
                        }
                    } else {
                        token = token ch
                    }
                    continue
                }
                if (mode == "comma") {
                    if (ch ~ /[[:space:]]/) continue
                    if (ch == ",") {
                        mode = "key"
                    } else if (ch == "}") {
                        object_depth = 0
                        root_closed = 1
                    } else {
                        fail()
                    }
                }
            }
        }
        END {
            if (failed || in_string || escaped || !root_closed \
                || object_depth != 0 || array_depth != 0 || !found) exit 2
        }
    ' "$json_file"
}

top_level_json_string() {
    local row kind value
    row="$(extract_unique_top_level_json_scalar "$1" "$2")" || return 1
    kind="${row%%$'\t'*}"
    value="${row#*$'\t'}"
    [ "$kind" = string ] && [ "$value" != "$row" ] || return 1
    printf '%s\n' "$value"
}

top_level_json_bare() {
    local row kind value
    row="$(extract_unique_top_level_json_scalar "$1" "$2")" || return 1
    kind="${row%%$'\t'*}"
    value="${row#*$'\t'}"
    [ "$kind" = bare ] && [ "$value" != "$row" ] || return 1
    printf '%s\n' "$value"
}

select_headless_channel_version() {
    local releases_url="$API_ROOT/releases?per_page=100"
    local tag candidate_version comparison candidate_metadata
    if ! github_curl "$releases_url" > "$TMP_DIR/releases.json"; then
        die "Could not read the ARC v0.8 release channel: $releases_url"
    fi
    if ! extract_top_level_release_tags "$TMP_DIR/releases.json" \
        > "$TMP_DIR/release-tags"; then
        die "GitHub returned malformed release-channel metadata"
    fi
    : > "$TMP_DIR/release-candidates"
    while IFS= read -r tag; do
        case "$tag" in v*) tag="${tag#v}" ;; *) continue ;; esac
        strict_version "$tag" || continue
        comparison="$(version_compare "$tag" "$HEADLESS_CHANNEL_MINIMUM_VERSION")"
        [ "$comparison" -ge 0 ] || continue
        if ! grep -Fxq -- "$tag" "$TMP_DIR/release-candidates"; then
            printf '%s\n' "$tag" >> "$TMP_DIR/release-candidates"
        fi
    done < "$TMP_DIR/release-tags"
    [ -s "$TMP_DIR/release-candidates" ] \
        || die "No ARC v0.8+ release tag is discoverable; use --version only with a reviewed exact tag"

    # A tag name in the collection is discovery input, not a channel member.
    # Walk candidates from highest to lowest and exact-resolve each one until
    # an immutable stable protected-publisher release is found. Otherwise a
    # manually published v999.0.0 draft/prerelease could poison every unpinned
    # install even though a valid lower stable release still exists.
    : > "$TMP_DIR/release-candidates-rejected"
    while :; do
        candidate_version=""
        while IFS= read -r tag; do
            grep -Fxq -- "$tag" "$TMP_DIR/release-candidates-rejected" && continue
            if [ -z "$candidate_version" ] \
                || [ "$(version_compare "$tag" "$candidate_version")" -gt 0 ]; then
                candidate_version="$tag"
            fi
        done < "$TMP_DIR/release-candidates"
        [ -n "$candidate_version" ] || break

        candidate_metadata="$TMP_DIR/release-candidate-$candidate_version.json"
        if github_curl "$API_ROOT/releases/tags/v$candidate_version" \
            > "$candidate_metadata" \
            && release_metadata_is_channel_eligible \
                "$candidate_metadata" "$candidate_version"; then
            mv -- "$candidate_metadata" "$TMP_DIR/release.json"
            REQUESTED_VERSION="$candidate_version"
            RELEASE_METADATA_PREFETCHED=true
            return 0
        fi
        printf '%s\n' "$candidate_version" >> "$TMP_DIR/release-candidates-rejected"
        rm -f -- "$candidate_metadata"
    done
    die "No immutable stable ARC v0.8+ release from the protected publisher is discoverable"
}

release_metadata_is_channel_eligible() {
    local metadata_file="$1" expected_version="$2"
    local key expected rejected flattened tags resolved
    for key in immutable draft prerelease; do
        case "$key" in
            immutable) expected=true; rejected=false ;;
            *) expected=false; rejected=true ;;
        esac
        grep -Eq "\"$key\"[[:space:]]*:[[:space:]]*$expected([[:space:]]*[,}])" \
            "$metadata_file" || return 1
        if grep -Eq "\"$key\"[[:space:]]*:[[:space:]]*$rejected([[:space:]]*[,}])" \
            "$metadata_file"; then
            return 1
        fi
    done
    flattened="$TMP_DIR/release-channel-one-line-$expected_version.json"
    tr -d '\r\n' < "$metadata_file" > "$flattened"
    grep -Eq '"author"[[:space:]]*:[[:space:]]*\{[[:space:]]*"login"[[:space:]]*:[[:space:]]*"github-actions\[bot\]"([[:space:]]*[,}])' \
        "$flattened" || return 1
    tags="$TMP_DIR/release-channel-tags-$expected_version"
    extract_top_level_release_tags "$metadata_file" > "$tags" || return 1
    [ "$(awk 'END { print NR + 0 }' "$tags")" -eq 1 ] || return 1
    resolved="$(sed -n '1p' "$tags")"
    [ "$resolved" = "v$expected_version" ]
}

RELEASE_METADATA_PREFETCHED=false

if [ -n "$REQUESTED_VERSION" ]; then
    REQUESTED_VERSION="${REQUESTED_VERSION#v}"
    strict_version "$REQUESTED_VERSION" \
        || die "Version must be strict X.Y.Z (got: $REQUESTED_VERSION)"
else
    select_headless_channel_version
fi
RELEASE_METADATA_URL="$API_ROOT/releases/tags/v$REQUESTED_VERSION"

info "Resolving a complete release for $PLATFORM"
if [ "$RELEASE_METADATA_PREFETCHED" = false ] \
    && ! github_curl "$RELEASE_METADATA_URL" > "$TMP_DIR/release.json"; then
    die "Could not read GitHub release metadata: $RELEASE_METADATA_URL"
fi

# SHA256SUMS and the payloads it authenticates live in the same GitHub
# release. A mutable release could replace both together, so a checksum alone
# is not an authenticity boundary. The protected publisher uses the repository
# GITHUB_TOKEN, which GitHub records as github-actions[bot]. Require that
# server-authenticated author as defense in depth; the owner-controlled
# detached manifest signature below remains the cryptographic trust boundary.
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

# GitHub's release API emits the top-level author object with `login` first.
# Flatten whitespace only for this exact server-owned field; do not accept a
# generic nested `login` from an asset uploader or other object.
tr -d '\r\n' < "$TMP_DIR/release.json" > "$TMP_DIR/release-one-line.json"
grep -Eq '"author"[[:space:]]*:[[:space:]]*\{[[:space:]]*"login"[[:space:]]*:[[:space:]]*"github-actions\[bot\]"([[:space:]]*[,}])' \
    "$TMP_DIR/release-one-line.json" \
    || die "Release metadata author must be github-actions[bot] from the protected publisher"

if ! extract_top_level_release_tags "$TMP_DIR/release.json" \
    > "$TMP_DIR/exact-release-tags"; then
    die "Exact release metadata is malformed"
fi
[ "$(awk 'END { print NR + 0 }' "$TMP_DIR/exact-release-tags")" -eq 1 ] \
    || die "Exact release metadata must contain exactly one top-level tag"
RESOLVED_TAG="$(sed -n '1p' "$TMP_DIR/exact-release-tags")"
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
    if [ "$UPDATE_ONLY" = true ] && [ "$COMPARISON" -eq 0 ] \
        && [ "$LEGACY_ADOPTION_ACTIVE" = false ]; then
        ok "Already up to date at v$INSTALLED_VERSION"
        exit 0
    fi
fi

RELEASE_URL="$DOWNLOAD_ROOT/$RESOLVED_TAG"
info "Downloading checksums for $RESOLVED_TAG"
github_curl "$RELEASE_URL/SHA256SUMS" > "$TMP_DIR/SHA256SUMS" \
    || die "Could not download $RELEASE_URL/SHA256SUMS"
github_curl "$RELEASE_URL/SHA256SUMS.sig" > "$TMP_DIR/SHA256SUMS.sig" \
    || die "Could not download $RELEASE_URL/SHA256SUMS.sig"

# The bootstrap installer comes from the owner-created protected source tag.
# This detached signature extends that trust to every release asset and defeats
# a manual or alternate-workflow first publication, even if GitHub reports it
# as immutable and authored by github-actions[bot].
printf '%s\n' \
    'arc-release namespaces="arc-release-manifest-v1" ssh-ed25519 AAAAC3NzaC1lZDI1NTE5AAAAIPs2NAiDRXit9EM96A2GdXZgRqvXtl0lvryEAEAEjQfY arc-release-manifest-v1' \
    > "$TMP_DIR/arc-release-allowed-signers"
if ! ssh-keygen -Y verify \
    -f "$TMP_DIR/arc-release-allowed-signers" \
    -I arc-release \
    -n arc-release-manifest-v1 \
    -s "$TMP_DIR/SHA256SUMS.sig" \
    < "$TMP_DIR/SHA256SUMS" >/dev/null 2>&1; then
    die "Release SHA256SUMS signature is invalid or not owner-authorized"
fi

if ! github_curl "$API_ROOT/commits/$RESOLVED_TAG" > "$TMP_DIR/tag-commit.json"; then
    die "Could not resolve the protected source tag commit: $RESOLVED_TAG"
fi
tr -d '\r\n' < "$TMP_DIR/tag-commit.json" > "$TMP_DIR/tag-commit-one-line.json"
RESOLVED_COMMIT="$(sed -n \
    's/^[[:space:]]*{[[:space:]]*"sha"[[:space:]]*:[[:space:]]*"\([0-9a-f]\{40\}\)".*/\1/p' \
    "$TMP_DIR/tag-commit-one-line.json")"
printf '%s\n' "$RESOLVED_COMMIT" | grep -Eq '^[0-9a-f]{40}$' \
    || die "Protected source tag did not resolve to an exact Git commit"

[ "$(sed -n '1p' "$TMP_DIR/SHA256SUMS")" = '# ARC release manifest v1' ] \
    || die "Signed release manifest has the wrong schema"
[ "$(sed -n '2p' "$TMP_DIR/SHA256SUMS")" = '# repository=FerrumVir/arc-chain' ] \
    || die "Signed release manifest targets the wrong repository"
[ "$(sed -n '3p' "$TMP_DIR/SHA256SUMS")" = "# tag=$RESOLVED_TAG" ] \
    || die "Signed release manifest targets the wrong tag"
[ "$(sed -n '4p' "$TMP_DIR/SHA256SUMS")" = "# commit=$RESOLVED_COMMIT" ] \
    || die "Signed release manifest targets the wrong source commit"
[ "$(grep -Ec '^# ' "$TMP_DIR/SHA256SUMS")" -eq 4 ] \
    || die "Signed release manifest contains unexpected metadata fields"

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

LEGACY_BOUNDARY_ASSET=arc-legacy-maintenance-boundary.json
RECOVERY_DESCRIPTOR_ASSET=arc-recovery-checkpoint-descriptor.json
CUTOVER_POLICY_ASSET=arc-cutover-policy.json
RELEASE_INSTALLER_BINDING="$TMP_DIR/arc-release-installer-binding.json"
RETIREMENT_MANIFEST_HASH=""
RETIREMENT_NETWORK_GENESIS_HASH=""
RETIREMENT_RECOVERY_DOMAIN=""
RETIREMENT_RECOVERY_EPOCH=""
RETIREMENT_VALIDATOR_SET_ID=""
RETIREMENT_SOURCE_HEIGHT=""
RETIREMENT_TRANSITION_HEIGHT=""

write_release_installer_binding() {
    local records="$TMP_DIR/release-binding-files" sorted="$TMP_DIR/release-binding-files-sorted"
    local asset digest first=true
    : > "$records"
    for asset in \
        "$NODE_ASSET" "$CLI_ASSET" genesis.toml \
        "$LEGACY_BOUNDARY_ASSET" "$RECOVERY_DESCRIPTOR_ASSET" "$CUTOVER_POLICY_ASSET"; do
        printf '%s\n' "$asset" | grep -Eq '^[A-Za-z0-9][A-Za-z0-9._-]*$' \
            || die "Internal release-binding asset name is unsafe: $asset"
        digest="$(sha256_file "$TMP_DIR/$asset")"
        printf '%s\n' "$digest" | grep -Eq '^[0-9a-f]{64}$' \
            || die "Could not hash release-binding asset: $asset"
        printf '%s\t%s\n' "$asset" "$digest" >> "$records"
    done
    LC_ALL=C sort -t $'\t' -k1,1 "$records" > "$sorted" \
        || die "Could not canonicalize the release binding"
    [ "$(cut -f1 "$sorted" | uniq -d | awk 'END { print NR + 0 }')" -eq 0 ] \
        || die "Release binding contains a duplicate asset"
    {
        printf '{"commit":"%s","files":{' "$RESOLVED_COMMIT"
        while IFS=$'\t' read -r asset digest; do
            if [ "$first" = true ]; then first=false; else printf ','; fi
            printf '"%s":"%s"' "$asset" "$digest"
        done < "$sorted"
        printf '},"manifest_signature_sha256":"%s"' \
            "$(sha256_file "$TMP_DIR/SHA256SUMS.sig")"
        printf ',"repository":"FerrumVir/arc-chain"'
        printf ',"schema":"arc.release-installer-binding.v1"'
        printf ',"signed_manifest_sha256":"%s"' \
            "$(sha256_file "$TMP_DIR/SHA256SUMS")"
        printf ',"tag":"%s"}\n' "$RESOLVED_TAG"
    } > "$RELEASE_INSTALLER_BINDING" \
        || die "Could not write the local release binding"
    chmod 600 "$RELEASE_INSTALLER_BINDING" \
        || die "Could not protect the local release binding"
}

verify_recovery_descriptor_for_retirement() {
    local output="$TMP_DIR/recovery-descriptor-verification.json"
    local status hash_value
    if ! "$TMP_DIR/$NODE_ASSET" recovery verify-descriptor \
        --descriptor "$TMP_DIR/$RECOVERY_DESCRIPTOR_ASSET" \
        --genesis "$TMP_DIR/genesis.toml" > "$output"; then
        die "The signed compact recovery descriptor failed cryptographic verification"
    fi
    [ -f "$output" ] && [ ! -L "$output" ] \
        && [ "$(wc -c < "$output" | tr -d ' ')" -le 1048576 ] \
        || die "Recovery descriptor verifier returned unsafe output"
    status="$(top_level_json_string "$output" status)" \
        || die "Recovery descriptor verifier returned no unique status"
    [ "$status" = VERIFIED_DESCRIPTOR_QUORUM ] \
        || die "Recovery descriptor verifier did not prove validator quorum"
    RETIREMENT_MANIFEST_HASH="$(top_level_json_string "$output" manifest_hash)" \
        || die "Recovery descriptor verifier omitted manifest_hash"
    RETIREMENT_NETWORK_GENESIS_HASH="$(top_level_json_string "$output" network_genesis_hash)" \
        || die "Recovery descriptor verifier omitted network_genesis_hash"
    RETIREMENT_RECOVERY_DOMAIN="$(top_level_json_string "$output" recovery_domain)" \
        || die "Recovery descriptor verifier omitted recovery_domain"
    for hash_value in \
        "$RETIREMENT_MANIFEST_HASH" "$RETIREMENT_NETWORK_GENESIS_HASH" \
        "$RETIREMENT_RECOVERY_DOMAIN"; do
        printf '%s\n' "$hash_value" | grep -Eq '^[0-9a-f]{64}$' \
            || die "Recovery descriptor verifier returned a malformed hash"
    done
    RETIREMENT_RECOVERY_EPOCH="$(top_level_json_bare "$output" recovery_epoch)" \
        || die "Recovery descriptor verifier omitted recovery_epoch"
    RETIREMENT_VALIDATOR_SET_ID="$(top_level_json_bare "$output" validator_set_id)" \
        || die "Recovery descriptor verifier omitted validator_set_id"
    RETIREMENT_SOURCE_HEIGHT="$(top_level_json_bare "$output" source_height)" \
        || die "Recovery descriptor verifier omitted source_height"
    RETIREMENT_TRANSITION_HEIGHT="$(top_level_json_bare "$output" transition_height)" \
        || die "Recovery descriptor verifier omitted transition_height"
    canonical_uint "$RETIREMENT_RECOVERY_EPOCH" \
        && canonical_uint "$RETIREMENT_VALIDATOR_SET_ID" \
        && canonical_uint "$RETIREMENT_SOURCE_HEIGHT" \
        && canonical_uint "$RETIREMENT_TRANSITION_HEIGHT" \
        || die "Recovery descriptor verifier returned malformed numeric identity"
    [ "$RETIREMENT_SOURCE_HEIGHT" -eq 137145 ] \
        && [ "$RETIREMENT_TRANSITION_HEIGHT" -eq 137146 ] \
        && [ "$RETIREMENT_RECOVERY_EPOCH" -eq 1 ] \
        && [ "$RETIREMENT_VALIDATOR_SET_ID" -eq 1 ] \
        || die "Recovery descriptor differs from the fixed ARC cutover boundary"
}

ensure_legacy_retirement_evidence_dirs() {
    local expected_uid directory
    [ "$LEGACY_ADOPTION_ACTIVE" = true ] || return 0
    expected_uid="$(legacy_expected_owner_uid)"
    for directory in "$LEGACY_RETIREMENT_DIR" "$LEGACY_RETIREMENT_RELEASE_DIR"; do
        if legacy_path_present "$directory"; then
            validate_owned_nonwritable_path "$directory" \
                "legacy retirement evidence directory" directory "$expected_uid"
            continue
        fi
        install_root_as_owner mkdir -- "$directory" \
            || die "Could not create legacy retirement evidence directory: $directory"
        install_root_as_owner chmod 700 "$directory" \
            || die "Could not protect legacy retirement evidence directory: $directory"
        sync
        validate_owned_nonwritable_path "$directory" \
            "legacy retirement evidence directory" directory "$expected_uid"
    done
}

publish_legacy_retirement_file() {
    local source="$1" destination="$2" label="$3"
    local expected_uid source_sha destination_sha staged parent
    expected_uid="$(legacy_expected_owner_uid)"
    source_sha="$(sha256_file "$source")"
    printf '%s\n' "$source_sha" | grep -Eq '^[0-9a-f]{64}$' \
        || die "Could not hash $label"
    if legacy_path_present "$destination"; then
        validate_owned_nonwritable_path "$destination" "$label" file "$expected_uid"
        destination_sha="$(sha256_file "$destination")"
        [ "$destination_sha" = "$source_sha" ] \
            || die "Existing $label differs from the exact retirement evidence"
        printf '%s\n' "$destination_sha"
        return 0
    fi
    parent="$(dirname -- "$destination")"
    staged="$(install_root_as_owner mktemp "$parent/.arc-retirement.new.XXXXXX")" \
        || die "Could not reserve $label"
    if ! install_root_as_owner cp -- "$source" "$staged" \
        || ! install_root_as_owner chmod 600 "$staged"; then
        install_root_as_owner rm -f -- "$staged" || true
        die "Could not stage $label"
    fi
    if ! install_root_as_owner mv -n -- "$staged" "$destination" \
        || legacy_path_present "$staged"; then
        install_root_as_owner rm -f -- "$staged" || true
        die "$label appeared concurrently"
    fi
    sync
    validate_owned_nonwritable_path "$destination" "$label" file "$expected_uid"
    destination_sha="$(sha256_file "$destination")"
    [ "$destination_sha" = "$source_sha" ] \
        || die "Published $label differs from its staged bytes"
    printf '%s\n' "$destination_sha"
}

json_quote() {
    local value="$1"
    if printf '%s' "$value" | LC_ALL=C grep -q '[[:cntrl:]]'; then
        die "Retirement evidence contains an unsupported control character"
    fi
    value="${value//\\/\\\\}"
    value="${value//\"/\\\"}"
    printf '"%s"' "$value"
}

write_legacy_retirement_supervisor_binding() {
    local supervisor_kind source_path source_sha executable_sha legacy_seed
    local temporary="$TMP_DIR/legacy-supervisor-binding.json" token first=true
    local -a argv=()
    case "$LEGACY_SUPERVISOR_KIND" in
        linux-system)
            supervisor_kind=systemd
            source_path="$LEGACY_PRESERVED_DIR/legacy-linux-arc-node.service"
            while IFS= read -r token; do argv+=("$token"); done \
                < <(legacy_systemd_exec_tokens "$source_path") ;;
        macos-launchd)
            supervisor_kind=launchd
            source_path="$LEGACY_PRESERVED_DIR/legacy-macos-com.arc.inference.plist"
            while IFS= read -r token; do argv+=("$token"); done \
                < <(plist_program_arguments "$source_path") ;;
        detached|detached-untracked)
            supervisor_kind=manual
            source_path="$LEGACY_PRESERVED_DIR/legacy-detached-topology"
            argv=("${LEGACY_DETACHED_RESTART_ARGS[@]}") ;;
        none)
            # A stopped no-service v0.7 default has no live supervisor source.
            # Bind the exact archived release version plus the only recognized
            # stake-zero command that the legacy default layout can resume.
            supervisor_kind=manual
            source_path="$LEGACY_PRESERVED_DIR/version.txt"
            legacy_seed="$(sed -n '1p' "$LEGACY_PRESERVED_DIR/identity.seed")"
            argv=(
                "$ARC_DIR/bin/arc-node"
                --rpc "0.0.0.0:$RPC_PORT"
                --p2p-port "$P2P_PORT"
                --seeds-file "$ARC_DIR/seeds.txt"
                --genesis "$ARC_DIR/genesis.toml"
                --validator-seed "$legacy_seed"
                --stake 0 --min-stake 0 --eth-rpc-port 0
                --data-dir "$ARC_DIR/data"
            )
            [ -z "$MODEL_PATH" ] || argv+=(--model "$MODEL_PATH")
            argv+=(--community-mode) ;;
        *) die "Internal unsupported retirement supervisor: $LEGACY_SUPERVISOR_KIND" ;;
    esac
    [ "${#argv[@]}" -gt 0 ] && [ "${argv[0]}" = "$ARC_DIR/bin/arc-node" ] \
        || die "Legacy retirement supervisor does not bind the exact v0.7 executable"
    validate_owned_nonwritable_path "$source_path" \
        "legacy retirement supervisor source" file "$(legacy_expected_owner_uid)"
    source_sha="$(sha256_file "$source_path")"
    executable_sha="$(sha256_file "$ARC_DIR/bin/arc-node")"
    {
        printf '{"argv":['
        for token in "${argv[@]}"; do
            if [ "$first" = true ]; then first=false; else printf ','; fi
            json_quote "$token"
        done
        printf '],"executable_path":'
        json_quote "$ARC_DIR/bin/arc-node"
        printf ',"executable_sha256":"%s","kind":' "$executable_sha"
        json_quote "$supervisor_kind"
        printf ',"schema":"arc.migration.legacy-v07-supervisor-binding.v1","source_path":'
        json_quote "$source_path"
        printf ',"source_sha256":"%s"}\n' "$source_sha"
    } > "$temporary" || die "Could not write the canonical legacy supervisor binding"
    LEGACY_RETIREMENT_SUPERVISOR_BINDING_SHA256="$(publish_legacy_retirement_file \
        "$temporary" "$LEGACY_RETIREMENT_SUPERVISOR_BINDING" \
        "legacy retirement supervisor binding")"
}

stage_legacy_retirement_release_evidence() {
    local asset
    ensure_legacy_retirement_evidence_dirs
    for asset in \
        "$LEGACY_BOUNDARY_ASSET" "$RECOVERY_DESCRIPTOR_ASSET" "$CUTOVER_POLICY_ASSET"; do
        publish_legacy_retirement_file "$TMP_DIR/$asset" \
            "$LEGACY_RETIREMENT_RELEASE_DIR/$asset" \
            "legacy retirement release asset $asset" >/dev/null
    done
    publish_legacy_retirement_file "$RELEASE_INSTALLER_BINDING" \
        "$LEGACY_RETIREMENT_RELEASE_DIR/arc-release-installer-binding.json" \
        "legacy retirement release binding" >/dev/null
    RELEASE_INSTALLER_BINDING="$LEGACY_RETIREMENT_RELEASE_DIR/arc-release-installer-binding.json"
    write_legacy_retirement_supervisor_binding
}

if [ "$LEGACY_ADOPTION_ACTIVE" = true ]; then
    download_checked "$LEGACY_BOUNDARY_ASSET"
    download_checked "$RECOVERY_DESCRIPTOR_ASSET"
    download_checked "$CUTOVER_POLICY_ASSET"
    write_release_installer_binding
    verify_recovery_descriptor_for_retirement
    stage_legacy_retirement_release_evidence
fi

network_https_curl() {
    local url="$1"
    curl --fail --silent --show-error --location --retry 2 \
        --proto '=https' --proto-redir '=https' --tlsv1.2 \
        --connect-timeout 10 --max-time 20 \
        --header 'Accept: application/json' \
        --header 'User-Agent: arc-chain-legacy-retirement-gate' \
        "$url"
}

legacy_http_listener_responds() {
    local url="$1"
    # No --fail: any HTTP status proves a legacy listener or proxy still
    # answers on the exact port the released worker uses. Connection refusal,
    # routing failure, or timeout proves that this host cannot open a new
    # legacy claim/submit channel at the cutover boundary.
    curl --silent --show-error --output /dev/null \
        --proto '=http' --connect-timeout 3 --max-time 5 "$url"
}

verify_legacy_v07_network_retirement() {
    local index origin host info_file error_file failed=0 hash_value
    local node_version protocol_version recovery_active recovery_epoch validator_set_id
    local validator_address checkpoint_hash transaction_domain network_genesis_hash height
    local expected_address shared_checkpoint="" shared_domain="" shared_genesis=""
    local info_pids=() probe_pids=() probe_labels=()
    [ "$LEGACY_ADOPTION_ACTIVE" = true ] || return 0
    [ "${#COMMUNITY_RPC_ORIGINS[@]}" -eq 6 ] \
        && [ "${#COMMUNITY_VALIDATOR_ADDRESSES[@]}" -eq 6 ] \
        || die "Legacy retirement gate has an incomplete fixed validator topology"

    info "Proving all six recovered v3 validators are live before retiring v0.7"
    for ((index=0; index<${#COMMUNITY_RPC_ORIGINS[@]}; index++)); do
        origin="${COMMUNITY_RPC_ORIGINS[$index]}"
        info_file="$TMP_DIR/legacy-retirement-info-$index.json"
        error_file="$TMP_DIR/legacy-retirement-info-$index.stderr"
        ( network_https_curl "$origin/network/info" > "$info_file" 2> "$error_file" ) &
        info_pids+=("$!")
    done
    for ((index=0; index<${#info_pids[@]}; index++)); do
        if ! wait "${info_pids[$index]}"; then
            warn "Recovered validator did not answer authenticated HTTPS: ${COMMUNITY_RPC_ORIGINS[$index]}"
            failed=1
        fi
    done
    [ "$failed" -eq 0 ] \
        || die "The recovered v3 fleet is not fully reachable; the running v0.7 node was not stopped"

    for ((index=0; index<${#COMMUNITY_RPC_ORIGINS[@]}; index++)); do
        origin="${COMMUNITY_RPC_ORIGINS[$index]}"
        info_file="$TMP_DIR/legacy-retirement-info-$index.json"
        [ -f "$info_file" ] && [ ! -L "$info_file" ] \
            && [ "$(wc -c < "$info_file" | tr -d ' ')" -le 1048576 ] \
            || die "Recovered validator returned unsafe /info evidence: $origin"
        node_version="$(top_level_json_string "$info_file" node_version)" \
            || die "Recovered validator /network/info has no unique node_version: $origin"
        protocol_version="$(top_level_json_string "$info_file" protocol_version)" \
            || die "Recovered validator /network/info has no unique protocol_version: $origin"
        strict_version "$node_version" \
            && [ "$(version_compare "$node_version" 0.8.0)" -ge 0 ] \
            || die "Validator is not running v0.8-or-newer node code: $origin"
        strict_version "$protocol_version" \
            && [ "$(version_compare "$protocol_version" 0.8.0)" -ge 0 ] \
            || die "Validator has not sealed a v0.8-or-newer protocol block: $origin"

        recovery_active="$(top_level_json_bare "$info_file" recovery_active)" \
            || die "Recovered validator /network/info has no unique recovery_active field: $origin"
        [ "$recovery_active" = true ] \
            || die "Validator has no active checkpoint recovery context: $origin"
        recovery_epoch="$(top_level_json_bare "$info_file" recovery_epoch)" \
            || die "Recovered validator /network/info has no unique recovery_epoch: $origin"
        validator_set_id="$(top_level_json_bare "$info_file" validator_set_id)" \
            || die "Recovered validator /network/info has no unique validator_set_id: $origin"
        height="$(top_level_json_bare "$info_file" height)" \
            || die "Recovered validator /network/info has no unique height: $origin"
        canonical_uint "$recovery_epoch" \
            && canonical_uint "$validator_set_id" \
            && canonical_uint "$height" \
            || die "Recovered validator returned malformed numeric identity: $origin"
        [ "$recovery_epoch" -eq "$RETIREMENT_RECOVERY_EPOCH" ] \
            && [ "$validator_set_id" -eq "$RETIREMENT_VALIDATOR_SET_ID" ] \
            && [ "$height" -ge "$RETIREMENT_TRANSITION_HEIGHT" ] \
            || die "Validator has not crossed the approved v0.7 retirement boundary: $origin"

        validator_address="$(top_level_json_string "$info_file" validator_address)" \
            || die "Recovered validator /network/info has no unique validator_address: $origin"
        expected_address="0x${COMMUNITY_VALIDATOR_ADDRESSES[$index]}"
        [ "$validator_address" = "$expected_address" ] \
            || die "Recovered validator identity differs for $origin"
        checkpoint_hash="$(top_level_json_string "$info_file" checkpoint_manifest_hash)" \
            || die "Recovered validator /network/info has no unique checkpoint_manifest_hash: $origin"
        transaction_domain="$(top_level_json_string "$info_file" transaction_domain)" \
            || die "Recovered validator /network/info has no unique transaction_domain: $origin"
        network_genesis_hash="$(top_level_json_string "$info_file" network_genesis_hash)" \
            || die "Recovered validator /network/info has no unique network_genesis_hash: $origin"
        for hash_value in "$checkpoint_hash" "$transaction_domain" "$network_genesis_hash"; do
            printf '%s\n' "$hash_value" | grep -Eq '^0x[0-9a-f]{64}$' \
                || die "Recovered validator returned a malformed checkpoint/domain/genesis hash: $origin"
        done
        [ "$checkpoint_hash" != "0x$(printf '%064d' 0)" ] \
            && [ "$transaction_domain" != "0x$(printf '%064d' 0)" ] \
            && [ "$network_genesis_hash" != "0x$(printf '%064d' 0)" ] \
            || die "Recovered validator returned a zero checkpoint/domain/genesis hash: $origin"
        [ "$checkpoint_hash" = "0x$RETIREMENT_MANIFEST_HASH" ] \
            && [ "$transaction_domain" = "0x$RETIREMENT_RECOVERY_DOMAIN" ] \
            && [ "$network_genesis_hash" = "0x$RETIREMENT_NETWORK_GENESIS_HASH" ] \
            || die "Recovered validator does not match the signed recovery descriptor: $origin"
        if [ -z "$shared_checkpoint" ]; then
            shared_checkpoint="$checkpoint_hash"
            shared_domain="$transaction_domain"
            shared_genesis="$network_genesis_hash"
        elif [ "$checkpoint_hash" != "$shared_checkpoint" ] \
            || [ "$transaction_domain" != "$shared_domain" ] \
            || [ "$network_genesis_hash" != "$shared_genesis" ]; then
            die "Recovered validators disagree on checkpoint, transaction domain, or genesis"
        fi
    done

    # Released v0.7 workers use raw HTTP on every seed's :9090 and :3001.
    # Prove those exact channels are unreachable from this worker before TERM;
    # a long timeout cannot turn v0.7's immediate-exit handler into a drain.
    for origin in "${COMMUNITY_RPC_ORIGINS[@]}"; do
        host="${origin#https://}"
        for legacy_port in 9090 3001; do
            probe_label="$host:$legacy_port"
            probe_labels+=("$probe_label")
            ( if legacy_http_listener_responds \
                    "http://$probe_label/health"; then
                  exit 0
              fi
              exit 1 ) &
            probe_pids+=("$!")
        done
    done
    for ((index=0; index<${#probe_pids[@]}; index++)); do
        if wait "${probe_pids[$index]}"; then
            die "Legacy v0.7 RPC is still reachable at ${probe_labels[$index]}; the old node was not stopped"
        fi
    done
    ok "All six exact validators match the signed checkpoint at or above block $RETIREMENT_TRANSITION_HEIGHT and legacy claim/submit ports are unreachable"
}

prepare_absent_v08_data_dir_for_retirement() {
    [ "$LEGACY_ADOPTION_ACTIVE" = true ] || return 0
    if ! legacy_path_present "$NODE_DATA_DIR"; then return 0; fi
    [ "$LEGACY_PARTIAL_RESUME" = true ] \
        || die "Legacy retirement requires an absent, unused v0.8 data directory: $NODE_DATA_DIR"
    [ -d "$NODE_DATA_DIR" ] && [ ! -L "$NODE_DATA_DIR" ] \
        || die "Pending legacy adoption has an unsafe v0.8 data path: $NODE_DATA_DIR"
    if find "$NODE_DATA_DIR" -mindepth 1 -maxdepth 1 -print -quit | grep -q .; then
        die "Pending legacy adoption already wrote v0.8 state; preserve it and inspect before retrying"
    fi
    install_root_as_owner rmdir -- "$NODE_DATA_DIR" \
        || die "Could not remove the empty pre-receipt v0.8 directory"
    sync
    [ ! -e "$NODE_DATA_DIR" ] && [ ! -L "$NODE_DATA_DIR" ] \
        || die "The v0.8 data directory still exists before retirement receipt"
}

capture_legacy_retirement_process() {
    local pid=""
    LEGACY_RETIREMENT_ALREADY_OFFLINE=false
    case "$LEGACY_SUPERVISOR_KIND" in
        linux-system)
            if as_root systemctl is-active --quiet arc-node.service; then
                pid="$(as_root systemctl show --property=MainPID --value arc-node.service 2>/dev/null)" \
                    || die "Could not inspect the active legacy systemd PID"
            fi ;;
        macos-launchd)
            [ -n "$LAUNCHD_DOMAIN" ] || LAUNCHD_DOMAIN="user/$TARGET_UID"
            if launchctl print "gui/$TARGET_UID" >/dev/null 2>&1; then
                LAUNCHD_DOMAIN="gui/$TARGET_UID"
            fi
            pid="$(launchd_service_pid com.arc.inference 2>/dev/null || true)" ;;
        detached|detached-untracked)
            if [ -n "$LEGACY_DETACHED_PID" ] \
                && kill -0 "$LEGACY_DETACHED_PID" 2>/dev/null; then
                pid="$LEGACY_DETACHED_PID"
            fi ;;
        none) ;;
        *) die "Internal unsupported retirement supervisor: $LEGACY_SUPERVISOR_KIND" ;;
    esac
    if [ -n "$pid" ]; then
        case "$pid" in ''|*[!0-9]*) die "Legacy supervisor returned a malformed PID" ;; esac
        [ "$pid" -gt 1 ] || die "Legacy supervisor returned an unsafe PID"
        LEGACY_RETIREMENT_PROCESS_PID="$pid"
    else
        LEGACY_RETIREMENT_PROCESS_PID=""
        LEGACY_RETIREMENT_ALREADY_OFFLINE=true
    fi
}

create_legacy_retirement_intent() {
    local output="$TMP_DIR/legacy-retirement-create.json" status mode reported_sha
    local legacy_executable_sha release_binding_sha boundary_sha descriptor_sha policy_sha
    local -a mode_args=()
    [ "$LEGACY_ADOPTION_ACTIVE" = true ] || return 0
    prepare_absent_v08_data_dir_for_retirement
    if legacy_path_present "$LEGACY_RETIREMENT_INTENT"; then
        validate_owned_nonwritable_path "$LEGACY_RETIREMENT_INTENT" \
            "legacy retirement intent" file "$(legacy_expected_owner_uid)"
        # Conservatively cross the no-restart boundary before parsing an
        # existing create-only record. A malformed protected record must fail
        # stopped; it must never cause the installer to revive v0.7.
        LEGACY_RETIREMENT_INTENT_DURABLE=true
    fi
    capture_legacy_retirement_process
    if [ "$LEGACY_RETIREMENT_ALREADY_OFFLINE" = true ]; then
        mode_args=(--already-offline)
    else
        mode_args=(--legacy-pid "$LEGACY_RETIREMENT_PROCESS_PID")
    fi
    legacy_executable_sha="$(sha256_file "$ARC_DIR/bin/arc-node")"
    release_binding_sha="$(sha256_file "$RELEASE_INSTALLER_BINDING")"
    boundary_sha="$(sha256_file "$LEGACY_RETIREMENT_RELEASE_DIR/$LEGACY_BOUNDARY_ASSET")"
    descriptor_sha="$(sha256_file "$LEGACY_RETIREMENT_RELEASE_DIR/$RECOVERY_DESCRIPTOR_ASSET")"
    policy_sha="$(sha256_file "$LEGACY_RETIREMENT_RELEASE_DIR/$CUTOVER_POLICY_ASSET")"
    if ! "$TMP_DIR/$NODE_ASSET" legacy-retirement create-intent \
        --intent-output "$LEGACY_RETIREMENT_INTENT" \
        --target-release "$RELEASE_INSTALLER_BINDING" \
        --target-release-sha256 "$release_binding_sha" \
        --maintenance-boundary "$LEGACY_RETIREMENT_RELEASE_DIR/$LEGACY_BOUNDARY_ASSET" \
        --maintenance-boundary-sha256 "$boundary_sha" \
        --cutover-policy "$LEGACY_RETIREMENT_RELEASE_DIR/$CUTOVER_POLICY_ASSET" \
        --cutover-policy-sha256 "$policy_sha" \
        --checkpoint-descriptor "$LEGACY_RETIREMENT_RELEASE_DIR/$RECOVERY_DESCRIPTOR_ASSET" \
        --checkpoint-descriptor-sha256 "$descriptor_sha" \
        "${mode_args[@]}" \
        --legacy-version "$LEGACY_SOURCE_VERSION" \
        --legacy-executable "$ARC_DIR/bin/arc-node" \
        --legacy-executable-sha256 "$legacy_executable_sha" \
        --supervisor-definition "$LEGACY_RETIREMENT_SUPERVISOR_BINDING" \
        --supervisor-definition-sha256 "$LEGACY_RETIREMENT_SUPERVISOR_BINDING_SHA256" \
        --data-dir "$ARC_DIR/data" \
        --v08-data-dir "$NODE_DATA_DIR" \
        --forensic-only > "$output"; then
        die "The compiled verifier refused to create or resume the v0.7 retirement intent"
    fi
    status="$(top_level_json_string "$output" status)" \
        || die "Retirement intent verifier returned no unique status"
    [ "$status" = RETIREMENT_INTENT_CREATED ] \
        || die "Retirement intent verifier returned an unexpected status"
    reported_sha="$(top_level_json_string "$output" sha256)" \
        || die "Retirement intent verifier omitted its SHA-256"
    printf '%s\n' "$reported_sha" | grep -Eq '^[0-9a-f]{64}$' \
        || die "Retirement intent verifier returned a malformed SHA-256"
    [ "$(sha256_file "$LEGACY_RETIREMENT_INTENT")" = "$reported_sha" ] \
        || die "Published retirement intent differs from the verifier result"
    mode="$(top_level_json_string "$output" retirement_mode)" \
        || die "Retirement intent verifier omitted its mode"
    case "$mode" in
        term_only)
            LEGACY_RETIREMENT_PROCESS_PID="$(top_level_json_bare "$output" legacy_pid)" \
                || die "Retirement intent verifier omitted its bound PID"
            LEGACY_RETIREMENT_PROCESS_BOOT_ID="$(top_level_json_string "$output" legacy_boot_id)" \
                || die "Retirement intent verifier omitted its boot identity"
            LEGACY_RETIREMENT_PROCESS_START_TICKS="$(top_level_json_bare "$output" legacy_start_ticks)" \
                || die "Retirement intent verifier omitted its start ticks"
            case "$LEGACY_RETIREMENT_PROCESS_PID:$LEGACY_RETIREMENT_PROCESS_START_TICKS" in
                *[!0-9:]*|:*) die "Retirement intent verifier returned malformed process identity" ;;
            esac
            LEGACY_RETIREMENT_ALREADY_OFFLINE=false ;;
        preexisting_offline)
            LEGACY_RETIREMENT_PROCESS_PID=""
            LEGACY_RETIREMENT_PROCESS_BOOT_ID=""
            LEGACY_RETIREMENT_PROCESS_START_TICKS=""
            LEGACY_RETIREMENT_ALREADY_OFFLINE=true ;;
        *) die "Retirement intent verifier returned an unsupported mode" ;;
    esac
    LEGACY_RETIREMENT_INTENT_SHA256="$reported_sha"
    LEGACY_RETIREMENT_INTENT_DURABLE=true
    ok "Published the create-only v0.7 retirement intent $reported_sha"
}

write_legacy_retirement_stop_evidence() {
    local temporary="$TMP_DIR/legacy-retirement-stop-evidence.json"
    local schema signals process_identity offline_observed_at
    [ "$LEGACY_ADOPTION_ACTIVE" = true ] || return 0
    [ "$LEGACY_RETIREMENT_INTENT_DURABLE" = true ] \
        || die "Internal refusal to write stop evidence without a durable intent"
    if legacy_path_present "$LEGACY_RETIREMENT_STOP_EVIDENCE"; then
        validate_owned_nonwritable_path "$LEGACY_RETIREMENT_STOP_EVIDENCE" \
            "legacy retirement stop evidence" file "$(legacy_expected_owner_uid)"
        LEGACY_RETIREMENT_STOP_EVIDENCE_SHA256="$(sha256_file \
            "$LEGACY_RETIREMENT_STOP_EVIDENCE")"
        return 0
    fi
    offline_observed_at="$(date -u '+%Y-%m-%dT%H:%M:%SZ')"
    if [ "$LEGACY_RETIREMENT_ALREADY_OFFLINE" = true ]; then
        schema=arc.migration.legacy-v07-preexisting-offline-evidence.v1
        signals='[]'
        process_identity=null
        LEGACY_RETIREMENT_SUPERVISOR_MECHANISM=preexisting-offline-verified-supervisor
    else
        schema=arc.migration.legacy-v07-term-only-stop-evidence.v1
        signals='["SIGTERM"]'
        process_identity="{\"boot_id\":$(json_quote "$LEGACY_RETIREMENT_PROCESS_BOOT_ID"),\"pid\":$LEGACY_RETIREMENT_PROCESS_PID,\"start_ticks\":$LEGACY_RETIREMENT_PROCESS_START_TICKS}"
        case "$LEGACY_SUPERVISOR_KIND" in
            linux-system) LEGACY_RETIREMENT_SUPERVISOR_MECHANISM=systemd-send-sigkill-no ;;
            macos-launchd) LEGACY_RETIREMENT_SUPERVISOR_MECHANISM=launchd-term-only ;;
            detached|detached-untracked) LEGACY_RETIREMENT_SUPERVISOR_MECHANISM=direct-term-only ;;
            *) die "TERM-only retirement has no supported supervisor mechanism" ;;
        esac
    fi
    {
        printf '{"legacy_exit_clean_claimed":false,"observation_started_at":'
        json_quote "$LEGACY_RETIREMENT_OBSERVATION_STARTED_AT"
        printf ',"offline_observed_at":'
        json_quote "$offline_observed_at"
        printf ',"process_identity":%s,"schema":' "$process_identity"
        json_quote "$schema"
        printf ',"supervisor":{"escalation_used":false,"exit_status_observed":false,"mechanism":'
        json_quote "$LEGACY_RETIREMENT_SUPERVISOR_MECHANISM"
        printf ',"send_sigkill_configured":false,"sigkill_sent":false,"signals_sent":%s}}\n' \
            "$signals"
    } > "$temporary" || die "Could not write canonical legacy stop evidence"
    LEGACY_RETIREMENT_STOP_EVIDENCE_SHA256="$(publish_legacy_retirement_file \
        "$temporary" "$LEGACY_RETIREMENT_STOP_EVIDENCE" \
        "legacy retirement stop evidence")"
}

finalize_legacy_retirement_receipt() {
    local output="$TMP_DIR/legacy-retirement-finalize.json" status reported_sha
    [ "$LEGACY_ADOPTION_ACTIVE" = true ] || return 0
    [ -n "$LEGACY_RETIREMENT_INTENT_SHA256" ] \
        && [ -n "$LEGACY_RETIREMENT_STOP_EVIDENCE_SHA256" ] \
        || die "Retirement finalization is missing sealed intent or stop evidence"
    if ! "$TMP_DIR/$NODE_ASSET" legacy-retirement finalize \
        --intent "$LEGACY_RETIREMENT_INTENT" \
        --intent-sha256 "$LEGACY_RETIREMENT_INTENT_SHA256" \
        --stop-evidence "$LEGACY_RETIREMENT_STOP_EVIDENCE" \
        --stop-evidence-sha256 "$LEGACY_RETIREMENT_STOP_EVIDENCE_SHA256" \
        --receipt-output "$LEGACY_RETIREMENT_RECEIPT" \
        --stability-seconds 10 --samples 3 > "$output"; then
        die "The compiled offline verifier refused the v0.7 retirement receipt"
    fi
    status="$(top_level_json_string "$output" status)" \
        || die "Retirement finalizer returned no unique status"
    [ "$status" = RETIREMENT_RECEIPT_CREATED ] \
        || die "Retirement finalizer returned an unexpected status"
    reported_sha="$(top_level_json_string "$output" sha256)" \
        || die "Retirement finalizer omitted its SHA-256"
    printf '%s\n' "$reported_sha" | grep -Eq '^[0-9a-f]{64}$' \
        || die "Retirement finalizer returned a malformed SHA-256"
    [ "$(sha256_file "$LEGACY_RETIREMENT_RECEIPT")" = "$reported_sha" ] \
        || die "Published retirement receipt differs from the verifier result"
    LEGACY_RETIREMENT_RECEIPT_SHA256="$reported_sha"

    # Only the receipt may authorize creation of fresh v0.8 state.
    if [ "$SERVICE_SCOPE" = system ]; then
        as_root mkdir -- "$NODE_DATA_DIR" \
            || die "Could not create the receipt-authorized v0.8 data directory"
        as_root chown "$TARGET_USER:$TARGET_GROUP" "$NODE_DATA_DIR"
        as_root chmod 700 "$NODE_DATA_DIR"
    else
        ensure_private_dir "$NODE_DATA_DIR"
    fi
    as_target test -w "$NODE_DATA_DIR" \
        || die "Install user $TARGET_USER cannot write receipt-authorized data directory $NODE_DATA_DIR"
    ok "Published the offline v0.7 retirement receipt $LEGACY_RETIREMENT_RECEIPT_SHA256"
}

atomic_copy() {
    local source="$1"
    local destination="$2"
    local mode="$3"
    local ownership="${4:-target}"
    local executor="${5:-install}"
    local staged="${destination}.new.$$"
    if [ "$executor" = root ] || { [ "$executor" = install ] && [ "$SERVICE_SCOPE" = system ]; }; then
        as_root cp -- "$source" "$staged" || return 1
        as_root chmod "$mode" "$staged" || return 1
        if [ "$ownership" = root ]; then
            as_root chown root:root "$staged" || return 1
        else
            as_root chown "$TARGET_USER:$TARGET_GROUP" "$staged" || return 1
        fi
        as_root mv -f -- "$staged" "$destination" || return 1
    else
        install_root_as_owner cp -- "$source" "$staged" || return 1
        install_root_as_owner chmod "$mode" "$staged" || return 1
        install_root_as_owner mv -f -- "$staged" "$destination" || return 1
    fi
}

TRANSACTION_PATHS=()
TRANSACTION_BACKUPS=()
TRANSACTION_EXISTED=()
TRANSACTION_EXECUTORS=()
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
transaction_as_owner() {
    install_root_as_owner "$@"
}

run_transaction_executor() {
    local executor="$1"
    shift
    if [ "$executor" = root ]; then as_root "$@"
    else transaction_as_owner "$@"
    fi
}

snapshot_transaction_path() {
    local path="$1"
    local executor="${2:-install}"
    local existing_index
    for ((existing_index=0; existing_index<${#TRANSACTION_PATHS[@]}; existing_index++)); do
        if [ "${TRANSACTION_PATHS[$existing_index]}" = "$path" ]; then
            [ "${TRANSACTION_EXECUTORS[$existing_index]}" = "$executor" ] \
                || die "Managed transaction path has conflicting privilege owners: $path"
            return
        fi
    done
    local index="${#TRANSACTION_PATHS[@]}"
    local backup="$TMP_DIR/transaction-backup-$index"

    if run_transaction_executor "$executor" test -L "$path"; then
        die "Refusing symlinked managed install path: $path"
    fi
    TRANSACTION_PATHS[index]="$path"
    TRANSACTION_BACKUPS[index]="$backup"
    TRANSACTION_EXECUTORS[index]="$executor"
    if run_transaction_executor "$executor" test -e "$path"; then
        run_transaction_executor "$executor" test -f "$path" \
            || die "Managed install path is not a regular file: $path"
        run_transaction_executor "$executor" cp -p -- "$path" "$backup" \
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
        system-user)
            if systemctl is-active --quiet arc-node.service; then
                PRIOR_NODE_ACTIVE=true
            fi
            if systemctl is-enabled --quiet arc-node.service; then
                PRIOR_NODE_ENABLED=true
            fi
            if systemctl is-active --quiet arc-node-update.timer; then
                PRIOR_UPDATER_ACTIVE=true
            fi
            if systemctl is-enabled --quiet arc-node-update.timer; then
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

capture_legacy_runtime_state() {
    [ "$LEGACY_ADOPTION_ACTIVE" = true ] || return 0
    case "$LEGACY_SUPERVISOR_KIND" in
        linux-system)
            if as_root systemctl is-active --quiet arc-node.service; then
                LEGACY_LINUX_NODE_ACTIVE=true
            fi
            if as_root systemctl is-enabled --quiet arc-node.service; then
                LEGACY_LINUX_NODE_ENABLED=true
            fi
            ;;
        macos-launchd)
            [ -n "$LAUNCHD_DOMAIN" ] || LAUNCHD_DOMAIN="user/$TARGET_UID"
            if launchctl print "gui/$TARGET_UID" >/dev/null 2>&1; then
                LAUNCHD_DOMAIN="gui/$TARGET_UID"
            fi
            if launchctl print "$LAUNCHD_DOMAIN/com.arc.inference" >/dev/null 2>&1; then
                LEGACY_MAC_NODE_LOADED=true
            fi
            if launchctl print "$LAUNCHD_DOMAIN/com.arc.updater" >/dev/null 2>&1; then
                LEGACY_MAC_UPDATER_LOADED=true
            fi ;;
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

    revalidate_legacy_sources_after_updater_fence

    snapshot_transaction_path "$ARC_DIR/bin/arc-node"
    snapshot_transaction_path "$ARC_DIR/bin/arc-cli"
    snapshot_transaction_path "$ARC_DIR/testnet-seeds.txt"
    snapshot_transaction_path "$ARC_DIR/genesis.toml"
    snapshot_transaction_path "$ARC_DIR/bin/arc-installer"
    snapshot_transaction_path "$SEED_FILE"
    snapshot_transaction_path "$KEY_FILE"
    snapshot_transaction_path "$LEGACY_SEED_EVIDENCE"
    snapshot_transaction_path "$ARC_DIR/node.env"
    snapshot_transaction_path "$ARC_DIR/bin/run-arc-node"
    snapshot_transaction_path "$CONFIG_FILE"

    case "$SERVICE_SCOPE" in
        system)
            snapshot_transaction_path "$SYSTEMD_UNIT_DIR/arc-node.service" root
            snapshot_transaction_path "$SYSTEMD_UNIT_DIR/arc-node-update.service" root
            snapshot_transaction_path "$SYSTEMD_UNIT_DIR/arc-node-update.timer" root
            ;;
        system-user)
            if [ "$SYSTEM_USER_BOOTSTRAP" = true ]; then
                snapshot_transaction_path "$SYSTEMD_UNIT_DIR/arc-node.service" root
                snapshot_transaction_path "$SYSTEMD_UNIT_DIR/arc-node-update.service" root
                snapshot_transaction_path "$SYSTEMD_UNIT_DIR/arc-node-update.timer" root
            fi ;;
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
    if [ "$LEGACY_ADOPTION_ACTIVE" = true ]; then
        if legacy_path_present "$ARC_DIR/bin/arc-auto-update.sh"; then
            snapshot_transaction_path "$ARC_DIR/bin/arc-auto-update.sh"
        fi
        case "$LEGACY_SUPERVISOR_KIND" in
            linux-system)
                snapshot_transaction_path "$LEGACY_LINUX_NODE_UNIT" root
                snapshot_transaction_path "$LEGACY_LINUX_UPDATER_SERVICE" root
                snapshot_transaction_path "$LEGACY_LINUX_UPDATER_TIMER" root ;;
            macos-launchd)
                snapshot_transaction_path "$LEGACY_MAC_NODE_PLIST"
                snapshot_transaction_path "$LEGACY_MAC_UPDATER_PLIST" ;;
            detached)
                snapshot_transaction_path "$LEGACY_NODE_PID_FILE" ;;
            detached-untracked) : ;;
        esac
    fi
    capture_service_state
    capture_legacy_runtime_state
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

remove_transaction_path() {
    local path="$1" executor="${2:-install}"
    run_transaction_executor "$executor" rm -f -- "$path"
}

launchd_service_pid() {
    local label="$1" output
    output="$(launchctl print "$LAUNCHD_DOMAIN/$label" 2>/dev/null)" || return 1
    printf '%s\n' "$output" | awk '
        $1 == "pid" {
            if ($2 != "=" || $3 !~ /^[1-9][0-9]*$/ || ++count > 1) exit 2
            pid = $3
        }
        END { if (count == 1) print pid }
    '
}

stop_launchd_node_gracefully() {
    local label="$1" expected_binary="$2" expected_wrapper="${3:-}"
    local pid command_line elapsed
    pid="$(launchd_service_pid "$label")" || return 1
    [ -n "$pid" ] || return 0

    command_line="$(ps -ww -p "$pid" -o command= 2>/dev/null)" || return 1
    case "$command_line" in
        "$expected_binary"|"$expected_binary "*) ;;
        "$expected_wrapper"|"$expected_wrapper "*)
            [ -n "$expected_wrapper" ] || return 1 ;;
        *) return 1 ;;
    esac
    kill -TERM "$pid" 2>/dev/null || return 1

    # Wait for the exact process to finish. The caller disables the label
    # first, so KeepAlive cannot turn this into an unbounded restart loop.
    # For v0.8 this bounds its real quiescence barrier; for v0.7 it proves
    # only TERM-without-KILL process death and must never be called a drain.
    for ((elapsed=0; elapsed<GRACEFUL_STOP_TIMEOUT_SECS; elapsed++)); do
        kill -0 "$pid" 2>/dev/null || return 0
        sleep 1
    done
    return 1
}

stop_legacy_detached_process() {
    local elapsed
    [ -n "$LEGACY_DETACHED_PID" ] || return 0
    if ! kill -0 "$LEGACY_DETACHED_PID" 2>/dev/null; then
        ensure_no_legacy_arc_processes
        return 0
    fi
    validate_legacy_detached_pid "$LEGACY_DETACHED_PID" "pre-stop process"
    kill -TERM "$LEGACY_DETACHED_PID" 2>/dev/null || return 1
    for ((elapsed=0; elapsed<GRACEFUL_STOP_TIMEOUT_SECS; elapsed++)); do
        if ! kill -0 "$LEGACY_DETACHED_PID" 2>/dev/null; then
            ensure_no_legacy_arc_processes
            return 0
        fi
        sleep 1
    done
    warn "Legacy detached node did not exit after TERM within ${GRACEFUL_STOP_TIMEOUT_SECS}s; it was not force-killed"
    return 1
}

retire_legacy_runtime() {
    [ "$LEGACY_ADOPTION_ACTIVE" = true ] || return 0
    info "Retiring the verified v0.7 $LEGACY_SUPERVISOR_KIND runtime before binary replacement"
    case "$LEGACY_SUPERVISOR_KIND" in
        linux-system)
            if legacy_path_present "$LEGACY_LINUX_UPDATER_SERVICE"; then
                as_root systemctl disable --now arc-updater.timer >/dev/null 2>&1 \
                    || return 1
                as_root systemctl disable --now arc-updater.service >/dev/null 2>&1 \
                    || return 1
            fi
            if legacy_path_present "$LEGACY_LINUX_NODE_UNIT"; then
                # The released v0.7 unit inherited systemd's short default
                # stop timeout. Override only the runtime instance so a
                # migration can never turn in-flight work into SIGKILL.
                as_root systemctl set-property --runtime arc-node.service \
                    "TimeoutStopSec=${GRACEFUL_STOP_TIMEOUT_SECS}s" \
                    SendSIGKILL=no >/dev/null 2>&1 || return 1
                as_root systemctl disable --now arc-node.service >/dev/null 2>&1 \
                    || return 1
                if as_root systemctl is-active --quiet arc-node.service; then
                    return 1
                fi
            fi
            if legacy_path_present "$LEGACY_LINUX_UPDATER_SERVICE" \
                && { as_root systemctl is-active --quiet arc-updater.service \
                    || as_root systemctl is-active --quiet arc-updater.timer \
                    || as_root systemctl is-enabled --quiet arc-updater.service \
                    || as_root systemctl is-enabled --quiet arc-updater.timer; }; then
                return 1
            fi
            remove_transaction_path "$LEGACY_LINUX_NODE_UNIT" root || return 1
            remove_transaction_path "$LEGACY_LINUX_UPDATER_SERVICE" root || return 1
            remove_transaction_path "$LEGACY_LINUX_UPDATER_TIMER" root || return 1
            as_root systemctl daemon-reload || return 1 ;;
        macos-launchd)
            [ -n "$LAUNCHD_DOMAIN" ] || LAUNCHD_DOMAIN="user/$TARGET_UID"
            if [ "$LEGACY_MAC_UPDATER_LOADED" = true ] \
                && launchctl print "$LAUNCHD_DOMAIN/com.arc.updater" \
                    >/dev/null 2>&1; then
                launchctl bootout "$LAUNCHD_DOMAIN/com.arc.updater" 2>/dev/null \
                    || return 1
            fi
            if [ "$LEGACY_MAC_NODE_LOADED" = true ]; then
                launchctl disable "$LAUNCHD_DOMAIN/com.arc.inference" || return 1
                stop_launchd_node_gracefully com.arc.inference \
                    "$ARC_DIR/bin/arc-node" || return 1
                launchctl bootout "$LAUNCHD_DOMAIN/com.arc.inference" 2>/dev/null \
                    || return 1
            fi
            launchctl print "$LAUNCHD_DOMAIN/com.arc.inference" >/dev/null 2>&1 \
                && return 1
            launchctl print "$LAUNCHD_DOMAIN/com.arc.updater" >/dev/null 2>&1 \
                && return 1
            remove_transaction_path "$LEGACY_MAC_NODE_PLIST" || return 1
            remove_transaction_path "$LEGACY_MAC_UPDATER_PLIST" || return 1 ;;
        detached|detached-untracked)
            if [ "$LEGACY_DETACHED_WAS_RUNNING" = true ]; then
                stop_legacy_detached_process || return 1
            fi
            if [ "$LEGACY_SUPERVISOR_KIND" = detached ]; then
                remove_transaction_path "$LEGACY_NODE_PID_FILE" || return 1
            fi ;;
    esac
    ensure_no_legacy_arc_processes
    if legacy_path_present "$ARC_DIR/bin/arc-auto-update.sh"; then
        remove_transaction_path "$ARC_DIR/bin/arc-auto-update.sh" || return 1
    fi
}

restart_legacy_detached_process() {
    local restarted_pid staged_pid_file
    [ "$LEGACY_DETACHED_WAS_RUNNING" = true ] || return 0
    [ "${#LEGACY_DETACHED_RESTART_ARGS[@]}" -gt 0 ] || return 1
    if [ "$LEGACY_DETACHED_HAD_PID_FILE" = false ] \
        && legacy_path_present "$LEGACY_NODE_PID_FILE"; then
        return 1
    fi
    nohup "${LEGACY_DETACHED_RESTART_ARGS[@]}" \
        >> "$ARC_DIR/node.log" 2>&1 &
    restarted_pid=$!
    kill -0 "$restarted_pid" 2>/dev/null || return 1
    [ "$LEGACY_DETACHED_HAD_PID_FILE" = true ] || return 0
    staged_pid_file="$LEGACY_NODE_PID_FILE.rollback-pid.$$"
    printf '%s\n' "$restarted_pid" > "$staged_pid_file" || return 1
    chmod 600 "$staged_pid_file" || return 1
    mv -f -- "$staged_pid_file" "$LEGACY_NODE_PID_FILE" || return 1
}

restore_legacy_runtime_state() {
    local status=0
    [ "$LEGACY_ADOPTION_ACTIVE" = true ] || return 0
    case "$LEGACY_SUPERVISOR_KIND" in
        linux-system)
            as_root systemctl daemon-reload || status=1
            if [ "$LEGACY_LINUX_NODE_ENABLED" = true ]; then
                as_root systemctl enable arc-node.service >/dev/null || status=1
            else
                as_root systemctl disable arc-node.service >/dev/null 2>&1 || true
            fi
            # Restore the exact legacy files and node state, but never re-arm
            # the unsigned updater while the durable migration marker remains.
            as_root systemctl disable --now arc-updater.timer \
                >/dev/null 2>&1 || status=1
            as_root systemctl disable --now arc-updater.service \
                >/dev/null 2>&1 || status=1
            if [ "$LEGACY_LINUX_NODE_ACTIVE" = true ]; then
                as_root systemctl restart arc-node.service || status=1
            else
                as_root systemctl stop arc-node.service >/dev/null 2>&1 || true
            fi ;;
        macos-launchd)
            if launchctl print "$LAUNCHD_DOMAIN/com.arc.inference" >/dev/null 2>&1; then
                launchctl disable "$LAUNCHD_DOMAIN/com.arc.inference" || status=1
                if ! stop_launchd_node_gracefully com.arc.inference \
                    "$ARC_DIR/bin/arc-node"; then
                    status=1
                elif ! launchctl bootout \
                    "$LAUNCHD_DOMAIN/com.arc.inference" 2>/dev/null; then
                    status=1
                fi
            fi
            launchctl disable "$LAUNCHD_DOMAIN/com.arc.updater" || status=1
            launchctl bootout "$LAUNCHD_DOMAIN/com.arc.updater" 2>/dev/null || true
            if [ "$LEGACY_MAC_NODE_LOADED" = true ]; then
                launchctl enable "$LAUNCHD_DOMAIN/com.arc.inference" || status=1
                launchctl bootstrap "$LAUNCHD_DOMAIN" "$LEGACY_MAC_NODE_PLIST" || status=1
            else
                launchctl disable "$LAUNCHD_DOMAIN/com.arc.inference" || status=1
            fi
            ;;
        detached|detached-untracked)
            restart_legacy_detached_process || status=1 ;;
    esac
    return "$status"
}

restore_transaction_path() {
    local index="$1"
    local path="${TRANSACTION_PATHS[$index]}"
    local backup="${TRANSACTION_BACKUPS[$index]}"
    local executor="${TRANSACTION_EXECUTORS[$index]}"
    local staged="${path}.rollback.$$"
    run_transaction_executor "$executor" rm -f -- "${path}.new.$$" || return 1
    if [ "${TRANSACTION_EXISTED[$index]}" = true ]; then
        run_transaction_executor "$executor" cp -p -- "$backup" "$staged" || return 1
        run_transaction_executor "$executor" mv -f -- "$staged" "$path" || return 1
    else
        run_transaction_executor "$executor" rm -f -- "$staged" "$path" || return 1
    fi
}

restart_managed_system_user_node() {
    local old_pid new_pid command_line active_state sub_state elapsed
    old_pid="$(systemctl show --property=MainPID --value arc-node.service 2>/dev/null)" \
        || return 1
    case "$old_pid" in ''|0|*[!0-9]*) return 1 ;; esac
    active_state="$(systemctl show --property=ActiveState --value arc-node.service 2>/dev/null)" \
        || return 1
    sub_state="$(systemctl show --property=SubState --value arc-node.service 2>/dev/null)" \
        || return 1
    [ "$active_state" = active ] && [ "$sub_state" = running ] || return 1
    command_line="$(ps -ww -p "$old_pid" -o command= 2>/dev/null)" || return 1
    case "$command_line" in
        "$ARC_DIR/bin/arc-node"|"$ARC_DIR/bin/arc-node "*) ;;
        *) return 1 ;;
    esac
    kill -TERM "$old_pid" 2>/dev/null || return 1
    for ((elapsed=0; elapsed<SYSTEM_USER_RESTART_TIMEOUT_SECS; elapsed++)); do
        new_pid="$(systemctl show --property=MainPID --value arc-node.service 2>/dev/null || true)"
        active_state="$(systemctl show --property=ActiveState --value arc-node.service 2>/dev/null || true)"
        sub_state="$(systemctl show --property=SubState --value arc-node.service 2>/dev/null || true)"
        case "$active_state" in
            active)
                if [ "$sub_state" = running ]; then
                    case "$new_pid" in ''|0|*[!0-9]*) return 1 ;; esac
                    if [ "$new_pid" != "$old_pid" ] && kill -0 "$new_pid" 2>/dev/null; then
                        command_line="$(ps -ww -p "$new_pid" -o command= 2>/dev/null)" \
                            || return 1
                        case "$command_line" in
                            "$ARC_DIR/bin/arc-node"|"$ARC_DIR/bin/arc-node "*) return 0 ;;
                            *) return 1 ;;
                        esac
                    fi
                elif [ "$sub_state" != stop-sigterm ] && [ "$sub_state" != stop-sigkill ]; then
                    return 1
                fi
                ;;
            activating|deactivating|reloading) ;;
            # A managed Restart=always service should pass through activating /
            # auto-restart. Inactive or failed is terminal evidence that the
            # bridge did not restart, never a successful update.
            inactive|failed|*) return 1 ;;
        esac
        sleep 1
    done
    return 1
}

restore_systemd_state() {
    local status=0
    if [ "$SERVICE_SCOPE" = system ] \
        || { [ "$SERVICE_SCOPE" = system-user ] && [ "$SYSTEM_USER_BOOTSTRAP" = true ]; }; then
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
    elif [ "$SERVICE_SCOPE" = system-user ]; then
        if [ "$PRIOR_NODE_ACTIVE" = true ]; then
            restart_managed_system_user_node || status=1
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
    local status=0 node_unloaded=true
    if launchctl print "$LAUNCHD_DOMAIN/network.arc.node" >/dev/null 2>&1; then
        launchctl disable "$LAUNCHD_DOMAIN/network.arc.node" || status=1
        if ! stop_launchd_node_gracefully network.arc.node \
            "$ARC_DIR/bin/arc-node" "$ARC_DIR/bin/run-arc-node"; then
            node_unloaded=false
            status=1
        elif ! launchctl bootout "$LAUNCHD_DOMAIN/network.arc.node" 2>/dev/null; then
            node_unloaded=false
            status=1
        fi
    fi
    if [ "$node_unloaded" = true ] && [ "$PRIOR_LAUNCHD_NODE_LOADED" = true ]; then
        # A disabled service cannot be bootstrapped. Restore the loaded state
        # first, then reapply the exact prior persistent enablement override.
        launchctl enable "$LAUNCHD_DOMAIN/network.arc.node" || status=1
        launchctl bootstrap "$LAUNCHD_DOMAIN" "$NODE_PLIST" || status=1
    fi
    if [ "$PRIOR_LAUNCHD_NODE_DISABLED" = true ]; then
        launchctl disable "$LAUNCHD_DOMAIN/network.arc.node" || status=1
    else
        launchctl enable "$LAUNCHD_DOMAIN/network.arc.node" || status=1
    fi

    # A scheduled updater can be the process executing this rollback. Keep a
    # previously loaded job in place; its restored on-disk plist is used on
    # the next login/bootstrap without terminating the active transaction.
    if [ "$PRIOR_LAUNCHD_UPDATER_LOADED" = true ]; then
        if ! launchctl print "$LAUNCHD_DOMAIN/network.arc.update" >/dev/null 2>&1; then
            launchctl enable "$LAUNCHD_DOMAIN/network.arc.update" || status=1
            launchctl bootstrap "$LAUNCHD_DOMAIN" "$UPDATE_PLIST" || status=1
        fi
    else
        launchctl bootout "$LAUNCHD_DOMAIN/network.arc.update" 2>/dev/null || true
    fi
    if [ "$PRIOR_LAUNCHD_UPDATER_DISABLED" = true ]; then
        launchctl disable "$LAUNCHD_DOMAIN/network.arc.update" || status=1
    else
        launchctl enable "$LAUNCHD_DOMAIN/network.arc.update" || status=1
    fi
    return "$status"
}

enforce_post_intent_fail_stop() {
    local status=0 pid="" unit label
    [ "$LEGACY_RETIREMENT_INTENT_DURABLE" = true ] || return 1
    case "$LEGACY_SUPERVISOR_KIND" in
        linux-system)
            # The restored on-disk unit may be v0.7 while systemd still has a
            # v0.8 process loaded. Apply the no-KILL runtime boundary before
            # stopping either generation, then leave every old/new updater
            # and node unit disabled.
            as_root systemctl set-property --runtime arc-node.service \
                "TimeoutStopSec=${GRACEFUL_STOP_TIMEOUT_SECS}s" \
                SendSIGKILL=no >/dev/null 2>&1 || true
            as_root systemctl disable --now arc-node.service >/dev/null 2>&1 \
                || status=1
            for unit in \
                arc-updater.timer arc-updater.service \
                arc-node-update.timer arc-node-update.service; do
                as_root systemctl disable --now "$unit" >/dev/null 2>&1 || true
            done
            as_root systemctl daemon-reload || status=1
            if as_root systemctl is-active --quiet arc-node.service; then status=1; fi ;;
        macos-launchd)
            [ -n "$LAUNCHD_DOMAIN" ] || LAUNCHD_DOMAIN="user/$TARGET_UID"
            if launchctl print "gui/$TARGET_UID" >/dev/null 2>&1; then
                LAUNCHD_DOMAIN="gui/$TARGET_UID"
            fi
            for label in com.arc.updater network.arc.update; do
                launchctl disable "$LAUNCHD_DOMAIN/$label" >/dev/null 2>&1 || true
                launchctl bootout "$LAUNCHD_DOMAIN/$label" >/dev/null 2>&1 || true
            done
            for label in com.arc.inference network.arc.node; do
                launchctl disable "$LAUNCHD_DOMAIN/$label" >/dev/null 2>&1 || status=1
                pid="$(launchd_service_pid "$label" 2>/dev/null || true)"
                if [ -n "$pid" ]; then
                    if [ "$label" = network.arc.node ]; then
                        stop_launchd_node_gracefully "$label" \
                            "$ARC_DIR/bin/arc-node" "$ARC_DIR/bin/run-arc-node" || status=1
                    else
                        stop_launchd_node_gracefully "$label" \
                            "$ARC_DIR/bin/arc-node" || status=1
                    fi
                fi
                launchctl bootout "$LAUNCHD_DOMAIN/$label" >/dev/null 2>&1 || true
            done ;;
        detached|detached-untracked|none)
            ensure_no_legacy_arc_processes || status=1 ;;
        *) status=1 ;;
    esac
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
    if [ "$LEGACY_RETIREMENT_INTENT_DURABLE" = true ]; then
        enforce_post_intent_fail_stop || status=1
    else
        case "$SERVICE_SCOPE" in
            system|system-user|user) restore_systemd_state || status=1 ;;
            launchd) restore_launchd_state || status=1 ;;
        esac
        restore_legacy_runtime_state || status=1
    fi
    TRANSACTION_ACTIVE=false
    TRANSACTION_ROLLING_BACK=false
    if [ "$status" -eq 0 ]; then
        if [ "$LEGACY_RETIREMENT_INTENT_DURABLE" = true ]; then
            ok "Previous ARC files restored; v0.7 remains stopped and every legacy updater stays fenced behind the durable retirement intent"
        elif [ "$LEGACY_ADOPTION_ACTIVE" = true ]; then
            ok "Previous ARC files and node state restored; the unsigned legacy updater remains fenced"
        else
            ok "Previous ARC installation and service state restored"
        fi
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
verify_legacy_v07_network_retirement
begin_install_transaction
create_legacy_retirement_intent
if [ "$LEGACY_ADOPTION_ACTIVE" = true ]; then
    LEGACY_RETIREMENT_OBSERVATION_STARTED_AT="$(date -u '+%Y-%m-%dT%H:%M:%SZ')"
fi
if ! retire_legacy_runtime; then
    rollback_install_transaction || true
    die "Could not retire the verified v0.7 runtime safely"
fi
if [ "$LEGACY_ADOPTION_ACTIVE" = true ]; then
    write_legacy_retirement_stop_evidence
    finalize_legacy_retirement_receipt
fi
transactional_copy "$TMP_DIR/$NODE_ASSET" "$ARC_DIR/bin/arc-node" "$PROGRAM_MODE" root
transactional_copy "$TMP_DIR/$CLI_ASSET" "$ARC_DIR/bin/arc-cli" "$PROGRAM_MODE" root
transactional_copy "$TMP_DIR/testnet-seeds.txt" "$ARC_DIR/testnet-seeds.txt" "$CONFIG_MODE" root
transactional_copy "$TMP_DIR/genesis.toml" "$ARC_DIR/genesis.toml" "$CONFIG_MODE" root
if [ "$INSTALL_UPDATER" = true ]; then
    transactional_copy "$TMP_DIR/install.sh" "$ARC_DIR/bin/arc-installer" "$PROGRAM_MODE" root
fi

LEGACY_SEED_SOURCE=""
if [ "$LEGACY_ADOPTION_ACTIVE" = true ]; then
    LEGACY_SEED_SOURCE="$LEGACY_PRESERVED_DIR/identity.seed"
elif [ -e "$SEED_FILE" ] || [ -L "$SEED_FILE" ]; then
    if [ ! -f "$SEED_FILE" ] || [ -L "$SEED_FILE" ] || [ ! -s "$SEED_FILE" ]; then
        die "Legacy identity seed is not a non-empty regular file: $SEED_FILE"
    fi
    chmod 600 "$SEED_FILE"
    LEGACY_SEED_SOURCE="$SEED_FILE"
fi

KEY_FILE_PREEXISTED=false
if [ -e "$KEY_FILE" ] || [ -L "$KEY_FILE" ]; then
    KEY_FILE_PREEXISTED=true
    if [ ! -f "$KEY_FILE" ] || [ -L "$KEY_FILE" ]; then
        die "Validator keyfile is not a regular non-symlink file: $KEY_FILE"
    fi
    chmod 600 "$KEY_FILE"
    set_target_owner "$KEY_FILE"
else
    if [ -n "$LEGACY_SEED_SOURCE" ]; then
        as_private_file_owner "$LEGACY_SEED_SOURCE" \
            "$ARC_DIR/bin/arc-cli" keygen --scheme ed25519 \
            --legacy-seed-file "$LEGACY_SEED_SOURCE" \
            --output "$KEY_FILE" >/dev/null \
            || die "Could not convert the protected legacy identity to a validator keyfile"
    else
        as_target "$ARC_DIR/bin/arc-cli" keygen --scheme ed25519 \
            --output "$KEY_FILE" >/dev/null \
            || die "Could not generate a persistent validator keyfile"
    fi
    set_target_owner "$KEY_FILE"
fi

VALIDATOR_ADDRESS="$(as_target "$ARC_DIR/bin/arc-cli" keygen --verify-keyfile "$KEY_FILE")" \
    || die "Installed validator keyfile failed its cryptographic self-check"
printf '%s\n' "$VALIDATOR_ADDRESS" | grep -Eq '^[0-9a-f]{64}$' \
    || die "Installed validator keyfile produced an invalid public address"

# If an interrupted earlier install left both representations, prove that the
# new keyfile preserves the same public identity before retiring the seed.
if [ -n "$LEGACY_SEED_SOURCE" ] && [ "$KEY_FILE_PREEXISTED" = true ]; then
    IDENTITY_CHECK_FILE="$ARC_DIR/identity/.legacy-validator-key-check.json"
    if [ -e "$IDENTITY_CHECK_FILE" ] || [ -L "$IDENTITY_CHECK_FILE" ]; then
        die "Refusing pre-existing legacy identity check path: $IDENTITY_CHECK_FILE"
    fi
    as_private_file_owner "$LEGACY_SEED_SOURCE" \
        "$ARC_DIR/bin/arc-cli" keygen --scheme ed25519 \
        --legacy-seed-file "$LEGACY_SEED_SOURCE" \
        --output "$IDENTITY_CHECK_FILE" >/dev/null \
        || die "Could not verify the legacy identity during keyfile migration"
    LEGACY_VALIDATOR_ADDRESS="$(as_private_file_owner "$IDENTITY_CHECK_FILE" \
        "$ARC_DIR/bin/arc-cli" keygen --verify-keyfile "$IDENTITY_CHECK_FILE")" \
        || die "Converted legacy identity failed its cryptographic self-check"
    rm -f -- "$IDENTITY_CHECK_FILE"
    IDENTITY_CHECK_FILE=""
    [ "$LEGACY_VALIDATOR_ADDRESS" = "$VALIDATOR_ADDRESS" ] \
        || die "Validator keyfile does not preserve the legacy node address"
    unset LEGACY_VALIDATOR_ADDRESS
fi
unset VALIDATOR_ADDRESS LEGACY_SEED_SOURCE KEY_FILE_PREEXISTED

# A pre-v0.8.0 installer seed is retained only as protected migration
# evidence. No active environment file or runner ever reads it again.
if [ -e "$SEED_FILE" ]; then
    transactional_copy "$SEED_FILE" "$LEGACY_SEED_EVIDENCE" 600
    set_target_owner "$LEGACY_SEED_EVIDENCE"
    rm -f -- "$SEED_FILE"
fi
rm -f -- "$ARC_DIR/node.env"

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
    --validator-key-file "$KEY_FILE"
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
    local restart_policy=on-failure
    command -v systemctl >/dev/null 2>&1 || {
        warn "systemctl is unavailable; use --no-service for an install-only setup"
        return 1
    }
    if [ "$SERVICE_SCOPE" = system-user ] && [ "$SYSTEM_USER_BOOTSTRAP" = false ]; then
        restart_managed_system_user_node
        return
    fi
    [ "$SERVICE_SCOPE" != system-user ] || restart_policy=always
    {
        if [ "$SERVICE_SCOPE" = system-user ]; then
            printf '%s\n' '# ARC managed system-user node unit v1'
        fi
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
            "Restart=$restart_policy" \
            'RestartSec=5' \
            "TimeoutStopSec=$GRACEFUL_STOP_TIMEOUT_SECS" \
            'SendSIGKILL=no' \
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
    transactional_copy "$TMP_DIR/arc-node.service" \
        "$SYSTEMD_UNIT_DIR/arc-node.service" 644 root root || return 1
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
            "TimeoutStopSec=$GRACEFUL_STOP_TIMEOUT_SECS" \
            'SendSIGKILL=no' \
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
        printf '<key>ExitTimeOut</key><integer>%s</integer>\n' \
            "$GRACEFUL_STOP_TIMEOUT_SECS"
        printf '<key>WorkingDirectory</key><string>%s</string>\n' "$(xml_escape "$ARC_DIR")"
        printf '<key>StandardOutPath</key><string>%s</string>\n' "$(xml_escape "$ARC_DIR/node.log")"
        printf '<key>StandardErrorPath</key><string>%s</string>\n' "$(xml_escape "$ARC_DIR/node.log")"
        printf '%s\n' '</dict></plist>'
    } > "$TMP_DIR/network.arc.node.plist" || return 1
    if command -v plutil >/dev/null 2>&1; then
        plutil -lint "$TMP_DIR/network.arc.node.plist" >/dev/null || return 1
    fi
    transactional_copy "$TMP_DIR/network.arc.node.plist" "$NODE_PLIST" 600 || return 1
    if launchctl print "$LAUNCHD_DOMAIN/network.arc.node" >/dev/null 2>&1; then
        # The already-loaded plist may predate ExitTimeOut. Drain its exact
        # node process explicitly before bootout so launchd's legacy default
        # timeout cannot truncate accepted work or the final WAL fsync.
        launchctl disable "$LAUNCHD_DOMAIN/network.arc.node" || return 1
        stop_launchd_node_gracefully network.arc.node \
            "$ARC_DIR/bin/arc-node" "$ARC_DIR/bin/run-arc-node" || return 1
        launchctl bootout "$LAUNCHD_DOMAIN/network.arc.node" 2>/dev/null || return 1
    fi
    launchctl enable "$LAUNCHD_DOMAIN/network.arc.node" || return 1
    launchctl bootstrap "$LAUNCHD_DOMAIN" "$NODE_PLIST" || return 1
    launchctl kickstart "$LAUNCHD_DOMAIN/network.arc.node" || return 1
}

install_systemd_updater_system() {
    local updater_scope=system
    if [ "$SERVICE_SCOPE" = system-user ] && [ "$SYSTEM_USER_BOOTSTRAP" = false ]; then
        return 0
    fi
    {
        if [ "$SERVICE_SCOPE" = system-user ]; then
            printf '%s\n' '# ARC managed system-user updater unit v1'
        fi
        printf '%s\n' '[Unit]' 'Description=Update ARC Chain from a checksummed release' '' '[Service]' 'Type=oneshot'
        if [ "$SERVICE_SCOPE" = system-user ]; then
            updater_scope=system-user
            printf 'User=%s\nGroup=%s\n' "$TARGET_USER" "$TARGET_GROUP"
            printf '%s\n' 'NoNewPrivileges=true' 'UMask=0077'
        else
            printf 'Environment="ARC_TARGET_USER=%s"\n' "$(systemd_escape "$TARGET_USER")"
        fi
        write_exec_start "$ARC_DIR/bin/arc-installer" --update-only \
            --install-dir "$ARC_DIR" --service-scope "$updater_scope"
    } > "$TMP_DIR/arc-node-update.service" || return 1
    {
        if [ "$SERVICE_SCOPE" = system-user ]; then
            printf '%s\n' '# ARC managed system-user updater timer v1'
        fi
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
    transactional_copy "$TMP_DIR/arc-node-update.service" \
        "$SYSTEMD_UNIT_DIR/arc-node-update.service" 644 root root || return 1
    transactional_copy "$TMP_DIR/arc-node-update.timer" \
        "$SYSTEMD_UNIT_DIR/arc-node-update.timer" 644 root root || return 1
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
    # The loaded updater can be this installer process. Its arguments and
    # schedule are stable, so replacing the plist is sufficient; unloading it
    # here would terminate a valid update in the middle of its transaction.
    if ! launchctl print "$LAUNCHD_DOMAIN/network.arc.update" >/dev/null 2>&1; then
        launchctl bootstrap "$LAUNCHD_DOMAIN" "$UPDATE_PLIST" || return 1
    fi
    launchctl enable "$LAUNCHD_DOMAIN/network.arc.update" || return 1
}

disable_updater() {
    case "$SERVICE_SCOPE" in
        system|system-user)
            as_root systemctl disable --now arc-node-update.timer 2>/dev/null || true
            as_root rm -f -- "$SYSTEMD_UNIT_DIR/arc-node-update.service" \
                "$SYSTEMD_UNIT_DIR/arc-node-update.timer" || return 1
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

if [ "$LEGACY_ADOPTION_ACTIVE" = true ]; then
    # Re-check immediately before the first v0.8 launch.  A v0.7 binary does
    # not honor the new lock namespace, and an operator could otherwise start
    # the old path again during the file transaction.
    ensure_no_legacy_arc_processes
fi

if [ "$SERVICE_SCOPE" != none ]; then
    info "Installing $SERVICE_SCOPE service for $TARGET_USER"
    case "$SERVICE_SCOPE" in
        system|system-user) install_systemd_system || rollback_and_die "Could not install/start the system service" ;;
        user) install_systemd_user || rollback_and_die "Could not install/start the user service" ;;
        launchd) install_launchd || rollback_and_die "Could not install/start the launchd service" ;;
    esac

    if [ "$INSTALL_UPDATER" = true ]; then
        case "$SERVICE_SCOPE" in
            system|system-user) install_systemd_updater_system || rollback_and_die "Could not install the system update timer" ;;
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

if [ "$LEGACY_ADOPTION_ACTIVE" = true ]; then
    validate_completed_legacy_adoption
fi
commit_install_transaction
if [ "$LEGACY_ADOPTION_ACTIVE" = true ]; then
    promote_legacy_adoption_marker
    ok "Adopted the verified v$LEGACY_SOURCE_VERSION default install; legacy data/configuration remain preserved"
fi

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
printf '  Identity:    %s (preserved on upgrades; key material never printed)\n' "$KEY_FILE"
if [ -z "$MODEL_PATH" ]; then
    printf '  Inference:   observer/router only; rerun with --model /absolute/model.gguf to serve local inference\n'
else
    printf '  Model:       %s\n' "$MODEL_PATH"
fi
