#!/usr/bin/env bash
# arc-compile — Compile Solidity contracts for ARC Chain deployment
set -euo pipefail

VERSION="0.1.0"
OPTIMIZE_RUNS=200

# ── Colors ──────────────────────────────────────────────────────────────────
RED='\033[0;31m'
GREEN='\033[0;32m'
YELLOW='\033[1;33m'
CYAN='\033[0;36m'
BOLD='\033[1m'
NC='\033[0m' # No Color

info()  { echo -e "${CYAN}[arc-compile]${NC} $*"; }
ok()    { echo -e "${GREEN}[arc-compile]${NC} $*"; }
warn()  { echo -e "${YELLOW}[arc-compile]${NC} $*"; }
err()   { echo -e "${RED}[arc-compile]${NC} $*" >&2; }

# ── Usage ───────────────────────────────────────────────────────────────────
usage() {
  cat <<EOF
${BOLD}arc-compile v${VERSION}${NC} — Compile Solidity contracts for ARC Chain

${BOLD}Usage:${NC}
  arc-compile <file.sol> [options]

${BOLD}Options:${NC}
  -o, --output <dir>    Output directory (default: build/ next to source)
  -r, --runs <n>        Optimizer runs (default: ${OPTIMIZE_RUNS})
  --no-optimize         Disable optimizer
  -h, --help            Show this help

${BOLD}Examples:${NC}
  arc-compile contracts/standards/ARC20.sol
  arc-compile contracts/standards/ARC20.sol -o out/
  arc-compile contracts/standards/ARC20.sol -r 1000

${BOLD}Contract address derivation:${NC}
  addr = keccak256(rlp([sender, nonce]))[12:]
  CREATE2: addr = keccak256(0xff ++ sender ++ salt ++ keccak256(initcode))[12:]
EOF
}

# ── Check solc ──────────────────────────────────────────────────────────────
check_solc() {
  if ! command -v solc &>/dev/null; then
    err "solc (Solidity compiler) is not installed."
    echo ""
    echo "Install instructions:"
    echo ""
    echo "  macOS (Homebrew):"
    echo "    brew tap ethereum/ethereum"
    echo "    brew install solidity"
    echo ""
    echo "  Linux (apt):"
    echo "    sudo add-apt-repository ppa:ethereum/ethereum"
    echo "    sudo apt-get update && sudo apt-get install solc"
    echo ""
    echo "  Linux (snap):"
    echo "    sudo snap install solc"
    echo ""
    echo "  Any platform (solc-select — recommended for version management):"
    echo "    pip install solc-select"
    echo "    solc-select install 0.8.24"
    echo "    solc-select use 0.8.24"
    echo ""
    echo "  Or download from: https://github.com/ethereum/solidity/releases"
    exit 1
  fi

  local solc_version
  solc_version=$(solc --version | grep -oE '[0-9]+\.[0-9]+\.[0-9]+' | head -1)
  info "Using solc v${solc_version}"
}

# ── Parse arguments ─────────────────────────────────────────────────────────
OPTIMIZE=true
OUTPUT_DIR=""
SOURCE_FILE=""

while [[ $# -gt 0 ]]; do
  case "$1" in
    -h|--help)
      usage
      exit 0
      ;;
    -o|--output)
      OUTPUT_DIR="$2"
      shift 2
      ;;
    -r|--runs)
      OPTIMIZE_RUNS="$2"
      shift 2
      ;;
    --no-optimize)
      OPTIMIZE=false
      shift
      ;;
    -*)
      err "Unknown option: $1"
      usage
      exit 1
      ;;
    *)
      if [[ -n "$SOURCE_FILE" ]]; then
        err "Multiple source files not supported. Got: $SOURCE_FILE and $1"
        exit 1
      fi
      SOURCE_FILE="$1"
      shift
      ;;
  esac
done

# ── Validate input ──────────────────────────────────────────────────────────
if [[ -z "$SOURCE_FILE" ]]; then
  err "No source file specified."
  echo ""
  usage
  exit 1
fi

if [[ ! -f "$SOURCE_FILE" ]]; then
  err "File not found: $SOURCE_FILE"
  exit 1
fi

if [[ "$SOURCE_FILE" != *.sol ]]; then
  err "File must have .sol extension: $SOURCE_FILE"
  exit 1
fi

# ── Resolve paths ───────────────────────────────────────────────────────────
SOURCE_DIR=$(dirname "$(realpath "$SOURCE_FILE")")
SOURCE_NAME=$(basename "$SOURCE_FILE" .sol)

if [[ -z "$OUTPUT_DIR" ]]; then
  OUTPUT_DIR="${SOURCE_DIR}/build"
fi

mkdir -p "$OUTPUT_DIR"

info "Compiling ${BOLD}${SOURCE_NAME}.sol${NC}"
info "Source:  $SOURCE_FILE"
info "Output:  $OUTPUT_DIR/"

# ── Build solc command ──────────────────────────────────────────────────────
SOLC_ARGS=(
  --abi
  --bin
  --overwrite
  -o "$OUTPUT_DIR"
)

if [[ "$OPTIMIZE" == true ]]; then
  SOLC_ARGS+=(--optimize --optimize-runs "$OPTIMIZE_RUNS")
  info "Optimizer: enabled (${OPTIMIZE_RUNS} runs)"
else
  info "Optimizer: disabled"
fi

# Add base path for import resolution
REPO_ROOT=$(git rev-parse --show-toplevel 2>/dev/null || dirname "$(realpath "$SOURCE_FILE")")
SOLC_ARGS+=(--base-path "$REPO_ROOT")

SOLC_ARGS+=("$SOURCE_FILE")

# ── Compile ─────────────────────────────────────────────────────────────────
info "Running: solc ${SOLC_ARGS[*]}"
echo ""

if solc "${SOLC_ARGS[@]}"; then
  echo ""
  ok "Compilation successful!"
  echo ""

  # List outputs
  for ext in abi bin; do
    for f in "$OUTPUT_DIR"/*."$ext"; do
      if [[ -f "$f" ]]; then
        local_name=$(basename "$f")
        size=$(wc -c < "$f" | tr -d ' ')
        echo -e "  ${GREEN}*${NC} ${local_name} (${size} bytes)"
      fi
    done
  done

  echo ""
  echo -e "${BOLD}Contract address derivation:${NC}"
  echo "  CREATE:  addr = keccak256(rlp([sender_address, nonce]))[12:]"
  echo "  CREATE2: addr = keccak256(0xff ++ deployer ++ salt ++ keccak256(init_code))[12:]"
  echo ""
  echo -e "${BOLD}Deploy to ARC Chain:${NC}"
  echo "  Local:   cast send --rpc-url http://localhost:9090/eth --private-key \$KEY --create \$(cat ${OUTPUT_DIR}/${SOURCE_NAME}.bin)"
  echo "  Testnet: cast send --rpc-url http://testnet.arc.ai:9090/eth --private-key \$KEY --create \$(cat ${OUTPUT_DIR}/${SOURCE_NAME}.bin)"
else
  echo ""
  err "Compilation failed."
  exit 1
fi
