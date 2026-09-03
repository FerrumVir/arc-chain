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
SCRIPTS_README="$REPO_ROOT/scripts/README.md"
ROLLOUT="$REPO_ROOT/docs/VALIDATOR-FLEET-ROLLOUT.md"
RECOVERY_README="$REPO_ROOT/scripts/recovery/README.md"
ARCHIVE_TOOL="$REPO_ROOT/scripts/recovery/archive-fleet-to-drive.sh"
DRIVE_PREFREEZE_GATE="$REPO_ROOT/scripts/recovery/verify-drive-prefreeze.sh"
DRIVE_IDENTITY_HELPER="$REPO_ROOT/scripts/recovery/drive-account-identity.py"
DRIVE_PREFREEZE_TEST="$REPO_ROOT/tests/release/drive_prefreeze_gate_test.sh"
CANARY_RUNBOOK="$REPO_ROOT/docs/MACOS-PRETAG-COMMUNITY-CANARY.md"
CANARY_HELPER="$REPO_ROOT/scripts/release/macos-community-canary.py"
VAULT_HELPER="$REPO_ROOT/scripts/release/restore-validator-vault.py"
CUTOVER_HANDOFF_HELPER="$REPO_ROOT/scripts/release/create-cutover-handoff-commit.py"
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

first_literal_line() {
    local file="$1" literal="$2"
    grep -nF -- "$literal" "$file" | head -n 1 | cut -d: -f1
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
    for file in "$README" "$HEADLESS" "$WALKTHROUGH" "$SCRIPTS_README"; do
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
    require_literal "$WALKTHROUGH" 'mined_failed|receipt_unavailable)' \
        'walkthrough does not stop on the exact terminal reward failure states' || return 1
    require_literal "$WALKTHROUGH" '.submitted==true and .included==true' \
        'walkthrough does not require submitted and included receipt evidence' || return 1
    require_literal "$WALKTHROUGH" '.receipt_url==( "/community/reward_receipt/" + .tx_hash )' \
        'walkthrough does not bind receipt_url to the exact transaction' || return 1

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
    rust_lines="$(cd "$REPO_ROOT" && git ls-files -z -- '*.rs' ':(exclude)vendor/**' | \
        xargs -0 wc -l | awk 'END { print $1 }')"
    rust_tests="$(cd "$REPO_ROOT" && git ls-files -z -- '*.rs' ':(exclude)vendor/**' | \
        xargs -0 grep -Eh '^[[:space:]]*#\[(tokio::)?test' | wc -l | tr -d ' ')"
    [ "$rust_lines" -ge 196000 ] || {
        printf 'README 196K+ Rust badge exceeds current measured non-vendored source lines: %s\n' "$rust_lines"
        return 1
    }
    [ "$rust_tests" -ge 1900 ] || {
        printf 'README 1,900+ Rust-test badge exceeds current non-vendored defined tests: %s\n' "$rust_tests"
        return 1
    }
    for literal in \
        'more than 196,000 physical lines of checked-in,' \
        'More than 1,900 Rust test functions' \
        'non-vendored Rust across ARC' \
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
    require_literal "$WALKTHROUGH" 'ARC_SERVICE_SCOPE=' \
        'walkthrough hard-codes the wrong service manager for adopted v0.7 nodes' || return 1
    require_literal "$WALKTHROUGH" 'system-user) sudo systemctl' \
        'walkthrough omits the migrated Linux global-service scope' || return 1
    require_literal "$README" 'bind RPC to `127.0.0.1` only' \
        'README does not disclose the managed loopback-only RPC default' || return 1
    require_literal "$HEADLESS" 'bound only to `127.0.0.1`' \
        'headless guide does not disclose the loopback-only RPC default' || return 1
    require_literal "$HEADLESS" 'restores that complete snapshot' \
        'headless guide still describes a binary-only rollback' || return 1
    require_literal "$README" 'rollback is not a migration' \
        'README could present install rollback as persisted-state migration' || return 1
    require_literal "$README" 'same-generation single-writer guard' \
        'README could imply the v0.8 lock fences released v0.7 processes' || return 1
    require_literal "$HEADLESS" 'Released v0.7 binaries acquire neither lock' \
        'headless guide omits the cross-generation lock boundary' || return 1
    require_literal "$ROLLOUT" 'released v0.7 validators do not acquire it' \
        'validator runbook could treat the v0.8 lock as legacy quiescence' || return 1
    require_literal "$DESKTOP_FIRST_RUN" 'released v0.7 binaries do not' \
        'desktop first-run guide omits the cross-generation lock boundary' || return 1
}

