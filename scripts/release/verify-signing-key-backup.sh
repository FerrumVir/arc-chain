#!/usr/bin/env bash
# Independently restore-test a downloaded encrypted ARC signing-key backup
# against the public identities committed in the exact release source tree.
set +x
set -Eeuo pipefail
ulimit -c 0
umask 077
unset BASH_ENV ENV CDPATH

die() {
    printf 'signing-key backup verification: %s\n' "$*" >&2
    exit 1
}

[ "$#" -eq 3 ] || die 'usage: verify-signing-key-backup.sh /absolute/backup.tar.gpg EXPECTED_MAIN_SHA EXPECTED_CIPHERTEXT_SHA256'
BACKUP="$1"
EXPECTED_MAIN_SHA="$2"
EXPECTED_CIPHERTEXT_SHA256="$3"
PASSPHRASE="${ARC_SIGNING_BACKUP_PASSPHRASE:-}"
unset ARC_SIGNING_BACKUP_PASSPHRASE

case "$BACKUP" in
    /*.tar.gpg) ;;
    *) die 'backup path must be an absolute .tar.gpg path' ;;
esac
[ -f "$BACKUP" ] && [ ! -L "$BACKUP" ] && [ -s "$BACKUP" ] \
    || die 'backup must be a nonempty regular non-symlink file'
[ "$(wc -c < "$BACKUP" | tr -d ' ')" -le 2097152 ] \
    || die 'encrypted backup exceeds the 2 MiB safety limit'
[[ "$EXPECTED_MAIN_SHA" =~ ^[0-9a-f]{40}$ ]] \
    || die 'expected main commit must be one full lowercase SHA'
[[ "$EXPECTED_CIPHERTEXT_SHA256" =~ ^[0-9a-f]{64}$ ]] \
    || die 'expected ciphertext SHA-256 must be 64 lowercase hexadecimal characters'
[ -n "$PASSPHRASE" ] && [ "${#PASSPHRASE}" -ge 32 ] \
    || die 'ARC_SIGNING_BACKUP_PASSPHRASE must contain at least 32 characters'

for command_name in cargo cmp git gpg npm python3 shasum ssh-keygen tar; do
    command -v "$command_name" >/dev/null 2>&1 \
        || die "required command is unavailable: $command_name"
done

REPO_ROOT="$(cd "$(dirname "${BASH_SOURCE[0]}")/../.." && pwd)"
ACTUAL_MAIN_SHA="$(git -C "$REPO_ROOT" rev-parse HEAD)"
[ "$ACTUAL_MAIN_SHA" = "$EXPECTED_MAIN_SHA" ] \
    || die "source checkout $ACTUAL_MAIN_SHA is not expected protected main $EXPECTED_MAIN_SHA"
for trust_path in \
    scripts/release/verify-signing-key-backup.sh \
    release/arc-release-allowed-signers \
    desktop/src-tauri/tauri.conf.json \
    desktop/package.json \
    desktop/package-lock.json \
    tests/release/tauri-updater-verifier/Cargo.toml \
    tests/release/tauri-updater-verifier/Cargo.lock \
    tests/release/tauri-updater-verifier/src/main.rs; do
    git -C "$REPO_ROOT" diff --quiet "$EXPECTED_MAIN_SHA" -- "$trust_path" \
        || die "local trust input differs from expected protected main: $trust_path"
done

BACKUP_SHA256="$(shasum -a 256 "$BACKUP" | awk '{print $1}')"
[ "$BACKUP_SHA256" = "$EXPECTED_CIPHERTEXT_SHA256" ] \
    || die "ciphertext SHA-256 mismatch: expected $EXPECTED_CIPHERTEXT_SHA256, got $BACKUP_SHA256"

WORK_DIR="$(mktemp -d "${TMPDIR:-/tmp}/arc-signing-verify.XXXXXXXX")"
cleanup() {
    unset PASSPHRASE
    if [ -n "${WORK_DIR:-}" ] && [ -d "$WORK_DIR" ]; then
        case "$WORK_DIR" in
            "${TMPDIR:-/tmp}"/arc-signing-verify.*)
                chmod -R u+rwX -- "$WORK_DIR" 2>/dev/null || true
                rm -rf -- "$WORK_DIR"
                ;;
            *) printf 'signing-key backup verification: refusing unsafe cleanup path: %s\n' "$WORK_DIR" >&2 ;;
        esac
    fi
}
trap cleanup EXIT HUP INT TERM

# Install the exact locked signer and compile the isolated public-key verifier
# before any private bytes are decrypted. No dependency resolution, builds, or
# network-capable package command runs while plaintext signing keys exist.
npm ci --prefix "$REPO_ROOT/desktop" --ignore-scripts >/dev/null
TAURI_SIGNER="$REPO_ROOT/desktop/node_modules/@tauri-apps/cli/tauri.js"
[ -f "$TAURI_SIGNER" ] && [ ! -L "$TAURI_SIGNER" ] && [ -x "$TAURI_SIGNER" ] \
    || die 'exact locked Tauri signer entrypoint is unavailable'
VERIFIER_TARGET="$WORK_DIR/verifier-target"
CARGO_TARGET_DIR="$VERIFIER_TARGET" cargo build --quiet --locked \
    --manifest-path "$REPO_ROOT/tests/release/tauri-updater-verifier/Cargo.toml"
VERIFIER_BINARY="$VERIFIER_TARGET/debug/tauri-updater-verifier"
[ -x "$VERIFIER_BINARY" ] || die 'isolated updater verifier did not build'
TAURI_PUBLIC_KEY="$(python3 - "$REPO_ROOT/desktop/src-tauri/tauri.conf.json" <<'PY'
import json
import sys
from pathlib import Path
value = json.loads(Path(sys.argv[1]).read_text(encoding="utf-8"))["plugins"]["updater"]["pubkey"]
if not isinstance(value, str) or not value.strip():
    raise SystemExit("embedded Tauri updater public key is unavailable")
print(value.strip())
PY
)"

printf '%s' "$PASSPHRASE" | gpg \
    --batch \
    --yes \
    --no-symkey-cache \
    --pinentry-mode loopback \
    --passphrase-fd 0 \
    --decrypt \
    --output "$WORK_DIR/signing-keys.tar" \
    "$BACKUP"
unset PASSPHRASE

EXPECTED_MEMBERS="$(printf '%s\n' \
    KEY-SHA256SUMS \
    release-manifest-ed25519 \
    release-manifest-ed25519.pub \
    tauri-updater.key \
    | LC_ALL=C sort)"
ACTUAL_MEMBERS="$(tar -tf "$WORK_DIR/signing-keys.tar" | LC_ALL=C sort)"
[ "$ACTUAL_MEMBERS" = "$EXPECTED_MEMBERS" ] \
    || die 'decrypted archive membership differs from the four-file contract'

mkdir -m 700 "$WORK_DIR/restored"
tar -xf "$WORK_DIR/signing-keys.tar" -C "$WORK_DIR/restored" \
    KEY-SHA256SUMS \
    release-manifest-ed25519 \
    release-manifest-ed25519.pub \
    tauri-updater.key
(
    cd "$WORK_DIR/restored"
    shasum -a 256 -c KEY-SHA256SUMS >/dev/null
)

ssh-keygen -y -f "$WORK_DIR/restored/release-manifest-ed25519" \
    > "$WORK_DIR/derived-manifest.pub"
cmp -s "$WORK_DIR/derived-manifest.pub" "$WORK_DIR/restored/release-manifest-ed25519.pub" \
    || die 'restored manifest private and public keys disagree'
cut -d ' ' -f 3- "$REPO_ROOT/release/arc-release-allowed-signers" \
    > "$WORK_DIR/allowed-manifest.pub"
cmp -s "$WORK_DIR/derived-manifest.pub" "$WORK_DIR/allowed-manifest.pub" \
    || die 'restored manifest key does not match the committed release trust root'

printf '%s\n' 'ARC downloaded-backup manifest canary v1' > "$WORK_DIR/manifest-canary"
ssh-keygen -Y sign \
    -f "$WORK_DIR/restored/release-manifest-ed25519" \
    -n arc-release-manifest-v1 \
    "$WORK_DIR/manifest-canary"
ssh-keygen -Y verify \
    -f "$REPO_ROOT/release/arc-release-allowed-signers" \
    -I arc-release \
    -n arc-release-manifest-v1 \
    -s "$WORK_DIR/manifest-canary.sig" \
    < "$WORK_DIR/manifest-canary" \
    || die 'restored manifest key failed the committed signing canary'

printf '%s\n' 'ARC downloaded-backup updater canary v1' > "$WORK_DIR/tauri-canary"
"$TAURI_SIGNER" signer sign \
    --private-key-path "$WORK_DIR/restored/tauri-updater.key" \
    --password "" \
    "$WORK_DIR/tauri-canary"
test -s "$WORK_DIR/tauri-canary.sig" || die 'restored updater key did not produce a signature'

"$VERIFIER_BINARY" \
    "$TAURI_PUBLIC_KEY" \
    "$WORK_DIR/tauri-canary" \
    "$WORK_DIR/tauri-canary.sig"

printf 'signing-key backup verification: PASS %s (main %s, sha256 %s)\n' \
    "$BACKUP" "$EXPECTED_MAIN_SHA" "$BACKUP_SHA256"
