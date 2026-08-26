#!/usr/bin/env bash
set -uo pipefail

TEST_DIR="$(CDPATH='' cd -- "$(dirname -- "$0")" && pwd)"
REPO_ROOT="$(CDPATH='' cd -- "$TEST_DIR/../.." && pwd)"
# shellcheck source=/dev/null
. "$TEST_DIR/helpers/testlib.sh"

ASSEMBLER="$REPO_ROOT/scripts/release/assemble-release.sh"
CANONICAL_HEADLESS_ASSETS='
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

ASSEMBLY_SANDBOXES=""
NEW_ASSEMBLY_SANDBOX=""
cleanup_assembly_sandboxes() {
    local sandbox
    for sandbox in $ASSEMBLY_SANDBOXES; do
        [ -n "$sandbox" ] && rm -rf "$sandbox"
    done
}
trap cleanup_assembly_sandboxes EXIT

write_nonempty() {
    local destination="$1" content="${2:-fixture}"
    mkdir -p "$(dirname "$destination")"
    printf '%s\n' "$content" >"$destination"
}

new_assembly_fixture() {
    local sandbox artifacts asset
    sandbox="$(mktemp -d "${TMPDIR:-/tmp}/arc-release-assembly.XXXXXX")"
    ASSEMBLY_SANDBOXES="$ASSEMBLY_SANDBOXES $sandbox"
    artifacts="$sandbox/artifacts"
    mkdir -p "$artifacts" "$sandbox/output"

    for asset in $CANONICAL_HEADLESS_ASSETS; do
        write_nonempty "$artifacts/headless/$asset" "binary fixture: $asset"
    done

    write_nonempty "$artifacts/arc-desktop-macos-arm64/ARC.Node.app.tar.gz"
    write_nonempty "$artifacts/arc-desktop-macos-arm64/ARC.Node.app.tar.gz.sig" 'mac-arm-signature'
    write_nonempty "$artifacts/arc-desktop-macos-arm64/ARC.Node.dmg"

    write_nonempty "$artifacts/arc-desktop-macos-x86_64/ARC.Node.app.tar.gz"
    write_nonempty "$artifacts/arc-desktop-macos-x86_64/ARC.Node.app.tar.gz.sig" 'mac-intel-signature'
    write_nonempty "$artifacts/arc-desktop-macos-x86_64/ARC.Node.dmg"

    write_nonempty "$artifacts/arc-desktop-windows-x86_64/ARC.Node_0.8.0_x64-setup.exe"
    write_nonempty "$artifacts/arc-desktop-windows-x86_64/ARC.Node_0.8.0_x64-setup.exe.sig" 'windows-signature'
    write_nonempty "$artifacts/arc-desktop-windows-x86_64/ARC.Node_0.8.0_x64.msi"

    write_nonempty "$artifacts/arc-desktop-linux-x86_64/ARC.Node_0.8.0_amd64.AppImage"
    write_nonempty "$artifacts/arc-desktop-linux-x86_64/ARC.Node_0.8.0_amd64.AppImage.sig" 'linux-signature'
    write_nonempty "$artifacts/arc-desktop-linux-x86_64/ARC.Node_0.8.0_amd64.deb"
    write_nonempty "$artifacts/arc-desktop-linux-x86_64/ARC.Node-0.8.0-1.x86_64.rpm"

    NEW_ASSEMBLY_SANDBOX="$sandbox"
}

run_assembler() {
    local sandbox="$1" release_tag="${2:-v0.8.0}"
    (
        cd "$REPO_ROOT" || exit 1
        env \
            ARTIFACTS_DIR="$sandbox/artifacts" \
            OUTPUT_DIR="$sandbox/output" \
            RELEASE_TAG="$release_tag" \
            RELEASE_DATE='2026-08-26T12:00:00Z' \
            REPOSITORY='FerrumVir/arc-chain' \
            /bin/bash "$ASSEMBLER"
    )
}

