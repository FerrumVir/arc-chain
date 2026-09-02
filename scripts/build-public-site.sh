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

# Assemble beside the final destination and rename only after every contract
# succeeds. Replacing a non-empty directory requires a sidecar receipt from an
# earlier successful run, so a mistyped path cannot erase unrelated files.
OUTPUT_BASENAME="$(basename -- "${OUTPUT_DIR%/}")"
case "$OUTPUT_BASENAME" in
    ''|.|..|*[!A-Za-z0-9._-]*) die "unsafe output-directory basename: $OUTPUT_BASENAME" ;;
esac
OUTPUT_PARENT="$(CDPATH='' cd -- "$(dirname -- "${OUTPUT_DIR%/}")" && pwd -P)" \
    || die "output-directory parent does not exist"
FINAL_OUTPUT_DIR="$OUTPUT_PARENT/$OUTPUT_BASENAME"
REPOSITORY_ROOT="$(pwd -P)"
[ "$FINAL_OUTPUT_DIR" != "$REPOSITORY_ROOT" ] \
    || die "refusing repository root as output directory"
[ ! -L "$FINAL_OUTPUT_DIR" ] || die "refusing symlinked output directory: $OUTPUT_DIR"
[ ! -e "$FINAL_OUTPUT_DIR" ] || [ -d "$FINAL_OUTPUT_DIR" ] \
    || die "output path exists but is not a directory: $OUTPUT_DIR"

OUTPUT_OWNER_RECEIPT="$OUTPUT_PARENT/.${OUTPUT_BASENAME}.arc-public-site-output-owner"
EXPECTED_OWNER_RECEIPT="arc-public-site-output-v1:$FINAL_OUTPUT_DIR"
if [ -e "$OUTPUT_OWNER_RECEIPT" ]; then
    [ -f "$OUTPUT_OWNER_RECEIPT" ] && [ ! -L "$OUTPUT_OWNER_RECEIPT" ] \
        || die "invalid output-directory ownership receipt: $OUTPUT_OWNER_RECEIPT"
    [ "$(cat -- "$OUTPUT_OWNER_RECEIPT")" = "$EXPECTED_OWNER_RECEIPT" ] \
        || die "output-directory ownership receipt does not match destination"
fi
if [ -d "$FINAL_OUTPUT_DIR" ] \
    && [ -n "$(find "$FINAL_OUTPUT_DIR" -mindepth 1 -maxdepth 1 -print -quit)" ] \
    && [ ! -f "$OUTPUT_OWNER_RECEIPT" ]; then
    die "refusing to replace non-empty output directory without ARC ownership receipt"
fi

for required in \
    dashboard/index.html dashboard/tailwind.css dashboard/app.css dashboard/app.js \
    explorer/index.html explorer/app.js explorer/styles.css \
    shared/frontend/arc-network.js shared/frontend/arc-network.json; do
    [ -s "$required" ] || die "required source is missing or empty: $required"
done

OUTPUT_STAGE_DIR="$(mktemp -d "$OUTPUT_PARENT/.${OUTPUT_BASENAME}.arc-public-site-stage.XXXXXX")" \
    || die "failed to create public-site staging directory"
RETIRED_OUTPUT_DIR=""
cleanup_output_stage() {
    if [ -n "${RETIRED_OUTPUT_DIR:-}" ] && [ -d "$RETIRED_OUTPUT_DIR" ]; then
        if [ ! -e "$FINAL_OUTPUT_DIR" ]; then
            mv -- "$RETIRED_OUTPUT_DIR" "$FINAL_OUTPUT_DIR" || true
        elif [ ! -d "${OUTPUT_STAGE_DIR:-}" ]; then
            rm -rf -- "$RETIRED_OUTPUT_DIR"
        fi
    fi
    if [ -n "${OUTPUT_STAGE_DIR:-}" ] && [ -d "$OUTPUT_STAGE_DIR" ]; then
        rm -rf -- "$OUTPUT_STAGE_DIR"
    fi
}
trap cleanup_output_stage EXIT
OUTPUT_DIR="$OUTPUT_STAGE_DIR"
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

(
    cd "$OUTPUT_DIR"
    # shellcheck disable=SC2094 # find explicitly excludes the redirection target.
    find . -type f ! -name SHA256SUMS -print0 \
        | LC_ALL=C sort -z \
        | xargs -0 shasum -a 256 > SHA256SUMS
)

OWNER_RECEIPT_STAGE="$(mktemp "$OUTPUT_PARENT/.${OUTPUT_BASENAME}.arc-public-site-owner.XXXXXX")" \
    || die "failed to stage output-directory ownership receipt"
printf '%s\n' "$EXPECTED_OWNER_RECEIPT" > "$OWNER_RECEIPT_STAGE"
chmod 600 "$OWNER_RECEIPT_STAGE"
mv -f -- "$OWNER_RECEIPT_STAGE" "$OUTPUT_OWNER_RECEIPT"

if [ -d "$FINAL_OUTPUT_DIR" ]; then
    RETIRED_OUTPUT_DIR="$(mktemp -d "$OUTPUT_PARENT/.${OUTPUT_BASENAME}.arc-public-site-retired.XXXXXX")" \
        || die "failed to reserve retired-output path"
    rmdir -- "$RETIRED_OUTPUT_DIR"
    mv -- "$FINAL_OUTPUT_DIR" "$RETIRED_OUTPUT_DIR"
fi
if ! mv -- "$OUTPUT_DIR" "$FINAL_OUTPUT_DIR"; then
    if [ -n "$RETIRED_OUTPUT_DIR" ] && [ -d "$RETIRED_OUTPUT_DIR" ]; then
        mv -- "$RETIRED_OUTPUT_DIR" "$FINAL_OUTPUT_DIR" || true
    fi
    die "failed to publish staged public-site directory"
fi
OUTPUT_STAGE_DIR=""
if [ -n "$RETIRED_OUTPUT_DIR" ] && [ -d "$RETIRED_OUTPUT_DIR" ]; then
    rm -rf -- "$RETIRED_OUTPUT_DIR"
fi
OUTPUT_DIR="$FINAL_OUTPUT_DIR"

printf 'public site: assembled %s files from %s\n' \
    "$(find "$OUTPUT_DIR" -type f | wc -l | tr -d ' ')" "$SOURCE_SHA"
