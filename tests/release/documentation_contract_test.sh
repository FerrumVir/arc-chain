#!/usr/bin/env bash
set -uo pipefail

TEST_DIR="$(CDPATH='' cd -- "$(dirname -- "$0")" && pwd)"
REPO_ROOT="$(CDPATH='' cd -- "$TEST_DIR/../.." && pwd)"
# shellcheck source=/dev/null
. "$TEST_DIR/helpers/testlib.sh"

README="$REPO_ROOT/README.md"
CHANGELOG="$REPO_ROOT/CHANGELOG.md"
HEADLESS="$REPO_ROOT/docs/HEADLESS_INSTALL.md"
WALKTHROUGH="$REPO_ROOT/docs/COMMUNITY-NODE-WALKTHROUGH.md"
ROLLOUT="$REPO_ROOT/docs/VALIDATOR-FLEET-ROLLOUT.md"
GETTING_STARTED="$REPO_ROOT/docs/GETTING_STARTED.md"
STATUS_DOC="$REPO_ROOT/docs/STATUS.md"
ANNOUNCEMENT="$REPO_ROOT/docs/ANNOUNCEMENT.md"
DEMO_RUNBOOK="$REPO_ROOT/docs/DEMO-RUNBOOK.md"
CANDIDATE_VERSION=0.7.12

require_literal() {
    local file="$1" literal="$2" message="$3"
    grep -Fq -- "$literal" "$file" || {
        printf '%s: %s\n' "$message" "$literal"
        return 1
    }
}

