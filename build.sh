#!/bin/bash
set -euo pipefail

SCRIPT_DIR="$(cd "$(dirname "$0")" && pwd)"

echo "=== Building ccextra ==="

cargo build --release

echo "=== Build complete ==="

# 替换二进制(不触碰运行中的服务)
cp "$SCRIPT_DIR/target/release/ccextra" "$SCRIPT_DIR/ccextra"
echo "ccextra → $SCRIPT_DIR/ccextra"
