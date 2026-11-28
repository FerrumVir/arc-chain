#!/usr/bin/env bash
set -uo pipefail

TEST_DIR="$(CDPATH='' cd -- "$(dirname -- "$0")" && pwd)"
REPO_ROOT="$(CDPATH='' cd -- "$TEST_DIR/../.." && pwd)"
# shellcheck source=/dev/null
. "$TEST_DIR/helpers/testlib.sh"

INSTALLER="$REPO_ROOT/install.sh"
CANONICAL_ASSETS='
arc-node-linux-x86_64
arc-cli-linux-x86_64
arc-node-linux-arm64
arc-cli-linux-arm64
arc-node-macos-arm64
arc-cli-macos-arm64
arc-node-macos-x86_64
arc-cli-macos-x86_64
arc-node-windows-x86_64.exe
arc-cli-windows-x86_64.exe
'
CUTOVER_ASSETS='
arc-cutover-policy.json
arc-legacy-maintenance-boundary.json
arc-recovery-checkpoint-descriptor.json
'

ACTIVE_SANDBOXES=""
NEW_SANDBOX=""
cleanup_sandboxes() {
    local sandbox
    for sandbox in $ACTIVE_SANDBOXES; do
        [ -n "$sandbox" ] && rm -rf "$sandbox"
    done
}
trap cleanup_sandboxes EXIT

canonical_pairs_for_version() {
    local version="$1" asset
    for asset in $CANONICAL_ASSETS; do
        printf '%s/%s ' "$version" "$asset"
    done
    printf '%s/SHA256SUMS %s/SHA256SUMS.sig %s/install.sh %s/testnet-seeds.txt %s/genesis.toml ' \
        "$version" "$version" "$version" "$version" "$version"
    for asset in $CUTOVER_ASSETS; do
        printf '%s/%s ' "$version" "$asset"
    done
    printf '\n'
}

file_sha256() {
    local file="$1"
    if command -v sha256sum >/dev/null 2>&1; then
        sha256sum "$file" | awk '{print $1}'
    else
        shasum -a 256 "$file" | awk '{print $1}'
    fi
}

new_sandbox() {
    local sandbox sandbox_root mock_bin command_name
    sandbox_root="${ARC_INSTALLER_TEST_TMPDIR:-$HOME}"
    sandbox="$(mktemp -d "$sandbox_root/arc-shard-installer.XXXXXX")"
    sandbox="$(CDPATH='' cd -- "$sandbox" && pwd -P)"
    ACTIVE_SANDBOXES="$ACTIVE_SANDBOXES $sandbox"
    mkdir -p "$sandbox/home" "$sandbox/bin" "$sandbox/tmp"
    : >"$sandbox/curl.log"
    : >"$sandbox/node-args.log"
    : >"$sandbox/preflight.log"
    : >"$sandbox/as-target.log"
    : >"$sandbox/service.log"
    : >"$sandbox/owner.log"
    printf 'MemTotal:       14575207 kB\nMemAvailable:   10485760 kB\n' >"$sandbox/proc-meminfo"

    mock_bin="$sandbox/bin"
    cp "$TEST_DIR/helpers/mock-curl.sh" "$mock_bin/curl"
    cp "$TEST_DIR/helpers/mock-platform-command.sh" "$mock_bin/platform-command"
    chmod +x "$mock_bin/curl" "$mock_bin/platform-command"
    for command_name in uname sleep free sysctl vm_stat openssl ssh-keygen hostname id getent chown stat runuser sudo systemctl launchctl ps; do
        ln -s platform-command "$mock_bin/$command_name"
    done
    NEW_SANDBOX="$sandbox"
}

reset_shard_test_env() {
    INSTALLER_OVERRIDE_UNDER_TEST=""
    ARC_DIR_UNDER_TEST=""
    ARC_NODE_VERSION_UNDER_TEST=""
    ARC_WORKER_MODE_UNDER_TEST=""
    ARC_TARGET_USER_UNDER_TEST=""
    SUDO_USER_UNDER_TEST=""
    ARC_INSTALL_MEMINFO_PATH_UNDER_TEST=""
    MOCK_INSTALL_SH_SOURCE_FILE_UNDER_TEST=""
    MOCK_SHARD_ADMITTED_LAYERS_UNDER_TEST=""
    MOCK_SHARD_CHECK_RAW_OUTPUT_UNDER_TEST=""
    MOCK_SHARD_CHECK_EXIT_CODE_UNDER_TEST=""
    MOCK_SHARD_REPORT_MODEL_ID_UNDER_TEST=""
    MOCK_CURRENT_UID_UNDER_TEST=""
    MOCK_CURRENT_USER_UNDER_TEST=""
    MOCK_TARGET_USER_UNDER_TEST=""
    MOCK_TARGET_UID_UNDER_TEST=""
    MOCK_TARGET_GROUP_UNDER_TEST=""
    MOCK_INSTALL_MARKER_UID_UNDER_TEST=""
    MOCK_WORKER_POLICY_UID_UNDER_TEST=""
    MOCK_ROOT_OWNED_PREFIX_UNDER_TEST=""
    MOCK_ROOT_OWNED_ANCESTORS_UNDER_TEST="false"
    MOCK_SUDO_EXECUTE_UNDER_TEST="false"
    MOCK_TARGET_UNREADABLE_PATH_UNDER_TEST=""
    MOCK_TAMPER_MANIFEST_SIGNATURE_UNDER_TEST="0"
    MOCK_PS_RSS_ROWS_UNDER_TEST=""
    MOCK_PS_EXPECT_PID_UNDER_TEST=""
    MOCK_PS_UID_UNDER_TEST=""
    MOCK_PS_PID_RSS_UNDER_TEST=""
    MOCK_PS_COMMAND_UNDER_TEST=""
    MOCK_SYSCTL_MEMSIZE_UNDER_TEST=""
    MOCK_VM_STAT_PAGE_SIZE_UNDER_TEST=""
    MOCK_VM_STAT_FREE_PAGES_UNDER_TEST=""
    MOCK_VM_STAT_ACTIVE_PAGES_UNDER_TEST=""
    MOCK_VM_STAT_INACTIVE_PAGES_UNDER_TEST=""
    MOCK_VM_STAT_SPECULATIVE_PAGES_UNDER_TEST=""
    MOCK_VM_STAT_PURGEABLE_PAGES_UNDER_TEST=""
    MOCK_HEALTH_PORT_UNDER_TEST=""
    MOCK_HEALTH_STATUS_UNDER_TEST="ok"
    MOCK_SYSTEMD_NODE_ACTIVE_UNDER_TEST="false"
    MOCK_SYSTEMD_NODE_ENABLED_UNDER_TEST="false"
    MOCK_SYSTEMD_UPDATER_ACTIVE_UNDER_TEST="false"
    MOCK_SYSTEMD_UPDATER_ENABLED_UNDER_TEST="false"
    MOCK_SYSTEMD_MAIN_PID_UNDER_TEST=""
    MOCK_SERVICE_FAIL_MATCH_UNDER_TEST=""
    ARC_INSTALL_TEST_FAIL_AFTER_COPY_UNDER_TEST=""
}