candidate_version_is_consistent() {
    local workspace_version desktop_cargo_version desktop_tauri_version
    workspace_version="$(awk '
        /^\[workspace\.package\]$/ { in_package=1; next }
        /^\[/ { in_package=0 }
        in_package && /^version[[:space:]]*=/ {
            gsub(/[[:space:]\"]/, "", $0)
            sub(/^version=/, "", $0)
            print
            exit
        }
    ' "$REPO_ROOT/Cargo.toml")"
    desktop_cargo_version="$(awk '
        /^\[package\]$/ { in_package=1; next }
        /^\[/ { in_package=0 }
        in_package && /^version[[:space:]]*=/ {
            gsub(/[[:space:]\"]/, "", $0)
            sub(/^version=/, "", $0)
            print
            exit
        }
    ' "$REPO_ROOT/desktop/src-tauri/Cargo.toml")"
    desktop_tauri_version="$(python3 -c \
        'import json,sys; print(json.load(open(sys.argv[1], encoding="utf-8"))["version"])' \
        "$REPO_ROOT/desktop/src-tauri/tauri.conf.json")"

    assert_equals "$CANDIDATE_VERSION" "$workspace_version" \
        'workspace version does not match the documented recovery candidate' || return 1
    assert_equals "$CANDIDATE_VERSION" "$desktop_cargo_version" \
        'desktop Cargo version does not match the recovery candidate' || return 1
    assert_equals "$CANDIDATE_VERSION" "$desktop_tauri_version" \
        'Tauri version does not match the recovery candidate' || return 1
    require_literal "$CHANGELOG" "## v$CANDIDATE_VERSION - Unreleased recovery candidate" \
        'changelog is missing the unreleased candidate heading' || return 1
    require_literal "$README" "v$CANDIDATE_VERSION/v3" \
        'README does not identify the candidate version' || return 1
}

candidate_install_commands_are_exact_and_honest() {
    local exact_url="https://github.com/FerrumVir/arc-chain/releases/download/v$CANDIDATE_VERSION/install.sh"
    local file
    for file in "$README" "$HEADLESS" "$WALKTHROUGH"; do
        require_literal "$file" "$exact_url" \
            'candidate install guide does not download the exact installer tag' || return 1
        require_literal "$file" "--version $CANDIDATE_VERSION" \
            'candidate install guide does not pin the matching installer version' || return 1
        require_literal "$file" 'not published' \
            'candidate install guide could be mistaken for an already-published release' || return 1
    done
    if grep -Fq '0.8.0' "$README" "$HEADLESS" "$WALKTHROUGH" "$ROLLOUT"; then
        printf 'a future v0.8.0 example remains in the v0.7.12 operator docs\n'
        return 1
    fi
}

manual_updater_commands_are_identical() {
    local user_command system_command file
    user_command='"$HOME/.arc/bin/arc-installer" --update-only --install-dir "$HOME/.arc"'
    system_command='sudo /var/lib/arc-chain/bin/arc-installer --update-only --install-dir /var/lib/arc-chain --system-service'
    for file in "$README" "$HEADLESS"; do
        require_literal "$file" "$user_command" \
            'user updater command drifted between operator docs' || return 1
        require_literal "$file" "$system_command" \
            'system updater command drifted between operator docs' || return 1
    done
    require_literal "$HEADLESS" 'do not pin v0.7.12' \
        'updater docs do not distinguish discovery from the pinned initial install' || return 1
}

headless_platform_claims_match_release_assets() {
    local asset
    for asset in \
        arc-node-linux-x86_64 \
        arc-node-linux-arm64 \
        arc-node-macos-arm64 \
        arc-node-macos-x86_64 \
        arc-node-windows-x86_64.exe
    do
        require_literal "$HEADLESS" "$asset" \
            'headless target table is missing a required release asset' || return 1
    done
    require_literal "$HEADLESS" 'There is no' \
        'headless guide does not call out unsupported combinations' || return 1
    require_literal "$HEADLESS" 'no Windows ARM64 release' \
        'headless guide incorrectly leaves Windows ARM64 support ambiguous' || return 1
}

activation_and_archived_guides_fail_closed_in_copy() {
    require_literal "$HEADLESS" 'Absence means consensus' \
        'headless guide does not explain absent activation fail-closed behavior' || return 1
    require_literal "$WALKTHROUGH" 'An absent genesis activation fails closed' \
        'walkthrough does not stop reward claims when activation is absent' || return 1
    require_literal "$ROLLOUT" 'Absence is the fail-closed disabled state' \
        'rollout does not define the disabled activation representation' || return 1

    require_literal "$GETTING_STARTED" 'Recovery notice (2026-08-26)' \
        'stale GUI guide lacks a recovery warning' || return 1
    require_literal "$STATUS_DOC" 'do **not** rely on the live, one-command join' \
        'archived status still presents old live/install claims without warning' || return 1
    require_literal "$ANNOUNCEMENT" 'DO NOT PUBLISH OR USE AS CURRENT INSTALL INSTRUCTIONS' \
        'archived announcement can still be mistaken for current marketing copy' || return 1
    require_literal "$DEMO_RUNBOOK" 'Do not use the legacy v0.7.7 installer' \
        'old demo runbook still presents v0.7.7 as the current CLI path' || return 1
    if grep -Eq 'ARC[.]Node[_-]0[.]7[.]11|ARC[.]Node-0[.]7[.]11' "$GETTING_STARTED"; then
        printf 'Getting Started still contains stale v0.7.11 Linux package commands\n'
        return 1
    fi
}

persistence_rpc_and_transaction_copy_match_the_installer() {
    local file
    for file in "$README" "$HEADLESS" "$WALKTHROUGH"; do
        require_literal "$file" 'genesis.network-hash' \
            'active operator copy omits the persisted network-identity marker' || return 1
    done
    require_literal "$README" 'Do not reuse a v0.7.11-or-earlier data directory.' \
        'README does not fail closed on v2 WAL reuse' || return 1
    require_literal "$HEADLESS" 'Do not point v0.7.12 at a v0.7.11-or-earlier data directory.' \
        'headless guide does not require a fresh observer data directory' || return 1
    require_literal "$WALKTHROUGH" 'Validators need the approved canonical checkpoint migration instead.' \
        'walkthrough omits the validator checkpoint-only migration rule' || return 1
    require_literal "$README" 'bind RPC to `127.0.0.1` only' \
        'README does not disclose the managed loopback-only RPC default' || return 1
    require_literal "$HEADLESS" 'bound only to `127.0.0.1`' \
        'headless guide does not disclose the loopback-only RPC default' || return 1
    require_literal "$HEADLESS" 'restores that complete snapshot' \
        'headless guide still describes a binary-only rollback' || return 1
    require_literal "$README" 'rollback is not a migration' \
        'README could present install rollback as persisted-state migration' || return 1
}

run_test 'workspace, desktop, changelog, and README agree on unreleased v0.7.12' candidate_version_is_consistent
run_test 'candidate install commands pin exact v0.7.12 without claiming publication' candidate_install_commands_are_exact_and_honest
run_test 'README and headless guide share the same unpinned update-only commands' manual_updater_commands_are_identical
run_test 'headless platform claims match the canonical release asset contract' headless_platform_claims_match_release_assets
run_test 'activation copy fails closed and archived guides carry recovery warnings' activation_and_archived_guides_fail_closed_in_copy
run_test 'operator docs require fresh v3 state, loopback RPC, and full transaction rollback' persistence_rpc_and_transaction_copy_match_the_installer

finish_tests
