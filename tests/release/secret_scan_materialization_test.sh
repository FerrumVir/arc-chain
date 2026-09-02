#!/usr/bin/env bash
set -Eeuo pipefail

TEST_DIR="$(CDPATH='' cd -- "$(dirname -- "$0")" && pwd)"
MATERIALIZER="$TEST_DIR/materialize_releasable_tree.py"
TEMP_ROOT="$(mktemp -d "${TMPDIR:-/tmp}/arc-secret-materialization.XXXXXX")"
cleanup() { rm -rf -- "$TEMP_ROOT"; }
trap cleanup EXIT HUP INT TERM

REPOSITORY="$TEMP_ROOT/repository"
INDEX_TREE="$TEMP_ROOT/index-tree"
WORKTREE="$TEMP_ROOT/worktree"
mkdir -p "$REPOSITORY"
git -C "$REPOSITORY" init -q
git -C "$REPOSITORY" config user.email 'release-test@arc.local'
git -C "$REPOSITORY" config user.name 'ARC release test'

printf 'safe committed bytes\n' >"$REPOSITORY/config.txt"
git -C "$REPOSITORY" add config.txt
git -C "$REPOSITORY" commit -qm initial

# Construct a credential-shaped marker at runtime so this regression test does
# not itself place a detector fixture in the release tree.
staged_marker="$(printf '%s%s' 'staged-credential-' 'must-be-scanned')"
printf '%s\n' "$staged_marker" >"$REPOSITORY/config.txt"
git -C "$REPOSITORY" add config.txt
printf 'safe working-copy replacement\n' >"$REPOSITORY/config.txt"

python3 "$MATERIALIZER" --index "$REPOSITORY" "$INDEX_TREE"
python3 "$MATERIALIZER" --worktree "$REPOSITORY" "$WORKTREE"

grep -Fqx "$staged_marker" "$INDEX_TREE/config.txt" || {
    printf 'index materialization lost the staged credential bytes\n' >&2
    exit 1
}
grep -Fqx 'safe working-copy replacement' "$WORKTREE/config.txt" || {
    printf 'worktree materialization did not preserve current bytes\n' >&2
    exit 1
}
if grep -Fq "$staged_marker" "$WORKTREE/config.txt"; then
    printf 'worktree fixture unexpectedly contains the staged bytes\n' >&2
    exit 1
fi

printf 'staged/index and working-copy materialization remain independent\n'