invoke_shard_installer() {
    local sandbox="$1" os="$2" arch="$3" fixture="$4" version="$5"
    shift 5
    local current_user="${MOCK_CURRENT_USER_UNDER_TEST:-arc-community-test}"
    local target_user="${MOCK_TARGET_USER_UNDER_TEST:-arc-community-test}"
    local target_uid="${MOCK_TARGET_UID_UNDER_TEST:-1000}"
    local env_args=(
        PATH="$sandbox/bin:/usr/bin:/bin:/usr/sbin:/sbin"
        HOME="$sandbox/home"
        USER="$current_user"
        SUDO_USER="${SUDO_USER_UNDER_TEST:-}"
        ARC_DIR="${ARC_DIR_UNDER_TEST:-$sandbox/arc}"
        ARC_NODE_VERSION="${ARC_NODE_VERSION_UNDER_TEST:-}"
        TEST_UNAME_S="$os"
        TEST_UNAME_M="$arch"
        MOCK_RELEASE_FILE="$fixture"
        MOCK_AVAILABLE_ASSETS="$(canonical_pairs_for_version "$version")"
        MOCK_CHECKSUM_ASSETS="$CANONICAL_ASSETS $CUTOVER_ASSETS testnet-seeds.txt genesis.toml install.sh"
        MOCK_INSTALL_SH_SOURCE_FILE="${MOCK_INSTALL_SH_SOURCE_FILE_UNDER_TEST:-}"
        MOCK_SHARD_ADMITTED_LAYERS="${MOCK_SHARD_ADMITTED_LAYERS_UNDER_TEST:-}"
        MOCK_SHARD_CHECK_RAW_OUTPUT="${MOCK_SHARD_CHECK_RAW_OUTPUT_UNDER_TEST:-}"
        MOCK_SHARD_CHECK_EXIT_CODE="${MOCK_SHARD_CHECK_EXIT_CODE_UNDER_TEST:-0}"
        MOCK_SHARD_REPORT_MODEL_ID="${MOCK_SHARD_REPORT_MODEL_ID_UNDER_TEST:-}"
        MOCK_TAMPER_MANIFEST_SIGNATURE="${MOCK_TAMPER_MANIFEST_SIGNATURE_UNDER_TEST:-0}"
        MOCK_CURL_LOG="$sandbox/curl.log"
        MOCK_SERVICE_LOG="$sandbox/service.log"
        MOCK_OWNER_LOG="$sandbox/owner.log"
        MOCK_CURRENT_UID="${MOCK_CURRENT_UID_UNDER_TEST:-1000}"
        MOCK_CURRENT_USER="$current_user"
        MOCK_TARGET_USER="$target_user"
        MOCK_TARGET_UID="$target_uid"
        MOCK_TARGET_GROUP="${MOCK_TARGET_GROUP_UNDER_TEST:-$target_user}"
        MOCK_INSTALL_MARKER_UID="${MOCK_INSTALL_MARKER_UID_UNDER_TEST:-$target_uid}"
        MOCK_WORKER_POLICY_UID="${MOCK_WORKER_POLICY_UID_UNDER_TEST:-$target_uid}"
        MOCK_ROOT_OWNED_PREFIX="${MOCK_ROOT_OWNED_PREFIX_UNDER_TEST:-}"
        MOCK_ROOT_OWNED_ANCESTORS="${MOCK_ROOT_OWNED_ANCESTORS_UNDER_TEST:-false}"
        MOCK_SUDO_EXECUTE="${MOCK_SUDO_EXECUTE_UNDER_TEST:-false}"
        MOCK_TARGET_UNREADABLE_PATH="${MOCK_TARGET_UNREADABLE_PATH_UNDER_TEST:-}"
        MOCK_TARGET_HOME="$sandbox/home"
        MOCK_HEALTH_PORT="${MOCK_HEALTH_PORT_UNDER_TEST:-}"
        MOCK_HEALTH_STATUS="${MOCK_HEALTH_STATUS_UNDER_TEST:-ok}"
        MOCK_RELEASE_COMMIT=aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa
        MOCK_SYSTEMD_NODE_ACTIVE="${MOCK_SYSTEMD_NODE_ACTIVE_UNDER_TEST:-false}"
        MOCK_SYSTEMD_NODE_ENABLED="${MOCK_SYSTEMD_NODE_ENABLED_UNDER_TEST:-false}"
        MOCK_SYSTEMD_UPDATER_ACTIVE="${MOCK_SYSTEMD_UPDATER_ACTIVE_UNDER_TEST:-false}"
        MOCK_SYSTEMD_UPDATER_ENABLED="${MOCK_SYSTEMD_UPDATER_ENABLED_UNDER_TEST:-false}"
        MOCK_SYSTEMD_MAIN_PID="${MOCK_SYSTEMD_MAIN_PID_UNDER_TEST:-}"
        MOCK_SYSTEMD_STATE_DIR="$sandbox/systemd-state"
        MOCK_LAUNCHD_STATE_DIR="$sandbox/launchd-state"
        MOCK_PS_RSS_ROWS="${MOCK_PS_RSS_ROWS_UNDER_TEST:-}"
        MOCK_PS_EXPECT_PID="${MOCK_PS_EXPECT_PID_UNDER_TEST:-}"
        MOCK_PS_UID="${MOCK_PS_UID_UNDER_TEST:-$target_uid}"
        MOCK_PS_PID_RSS="${MOCK_PS_PID_RSS_UNDER_TEST:-0}"
        MOCK_PS_COMMAND="${MOCK_PS_COMMAND_UNDER_TEST:-}"
        MOCK_SYSCTL_MEMSIZE="${MOCK_SYSCTL_MEMSIZE_UNDER_TEST:-17179869184}"
        MOCK_VM_STAT_PAGE_SIZE="${MOCK_VM_STAT_PAGE_SIZE_UNDER_TEST:-4096}"
        MOCK_VM_STAT_FREE_PAGES="${MOCK_VM_STAT_FREE_PAGES_UNDER_TEST:-3145728}"
        MOCK_VM_STAT_ACTIVE_PAGES="${MOCK_VM_STAT_ACTIVE_PAGES_UNDER_TEST:-262144}"
        MOCK_VM_STAT_INACTIVE_PAGES="${MOCK_VM_STAT_INACTIVE_PAGES_UNDER_TEST:-0}"
        MOCK_VM_STAT_SPECULATIVE_PAGES="${MOCK_VM_STAT_SPECULATIVE_PAGES_UNDER_TEST:-0}"
        MOCK_VM_STAT_PURGEABLE_PAGES="${MOCK_VM_STAT_PURGEABLE_PAGES_UNDER_TEST:-0}"
        MOCK_SERVICE_FAIL_MATCH="${MOCK_SERVICE_FAIL_MATCH_UNDER_TEST:-}"
        MOCK_SERVICE_FAIL_ONCE_FILE="$sandbox/service-fail-once"
        ARC_INSTALL_TEST_FAIL_AFTER_COPY="${ARC_INSTALL_TEST_FAIL_AFTER_COPY_UNDER_TEST:-}"
        ARC_INSTALL_MEMINFO_PATH="${ARC_INSTALL_MEMINFO_PATH_UNDER_TEST:-$sandbox/proc-meminfo}"
        ARC_HEALTH_TIMEOUT=4
        ARC_TEST_NODE_ARGS_LOG="$sandbox/node-args.log"
        ARC_TEST_SHARD_PREFLIGHT_LOG="$sandbox/preflight.log"
        ARC_TEST_AS_TARGET_LOG="$sandbox/as-target.log"
        TMPDIR="$sandbox/tmp"
        NO_COLOR=1
        LC_ALL=C
    )
    if [ -n "${ARC_WORKER_MODE_UNDER_TEST:-}" ]; then
        env_args+=(ARC_WORKER_MODE="$ARC_WORKER_MODE_UNDER_TEST")
    fi
    if [ -n "${ARC_TARGET_USER_UNDER_TEST:-}" ]; then
        env_args+=(ARC_TARGET_USER="$ARC_TARGET_USER_UNDER_TEST")
    fi
    env -i "${env_args[@]}" /bin/bash "${INSTALLER_OVERRIDE_UNDER_TEST:-$INSTALLER}" "$@"
}

write_canonical_model() {
    printf 'ARC_MOCK_CANONICAL_SHARD_MODEL_V1\n' >"$1"
}

desktop_pin_and_nominal_ram_parity_contract() {
    python3 - "$INSTALLER" \
        "$REPO_ROOT/desktop/src-tauri/src/node_manager.rs" \
        "$REPO_ROOT/desktop/src-tauri/src/hardware.rs" <<'PY'
import re
import subprocess
import sys

installer_path, node_manager_path, hardware_path = sys.argv[1:]
with open(installer_path, encoding="utf-8") as handle:
    installer = handle.read()
with open(node_manager_path, encoding="utf-8") as handle:
    node_manager = handle.read()
with open(hardware_path, encoding="utf-8") as handle:
    hardware = handle.read()


def shell_scalar(name):
    match = re.search(rf"^{re.escape(name)}=([^\n]+)$", installer, re.MULTILINE)
    if not match:
        raise SystemExit(f"install.sh is missing {name}")
    return match.group(1).strip().strip("\"'")


def rust_pin_field(field):
    block = re.search(
        r"(?:PRODUCTION_SHARD_COORDINATOR_PIN:\s*ShardCoordinatorReleasePin\s*=\s*ShardCoordinatorReleasePin|PRODUCTION_SHARD_COORDINATOR:\s*Option<PinnedShardCoordinator>\s*=\s*Some\(PinnedShardCoordinator)\s*\{([^}]+)\}",
        node_manager,
        re.DOTALL,
    )
    if not block:
        raise SystemExit("could not locate production shard coordinator pin in node_manager.rs")
    match = re.search(rf'\b{re.escape(field)}:\s*"([^"]+)"', block.group(1))
    if not match:
        raise SystemExit(f"missing {field} in production shard coordinator pin")
    return match.group(1)


for shell_key, rust_key in (
    ("SHARD_COORDINATOR_URL", "coordinator_url"),
    ("SHARD_COORDINATOR_ID", "coordinator_id"),
    ("SHARD_TRANSACTION_DOMAIN", "transaction_domain"),
    ("SHARD_EXPECTED_MODEL_ID", "expected_model_id"),
):
    lhs = shell_scalar(shell_key)
    rhs = rust_pin_field(rust_key)
    if lhs != rhs:
        raise SystemExit(f"{shell_key} ({lhs}) != desktop {rust_key} ({rhs})")

min_ram = re.search(r"pub const WORKER_MIN_NOMINAL_RAM_GB:\s*u64\s*=\s*(\d+);", hardware)
max_reserved = re.search(r"const MAX_RESERVED_PERCENT:\s*u64\s*=\s*(\d+);", hardware)
sizes_block = re.search(r"const MARKETED_RAM_SIZES_GB:\s*&\[u64\]\s*=\s*&\[([^\]]+)\];", hardware)
if not (min_ram and max_reserved and sizes_block):
    raise SystemExit("could not parse nominal RAM constants from hardware.rs")
if shell_scalar("WORKER_MIN_NOMINAL_RAM_GB") != min_ram.group(1):
    raise SystemExit("WORKER_MIN_NOMINAL_RAM_GB drifted from hardware.rs")
if shell_scalar("MAX_RESERVED_RAM_PERCENT") != max_reserved.group(1):
    raise SystemExit("MAX_RESERVED_RAM_PERCENT drifted from hardware.rs")

rust_sizes = [int(x) for x in re.findall(r"\d+", sizes_block.group(1))]
shell_sizes_match = re.search(r"^MARKETED_RAM_SIZES_GB=\(\s*([^)]+)\)", installer, re.MULTILINE | re.DOTALL)
if not shell_sizes_match:
    raise SystemExit("install.sh is missing MARKETED_RAM_SIZES_GB")
shell_sizes = [int(x) for x in re.findall(r"\d+", shell_sizes_match.group(1))]
if shell_sizes != rust_sizes:
    raise SystemExit(f"MARKETED_RAM_SIZES_GB drifted: {shell_sizes} != {rust_sizes}")

func_match = re.search(
    r"^nominal_ram_gb\(\) \{.*?^\}",
    installer,
    re.MULTILINE | re.DOTALL,
)
if not func_match:
    raise SystemExit("could not extract nominal_ram_gb from install.sh")
snippet = "\n".join(
    [
        f"GIB_BYTES={shell_scalar('GIB_BYTES')}",
        f"MAX_RESERVED_RAM_PERCENT={shell_scalar('MAX_RESERVED_RAM_PERCENT')}",
        shell_sizes_match.group(0),
        func_match.group(0),
    ]
)

cases = [
    (17179869184, 16),
    (14925011353, 16),
    (14602888819, 16),
    (14602888818, 13),
    (14495514624, 13),
    (12455405158, 12),
    (11686449570, 12),
    (8589934592, 8),
]
for visible_bytes, expected_gb in cases:
    out = subprocess.check_output(
        ["/bin/bash", "-c", f"{snippet}\nnominal_ram_gb {visible_bytes}"],
        text=True,
    ).strip()
    if int(out) != expected_gb:
        raise SystemExit(
            f"nominal_ram_gb({visible_bytes}) returned {out}, expected {expected_gb}"
        )
PY
}

