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
    dashboard/index.html dashboard/tailwind.css \
    explorer/index.html explorer/app.js explorer/styles.css \
    wallet/index.html docs/STATUS.md; do
    [ -s "$required" ] || die "required source is missing or empty: $required"
done

rm -rf -- "$OUTPUT_DIR"
mkdir -p -- "$OUTPUT_DIR/explorer" "$OUTPUT_DIR/wallet" "$OUTPUT_DIR/docs"

cp -- dashboard/index.html "$OUTPUT_DIR/index.html"
cp -- dashboard/tailwind.css "$OUTPUT_DIR/tailwind.css"
cp -- explorer/index.html explorer/app.js explorer/styles.css "$OUTPUT_DIR/explorer/"
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
