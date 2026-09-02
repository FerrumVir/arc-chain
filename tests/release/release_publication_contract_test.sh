#!/usr/bin/env bash
set -Eeuo pipefail

REPO_ROOT="$(CDPATH='' cd -- "$(dirname -- "$0")/../.." && pwd)"
DELETE_HELPER="$REPO_ROOT/scripts/release/delete-release-by-id.sh"
WORKDIR="$(mktemp -d "${TMPDIR:-/tmp}/arc-release-cleanup.XXXXXX")"
cleanup() {
    rm -rf -- "$WORKDIR"
}
trap cleanup EXIT HUP INT TERM

python3 "$REPO_ROOT/tests/release/test_verify_github_release.py"

mkdir -p "$WORKDIR/bin"
cat > "$WORKDIR/bin/gh" <<'MOCK'
#!/usr/bin/env bash
set -Eeuo pipefail
printf '%s\n' "$@" > "$ARC_TEST_GH_LOG"
MOCK
chmod +x "$WORKDIR/bin/gh"

export ARC_TEST_GH_LOG="$WORKDIR/gh.log"
PATH="$WORKDIR/bin:$PATH" "$DELETE_HELPER" FerrumVir/arc-chain 12345

expected="$WORKDIR/expected.log"
cat > "$expected" <<'EXPECTED'
api
--method
DELETE
repos/FerrumVir/arc-chain/releases/12345
EXPECTED
cmp "$expected" "$ARC_TEST_GH_LOG"

if PATH="$WORKDIR/bin:$PATH" "$DELETE_HELPER" FerrumVir/arc-chain 'v0.8.0' \
    >"$WORKDIR/invalid.out" 2>"$WORKDIR/invalid.err"; then
    printf 'cleanup helper accepted a tag in place of an immutable release id\n' >&2
    exit 1
fi
if [ -s "$WORKDIR/invalid.out" ] || ! grep -Fq 'invalid positive release id' "$WORKDIR/invalid.err"; then
    printf 'cleanup helper did not fail closed on a non-numeric release id\n' >&2
    exit 1
fi

if grep -Eq 'cleanup-tag|git/refs/tags|releases/tags' "$DELETE_HELPER"; then
    printf 'release cleanup helper can address or delete the protected tag\n' >&2
    exit 1
fi

printf 'release publication verifier and release-only cleanup contract passed\n'
