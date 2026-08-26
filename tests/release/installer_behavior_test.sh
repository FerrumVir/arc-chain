#!/usr/bin/env bash
set -uo pipefail

TEST_DIR="$(CDPATH='' cd -- "$(dirname -- "$0")" && pwd)"
REPO_ROOT="$(CDPATH='' cd -- "$TEST_DIR/../.." && pwd)"
# shellcheck source=/dev/null
. "$TEST_DIR/helpers/testlib.sh"

INSTALLER="$REPO_ROOT/scripts/install-community-node.sh"
LEGACY_INSTALLER="$REPO_ROOT/scripts/install-node.sh"
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
    printf '%s/SHA256SUMS %s/install.sh %s/testnet-seeds.txt %s/genesis.toml\n' \
        "$version" "$version" "$version" "$version"
}

new_sandbox() {
    local sandbox mock_bin command_name
    sandbox="$(mktemp -d "${TMPDIR:-/tmp}/arc-installer-contract.XXXXXX")"
    ACTIVE_SANDBOXES="$ACTIVE_SANDBOXES $sandbox"
    mkdir -p "$sandbox/home" "$sandbox/arc" "$sandbox/bin" "$sandbox/tmp"
    : >"$sandbox/curl.log"
    : >"$sandbox/node-args.log"
    : >"$sandbox/service.log"
    : >"$sandbox/owner.log"

    mock_bin="$sandbox/bin"
    cp "$TEST_DIR/helpers/mock-curl.sh" "$mock_bin/curl"
    cp "$TEST_DIR/helpers/mock-platform-command.sh" "$mock_bin/platform-command"
    chmod +x "$mock_bin/curl" "$mock_bin/platform-command"
    for command_name in uname sleep free sysctl openssl hostname id getent chown sudo systemctl launchctl; do
        ln -s platform-command "$mock_bin/$command_name"
    done
    NEW_SANDBOX="$sandbox"
}

invoke_installer() {
    local sandbox="$1" os="$2" arch="$3" fixture="$4" version="$5"
    shift 5
    env -i \
        PATH="$sandbox/bin:/usr/bin:/bin:/usr/sbin:/sbin" \
        HOME="$sandbox/home" \
        USER='arc-community-test' \
        SUDO_USER="${SUDO_USER_UNDER_TEST:-}" \
        ARC_DIR="$sandbox/arc" \
        ARC_NODE_VERSION="${ARC_NODE_VERSION_UNDER_TEST:-}" \
        TEST_UNAME_S="$os" \
        TEST_UNAME_M="$arch" \
        MOCK_RELEASE_FILE="$fixture" \
        MOCK_AVAILABLE_ASSETS="$(canonical_pairs_for_version "$version")" \
        MOCK_CHECKSUM_ASSETS="$CANONICAL_ASSETS testnet-seeds.txt genesis.toml install.sh" \
        MOCK_CURL_LOG="$sandbox/curl.log" \
        MOCK_SERVICE_LOG="$sandbox/service.log" \
        MOCK_OWNER_LOG="$sandbox/owner.log" \
        MOCK_CURRENT_UID="${MOCK_CURRENT_UID_UNDER_TEST:-1000}" \
        MOCK_CURRENT_USER="${MOCK_CURRENT_USER_UNDER_TEST:-arc-community-test}" \
        MOCK_TARGET_USER="${MOCK_TARGET_USER_UNDER_TEST:-arc-community-test}" \
        MOCK_TARGET_UID="${MOCK_TARGET_UID_UNDER_TEST:-1000}" \
        MOCK_TARGET_GROUP="${MOCK_TARGET_GROUP_UNDER_TEST:-arc-community-test}" \
        MOCK_TARGET_HOME="$sandbox/home" \
        MOCK_HEALTH_PORT="${MOCK_HEALTH_PORT_UNDER_TEST:-}" \
        MOCK_HEALTH_STATUS="${MOCK_HEALTH_STATUS_UNDER_TEST:-ok}" \
        MOCK_TAMPER_BINARY="${MOCK_TAMPER_BINARY_UNDER_TEST:-0}" \
        MOCK_SYSTEMD_NODE_ACTIVE="${MOCK_SYSTEMD_NODE_ACTIVE_UNDER_TEST:-false}" \
        MOCK_SYSTEMD_NODE_ENABLED="${MOCK_SYSTEMD_NODE_ENABLED_UNDER_TEST:-false}" \
        MOCK_SYSTEMD_UPDATER_ACTIVE="${MOCK_SYSTEMD_UPDATER_ACTIVE_UNDER_TEST:-false}" \
        MOCK_SYSTEMD_UPDATER_ENABLED="${MOCK_SYSTEMD_UPDATER_ENABLED_UNDER_TEST:-false}" \
        MOCK_SERVICE_FAIL_MATCH="${MOCK_SERVICE_FAIL_MATCH_UNDER_TEST:-}" \
        MOCK_SERVICE_FAIL_ONCE_FILE="$sandbox/service-fail-once" \
        ARC_INSTALL_TEST_FAIL_AFTER_COPY="${ARC_INSTALL_TEST_FAIL_AFTER_COPY_UNDER_TEST:-}" \
        ARC_HEALTH_TIMEOUT="${ARC_HEALTH_TIMEOUT_UNDER_TEST:-180}" \
        ARC_TEST_NODE_ARGS_LOG="$sandbox/node-args.log" \
        TMPDIR="$sandbox/tmp" \
        NO_COLOR=1 \
        LC_ALL=C \
        /bin/bash "$INSTALLER" "$@"
}

