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
CUTOVER_ASSETS='
arc-cutover-policy.json
arc-legacy-maintenance-boundary.json
arc-recovery-checkpoint-descriptor.json
'

ACTIVE_SANDBOXES=""
ACTIVE_TEST_PIDS=""
NEW_SANDBOX=""
cleanup_sandboxes() {
    local sandbox test_pid
    for test_pid in $ACTIVE_TEST_PIDS; do
        kill -KILL "$test_pid" 2>/dev/null || true
        wait "$test_pid" 2>/dev/null || true
    done
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

new_sandbox() {
    local sandbox sandbox_root mock_bin command_name
    # The installer correctly refuses /tmp as a persistent chain-data parent.
    # Put contract sandboxes below the test user's home (or an explicit safe
    # override) so normal fixtures exercise supported directory policy.
    sandbox_root="${ARC_INSTALLER_TEST_TMPDIR:-$HOME}"
    sandbox="$(mktemp -d "$sandbox_root/.arc-installer-contract.XXXXXX")"
    # Use the physical spelling so host-level aliases such as macOS /var do
    # not look like an installer-controlled symlink in Linux contract tests.
    sandbox="$(CDPATH='' cd -- "$sandbox" && pwd -P)"
    ACTIVE_SANDBOXES="$ACTIVE_SANDBOXES $sandbox"
    mkdir -p "$sandbox/home" "$sandbox/bin" "$sandbox/tmp"
    : >"$sandbox/curl.log"
    : >"$sandbox/node-args.log"
    : >"$sandbox/service.log"
    : >"$sandbox/owner.log"

    mock_bin="$sandbox/bin"
    cp "$TEST_DIR/helpers/mock-curl.sh" "$mock_bin/curl"
    cp "$TEST_DIR/helpers/mock-platform-command.sh" "$mock_bin/platform-command"
    chmod +x "$mock_bin/curl" "$mock_bin/platform-command"
    for command_name in uname sleep free sysctl openssl ssh-keygen hostname id getent chown stat runuser sudo systemctl launchctl ps; do
        ln -s platform-command "$mock_bin/$command_name"
    done
    NEW_SANDBOX="$sandbox"
}

unsafe_data_directories_fail_before_side_effects() {
    local sandbox output status unsafe_path
    for unsafe_path in / /etc /etc/arc-chain /root /tmp/arc-chain /usr /usr/local/arc-chain /var /var/lib; do
        new_sandbox
        sandbox="$NEW_SANDBOX"
        output="$sandbox/unsafe-data.out"
        invoke_installer "$sandbox" Linux x86_64 \
            "$TEST_DIR/fixtures/release-v0.8.0.json" '0.8.0' \
            --no-service --no-auto-update --data-dir "$unsafe_path" >"$output" 2>&1
        status=$?
        if [ "$status" -eq 0 ]; then
            printf 'installer accepted unsafe data directory: %s\n' "$unsafe_path"
            return 1
        fi
        [ ! -s "$sandbox/curl.log" ] || {
            printf 'unsafe data directory reached release downloads (%s):\n' "$unsafe_path"
            cat "$sandbox/curl.log"
            return 1
        }
        [ ! -s "$sandbox/owner.log" ] || {
            printf 'unsafe data directory reached ownership changes (%s):\n' "$unsafe_path"
            cat "$sandbox/owner.log"
            return 1
        }
        assert_file_contains "$output" 'Refusing unsafe data directory' \
            "unsafe data directory did not fail with the dedicated-path guard: $unsafe_path" \
            || return 1
    done
}

relative_and_ambiguous_data_directories_fail_closed() {
    local sandbox output status unsafe_path
    for unsafe_path in 'relative/arc-data' '/srv/arc-data/../etc' '//etc/arc-data'; do
        new_sandbox
        sandbox="$NEW_SANDBOX"
        output="$sandbox/ambiguous-data.out"
        invoke_installer "$sandbox" Linux x86_64 \
            "$TEST_DIR/fixtures/release-v0.8.0.json" '0.8.0' \
            --no-service --no-auto-update --data-dir "$unsafe_path" >"$output" 2>&1
        status=$?
        if [ "$status" -eq 0 ]; then
            printf 'installer accepted non-canonical data directory: %s\n' "$unsafe_path"
            return 1
        fi
        [ ! -s "$sandbox/curl.log" ] || {
            printf 'non-canonical data directory reached release downloads (%s):\n' "$unsafe_path"
            cat "$sandbox/curl.log"
            return 1
        }
    done
}

symlinked_data_directory_or_ancestor_fails_closed() {
    local sandbox output status data_path case_name
    for case_name in final ancestor; do
        new_sandbox
        sandbox="$NEW_SANDBOX"
        mkdir -p "$sandbox/symlink-target"
        ln -s "$sandbox/symlink-target" "$sandbox/data-link"
        if [ "$case_name" = final ]; then
            data_path="$sandbox/data-link"
        else
            data_path="$sandbox/data-link/arc-data"
        fi
        output="$sandbox/symlink-data.out"
        invoke_installer "$sandbox" Linux x86_64 \
            "$TEST_DIR/fixtures/release-v0.8.0.json" '0.8.0' \
            --no-service --no-auto-update --data-dir "$data_path" >"$output" 2>&1
        status=$?
        if [ "$status" -eq 0 ]; then
            printf 'installer accepted a symlinked data %s: %s\n' "$case_name" "$data_path"
            return 1
        fi
        [ ! -s "$sandbox/curl.log" ] || {
            printf 'symlinked data %s reached release downloads:\n' "$case_name"
            cat "$sandbox/curl.log"
            return 1
        }
        [ ! -e "$sandbox/symlink-target/arc-data" ] || {
            printf 'symlink ancestor rejection created the target data directory\n'
            return 1
        }
        assert_file_contains "$output" 'symlink component' \
            "symlinked data $case_name did not fail at component validation" || return 1
    done
}

dedicated_custom_data_directory_installs_normally() {
    local sandbox output custom_data
    new_sandbox
    sandbox="$NEW_SANDBOX"
    custom_data="$sandbox/custom-volume/arc-data"
    output="$sandbox/custom-data.out"
    if ! invoke_installer "$sandbox" Linux x86_64 \
        "$TEST_DIR/fixtures/release-v0.8.0.json" '0.8.0' \
        --no-service --no-auto-update --data-dir "$custom_data" >"$output" 2>&1; then
        printf 'installer rejected a dedicated absolute custom data directory:\n'
        sed -n '1,140p' "$output"
        return 1
    fi
    [ -d "$custom_data" ] || {
        printf 'installer did not create the dedicated custom data directory\n'
        return 1
    }
    assert_file_contains "$sandbox/arc/install.conf" "^data_dir=$custom_data$" \
        'installer did not persist the validated custom data directory' || return 1
    assert_file_contains "$sandbox/arc/bin/run-arc-node" "--data-dir $custom_data" \
        'generated runner does not use the validated custom data directory' || return 1
}

protected_install_roots_fail_before_side_effects() {
    local sandbox output status unsafe_path
    local protected_roots='/
/var/lib
/usr/local
/srv
/opt
/home
/mnt
/tmp'
    while IFS= read -r unsafe_path; do
        [ -n "$unsafe_path" ] || continue
        new_sandbox
        sandbox="$NEW_SANDBOX"
        output="$sandbox/protected-install-root.out"
        invoke_installer "$sandbox" Linux x86_64 \
            "$TEST_DIR/fixtures/release-v0.8.0.json" '0.8.0' \
            --install-dir "$unsafe_path" --data-dir "$sandbox/safe-data" \
            --no-service --no-auto-update >"$output" 2>&1
        status=$?
        if [ "$status" -eq 0 ]; then
            printf 'installer accepted protected install root: %s\n' "$unsafe_path"
            return 1
        fi
        assert_file_contains "$output" 'Refusing unsafe install directory' \
            "protected install root did not fail at the path guard: $unsafe_path" || return 1
        [ ! -s "$sandbox/curl.log" ] || {
            printf 'protected install root reached release downloads (%s):\n' "$unsafe_path"
            cat "$sandbox/curl.log"
            return 1
        }
    done <<EOF
$protected_roots
EOF

    new_sandbox
    sandbox="$NEW_SANDBOX"
    output="$sandbox/home-install-root.out"
    invoke_installer "$sandbox" Linux x86_64 \
        "$TEST_DIR/fixtures/release-v0.8.0.json" '0.8.0' \
        --install-dir "$sandbox/home" --data-dir "$sandbox/safe-data" \
        --no-service --no-auto-update >"$output" 2>&1
    status=$?
    [ "$status" -ne 0 ] || {
        printf 'installer accepted the target home itself as install root\n'
        return 1
    }
    assert_file_contains "$output" 'Refusing unsafe install directory' \
        'target home did not fail at the protected install-root guard' || return 1
}

symlinked_or_traversal_install_root_fails_closed() {
    local sandbox output status unsafe_path case_name
    for case_name in symlink traversal; do
        new_sandbox
        sandbox="$NEW_SANDBOX"
        if [ "$case_name" = symlink ]; then
            mkdir -p "$sandbox/install-target"
            printf 'must survive\n' > "$sandbox/install-target/user-sentinel"
            ln -s "$sandbox/install-target" "$sandbox/install-link"
            unsafe_path="$sandbox/install-link"
        else
            unsafe_path="$sandbox/safe-parent/../install-root"
        fi
        output="$sandbox/rejected-install-root.out"
        invoke_installer "$sandbox" Linux x86_64 \
            "$TEST_DIR/fixtures/release-v0.8.0.json" '0.8.0' \
            --install-dir "$unsafe_path" --data-dir "$sandbox/safe-data" \
            --no-service --no-auto-update >"$output" 2>&1
        status=$?
        [ "$status" -ne 0 ] || {
            printf 'installer accepted %s install root: %s\n' "$case_name" "$unsafe_path"
            return 1
        }
        [ ! -s "$sandbox/curl.log" ] || {
            printf '%s install root reached release downloads\n' "$case_name"
            return 1
        }
        if [ "$case_name" = symlink ]; then
            assert_file_contains "$output" 'symlink component' \
                'symlink install root did not fail at component validation' || return 1
            assert_file_contains "$sandbox/install-target/user-sentinel" '^must survive$' \
                'symlink rejection changed the target directory' || return 1
        else
            assert_file_contains "$output" "must not contain '\\.' or '\\.\.' components" \
                'traversal install root did not fail at lexical validation' || return 1
        fi
    done
}

existing_unmarked_install_root_is_never_claimed_or_purged() {
    local sandbox output status before_mode
    new_sandbox
    sandbox="$NEW_SANDBOX"
    mkdir -p "$sandbox/arc"
    printf 'unrelated user data\n' > "$sandbox/arc/user-sentinel"
    mkdir -p "$sandbox/arc/bin"
    printf 'unrelated executable bytes\n' >"$sandbox/arc/bin/arc-node"
    chmod 751 "$sandbox/arc"
    before_mode="$(file_mode "$sandbox/arc")"

    output="$sandbox/unmarked-install.out"
    invoke_installer "$sandbox" Linux x86_64 \
        "$TEST_DIR/fixtures/release-v0.8.0.json" '0.8.0' \
        --no-service --no-auto-update >"$output" 2>&1
    status=$?
    [ "$status" -ne 0 ] || {
        printf 'installer claimed a pre-existing unmarked directory\n'
        return 1
    }
    assert_file_contains "$output" 'Refusing unmarked install directory' \
        'pre-existing unmarked directory did not fail at ownership validation' || return 1
    assert_equals "$before_mode" "$(file_mode "$sandbox/arc")" \
        'unmarked install rejection changed directory permissions' || return 1
    assert_file_contains "$sandbox/arc/user-sentinel" '^unrelated user data$' \
        'unmarked install rejection changed user data' || return 1
    [ ! -e "$sandbox/arc/.arc-chain-install-root" ] || {
        printf 'installer planted an ownership marker in an existing directory\n'
        return 1
    }
    [ ! -s "$sandbox/curl.log" ] || {
        printf 'unmarked install root reached release downloads\n'
        return 1
    }

    output="$sandbox/unmarked-uninstall.out"
    invoke_installer "$sandbox" Linux x86_64 \
        "$TEST_DIR/fixtures/release-v0.8.0.json" '0.8.0' \
        --uninstall --no-service --no-auto-update >"$output" 2>&1
    status=$?
    [ "$status" -ne 0 ] || {
        printf 'uninstall accepted a pre-existing unmarked directory\n'
        return 1
    }
    assert_file_contains "$sandbox/arc/bin/arc-node" '^unrelated executable bytes$' \
        'unmarked uninstall removed a lookalike executable' || return 1
    assert_file_contains "$sandbox/arc/user-sentinel" '^unrelated user data$' \
        'unmarked uninstall changed unrelated user data' || return 1

    output="$sandbox/unmarked-purge.out"
    invoke_installer "$sandbox" Linux x86_64 \
        "$TEST_DIR/fixtures/release-v0.8.0.json" '0.8.0' \
        --uninstall --purge --no-service --no-auto-update >"$output" 2>&1
    status=$?
    [ "$status" -ne 0 ] || {
        printf 'purge accepted a pre-existing unmarked directory\n'
        return 1
    }
    assert_file_contains "$output" 'Refusing unmarked install directory' \
        'unmarked purge did not fail before recursive deletion' || return 1
    assert_file_contains "$sandbox/arc/user-sentinel" '^unrelated user data$' \
        'unmarked purge deleted or changed user data' || return 1
    assert_equals "$before_mode" "$(file_mode "$sandbox/arc")" \
        'unmarked purge changed directory permissions' || return 1
}

marked_install_root_purges_only_its_bound_tree() {
    local sandbox output marker
    new_sandbox
    sandbox="$NEW_SANDBOX"
    output="$sandbox/marked-install.out"
    invoke_installer "$sandbox" Linux x86_64 \
        "$TEST_DIR/fixtures/release-v0.8.0.json" '0.8.0' \
        --no-service --no-auto-update >"$output" 2>&1 || {
            sed -n '1,140p' "$output"
            return 1
        }
    marker="$sandbox/arc/.arc-chain-install-root"
    [ ! -L "$marker" ] && [ -f "$marker" ] || {
        printf 'fresh install did not create a regular install-root marker\n'
        return 1
    }
    assert_equals \
        "$(printf 'arc-chain-managed-install-root-v1\npath=%s' "$sandbox/arc")" \
        "$(cat "$marker")" \
        'install-root marker is not bound to the exact managed directory' || return 1
    printf 'owned chain bytes\n' > "$sandbox/arc/data/owned-sentinel"
    printf 'outside user data\n' > "$sandbox/outside-sentinel"
    : > "$sandbox/curl.log"
    output="$sandbox/marked-purge.out"
    invoke_installer "$sandbox" Linux x86_64 \
        "$TEST_DIR/fixtures/release-v0.8.0.json" '0.8.0' \
        --uninstall --purge --no-service --no-auto-update >"$output" 2>&1 || {
            sed -n '1,160p' "$output"
            return 1
        }
    [ ! -e "$sandbox/arc" ] && [ ! -L "$sandbox/arc" ] || {
        printf 'marked purge did not remove its bound install root\n'
        return 1
    }
    assert_file_contains "$sandbox/outside-sentinel" '^outside user data$' \
        'marked purge changed data outside its bound install root' || return 1
    [ ! -s "$sandbox/curl.log" ] || {
        printf 'uninstall/purge unexpectedly made release requests\n'
        return 1
    }
}

copied_or_symlinked_marker_cannot_authorize_purge() {
    local sandbox output status marker_case foreign_root
    new_sandbox
    sandbox="$NEW_SANDBOX"
    output="$sandbox/source-install.out"
    invoke_installer "$sandbox" Linux x86_64 \
        "$TEST_DIR/fixtures/release-v0.8.0.json" '0.8.0' \
        --no-service --no-auto-update >"$output" 2>&1 || return 1

    for marker_case in copied symlink; do
        foreign_root="$sandbox/foreign-$marker_case"
        mkdir -p "$foreign_root"
        printf 'foreign user data\n' > "$foreign_root/user-sentinel"
        if [ "$marker_case" = copied ]; then
            cp "$sandbox/arc/.arc-chain-install-root" \
                "$foreign_root/.arc-chain-install-root"
        else
            ln -s "$sandbox/arc/.arc-chain-install-root" \
                "$foreign_root/.arc-chain-install-root"
        fi
        output="$sandbox/foreign-$marker_case-purge.out"
        invoke_installer "$sandbox" Linux x86_64 \
            "$TEST_DIR/fixtures/release-v0.8.0.json" '0.8.0' \
            --install-dir "$foreign_root" --data-dir "$foreign_root/data" \
            --uninstall --purge --no-service --no-auto-update >"$output" 2>&1
        status=$?
        [ "$status" -ne 0 ] || {
            printf 'purge accepted a %s marker in an unrelated directory\n' "$marker_case"
            return 1
        }
        assert_file_contains "$foreign_root/user-sentinel" '^foreign user data$' \
            "$marker_case marker purge changed foreign user data" || return 1
        if [ "$marker_case" = copied ]; then
            assert_file_contains "$output" 'not bound to this exact directory' \
                'copied marker did not fail exact-directory binding' || return 1
        else
            assert_file_contains "$output" 'Refusing symlinked ARC install-root marker' \
                'symlink marker did not fail closed' || return 1
        fi
    done
}

write_legacy_v07_fixture() {
    local legacy_root="$1" version="${2:-0.7.11}"
    mkdir -p "$legacy_root/bin" "$legacy_root/data"
    {
        printf '%s\n' '#!/usr/bin/env bash'
        # shellcheck disable=SC2016 # Expansion belongs to the generated fixture.
        printf '%s\n' 'if [ "${1:-}" = --version ]; then'
        printf "    printf 'arc-node %s\\n'\n" "$version"
        printf '%s\n' '    exit 0' 'fi' 'exit 0'
    } > "$legacy_root/bin/arc-node"
    {
        printf '%s\n' '#!/usr/bin/env bash'
        printf '%s\n' '# ARC Chain auto-updater. Checks GitHub for a newer release.'
        printf '%s\n' 'exit 0'
    } > "$legacy_root/bin/arc-auto-update.sh"
    chmod 755 "$legacy_root/bin/arc-node" "$legacy_root/bin/arc-auto-update.sh"
    printf '%s\n' "$version" > "$legacy_root/version.txt"
    printf '%s\n' \
        '# ARC Testnet Seed Nodes' \
        '149.28.32.76:9091 # NYC' > "$legacy_root/seeds.txt"
    printf '%s\n' \
        '# ARC Chain Genesis Configuration' \
        '[chain]' \
        'name = "arc-testnet"' \
        'chain_id = "0x415243"' > "$legacy_root/genesis.toml"
    printf '%s\n' 'community-legacy-host-01020304' > "$legacy_root/identity.seed"
    printf '%s\n' 'legacy chain state must survive' > "$legacy_root/data/state.wal"
    printf '%s\n' 'legacy model bytes must survive' > "$legacy_root/community-model.gguf"
    chmod 755 "$legacy_root" "$legacy_root/bin" "$legacy_root/data"
    chmod 644 "$legacy_root/version.txt" "$legacy_root/seeds.txt" \
        "$legacy_root/genesis.toml" "$legacy_root/identity.seed"
    chmod 600 "$legacy_root/data/state.wal" "$legacy_root/community-model.gguf"
}

prepare_installer_systemd_sandbox() {
    local sandbox="$1"
    mkdir -p "$sandbox/systemd-system"
    sed "s#^SYSTEMD_UNIT_DIR=/etc/systemd/system\$#SYSTEMD_UNIT_DIR=$sandbox/systemd-system#" \
        "$REPO_ROOT/install.sh" >"$sandbox/install-under-test.sh"
    chmod +x "$sandbox/install-under-test.sh"
    INSTALLER_OVERRIDE_UNDER_TEST="$sandbox/install-under-test.sh"
    MOCK_ROOT_OWNED_PREFIX_UNDER_TEST="$sandbox/systemd-system"
    MOCK_SUDO_EXECUTE_UNDER_TEST=true
}

write_legacy_linux_supervisor_fixture() {
    local legacy_root="$1" unit_dir="$2" rpc_port="${3:-18444}" p2p_port="${4:-18445}"
    local model_path="$legacy_root/community-model.gguf"
    mkdir -p "$unit_dir"
    cat >"$unit_dir/arc-node.service" <<EOF
[Unit]
Description=ARC Chain Inference Node
After=network.target

[Service]
Type=simple
User=arc-community-test
WorkingDirectory=$legacy_root
Environment=ARC_DIR=$legacy_root
ExecStart=$legacy_root/bin/arc-node \\
    --rpc 0.0.0.0:$rpc_port \\
    --p2p-port $p2p_port \\
    --seeds-file $legacy_root/seeds.txt \\
    --genesis $legacy_root/genesis.toml \\
    --validator-seed community-legacy-host-01020304 \\
    --stake 0 --min-stake 0 \\
    --eth-rpc-port 0 \\
    --data-dir $legacy_root/data \\
    --model $model_path \\
    --community-mode
Restart=always
RestartSec=5
StandardOutput=append:$legacy_root/node.log
StandardError=append:$legacy_root/node.log

[Install]
WantedBy=multi-user.target
EOF
    cat >"$unit_dir/arc-updater.service" <<EOF
[Unit]
Description=ARC Chain auto-updater (one-shot)

[Service]
Type=oneshot
User=arc-community-test
Environment=ARC_DIR=$legacy_root
ExecStart=$legacy_root/bin/arc-auto-update.sh
EOF
    cat >"$unit_dir/arc-updater.timer" <<'EOF'
[Unit]
Description=ARC Chain auto-updater daily

[Timer]
OnCalendar=*-*-* 04:17:00
Persistent=true

[Install]
WantedBy=timers.target
EOF
    chmod 644 "$unit_dir/arc-node.service" "$unit_dir/arc-updater.service" \
        "$unit_dir/arc-updater.timer"
}

write_legacy_macos_supervisor_fixture() {
    local legacy_root="$1" rpc_port="${2:-18444}" p2p_port="${3:-18445}"
    local agent_dir="$legacy_root/../Library/LaunchAgents"
    mkdir -p "$agent_dir"
    cat >"$agent_dir/com.arc.inference.plist" <<EOF
<?xml version="1.0" encoding="UTF-8"?>
<!DOCTYPE plist PUBLIC "-//Apple//DTD PLIST 1.0//EN" "http://www.apple.com/DTDs/PropertyList-1.0.dtd">
<plist version="1.0">
<dict>
    <key>Label</key><string>com.arc.inference</string>
    <key>ProgramArguments</key>
    <array>
        <string>$legacy_root/bin/arc-node</string>
        <string>--rpc</string><string>0.0.0.0:$rpc_port</string>
        <string>--p2p-port</string><string>$p2p_port</string>
        <string>--seeds-file</string><string>$legacy_root/seeds.txt</string>
        <string>--genesis</string><string>$legacy_root/genesis.toml</string>
        <string>--validator-seed</string><string>community-legacy-host-01020304</string>
        <string>--stake</string><string>0</string>
        <string>--min-stake</string><string>0</string>
        <string>--eth-rpc-port</string><string>0</string>
        <string>--data-dir</string><string>$legacy_root/data</string>
        <string>--model</string><string>$legacy_root/community-model.gguf</string>
        <string>--community-mode</string>
    </array>
    <key>EnvironmentVariables</key>
    <dict>
        <key>ARC_DIR</key><string>$legacy_root</string>
    </dict>
    <key>WorkingDirectory</key><string>$legacy_root</string>
    <key>RunAtLoad</key><true/>
    <key>KeepAlive</key><true/>
    <key>ProcessType</key><string>Background</string>
    <key>Nice</key><integer>15</integer>
    <key>LowPriorityBackgroundIO</key><true/>
    <key>StandardOutPath</key><string>$legacy_root/node.log</string>
    <key>StandardErrorPath</key><string>$legacy_root/node.log</string>
</dict>
</plist>
EOF
    cat >"$agent_dir/com.arc.updater.plist" <<EOF
<?xml version="1.0" encoding="UTF-8"?>
<!DOCTYPE plist PUBLIC "-//Apple//DTD PLIST 1.0//EN" "http://www.apple.com/DTDs/PropertyList-1.0.dtd">
<plist version="1.0">
<dict>
    <key>Label</key><string>com.arc.updater</string>
    <key>ProgramArguments</key>
    <array>
        <string>$legacy_root/bin/arc-auto-update.sh</string>
    </array>
    <key>EnvironmentVariables</key>
    <dict>
        <key>ARC_DIR</key><string>$legacy_root</string>
    </dict>
    <key>StartCalendarInterval</key>
    <dict>
        <key>Hour</key><integer>4</integer>
        <key>Minute</key><integer>17</integer>
    </dict>
    <key>StandardOutPath</key><string>$legacy_root/auto-update.log</string>
    <key>StandardErrorPath</key><string>$legacy_root/auto-update.log</string>
</dict>
</plist>
EOF
    chmod 600 "$agent_dir/com.arc.inference.plist" "$agent_dir/com.arc.updater.plist"
}

reset_legacy_supervisor_test_environment() {
    INSTALLER_OVERRIDE_UNDER_TEST=''
    MOCK_ROOT_OWNED_PREFIX_UNDER_TEST=''
    MOCK_SUDO_EXECUTE_UNDER_TEST=false
    MOCK_SYSTEMD_NODE_ACTIVE_UNDER_TEST=false
    MOCK_SYSTEMD_NODE_ENABLED_UNDER_TEST=false
    MOCK_SYSTEMD_UPDATER_ACTIVE_UNDER_TEST=false
    MOCK_SYSTEMD_UPDATER_ENABLED_UNDER_TEST=false
    MOCK_LEGACY_UPDATER_ACTIVE_UNDER_TEST=false
    MOCK_LEGACY_UPDATER_ENABLED_UNDER_TEST=false
    MOCK_LEGACY_UPDATER_SERVICE_ACTIVE_UNDER_TEST=false
    MOCK_LEGACY_UPDATER_SERVICE_ENABLED_UNDER_TEST=false
    MOCK_LAUNCHD_NODE_LOADED_UNDER_TEST=false
    MOCK_LAUNCHD_UPDATER_LOADED_UNDER_TEST=false
    MOCK_LEGACY_LAUNCHD_NODE_LOADED_UNDER_TEST=false
    MOCK_LEGACY_LAUNCHD_UPDATER_LOADED_UNDER_TEST=false
    MOCK_LAUNCHD_NODE_PID_UNDER_TEST=''
    MOCK_LEGACY_LAUNCHD_NODE_PID_UNDER_TEST=''
    MOCK_PS_COMMAND_UNDER_TEST=''
    MOCK_PS_ALL_ROWS_UNDER_TEST=''
    MOCK_PS_UID_UNDER_TEST=''
    MOCK_SYSTEMD_MAIN_PID_UNDER_TEST=''
    MOCK_SYSTEMD_MAIN_PID_AFTER_UNDER_TEST=''
    MOCK_SYSTEMD_RESTART_DELAY_POLLS_UNDER_TEST=''
    MOCK_REAL_SLEEP_UNDER_TEST=false
    ARC_INSTALL_TEST_FAIL_AFTER_COPY_UNDER_TEST=''
    ARC_INSTALL_TEST_FAIL_AFTER_LEGACY_MARKER_FSYNC_UNDER_TEST=''
    ARC_NODE_VERSION_UNDER_TEST=''
    MOCK_HEALTH_PORT_UNDER_TEST=''
    ARC_HEALTH_TIMEOUT_UNDER_TEST=''
    MOCK_TARGET_UID_UNDER_TEST=''
    MOCK_RELEASE_FIXTURE_DIR_UNDER_TEST=''
    MOCK_V3_RETIREMENT_MODE_UNDER_TEST=''
    MOCK_LEGACY_LISTENER_OPEN_UNDER_TEST=''
    MOCK_RETIREMENT_CREATE_FAIL_UNDER_TEST=''
    MOCK_RETIREMENT_FINALIZE_FAIL_UNDER_TEST=''
}

legacy_default_adoption_preserves_state_config_model_and_identity() {
    local sandbox legacy_root output status host_uid data_hash model_hash
    local version_hash seeds_hash genesis_hash identity_hash legacy_address
    new_sandbox
    sandbox="$NEW_SANDBOX"
    legacy_root="$sandbox/home/.arc"
    write_legacy_v07_fixture "$legacy_root"
    host_uid="$(id -u)"
    MOCK_TARGET_UID_UNDER_TEST="$host_uid"
    data_hash="$(file_sha256 "$legacy_root/data/state.wal")"
    model_hash="$(file_sha256 "$legacy_root/community-model.gguf")"
    version_hash="$(file_sha256 "$legacy_root/version.txt")"
    seeds_hash="$(file_sha256 "$legacy_root/seeds.txt")"
    genesis_hash="$(file_sha256 "$legacy_root/genesis.toml")"
    identity_hash="$(file_sha256 "$legacy_root/identity.seed")"

    # Force a pre-download failure after adoption reservation. The pending
    # marker must be durable, must not authorize purge, and must resume safely.
    ARC_NODE_VERSION_UNDER_TEST='0.8'
    output="$sandbox/legacy-reservation.out"
    invoke_installer "$sandbox" Linux x86_64 \
        "$TEST_DIR/fixtures/release-v0.8.0.json" '0.8.0' \
        --install-dir "$legacy_root" --model "$legacy_root/community-model.gguf" \
        --no-service --no-auto-update >"$output" 2>&1
    status=$?
    ARC_NODE_VERSION_UNDER_TEST=''
    [ "$status" -ne 0 ] || {
        printf 'legacy reservation failure injection unexpectedly installed\n'
        MOCK_TARGET_UID_UNDER_TEST=''
        return 1
    }
    [ -f "$legacy_root/.arc-chain-legacy-adoption-pending" ] \
        && [ ! -e "$legacy_root/.arc-chain-install-root" ] || {
            printf 'legacy reservation did not leave only the pending marker\n'
            MOCK_TARGET_UID_UNDER_TEST=''
            return 1
        }
    : > "$sandbox/curl.log"
    output="$sandbox/pending-old-data.out"
    invoke_installer "$sandbox" Linux x86_64 \
        "$TEST_DIR/fixtures/release-v0.8.0.json" '0.8.0' \
        --install-dir "$legacy_root" --data-dir "$legacy_root/data" \
        --model "$legacy_root/community-model.gguf" \
        --no-service --no-auto-update >"$output" 2>&1
    status=$?
    [ "$status" -ne 0 ] && [ ! -s "$sandbox/curl.log" ] || {
        printf 'pending adoption allowed rebinding to the v0.7 data directory\n'
        MOCK_TARGET_UID_UNDER_TEST=''
        return 1
    }
    output="$sandbox/pending-purge.out"
    invoke_installer "$sandbox" Linux x86_64 \
        "$TEST_DIR/fixtures/release-v0.8.0.json" '0.8.0' \
        --install-dir "$legacy_root" --uninstall --purge \
        --no-service --no-auto-update >"$output" 2>&1
    status=$?
    [ "$status" -ne 0 ] && [ -f "$legacy_root/data/state.wal" ] || {
        printf 'pending legacy adoption authorized purge or lost state\n'
        MOCK_TARGET_UID_UNDER_TEST=''
        return 1
    }

    output="$sandbox/legacy-upgrade.out"
    invoke_installer "$sandbox" Linux x86_64 \
        "$TEST_DIR/fixtures/release-v0.8.0.json" '0.8.0' \
        --install-dir "$legacy_root" --model "$legacy_root/community-model.gguf" \
        --update-only --no-service --no-auto-update >"$output" 2>&1 || {
            sed -n '1,200p' "$output"
            MOCK_TARGET_UID_UNDER_TEST=''
            return 1
        }
    MOCK_TARGET_UID_UNDER_TEST=''

    [ -f "$legacy_root/.arc-chain-install-root" ] \
        && [ ! -e "$legacy_root/.arc-chain-legacy-adoption-pending" ] || {
            printf 'completed legacy adoption did not promote its ownership marker\n'
            return 1
        }
    assert_equals "$data_hash" "$(file_sha256 "$legacy_root/data/state.wal")" \
        'legacy adoption changed v0.7 chain state' || return 1
    assert_equals "$model_hash" "$(file_sha256 "$legacy_root/community-model.gguf")" \
        'legacy adoption changed the community model' || return 1
    [ -d "$legacy_root/data-v0.8" ] || {
        printf 'legacy adoption did not select a fresh v0.8 data directory\n'
        return 1
    }
    assert_file_contains "$legacy_root/install.conf" "^data_dir=$legacy_root/data-v0.8$" \
        'legacy adoption configured the old v0.7 data directory' || return 1
    assert_file_contains "$legacy_root/install.conf" \
        "^model_path=$legacy_root/community-model.gguf$" \
        'legacy adoption did not preserve the selected model path' || return 1
    [ -f "$legacy_root/identity/validator-key.json" ] \
        && [ ! -e "$legacy_root/identity/validator-seed" ] \
        && [ ! -e "$legacy_root/node.env" ] || {
            printf 'legacy adoption did not retire active seed/env identity material\n'
            return 1
        }
    legacy_address="$(printf '%s\n' "$(sed -n '1p' "$legacy_root/legacy-v0.7-preserved/identity.seed")" | file_sha256 /dev/stdin)"
    assert_file_contains "$legacy_root/identity/validator-key.json" \
        "\"address\": \"$legacy_address\"" \
        'legacy adoption did not preserve the exact community address' || return 1
    assert_equals 600 "$(file_mode "$legacy_root/identity/validator-key.json")" \
        'legacy validator keyfile permissions are not 0600' || return 1
    assert_equals "$version_hash" \
        "$(file_sha256 "$legacy_root/legacy-v0.7-preserved/version.txt")" \
        'legacy adoption did not preserve version.txt' || return 1
    assert_equals "$seeds_hash" \
        "$(file_sha256 "$legacy_root/legacy-v0.7-preserved/seeds.txt")" \
        'legacy adoption did not preserve seeds.txt' || return 1
    assert_equals "$genesis_hash" \
        "$(file_sha256 "$legacy_root/legacy-v0.7-preserved/genesis.toml")" \
        'legacy adoption did not preserve genesis.toml' || return 1
    assert_equals "$identity_hash" \
        "$(file_sha256 "$legacy_root/legacy-v0.7-preserved/identity.seed")" \
        'legacy adoption did not preserve identity.seed' || return 1
    assert_file_contains "$output" 'Adopted the verified v0\.7\.11 default install' \
        'legacy adoption did not report the verified source version' || return 1
}

legacy_adoption_refuses_custom_and_hostile_lookalikes() {
    local sandbox legacy_root model_path output status host_uid case_name
    host_uid="$(id -u)"
    for case_name in custom version-mismatch symlink model-symlink-ancestor writable-ancestor; do
        new_sandbox
        sandbox="$NEW_SANDBOX"
        if [ "$case_name" = custom ]; then
            legacy_root="$sandbox/custom-legacy"
        else
            legacy_root="$sandbox/home/.arc"
        fi
        write_legacy_v07_fixture "$legacy_root"
        model_path="$legacy_root/community-model.gguf"
        case "$case_name" in
            version-mismatch) printf '%s\n' '0.7.10' > "$legacy_root/version.txt" ;;
            symlink)
                mv "$legacy_root/genesis.toml" "$sandbox/legacy-genesis.toml"
                ln -s "$sandbox/legacy-genesis.toml" "$legacy_root/genesis.toml" ;;
            model-symlink-ancestor)
                mkdir -p "$sandbox/legacy-models"
                mv "$legacy_root/community-model.gguf" \
                    "$sandbox/legacy-models/community-model.gguf"
                ln -s "$sandbox/legacy-models" "$legacy_root/models"
                model_path="$legacy_root/models/community-model.gguf" ;;
            writable-ancestor) chmod 777 "$sandbox/home" ;;
        esac
        MOCK_TARGET_UID_UNDER_TEST="$host_uid"
        output="$sandbox/hostile-$case_name.out"
        invoke_installer "$sandbox" Linux x86_64 \
            "$TEST_DIR/fixtures/release-v0.8.0.json" '0.8.0' \
            --install-dir "$legacy_root" --model "$model_path" \
            --no-service --no-auto-update >"$output" 2>&1
        status=$?
        MOCK_TARGET_UID_UNDER_TEST=''
        [ "$status" -ne 0 ] || {
            printf 'legacy adoption accepted hostile %s lookalike\n' "$case_name"
            return 1
        }
        [ -f "$legacy_root/data/state.wal" ] || {
            printf 'hostile %s refusal lost legacy-looking user state\n' "$case_name"
            return 1
        }
        [ ! -e "$legacy_root/.arc-chain-install-root" ] \
            && [ ! -e "$legacy_root/.arc-chain-legacy-adoption-pending" ] || {
                printf 'hostile %s lookalike received an ARC ownership marker\n' "$case_name"
                return 1
            }
        [ ! -s "$sandbox/curl.log" ] || {
            printf 'hostile %s lookalike reached release downloads\n' "$case_name"
            return 1
        }
    done
}