three_cycle_updater_reconciles_legacy_whole_worker_and_stamps_running_installer() {
    local sandbox model_path output key_before data_sentinel old_installer
    local v23_installer_sha replacement_stub_sha
    reset_shard_test_env
    new_sandbox
    sandbox="$NEW_SANDBOX"
    model_path="$sandbox/canonical-model.gguf"
    write_canonical_model "$model_path"
    MOCK_HEALTH_PORT_UNDER_TEST=18444
    ARC_NODE_VERSION_UNDER_TEST=0.8.0
    output="$sandbox/initial-v080.out"
    invoke_shard_installer "$sandbox" Linux x86_64 \
        "$TEST_DIR/fixtures/release-v0.8.0.json" 0.8.0 \
        --user-service --port 18444 --model "$model_path" --worker-mode full >"$output" 2>&1 || {
            cat "$output"
            return 1
        }
    rm -f "$sandbox/arc/worker-policy.conf"
    old_installer="$sandbox/pre-shard-v26-installer.sh"
    {
        printf '%s\n' '#!/usr/bin/env bash' 'set -Eeuo pipefail'
        printf 'cp %q %q\n' "$INSTALLER" "$sandbox/arc/bin/arc-installer"
        printf 'chmod 700 %q\n' "$sandbox/arc/bin/arc-installer"
        printf 'sed -i.bak '\''s/0\.8\.0/0.8.23/g'\'' %q %q %q\n' \
            "$sandbox/arc/bin/arc-node" "$sandbox/arc/bin/arc-cli" "$sandbox/arc/install.conf"
        printf 'rm -f %q.bak %q.bak %q.bak\n' \
            "$sandbox/arc/bin/arc-node" "$sandbox/arc/bin/arc-cli" "$sandbox/arc/install.conf"
    } >"$old_installer"
    chmod 700 "$old_installer"
    cp "$old_installer" "$sandbox/arc/bin/arc-installer"

    key_before="$(file_sha256 "$sandbox/arc/identity/validator-key.json")"
    data_sentinel="$sandbox/arc/data/chain-sentinel"
    printf 'existing-chain-state\n' >"$data_sentinel"

    /bin/bash "$sandbox/arc/bin/arc-installer" --update-only || return 1
    assert_equals 'arc-node 0.8.23' "$("$sandbox/arc/bin/arc-node" --version)" \
        'cycle 1 simulation did not advance binary to 0.8.23' || return 1
    assert_file_contains "$sandbox/arc/bin/run-arc-node" '--full-integer-worker' \
        'cycle 1 simulation did not leave pre-shard whole-worker launcher' || return 1
    [ ! -e "$sandbox/arc/worker-policy.conf" ] || {
        printf 'cycle 1 simulation unexpectedly created worker-policy.conf\n'
        return 1
    }

    v23_installer_sha="$(file_sha256 "$sandbox/arc/bin/arc-installer")"
    printf '%s\n' '#!/usr/bin/env bash' 'exit 0' >"$sandbox/replacement-stub.sh"
    chmod 755 "$sandbox/replacement-stub.sh"
    replacement_stub_sha="$(file_sha256 "$sandbox/replacement-stub.sh")"
    [ "$v23_installer_sha" != "$replacement_stub_sha" ] || return 1

    : >"$sandbox/curl.log"
    : >"$sandbox/service.log"
    : >"$sandbox/preflight.log"
    INSTALLER_OVERRIDE_UNDER_TEST="$sandbox/arc/bin/arc-installer"
    MOCK_INSTALL_SH_SOURCE_FILE_UNDER_TEST="$INSTALLER"
    MOCK_SYSTEMD_NODE_ACTIVE_UNDER_TEST=true
    MOCK_SYSTEMD_NODE_ENABLED_UNDER_TEST=true
    MOCK_SYSTEMD_UPDATER_ACTIVE_UNDER_TEST=true
    MOCK_SYSTEMD_UPDATER_ENABLED_UNDER_TEST=true
    ARC_NODE_VERSION_UNDER_TEST=""
    output="$sandbox/cycle2-reconcile.out"
    invoke_shard_installer "$sandbox" Linux x86_64 \
        "$TEST_DIR/fixtures/release-v0.8.23.json" 0.8.23 \
        --update-only >"$output" 2>&1 || {
            cat "$output"
            reset_shard_test_env
            return 1
        }
    assert_file_contains "$output" 'Reconciling headless worker launcher policy at v0\.8\.23' \
        'cycle 2 did not log same-version worker policy reconciliation' || return 1
    assert_file_contains "$sandbox/preflight.log" 'community-shard-worker .*--max-working-set-bytes 7462505472 --max-resident-layers 6 --threads 1 --check-only' \
        'cycle 2 did not run check-only preflight with min(visible-4GiB, visible/2) budget on 13.9GiB host' || return 1
    assert_file_contains "$sandbox/arc/bin/run-arc-node" '--community-shard-worker' \
        'cycle 2 did not switch runner to --community-shard-worker' || return 1
    assert_file_contains "$sandbox/arc/bin/run-arc-node" '--threads 1' \
        'cycle 2 shard runner omitted --threads 1' || return 1
    assert_file_contains "$sandbox/arc/bin/run-arc-node" 'https://149\.28\.153\.31' \
        'cycle 2 shard runner omitted pinned coordinator URL' || return 1
    assert_file_not_contains "$sandbox/arc/bin/run-arc-node" '--full-integer-worker' \
        'cycle 2 shard runner kept --full-integer-worker' || return 1
    assert_file_not_contains "$sandbox/arc/bin/run-arc-node" '--community-mode' \
        'cycle 2 shard runner kept --community-mode' || return 1
    assert_file_not_contains "$sandbox/arc/bin/run-arc-node" '--community-rpc-url' \
        'cycle 2 shard runner kept --community-rpc-url' || return 1
    assert_equals "$key_before" "$(file_sha256 "$sandbox/arc/identity/validator-key.json")" \
        'cycle 2 changed validator-key.json' || return 1
    assert_file_contains "$data_sentinel" '^existing-chain-state$' \
        'cycle 2 lost existing chain data' || return 1
    assert_equals '7' "$(wc -l < "$sandbox/arc/install.conf" | tr -d ' ')" \
        'install.conf gained unexpected keys beyond v1 schema' || return 1
    assert_file_contains "$sandbox/arc/worker-policy.conf" "^installer_sha256=$v23_installer_sha$" \
        'worker-policy.conf did not stamp the running installer SHA-256' || return 1
    assert_file_contains "$sandbox/arc/worker-policy.conf" '^configured_worker_mode=auto$' \
        'worker-policy.conf did not record configured_worker_mode=auto' || return 1
    assert_file_contains "$sandbox/arc/worker-policy.conf" '^resolved_worker_mode=shard$' \
        'worker-policy.conf did not record resolved_worker_mode=shard' || return 1

    : >"$sandbox/curl.log"
    : >"$sandbox/service.log"
    output="$sandbox/cycle3-noop.out"
    invoke_shard_installer "$sandbox" Linux x86_64 \
        "$TEST_DIR/fixtures/release-v0.8.23.json" 0.8.23 \
        --update-only >"$output" 2>&1 || {
            cat "$output"
            reset_shard_test_env
            return 1
        }
    reset_shard_test_env
    assert_file_contains "$output" 'Already up to date at v0\.8\.23' \
        'cycle 3 did not no-op when policy and installer hash matched' || return 1
    assert_file_not_contains "$sandbox/curl.log" '/releases/download/' \
        'cycle 3 re-downloaded release payloads on an already reconciled install' || return 1
    assert_file_not_contains "$sandbox/service.log" 'restart arc-node\.service' \
        'cycle 3 restarted arc-node.service on an already reconciled install' || return 1
}

upgrade_stamps_running_installer_not_replacement_until_next_cycle() {
    local sandbox model_path running_copy replacement_copy running_sha replacement_sha output
    reset_shard_test_env
    new_sandbox
    sandbox="$NEW_SANDBOX"
    model_path="$sandbox/canonical-model.gguf"
    write_canonical_model "$model_path"
    running_copy="$sandbox/running-v080-installer.sh"
    replacement_copy="$sandbox/replacement-v0823-installer.sh"
    cp "$INSTALLER" "$running_copy"
    cp "$INSTALLER" "$replacement_copy"
    printf '\n' >>"$replacement_copy"
    chmod 700 "$running_copy" "$replacement_copy"
    running_sha="$(file_sha256 "$running_copy")"
    replacement_sha="$(file_sha256 "$replacement_copy")"
    [ "$running_sha" != "$replacement_sha" ] || return 1

    INSTALLER_OVERRIDE_UNDER_TEST="$running_copy"
    MOCK_INSTALL_SH_SOURCE_FILE_UNDER_TEST="$running_copy"
    ARC_NODE_VERSION_UNDER_TEST=0.8.0
    invoke_shard_installer "$sandbox" Linux x86_64 \
        "$TEST_DIR/fixtures/release-v0.8.0.json" 0.8.0 \
        --no-service --model "$model_path" >"$sandbox/install-080.out" 2>&1 || {
            cat "$sandbox/install-080.out"
            reset_shard_test_env
            return 1
        }
    cp "$running_copy" "$sandbox/arc/bin/arc-installer"

    INSTALLER_OVERRIDE_UNDER_TEST="$sandbox/arc/bin/arc-installer"
    MOCK_INSTALL_SH_SOURCE_FILE_UNDER_TEST="$replacement_copy"
    ARC_NODE_VERSION_UNDER_TEST=0.8.23
    output="$sandbox/upgrade-0823.out"
    invoke_shard_installer "$sandbox" Linux x86_64 \
        "$TEST_DIR/fixtures/release-v0.8.23.json" 0.8.23 \
        --no-service >"$output" 2>&1 || {
            cat "$output"
            reset_shard_test_env
            return 1
        }
    assert_file_contains "$sandbox/arc/worker-policy.conf" "^installer_sha256=$running_sha$" \
        'upgrade stamped replacement installer hash instead of the running installer body' || return 1
    reset_shard_test_env
}

observer_and_explicit_full_modes_persist_across_updates() {
    local sandbox model_path
    reset_shard_test_env
    new_sandbox
    sandbox="$NEW_SANDBOX"
    MOCK_INSTALL_SH_SOURCE_FILE_UNDER_TEST="$INSTALLER"
    ARC_NODE_VERSION_UNDER_TEST=0.8.0
    invoke_shard_installer "$sandbox" Linux x86_64 \
        "$TEST_DIR/fixtures/release-v0.8.0.json" 0.8.0 \
        --no-service >"$sandbox/observer-080.out" 2>&1 || return 1
    assert_file_contains "$sandbox/arc/worker-policy.conf" '^resolved_worker_mode=observer$' \
        'observer install did not persist resolved_worker_mode=observer' || return 1
    assert_file_not_contains "$sandbox/arc/bin/run-arc-node" '--community-shard-worker|--full-integer-worker|--model' \
        'observer runner included worker or model flags' || return 1
    ARC_NODE_VERSION_UNDER_TEST=0.8.23
    invoke_shard_installer "$sandbox" Linux x86_64 \
        "$TEST_DIR/fixtures/release-v0.8.23.json" 0.8.23 \
        --no-service >"$sandbox/observer-0823.out" 2>&1 || return 1
    assert_file_contains "$sandbox/arc/worker-policy.conf" '^resolved_worker_mode=observer$' \
        'observer update changed resolved_worker_mode' || return 1

    reset_shard_test_env
    new_sandbox
    sandbox="$NEW_SANDBOX"
    model_path="$sandbox/canonical-model.gguf"
    write_canonical_model "$model_path"
    MOCK_INSTALL_SH_SOURCE_FILE_UNDER_TEST="$INSTALLER"
    ARC_NODE_VERSION_UNDER_TEST=0.8.0
    invoke_shard_installer "$sandbox" Linux x86_64 \
        "$TEST_DIR/fixtures/release-v0.8.0.json" 0.8.0 \
        --no-service --model "$model_path" --worker-mode full >"$sandbox/full-080.out" 2>&1 || return 1
    assert_file_contains "$sandbox/arc/worker-policy.conf" '^configured_worker_mode=full$' \
        'explicit --worker-mode full was not persisted' || return 1
    assert_file_contains "$sandbox/arc/bin/run-arc-node" '--full-integer-worker' \
        'explicit --worker-mode full did not emit --full-integer-worker' || return 1
    INSTALLER_OVERRIDE_UNDER_TEST="$sandbox/arc/bin/arc-installer"
    ARC_NODE_VERSION_UNDER_TEST=0.8.23
    invoke_shard_installer "$sandbox" Linux x86_64 \
        "$TEST_DIR/fixtures/release-v0.8.23.json" 0.8.23 \
        --no-service >"$sandbox/full-0823.out" 2>&1 || return 1
    reset_shard_test_env
    assert_file_contains "$sandbox/arc/worker-policy.conf" '^configured_worker_mode=full$' \
        'update lost saved configured_worker_mode=full' || return 1
    assert_file_contains "$sandbox/arc/bin/run-arc-node" '--full-integer-worker' \
        'update switched explicit full worker into shard mode' || return 1
    assert_file_not_contains "$sandbox/arc/bin/run-arc-node" '--community-shard-worker' \
        'explicit full worker gained --community-shard-worker' || return 1
}

