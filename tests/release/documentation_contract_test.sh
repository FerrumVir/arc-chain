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
RECOVERY_README="$REPO_ROOT/scripts/recovery/README.md"
ARCHIVE_TOOL="$REPO_ROOT/scripts/recovery/archive-fleet-to-drive.sh"
CANARY_RUNBOOK="$REPO_ROOT/docs/MACOS-PRETAG-COMMUNITY-CANARY.md"
CANARY_HELPER="$REPO_ROOT/scripts/release/macos-community-canary.py"
VAULT_HELPER="$REPO_ROOT/scripts/release/restore-validator-vault.py"
PRODUCTION_RECOVERY_AUDIT="$REPO_ROOT/docs/PRODUCTION-RECOVERY-AUDIT-2026-08-26.md"
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
    require_literal "$CHANGELOG" "## v$CANDIDATE_VERSION - Release-preparation snapshot (2026-08-31)" \
        'changelog is missing the dated release-preparation heading' || return 1
    require_literal "$CHANGELOG" 'Tag-stable lifecycle note' \
        'changelog lifecycle statement can become false after publication' || return 1
    require_literal "$README" "v$CANDIDATE_VERSION / protocol v3" \
        'README does not identify the candidate version' || return 1
    require_literal "$README" 'Source-freeze snapshot (2026-08-31; tag-stable)' \
        'README release status is not preserved as a dated source-freeze fact' || return 1
    require_literal "$README" 'pre-tag statement, not a live status probe' \
        'README can be mistaken for a live publication-status probe' || return 1
}

candidate_install_commands_are_exact_and_honest() {
    local exact_url="https://raw.githubusercontent.com/FerrumVir/arc-chain/v$CANDIDATE_VERSION/install.sh"
    local digest_line download_line execute_line file installer_sha
    if command -v sha256sum >/dev/null 2>&1; then
        installer_sha="$(sha256sum "$REPO_ROOT/install.sh" | awk '{print $1}')"
    else
        installer_sha="$(shasum -a 256 "$REPO_ROOT/install.sh" | awk '{print $1}')"
    fi
    for file in "$README" "$HEADLESS" "$WALKTHROUGH"; do
        require_literal "$file" "$exact_url" \
            'candidate install guide does not download the exact installer tag' || return 1
        require_literal "$file" "--version $CANDIDATE_VERSION" \
            'candidate install guide does not pin the matching installer version' || return 1
        require_literal "$file" "$installer_sha" \
            'candidate install guide does not pin the exact installer SHA-256' || return 1
        require_literal "$file" "ARC_INSTALL_SHA256=$installer_sha" \
            'candidate install guide does not bind the downloaded script to its digest' || return 1
        require_literal "$file" 'sha256sum -c -' \
            'candidate install guide does not verify the bootstrap on Linux' || return 1
        require_literal "$file" 'shasum -a 256 -c -' \
            'candidate install guide does not verify the bootstrap on macOS' || return 1
        require_literal "$file" "--proto '=https' --proto-redir '=https' --tlsv1.2" \
            'candidate install guide permits a non-HTTPS bootstrap or redirect' || return 1
        require_literal "$file" 'not published' \
            'candidate install guide could be mistaken for an already-published release' || return 1
        download_line="$(grep -nF -- "$exact_url" "$file" | head -n 1 | cut -d: -f1)"
        digest_line="$(grep -nF -- "ARC_INSTALL_SHA256=$installer_sha" "$file" | head -n 1 | cut -d: -f1)"
        execute_line="$(grep -nF -- "bash install.sh --version $CANDIDATE_VERSION" "$file" | head -n 1 | cut -d: -f1)"
        if [ -z "$download_line" ] || [ -z "$digest_line" ] || [ -z "$execute_line" ] \
            || [ "$download_line" -ge "$digest_line" ] || [ "$digest_line" -ge "$execute_line" ]; then
            printf 'candidate guide executes install.sh before verifying the pinned digest: %s\n' "$file"
            return 1
        fi
    done
    require_literal "$HEADLESS" 'SHA256SUMS.sig' \
        'headless guide omits the detached release-manifest signature' || return 1
    require_literal "$HEADLESS" 'arc-release-manifest-v1' \
        'headless guide omits the exact signature namespace' || return 1
    if grep -Fq '0.7.12' "$README" "$HEADLESS" "$WALKTHROUGH" "$ROLLOUT"; then
        printf 'the superseded v0.7.12 candidate remains in active v0.8.0 operator docs\n'
        return 1
    fi
}

