#!/usr/bin/env bash
set -uo pipefail

TEST_DIR="$(CDPATH='' cd -- "$(dirname -- "$0")" && pwd)"
REPO_ROOT="$(CDPATH='' cd -- "$TEST_DIR/../.." && pwd)"
# shellcheck source=/dev/null
. "$TEST_DIR/helpers/testlib.sh"

RELEASE_WORKFLOW="$REPO_ROOT/.github/workflows/release.yml"
CI_WORKFLOW="$REPO_ROOT/.github/workflows/ci.yml"
GOLDEN_WORKFLOW="$REPO_ROOT/.github/workflows/golden-vectors.yml"
DEPENDABOT_CONFIG="$REPO_ROOT/.github/dependabot.yml"
ASSEMBLER="$REPO_ROOT/scripts/release/assemble-release.sh"
SIGNING_KEY_BACKUP="$REPO_ROOT/scripts/release/backup-signing-keys.sh"
INSTALLER="$REPO_ROOT/install.sh"
LEGACY_INSTALLER="$REPO_ROOT/scripts/install-node.sh"
GENESIS_VALIDATOR="$REPO_ROOT/scripts/release/validate-genesis.py"
SECRET_SCANNER="$TEST_DIR/current_tree_secret_scan.sh"
SECRET_MATERIALIZER="$TEST_DIR/materialize_releasable_tree.py"
QUALITY_HARNESS="$REPO_ROOT/scripts/ci_check.sh"
CARGO_DENY_RUNNER="$REPO_ROOT/scripts/ci/run-cargo-deny.sh"
VENDORED_ADVISORY_SHADOW="$REPO_ROOT/scripts/ci/vendored-advisory-shadow.py"
COMMUNITY_JOIN="$REPO_ROOT/scripts/join-testnet.sh"
INFERENCE_JOIN="$REPO_ROOT/scripts/join-inference.sh"
INFERENCE_INSTALL="$REPO_ROOT/scripts/install-inference-node.sh"
UPDATER_SIGNATURE_GATE="$TEST_DIR/verify_tauri_updater_signatures.sh"
UPDATER_VERIFIER_MANIFEST="$TEST_DIR/tauri-updater-verifier/Cargo.toml"
UPDATER_FIXTURE_DIR="$TEST_DIR/fixtures/tauri-updater"
RELEASE_ALLOWED_SIGNERS="$REPO_ROOT/release/arc-release-allowed-signers"
RELEASE_PREFLIGHT_WORKFLOW="$REPO_ROOT/.github/workflows/release-signing-preflight.yml"
RECOVERY_RELEASE_HANDOFF_WORKFLOW="$REPO_ROOT/.github/workflows/recovery-release-handoff.yml"
SIGNING_BACKUP_WORKFLOW="$REPO_ROOT/.github/workflows/release-signing-backup.yml"
SIGNING_BACKUP_VERIFY="$REPO_ROOT/scripts/release/verify-signing-key-backup.sh"
PRETAG_SELECTOR="$REPO_ROOT/scripts/release/select-pretag-artifacts.py"
PRETAG_MATERIALIZER="$REPO_ROOT/scripts/release/materialize-pretag-artifacts.py"
PRETAG_LIVE_VERIFY="$REPO_ROOT/scripts/release/verify-pretag-run-and-artifacts.sh"
PROTECTED_PRETAG_ARTIFACT="$REPO_ROOT/scripts/release/protected_pretag_artifact.py"
MACOS_COMMUNITY_CANARY="$REPO_ROOT/scripts/release/macos-community-canary.py"
MACOS_COMMUNITY_CANARY_DOC="$REPO_ROOT/docs/MACOS-PRETAG-COMMUNITY-CANARY.md"
MACOS_COMMUNITY_CANARY_TEST="$TEST_DIR/macos_community_canary_test.sh"
RELEASE_SERVER_VERIFIER="$REPO_ROOT/scripts/release/verify-github-release.py"
RELEASE_DELETE_HELPER="$REPO_ROOT/scripts/release/delete-release-by-id.sh"
VALIDATOR_VAULT_RESTORE="$REPO_ROOT/scripts/release/restore-validator-vault.py"
PRODUCTION_MANIFEST_BUILDER="$REPO_ROOT/scripts/recovery/build-production-manifest.py"
PRODUCTION_MANIFEST_TEST="$TEST_DIR/production_manifest_builder_test.sh"
POSTRELEASE_PUBLIC_TRUTH="$REPO_ROOT/scripts/release/build-postrelease-public-truth.py"
POSTRELEASE_PUBLIC_TRUTH_TEST="$TEST_DIR/postrelease_public_truth_test.sh"
PUBLIC_PRODUCTION_STATUS="$REPO_ROOT/shared/frontend/production-status.json"
ROOT_README="$REPO_ROOT/README.md"
RECOVERY_RUNBOOK="$REPO_ROOT/scripts/recovery/README.md"
RECOVERY_ROLLOUT="$REPO_ROOT/scripts/recovery/recovery_rollout.py"
RECOVERY_ARCHIVE="$REPO_ROOT/scripts/recovery/archive-fleet-to-drive.sh"
OWNER_EMERGENCY_HELPER="$REPO_ROOT/scripts/recovery/owner-emergency-recovery.py"
OWNER_EMERGENCY_SCHEMA="$REPO_ROOT/scripts/recovery/owner-emergency-recovery.schema.json"
OWNER_EMERGENCY_TEST="$REPO_ROOT/scripts/recovery/test_owner_emergency_recovery.py"
OWNER_EMERGENCY_WORKFLOW="$REPO_ROOT/.github/workflows/owner-emergency-recovery-approval.yml"
RELEASE_TEST_RUNNER="$TEST_DIR/run.sh"
DESKTOP_CARGO_LOCK="$REPO_ROOT/desktop/src-tauri/Cargo.lock"
DESKTOP_NPM_LOCK="$REPO_ROOT/desktop/package-lock.json"
CANONICAL_SEEDS="$REPO_ROOT/testnet-seeds.txt"
DESKTOP_SEEDS="$REPO_ROOT/desktop/src-tauri/resources/testnet-seeds.txt"
VALIDATOR_TRANSPORT="$REPO_ROOT/crates/arc-net/src/transport.rs"
NODE_MAIN="$REPO_ROOT/crates/arc-node/src/main.rs"
DESKTOP_NODE_MANAGER="$REPO_ROOT/desktop/src-tauri/src/node_manager.rs"
DESKTOP_SHUTDOWN_INTEGRATION="$REPO_ROOT/crates/arc-node/tests/desktop_shutdown_lifecycle.rs"

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
    local asset headless_block preflight_headless_block platform_count
    for asset in $REQUIRED_ASSETS; do
        grep -Fq "$asset" "$ASSEMBLER" || {
            printf 'release assembler does not independently require asset: %s\n' "$asset"
                return 1
            }
    done
    preflight_headless_block="$(awk '
        /^  headless-runtime:/ { capture=1 }
        capture { print }
        capture && /^  desktop-unsigned:/ { exit }
    ' "$RELEASE_PREFLIGHT_WORKFLOW")"
    grep -Fq 'cargo build --release --locked -p arc-node' \
        <<< "$preflight_headless_block" || {
        printf 'pre-tag workflow does not build the headless node from Cargo.lock\n'
        return 1
    }
    grep -Fq 'cargo build --release --locked -p arc-cli' \
        <<< "$preflight_headless_block" || {
        printf 'pre-tag workflow does not build the headless CLI from Cargo.lock\n'
        return 1
    }
    platform_count="$(printf '%s\n' "$preflight_headless_block" \
        | grep -Ec '^[[:space:]]+- platform:' || true)"
    [ "$platform_count" -eq 5 ] || {
        printf 'pre-tag headless matrix must build all five release targets; found %s\n' \
            "$platform_count"
        return 1
    }
    headless_block="$(awk '
        /^  headless:/ { capture=1 }
        capture { print }
        capture && /^  linux-server-compat:/ { exit }
    ' "$RELEASE_WORKFLOW")"
    for required in \
        'artifact-ids: ${{ fromJSON(needs.validate.outputs.pretag_artifacts)[matrix.platform].headless.id }}' \
        'skip-decompress: true' \
        'digest-mismatch: error' \
        'scripts/release/materialize-pretag-artifacts.py' \
        'Re-smoke the exact binaries selected for publication'
    do
        printf '%s\n' "$headless_block" | grep -Fq -- "$required" || {
            printf 'headless release consumer is not exact-preflight bound: %s\n' \
                "$required"
            return 1
        }
    done
    if grep -Fq 'cargo build --release --locked -p arc-node' "$headless_block"; then
        printf 'tag workflow independently rebuilds headless release bytes\n'
        return 1
    fi
}

desktop_tauri_packages_are_release_compatible() {
    python3 - "$DESKTOP_CARGO_LOCK" "$DESKTOP_NPM_LOCK" <<'PY'
import json
import sys
import tomllib

cargo_lock_path, npm_lock_path = sys.argv[1:]
with open(cargo_lock_path, "rb") as handle:
    cargo_lock = tomllib.load(handle)
with open(npm_lock_path, encoding="utf-8") as handle:
    npm_lock = json.load(handle)


def cargo_version(name):
    versions = sorted(
        {package["version"] for package in cargo_lock["package"] if package["name"] == name}
    )
    if len(versions) != 1:
        raise SystemExit(
            f"desktop Cargo.lock must contain exactly one {name!r} version; found {versions}"
        )
    return versions[0]


def npm_version(name):
    package = npm_lock.get("packages", {}).get(f"node_modules/{name}")
    if not package or not package.get("version"):
        raise SystemExit(f"desktop package-lock.json is missing locked {name!r}")
    return package["version"]


def major_minor(version):
    fields = version.split(".")
    if len(fields) < 2 or not all(field.isdigit() for field in fields[:2]):
        raise SystemExit(f"cannot compare non-semver package version {version!r}")
    return tuple(map(int, fields[:2]))


rust_tauri = cargo_version("tauri")
for js_package in ("@tauri-apps/api", "@tauri-apps/cli"):
    js_version = npm_version(js_package)
    if major_minor(js_version) != major_minor(rust_tauri):
        raise SystemExit(
            "Tauri release packages are incompatible: "
            f"Rust tauri={rust_tauri}, {js_package}={js_version}; "
            "the exact `npx tauri build --ci ... -- --locked` release command will refuse this graph"
        )

for rust_package, js_package in (
    ("tauri-plugin-updater", "@tauri-apps/plugin-updater"),
    ("tauri-plugin-process", "@tauri-apps/plugin-process"),
    ("tauri-plugin-shell", "@tauri-apps/plugin-shell"),
):
    rust_version = cargo_version(rust_package)
    js_version = npm_version(js_package)
    if js_version != rust_version:
        raise SystemExit(
            "paired Tauri plugin versions drifted: "
            f"{rust_package}={rust_version}, {js_package}={js_version}"
        )
PY
}