ineligible_preflight_and_memory_auto_fallback_vs_explicit_shard_fail_closed() {
    local sandbox canonical_model custom_model runner_before policy_before status case_name
    for case_name in noncanonical sub16gb-13_5gib cliff-11_69gb external-pressure five-layers malformed-report wrong-model-id; do
        reset_shard_test_env
        new_sandbox
        sandbox="$NEW_SANDBOX"
        canonical_model="$sandbox/canonical.gguf"
        custom_model="$sandbox/custom.gguf"
        write_canonical_model "$canonical_model"
        printf 'custom-noncanonical-weights\n' >"$custom_model"
        MOCK_HEALTH_PORT_UNDER_TEST=18444
        ARC_NODE_VERSION_UNDER_TEST=0.8.0
        invoke_shard_installer "$sandbox" Linux x86_64 \
            "$TEST_DIR/fixtures/release-v0.8.0.json" 0.8.0 \
            --user-service --port 18444 --model "$canonical_model" --worker-mode full >"$sandbox/base.out" 2>&1 || return 1
        runner_before="$(file_sha256 "$sandbox/arc/bin/run-arc-node")"
        policy_before="$(file_sha256 "$sandbox/arc/worker-policy.conf")"
        : >"$sandbox/service.log"

        local target_model="$canonical_model"
        case "$case_name" in
            noncanonical) target_model="$custom_model" ;;
            sub16gb-13_5gib)
                printf 'MemTotal:       14155776 kB\nMemAvailable:   10485760 kB\n' >"$sandbox/low-meminfo"
                ARC_INSTALL_MEMINFO_PATH_UNDER_TEST="$sandbox/low-meminfo"
                ;;
            cliff-11_69gb)
                printf 'MemTotal:       11412549 kB\nMemAvailable:    9437184 kB\n' >"$sandbox/cliff-meminfo"
                ARC_INSTALL_MEMINFO_PATH_UNDER_TEST="$sandbox/cliff-meminfo"
                ;;
            external-pressure)
                printf 'MemTotal:       16777216 kB\nMemAvailable:    2097152 kB\n' >"$sandbox/pressured-meminfo"
                ARC_INSTALL_MEMINFO_PATH_UNDER_TEST="$sandbox/pressured-meminfo"
                ;;
            five-layers) MOCK_SHARD_ADMITTED_LAYERS_UNDER_TEST=5 ;;
            malformed-report) MOCK_SHARD_CHECK_RAW_OUTPUT_UNDER_TEST='{"mode":"check-only","admission":' ;;
            wrong-model-id) MOCK_SHARD_REPORT_MODEL_ID_UNDER_TEST=0000000000000000000000000000000000000000000000000000000000000000 ;;
        esac

        MOCK_SYSTEMD_NODE_ACTIVE_UNDER_TEST=true
        MOCK_SYSTEMD_NODE_ENABLED_UNDER_TEST=true
        ARC_NODE_VERSION_UNDER_TEST=0.8.23
        : >"$sandbox/curl.log"
        : >"$sandbox/preflight.log"
        invoke_shard_installer "$sandbox" Linux x86_64 \
            "$TEST_DIR/fixtures/release-v0.8.23.json" 0.8.23 \
            --user-service --port 18444 --model "$target_model" --worker-mode shard >"$sandbox/explicit-$case_name.out" 2>&1
        status=$?
        [ "$status" -ne 0 ] || {
            printf 'explicit --worker-mode shard unexpectedly succeeded on %s\n' "$case_name"
            reset_shard_test_env
            return 1
        }
        case "$case_name" in
            sub16gb-13_5gib|cliff-11_69gb|external-pressure)
                [ ! -s "$sandbox/preflight.log" ] || {
                    printf 'memory-ineligible case %s executed model preflight\n' "$case_name"
                    reset_shard_test_env
                    return 1
                }
                ;;
            *)
                [ -s "$sandbox/preflight.log" ] || {
                    printf 'explicit shard case %s failed before check-only preflight ran\n' "$case_name"
                    cat "$sandbox/explicit-$case_name.out"
                    reset_shard_test_env
                    return 1
                }
                ;;
        esac
        assert_equals "$runner_before" "$(file_sha256 "$sandbox/arc/bin/run-arc-node")" \
            "explicit shard failure ($case_name) modified run-arc-node" || return 1
        assert_equals "$policy_before" "$(file_sha256 "$sandbox/arc/worker-policy.conf")" \
            "explicit shard failure ($case_name) modified worker-policy.conf" || return 1
        assert_file_not_contains "$sandbox/service.log" 'stop|restart|disable' \
            "explicit shard failure ($case_name) touched the running service" || return 1

        invoke_shard_installer "$sandbox" Linux x86_64 \
            "$TEST_DIR/fixtures/release-v0.8.23.json" 0.8.23 \
            --user-service --port 18444 --model "$target_model" --worker-mode auto >"$sandbox/auto-$case_name.out" 2>&1 || {
                cat "$sandbox/auto-$case_name.out"
                reset_shard_test_env
                return 1
            }
        assert_file_contains "$sandbox/arc/bin/run-arc-node" '--full-integer-worker' \
            "auto fallback ($case_name) did not emit --full-integer-worker" || return 1
        assert_file_contains "$sandbox/arc/worker-policy.conf" '^resolved_worker_mode=full$' \
            "auto fallback ($case_name) did not persist resolved_worker_mode=full" || return 1
    done
    reset_shard_test_env
}

reclaimable_own_full_worker_rss_allows_migration_without_stopping_first() {
    local sandbox canonical_model
    reset_shard_test_env
    new_sandbox
    sandbox="$NEW_SANDBOX"
    canonical_model="$sandbox/canonical.gguf"
    write_canonical_model "$canonical_model"
    MOCK_HEALTH_PORT_UNDER_TEST=18444
    ARC_NODE_VERSION_UNDER_TEST=0.8.0
    invoke_shard_installer "$sandbox" Linux x86_64 \
        "$TEST_DIR/fixtures/release-v0.8.0.json" 0.8.0 \
        --user-service --port 18444 --model "$canonical_model" --worker-mode full >"$sandbox/reclaim-base.out" 2>&1 || {
            cat "$sandbox/reclaim-base.out"
            reset_shard_test_env
            return 1
        }

    printf 'MemTotal:       16777216 kB\nMemAvailable:    3145728 kB\n' >"$sandbox/own-worker-meminfo"
    ARC_INSTALL_MEMINFO_PATH_UNDER_TEST="$sandbox/own-worker-meminfo"
    MOCK_SYSTEMD_NODE_ACTIVE_UNDER_TEST=true
    MOCK_SYSTEMD_NODE_ENABLED_UNDER_TEST=true
    MOCK_SYSTEMD_MAIN_PID_UNDER_TEST=4242
    MOCK_PS_EXPECT_PID_UNDER_TEST=4242
    MOCK_PS_UID_UNDER_TEST=1000
    MOCK_PS_PID_RSS_UNDER_TEST=4404019
    MOCK_PS_COMMAND_UNDER_TEST="$sandbox/arc/bin/arc-node --rpc 127.0.0.1:18444 --data-dir $sandbox/arc/data --model $canonical_model --full-integer-worker"
    ARC_NODE_VERSION_UNDER_TEST=0.8.23
    invoke_shard_installer "$sandbox" Linux x86_64 \
        "$TEST_DIR/fixtures/release-v0.8.23.json" 0.8.23 \
        --user-service --port 18444 --model "$canonical_model" --worker-mode shard >"$sandbox/reclaimable.out" 2>&1 || {
            cat "$sandbox/reclaimable.out"
            reset_shard_test_env
            return 1
        }
    assert_file_contains "$sandbox/arc/worker-policy.conf" '^resolved_worker_mode=shard$' \
        'validated managed full-worker service RSS did not qualify host for shard migration' || return 1
    MOCK_INSTALL_SH_SOURCE_FILE_UNDER_TEST="$INSTALLER"
    MOCK_PS_COMMAND_UNDER_TEST="$sandbox/arc/bin/arc-node --rpc 127.0.0.1:18444 --data-dir $sandbox/arc/data --model $canonical_model --community-shard-worker"
    MOCK_PS_PID_RSS_UNDER_TEST=2097152
    printf 'MemTotal:       16777216 kB\nMemAvailable:    5242880 kB\n' >"$sandbox/own-worker-meminfo"
    local configured_mode
    for configured_mode in auto shard; do
        invoke_shard_installer "$sandbox" Linux x86_64 \
            "$TEST_DIR/fixtures/release-v0.8.23.json" 0.8.23 \
            --update-only --worker-mode "$configured_mode" >"$sandbox/partial-$configured_mode.out" 2>&1 || {
                cat "$sandbox/partial-$configured_mode.out"
                reset_shard_test_env
                return 1
            }
        assert_file_contains "$sandbox/arc/worker-policy.conf" '^resolved_worker_mode=shard$' \
            "managed partial-worker RSS was excluded during $configured_mode reconciliation" || return 1
    done
    printf 'MemTotal:       16777216 kB\nMemAvailable:       1024 kB\n' >"$sandbox/own-worker-meminfo"
    MOCK_PS_PID_RSS_UNDER_TEST=0
    : >"$sandbox/service.log"
    invoke_shard_installer "$sandbox" Linux x86_64 \
        "$TEST_DIR/fixtures/release-v0.8.23.json" 0.8.23 \
        --update-only >"$sandbox/partial-noop.out" 2>&1 || {
            cat "$sandbox/partial-noop.out"
            reset_shard_test_env
            return 1
        }
    assert_file_contains "$sandbox/partial-noop.out" 'Already up to date at v0\.8\.23' \
        'reconciled explicit shard no-op unnecessarily requalified memory' || return 1
    assert_file_not_contains "$sandbox/service.log" 'stop|restart|disable' \
        'reconciled explicit shard no-op touched the running worker' || return 1
    reset_shard_test_env
    assert_file_contains "$sandbox/arc/bin/run-arc-node" '--community-shard-worker' \
        'validated managed full-worker RSS migration did not switch runner to --community-shard-worker' || return 1
}

