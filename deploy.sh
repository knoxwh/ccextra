#!/bin/bash
set -euo pipefail

SCRIPT_DIR="$(cd "$(dirname "$0")" && pwd)"

# 检查是否有进程在跑(仅用于是否先 stop;编完后总会 start)
WAS_RUNNING=false
if pgrep -f "$SCRIPT_DIR/ccextra" >/dev/null 2>&1; then
    WAS_RUNNING=true
fi
if lsof -ti :8222 -sTCP:LISTEN >/dev/null 2>&1; then
    WAS_RUNNING=true
fi

echo "=== Building ccextra ==="

cargo build --release

echo "=== Build complete ==="

# build 成功后再停服,避免编失败时无谓中断
if [ "$WAS_RUNNING" = true ]; then
    echo "=== Stopping running service ==="
    "$SCRIPT_DIR/stop.sh"
fi

# 替换二进制
cp "$SCRIPT_DIR/target/release/ccextra" "$SCRIPT_DIR/ccextra"
echo "ccextra → $SCRIPT_DIR/ccextra"

# 编完必起:即使原先没在跑,也拉起来,避免「只停不起 / 已死后 deploy 不启」
echo "=== Starting service ==="
if ! "$SCRIPT_DIR/start.sh"; then
    echo "错误: deploy 后 start 失败,请检查日志与端口 8222" >&2
    exit 1
fi