assert_log_contains_literal() {
    local file="$1" literal="$2" message="$3"
    grep -Fq -- "$literal" "$file" || {
        printf '%s\n' "$message"
        return 1
    }
}

file_mode() {
    local file="$1"
    stat -f '%Lp' "$file" 2>/dev/null || stat -c '%a' "$file"
}

file_sha256() {
    local file="$1"
    if command -v sha256sum >/dev/null 2>&1; then
        sha256sum "$file" | awk '{print $1}'
    else
        shasum -a 256 "$file" | awk '{print $1}'
    fi
}

install_only_platform_matrix() {
    local os arch node_asset cli_asset sandbox output status
    local matrix='
Linux|x86_64|arc-node-linux-x86_64|arc-cli-linux-x86_64
Linux|amd64|arc-node-linux-x86_64|arc-cli-linux-x86_64
Linux|arm64|arc-node-linux-arm64|arc-cli-linux-arm64
Linux|aarch64|arc-node-linux-arm64|arc-cli-linux-arm64
Darwin|arm64|arc-node-macos-arm64|arc-cli-macos-arm64
Darwin|x86_64|arc-node-macos-x86_64|arc-cli-macos-x86_64
'

    while IFS='|' read -r os arch node_asset cli_asset; do
        [ -z "$os" ] && continue
        new_sandbox
        sandbox="$NEW_SANDBOX"
        output="$sandbox/install.out"
        ARC_NODE_VERSION_UNDER_TEST=''
        MOCK_TAMPER_BINARY_UNDER_TEST=0
        invoke_installer "$sandbox" "$os" "$arch" \
            "$TEST_DIR/fixtures/release-v0.8.0.json" '0.8.0' \
            --no-service --no-auto-update --port 18444 >"$output" 2>&1
        status=$?
        if [ "$status" -ne 0 ]; then
            printf '%s/%s offline install failed (exit %s):\n' "$os" "$arch" "$status"
            sed -n '1,120p' "$output"
            return 1
        fi
        assert_log_contains_literal "$sandbox/curl.log" "/v0.8.0/$node_asset" \
            "$os/$arch did not request $node_asset from the exact tag" || return 1
        assert_log_contains_literal "$sandbox/curl.log" "/v0.8.0/$cli_asset" \
            "$os/$arch did not request $cli_asset from the exact tag" || return 1
        assert_log_contains_literal "$sandbox/curl.log" '/v0.8.0/SHA256SUMS' \
            "$os/$arch did not request SHA256SUMS from the exact tag" || return 1
        if [ ! -x "$sandbox/arc/bin/arc-node" ] || [ ! -x "$sandbox/arc/bin/arc-cli" ]; then
            printf '%s/%s did not install executable arc-node and arc-cli files\n' "$os" "$arch"
            find "$sandbox/arc" -maxdepth 3 -type f -print
            return 1
        fi
        if [ -s "$sandbox/node-args.log" ]; then
            printf '%s/%s --no-service unexpectedly started a node:\n' "$os" "$arch"
            cat "$sandbox/node-args.log"
            return 1
        fi
        if grep -Fq '/health' "$sandbox/curl.log"; then
            printf '%s/%s --no-service unexpectedly performed a localhost health probe\n' "$os" "$arch"
            return 1
        fi
        if grep -Eq 'releases/latest/download|releases\?per_page=|(^|[[:space:]])-r([[:space:]]|$)' "$sandbox/curl.log"; then
            printf '%s/%s used an ambiguous latest download, release walking, or Range probe:\n' "$os" "$arch"
            cat "$sandbox/curl.log"
            return 1
        fi
    done <<EOF
$matrix
EOF
}

