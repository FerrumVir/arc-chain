#!/usr/bin/env bash
set -uo pipefail

TEST_DIR="$(CDPATH='' cd -- "$(dirname -- "$0")" && pwd)"
REPO_ROOT="$(CDPATH='' cd -- "$TEST_DIR/../.." && pwd)"
# shellcheck source=/dev/null
. "$TEST_DIR/helpers/testlib.sh"

RELEASE_WORKFLOW="$REPO_ROOT/.github/workflows/release.yml"
CI_WORKFLOW="$REPO_ROOT/.github/workflows/ci.yml"
ASSEMBLER="$REPO_ROOT/scripts/release/assemble-release.sh"
INSTALLER="$REPO_ROOT/install.sh"
LEGACY_INSTALLER="$REPO_ROOT/scripts/install-node.sh"
GENESIS_VALIDATOR="$REPO_ROOT/scripts/release/validate-genesis.py"
SECRET_SCANNER="$TEST_DIR/current_tree_secret_scan.sh"
SECRET_MATERIALIZER="$TEST_DIR/materialize_releasable_tree.py"
QUALITY_HARNESS="$REPO_ROOT/scripts/ci_check.sh"
COMMUNITY_JOIN="$REPO_ROOT/scripts/join-testnet.sh"
INFERENCE_JOIN="$REPO_ROOT/scripts/join-inference.sh"
INFERENCE_INSTALL="$REPO_ROOT/scripts/install-inference-node.sh"
UPDATER_SIGNATURE_GATE="$TEST_DIR/verify_tauri_updater_signatures.sh"
UPDATER_VERIFIER_MANIFEST="$TEST_DIR/tauri-updater-verifier/Cargo.toml"
UPDATER_FIXTURE_DIR="$TEST_DIR/fixtures/tauri-updater"
DESKTOP_CARGO_LOCK="$REPO_ROOT/desktop/src-tauri/Cargo.lock"
DESKTOP_NPM_LOCK="$REPO_ROOT/desktop/package-lock.json"
CANONICAL_SEEDS="$REPO_ROOT/testnet-seeds.txt"
DESKTOP_SEEDS="$REPO_ROOT/desktop/src-tauri/resources/testnet-seeds.txt"
VALIDATOR_TRANSPORT="$REPO_ROOT/crates/arc-net/src/transport.rs"

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
    local asset headless_block
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
    headless_block="$(awk '
        /^  headless:/ { capture=1 }
        capture { print }
        capture && /^  linux-server-compat:/ { exit }
    ' "$RELEASE_WORKFLOW")"
    for required in \
        'dtolnay/rust-toolchain@6c977a6ca4077a0ceb28ffbe03f59d46e9ac8772' \
        'toolchain: nightly-2026-03-16'
    do
        printf '%s\n' "$headless_block" | grep -Fq -- "$required" || {
            printf 'headless release build does not explicitly install the workspace nightly: %s\n' \
                "$required"
            return 1
        }
    done
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
    if [ "$(grep -Rhc '^[[:space:]]*gh release create ' "$REPO_ROOT/.github/workflows"/*.yml | awk '{sum += $1} END {print sum + 0}')" -ne 1 ]; then
        printf 'release graph must contain exactly one create-only GitHub release publisher\n'
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
    for required in \
        'require_release_boolean immutable true' \
        'require_release_boolean draft false' \
        'require_release_boolean prerelease false' \
        "--proto '=https'" \
        "--proto-redir '=https'"
    do
        grep -Fq -- "$required" "$INSTALLER" || {
            printf 'installer/update path lacks immutable HTTPS release binding: %s\n' "$required"
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
    local smoke_block docker_run_count
    smoke_block="$(awk '
        /^  linux-server-compat:/ { capture=1 }
        capture { print }
        capture && /^  desktop:/ { exit }
    ' "$RELEASE_WORKFLOW")"

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
        'for ubuntu in 24.04 26.04' \
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
    local ci_audit_block manifest_count quality_block supply_block
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

    printf '%s\n' "$quality_block" \
        | grep -Fq 'npm --prefix "$package" audit --package-lock-only --audit-level=low' \
        || {
            printf 'release-quality does not run the shared blocking npm audit loop\n'
            return 1
        }
    printf '%s\n' "$quality_block" \
        | grep -Fq 'for package in dashboard desktop sdk/typescript sdks/typescript; do' || {
            printf 'release-quality npm audit loop does not enumerate all four lockfiles\n'
            return 1
        }
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
        'runner: ubuntu-22.04' \
        'platform: linux-arm64' \
        'runner: ubuntu-22.04-arm' \
        'platform: macos-arm64' \
        'runner: macos-14' \
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
    grep -Fq 'find "$RELEASE_FILES_DIR" -type f -name '\''*.sig'\'' -print0' \
        "$UPDATER_SIGNATURE_GATE" || {
        printf 'updater-key proof does not enumerate every published signature\n'
        return 1
    }
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
    local desktop_block publish_block
    desktop_block="$(awk '
        /^  desktop:/ { capture=1 }
        capture { print }
        capture && /^  publish:/ { exit }
    ' "$RELEASE_WORKFLOW")"
    publish_block="$(awk '
        /^  publish:/ { capture=1 }
        capture { print }
    ' "$RELEASE_WORKFLOW")"

    for block in "$desktop_block" "$publish_block"; do
        printf '%s\n' "$block" | grep -Eq '^[[:space:]]+environment:[[:space:]]+release$' || {
            printf 'a signing/publication job is not bound to the release environment\n'
            return 1
        }
        printf '%s\n' "$block" | grep -Eq '^[[:space:]]+RUSTUP_TOOLCHAIN:[[:space:]]+stable$' || {
            printf 'desktop signing/publisher release tools are not pinned to stable Rust\n'
            return 1
        }
    done
    printf '%s\n' "$desktop_block" \
        | grep -Fq 'needs: [validate, release-quality, release-supply-chain, cross-arch-golden-vectors]' \
        || {
            printf 'desktop signing can start before exact-commit quality, dependency, and golden-vector gates pass\n'
            return 1
        }
    printf '%s\n' "$desktop_block" \
        | grep -Fq 'npx tauri build --ci ${{ matrix.tauri-args }} -- --locked' || {
            printf 'desktop signing build is not bound to its committed Cargo.lock\n'
            return 1
        }
    for required in \
        'require full commit-SHA' \
        'protected `release` environment' \
        'required reviewers' \
        'move `TAURI_SIGNING_PRIVATE_KEY`' \
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
    local checkout_count credential_count sha_checkout_count validate_block quality_block publish_block ref_values
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
    publish_block="$(awk '
        /^  publish:/ { capture=1 }
        capture { print }
    ' "$RELEASE_WORKFLOW")"

    for required in \
        'sha: ${{ steps.release.outputs.sha }}' \
        'echo "sha=$TAG_COMMIT"' \
        'git fetch --no-tags --force origin main:refs/remotes/origin/main' \
        'git merge-base --is-ancestor "$TAG_COMMIT" refs/remotes/origin/main'
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
    printf '%s\n' "$publish_block" \
        | grep -Fq 'needs: [validate, release-quality, release-supply-chain, cross-arch-golden-vectors, headless, linux-server-compat, desktop]' \
        || {
            printf 'publisher can run without an exact-ref quality, supply-chain, golden-vector, or asset gate\n'
            return 1
        }
    for required in \
        'environment: release' \
        'ref: ${{ needs.validate.outputs.sha }}' \
        'EXPECTED_SHA: ${{ needs.validate.outputs.sha }}' \
        'git ls-remote --exit-code --tags origin' \
        'if [ "$REMOTE_SHA" != "$EXPECTED_SHA" ]; then' \
        '"repos/$GITHUB_REPOSITORY/immutable-releases" --jq '\''.enabled'\''' \
        'if [ "$IMMUTABLE_RELEASES_ENABLED" != true ]; then' \
        'gh api --paginate "repos/$GITHUB_REPOSITORY/releases?per_page=100"' \
        'if grep -Fxq "$RELEASE_TAG" "$EXISTING_TAGS"; then' \
        'gh release create "$RELEASE_TAG" release-files/*' \
        '--verify-tag' \
        '--target "$EXPECTED_SHA"' \
        '"repos/$GITHUB_REPOSITORY/releases/tags/$RELEASE_TAG" --jq '\''.immutable'\''' \
        'if [ "$CREATED_RELEASE_IMMUTABLE" != true ]; then' \
        'releases/download/${{ needs.validate.outputs.tag }}/install.sh' \
        'bash install.sh --version ${{ needs.validate.outputs.version }}'
    do
        printf '%s\n' "$publish_block" | grep -Fq -- "$required" || {
            printf 'create-only exact-commit publisher is missing: %s\n' "$required"
            return 1
        }
    done
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
        "$INFERENCE_INSTALL" "$ASSEMBLER" "$TEST_DIR"/*.sh "$TEST_DIR"/helpers/*.sh; do
        bash -n "$file" || return 1
    done
}

run_test 'required headless assets are built and gate the sole publisher' required_assets_are_built_and_gated
run_test 'locked Rust and JavaScript Tauri packages are release-compatible' desktop_tauri_packages_are_release_compatible
run_test 'desktop packages the exact canonical release seed list' packaged_desktop_network_resources_match_release
run_test 'Linux ARM uses canonical arm64 names and is release-blocking' linux_arm_asset_name_is_consistent_and_required
run_test 'release publishes and gates a SHA256SUMS manifest' checksum_manifest_is_published_and_gated
run_test 'installer and update-only path verify SHA-256 before replacement' installer_and_updater_verify_checksums
run_test 'raw node consumers use exact versioned release URLs' raw_node_downloads_are_version_pinned
run_test 'update-only path refuses equal and older semantic versions' updater_has_semver_downgrade_guard
run_test 'installer normalizes service identity and protects its seed' installer_normalizes_service_identity_and_secret_permissions
run_test 'release assembly validates the exact shipped genesis before packaging' release_genesis_is_validated_before_packaging
run_test 'legacy installer cannot create or expose a staked validator identity' legacy_installer_cannot_create_a_validator_identity
run_test 'community join entrypoints delegate to the stake-zero checksummed installer' community_join_entrypoints_are_stake_zero_wrappers
run_test 'secret scans are pinned to the CI commit and local releasable worktree' secret_scan_is_pinned_to_the_current_tree
run_test 'Ubuntu 24/26 smoke boots a real GUI-free node and checks health' linux_compat_smoke_executes_real_headless_node
run_test 'release actions are exact-SHA pinned to the reviewed allowlist' release_actions_are_exact_sha_allowlisted
run_test 'GitHub-owned actions are exact-SHA pinned to the reviewed Node 24 releases' github_owned_actions_are_node24_exact_sha_allowlisted
run_test 'CI/release cargo and npm supply-chain audits are exact-ref and blocking' release_supply_chain_and_npm_audits_are_blocking
run_test 'release golden vectors cover Linux x86/ARM, both Macs, and Windows' cross_arch_golden_vectors_gate_publication
run_test 'updater signatures verify against the embedded key and reject rotation' updater_signatures_are_verified_against_the_embedded_key
run_test 'signing and publishing require the owner-protected release environment' release_secret_jobs_require_the_owner_environment
run_test 'publisher pins one validated commit, rechecks the tag, and refuses release replacement' publish_is_pinned_to_one_validated_commit_and_create_only
run_test 'validator QUIC forbids anonymous/test-only TLS and retains exact allowlisting' validator_transport_cannot_regress_to_anonymous_or_test_only_tls
run_test 'release-related shell scripts pass bash syntax validation' relevant_shell_is_syntax_valid

finish_tests