unmanaged_manual_and_foreign_worker_rss_is_never_credited() {
    local sandbox canonical_model case_name status
    for case_name in no-service-manual inactive-service-manual foreign-uid wrong-binary wrong-data-dir non-full-worker; do
        reset_shard_test_env
        new_sandbox
        sandbox="$NEW_SANDBOX"
        canonical_model="$sandbox/canonical.gguf"
        write_canonical_model "$canonical_model"
        printf 'MemTotal:       16777216 kB\nMemAvailable:    3145728 kB\n' >"$sandbox/pressured-meminfo"
        ARC_INSTALL_MEMINFO_PATH_UNDER_TEST="$sandbox/pressured-meminfo"
        MOCK_PS_RSS_ROWS_UNDER_TEST="9191 1000 4404019 $sandbox/arc/bin/arc-node --rpc 127.0.0.1:18444 --data-dir $sandbox/arc/data --model $canonical_model --full-integer-worker"
        MOCK_SYSTEMD_MAIN_PID_UNDER_TEST=4242
        MOCK_PS_EXPECT_PID_UNDER_TEST=4242
        MOCK_PS_UID_UNDER_TEST=1000
        MOCK_PS_PID_RSS_UNDER_TEST=4404019
        MOCK_PS_COMMAND_UNDER_TEST="$sandbox/arc/bin/arc-node --rpc 127.0.0.1:18444 --data-dir $sandbox/arc/data --model $canonical_model --full-integer-worker"
        MOCK_HEALTH_PORT_UNDER_TEST=18444
        ARC_NODE_VERSION_UNDER_TEST=0.8.23

        local scope_flag=--user-service
        MOCK_SYSTEMD_NODE_ACTIVE_UNDER_TEST=true
        MOCK_SYSTEMD_NODE_ENABLED_UNDER_TEST=true
        case "$case_name" in
            no-service-manual)
                scope_flag=--no-service
                ;;
            inactive-service-manual)
                MOCK_SYSTEMD_NODE_ACTIVE_UNDER_TEST=false
                ;;
            foreign-uid)
                MOCK_PS_UID_UNDER_TEST=9999
                ;;
            wrong-binary)
                MOCK_PS_COMMAND_UNDER_TEST="/usr/local/bin/arc-node --rpc 127.0.0.1:18444 --data-dir $sandbox/arc/data --model $canonical_model --full-integer-worker"
                ;;
            wrong-data-dir)
                MOCK_PS_COMMAND_UNDER_TEST="$sandbox/arc/bin/arc-node --rpc 127.0.0.1:18444 --data-dir $sandbox/foreign-data --model $canonical_model --full-integer-worker"
                ;;
            non-full-worker)
                MOCK_PS_COMMAND_UNDER_TEST="$sandbox/arc/bin/arc-node --rpc 127.0.0.1:18444 --data-dir $sandbox/arc/data --model $canonical_model --community-mode"
                ;;
        esac

        invoke_shard_installer "$sandbox" Linux x86_64 \
            "$TEST_DIR/fixtures/release-v0.8.23.json" 0.8.23 \
            "$scope_flag" --port 18444 --model "$canonical_model" --worker-mode shard >"$sandbox/reject-rss-$case_name.out" 2>&1
        status=$?
        [ "$status" -ne 0 ] || {
            printf 'unverified RSS case %s unexpectedly qualified explicit shard mode\n' "$case_name"
            reset_shard_test_env
            return 1
        }
        [ ! -s "$sandbox/preflight.log" ] || {
            printf 'unverified RSS case %s executed model preflight\n' "$case_name"
            reset_shard_test_env
            return 1
        }
        assert_file_not_contains "$sandbox/service.log" 'stop|restart|disable' \
            "unverified RSS rejection ($case_name) touched service state" || return 1

        invoke_shard_installer "$sandbox" Linux x86_64 \
            "$TEST_DIR/fixtures/release-v0.8.23.json" 0.8.23 \
            "$scope_flag" --port 18444 --model "$canonical_model" --worker-mode auto >"$sandbox/auto-rss-$case_name.out" 2>&1 || {
                cat "$sandbox/auto-rss-$case_name.out"
                reset_shard_test_env
                return 1
            }
        assert_file_contains "$sandbox/arc/worker-policy.conf" '^resolved_worker_mode=full$' \
            "unverified RSS case ($case_name) did not fall back to full in auto mode" || return 1
    done
    reset_shard_test_env
}

cli_subcommand_contract_binds_root_community_shard_worker_without_operator_prefix() {
    local sandbox canonical_model status
    python3 - "$INSTALLER" \
        "$REPO_ROOT/crates/arc-node/src/main.rs" \
        "$REPO_ROOT/crates/arc-node/src/community_shard_cli.rs" <<'PY'
import re
import sys

installer_path, main_rs_path, shard_cli_path = sys.argv[1:]
with open(installer_path, encoding="utf-8") as handle:
    installer = handle.read()
with open(main_rs_path, encoding="utf-8") as handle:
    main_rs = handle.read()
with open(shard_cli_path, encoding="utf-8") as handle:
    shard_cli = handle.read()

cli_struct = re.search(r"struct\s+Cli\s*\{(.*?)^\}", main_rs, re.MULTILINE | re.DOTALL)
if not cli_struct:
    raise SystemExit("could not locate struct Cli in main.rs")
if not re.search(
    r"#\[command\(\s*subcommand\s*\)\]\s*operator_command:\s*Option<OperatorCommand>",
    cli_struct.group(1),
):
    raise SystemExit("Cli.operator_command is no longer a flattened #[command(subcommand)]")

operator_enum = re.search(r"enum\s+OperatorCommand\s*\{(.*?)^\}", main_rs, re.MULTILINE | re.DOTALL)
if not operator_enum or "CommunityShardWorker(" not in operator_enum.group(1):
    raise SystemExit("OperatorCommand::CommunityShardWorker variant missing from main.rs")

if not re.search(
    r'fn\s+base_argv\(\).*?"arc-node"\.into\(\),\s*"community-shard-worker"\.into\(\)',
    shard_cli,
    re.DOTALL,
):
    raise SystemExit("community_shard_cli.rs base_argv no longer binds root community-shard-worker")
if 'wrong_prefix.insert(1, "operator".into())' not in shard_cli:
    raise SystemExit("community_shard_cli.rs no longer asserts rejection of operator prefix")

func_match = re.search(
    r"^qualify_shard_binary_check_only\(\) \{.*?^\}",
    installer,
    re.MULTILINE | re.DOTALL,
)
if not func_match:
    raise SystemExit("install.sh is missing qualify_shard_binary_check_only")
body = func_match.group(0)
if re.search(r'"\$node_binary"\s+operator\s+community-shard-worker\b', body):
    raise SystemExit("install.sh wrongly prefixes community-shard-worker with operator")
if not re.search(r'"\$node_binary"\s+community-shard-worker\b', body):
    raise SystemExit("install.sh does not invoke root subcommand community-shard-worker")
if not re.search(r'preflight_executor=\(\s*["\']?command["\']?\s*\)', body) or not re.search(
    r'preflight_executor=\(\s*["\']?as_target["\']?\s*\)', body
):
    raise SystemExit("qualify_shard_binary_check_only lost command vs as_target privilege split")
PY
    status=$?
    [ "$status" -eq 0 ] || return 1

    reset_shard_test_env
    new_sandbox
    sandbox="$NEW_SANDBOX"
    canonical_model="$sandbox/canonical.gguf"
    write_canonical_model "$canonical_model"
    ARC_NODE_VERSION_UNDER_TEST=0.8.23
    invoke_shard_installer "$sandbox" Linux x86_64 \
        "$TEST_DIR/fixtures/release-v0.8.23.json" 0.8.23 \
        --no-service --model "$canonical_model" --worker-mode shard >"$sandbox/cli-check.out" 2>&1 || {
            cat "$sandbox/cli-check.out"
            reset_shard_test_env
            return 1
        }
    reset_shard_test_env
    "$sandbox/arc/bin/arc-node" operator community-shard-worker --check-only >"$sandbox/bad-op.out" 2>&1
    status=$?
    [ "$status" -eq 2 ] || {
        printf 'mock arc-node did not reject bogus operator prefix with exit 2 (got %s)\n' "$status"
        return 1
    }
    assert_file_contains "$sandbox/bad-op.out" "unrecognized subcommand 'operator'" \
        'mock arc-node did not report clap unrecognized operator error' || return 1
}

explicit_shard_mode_succeeds_end_to_end_with_early_qualification_helpers() {
    local sandbox canonical_model output
    reset_shard_test_env
    new_sandbox
    sandbox="$NEW_SANDBOX"
    canonical_model="$sandbox/canonical.gguf"
    write_canonical_model "$canonical_model"
    MOCK_HEALTH_PORT_UNDER_TEST=18444
    ARC_NODE_VERSION_UNDER_TEST=0.8.23
    output="$sandbox/explicit-shard-ok.out"
    invoke_shard_installer "$sandbox" Linux x86_64 \
        "$TEST_DIR/fixtures/release-v0.8.23.json" 0.8.23 \
        --user-service --port 18444 --model "$canonical_model" --worker-mode shard >"$output" 2>&1 || {
            cat "$output"
            reset_shard_test_env
            return 1
        }
    assert_file_not_contains "$output" 'command not found' \
        'explicit --worker-mode shard hit an undefined shell helper' || {
            reset_shard_test_env
            return 1
        }
    assert_file_contains "$sandbox/preflight.log" 'community-shard-worker .*--max-working-set-bytes 7462505472 --max-resident-layers 6 --threads 1 --check-only' \
        'explicit --worker-mode shard did not reach binary --check-only preflight' || {
            reset_shard_test_env
            return 1
        }
    assert_file_contains "$sandbox/arc/worker-policy.conf" '^configured_worker_mode=shard$' \
        'explicit shard install did not record configured_worker_mode=shard' || {
            reset_shard_test_env
            return 1
        }
    assert_file_contains "$sandbox/arc/worker-policy.conf" '^resolved_worker_mode=shard$' \
        'explicit shard install did not record resolved_worker_mode=shard' || {
            reset_shard_test_env
            return 1
        }
    assert_file_contains "$sandbox/arc/bin/run-arc-node" '--community-shard-worker' \
        'explicit shard install did not configure --community-shard-worker' || {
            reset_shard_test_env
            return 1
        }

    : >"$sandbox/preflight.log"
    ARC_WORKER_MODE_UNDER_TEST=shard
    output="$sandbox/env-shard-ok.out"
    invoke_shard_installer "$sandbox" Linux x86_64 \
        "$TEST_DIR/fixtures/release-v0.8.23.json" 0.8.23 \
        --no-service --model "$canonical_model" >"$output" 2>&1 || {
            cat "$output"
            reset_shard_test_env
            return 1
        }
    reset_shard_test_env
    assert_file_not_contains "$output" 'command not found' \
        'ARC_WORKER_MODE=shard hit an undefined shell helper' || return 1
    [ -s "$sandbox/preflight.log" ] || {
        printf 'ARC_WORKER_MODE=shard did not execute check-only preflight\n'
        return 1
    }
}

