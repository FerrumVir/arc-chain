#!/usr/bin/env bash
# ARC Proof Kit for macOS and Linux: check, on this computer, that the public
# SmolLM3-3B model gives the published answer bit for bit (docs/proof-kit.md).
#
# Usage:
#   scripts/proof-kit/run.sh [--bin PATH | --release TAG] [kit options]
#
# Kit options (passed to `arc-modern proof`):
#   --dry-run                print the exact JSON a submission would send; send nothing
#   --submit --endpoint URL  send the result to a Hash Wall, after you see it and type yes
#   --dir DIR                where the model and results live (default: your cache folder)
#   --backends LIST          cpu-scalar,cpu-simd (default: every kernel this CPU has)
#   --threads N  --keep-source  --no-island  --force  --gpu
#
# Where arc-modern comes from (first match wins):
#   --bin PATH, or ARC_MODERN_BIN      a binary you already have
#   --release TAG, or ARC_PROOF_KIT_RELEASE
#                                      that GitHub release's arc-modern asset, checked
#                                      against the release's signed SHA256SUMS
#   otherwise                          built from this checkout with cargo
#
# Without --submit nothing about this computer is sent anywhere. The only
# downloads are the pinned model files from Hugging Face (and, with --release,
# the binary from GitHub).
set -Eeuo pipefail

readonly REPO_URL="https://github.com/FerrumVir/arc-chain"
# The release-manifest signing key and namespace install.sh trusts.
readonly RELEASE_SIGNER='arc-release namespaces="arc-release-manifest-v1" ssh-ed25519 AAAAC3NzaC1lZDI1NTE5AAAAIPs2NAiDRXit9EM96A2GdXZgRqvXtl0lvryEAEAEjQfY arc-release-manifest-v1'

SCRIPT_DIR="$(CDPATH='' cd -- "$(dirname -- "$0")" && pwd -P)"
REPO_ROOT="$(CDPATH='' cd -- "$SCRIPT_DIR/../.." && pwd -P)"

usage() {
  sed -n '2,24p' "$0" | sed 's/^# \{0,1\}//'
}

die() {
  printf 'proof kit: %s\n' "$*" >&2
  exit 1
}

bin="${ARC_MODERN_BIN:-}"
release="${ARC_PROOF_KIT_RELEASE:-}"
kit_dir="${ARC_PROOF_KIT_DIR:-}"
kit_args=()
while [ "$#" -gt 0 ]; do
  case "$1" in
    --bin) bin="${2:?--bin needs a path}"; shift 2 ;;
    --release) release="${2:?--release needs a tag}"; shift 2 ;;
    -h | --help) usage; exit 0 ;;
    --dir)
      kit_dir="${2:?--dir needs a value}"
      kit_args+=("$1" "$2")
      shift 2
      ;;
    *) kit_args+=("$1"); shift ;;
  esac
done

if [ -z "$kit_dir" ]; then
  case "$(uname -s)" in
    Darwin) kit_dir="$HOME/Library/Caches/arc-proof-kit" ;;
    *) kit_dir="${XDG_CACHE_HOME:-$HOME/.cache}/arc-proof-kit" ;;
  esac
fi

sha256_of() {
  if command -v sha256sum >/dev/null 2>&1; then
    sha256sum "$1" | awk '{ print $1 }'
  else
    shasum -a 256 "$1" | awk '{ print $1 }'
  fi
}

release_platform() {
  case "$(uname -s):$(uname -m)" in
    Darwin:arm64) echo macos-arm64 ;;
    Darwin:x86_64) echo macos-x86_64 ;;
    Linux:x86_64 | Linux:amd64) echo linux-x86_64 ;;
    Linux:aarch64 | Linux:arm64) echo linux-arm64 ;;
    *) return 1 ;;
  esac
}

# Download arc-modern from a GitHub release and check it the way install.sh
# checks release assets: the SHA256SUMS signature, then the asset's SHA-256.
fetch_release() {
  local tag="$1" platform asset dir base expected actual
  case "$tag" in
    v[0-9]*.[0-9]*.[0-9]*) ;;
    *) die "--release takes a tag such as v0.8.12, not $tag" ;;
  esac
  platform="$(release_platform)" || die "no release binary for $(uname -s) $(uname -m); build from a checkout instead"
  asset="arc-modern-$platform"
  dir="$kit_dir/bin/$tag"
  base="$REPO_URL/releases/download/$tag"
  mkdir -p "$dir"
  download() {
    curl --fail --location --proto '=https' --proto-redir '=https' --tlsv1.2 --retry 3 \
      --silent --show-error --output "$dir/$1.download" "$base/$1" ||
      die "could not download $base/$1"
    mv "$dir/$1.download" "$dir/$1"
  }
  download SHA256SUMS
  download SHA256SUMS.sig
  command -v ssh-keygen >/dev/null 2>&1 || die "ssh-keygen is needed to check the release signature"
  printf '%s\n' "$RELEASE_SIGNER" > "$dir/allowed-signers"
  ssh-keygen -Y verify -f "$dir/allowed-signers" -I arc-release -n arc-release-manifest-v1 \
    -s "$dir/SHA256SUMS.sig" < "$dir/SHA256SUMS" >/dev/null 2>&1 ||
    die "the SHA256SUMS signature of $tag is invalid or not from the ARC release key"
  expected="$(awk -v n="$asset" '$2 == n || $2 == "*" n { print $1; exit }' "$dir/SHA256SUMS")"
  [ -n "$expected" ] ||
    die "release $tag (signature verified) has no $asset; build from a checkout instead"
  download "$asset"
  actual="$(sha256_of "$dir/$asset")"
  [ "$actual" = "$expected" ] || die "$asset has SHA-256 $actual; the signed SHA256SUMS says $expected"
  chmod +x "$dir/$asset"
  bin="$dir/$asset"
  echo "proof kit: using $asset from release $tag (signature and SHA-256 verified)" >&2
}

build_from_checkout() {
  [ -f "$REPO_ROOT/Cargo.toml" ] && [ -d "$REPO_ROOT/crates/arc-inference" ] ||
    die "not inside an arc-chain checkout; pass --bin PATH or --release TAG"
  command -v cargo >/dev/null 2>&1 ||
    die "cargo was not found: install Rust from https://rustup.rs (the repository pins its toolchain), or pass --release TAG"
  echo "proof kit: building arc-modern from this checkout (the first build takes several minutes)" >&2
  # Release optimisation without fat LTO builds much faster; integer results
  # cannot depend on optimisation settings, which is part of what is tested.
  (
    cd "$REPO_ROOT"
    CARGO_PROFILE_RELEASE_LTO="${CARGO_PROFILE_RELEASE_LTO:-off}" \
      CARGO_PROFILE_RELEASE_CODEGEN_UNITS="${CARGO_PROFILE_RELEASE_CODEGEN_UNITS:-16}" \
      cargo build --release --locked -p arc-inference --bin arc-modern >&2
  )
  bin="${CARGO_TARGET_DIR:-$REPO_ROOT/target}/release/arc-modern"
}

if [ -z "$bin" ] && [ -n "$release" ]; then
  fetch_release "$release"
fi
if [ -z "$bin" ]; then
  build_from_checkout
fi
[ -x "$bin" ] || die "$bin is not an executable arc-modern binary"

exec "$bin" proof ${kit_args[@]+"${kit_args[@]}"}
