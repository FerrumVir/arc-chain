#!/usr/bin/env bash
set -uo pipefail

TEST_DIR="$(CDPATH='' cd -- "$(dirname -- "$0")" && pwd)"
REPO_ROOT="$(CDPATH='' cd -- "$TEST_DIR/../.." && pwd)"
# shellcheck source=/dev/null
. "$TEST_DIR/helpers/testlib.sh"

WORKFLOW="$REPO_ROOT/.github/workflows/deploy-explorer.yml"
BUILDER="$REPO_ROOT/scripts/build-public-site.sh"
LIVE_DASHBOARD_GATE="$REPO_ROOT/dashboard/test-live.mjs"
LIVE_EXPLORER_GATE="$REPO_ROOT/explorer/test-live.mjs"

every_action_is_commit_sha_pinned() {
    local workflow ref
    while IFS= read -r workflow; do
        while IFS= read -r ref; do
            printf '%s\n' "$ref" | grep -Eq '^[^@[:space:]]+@[0-9a-f]{40}$' || {
                printf 'workflow action is not pinned to a full Git object SHA: %s (%s)\n' \
                    "$ref" "$workflow"
                return 1
            }
        done < <(sed -n 's/^[[:space:]]*-\{0,1\}[[:space:]]*uses:[[:space:]]*\([^#[:space:]]*\).*/\1/p' "$workflow")
    done < <(find "$REPO_ROOT/.github/workflows" -type f -name '*.yml' | LC_ALL=C sort)
}

pages_workflow_is_pinned_and_self_contained() {
    local deploy_action_line final_gate_line
    for pin in \
        'actions/checkout@3d3c42e5aac5ba805825da76410c181273ba90b1' \
        'actions/setup-node@820762786026740c76f36085b0efc47a31fe5020' \
        'actions/configure-pages@45bfe0192ca1faeb007ade9deae92b16b8254a0d' \
        'actions/upload-pages-artifact@fc324d3547104276b827a68afc52ff2a11cc49c9' \
        'actions/deploy-pages@cd2ce8fcbc39b97be8ca5fce6e763baed58fa128'
    do
        grep -Fq "$pin" "$WORKFLOW" || {
            printf 'public-console workflow is missing pinned action: %s\n' "$pin"
            return 1
        }
    done

    if grep -Eq 'uses:[[:space:]]+[^#[:space:]]+@v[0-9]' "$WORKFLOW"; then
        printf 'public-console workflow contains a mutable major-version action ref\n'
        return 1
    fi
    if grep -Eqi 'vercel|VERCEL_(TOKEN|ORG_ID|PROJECT_ID)' "$WORKFLOW"; then
        printf 'public-console deployment still depends on unconfigured Vercel state\n'
        return 1
    fi
    grep -Fq 'pages: write' "$WORKFLOW" \
        && grep -Fq 'id-token: write' "$WORKFLOW" \
        && grep -Fq 'name: github-pages' "$WORKFLOW" \
        && grep -Fq 'url: ${{ steps.deployment.outputs.page_url }}' "$WORKFLOW" \
        && grep -Fq 'needs: build' "$WORKFLOW" \
        && grep -Fq 'node shared/frontend/test-arc-network.mjs' "$WORKFLOW" \
        && grep -Fq 'case "$NETWORK_STATE" in' "$WORKFLOW" \
        && grep -Fq 'maintenance)' "$WORKFLOW" \
        && grep -Fq 'recovered|degraded)' "$WORKFLOW" \
        && grep -Fq 'ARC_LIVE_CONFIG=shared/frontend/arc-network.json node dashboard/test-live.mjs' "$WORKFLOW" \
        && grep -Fq 'ARC_LIVE_CONFIG=shared/frontend/arc-network.json node explorer/test-live.mjs' "$WORKFLOW" \
        && grep -Fq './scripts/build-public-site.sh public-site' "$WORKFLOW" || return 1

    for required in \
        'branches: [main]' \
        'cancel-in-progress: true' \
        'ref: ${{ github.sha }}' \
        'fetch-depth: 1' \
        '[ "$EXPECTED_REF" = refs/heads/main ]' \
        'git fetch --no-tags --force origin main:refs/remotes/origin/main' \
        'This Pages candidate was superseded before validation began.' \
        'Protected main advanced while the Pages candidate was assembled.' \
        'Final current-main gate before Pages publication' \
        'A newer protected-main commit superseded this Pages deployment.' \
        'Detect a main advance during Pages publication'
    do
        grep -Fq -- "$required" "$WORKFLOW" || {
            printf 'Pages exact-main/supersession contract omits: %s\n' "$required"
            return 1
        }
    done
    if grep -Eq '^[[:space:]]+paths:' "$WORKFLOW"; then
        printf 'Pages still skips unrelated main commits, allowing an older deployment to remain canonical\n'
        return 1
    fi
    [ "$(grep -Fc 'ref: ${{ github.sha }}' "$WORKFLOW")" -eq 2 ] || {
        printf 'both Pages build and deploy jobs must check out the one event SHA\n'
        return 1
    }
    [ "$(grep -Fc 'git fetch --no-tags --force origin main:refs/remotes/origin/main' \
        "$WORKFLOW")" -eq 4 ] || {
        printf 'Pages must re-resolve protected main before build, upload, deployment, and after deployment\n'
        return 1
    }
    final_gate_line="$(grep -nF 'Final current-main gate before Pages publication' \
        "$WORKFLOW" | cut -d: -f1)"
    deploy_action_line="$(grep -nF 'actions/deploy-pages@' "$WORKFLOW" | cut -d: -f1)"
    [ -n "$final_gate_line" ] && [ -n "$deploy_action_line" ] \
        && [ "$final_gate_line" -lt "$deploy_action_line" ] || {
            printf 'Pages deploy action is not preceded by the final exact-main gate\n'
            return 1
        }

    # Job-scoped permissions keep build code away from the OIDC token while
    # the deployment job has only the three capabilities it needs.
    [ "$(grep -Fc 'id-token: write' "$WORKFLOW")" -eq 1 ] || {
        printf 'Pages OIDC write permission must exist only on the deploy job\n'
        return 1
    }
}