community_support_and_history_copy_is_actionable() {
    for literal in \
        '### Community support answer sheet' \
        'Publishing v0.8.0 alone does not update them.' \
        'intentionally not promoted to GitHub' \
        'unsigned legacy updater is not allowed' \
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
    require_literal "$DESKTOP_README" '225 tests in' \
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
        assert_equals '225 tests in 20 files' "$playwright_inventory" \
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
    for literal in \
        'FileVault-protected directory' \
        'dedicated AES-256 encrypted APFS image' \
        'Encryption = AES-256' \
        'Properties.Encrypted = 1' \
        'An unencrypted host must never stage the artifact outside that mounted volume'
    do
        require_literal "$ROLLOUT" "$literal" \
            'signing-key backup runbook omits its encrypted-at-rest staging boundary' \
            || return 1
    done
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
    for option in \
        --legacy-maintenance-evidence-bundle \
        --legacy-maintenance-evidence-bundle-sidecar \
        --legacy-maintenance-evidence-bundle-sha256 \
        --legacy-maintenance-boundary \
        --legacy-maintenance-boundary-sidecar \
        --legacy-maintenance-boundary-sha256 \
        --ssh-identity --ssh-identity-sha256
    do
        printf '%s\n' "$help" | grep -Fq -- "$option" || {
            printf 'vault install parser omits documented option: %s\n' "$option"
            return 1
        }
        require_literal "$ROLLOUT" "$option" \
            'validator rollout omits required vault install option' || return 1
        require_literal "$RECOVERY_README" "$option" \
            'recovery README omits required vault install option' || return 1
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

    for literal in \
        '--sample-legacy-public-height-output "$legacy_public_height_receipt"' \
        'scripts/release/select-pretag-artifacts.py' \
        'scripts/release/verify-pretag-run-and-artifacts.sh' \
        'scripts/release/materialize-pretag-artifacts.py' \
        'arc.recovery.pretag-artifact-input-set.v1' \
        '/etc/ssl/certs/ca-certificates.crt' \
        '527fbf917c39189a1e3b31d34fa955601680b2d5c8055d2a87b8b9588dec7bb9' \
        'b7105518e3ed1c0761f232e44fc09345535533c9cb0abf0e12809416c7ac64d9' \
        'offline_signer "$signing_binary" recovery sign' \
        'offline_signer "$arc_node_linux" recovery verify' \
        '/proc/self/fd/9 --config /proc/self/fd/8' \
        '--expected-prearchive-rollout-sha256' \
        '--output "$finalizer_attempt"'
    do
        require_literal "$RECOVERY_README" "$literal" \
            'recovery README omits an exact protected materialization/finalization command' \
            || return 1
    done
    python3 - "$RECOVERY_README" <<'PY' || return 1
import pathlib
import re
import sys

text = pathlib.Path(sys.argv[1]).read_text(encoding="utf-8")
start = text.index("scripts/recovery/archive-fleet-to-drive.sh prepare-writers")
end = text.index("Install the restored keys only after that successful capture.", start)
capture = text[start:end]
calls = [
    match.start()
    for match in re.finditer(
        re.escape("scripts/recovery/archive-fleet-to-drive.sh capture \\"), capture
    )
]
if len(calls) != 2:
    raise SystemExit("recovery README must contain exactly one capture plan and one capture execute call")
nonce = capture.index('legacy_height_attempt_nonce="$(')
receipt = capture.index('legacy_public_height_receipt="/secure/operator/legacy-public-height.')
authorization = capture.index('ARC_RECOVERY_FREEZE_GO="FREEZE $freeze_sha256 CAPTURE $capture_id"')
execute = capture.index("    --execute", calls[1])
sealed_hash = capture.index('legacy_public_height_sha256="$(arc_sha256 "$legacy_public_height_receipt")"')
if not nonce < receipt < calls[0] < authorization < calls[1] < execute < sealed_hash:
    raise SystemExit("late public-height receipt plan/execute/hash order differs")
flag = '--sample-legacy-public-height-output "$legacy_public_height_receipt"'
if capture.count(flag) != 2 or flag not in capture[calls[0]:authorization] or flag not in capture[calls[1]:execute]:
    raise SystemExit("capture plan and execute must share the exact late-sample output path")
if "scripts/recovery/legacy-public-height.py sample" in capture:
    raise SystemExit("recovery README samples public height before capture prerequisites")
for statement in (
    "do not sample it here",
    "execute phase samples only after the slow inspector, Drive, and live-",
    "observation prerequisites, immediately before the authenticated cross-proof",
):
    if statement not in capture:
        raise SystemExit(f"recovery README omits late-sampling timing contract: {statement}")
PY
    for literal in \
        '/secure/operator/arc-offline-stop-evidence.json' \
        '/secure/operator/arc-validator-maintenance-ed25519' \
        '/secure/operator/VALIDATOR-KEY-INSTALL-RECEIPT.json' \
        '/secure/operator/arc-recovery-final.lock.json'
    do
        require_literal "$RECOVERY_README" "$literal" \
            'recovery README omits a canonical operator path' || return 1
        require_literal "$ROLLOUT" "$literal" \
            'validator rollout omits a canonical operator path' || return 1
    done
    if grep -Eq '/secure/operator/(offline-stop-evidence\.json|arc-validator-key-install\.json|id_arc_recovery_ed25519|arc-recovery\.final(\.lock)?\.json)' \
        "$RECOVERY_README" "$ROLLOUT"; then
        printf 'operator runbooks retain a contradictory pre-normalization path\n'
        return 1
    fi
}