complete_fixture_produces_verifiable_contract() {
    local sandbox asset expected_count actual_count filename
    new_assembly_fixture
    sandbox="$NEW_ASSEMBLY_SANDBOX"
    if ! run_assembler "$sandbox" >"$sandbox/assemble.out" 2>&1; then
        sed -n '1,160p' "$sandbox/assemble.out"
        return 1
    fi

    for asset in $CANONICAL_HEADLESS_ASSETS; do
        if [ ! -s "$sandbox/output/$asset" ]; then
            printf 'assembled release is missing canonical headless asset: %s\n' "$asset"
            return 1
        fi
    done
    [ -s "$sandbox/output/SHA256SUMS" ] || {
        printf 'assembled release has no SHA256SUMS\n'
        return 1
    }

    expected_count="$(find "$sandbox/output" -maxdepth 1 -type f ! -name SHA256SUMS | wc -l | tr -d ' ')"
    actual_count="$(awk 'NF == 2 { count += 1 } END { print count + 0 }' "$sandbox/output/SHA256SUMS")"
    assert_equals "$expected_count" "$actual_count" 'SHA256SUMS must cover every other published file exactly once' || return 1

    while read -r _hash filename; do
        if [ ! -s "$sandbox/output/$filename" ]; then
            printf 'SHA256SUMS names a missing/empty file: %s\n' "$filename"
            return 1
        fi
    done <"$sandbox/output/SHA256SUMS"

    if command -v sha256sum >/dev/null 2>&1; then
        (cd "$sandbox/output" && sha256sum -c SHA256SUMS >/dev/null) || return 1
    else
        (cd "$sandbox/output" && shasum -a 256 -c SHA256SUMS >/dev/null) || return 1
    fi

    assert_file_contains "$sandbox/output/latest.json" '"version":[[:space:]]*"0\.8\.0"' \
        'latest.json does not carry the immutable tag version' || return 1
    assert_file_not_contains "$sandbox/output/latest.json" 'releases/latest/download' \
        'latest.json payload URLs must point at the immutable release tag' || return 1
    assert_file_contains "$sandbox/output/latest.json" '/releases/download/v0\.8\.0/' \
        'latest.json has no exact-tag artifact URL' || return 1
}

every_headless_asset_is_individually_required() {
    local asset sandbox output status
    for asset in $CANONICAL_HEADLESS_ASSETS; do
        new_assembly_fixture
        sandbox="$NEW_ASSEMBLY_SANDBOX"
        output="$sandbox/missing.out"
        find "$sandbox/artifacts" -type f -name "$asset" -delete
        run_assembler "$sandbox" >"$output" 2>&1
        status=$?
        if [ "$status" -eq 0 ]; then
            printf 'assembler accepted release with missing canonical asset: %s\n' "$asset"
            return 1
        fi
        if ! grep -Fq "$asset" "$output"; then
            printf 'missing-asset failure did not identify %s:\n' "$asset"
            sed -n '1,80p' "$output"
            return 1
        fi
        if [ -s "$sandbox/output/SHA256SUMS" ]; then
            printf 'failed assembly left a publish-ready SHA256SUMS after %s was missing\n' "$asset"
            return 1
        fi
    done
}

duplicate_asset_is_rejected() {
    local sandbox output status
    new_assembly_fixture
    sandbox="$NEW_ASSEMBLY_SANDBOX"
    write_nonempty "$sandbox/artifacts/duplicate/arc-node-linux-x86_64" 'collision fixture'
    output="$sandbox/duplicate.out"
    run_assembler "$sandbox" >"$output" 2>&1
    status=$?
    if [ "$status" -eq 0 ]; then
        printf 'assembler silently chose one of two same-named assets\n'
        return 1
    fi
    grep -Eq 'exactly one.*arc-node-linux-x86_64.*found 2' "$output" || {
        printf 'duplicate failure was not explicit:\n'
        sed -n '1,80p' "$output"
        return 1
    }
}

non_semver_release_tag_is_rejected() {
    local sandbox output status
    new_assembly_fixture
    sandbox="$NEW_ASSEMBLY_SANDBOX"
    output="$sandbox/tag.out"
    run_assembler "$sandbox" 'v0.8.0-rc1' >"$output" 2>&1
    status=$?
    if [ "$status" -eq 0 ]; then
        printf 'assembler accepted non-strict release tag v0.8.0-rc1\n'
        return 1
    fi
}

run_test 'complete fixture produces exact-tag manifest and verified SHA256SUMS' complete_fixture_produces_verifiable_contract
run_test 'each of the ten canonical headless assets is independently required' every_headless_asset_is_individually_required
run_test 'duplicate same-named artifacts fail closed' duplicate_asset_is_rejected
run_test 'release assembly accepts only strict vX.Y.Z tags' non_semver_release_tag_is_rejected

finish_tests