live_publication_gates_require_six_maintenance_interlocks() {
    local gate
    for gate in "$LIVE_DASHBOARD_GATE" "$LIVE_EXPLORER_GATE"; do
        grep -Fq 'network.auditMaintenanceInterlock({ resolver, fetchImpl: fetch })' "$gate" || {
            printf 'active Pages live gate does not fetch maintenance evidence: %s\n' "$gate"
            return 1
        }
    done
    grep -Fq 'dashboard.activeFleetPublicationError(config, fleet, maintenanceAudit)' \
        "$LIVE_DASHBOARD_GATE" || {
            printf 'dashboard live gate does not pass maintenance evidence to publication policy\n'
            return 1
        }
    if ! grep -Fq 'assert.equal(maintenanceAudit.state, "healthy"' "$LIVE_EXPLORER_GATE" \
        || ! grep -Fq 'assert.equal(maintenanceAudit.samples.length, 6' "$LIVE_EXPLORER_GATE"; then
            printf 'explorer live gate does not require six fresh healthy maintenance interlocks\n'
            return 1
    fi
}

site_builder_is_reproducible_and_complete() (
    local sandbox output first_hash second_hash
    sandbox="$(mktemp -d)"
    output="$sandbox/site"
    trap 'rm -rf -- "$sandbox"' EXIT

    (
        cd "$REPO_ROOT" || exit 1
        ARC_PUBLIC_SITE_SHA=contract-test "$BUILDER" "$output"
    ) || return 1

    for path in \
        index.html tailwind.css app.css app.js .nojekyll deployed-commit.txt SHA256SUMS \
        explorer/index.html explorer/app.js explorer/styles.css \
        shared/frontend/arc-network.js shared/frontend/arc-network.json
    do
        [ -e "$output/$path" ] || {
            printf 'assembled public site is missing %s\n' "$path"
            return 1
        }
    done
    for forbidden in wallet/index.html docs/STATUS.md; do
        [ ! -e "$output/$forbidden" ] || {
            printf 'assembled public site contains forbidden legacy surface %s\n' "$forbidden"
            return 1
        }
    done
    grep -Fq 'content="./shared/frontend/arc-network.json"' "$output/index.html" || return 1
    grep -Fq 'src="./shared/frontend/arc-network.js' "$output/index.html" || return 1
    ! grep -Fq '../shared/frontend' "$output/index.html" || return 1
    [ "$(grep -Fc 'href="./explorer/"' "$output/index.html")" -eq 2 ] || {
        printf 'assembled root dashboard does not contain both prefix-safe explorer links\n'
        return 1
    }
    ! grep -Fq '../explorer/' "$output/index.html" || return 1
    grep -Fq './explorer/#/tx/' "$output/app.js" || return 1
    ! grep -Fq '../explorer/' "$output/app.js" || return 1
    [ "$(cat "$output/deployed-commit.txt")" = contract-test ] || return 1
    (cd "$output" && shasum -a 256 -c SHA256SUMS) >/dev/null || return 1
    first_hash="$(shasum -a 256 "$output/SHA256SUMS")"

    (
        cd "$REPO_ROOT" || exit 1
        ARC_PUBLIC_SITE_SHA=contract-test "$BUILDER" "$output"
    ) || return 1
    second_hash="$(shasum -a 256 "$output/SHA256SUMS")"
    [ "$first_hash" = "$second_hash" ] || {
        printf 'public-site manifest changed across identical assemblies\n'
        return 1
    }
)