tampered_binary_is_rejected() {
    local sandbox output status
    new_sandbox
    sandbox="$NEW_SANDBOX"
    output="$sandbox/install.out"
    ARC_NODE_VERSION_UNDER_TEST=''
    MOCK_TAMPER_BINARY_UNDER_TEST=1
    invoke_installer "$sandbox" Linux x86_64 \
        "$TEST_DIR/fixtures/release-v0.8.0.json" '0.8.0' \
        --no-service --no-auto-update >"$output" 2>&1
    status=$?
    MOCK_TAMPER_BINARY_UNDER_TEST=0

    if ! grep -Fq '/v0.8.0/arc-node-linux-x86_64' "$sandbox/curl.log" \
        || ! grep -Fq '/v0.8.0/SHA256SUMS' "$sandbox/curl.log"; then
        printf 'test did not reach binary + checksum verification; installer failed for another reason:\n'
        cat "$sandbox/curl.log"
        sed -n '1,100p' "$output"
        return 1
    fi
    if [ "$status" -eq 0 ]; then
        printf 'installer accepted binaries that do not match SHA256SUMS\n'
        sed -n '1,120p' "$output"
        return 1
    fi
    if [ -x "$sandbox/arc/bin/arc-node" ] || [ -x "$sandbox/arc/bin/arc-cli" ]; then
        printf 'installer left a tampered executable installed after verification failed\n'
        return 1
    fi
}

invalid_version_pin_fails_before_asset_download() {
    local sandbox output status
    new_sandbox
    sandbox="$NEW_SANDBOX"
    output="$sandbox/install.out"
    ARC_NODE_VERSION_UNDER_TEST='0.8'
    MOCK_TAMPER_BINARY_UNDER_TEST=0
    invoke_installer "$sandbox" Linux x86_64 \
        "$TEST_DIR/fixtures/release-v0.8.0.json" '0.8.0' \
        --no-service --no-auto-update >"$output" 2>&1
    status=$?
    ARC_NODE_VERSION_UNDER_TEST=''

    if [ "$status" -eq 0 ]; then
        printf 'installer accepted non-X.Y.Z ARC_NODE_VERSION=0.8\n'
        return 1
    fi
    if grep -Fq '/releases/download/' "$sandbox/curl.log"; then
        printf 'invalid version reached release asset download before validation:\n'
        cat "$sandbox/curl.log"
        return 1
    fi
}

no_service_no_updater_really_is_install_only() {
    local sandbox output seed_value
    new_sandbox
    sandbox="$NEW_SANDBOX"
    output="$sandbox/install.out"
    ARC_NODE_VERSION_UNDER_TEST='v0.8.0'
    MOCK_TAMPER_BINARY_UNDER_TEST=0
    if ! invoke_installer "$sandbox" Linux x86_64 \
        "$TEST_DIR/fixtures/release-v0.8.0.json" '0.8.0' \
        --no-service --no-auto-update --port 18444 >"$output" 2>&1; then
        sed -n '1,120p' "$output"
        return 1
    fi
    ARC_NODE_VERSION_UNDER_TEST=''

    if [ -s "$sandbox/service.log" ]; then
        printf 'install-only mode invoked a service manager:\n'
        cat "$sandbox/service.log"
        return 1
    fi
    if find "$sandbox/arc" -type f \( -name '*auto-update*' -o -name '*updater*' \) | grep -q .; then
        printf 'install-only mode installed updater files despite --no-auto-update:\n'
        find "$sandbox/arc" -type f \( -name '*auto-update*' -o -name '*updater*' \) -print
        return 1
    fi
    assert_file_contains "$sandbox/arc/install.conf" '^rpc_port=18444$' \
        'custom RPC port was not persisted for later updates/health checks' || return 1
    assert_file_contains "$sandbox/arc/install.conf" '^p2p_port=18445$' \
        'default P2P port is not custom RPC port + 1' || return 1
    assert_file_contains "$sandbox/arc/install.conf" '^model_path=$' \
        'empty model choice was not persisted unambiguously' || return 1
    assert_file_contains "$sandbox/arc/install.conf" '^service_scope=none$' \
        'install-only mode was not persisted as service_scope=none' || return 1
    assert_file_contains "$sandbox/arc/bin/run-arc-node" '--rpc 127\.0\.0\.1:18444' \
        'generated runner does not bind the custom RPC port to loopback' || return 1
    assert_file_not_contains "$sandbox/arc/bin/run-arc-node" '--rpc 0\.0\.0\.0:' \
        'managed stake-zero runner exposes unauthenticated RPC on every interface' || return 1
    assert_file_contains "$sandbox/arc/bin/run-arc-node" '--p2p-port 18445' \
        'generated runner does not use RPC+1 for P2P' || return 1
    assert_file_contains "$sandbox/arc/bin/run-arc-node" '--community-mode' \
        'generated runner omits mandatory stake-0 community mode' || return 1
    assert_file_contains "$sandbox/arc/bin/run-arc-node" '--stake 0' \
        'generated observer runner does not force stake to zero' || return 1
    assert_file_contains "$sandbox/arc/bin/run-arc-node" '--min-stake 0' \
        'generated observer runner does not disable validator minimum stake' || return 1
    assert_file_contains "$sandbox/arc/genesis.toml" \
        '^validator_set_complete[[:space:]]*=[[:space:]]*false$' \
        'installed release genesis is not an explicit observer placeholder' || return 1
    assert_file_not_contains "$sandbox/arc/genesis.toml" '^\[\[validators\]\]' \
        'installed observer genesis contains a partial validator set' || return 1
    assert_file_not_contains "$sandbox/arc/bin/run-arc-node" '--model([[:space:]]|$)' \
        'generated runner passes --model even though no model was configured' || return 1
    assert_file_not_contains "$sandbox/arc/bin/run-arc-node" '--validator-seed' \
        'generated runner exposes validator identity through argv' || return 1

    assert_equals 600 "$(file_mode "$sandbox/arc/identity/validator-seed")" \
        'validator seed permissions are not 0600' || return 1
    assert_equals 600 "$(file_mode "$sandbox/arc/node.env")" \
        'validator environment file permissions are not 0600' || return 1
    seed_value="$(sed -n '1p' "$sandbox/arc/identity/validator-seed")"
    if grep -Fq "$seed_value" "$output" || grep -Fq "$seed_value" "$sandbox/arc/bin/run-arc-node"; then
        printf 'validator identity leaked into installer output or process argv wrapper\n'
        return 1
    fi
}

