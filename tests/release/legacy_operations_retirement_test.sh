#!/usr/bin/env bash
set -uo pipefail

TEST_DIR="$(CDPATH='' cd -- "$(dirname -- "$0")" && pwd)"
REPO_ROOT="$(CDPATH='' cd -- "$TEST_DIR/../.." && pwd)"
# shellcheck source=/dev/null
. "$TEST_DIR/helpers/testlib.sh"

RETIRED_SCRIPTS=(
    "$REPO_ROOT/scripts/arc-watchdog.sh"
    "$REPO_ROOT/scripts/arc-export-volatile-state.sh"
    "$REPO_ROOT/scripts/rolling-upgrade.sh"
    "$REPO_ROOT/scripts/arc-rolling-restart.sh"
    "$REPO_ROOT/scripts/arc-remote-relaunch.sh"
    "$REPO_ROOT/scripts/install-self-heal.sh"
    "$REPO_ROOT/scripts/arc-self-heal.sh"
    "$REPO_ROOT/scripts/deploy-testnet.sh"
    "$REPO_ROOT/deploy/setup-testnet.sh"
    "$REPO_ROOT/deploy/monitor.sh"
    "$REPO_ROOT/deploy/teardown.sh"
    "$REPO_ROOT/scripts/arc-tunnel-watchdog.sh"
    "$REPO_ROOT/scripts/arc-health-check.sh"
    "$REPO_ROOT/scripts/deploy-explorer.sh"
    "$REPO_ROOT/scripts/setup-vps.sh"
    "$REPO_ROOT/scripts/create-testnet.sh"
    "$REPO_ROOT/scripts/testnet.sh"
    "$REPO_ROOT/scripts/run_cluster.sh"
    "$REPO_ROOT/scripts/arc-community-register.sh"
    "$REPO_ROOT/scripts/tps-generator.sh"
    "$REPO_ROOT/scripts/load-test.sh"
    "$REPO_ROOT/scripts/inference-benchmark.sh"
    "$REPO_ROOT/scripts/inference-router.sh"
    "$REPO_ROOT/scripts/inference-tps-bench.sh"
    "$REPO_ROOT/scripts/monitor-testnet.sh"
    "$REPO_ROOT/scripts/run-node.sh"
)

retirement_guards_are_early_and_unconditional() {
    local script header
    for script in "${RETIRED_SCRIPTS[@]}"; do
        header="$(sed -n '1,7p' "$script")"
        assert_equals '#!/usr/bin/env bash' "$(sed -n '1p' "$script")" \
            "$script lost its bash entrypoint" || return 1
        assert_equals '# ARC_RETIRED_LIVE_TOOL_V3_REQUIRED' "$(sed -n '2p' "$script")" \
            "$script does not declare the v3 retirement boundary first" || return 1
        assert_equals 'set -euo pipefail' "$(sed -n '3p' "$script")" \
            "$script does not enable strict mode before its retirement message" || return 1
        assert_equals 'exit 78' "$(sed -n '7p' "$script")" \
            "$script does not exit with EX_CONFIG before legacy logic" || return 1
        if printf '%s\n' "$header" | grep -Eq \
            '^[[:space:]]*(if|case|while|until|for)[[:space:]]|\$\{|\$[@*]|ARC_(ALLOW|FORCE|UNSAFE|OVERRIDE)'; then
            printf '%s has a conditional or override in its retirement preamble\n' "$script"
            return 1
        fi
    done
}

install_operation_shims() {
    local fake_bin="$1" marker="$2" operation
    mkdir -p "$fake_bin"
    cat > "$fake_bin/blocked-operation" <<'SHIM'
#!/usr/bin/env bash
printf '%s %s\n' "${0##*/}" "$*" >> "${ARC_OPERATION_MARKER:?}"
exit 99
SHIM
    chmod +x "$fake_bin/blocked-operation"
    for operation in \
        ssh scp rsync hcloud vultr vultr-cli curl wget systemctl service \
        pkill killall screen setsid nohup rm mv cp install chown chmod ufw \
        apt apt-get dnf yum brew docker terraform ansible ansible-playbook sleep \
        sudo git cargo rustup npm npx pip pip3 python python3 tc
    do
        ln -s blocked-operation "$fake_bin/$operation"
    done
    : > "$marker"
}

