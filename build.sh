#!/bin/bash
set -euo pipefail

SCRIPT_DIR="$(cd "$(dirname "$0")" && pwd)"

# 检查是否有进程在跑
WAS_RUNNING=false
if pgrep -f "$SCRIPT_DIR/ccextra" >/dev/null 2>&1; then
    WAS_RUNNING=true
fi
if lsof -ti :8222 -sTCP:LISTEN >/dev/null 2>&1; then
    WAS_RUNNING=true
fi

echo "=== Building ccextra ==="

# Cursor SDK sidecar 前置检查:Node >= 24、npm ci、@cursor/sdk 版本锁定
SIDECAR_DIR="$SCRIPT_DIR/sidecar/cursor"
if [ -d "$SIDECAR_DIR" ]; then
    if ! command -v node >/dev/null 2>&1; then
        echo "错误: 检测到 sidecar/cursor 但未安装 Node(需要 >= 24)" >&2
        exit 1
    fi
    NODE_MAJOR="$(node --version | tr -d 'v' | cut -d. -f1)"
    if [ "$NODE_MAJOR" -lt 24 ]; then
        echo "错误: Node 版本 $NODE_MAJOR < 24,sidecar 无法运行" >&2
        exit 1
    fi
    if [ ! -f "$SIDECAR_DIR/package-lock.json" ]; then
        echo "错误: 缺少 $SIDECAR_DIR/package-lock.json" >&2
        exit 1
    fi
    echo "=== Installing sidecar dependencies ==="
    npm --prefix "$SIDECAR_DIR" ci --no-fund --no-audit
    if ! node -e 'const p=require("'"$SIDECAR_DIR"'/node_modules/@cursor/sdk/package.json"); if (p.version !== "1.0.34") { console.error("错误: @cursor/sdk 版本 " + p.version + " != 1.0.34"); process.exit(1) }'; then
        echo "错误: @cursor/sdk 版本检查失败,请重新 npm ci" >&2
        exit 1
    fi
fi

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