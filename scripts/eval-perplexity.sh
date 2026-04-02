#!/usr/bin/env bash
set -euo pipefail

# Perplexity evaluation for ARC Chain integer inference engine.
# Downloads model + dataset if not present, runs INT8 (and optionally INT16) evaluation.
#
# Usage: ./scripts/eval-perplexity.sh [model_path] [dataset_path] [max_tokens]

MODEL_DIR="${ARC_MODEL_DIR:-$HOME/.arc-models}"
MODEL="${1:-$MODEL_DIR/llama-2-7b.Q8_0.gguf}"
DATASET="${2:-$MODEL_DIR/wikitext-2-raw/wiki.test.raw}"
MAX_TOKENS="${3:-512}"

mkdir -p "$MODEL_DIR"

if [ ! -f "$MODEL" ]; then
    echo "Model not found at $MODEL"
    echo ""
    echo "To run perplexity evaluation, download a GGUF model file:"
    echo "  huggingface-cli download TheBloke/Llama-2-7B-GGUF llama-2-7b.Q8_0.gguf --local-dir $MODEL_DIR"
    echo ""
    echo "Or set ARC_MODEL_DIR to a directory containing the model."
    exit 1
fi

if [ ! -f "$DATASET" ]; then
    echo "WikiText-2 dataset not found at $DATASET"
    echo "Downloading..."
    curl -L -o "$MODEL_DIR/wikitext-2-raw-v1.zip" \
        "https://s3.amazonaws.com/research.metamind.io/wikitext/wikitext-2-raw-v1.zip"
    (cd "$MODEL_DIR" && unzip -o wikitext-2-raw-v1.zip)
    echo "Dataset downloaded to $MODEL_DIR/wikitext-2-raw/"
fi

echo "=== INT8 Perplexity Evaluation ==="
echo "Model: $MODEL"
echo "Dataset: $DATASET"
echo "Max tokens: $MAX_TOKENS"
echo ""

cargo run --example eval_perplexity --features candle --release -- \
    "$MODEL" "$DATASET" "$MAX_TOKENS"

# Try INT16 if feature is available
echo ""
echo "=== Attempting INT16 evaluation ==="
if cargo build --example eval_perplexity --features "candle,int16" --release 2>/dev/null; then
    cargo run --example eval_perplexity --features "candle,int16" --release -- \
        "$MODEL" "$DATASET" "$MAX_TOKENS"
else
    echo "INT16 feature not available (compile with --features int16)"
fi