legacy_linux_system_supervisor_is_transactionally_adopted() {
    local sandbox legacy_root unit_dir output host_uid old_node_unit_hash old_updater_hash
    reset_legacy_supervisor_test_environment
    new_sandbox
    sandbox="$NEW_SANDBOX"
    legacy_root="$sandbox/home/.arc"
    unit_dir="$sandbox/systemd-system"
    write_legacy_v07_fixture "$legacy_root"
    prepare_installer_systemd_sandbox "$sandbox"
    write_legacy_linux_supervisor_fixture "$legacy_root" "$unit_dir" 18444 18445
    old_node_unit_hash="$(file_sha256 "$unit_dir/arc-node.service")"
    old_updater_hash="$(file_sha256 "$unit_dir/arc-updater.service")"
    host_uid="$(id -u)"
    MOCK_TARGET_UID_UNDER_TEST="$host_uid"
    MOCK_SYSTEMD_NODE_ACTIVE_UNDER_TEST=true
    MOCK_SYSTEMD_MAIN_PID_UNDER_TEST=999999
    MOCK_SYSTEMD_NODE_ENABLED_UNDER_TEST=true
    MOCK_LEGACY_UPDATER_ACTIVE_UNDER_TEST=true
    MOCK_LEGACY_UPDATER_ENABLED_UNDER_TEST=true
    MOCK_LEGACY_UPDATER_SERVICE_ACTIVE_UNDER_TEST=true
    MOCK_LEGACY_UPDATER_SERVICE_ENABLED_UNDER_TEST=true
    MOCK_HEALTH_PORT_UNDER_TEST=18444
    ARC_HEALTH_TIMEOUT_UNDER_TEST=4
    output="$sandbox/linux-legacy-adoption.out"
    if ! invoke_installer "$sandbox" Linux x86_64 \
        "$TEST_DIR/fixtures/release-v0.8.0.json" 0.8.0 \
        --install-dir "$legacy_root" >"$output" 2>&1; then
        sed -n '1,240p' "$output"
        reset_legacy_supervisor_test_environment
        return 1
    fi
    reset_legacy_supervisor_test_environment

    [ -f "$legacy_root/.arc-chain-install-root" ] \
        && [ ! -e "$legacy_root/.arc-chain-legacy-adoption-pending" ] \
        || { printf 'Linux supervisor adoption did not commit its final marker\n'; return 1; }
    [ ! -e "$unit_dir/arc-updater.service" ] \
        && [ ! -e "$unit_dir/arc-updater.timer" ] \
        && [ ! -e "$legacy_root/bin/arc-auto-update.sh" ] \
        || { printf 'legacy Linux updater can still relaunch after commit\n'; return 1; }
    assert_file_contains "$unit_dir/arc-node.service" \
        '^# ARC managed system-user node unit v1$' \
        'legacy Linux node unit was not replaced by the managed bridge' || return 1
    assert_file_contains "$unit_dir/arc-node.service" '^User=arc-community-test$' \
        'managed Linux node does not run as the community user' || return 1
    assert_file_contains "$unit_dir/arc-node.service" \
        "^ExecStart=\"$legacy_root/bin/run-arc-node\" " \
        'managed Linux node unit still targets the legacy executable' || return 1
    assert_file_contains "$unit_dir/arc-node-update.service" '^User=arc-community-test$' \
        'managed Linux updater does not run as the community user' || return 1
    assert_file_contains "$unit_dir/arc-node-update.service" \
        '--service-scope" "system-user"' \
        'managed Linux updater did not persist its bounded system-user scope' || return 1
    assert_file_contains "$legacy_root/install.conf" '^service_scope=system-user$' \
        'Linux adoption did not persist the system-user bridge' || return 1
    assert_file_contains "$legacy_root/install.conf" '^rpc_port=18444$' \
        'Linux adoption did not retain the legacy RPC port' || return 1
    assert_file_contains "$legacy_root/install.conf" '^p2p_port=18445$' \
        'Linux adoption did not retain the legacy P2P port' || return 1
    assert_file_contains "$legacy_root/install.conf" \
        "^model_path=$legacy_root/community-model.gguf$" \
        'Linux adoption did not retain the active legacy model' || return 1
    assert_equals "$old_node_unit_hash" \
        "$(file_sha256 "$legacy_root/legacy-v0.7-preserved/legacy-linux-arc-node.service")" \
        'Linux adoption did not archive the exact prior node unit' || return 1
    assert_equals "$old_updater_hash" \
        "$(file_sha256 "$legacy_root/legacy-v0.7-preserved/legacy-linux-arc-updater.service")" \
        'Linux adoption did not archive the exact prior updater unit' || return 1
    assert_file_contains "$sandbox/systemd-state/arc-updater.timer.active" '^false$' \
        'legacy Linux updater timer remained active after commit' || return 1
    assert_file_contains "$sandbox/systemd-state/arc-updater.timer.enabled" '^false$' \
        'legacy Linux updater timer remained enabled after commit' || return 1
    assert_file_contains "$sandbox/systemd-state/arc-updater.service.active" '^false$' \
        'legacy Linux updater service remained active after commit' || return 1
    assert_file_contains "$sandbox/systemd-state/arc-updater.service.enabled" '^false$' \
        'legacy Linux updater service remained enabled after commit' || return 1
}