sudo_root_install_targets_the_invoking_user() {
    local sandbox output status
    new_sandbox
    sandbox="$NEW_SANDBOX"
    output="$sandbox/root-install.out"
    ARC_NODE_VERSION_UNDER_TEST='v0.8.0'
    MOCK_TAMPER_BINARY_UNDER_TEST=0
    MOCK_CURRENT_UID_UNDER_TEST=0
    MOCK_CURRENT_USER_UNDER_TEST=root
    SUDO_USER_UNDER_TEST=arc-community-test

    invoke_installer "$sandbox" Linux x86_64 \
        "$TEST_DIR/fixtures/release-v0.8.0.json" '0.8.0' \
        --no-service --no-auto-update >"$output" 2>&1
    status=$?

    MOCK_CURRENT_UID_UNDER_TEST=1000
    MOCK_CURRENT_USER_UNDER_TEST=arc-community-test
    SUDO_USER_UNDER_TEST=''
    ARC_NODE_VERSION_UNDER_TEST=''

    if [ "$status" -ne 0 ]; then
        printf 'sudo/root simulation failed instead of normalizing SUDO_USER:\n'
        sed -n '1,140p' "$output"
        return 1
    fi
    if ! grep -Fq 'arc-community-test:arc-community-test' "$sandbox/owner.log"; then
        printf 'root path never assigned installed files to the invoking SUDO_USER:\n'
        cat "$sandbox/owner.log"
        return 1
    fi
    if grep -Eq 'chown root(:|[[:space:]])' "$sandbox/owner.log"; then
        printf 'sudo/root path assigned community install files to root\n'
        return 1
    fi
}

windows_is_manual_only() {
    local sandbox output status
    new_sandbox
    sandbox="$NEW_SANDBOX"
    output="$sandbox/windows.out"
    ARC_NODE_VERSION_UNDER_TEST=''
    invoke_installer "$sandbox" MINGW64_NT x86_64 \
        "$TEST_DIR/fixtures/release-v0.8.0.json" '0.8.0' \
        --no-service --no-auto-update >"$output" 2>&1
    status=$?
    if [ "$status" -eq 0 ]; then
        printf 'shell installer incorrectly accepted Windows; Windows is manual-binary only\n'
        return 1
    fi
    if [ -s "$sandbox/curl.log" ]; then
        printf 'unsupported Windows path made release requests before failing:\n'
        cat "$sandbox/curl.log"
        return 1
    fi
}

