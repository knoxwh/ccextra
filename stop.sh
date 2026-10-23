#!/bin/bash
set -euo pipefail

SCRIPT_DIR="$(cd "$(dirname "$0")" && pwd)"
LOG_DIR="$SCRIPT_DIR/logs"
CC_LOG="$LOG_DIR/ccextra.launchd.log"
BIN="$SCRIPT_DIR/ccextra"
LABEL="com.wanghao.ccextra"
UID_NUM="$(id -u)"
DOMAIN="gui/${UID_NUM}"
SERVICE="${DOMAIN}/${LABEL}"

# 记录谁触发了 stop，方便下次对照 SIGTERM（写入 launchd 活跃日志）
log_stop_caller() {
    local ts user uid ppid parent_cmd tty_name term_val ssh_val line
    ts=$(date '+%Y-%m-%d %H:%M:%S %Z')
    user=$(id -un 2>/dev/null || echo '?')
    uid=$(id -u 2>/dev/null || echo '?')
    ppid=${PPID:-?}
    # 压成单行并截断，避免 Cursor/agent 壳把整段 dump 脚本写进日志
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

# KeepAlive 下必须先 bootout，禁止只 kill
if launchctl print "$SERVICE" >/dev/null 2>&1; then
    echo "bootout $SERVICE"
    launchctl bootout "$SERVICE" 2>/dev/null || true
fi

# 等待 launchd 子进程退出
for _ in $(seq 1 20); do
    if ! pgrep -f "$BIN" >/dev/null 2>&1 \
        && ! lsof -ti :8222 -sTCP:LISTEN >/dev/null 2>&1; then
        break
    fi
    sleep 0.25
done

# 孤儿：本仓库二进制
for pid in $(pgrep -f "$BIN" 2>/dev/null || true); do
    echo "Stopping orphan $BIN (PID: $pid)"
    kill "$pid" 2>/dev/null || true
done

# 孤儿：仍占 8222 的监听者
for pid in $(lsof -ti :8222 -sTCP:LISTEN 2>/dev/null || true); do
    cmd=$(ps -p "$pid" -o command= 2>/dev/null | head -1 || true)
    echo "Stopping process on port 8222 (PID: $pid, cmd: $cmd)"
    kill "$pid" 2>/dev/null || true
done

for _ in $(seq 1 10); do
    if ! pgrep -f "$BIN" >/dev/null 2>&1 \
        && ! lsof -ti :8222 -sTCP:LISTEN >/dev/null 2>&1; then
        echo "ccextra stopped"
        break
    fi
    sleep 0.5
done

# 强杀残留
if pgrep -f "$BIN" >/dev/null 2>&1; then
    echo "Force killing $BIN"
    pkill -9 -f "$BIN" 2>/dev/null || true
fi
for pid in $(lsof -ti :8222 -sTCP:LISTEN 2>/dev/null || true); do
    echo "Force killing process on port 8222 (PID: $pid)"
    kill -9 "$pid" 2>/dev/null || true
done
echo "=== ccextra stopped ==="