retired_scripts_exit_before_any_operation() {
    local temp_dir fake_bin marker script output status
    temp_dir="$(mktemp -d "${TMPDIR:-/tmp}/arc-legacy-ops.XXXXXX")"
    fake_bin="$temp_dir/bin"
    marker="$temp_dir/operation-called"
    mkdir -p "$temp_dir/home"
    install_operation_shims "$fake_bin" "$marker"

    for script in "${RETIRED_SCRIPTS[@]}"; do
        # Refuse to execute a regression that has already lost the static
        # boundary; this keeps the contract test itself offline and harmless.
        if [ "$(sed -n '7p' "$script")" != 'exit 78' ]; then
            printf '%s has no verified early exit; refusing to execute it\n' "$script"
            /bin/rm -rf -- "$temp_dir"
            return 1
        fi
        : > "$marker"
        output="$(
            HOME="$temp_dir/home" \
            PATH="$fake_bin:/usr/bin:/bin" \
            ARC_OPERATION_MARKER="$marker" \
            /bin/bash "$script" --run --force --continue-on-fail 2>&1
        )"
        status=$?
        assert_equals 78 "$status" "$script did not return EX_CONFIG" || {
            /bin/rm -rf -- "$temp_dir"
            return 1
        }
        if ! printf '%s\n' "$output" | grep -Fq 'RETIRED:'; then
            printf '%s did not explain that it is retired\n' "$script"
            /bin/rm -rf -- "$temp_dir"
            return 1
        fi
        if [ -s "$marker" ]; then
            printf '%s reached an operational command before exiting:\n' "$script"
            sed -n '1,8p' "$marker"
            /bin/rm -rf -- "$temp_dir"
            return 1
        fi
    done

    /bin/rm -rf -- "$temp_dir"
}

deploy_makefile_has_no_operational_escape_hatch() {
    local makefile="$REPO_ROOT/deploy/Makefile" target output status
    local targets='setup status watch teardown logs ssh restart restart-all health'

    if grep -En '^\t.*(ssh|scp|rsync|hcloud|curl|systemctl|rm[[:space:]])' "$makefile"; then
        printf 'deploy/Makefile still contains an operational recipe\n'
        return 1
    fi
    for target in $targets; do
        output="$(make -s -C "$REPO_ROOT/deploy" "$target" 2>&1)"
        status=$?
        if [ "$status" -eq 0 ]; then
            printf 'deploy/Makefile target %s did not fail closed\n' "$target"
            return 1
        fi
        if ! printf '%s\n' "$output" | grep -Fq 'RETIRED:'; then
            printf 'deploy/Makefile target %s lacks the recovery message\n' "$target"
            return 1
        fi
    done
    make -s -C "$REPO_ROOT/deploy" help >/dev/null || {
        printf 'deploy/Makefile local help target should remain usable\n'
        return 1
    }
}

cloud_templates_are_inert_and_non_validator() {
    local cloud_init="$REPO_ROOT/deploy/cloud-init.yml" config

    assert_file_contains "$cloud_init" '^#cloud-config$' \
        'retired cloud-init file is not valid cloud-config input' || return 1
    assert_file_contains "$cloud_init" 'intentionally inert' \
        'retired cloud-init file does not explain its no-op contract' || return 1
    assert_file_not_contains "$cloud_init" \
        '(^|[[:space:]])(runcmd|bootcmd|packages|users):|curl|wget|systemctl|ufw|releases/latest|arc-node-linux' \
        'retired cloud-init still installs, downloads, or enables software' || return 1

    for config in "$REPO_ROOT"/deploy/config/node-*.toml; do
        assert_file_contains "$config" '^listen = "127[.]0[.]0[.]1:0"$' \
            "$config is not loopback-only" || return 1
        assert_file_contains "$config" '^stake = 0$' \
            "$config is not stake-zero" || return 1
        assert_file_contains "$config" '^min_stake = 0$' \
            "$config does not disable minimum validator stake" || return 1
        assert_file_not_contains "$config" \
            '(^|[[:space:]])seed[[:space:]]*=|stake[[:space:]]*=[[:space:]]*[1-9]|0[.]0[.]0[.]0|__NODE_[0-9]+_IP__' \
            "$config still contains a validator seed, positive stake, public listener, or host placeholder" || return 1
    done
}

