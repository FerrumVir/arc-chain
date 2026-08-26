#!/usr/bin/env bash
set -Eeuo pipefail

die() {
    printf 'updater signature gate: %s\n' "$*" >&2
    exit 1
}

if [ "$#" -ne 2 ]; then
    die "usage: $0 TAURI_CONFIG RELEASE_FILES_DIR"
fi

TAURI_CONFIG="$1"
RELEASE_FILES_DIR="$2"
SCRIPT_DIR="$(CDPATH='' cd -- "$(dirname -- "$0")" && pwd)"
REPO_ROOT="$(CDPATH='' cd -- "$SCRIPT_DIR/../.." && pwd)"
VERIFIER_MANIFEST="$SCRIPT_DIR/tauri-updater-verifier/Cargo.toml"
VERIFIER_TARGET="$REPO_ROOT/target/release-tools/tauri-updater-verifier"
VERIFIER_BIN="$VERIFIER_TARGET/debug/tauri-updater-verifier"

[ -f "$TAURI_CONFIG" ] || die "missing Tauri configuration: $TAURI_CONFIG"
[ -d "$RELEASE_FILES_DIR" ] || die "missing release directory: $RELEASE_FILES_DIR"
command -v cargo >/dev/null 2>&1 || die "cargo is required"
command -v python3 >/dev/null 2>&1 || die "python3 is required"

TAURI_UPDATER_PUBLIC_KEY="$({
    python3 - "$TAURI_CONFIG" <<'PY'
import json
import pathlib
import sys

config_path = pathlib.Path(sys.argv[1])
config = json.loads(config_path.read_text(encoding="utf-8"))
public_key = config.get("plugins", {}).get("updater", {}).get("pubkey")
if not isinstance(public_key, str) or not public_key.strip():
    raise SystemExit(f"missing plugins.updater.pubkey in {config_path}")
print(public_key.strip())
PY
} 2>&1)" || die "$TAURI_UPDATER_PUBLIC_KEY"

CARGO_TARGET_DIR="$VERIFIER_TARGET" \
    cargo build --quiet --locked --manifest-path "$VERIFIER_MANIFEST"
[ -x "$VERIFIER_BIN" ] || die "verifier binary was not built: $VERIFIER_BIN"

signature_count=0
while IFS= read -r -d '' signature_path; do
    payload_path="${signature_path%.sig}"
    [ -s "$payload_path" ] || die "signature has no nonempty payload: $signature_path"
    [ -s "$signature_path" ] || die "empty updater signature: $signature_path"
    "$VERIFIER_BIN" "$TAURI_UPDATER_PUBLIC_KEY" "$payload_path" "$signature_path" \
        || die "signature does not match embedded updater key: $signature_path"
    signature_count=$((signature_count + 1))
done < <(find "$RELEASE_FILES_DIR" -type f -name '*.sig' -print0)

[ "$signature_count" -eq 4 ] \
    || die "expected exactly four signed updater payloads, found $signature_count"

printf 'Verified %s updater payloads against %s\n' "$signature_count" "$TAURI_CONFIG"
