#!/usr/bin/env bash
set -uo pipefail

TEST_DIR="$(CDPATH='' cd -- "$(dirname -- "$0")" && pwd)"
REPO_ROOT="$(CDPATH='' cd -- "$TEST_DIR/../.." && pwd)"
# shellcheck source=/dev/null
. "$TEST_DIR/helpers/testlib.sh"

WORKFLOW="$REPO_ROOT/.github/workflows/deploy-explorer.yml"
BUILDER="$REPO_ROOT/scripts/build-public-site.sh"

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
    for pin in \
        'actions/checkout@11d5960a326750d5838078e36cf38b85af677262' \
        'actions/setup-node@49933ea5288caeca8642d1e84afbd3f7d6820020' \
        'actions/configure-pages@983d7736d9b0ae728b81ab479565c72886d7745b' \
        'actions/upload-pages-artifact@56afc609e74202658d3ffba0e8f6dda462b719fa' \
        'actions/deploy-pages@d6db90164ac5ed86f2b6aed7e0febac5b3c0c03e'
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
        && grep -Fq './scripts/build-public-site.sh public-site' "$WORKFLOW"
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
        index.html tailwind.css .nojekyll deployed-commit.txt SHA256SUMS \
        explorer/index.html explorer/app.js explorer/styles.css \
        wallet/index.html docs/STATUS.md
    do
        [ -e "$output/$path" ] || {
            printf 'assembled public site is missing %s\n' "$path"
            return 1
        }
    done
    [ "$(cat "$output/deployed-commit.txt")" = contract-test ] || return 1
    (cd / && shasum -a 256 -c "$output/SHA256SUMS") >/dev/null || return 1
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

run_test 'Pages workflow uses pinned, secret-free deployment actions' pages_workflow_is_pinned_and_self_contained
run_test 'every GitHub Action is pinned to a full immutable SHA' every_action_is_commit_sha_pinned
run_test 'public console assembles reproducibly with every product surface' site_builder_is_reproducible_and_complete
run_test 'public-console builder refuses destructive broad targets' site_builder_rejects_broad_targets

finish_tests
