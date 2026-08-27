#!/usr/bin/env bash
# Assemble the dependency-free ARC public console into one immutable directory.
set -Eeuo pipefail
umask 022

OUTPUT_DIR="${1:-public-site}"
SOURCE_SHA="${ARC_PUBLIC_SITE_SHA:-$(git rev-parse HEAD)}"

die() {
    printf 'public site: %s\n' "$*" >&2
    exit 1
}

case "$OUTPUT_DIR" in
    ''|/|.|..|"$PWD") die "refusing unsafe output directory: $OUTPUT_DIR" ;;
esac
case "/$OUTPUT_DIR/" in
    *'/../'*|*'/./'*) die "output directory must not contain dot segments: $OUTPUT_DIR" ;;
esac
[ ! -L "${OUTPUT_DIR%/}" ] || die "refusing symlinked output directory: $OUTPUT_DIR"

for required in \
    dashboard/index.html dashboard/tailwind.css dashboard/app.css dashboard/app.js \
    explorer/index.html explorer/app.js explorer/styles.css \
    shared/frontend/arc-network.js shared/frontend/arc-network.json \
    wallet/index.html docs/STATUS.md; do
    [ -s "$required" ] || die "required source is missing or empty: $required"
done

rm -rf -- "$OUTPUT_DIR"
mkdir -p -- \
    "$OUTPUT_DIR/explorer" "$OUTPUT_DIR/wallet" "$OUTPUT_DIR/docs" \
    "$OUTPUT_DIR/shared/frontend"

# The source dashboard lives one directory below the shared resolver. At the
# public-site root it is a sibling instead, so rewrite only these two audited
# relative URLs. This keeps the same source usable in repository-local tests
# and on GitHub project Pages (which may itself have a path prefix).
sed \
    -e 's#content="../shared/frontend/arc-network.json"#content="./shared/frontend/arc-network.json"#' \
    -e 's#src="../shared/frontend/arc-network.js#src="./shared/frontend/arc-network.js#' \
    dashboard/index.html > "$OUTPUT_DIR/index.html"
grep -Fq 'content="./shared/frontend/arc-network.json"' "$OUTPUT_DIR/index.html" \
    || die "dashboard network-config URL rewrite failed"
grep -Fq 'src="./shared/frontend/arc-network.js' "$OUTPUT_DIR/index.html" \
    || die "dashboard resolver URL rewrite failed"
if grep -Fq '../shared/frontend' "$OUTPUT_DIR/index.html"; then
    die "dashboard retained a source-tree-only shared URL"
fi

cp -- dashboard/tailwind.css dashboard/app.css dashboard/app.js "$OUTPUT_DIR/"
cp -- explorer/index.html explorer/app.js explorer/styles.css "$OUTPUT_DIR/explorer/"
cp -- shared/frontend/arc-network.js shared/frontend/arc-network.json \
    "$OUTPUT_DIR/shared/frontend/"
cp -- wallet/index.html "$OUTPUT_DIR/wallet/index.html"
cp -- docs/STATUS.md "$OUTPUT_DIR/docs/STATUS.md"

# The console is already a complete static site; do not let Jekyll transform it.
: > "$OUTPUT_DIR/.nojekyll"
printf '%s\n' "$SOURCE_SHA" > "$OUTPUT_DIR/deployed-commit.txt"

find "$OUTPUT_DIR" -type f ! -name SHA256SUMS -print0 \
    | LC_ALL=C sort -z \
    | xargs -0 shasum -a 256 > "$OUTPUT_DIR/SHA256SUMS"

printf 'public site: assembled %s files from %s\n' \
    "$(find "$OUTPUT_DIR" -type f | wc -l | tr -d ' ')" "$SOURCE_SHA"
