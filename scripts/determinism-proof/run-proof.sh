#!/usr/bin/env bash
# Run ARC's cross-platform determinism proof on this machine (macOS or Linux)
# and print the combined SHA-256 of its transcript.
#
# The pinned Llama-2-7B-Chat Q4_K_M weights, the fixed prompts in prompts.json
# and greedy decoding produce a byte-identical transcript on every CPU platform
# the CI proof covers. Compare the printed hash with the published value in
# docs/determinism-proof.md (also in expected-sha256.txt). CPU only.
#
# Usage:
#   scripts/determinism-proof/run-proof.sh [--kernel scalar|simd] [--model PATH]
#       [--out-dir DIR] [--max-new-tokens N] [--prompt-limit N]
#       [--shard K/COUNT] [--deadline-seconds S]
#
# Needs rustup (the repository pins its toolchain), curl, about 5 GB of disk
# for the model and about 8 GB of free memory; less memory works, slowly,
# through swap. docs/determinism-proof.md lists measured CI run times.
set -Eeuo pipefail

readonly MODEL_URL="https://huggingface.co/TheBloke/Llama-2-7B-Chat-GGUF/resolve/191239b3e26b2882fb562ffccdd1cf0f65402adb/llama-2-7b-chat.Q4_K_M.gguf"
readonly MODEL_SHA256="08a5566d61d7cb6b420c3e4387a39e0078e1f2fe5f055f3a03887385304d4bfa"
readonly MODEL_BYTES=4081004224
readonly DEFAULT_MAX_NEW_TOKENS=32

SCRIPT_DIR="$(CDPATH='' cd -- "$(dirname -- "$0")" && pwd -P)"
REPO_ROOT="$(CDPATH='' cd -- "$SCRIPT_DIR/../.." && pwd -P)"

kernel=scalar
model="${ARC_PROOF_MODEL:-}"
out_dir="${ARC_PROOF_OUT_DIR:-$REPO_ROOT/target/determinism-proof}"
max_new_tokens="$DEFAULT_MAX_NEW_TOKENS"
prompt_limit=""
shard=""
deadline=""

usage() {
  sed -n '2,17p' "$0" | sed 's/^# \{0,1\}//'
}

while [ "$#" -gt 0 ]; do
  case "$1" in
    --kernel) kernel="${2:?--kernel needs a value}"; shift 2 ;;
    --model) model="${2:?--model needs a value}"; shift 2 ;;
    --out-dir) out_dir="${2:?--out-dir needs a value}"; shift 2 ;;
    --max-new-tokens) max_new_tokens="${2:?--max-new-tokens needs a value}"; shift 2 ;;
    --prompt-limit) prompt_limit="${2:?--prompt-limit needs a value}"; shift 2 ;;
    --shard) shard="${2:?--shard needs a value}"; shift 2 ;;
    --deadline-seconds) deadline="${2:?--deadline-seconds needs a value}"; shift 2 ;;
    -h | --help) usage; exit 0 ;;
    *) echo "unknown argument: $1" >&2; usage >&2; exit 2 ;;
  esac
done

case "$kernel" in
  scalar | simd) ;;
  *) echo "--kernel must be scalar or simd" >&2; exit 2 ;;
esac

sha256_of() {
  if command -v sha256sum >/dev/null 2>&1; then
    sha256sum "$1" | awk '{ print $1 }'
  else
    shasum -a 256 "$1" | awk '{ print $1 }'
  fi
}

if [ -z "$model" ]; then
  model_dir="${ARC_PROOF_MODEL_DIR:-${XDG_CACHE_HOME:-$HOME/.cache}/arc-determinism-proof}"
  model="$model_dir/llama-2-7b-chat.Q4_K_M.gguf"
fi
if [ ! -f "$model" ]; then
  mkdir -p "$(dirname "$model")"
  echo "Downloading the pinned model (4.1 GB) to $model" >&2
  curl --fail --location --retry 5 --retry-all-errors --connect-timeout 20 \
    --proto '=https' --proto-redir '=https' --tlsv1.2 \
    --output "$model.partial" "$MODEL_URL"
  mv "$model.partial" "$model"
fi

size="$(wc -c < "$model" | tr -d ' ')"
if [ "$size" != "$MODEL_BYTES" ]; then
  echo "model is $size bytes, expected $MODEL_BYTES: wrong or truncated file at $model" >&2
  exit 1
fi
echo "Verifying the model's SHA-256 ..." >&2
actual_sha256="$(sha256_of "$model")"
if [ "$actual_sha256" != "$MODEL_SHA256" ]; then
  echo "model SHA-256 is $actual_sha256, expected $MODEL_SHA256" >&2
  exit 1
fi
echo "model OK: sha256 $actual_sha256" >&2

mkdir -p "$out_dir"
suffix="$kernel"
if [ -n "$shard" ]; then
  suffix="$kernel-shard-${shard//\//-of-}"
fi
transcript="$out_dir/transcript-$suffix.txt"
run_json="$out_dir/run-$suffix.json"
driver_args=(
  --model "$model"
  --prompts "$SCRIPT_DIR/prompts.json"
  --kernel "$kernel"
  --max-new-tokens "$max_new_tokens"
  --transcript "$transcript"
  --run-json "$run_json"
)
if [ -n "$prompt_limit" ]; then
  driver_args+=(--prompt-limit "$prompt_limit")
fi
if [ -n "$shard" ]; then
  driver_args+=(--shard "$shard")
fi
if [ -n "$deadline" ]; then
  driver_args+=(--deadline-seconds "$deadline")
fi

cd "$REPO_ROOT"
cargo run --locked --release -p arc-inference --features candle \
  --example determinism_proof -- "${driver_args[@]}"

combined="$(sha256_of "$transcript")"
echo
echo "transcript:        $transcript"
echo "combined SHA-256:  $combined"
expected_file="$SCRIPT_DIR/expected-sha256.txt"
if [ -n "$prompt_limit$shard" ] || [ "$max_new_tokens" != "$DEFAULT_MAX_NEW_TOKENS" ]; then
  echo "Non-default workload: compare with another machine that used the same options."
elif [ -f "$expected_file" ]; then
  expected="$(grep -m 1 -E '^[0-9a-f]{64}$' "$expected_file" || true)"
  echo "published SHA-256: ${expected:-none recorded}"
  if [ "$combined" = "$expected" ]; then
    echo "RESULT: MATCH. This machine produced the published transcript byte for byte."
  else
    echo "RESULT: DIFFERENT. Please report it with $run_json and $transcript attached."
    exit 3
  fi
fi