update_only_refuses_downgrade() {
    local sandbox output status before_hash after_hash
    new_sandbox
    sandbox="$NEW_SANDBOX"
    output="$sandbox/base-install.out"
    ARC_NODE_VERSION_UNDER_TEST='v0.8.0'
    if ! invoke_installer "$sandbox" Linux x86_64 \
        "$TEST_DIR/fixtures/release-v0.8.0.json" '0.8.0' \
        --no-service --no-auto-update --port 18444 >"$output" 2>&1; then
        sed -n '1,120p' "$output"
        return 1
    fi
    before_hash="$(file_sha256 "$sandbox/arc/bin/arc-node")"

    : >"$sandbox/curl.log"
    ARC_NODE_VERSION_UNDER_TEST=''
    output="$sandbox/downgrade.out"
    invoke_installer "$sandbox" Linux x86_64 \
        "$TEST_DIR/fixtures/release-v0.7.9.json" '0.7.9' \
        --update-only --no-service --no-auto-update >"$output" 2>&1
    status=$?
    after_hash="$(file_sha256 "$sandbox/arc/bin/arc-node")"

    if [ "$status" -eq 0 ]; then
        printf 'update-only accepted downgrade from 0.8.0 to 0.7.9\n'
        return 1
    fi
    assert_equals "$before_hash" "$after_hash" 'downgrade refusal changed the installed node bytes' || return 1
    assert_file_contains "$output" 'Refusing downgrade from v0\.8\.0 to v0\.7\.9' \
        'downgrade error does not state both versions' || return 1
    if grep -Fq '/releases/download/' "$sandbox/curl.log"; then
        printf 'downgrade guard ran after artifact downloads instead of before them:\n'
        cat "$sandbox/curl.log"
        return 1
    fi
}

update_only_preserves_custom_port_and_empty_model() {
    local sandbox output
    new_sandbox
    sandbox="$NEW_SANDBOX"
    ARC_NODE_VERSION_UNDER_TEST='v0.8.0'
    output="$sandbox/base-install.out"
    if ! invoke_installer "$sandbox" Linux amd64 \
        "$TEST_DIR/fixtures/release-v0.8.0.json" '0.8.0' \
        --no-service --no-auto-update --port 18444 >"$output" 2>&1; then
        sed -n '1,120p' "$output"
        return 1
    fi

    : >"$sandbox/curl.log"
    ARC_NODE_VERSION_UNDER_TEST=''
    output="$sandbox/upgrade.out"
    if ! invoke_installer "$sandbox" Linux amd64 \
        "$TEST_DIR/fixtures/release-v0.8.1.json" '0.8.1' \
        --update-only --no-service --no-auto-update >"$output" 2>&1; then
        sed -n '1,140p' "$output"
        return 1
    fi
    "$sandbox/arc/bin/arc-node" --version | grep -Fq '0.8.1' || {
        printf 'update-only did not replace node with v0.8.1\n'
        return 1
    }
    assert_file_contains "$sandbox/arc/install.conf" '^rpc_port=18444$' \
        'update-only lost the custom RPC port' || return 1
    assert_file_contains "$sandbox/arc/install.conf" '^p2p_port=18445$' \
        'update-only lost the derived P2P port' || return 1
    assert_file_contains "$sandbox/arc/install.conf" '^model_path=$' \
        'update-only changed an intentionally empty model path' || return 1
    assert_file_not_contains "$sandbox/arc/bin/run-arc-node" '--model([[:space:]]|$)' \
        'update-only introduced an empty --model argument' || return 1
    assert_log_contains_literal "$sandbox/curl.log" '/releases/download/v0.8.1/arc-node-linux-x86_64' \
        'update-only did not use the exact newer tag' || return 1
    if grep -Fq '/health' "$sandbox/curl.log"; then
        printf 'no-service update unexpectedly health-probed a process it did not start\n'
        return 1
    fi
}

degraded_service_health_is_reported_truthfully() {
    local sandbox output
    new_sandbox
    sandbox="$NEW_SANDBOX"
    output="$sandbox/degraded-health.out"
    ARC_NODE_VERSION_UNDER_TEST='v0.8.0'
    MOCK_HEALTH_PORT_UNDER_TEST=9944
    MOCK_HEALTH_STATUS_UNDER_TEST=degraded

    if ! invoke_installer "$sandbox" Linux x86_64 \
        "$TEST_DIR/fixtures/release-v0.8.0.json" '0.8.0' \
        --user-service --no-auto-update >"$output" 2>&1; then
        sed -n '1,160p' "$output"
        MOCK_HEALTH_PORT_UNDER_TEST=''
        MOCK_HEALTH_STATUS_UNDER_TEST=''
        ARC_NODE_VERSION_UNDER_TEST=''
        return 1
    fi

    MOCK_HEALTH_PORT_UNDER_TEST=''
    MOCK_HEALTH_STATUS_UNDER_TEST=''
    ARC_NODE_VERSION_UNDER_TEST=''
    assert_file_contains "$output" 'reports status=degraded' \
        'installer hid a degraded node behind a generic healthy message' || return 1
    assert_file_not_contains "$output" 'is healthy' \
        'installer still labels a degraded migration observer as healthy' || return 1
    assert_file_not_contains "$output" 'restoring the previous arc-node binary' \
        'a truthful degraded migration observer should remain installed for diagnosis' || return 1
}

