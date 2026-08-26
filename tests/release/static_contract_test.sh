#!/usr/bin/env bash
set -uo pipefail

TEST_DIR="$(CDPATH='' cd -- "$(dirname -- "$0")" && pwd)"
REPO_ROOT="$(CDPATH='' cd -- "$TEST_DIR/../.." && pwd)"
# shellcheck source=/dev/null
. "$TEST_DIR/helpers/testlib.sh"

RELEASE_WORKFLOW="$REPO_ROOT/.github/workflows/release.yml"
ASSEMBLER="$REPO_ROOT/scripts/release/assemble-release.sh"
INSTALLER="$REPO_ROOT/install.sh"

REQUIRED_ASSETS='
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
CHECKSUM_MANIFEST='SHA256SUMS'

required_assets_are_built_and_gated() {
    local asset
    for asset in $REQUIRED_ASSETS; do
        grep -Fq "$asset" "$ASSEMBLER" || {
            printf 'release assembler does not independently require asset: %s\n' "$asset"
                return 1
            }
    done
    grep -Fq 'cargo build --release --locked -p arc-node' "$RELEASE_WORKFLOW" || {
        printf 'release workflow does not build the headless node from Cargo.lock\n'
        return 1
    }
    grep -Fq 'cargo build --release --locked -p arc-cli' "$RELEASE_WORKFLOW" || {
        printf 'release workflow does not build the headless CLI from Cargo.lock\n'
        return 1
    }
}

linux_arm_asset_name_is_consistent_and_required() {
    local mapping_count arm_matrix_block
    grep -Fq 'platform: linux-arm64' "$RELEASE_WORKFLOW" || {
        printf 'release workflow does not build the canonical Linux ARM node asset\n'
        return 1
    }
    if grep -Fq 'arc-node-linux-aarch64' "$RELEASE_WORKFLOW" "$ASSEMBLER" "$INSTALLER" \
        || grep -Fq 'arc-cli-linux-aarch64' "$RELEASE_WORKFLOW" "$ASSEMBLER" "$INSTALLER"; then
        printf 'release contract uses arm64 in filenames; a legacy aarch64 asset name remains\n'
        return 1
    fi

    arm_matrix_block="$(awk '
        /target:[[:space:]]*aarch64-unknown-linux-gnu/ { capture=1 }
        capture { print }
        capture && /optional:/ { exit }
        capture && /steps:/ { exit }
    ' "$RELEASE_WORKFLOW")"
    if printf '%s\n' "$arm_matrix_block" | grep -Eq 'optional:[[:space:]]*true'; then
        printf 'Linux ARM is a canonical required asset but its matrix leg is still optional\n'
        return 1
    fi

    mapping_count="$(grep -Ec 'Linux:aarch64\|Linux:arm64' "$INSTALLER" || true)"
    if [ "$mapping_count" -lt 1 ] || ! grep -Eq 'PLATFORM=linux-arm64' "$INSTALLER"; then
        printf 'installer does not map Linux ARM aliases to arc-node-linux-arm64\n'
        return 1
    fi
}

checksum_manifest_is_published_and_gated() {
    grep -Fq "$CHECKSUM_MANIFEST" "$ASSEMBLER" || {
        printf '%s is never produced by the release assembler\n' "$CHECKSUM_MANIFEST"
        return 1
    }
    grep -Eq '(sha256sum|shasum[[:space:]]+-a[[:space:]]+256)' "$ASSEMBLER" || {
        printf 'release assembler mentions %s but does not calculate SHA-256 hashes\n' "$CHECKSUM_MANIFEST"
        return 1
    }
    grep -Fq './scripts/release/assemble-release.sh' "$RELEASE_WORKFLOW" || {
        printf 'publisher does not run the release assembler before uploading\n'
        return 1
    }
    if [ "$(grep -Rhc 'uses: softprops/action-gh-release@' "$REPO_ROOT/.github/workflows"/*.yml | awk '{sum += $1} END {print sum + 0}')" -ne 1 ]; then
        printf 'release graph must contain exactly one GitHub release publisher\n'
        return 1
    fi
}

installer_and_updater_verify_checksums() {
    grep -Fq "$CHECKSUM_MANIFEST" "$INSTALLER" || {
        printf 'installer/update path does not download %s\n' "$CHECKSUM_MANIFEST"
        return 1
    }
    grep -Eq '(sha256sum|shasum[[:space:]]+-a[[:space:]]+256)' "$INSTALLER" || {
        printf 'installer/update path does not verify SHA-256 before replacement\n'
        return 1
    }
    grep -Fq -- '--update-only' "$INSTALLER" || {
        printf 'installed updater has no explicit noninteractive --update-only mode\n'
        return 1
    }
}