legacy_linux_post_intent_failure_restores_files_but_stays_stopped() {
    local sandbox legacy_root unit_dir output status host_uid
    local binary_hash node_unit_hash updater_service_hash updater_timer_hash updater_script_hash
    reset_legacy_supervisor_test_environment
    new_sandbox
    sandbox="$NEW_SANDBOX"
    legacy_root="$sandbox/home/.arc"
    unit_dir="$sandbox/systemd-system"
    write_legacy_v07_fixture "$legacy_root"
    prepare_installer_systemd_sandbox "$sandbox"
    write_legacy_linux_supervisor_fixture "$legacy_root" "$unit_dir" 18444 18445
    binary_hash="$(file_sha256 "$legacy_root/bin/arc-node")"
    node_unit_hash="$(file_sha256 "$unit_dir/arc-node.service")"
    updater_service_hash="$(file_sha256 "$unit_dir/arc-updater.service")"
    updater_timer_hash="$(file_sha256 "$unit_dir/arc-updater.timer")"
    updater_script_hash="$(file_sha256 "$legacy_root/bin/arc-auto-update.sh")"
    host_uid="$(id -u)"
    MOCK_TARGET_UID_UNDER_TEST="$host_uid"
    MOCK_SYSTEMD_NODE_ACTIVE_UNDER_TEST=true
    MOCK_SYSTEMD_MAIN_PID_UNDER_TEST=999999
    MOCK_SYSTEMD_NODE_ENABLED_UNDER_TEST=false
    MOCK_LEGACY_UPDATER_ACTIVE_UNDER_TEST=false
    MOCK_LEGACY_UPDATER_ENABLED_UNDER_TEST=true
    MOCK_LEGACY_UPDATER_SERVICE_ACTIVE_UNDER_TEST=true
    MOCK_LEGACY_UPDATER_SERVICE_ENABLED_UNDER_TEST=false
    ARC_INSTALL_TEST_FAIL_AFTER_COPY_UNDER_TEST=1
    output="$sandbox/linux-legacy-rollback.out"
    invoke_installer "$sandbox" Linux x86_64 \
        "$TEST_DIR/fixtures/release-v0.8.0.json" 0.8.0 \
        --install-dir "$legacy_root" >"$output" 2>&1
    status=$?
    reset_legacy_supervisor_test_environment
    [ "$status" -ne 0 ] \
        || { printf 'injected Linux legacy migration failure unexpectedly committed\n'; return 1; }

    assert_equals "$binary_hash" "$(file_sha256 "$legacy_root/bin/arc-node")" \
        'legacy rollback did not restore the exact v0.7 binary' || return 1
    assert_equals "$node_unit_hash" "$(file_sha256 "$unit_dir/arc-node.service")" \
        'legacy rollback did not restore the exact node unit' || return 1
    assert_equals "$updater_service_hash" "$(file_sha256 "$unit_dir/arc-updater.service")" \
        'legacy rollback did not restore the exact updater service' || return 1
    assert_equals "$updater_timer_hash" "$(file_sha256 "$unit_dir/arc-updater.timer")" \
        'legacy rollback did not restore the exact updater timer' || return 1
    assert_equals "$updater_script_hash" \
        "$(file_sha256 "$legacy_root/bin/arc-auto-update.sh")" \
        'legacy rollback did not restore the exact updater executable' || return 1
    assert_file_contains "$sandbox/systemd-state/arc-node.service.active" '^false$' \
        'post-intent rollback restarted the retired v0.7 node' || return 1
    assert_file_contains "$sandbox/systemd-state/arc-node.service.enabled" '^false$' \
        'legacy rollback did not restore disabled node state' || return 1
    assert_file_contains "$sandbox/systemd-state/arc-updater.timer.active" '^false$' \
        'legacy rollback did not restore inactive updater timer state' || return 1
    assert_file_contains "$sandbox/systemd-state/arc-updater.timer.enabled" '^false$' \
        'legacy rollback re-armed the unsigned updater timer' || return 1
    assert_file_contains "$sandbox/systemd-state/arc-updater.service.active" '^false$' \
        'legacy rollback restarted the unsigned updater service' || return 1
    assert_file_contains "$sandbox/systemd-state/arc-updater.service.enabled" '^false$' \
        'legacy rollback enabled the unsigned updater service' || return 1
    [ -f "$legacy_root/.arc-chain-legacy-adoption-pending" ] \
        && [ ! -e "$legacy_root/.arc-chain-install-root" ] \
        || { printf 'failed migration enabled purge before commit\n'; return 1; }
    [ -f "$legacy_root/legacy-v0.7-preserved/retirement-v0.8/retirement-intent.json" ] \
        && [ -f "$legacy_root/legacy-v0.7-preserved/retirement-v0.8/retirement-receipt.json" ] \
        || { printf 'post-intent rollback lost its create-only retirement evidence\n'; return 1; }
    assert_file_contains "$output" \
        'v0\.7 remains stopped and every legacy updater stays fenced behind the durable retirement intent' \
        'post-intent Linux failure did not report its fail-stopped rollback' || return 1
}