auto_update_is_a_local_installed_updater_only() {
    local updater="$REPO_ROOT/scripts/auto-update.sh"
    local temp_dir install_dir marker output status

    if grep -En '^[[:space:]]*(ssh|scp|rsync|curl|wget|systemctl|service|sudo|hcloud)[[:space:]]' \
        "$updater"; then
        printf 'auto-update.sh contains a direct remote, fleet, or service command\n'
        return 1
    fi
    assert_file_contains "$updater" \
        '^exec "\$INSTALLER" --update-only --install-dir "\$ARC_DIR"$' \
        'auto-update.sh does not delegate exactly to the installed checksum-verifying updater' || return 1

    output="$(ARC_DIR=relative /bin/bash "$updater" --once 2>&1)"
    status=$?
    if [ "$status" -eq 0 ] || ! printf '%s\n' "$output" | grep -Fq 'absolute local install path'; then
        printf 'auto-update.sh accepted a relative or ambiguous install path\n'
        return 1
    fi

    temp_dir="$(mktemp -d "${TMPDIR:-/tmp}/arc-local-update.XXXXXX")"
    install_dir="$temp_dir/community"
    marker="$temp_dir/args"
    mkdir -p "$install_dir/bin"
    cat > "$install_dir/bin/arc-installer" <<'UPDATER'
#!/usr/bin/env bash
printf '%s\n' "$*" > "${ARC_UPDATE_MARKER:?}"
UPDATER
    chmod +x "$install_dir/bin/arc-installer"
    ARC_DIR="$install_dir" ARC_UPDATE_MARKER="$marker" /bin/bash "$updater" --once || {
        printf 'auto-update.sh did not invoke the installed local updater\n'
        /bin/rm -rf -- "$temp_dir"
        return 1
    }
    assert_equals "--update-only --install-dir $install_dir" "$(cat "$marker")" \
        'auto-update.sh changed the local updater contract' || {
        /bin/rm -rf -- "$temp_dir"
        return 1
    }
    /bin/rm -rf -- "$temp_dir"
}

compatibility_installers_fail_closed_without_local_canonical_installer() {
    local temp_dir fake_bin marker wrapper copy output status
    temp_dir="$(mktemp -d "${TMPDIR:-/tmp}/arc-local-wrapper.XXXXXX")"
    fake_bin="$temp_dir/bin"
    marker="$temp_dir/operation-called"
    mkdir -p "$temp_dir/wrappers" "$temp_dir/home"
    install_operation_shims "$fake_bin" "$marker"

    for wrapper in \
        "$REPO_ROOT/scripts/install-node.sh" \
        "$REPO_ROOT/scripts/install-community-node.sh" \
        "$REPO_ROOT/scripts/sero-quickstart.sh"
    do
        copy="$temp_dir/wrappers/${wrapper##*/}"
        /bin/cp "$wrapper" "$copy"
        : > "$marker"
        output="$(
            HOME="$temp_dir/home" \
            PATH="$fake_bin:/usr/bin:/bin" \
            ARC_OPERATION_MARKER="$marker" \
            /bin/bash "$copy" 2>&1
        )"
        status=$?
        assert_equals 78 "$status" "${wrapper##*/} did not fail closed without local install.sh" || {
            /bin/rm -rf -- "$temp_dir"
            return 1
        }
        if ! printf '%s\n' "$output" | grep -Fq 'mutable branch'; then
            printf '%s does not explain why remote fallback is refused\n' "${wrapper##*/}"
            /bin/rm -rf -- "$temp_dir"
            return 1
        fi
        if [ -s "$marker" ]; then
            printf '%s reached an external operation without local install.sh\n' "${wrapper##*/}"
            /bin/rm -rf -- "$temp_dir"
            return 1
        fi
    done
    /bin/rm -rf -- "$temp_dir"
}