raw_node_downloads_are_version_pinned() {
    if grep -Eq 'releases/latest/download/(arc-(node|cli)|\$\{?(NODE_|CLI_)?ASSET)' "$INSTALLER"; then
        printf 'unversioned raw binary download remains in install.sh:\n'
        grep -En 'releases/latest/download/(arc-(node|cli)|\$\{?(NODE_|CLI_)?ASSET)' "$INSTALLER" | sed -n '1,6p'
        return 1
    fi

    grep -Fq 'ARC_NODE_VERSION' "$INSTALLER" || {
        printf 'installer has no explicit ARC_NODE_VERSION pin\n'
        return 1
    }
    # shellcheck disable=SC2016 # These are intentional literals in production source.
    if ! grep -Fq 'RELEASE_URL="$DOWNLOAD_ROOT/$RESOLVED_TAG"' "$INSTALLER" \
        || ! grep -Fq 'github_curl "$RELEASE_URL/$asset"' "$INSTALLER"; then
        printf 'installer does not bind binary downloads to its exact resolved tag\n'
        return 1
    fi
    if grep -Eq '/releases\?per_page=|(^|[[:space:]])-r[[:space:]]+0-0' "$INSTALLER"; then
        printf 'installer still walks releases or probes assets with Range requests instead of resolving one release metadata object\n'
        return 1
    fi
}

updater_has_semver_downgrade_guard() {
    # Equality alone is insufficient: if the newest asset-bearing release is
    # older than the local build, an updater must keep the local build.
    if ! grep -Eq '(version_(gt|compare|is_newer)|sort[[:space:]]+-V|dpkg[[:space:]]+--compare-versions)' "$INSTALLER"; then
        printf 'embedded updater has no numeric semantic-version greater-than guard\n'
        return 1
    fi
}

installer_normalizes_service_identity_and_secret_permissions() {
    # shellcheck disable=SC2016 # Detect the unsafe literal, not this test's user.
    if grep -Fq 'User=$USER' "$INSTALLER"; then
        # shellcheck disable=SC2016 # Report the unsafe literal verbatim.
        printf 'systemd units still bind ownership to ambient $USER (breaks sudo/root installs)\n'
        return 1
    fi
    grep -Fq 'SUDO_USER' "$INSTALLER" || {
        printf 'installer never normalizes a sudo invocation back to the invoking user\n'
        return 1
    }
    # shellcheck disable=SC2016 # Regex intentionally matches a literal shell variable.
    if ! grep -Eq '(chmod[[:space:]]+0?600[[:space:]]+"?\$SEED_FILE|umask[[:space:]]+0?77)' "$INSTALLER"; then
        printf 'identity seed is not protected by chmod 0600 or umask 077\n'
        return 1
    fi
}

linux_compat_smoke_has_one_docker_run() {
    local smoke_block docker_run_count
    smoke_block="$(awk '
        /^  linux-server-compat:/ { capture=1 }
        capture { print }
        capture && /^  desktop:/ { exit }
    ' "$RELEASE_WORKFLOW")"

    docker_run_count="$(printf '%s\n' "$smoke_block" | grep -Ec '^[[:space:]]+docker run --rm[[:space:]]*\\$' || true)"
    if [ "$docker_run_count" -ne 1 ]; then
        printf 'Ubuntu compatibility loop must contain exactly one docker run command; found %s\n' "$docker_run_count"
        return 1
    fi
    if printf '%s\n' "$smoke_block" | awk '
        /^[[:space:]]+docker run --rm[[:space:]]*\\$/ {
            if (previous) duplicate=1
            previous=1
            next
        }
        { previous=0 }
        END { exit duplicate ? 0 : 1 }
    '; then
        printf 'Ubuntu compatibility loop contains consecutive duplicate docker run commands\n'
        return 1
    fi
    # shellcheck disable=SC2016 # GitHub expression literals are the contract.
    for required in 'for ubuntu in 24.04 26.04' 'arc-node-${{ matrix.platform }}' 'arc-cli-${{ matrix.platform }}' '--env DISPLAY='; do
        printf '%s\n' "$smoke_block" | grep -Fq -- "$required" || {
            printf 'Ubuntu compatibility smoke is missing: %s\n' "$required"
            return 1
        }
    done
}

relevant_shell_is_syntax_valid() {
    local file
    for file in "$INSTALLER" "$ASSEMBLER" "$TEST_DIR"/*.sh "$TEST_DIR"/helpers/*.sh; do
        bash -n "$file" || return 1
    done
}

run_test 'required headless assets are built and gate the sole publisher' required_assets_are_built_and_gated
run_test 'Linux ARM uses canonical arm64 names and is release-blocking' linux_arm_asset_name_is_consistent_and_required
run_test 'release publishes and gates a SHA256SUMS manifest' checksum_manifest_is_published_and_gated
run_test 'installer and update-only path verify SHA-256 before replacement' installer_and_updater_verify_checksums
run_test 'raw node consumers use exact versioned release URLs' raw_node_downloads_are_version_pinned
run_test 'update-only path refuses equal and older semantic versions' updater_has_semver_downgrade_guard
run_test 'installer normalizes service identity and protects its seed' installer_normalizes_service_identity_and_secret_permissions
run_test 'Ubuntu compatibility loop has one non-duplicated docker invocation' linux_compat_smoke_has_one_docker_run
run_test 'release-related shell scripts pass bash syntax validation' relevant_shell_is_syntax_valid

finish_tests
