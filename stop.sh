#!/bin/bash
set -euo pipefail

SCRIPT_DIR="$(cd "$(dirname "$0")" && pwd)"
TODAY=$(date +%Y-%m-%d)
LOG_DIR="$SCRIPT_DIR/logs"
CC_LOG="$LOG_DIR/ccextra.$TODAY.log"

# 记录谁触发了 stop,方便下次对照 SIGTERM(只写当日 ccextra 日志)
log_stop_caller() {
    local ts user uid ppid parent_cmd tty_name term_val ssh_val line
    ts=$(date '+%Y-%m-%d %H:%M:%S %Z')
    user=$(id -un 2>/dev/null || echo '?')
    uid=$(id -u 2>/dev/null || echo '?')
    ppid=${PPID:-?}
    # 压成单行并截断,避免 Cursor/agent 壳把整段 dump 脚本写进日志
    parent_cmd=$(ps -p "$ppid" -o args= 2>/dev/null | tr '\n\r\t' ' ' | tr -s ' ' || true)
    parent_cmd=${parent_cmd:-?}
    if [ "${#parent_cmd}" -gt 240 ]; then
        parent_cmd="${parent_cmd:0:240}..."
    fi
    tty_name=$(tty 2>/dev/null || true)
    if [ -z "$tty_name" ] || [ "$tty_name" = "not a tty" ]; then
        tty_name=$(ps -p $$ -o tty= 2>/dev/null | tr -d ' ' || true)
        tty_name=${tty_name:-?}
    fi
    term_val=${TERM:-}
    ssh_val=${SSH_CLIENT:-${SSH_CONNECTION:-}}

    line="[ops] stop.sh caller ts=$ts user=$user uid=$uid ppid=$ppid tty=$tty_name parent=\"$parent_cmd\""
    if [ -n "$term_val" ]; then
        line="$line TERM=$term_val"
    fi
    if [ -n "$ssh_val" ]; then
        line="$line SSH_CLIENT=\"$ssh_val\""
    fi

    mkdir -p "$LOG_DIR"
    echo "$line" >> "$CC_LOG" 2>/dev/null || true
}

echo "=== Stopping ccextra ==="
log_stop_caller

# 匹配本仓库二进制的完整路径,避免误杀命令行恰好含 ccextra 的无关进程
CC_NAMES="$SCRIPT_DIR/ccextra"
for name in $CC_NAMES; do
    for pid in $(pgrep -f "$name" 2>/dev/null || true); do
        echo "Stopping $name (PID: $pid)"
        kill "$pid" 2>/dev/null || true
    done
done

# 杀占用 8222 端口的进程(只杀监听者,避免误杀连到该端口的客户端如 Claude Code)
for pid in $(lsof -ti :8222 -sTCP:LISTEN 2>/dev/null || true); do
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
    if ! $CC_ALIVE && ! lsof -ti :8222 -sTCP:LISTEN >/dev/null 2>&1; then
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
for pid in $(lsof -ti :8222 -sTCP:LISTEN 2>/dev/null || true); do
    echo "Force killing process on port 8222 (PID: $pid)"
    kill -9 "$pid" 2>/dev/null || true
done
echo "=== ccextra stopped ==="