packaged_desktop_network_resources_match_release() {
    cmp -s "$CANONICAL_SEEDS" "$DESKTOP_SEEDS" || {
        printf 'desktop packaged seed list differs from the canonical release seed list\n'
        diff -u "$CANONICAL_SEEDS" "$DESKTOP_SEEDS" | sed -n '1,80p'
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
    if grep -Rqs 'uses: softprops/action-gh-release@' "$REPO_ROOT/.github/workflows"/*.yml; then
        printf 'release graph still uses an update-capable third-party release action\n'
        return 1
    fi
    if [ "$(grep -Fc 'gh api --method POST' "$RELEASE_WORKFLOW")" -ne 1 ] \
        || [ "$(grep -Fc 'gh api --method PATCH' "$RELEASE_WORKFLOW")" -ne 1 ] \
        || [ "$(grep -Fc 'gh release upload "$RELEASE_TAG" release-files/*' "$RELEASE_WORKFLOW")" -ne 1 ]; then
        printf 'release graph must contain one hidden-draft create, one upload, and one publish mutation\n'
        return 1
    fi
    if grep -Rqs '^[[:space:]]*gh release create ' "$REPO_ROOT/.github/workflows"/*.yml \
        || grep -Eq '^[[:space:]]+--clobber([[:space:]]|$)' "$RELEASE_WORKFLOW"; then
        printf 'release graph contains a second create path or overwrite-capable upload\n'
        return 1
    fi
}

installer_and_updater_verify_checksums() {
    local allowed_signer
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
    for required in \
        'require_release_boolean immutable true' \
        'require_release_boolean draft false' \
        'require_release_boolean prerelease false' \
        'Release metadata author must be github-actions[bot] from the protected publisher' \
        'SHA256SUMS.sig' \
        'ssh-keygen -Y verify' \
        '-n arc-release-manifest-v1' \
        'Release SHA256SUMS signature is invalid or not owner-authorized' \
        "--proto '=https'" \
        "--proto-redir '=https'"
    do
        grep -Fq -- "$required" "$INSTALLER" || {
            printf 'installer/update path lacks immutable HTTPS release binding: %s\n' "$required"
            return 1
        }
    done
    [ -s "$RELEASE_ALLOWED_SIGNERS" ] || {
        printf 'checked-in release allowed-signers file is missing\n'
        return 1
    }
    allowed_signer="$(cat "$RELEASE_ALLOWED_SIGNERS")"
    [ "$(grep -Fc -- "$allowed_signer" "$INSTALLER")" -eq 1 ] || {
        printf 'installer signer line differs from the checked-in allowed-signers contract\n'
        return 1
    }
    for header in \
        '# ARC release manifest v1' \
        '# repository=FerrumVir/arc-chain' \
        '# tag=$RESOLVED_TAG' \
        '# commit=$RESOLVED_COMMIT'
    do
        grep -Fq -- "$header" "$INSTALLER" || {
            printf 'installer does not enforce signed manifest header: %s\n' "$header"
            return 1
        }
    done
}

release_manifest_has_owner_signature_and_preflight() {
    local allowed_signer file
    allowed_signer="$(cat "$RELEASE_ALLOWED_SIGNERS")"
    for file in "$RELEASE_WORKFLOW" "$RELEASE_PREFLIGHT_WORKFLOW"; do
        [ -f "$file" ] || {
            printf 'release signing workflow is missing: %s\n' "$file"
            return 1
        }
        for required in \
            'ARC_RELEASE_MANIFEST_PRIVATE_KEY' \
            '-Y sign' \
            '-Y verify' \
            '-I arc-release' \
            '-n arc-release-manifest-v1' \
            'release/arc-release-allowed-signers'
        do
            grep -Fq -- "$required" "$file" || {
                printf 'release signing workflow %s omits: %s\n' "$file" "$required"
                return 1
            }
        done
        [ "$(grep -Fc -- "$allowed_signer" "$file")" -eq 1 ] || {
            printf 'release signing workflow %s differs from the checked-in manifest trust root\n' "$file"
            return 1
        }
    done
    for required in \
        'RELEASE_COMMIT: ${{ needs.validate.outputs.sha }}' \
        'release-files/SHA256SUMS.sig' \
        'gh release upload "$RELEASE_TAG" release-files/*' \
        'python3 scripts/release/verify-github-release.py'
    do
        grep -Fq -- "$required" "$RELEASE_WORKFLOW" || {
            printf 'release publisher omits signed manifest contract: %s\n' "$required"
            return 1
        }
    done
    for required in \
        'environment: release' \
        'ref: ${{ github.sha }}' \
        'TAURI_SIGNING_PRIVATE_KEY' \
        'TAURI_SIGNING_PRIVATE_KEY_PASSWORD: ""' \
        'signer sign' \
        'tauri-updater-verifier' \
        'ARC_SIGNING_BACKUP_PASSPHRASE' \
        'gh run download "$backup_run_id"' \
        'Restore only the four bounded members and exercise both keys' \
        'Verify the updater canary only after all recovered secrets are gone' \
        'No repository script, package lifecycle, compiler, or source-generated' \
        'sha: ${{ steps.candidate.outputs.sha }}' \
        'name: seal exact-main pre-tag evidence' \
        'name: Package a hash-bound validator-staging artifact' \
        'name: Package the exact signed desktop candidate' \
        'platform: linux-x86_64' \
        'platform: linux-arm64' \
        'platform: macos-arm64' \
        'platform: macos-x86_64' \
        'platform: windows-x86_64' \
        'python3 scripts/release/package-pretag-artifact.py' \
        'name: Seal all nine immutable artifact IDs and digests' \
        'python3 scripts/release/select-pretag-artifacts.py' \
        'retention-days: 30' \
        'overwrite: false'
    do
        grep -Fq -- "$required" "$RELEASE_PREFLIGHT_WORKFLOW" || {
            printf 'pre-tag signing canary omits: %s\n' "$required"
            return 1
        }
    done
    [ "$(grep -Fc 'TAURI_SIGNING_PRIVATE_KEY_PASSWORD: ""' \
        "$RELEASE_PREFLIGHT_WORKFLOW")" -eq 2 ] || {
        printf 'pre-tag canary and sole desktop bundle build must explicitly provide the empty retained-key password\n'
        return 1
    }
    if grep -Fq 'TAURI_SIGNING_PRIVATE_KEY' "$RELEASE_WORKFLOW"; then
        printf 'tag workflow must consume pre-signed bytes instead of exposing the updater key again\n'
        return 1
    fi
    [ "$(grep -Fc '    needs: validate' "$RELEASE_PREFLIGHT_WORKFLOW")" -eq 5 ] || {
        printf 'every pre-tag evidence job must consume the one sealed protected-main SHA\n'
        return 1
    }
}

pretag_exact_byte_handoff_is_fail_closed() {
    local block count desktop_preflight headless_preflight publish_block
    headless_preflight="$(awk '
        /^  headless-runtime:/ { capture=1 }
        capture { print }
        capture && /^  desktop-unsigned:/ { exit }
    ' "$RELEASE_PREFLIGHT_WORKFLOW")"
    desktop_preflight="$(awk '
        /^  desktop-bundle:/ { capture=1 }
        capture { print }
        capture && /^  seal:/ { exit }
    ' "$RELEASE_PREFLIGHT_WORKFLOW")"
    publish_block="$(awk '
        /^  publish:/ { capture=1 }
        capture { print }
    ' "$RELEASE_WORKFLOW")"

    [ "$(printf '%s\n' "$headless_preflight" \
        | grep -Ec '^[[:space:]]+- platform:' || true)" -eq 5 ] || {
        printf 'pre-tag candidate omits one of five headless release groups\n'
        return 1
    }
    [ "$(printf '%s\n' "$desktop_preflight" \
        | grep -Ec '^[[:space:]]+- platform:' || true)" -eq 4 ] || {
        printf 'pre-tag candidate omits one of four desktop release groups\n'
        return 1
    }
    for required in \
        'package-pretag-artifact.py' \
        'steps.package.outputs.artifact_name' \
        'steps.upload.outputs.artifact-id' \
        'steps.upload.outputs.artifact-digest' \
        'retention-days: 30' \
        'compression-level: 0' \
        'overwrite: false'
    do
        grep -Fq -- "$required" "$RELEASE_PREFLIGHT_WORKFLOW" || {
            printf 'pre-tag artifact production omits: %s\n' "$required"
            return 1
        }
    done
    for required in \
        'pretag_artifacts: ${{ steps.release.outputs.pretag_artifacts }}' \
        'pretag_artifact_ids: ${{ steps.release.outputs.pretag_artifact_ids }}' \
        'pretag_run_id: ${{ steps.release.outputs.pretag_run_id }}' \
        'pretag_run_attempt: ${{ steps.release.outputs.pretag_run_attempt }}' \
        '--expected-artifacts-json "$EXPECTED_ARTIFACTS_JSON"' \
        'artifact-ids: ${{ needs.validate.outputs.pretag_artifact_ids }}' \
        'Verify and materialize every exact preflight artifact'
    do
        grep -Fq -- "$required" \
            "$RELEASE_WORKFLOW" "$PRETAG_LIVE_VERIFY" || {
            printf 'tag workflow exact-byte handoff omits: %s\n' "$required"
            return 1
        }
    done

    count="$(grep -Fc \
        'actions/download-artifact@3e5f45b2cfb9172054b4087a40e8e0b5a5461e7c' \
        "$RELEASE_WORKFLOW")"
    [ "$count" -eq 12 ] || {
        printf 'tag workflow must contain four pre-tag plus eight isolated recovery/signing/publication evidence downloads; found %s\n' "$count"
        return 1
    }
    [ "$(grep -Fc 'skip-decompress: true' "$RELEASE_WORKFLOW")" -eq 5 ] || {
        printf 'all four raw pre-tag downloads plus the cutover handoff must preserve their server ZIPs\n'
        return 1
    }
    [ "$(grep -Fc 'digest-mismatch: error' "$RELEASE_WORKFLOW")" -eq 12 ] || {
        printf 'all twelve exact-ID downloads must fail on a server digest mismatch\n'
        return 1
    }
    [ "$(grep -Fc 'Revalidate the selected run, attempt, IDs, and digests' \
        "$RELEASE_WORKFLOW")" -eq 4 ] || {
        printf 'every cross-run artifact download needs an adjacent live run/attempt/ID check\n'
        return 1
    }
    [ "$(grep -Fc 'actions/upload-artifact@043fb46d1a93c77aae656e7c1c64a875d1fc6a0a' \
        "$RELEASE_WORKFLOW")" -eq 4 ] || {
        printf 'tag workflow must upload exactly unsigned, sealed, draft-evidence, and published-evidence handoffs\n'
        return 1
    }
    for handoff_path in \
        'unsigned-release-handoff/*' \
        'sealed-release-handoff/*' \
        'draft-evidence/release-draft.json' \
        'published-evidence/release-published.json'
    do
        grep -Fq -- "          path: $handoff_path" "$RELEASE_WORKFLOW" || {
            printf 'release workflow does not upload the expected handoff path: %s\n' "$handoff_path"
            return 1
        }
    done

    for job in headless linux-server-compat desktop; do
        block="$(awk -v job="  $job:" '
            $0 == job { capture=1 }
            capture { print }
            capture && $0 ~ /^  [a-zA-Z0-9_-]+:$/ && $0 != job { exit }
        ' "$RELEASE_WORKFLOW")"
        for permission in 'actions: read' 'contents: read'; do
            printf '%s\n' "$block" | grep -Fq "$permission" || {
                printf '%s download job lacks explicit %s permission\n' \
                    "$job" "$permission"
                return 1
            }
        done
    done
    for permission in 'actions: read' 'contents: write'; do
        printf '%s\n' "$publish_block" | grep -Fq "$permission" || {
            printf 'publisher lacks minimal required permission: %s\n' "$permission"
            return 1
        }
    done
    if printf '%s\n' "$publish_block" | grep -Eq 'packages: write|pull-requests: write|id-token: write'; then
        printf 'publisher has unrelated write permissions\n'
        return 1
    fi

    for required in \
        'downloaded Actions ZIP does not match the selected artifact.digest' \
        'info.flag_bits & 0x1' \
        'Actions ZIP contains duplicate entry' \
        'Actions ZIP membership differs' \
        'Actions ZIP exceeds the allowed expansion bound' \
        'archive contains an unsafe path' \
        'archive contains a non-regular member' \
        'archive contains duplicate member' \
        'inner candidate archive exceeds the allowed expansion bound' \
        'payload hash mismatch'
    do
        grep -Fq -- "$required" "$PRETAG_MATERIALIZER" || {
            printf 'exact-byte materializer omits adversarial check: %s\n' "$required"
            return 1
        }
    done
    for required in \
        'EXPECTED_GROUPS' \
        'artifact ID' \
        'expired or has unknown expiry state' \
        'server SHA-256 digest' \
        'unexpected current-attempt pre-tag artifacts' \
        'differ from the validated selection'
    do
        grep -Fq -- "$required" "$PRETAG_SELECTOR" || {
            printf 'pre-tag selector omits fail-closed check: %s\n' "$required"
            return 1
        }
    done
}

upload_artifact_digests_are_canonicalized_at_job_boundaries() {
    # The exact pinned actions/upload-artifact implementation exposes its
    # artifact-digest output as 64 bare lowercase hex characters. GitHub's
    # artifact REST objects expose the same value as sha256:<hex>. Keep the
    # repository-facing job-output contract in the REST form, exactly once at
    # each producer boundary, so downstream validators never receive a bare or
    # double-prefixed digest.
    local upload_pin evidence_boundary evidence_boundary_count
    upload_pin='actions/upload-artifact@043fb46d1a93c77aae656e7c1c64a875d1fc6a0a'
    evidence_boundary="evidence_digest: \${{ format('sha256:{0}', steps.evidence.outputs.artifact-digest) }}"
    for workflow in \
        "$RECOVERY_RELEASE_HANDOFF_WORKFLOW" \
        "$RELEASE_PREFLIGHT_WORKFLOW" \
        "$RELEASE_WORKFLOW"
    do
        grep -Fq "$upload_pin" "$workflow" || {
            printf 'digest contract is not tied to the reviewed upload-artifact implementation: %s\n' \
                "$workflow"
            return 1
        }
    done

    grep -Fq \
        "artifact-digest: \${{ format('sha256:{0}', steps.upload.outputs.artifact-digest) }}" \
        "$RECOVERY_RELEASE_HANDOFF_WORKFLOW" || {
        printf 'recovery handoff does not canonicalize its job digest output\n'
        return 1
    }
    for required in \
        "artifact_digest: \${{ format('sha256:{0}', steps.upload.outputs.artifact-digest) }}" \
        '[[ "$EXPECTED_ARTIFACT_DIGEST" =~ ^sha256:[0-9a-f]{64}$ ]]'
    do
        grep -Fq -- "$required" "$RELEASE_PREFLIGHT_WORKFLOW" || {
            printf 'desktop signer handoff digest boundary omits: %s\n' "$required"
            return 1
        }
    done
    for required in \
        "artifact_digest: \${{ format('sha256:{0}', steps.unsigned-upload.outputs.artifact-digest) }}" \
        "artifact_digest: \${{ format('sha256:{0}', steps.sealed-upload.outputs.artifact-digest) }}" \
        "evidence_digest: \${{ format('sha256:{0}', steps.evidence.outputs.artifact-digest) }}"
    do
        grep -Fq -- "$required" "$RELEASE_WORKFLOW" || {
            printf 'release handoff digest boundary omits: %s\n' "$required"
            return 1
        }
    done
    evidence_boundary_count="$(grep -Fc "$evidence_boundary" "$RELEASE_WORKFLOW")"
    [ "$evidence_boundary_count" -eq 2 ] || {
        printf 'both draft and published evidence outputs must canonicalize exactly once\n'
        return 1
    }
    for required in \
        '[[ "$ARTIFACT_DIGEST" =~ ^[0-9a-f]{64}$ ]]' \
        '--arg digest "sha256:$ARTIFACT_DIGEST"'
    do
        grep -Fq -- "$required" "$RECOVERY_RELEASE_HANDOFF_WORKFLOW" || {
            printf 'same-step recovery artifact/API digest comparison omits: %s\n' "$required"
            return 1
        }
    done
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
    if grep -Eq '(^|[[:space:]])-r[[:space:]]+0-0|/releases/latest' "$INSTALLER"; then
        printf 'installer still trusts global latest or probes assets with Range requests\n'
        return 1
    fi
    [ "$(grep -Fc 'releases?per_page=100' "$INSTALLER")" -eq 1 ] || {
        printf 'installer does not use one bounded v0.8 channel-discovery page\n'
        return 1
    }
    grep -Fq 'RELEASE_METADATA_URL="$API_ROOT/releases/tags/v$REQUESTED_VERSION"' \
        "$INSTALLER" || {
        printf 'channel discovery does not converge on one exact-tag metadata object\n'
        return 1
    }
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
    if ! grep -Fq '"$ARC_DIR/bin/arc-cli"' "$INSTALLER" \
        || ! grep -Fq -- '--rpc "http://127.0.0.1:$RPC_PORT" health' "$INSTALLER"; then
        printf 'installer health gate does not use the JSON-validating released CLI on loopback\n'
        return 1
    fi
}

release_genesis_is_validated_before_packaging() {
    grep -Fq 'validate-genesis.py' "$ASSEMBLER" || {
        printf 'release assembler never invokes the fail-closed genesis validator\n'
        return 1
    }
    # shellcheck disable=SC2016 # Intentional literal contract in the assembler.
    grep -Fq 'python3 "$GENESIS_VALIDATOR" "$GENESIS_FILE"' "$ASSEMBLER" || {
        printf 'release assembler does not validate the exact genesis file it packages\n'
        return 1
    }
    python3 "$GENESIS_VALIDATOR" "$REPO_ROOT/genesis.toml" >/dev/null || {
        printf 'canonical release genesis does not satisfy the release safety contract\n'
        return 1
    }
}

legacy_installer_cannot_create_a_validator_identity() {
    if grep -Eq -- '(^|[[:space:]])--stake[[:space:]]+[1-9]|stake[[:space:]]*=[[:space:]]*[1-9]' \
        "$LEGACY_INSTALLER"; then
        printf 'legacy installer can still configure non-zero validator stake\n'
        return 1
    fi
    if grep -Eq -- '--validator-seed|--validator-key-file|insecure-dev-validator-seed|openssl[[:space:]]+(genpkey|rand)' \
        "$LEGACY_INSTALLER"; then
        printf 'legacy installer can still generate or pass production validator identity material\n'
        return 1
    fi
    # shellcheck disable=SC2016 # Intentional literal contract in the wrapper.
    grep -Fq 'exec bash "$SCRIPT_DIR/../install.sh" "$@"' "$LEGACY_INSTALLER" || {
        printf 'repository-local legacy path does not delegate to root install.sh\n'
        return 1
    }
    if grep -Eq 'raw[.]githubusercontent[.]com|curl[[:space:]].*install[.]sh' \
        "$LEGACY_INSTALLER"; then
        printf 'legacy installer still downloads executable code from a mutable branch\n'
        return 1
    fi
    grep -Fq 'Refusing to download or execute an installer from a mutable branch.' \
        "$LEGACY_INSTALLER" || {
        printf 'legacy installer does not fail closed when local install.sh is absent\n'
        return 1
    }
}

community_join_entrypoints_are_stake_zero_wrappers() {
    local script
    for script in "$COMMUNITY_JOIN" "$INFERENCE_JOIN" "$INFERENCE_INSTALL"; do
        grep -Fq 'install.sh' "$script" || {
            printf '%s does not delegate to the supported installer\n' "$script"
            return 1
        }
        if grep -Eq -- '--stake[[:space:]]+[1-9]|--validator-seed|releases/latest/download|huggingface\.co' "$script"; then
            printf '%s retains an unsafe stake, seed argv, moving binary, or unchecked model download\n' "$script"
            return 1
        fi
    done
    grep -Fq -- '--system-service' "$INFERENCE_INSTALL" || {
        printf 'legacy system-service wrapper does not preserve its documented service scope\n'
        return 1
    }
    grep -Fq 'A verified local GGUF is required' "$INFERENCE_JOIN" || {
        printf 'inference join does not fail closed without an operator-verified model\n'
        return 1
    }
}

secret_scan_is_pinned_to_the_current_tree() {
    grep -Fq 'secret-scan:' "$CI_WORKFLOW" || {
        printf 'blocking CI workflow has no current-tree secret scan job\n'
        return 1
    }
    grep -Fq 'bash tests/release/current_tree_secret_scan.sh' "$CI_WORKFLOW" || {
        printf 'CI secret scan does not run the reviewed contract script\n'
        return 1
    }
    grep -Fq 'GITLEAKS_VERSION=8.30.1' "$SECRET_SCANNER" || {
        printf 'Gitleaks binary version is not pinned\n'
        return 1
    }
    grep -Fq 'ARCHIVE_SHA256=' "$SECRET_SCANNER" || {
        printf 'Gitleaks download is not bound to a checksum\n'
        return 1
    }
    grep -Fq 'current_tree_secret_scan.sh --worktree' "$QUALITY_HARNESS" || {
        printf 'local full gate does not scan releasable working-tree changes\n'
        return 1
    }
    grep -Fq 'materialize_releasable_tree.py' "$SECRET_SCANNER" || {
        printf 'worktree secret scan does not use the reviewed tree materializer\n'
        return 1
    }
    grep -Fq 'for tree_mode in index worktree' "$SECRET_SCANNER" || {
        printf 'local secret scan does not inspect both staged and working-copy bytes\n'
        return 1
    }
    grep -Fq '"ls-files", "--stage", "-z"' "$SECRET_MATERIALIZER" || {
        printf 'index secret scan does not enumerate exact staged Git blobs\n'
        return 1
    }
    grep -Fq '"--cached",' "$SECRET_MATERIALIZER" || {
        printf 'worktree secret scan does not enumerate tracked Git files\n'
        return 1
    }
    # shellcheck disable=SC2016 # Intentional literal contract in the scanner.
    grep -Fq 'git -C "$REPO_ROOT" archive --format=tar HEAD' "$SECRET_SCANNER" || {
        printf 'secret scanner does not materialize exactly the current commit tree\n'
        return 1
    }
    # shellcheck disable=SC2016 # Intentional literal contract in the scanner.
    grep -Fq '"$GITLEAKS_BIN" dir' "$SECRET_SCANNER" || {
        printf 'secret scanner is not using Gitleaks directory mode\n'
        return 1
    }
    if grep -Eq 'fetch-depth:[[:space:]]*0|gitleaks([^[:alnum:]]|.*[[:space:]])git([[:space:]]|$)' \
        "$CI_WORKFLOW" "$SECRET_SCANNER"; then
        printf 'secret gate scans compromised Git history instead of the current tree\n'
        return 1
    fi
}

linux_compat_smoke_executes_real_headless_node() {
    local smoke_block preflight_block docker_run_count
    smoke_block="$(awk '
        /^  linux-server-compat:/ { capture=1 }
        capture { print }
        capture && /^  desktop:/ { exit }
    ' "$RELEASE_WORKFLOW")"
    preflight_block="$(awk '
        /^  headless-runtime:/ { capture=1 }
        capture { print }
        capture && /^  desktop-unsigned:/ { exit }
    ' "$RELEASE_PREFLIGHT_WORKFLOW")"

    printf '%s\n' "$preflight_block" | grep -A4 -F -- '- platform: linux-x86_64' \
        | grep -Fq 'runner: ubuntu-22.04' || {
        printf 'linux-x86_64 pre-tag bytes are not built on the Ubuntu 22 ABI baseline\n'
        return 1
    }

    docker_run_count="$(printf '%s\n' "$smoke_block" | grep -Ec '^[[:space:]]+docker run --rm[[:space:]]*\\$' || true)"
    if [ "$docker_run_count" -ne 2 ]; then
        printf 'Ubuntu compatibility loop must contain one version invocation and one runtime invocation; found %s\n' "$docker_run_count"
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
    for required in \
        'ref: ${{ needs.validate.outputs.sha }}' \
        "ubuntu_versions: '22.04 24.04 26.04'" \
        "ubuntu_versions: '24.04 26.04'" \
        'UBUNTU_VERSIONS: ${{ matrix.ubuntu_versions }}' \
        'for ubuntu in $UBUNTU_VERSIONS' \
        'arc-node-${{ matrix.platform }}' \
        'arc-cli-${{ matrix.platform }}' \
        '--env DISPLAY=' \
        '--genesis /config/genesis.toml' \
        '--no-community' \
        'GET /health HTTP/1.1' \
        'grep -q "HTTP/1.1 200"' \
        '"status":"(ok|degraded)"'
    do
        printf '%s\n' "$smoke_block" | grep -Fq -- "$required" || {
            printf 'Ubuntu compatibility smoke is missing: %s\n' "$required"
            return 1
        }
    done
}

release_actions_are_exact_sha_allowlisted() {
    local actual expected ref
    expected="$(printf '%s\n' \
        'Swatinem/rust-cache@49a0bdc70d2e1b713ca9e2869b211fcce03d3c1c' \
        'actions/checkout@3d3c42e5aac5ba805825da76410c181273ba90b1' \
        'actions/download-artifact@3e5f45b2cfb9172054b4087a40e8e0b5a5461e7c' \
        'actions/setup-node@820762786026740c76f36085b0efc47a31fe5020' \
        'actions/upload-artifact@043fb46d1a93c77aae656e7c1c64a875d1fc6a0a' \
        'dtolnay/rust-toolchain@4360b52568e2003a75bf9bc1d59f33a8e3fc893c' \
        'dtolnay/rust-toolchain@6c977a6ca4077a0ceb28ffbe03f59d46e9ac8772' \
        | LC_ALL=C sort)"
    actual="$(awk '
        /^[[:space:]]*(-[[:space:]]+)?uses:/ {
            for (field = 1; field <= NF; field++) {
                if ($field == "uses:") print $(field + 1)
            }
        }
    ' "$RELEASE_WORKFLOW" | LC_ALL=C sort -u)"
    if [ "$actual" != "$expected" ]; then
        printf 'release action allowlist changed or contains an unreviewed ref\nexpected:\n%s\nactual:\n%s\n' \
            "$expected" "$actual"
        return 1
    fi

    while IFS= read -r ref; do
        if [[ ! "$ref" =~ @[0-9a-f]{40}$ ]]; then
            printf 'release action is not pinned to a full commit SHA: %s\n' "$ref"
            return 1
        fi
    done <<< "$actual"
}

github_owned_actions_are_node24_exact_sha_allowlisted() {
    local actual expected
    expected="$(printf '%s\n' \
        'actions/checkout@3d3c42e5aac5ba805825da76410c181273ba90b1' \
        'actions/configure-pages@45bfe0192ca1faeb007ade9deae92b16b8254a0d' \
        'actions/deploy-pages@cd2ce8fcbc39b97be8ca5fce6e763baed58fa128' \
        'actions/download-artifact@3e5f45b2cfb9172054b4087a40e8e0b5a5461e7c' \
        'actions/setup-node@820762786026740c76f36085b0efc47a31fe5020' \
        'actions/setup-python@ece7cb06caefa5fff74198d8649806c4678c61a1' \
        'actions/upload-artifact@043fb46d1a93c77aae656e7c1c64a875d1fc6a0a' \
        'actions/upload-pages-artifact@fc324d3547104276b827a68afc52ff2a11cc49c9' \
        | LC_ALL=C sort)"
    actual="$(find "$REPO_ROOT/.github/workflows" -type f -name '*.yml' -print0 \
        | LC_ALL=C sort -z \
        | xargs -0 sed -n 's/^[[:space:]]*-\{0,1\}[[:space:]]*uses:[[:space:]]*\(actions\/[^#[:space:]]*@[0-9a-f]*\).*/\1/p' \
        | LC_ALL=C sort -u)"
    if [ "$actual" != "$expected" ]; then
        printf 'GitHub-owned action allowlist changed or contains a non-Node-24 pin\nexpected:\n%s\nactual:\n%s\n' \
            "$expected" "$actual"
        return 1
    fi
}

release_supply_chain_and_npm_audits_are_blocking() {
    local actual_dependabot_directories ci_audit_block expected_npm_directories
    local manifest_count quality_block supply_block
    ci_audit_block="$(awk '
        /^  audit:/ { capture=1 }
        capture { print }
        capture && /^  shell-syntax:/ { exit }
    ' "$CI_WORKFLOW")"
    quality_block="$(awk '
        /^  release-quality:/ { capture=1 }
        capture { print }
        capture && /^  release-supply-chain:/ { exit }
    ' "$RELEASE_WORKFLOW")"
    supply_block="$(awk '
        /^  release-supply-chain:/ { capture=1 }
        capture { print }
        capture && /^  cross-arch-golden-vectors:/ { exit }
    ' "$RELEASE_WORKFLOW")"

    for required in \
        'needs: validate' \
        'ref: ${{ needs.validate.outputs.sha }}' \
        'bash scripts/ci/run-cargo-deny.sh "${{ matrix.manifest }}"'
    do
        printf '%s\n' "$supply_block" | grep -Fq -- "$required" || {
            printf 'blocking release dependency-policy gate is missing: %s\n' "$required"
            return 1
        }
    done
    if printf '%s\n' "$supply_block" | grep -Eq 'continue-on-error:[[:space:]]*true'; then
        printf 'release cargo-deny gate is still advisory\n'
        return 1
    fi
    manifest_count="$(printf '%s\n' "$supply_block" \
        | grep -Ec '^[[:space:]]+manifest:[[:space:]]' || true)"
    if [ "$manifest_count" -ne 3 ]; then
        printf 'release cargo-deny matrix must cover exactly three locked Rust graphs; found %s\n' \
            "$manifest_count"
        return 1
    fi
    for manifest in \
        'manifest: Cargo.toml' \
        'manifest: desktop/src-tauri/Cargo.toml' \
        'manifest: tests/release/tauri-updater-verifier/Cargo.toml'
    do
        printf '%s\n' "$supply_block" | grep -Fq -- "$manifest" || {
            printf 'release cargo-deny matrix omits shipped/release-tool graph: %s\n' "$manifest"
            return 1
        }
    done

    for required in \
        'set -euo pipefail' \
        'Cargo.toml' \
        'desktop/src-tauri/Cargo.toml' \
        'tests/release/tauri-updater-verifier/Cargo.toml' \
        'bash scripts/ci/run-cargo-deny.sh "$manifest"'
    do
        printf '%s\n' "$ci_audit_block" | grep -Fq -- "$required" || {
            printf 'pull-request dependency-policy gate is missing: %s\n' "$required"
            return 1
        }
    done
    if printf '%s\n' "$ci_audit_block" | grep -Eq 'continue-on-error:[[:space:]]*true'; then
        printf 'pull-request cargo-deny gate is advisory\n'
        return 1
    fi

    for required in \
        'SHADOW_HELPER="$SCRIPT_DIR/vendored-advisory-shadow.py"' \
        'SHADOW_PROFILE=root' \
        'SHADOW_PROFILE=desktop' \
        'python3 "$SHADOW_HELPER" write-policy' \
        '--config "$SHADOW_POLICY"' \
        '--metadata-path "$SHADOW_METADATA"' \
        'check --audit-compatible-output advisories' \
        'python3 "$SHADOW_HELPER" verify-report'
    do
        if [ "$(grep -Fc -- "$required" "$CARGO_DENY_RUNNER")" -ne 1 ]; then
            printf 'vendored advisory registry-shadow wiring drifted: %s\n' "$required"
            return 1
        fi
    done
    if grep -Fq -- 'vendored-glib-advisory-shadow.py' "$CARGO_DENY_RUNNER"; then
        printf 'cargo-deny runner retained the partial glib-only advisory helper\n'
        return 1
    fi
    for required in \
        'write_shadow_policy' \
        'ignore = []' \
        'registry-shadow advisory policy must be suppression-free' \
        'root vendored packages have live advisory findings'
    do
        grep -Fq -- "$required" "$VENDORED_ADVISORY_SHADOW" || {
            printf 'vendored advisory helper omits fail-closed invariant: %s\n' "$required"
            return 1
        }
    done

    printf '%s\n' "$quality_block" \
        | grep -Fq 'npm --prefix "$package" audit --package-lock-only --audit-level=low' \
        || {
            printf 'release-quality does not run the shared blocking npm audit loop\n'
            return 1
        }
    printf '%s\n' "$quality_block" \
        | grep -Fq 'for package in dashboard desktop sdk sdk/typescript sdks/typescript; do' || {
            printf 'release-quality npm audit loop does not enumerate all five lockfiles\n'
            return 1
        }

    expected_npm_directories="$(printf '%s\n' \
        /dashboard \
        /desktop \
        /sdk \
        /sdk/typescript \
        /sdks/typescript \
        | LC_ALL=C sort)"
    actual_dependabot_directories="$(awk '
        /package-ecosystem:[[:space:]]*npm/ { npm_entry=1; next }
        npm_entry && /directory:/ {
            sub(/^.*directory:[[:space:]]*/, "")
            gsub(/["'\''[:space:]]/, "")
            print
            npm_entry=0
        }
    ' "$DEPENDABOT_CONFIG" | LC_ALL=C sort)"
    if [ "$actual_dependabot_directories" != "$expected_npm_directories" ]; then
        printf 'Dependabot npm coverage does not match the five tracked lockfile directories\nexpected:\n%s\nactual:\n%s\n' \
            "$expected_npm_directories" "$actual_dependabot_directories"
        return 1
    fi
}

cross_arch_golden_vectors_gate_publication() {
    local golden_block platform_count
    golden_block="$(awk '
        /^  cross-arch-golden-vectors:/ { capture=1 }
        capture { print }
        capture && /^  headless:/ { exit }
    ' "$RELEASE_WORKFLOW")"

    platform_count="$(printf '%s\n' "$golden_block" \
        | grep -Ec '^[[:space:]]*- platform:' || true)"
    if [ "$platform_count" -ne 5 ]; then
        printf 'release golden-vector matrix must contain exactly five platform legs; found %s\n' \
            "$platform_count"
        return 1
    fi
    for required in \
        'needs: validate' \
        'ref: ${{ needs.validate.outputs.sha }}' \
        'platform: linux-x86_64' \
        'runner: ubuntu-24.04' \
        'platform: linux-arm64' \
        'runner: ubuntu-24.04-arm' \
        'platform: macos-arm64' \
        'runner: macos-15' \
        'platform: macos-x86_64' \
        'runner: macos-15-intel' \
        'platform: windows-x86_64' \
        'runner: windows-latest' \
        'dtolnay/rust-toolchain@6c977a6ca4077a0ceb28ffbe03f59d46e9ac8772' \
        'toolchain: nightly-2026-03-16' \
        'cargo test -p arc-inference --lib --locked golden -- --nocapture'
    do
        printf '%s\n' "$golden_block" | grep -Fq -- "$required" || {
            printf 'cross-architecture release golden-vector gate is missing: %s\n' "$required"
            return 1
        }
    done
}

branch_golden_vectors_prove_manifest_verification_on_every_os() {
    for required in \
        'os: [ubuntu-latest, macos-15, macos-15-intel, windows-latest]' \
        'Prove namespaced manifest verification on every installer OS' \
        'ssh-keygen -Y sign' \
        'ssh-keygen -Y verify' \
        '-n arc-release-manifest-v1' \
        'if [ "$RUNNER_OS" = Windows ]; then' \
        'cmd.exe /d /c "ssh-keygen -Y verify'
    do
        grep -Fq -- "$required" "$GOLDEN_WORKFLOW" || {
            printf 'branch golden-vector workflow omits installer signature portability proof: %s\n' "$required"
            return 1
        }
    done
}

cross_os_workspace_tests_are_blocking() {
    local test_block
    test_block="$(awk '
        /^  test:/ { capture=1 }
        capture { print }
        capture && /^  integration:/ { exit }
    ' "$CI_WORKFLOW")"
    for required in \
        'os: [ubuntu-latest, macos-15, windows-latest]' \
        'cargo check --workspace --all-targets --locked' \
        'cargo test --workspace --lib --locked'
    do
        printf '%s\n' "$test_block" | grep -Fq -- "$required" || {
            printf 'cross-OS workspace test gate omits: %s\n' "$required"
            return 1
        }
    done
    if printf '%s\n' "$test_block" | grep -Fq 'continue-on-error'; then
        printf 'Mac/Windows workspace tests are still optional\n'
        return 1
    fi
}

nondefault_release_features_have_distinct_blocking_statuses() {
    local benchmark_block candle_block checkout_count credential_count
    candle_block="$(awk '
        /^  release-candle:/ { capture=1 }
        capture { print }
        capture && /^  benchmark-tools:/ { exit }
    ' "$CI_WORKFLOW")"
    benchmark_block="$(awk '
        /^  benchmark-tools:/ { capture=1 }
        capture { print }
        capture && /^  test:/ { exit }
    ' "$CI_WORKFLOW")"

    for required in \
        'name: release Candle feature' \
        'runs-on: ubuntu-24.04' \
        'toolchain: nightly-2026-03-16' \
        'cargo clippy -p arc-inference -p arc-node --all-targets --features candle --locked -- -D warnings' \
        'cargo test -p arc-inference -p arc-node --lib --features candle --locked'
    do
        printf '%s\n' "$candle_block" | grep -Fq -- "$required" || {
            printf 'distinct release Candle status omits: %s\n' "$required"
            return 1
        }
    done
    for required in \
        'name: benchmark-tools feature' \
        'runs-on: ubuntu-24.04' \
        'toolchain: nightly-2026-03-16' \
        'cargo clippy -p arc-crypto -p arc-state -p arc-node -p arc-bench --all-targets --features benchmark-tools --locked -- -D warnings' \
        'cargo check -p arc-crypto -p arc-state -p arc-node -p arc-bench --all-targets --features benchmark-tools --locked' \
        'cargo test -p arc-crypto -p arc-state -p arc-node -p arc-bench --lib --bins --tests --features benchmark-tools --locked'
    do
        printf '%s\n' "$benchmark_block" | grep -Fq -- "$required" || {
            printf 'distinct benchmark-tools status omits: %s\n' "$required"
            return 1
        }
    done
    if printf '%s\n%s\n' "$candle_block" "$benchmark_block" \
        | grep -Eq 'continue-on-error:[[:space:]]*true'; then
        printf 'a nondefault production/security feature status is advisory\n'
        return 1
    fi

    for required in \
        'cargo clippy -p arc-inference -p arc-node --all-targets --features candle --locked -- -D warnings' \
        'cargo test -p arc-inference -p arc-node --lib --features candle --locked' \
        'cargo clippy -p arc-crypto -p arc-state -p arc-node -p arc-bench --all-targets --features benchmark-tools --locked -- -D warnings' \
        'cargo check -p arc-crypto -p arc-state -p arc-node -p arc-bench --all-targets --features benchmark-tools --locked' \
        'cargo test -p arc-crypto -p arc-state -p arc-node -p arc-bench --lib --bins --tests --features benchmark-tools --locked'
    do
        grep -Fq -- "$required" "$QUALITY_HARNESS" || {
            printf 'local full gate does not mirror nondefault CI feature command: %s\n' "$required"
            return 1
        }
    done

    grep -Eq '^[[:space:]]*contents:[[:space:]]+read$' "$CI_WORKFLOW" || {
        printf 'CI does not declare a read-only default token\n'
        return 1
    }
    checkout_count="$(grep -Fc \
        'uses: actions/checkout@3d3c42e5aac5ba805825da76410c181273ba90b1' \
        "$CI_WORKFLOW")"
    credential_count="$(grep -Fc 'persist-credentials: false' "$CI_WORKFLOW")"
    if [ "$checkout_count" -ne "$credential_count" ]; then
        printf 'every CI checkout must discard its persisted token (checkouts=%s hardened=%s)\n' \
            "$checkout_count" "$credential_count"
        return 1
    fi
}

shellcheck_gates_share_the_blocking_warning_policy() {
    local release_contract_block shellcheck_block
    local release_command_count release_warning_count
    local shellcheck_command_count shellcheck_warning_count
    release_contract_block="$(awk '
        /^  release-contract:/ { capture=1 }
        capture { print }
        capture && /^  secret-scan:/ { exit }
    ' "$CI_WORKFLOW")"
    shellcheck_block="$(awk '
        /^  shellcheck:/ { capture=1 }
        capture { print }
        capture && /^  forge:/ { exit }
    ' "$CI_WORKFLOW")"

    release_command_count="$(printf '%s\n' "$release_contract_block" \
        | grep -Ec '^[[:space:]]*shellcheck[[:space:]]' || true)"
    release_warning_count="$(printf '%s\n' "$release_contract_block" \
        | grep -Ec '^[[:space:]]*shellcheck -S warning([[:space:]]|$)' || true)"
    if [ "$release_command_count" -ne 2 ] || [ "$release_warning_count" -ne 2 ]; then
        printf 'release contract must run exactly two warning/error ShellCheck commands (commands=%s warning-policy=%s)\n' \
            "$release_command_count" "$release_warning_count"
        return 1
    fi

    shellcheck_command_count="$(printf '%s\n' "$shellcheck_block" \
        | grep -Ec '^[[:space:]]*shellcheck[[:space:]]' || true)"
    shellcheck_warning_count="$(printf '%s\n' "$shellcheck_block" \
        | grep -Ec '^[[:space:]]*shellcheck -S warning([[:space:]]|$)' || true)"
    if [ "$shellcheck_command_count" -ne 2 ] || [ "$shellcheck_warning_count" -ne 1 ]; then
        printf 'protected ShellCheck job must version-check once and run one warning/error lint command (commands=%s warning-policy=%s)\n' \
            "$shellcheck_command_count" "$shellcheck_warning_count"
        return 1
    fi

    grep -Fq 'shellcheck -S warning "${files[@]}"' "$QUALITY_HARNESS" || {
        printf 'local quality harness does not mirror the blocking warning/error ShellCheck policy\n'
        return 1
    }
    if printf '%s\n%s\n' "$release_contract_block" "$shellcheck_block" \
        | grep -Eq 'continue-on-error:[[:space:]]*true'; then
        printf 'a blocking ShellCheck gate is advisory\n'
        return 1
    fi
}

updater_signatures_are_verified_against_the_embedded_key() {
    local fixture_bin target_dir valid_key wrong_key
    for required in \
        'tests/release/verify_tauri_updater_signatures.sh' \
        'desktop/src-tauri/tauri.conf.json release-files'
    do
        grep -Fq -- "$required" "$RELEASE_WORKFLOW" || {
            printf 'publisher does not invoke updater-key proof: %s\n' "$required"
            return 1
        }
    done
    for required in \
        'arc-desktop-macos-arm64.app.tar.gz.sig' \
        'arc-desktop-macos-x86_64.app.tar.gz.sig' \
        'arc-desktop-windows-x86_64-setup.exe.sig' \
        'arc-desktop-linux-x86_64.AppImage.sig' \
        'SHA256SUMS.sig' \
        'unexpected published signature artifact'
    do
        grep -Fq -- "$required" "$UPDATER_SIGNATURE_GATE" || {
            printf 'updater-key proof omits or confuses signature artifact: %s\n' "$required"
            return 1
        }
    done
    grep -Fq 'expected exactly four signed updater payloads' "$UPDATER_SIGNATURE_GATE" || {
        printf 'updater-key proof does not require the complete four-platform signature set\n'
        return 1
    }
    grep -Fq 'minisign-verify = "=0.2.5"' "$UPDATER_VERIFIER_MANIFEST" || {
        printf 'updater verifier crypto dependency is not exactly pinned\n'
        return 1
    }

    target_dir="$REPO_ROOT/target/release-contract-verifier"
    CARGO_TARGET_DIR="$target_dir" cargo build --quiet --locked \
        --manifest-path "$UPDATER_VERIFIER_MANIFEST" || return 1
    fixture_bin="$target_dir/debug/tauri-updater-verifier"
    valid_key="$(tr -d '\r\n' < "$UPDATER_FIXTURE_DIR/valid.pubkey")"
    wrong_key="$(tr -d '\r\n' < "$UPDATER_FIXTURE_DIR/wrong.pubkey")"
    "$fixture_bin" "$valid_key" "$UPDATER_FIXTURE_DIR/payload.txt" \
        "$UPDATER_FIXTURE_DIR/payload.txt.sig" >/dev/null || {
        printf 'reviewed valid updater signature fixture did not verify\n'
        return 1
    }
    if "$fixture_bin" "$wrong_key" "$UPDATER_FIXTURE_DIR/payload.txt" \
        "$UPDATER_FIXTURE_DIR/payload.txt.sig" >/dev/null 2>&1; then
        printf 'rotated/wrong updater key unexpectedly verified the fixture signature\n'
        return 1
    fi
}

release_secret_jobs_require_the_owner_environment() {
    local desktop_block unsigned_block handoff_block signer_block signing_step
    local assembly_block manifest_block manifest_signing_step publish_draft_block publish_block
    local draft_verify_block published_verify_block backup_readiness_block backup_workflow_block backup_restore_step
    local validate_line rehydrate_line signing_line manifest_sign_line manifest_cleanup_line manifest_verify_line
    desktop_block="$(awk '
        /^  desktop:/ { capture=1 }
        capture { print }
        capture && /^  assemble-release:/ { exit }
    ' "$RELEASE_WORKFLOW")"
    unsigned_block="$(awk '
        /^  desktop-unsigned:/ { capture=1 }
        capture { print }
        capture && /^  desktop-signer-handoff:/ { exit }
    ' "$RELEASE_PREFLIGHT_WORKFLOW")"
    handoff_block="$(awk '
        /^  desktop-signer-handoff:/ { capture=1 }
        capture { print }
        capture && /^  desktop-bundle:/ { exit }
    ' "$RELEASE_PREFLIGHT_WORKFLOW")"
    signer_block="$(awk '
        /^  desktop-bundle:/ { capture=1 }
        capture { print }
        capture && /^  seal:/ { exit }
    ' "$RELEASE_PREFLIGHT_WORKFLOW")"

    # An empty capture means we failed to READ the workflow, not that the job is
    # missing a literal. Under resource pressure a command substitution can fork
    # -fail and return empty, which then reports as "omits <literal>" and sends
    # the reader hunting a non-existent workflow regression. Distinguish them.
    for captured in unsigned_block handoff_block signer_block; do
        eval "captured_value=\${$captured}"
        [ -n "$captured_value" ] || {
            printf 'could not capture %s from %s (empty awk result, not a missing literal)\n' \
                "$captured" "$RELEASE_PREFLIGHT_WORKFLOW"
            return 1
        }
    done
    assembly_block="$(awk '
        /^  assemble-release:/ { capture=1 }
        capture { print }
        capture && /^  manifest-sign:/ { exit }
    ' "$RELEASE_WORKFLOW")"
    manifest_block="$(awk '
        /^  manifest-sign:/ { capture=1 }
        capture { print }
        capture && /^  publish-draft:/ { exit }
    ' "$RELEASE_WORKFLOW")"
    publish_draft_block="$(awk '
        /^  publish-draft:/ { capture=1 }
        capture { print }
        capture && /^  verify-draft-release:/ { exit }
    ' "$RELEASE_WORKFLOW")"
    draft_verify_block="$(awk '
        /^  verify-draft-release:/ { capture=1 }
        capture { print }
        capture && /^  cleanup-rejected-draft:/ { exit }
    ' "$RELEASE_WORKFLOW")"
    publish_block="$(awk '
        /^  publish:/ { capture=1 }
        capture { print }
        capture && /^  verify-published-release:/ { exit }
    ' "$RELEASE_WORKFLOW")"
    published_verify_block="$(awk '
        /^  verify-published-release:/ { capture=1 }
        capture { print }
    ' "$RELEASE_WORKFLOW")"
    backup_readiness_block="$(awk '
        /^  backup-readiness:/ { capture=1 }
        capture { print }
        capture && /^  manifest-key:/ { exit }
    ' "$RELEASE_PREFLIGHT_WORKFLOW")"
    backup_workflow_block="$(cat "$SIGNING_BACKUP_WORKFLOW")"

    for block in "$signer_block" "$manifest_block" "$backup_readiness_block" "$backup_workflow_block"; do
        printf '%s\n' "$block" | grep -Eq '^[[:space:]]+environment:[[:space:]]+release$' || {
            printf 'a signing-key or recovery-passphrase job is not bound to the release environment\n'
            return 1
        }
    done
    for block in "$unsigned_block" "$handoff_block" "$assembly_block" \
        "$publish_draft_block" "$draft_verify_block" "$publish_block" "$published_verify_block"; do
        if printf '%s\n' "$block" | grep -Eq '^[[:space:]]+environment:[[:space:]]+release$'; then
            printf 'a no-secret build, handoff, verifier, or publisher job requests the release environment\n'
            return 1
        fi
    done
    for required in \
        'npm exec --offline -- tauri build --ci --no-bundle' \
        'npm exec --offline -- tauri bundle --ci --no-sign' \
        'unset TAURI_SIGNING_PRIVATE_KEY TAURI_SIGNING_PRIVATE_KEY_PASSWORD' \
        'Normalize and package the exact no-key bundle handoff' \
        'overwrite: false'
    do
        printf '%s\n' "$unsigned_block" | grep -Fq -- "$required" || {
            printf 'no-key desktop build omits: %s\n' "$required"
            return 1
        }
    done
    if printf '%s\n' "$unsigned_block" | grep -Fq '${{ secrets.TAURI_SIGNING_PRIVATE_KEY }}'; then
        printf 'no-key desktop build can receive the updater private key\n'
        return 1
    fi
    for required in \
        'needs: [validate, desktop-unsigned]' \
        'Materialize and verify every handoff without release secrets' \
        'arc.desktop-signer-handoff.v1' \
        'steps.upload.outputs.artifact-id' \
        'steps.upload.outputs.artifact-digest' \
        'retention-days: 2' \
        'overwrite: false'
    do
        printf '%s\n' "$handoff_block" | grep -Fq -- "$required" || {
            printf 'unprivileged desktop signer handoff omits: %s\n' "$required"
            return 1
        }
    done
    if printf '%s\n' "$handoff_block" | grep -Eq 'TAURI_SIGNING_PRIVATE_KEY|environment:[[:space:]]+release'; then
        printf 'unprivileged desktop handoff can receive an updater key\n'
        return 1
    fi
    for required in \
        'needs: [validate, desktop-signer-handoff]' \
        'artifact-ids: ${{ needs.desktop-signer-handoff.outputs.artifact_id }}' \
        'merge-multiple: true' \
        'digest-mismatch: error' \
        'No repository program runs before the updater-key step' \
        'npm ci --prefix desktop --ignore-scripts' \
        'Freeze the exact locked file-signing surface without executing it' \
        'Sign only the verified updater payload' \
        '"$ARC_NODE_BIN" "$ARC_TAURI_CLI" signer sign "$updater"' \
        'unsigned-desktop-handoff.py verify-signed' \
        'tauri-updater-verifier'
    do
        printf '%s\n' "$signer_block" | grep -Fq -- "$required" || {
            printf 'isolated updater signer omits: %s\n' "$required"
            return 1
        }
    done
    validate_line="$(printf '%s\n' "$signer_block" | grep -nF \
        'Validate the sealed handoff and make payloads non-executable' | cut -d: -f1)"
    rehydrate_line="$(printf '%s\n' "$signer_block" | grep -nF \
        'npm ci --prefix desktop --ignore-scripts' | cut -d: -f1)"
    signing_line="$(printf '%s\n' "$signer_block" | grep -nF \
        'Sign only the verified updater payload' | cut -d: -f1)"
    if [ -z "$validate_line" ] || [ -z "$rehydrate_line" ] || [ -z "$signing_line" ] \
        || [ "$validate_line" -ge "$rehydrate_line" ] \
        || [ "$rehydrate_line" -ge "$signing_line" ]; then
        printf 'desktop signer validation and locked rehydration do not precede key exposure\n'
        return 1
    fi
    signing_step="$(printf '%s\n' "$signer_block" | awk '
        /- name: Sign only the verified updater payload/ { capture=1 }
        capture { print }
        capture && /- name: Prove signer output/ { exit }
    ')"
    if printf '%s\n' "$signing_step" \
        | grep -Eq 'python|npm|cargo|rustc|build\.rs|tauri[[:space:]]+bundle|scripts/release'; then
        printf 'updater signing-key step executes a repository, lifecycle, compiler, or bundle surface\n'
        return 1
    fi
    if [ "$(printf '%s\n' "$signer_block" | grep -Fc 'TAURI_SIGNING_PRIVATE_KEY: ${{ secrets.TAURI_SIGNING_PRIVATE_KEY }}')" -ne 1 ]; then
        printf 'updater private key is not scoped to exactly one direct-signing step\n'
        return 1
    fi
    pre_signer="$(printf '%s\n' "$signer_block" | awk '
        /- name: Sign only the verified updater payload/ { exit }
        { print }
    ')"
    if printf '%s\n' "$pre_signer" | grep -Eq \
        '(^|[[:space:]])(bash|sh|python3?|node)[[:space:]]+([^|;&]*\/)?scripts\/|cargo[[:space:]]+(build|run|test)|npm[[:space:]]+(run|exec)|signer[[:space:]]+sign[[:space:]]'; then
        printf 'updater signer executes repository/lifecycle/compiler/signer code before key exposure\n'
        return 1
    fi
    for required in \
        'needs: [validate, release-quality, release-supply-chain, cross-arch-golden-vectors, headless, linux-server-compat, desktop]' \
        'Stage the exact unsigned manifest handoff' \
        'release-manifest-handoff.py stage' \
        'Upload the create-only unsigned manifest handoff' \
        'contents: read'
    do
        printf '%s\n' "$assembly_block" | grep -Fq -- "$required" || {
            printf 'no-secret release assembly omits: %s\n' "$required"
            return 1
        }
    done
    if printf '%s\n' "$assembly_block" | grep -Eq 'ARC_RELEASE_MANIFEST_PRIVATE_KEY|contents:[[:space:]]+write'; then
        printf 'release assembly can access manifest signing or publication authority\n'
        return 1
    fi
    for required in \
        'needs: [validate, assemble-release]' \
        'environment: release' \
        'artifact-ids: ${{ needs.assemble-release.outputs.artifact_id }}' \
        'No repository program executes before the private-key step' \
        'Validate the complete unsigned allowlist and freeze all inputs' \
        'Sign only the frozen SHA256SUMS manifest' \
        'ARC_RELEASE_MANIFEST_PRIVATE_KEY: ${{ secrets.ARC_RELEASE_MANIFEST_PRIVATE_KEY }}' \
        '/usr/bin/ssh-keygen -Y sign' \
        'Verify manifest signature after private-key removal' \
        '/usr/bin/ssh-keygen -Y verify' \
        'cleanup_key' \
        'Upload the create-only sealed release handoff'
    do
        printf '%s\n' "$manifest_block" | grep -Fq -- "$required" || {
            printf 'isolated release-manifest signer omits: %s\n' "$required"
            return 1
        }
    done
    manifest_signing_step="$(printf '%s\n' "$manifest_block" | awk '
        /- name: Sign only the frozen SHA256SUMS manifest/ { capture=1 }
        capture { print }
        capture && /- name: Prove original bytes unchanged/ { exit }
    ')"
    if printf '%s\n' "$manifest_signing_step" \
        | grep -Eq 'python|npm|cargo|rustc|scripts/release|source[[:space:]]'; then
        printf 'manifest signing-key step executes repository or compiler code\n'
        return 1
    fi
    manifest_sign_line="$(printf '%s\n' "$manifest_signing_step" | grep -nF '/usr/bin/ssh-keygen -Y sign' | cut -d: -f1)"
    manifest_cleanup_line="$(printf '%s\n' "$manifest_signing_step" | grep -nF '          cleanup_key' | tail -n 1 | cut -d: -f1)"
    manifest_verify_line="$(printf '%s\n' "$manifest_signing_step" | grep -nF '/usr/bin/ssh-keygen -Y verify' | cut -d: -f1)"
    if [ -z "$manifest_sign_line" ] || [ -z "$manifest_cleanup_line" ] || [ -z "$manifest_verify_line" ] \
        || [ "$manifest_sign_line" -ge "$manifest_cleanup_line" ] \
        || [ "$manifest_cleanup_line" -ge "$manifest_verify_line" ]; then
        printf 'manifest private key is not removed immediately after signing and before verification\n'
        return 1
    fi

    for required in \
        'Select and download the exact-main ciphertext before secret access' \
        'npm ci --prefix desktop --ignore-scripts' \
        'Restore only the four bounded members and exercise both keys' \
        'canonicalize_manifest_public_key' \
        'NR != 1 { exit 1 }' \
        'if (NF != 2 && NF != 3) exit 1' \
        'if ($1 != "ssh-ed25519") exit 1' \
        'if ($2 !~ /^[A-Za-z0-9+\/]+={0,2}$/) exit 1' \
        'if (NF == 3 && $3 != "arc-release-manifest-v1") exit 1' \
        '"$expected_manifest" "$expected_manifest_canonical"' \
        '"$derived_manifest_canonical" "$expected_manifest_canonical"' \
        '"$restored_manifest_canonical" "$expected_manifest_canonical"' \
        'cleanup_plaintext' \
        'Verify the updater canary only after all recovered secrets are gone' \
        'ARC_SIGNING_BACKUP_PASSPHRASE: ${{ secrets.ARC_SIGNING_BACKUP_PASSPHRASE }}'
    do
        printf '%s\n' "$backup_readiness_block" | grep -Fq -- "$required" || {
            printf 'isolated signing-backup readiness job omits: %s\n' "$required"
            return 1
        }
    done
    backup_restore_step="$(printf '%s\n' "$backup_readiness_block" | awk '
        /- name: Restore only the four bounded members and exercise both keys/ { capture=1 }
        capture { print }
        capture && /- uses: dtolnay\/rust-toolchain@/ { exit }
    ')"
    if printf '%s\n' "$backup_restore_step" \
        | grep -Eq 'scripts\/|python|cargo|rustc|npm[[:space:]]+(run|exec)|source[[:space:]]|bash[[:space:]]'; then
        printf 'backup recovery-secret step executes repository, lifecycle, compiler, or shell-source code\n'
        return 1
    fi
    restore_line="$(printf '%s\n' "$backup_readiness_block" | grep -nF \
        'Restore only the four bounded members and exercise both keys' | cut -d: -f1)"
    cleanup_line="$(printf '%s\n' "$backup_readiness_block" | grep -nF \
        '          cleanup_plaintext' | tail -1 | cut -d: -f1)"
    verifier_line="$(printf '%s\n' "$backup_readiness_block" | grep -nF \
        'Verify the updater canary only after all recovered secrets are gone' | cut -d: -f1)"
    if [ -z "$restore_line" ] || [ -z "$cleanup_line" ] || [ -z "$verifier_line" ] \
        || [ "$restore_line" -ge "$cleanup_line" ] || [ "$cleanup_line" -ge "$verifier_line" ]; then
        printf 'backup passphrase/plaintext cleanup does not precede repository verifier compilation\n'
        return 1
    fi

    if grep -Fq 'actions/checkout@' "$SIGNING_BACKUP_WORKFLOW" \
        || grep -Eq 'scripts\/|python|node|npm|cargo|rustc' "$SIGNING_BACKUP_WORKFLOW"; then
        printf 'protected signing-key backup workflow checks out or invokes repository/package/compiler code\n'
        return 1
    fi
    backup_key_step="$(awk '
        /- name: Create restore-tested ciphertext/ { capture=1 }
        capture { print }
        capture && /- name: Upload ciphertext only/ { exit }
    ' "$SIGNING_BACKUP_WORKFLOW")"
    for required in \
        'backup_passphrase="$ARC_SIGNING_BACKUP_PASSPHRASE"' \
        'unset ARC_SIGNING_BACKUP_PASSPHRASE' \
        '/usr/bin/gpg' \
        '/usr/bin/ssh-keygen' \
        '/usr/bin/shred -u' \
        'cleanup_keys'
    do
        printf '%s\n' "$backup_key_step" | grep -Fq -- "$required" || {
            printf 'inline signing-key backup secret window omits: %s\n' "$required"
            return 1
        }
    done
    for required in \
        'needs: [validate, manifest-sign]' \
        'This privileged job deliberately has no checkout' \
        'artifact-ids: ${{ needs.manifest-sign.outputs.artifact_id }}' \
        'gh release upload "$RELEASE_TAG" release-files/*' \
        '## Upgrading from v0.7.x' \
        'The v0.8 `.arc-node.lock` is a same-generation guard' \
        'fresh `data-v0.8/`' \
        'fresh `data-v3*`' \
        'Upload the exact draft API evidence'
    do
        grep -Fq -- "$required" <<< "$publish_draft_block" || {
            printf 'isolated draft publisher omits: %s\n' "$required"
            return 1
        }
    done
    for required in \
        'needs: [validate, manifest-sign, publish-draft]' \
        'contents: read' \
        'python3 scripts/release/verify-github-release.py' \
        '--draft true --immutable false'
    do
        grep -Fq -- "$required" <<< "$draft_verify_block" || {
            printf 'unprivileged draft verifier omits: %s\n' "$required"
            return 1
        }
    done
    for required in \
        'needs: [validate, publish-draft, verify-draft-release]' \
        'The fresh mutation runner' \
        'cmp -s "$canonical_expected" "$canonical_current"' \
        '-F draft=false -f make_latest=false' \
        'publication_attempted=true' \
        'for poll_attempt in {1..12}' \
        'if [ "$publication_attempted" != true ]; then' \
        'Upload the immutable release API evidence'
    do
        grep -Fq -- "$required" <<< "$publish_block" || {
            printf 'isolated final publisher omits: %s\n' "$required"
            return 1
        }
    done
    for required in \
        'needs: [validate, manifest-sign, publish-draft, publish]' \
        'contents: read' \
        'python3 scripts/release/verify-github-release.py' \
        '--draft false --immutable true'
    do
        grep -Fq -- "$required" <<< "$published_verify_block" || {
            printf 'unprivileged immutable-release verifier omits: %s\n' "$required"
            return 1
        }
    done
    if printf '%s\n%s\n' "$publish_draft_block" "$publish_block" \
        | grep -Eq 'ARC_RELEASE_MANIFEST_PRIVATE_KEY|TAURI_SIGNING_PRIVATE_KEY|ARC_SIGNING_BACKUP_PASSPHRASE|environment:[[:space:]]+release'; then
        printf 'publisher can access a signing key or release environment\n'
        return 1
    fi
    for required in \
        'needs: [validate, release-quality, release-supply-chain, cross-arch-golden-vectors]' \
        'Download the immutable signed desktop artifact by ID' \
        'materialize-pretag-artifacts.py'
    do
        printf '%s\n' "$desktop_block" | grep -Fq -- "$required" || {
            printf 'tag-time desktop verification omits: %s\n' "$required"
            return 1
        }
    done
    if printf '%s\n' "$desktop_block" \
        | grep -Eq 'TAURI_SIGNING_PRIVATE_KEY|npx tauri build|environment:[[:space:]]+release'; then
        printf 'tag-time desktop job rebuilds or re-exposes signing state instead of consuming reviewed bytes\n'
        return 1
    fi
    for required in \
        'require full commit-SHA' \
        'protected `release` environment' \
        'required reviewers' \
        'move `TAURI_SIGNING_PRIVATE_KEY`' \
        'two `~ALL` tag rulesets' \
        'keep release creation' \
        'fresh no-checkout job' \
        'fixed system executables may run' \
        'immutable' \
        'Apple Developer ID' \
        'Windows Authenticode' \
        'label macOS and Windows packages unsigned'
    do
        grep -Fq -- "$required" "$REPO_ROOT/docs/VALIDATOR-FLEET-ROLLOUT.md" || {
            printf 'rollout does not document owner-controlled release gate: %s\n' "$required"
            return 1
        }
    done
}

publish_is_pinned_to_one_validated_commit_and_create_only() {
    local checkout_count credential_count sha_checkout_count validate_block quality_block assembly_block ref_values
    local publish_draft_block draft_verify_block cleanup_block publish_block published_verify_block
    local trap_line upload_line compare_line publish_line
    validate_block="$(awk '
        /^  validate:/ { capture=1 }
        capture { print }
        capture && /^  release-quality:/ { exit }
    ' "$RELEASE_WORKFLOW")"
    quality_block="$(awk '
        /^  release-quality:/ { capture=1 }
        capture { print }
        capture && /^  release-supply-chain:/ { exit }
    ' "$RELEASE_WORKFLOW")"
    assembly_block="$(awk '
        /^  assemble-release:/ { capture=1 }
        capture { print }
        capture && /^  manifest-sign:/ { exit }
    ' "$RELEASE_WORKFLOW")"
    publish_draft_block="$(awk '
        /^  publish-draft:/ { capture=1 }
        capture { print }
        capture && /^  verify-draft-release:/ { exit }
    ' "$RELEASE_WORKFLOW")"
    draft_verify_block="$(awk '
        /^  verify-draft-release:/ { capture=1 }
        capture { print }
        capture && /^  cleanup-rejected-draft:/ { exit }
    ' "$RELEASE_WORKFLOW")"
    cleanup_block="$(awk '
        /^  cleanup-rejected-draft:/ { capture=1 }
        capture { print }
        capture && /^  publish:/ { exit }
    ' "$RELEASE_WORKFLOW")"
    publish_block="$(awk '
        /^  publish:/ { capture=1 }
        capture { print }
        capture && /^  verify-published-release:/ { exit }
    ' "$RELEASE_WORKFLOW")"
    published_verify_block="$(awk '
        /^  verify-published-release:/ { capture=1 }
        capture { print }
    ' "$RELEASE_WORKFLOW")"

    for required in \
        'sha: ${{ steps.release.outputs.sha }}' \
        'echo "sha=$TAG_COMMIT"' \
        'git fetch --no-tags --force origin main:refs/remotes/origin/main' \
        'if [ "$TAG_COMMIT" != "$MAIN_COMMIT" ]; then' \
        'REQUESTED_PREFLIGHT_RUN_ID: ${{ inputs.pretag_run_id }}' \
        'REQUESTED_PREFLIGHT_RUN_ATTEMPT: ${{ inputs.pretag_run_attempt }}' \
        'actions/runs/$PREFLIGHT_RUN_ID/attempts/$PREFLIGHT_RUN_ATTEMPT' \
        '.status == "completed"' \
        '.conclusion == "success"' \
        'PREFLIGHT_RUN_ATTEMPT="$REQUESTED_PREFLIGHT_RUN_ATTEMPT"' \
        'python3 scripts/release/select-pretag-artifacts.py' \
        '--github-output "$GITHUB_OUTPUT"'
    do
        printf '%s\n' "$validate_block" | grep -Fq -- "$required" || {
            printf 'release validation does not export the validated tag commit: missing %s\n' "$required"
            return 1
        }
    done

    for required in \
        'needs: validate' \
        'ref: ${{ needs.validate.outputs.sha }}' \
        'node-version: 24' \
        'toolchain: nightly-2026-03-16' \
        './scripts/ci_check.sh --full'
    do
        printf '%s\n' "$quality_block" | grep -Fq -- "$required" || {
            printf 'validated-commit release-quality job is missing: %s\n' "$required"
            return 1
        }
    done
    if grep -Fq 'ref: ${{ needs.validate.outputs.tag }}' "$RELEASE_WORKFLOW"; then
        printf 'a downstream release job still checks out the movable tag instead of the validated SHA\n'
        return 1
    fi
    checkout_count="$(grep -Fc \
        'uses: actions/checkout@3d3c42e5aac5ba805825da76410c181273ba90b1' \
        "$RELEASE_WORKFLOW")"
    credential_count="$(grep -Fc 'persist-credentials: false' "$RELEASE_WORKFLOW")"
    if [ "$credential_count" -ne "$checkout_count" ]; then
        printf 'every release checkout must remove persisted GitHub credentials (checkouts=%s hardened=%s)\n' \
            "$checkout_count" "$credential_count"
        return 1
    fi
    sha_checkout_count="$(grep -Fc 'ref: ${{ needs.validate.outputs.sha }}' \
        "$RELEASE_WORKFLOW")"
    if [ "$checkout_count" -ne "$((sha_checkout_count + 1))" ]; then
        printf 'every checkout after validation must use the exported commit SHA (checkouts=%s SHA refs=%s)\n' \
            "$checkout_count" "$sha_checkout_count"
        return 1
    fi
    ref_values="$(awk '/^[[:space:]]+ref:/ { sub(/^[[:space:]]+/, ""); print }' \
        "$RELEASE_WORKFLOW" | LC_ALL=C sort -u)"
    if [ "$ref_values" != 'ref: ${{ inputs.tag || github.ref }}
ref: ${{ needs.validate.outputs.sha }}' ]; then
        printf 'release workflow contains a checkout ref outside initial tag resolution and pinned SHA:\n%s\n' \
            "$ref_values"
        return 1
    fi
    printf '%s\n' "$assembly_block" \
        | grep -Fq 'needs: [validate, release-quality, release-supply-chain, cross-arch-golden-vectors, headless, linux-server-compat, desktop]' \
        || {
            printf 'release assembly can run without an exact-ref quality, supply-chain, golden-vector, or asset gate\n'
            return 1
        }
    if grep -Fq 'repos/$GITHUB_REPOSITORY/immutable-releases' "$RELEASE_WORKFLOW"; then
        printf 'least-privilege publisher still calls the Administration-only immutable-settings endpoint\n'
        return 1
    fi
    if grep -Eq 'administration:[[:space:]]+read|ADMIN(_|ISTRATION).*TOKEN' "$RELEASE_WORKFLOW"; then
        printf 'publisher introduces a long-lived administration credential instead of retaining least privilege\n'
        return 1
    fi
    for required in \
        'repos/FerrumVir/arc-chain/immutable-releases' \
        'Run that command from the existing owner/admin `gh` session immediately before' \
        'Before publication, a failed upload or independent verification may delete' \
        'Once the publication PATCH has been attempted, every failure path retains' \
        'stops for manual verification' \
        'do not rerun the' \
        'tag or release workflow'
    do
        grep -Fq -- "$required" "$REPO_ROOT/docs/VALIDATOR-FLEET-ROLLOUT.md" || {
            printf 'operator pre-tag immutable-settings runbook omits: %s\n' "$required"
            return 1
        }
    done
    if grep -Fq -- 'immediately deletes that exact release ID without' \
        "$REPO_ROOT/docs/VALIDATOR-FLEET-ROLLOUT.md"; then
        printf 'operator runbook still claims a post-publication release is deleted\n'
        return 1
    fi
    for required in \
        'needs: [validate, manifest-sign]' \
        'This privileged job deliberately has no checkout' \
        'artifact-ids: ${{ needs.manifest-sign.outputs.artifact_id }}' \
        'EXPECTED_SHA: ${{ needs.validate.outputs.sha }}' \
        'SELECTED_PREFLIGHT_RUN_ID: ${{ needs.validate.outputs.pretag_run_id }}' \
        'SELECTED_PREFLIGHT_RUN_ATTEMPT: ${{ needs.validate.outputs.pretag_run_attempt }}' \
        '.run_attempt == $attempt' \
        'repos/$GITHUB_REPOSITORY/git/ref/tags/$RELEASE_TAG' \
        'repos/$GITHUB_REPOSITORY/branches/main' \
        'gh api --paginate "repos/$GITHUB_REPOSITORY/releases?per_page=100"' \
        'gh api --method POST "repos/$GITHUB_REPOSITORY/releases"' \
        'draft:true,prerelease:false,make_latest:"false"' \
        'trap cleanup_draft EXIT' \
        'gh release upload "$RELEASE_TAG" release-files/*' \
        'and .draft == true and .immutable == false' \
        'Upload the exact draft API evidence' \
        'raw.githubusercontent.com/${{ github.repository }}/${{ needs.validate.outputs.tag }}/install.sh' \
        'bash install.sh --version ${{ needs.validate.outputs.version }}'
    do
        grep -Fq -- "$required" <<< "$publish_draft_block" || {
            printf 'isolated create-only draft publisher is missing: %s\n' "$required"
            return 1
        }
    done
    for required in \
        'needs: [validate, manifest-sign, publish-draft]' \
        'contents: read' \
        'artifact-ids: ${{ needs.publish-draft.outputs.evidence_id }}' \
        'python3 scripts/release/verify-github-release.py' \
        '--draft true --immutable false'
    do
        grep -Fq -- "$required" <<< "$draft_verify_block" || {
            printf 'read-only draft verifier is missing: %s\n' "$required"
            return 1
        }
    done
    for required in \
        "needs.verify-draft-release.result != 'success'" \
        'contents: write' \
        'gh api --method DELETE' \
        'repos/$GITHUB_REPOSITORY/releases/$RELEASE_ID'
    do
        grep -Fq -- "$required" <<< "$cleanup_block" || {
            printf 'rejected-draft cleanup is missing: %s\n' "$required"
            return 1
        }
    done
    for required in \
        'needs: [validate, publish-draft, verify-draft-release]' \
        'The fresh mutation runner receives only previously verified API JSON' \
        'repos/$GITHUB_REPOSITORY/git/ref/tags/$RELEASE_TAG' \
        'repos/$GITHUB_REPOSITORY/branches/main' \
        'cmp -s "$canonical_expected" "$canonical_current"' \
        'gh api --method PATCH' \
        '-F draft=false -f make_latest=false' \
        'publication_attempted=true' \
        'for poll_attempt in {1..12}' \
        'state is unconfirmed and cleanup is forbidden' \
        'Preserve release state if publication or evidence sealing failed' \
        'and .draft == false and .immutable == true' \
        'Upload the immutable release API evidence'
    do
        grep -Fq -- "$required" <<< "$publish_block" || {
            printf 'independently gated immutable publisher is missing: %s\n' "$required"
            return 1
        }
    done
    for required in \
        'needs: [validate, manifest-sign, publish-draft, publish]' \
        'contents: read' \
        'artifact-ids: ${{ needs.publish.outputs.evidence_id }}' \
        'python3 scripts/release/verify-github-release.py' \
        '--draft false --immutable true'
    do
        grep -Fq -- "$required" <<< "$published_verify_block" || {
            printf 'read-only immutable-release verifier is missing: %s\n' "$required"
            return 1
        }
    done
    for required in \
        'asset.get("digest")' \
        'asset.get("size")' \
        'asset.get("state")' \
        'github-actions[bot]' \
        'duplicate remote release asset' \
        'release asset name set mismatch' \
        'uploader.get("login")'
    do
        grep -Fq -- "$required" "$RELEASE_SERVER_VERIFIER" || {
            printf 'server-side release verifier omits: %s\n' "$required"
            return 1
        }
    done
    trap_line="$(printf '%s\n' "$publish_draft_block" | grep -nF 'trap cleanup_draft EXIT' | cut -d: -f1)"
    upload_line="$(printf '%s\n' "$publish_draft_block" | grep -nF 'gh release upload "$RELEASE_TAG" release-files/*' | cut -d: -f1)"
    compare_line="$(printf '%s\n' "$publish_block" | grep -nF 'cmp -s "$canonical_expected" "$canonical_current"' | cut -d: -f1)"
    publish_line="$(printf '%s\n' "$publish_block" | grep -nF -- '-F draft=false' | cut -d: -f1)"
    if [ -z "$trap_line" ] || [ -z "$upload_line" ] || [ -z "$compare_line" ] \
        || [ -z "$publish_line" ] || [ "$trap_line" -ge "$upload_line" ] \
        || [ "$compare_line" -ge "$publish_line" ]; then
        printf 'draft cleanup/upload or independently verified compare/publish ordering regressed\n'
        return 1
    fi
    if printf '%s\n%s\n%s\n' "$publish_draft_block" "$cleanup_block" "$publish_block" \
        | grep -Eq 'git[[:space:]]+(fetch|ls-remote|config)|scripts/release|actions/checkout@'; then
        printf 'a contents-write publisher can execute checkout, Git config/hooks, or repository code\n'
        return 1
    fi
    if grep -Eq 'cleanup-tag|git/refs/tags|releases/tags' "$RELEASE_DELETE_HELPER"; then
        printf 'release cleanup helper can address the protected tag\n'
        return 1
    fi
}

validator_transport_cannot_regress_to_anonymous_or_test_only_tls() {
    local forbidden
    for forbidden in \
        'TestnetCertVerifier' \
        '.with_no_client_auth()' \
        'certificate pinning is not implemented'
    do
        if grep -Fq -- "$forbidden" "$VALIDATOR_TRANSPORT"; then
            printf 'validator transport contains forbidden TLS bypass: %s\n' "$forbidden"
            return 1
        fi
    done
    for required in \
        'allowed_validators' \
        'make_server_config' \
        'make_client_config'
    do
        grep -Fq -- "$required" "$VALIDATOR_TRANSPORT" || {
            printf 'validator transport omits authenticated allowlist contract: %s\n' "$required"
            return 1
        }
    done
}

relevant_shell_is_syntax_valid() {
    local file
    for file in "$INSTALLER" "$LEGACY_INSTALLER" "$COMMUNITY_JOIN" "$INFERENCE_JOIN" \
        "$INFERENCE_INSTALL" "$ASSEMBLER" "$PRETAG_LIVE_VERIFY" "$RELEASE_DELETE_HELPER" \
        "$TEST_DIR"/*.sh "$TEST_DIR"/helpers/*.sh; do
        bash -n "$file" || return 1
    done
}

postrelease_public_truth_is_hermetic_exact_and_release_gated() {
    local required
    [ -x "$POSTRELEASE_PUBLIC_TRUTH" ] || {
        printf 'post-release public-truth builder is missing or not executable\n'
        return 1
    }
    [ -x "$POSTRELEASE_PUBLIC_TRUTH_TEST" ] || {
        printf 'post-release public-truth test wrapper is missing or not executable\n'
        return 1
    }
    grep -Fq 'postrelease_public_truth_test.sh' "$RELEASE_TEST_RUNNER" || {
        printf 'post-release public-truth tests are not wired into the release runner\n'
        return 1
    }
    python3 -m py_compile "$POSTRELEASE_PUBLIC_TRUTH" \
        "$TEST_DIR/test_postrelease_public_truth.py" || return 1
    for required in \
        'REWARD_SCHEMA = "arc.recovery.reward-evidence.v3"' \
        '"arc-node-linux-arm64"' \
        '"arc-cli-linux-arm64"' \
        'PROTOCOL_VERSION_RE.fullmatch(checkpoint["protocolVersion"])' \
        'len(sources) != 12' \
        'frontend source identities and endpoints must be unique' \
        'legacy forks do not share one sealed capture' \
        'REWARD_PER_RECEIPT_BASE = 2_500_000_000' \
        '"canonical_cutoff"' \
        'object_pairs_hook=reject_duplicate_keys' \
        'parse_constant=reject_nonfinite_number' \
        'os.O_NOFOLLOW' \
        'os.O_EXCL' \
        'output_dir.mkdir(mode=0o700, parents=False, exist_ok=False)'
    do
        grep -Fq -- "$required" "$POSTRELEASE_PUBLIC_TRUTH" || {
            printf 'post-release public-truth builder omits: %s\n' "$required"
            return 1
        }
    done
    if grep -Fq -- 'arc-node-linux-aarch64' "$POSTRELEASE_PUBLIC_TRUTH" \
        || grep -Fq -- 'arc-cli-linux-aarch64' "$POSTRELEASE_PUBLIC_TRUTH"; then
        printf 'post-release public-truth builder uses noncanonical Linux ARM asset names\n'
        return 1
    fi
    python3 - "$ROOT_README" "$PUBLIC_PRODUCTION_STATUS" <<'PY' || return 1
import json
import pathlib
import sys

readme = pathlib.Path(sys.argv[1]).read_text(encoding="utf-8")
begin = "<!-- ARC_PUBLIC_TRUTH_BEGIN -->"
end = "<!-- ARC_PUBLIC_TRUTH_END -->"
if readme.count(begin) != 1 or readme.count(end) != 1 or readme.index(begin) >= readme.index(end):
    raise SystemExit("README public-truth markers are not one ordered pair")

def unique_pairs(pairs):
    value = {}
    for key, item in pairs:
        if key in value:
            raise ValueError(f"duplicate JSON key: {key}")
        value[key] = item
    return value

status = json.loads(
    pathlib.Path(sys.argv[2]).read_text(encoding="utf-8"),
    object_pairs_hook=unique_pairs,
)
if status.get("schema") != "arc.public-production-status.v1":
    raise SystemExit("public production status has the wrong schema")
if status.get("state") == "maintenance":
    if set(status) != {"schema", "state", "notice"} or not isinstance(status["notice"], str) or not status["notice"].strip():
        raise SystemExit("maintenance public status is not the exact fail-closed shape")
elif status.get("state") == "recovered":
    required = {"acceptance", "checkpoint", "fleet", "network", "pages", "release", "rewards", "schema", "services", "state"}
    if set(status) != required:
        raise SystemExit("recovered public status does not have the exact generated shape")
else:
    raise SystemExit("public production status state is unsupported")
PY
}

production_manifest_builder_is_release_gated() {
    [ -x "$PRODUCTION_MANIFEST_BUILDER" ] || {
        printf 'production manifest builder is missing or not executable\n'
        return 1
    }
    [ -x "$PRODUCTION_MANIFEST_TEST" ] || {
        printf 'production manifest builder test wrapper is missing or not executable\n'
        return 1
    }
    grep -Fq 'production_manifest_builder_test.sh' "$RELEASE_TEST_RUNNER" || {
        printf 'production manifest builder is not wired into the release test runner\n'
        return 1
    }
    for required in \
        'from recovery_freeze import FreezeValidationError, validate_pinned_freeze_plan' \
        'legacy-public-height.py' \
        'import recovery_rollout as rollout' \
        'arc.validator-vault.offline-stop-evidence.v2' \
        'arc.recovery.offline-stop-remote-verification.v1' \
        'arc.recovery.offline-stop-challenged-status.v1' \
        'offline-stop.v4' \
        'secrets.token_hex(32)' \
        'validate_known_hosts' \
        'SYSTEM_PYTHON_ENTRYPOINT = Path("/usr/bin/python3")' \
        'def _system_python()' \
        'allow_multiple_hardlinks=True' \
        'ssh_known_hosts' \
        'MAX_OFFLINE_STOP_VERIFICATION_AGE_SECONDS = 300' \
        'MAX_OFFLINE_STOP_VERIFICATION_DURATION_MS = 120_000' \
        'MAX_LEGACY_HEIGHT_TO_AUTHENTICATED_CROSS_SECONDS = 300' \
        'load_intrinsic_legacy_public_height_receipt' \
        'validate_sealed_legacy_height_capture_timeline' \
        'quarantine_generation_ledger_sha256' \
        'CADDY_LINUX_AMD64_SHA256' \
        'ARCHIVE_FINALIZATION_FIELDS' \
        'prearchive_projection_digest' \
        'exact_mode=0o400' \
        'os.O_EXCL'
    do
        grep -Fq -- "$required" "$PRODUCTION_MANIFEST_BUILDER" || {
            printf 'production manifest builder omits sealed contract: %s\n' "$required"
            return 1
        }
    done
    for required in \
        'build-production-manifest.py prearchive' \
        '--offline-stop-evidence' \
        '--ssh-known-hosts' \
        '--ssh-identity' \
        'build-production-manifest.py finalize'
    do
        grep -Fq -- "$required" "$RECOVERY_RUNBOOK" || {
            printf 'production recovery runbook omits builder contract: %s\n' "$required"
            return 1
        }
    done
    for required in \
        'verify-offline-stop' \
        'stopped-status-challenged' \
        '/usr/bin/ssh -F /dev/null' \
        'IdentityAgent=none' \
        'ProxyCommand=none' \
        'StrictHostKeyChecking=yes' \
        'verify_offline_stop_transport_tools' \
        '--python-path' \
        '--python-sha256' \
        '--ssh-sha256' \
        'reviewed Python path differs from /usr/bin/python3 resolution' \
        'arc.recovery.offline-stop-remote-verification.v1' \
        'sample_legacy_public_height_late' \
        'validate_durable_legacy_height_cross_proof' \
        'ssh-known-hosts'
    do
        grep -Fq -- "$required" "$REPO_ROOT/scripts/recovery/archive-fleet-to-drive.sh" \
            "$REPO_ROOT/scripts/recovery/archive-node.sh" || {
            printf 'production offline-stop verifier omits closed contract: %s\n' "$required"
            return 1
        }
    done
    python3 - "$REPO_ROOT/scripts/recovery/archive-fleet-to-drive.sh" \
        "$PRODUCTION_MANIFEST_BUILDER" <<'PY' || return 1
import pathlib, sys
text = pathlib.Path(sys.argv[1]).read_text(encoding="utf-8")
phase = text[text.index("verify_offline_stop_phase()") : text.index("create_offline_stop_evidence()")]
assert "/usr/bin/stat -c" not in phase
assert "/usr/bin/readlink -f" not in phase
assert 'python3() {' in text
assert '"$ARC_OPERATOR_PYTHON_BIN" -I "$@"' in text
assert "python3 -" in phase

capture = text[text.index("capture_phase()") : text.index("manifest_field()")]
capture_order = (
    capture.index('if [ "$execute" != true ]'),
    capture.index("capture_all_live_observations"),
    capture.index('legacy_height_receipt_sha="$(sample_legacy_public_height_late'),
    capture.index('capture_authenticated_legacy_height_cross_proof "$freeze_plan"'),
    capture.index("run_quarantine_generation_rounds"),
)
assert capture_order == tuple(sorted(capture_order))

builder = pathlib.Path(sys.argv[2]).read_text(encoding="utf-8")
timeline = builder[
    builder.index("def validate_sealed_legacy_height_capture_timeline(") :
    builder.index("def validate_remote_stop_verification(")
]
timeline_order = (
    timeline.index("height_completed = _parse_utc_seconds("),
    timeline.index("fleet_started = _parse_utc_seconds("),
    timeline.index("if not height_completed <= fleet_started <= fleet_completed:"),
    timeline.index("if (fleet_started - height_completed).total_seconds()"),
    timeline.index("> MAX_LEGACY_HEIGHT_TO_AUTHENTICATED_CROSS_SECONDS:"),
    timeline.index("sealed legacy public-height receipt exceeded the 300-second authenticated cross-proof boundary"),
)
assert timeline_order == tuple(sorted(timeline_order))
assert "MAX_LEGACY_HEIGHT_TO_FIRST_QUARANTINE_SECONDS" not in builder
PY
}

windows_desktop_shutdown_is_private_authenticated_and_durable() {
    local required
    for required in \
        'desktop_shutdown_token_file: Option<PathBuf>' \
        'prepare_desktop_shutdown_control(' \
        'constant_time_token_eq(' \
        'take_authenticated_desktop_shutdown_request(' \
        'wait_for_authenticated_desktop_shutdown(' \
        'remove_private_while_open(' \
        'complete_startup_shutdown_if_requested(' \
        'broadcast_node_shutdown('
    do
        grep -Fq -- "$required" "$NODE_MAIN" || {
            printf 'arc-node omits Windows desktop shutdown contract: %s\n' "$required"
            return 1
        }
    done
    for required in \
        'GRACEFUL_STOP_TIMEOUT_SECS: u64 = 4_420' \
        'DESKTOP_SHUTDOWN_TOKEN_FILE_NAME' \
        'write_desktop_shutdown_request(' \
        'desktop_shutdown_control_from_command(' \
        'WindowsProcessHandle' \
        'GetProcessTimes' \
        'QueryFullProcessImageNameW' \
        'PROCESS_SYNCHRONIZE' \
        'process.start_time() == identity.start_time' \
        'legacy_windows_command_matches(' \
        'stop_one_proven_legacy_node(' \
        'LEGACY MIGRATION RECEIPT:' \
        'restore_child_after_failed_stop(' \
        'std::fs::hard_link(&temporary_file, &control.request_file)' \
        'CREATE_NO_WINDOW | CREATE_NEW_PROCESS_GROUP'
    do
        grep -Fq -- "$required" "$DESKTOP_NODE_MANAGER" || {
            printf 'desktop omits Windows graceful-stop contract: %s\n' "$required"
            return 1
        }
    done
    if grep -Fq '.route("/node/shutdown"' "$REPO_ROOT/crates/arc-node/src/rpc.rs"; then
        printf 'desktop shutdown was exposed as an unaudited network RPC route\n'
        return 1
    fi
    for required in \
        'private_desktop_request_stops_node_during_initialization' \
        'sigterm_is_armed_before_synchronous_initialization' \
        'startup-hash-fixture.gguf' \
        'publish_private_request' \
        'shutdown requested before persistent state opened'
    do
        grep -Fq -- "$required" "$DESKTOP_SHUTDOWN_INTEGRATION" || {
            printf 'Windows process lifecycle integration omits: %s\n' "$required"
            return 1
        }
    done
    if grep -Fq 'ARC_TEST_STARTUP_PAUSE_MS' "$NODE_MAIN" "$DESKTOP_SHUTDOWN_INTEGRATION"; then
        printf 'startup shutdown proof still depends on a debug-only pause hook\n'
        return 1
    fi
    grep -Fq '#[tokio::main(flavor = "multi_thread", worker_threads = 2)]' "$NODE_MAIN" || {
        printf 'one-vCPU startup does not retain a lifecycle scheduler worker\n'
        return 1
    }
    grep -Fq 'cargo test -p arc-node --test desktop_shutdown_lifecycle --locked' "$CI_WORKFLOW" || {
        printf 'Windows CI matrix does not run the desktop shutdown process integration\n'
        return 1
    }

    local signal_line download_line control_line state_line
    signal_line="$(grep -nF 'let shutdown_requested = Arc::new' "$NODE_MAIN" | head -1 | cut -d: -f1)"
    download_line="$(grep -nF 'auto_download_model(&shutdown_requested).await' "$NODE_MAIN" | head -1 | cut -d: -f1)"
    control_line="$(grep -nF 'let desktop_shutdown_control = prepare_desktop_shutdown_control' "$NODE_MAIN" | head -1 | cut -d: -f1)"
    state_line="$(grep -nF 'StateDB::with_genesis_persistent_recovery' "$NODE_MAIN" | tail -1 | cut -d: -f1)"
    if [[ -z "$signal_line" || -z "$download_line" || -z "$control_line" || -z "$state_line" \
        || "$signal_line" -ge "$download_line" || "$control_line" -ge "$state_line" ]]; then
        printf 'shutdown admission is not armed before auto-download and persistent recovery\n'
        return 1
    fi
}

signing_key_backup_is_encrypted_create_only_and_restore_tested() {
    for required in \
        'ARC_SIGNING_BACKUP_PASSPHRASE' \
        'refusing to replace existing backup' \
        '--symmetric' \
        '--cipher-algo AES256' \
        '--s2k-digest-algo SHA512' \
        '--decrypt' \
        'shasum -a 256 -c KEY-SHA256SUMS' \
        'cmp -s -- "$TAURI_KEY"' \
        'cmp -s -- "$MANIFEST_KEY"'
    do
        grep -Fq -- "$required" "$SIGNING_KEY_BACKUP" || {
            printf 'signing-key backup omits required encrypted restore contract: %s\n' "$required"
            return 1
        }
    done
    if grep -Eq 'rclone|arc-drive|Google Drive' "$SIGNING_KEY_BACKUP"; then
        printf 'signing-key backup script must not implicitly transmit private material\n'
        return 1
    fi
    for required in \
        'environment: release' \
        'runs-on: ubuntu-24.04' \
        'BACKUP_EXISTING_RELEASE_KEYS' \
        '[ "$GITHUB_REPOSITORY" = "FerrumVir/arc-chain" ]' \
        '[ "$GITHUB_REF" = "refs/heads/main" ]' \
        '[ "$GITHUB_SHA" = "$EXPECTED_MAIN_SHA" ]' \
        'repos/$GITHUB_REPOSITORY/branches/main' \
        'never checks out or executes repository code' \
        'ARC_RELEASE_MANIFEST_PRIVATE_KEY: ${{ secrets.ARC_RELEASE_MANIFEST_PRIVATE_KEY }}' \
        'TAURI_SIGNING_PRIVATE_KEY: ${{ secrets.TAURI_SIGNING_PRIVATE_KEY }}' \
        'ARC_SIGNING_BACKUP_PASSPHRASE: ${{ secrets.ARC_SIGNING_BACKUP_PASSPHRASE }}' \
        '/usr/bin/printf '\''%s\n'\'' "$ARC_RELEASE_MANIFEST_PRIVATE_KEY" > "$manifest_key"' \
        '/usr/bin/gpg' \
        '/usr/bin/ssh-keygen' \
        '/usr/bin/shred -u' \
        'compression-level: 0' \
        'retention-days: 1' \
        'overwrite: false' \
        'include-hidden-files: false' \
        'cleanup_keys' \
        'arc-signing-backup-$EXPECTED_MAIN_SHA-$GITHUB_RUN_ID-$GITHUB_RUN_ATTEMPT-$cipher_sha256' \
        'Remove runner ciphertext'
    do
        grep -Fq -- "$required" "$SIGNING_BACKUP_WORKFLOW" || {
            printf 'protected signing-key backup workflow omits: %s\n' "$required"
            return 1
        }
    done
    if grep -Eq 'pull_request|push:' "$SIGNING_BACKUP_WORKFLOW"; then
        printf 'signing-key backup workflow must remain manual-only\n'
        return 1
    fi
    if grep -Eq 'actions/checkout@|scripts/|python|node|npm|cargo|rustc' \
        "$SIGNING_BACKUP_WORKFLOW"; then
        printf 'protected signing-key backup workflow can execute repository/package/compiler code\n'
        return 1
    fi
    for required in \
        'decrypted archive membership differs from the four-file contract' \
        'shasum -a 256 -c KEY-SHA256SUMS' \
        'release/arc-release-allowed-signers' \
        'awk '\''{print $3 " " $4}'\'' "$REPO_ROOT/release/arc-release-allowed-signers"' \
        'EXPECTED_CIPHERTEXT_SHA256' \
        'git -C "$REPO_ROOT" diff --quiet "$EXPECTED_MAIN_SHA"' \
        'ssh-keygen -Y sign' \
        'ssh-keygen -Y verify' \
        'node_modules/@tauri-apps/cli/tauri.js' \
        '--private-key-path' \
        'tauri-updater-verifier'
    do
        grep -Fq -- "$required" "$SIGNING_BACKUP_VERIFY" || {
            printf 'downloaded signing-key verifier omits: %s\n' "$required"
            return 1
        }
    done
}

validator_vault_restore_and_install_are_fail_closed() {
    [ -x "$VALIDATOR_VAULT_RESTORE" ] || {
        printf 'validator-vault restore/install helper is missing or not executable\n'
        return 1
    }
    for required in \
        'arc.validator-vault.restore.v1' \
        'arc.validator-vault.install.v1' \
        'arc.validator-vault.offline-stop-evidence.v2' \
        'arc.recovery.offline-stop-status.v1' \
        'arc.recovery.offline-stop.v4' \
        'validate_pinned_freeze_plan' \
        'validate_fresh_stopped_status' \
        'pin_openssl_runtime' \
        'did not load the reviewed private' \
        'pin_transport_runtime' \
        '"-S"' \
        '"-F"' \
        '"-i"' \
        'IdentityAgent=none' \
        'pretag_initial_provenance' \
        'pretag_final_provenance' \
        '149.28.32.76' \
        '140.82.16.112' \
        '136.244.109.1' \
        '104.238.171.11' \
        '202.182.107.41' \
        '149.28.153.31' \
        'keygen", "--verify-keyfile"' \
        'identity_before' \
        'write_all(descriptor, payload)' \
        'os.scandir(descriptor)' \
        'pinned pre-tag ARC CLI changed' \
        'key changed during SCP upload' \
        'key changed before SCP upload' \
        'stat -c %h "$destination"' \
        'exact_mode=0o400' \
        'stopped_status_sha256' \
        'stopped_status_argv_sha256' \
        'StrictHostKeyChecking=yes' \
        'UserKnownHostsFile=' \
        '/etc/arc-v3/validator-key.json' \
        'ln -- "$temporary" "$destination"'
    do
        grep -Fq -- "$required" "$VALIDATOR_VAULT_RESTORE" || {
            printf 'validator-vault restore/install static contract omits: %s\n' "$required"
            return 1
        }
    done
    if grep -Eq -- 'extractall|[.]extract[(]|[.]rglob[(]|open[(]"r[+]b"|shell[[:space:]]*=[[:space:]]*True|StrictHostKeyChecking=(no|accept-new)|sshpass' \
        "$VALIDATOR_VAULT_RESTORE"; then
        printf 'validator-vault helper contains an unsafe extraction, shell, or SSH fallback\n'
        return 1
    fi
}

recovery_plan_remote_transport_is_read_only() {
    python3 - "$RECOVERY_ROLLOUT" <<'PY'
import ast
import pathlib
import sys

path = pathlib.Path(sys.argv[1])
source = path.read_text(encoding="utf-8")
tree = ast.parse(source)
rollout_class = next(
    node
    for node in tree.body
    if isinstance(node, ast.ClassDef) and node.name == "RecoveryRollout"
)
methods = {
    node.name: node
    for node in rollout_class.body
    if isinstance(node, (ast.FunctionDef, ast.AsyncFunctionDef))
}

required_methods = {
    "_preflight_production", "ssh_read_only", "ssh", "_run_ssh_script",
    "_rclone_cat_pinned_archive_object",
}
if not required_methods.issubset(methods):
    raise SystemExit("recovery rollout omits the explicit read-only SSH boundary")

preflight_calls = []
for call in ast.walk(methods["_preflight_production"]):
    if (
        isinstance(call, ast.Call)
        and isinstance(call.func, ast.Attribute)
        and isinstance(call.func.value, ast.Name)
        and call.func.value.id == "self"
        and call.func.attr in {"ssh_read_only", "ssh", "scp", "_run_ssh_script"}
    ):
        preflight_calls.append(call.func.attr)
if preflight_calls.count("ssh_read_only") != 3 or any(
    name != "ssh_read_only" for name in preflight_calls
):
    raise SystemExit(
        "production preflight must contain exactly three streamed read-only SSH call sites"
    )

read_only = ast.get_source_segment(source, methods["ssh_read_only"]) or ""
for required in (
    '"/usr/bin/env", "-i"',
    '"HOME=/root"',
    '"PATH=/usr/bin:/bin:/usr/sbin:/sbin"',
    '"LANG=C", "LC_ALL=C"',
    '"/bin/sh", "-s", "--"',
    "self._run_ssh_script(",
):
    if required not in read_only:
        raise SystemExit(f"read-only SSH transport omits exact streamed boundary: {required}")
for forbidden in (
    ".arc-recovery-rollout-helpers", "mkdir", "mktemp", "/bin/ln", "/bin/chmod"
):
    if forbidden in read_only:
        raise SystemExit(f"read-only SSH transport can persist remote state: {forbidden}")

mutating = ast.get_source_segment(source, methods["ssh"]) or ""
if "/root/.arc-recovery-rollout-helpers" not in mutating:
    raise SystemExit("execute transport lost its content-addressed helper contract")

rclone_read = ast.get_source_segment(
    source, methods["_rclone_cat_pinned_archive_object"]
) or ""
for required in (
    "tempfile.TemporaryDirectory(",
    'private_config = temporary_root / "rclone.conf"',
    "_exclusive_write(private_config, config_payload, 0o600)",
    '"HOME": os.fspath(temporary_root)',
    "finally:",
    "self._assert_production_rclone_transport()",
):
    if required not in rclone_read:
        raise SystemExit(
            f"pre-GO rclone metadata read omits private transport boundary: {required}"
        )
if "os.fspath(self.production_rclone_config)" in rclone_read:
    raise SystemExit("pre-GO rclone metadata read passes the operator config directly")
if "no persistent recovery-managed change" not in source:
    raise SystemExit("plan output does not state the scoped recovery-state boundary")
PY
}

archive_transport_configuration_is_invocation_scoped() {
    python3 - "$RECOVERY_ARCHIVE" <<'PY' || return 1
import pathlib
import re
import sys

text = pathlib.Path(sys.argv[1]).read_text(encoding="utf-8")
boundaries = {
    "audit_writers": "seal_freeze_plan() {",
    "seal_freeze_plan": "freeze_plan_hash() {",
    "prepare_writers": "run_remote() {",
    "verify_offline_stop_phase": "create_offline_stop_evidence() {",
    "capture_phase": "manifest_field() {",
    "verify_installed_keys_phase": "upload_immutable() {",
    "verify_complete_phase": "verify_reference_pair() (",
    # seal_phase ends at the next top-level declaration. The previous anchor
    # COMMAND="${1:-}" now sits ~1,600 lines further down, past the whole
    # dispatcher, so the slice swallowed its `trap - EXIT` and aborted this
    # gate with "seal_phase overrides its invocation EXIT cleanup" before a
    # single dispatcher assertion could run.
    "seal_phase": 'archive_write_current_process_id() {',
}
bodies = {}
for name, next_declaration in boundaries.items():
    declaration = f"{name}() {{"
    start = text.index(declaration)
    end = text.index(next_declaration, start + len(declaration))
    body = text[start:end]
    bodies[name] = body
    meaningful = [
        line.strip()
        for line in body.splitlines()[1:]
        if line.strip() and not line.lstrip().startswith("#")
    ]
    if not meaningful or meaningful[0] != "begin_temporary_scope":
        raise SystemExit(f"{name} does not enter its temporary scope before parsing/configuration")
    if re.search(r"(?m)^\s*trap\s+.*\sEXIT\s*$", body):
        raise SystemExit(f"{name} overrides its invocation EXIT cleanup")

transport_owners = {
    name for name, body in bodies.items()
    if re.search(r"(?m)^\s*configure_operator_transport\s+(?:true|false)\s*$", body)
}
python_owners = {
    name for name, body in bodies.items()
    if re.search(r"(?m)^\s*configure_operator_python\s*$", body)
}
if transport_owners != {
    "audit_writers", "prepare_writers", "capture_phase",
    "verify_installed_keys_phase", "verify_complete_phase", "seal_phase",
}:
    raise SystemExit(f"unexpected transport configuration owners: {sorted(transport_owners)}")
if python_owners != {"seal_freeze_plan", "verify_offline_stop_phase"}:
    raise SystemExit(f"unexpected direct Python configuration owners: {sorted(python_owners)}")

all_transport_calls = re.findall(
    r"(?m)^\s*configure_operator_transport\s+(?:true|false)\s*$", text
)
all_python_calls = re.findall(r"(?m)^\s*configure_operator_python\s*$", text)
if len(all_transport_calls) != 6 or len(all_python_calls) != 3:
    # The third Python call is the transport helper's internal dependency.
    raise SystemExit("a configure call was added outside the enumerated command scopes")

cleanup_start = text.index("cleanup_temporary_root() {")
cleanup_end = text.index("begin_temporary_scope() {", cleanup_start)
cleanup = text[cleanup_start:cleanup_end]
scope_start = cleanup_end
scope_end = text.index("die() {", scope_start)
scope = text[scope_start:scope_end]
roots = (
    "ARCHIVE_FLEET_TEMP_ROOT", "ARCHIVE_FLEET_PINNED_ROOT",
    "ARCHIVE_FLEET_PINNED_TRANSPORT_ROOT", "ARCHIVE_FLEET_PINNED_PYTHON_ROOT",
)
for root in roots:
    if f'"${root}"' not in cleanup:
        raise SystemExit(f"cleanup omits {root}")
# The sweep is a loop over exactly those four roots. Assert it does not merely
# call rm: it must verify absence afterwards and propagate a non-zero status,
# because an unchecked rm reported a partial credential sweep as success.
for required in (
    'rm -rf -- "$root" || cleanup_status=1',
    'if [ -e "$root" ] || [ -L "$root" ]; then',
    'FATAL credential sweep incomplete',
    'return "$cleanup_status"',
):
    if required not in cleanup:
        raise SystemExit(f"cleanup no longer guarantees a verified sweep: {required}")
    if f'{root}=""' not in scope:
        raise SystemExit(f"new command scope can inherit {root}")
if scope.count("trap cleanup_temporary_root EXIT") != 1:
    raise SystemExit("temporary scope does not install exactly one EXIT cleanup")
for required in ("trap 'exit 129' HUP", "trap 'exit 130' INT", "trap 'exit 143' TERM"):
    if scope.count(required) != 1:
        raise SystemExit(f"temporary scope omits exact signal cleanup status: {required}")
if '( audit_writers --legacy-validator-set "$legacy_validators" --output "$output" )' not in bodies["prepare_writers"]:
    raise SystemExit("prepare-writers no longer exercises the nested audit scope")

dispatcher_start = text.index("archive_write_current_process_id() {")
dispatcher = text[dispatcher_start:]
for required in (
    "set -m",
    "set +m",
    'ARC_ARCHIVE_DISPATCH_SIGNAL_FORWARDED=false',
    'builtin kill -s "$ARC_ARCHIVE_DISPATCH_SIGNAL" --',
    # The phase is a fresh profile-free Bash that drops every inherited
    # function before sourcing the orchestrator. That unset loop is the
    # defense against exported functions shadowing rm/mv/mktemp.
    'for imported_function in $(builtin compgen -A function); do',
    'arc-archive-dispatch-phase',
    'mkdir -m 700 "$gate/runtime"',
    'TMPDIR="$gate/runtime"',
    'export TMPDIR',
    'archive_remove_dispatch_gate "$gate"',
    'archive_remove_dispatch_gate_until_absent "$gate"',
    # The sentinel anchors the phase PGID so it cannot be reused while members
    # are being killed by exact PID.
    '[ "$sentinel_pgid" = "$phase_pgid" ] || exit 125',
    # The guardian must lead its OWN group. This is precisely what makes its
    # archive_process_group_has_members_except calls sound while the sentinel's
    # were not: the guardian's ps children land outside the counted group.
    '[ "$watchdog_pid" = "$watchdog_pgid" ] || exit 125',
    # The guardian heartbeat is identity-bound to the exact watchdog pid/pgid.
    '[ "$ready_pid" = "$watchdog_pid" ] && [ "$ready_pgid" = "$watchdog_pgid" ]',
    # The supervisor accepts watchdog.ready only from a process that leads its
    # own process group AND whose group is not the phase group -- the property
    # that makes the guardian's membership queries trustworthy.
    '[ "$ready_pid" = "$ready_pgid" ] && [ "$ready_pgid" != "$phase_pgid" ]',
    'observed_parent="$(archive_process_field ppid "$watchdog_pid")"',
    # Bash 3.2 + set -u: an unguarded empty-array expansion is fatal and no
    # caller-side "|| true" suppresses it. Dying there leaves the whole group
    # SIGSTOPped with its matching CONT never reached.
    '${targets[@]+"${targets[@]}"}',
    'wait "$phase_pid"',
    'archive_restore_signal_trap "$saved_hup" HUP',
    'archive_restore_signal_trap "$saved_int" INT',
    'archive_restore_signal_trap "$saved_term" TERM',
):
    if required not in dispatcher:
        raise SystemExit(f"archive dispatcher omits supervised signal contract: {required}")
phase_body = text[
    text.index("archive_dispatch_phase() {"):
    text.index("archive_dispatch_parent_watchdog() {")
]
if '\n    "$command_name" "$@"\n' not in phase_body:
    raise SystemExit("archive phase is not invoked as a direct fail-fast simple command")
if re.search(r'(?m)^\s*(?:if|!)[^\n]*"\$command_name"', phase_body) or \
        re.search(r'"\$command_name"[^\n]*(?:&&|\|\|)', phase_body):
    raise SystemExit("archive phase command is in a context that disables function errexit")
if 'TMPDIR="$work_root" verify_reference_pair' in text:
    raise SystemExit("seal reference scratch escapes the supervisor-owned dispatch gate")
if 'mktemp -d "$work_root/arc-archive-seal.' in text:
    raise SystemExit("seal execution scratch escapes the supervisor-owned dispatch gate")
if 'gate_parent="$requested_work_root"' not in dispatcher:
    raise SystemExit("seal dispatcher gate no longer uses its protected large work root")
sentinel_body = text[
    text.index("archive_dispatch_sentinel() {"):
    text.index("archive_dispatch_parent_watchdog() {")
]
# REGRESSION GUARD (credential leak). The sentinel runs inside phase_pgid, so
# any call it makes to archive_process_group_has_members_except forks a /bin/ps
# child INTO the very group being counted; that child is not in the exclusion
# list, so the answer is unconditionally "members exist". Gating the terminal
# sweep on such a call made archive_remove_dispatch_gate_until_absent
# unreachable and stranded the 0700 gate -- which IS the phase TMPDIR holding
# id_ed25519, known_hosts and rclone.conf -- on every guardian-kill path.
# Excluding the caller's own PID does NOT fix it: Bash forks twice for
# "$(...)", so the exec'd ps is a grandchild of the sentinel.
if "archive_process_group_has_members_except" in sentinel_body:
    raise SystemExit(
        "sentinel queries phase-group membership from inside phase_pgid; its own "
        "ps child is counted and the terminal gate sweep becomes unreachable"
    )
finalized_gate = sentinel_body.index('if [ "$guardian_finalized" = true ]; then')
sentinel_sweep = sentinel_body.index(
    'archive_remove_dispatch_gate_until_absent "$gate"', finalized_gate
)
if not finalized_gate < sentinel_sweep:
    raise SystemExit("sentinel sweep is not gated on the guardian completion receipt")

# The receipt that sweep now trusts must itself be earned: the guardian only
# publishes guardian.finalized after its anchor-validated drain loop empties
# the group. Its identical membership calls ARE sound because it leads its own
# PGID (asserted in the required-literal list above).
guardian_body = text[text.index("archive_dispatch_parent_watchdog() {"):]
guardian_drain = guardian_body.index(
    'while archive_process_group_has_members_except "$phase_pgid" "$sentinel_pid"; do'
)
guardian_receipt = guardian_body.index("guardian.finalized.partial", guardian_drain)
if not guardian_drain < guardian_receipt:
    raise SystemExit(
        "guardian publishes its completion receipt before draining the phase group"
    )

dispatch_body = dispatcher[
    dispatcher.index("dispatch_archive_command() {"):
    dispatcher.index('COMMAND="${1:-}"')
]
setup_return = dispatch_body.rindex('[ "$setup_status" -eq 0 ] || return "$setup_status"')
signal_return = dispatch_body.rindex('return "$ARC_ARCHIVE_DISPATCH_SIGNAL_STATUS"')
if signal_return > setup_return:
    raise SystemExit("internal cleanup status masks the required exact signal status")

case_start = text.index('case "$COMMAND" in', dispatcher_start)
case_body = text[case_start:]
case_routes = {
    "prepare-writers": "prepare_writers",
    "audit-writers": "audit_writers",
    "seal-freeze-plan": "seal_freeze_plan",
    "capture": "capture_phase",
    "verify-offline-stop": "verify_offline_stop_phase",
    "verify-installed-keys": "verify_installed_keys_phase",
    "seal": "seal_phase",
    "verify-complete": "verify_complete_phase",
}
for cli_name, function_name in case_routes.items():
    route = f'{cli_name}) dispatch_archive_command {function_name} "$@" ;;'
    if case_body.count(route) != 1:
        raise SystemExit(f"{cli_name} does not route exactly once through the dispatcher")
if "-h|--help|help|'') usage ;;" not in case_body:
    raise SystemExit("global help no longer bypasses the supervised phase dispatcher")
for required in (
    'create("known_hosts", known_payload, 0o400)',
    'create("id_ed25519", identity_payload, 0o400)',
    'create("rclone.conf", config_payload, 0o600)',
    'ARC_OPERATOR_RCLONE_CONFIG="$runtime/rclone.conf"',
    'HOME="$ARCHIVE_FLEET_PINNED_TRANSPORT_ROOT"',
    '"$ARC_OPERATOR_RCLONE_BIN" --config "$ARC_OPERATOR_RCLONE_CONFIG"',
):
    if required not in text:
        raise SystemExit(f"private transport copy/wrapper contract omits: {required}")
capture = bodies["capture_phase"]
if not re.search(
    r'inspector_stage_root="\$\(mktemp -d\)"\s*\n'
    r'\s*ARCHIVE_FLEET_TEMP_ROOT="\$inspector_stage_root"', capture
):
    raise SystemExit("capture inspector scratch is not cleanup-owned immediately after allocation")
PY
}

macos_pretag_community_canary_is_exact_private_and_fail_closed() {
    local required origin
    for required in \
        'STOP_BUDGET_SECONDS = 4_420' \
        'START_PROOF_SECONDS = 300' \
        'NO_PID_STABILITY_OBSERVATIONS = 3' \
        'NO_PID_STABILITY_INTERVAL_SECONDS = 1' \
        'CANONICAL_MODEL_SIZE_BYTES = 4_081_004_224' \
        '08a5566d61d7cb6b420c3e4387a39e0078e1f2fe5f055f3a03887385304d4bfa' \
        '8394894aaf32aff64df5c6988186e4802cb77a62daf259d8f5cab11d818ed269' \
        '"--rpc",' \
        'RPC = "127.0.0.1:19944"' \
        '"--p2p-port",' \
        '"--stake",' \
        '"--community-mode",' \
        '"--full-integer-worker",' \
        '"--validator-key-file",' \
        '"--community-rpc-url", url' \
        'os.link(staging, path, follow_symlinks=False)' \
        'os.link(staging, destination, follow_symlinks=False)' \
        'paths.model' \
        '0o400' \
        'fcntl.flock(descriptor, fcntl.LOCK_EX)' \
        'lifecycle_lock=home / f".{LABEL}.lifecycle.lock"' \
        'no write progress' \
        '"launchctl": Path("/bin/launchctl")' \
        '"ps": Path("/bin/ps")' \
        '"lsof": Path("/usr/sbin/lsof")' \
        'unmapped non-absolute platform command is forbidden' \
        '"PATH": "/usr/bin:/bin:/usr/sbin:/sbin"' \
        'FIXED_RUNTIME_PATH = "/usr/bin:/bin:/usr/sbin:/sbin"' \
        'RUNNER_ENV_UNSET' \
        'DYLD_INSERT_LIBRARIES' \
        'BASH_ENV' \
        'OPENSSL_CONF' \
        'HTTPS_PROXY' \
        '"/usr/bin/env",' \
        '"-i",' \
        'protected canary runner tool is unsafe' \
        '("ps", "-ww", "-p", str(pid), "-o", "command=")' \
        '("ps", "-ww", "-p", str(pid), "-o", "comm=")' \
        'result.returncode == 1 and result.stdout == "" and result.stderr == ""' \
        'ps process-existence proof returned an unexpected error' \
        '("lsof", "-a", "-p", str(pid), "-d", "txt", "-Fn")' \
        '"-sTCP:LISTEN"' \
        'listener_names != [RPC]' \
        '"-iUDP"' \
        'udp_names' \
        'must own no UDP sockets' \
        'print-disabled' \
        'result.returncode == 113' \
        'Could not find service' \
        '_classify_launchctl_print' \
        'recovered_loaded_disabled' \
        '_classify_start_phase' \
        '_prove_pid_graceful_drain' \
        'listener_names == [RPC]' \
        'rpc_listener_closed' \
        'gracefully draining canary reopened its loopback RPC listener' \
        '("ps", "-ww", "-axo", "pid=,command=")' \
        'active count' \
        'last exit code' \
        'recovered_loaded_disabled_no_pid' \
        'stable_no_pid_observations' \
        'service_snapshot_sha256' \
        'failed-start canary did not exit within the graceful' \
        'bootout was not attempted across a racy no-PID observation' \
        'loaded canary has no provable PID' \
        '("launchctl", "kill", "SIGTERM", self._service_target())' \
        'cleanup_preserves": ["model", "key", "data", "evidence"]' \
        'pretag_actions_proof' \
        'values["proof"].recheck()' \
        'LIVE-PROVENANCE.json' \
        'LIVE-RECHECK.json' \
        'validate_full_live_provenance' \
        '--raw-actions-zip' \
        '--expected-artifact-id' \
        '--curl-sha256' \
        '--ca-bundle-sha256' \
        'Cache-Control: no-cache' \
        'Pragma: no-cache'
    do
        grep -Fq -- "$required" "$MACOS_COMMUNITY_CANARY" \
            "$PROTECTED_PRETAG_ARTIFACT" || {
            printf 'macOS pre-tag community canary omits: %s\n' "$required"
            return 1
        }
    done
    if grep -Eq -- '--candidate-dir|--expected-artifact-digest|--expected-archive-sha256' \
        "$MACOS_COMMUNITY_CANARY" "$MACOS_COMMUNITY_CANARY_DOC"; then
        printf 'macOS pre-tag canary retains a removed local-receipt authorization flag\n'
        return 1
    fi
    for origin in \
        https://149.28.32.76 \
        https://140.82.16.112 \
        https://136.244.109.1 \
        https://104.238.171.11 \
        https://202.182.107.41 \
        https://149.28.153.31
    do
        grep -Fq -- "\"$origin\"" "$MACOS_COMMUNITY_CANARY" || {
            printf 'macOS pre-tag community canary omits exact HTTPS origin: %s\n' "$origin"
            return 1
        }
    done
    if grep -Fq -- 'SIGKILL' "$MACOS_COMMUNITY_CANARY"; then
        printf 'macOS pre-tag community canary contains a force-kill path\n'
        return 1
    fi
    for command_name in plan install start status stop cleanup; do
        grep -Fq -- "\"$command_name\"" "$MACOS_COMMUNITY_CANARY" || {
            printf 'macOS pre-tag community canary omits command: %s\n' "$command_name"
            return 1
        }
    done
    for required in \
        'does not load or start a process' \
        'no seeds file' \
        '`--peers`' \
        'never sends a force' \
        'not treated as process death or WAL completion.' \
        'three one-second-apart' \
        'unregister the inert label without signaling' \
        'delete the GGUF, dedicated key, chain data' \
        'raw exact-ID GitHub'
    do
        grep -Fq -- "$required" "$MACOS_COMMUNITY_CANARY_DOC" || {
            printf 'macOS pre-tag community canary runbook omits: %s\n' "$required"
            return 1
        }
    done
    grep -Fq 'test_macos_community_canary.py' "$MACOS_COMMUNITY_CANARY_TEST" || {
        printf 'macOS pre-tag community canary hermetic test wrapper is missing\n'
        return 1
    }
}

owner_emergency_recovery_authorization_is_durable_and_exact() {
    local required
    for required in \
        'RECEIPT_SCHEMA = "arc.recovery.owner-emergency-recovery.v2"' \
        'REPOSITORY = "FerrumVir/arc-chain"' \
        'WORKFLOW_PATH = ".github/workflows/owner-emergency-recovery-approval.yml"' \
        'AUTHORIZATION_KIND = "owner_emergency_recovery"' \
        'authenticated by an exact GitHub' \
        'REASON_CODE = "legacy_fleet_divergence_history_preserving_v080_cutover"' \
        'SOURCE_HEIGHT = 137_145' \
        'TRANSITION_HEIGHT = 137_146' \
        'RECOVERY_EPOCH = 1' \
        'VALIDATOR_SET_ID = 1' \
        'SIGNATURES_REQUIRED = 5' \
        'AUTHORIZED_SIGNER_ORDER' \
        'UNUSED_RECOVERY_MEMBER' \
        'approval run actor' \
        'approval run triggering actor' \
        'approval exact-attempt jobs do not contain exactly one authorization job' \
        'approval artifact API identity differs from the exact run attempt' \
        'downloaded approval artifact differs from the GitHub server digest' \
        'approval artifact must contain only' \
        'fail(f"create-only {label} already exists")' \
        'owner emergency-recovery receipt is stale' \
        'exact_modes={0o400}' \
        'SAFE_PATH_COMPONENT_RE' \
        'os.O_EXCL' \
        'os.O_NOFOLLOW'
    do
        grep -Fq -- "$required" "$OWNER_EMERGENCY_HELPER" || {
            printf 'owner emergency-recovery helper omits: %s\n' "$required"
            return 1
        }
    done
    for required in \
        '"const": "arc.recovery.owner-emergency-recovery.v2"' \
        '"const": ".github/workflows/owner-emergency-recovery-approval.yml"' \
        '"const": "workflow_dispatch"' \
        '"const": "owner_emergency_recovery"' \
        '"const": "legacy_fleet_divergence_history_preserving_v080_cutover"' \
        '"const": 137145' \
        '"const": 137146' \
        '"const": 5' \
        '"const": 40000000' \
        '"const": "FerrumVir"' \
        '"const": 111036403'
    do
        grep -Fq -- "$required" "$OWNER_EMERGENCY_SCHEMA" || {
            printf 'owner emergency-recovery schema omits: %s\n' "$required"
            return 1
        }
    done
    for required in \
        'ACTOR_LOGIN: ${{ github.actor }}' \
        'ACTOR_ID: ${{ github.actor_id }}' \
        'TRIGGERING_ACTOR_LOGIN: ${{ github.triggering_actor }}' \
        '[ "$ACTOR_LOGIN" = FerrumVir ]' \
        '[ "$ACTOR_ID" = 111036403 ]' \
        '[ "$TRIGGERING_ACTOR_LOGIN" = FerrumVir ]' \
        'actions/runs/$GITHUB_RUN_ID/attempts/$GITHUB_RUN_ATTEMPT' \
        '.actor.login == "FerrumVir" and .actor.id == 111036403' \
        '.triggering_actor.login == "FerrumVir"' \
        'overwrite: false' \
        'retention-days: 90'
    do
        grep -Fq -- "$required" "$OWNER_EMERGENCY_WORKFLOW" || {
            printf 'owner-authentication workflow omits: %s\n' "$required"
            return 1
        }
    done
    if grep -Fq 'secrets.' "$OWNER_EMERGENCY_WORKFLOW" \
        || grep -Eq '^[[:space:]]+(contents|actions|packages|deployments|id-token): write' \
            "$OWNER_EMERGENCY_WORKFLOW"; then
        printf 'owner-authentication workflow acquired a secret or mutation authority\n'
        return 1
    fi
    python3 -m py_compile "$OWNER_EMERGENCY_HELPER" "$OWNER_EMERGENCY_TEST" || return 1
    python3 - "$OWNER_EMERGENCY_SCHEMA" <<'PY' || return 1
import json, pathlib, sys
schema = json.loads(pathlib.Path(sys.argv[1]).read_text(encoding="utf-8"))
if schema.get("type") != "object" or schema.get("additionalProperties") is not False:
    raise SystemExit("owner emergency-recovery receipt schema is not closed")
PY
    grep -Fq -- 'owner_emergency_recovery_test.sh' "$RELEASE_TEST_RUNNER" || {
        printf 'release harness does not run the owner emergency-recovery contract\n'
        return 1
    }
    grep -Fq -- 'test_cli_verifies_exact_owner_attempt_and_materializes_create_only_pair' \
        "$OWNER_EMERGENCY_TEST" || {
        printf 'owner emergency-recovery behavioral test is missing\n'
        return 1
    }
    if grep -Fq -- 'checkpoint_approval_phrase=' "$RECOVERY_RUNBOOK" \
        || grep -Fq -- 'After all six operators compare it out of band' "$RECOVERY_RUNBOOK"; then
        printf 'recovery signing still relies on the ceremonial six-operator phrase\n'
        return 1
    fi
}

run_test 'required headless assets are built and gate the sole publisher' required_assets_are_built_and_gated
run_test 'locked Rust and JavaScript Tauri packages are release-compatible' desktop_tauri_packages_are_release_compatible
run_test 'desktop packages the exact canonical release seed list' packaged_desktop_network_resources_match_release
run_test 'Linux ARM uses canonical arm64 names and is release-blocking' linux_arm_asset_name_is_consistent_and_required
run_test 'release publishes and gates a SHA256SUMS manifest' checksum_manifest_is_published_and_gated
run_test 'installer and update-only path verify SHA-256 before replacement' installer_and_updater_verify_checksums
run_test 'headless manifest is owner-signed and both protected keys have a pre-tag canary' release_manifest_has_owner_signature_and_preflight
run_test 'pre-tag artifacts are complete, immutable-ID bound, digest-verified, and never rebuilt after tagging' pretag_exact_byte_handoff_is_fail_closed
run_test 'upload-artifact digests are canonicalized exactly once at every job boundary' upload_artifact_digests_are_canonicalized_at_job_boundaries
run_test 'raw node consumers use exact versioned release URLs' raw_node_downloads_are_version_pinned
run_test 'update-only path refuses equal and older semantic versions' updater_has_semver_downgrade_guard
run_test 'installer normalizes service identity and protects its seed' installer_normalizes_service_identity_and_secret_permissions
run_test 'release assembly validates the exact shipped genesis before packaging' release_genesis_is_validated_before_packaging
run_test 'legacy installer cannot create or expose a staked validator identity' legacy_installer_cannot_create_a_validator_identity
run_test 'community join entrypoints delegate to the stake-zero checksummed installer' community_join_entrypoints_are_stake_zero_wrappers
run_test 'secret scans are pinned to the CI commit and local releasable worktree' secret_scan_is_pinned_to_the_current_tree
run_test 'Ubuntu 22/24/26 smoke boots the compatible real GUI-free node and checks health' linux_compat_smoke_executes_real_headless_node
run_test 'release actions are exact-SHA pinned to the reviewed allowlist' release_actions_are_exact_sha_allowlisted
run_test 'GitHub-owned actions are exact-SHA pinned to the reviewed Node 24 releases' github_owned_actions_are_node24_exact_sha_allowlisted
run_test 'CI/release cargo and npm supply-chain audits are exact-ref and blocking' release_supply_chain_and_npm_audits_are_blocking
run_test 'release golden vectors cover Linux x86/ARM, both Macs, and Windows' cross_arch_golden_vectors_gate_publication
run_test 'branch golden vectors prove manifest verification on every installer OS' branch_golden_vectors_prove_manifest_verification_on_every_os
run_test 'workspace compile and library tests block on Linux, Mac, and Windows' cross_os_workspace_tests_are_blocking
run_test 'Candle and benchmark-tools have distinct protected statuses mirrored locally' nondefault_release_features_have_distinct_blocking_statuses
run_test 'all ShellCheck gates share the blocking warning/error policy' shellcheck_gates_share_the_blocking_warning_policy
run_test 'updater signatures verify against the embedded key and reject rotation' updater_signatures_are_verified_against_the_embedded_key
run_test 'signing and publishing require the owner-protected release environment' release_secret_jobs_require_the_owner_environment
run_test 'publisher pins one validated commit, rechecks the tag, and refuses release replacement' publish_is_pinned_to_one_validated_commit_and_create_only
run_test 'validator QUIC forbids anonymous/test-only TLS and retains exact allowlisting' validator_transport_cannot_regress_to_anonymous_or_test_only_tls
run_test 'Windows desktop shutdown is private, authenticated, and uses the full durability budget' windows_desktop_shutdown_is_private_authenticated_and_durable
run_test 'signing-key backups are encrypted, create-only, and restore-tested' signing_key_backup_is_encrypted_create_only_and_restore_tested
run_test 'validator-vault restore/install is profile-bound, offline-proof gated, and create-only' validator_vault_restore_and_install_are_fail_closed
run_test 'production manifest builder is hermetic, sealed, documented, and release-gated' production_manifest_builder_is_release_gated
run_test 'post-release public truth is hermetic, exact-evidence bound, and release-gated' postrelease_public_truth_is_hermetic_exact_and_release_gated
run_test 'production recovery plan streams probes without persistent remote helpers' recovery_plan_remote_transport_is_read_only
run_test 'archive transport credentials and temp roots are invocation-scoped' archive_transport_configuration_is_invocation_scoped
run_test 'macOS pre-tag community canary is exact, private, SIGTERM-only, and preservation-safe' macos_pretag_community_canary_is_exact_private_and_fail_closed
run_test 'owner emergency recovery is canonical, create-only, fresh, and exact-input bound' owner_emergency_recovery_authorization_is_durable_and_exact
run_test 'release-related shell scripts pass bash syntax validation' relevant_shell_is_syntax_valid

finish_tests
