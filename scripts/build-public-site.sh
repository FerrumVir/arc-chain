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
    shared/frontend/arc-network.js shared/frontend/arc-network.json; do
    [ -s "$required" ] || die "required source is missing or empty: $required"
done

rm -rf -- "$OUTPUT_DIR"
mkdir -p -- \
    "$OUTPUT_DIR/explorer" \
    "$OUTPUT_DIR/shared/frontend"

# The source dashboard lives under dashboard/, but the deployed dashboard lives
# at the public-site root. Rewrite every audited path whose relative base changes
# during that move. Relative `./` links preserve the GitHub project-Pages prefix.
sed \
    -e 's#content="../shared/frontend/arc-network.json"#content="./shared/frontend/arc-network.json"#' \
    -e 's#src="../shared/frontend/arc-network.js#src="./shared/frontend/arc-network.js#' \
    -e 's#href="../explorer/#href="./explorer/#g' \
    dashboard/index.html > "$OUTPUT_DIR/index.html"
grep -Fq 'content="./shared/frontend/arc-network.json"' "$OUTPUT_DIR/index.html" \
    || die "dashboard network-config URL rewrite failed"
grep -Fq 'src="./shared/frontend/arc-network.js' "$OUTPUT_DIR/index.html" \
    || die "dashboard resolver URL rewrite failed"
if grep -Fq '../shared/frontend' "$OUTPUT_DIR/index.html"; then
    die "dashboard retained a source-tree-only shared URL"
fi
grep -Fq 'href="./explorer/"' "$OUTPUT_DIR/index.html" \
    || die "dashboard explorer URL rewrite failed"
if grep -Fq '../explorer/' "$OUTPUT_DIR/index.html"; then
    die "dashboard retained a source-tree-only explorer URL"
fi

cp -- dashboard/tailwind.css dashboard/app.css "$OUTPUT_DIR/"
sed 's#\.\./explorer/#./explorer/#g' dashboard/app.js > "$OUTPUT_DIR/app.js"
grep -Fq './explorer/#/tx/' "$OUTPUT_DIR/app.js" \
    || die "dashboard receipt explorer URL rewrite failed"
if grep -Fq '../explorer/' "$OUTPUT_DIR/app.js"; then
    die "dashboard retained a source-tree-only receipt URL"
fi
cp -- explorer/index.html explorer/app.js explorer/styles.css "$OUTPUT_DIR/explorer/"
cp -- shared/frontend/arc-network.js shared/frontend/arc-network.json \
    "$OUTPUT_DIR/shared/frontend/"

# The legacy wallet is deliberately excluded: it stores a private key in
# localStorage, imports live remote code, and calls plaintext/retired RPC
# routes. Historical STATUS.md is also not a live product surface. Neither may
# be reintroduced without a dedicated security and current-truth contract.

# The console is already a complete static site; do not let Jekyll transform it.
: > "$OUTPUT_DIR/.nojekyll"
printf '%s\n' "$SOURCE_SHA" > "$OUTPUT_DIR/deployed-commit.txt"

find "$OUTPUT_DIR" -type f ! -name SHA256SUMS -print0 \
    | LC_ALL=C sort -z \
    | xargs -0 shasum -a 256 > "$OUTPUT_DIR/SHA256SUMS"

printf 'public site: assembled %s files from %s\n' \
    "$(find "$OUTPUT_DIR" -type f | wc -l | tr -d ' ')" "$SOURCE_SHA"
