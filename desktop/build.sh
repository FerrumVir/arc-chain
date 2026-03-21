#!/bin/bash
# ARC Node Desktop — build script
# Builds the Tauri desktop application for the current platform.

set -e
cd "$(dirname "$0")"

echo "==> Installing frontend dependencies..."
npm install

echo "==> Copying index.html to dist..."
mkdir -p src/dist
cp src/index.html src/dist/index.html

echo "==> Building Tauri app..."
npx tauri build

echo ""
echo "Build complete!"
echo "Find the app in src-tauri/target/release/bundle/"
