#!/usr/bin/env bash
set -uo pipefail

TEST_DIR="$(CDPATH='' cd -- "$(dirname -- "$0")" && pwd)"
REPO_ROOT="$(CDPATH='' cd -- "$TEST_DIR/../.." && pwd)"
# shellcheck source=/dev/null
. "$TEST_DIR/helpers/testlib.sh"

WORKFLOW="$REPO_ROOT/.github/workflows/post-release-acceptance.yml"
HELPER="$REPO_ROOT/scripts/release/published-artifact-acceptance.py"
RUNBOOK="$REPO_ROOT/scripts/recovery/README.md"
ROLLOUT="$REPO_ROOT/docs/VALIDATOR-FLEET-ROLLOUT.md"

require_literal() {
    local file="$1" literal="$2" message="$3"
    grep -Fq -- "$literal" "$file" || {
        printf '%s: %s\n' "$message" "$literal"
        return 1
    }
}

workflow_is_read_only_and_exercises_published_bytes() {
    local literal
    test -f "$WORKFLOW" && test -f "$HELPER" || return 1
    for literal in \
        'permissions:' \
        'actions: read' \
        'contents: read' \
        '[ "$GITHUB_REF" = refs/tags/v0.8.0 ]' \
        'releases/assets/$id' \
        '--appimage-extract > "$evidence/extract.stdout"' \
        'appimage_visible_window' \
        'desktop_visible_window' \
        'MainWindowHandle' \
        'CGWindowListCopyWindowInfo' \
        'docker run --rm --network none' \
        'legacy_history_preserved' \
        'legacy_model_preserved' \
        'headless_update_only' \
        'attempts/$RELEASE_RUN_ATTEMPT/jobs?per_page=100' \
        'select-published-evidence' \
        'skip-decompress: true' \
        'component-artifacts.json' \
        'canonical/evidence/linux-x86_64' \
        'canonical/evidence/macos-arm64' \
        'canonical/evidence/macos-x86_64' \
        'canonical/evidence/windows-x86_64' \
        'canonical/evidence/release' \
        'binding/published-evidence.zip' \
        'EVIDENCE-MANIFEST.json' \
        '--evidence-manifest canonical/EVIDENCE-MANIFEST.json' \
        '! -name POST-RELEASE-ARTIFACT-ACCEPTANCE.SHA256SUMS -print0' \
        'WindowsInstaller.Installer' \
        "MSI ProductVersion is \$msiProductVersion" \
        "msi_product_version = \$msiProductVersion" \
        'embedded_app_product_version = $embeddedProductVersion' \
        'arc-published-artifact-acceptance-v0.8.0-' \
        '[[ "$ARTIFACT_DIGEST" =~ ^[0-9a-f]{64}$ ]]' \
        'digest sha256:$ARTIFACT_DIGEST'
    do
        require_literal "$WORKFLOW" "$literal" \
            'published-artifact workflow omits a required acceptance boundary' || return 1
    done
    if grep -Fq '[[ "$ARTIFACT_DIGEST" =~ ^sha256:' "$WORKFLOW"; then
        printf 'upload-artifact output was incorrectly treated as an API-prefixed digest\n'
        return 1
    fi
    if grep -Fq ".StartsWith('0.8.0')" "$WORKFLOW"; then
        printf 'Windows package version acceptance still uses a prefix match\n'
        return 1
    fi
    if grep -Eq '^[[:space:]]+(contents|actions|packages|deployments|id-token): write' \
        "$WORKFLOW"; then
        printf 'published-artifact workflow acquired mutation authority\n'
        return 1
    fi
    require_literal "$HELPER" \
        '"id": 432306066' 'legacy v0.7.7 asset ID is not pinned' || return 1
    require_literal "$HELPER" \
        '"size": 23286656' 'legacy v0.7.7 asset size is not pinned' || return 1
    require_literal "$HELPER" \
        '1cfc3039786d023cde24ad0b452f35735b39f9e83aaf293e6ed0bf623a11b20c' \
        'legacy v0.7.7 asset digest is not pinned' || return 1
    require_literal "$HELPER" \
        'EXPECTED_RELEASE_JOBS = frozenset(' \
        'exact release-attempt job allowlist is not pinned' || return 1
    require_literal "$HELPER" \
        'arc.release-published-evidence-artifact.v1' \
        'publication evidence artifact metadata is not sealed' || return 1
    require_literal "$HELPER" \
        'arc-release-published-evidence-{args.commit}-{run_id}-{run_attempt}-' \
        'publication evidence name is not exact-attempt bound' || return 1
    require_literal "$HELPER" \
        'arc.published-artifact-evidence-manifest.v1' \
        'canonical raw evidence is not recursively manifested' || return 1
}

runbook_binds_the_exact_canonical_artifact() {
    local literal
    for literal in \
        'post-release-acceptance.yml --repo FerrumVir/arc-chain --ref v0.8.0' \
        'PUBLISHED-ARTIFACT-ACCEPTANCE-RUN.json' \
        'PUBLISHED-ARTIFACT-ACCEPTANCE-SELECTION.json' \
        'published_acceptance_run_attempt' \
        'published_acceptance_artifact_id' \
        'published_acceptance_artifact_digest' \
        'canonical_receipt_sha256' \
        'published-artifact-acceptance.py" aggregate' \
        'EVIDENCE-MANIFEST.json' \
        'evidence/release/published-evidence.zip' \
        'published acceptance evidence contract is not exactly 36 files' \
        '--evidence-manifest "$published_acceptance_root/EVIDENCE-MANIFEST.json"' \
        '--evidence-root "$published_acceptance_root/evidence"' \
        'arc.post-release-acceptance.v2'
    do
        require_literal "$RUNBOOK" "$literal" \
            'production runbook does not bind exact published acceptance evidence' || return 1
    done
    require_literal "$ROLLOUT" \
        'canonical artifact ID and `sha256:` digest' \
        'rollout summary omits canonical published-artifact identity' || return 1
}

helper_fails_closed_under_adversarial_fixtures() {
    cd "$REPO_ROOT" || return 1
    python3 -m unittest -q tests.release.test_published_artifact_acceptance
}

run_test 'published artifact workflow is read-only and exercises real release bytes' \
    workflow_is_read_only_and_exercises_published_bytes
run_test 'production docs bind exact acceptance run, attempt, artifact ID, and digest' \
    runbook_binds_the_exact_canonical_artifact
run_test 'published artifact verifier fails closed under adversarial fixtures' \
    helper_fails_closed_under_adversarial_fixtures

finish_tests