operator_recovery_commands_are_linux_pinned_and_ordered() {
    local file literal pair count line previous audit_block handoff_help handoff_section

    (
        local procedure_file rollout_file
        procedure_file="$(mktemp)" || exit 1
        rollout_file="$(mktemp)" || exit 1
        trap 'rm -f -- "$procedure_file" "$rollout_file"' EXIT
        python3 - "$RECOVERY_README" "$procedure_file" <<'PY' || exit 1
import pathlib
import re
import sys

source = pathlib.Path(sys.argv[1]).read_text(encoding="utf-8")
begin = "<!-- BEGIN EXECUTABLE PRODUCTION RECOVERY PROCEDURE -->"
end = "<!-- END EXECUTABLE PRODUCTION RECOVERY PROCEDURE -->"
if source.count(begin) != 1 or source.count(end) != 1:
    raise SystemExit("executable production procedure markers are absent or duplicated")
region = source.split(begin, 1)[1].split(end, 1)[0]
blocks = []
inside = False
current = []
for line in region.splitlines():
    if line == "```bash":
        if inside:
            raise SystemExit("nested bash fence in executable production procedure")
        inside = True
        current = []
    elif line == "```" and inside:
        inside = False
        blocks.append("\n".join(current) + "\n")
    elif inside:
        current.append(line)
if inside or not blocks:
    raise SystemExit("unterminated or empty executable production procedure")
embedded_python = 0
for block_index, block in enumerate(blocks):
    lines = block.splitlines()
    cursor = 0
    while cursor < len(lines):
        if re.search(r"<<'PY'\s*$", lines[cursor]):
            try:
                end_cursor = lines.index("PY", cursor + 1)
            except ValueError:
                raise SystemExit(f"unclosed embedded Python in bash block {block_index}") from None
            compile(
                "\n".join(lines[cursor + 1 : end_cursor]) + "\n",
                f"executable-runbook-block-{block_index}-python-{cursor}",
                "exec",
            )
            embedded_python += 1
            cursor = end_cursor
        cursor += 1
if embedded_python != 6:
    raise SystemExit(f"expected six compiled embedded Python programs, got {embedded_python}")
pathlib.Path(sys.argv[2]).write_text(
    "#!/usr/bin/env bash\n" + "\n".join(blocks), encoding="utf-8"
)

# ShellCheck catches variables that are never assigned, but it deliberately
# performs flow-insensitive assignment discovery.  The production procedure is
# run with `set -u`, so independently reject a parameter expansion that appears
# lexically before its definition.  Ignore quoted heredoc bodies and single-
# quoted jq/Python programs because those dollars are not shell expansions.
combined = "\n".join(blocks)
if not combined.startswith("set -Eeuo pipefail\n"):
    raise SystemExit("executable production procedure does not enable nounset first")
signer_start_marker = "offline_signer_python='"
signer_end_marker = "'\noffline_signer()"
if combined.count(signer_start_marker) != 1 or combined.count(signer_end_marker) != 1:
    raise SystemExit("offline signer Python assignment is absent or duplicated")
signer_source = combined.split(signer_start_marker, 1)[1].split(signer_end_marker, 1)[0]
compile(signer_source, "executable-runbook-offline-signer", "exec")

def undefined_before_definition(program):
    defined = {
        "IFS", "OLDPWD", "OPTARG", "OPTIND", "PPID", "PWD", "RANDOM",
        "SECONDS", "UID", "EUID",
    }
    single_quoted = False
    heredoc = None
    undefined = []
    for line_number, original in enumerate(program.splitlines(), 1):
        if heredoc is not None:
            if original == heredoc:
                heredoc = None
            continue
        if original.lstrip().startswith("#"):
            continue
        visible = []
        escaped = False
        for character in original:
            if escaped:
                visible.append(" ")
                escaped = False
            elif character == "\\" and not single_quoted:
                visible.append(" ")
                escaped = True
            elif character == "'":
                single_quoted = not single_quoted
                visible.append(" ")
            elif single_quoted:
                visible.append(" ")
            else:
                visible.append(character)
        shell = "".join(visible)
        references = []
        for match in re.finditer(
            r"\$(?:\{[#!]?(?P<braced>[A-Za-z_][A-Za-z0-9_]*)[^}]*\}|(?P<plain>[A-Za-z_][A-Za-z0-9_]*))",
            shell,
        ):
            references.append(match.group("braced") or match.group("plain"))
        for name in references:
            if name not in defined:
                undefined.append((line_number, name, original.strip()))

        assignment = re.match(
            r"\s*(?:(?:export|readonly)\s+)?([A-Za-z_][A-Za-z0-9_]*)=", shell
        )
        if assignment:
            defined.add(assignment.group(1))
        loop = re.search(r"\bfor\s+([A-Za-z_][A-Za-z0-9_]*)\s+in\b", shell)
        if loop:
            defined.add(loop.group(1))
        read = re.search(r"\bread(?:\s+-[A-Za-z]+)*\s+([^;<>&|]+)", shell)
        if read:
            for token in read.group(1).split():
                if re.fullmatch(r"[A-Za-z_][A-Za-z0-9_]*", token):
                    defined.add(token)
        local = re.search(r"\blocal\s+(.+)", shell)
        if local:
            for token in local.group(1).split():
                name = token.split("=", 1)[0]
                if re.fullmatch(r"[A-Za-z_][A-Za-z0-9_]*", name):
                    defined.add(name)
        marker = re.search(
            r"<<-?\s*(['\"]?)([A-Za-z_][A-Za-z0-9_]*)\1", original
        )
        if marker:
            heredoc = marker.group(2)
    if single_quoted or heredoc is not None:
        raise SystemExit("static nounset audit encountered an unterminated quote or heredoc")
    return undefined

undefined = undefined_before_definition(combined)
if undefined:
    details = "; ".join(
        f"line {line}: ${name} in {source!r}" for line, name, source in undefined
    )
    raise SystemExit(f"shell variable used before definition under set -u: {details}")

# Prove the order checker itself fails when a real required assignment is
# removed; this prevents a flow-insensitive regression from making the test
# green merely because the variable is assigned somewhere later in the file.
assignment = "protected_main_sha='<exact 40-character protected-main SHA after merge>'"
if combined.count(assignment) != 1:
    raise SystemExit("protected-main assignment is absent or duplicated")
mutated = combined.replace(assignment, "# mutation: assignment removed", 1)
if not any(name == "protected_main_sha" for _line, name, _source in undefined_before_definition(mutated)):
    raise SystemExit("definition-before-use audit false-passed its protected-main mutation")

def one(marker):
    matches = [(index, block) for index, block in enumerate(blocks) if marker in block]
    if len(matches) != 1:
        raise SystemExit(f"expected one cohesive bash block for {marker!r}, got {len(matches)}")
    return matches[0]

def require(block, marker, *tokens):
    start = block.find(marker)
    if start < 0:
        raise SystemExit(f"command marker is absent: {marker}")
    command = block[start:]
    for token in tokens:
        if token not in command:
            raise SystemExit(f"{marker} command omits cohesive token: {token}")

def require_order(block, *tokens):
    positions = [block.find(token) for token in tokens]
    if any(position < 0 for position in positions) or positions != sorted(positions):
        raise SystemExit(f"cohesive command order differs: {tokens!r}")

initial = one("scripts/release/materialize-pretag-artifacts.py")
restore = one("scripts/release/restore-validator-vault.py restore")
capture = one("scripts/recovery/archive-fleet-to-drive.sh prepare-writers")
install = one("scripts/release/restore-validator-vault.py install")
export = one('"$arc_node_linux" recovery export')
sign = one('offline_signer "$signing_binary" recovery sign')
verify = one('offline_signer "$arc_node_linux" recovery verify')
prearchive = one("scripts/recovery/build-production-manifest.py prearchive")
archive = one("scripts/recovery/archive-fleet-to-drive.sh seal \\")
downloads = one('\ndownload_root="$(\n')
finalize = one("scripts/recovery/build-production-manifest.py finalize")
frontend = one("scripts/recovery/recovery_rollout.py frontend-config")
ordered = [initial[0], restore[0], capture[0], install[0], export[0], sign[0],
           verify[0], prearchive[0], archive[0], downloads[0], finalize[0], frontend[0]]
if ordered != sorted(ordered) or len(set(ordered)) != len(ordered):
    raise SystemExit("executable production bash blocks are absent or out of mandatory order")

require(
    initial[1], "arc_install_or_reuse_exact() {",
    "arc_git() {", "GIT_CONFIG_NOSYSTEM=1", "arc_git clone", "checkout --detach", "diff-index --quiet HEAD --",
    "status --porcelain=v1 --untracked-files=all", "validator-vault-rewrap.yml",
    "expected exactly one live exact-run rewrap artifact", "os.O_EXCL | os.O_NOFOLLOW",
    'test "$artifact_size" -le 4294967296',
    '/usr/bin/head --bytes="$((artifact_size + 1))"',
    "canonical_receipt", 'payloads["REWRAP-RECEIPT.json"] != canonical_receipt',
    'create(receipt_output, payloads["REWRAP-RECEIPT.json"])', "cms_sha256=\"$(",
    "os.O_RDONLY | os.O_NOFOLLOW",
    "canonical output exists with different bytes; preserve both and stop",
)
require(
    restore[1], "scripts/release/restore-validator-vault.py restore",
    '--cms "$cms_path"', '--expected-cms-sha256 "$cms_sha256"',
    '--rewrap-receipt "$rewrap_receipt"', '--source-main-sha "$protected_main_sha"',
    '--raw-actions-zip "$pretag_raw_root/headless-linux-x86_64/actions.zip"',
    '--pretag-run-id "$pretag_run_id"', '--pretag-run-attempt "$pretag_run_attempt"',
    '--pretag-artifact-id "$pretag_linux_x86_64_artifact_id"',
    '--restore-certificate /secure/operator/restore.cert.pem',
    '--restore-private-key /secure/operator/restore.key.pem',
    '--output-dir /secure/operator/arc-v0.8-validator-restore',
)
require(
    install[1], "scripts/release/restore-validator-vault.py install",
    '--legacy-maintenance-evidence-bundle "$legacy_maintenance_evidence_bundle"',
    '--legacy-maintenance-evidence-bundle-sidecar "$legacy_maintenance_evidence_bundle_sidecar"',
    '--legacy-maintenance-evidence-bundle-sha256 "$legacy_maintenance_evidence_bundle_sha256"',
    '--legacy-maintenance-boundary "$legacy_maintenance_boundary"',
    '--legacy-maintenance-boundary-sidecar "$legacy_maintenance_boundary_sidecar"',
    '--legacy-maintenance-boundary-sha256 "$legacy_maintenance_boundary_sha256"',
    '--offline-stop-evidence "$offline_stop_evidence"',
    '--offline-stop-evidence-sidecar "$offline_stop_evidence_sidecar"',
    '--offline-stop-evidence-sha256 "$offline_stop_evidence_sha256"',
    '--known-hosts "$known_hosts"', '--known-hosts-sha256 "$known_hosts_sha256"',
    '--ssh-identity "$ssh_identity"', '--ssh-identity-sha256 "$ssh_identity_sha256"',
    '--receipt-output /secure/operator/VALIDATOR-KEY-INSTALL-RECEIPT.json',
)
require(
    export[1], "reference_pair=/secure/operator/reference-pair",
    '"state.snapshot.lz4"', "1_160_246",
    "ecb4e39d45e6711cffcd78183851587e4deb37ad63163f541ef6c1f821a4ce47",
    '"state.wal"', "83_385_625",
    "3820e112af1684567f0336abe73ae9aafc4228d0e02a5fccb1ff32f64dfed44c",
    '"latest.json"',
    "0c9bcafd99375de7e3167c271350279c4d267dd9cf91de37aa830a2b817f80af",
    '"snapshot-info.json"',
    "98f327fb9c4405cd0f6e7c31052d571a024738df5bf6987ad78d9b1ba5856b49",
    'read_locked(root_fd, "SHA256SUMS", 324)',
    "reference_source_consensus_round=9774808", "reference_block_height=137145",
    "8fac459a8de0164b28e30d3f67adf6aefe01054912a3d1ae5c53765e59935a90",
    "d300a2bb8dbe7f6da9596b550f31efd36eb842a1861e294c25740a19c8e3bc6d",
    '"$arc_node_linux" recovery export',
    'candidate_attempt_root="$(', '--output "$candidate_attempt"',
    'arc_install_or_reuse_exact "$candidate_attempt" "$candidate_checkpoint"',
    'candidate_checkpoint_sha256="$(arc_sha256 "$candidate_checkpoint")"',
    '--source-consensus-round "$reference_source_consensus_round"',
)
require_order(
    export[1],
    'candidate_attempt_root="$(',
    '"$arc_node_linux" recovery export',
    'chmod 0400 "$candidate_attempt"',
    'arc_install_or_reuse_exact "$candidate_attempt" "$candidate_checkpoint"',
)
require(
    sign[1], "unset GH_TOKEN", 'signing_unshare=/usr/bin/unshare',
    '"$signing_unshare" --net --', 'interfaces != {"lo"}',
    'with os.scandir("/proc/self/fd") as entries:',
    'os.close(descriptor)', 'if error.errno != errno.EBADF:',
    'offline_signer "$signing_binary" recovery inspect',
    'offline_signer "$signing_binary" recovery sign',
    'signing_attempt_root="$(',
    'outgoing_checkpoint="$signing_attempt_root/candidate.signed-$((index + 1)).arcchkpt"',
    'arc_install_or_reuse_exact "$incoming_checkpoint" "$recovery_checkpoint"',
)
require_order(
    sign[1],
    'offline_signer "$signing_binary" recovery inspect',
    'offline_signer "$signing_binary" recovery sign',
    'arc_install_or_reuse_exact "$incoming_checkpoint" "$recovery_checkpoint"',
)
require(
    prearchive[1], "prearchive_output=/secure/operator/arc-recovery.prearchive.json",
    'prearchive_result="$(',
    '"$ARC_RECOVERY_PYTHON_PATH" -I scripts/recovery/build-production-manifest.py prearchive',
    'production_stage_root=/secure/operator/production-input-stage-v0.8.0',
    '--stage-root "$production_stage_root"',
    '--source-snapshot "$reference_pair/state.snapshot.lz4"',
    '--source-wal "$reference_pair/state.wal"',
    'prearchive_existing=0', 'elif [ "$prearchive_existing" = 3 ]; then',
    'builder.load_private_rollout(manifest_path)',
    'with builder.stable_artifact_identity_window(value):',
    'partial prearchive output/stage set exists',
    'locked_sha256="$(', '"${prearchive_output##*/}"',
)
if prearchive[1].count('locked_sha256="$(') != 2:
    raise SystemExit("prearchive new/reuse branches do not both define locked_sha256")
if archive[1].count(
    '--validator-public-keys /secure/operator/production-input-stage-v0.8.0/validator-public-keys.json'
) != 2:
    raise SystemExit("archive plan/execute do not both use the staged validator-public-key bytes")
if archive[1].count(
    '--validator-install-receipt /secure/operator/production-input-stage-v0.8.0/private/VALIDATOR-KEY-INSTALL-RECEIPT.json'
) != 2 or archive[1].count(
    '--vault-restore-receipt /secure/operator/production-input-stage-v0.8.0/private/VALIDATOR-VAULT-RESTORE-RECEIPT.json'
) != 2:
    raise SystemExit("archive plan/execute do not both use the staged private receipt bytes")
require(
    downloads[1], 'download_root="$(',
    '/usr/bin/mktemp -d "/secure/operator/downloaded.$capture_id.XXXXXXXX"',
    'set -o noclobber', '--count=16777217',
)
require(
    finalize[1], 'final_manifest=/secure/operator/arc-recovery-final.lock.json',
    '"$ARC_RECOVERY_PYTHON_PATH" -I scripts/recovery/build-production-manifest.py finalize',
    '--complete "$download_root/COMPLETE.json"',
    '--archive-manifest "$download_root/ARCHIVE-MANIFEST.json"',
    '--archive-manifest-sidecar "$download_root/ARCHIVE-MANIFEST.json.sha256"',
    '--sha256sums "$download_root/SHA256SUMS"',
    '--drive-archive-seal-prefreeze "$download_root/drive-archive-seal-prefreeze.json"',
    '--drive-archive-seal-attempt "$download_root/drive-archive-seal-attempt.json"',
    '--github-gist-write-canary "$download_root/github-gist-write-canary.json"',
    '--output "$finalizer_attempt"',
    'finalizer_attempt_root="$(',
    'arc_install_or_reuse_exact "$finalizer_attempt_sidecar" "$final_manifest_sidecar"',
    'arc_install_or_reuse_exact "$finalizer_attempt" "$final_manifest"',
    'final_rollout_sha256="$(' , 'legacy_wal_policy="$(' ,
    '"$ARC_RECOVERY_PYTHON_PATH" -I scripts/recovery/recovery_rollout.py run', '--execute',
    '--reward-evidence-output "$reward_evidence"',
)
if finalize[1].count(
    'arc_install_or_reuse_exact "$finalizer_attempt_sidecar" "$final_manifest_sidecar"'
) != 1 or finalize[1].count(
    'arc_install_or_reuse_exact "$finalizer_attempt" "$final_manifest"'
) != 1:
    raise SystemExit("finalizer attempt outputs are not each installed exactly once")
rollout_calls = [
    match.start()
    for match in re.finditer(
        re.escape('"$ARC_RECOVERY_PYTHON_PATH" -I scripts/recovery/recovery_rollout.py run'),
        finalize[1],
    )
]
if len(rollout_calls) != 2:
    raise SystemExit("final rollout block must contain exactly one plan and one execute call")
require_order(
    finalize[1],
    '"$ARC_RECOVERY_PYTHON_PATH" -I scripts/recovery/build-production-manifest.py finalize',
    'final_rollout_sha256="$(',
    'arc_install_or_reuse_exact "$finalizer_attempt_sidecar" "$final_manifest_sidecar"',
    'arc_install_or_reuse_exact "$finalizer_attempt" "$final_manifest"',
    'legacy_wal_policy="$(',
    '"$ARC_RECOVERY_PYTHON_PATH" -I scripts/recovery/recovery_rollout.py run',
    'ARC_RECOVERY_GO="GO $final_rollout_sha256',
)
if not rollout_calls[0] < finalize[1].find('ARC_RECOVERY_GO="GO $final_rollout_sha256') < rollout_calls[1]:
    raise SystemExit("final rollout execute authorization is not between plan and execute calls")
require(
    frontend[1], '"$ARC_RECOVERY_PYTHON_PATH" -I scripts/recovery/recovery_rollout.py frontend-config',
    '--manifest "$final_manifest"', '--reward-evidence "$reward_evidence"',
    '--output "$frontend_config"', 'arc-network.recovered.json.sha256',
)
if frontend[1].count(
    '"$ARC_RECOVERY_PYTHON_PATH" -I scripts/recovery/recovery_rollout.py frontend-config'
) != 1:
    raise SystemExit("frontend config generator must execute exactly once")
PY
        python3 - "$ROLLOUT" "$rollout_file" <<'PY' || exit 1
import pathlib
import sys

blocks = []
current = []
inside = False
for line in pathlib.Path(sys.argv[1]).read_text(encoding="utf-8").splitlines():
    if line == "```bash":
        if inside:
            raise SystemExit("nested bash fence in validator rollout")
        inside = True
        current = []
    elif line == "```" and inside:
        inside = False
        blocks.append("\n".join(current) + "\n")
    elif inside:
        current.append(line)
if inside or not blocks:
    raise SystemExit("unterminated or absent validator-rollout bash fences")
pathlib.Path(sys.argv[2]).write_text(
    "#!/usr/bin/env bash\n" + "\n".join(blocks), encoding="utf-8"
)
PY
        bash -n "$procedure_file" "$rollout_file" || exit 1
        shellcheck -S warning --exclude=SC2016,SC2034,SC2155 \
            "$procedure_file" "$rollout_file" || exit 1
    ) || return 1

    if grep -Fq -- 'config redacted' "$DRIVE_PREFREEZE_GATE"; then
        printf 'production Drive gate still reads the client identity from redacted output\n'
        return 1
    fi
    for literal in \
        '"$RCLONE_BIN" config show "$REMOTE_NAME"' \
        "grep -Eq '^[0-9a-f]{64} [0-9a-f]{64} [0-9a-f]{64}$'" \
        'POST_CLIENT_SHA="${POST_IDENTITY%% *}"' \
        'effective OAuth client changed during Drive prefreeze execution'
    do
        require_literal "$DRIVE_PREFREEZE_GATE" "$literal" \
            'Drive gate no longer binds one three-hash in-memory identity stream' || return 1
    done
    require_literal "$DRIVE_IDENTITY_HELPER" \
        'print(client_hash, account_hash, permission_hash)' \
        'Drive identity helper no longer emits exactly the three public hashes' || return 1
    for literal in \
        'Real rclone v1.75.0 redacts both OAuth client fields' \
        'FAKE_REDACTED_CLIENT=true' \
        'FAKE_REDACTED_CLIENT_SECRET=true' \
        'FAKE_CLIENT_SWITCH=true' \
        'config redacted capability-drive'
    do
        require_literal "$DRIVE_PREFREEZE_TEST" "$literal" \
            'Drive prefreeze regression test omits real redaction or client drift' || return 1
    done

    for file in "$RECOVERY_README" "$ROLLOUT"; do
        for literal in \
            'export PATH=/secure/operator/tools:/usr/bin:/bin' \
            '/etc/ssl/certs/ca-certificates.crt' \
            'ecd9dc38bc3efb7dbd6431f57e29d2f8d6a0f0d211e1464b3fef2cbfe266fcd2' \
            '74b4ce8f74b377f18ef1b3df7279c26cb3cd14c49e39ab1498575b209dc3f70f' \
            '/secure/operator/tools/openssl-3.0.13' \
            '724acbe911513d13f52bae0b8969b20336cd8618fc67898a6bf7847bf1a270ad' \
            '/secure/operator/tools/libssl.so.3' \
            '0c0f298a5b4b44526d20a07d126a55bf44b85eaab053b2b0118e5d806d28ea13' \
            '/secure/operator/tools/libcrypto.so.3' \
            'd6fc1bc9de29c55fc905f77edba1ccc7c7a50b32bd2bb9086b0d0b00104eafc4' \
            '/secure/operator/tools/ssh' \
            '47adf415134df7eff017e9557634696ba6b2a09f5a3bb1436d91d99b8a1cd5a6' \
            '/secure/operator/tools/scp' \
            '92608e03bd81bf6cd96697ce3379fdf6a4c9bdba6a699f16bcc80cf0f49ce144' \
            '/usr/bin/python3.12' \
            '1643dacd9feaedc58f3cc581e4d22577dfe25c09b10282936186ccf0f2e61118' \
            '.artifacts["linux-x86_64"].headless.id' \
            '/secure/operator/arc-v0.8-validator-restore/validator-public-keys.json'
        do
            require_literal "$file" "$literal" \
                'operator runbook omits an exact Ubuntu path, hash, or materialized input' \
                || return 1
        done
        if grep -Eq '/private/etc/ssl/cert[.]pem|/opt/homebrew/' "$file"; then
            printf 'Ubuntu operator runbook retains a macOS proof/runtime path: %s\n' "$file"
            return 1
        fi
    done

    for literal in \
        '/secure/operator/tools/gh' \
        'c1be595a7357120e28886922c050fed34ad347c36adf37370ad91d4972a416d5' \
        'f3f9aff817f9766029e50adf9a7963c169e475b8f10c7927823568a0d9443db7' \
        '97c826f7e1a3940f6d18095ccdb0eaeebb5d66ec16fe60b9c5c47690e707485d' \
        '9a7b57700dc7acf0faeca152fc341f237704e81965b5a9656fe8ccee4931444a' \
        '73c7bd17ff0e6e52331a5adf7574e492f137ef52f9b288908413901f33c723b1' \
        '29a77804fd021a47d43afaf1c51c2a877c66ff56699e1d3173be6d57536b8e3b' \
        '700000000000' \
        '50000000000-byte (50 GB) safety margin' \
        '750000000000-byte (750 GB decimal)' \
        'https://developers.google.com/workspace/drive/api/guides/limits' \
        'I ATTEST 700000000000 BYTES REMAIN AND ARC IS THE ONLY DRIVE UPLOAD WRITER THIS QUOTA WINDOW' \
        'a23c8863860669003dc4660039fe642f5795c8c2195898ebc5d01afa1ac3d11c' \
        '3b0701113d8982d71c8cc74e5a1949f03c6f71da804cf4f3507315afbf07042c' \
        '27421348ac188f7381634ce1d521fe9fe774c75cab0d0d2086a052c9bac2da4b' \
        '74b4ce8f74b377f18ef1b3df7279c26cb3cd14c49e39ab1498575b209dc3f70f' \
        'systemctl mask --runtime --now' \
        '3.12.3-1ubuntu0.15' \
        '1:9.6p1-3ubuntu13.18' \
        '8.5.0-2ubuntu10.13' \
        '20260601~24.04.1' \
        '2.39.3-9ubuntu6.6' \
        'checkout --detach "$protected_main_sha"' \
        'status --porcelain=v1 --untracked-files=all' \
        '.github/workflows/validator-vault-rewrap.yml' \
        'bdb2dd477fe10e06e63123d6080f321fce4a251479a5af8a59ae2b47814ed7e9' \
        '6707f8b1dbc1f2d37d9a873a7e3d2c870d2b46db36f15a6df5293547680bfd43' \
        '/secure/operator/pretag-materialized-v0.8.0/headless-linux-x86_64/arc-node-linux-x86_64' \
        '/secure/operator/pretag-materialized-v0.8.0/headless-linux-x86_64/genesis.toml' \
        'scripts/recovery/legacy-validator-set-40m.json.sha256' \
        'os.O_WRONLY | os.O_CREAT | os.O_EXCL | os.O_NOFOLLOW' \
        '1615413b0cad59eedc8f9aa8ce41427e866f4b868f5b2148be48a1d722d7a3db' \
        '17238873' \
        "test \"\$(/usr/bin/tar -tzf \"\$caddy_archive\")\" = \"\$(printf 'LICENSE\\nREADME.md\\ncaddy')\"" \
        '/proc/self/fd/9 --config /proc/self/fd/8'
    do
        require_literal "$RECOVERY_README" "$literal" \
            'recovery README omits a create-only input or Linux archive/download invariant' \
            || return 1
    done

    handoff_help="$(python3 "$CUTOVER_HANDOFF_HELPER" --help)" || return 1
    for literal in \
        --repository-root --full-handoff-dir --verifier-binary \
        --inspector-binary --genesis --main-commit --tag --repository --push-remote
    do
        printf '%s\n' "$handoff_help" | grep -Fq -- "$literal" || {
            printf 'cutover handoff helper omits documented option: %s\n' "$literal"
            return 1
        }
        require_literal "$RECOVERY_README" "$literal" \
            'recovery README omits compact handoff helper option' || return 1
    done
    for literal in \
        'arc-recovery-final.lock.json.sha256' \
        'legacy-maintenance-boundary.json' \
        'recovery.arcchkpt' \
        'refs/arc-recovery-handoffs/$protected_main_sha' \
        'scripts/release/create-cutover-handoff-commit.py' \
        'workflow run recovery-release-handoff.yml' \
        'arc-recovery-release-handoff-$protected_main_sha' \
        'handoff_run_id=' \
        'handoff_artifact_id="$(printf' \
        'handoff_artifact_digest="$(printf' \
        'unset GH_TOKEN' \
        'GH_TOKEN="$GH_TOKEN" "$ARC_RECOVERY_PYTHON_PATH" -I' \
        '--push-remote origin' \
        '.local_ref_state == "created" or .local_ref_state == "reused"' \
        '.remote_ref_state == "created" or .remote_ref_state == "reused"' \
        'collaborators?affiliation=direct&per_page=100' \
        'direct_collaborators_before=' \
        'repos/FerrumVir/arc-chain/collaborators/arisarcmarket' \
        '-f permission=pull' \
        'direct_collaborators_after=' \
        '.[1].login == "arisarcmarket" and .[1].role_name == "read"' \
        'repos/FerrumVir/arc-chain/invitations?per_page=100' \
        'pending_writer_invitations' \
        'remote_release_count=' \
        '[.[] | select(.tag_name == "v0.8.0")] | length' \
        'repos/FerrumVir/arc-chain/immutable-releases --jq .enabled' \
        'repos/FerrumVir/arc-chain/rulesets/21690216' \
        'Restrict all ARC tag creation' \
        'repos/FerrumVir/arc-chain/rulesets/21667203' \
        'Protect all ARC tags from mutation' \
        'tag_ref_push_status=0' \
        '[.[] | select(.ref == "refs/tags/v0.8.0")] as $matches' \
        'GIT_CONFIG_NOSYSTEM=1 GIT_CONFIG_GLOBAL=/dev/null' \
        'GH_TOKEN="$GH_TOKEN" GH_PROMPT_DISABLED=1' \
        '-c core.hooksPath=/dev/null' \
        '-c credential.helper=' \
        'credential.https://github.com.helper=!$ARC_RECOVERY_GH_PATH auth git-credential' \
        '-c http.extraHeader=' \
        '-c http.https://github.com/.extraHeader=' \
        '-c protocol.allow=never' \
        '-c protocol.https.allow=always' \
        'push --porcelain --atomic --no-verify' \
        '--force-with-lease=refs/tags/v0.8.0:' \
        '$protected_main_sha:refs/tags/v0.8.0' \
        'remote_tag_after_state=' \
        'absence is proven and a fresh retry is safe' \
        'mandatory post-query proved the exact tag' \
        'tag_push_run_id=' \
        'actions/runs/$tag_push_run_id/jobs?filter=all&per_page=100' \
        'Release requires workflow_dispatch with a positive cutover_handoff_artifact_id.' \
        'workflow run release.yml' \
        '-f tag=v0.8.0' \
        '-f cutover_handoff_artifact_id="$handoff_artifact_id"' \
        '-f cutover_handoff_artifact_digest="$handoff_artifact_digest"' \
        'a tag-push event' \
        'cannot carry the protected handoff artifact ID' \
        'Do not move, delete, recreate, or re-push the tag' \
        'never rerun the tag' \
        'or release workflow'
    do
        require_literal "$RECOVERY_README" "$literal" \
            'recovery README omits compact handoff or manual release contract' \
            || return 1
    done
    for literal in \
        'HANDOFF-RUN-SELECTION.json' \
        'TAG-PUSH-RUN-SELECTION.json' \
        'RELEASE-RUN-SELECTION.json' \
        'Verify the immutable GitHub release without publication authority' \
        'expected_release_assets=' \
        '.immutable == true' \
        '| length) == 32' \
        'RELEASE-API-FINAL.json' \
        'frontend-push.XXXXXXXX' \
        '-c http.sslVerify=true' \
        'FRONTEND-MAIN-RULESET-BASELINE.json' \
        '.parameters.required_approving_review_count = 0' \
        '.parameters.require_last_push_approval = false' \
        'trap frontend_restore_on_exit EXIT' \
        '-f commit_title="$frontend_pr_title" -f merge_method=squash' \
        '"$frontend_main_sha $protected_main_sha"' \
        'PAGES-RUN-SELECTION.json' \
        'Verify and assemble public console' \
        'Publish GitHub Pages' \
        'repos/FerrumVir/arc-chain/deployments' \
        './shared/frontend/arc-network.json' \
        'arc.post-release-installer-canary.v1' \
        'Already up to date at v0.8.0' \
        'scripts/recovery/recovery_rollout.py verify' \
        'POST-RELEASE-ACCEPTANCE.json'
    do
        require_literal "$RECOVERY_README" "$literal" \
            'recovery README omits an executable release or post-release receipt gate' \
            || return 1
    done
    for forbidden in \
        "handoff_run_id='<" \
        "handoff_run_attempt='<" \
        "tag_push_run_id='<" \
        "release_run_id='<"
    do
        if grep -Fq -- "$forbidden" "$RECOVERY_README"; then
            printf 'recovery README retains unsafe manual run placeholder: %s\n' "$forbidden"
            return 1
        fi
    done
    if grep -Fq -- '.status == "built" and .build_type == "workflow"' \
        "$RECOVERY_README"; then
        printf 'recovery README treats nullable Pages status as a deployment proof\n'
        return 1
    fi
    for literal in \
        'scripts/release/create-cutover-handoff-commit.py --full-handoff-dir' \
        'Manually dispatch' \
        '`recovery-release-handoff.yml`' \
        '`cutover_handoff_artifact_id`' \
        '`cutover_handoff_artifact_digest`' \
        'exact direct set `FerrumVir` plus `arisarcmarket`' \
        '`arisarcmarket` to `pull`' \
        'zero pending invitations' \
        '21690216' \
        '21667203' \
        'isolated authenticated Git push' \
        '`--force-with-lease=refs/tags/v0.8.0:`' \
        'proven absence is safe to retry' \
        'automatic `release.yml`' \
        'tag-push run is expected to fail' \
        'expected to fail in the initial validation job' \
        'Prove that exact error' \
        'move or recreate the tag'
    do
        require_literal "$ROLLOUT" "$literal" \
            'validator rollout omits compact handoff or manual release contract' \
            || return 1
    done
    for literal in \
        'API-select exactly one workflow/path/event/branch/SHA run' \
        'independent immutable-release verifier' \
        'unique 32-asset digest-bound contract' \
        'temporarily' \
        'approval count and last-push approval' \
        'squash merge' \
        'successful `github-pages` deployment' \
        '`POST-RELEASE-ACCEPTANCE.json`'
    do
        require_literal "$ROLLOUT" "$literal" \
            'validator rollout omits an executable release or post-release receipt gate' \
            || return 1
    done
    if grep -Fq -- 'immediately deletes that exact release ID without' \
        "$RECOVERY_README" "$ROLLOUT"; then
        printf 'operator docs still claim an attempted publication is deleted\n'
        return 1
    fi
    handoff_section="$(awk '
        /^## Create the compact release handoff and publish$/ { capture=1 }
        capture { print }
        capture && /^Keep the runtime package-mutation masks/ { exit }
    ' "$RECOVERY_README")"
    if printf '%s\n' "$handoff_section" | grep -Fq 'export GH_TOKEN'; then
        printf 'compact handoff runbook exports GitHub authority to repository code\n'
        return 1
    fi
    previous=0
    for literal in \
        'unset GH_TOKEN' \
        'GH_TOKEN="$(' \
        'handoff_receipt="$(' \
        'workflow run recovery-release-handoff.yml' \
        'handoff_artifact_id="$(printf' \
        'repos/FerrumVir/arc-chain/collaborators/arisarcmarket' \
        'pending_writer_invitations=' \
        'creation_ruleset=' \
        'mutation_ruleset=' \
        'tag_ref_push_status=0' \
        'tag_push_run_id=' \
        'workflow run release.yml'
    do
        line="$(printf '%s\n' "$handoff_section" | grep -nF -- "$literal" \
            | head -n 1 | cut -d: -f1)"
        if [ -z "$line" ] || [ "$line" -le "$previous" ]; then
            printf 'compact handoff/release command is absent or out of order: %s\n' \
                "$literal"
            return 1
        fi
        previous="$line"
    done

    post_release_acceptance_line="$(grep -nF -- 'POST-RELEASE-ACCEPTANCE.json' \
        "$RECOVERY_README" | tail -n 1 | cut -d: -f1)"
    post_release_unmask_line="$(grep -nF -- '/usr/bin/systemctl unmask --runtime' \
        "$RECOVERY_README" | tail -n 1 | cut -d: -f1)"
    procedure_end_line="$(grep -nF -- \
        '<!-- END EXECUTABLE PRODUCTION RECOVERY PROCEDURE -->' \
        "$RECOVERY_README" | cut -d: -f1)"
    if [ -z "$post_release_acceptance_line" ] || [ -z "$post_release_unmask_line" ] \
        || [ -z "$procedure_end_line" ] \
        || [ "$post_release_acceptance_line" -ge "$post_release_unmask_line" ] \
        || [ "$post_release_unmask_line" -ge "$procedure_end_line" ]; then
        printf 'executable procedure ends before post-release acceptance and unmask gates\n'
        return 1
    fi

    if grep -Eq '751619276800|750 GiB|700 GiB' \
        "$RECOVERY_README" "$DRIVE_PREFREEZE_GATE"; then
        printf 'Drive recovery policy retains a binary-unit quota above the decimal cap\n'
        return 1
    fi

    for literal in \
        '700000000000 bytes (700 GB decimal)' \
        '50000000000 bytes (50 GB)' \
        'https://developers.google.com/workspace/drive/api/guides/limits'
    do
        require_literal "$ROLLOUT" "$literal" \
            'validator rollout omits the reviewed decimal Drive quota boundary' \
            || return 1
    done

    for pair in \
        '["headless","linux-x86_64"]' \
        '["headless","linux-arm64"]' \
        '["headless","macos-arm64"]' \
        '["headless","macos-x86_64"]' \
        '["headless","windows-x86_64"]' \
        '["desktop","linux-x86_64"]' \
        '["desktop","macos-arm64"]' \
        '["desktop","macos-x86_64"]' \
        '["desktop","windows-x86_64"]'
    do
        count="$(grep -Fc -- "$pair" "$RECOVERY_README")"
        if [ "$count" -lt 2 ]; then
            printf 'pre-tag selection/input-set commands do not carry both shapes for %s\n' "$pair"
            return 1
        fi
    done
    require_literal "$RECOVERY_README" 'artifact_id: $s.artifacts[$platform][$kind].id' \
        'pre-tag input set does not bind the selected artifact ID' || return 1
    require_literal "$RECOVERY_README" 'raw_actions_zip:' \
        'pre-tag input set does not bind the raw Actions response' || return 1

    previous=0
    for literal in \
        'scripts/release/materialize-pretag-artifacts.py' \
        'scripts/release/restore-validator-vault.py restore' \
        'scripts/recovery/archive-fleet-to-drive.sh prepare-writers' \
        'scripts/recovery/archive-fleet-to-drive.sh seal-freeze-plan' \
        $'scripts/recovery/archive-fleet-to-drive.sh capture \\' \
        'scripts/release/restore-validator-vault.py install' \
        $'"$arc_node_linux" recovery export \\' \
        $'offline_signer "$signing_binary" recovery sign \\' \
        $'scripts/recovery/build-production-manifest.py prearchive \\' \
        $'scripts/recovery/archive-fleet-to-drive.sh seal \\'
    do
        line="$(first_literal_line "$RECOVERY_README" "$literal")"
        if [ -z "$line" ] || [ "$line" -le "$previous" ]; then
            printf 'recovery command is absent or out of mandatory order: %s\n' "$literal"
            return 1
        fi
        previous="$line"
    done

    previous="$(first_literal_line "$ROLLOUT" 'Only after this successful capture')"
    line="$(first_literal_line "$ROLLOUT" $'"$ARC_RECOVERY_PYTHON_PATH" -I scripts/release/restore-validator-vault.py install \\')"
    if [ -z "$previous" ] || [ -z "$line" ] || [ "$line" -le "$previous" ]; then
        printf 'validator rollout places vault install before the successful capture boundary\n'
        return 1
    fi

    count="$(grep -Fc -- '--validator-public-keys /secure/operator/production-input-stage-v0.8.0/validator-public-keys.json' "$RECOVERY_README")"
    assert_equals 2 "$count" \
        'archive plan and execute do not both use the staged validator manifest' || return 1

    for literal in \
        'signing_keys=(' \
        'test "${#signing_keys[@]}" = 5' \
        'NYC.validator-key.json' 'LAX.validator-key.json' 'AMS.validator-key.json' \
        'LHR.validator-key.json' 'NRT.validator-key.json' \
        'protected build-metadata hash' \
        $'(cd /secure/operator && \\' \
        '/usr/bin/sha256sum --check --strict arc-network.recovered.json.sha256)'
    do
        require_literal "$RECOVERY_README" "$literal" \
            'recovery README omits the isolated five-key or frontend-sidecar contract' \
            || return 1
    done

    audit_block="$(awk '
        /A later read-only audit can use externally captured evidence:/ { capture=1; lines=0 }
        capture { print; lines += 1; if (lines == 8) exit }
    ' "$RECOVERY_README")"
    printf '%s\n' "$audit_block" \
        | grep -Fq -- '--manifest /secure/operator/arc-recovery-final.lock.json' || {
        printf 'final reward audit still targets the pre-final recovery manifest\n'
        return 1
    }

    if grep -Eq -- \
        '/secure/operator/pretag-linux-x86_64/arc-node|--genesis /secure/operator/genesis[.]toml|--validator-public-keys /secure/operator/validator-public-keys[.]json|five physically separate signing stations|never gather five private keys|shasum -a 256 -c /secure/operator/arc-network[.]recovered[.]json[.]sha256' \
        "$RECOVERY_README"; then
        printf 'recovery README retains a stale materialization, signing, or sidecar command\n'
        return 1
    fi
}