legacy_retirement_failures_obey_the_intent_boundary() {
    local sandbox legacy_root unit_dir output status host_uid binary_hash

    # A create-intent refusal is still before the irreversible boundary: the
    # exact v0.7 node may be restored, but its unsigned updater stays fenced.
    reset_legacy_supervisor_test_environment
    new_sandbox
    sandbox="$NEW_SANDBOX"
    legacy_root="$sandbox/home/.arc"
    unit_dir="$sandbox/systemd-system"
    write_legacy_v07_fixture "$legacy_root"
    prepare_installer_systemd_sandbox "$sandbox"
    write_legacy_linux_supervisor_fixture "$legacy_root" "$unit_dir" 18444 18445
    binary_hash="$(file_sha256 "$legacy_root/bin/arc-node")"
    host_uid="$(id -u)"
    MOCK_TARGET_UID_UNDER_TEST="$host_uid"
    MOCK_SYSTEMD_NODE_ACTIVE_UNDER_TEST=true
    MOCK_SYSTEMD_MAIN_PID_UNDER_TEST=999999
    MOCK_SYSTEMD_NODE_ENABLED_UNDER_TEST=true
    MOCK_RETIREMENT_CREATE_FAIL_UNDER_TEST=1
    output="$sandbox/intent-create-failure.out"
    invoke_installer "$sandbox" Linux x86_64 \
        "$TEST_DIR/fixtures/release-v0.8.0.json" 0.8.0 \
        --install-dir "$legacy_root" >"$output" 2>&1
    status=$?
    [ "$status" -ne 0 ] || { printf 'retirement intent failure unexpectedly committed\n'; return 1; }
    assert_equals "$binary_hash" "$(file_sha256 "$legacy_root/bin/arc-node")" \
        'create-intent failure changed the v0.7 executable' || return 1
    [ ! -e "$legacy_root/legacy-v0.7-preserved/retirement-v0.8/retirement-intent.json" ] \
        || { printf 'failed create-intent published a retirement intent\n'; return 1; }
    assert_file_contains "$sandbox/systemd-state/arc-node.service.active" '^true$' \
        'pre-intent failure did not restore the previously active v0.7 node' || return 1
    assert_file_contains "$sandbox/systemd-state/arc-updater.timer.active" '^false$' \
        'pre-intent failure rearmed the unsigned updater' || return 1

    # Once intent publication succeeds, a finalizer refusal must restore files
    # but leave v0.7 stopped. No receipt and no v0.8 state may appear.
    reset_legacy_supervisor_test_environment
    new_sandbox
    sandbox="$NEW_SANDBOX"
    legacy_root="$sandbox/home/.arc"
    unit_dir="$sandbox/systemd-system"
    write_legacy_v07_fixture "$legacy_root"
    prepare_installer_systemd_sandbox "$sandbox"
    write_legacy_linux_supervisor_fixture "$legacy_root" "$unit_dir" 18444 18445
    binary_hash="$(file_sha256 "$legacy_root/bin/arc-node")"
    MOCK_TARGET_UID_UNDER_TEST="$host_uid"
    MOCK_SYSTEMD_NODE_ACTIVE_UNDER_TEST=true
    MOCK_SYSTEMD_MAIN_PID_UNDER_TEST=999999
    MOCK_SYSTEMD_NODE_ENABLED_UNDER_TEST=true
    MOCK_RETIREMENT_FINALIZE_FAIL_UNDER_TEST=1
    output="$sandbox/retirement-finalize-failure.out"
    invoke_installer "$sandbox" Linux x86_64 \
        "$TEST_DIR/fixtures/release-v0.8.0.json" 0.8.0 \
        --install-dir "$legacy_root" >"$output" 2>&1
    status=$?
    reset_legacy_supervisor_test_environment
    [ "$status" -ne 0 ] || { printf 'retirement finalizer failure unexpectedly committed\n'; return 1; }
    assert_equals "$binary_hash" "$(file_sha256 "$legacy_root/bin/arc-node")" \
        'post-intent finalizer failure did not restore the exact v0.7 executable' || return 1
    [ -f "$legacy_root/legacy-v0.7-preserved/retirement-v0.8/retirement-intent.json" ] \
        && [ -f "$legacy_root/legacy-v0.7-preserved/retirement-v0.8/stop-evidence.json" ] \
        && [ ! -e "$legacy_root/legacy-v0.7-preserved/retirement-v0.8/retirement-receipt.json" ] \
        && [ ! -e "$legacy_root/data-v0.8" ] \
        || { printf 'finalizer failure crossed the receipt-authorized state boundary\n'; return 1; }
    assert_file_contains "$sandbox/systemd-state/arc-node.service.active" '^false$' \
        'post-intent finalizer failure restarted v0.7' || return 1
    assert_file_contains "$output" \
        'v0\.7 remains stopped and every legacy updater stays fenced behind the durable retirement intent' \
        'post-intent finalizer failure did not report fail-stopped recovery' || return 1
}

legacy_updater_is_fenced_before_release_failure_and_stays_fenced() {
    local sandbox legacy_root unit_dir fixture output status host_uid
    reset_legacy_supervisor_test_environment
    new_sandbox
    sandbox="$NEW_SANDBOX"
    legacy_root="$sandbox/home/.arc"
    unit_dir="$sandbox/systemd-system"
    write_legacy_v07_fixture "$legacy_root"
    prepare_installer_systemd_sandbox "$sandbox"
    write_legacy_linux_supervisor_fixture "$legacy_root" "$unit_dir" 18444 18445
    fixture="$sandbox/untrusted-release.json"
    sed 's/"immutable": true/"immutable": false/' \
        "$TEST_DIR/fixtures/release-v0.8.0.json" > "$fixture"
    host_uid="$(id -u)"
    MOCK_TARGET_UID_UNDER_TEST="$host_uid"
    MOCK_SYSTEMD_NODE_ACTIVE_UNDER_TEST=true
    MOCK_SYSTEMD_MAIN_PID_UNDER_TEST=999999
    MOCK_SYSTEMD_NODE_ENABLED_UNDER_TEST=true
    MOCK_LEGACY_UPDATER_ACTIVE_UNDER_TEST=true
    MOCK_LEGACY_UPDATER_ENABLED_UNDER_TEST=true
    MOCK_LEGACY_UPDATER_SERVICE_ACTIVE_UNDER_TEST=true
    MOCK_LEGACY_UPDATER_SERVICE_ENABLED_UNDER_TEST=false
    output="$sandbox/pre-network-fence.out"
    invoke_installer "$sandbox" Linux x86_64 "$fixture" 0.8.0 \
        --install-dir "$legacy_root" >"$output" 2>&1
    status=$?
    reset_legacy_supervisor_test_environment
    [ "$status" -ne 0 ] \
        || { printf 'untrusted release unexpectedly crossed the migration fence\n'; return 1; }
    assert_file_contains "$sandbox/systemd-state/arc-updater.timer.active" '^false$' \
        'pre-download failure restarted the legacy updater timer' || return 1
    assert_file_contains "$sandbox/systemd-state/arc-updater.timer.enabled" '^false$' \
        'pre-download failure re-enabled the legacy updater timer' || return 1
    assert_file_contains "$sandbox/systemd-state/arc-updater.service.active" '^false$' \
        'pre-download failure restarted the legacy updater service' || return 1
    assert_file_contains "$sandbox/systemd-state/arc-updater.service.enabled" '^false$' \
        'pre-download failure changed legacy updater-service enablement' || return 1
    if grep -Fq 'set-property --runtime arc-node.service' "$sandbox/service.log" \
        || grep -Fq 'disable --now arc-node.service' "$sandbox/service.log"; then
        printf 'release rejection stopped the legacy node instead of only fencing its updater\n'
        return 1
    fi
    assert_file_contains "$legacy_root/bin/arc-node" 'arc-node 0\.7\.11' \
        'release rejection replaced the v0.7 node binary' || return 1
    assert_file_contains "$legacy_root/data/state.wal" '^legacy chain state must survive$' \
        'release rejection changed legacy chain state' || return 1
    [ -f "$legacy_root/.arc-chain-legacy-adoption-pending" ] \
        && [ -d "$legacy_root/legacy-v0.7-preserved" ] \
        || { printf 'release rejection did not retain archive-first migration evidence\n'; return 1; }
}

legacy_retirement_gate_fails_before_v07_stop() {
    local gate_case sandbox legacy_root unit_dir output status host_uid
    local binary_hash data_hash
    for gate_case in offline recovery-inactive low-height split-manifest \
        split-network-genesis \
        wrong-validator old-node duplicate-field leading-zero-height \
        huge-height exponent-height legacy-listener
    do
        reset_legacy_supervisor_test_environment
        new_sandbox
        sandbox="$NEW_SANDBOX"
        legacy_root="$sandbox/home/.arc"
        unit_dir="$sandbox/systemd-system"
        write_legacy_v07_fixture "$legacy_root"
        prepare_installer_systemd_sandbox "$sandbox"
        write_legacy_linux_supervisor_fixture "$legacy_root" "$unit_dir" 18444 18445
        binary_hash="$(file_sha256 "$legacy_root/bin/arc-node")"
        data_hash="$(file_sha256 "$legacy_root/data/state.wal")"
        host_uid="$(id -u)"
        MOCK_TARGET_UID_UNDER_TEST="$host_uid"
        MOCK_SYSTEMD_NODE_ACTIVE_UNDER_TEST=true
        MOCK_SYSTEMD_NODE_ENABLED_UNDER_TEST=true
        MOCK_LEGACY_UPDATER_ACTIVE_UNDER_TEST=true
        MOCK_LEGACY_UPDATER_ENABLED_UNDER_TEST=true
        MOCK_LEGACY_UPDATER_SERVICE_ACTIVE_UNDER_TEST=true
        MOCK_LEGACY_UPDATER_SERVICE_ENABLED_UNDER_TEST=false
        if [ "$gate_case" = legacy-listener ]; then
            MOCK_V3_RETIREMENT_MODE_UNDER_TEST=ok
            MOCK_LEGACY_LISTENER_OPEN_UNDER_TEST=all
        else
            MOCK_V3_RETIREMENT_MODE_UNDER_TEST="$gate_case"
        fi
        output="$sandbox/retirement-gate-$gate_case.out"
        invoke_installer "$sandbox" Linux x86_64 \
            "$TEST_DIR/fixtures/release-v0.8.0.json" 0.8.0 \
            --install-dir "$legacy_root" >"$output" 2>&1
        status=$?
        reset_legacy_supervisor_test_environment
        [ "$status" -ne 0 ] || {
            printf 'legacy retirement gate accepted unsafe evidence: %s\n' "$gate_case"
            return 1
        }
        assert_equals "$binary_hash" "$(file_sha256 "$legacy_root/bin/arc-node")" \
            "retirement gate $gate_case replaced the v0.7 node" || return 1
        assert_equals "$data_hash" "$(file_sha256 "$legacy_root/data/state.wal")" \
            "retirement gate $gate_case changed legacy chain history" || return 1
        if [ -e "$sandbox/systemd-state/arc-node.service.active" ]; then
            assert_file_contains "$sandbox/systemd-state/arc-node.service.active" '^true$' \
                "retirement gate $gate_case stopped the legacy node" || return 1
        fi
        if grep -Fq 'disable --now arc-node.service' "$sandbox/service.log" \
            || grep -Fq 'set-property --runtime arc-node.service' "$sandbox/service.log"; then
            printf 'retirement gate %s crossed the v0.7 stop boundary\n' "$gate_case"
            return 1
        fi
    done
}

installer_distinguishes_v07_retirement_from_v08_quiescence() {
    local gate_line transaction_line
    assert_file_not_contains "$REPO_ROOT/install.sh" \
        'kill -KILL "\$LEGACY_DETACHED_PID"' \
        'legacy detached retirement still force-kills the old process' || return 1
    assert_file_contains "$REPO_ROOT/install.sh" \
        "a long timeout cannot turn v0\.7's immediate-exit handler into a drain" \
        'installer source falsely treats a timed v0.7 TERM as quiescence' || return 1
    assert_file_not_contains "$REPO_ROOT/install.sh" \
        'Legacy detached node did not drain' \
        'legacy detached retirement still claims a drain it cannot prove' || return 1
    [ "$(grep -Fc "'SendSIGKILL=no'" "$REPO_ROOT/install.sh")" -ge 2 ] || {
        printf 'managed systemd node units can still force-kill after TimeoutStopSec\n'
        return 1
    }
    assert_file_contains "$REPO_ROOT/install.sh" \
        'systemctl set-property --runtime arc-node\.service' \
        'legacy systemd migration does not install a no-SIGKILL runtime fence' || return 1
    gate_line="$(grep -n '^verify_legacy_v07_network_retirement$' "$REPO_ROOT/install.sh" \
        | tail -n1 | cut -d: -f1)"
    transaction_line="$(grep -n '^begin_install_transaction$' "$REPO_ROOT/install.sh" \
        | tail -n1 | cut -d: -f1)"
    [ -n "$gate_line" ] && [ -n "$transaction_line" ] \
        && [ "$gate_line" -lt "$transaction_line" ] || {
        printf 'legacy retirement evidence is not checked before the stop transaction\n'
        return 1
    }
}