legacy_installer_delegates_without_generating_validator_material() {
    local sandbox output expected_args actual_args
    new_sandbox
    sandbox="$NEW_SANDBOX"
    mkdir -p "$sandbox/legacy/scripts"
    cp "$LEGACY_INSTALLER" "$sandbox/legacy/scripts/install-node.sh"
    cat >"$sandbox/legacy/install.sh" <<'SH'
#!/usr/bin/env bash
set -eu
printf '%s\n' "$@" >"${ARC_LEGACY_ARGS_LOG:?}"
SH
    chmod +x "$sandbox/legacy/install.sh"
    output="$sandbox/legacy.out"
    env -i \
        PATH='/usr/bin:/bin' \
        ARC_LEGACY_ARGS_LOG="$sandbox/legacy-args.log" \
        /bin/bash "$sandbox/legacy/scripts/install-node.sh" \
        --no-service --no-auto-update --port 18444 >"$output" 2>&1 || {
            cat "$output"
            return 1
        }
    expected_args=$'--no-service\n--no-auto-update\n--port\n18444'
    actual_args="$(cat "$sandbox/legacy-args.log")"
    assert_equals "$expected_args" "$actual_args" \
        'legacy wrapper did not forward arguments exactly to root install.sh' || return 1
    assert_file_contains "$output" 'legacy validator installation is retired' \
        'legacy wrapper does not explain the safe migration' || return 1
    if find "$sandbox/legacy" -type f \( -name '*validator*' -o -name '*.key' \) | grep -q .; then
        printf 'legacy wrapper generated validator material before delegating:\n'
        find "$sandbox/legacy" -type f -print
        return 1
    fi
}

transaction_state() {
    local sandbox="$1"
    local label path
    while IFS='|' read -r label path; do
        [ -n "$label" ] || continue
        if [ -f "$path" ]; then
            printf '%s|file|%s|%s\n' "$label" "$(file_sha256 "$path")" "$(file_mode "$path")"
        else
            printf '%s|absent\n' "$label"
        fi
    done <<EOF
arc-node|$sandbox/arc/bin/arc-node
arc-cli|$sandbox/arc/bin/arc-cli
arc-installer|$sandbox/arc/bin/arc-installer
seeds|$sandbox/arc/testnet-seeds.txt
genesis|$sandbox/arc/genesis.toml
identity|$sandbox/arc/identity/validator-seed
node-env|$sandbox/arc/node.env
runner|$sandbox/arc/bin/run-arc-node
install-config|$sandbox/arc/install.conf
node-unit|$sandbox/home/.config/systemd/user/arc-node.service
updater-unit|$sandbox/home/.config/systemd/user/arc-node-update.service
updater-timer|$sandbox/home/.config/systemd/user/arc-node-update.timer
model|$sandbox/model.gguf
data-sentinel|$sandbox/arc/data/existing-user-data
EOF
}

reset_transaction_test_environment() {
    ARC_NODE_VERSION_UNDER_TEST=''
    MOCK_HEALTH_PORT_UNDER_TEST=''
    MOCK_HEALTH_STATUS_UNDER_TEST=''
    ARC_HEALTH_TIMEOUT_UNDER_TEST=''
    ARC_INSTALL_TEST_FAIL_AFTER_COPY_UNDER_TEST=''
    MOCK_SYSTEMD_NODE_ACTIVE_UNDER_TEST=false
    MOCK_SYSTEMD_NODE_ENABLED_UNDER_TEST=false
    MOCK_SYSTEMD_UPDATER_ACTIVE_UNDER_TEST=false
    MOCK_SYSTEMD_UPDATER_ENABLED_UNDER_TEST=false
    MOCK_SERVICE_FAIL_MATCH_UNDER_TEST=''
}

prepare_transactional_user_install() {
    local sandbox="$1" output="$2"
    mkdir -p "$sandbox/arc/data"
    printf 'existing user chain bytes\n' >"$sandbox/arc/data/existing-user-data"
    printf 'existing local model bytes\n' >"$sandbox/model.gguf"
    ARC_NODE_VERSION_UNDER_TEST=v0.8.0
    MOCK_HEALTH_PORT_UNDER_TEST=18444
    MOCK_HEALTH_STATUS_UNDER_TEST=ok
    ARC_HEALTH_TIMEOUT_UNDER_TEST=4
    if ! invoke_installer "$sandbox" Linux x86_64 \
        "$TEST_DIR/fixtures/release-v0.8.0.json" 0.8.0 \
        --user-service --port 18444 --model "$sandbox/model.gguf" >"$output" 2>&1; then
        sed -n '1,180p' "$output"
        reset_transaction_test_environment
        return 1
    fi
    ARC_NODE_VERSION_UNDER_TEST=''
    MOCK_SYSTEMD_NODE_ACTIVE_UNDER_TEST=true
    MOCK_SYSTEMD_NODE_ENABLED_UNDER_TEST=true
    MOCK_SYSTEMD_UPDATER_ACTIVE_UNDER_TEST=true
    MOCK_SYSTEMD_UPDATER_ENABLED_UNDER_TEST=true
}

