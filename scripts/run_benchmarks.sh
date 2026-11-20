#!/usr/bin/env bash

set -euo pipefail

echo "================================================================"
echo " ARC Chain - Full Benchmark Suite"
echo " $(date)"
echo "================================================================"
echo ""

echo "[1/4] Building release binaries..."
cargo build --release --workspace 2>&1 | tail -5

echo ""
echo "[2/4] Running production pipeline benchmark..."
cargo run --release --bin arc-bench-production 2>&1

echo ""
echo "[3/4] Running ETH-weighted mixed workload benchmark..."
cargo run --release --bin arc-bench-mixed 2>&1

echo ""
echo "[4/4] Running signed transaction benchmark..."
cargo run --release --bin arc-bench-signed 2>&1 || echo "  (skipped - signed bench not available)"

echo ""
echo "================================================================"
echo " Benchmark suite complete - $(date)"
echo "================================================================"