legacy_marker_fsync_crash_keeps_v07_runnable_and_resumes() {
    local sandbox legacy_root unit_dir output status host_uid
    local binary_hash node_unit_hash updater_service_hash updater_timer_hash data_hash
    reset_legacy_supervisor_test_environment
    new_sandbox
    sandbox="$NEW_SANDBOX"
    legacy_root="$sandbox/home/.arc"
    unit_dir="$sandbox/systemd-system"
    write_legacy_v07_fixture "$legacy_root"
    prepare_installer_systemd_sandbox "$sandbox"
    write_legacy_linux_supervisor_fixture "$legacy_root" "$unit_dir" 18444 18445
    binary_hash="$(file_sha256 "$legacy_root/bin/arc-node")"
    node_unit_hash="$(file_sha256 "$unit_dir/arc-node.service")"
    updater_service_hash="$(file_sha256 "$unit_dir/arc-updater.service")"
    updater_timer_hash="$(file_sha256 "$unit_dir/arc-updater.timer")"
    data_hash="$(file_sha256 "$legacy_root/data/state.wal")"
    host_uid="$(id -u)"
    MOCK_TARGET_UID_UNDER_TEST="$host_uid"
    MOCK_SYSTEMD_NODE_ACTIVE_UNDER_TEST=true
    MOCK_SYSTEMD_MAIN_PID_UNDER_TEST=999999
    MOCK_SYSTEMD_NODE_ENABLED_UNDER_TEST=true
    MOCK_LEGACY_UPDATER_ACTIVE_UNDER_TEST=true
    MOCK_LEGACY_UPDATER_ENABLED_UNDER_TEST=true
    ARC_INSTALL_TEST_FAIL_AFTER_LEGACY_MARKER_FSYNC_UNDER_TEST=1
    output="$sandbox/legacy-marker-fsync-crash.out"
    invoke_installer "$sandbox" Linux x86_64 \
        "$TEST_DIR/fixtures/release-v0.8.0.json" 0.8.0 \
        --install-dir "$legacy_root" >"$output" 2>&1
    status=$?
    ARC_INSTALL_TEST_FAIL_AFTER_LEGACY_MARKER_FSYNC_UNDER_TEST=''
    [ "$status" -ne 0 ] \
        || { printf 'legacy marker fsync crash injection unexpectedly committed\n'; return 1; }
    [ -f "$legacy_root/.arc-chain-legacy-adoption-pending" ] \
        && [ -d "$legacy_root/legacy-v0.7-preserved" ] \
        && [ ! -e "$legacy_root/.arc-chain-install-root" ] \
        || { printf 'marker fsync crash did not retain archive-first pending evidence\n'; return 1; }
    assert_file_contains "$output" \
        'Injected failure after durable legacy-adoption marker publication' \
        'marker fsync crash did not stop at the exact injected boundary' || return 1
    if grep -Fq 'disable --now arc-node.service' "$sandbox/service.log" \
        || grep -Fq 'stop arc-node.service' "$sandbox/service.log"; then
        printf 'marker fsync crash changed the running legacy node supervisor\n'
        return 1
    fi
    assert_file_contains "$sandbox/systemd-state/arc-updater.timer.active" '^false$' \
        'marker fsync crash restarted the unsigned updater timer' || return 1
    assert_file_contains "$sandbox/systemd-state/arc-updater.timer.enabled" '^false$' \
        'marker fsync crash re-enabled the unsigned updater timer' || return 1
    assert_equals "$binary_hash" "$(file_sha256 "$legacy_root/bin/arc-node")" \
        'marker fsync crash changed the runnable v0.7 binary' || return 1
    assert_equals "$node_unit_hash" "$(file_sha256 "$unit_dir/arc-node.service")" \
        'marker fsync crash changed the live v0.7 node unit' || return 1
    assert_equals "$updater_service_hash" "$(file_sha256 "$unit_dir/arc-updater.service")" \
        'marker fsync crash changed the live v0.7 updater service' || return 1
    assert_equals "$updater_timer_hash" "$(file_sha256 "$unit_dir/arc-updater.timer")" \
        'marker fsync crash changed the live v0.7 updater timer' || return 1
    [ ! -s "$sandbox/curl.log" ] \
        || { printf 'marker fsync crash reached release downloads\n'; return 1; }

    # The next invocation must bind to the durable marker, reconstruct the
    # exact archive from the still-live sources, and commit only after the new
    # service is healthy on its preserved RPC port.
    MOCK_HEALTH_PORT_UNDER_TEST=18444
    ARC_HEALTH_TIMEOUT_UNDER_TEST=4
    output="$sandbox/legacy-marker-fsync-resume.out"
    if ! invoke_installer "$sandbox" Linux x86_64 \
        "$TEST_DIR/fixtures/release-v0.8.0.json" 0.8.0 \
        --install-dir "$legacy_root" --update-only >"$output" 2>&1; then
        sed -n '1,240p' "$output"
        reset_legacy_supervisor_test_environment
        return 1
    fi
    reset_legacy_supervisor_test_environment
    [ -f "$legacy_root/.arc-chain-install-root" ] \
        && [ ! -e "$legacy_root/.arc-chain-legacy-adoption-pending" ] \
        && [ -d "$legacy_root/data-v0.8" ] \
        || { printf 'marker fsync resume did not commit archive plus fresh v0.8 state\n'; return 1; }
    assert_equals "$data_hash" "$(file_sha256 "$legacy_root/data/state.wal")" \
        'marker fsync resume changed preserved v0.7 chain state' || return 1
    assert_equals "$node_unit_hash" \
        "$(file_sha256 "$legacy_root/legacy-v0.7-preserved/legacy-linux-arc-node.service")" \
        'marker fsync resume did not archive the exact live node unit' || return 1
    assert_equals "$updater_service_hash" \
        "$(file_sha256 "$legacy_root/legacy-v0.7-preserved/legacy-linux-arc-updater.service")" \
        'marker fsync resume did not archive the exact live updater service' || return 1
    assert_file_contains "$legacy_root/install.conf" \
        "^data_dir=$legacy_root/data-v0.8$" \
        'marker fsync resume did not bind a fresh v0.8 data directory' || return 1
}

pending_linux_adoption_resumes_only_its_bound_scope() {
    local sandbox legacy_root unit_dir output status host_uid
    reset_legacy_supervisor_test_environment
    new_sandbox
    sandbox="$NEW_SANDBOX"
    legacy_root="$sandbox/home/.arc"
    unit_dir="$sandbox/systemd-system"
    write_legacy_v07_fixture "$legacy_root"
    prepare_installer_systemd_sandbox "$sandbox"
    write_legacy_linux_supervisor_fixture "$legacy_root" "$unit_dir" 18444 18445
    host_uid="$(id -u)"
    MOCK_TARGET_UID_UNDER_TEST="$host_uid"
    ARC_NODE_VERSION_UNDER_TEST=0.8
    output="$sandbox/linux-reservation.out"
    invoke_installer "$sandbox" Linux x86_64 \
        "$TEST_DIR/fixtures/release-v0.8.0.json" 0.8.0 \
        --install-dir "$legacy_root" >"$output" 2>&1
    status=$?
    ARC_NODE_VERSION_UNDER_TEST=''
    [ "$status" -ne 0 ] && [ -f "$legacy_root/.arc-chain-legacy-adoption-pending" ] \
        || { printf 'could not create a pending Linux adoption fixture\n'; reset_legacy_supervisor_test_environment; return 1; }
    assert_file_contains "$legacy_root/.arc-chain-legacy-adoption-pending" \
        '^service_scope=system-user$' 'pending adoption did not bind system-user scope' || return 1

    rm -f "$unit_dir/arc-node.service" "$unit_dir/arc-updater.service" \
        "$unit_dir/arc-updater.timer" "$legacy_root/bin/arc-auto-update.sh"
    {
        printf '%s\n' '#!/usr/bin/env bash'
        # shellcheck disable=SC2016 # Expansion belongs to the generated fixture.
        printf '%s\n' 'if [ "${1:-}" = --version ]; then printf "arc-node 0.8.0\n"; exit 0; fi'
        printf '%s\n' 'exit 0'
    } >"$legacy_root/bin/arc-node"
    chmod 755 "$legacy_root/bin/arc-node"
    : >"$sandbox/curl.log"
    output="$sandbox/linux-wrong-scope-resume.out"
    invoke_installer "$sandbox" Linux x86_64 \
        "$TEST_DIR/fixtures/release-v0.8.0.json" 0.8.0 \
        --install-dir "$legacy_root" --update-only --no-service >"$output" 2>&1
    status=$?
    [ "$status" -ne 0 ] && [ ! -s "$sandbox/curl.log" ] \
        || { printf 'pending adoption accepted a conflicting no-service scope\n'; reset_legacy_supervisor_test_environment; return 1; }
    assert_file_contains "$output" 'bound service scope' \
        'conflicting resume did not explain its bound scope' || return 1

    MOCK_HEALTH_PORT_UNDER_TEST=18444
    ARC_HEALTH_TIMEOUT_UNDER_TEST=4
    output="$sandbox/linux-bound-resume.out"
    if ! invoke_installer "$sandbox" Linux x86_64 \
        "$TEST_DIR/fixtures/release-v0.8.0.json" 0.8.0 \
        --install-dir "$legacy_root" --update-only >"$output" 2>&1; then
        sed -n '1,240p' "$output"
        reset_legacy_supervisor_test_environment
        return 1
    fi
    reset_legacy_supervisor_test_environment
    assert_file_contains "$legacy_root/install.conf" '^service_scope=system-user$' \
        'resumed adoption did not reuse its bound system-user scope' || return 1
    assert_file_contains "$legacy_root/install.conf" '^rpc_port=18444$' \
        'resumed adoption did not reuse its bound legacy RPC port' || return 1
    assert_file_contains "$unit_dir/arc-node.service" '^User=arc-community-test$' \
        'resumed adoption did not recreate the target-user node bridge' || return 1
}

legacy_macos_agents_are_retired_and_replaced() {
    local sandbox legacy_root agent_dir output host_uid
    reset_legacy_supervisor_test_environment
    new_sandbox
    sandbox="$NEW_SANDBOX"
    legacy_root="$sandbox/home/.arc"
    agent_dir="$sandbox/home/Library/LaunchAgents"
    write_legacy_v07_fixture "$legacy_root"
    write_legacy_macos_supervisor_fixture "$legacy_root" 18444 18445
    host_uid="$(id -u)"
    MOCK_TARGET_UID_UNDER_TEST="$host_uid"
    MOCK_LEGACY_LAUNCHD_NODE_LOADED_UNDER_TEST=true
    MOCK_LEGACY_LAUNCHD_UPDATER_LOADED_UNDER_TEST=true
    MOCK_HEALTH_PORT_UNDER_TEST=18444
    ARC_HEALTH_TIMEOUT_UNDER_TEST=4
    output="$sandbox/macos-legacy-adoption.out"
    if ! invoke_installer "$sandbox" Darwin x86_64 \
        "$TEST_DIR/fixtures/release-v0.8.0.json" 0.8.0 \
        --install-dir "$legacy_root" >"$output" 2>&1; then
        sed -n '1,240p' "$output"
        reset_legacy_supervisor_test_environment
        return 1
    fi
    reset_legacy_supervisor_test_environment
    [ ! -e "$agent_dir/com.arc.inference.plist" ] \
        && [ ! -e "$agent_dir/com.arc.updater.plist" ] \
        && [ ! -e "$legacy_root/bin/arc-auto-update.sh" ] \
        || { printf 'legacy macOS agent/updater can still relaunch after commit\n'; return 1; }
    [ -f "$agent_dir/network.arc.node.plist" ] \
        && [ -f "$agent_dir/network.arc.update.plist" ] \
        || { printf 'managed macOS agents were not installed\n'; return 1; }
    assert_file_contains "$legacy_root/install.conf" '^rpc_port=18444$' \
        'macOS adoption did not retain the legacy RPC port' || return 1
    assert_file_contains "$legacy_root/install.conf" \
        "^model_path=$legacy_root/community-model.gguf$" \
        'macOS adoption did not retain the active legacy model' || return 1
    assert_file_contains "$sandbox/launchd-state/com.arc.inference.loaded" '^false$' \
        'legacy macOS node agent remained loaded after commit' || return 1
    assert_file_contains "$sandbox/launchd-state/com.arc.updater.loaded" '^false$' \
        'legacy macOS updater agent remained loaded after commit' || return 1
    assert_file_contains "$agent_dir/network.arc.node.plist" \
        '<key>ExitTimeOut</key><integer>4420</integer>' \
        'managed macOS node agent lacks the complete graceful-stop budget' || return 1
}

managed_macos_update_drains_old_node_without_unloading_its_updater() {
    local sandbox output host_uid node_pid status
    reset_legacy_supervisor_test_environment
    new_sandbox
    sandbox="$NEW_SANDBOX"
    host_uid="$(id -u)"
    MOCK_TARGET_UID_UNDER_TEST="$host_uid"
    MOCK_HEALTH_PORT_UNDER_TEST=9944
    ARC_HEALTH_TIMEOUT_UNDER_TEST=4
    output="$sandbox/macos-initial-install.out"
    if ! invoke_installer "$sandbox" Darwin x86_64 \
        "$TEST_DIR/fixtures/release-v0.8.0.json" 0.8.0 >"$output" 2>&1; then
        sed -n '1,240p' "$output"
        reset_legacy_supervisor_test_environment
        return 1
    fi
    assert_file_contains "$sandbox/home/Library/LaunchAgents/network.arc.node.plist" \
        '<key>ExitTimeOut</key><integer>4420</integer>' \
        'fresh managed macOS agent lacks the complete graceful-stop budget' || return 1

    # Model a node which acknowledges SIGTERM immediately but needs time to
    # drain accepted work and flush durable state before its process exits.
    (
        trap 'trap - TERM; /bin/sleep 1; exit 0' TERM
        while :; do /bin/sleep 1; done
    ) &
    node_pid=$!
    ACTIVE_TEST_PIDS="$ACTIVE_TEST_PIDS $node_pid"
    MOCK_LAUNCHD_NODE_PID_UNDER_TEST="$node_pid"
    MOCK_PS_COMMAND_UNDER_TEST="$sandbox/arc/bin/arc-node --rpc 127.0.0.1:9944"
    MOCK_REAL_SLEEP_UNDER_TEST=true
    : >"$sandbox/service.log"
    output="$sandbox/macos-delayed-update.out"
    invoke_installer "$sandbox" Darwin x86_64 \
        "$TEST_DIR/fixtures/release-v0.8.1.json" 0.8.1 \
        --update-only >"$output" 2>&1
    status=$?

    if kill -0 "$node_pid" 2>/dev/null; then
        kill -KILL "$node_pid" 2>/dev/null || true
        wait "$node_pid" 2>/dev/null || true
        reset_legacy_supervisor_test_environment
        printf 'managed macOS update did not wait for the SIGTERM-delayed node\n'
        return 1
    fi
    wait "$node_pid" 2>/dev/null || true
    reset_legacy_supervisor_test_environment
    if [ "$status" -ne 0 ]; then
        sed -n '1,260p' "$output"
        return 1
    fi
    "$sandbox/arc/bin/arc-node" --version | grep -Fq '0.8.1' || {
        printf 'managed macOS delayed update did not commit v0.8.1\n'
        return 1
    }
    assert_file_contains "$sandbox/home/Library/LaunchAgents/network.arc.node.plist" \
        '<key>ExitTimeOut</key><integer>4420</integer>' \
        'updated managed macOS agent lost the graceful-stop budget' || return 1
    if grep -Eq '^launchctl bootout (user|gui)/[0-9]+/network\.arc\.update$' \
        "$sandbox/service.log"; then
        printf 'managed macOS updater unloaded itself during its transaction\n'
        return 1
    fi
    if grep -Fq 'launchctl kickstart -k ' "$sandbox/service.log"; then
        printf 'managed macOS update force-restarted the freshly bootstrapped node\n'
        return 1
    fi
}

