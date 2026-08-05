#!/bin/bash
set -euo pipefail

SCRIPT_DIR="$(cd "$(dirname "$0")" && pwd)"

echo "=== Stopping ccextra ==="

CC_NAMES="ccextra"
for name in $CC_NAMES; do
    for pid in $(pgrep -f "$name" 2>/dev/null || true); do
        echo "Stopping $name (PID: $pid)"
        kill "$pid" 2>/dev/null || true
    done
done

# 杀占用 8222 端口的进程
for pid in $(lsof -ti :8222 2>/dev/null || true); do
    cmd=$(ps -p "$pid" -o command= 2>/dev/null | head -1 || true)
    echo "Stopping process on port 8222 (PID: $pid, cmd: $cmd)"
    kill "$pid" 2>/dev/null || true
done

# 等待退出
for i in $(seq 1 10); do
    CC_ALIVE=false
    for name in $CC_NAMES; do
        if pgrep -f "$name" >/dev/null 2>&1; then
            CC_ALIVE=true
            break
        fi
    done
    if ! $CC_ALIVE && ! lsof -ti :8222 >/dev/null 2>&1; then
        echo "ccextra stopped"
        break
    fi
    sleep 0.5
done

# 强杀残留
for name in $CC_NAMES; do
    if pgrep -f "$name" >/dev/null 2>&1; then
        echo "Force killing $name"
        pkill -9 -f "$name" 2>/dev/null || true
    fi
done
for pid in $(lsof -ti :8222 2>/dev/null || true); do
    echo "Force killing process on port 8222 (PID: $pid)"
    kill -9 "$pid" 2>/dev/null || true
done

echo "=== ccextra stopped ==="