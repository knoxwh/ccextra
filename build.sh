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

# 如果进程在跑,先 stop
if [ "$WAS_RUNNING" = true ]; then
    echo "=== Services running, stopping before build ==="
    "$SCRIPT_DIR/stop.sh"
fi

echo "=== Building ccextra ==="
cargo build --release
cp "$SCRIPT_DIR/target/release/ccextra" "$SCRIPT_DIR/ccextra"
echo "ccextra → $SCRIPT_DIR/ccextra"

echo "=== Build complete ==="

# 如果之前有进程跑,build 后重新启动
if [ "$WAS_RUNNING" = true ]; then
    echo "=== Restarting services ==="
    "$SCRIPT_DIR/start.sh"
fi