assert_failed_update_restored_everything() {
    local sandbox="$1" before_state="$2" output="$3" failure_name="$4"
    local after_state
    after_state="$(transaction_state "$sandbox")"
    assert_equals "$before_state" "$after_state" \
        "$failure_name left a mixed binary/config/service/identity installation" || return 1
    assert_file_contains "$output" 'restoring every previously managed file and service state' \
        "$failure_name did not run the full install transaction rollback" || return 1
    assert_file_contains "$output" 'Previous ARC installation and service state restored' \
        "$failure_name did not report a successful full rollback" || return 1
    assert_log_contains_literal "$sandbox/service.log" 'systemctl --user restart arc-node.service' \
        "$failure_name did not restart the previously active node service" || return 1
    if find "$sandbox/arc" "$sandbox/home/.config/systemd/user" \
        -type f \( -name '*.new.*' -o -name '*.rollback.*' \) | grep -q .; then
        printf '%s left staged transaction files behind:\n' "$failure_name"
        find "$sandbox/arc" "$sandbox/home/.config/systemd/user" \
            -type f \( -name '*.new.*' -o -name '*.rollback.*' \) -print
        return 1
    fi
    if [ -e "$sandbox/arc/.install.lock" ]; then
        printf '%s left the installer lock behind\n' "$failure_name"
        return 1
    fi
}

fresh_mid_copy_failure_removes_new_managed_files() {
    local sandbox output status
    new_sandbox
    sandbox="$NEW_SANDBOX"
    output="$sandbox/fresh-mid-copy.out"
    ARC_NODE_VERSION_UNDER_TEST=v0.8.0
    ARC_INSTALL_TEST_FAIL_AFTER_COPY_UNDER_TEST=3
    invoke_installer "$sandbox" Linux x86_64 \
        "$TEST_DIR/fixtures/release-v0.8.0.json" 0.8.0 \
        --no-service --no-auto-update >"$output" 2>&1
    status=$?
    reset_transaction_test_environment
    if [ "$status" -eq 0 ]; then
        printf 'injected fresh-install copy failure unexpectedly succeeded\n'
        return 1
    fi
    if find "$sandbox/arc" -type f | grep -q .; then
        printf 'fresh-install rollback retained newly introduced managed files:\n'
        find "$sandbox/arc" -type f -print
        return 1
    fi
    assert_file_contains "$output" 'Previous ARC installation and service state restored' \
        'fresh-install copy failure did not complete rollback' || return 1
}

mid_copy_update_failure_restores_full_install() {
    local sandbox output before_state status
    new_sandbox
    sandbox="$NEW_SANDBOX"
    output="$sandbox/base-transaction-install.out"
    prepare_transactional_user_install "$sandbox" "$output" || return 1
    before_state="$(transaction_state "$sandbox")"
    : >"$sandbox/service.log"
    output="$sandbox/mid-copy-update.out"
    ARC_INSTALL_TEST_FAIL_AFTER_COPY_UNDER_TEST=4
    invoke_installer "$sandbox" Linux x86_64 \
        "$TEST_DIR/fixtures/release-v0.8.1.json" 0.8.1 \
        --update-only >"$output" 2>&1
    status=$?
    ARC_INSTALL_TEST_FAIL_AFTER_COPY_UNDER_TEST=''
    if [ "$status" -eq 0 ]; then
        printf 'injected mid-copy update failure unexpectedly succeeded\n'
        reset_transaction_test_environment
        return 1
    fi
    assert_failed_update_restored_everything "$sandbox" "$before_state" "$output" \
        'mid-copy update failure' || {
            reset_transaction_test_environment
            return 1
        }
    reset_transaction_test_environment
}

service_failure_restores_full_install() {
    local sandbox output before_state status
    new_sandbox
    sandbox="$NEW_SANDBOX"
    output="$sandbox/base-transaction-install.out"
    prepare_transactional_user_install "$sandbox" "$output" || return 1
    before_state="$(transaction_state "$sandbox")"
    : >"$sandbox/service.log"
    rm -f "$sandbox/service-fail-once"
    output="$sandbox/service-failure-update.out"
    MOCK_SERVICE_FAIL_MATCH_UNDER_TEST='restart arc-node.service'
    invoke_installer "$sandbox" Linux x86_64 \
        "$TEST_DIR/fixtures/release-v0.8.1.json" 0.8.1 \
        --update-only >"$output" 2>&1
    status=$?
    MOCK_SERVICE_FAIL_MATCH_UNDER_TEST=''
    if [ "$status" -eq 0 ]; then
        printf 'injected node-service restart failure unexpectedly succeeded\n'
        reset_transaction_test_environment
        return 1
    fi
    assert_failed_update_restored_everything "$sandbox" "$before_state" "$output" \
        'service restart failure' || {
            reset_transaction_test_environment
            return 1
        }
    reset_transaction_test_environment
}

