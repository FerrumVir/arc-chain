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
SESSION_HANDOFF="$REPO_ROOT/docs/SESSION_HANDOFF.md"
DESKTOP_README="$REPO_ROOT/desktop/README.md"
DESKTOP_FIRST_RUN="$REPO_ROOT/desktop/FIRST-RUN.md"
DESKTOP_CANONICAL="$REPO_ROOT/desktop/DESKTOP_CANONICAL.md"
DESKTOP_DISTRIBUTION="$REPO_ROOT/desktop/DISTRIBUTION.md"
DESKTOP_GAPS="$REPO_ROOT/desktop/PRODUCTION_GAPS.md"
CLAUDE_GUIDE="$REPO_ROOT/CLAUDE.md"
RELEASE_WORKFLOW="$REPO_ROOT/.github/workflows/release.yml"
CANDIDATE_VERSION=0.8.0

require_literal() {
    local file="$1" literal="$2" message="$3"
    grep -Fq -- "$literal" "$file" || {
        printf '%s: %s\n' "$message" "$literal"
        return 1
    }
}

candidate_version_is_consistent() {
    local workspace_version desktop_cargo_version desktop_tauri_version
    local desktop_npm_version desktop_npm_lock_version desktop_npm_lock_root_version
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
    desktop_npm_version="$(python3 -c \
        'import json,sys; print(json.load(open(sys.argv[1], encoding="utf-8"))["version"])' \
        "$REPO_ROOT/desktop/package.json")"
    desktop_npm_lock_version="$(python3 -c \
        'import json,sys; print(json.load(open(sys.argv[1], encoding="utf-8"))["version"])' \
        "$REPO_ROOT/desktop/package-lock.json")"
    desktop_npm_lock_root_version="$(python3 -c \
        'import json,sys; print(json.load(open(sys.argv[1], encoding="utf-8"))["packages"][""]["version"])' \
        "$REPO_ROOT/desktop/package-lock.json")"

    assert_equals "$CANDIDATE_VERSION" "$workspace_version" \
        'workspace version does not match the documented recovery candidate' || return 1
    assert_equals "$CANDIDATE_VERSION" "$desktop_cargo_version" \
        'desktop Cargo version does not match the recovery candidate' || return 1
    assert_equals "$CANDIDATE_VERSION" "$desktop_tauri_version" \
        'Tauri version does not match the recovery candidate' || return 1
    assert_equals "$CANDIDATE_VERSION" "$desktop_npm_version" \
        'desktop npm package version does not match the recovery candidate' || return 1
    assert_equals "$CANDIDATE_VERSION" "$desktop_npm_lock_version" \
        'desktop npm lock document version does not match the recovery candidate' || return 1
    assert_equals "$CANDIDATE_VERSION" "$desktop_npm_lock_root_version" \
        'desktop npm lock package root does not match the recovery candidate' || return 1
    require_literal "$RELEASE_WORKFLOW" '"desktop-npm-lock-root:$DESKTOP_NPM_LOCK_ROOT_VERSION"' \
        'release tag gate does not validate the npm lock package root' || return 1
    require_literal "$CHANGELOG" "## v$CANDIDATE_VERSION - Unreleased recovery candidate" \
        'changelog is missing the unreleased candidate heading' || return 1
    require_literal "$README" "v$CANDIDATE_VERSION / protocol v3" \
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
        require_literal "$file" "--proto '=https' --proto-redir '=https' --tlsv1.2" \
            'candidate install guide permits a non-HTTPS bootstrap or redirect' || return 1
        require_literal "$file" 'not published' \
            'candidate install guide could be mistaken for an already-published release' || return 1
    done
    require_literal "$HEADLESS" '$2 == "install.sh"' \
        'headless guide does not isolate the installer checksum row' || return 1
    require_literal "$HEADLESS" 'END { exit !found }' \
        'headless guide does not isolate and require the installer checksum row' || return 1
    if grep -Fq '0.7.12' "$README" "$HEADLESS" "$WALKTHROUGH" "$ROLLOUT"; then
        printf 'the superseded v0.7.12 candidate remains in active v0.8.0 operator docs\n'
        return 1
    fi
}

