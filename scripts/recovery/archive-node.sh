#!/usr/bin/env bash
# Runs on one legacy validator after an explicit fleet archive authorization.
set -Eeuo pipefail
umask 077

MANIFEST_SHA256="${1:-}"
NODE_NAME="${2:-}"

printf '%s\n' "$MANIFEST_SHA256" | grep -Eq '^[0-9a-f]{64}$' || {
    printf 'archive node: manifest must be exactly 64 lowercase hexadecimal characters\n' >&2
    exit 2
}
case "$NODE_NAME" in nyc|lax|ams|lhr|nrt|sgp) ;; *) printf 'archive node: invalid node name\n' >&2; exit 2 ;; esac

for command_name in tar zstd sha256sum sync pgrep systemctl stat grep; do
    command -v "$command_name" >/dev/null 2>&1 || {
        printf 'archive node: required command is missing: %s\n' "$command_name" >&2
        exit 2
    }
done
[ -d /root/arc-chain ] || { printf 'archive node: /root/arc-chain is missing\n' >&2; exit 1; }

ARCHIVE_ROOT="/root/arc-recovery-archive/$MANIFEST_SHA256"
ARCHIVE="$ARCHIVE_ROOT/legacy-$NODE_NAME.tar.zst"
CHECKSUM="$ARCHIVE.sha256"
INVENTORY="$ARCHIVE_ROOT/legacy-$NODE_NAME.inventory"
mkdir -p -- "$ARCHIVE_ROOT"
chmod 700 -- /root/arc-recovery-archive "$ARCHIVE_ROOT"

# A clean SIGTERM gives the node its normal WAL flush path. The supervisor has
# KillMode=process on legacy hosts, so stopping it does not stop arc-node.
systemctl stop arc-self-heal.service 2>/dev/null || true
systemctl stop arc-node.service 2>/dev/null || true
if pgrep -x arc-node >/dev/null 2>&1; then
    pkill -TERM -x arc-node
fi
for _ in $(seq 1 120); do
    pgrep -x arc-node >/dev/null 2>&1 || break
    sleep 0.5
done
if pgrep -x arc-node >/dev/null 2>&1; then
    printf 'archive node: arc-node did not complete a clean shutdown; refusing SIGKILL and archive\n' >&2
    exit 1
fi
sync

if [ -s "$ARCHIVE" ] && [ -s "$CHECKSUM" ]; then
    (cd "$ARCHIVE_ROOT" && sha256sum --check "${CHECKSUM##*/}") >/dev/null || {
        printf 'archive node: existing archive checksum failed; refusing replacement\n' >&2
        exit 1
    }
    printf 'archive node: existing verified archive node=%s bytes=%s sha256=%s\n' \
        "$NODE_NAME" "$(stat -c %s "$ARCHIVE")" "$(cut -d' ' -f1 "$CHECKSUM")"
    exit 0
fi
if [ -e "$ARCHIVE" ] || [ -e "$CHECKSUM" ]; then
    printf 'archive node: partial archive exists; refusing replacement\n' >&2
    exit 1
fi

{
    printf 'manifest_sha256=%s\n' "$MANIFEST_SHA256"
    printf 'node=%s\n' "$NODE_NAME"
    printf 'hostname=%s\n' "$(hostname)"
    printf 'archived_at=%s\n' "$(date -u +%Y-%m-%dT%H:%M:%SZ)"
    printf 'kernel=%s\n' "$(uname -srmo)"
    printf 'arc_chain_bytes=%s\n' "$(du -s -B1 /root/arc-chain | cut -f1)"
    if [ -x /root/arc-chain/target/release/arc-node ]; then
        printf 'binary_sha256=%s\n' "$(sha256sum /root/arc-chain/target/release/arc-node | cut -d' ' -f1)"
    fi
    if git -C /root/arc-chain rev-parse --verify HEAD >/dev/null 2>&1; then
        printf 'source_commit=%s\n' "$(git -C /root/arc-chain rev-parse HEAD)"
    fi
} > "$INVENTORY"

paths=(root/arc-chain)
for optional in \
    etc/systemd/system/arc-self-heal.service \
    etc/systemd/system/arc-node.service \
    root/.config/systemd/user/arc-self-heal.service \
    root/.config/systemd/user/arc-node.service; do
    [ -e "/$optional" ] && paths+=("$optional")
done

TEMP_ARCHIVE="$ARCHIVE.partial"
trap 'rm -f -- "$TEMP_ARCHIVE"' EXIT
tar --create --zstd --numeric-owner --acls --xattrs --sparse --one-file-system \
    --file "$TEMP_ARCHIVE" --directory / "${paths[@]}"
sync
mv -- "$TEMP_ARCHIVE" "$ARCHIVE"
(cd "$ARCHIVE_ROOT" && sha256sum "${ARCHIVE##*/}" > "${CHECKSUM##*/}")
chmod 600 -- "$ARCHIVE" "$CHECKSUM" "$INVENTORY"

printf 'archive node: created node=%s bytes=%s sha256=%s\n' \
    "$NODE_NAME" "$(stat -c %s "$ARCHIVE")" "$(cut -d' ' -f1 "$CHECKSUM")"
