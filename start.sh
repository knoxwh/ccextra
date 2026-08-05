#!/bin/bash
set -euo pipefail

SCRIPT_DIR="$(cd "$(dirname "$0")" && pwd)"
TODAY=$(date +%Y-%m-%d)
LOG_DIR="$SCRIPT_DIR/logs"
BIN="$SCRIPT_DIR/ccextra"

mkdir -p "$LOG_DIR"

# 清理当日之前的日志
find "$LOG_DIR" -name '*.log' | while read -r f; do
    basename=$(basename "$f")
    if [[ "$basename" =~ \.([0-9]{4}-[0-9]{2}-[0-9]{2})\.log$ ]]; then
        file_date="${BASH_REMATCH[1]}"
        if [ "$file_date" != "$TODAY" ]; then
            echo "Removing old log: $f"
            rm -f "$f"
        fi
    fi
done

CC_LOG="$LOG_DIR/ccextra.$TODAY.log"

if [ ! -x "$BIN" ]; then
    echo "错误: 未找到二进制 $BIN,请先运行 build.sh" >&2
    exit 1
fi

# 端口占用检查
if lsof -ti :8222 >/dev/null 2>&1; then
    echo "端口 8222 已被占用,请先 stop.sh" >&2
    exit 1
fi

echo "=== Starting ccextra ==="
cd "$SCRIPT_DIR"
nohup "$BIN" \
    --config "$SCRIPT_DIR/config.yaml" \
    >> "$CC_LOG" 2>&1 &
CC_PID=$!
echo "ccextra started (PID: $CC_PID)  log: $CC_LOG"

# 等待就绪
for i in $(seq 1 30); do
    if curl -sf http://127.0.0.1:8222/health >/dev/null 2>&1; then
        echo "ccextra ready"
        break
    fi
    sleep 0.2
done