site_builder_rejects_broad_targets() {
    (
        cd "$REPO_ROOT" || exit 1
        "$BUILDER" / >/dev/null 2>&1
    ) && {
        printf 'public-site builder accepted filesystem root as output\n'
        return 1
    }
    (
        cd "$REPO_ROOT" || exit 1
        "$BUILDER" . >/dev/null 2>&1
    ) && {
        printf 'public-site builder accepted repository root as output\n'
        return 1
    }
    return 0
}

site_builder_preserves_unowned_and_last_good_outputs() (
    local sandbox protected output before_hash after_hash
    sandbox="$(mktemp -d)"
    protected="$sandbox/home-like"
    output="$sandbox/site"
    trap 'rm -rf -- "$sandbox"' EXIT
    mkdir -p "$protected"
    printf 'do not delete\n' > "$protected/important.txt"

    (
        cd "$REPO_ROOT" || exit 1
        "$BUILDER" "$protected" >/dev/null 2>&1
    ) && {
        printf 'public-site builder replaced an unowned non-empty directory\n'
        return 1
    }
    [ "$(cat "$protected/important.txt")" = 'do not delete' ] || {
        printf 'public-site builder altered an unowned directory on refusal\n'
        return 1
    }

    (
        cd "$REPO_ROOT" || exit 1
        ARC_PUBLIC_SITE_SHA=contract-test "$BUILDER" "$output" >/dev/null
    ) || return 1
    before_hash="$(shasum -a 256 "$output/SHA256SUMS")"
    (
        cd "$sandbox" || exit 1
        ARC_PUBLIC_SITE_SHA=must-not-publish "$BUILDER" "$output" >/dev/null 2>&1
    ) && {
        printf 'public-site builder unexpectedly succeeded without source inputs\n'
        return 1
    }
    after_hash="$(shasum -a 256 "$output/SHA256SUMS")"
    [ "$before_hash" = "$after_hash" ] || {
        printf 'failed public-site assembly replaced the last good output\n'
        return 1
    }
)

run_test 'Pages workflow uses pinned, secret-free deployment actions' pages_workflow_is_pinned_and_self_contained
run_test 'every GitHub Action is pinned to a full immutable SHA' every_action_is_commit_sha_pinned
run_test 'active Pages gates require six healthy maintenance interlocks' live_publication_gates_require_six_maintenance_interlocks
run_test 'public console assembles reproducibly without unsafe legacy surfaces' site_builder_is_reproducible_and_complete
run_test 'public-console builder refuses destructive broad targets' site_builder_rejects_broad_targets
run_test 'public-console builder preserves unowned and last-good outputs' site_builder_preserves_unowned_and_last_good_outputs

finish_tests
