#!/usr/bin/env bash
# Bind the desktop lab commit to the exact base tree plus desktop lab files only.
#
# THROWAWAY LAB FILE. The desktop lab branch may carry several commits; what matters is the TREE:
# the base commit's tree plus ADDED files under wave0-lab-desktop/ and the one desktop lab workflow.
# Nothing in the base tree is modified, renamed or removed.
set -Eeuo pipefail

cfg="${1:-wave0-lab-desktop/config.json}"
base_commit="$(jq -er .base_commit "$cfg")"
base_tree="$(jq -er .base_tree "$cfg")"
workflow=".github/workflows/wave0-lab-desktop.yml"

test "$(git rev-parse HEAD)" = "${GITHUB_SHA:-$(git rev-parse HEAD)}"
git fetch --no-tags --depth=1 origin "$base_commit"
test "$(git rev-parse "$base_commit")" = "$base_commit"
test "$(git rev-parse "$base_commit^{tree}")" = "$base_tree"

diff_file="$(mktemp)"
git diff --name-status --no-renames "$base_commit" HEAD | sort > "$diff_file"
cat "$diff_file"
bad="$(awk -F'\t' -v w="$workflow" '!($1 == "A" && ($2 == w || index($2, "wave0-lab-desktop/") == 1))' "$diff_file")"
if [ -n "$bad" ]; then
    printf '::error::the desktop lab tree must be the base tree plus added desktop lab files only; offending entries:\n%s\n' "$bad" >&2
    exit 1
fi
for required in "$workflow" "wave0-lab-desktop/config.json"; do
    if ! grep -qxF -- "$(printf 'A\t%s' "$required")" "$diff_file"; then
        echo "::error::$required is missing from the desktop lab tree" >&2
        exit 1
    fi
done
echo "Desktop lab commit $(git rev-parse HEAD): tree = base $base_commit (tree $base_tree) + $(wc -l < "$diff_file" | tr -d ' ') added desktop lab files."