recovery_plan_read_only_scope_is_truthful() {
    local literal
    for literal in \
        'makes no persistent recovery-managed change' \
        'Production probes stream directly over the pinned' \
        'do not install a remote rollout helper before the exact GO' \
        'Normal SSH and service audit logs may record that read access.'
    do
        require_literal "$RECOVERY_README" "$literal" \
            'recovery README omits the scoped non-mutating plan contract' \
            || return 1
    done
    for literal in \
        'defaults to recovery-state read-only' \
        'streams production probes over pinned SSH without installing a remote rollout' \
        'before any persistent' \
        'Normal SSH and service audit logs may record plan access.'
    do
        require_literal "$PRODUCTION_RECOVERY_AUDIT" "$literal" \
            'production recovery audit omits the scoped non-mutating plan contract' \
            || return 1
    done
    if grep -Fq -- \
        'It changes no local/remote directory, process, service,' \
        "$RECOVERY_README"; then
        printf 'recovery README retains the impossible absolute no-host-change claim\n'
        return 1
    fi
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
run_test 'operator recovery commands are Ubuntu-pinned, create-only, and ordered' operator_recovery_commands_are_linux_pinned_and_ordered
run_test 'recovery plan documents a scoped persistent-state read-only boundary' recovery_plan_read_only_scope_is_truthful

finish_tests