new_system_sandbox() {
    local sys_base sandbox mock_bin command_name
    case "${HOME:-}" in
        ''|/tmp|/tmp/*|/var/tmp|/var/tmp/*) sys_base="$REPO_ROOT" ;;
        *) sys_base="$HOME" ;;
    esac
    sandbox="$(mktemp -d "$sys_base/.arc-shard-sys.XXXXXX")"
    sandbox="$(CDPATH='' cd -- "$sandbox" && pwd -P)"
    ACTIVE_SANDBOXES="$ACTIVE_SANDBOXES $sandbox"
    mkdir -p "$sandbox/home" "$sandbox/bin" "$sandbox/tmp" "$sandbox/systemd-system" "$sandbox/system-root"
    chmod 755 "$sandbox" "$sandbox/systemd-system" "$sandbox/system-root"
    : >"$sandbox/curl.log"
    : >"$sandbox/node-args.log"
    : >"$sandbox/preflight.log"
    : >"$sandbox/as-target.log"
    : >"$sandbox/service.log"
    : >"$sandbox/owner.log"
    printf 'MemTotal:       16777216 kB\nMemAvailable:   10485760 kB\n' >"$sandbox/proc-meminfo"
    sed "s#^SYSTEMD_UNIT_DIR=/etc/systemd/system\$#SYSTEMD_UNIT_DIR=$sandbox/systemd-system#" \
        "$INSTALLER" >"$sandbox/install-sys.sh"
    chmod 700 "$sandbox/install-sys.sh"
    mock_bin="$sandbox/bin"
    cp "$TEST_DIR/helpers/mock-curl.sh" "$mock_bin/curl"
    cp "$TEST_DIR/helpers/mock-platform-command.sh" "$mock_bin/platform-command"
    chmod +x "$mock_bin/curl" "$mock_bin/platform-command"
    for command_name in uname sleep free sysctl vm_stat openssl ssh-keygen hostname id getent chown stat runuser sudo systemctl launchctl ps; do
        ln -s platform-command "$mock_bin/$command_name"
    done
    NEW_SANDBOX="$sandbox"
}

configure_root_to_nonroot_system_env() {
    local sandbox="$1"
    INSTALLER_OVERRIDE_UNDER_TEST="$sandbox/install-sys.sh"
    MOCK_INSTALL_SH_SOURCE_FILE_UNDER_TEST="$sandbox/install-sys.sh"
    ARC_DIR_UNDER_TEST="$sandbox/system-root/arc-chain"
    MOCK_CURRENT_UID_UNDER_TEST=0
    MOCK_CURRENT_USER_UNDER_TEST=root
    SUDO_USER_UNDER_TEST=arc-community-test
    MOCK_TARGET_USER_UNDER_TEST=arc-community-test
    MOCK_TARGET_UID_UNDER_TEST=1000
    MOCK_TARGET_GROUP_UNDER_TEST=arc-community-test
    MOCK_INSTALL_MARKER_UID_UNDER_TEST=0
    MOCK_WORKER_POLICY_UID_UNDER_TEST=0
    MOCK_ROOT_OWNED_PREFIX_UNDER_TEST="$sandbox"
    MOCK_ROOT_OWNED_ANCESTORS_UNDER_TEST=true
    MOCK_SUDO_EXECUTE_UNDER_TEST=true
    MOCK_HEALTH_PORT_UNDER_TEST=18444
}

root_to_nonroot_preflight_checks_model_as_target_and_runs_staged_binary_as_installer() {
    local sandbox arc_dir data_dir canonical_model root_only_model symlink_model status preflight_lines
    reset_shard_test_env
    new_system_sandbox
    sandbox="$NEW_SANDBOX"
    arc_dir="$sandbox/system-root/arc-chain"
    data_dir="$sandbox/system-root/arc-data"
    canonical_model="$sandbox/canonical.gguf"
    root_only_model="$sandbox/root-only.gguf"
    symlink_model="$sandbox/symlink-model.gguf"
    write_canonical_model "$canonical_model"
    write_canonical_model "$root_only_model"
    ln -s "$canonical_model" "$symlink_model"

    configure_root_to_nonroot_system_env "$sandbox"
    MOCK_TARGET_UNREADABLE_PATH_UNDER_TEST="$root_only_model"
    ARC_NODE_VERSION_UNDER_TEST=0.8.23
    invoke_shard_installer "$sandbox" Linux x86_64 \
        "$TEST_DIR/fixtures/release-v0.8.23.json" 0.8.23 \
        --system-service --install-dir "$arc_dir" --data-dir "$data_dir" \
        --port 18444 --model "$root_only_model" --worker-mode shard >"$sandbox/root-unreadable.out" 2>&1
    status=$?
    [ "$status" -ne 0 ] || {
        printf 'root->nonroot install accepted a model unreadable by TARGET_USER\n'
        reset_shard_test_env
        return 1
    }
    [ ! -s "$sandbox/preflight.log" ] || {
        printf 'root->nonroot install executed preflight despite target-unreadable model\n'
        reset_shard_test_env
        return 1
    }

    MOCK_TARGET_UNREADABLE_PATH_UNDER_TEST=""
    : >"$sandbox/preflight.log"
    invoke_shard_installer "$sandbox" Linux x86_64 \
        "$TEST_DIR/fixtures/release-v0.8.23.json" 0.8.23 \
        --system-service --install-dir "$arc_dir" --data-dir "$data_dir" \
        --port 18444 --model "$symlink_model" --worker-mode shard >"$sandbox/root-symlink.out" 2>&1
    status=$?
    [ "$status" -ne 0 ] || {
        printf 'root->nonroot explicit shard install accepted symlinked model path\n'
        reset_shard_test_env
        return 1
    }
    [ ! -s "$sandbox/preflight.log" ] || {
        printf 'symlinked model reached check-only binary execution\n'
        reset_shard_test_env
        return 1
    }

    : >"$sandbox/preflight.log"
    : >"$sandbox/as-target.log"
    printf 'MemTotal:       16777216 kB\nMemAvailable:    2097152 kB\n' >"$sandbox/pressured-meminfo"
    ARC_INSTALL_MEMINFO_PATH_UNDER_TEST="$sandbox/pressured-meminfo"
    invoke_shard_installer "$sandbox" Linux x86_64 \
        "$TEST_DIR/fixtures/release-v0.8.23.json" 0.8.23 \
        --system-service --install-dir "$arc_dir" --data-dir "$data_dir" \
        --port 18444 --model "$canonical_model" --worker-mode auto >"$sandbox/root-auto-pressured.out" 2>&1 || {
            cat "$sandbox/root-auto-pressured.out"
            reset_shard_test_env
            return 1
        }
    assert_file_contains "$arc_dir/worker-policy.conf" '^resolved_worker_mode=full$' \
        'pressured root->nonroot auto install did not fall back to full' || {
            reset_shard_test_env
            return 1
        }

    ARC_INSTALL_MEMINFO_PATH_UNDER_TEST="$sandbox/proc-meminfo"
    INSTALLER_OVERRIDE_UNDER_TEST="$arc_dir/bin/arc-installer"
    ARC_TARGET_USER_UNDER_TEST=arc-community-test
    SUDO_USER_UNDER_TEST=""
    ARC_NODE_VERSION_UNDER_TEST=""
    MOCK_SYSTEMD_NODE_ACTIVE_UNDER_TEST=true
    MOCK_SYSTEMD_NODE_ENABLED_UNDER_TEST=true
    MOCK_SYSTEMD_UPDATER_ACTIVE_UNDER_TEST=true
    MOCK_SYSTEMD_UPDATER_ENABLED_UNDER_TEST=true
    : >"$sandbox/preflight.log"
    : >"$sandbox/as-target.log"
    invoke_shard_installer "$sandbox" Linux x86_64 \
        "$TEST_DIR/fixtures/release-v0.8.23.json" 0.8.23 \
        --update-only --install-dir "$arc_dir" --service-scope system >"$sandbox/root-reassess.out" 2>&1 || {
            cat "$sandbox/root-reassess.out"
            reset_shard_test_env
            return 1
        }
    reset_shard_test_env

    assert_file_not_contains "$sandbox/root-reassess.out" 'permission denied traversing 0700 root TMP_DIR' \
        'root->nonroot preflight tried to traverse 0700 root TMP_DIR as target user' || return 1
    assert_file_not_contains "$sandbox/as-target.log" '/arc-install\.' \
        'as_target was invoked on a private root-owned TMP_DIR path' || return 1
    assert_file_contains "$sandbox/as-target.log" "runuser arc-community-test test -r $canonical_model" \
        'root->nonroot preflight did not verify model readability as TARGET_USER' || return 1
    preflight_lines="$(wc -l < "$sandbox/preflight.log" | tr -d ' ')"
    assert_equals '2' "$preflight_lines" \
        'same-version root->nonroot promotion did not run both installed-binary reassessment and staged-binary preflight' || return 1
    assert_equals '1' "$(sed -n '1p' "$sandbox/preflight.log" | grep -Ec "^exec_user=arc-community-test bin=$arc_dir/bin/arc-node community-shard-worker ")" \
        'installed-binary reassessment did not run as TARGET_USER (as_target)' || return 1
    assert_equals '1' "$(sed -n '2p' "$sandbox/preflight.log" | grep -Ec '^exec_user=root bin=.*/arc-install\.[^ ]*/arc-node-linux-x86_64 community-shard-worker ')" \
        'staged-binary preflight did not run as installer user (command) from private TMP_DIR' || return 1
    assert_file_contains "$arc_dir/worker-policy.conf" '^resolved_worker_mode=shard$' \
        'root->nonroot same-version reassessment did not promote resolved_worker_mode to shard' || return 1
}