legacy_detached_pid_is_exactly_verified_and_retired() {
    local sandbox legacy_root output host_uid detached_pid waiter_pid status
    reset_legacy_supervisor_test_environment
    new_sandbox
    sandbox="$NEW_SANDBOX"
    legacy_root="$sandbox/home/.arc"
    write_legacy_v07_fixture "$legacy_root"
    prepare_installer_systemd_sandbox "$sandbox"
    (
        /bin/sleep 300 &
        printf '%s\n' "$!" >"$sandbox/detached-real-pid"
        wait "$!"
    ) &
    waiter_pid=$!
    while [ ! -s "$sandbox/detached-real-pid" ]; do /bin/sleep 0.01; done
    detached_pid="$(sed -n '1p' "$sandbox/detached-real-pid")"
    printf '%s\n' "$detached_pid" >"$legacy_root/node.pid"
    chmod 600 "$legacy_root/node.pid"
    MOCK_PS_COMMAND_UNDER_TEST="$legacy_root/bin/arc-node --rpc 0.0.0.0:18444 --p2p-port 18445 --seeds-file $legacy_root/seeds.txt --genesis $legacy_root/genesis.toml --validator-seed community-legacy-host-01020304 --stake 0 --min-stake 0 --eth-rpc-port 0 --data-dir $legacy_root/data --model $legacy_root/community-model.gguf --community-mode"
    MOCK_REAL_SLEEP_UNDER_TEST=true
    host_uid="$(id -u)"
    MOCK_TARGET_UID_UNDER_TEST="$host_uid"
    output="$sandbox/detached-legacy-adoption.out"
    invoke_installer "$sandbox" Linux x86_64 \
        "$TEST_DIR/fixtures/release-v0.8.0.json" 0.8.0 \
        --install-dir "$legacy_root" --no-service --no-auto-update >"$output" 2>&1
    status=$?
    wait "$waiter_pid" 2>/dev/null || true
    reset_legacy_supervisor_test_environment
    if [ "$status" -ne 0 ]; then
        sed -n '1,240p' "$output"
        return 1
    fi
    kill -0 "$detached_pid" 2>/dev/null \
        && { printf 'verified detached v0.7 process survived committed adoption\n'; return 1; }
    [ ! -e "$legacy_root/node.pid" ] \
        && [ ! -e "$legacy_root/bin/arc-auto-update.sh" ] \
        || { printf 'detached legacy PID/updater artifacts survived commit\n'; return 1; }
    assert_file_contains "$legacy_root/install.conf" '^service_scope=none$' \
        'detached no-service adoption did not remain install-only' || return 1
    assert_file_contains "$legacy_root/install.conf" '^rpc_port=18444$' \
        'detached adoption did not retain the verified RPC port' || return 1
    assert_file_contains "$legacy_root/legacy-v0.7-preserved/legacy-node.pid" \
        "^$detached_pid$" 'detached adoption did not archive the exact prior PID' || return 1
}

legacy_untracked_no_sudo_process_is_discovered_and_retired() {
    local sandbox legacy_root output host_uid detached_pid waiter_pid status command_line
    reset_legacy_supervisor_test_environment
    new_sandbox
    sandbox="$NEW_SANDBOX"
    legacy_root="$sandbox/home/.arc"
    write_legacy_v07_fixture "$legacy_root"
    (
        /bin/sleep 300 &
        printf '%s\n' "$!" >"$sandbox/untracked-real-pid"
        wait "$!"
    ) &
    waiter_pid=$!
    while [ ! -s "$sandbox/untracked-real-pid" ]; do /bin/sleep 0.01; done
    detached_pid="$(sed -n '1p' "$sandbox/untracked-real-pid")"
    host_uid="$(id -u)"
    command_line="$legacy_root/bin/arc-node --rpc 0.0.0.0:19444 --p2p-port 19445 --seeds-file $legacy_root/seeds.txt --genesis $legacy_root/genesis.toml --validator-seed community-legacy-host-01020304 --stake 0 --min-stake 0 --eth-rpc-port 0 --data-dir $legacy_root/data --model $legacy_root/community-model.gguf"
    MOCK_TARGET_UID_UNDER_TEST="$host_uid"
    MOCK_PS_UID_UNDER_TEST="$host_uid"
    MOCK_PS_COMMAND_UNDER_TEST="$command_line"
    MOCK_PS_ALL_ROWS_UNDER_TEST="$detached_pid $host_uid $command_line"
    MOCK_REAL_SLEEP_UNDER_TEST=true
    output="$sandbox/untracked-legacy-adoption.out"
    invoke_installer "$sandbox" Linux x86_64 \
        "$TEST_DIR/fixtures/release-v0.8.0.json" 0.8.0 \
        --install-dir "$legacy_root" --no-service --no-auto-update >"$output" 2>&1
    status=$?
    wait "$waiter_pid" 2>/dev/null || true
    reset_legacy_supervisor_test_environment
    if [ "$status" -ne 0 ]; then
        sed -n '1,260p' "$output"
        return 1
    fi
    kill -0 "$detached_pid" 2>/dev/null \
        && { printf 'historical no-sudo v0.7 process survived committed adoption\n'; return 1; }
    [ ! -e "$legacy_root/node.pid" ] \
        || { printf 'untracked legacy topology was changed into a PID-file topology\n'; return 1; }
    assert_file_contains \
        "$legacy_root/legacy-v0.7-preserved/legacy-detached-topology" \
        '^pid_file=false$' \
        'untracked legacy process topology was not archived' || return 1
    assert_file_contains \
        "$legacy_root/legacy-v0.7-preserved/legacy-detached-topology" \
        '^community_mode=false$' \
        'historical no-sudo community-mode shape was not preserved' || return 1
    assert_file_contains "$legacy_root/install.conf" '^rpc_port=19444$' \
        'untracked legacy process RPC configuration was not adopted' || return 1
}

legacy_linux_supervisor_lookalikes_fail_before_reservation() {
    local sandbox legacy_root unit_dir output status host_uid case_name
    host_uid="$(id -u)"
    for case_name in wrong-user duplicate-user extra-argument lifecycle-hook incomplete-updater symlink-unit; do
        reset_legacy_supervisor_test_environment
        new_sandbox
        sandbox="$NEW_SANDBOX"
        legacy_root="$sandbox/home/.arc"
        unit_dir="$sandbox/systemd-system"
        write_legacy_v07_fixture "$legacy_root"
        prepare_installer_systemd_sandbox "$sandbox"
        write_legacy_linux_supervisor_fixture "$legacy_root" "$unit_dir" 18444 18445
        case "$case_name" in
            wrong-user) sed -i.bak 's/^User=arc-community-test$/User=root/' "$unit_dir/arc-node.service"; rm -f "$unit_dir/arc-node.service.bak" ;;
            duplicate-user) printf '%s\n' 'User=root' >>"$unit_dir/arc-node.service" ;;
            extra-argument)
                sed -i.bak 's/    --community-mode$/    --auto-shard --community-mode/' \
                    "$unit_dir/arc-node.service"
                rm -f "$unit_dir/arc-node.service.bak" ;;
            lifecycle-hook) printf '%s\n' 'ExecStartPre=/bin/true' >>"$unit_dir/arc-node.service" ;;
            incomplete-updater) rm -f "$unit_dir/arc-updater.timer" ;;
            symlink-unit)
                mv "$unit_dir/arc-node.service" "$sandbox/lookalike-node.service"
                ln -s "$sandbox/lookalike-node.service" "$unit_dir/arc-node.service" ;;
        esac
        MOCK_TARGET_UID_UNDER_TEST="$host_uid"
        output="$sandbox/hostile-supervisor-$case_name.out"
        invoke_installer "$sandbox" Linux x86_64 \
            "$TEST_DIR/fixtures/release-v0.8.0.json" 0.8.0 \
            --install-dir "$legacy_root" >"$output" 2>&1
        status=$?
        [ "$status" -ne 0 ] \
            || { printf 'legacy supervisor accepted hostile %s layout\n' "$case_name"; return 1; }
        [ ! -e "$legacy_root/.arc-chain-legacy-adoption-pending" ] \
            && [ ! -e "$legacy_root/.arc-chain-install-root" ] \
            || { printf 'hostile %s supervisor received an ownership marker\n' "$case_name"; return 1; }
        [ ! -s "$sandbox/curl.log" ] \
            || { printf 'hostile %s supervisor reached release downloads\n' "$case_name"; return 1; }
        [ -f "$legacy_root/data/state.wal" ] \
            || { printf 'hostile %s supervisor refusal lost legacy data\n' "$case_name"; return 1; }
    done
    reset_legacy_supervisor_test_environment
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
        MOCK_RELEASE_FIXTURE_DIR="${MOCK_RELEASE_FIXTURE_DIR_UNDER_TEST:-}" \
        MOCK_RELEASE_LIST_FILE="${MOCK_RELEASE_LIST_FILE_UNDER_TEST:-}" \
        MOCK_V3_RETIREMENT_MODE="${MOCK_V3_RETIREMENT_MODE_UNDER_TEST:-ok}" \
        MOCK_LEGACY_LISTENER_OPEN="${MOCK_LEGACY_LISTENER_OPEN_UNDER_TEST:-}" \
        MOCK_RETIREMENT_CREATE_FAIL="${MOCK_RETIREMENT_CREATE_FAIL_UNDER_TEST:-0}" \
        MOCK_RETIREMENT_FINALIZE_FAIL="${MOCK_RETIREMENT_FINALIZE_FAIL_UNDER_TEST:-0}" \
        MOCK_AVAILABLE_ASSETS="$(canonical_pairs_for_version "$version")" \
        MOCK_CHECKSUM_ASSETS="$CANONICAL_ASSETS $CUTOVER_ASSETS testnet-seeds.txt genesis.toml install.sh" \
        MOCK_CURL_LOG="$sandbox/curl.log" \
        MOCK_SERVICE_LOG="$sandbox/service.log" \
        MOCK_OWNER_LOG="$sandbox/owner.log" \
        MOCK_CURRENT_UID="${MOCK_CURRENT_UID_UNDER_TEST:-1000}" \
        MOCK_CURRENT_USER="${MOCK_CURRENT_USER_UNDER_TEST:-arc-community-test}" \
        MOCK_TARGET_USER="${MOCK_TARGET_USER_UNDER_TEST:-arc-community-test}" \
        MOCK_TARGET_UID="${MOCK_TARGET_UID_UNDER_TEST:-1000}" \
        MOCK_TARGET_GROUP="${MOCK_TARGET_GROUP_UNDER_TEST:-arc-community-test}" \
        MOCK_INSTALL_MARKER_UID="${MOCK_INSTALL_MARKER_UID_UNDER_TEST:-${MOCK_TARGET_UID_UNDER_TEST:-1000}}" \
        MOCK_ROOT_OWNED_PREFIX="${MOCK_ROOT_OWNED_PREFIX_UNDER_TEST:-}" \
        MOCK_SUDO_EXECUTE="${MOCK_SUDO_EXECUTE_UNDER_TEST:-false}" \
        MOCK_TARGET_HOME="$sandbox/home" \
        MOCK_HEALTH_PORT="${MOCK_HEALTH_PORT_UNDER_TEST:-}" \
        MOCK_HEALTH_STATUS="${MOCK_HEALTH_STATUS_UNDER_TEST:-ok}" \
        MOCK_TAMPER_BINARY="${MOCK_TAMPER_BINARY_UNDER_TEST:-0}" \
        MOCK_TAMPER_MANIFEST_SIGNATURE="${MOCK_TAMPER_MANIFEST_SIGNATURE_UNDER_TEST:-0}" \
        MOCK_MISSING_CHECKSUM="${MOCK_MISSING_CHECKSUM_UNDER_TEST:-0}" \
        MOCK_MISSING_MANIFEST_SIGNATURE="${MOCK_MISSING_MANIFEST_SIGNATURE_UNDER_TEST:-0}" \
        MOCK_DUPLICATE_CHECKSUM_ASSET="${MOCK_DUPLICATE_CHECKSUM_ASSET_UNDER_TEST:-}" \
        MOCK_RELEASE_COMMIT="${MOCK_RELEASE_COMMIT_UNDER_TEST:-aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa}" \
        MOCK_SYSTEMD_NODE_ACTIVE="${MOCK_SYSTEMD_NODE_ACTIVE_UNDER_TEST:-false}" \
        MOCK_SYSTEMD_NODE_ENABLED="${MOCK_SYSTEMD_NODE_ENABLED_UNDER_TEST:-false}" \
        MOCK_SYSTEMD_UPDATER_ACTIVE="${MOCK_SYSTEMD_UPDATER_ACTIVE_UNDER_TEST:-false}" \
        MOCK_SYSTEMD_UPDATER_ENABLED="${MOCK_SYSTEMD_UPDATER_ENABLED_UNDER_TEST:-false}" \
        MOCK_LEGACY_UPDATER_ACTIVE="${MOCK_LEGACY_UPDATER_ACTIVE_UNDER_TEST:-false}" \
        MOCK_LEGACY_UPDATER_ENABLED="${MOCK_LEGACY_UPDATER_ENABLED_UNDER_TEST:-false}" \
        MOCK_LEGACY_UPDATER_SERVICE_ACTIVE="${MOCK_LEGACY_UPDATER_SERVICE_ACTIVE_UNDER_TEST:-false}" \
        MOCK_LEGACY_UPDATER_SERVICE_ENABLED="${MOCK_LEGACY_UPDATER_SERVICE_ENABLED_UNDER_TEST:-false}" \
        MOCK_SYSTEMD_STATE_DIR="$sandbox/systemd-state" \
        MOCK_SYSTEMD_MAIN_PID="${MOCK_SYSTEMD_MAIN_PID_UNDER_TEST:-}" \
        MOCK_SYSTEMD_MAIN_PID_AFTER="${MOCK_SYSTEMD_MAIN_PID_AFTER_UNDER_TEST:-}" \
        MOCK_SYSTEMD_RESTART_DELAY_POLLS="${MOCK_SYSTEMD_RESTART_DELAY_POLLS_UNDER_TEST:-}" \
        MOCK_LAUNCHD_NODE_LOADED="${MOCK_LAUNCHD_NODE_LOADED_UNDER_TEST:-false}" \
        MOCK_LAUNCHD_UPDATER_LOADED="${MOCK_LAUNCHD_UPDATER_LOADED_UNDER_TEST:-false}" \
        MOCK_LEGACY_LAUNCHD_NODE_LOADED="${MOCK_LEGACY_LAUNCHD_NODE_LOADED_UNDER_TEST:-false}" \
        MOCK_LEGACY_LAUNCHD_UPDATER_LOADED="${MOCK_LEGACY_LAUNCHD_UPDATER_LOADED_UNDER_TEST:-false}" \
        MOCK_LAUNCHD_NODE_PID="${MOCK_LAUNCHD_NODE_PID_UNDER_TEST:-}" \
        MOCK_LEGACY_LAUNCHD_NODE_PID="${MOCK_LEGACY_LAUNCHD_NODE_PID_UNDER_TEST:-}" \
        MOCK_LAUNCHD_STATE_DIR="$sandbox/launchd-state" \
        MOCK_PS_COMMAND="${MOCK_PS_COMMAND_UNDER_TEST:-}" \
        MOCK_PS_ALL_ROWS="${MOCK_PS_ALL_ROWS_UNDER_TEST:-}" \
        MOCK_PS_UID="${MOCK_PS_UID_UNDER_TEST:-${MOCK_TARGET_UID_UNDER_TEST:-1000}}" \
        MOCK_REAL_SLEEP="${MOCK_REAL_SLEEP_UNDER_TEST:-false}" \
        MOCK_SERVICE_FAIL_MATCH="${MOCK_SERVICE_FAIL_MATCH_UNDER_TEST:-}" \
        MOCK_SERVICE_FAIL_ONCE_FILE="$sandbox/service-fail-once" \
        ARC_INSTALL_TEST_FAIL_AFTER_COPY="${ARC_INSTALL_TEST_FAIL_AFTER_COPY_UNDER_TEST:-}" \
        ARC_INSTALL_TEST_FAIL_AFTER_LEGACY_MARKER_FSYNC="${ARC_INSTALL_TEST_FAIL_AFTER_LEGACY_MARKER_FSYNC_UNDER_TEST:-}" \
        ARC_HEALTH_TIMEOUT="${ARC_HEALTH_TIMEOUT_UNDER_TEST:-180}" \
        ARC_TEST_NODE_ARGS_LOG="$sandbox/node-args.log" \
        TMPDIR="$sandbox/tmp" \
        NO_COLOR=1 \
        LC_ALL=C \
        /bin/bash "${INSTALLER_OVERRIDE_UNDER_TEST:-$INSTALLER}" "$@"
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
    # GNU stat accepts `-f` but interprets it as filesystem statistics, so a
    # BSD-first probe can succeed with several lines of unrelated output.
    # GNU's file-format form is unambiguous; BSD stat rejects it and falls
    # through to its native `%Lp` format.
    stat -c '%a' "$file" 2>/dev/null || stat -f '%Lp' "$file"
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
        assert_log_contains_literal "$sandbox/curl.log" '/v0.8.0/SHA256SUMS.sig' \
            "$os/$arch did not request the detached manifest signature" || return 1
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
        if grep -Eq 'releases/latest|releases/latest/download|(^|[[:space:]])-r([[:space:]]|$)' "$sandbox/curl.log"; then
            printf '%s/%s used global latest or a Range probe:\n' "$os" "$arch"
            cat "$sandbox/curl.log"
            return 1
        fi
        if [ "$(grep -Fc '/releases?per_page=100' "$sandbox/curl.log")" -ne 1 ]; then
            printf '%s/%s did not discover exactly one v0.8 channel page:\n' "$os" "$arch"
            cat "$sandbox/curl.log"
            return 1
        fi
    done <<EOF