production_origins_and_evidence_are_exact() {
    local origin
    for origin in \
        https://149.28.32.76 \
        https://140.82.16.112 \
        https://136.244.109.1 \
        https://104.238.171.11 \
        https://202.182.107.41 \
        https://149.28.153.31
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

    require_literal "$README" 'SHA-pinned Caddy 2.11.4' \
        'README omits the exact candidate direct-IP TLS gateway' || return 1
    require_literal "$ROLLOUT" "Let's Encrypt's production ACME" \
        'rollout omits the public direct-IP certificate issuer' || return 1
    require_literal "$ROLLOUT" '`shortlived` profile and HTTP-01 challenge' \
        'rollout omits the direct-IP certificate profile/challenge boundary' || return 1
    require_literal "$REPO_ROOT/scripts/recovery/recovery_rollout.py" \
        'profile shortlived' \
        'rollout implementation no longer pins the short-lived ACME profile' || return 1
    require_literal "$REPO_ROOT/scripts/recovery/recovery_rollout.py" \
        'disable_tlsalpn_challenge' \
        'rollout implementation no longer forces the HTTP-01 path' || return 1
    if grep -ERq 'https://[0-9-]+[.](nip|sslip)[.]io' \
        "$README" "$CHANGELOG" "$HEADLESS" "$WALKTHROUGH" "$ROLLOUT" \
        "$GETTING_STARTED" "$REPO_ROOT/scripts/recovery/README.md"; then
        printf 'active candidate documentation retains a wildcard-DNS RPC origin\n'
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
    for release_claim in \
        'built on the oldest supported Ubuntu baseline, 22.04' \
        '22.04, 24.04, and 26.04 containers with `DISPLAY` unset' \
        'Linux ARM64 is built on Ubuntu 24.04 ARM'
    do
        require_literal "$HEADLESS" "$release_claim" \
            'headless guide omits the enforced Ubuntu compatibility boundary' || return 1
    done
    for readme_claim in \
        'built on Ubuntu 22.04 and must boot with `DISPLAY` unset on Ubuntu 22.04' \
        'the ARM64 artifact has the same GUI-free'
    do
        require_literal "$README" "$readme_claim" \
            'README omits the enforced Ubuntu compatibility boundary' || return 1
    done
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

    for literal in \
        'ARC_INSTALL_SHA256=__ARC_INSTALL_SHA256__' \
        'installer_row="$(/usr/bin/sha256sum release-files/install.sh)"' \
        'installer_sha256="${installer_row%% *}"' \
        "placeholder_count=\"\$(/usr/bin/grep -Fo '__ARC_INSTALL_SHA256__'" \
        'RELEASE_NOTES="${RELEASE_NOTES/__ARC_INSTALL_SHA256__/$installer_sha256}"' \
        'Installer-digest placeholder survived release-note materialization.'
    do
        require_literal "$RELEASE_WORKFLOW" "$literal" \
            'generated release notes do not materialize the exact bootstrap digest' || return 1
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

    require_literal "$GETTING_STARTED" 'Source-freeze recovery notice (2026-08-31; tag-stable)' \
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
    [ "$rust_lines" -ge 175000 ] || {
        printf 'README 175K+ Rust badge exceeds current measured source lines: %s\n' "$rust_lines"
        return 1
    }
    [ "$rust_tests" -ge 1800 ] || {
        printf 'README 1,800+ Rust-test badge exceeds current defined tests: %s\n' "$rust_tests"
        return 1
    }
    for literal in \
        'more than 175,000 physical lines of Rust' \
        'More than 1,800 Rust test functions' \
        'plus one narrowly' \
        'vendored `wasmer-derive` workspace member'
    do
        require_literal "$README" "$literal" \
            'README source inventory is stale or not reproducible' || return 1
    done
    if grep -Eq '121,900|1,363|34-endpoint|Every line is original|Built solo from scratch, every line' "$README"; then
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

community_support_and_history_copy_is_actionable() {
    for literal in \
        '### Community support answer sheet' \
        'Publishing v0.8.0 alone does not update them.' \
        '[2–3 minute community-node walkthrough]' \
        '## Block-level history and explorer continuity' \
        'The recovery boundary is exactly `H+1`' \
        'no public explorer URL is supported yet.'
    do
        require_literal "$README" "$literal" \
            'README omits a current community support or history-continuity answer' || return 1
    done

    for literal in \
        '### Troubleshooting and permissions' \
        'Do not use' \
        '`chmod -R 777`' \
        '`chown -R`' \
        'Do not mix the two scopes.' \
        'scripts/arc-diagnose.sh'
    do
        require_literal "$HEADLESS" "$literal" \
            'headless guide omits a safe permission or diagnostic instruction' || return 1
    done

    for literal in \
        '#### Download and verify the exact worker model' \
        'exactly 4,081,004,224 bytes' \
        '191239b3e26b2882fb562ffccdd1cf0f65402adb' \
        '08a5566d61d7cb6b420c3e4387a39e0078e1f2fe5f055f3a03887385304d4bfa' \
        "--proto '=https' --proto-redir '=https' --tlsv1.2" \
        'sha256sum -c -' \
        'shasum -a 256 -c -'
    do
        require_literal "$HEADLESS" "$literal" \
            'headless guide omits the exact worker-model acquisition contract' || return 1
    done

    require_literal "$WALKTHROUGH" 'download-and-verify-the-exact-worker-model' \
        'recording walkthrough does not point to the exact pre-staged model procedure' || return 1

    if grep -R -Fq -- '/resolve/main/' \
        "$HEADLESS" "$SESSION_HANDOFF" "$REPO_ROOT/docs/TIER1_ONCHAIN_INFERENCE_PLAN.md"; then
        printf '%s\n' 'operator model-acquisition documentation uses a mutable Hugging Face revision'
        return 1
    fi
    for model_doc in \
        "$HEADLESS" \
        "$SESSION_HANDOFF" \
        "$REPO_ROOT/docs/TIER1_ONCHAIN_INFERENCE_PLAN.md" \
        "$REPO_ROOT/papers/foundations-trustworthy-ai.typ"
    do
        require_literal "$model_doc" '191239b3e26b2882fb562ffccdd1cf0f65402adb' \
            'model-acquisition documentation omits the immutable model revision' || return 1
        require_literal "$model_doc" '08a5566d61d7cb6b420c3e4387a39e0078e1f2fe5f055f3a03887385304d4bfa' \
            'model-acquisition documentation omits the exact model digest' || return 1
    done

    require_literal "$GETTING_STARTED" '### Verify a block, transaction, or reward in the explorer' \
        'desktop guide omits block-level recovery and explorer status' || return 1
    require_literal "$GETTING_STARTED" 'Explicit alternate-source views remain' \
        'desktop guide could silently blend non-canonical history' || return 1
}

factual_candidate_copy_matches_source_and_release_gates() {
    local desktop_spec_files playwright_inventory
    desktop_spec_files="$(find "$REPO_ROOT/desktop/tests" -maxdepth 1 -type f \
        -name '*.spec.ts' | wc -l | tr -d ' ')"
    assert_equals 20 "$desktop_spec_files" \
        'documented Playwright file inventory drifted from the current tree' || return 1
    require_literal "$DESKTOP_README" '211 tests in' \
        'desktop README does not carry the audited Playwright test inventory' || return 1
    require_literal "$DESKTOP_README" '20 files.' \
        'desktop README does not carry the audited Playwright file inventory' || return 1
    if grep -Eq '176 tests|68[[:space:]]+native tests' "$DESKTOP_README"; then
        printf 'desktop README retains a stale hard-coded test inventory\n'
        return 1
    fi
    # The offline release-contract job intentionally does not install npm
    # dependencies. When Playwright is present (full local/desktop CI), bind the
    # human-readable inventory to Playwright's own collector as well.
    if [ -x "$REPO_ROOT/desktop/node_modules/.bin/playwright" ]; then
        playwright_inventory="$(
            cd "$REPO_ROOT/desktop" || exit 1
            CI='' ./node_modules/.bin/playwright test --list 2>/dev/null \
                | sed -n 's/^Total: //p' | tail -n 1
        )"
        assert_equals '211 tests in 20 files' "$playwright_inventory" \
            'desktop README Playwright inventory differs from playwright --list' || return 1
    fi

    require_literal "$REPO_ROOT/genesis.toml" \
        'community_rewards_v1_activation_height = 137146' \
        'canonical genesis no longer matches the documented activation height' || return 1
    require_literal "$CHANGELOG" 'schedules activation at block `137146`' \
        'changelog does not match the checked-in activation schedule' || return 1
    if grep -Fq 'no activation schedule' "$CHANGELOG"; then
        printf 'changelog still denies the checked-in activation schedule\n'
        return 1
    fi

    require_literal "$REPO_ROOT/crates/arc-node/src/rpc.rs" \
        'const COMMUNITY_REWARD_APPROVAL_COLLECTION_READY: bool = true;' \
        'reward approval collection is no longer source-enabled as documented' || return 1
    require_literal "$DEMO_RUNBOOK" \
        'Authenticated approval collection is implemented and tested in this source' \
        'archived demo runbook misstates candidate approval-collection readiness' || return 1
    require_literal "$DEMO_RUNBOOK" 'it is not deployed on the public v2 fleet' \
        'demo runbook confuses source readiness with public deployment' || return 1
    if grep -Fq 'Approval collection is intentionally unavailable in this candidate' \
        "$DEMO_RUNBOOK"; then
        printf 'demo runbook retains the disproven approval-collection claim\n'
        return 1
    fi

    require_literal "$RELEASE_WORKFLOW" "ubuntu_versions: '22.04 24.04 26.04'" \
        'release workflow lost the Linux x86_64 Ubuntu runtime matrix' || return 1
    require_literal "$RELEASE_WORKFLOW" "ubuntu_versions: '24.04 26.04'" \
        'release workflow lost the Linux ARM64 Ubuntu runtime matrix' || return 1
    require_literal "$ROLLOUT" 'Ubuntu 22.04, 24.04, and 26.04 containers' \
        'rollout omits a release-gated Linux x86_64 environment' || return 1
    require_literal "$ROLLOUT" 'Linux ARM64 in clean Ubuntu 24.04 and 26.04' \
        'rollout omits a release-gated Linux ARM64 environment' || return 1

    require_literal "$README" 'In the read-only 2026-08-28 public-v2 snapshot' \
        'README leaves a legacy public-v2 observation undated' || return 1
    require_literal "$GETTING_STARTED" 'That is dated public-v2' \
        'Getting Started presents a legacy snapshot as current state' || return 1
    if grep -Fq "On today's public v2 seeds" "$README" \
        || grep -Fq 'That is the current state of the testnet' "$GETTING_STARTED"; then
        printf 'active documentation retains an unpinned legacy-fleet claim\n'
        return 1
    fi
    require_literal "$STATUS_DOC" \
        '[`docs/COMMUNITY-NODE-WALKTHROUGH.md`](COMMUNITY-NODE-WALKTHROUGH.md)' \
        'archived status does not direct readers to the current gated walkthrough' || return 1
    if grep -Fq '[`docs/DEMO-RUNBOOK.md`](DEMO-RUNBOOK.md) — the current run-of-show' \
        "$STATUS_DOC"; then
        printf 'archived status still labels the public-v2 demo as current\n'
        return 1
    fi

    for literal in \
        'no older-macOS or Windows-version runtime floor is' \
        'built/packaged on macOS 15' \
        'built/packaged on GitHub `windows-latest`'
    do
        require_literal "$README" "$literal" \
            'README overstates the candidate desktop OS compatibility evidence' || return 1
    done
    require_literal "$HEADLESS" \
        'it does not claim an older' \
        'headless guide overstates macOS/Windows version compatibility' || return 1
    require_literal "$GETTING_STARTED" \
        'does not yet establish a Windows 10/11 compatibility floor' \
        'desktop guide overstates Windows version compatibility' || return 1
    if grep -Fq '| **macOS 11+' "$README" \
        || grep -Fq '| **Windows 10/11' "$README" \
        || grep -Fq '| macOS 11+' "$HEADLESS" \
        || grep -Fq '### Windows 10 / 11' "$GETTING_STARTED"; then
        printf 'active documentation retains an untested OS-version support claim\n'
        return 1
    fi

    if grep -Ei 'use at least 16 GB|with at least 16 GB' \
        "$README" "$HEADLESS" "$GETTING_STARTED" \
        "$REPO_ROOT/docs/PRODUCTION-RECOVERY-AUDIT-2026-08-26.md"; then
        printf 'active documentation promotes an unvalidated 16 GB minimum\n'
        return 1
    fi
    for file in \
        "$README" \
        "$HEADLESS" \
        "$GETTING_STARTED" \
        "$REPO_ROOT/docs/PRODUCTION-RECOVERY-AUDIT-2026-08-26.md"
    do
        grep -Eq 'no validated minimum-RAM claim|not established a minimum-RAM figure|does not establish a minimum-RAM' \
            "$file" || {
            printf 'hardware guide does not state the evidence boundary: %s\n' "$file"
            return 1
        }
    done
}