same_version_auto_full_fallback_noops_when_unchanged_and_promotes_on_recovery() {
    local sandbox canonical_model model_slot policy_sha runner_sha status
    reset_shard_test_env
    new_sandbox
    sandbox="$NEW_SANDBOX"
    canonical_model="$sandbox/canonical.gguf"
    write_canonical_model "$canonical_model"
    printf 'MemTotal:       16777216 kB\nMemAvailable:    2097152 kB\n' >"$sandbox/pressured-meminfo"
    ARC_INSTALL_MEMINFO_PATH_UNDER_TEST="$sandbox/pressured-meminfo"
    MOCK_INSTALL_SH_SOURCE_FILE_UNDER_TEST="$INSTALLER"
    MOCK_HEALTH_PORT_UNDER_TEST=18444
    ARC_NODE_VERSION_UNDER_TEST=0.8.23
    invoke_shard_installer "$sandbox" Linux x86_64 \
        "$TEST_DIR/fixtures/release-v0.8.23.json" 0.8.23 \
        --user-service --port 18444 --model "$canonical_model" --worker-mode auto >"$sandbox/p-init.out" 2>&1 || {
            cat "$sandbox/p-init.out"
            reset_shard_test_env
            return 1
        }
    assert_file_contains "$sandbox/arc/worker-policy.conf" '^configured_worker_mode=auto$' \
        'initial pressured install did not persist configured_worker_mode=auto' || return 1
    assert_file_contains "$sandbox/arc/worker-policy.conf" '^resolved_worker_mode=full$' \
        'initial pressured install did not fall back to resolved_worker_mode=full' || return 1
    policy_sha="$(file_sha256 "$sandbox/arc/worker-policy.conf")"
    runner_sha="$(file_sha256 "$sandbox/arc/bin/run-arc-node")"

    INSTALLER_OVERRIDE_UNDER_TEST="$sandbox/arc/bin/arc-installer"
    ARC_NODE_VERSION_UNDER_TEST=""
    MOCK_SYSTEMD_NODE_ACTIVE_UNDER_TEST=true
    MOCK_SYSTEMD_NODE_ENABLED_UNDER_TEST=true
    MOCK_SYSTEMD_UPDATER_ACTIVE_UNDER_TEST=true
    MOCK_SYSTEMD_UPDATER_ENABLED_UNDER_TEST=true
    MOCK_SYSTEMD_MAIN_PID_UNDER_TEST=4242
    MOCK_PS_EXPECT_PID_UNDER_TEST=4242
    MOCK_PS_UID_UNDER_TEST=1000
    MOCK_PS_PID_RSS_UNDER_TEST=2097152
    MOCK_PS_COMMAND_UNDER_TEST="$sandbox/arc/bin/arc-node --rpc 127.0.0.1:18444 --data-dir $sandbox/arc/data --model $canonical_model --full-integer-worker"
    : >"$sandbox/curl.log"
    : >"$sandbox/service.log"
    invoke_shard_installer "$sandbox" Linux x86_64 \
        "$TEST_DIR/fixtures/release-v0.8.23.json" 0.8.23 \
        --update-only >"$sandbox/p-still-pressured.out" 2>&1 || {
            cat "$sandbox/p-still-pressured.out"
            reset_shard_test_env
            return 1
        }
    assert_file_contains "$sandbox/p-still-pressured.out" 'Already up to date at v0\.8\.23' \
        'unchanged memory-pressure fallback did not no-op on same-version update-only' || return 1
    assert_file_not_contains "$sandbox/curl.log" '/releases/download/' \
        'unchanged memory-pressure fallback downloaded release assets' || return 1
    assert_file_not_contains "$sandbox/service.log" 'restart arc-node\.service' \
        'unchanged memory-pressure fallback restarted arc-node.service' || return 1
    assert_equals "$policy_sha" "$(file_sha256 "$sandbox/arc/worker-policy.conf")" \
        'unchanged memory-pressure fallback rewrote worker-policy.conf' || return 1
    assert_equals "$runner_sha" "$(file_sha256 "$sandbox/arc/bin/run-arc-node")" \
        'unchanged memory-pressure fallback rewrote run-arc-node' || return 1

    ARC_INSTALL_MEMINFO_PATH_UNDER_TEST="$sandbox/proc-meminfo"
    MOCK_TAMPER_MANIFEST_SIGNATURE_UNDER_TEST=1
    : >"$sandbox/service.log"
    invoke_shard_installer "$sandbox" Linux x86_64 \
        "$TEST_DIR/fixtures/release-v0.8.23.json" 0.8.23 \
        --update-only >"$sandbox/p-tampered-sig.out" 2>&1
    status=$?
    [ "$status" -ne 0 ] || {
        printf 'same-version auto->shard promotion bypassed manifest signature verification\n'
        reset_shard_test_env
        return 1
    }
    assert_file_contains "$sandbox/p-tampered-sig.out" 'Release SHA256SUMS signature is invalid' \
        'tampered signature during same-version promotion did not fail at signature check' || return 1
    assert_equals "$policy_sha" "$(file_sha256 "$sandbox/arc/worker-policy.conf")" \
        'tampered signature during same-version promotion modified worker-policy.conf' || return 1
    assert_equals "$runner_sha" "$(file_sha256 "$sandbox/arc/bin/run-arc-node")" \
        'tampered signature during same-version promotion modified run-arc-node' || return 1
    assert_file_not_contains "$sandbox/service.log" 'restart arc-node\.service' \
        'tampered signature during same-version promotion restarted arc-node.service' || return 1

    MOCK_TAMPER_MANIFEST_SIGNATURE_UNDER_TEST=0
    : >"$sandbox/service.log"
    invoke_shard_installer "$sandbox" Linux x86_64 \
        "$TEST_DIR/fixtures/release-v0.8.23.json" 0.8.23 \
        --update-only >"$sandbox/p-promote.out" 2>&1 || {
            cat "$sandbox/p-promote.out"
            reset_shard_test_env
            return 1
        }
    assert_file_contains "$sandbox/p-promote.out" 'Reconciling headless worker launcher policy at v0\.8\.23' \
        'cleared memory pressure did not trigger same-version policy reconciliation' || return 1
    assert_file_contains "$sandbox/arc/worker-policy.conf" '^resolved_worker_mode=shard$' \
        'cleared memory pressure did not promote resolved_worker_mode to shard' || return 1
    assert_file_contains "$sandbox/arc/bin/run-arc-node" '--community-shard-worker' \
        'cleared memory pressure did not switch runner to --community-shard-worker' || return 1

    : >"$sandbox/curl.log"
    : >"$sandbox/service.log"
    invoke_shard_installer "$sandbox" Linux x86_64 \
        "$TEST_DIR/fixtures/release-v0.8.23.json" 0.8.23 \
        --update-only >"$sandbox/p-post-noop.out" 2>&1 || {
            cat "$sandbox/p-post-noop.out"
            reset_shard_test_env
            return 1
        }
    assert_file_contains "$sandbox/p-post-noop.out" 'Already up to date at v0\.8\.23' \
        'promoted shard install did not no-op on subsequent same-version update-only' || return 1
    assert_file_not_contains "$sandbox/curl.log" '/releases/download/' \
        'promoted shard install re-downloaded assets on subsequent cycle' || return 1
    assert_file_not_contains "$sandbox/service.log" 'restart arc-node\.service' \
        'promoted shard install restarted service on subsequent cycle' || return 1

    reset_shard_test_env
    new_sandbox
    sandbox="$NEW_SANDBOX"
    model_slot="$sandbox/model-slot.gguf"
    printf 'temporary-noncanonical-weights\n' >"$model_slot"
    MOCK_INSTALL_SH_SOURCE_FILE_UNDER_TEST="$INSTALLER"
    MOCK_HEALTH_PORT_UNDER_TEST=18444
    ARC_NODE_VERSION_UNDER_TEST=0.8.23
    invoke_shard_installer "$sandbox" Linux x86_64 \
        "$TEST_DIR/fixtures/release-v0.8.23.json" 0.8.23 \
        --user-service --port 18444 --model "$model_slot" --worker-mode auto >"$sandbox/m-init.out" 2>&1 || {
            cat "$sandbox/m-init.out"
            reset_shard_test_env
            return 1
        }
    assert_file_contains "$sandbox/arc/worker-policy.conf" '^resolved_worker_mode=full$' \
        'noncanonical model auto install did not fall back to full' || return 1
    policy_sha="$(file_sha256 "$sandbox/arc/worker-policy.conf")"

    INSTALLER_OVERRIDE_UNDER_TEST="$sandbox/arc/bin/arc-installer"
    ARC_NODE_VERSION_UNDER_TEST=""
    MOCK_SYSTEMD_NODE_ACTIVE_UNDER_TEST=true
    MOCK_SYSTEMD_NODE_ENABLED_UNDER_TEST=true
    MOCK_SYSTEMD_UPDATER_ACTIVE_UNDER_TEST=true
    MOCK_SYSTEMD_UPDATER_ENABLED_UNDER_TEST=true
    : >"$sandbox/curl.log"
    : >"$sandbox/service.log"
    invoke_shard_installer "$sandbox" Linux x86_64 \
        "$TEST_DIR/fixtures/release-v0.8.23.json" 0.8.23 \
        --update-only >"$sandbox/m-unchanged.out" 2>&1 || {
            cat "$sandbox/m-unchanged.out"
            reset_shard_test_env
            return 1
        }
    assert_file_contains "$sandbox/m-unchanged.out" 'Already up to date at v0\.8\.23' \
        'unchanged noncanonical model fallback did not no-op' || return 1
    assert_file_not_contains "$sandbox/curl.log" '/releases/download/' \
        'unchanged noncanonical model fallback downloaded release assets' || return 1
    assert_file_not_contains "$sandbox/service.log" 'restart arc-node\.service' \
        'unchanged noncanonical model fallback restarted arc-node.service' || return 1
    assert_equals "$policy_sha" "$(file_sha256 "$sandbox/arc/worker-policy.conf")" \
        'unchanged noncanonical model fallback rewrote worker-policy.conf' || return 1

    write_canonical_model "$model_slot"
    invoke_shard_installer "$sandbox" Linux x86_64 \
        "$TEST_DIR/fixtures/release-v0.8.23.json" 0.8.23 \
        --update-only >"$sandbox/m-restored.out" 2>&1 || {
            cat "$sandbox/m-restored.out"
            reset_shard_test_env
            return 1
        }
    reset_shard_test_env
    assert_file_contains "$sandbox/arc/worker-policy.conf" '^resolved_worker_mode=shard$' \
        'restored canonical model did not promote auto/full to shard at same version' || return 1
    assert_file_contains "$sandbox/arc/bin/run-arc-node" '--community-shard-worker' \
        'restored canonical model did not switch runner to --community-shard-worker' || return 1
}