health_failure_restores_full_install() {
    local sandbox output before_state status
    new_sandbox
    sandbox="$NEW_SANDBOX"
    output="$sandbox/base-transaction-install.out"
    prepare_transactional_user_install "$sandbox" "$output" || return 1
    before_state="$(transaction_state "$sandbox")"
    : >"$sandbox/service.log"
    output="$sandbox/health-failure-update.out"
    MOCK_HEALTH_STATUS_UNDER_TEST=starting
    ARC_HEALTH_TIMEOUT_UNDER_TEST=2
    invoke_installer "$sandbox" Linux x86_64 \
        "$TEST_DIR/fixtures/release-v0.8.1.json" 0.8.1 \
        --update-only >"$output" 2>&1
    status=$?
    MOCK_HEALTH_STATUS_UNDER_TEST=ok
    if [ "$status" -eq 0 ]; then
        printf 'non-ready health response unexpectedly committed an update\n'
        reset_transaction_test_environment
        return 1
    fi
    assert_failed_update_restored_everything "$sandbox" "$before_state" "$output" \
        'health classification failure' || {
            reset_transaction_test_environment
            return 1
        }
    reset_transaction_test_environment
}

transaction_contract_covers_every_service_scope() {
    local required_literal
    for required_literal in \
        'snapshot_transaction_path /etc/systemd/system/arc-node.service' \
        'snapshot_transaction_path /etc/systemd/system/arc-node-update.service' \
        'snapshot_transaction_path /etc/systemd/system/arc-node-update.timer' \
        'snapshot_transaction_path "$USER_UNIT_DIR/arc-node.service"' \
        'snapshot_transaction_path "$USER_UNIT_DIR/arc-node-update.service"' \
        'snapshot_transaction_path "$USER_UNIT_DIR/arc-node-update.timer"' \
        'snapshot_transaction_path "$NODE_PLIST"' \
        'snapshot_transaction_path "$UPDATE_PLIST"'
    do
        assert_log_contains_literal "$REPO_ROOT/install.sh" "$required_literal" \
            "installer transaction omits service-managed path: $required_literal" || return 1
    done
    assert_file_not_contains "$REPO_ROOT/install.sh" \
        'as_root cp -- "\$TMP_DIR/arc-node[^ ]*" /etc/systemd/system/' \
        'system service files bypass transactional_copy' || return 1
    assert_file_contains "$REPO_ROOT/install.sh" \
        '^commit_install_transaction$' \
        'installer never commits the full transaction after health succeeds' || return 1
}

run_test 'offline platform aliases install exact-tag node + CLI assets without starting' install_only_platform_matrix
run_test 'checksum mismatch rejects and removes staged executables' tampered_binary_is_rejected
run_test 'ARC_NODE_VERSION requires strict X.Y.Z before network-shaped requests' invalid_version_pin_fails_before_asset_download
run_test '--no-service --no-auto-update has no start, health, service, or updater side effects' no_service_no_updater_really_is_install_only
run_test 'sudo/root execution normalizes ownership to the invoking community user' sudo_root_install_targets_the_invoking_user
run_test 'Windows remains a documented manual-binary path, not a shell install path' windows_is_manual_only
run_test 'update-only refuses downgrade before downloading artifacts' update_only_refuses_downgrade
run_test 'update-only preserves custom ports and an intentionally empty model' update_only_preserves_custom_port_and_empty_model
run_test 'degraded service health remains installed but is never labeled healthy' degraded_service_health_is_reported_truthfully
run_test 'legacy installer delegates to stake-zero root installer without key generation' legacy_installer_delegates_without_generating_validator_material
run_test 'fresh mid-copy failure removes every newly introduced managed file' fresh_mid_copy_failure_removes_new_managed_files
run_test 'mid-copy update failure restores the complete prior install transaction' mid_copy_update_failure_restores_full_install
run_test 'service restart failure restores files and prior service state' service_failure_restores_full_install
run_test 'non-ready health failure restores files and prior service state' health_failure_restores_full_install
run_test 'systemd system/user and launchd files share the full transaction boundary' transaction_contract_covers_every_service_scope

finish_tests