hardened_canary_and_vault_commands_match_current_parsers() {
    local file help option
    help="$(python3 "$CANARY_HELPER" plan --help)" || return 1
    for option in \
        --raw-actions-zip --model --expected-commit --expected-run-id \
        --expected-run-attempt --expected-artifact-id --curl --curl-sha256 \
        --ca-bundle --ca-bundle-sha256
    do
        printf '%s\n' "$help" | grep -Fq -- "$option" || {
            printf 'canary parser omits documented option: %s\n' "$option"
            return 1
        }
        require_literal "$CANARY_RUNBOOK" "$option" \
            'canary runbook omits required parser option' || return 1
    done
    require_literal "$CANARY_RUNBOOK" '/private/etc/ssl/cert.pem' \
        'canary runbook does not use the normalized protected macOS CA path' || return 1
    if grep -Eq -- '--candidate-dir|--expected-artifact-digest|--expected-archive-sha256' \
        "$CANARY_RUNBOOK"; then
        printf 'canary runbook retains an option removed from the hardened parser\n'
        return 1
    fi

    help="$(python3 "$VAULT_HELPER" restore --help)" || return 1
    for option in \
        --raw-actions-zip --pretag-run-id --pretag-run-attempt \
        --pretag-artifact-id --curl --curl-sha256 --ca-bundle \
        --ca-bundle-sha256
    do
        printf '%s\n' "$help" | grep -Fq -- "$option" || {
            printf 'vault restore parser omits documented option: %s\n' "$option"
            return 1
        }
        require_literal "$ROLLOUT" "$option" \
            'validator rollout omits required vault proof option' || return 1
    done
    help="$(python3 "$VAULT_HELPER" install --help)" || return 1
    for option in --ssh-identity --ssh-identity-sha256
    do
        printf '%s\n' "$help" | grep -Fq -- "$option" || {
            printf 'vault install parser omits documented option: %s\n' "$option"
            return 1
        }
        require_literal "$ROLLOUT" "$option" \
            'validator rollout omits explicit SSH identity option' || return 1
    done
    if grep -Eq -- '--use-ssh-agent|--arc-cli-build-metadata|--arc-cli-sha256|--genesis-sha256' \
        "$ROLLOUT"; then
        printf 'validator rollout retains a removed vault/agent option\n'
        return 1
    fi
    require_literal "$VAULT_HELPER" \
        'arc.validator-vault.offline-stop-evidence.v2' \
        'validator vault helper no longer requires the reviewed offline-stop schema' || return 1
    for file in "$ROLLOUT" "$RECOVERY_README" "$PRODUCTION_RECOVERY_AUDIT"; do
        require_literal "$file" \
            'arc.validator-vault.offline-stop-evidence.v2' \
            'operator documentation drifted from the reviewed offline-stop schema' || return 1
        if grep -Fq 'arc.validator-vault.offline-stop-evidence.v1' "$file"; then
            printf 'operator documentation retains retired offline-stop schema v1: %s\n' "$file"
            return 1
        fi
    done
}

