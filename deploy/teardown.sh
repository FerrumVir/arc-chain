#!/usr/bin/env bash
# =========================================================================
# ARC Chain — Testnet Teardown
# =========================================================================
# Deletes all 4 Hetzner Cloud testnet servers.
# Requires confirmation before proceeding.
#
# Usage:
#   ./teardown.sh          # Interactive confirmation
#   ./teardown.sh --force  # Skip confirmation (CI/automation)
# =========================================================================

set -euo pipefail

SCRIPT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
NODE_COUNT=4
FORCE=false

# Colors
RED='\033[0;31m'
GREEN='\033[0;32m'
YELLOW='\033[1;33m'
BOLD='\033[1m'
NC='\033[0m'

for arg in "$@"; do
    case "$arg" in
        --force|-f) FORCE=true ;;
    esac
done

if ! command -v hcloud &>/dev/null; then
    echo -e "${RED}ERROR: hcloud CLI not found.${NC}"
    exit 1
fi

# -- List servers to delete ------------------------------------------------

echo -e "${BOLD}ARC Chain Testnet — Teardown${NC}"
echo "============================================"
echo ""

SERVERS_FOUND=0
for i in $(seq 0 $((NODE_COUNT - 1))); do
    NAME="arc-node-${i}"
    if hcloud server describe "$NAME" &>/dev/null; then
        IP=$(hcloud server ip "$NAME" 2>/dev/null || echo "unknown")
        echo -e "  ${RED}DELETE${NC}  $NAME ($IP)"
        SERVERS_FOUND=$((SERVERS_FOUND + 1))
    else
        echo -e "  ${YELLOW}SKIP${NC}    $NAME (not found)"
    fi
done

echo ""

if [ "$SERVERS_FOUND" -eq 0 ]; then
    echo -e "${GREEN}No testnet servers found. Nothing to do.${NC}"
    exit 0
fi

# -- Confirmation ----------------------------------------------------------

if [ "$FORCE" != true ]; then
    echo -e "${YELLOW}This will permanently delete $SERVERS_FOUND server(s) and all their data.${NC}"
    echo -n "Type 'yes' to confirm: "
    read -r CONFIRM
    if [ "$CONFIRM" != "yes" ]; then
        echo "Aborted."
        exit 0
    fi
    echo ""
fi

# -- Delete servers --------------------------------------------------------

for i in $(seq 0 $((NODE_COUNT - 1))); do
    NAME="arc-node-${i}"
    if hcloud server describe "$NAME" &>/dev/null; then
        echo -n "  Deleting $NAME..."
        hcloud server delete "$NAME" --quiet 2>/dev/null || hcloud server delete "$NAME"
        echo -e " ${GREEN}done${NC}"
    fi
done

# -- Cleanup local state ---------------------------------------------------

IP_FILE="${SCRIPT_DIR}/.node-ips"
if [ -f "$IP_FILE" ]; then
    rm -f "$IP_FILE"
    echo ""
    echo "  Removed .node-ips"
fi

echo ""
echo -e "${GREEN}Testnet torn down. All servers deleted.${NC}"
