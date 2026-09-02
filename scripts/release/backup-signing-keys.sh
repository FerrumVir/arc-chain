#!/usr/bin/env bash
# Create a create-only, encrypted, immediately restore-tested backup of the two
# long-lived ARC release signing keys. The passphrase is supplied only through
# ARC_SIGNING_BACKUP_PASSPHRASE and is removed from the child-process
# environment before GnuPG is invoked.
set +x
set -Eeuo pipefail
ulimit -c 0
umask 077
unset BASH_ENV ENV CDPATH

die() {
    printf 'signing-key backup: %s\n' "$*" >&2
    exit 1
}

[ "$#" -eq 3 ] || die 'usage: backup-signing-keys.sh TAURI_KEY MANIFEST_KEY OUTPUT.tar.gpg'

TAURI_KEY="$1"
MANIFEST_KEY="$2"
OUTPUT="$3"
PASSPHRASE="${ARC_SIGNING_BACKUP_PASSPHRASE:-}"
unset ARC_SIGNING_BACKUP_PASSPHRASE

[ -n "$PASSPHRASE" ] || die 'ARC_SIGNING_BACKUP_PASSPHRASE is required'
[ "${#PASSPHRASE}" -ge 32 ] || die 'backup passphrase must contain at least 32 characters'
for command_name in cmp gpg install mktemp shasum ssh-keygen tar; do
    command -v "$command_name" >/dev/null 2>&1 || die "required command is unavailable: $command_name"
done
for key in "$TAURI_KEY" "$MANIFEST_KEY"; do
    [ -f "$key" ] && [ ! -L "$key" ] && [ -s "$key" ] \
        || die "private key is missing, empty, or symlinked: $key"
done

case "$OUTPUT" in
    /*.tar.gpg) ;;
    *) die 'output must be an absolute .tar.gpg path' ;;
esac
[ ! -e "$OUTPUT" ] || die "refusing to replace existing backup: $OUTPUT"
[ ! -L "$(dirname -- "$OUTPUT")" ] || die 'output parent must not be a symlink'
[ -d "$(dirname -- "$OUTPUT")" ] || die 'output parent does not exist'

WORK_DIR="$(mktemp -d "${TMPDIR:-/tmp}/arc-signing-backup.XXXXXXXX")"
cleanup() {
    if [ -n "${WORK_DIR:-}" ] && [ -d "$WORK_DIR" ]; then
        case "$WORK_DIR" in
            "${TMPDIR:-/tmp}"/arc-signing-backup.*)
                chmod -R u+rwX -- "$WORK_DIR" 2>/dev/null || true
                rm -rf -- "$WORK_DIR"
                ;;
            *) printf 'signing-key backup: refusing unsafe cleanup path: %s\n' "$WORK_DIR" >&2 ;;
        esac
    fi
}
trap cleanup EXIT HUP INT TERM

PAYLOAD="$WORK_DIR/payload"
RESTORED="$WORK_DIR/restored"
mkdir -p -- "$PAYLOAD" "$RESTORED"
install -m 600 -- "$TAURI_KEY" "$PAYLOAD/tauri-updater.key"
install -m 600 -- "$MANIFEST_KEY" "$PAYLOAD/release-manifest-ed25519"
ssh-keygen -y -f "$MANIFEST_KEY" > "$PAYLOAD/release-manifest-ed25519.pub"
chmod 600 "$PAYLOAD/release-manifest-ed25519.pub"

(
    cd "$PAYLOAD"
    shasum -a 256 tauri-updater.key release-manifest-ed25519 \
        > KEY-SHA256SUMS
    chmod 600 KEY-SHA256SUMS
    tar -cf "$WORK_DIR/signing-keys.tar" \
        KEY-SHA256SUMS \
        release-manifest-ed25519 \
        release-manifest-ed25519.pub \
        tauri-updater.key
)

printf '%s' "$PASSPHRASE" | gpg \
    --batch \
    --yes \
    --no-symkey-cache \
    --pinentry-mode loopback \
    --passphrase-fd 0 \
    --symmetric \
    --cipher-algo AES256 \
    --s2k-mode 3 \
    --s2k-digest-algo SHA512 \
    --s2k-count 65011712 \
    --compress-algo none \
    --output "$WORK_DIR/signing-keys.tar.gpg" \
    "$WORK_DIR/signing-keys.tar"
install -m 600 -- "$WORK_DIR/signing-keys.tar.gpg" "$OUTPUT"

# A backup is not accepted until the just-written ciphertext restores and both
# private keys compare byte-for-byte with their sources.
printf '%s' "$PASSPHRASE" | gpg \
    --batch \
    --yes \
    --no-symkey-cache \
    --pinentry-mode loopback \
    --passphrase-fd 0 \
    --decrypt \
    --output "$WORK_DIR/restored.tar" \
    "$OUTPUT"
unset PASSPHRASE
tar -xf "$WORK_DIR/restored.tar" -C "$RESTORED"
(
    cd "$RESTORED"
    shasum -a 256 -c KEY-SHA256SUMS >/dev/null
)
cmp -s -- "$TAURI_KEY" "$RESTORED/tauri-updater.key" \
    || die 'restored Tauri updater key differs from its source'
cmp -s -- "$MANIFEST_KEY" "$RESTORED/release-manifest-ed25519" \
    || die 'restored manifest key differs from its source'
ssh-keygen -y -f "$RESTORED/release-manifest-ed25519" \
    > "$WORK_DIR/restored-manifest.pub"
cmp -s -- "$PAYLOAD/release-manifest-ed25519.pub" "$WORK_DIR/restored-manifest.pub" \
    || die 'restored manifest key has a different public identity'

OUTPUT_SHA256="$(shasum -a 256 "$OUTPUT" | awk '{print $1}')"
printf 'signing-key backup: restore-tested %s (sha256 %s)\n' "$OUTPUT" "$OUTPUT_SHA256"