recovery_archive_commands_are_exact_and_resumable() {
    local help file literal
    help="$("$ARCHIVE_TOOL" help)" || return 1
    for literal in \
        'export ARC_RECOVERY_PYTHON_PATH=/usr/bin/python3.12' \
        'test ! -L "$ARC_RECOVERY_PYTHON_PATH"' \
        'export ARC_RECOVERY_GH_PATH=' \
        'export ARC_RECOVERY_GH_SHA256=' \
        'export ARC_RECOVERY_GITHUB_LOGIN=FerrumVir' \
        '--validator-install-receipt' '--vault-restore-receipt' \
        '--finalization-intent' '--work-root'
    do
        printf '%s\n' "$help" | grep -Fq -- "$literal" || {
            printf 'archive help omits hardened executable contract: %s\n' "$literal"
            return 1
        }
        require_literal "$RECOVERY_README" "$literal" \
            'recovery README omits hardened executable contract' || return 1
    done
    if grep -Fqx 'export ARC_RECOVERY_PYTHON_PATH=/usr/bin/python3' \
        "$RECOVERY_README" "$ARCHIVE_TOOL" "$ROLLOUT"; then
        printf 'operator docs still export the symlink-prone unversioned Python path\n'
        return 1
    fi
    for literal in \
        '--drive-archive-seal-prefreeze' \
        '--github-gist-write-canary' \
        'GET /gists/{id}/{revision}' \
        'github-gist-write-canary.json' \
        '--rollback-journal /secure/operator/rollback-final-rollout' \
        '--reward-evidence-output /secure/operator/recovery-v3.reward-evidence.json'
    do
        require_literal "$RECOVERY_README" "$literal" \
            'recovery README omits final archive/cutover input' || return 1
    done
    for literal in \
        '--drive-archive-seal-prefreeze' '--github-gist-write-canary' '--rollback-journal' \
        '--reward-evidence-output' 'ARC_RECOVERY_GH_SHA256' \
        'continuity_safety_margin=128' 'global_absence_claimed=false'
    do
        require_literal "$ROLLOUT" "$literal" \
            'top-level rollout omits required final archive/cutover contract' || return 1
    done
    for file in "$RECOVERY_README" "$ROLLOUT"; do
        for literal in \
            '1.24.0-2ubuntu7.17' \
            '1f16b72bea2f44e5d04fe6cf9e3e4b0dec53a82c50c7c1533c302a8ecaeccacf' \
            'arc-rpc-filter' 'auth_request' 'admin off' '`forward_auth`' \
            'held' 'stopped' 'disabled'
        do
            require_literal "$file" "$literal" \
                'operator docs omit the exact nginx/Caddy security boundary' || return 1
        done
    done
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
run_test 'community support, permissions, and block-history answers stay actionable' community_support_and_history_copy_is_actionable
run_test 'candidate facts remain source-bound and dated without unsupported floors' factual_candidate_copy_matches_source_and_release_gates
run_test 'hardened canary and vault runbooks match their current command parsers' hardened_canary_and_vault_commands_match_current_parsers
run_test 'recovery archive commands pin tools, external intent, and exact rollout journals' recovery_archive_commands_are_exact_and_resumable

finish_tests