preserved_smoke_tools_are_loopback_only_and_truthful() {
    local temp_dir fake_bin marker output status
    temp_dir="$(mktemp -d "${TMPDIR:-/tmp}/arc-local-smoke.XXXXXX")"
    fake_bin="$temp_dir/bin"
    marker="$temp_dir/operation-called"
    install_operation_shims "$fake_bin" "$marker"

    output="$(
        HOME="$temp_dir" \
        PATH="$fake_bin:/usr/bin:/bin" \
        ARC_OPERATION_MARKER="$marker" \
        /bin/bash "$REPO_ROOT/scripts/test-inference.sh" hello 149.28.32.76:9090 2>&1
    )"
    status=$?
    assert_equals 78 "$status" 'test-inference.sh did not retire its remote endpoint path' || {
        /bin/rm -rf -- "$temp_dir"
        return 1
    }
    if [ -s "$marker" ]; then
        printf 'test-inference.sh contacted a public endpoint before rejecting it\n'
        /bin/rm -rf -- "$temp_dir"
        return 1
    fi
    if ! printf '%s\n' "$output" | grep -Fq 'accepts only 127.0.0.1'; then
        printf 'test-inference.sh does not explain its loopback-only contract\n'
        /bin/rm -rf -- "$temp_dir"
        return 1
    fi
    assert_file_contains "$REPO_ROOT/scripts/quick-test.sh" \
        '^RPC="http://127[.]0[.]0[.]1:9944"$' \
        'quick-test.sh is not fixed to loopback' || {
        /bin/rm -rf -- "$temp_dir"
        return 1
    }
    assert_file_contains "$REPO_ROOT/scripts/check-attestations.sh" \
        'raw 0x16 attestations pay nothing' \
        'attestation viewer still implies raw attestations are earnings' || {
        /bin/rm -rf -- "$temp_dir"
        return 1
    }
    /bin/rm -rf -- "$temp_dir"
}

