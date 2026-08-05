#!/bin/bash
set -euo pipefail

SCRIPT_DIR="$(cd "$(dirname "$0")" && pwd)"

# 检查是否有进程在跑
WAS_RUNNING=false
if pgrep -f "ccextra" >/dev/null 2>&1; then
    WAS_RUNNING=true
fi
if lsof -ti :8222 >/dev/null 2>&1; then
    WAS_RUNNING=true
fi

echo "=== Building ccextra ==="
cargo build --release

echo "=== Build complete ==="

# build 成功后再停服
if [ "$WAS_RUNNING" = true ]; then
    echo "=== Stopping running service ==="
    "$SCRIPT_DIR/stop.sh"
fi

# 替换二进制
cp "$SCRIPT_DIR/target/release/ccextra" "$SCRIPT_DIR/ccextra"
echo "ccextra → $SCRIPT_DIR/ccextra"

# 重启
if [ "$WAS_RUNNING" = true ]; then
    echo "=== Starting service ==="
    "$SCRIPT_DIR/start.sh"
fi