$matrix
EOF
}

minified_github_api_json_without_newline_installs_exact_assets() {
    local sandbox fixture output status
    new_sandbox
    sandbox="$NEW_SANDBOX"
    fixture="$sandbox/release-minified.json"
    # Command substitution strips all trailing newlines; `tr` removes the
    # internal pretty-print newlines, reproducing GitHub's one-line API body.
    printf '%s' "$(tr -d '\n' < "$TEST_DIR/fixtures/release-v0.8.0.json")" > "$fixture"
    [ "$(wc -l < "$fixture" | tr -d ' ')" -eq 0 ] || {
        printf 'minified release fixture unexpectedly retained a newline\n'
        return 1
    }
    output="$sandbox/minified-install.out"
    ARC_NODE_VERSION_UNDER_TEST=''
    invoke_installer "$sandbox" Linux amd64 "$fixture" 0.8.0 \
        --no-service --no-auto-update >"$output" 2>&1
    status=$?
    if [ "$status" -ne 0 ]; then
        printf 'installer rejected GitHub-shaped minified JSON (exit %s):\n' "$status"
        sed -n '1,120p' "$output"
        return 1
    fi
    for asset in arc-node-linux-x86_64 arc-cli-linux-x86_64 SHA256SUMS testnet-seeds.txt genesis.toml; do
        assert_log_contains_literal "$sandbox/curl.log" "/v0.8.0/$asset" \
            "minified API install did not fetch exact-tag $asset" || return 1
    done
}

v08_channel_selection_ignores_global_latest_and_nested_tags() {
    local sandbox list_fixture output status
    new_sandbox
    sandbox="$NEW_SANDBOX"
    list_fixture="$sandbox/releases-list.json"
    printf '%s\n' \
        '[' \
        '  {"tag_name":"v0.8.0","body":"escaped \\"tag_name\\":\\"v99.0.0\\" text"},' \
        '  {"tag_name":"v0.7.11"},' \
        '  {"tag_name":"v0.8.1","assets":[{"tag_name":"v98.0.0"}]}' \
        ']' > "$list_fixture"
    output="$sandbox/channel-install.out"
    ARC_NODE_VERSION_UNDER_TEST=''
    MOCK_RELEASE_LIST_FILE_UNDER_TEST="$list_fixture"
    invoke_installer "$sandbox" Linux x86_64 \
        "$TEST_DIR/fixtures/release-v0.8.1.json" '0.8.1' \
        --no-service --no-auto-update >"$output" 2>&1
    status=$?
    MOCK_RELEASE_LIST_FILE_UNDER_TEST=''
    if [ "$status" -ne 0 ]; then
        printf 'v0.8 channel discovery failed (exit %s):\n' "$status"
        sed -n '1,140p' "$output"
        return 1
    fi
    assert_log_contains_literal "$sandbox/curl.log" '/releases?per_page=100' \
        'installer did not query the dedicated v0.8 release collection' || return 1
    assert_log_contains_literal "$sandbox/curl.log" '/releases/tags/v0.8.1' \
        'installer did not resolve the highest discovered v0.8 tag exactly' || return 1
    if grep -Fq '/releases/latest' "$sandbox/curl.log" \
        || grep -Eq '/releases/tags/v(98|99)\.' "$sandbox/curl.log"; then
        printf 'channel selection trusted global latest or a nested/body tag:\n'
        cat "$sandbox/curl.log"
        return 1
    fi
}

v08_channel_skips_higher_untrusted_tags_for_stable_release() {
    local sandbox fixture_dir list_fixture output status rejected_tag
    new_sandbox
    sandbox="$NEW_SANDBOX"
    fixture_dir="$sandbox/exact-releases"
    list_fixture="$sandbox/releases-list.json"
    mkdir -p "$fixture_dir"
    cp "$TEST_DIR/fixtures/release-v0.8.1.json" "$fixture_dir/v0.8.1.json"
    sed -e 's/"tag_name": "v0.8.1"/"tag_name": "v0.9.0"/' \
        -e 's/"prerelease": false/"prerelease": true/' \
        "$TEST_DIR/fixtures/release-v0.8.1.json" > "$fixture_dir/v0.9.0.json"
    sed -e 's/"tag_name": "v0.8.1"/"tag_name": "v0.10.0"/' \
        -e 's/github-actions\[bot\]/manual-publisher/' \
        "$TEST_DIR/fixtures/release-v0.8.1.json" > "$fixture_dir/v0.10.0.json"
    sed -e 's/"tag_name": "v0.8.1"/"tag_name": "v0.11.0"/' \
        -e 's/"immutable": true/"immutable": false/' \
        "$TEST_DIR/fixtures/release-v0.8.1.json" > "$fixture_dir/v0.11.0.json"
    printf '%s\n' \
        '[' \
        '  {"tag_name":"v0.9.0"},' \
        '  {"tag_name":"v0.8.1"},' \
        '  {"tag_name":"v0.11.0"},' \
        '  {"tag_name":"v0.10.0"}' \
        ']' > "$list_fixture"

    output="$sandbox/channel-untrusted-fallback.out"
    ARC_NODE_VERSION_UNDER_TEST=''
    MOCK_RELEASE_LIST_FILE_UNDER_TEST="$list_fixture"
    MOCK_RELEASE_FIXTURE_DIR_UNDER_TEST="$fixture_dir"
    invoke_installer "$sandbox" Linux x86_64 \
        "$TEST_DIR/fixtures/release-v0.8.1.json" '0.8.1' \
        --no-service --no-auto-update >"$output" 2>&1
    status=$?
    MOCK_RELEASE_LIST_FILE_UNDER_TEST=''
    MOCK_RELEASE_FIXTURE_DIR_UNDER_TEST=''
    if [ "$status" -ne 0 ]; then
        printf 'v0.8 channel did not fall through poisoned higher tags (exit %s):\n' "$status"
        sed -n '1,180p' "$output"
        return 1
    fi
    for rejected_tag in v0.11.0 v0.10.0 v0.9.0; do
        assert_log_contains_literal "$sandbox/curl.log" "/releases/tags/$rejected_tag" \
            "installer did not exact-check rejected channel tag $rejected_tag" || return 1
        if grep -Fq "/releases/download/$rejected_tag/" "$sandbox/curl.log"; then
            printf 'installer downloaded an asset from rejected channel tag %s\n' "$rejected_tag"
            return 1
        fi
    done
    assert_log_contains_literal "$sandbox/curl.log" '/releases/tags/v0.8.1' \
        'installer did not fall through to the trusted stable tag' || return 1
    assert_file_contains "$sandbox/arc/bin/arc-node" 'arc-node 0\.8\.1' \
        'installer did not commit the highest trusted stable release' || return 1
}

v08_channel_fails_closed_without_a_compatible_or_complete_list() {
    local sandbox list_fixture output status case_name
    for case_name in v07-only truncated; do
        new_sandbox
        sandbox="$NEW_SANDBOX"
        list_fixture="$sandbox/releases-list.json"
        if [ "$case_name" = v07-only ]; then
            printf '%s\n' '[{"tag_name":"v0.7.11"}]' > "$list_fixture"
        else
            printf '%s' '[{"tag_name":"v0.8.1"}' > "$list_fixture"
        fi
        output="$sandbox/channel-rejected.out"
        ARC_NODE_VERSION_UNDER_TEST=''
        MOCK_RELEASE_LIST_FILE_UNDER_TEST="$list_fixture"
        invoke_installer "$sandbox" Linux x86_64 \
            "$TEST_DIR/fixtures/release-v0.8.1.json" '0.8.1' \
            --no-service --no-auto-update >"$output" 2>&1
        status=$?
        MOCK_RELEASE_LIST_FILE_UNDER_TEST=''
        [ "$status" -ne 0 ] || {
            printf 'installer accepted a %s v0.8 channel response\n' "$case_name"
            return 1
        }
        if grep -Fq '/releases/tags/' "$sandbox/curl.log" \
            || grep -Fq '/releases/download/' "$sandbox/curl.log"; then
            printf '%s channel rejection reached exact metadata or payloads:\n' "$case_name"
            cat "$sandbox/curl.log"
            return 1
        fi
    done
}

untrusted_release_metadata_fails_before_asset_download() {
    local sandbox fixture output status field replacement
    for field in immutable draft prerelease author; do
        new_sandbox
        sandbox="$NEW_SANDBOX"
        fixture="$sandbox/rejected-$field.json"
        case "$field" in
            immutable) replacement='s/"immutable": true/"immutable": false/' ;;
            draft) replacement='s/"draft": false/"draft": true/' ;;
            prerelease) replacement='s/"prerelease": false/"prerelease": true/' ;;
            author) replacement='s/github-actions\[bot\]/manual-publisher/' ;;
        esac
        sed "$replacement" "$TEST_DIR/fixtures/release-v0.8.0.json" > "$fixture"
        output="$sandbox/rejected-$field.out"
        ARC_NODE_VERSION_UNDER_TEST='0.8.0'
        invoke_installer "$sandbox" Linux amd64 "$fixture" 0.8.0 \
            --no-service --no-auto-update >"$output" 2>&1
        status=$?
        ARC_NODE_VERSION_UNDER_TEST=''

        if [ "$status" -eq 0 ]; then
            printf 'installer accepted release metadata with rejected %s state\n' "$field"
            return 1
        fi
        if grep -Fq '/releases/download/' "$sandbox/curl.log"; then
            printf 'installer downloaded assets before rejecting %s metadata:\n' "$field"
            cat "$sandbox/curl.log"
            return 1
        fi
        if find "$sandbox/arc/bin" -type f | grep -q .; then
            printf 'installer introduced executables before rejecting %s metadata\n' "$field"
            find "$sandbox/arc/bin" -type f -print
            return 1
        fi
    done
}

tampered_manifest_signature_fails_before_payload_download() {
    local sandbox output status
    new_sandbox
    sandbox="$NEW_SANDBOX"
    output="$sandbox/rejected-signature.out"
    MOCK_TAMPER_MANIFEST_SIGNATURE_UNDER_TEST=1
    invoke_installer "$sandbox" Linux x86_64 \
        "$TEST_DIR/fixtures/release-v0.8.0.json" '0.8.0' \
        --no-service --no-auto-update >"$output" 2>&1
    status=$?
    MOCK_TAMPER_MANIFEST_SIGNATURE_UNDER_TEST=0

    [ "$status" -ne 0 ] || {
        printf 'installer accepted a tampered manifest signature\n'
        return 1
    }
    grep -Fq '/v0.8.0/SHA256SUMS.sig' "$sandbox/curl.log" || {
        printf 'signature rejection test never downloaded the signature\n'
        return 1
    }
    if grep -Eq '/v0\.8\.0/(arc-node|arc-cli)-' "$sandbox/curl.log"; then
        printf 'installer downloaded executable payloads before rejecting the signature\n'
        cat "$sandbox/curl.log"
        return 1
    fi
}

missing_checksum_or_signature_fails_before_payload_download() {
    local sandbox output status missing
    for missing in checksum signature; do
        new_sandbox
        sandbox="$NEW_SANDBOX"
        output="$sandbox/missing-$missing.out"
        MOCK_MISSING_CHECKSUM_UNDER_TEST=0
        MOCK_MISSING_MANIFEST_SIGNATURE_UNDER_TEST=0
        if [ "$missing" = checksum ]; then
            MOCK_MISSING_CHECKSUM_UNDER_TEST=1
        else
            MOCK_MISSING_MANIFEST_SIGNATURE_UNDER_TEST=1
        fi
        invoke_installer "$sandbox" Linux x86_64 \
            "$TEST_DIR/fixtures/release-v0.8.0.json" '0.8.0' \
            --no-service --no-auto-update >"$output" 2>&1
        status=$?
        MOCK_MISSING_CHECKSUM_UNDER_TEST=0
        MOCK_MISSING_MANIFEST_SIGNATURE_UNDER_TEST=0
        [ "$status" -ne 0 ] || {
            printf 'installer accepted a missing %s\n' "$missing"
            return 1
        }
        if grep -Eq '/v0\.8\.0/(arc-node|arc-cli)-' "$sandbox/curl.log"; then
            printf 'installer downloaded executable payloads after a missing %s\n' "$missing"
            cat "$sandbox/curl.log"
            return 1
        fi
    done
}