macos_vm_stat_does_not_double_count_overlapping_purgeable_pages() {
    local sandbox canonical_model status
    reset_shard_test_env
    new_sandbox
    sandbox="$NEW_SANDBOX"
    canonical_model="$sandbox/canonical.gguf"
    write_canonical_model "$canonical_model"
    MOCK_SYSCTL_MEMSIZE_UNDER_TEST=17179869184
    MOCK_VM_STAT_PAGE_SIZE_UNDER_TEST=16384
    MOCK_VM_STAT_FREE_PAGES_UNDER_TEST=131072
    MOCK_VM_STAT_ACTIVE_PAGES_UNDER_TEST=524288
    MOCK_VM_STAT_INACTIVE_PAGES_UNDER_TEST=131072
    MOCK_VM_STAT_SPECULATIVE_PAGES_UNDER_TEST=65536
    MOCK_VM_STAT_PURGEABLE_PAGES_UNDER_TEST=196608
    ARC_NODE_VERSION_UNDER_TEST=0.8.23

    invoke_shard_installer "$sandbox" Darwin arm64 \
        "$TEST_DIR/fixtures/release-v0.8.23.json" 0.8.23 \
        --no-service --model "$canonical_model" --worker-mode shard >"$sandbox/mac-overlap-explicit.out" 2>&1
    status=$?
    [ "$status" -ne 0 ] || {
        printf 'macOS vm_stat double-counted overlapping purgeable pages to pass 6 GiB floor\n'
        reset_shard_test_env
        return 1
    }
    [ ! -s "$sandbox/preflight.log" ] || {
        printf 'macOS overlapping purgeable explicit shard failure executed model preflight\n'
        reset_shard_test_env
        return 1
    }
    assert_file_contains "$sandbox/mac-overlap-explicit.out" 'effective available RAM 5368709120 bytes is below the 6442450944-byte migration floor' \
        'macOS vm_stat did not compute exact 5 GiB disjoint free+inactive+speculative bytes' || {
            reset_shard_test_env
            return 1
        }

    invoke_shard_installer "$sandbox" Darwin arm64 \
        "$TEST_DIR/fixtures/release-v0.8.23.json" 0.8.23 \
        --no-service --model "$canonical_model" --worker-mode auto >"$sandbox/mac-overlap-auto.out" 2>&1 || {
            cat "$sandbox/mac-overlap-auto.out"
            reset_shard_test_env
            return 1
        }
    assert_file_contains "$sandbox/arc/worker-policy.conf" '^resolved_worker_mode=full$' \
        'macOS overlapping purgeable auto mode did not fall back to full' || {
            reset_shard_test_env
            return 1
        }

    MOCK_VM_STAT_FREE_PAGES_UNDER_TEST=196608
    MOCK_VM_STAT_PURGEABLE_PAGES_UNDER_TEST=0
    invoke_shard_installer "$sandbox" Darwin arm64 \
        "$TEST_DIR/fixtures/release-v0.8.23.json" 0.8.23 \
        --no-service --model "$canonical_model" --worker-mode shard >"$sandbox/mac-disjoint-ok.out" 2>&1 || {
            cat "$sandbox/mac-disjoint-ok.out"
            reset_shard_test_env
            return 1
        }
    reset_shard_test_env
    assert_file_contains "$sandbox/arc/worker-policy.conf" '^resolved_worker_mode=shard$' \
        'macOS disjoint 6 GiB free+inactive+speculative did not qualify explicit shard mode' || return 1
}

malformed_and_hostile_worker_policy_files_fail_before_network() {
    local sandbox canonical_model case_name status
    for case_name in bad-header unknown-key invalid-mode missing-key symlink group-writable wrong-owner; do
        reset_shard_test_env
        new_sandbox
        sandbox="$NEW_SANDBOX"
        canonical_model="$sandbox/canonical.gguf"
        write_canonical_model "$canonical_model"
        ARC_NODE_VERSION_UNDER_TEST=0.8.0
        invoke_shard_installer "$sandbox" Linux x86_64 \
            "$TEST_DIR/fixtures/release-v0.8.0.json" 0.8.0 \
            --no-service --model "$canonical_model" >"$sandbox/base-policy.out" 2>&1 || return 1
        : >"$sandbox/curl.log"
        case "$case_name" in
            bad-header)
                sed -i.bak '1s/.*/# wrong header/' "$sandbox/arc/worker-policy.conf"
                rm -f "$sandbox/arc/worker-policy.conf.bak"
                ;;
            unknown-key)
                printf 'unexpected_key=1\n' >>"$sandbox/arc/worker-policy.conf"
                ;;
            invalid-mode)
                sed -i.bak 's/^configured_worker_mode=.*/configured_worker_mode=bogus/' "$sandbox/arc/worker-policy.conf"
                rm -f "$sandbox/arc/worker-policy.conf.bak"
                ;;
            missing-key)
                grep -v '^resolved_worker_mode=' "$sandbox/arc/worker-policy.conf" >"$sandbox/policy.tmp"
                mv "$sandbox/policy.tmp" "$sandbox/arc/worker-policy.conf"
                ;;
            symlink)
                mv "$sandbox/arc/worker-policy.conf" "$sandbox/outside-policy.conf"
                ln -s "$sandbox/outside-policy.conf" "$sandbox/arc/worker-policy.conf"
                ;;
            group-writable)
                chmod 660 "$sandbox/arc/worker-policy.conf"
                ;;
            wrong-owner)
                MOCK_WORKER_POLICY_UID_UNDER_TEST=9999
                ;;
        esac
        ARC_NODE_VERSION_UNDER_TEST=0.8.23
        invoke_shard_installer "$sandbox" Linux x86_64 \
            "$TEST_DIR/fixtures/release-v0.8.23.json" 0.8.23 \
            --no-service --update-only >"$sandbox/reject-$case_name.out" 2>&1
        status=$?
        [ "$status" -ne 0 ] || {
            printf 'installer accepted hostile worker-policy.conf case: %s\n' "$case_name"
            reset_shard_test_env
            return 1
        }
        [ ! -s "$sandbox/curl.log" ] || {
            printf 'hostile worker-policy.conf (%s) reached network downloads\n' "$case_name"
            reset_shard_test_env
            return 1
        }
    done
    reset_shard_test_env
}

transaction_rollback_restores_or_removes_worker_policy_atomically() {
    local sandbox canonical_model policy_before runner_before status
    reset_shard_test_env
    new_sandbox
    sandbox="$NEW_SANDBOX"
    canonical_model="$sandbox/canonical.gguf"
    write_canonical_model "$canonical_model"
    MOCK_HEALTH_PORT_UNDER_TEST=18444
    ARC_NODE_VERSION_UNDER_TEST=0.8.0
    invoke_shard_installer "$sandbox" Linux x86_64 \
        "$TEST_DIR/fixtures/release-v0.8.0.json" 0.8.0 \
        --user-service --port 18444 --model "$canonical_model" --worker-mode full >"$sandbox/rb-base.out" 2>&1 || return 1

    policy_before="$(file_sha256 "$sandbox/arc/worker-policy.conf")"
    runner_before="$(file_sha256 "$sandbox/arc/bin/run-arc-node")"
    MOCK_SYSTEMD_NODE_ACTIVE_UNDER_TEST=true
    MOCK_SYSTEMD_NODE_ENABLED_UNDER_TEST=true
    MOCK_HEALTH_STATUS_UNDER_TEST=starting
    ARC_NODE_VERSION_UNDER_TEST=0.8.23
    invoke_shard_installer "$sandbox" Linux x86_64 \
        "$TEST_DIR/fixtures/release-v0.8.23.json" 0.8.23 \
        --user-service --port 18444 --model "$canonical_model" --worker-mode shard >"$sandbox/rb-fail1.out" 2>&1
    status=$?
    [ "$status" -ne 0 ] || return 1
    assert_equals "$policy_before" "$(file_sha256 "$sandbox/arc/worker-policy.conf")" \
        'failed health check did not restore prior worker-policy.conf' || return 1
    assert_equals "$runner_before" "$(file_sha256 "$sandbox/arc/bin/run-arc-node")" \
        'failed health check did not restore prior run-arc-node' || return 1

    rm -f "$sandbox/arc/worker-policy.conf"
    invoke_shard_installer "$sandbox" Linux x86_64 \
        "$TEST_DIR/fixtures/release-v0.8.23.json" 0.8.23 \
        --user-service --port 18444 --model "$canonical_model" --worker-mode shard >"$sandbox/rb-fail2.out" 2>&1
    status=$?
    reset_shard_test_env
    [ "$status" -ne 0 ] || return 1
    [ ! -e "$sandbox/arc/worker-policy.conf" ] || {
        printf 'rollback did not remove newly introduced worker-policy.conf\n'
        return 1
    }
    assert_equals "$runner_before" "$(file_sha256 "$sandbox/arc/bin/run-arc-node")" \
        'rollback from absent worker-policy.conf did not restore prior run-arc-node' || return 1
}

run_test 'desktop release pins and nominal_ram_gb boundaries match install.sh' desktop_pin_and_nominal_ram_parity_contract
run_test 'root CLI subcommand and preflight privilege contract matches arc-node source' cli_subcommand_contract_binds_root_community_shard_worker_without_operator_prefix
run_test 'explicit shard mode succeeds end-to-end with early qualification helpers defined' explicit_shard_mode_succeeds_end_to_end_with_early_qualification_helpers
run_test 'three-cycle updater migrates pre-shard whole worker on cycle 2 and no-ops on cycle 3' three_cycle_updater_reconciles_legacy_whole_worker_and_stamps_running_installer
run_test 'version upgrade stamps running installer SHA-256 instead of replacement asset' upgrade_stamps_running_installer_not_replacement_until_next_cycle
run_test 'observer and explicit full worker modes persist across updates' observer_and_explicit_full_modes_persist_across_updates
run_test 'ineligible model/RAM/pressure/layers fall back in auto and fail before stop in explicit shard' ineligible_preflight_and_memory_auto_fallback_vs_explicit_shard_fail_closed
run_test 'validated managed full-worker service RSS qualifies migration without stopping old node first' reclaimable_own_full_worker_rss_allows_migration_without_stopping_first
run_test 'unmanaged, manual, inactive, wrong-UID, wrong-binary, and wrong-data-dir RSS is never credited' unmanaged_manual_and_foreign_worker_rss_is_never_credited
run_test 'root->nonroot preflight checks model as target user and runs staged vs installed binary with split privilege' root_to_nonroot_preflight_checks_model_as_target_and_runs_staged_binary_as_installer
run_test 'same-version auto/full fallback no-ops when unchanged and promotes with signature verification on recovery' same_version_auto_full_fallback_noops_when_unchanged_and_promotes_on_recovery
run_test 'macOS vm_stat ignores overlapping purgeable pages and counts disjoint free/inactive/speculative pages' macos_vm_stat_does_not_double_count_overlapping_purgeable_pages
run_test 'malformed and hostile worker-policy.conf files fail closed before network' malformed_and_hostile_worker_policy_files_fail_before_network
run_test 'transaction rollback restores or removes worker-policy.conf atomically' transaction_rollback_restores_or_removes_worker_policy_atomically

finish_tests
