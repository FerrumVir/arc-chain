#!/usr/bin/env bash
# Freeze all six legacy validators, create local immutable archives, and copy
# them to the operator-authorized Google Drive remote. Dry-run is the default.
set -Eeuo pipefail
umask 077

SCRIPT_DIR="$(CDPATH='' cd -- "$(dirname -- "${BASH_SOURCE[0]}")" && pwd)"
REMOTE_HELPER="$SCRIPT_DIR/archive-node.sh"
MANIFEST_SHA256=""
EXECUTE=false
DRIVE_REMOTE="${ARC_RECOVERY_DRIVE_REMOTE:-arc-drive:ARC Chain Recovery}"
SSH_USER="${ARC_RECOVERY_SSH_USER:-root}"

NODES=(
    'nyc=149.28.32.76'
    'lax=140.82.16.112'
    'ams=136.244.109.1'
    'lhr=104.238.171.11'
    'nrt=202.182.107.41'
    'sgp=149.28.153.31'
)

usage() {
    cat <<'EOF'
Usage:
  archive-fleet-to-drive.sh --manifest SHA256 [--plan]
  ARC_RECOVERY_GO='GO SHA256' archive-fleet-to-drive.sh --manifest SHA256 --execute

The default --plan is read-only. --execute stops all six legacy arc-node
processes cleanly, creates one archive on each validator without deleting any
legacy data, and uploads those archives to the configured Google Drive remote.
EOF
}

while [ "$#" -gt 0 ]; do
    case "$1" in
        --manifest) [ "$#" -ge 2 ] || { usage >&2; exit 2; }; MANIFEST_SHA256="$2"; shift 2 ;;
        --execute) EXECUTE=true; shift ;;
        --plan) EXECUTE=false; shift ;;
        -h|--help) usage; exit 0 ;;
        *) printf 'archive fleet: unknown option: %s\n' "$1" >&2; usage >&2; exit 2 ;;
    esac
done

printf '%s\n' "$MANIFEST_SHA256" | grep -Eq '^[0-9a-f]{64}$' || {
    printf 'archive fleet: --manifest must be exactly 64 lowercase hexadecimal characters\n' >&2
    exit 2
}
[ -x "$REMOTE_HELPER" ] || { printf 'archive fleet: helper is missing or not executable\n' >&2; exit 2; }
for command_name in ssh scp rclone awk sed grep mktemp; do
    command -v "$command_name" >/dev/null 2>&1 || {
        printf 'archive fleet: required command is missing: %s\n' "$command_name" >&2
        exit 2
    }
done

printf 'ARC legacy archive plan\n'
printf '  manifest: %s\n' "$MANIFEST_SHA256"
printf '  drive:    %s/%s\n' "$DRIVE_REMOTE" "$MANIFEST_SHA256"
printf '  nodes:    %s\n' "${#NODES[@]}"

for entry in "${NODES[@]}"; do
    name="${entry%%=*}"
    host="${entry#*=}"
    ssh -o BatchMode=yes -o ConnectTimeout=8 -o StrictHostKeyChecking=yes \
        "$SSH_USER@$host" -- \
        'test -d /root/arc-chain && command -v zstd >/dev/null && command -v sha256sum >/dev/null'
    printf '  ready:    %s %s\n' "$name" "$host"
done
rclone lsd "$DRIVE_REMOTE" >/dev/null

if [ "$EXECUTE" != true ]; then
    printf 'archive fleet: PLAN ONLY; no service, file, or Drive object was changed\n'
    exit 0
fi
EXPECTED_GO="GO $MANIFEST_SHA256"
if [ "${ARC_RECOVERY_GO:-}" != "$EXPECTED_GO" ]; then
    printf 'archive fleet: execution requires ARC_RECOVERY_GO=%q\n' "$EXPECTED_GO" >&2
    exit 2
fi

LOG_ROOT="$(mktemp -d)"
trap 'rm -rf -- "$LOG_ROOT"' EXIT
pids=()
names=()
for entry in "${NODES[@]}"; do
    name="${entry%%=*}"
    host="${entry#*=}"
    (
        scp -q -o BatchMode=yes -o StrictHostKeyChecking=yes \
            "$REMOTE_HELPER" "$SSH_USER@$host:/root/.arc-recovery-archive-node.sh"
        ssh -o BatchMode=yes -o StrictHostKeyChecking=yes "$SSH_USER@$host" -- \
            /root/.arc-recovery-archive-node.sh "$MANIFEST_SHA256" "$name"
    ) > "$LOG_ROOT/$name.log" 2>&1 &
    pids+=("$!")
    names+=("$name")
done

archive_failed=0
for index in "${!pids[@]}"; do
    if wait "${pids[$index]}"; then
        sed -n '1,20p' "$LOG_ROOT/${names[$index]}.log"
    else
        printf 'archive fleet: node archive failed: %s\n' "${names[$index]}" >&2
        sed -n '1,80p' "$LOG_ROOT/${names[$index]}.log" >&2
        archive_failed=1
    fi
done
[ "$archive_failed" -eq 0 ] || {
    printf 'archive fleet: at least one node failed; legacy nodes remain stopped and no data was deleted\n' >&2
    exit 1
}

DESTINATION="$DRIVE_REMOTE/$MANIFEST_SHA256"
rclone mkdir "$DESTINATION"
for entry in "${NODES[@]}"; do
    name="${entry%%=*}"
    host="${entry#*=}"
    source_dir=":sftp:/root/arc-recovery-archive/$MANIFEST_SHA256"
    ssh_command="ssh -o BatchMode=yes -o StrictHostKeyChecking=yes $SSH_USER@$host"
    for suffix in tar.zst tar.zst.sha256 inventory; do
        filename="legacy-$name.$suffix"
        rclone copyto "$source_dir/$filename" "$DESTINATION/$filename" \
            --sftp-ssh "$ssh_command" --checksum --metadata --retries 5 --low-level-retries 20
    done
    rclone check "$source_dir" "$DESTINATION" \
        --sftp-ssh "$ssh_command" --checksum --one-way \
        --include "legacy-$name.tar.zst" \
        --include "legacy-$name.tar.zst.sha256" \
        --include "legacy-$name.inventory"
    printf 'archive fleet: uploaded and hash-checked %s\n' "$name"
done

printf 'archive fleet: COMPLETE manifest=%s destination=%s\n' \
    "$MANIFEST_SHA256" "$DESTINATION"
