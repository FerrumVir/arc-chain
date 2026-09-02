#!/usr/bin/env bash
# Format only first-party workspace packages.
#
# `cargo fmt --all` also walks the vendored Stwo path dependencies. Rewriting
# audited third-party source creates a huge, unauditable diff and makes future
# vendor updates harder to verify. This wrapper asks Cargo for the actual ARC
# workspace members and formats each package explicitly.
set -euo pipefail

MODE="${1:-format}"
case "$MODE" in
    format|--check) ;;
    *)
        echo "usage: $0 [--check]" >&2
        exit 2
        ;;
esac

REPO_ROOT="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
cd "$REPO_ROOT"

PACKAGES_FILE="$(mktemp)"
trap 'rm -f "$PACKAGES_FILE"' EXIT

cargo metadata --locked --no-deps --format-version 1 |
    python3 -c '
import json
import sys

metadata = json.load(sys.stdin)
members = set(metadata["workspace_members"])
names = sorted(package["name"] for package in metadata["packages"] if package["id"] in members)
print("\n".join(names))
' > "$PACKAGES_FILE"

if [ ! -s "$PACKAGES_FILE" ]; then
    echo "error: Cargo reported no workspace packages" >&2
    exit 1
fi

while IFS= read -r package; do
    echo "rustfmt: $package"
    if [ "$MODE" = "--check" ]; then
        cargo fmt --package "$package" -- --check
    else
        cargo fmt --package "$package"
    fi
done < "$PACKAGES_FILE"
