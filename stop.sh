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
# 只匹配 ccextra 本体:裸 "$BIN" 前缀也会命中同目录 ccextra-gohttp(sidecar 由下方孤儿段处理)
BIN_RE="^${BIN}( |\$)"
SIDECAR="ccextra-gohttp"
SOCK_DIR="$SCRIPT_DIR/.cache"

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
    if ! pgrep -f "$BIN_RE" >/dev/null 2>&1 \
        && ! lsof -ti :8222 -sTCP:LISTEN >/dev/null 2>&1; then
        break
    fi
    sleep 0.25
done

# 孤儿：本仓库二进制
for pid in $(pgrep -f "$BIN_RE" 2>/dev/null || true); do
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
    if ! pgrep -f "$BIN_RE" >/dev/null 2>&1 \
        && ! lsof -ti :8222 -sTCP:LISTEN >/dev/null 2>&1; then
        echo "ccextra stopped"
        break
    fi
    sleep 0.5
done

# 强杀残留
if pgrep -f "$BIN_RE" >/dev/null 2>&1; then
    echo "Force killing $BIN"
    pkill -9 -f "$BIN_RE" 2>/dev/null || true
fi
for pid in $(lsof -ti :8222 -sTCP:LISTEN 2>/dev/null || true); do
    echo "Force killing process on port 8222 (PID: $pid)"
    kill -9 "$pid" 2>/dev/null || true
done

# 孤儿 sidecar:进程名精确为 ccextra-gohttp 且 PPID=1(父 ccextra 已死、被 launchd 收养)。
# 父进程仍活着的(另一实例 / 测试)不动。先 SIGTERM(它自己删 socket),超时才 -9。
orphan_sidecars() {
    local pid ppid
    for pid in $(pgrep -x -U "$UID_NUM" "$SIDECAR" 2>/dev/null || true); do
        ppid=$(ps -o ppid= -p "$pid" 2>/dev/null | tr -d ' ' || true)
        [ "$ppid" = "1" ] && echo "$pid"
    done
    return 0
}
for pid in $(orphan_sidecars); do
    echo "Stopping orphan $SIDECAR (PID: $pid)"
    kill "$pid" 2>/dev/null || true
done
for _ in $(seq 1 10); do
    [ -z "$(orphan_sidecars)" ] && break
    sleep 0.2
done
for pid in $(orphan_sidecars); do
    echo "Force killing orphan $SIDECAR (PID: $pid)"
    kill -9 "$pid" 2>/dev/null || true
done
# 残留 socket:文件名 ccextra-gohttp-<ccextra pid>-<seq>.sock,所属 ccextra 已不在才删
for sock in "$SOCK_DIR/$SIDECAR"-*.sock; do
    [ -S "$sock" ] || continue
    owner=${sock##*/"$SIDECAR"-}
    owner=${owner%%-*}
    if [[ "$owner" =~ ^[0-9]+$ ]] && ! kill -0 "$owner" 2>/dev/null; then
        echo "Removing stale socket $sock"
        rm -f "$sock"
    fi
done
echo "=== ccextra stopped ==="