duplicate_checksum_entry_fails_before_replacement() {
    local sandbox output status
    new_sandbox
    sandbox="$NEW_SANDBOX"
    output="$sandbox/duplicate-checksum.out"
    MOCK_DUPLICATE_CHECKSUM_ASSET_UNDER_TEST=arc-node-linux-x86_64
    invoke_installer "$sandbox" Linux x86_64 \
        "$TEST_DIR/fixtures/release-v0.8.0.json" '0.8.0' \
        --no-service --no-auto-update >"$output" 2>&1
    status=$?
    MOCK_DUPLICATE_CHECKSUM_ASSET_UNDER_TEST=''
    [ "$status" -ne 0 ] || {
        printf 'installer accepted duplicate checksum rows for one executable\n'
        return 1
    }
    [ ! -e "$sandbox/arc/bin/arc-node" ] || {
        printf 'duplicate checksum failure introduced the node executable\n'
        return 1
    }
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
    local sandbox output origin origin_count
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
    origin_count="$(grep -o -- '--community-rpc-url' "$sandbox/arc/bin/run-arc-node" | wc -l | tr -d ' ')"
    assert_equals 6 "$origin_count" \
        'generated runner must pass exactly six explicit community RPC origins' || return 1
    for origin in \
        https://149.28.32.76 \
        https://140.82.16.112 \
        https://136.244.109.1 \
        https://104.238.171.11 \
        https://202.182.107.41 \
        https://149.28.153.31
    do
        assert_log_contains_literal "$sandbox/arc/bin/run-arc-node" \
            "--community-rpc-url $origin" \
            "generated runner is missing reviewed origin: $origin" || return 1
    done
    assert_file_contains "$sandbox/arc/genesis.toml" \
        '^validator_set_complete[[:space:]]*=[[:space:]]*true$' \
        'installed release genesis does not carry the approved validator set' || return 1
    assert_file_contains "$sandbox/arc/genesis.toml" '^\[\[validators\]\]' \
        'installed release genesis omitted its public validator set' || return 1
    assert_file_contains "$sandbox/arc/genesis.toml" \
        '^community_rewards_v1_activation_height[[:space:]]*=[[:space:]]*137146$' \
        'installed release genesis omitted the checkpoint-bound activation height' || return 1
    assert_file_not_contains "$sandbox/arc/bin/run-arc-node" '--model([[:space:]]|$)' \
        'generated runner passes --model even though no model was configured' || return 1
    assert_file_not_contains "$sandbox/arc/bin/run-arc-node" '--full-integer-worker' \
        'observer-only runner enables the full integer worker without a model' || return 1
    assert_file_not_contains "$sandbox/arc/bin/run-arc-node" '--validator-seed' \
        'generated runner exposes validator identity through argv' || return 1
    assert_file_not_contains "$sandbox/arc/bin/run-arc-node" 'ARC_VALIDATOR_SEED' \
        'generated runner exports legacy validator seed material' || return 1
    assert_file_contains "$sandbox/arc/bin/run-arc-node" \
        "--validator-key-file $sandbox/arc/identity/validator-key.json" \
        'generated runner does not use the persistent validator keyfile' || return 1
    assert_equals 600 "$(file_mode "$sandbox/arc/identity/validator-key.json")" \
        'validator keyfile permissions are not 0600' || return 1
    [ ! -e "$sandbox/arc/identity/validator-seed" ] && [ ! -e "$sandbox/arc/node.env" ] || {
        printf 'fresh install retained active seed or environment identity material\n'
        return 1
    }
    assert_log_contains_literal "$output" \
        "$sandbox/arc/identity/validator-key.json" \
        'install summary does not point operators to the active validator keyfile' || return 1
    if grep -Fq "$sandbox/arc/identity/validator-seed" "$output"; then
        printf 'install summary points operators to the retired validator-seed path\n'
        return 1
    fi
    key_secret="$(sed -n 's/^[[:space:]]*\"secret_key\": \"\([0-9a-f]*\)\".*/\1/p' "$sandbox/arc/identity/validator-key.json")"
    if [ -z "$key_secret" ] || grep -Fq "$key_secret" "$output" \
        || grep -Fq "$key_secret" "$sandbox/arc/bin/run-arc-node"; then
        printf 'validator key material was absent or leaked into installer output/runner\n'
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
    ARC_NODE_VERSION_UNDER_TEST='v0.7.9'
    output="$sandbox/downgrade.out"
    invoke_installer "$sandbox" Linux x86_64 \
        "$TEST_DIR/fixtures/release-v0.7.9.json" '0.7.9' \
        --update-only --no-service --no-auto-update >"$output" 2>&1
    status=$?
    ARC_NODE_VERSION_UNDER_TEST=''
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
    assert_file_contains "$sandbox/home/.config/systemd/user/arc-node.service" \
        '^TimeoutStopSec=4420$' \
        'managed node service can still SIGKILL valid community late-submit work before its writer barrier' \
        || return 1
}

managed_system_user_update_waits_past_thirty_seconds_for_graceful_restart() {
    local sandbox legacy_root unit_dir output host_uid old_pid new_pid status polls
    reset_legacy_supervisor_test_environment
    new_sandbox
    sandbox="$NEW_SANDBOX"
    legacy_root="$sandbox/home/.arc"
    unit_dir="$sandbox/systemd-system"
    write_legacy_v07_fixture "$legacy_root"
    prepare_installer_systemd_sandbox "$sandbox"
    write_legacy_linux_supervisor_fixture "$legacy_root" "$unit_dir" 18444 18445
    host_uid="$(id -u)"
    MOCK_TARGET_UID_UNDER_TEST="$host_uid"
    MOCK_SYSTEMD_NODE_ACTIVE_UNDER_TEST=true
    MOCK_SYSTEMD_MAIN_PID_UNDER_TEST=999999
    MOCK_SYSTEMD_NODE_ENABLED_UNDER_TEST=true
    MOCK_HEALTH_PORT_UNDER_TEST=18444
    ARC_HEALTH_TIMEOUT_UNDER_TEST=4
    output="$sandbox/system-user-base.out"
    if ! invoke_installer "$sandbox" Linux x86_64 \
        "$TEST_DIR/fixtures/release-v0.8.0.json" 0.8.0 \
        --install-dir "$legacy_root" >"$output" 2>&1; then
        sed -n '1,240p' "$output"
        reset_legacy_supervisor_test_environment
        return 1
    fi

    /bin/sleep 300 &
    old_pid=$!
    /bin/sleep 300 &
    new_pid=$!
    MOCK_SYSTEMD_MAIN_PID_UNDER_TEST="$old_pid"
    MOCK_SYSTEMD_MAIN_PID_AFTER_UNDER_TEST="$new_pid"
    MOCK_SYSTEMD_RESTART_DELAY_POLLS_UNDER_TEST=31
    MOCK_PS_COMMAND_UNDER_TEST="$legacy_root/bin/arc-node --rpc 127.0.0.1:18444"
    rm -f "$sandbox/systemd-state/arc-node.service.restart-polls" \
        "$sandbox/systemd-state/arc-node.service.restart-mainpid-seen"
    output="$sandbox/system-user-delayed-update.out"
    invoke_installer "$sandbox" Linux x86_64 \
        "$TEST_DIR/fixtures/release-v0.8.1.json" 0.8.1 \
        --install-dir "$legacy_root" --update-only >"$output" 2>&1
    status=$?
    wait "$old_pid" 2>/dev/null || true
    kill "$new_pid" 2>/dev/null || true
    wait "$new_pid" 2>/dev/null || true
    polls="$(sed -n '1p' "$sandbox/systemd-state/arc-node.service.restart-polls" 2>/dev/null)"
    reset_legacy_supervisor_test_environment

    if [ "$status" -ne 0 ]; then
        printf 'system-user update abandoned a valid graceful restart after 30 seconds:\n'
        sed -n '1,240p' "$output"
        return 1
    fi
    [ "${polls:-0}" -gt 30 ] || {
        printf 'delayed restart fixture did not hold the old PID past 30 lifecycle polls\n'
        return 1
    }
    "$legacy_root/bin/arc-node" --version | grep -Fq '0.8.1' || {
        printf 'system-user delayed restart did not commit the v0.8.1 binary\n'
        return 1
    }
    assert_file_not_contains "$output" 'Install/update failed' \
        'valid delayed system-user restart entered rollback' || return 1
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
identity-key|$sandbox/arc/identity/validator-key.json
legacy-seed|$sandbox/arc/identity/validator-seed
legacy-evidence|$sandbox/arc/identity/legacy-validator-seed.evidence
node-env|$sandbox/arc/node.env
runner|$sandbox/arc/bin/run-arc-node
install-config|$sandbox/arc/install.conf
install-root-marker|$sandbox/arc/.arc-chain-install-root
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
    printf 'existing user chain bytes\n' >"$sandbox/arc/data/existing-user-data"
    assert_log_contains_literal "$sandbox/arc/bin/run-arc-node" \
        "--model $sandbox/model.gguf --full-integer-worker" \
        'model-backed community install does not enable the non-shard full integer worker role' \
        || return 1
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
    if find "$sandbox/arc" -type f ! -name '.arc-chain-install-root' | grep -q .; then
        printf 'fresh-install rollback retained newly introduced managed files:\n'
        find "$sandbox/arc" -type f ! -name '.arc-chain-install-root' -print
        return 1
    fi
    assert_file_contains "$sandbox/arc/.arc-chain-install-root" \
        '^arc-chain-managed-install-root-v1$' \
        'fresh-install rollback lost the install-root ownership marker' || return 1
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
    # shellcheck disable=SC2016 # These are literal source-code contracts.
    for required_literal in \
        'snapshot_transaction_path "$SYSTEMD_UNIT_DIR/arc-node.service" root' \
        'snapshot_transaction_path "$SYSTEMD_UNIT_DIR/arc-node-update.service" root' \
        'snapshot_transaction_path "$SYSTEMD_UNIT_DIR/arc-node-update.timer" root' \
        'snapshot_transaction_path "$USER_UNIT_DIR/arc-node.service"' \
        'snapshot_transaction_path "$USER_UNIT_DIR/arc-node-update.service"' \
        'snapshot_transaction_path "$USER_UNIT_DIR/arc-node-update.timer"' \
        'snapshot_transaction_path "$NODE_PLIST"' \
        'snapshot_transaction_path "$UPDATE_PLIST"'
    do
        assert_log_contains_literal "$REPO_ROOT/install.sh" "$required_literal" \
            "installer transaction omits service-managed path: $required_literal" || return 1
    done
    # shellcheck disable=SC2016 # This is a literal source-code anti-pattern.
    assert_file_not_contains "$REPO_ROOT/install.sh" \
        'as_root cp -- "\$TMP_DIR/arc-node[^ ]*" /etc/systemd/system/' \
        'system service files bypass transactional_copy' || return 1
    assert_file_contains "$REPO_ROOT/install.sh" \
        '^commit_install_transaction$' \
        'installer never commits the full transaction after health succeeds' || return 1
}

run_test 'offline platform aliases install exact-tag node + CLI assets without starting' install_only_platform_matrix
run_test 'minified GitHub API JSON with no trailing newline installs exact-tag assets' minified_github_api_json_without_newline_installs_exact_assets
run_test 'v0.8 channel ignores global latest plus body/nested tag injection and resolves the highest exact tag' v08_channel_selection_ignores_global_latest_and_nested_tags
run_test 'v0.8 channel skips higher mutable, manual, and prerelease poison tags' v08_channel_skips_higher_untrusted_tags_for_stable_release
run_test 'v0.8 channel fails closed without a compatible complete release list' v08_channel_fails_closed_without_a_compatible_or_complete_list
run_test 'mutable, draft, prerelease, and manual-publisher metadata fail before any asset download' untrusted_release_metadata_fails_before_asset_download
run_test 'tampered release-manifest signature fails before executable download' tampered_manifest_signature_fails_before_payload_download
run_test 'missing checksum or signature fails before executable download' missing_checksum_or_signature_fails_before_payload_download
run_test 'duplicate executable checksum rows fail before replacement' duplicate_checksum_entry_fails_before_replacement
run_test 'checksum mismatch rejects and removes staged executables' tampered_binary_is_rejected
run_test 'ARC_NODE_VERSION requires strict X.Y.Z before network-shaped requests' invalid_version_pin_fails_before_asset_download
run_test 'system roots and namespaces are rejected as ARC data directories before side effects' unsafe_data_directories_fail_before_side_effects
run_test 'relative and traversal-shaped ARC data directories fail closed' relative_and_ambiguous_data_directories_fail_closed
run_test 'symlinked ARC data directories and ancestors fail before side effects' symlinked_data_directory_or_ancestor_fails_closed
run_test 'a dedicated absolute custom ARC data directory installs normally' dedicated_custom_data_directory_installs_normally
run_test 'broad system and home roots are rejected as install directories' protected_install_roots_fail_before_side_effects
run_test 'symlinked and traversal-shaped install roots fail closed' symlinked_or_traversal_install_root_fails_closed
run_test 'an existing unmarked directory is neither claimed, uninstalled, nor purged' existing_unmarked_install_root_is_never_claimed_or_purged
run_test 'a marked install root purges only its exact bound tree' marked_install_root_purges_only_its_bound_tree
run_test 'copied and symlinked markers cannot authorize purge of another tree' copied_or_symlinked_marker_cannot_authorize_purge
run_test 'verified v0.7 default adoption preserves state, config, model, and identity' legacy_default_adoption_preserves_state_config_model_and_identity
run_test 'legacy adoption rejects custom roots and hostile default lookalikes' legacy_adoption_refuses_custom_and_hostile_lookalikes
run_test 'real v0.7 Linux global supervisor is retired into a target-user managed bridge' legacy_linux_system_supervisor_is_transactionally_adopted
run_test 'post-intent v0.7 Linux failure restores files but never revives the retired node' legacy_linux_post_intent_failure_restores_files_but_stays_stopped
run_test 'retirement create/finalize failures obey the durable no-restart boundary' legacy_retirement_failures_obey_the_intent_boundary
run_test 'legacy unsigned updater is fenced before release resolution and stays fenced on rejection' legacy_updater_is_fenced_before_release_failure_and_stays_fenced
run_test 'unsafe or split recovery evidence fails before the v0.7 stop boundary' legacy_retirement_gate_fails_before_v07_stop
run_test 'installer distinguishes v0.7 TERM-only retirement from v0.8 quiescence' installer_distinguishes_v07_retirement_from_v08_quiescence
run_test 'marker-fsync crash leaves v0.7 runnable and resumes into an exact archive plus fresh state' legacy_marker_fsync_crash_keeps_v07_runnable_and_resumes
run_test 'pending Linux migration resumes only its bound system-user scope' pending_linux_adoption_resumes_only_its_bound_scope
run_test 'real v0.7 macOS agents are retired and replaced transactionally' legacy_macos_agents_are_retired_and_replaced
run_test 'managed macOS updates drain old nodes and never unload their running updater' managed_macos_update_drains_old_node_without_unloading_its_updater
run_test 'verified detached v0.7 PID is retired without losing its configuration' legacy_detached_pid_is_exactly_verified_and_retired
run_test 'historical no-root/no-sudo v0.7 process without node.pid is discovered and retired' legacy_untracked_no_sudo_process_is_discovered_and_retired
run_test 'hostile Linux supervisor lookalikes fail before reservation or download' legacy_linux_supervisor_lookalikes_fail_before_reservation
run_test '--no-service --no-auto-update has no start, health, service, or updater side effects' no_service_no_updater_really_is_install_only
run_test 'sudo/root execution normalizes ownership to the invoking community user' sudo_root_install_targets_the_invoking_user
run_test 'Windows remains a documented manual-binary path, not a shell install path' windows_is_manual_only
run_test 'update-only refuses downgrade before downloading artifacts' update_only_refuses_downgrade
run_test 'update-only preserves custom ports and an intentionally empty model' update_only_preserves_custom_port_and_empty_model
run_test 'degraded service health remains installed but is never labeled healthy' degraded_service_health_is_reported_truthfully
run_test 'managed system-user update waits through a graceful exit beyond 30 seconds' managed_system_user_update_waits_past_thirty_seconds_for_graceful_restart
run_test 'legacy installer delegates to stake-zero root installer without key generation' legacy_installer_delegates_without_generating_validator_material
run_test 'fresh mid-copy failure removes every newly introduced managed file' fresh_mid_copy_failure_removes_new_managed_files
run_test 'mid-copy update failure restores the complete prior install transaction' mid_copy_update_failure_restores_full_install
run_test 'service restart failure restores files and prior service state' service_failure_restores_full_install
run_test 'non-ready health failure restores files and prior service state' health_failure_restores_full_install
run_test 'systemd system/user and launchd files share the full transaction boundary' transaction_contract_covers_every_service_scope

finish_tests
