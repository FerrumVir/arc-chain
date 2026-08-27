#!/usr/bin/env bash
# Runs on one legacy validator after an explicit fleet archive authorization.
# The uploaded artifact is deliberately a public-chain recovery bundle: the
# canonical state WAL, exact binary/genesis inputs, and read-only tip/DAG
# evidence. It excludes private validator material, service environments,
# build caches, models, Git objects, and the multi-gigabyte non-canonical DAG
# trace. Every excluded byte remains untouched on the legacy disk.
set -Eeuo pipefail
umask 077

MANIFEST_SHA256="${1:-}"
NODE_NAME="${2:-}"

printf '%s\n' "$MANIFEST_SHA256" | grep -Eq '^[0-9a-f]{64}$' || {
    printf 'archive node: manifest must be exactly 64 lowercase hexadecimal characters\n' >&2
    exit 2
}
case "$NODE_NAME" in nyc|lax|ams|lhr|nrt|sgp) ;; *) printf 'archive node: invalid node name\n' >&2; exit 2 ;; esac

for command_name in tar zstd sha256sum sync pgrep systemctl stat grep curl find; do
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
EVIDENCE_ROOT="$ARCHIVE_ROOT/legacy-$NODE_NAME-evidence"
mkdir -p -- "$ARCHIVE_ROOT"
chmod 700 -- /root/arc-recovery-archive "$ARCHIVE_ROOT"

# Idempotent retries must not stop an already archived node or replace any
# evidence. A complete existing bundle is accepted only after re-verification.
if [ -s "$ARCHIVE" ] && [ -s "$CHECKSUM" ] && [ -s "$INVENTORY" ]; then
    (cd "$ARCHIVE_ROOT" && sha256sum --check "${CHECKSUM##*/}") >/dev/null || {
        printf 'archive node: existing archive checksum failed; refusing replacement\n' >&2
        exit 1
    }
    printf 'archive node: existing verified archive node=%s bytes=%s sha256=%s\n' \
        "$NODE_NAME" "$(stat -c %s "$ARCHIVE")" "$(cut -d' ' -f1 "$CHECKSUM")"
    exit 0
fi
if [ -e "$ARCHIVE" ] || [ -e "$CHECKSUM" ] || [ -e "$INVENTORY" ] || [ -e "$EVIDENCE_ROOT" ]; then
    printf 'archive node: partial archive or evidence exists; refusing replacement\n' >&2
    exit 1
fi

# Capture public facts before the legacy process stops. These are supporting
# evidence only; the recovery exporter independently derives the authoritative
# H/hash/root from the stopped state WAL.
mkdir -- "$EVIDENCE_ROOT"
chmod 700 -- "$EVIDENCE_ROOT"
capture_public_endpoint() {
    endpoint="$1"
    destination="$2"
    for port in 9090 9944; do
        if curl --fail --silent --show-error --max-time 10 \
            "http://127.0.0.1:$port$endpoint" > "$destination.partial"; then
            mv -- "$destination.partial" "$destination"
            chmod 600 -- "$destination"
            return 0
        fi
    done
    rm -f -- "$destination.partial"
    printf 'archive node: could not capture required public endpoint %s\n' "$endpoint" >&2
    return 1
}
capture_public_endpoint /health "$EVIDENCE_ROOT/health.json"
capture_public_endpoint /block/latest "$EVIDENCE_ROOT/latest-block.json"
capture_public_endpoint /sync/dag_state "$EVIDENCE_ROOT/dag-state.json"
capture_public_endpoint /validators "$EVIDENCE_ROOT/validators.json"

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

[ -s /root/arc-chain/arc-data/state.wal ] || {
    printf 'archive node: stopped canonical state WAL is missing or empty\n' >&2
    exit 1
}

{
    printf 'manifest_sha256=%s\n' "$MANIFEST_SHA256"
    printf 'node=%s\n' "$NODE_NAME"
    printf 'hostname=%s\n' "$(hostname)"
    printf 'archived_at=%s\n' "$(date -u +%Y-%m-%dT%H:%M:%SZ)"
    printf 'kernel=%s\n' "$(uname -srmo)"
    printf 'archive_scope=public-chain-recovery-bundle-v1\n'
    printf 'state_wal_bytes=%s\n' "$(stat -c %s /root/arc-chain/arc-data/state.wal)"
    printf 'state_wal_sha256=%s\n' "$(sha256sum /root/arc-chain/arc-data/state.wal | cut -d' ' -f1)"
    if [ -d /root/arc-chain/arc-data/dag-wal ]; then
        printf 'legacy_dag_wal_bytes_retained_on_node=%s\n' "$(du -s -B1 /root/arc-chain/arc-data/dag-wal | cut -f1)"
        printf 'legacy_dag_wal_segments_retained_on_node=%s\n' "$(find /root/arc-chain/arc-data/dag-wal -maxdepth 1 -type f -name 'wal-*.bin' | wc -l)"
    fi
    printf 'excluded_private_material=true\n'
    printf 'excluded_build_models_git_and_dag_trace=true\n'
    if [ -x /root/arc-chain/target/release/arc-node ]; then
        printf 'binary_sha256=%s\n' "$(sha256sum /root/arc-chain/target/release/arc-node | cut -d' ' -f1)"
    fi
    if git -C /root/arc-chain rev-parse --verify HEAD >/dev/null 2>&1; then
        printf 'source_commit=%s\n' "$(git -C /root/arc-chain rev-parse HEAD)"
    fi
} > "$INVENTORY"

paths=(
    root/arc-chain/arc-data/state.wal
    "${EVIDENCE_ROOT#/}"
)
for optional in \
    root/arc-chain/genesis.toml \
    root/arc-chain/deploy/config/genesis.toml \
    root/arc-chain/testnet-seeds.txt \
    root/arc-chain/Cargo.lock \
    root/arc-chain/rust-toolchain.toml \
    root/arc-chain/target/release/arc-node; do
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
