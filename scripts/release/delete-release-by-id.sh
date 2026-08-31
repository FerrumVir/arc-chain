#!/usr/bin/env bash
# Delete only a GitHub release object created by the current publisher.
# The protected tag is intentionally not addressed or removed.
set -Eeuo pipefail

if [ "$#" -ne 2 ]; then
    printf 'usage: %s OWNER/REPOSITORY RELEASE_ID\n' "$0" >&2
    exit 2
fi

repository="$1"
release_id="$2"

if [[ ! "$repository" =~ ^[A-Za-z0-9_.-]+/[A-Za-z0-9_.-]+$ ]]; then
    printf 'invalid GitHub repository: %s\n' "$repository" >&2
    exit 2
fi
if [[ ! "$release_id" =~ ^[1-9][0-9]*$ ]]; then
    printf 'invalid positive release id: %s\n' "$release_id" >&2
    exit 2
fi

gh api --method DELETE "repos/$repository/releases/$release_id"