no_unretired_script_hardcodes_public_writes_or_rpc_bind() {
    local script
    for script in "$REPO_ROOT"/scripts/*.sh; do
        if grep -Eq \
            '149[.]28[.]|140[.]82[.]|136[.]244[.]|104[.]238[.]|202[.]182[.]|216[.]238[.]|139[.]84[.]' \
            "$script" \
            && grep -Eq -- '-X[[:space:]]+POST|--request[[:space:]]+POST' "$script"; then
            printf '%s still contains a hard-coded public-v2 write path\n' "$script"
            return 1
        fi
        if grep -Eq -- '--rpc([[:space:]]+|=)("?0[.]0[.]0[.]0|0[.]0[.]0[.]0)' "$script"; then
            printf '%s still launches a publicly bound RPC listener\n' "$script"
            return 1
        fi
    done
}

current_scripts_and_docs_forbid_mutable_remote_execution() {
    local file
    for file in \
        "$REPO_ROOT"/scripts/*.sh \
        "$REPO_ROOT/README.md" \
        "$REPO_ROOT/docs/GETTING_STARTED.md" \
        "$REPO_ROOT/scripts/README.md" \
        "$REPO_ROOT/testnet/README.md" \
        "$REPO_ROOT/deploy/README.md"
    do
        if grep -Eq 'raw[.]githubusercontent[.]com/[^/]+/[^/]+/(main|master)/' "$file"; then
            printf '%s still references executable content from mutable raw main\n' "$file"
            return 1
        fi
    done
}

operator_docs_name_the_recovery_boundary() {
    assert_file_contains "$REPO_ROOT/scripts/README.md" \
        'All legacy scripts that can provision, upgrade, restart, or self-heal' \
        'scripts README does not state the unconditional retirement boundary' || return 1
    assert_file_contains "$REPO_ROOT/scripts/README.md" \
        'There is no environment flag or command-line override' \
        'scripts README leaves an override ambiguous' || return 1
    assert_file_contains "$REPO_ROOT/scripts/README.md" \
        'scripts/recovery/recovery_rollout[.]py' \
        'scripts README does not name the manifest-bound recovery tool' || return 1
    assert_file_contains "$REPO_ROOT/deploy/README.md" \
        'not an approved way' \
        'deploy README still presents v2 tooling as operational' || return 1
    assert_file_contains "$REPO_ROOT/deploy/README.md" \
        'scripts/recovery/recovery_rollout[.]py' \
        'deploy README does not name the manifest-bound recovery tool' || return 1
    assert_file_contains "$REPO_ROOT/testnet/README.md" \
        'There is currently no supported “quick join” or validator-deployment command' \
        'testnet README still presents a live quick-join or validator deployment path' || return 1
    if grep -Eq \
        'raw[.]githubusercontent[.]com/[^/]+/[^/]+/(main|master)/|releases/latest|^[[:space:]]*curl.*[|][[:space:]]*(ba)?sh|--validator-seed|make[[:space:]]+(setup|restart|teardown)' \
        "$REPO_ROOT/testnet/README.md" "$REPO_ROOT/deploy/README.md" "$REPO_ROOT/scripts/README.md"; then
        printf 'current operator docs still contain a raw-main/latest installer or retired command\n'
        return 1
    fi
}

historical_docs_are_non_operational_and_truthful() {
    assert_file_contains "$REPO_ROOT/PLAN.md" \
        '^# .*\(ARCHIVED\)$' \
        'historical plan is not clearly archived at its title' || return 1
    assert_file_not_contains "$REPO_ROOT/PLAN.md" \
        'scripts/rolling-upgrade[.]sh|--reset-state' \
        'historical plan still contains a runnable legacy reset or rolling command' || return 1
    assert_file_contains "$REPO_ROOT/ARCHITECTURE.md" \
        'ARCHIVED — NOT AN OPERATOR RUNBOOK OR CURRENT NETWORK STATUS' \
        'historical architecture still presents itself as current authority' || return 1
    assert_file_contains "$REPO_ROOT/ARCHITECTURE.md" \
        'raw `0x16` events are not earnings' \
        'historical architecture still implies a raw attestation is payment' || return 1
    assert_file_not_contains "$REPO_ROOT/ARCHITECTURE.md" \
        'earnings \(tx 0x16 events\)|credits the worker.*InferenceAttestation' \
        'historical architecture retains the false raw-attestation reward path' || return 1
    assert_file_contains "$REPO_ROOT/docs/SERO-DEMO.md" \
        'ARCHIVED HISTORICAL MATERIAL — DO NOT USE AS A WALKTHROUGH' \
        'historical Sero demo is not clearly archived' || return 1
    assert_file_not_contains "$REPO_ROOT/docs/SERO-DEMO.md" \
        'https?://[0-9]|curl|install-community-node[.]sh' \
        'historical Sero demo still exposes a live IP or runnable installer command' || return 1
}

run_test 'legacy live/cloud scripts have an early unconditional retirement guard' \
    retirement_guards_are_early_and_unconditional
run_test 'retired scripts exit before SSH, cloud, service, process, or destructive commands' \
    retired_scripts_exit_before_any_operation
run_test 'deploy Makefile exposes only local help and fail-closed legacy targets' \
    deploy_makefile_has_no_operational_escape_hatch
run_test 'cloud-init and node templates are inert, loopback-only, and stake-zero' \
    cloud_templates_are_inert_and_non_validator
run_test 'auto-update delegates only to an absolute local checksum-verifying updater' \
    auto_update_is_a_local_installed_updater_only
run_test 'compatibility installers refuse mutable remote fallback code' \
    compatibility_installers_fail_closed_without_local_canonical_installer
run_test 'preserved local smoke tools reject public endpoints and raw-attestation reward claims' \
    preserved_smoke_tools_are_loopback_only_and_truthful
run_test 'no unretired shell script hard-codes a public-v2 write or public RPC bind' \
    no_unretired_script_hardcodes_public_writes_or_rpc_bind
run_test 'current shell and operator docs never execute code from mutable raw main' \
    current_scripts_and_docs_forbid_mutable_remote_execution
run_test 'operator docs describe the v3 recovery boundary without an override' \
    operator_docs_name_the_recovery_boundary
run_test 'historical plans and demos cannot be mistaken for live operations' \
    historical_docs_are_non_operational_and_truthful

finish_tests