production_origins_and_evidence_are_exact() {
    local origin
    for origin in \
        https://149-28-32-76.nip.io \
        https://140-82-16-112.nip.io \
        https://136-244-109-1.nip.io \
        https://104-238-171-11.nip.io \
        https://202-182-107-41.nip.io \
        https://149-28-153-31.nip.io
    do
        require_literal "$README" "$origin" \
            'README is missing an exact production HTTPS origin' || return 1
        require_literal "$REPO_ROOT/desktop/src-tauri/src/rpc_client.rs" "$origin" \
            'Rust desktop defaults are missing an exact production HTTPS origin' || return 1
        require_literal "$REPO_ROOT/desktop/src/lib/tauri.ts" "$origin" \
            'browser fallback defaults are missing an exact production HTTPS origin' || return 1
    done

    if grep -Eq 'releases/latest/download/arc-(node|cli|desktop)' "$README"; then
        printf 'README still sends a release asset through the stale moving latest alias\n'
        return 1
    fi
    require_literal "$README" 'only a successful mined receipt confirms the 1 ARC credit' \
        'README presents faucet submission as confirmed credit' || return 1
    require_literal "$README" 'confirmed mined `0x25` receipt rows' \
        'README does not define the worker earnings evidence boundary' || return 1
    require_literal "$README" 'otherwise the value is null and the API returns the reason' \
        'README does not define fail-closed projected earnings' || return 1
    require_literal "$WALKTHROUGH" '/community/reward_receipt/$ARC_REWARD_TX' \
        'walkthrough does not poll the exact reward receipt endpoint' || return 1
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
    require_literal "$HEADLESS" 'do not pin v0.8.0' \
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

desktop_and_release_notes_match_the_artifact_and_reward_contract() {
    local asset release_notes
    release_notes="$(awk '
        /^[[:space:]]+RELEASE_NOTES:[[:space:]]+[|]$/ { capture=1; next }
        capture && /^[[:space:]]+run:[[:space:]]+[|]$/ { exit }
        capture { print }
    ' "$RELEASE_WORKFLOW")"

    for asset in \
        arc-node-linux-x86_64 \
        arc-cli-linux-x86_64 \
        arc-node-linux-arm64 \
        arc-cli-linux-arm64 \
        arc-node-macos-arm64 \
        arc-cli-macos-arm64 \
        arc-node-macos-x86_64 \
        arc-cli-macos-x86_64 \
        arc-node-windows-x86_64.exe \
        arc-cli-windows-x86_64.exe \
        arc-desktop-macos-arm64.dmg \
        arc-desktop-macos-x86_64.dmg \
        arc-desktop-windows-x86_64-setup.exe \
        arc-desktop-windows-x86_64.msi \
        arc-desktop-linux-x86_64.AppImage \
        arc-desktop-linux-x86_64.deb \
        arc-desktop-linux-x86_64.rpm
    do
        printf '%s\n' "$release_notes" | grep -Fq -- "$asset" || {
            printf 'generated v0.8.0 release notes omit exact artifact name: %s\n' "$asset"
            return 1
        }
    done

    for asset in \
        arc-desktop-macos-arm64.dmg \
        arc-desktop-macos-x86_64.dmg \
        arc-desktop-windows-x86_64-setup.exe \
        arc-desktop-windows-x86_64.msi \
        arc-desktop-linux-x86_64.AppImage \
        arc-desktop-linux-x86_64.deb \
        arc-desktop-linux-x86_64.rpm
    do
        require_literal "$README" "$asset" \
            'README desktop table drifted from a normalized release asset' || return 1
    done

    for literal in \
        'Linux ARM64 is headless-only' \
        'download or install without confirmation' \
        'stake-zero' \
        'only a successful mined `0x25`' \
        'Projected earnings are' \
        'null with an explicit reason'
    do
        printf '%s\n' "$release_notes" | grep -Fq -- "$literal" || {
            printf 'generated v0.8.0 release notes omit required support/evidence copy: %s\n' "$literal"
            return 1
        }
    done

    if grep -Eq 'releases/(download|tag)/v0[.]7[.](10|11)|releases/latest/download/arc-(node|cli|desktop)' \
        "$README" "$HEADLESS" "$WALKTHROUGH"; then
        printf 'an active v0.8.0 guide still links a stale desktop-only or moving binary asset\n'
        return 1
    fi
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
    require_literal "$SESSION_HANDOFF" 'Historical implementation handoff, not current rollout state' \
        'old inference session handoff can be mistaken for current rollout state' || return 1
    require_literal "$DESKTOP_CANONICAL" 'Historical recovery note, not the current desktop contract' \
        'old canonical-desktop snapshot lacks an archive boundary' || return 1
    require_literal "$DESKTOP_DISTRIBUTION" 'Historical planning document, not a v0.8.0 ship checklist' \
        'old Android distribution plan can be mistaken for the v0.8 ship gate' || return 1
    require_literal "$DESKTOP_GAPS" 'Historical gap list, not current release status' \
        'old desktop gap list can be mistaken for current release status' || return 1
    require_literal "$CLAUDE_GUIDE" 'Do not use this file as current operator guidance' \
        'old session guide can be mistaken for live-network instructions' || return 1
    if grep -Eq 'ARC[.]Node[_-]0[.]7[.]11|ARC[.]Node-0[.]7[.]11' "$GETTING_STARTED"; then
        printf 'Getting Started still contains stale v0.7.11 Linux package commands\n'
        return 1
    fi
}

readme_counts_and_desktop_secret_copy_match_the_tree() {
    local rust_lines rust_tests
    rust_lines="$(find "$REPO_ROOT/crates" "$REPO_ROOT/agents" "$REPO_ROOT/relayer" \
        -type f -name '*.rs' -print0 | xargs -0 wc -l | awk 'END { print $1 }')"
    rust_tests="$(find "$REPO_ROOT/crates" "$REPO_ROOT/agents" "$REPO_ROOT/relayer" \
        -type f -name '*.rs' -print0 | xargs -0 grep -Eh \
        '^[[:space:]]*#\[(tokio::)?test' | wc -l | tr -d ' ')"
    [ "$rust_lines" -ge 167000 ] || {
        printf 'README 167K+ Rust badge exceeds current measured source lines: %s\n' "$rust_lines"
        return 1
    }
    [ "$rust_tests" -ge 1700 ] || {
        printf 'README 1,700+ Rust-test badge exceeds current defined tests: %s\n' "$rust_tests"
        return 1
    }
    for literal in \
        'more than 167,000 physical lines of Rust' \
        'More than 1,700 Rust test functions' \
        'plus one narrowly' \
        'vendored `wasmer-derive` workspace member'
    do
        require_literal "$README" "$literal" \
            'README source inventory is stale or not reproducible' || return 1
    done
    if grep -Eq '121,900|1,363|34-endpoint|Every line is original' "$README"; then
        printf 'README retains a disproven source-count, endpoint-count, or authorship claim\n'
        return 1
    fi
    require_literal "$DESKTOP_README" 'recovery phrase is present' \
        'desktop README falsely implies the native store excludes its signing secret' || return 1
    require_literal "$GETTING_STARTED" 'v0.8.0 does not yet use an OS keychain' \
        'user guide omits the desktop recovery-secret storage boundary' || return 1
    require_literal "$DESKTOP_FIRST_RUN" 'only a successful mined receipt' \
        'first-run guide still upgrades faucet submission into confirmed credit' || return 1
}

persistence_rpc_and_transaction_copy_match_the_installer() {
    local file
    for file in "$README" "$HEADLESS" "$WALKTHROUGH"; do
        require_literal "$file" 'genesis.network-hash' \
            'active operator copy omits the persisted network-identity marker' || return 1
    done
    require_literal "$README" 'Do not reuse a v0.7.11-or-earlier data directory.' \
        'README does not fail closed on v2 WAL reuse' || return 1
    require_literal "$HEADLESS" 'Do not point v0.8.0 at a v0.7.11-or-earlier data directory.' \
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

run_test 'workspace, desktop, changelog, and README agree on unreleased v0.8.0' candidate_version_is_consistent
run_test 'candidate install commands pin exact v0.8.0 without claiming publication' candidate_install_commands_are_exact_and_honest
run_test 'README and headless guide share the same unpinned update-only commands' manual_updater_commands_are_identical
run_test 'headless platform claims match the canonical release asset contract' headless_platform_claims_match_release_assets
run_test 'desktop docs and generated release notes match artifacts, updater, and reward evidence' desktop_and_release_notes_match_the_artifact_and_reward_contract
run_test 'production origins and receipt-backed status copy are exact' production_origins_and_evidence_are_exact
run_test 'activation copy fails closed and archived guides carry recovery warnings' activation_and_archived_guides_fail_closed_in_copy
run_test 'operator docs require fresh v3 state, loopback RPC, and full transaction rollback' persistence_rpc_and_transaction_copy_match_the_installer
run_test 'README counts and desktop secret/payment copy match the current tree' readme_counts_and_desktop_secret_copy_match_the_tree

finish_tests
