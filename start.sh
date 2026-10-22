#!/bin/bash
set -euo pipefail

SCRIPT_DIR="$(cd "$(dirname "$0")" && pwd)"
TODAY=$(date +%Y-%m-%d)
LOG_DIR="$SCRIPT_DIR/logs"
BIN="$SCRIPT_DIR/ccextra"
LABEL="com.wanghao.ccextra"
UID_NUM="$(id -u)"
DOMAIN="gui/${UID_NUM}"
SERVICE="${DOMAIN}/${LABEL}"
INSTALL_SH="$SCRIPT_DIR/scripts/macos/install-launchagent.sh"

mkdir -p "$LOG_DIR"

# 清理当日之前的按日滚动日志（不动 launchd 固定 out/err）
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

if [ ! -x "$BIN" ]; then
    echo "错误: 未找到二进制 $BIN，请先运行 build.sh 或 deploy.sh" >&2
    exit 1
fi

if [ ! -x "$INSTALL_SH" ]; then
    echo "错误: 缺少 $INSTALL_SH" >&2
    exit 1
fi

# 确保 plist 已安装（不在这里 bootstrap，避免无谓 bounce）
"$INSTALL_SH"

is_loaded() {
    launchctl print "$SERVICE" >/dev/null 2>&1
}

health_ok() {
    curl -sf http://127.0.0.1:8222/health >/dev/null 2>&1
}

# 端口监听者 PID（仅 LISTEN）
listen_pids() {
    lsof -ti :8222 -sTCP:LISTEN 2>/dev/null || true
}

# 本仓库二进制的进程
our_pids() {
    pgrep -f "$BIN" 2>/dev/null || true
}

# 若 8222 被非本仓库二进制占用则失败
fail_if_foreign_listener() {
    local pid cmd
    for pid in $(listen_pids); do
        cmd=$(ps -p "$pid" -o command= 2>/dev/null || true)
        case "$cmd" in
            "$BIN"*|"$BIN "*) ;;
            *)
                echo "端口 8222 已被其他进程占用 (PID: $pid, cmd: $cmd)，请先处理后再 start" >&2
                exit 1
                ;;
        esac
    done
}

# 清掉非 launchd 拉起的同二进制残留（KeepAlive 下勿在已 load 时乱杀）
kill_stray_orphans() {
    local pid ppid
    for pid in $(our_pids); do
        ppid=$(ps -p "$pid" -o ppid= 2>/dev/null | tr -d ' ' || true)
        # launchd 子进程的 ppid 通常为 1
        if [ "${ppid:-}" = "1" ]; then
            continue
        fi
        echo "Stopping stray non-launchd ccextra (PID: $pid, ppid: ${ppid:-?})"
        kill "$pid" 2>/dev/null || true
    done
    for _ in $(seq 1 10); do
        local left=0
        for pid in $(our_pids); do
            ppid=$(ps -p "$pid" -o ppid= 2>/dev/null | tr -d ' ' || true)
            if [ "${ppid:-}" != "1" ]; then
                left=1
                break
            fi
        done
        [ "$left" = "0" ] && break
        sleep 0.2
    done
}

wait_health() {
    local i
    for i in $(seq 1 50); do
        if health_ok; then
            local pid
            pid=$(listen_pids | head -1 || true)
            echo "ccextra ready (PID: ${pid:-?}) via LaunchAgent $LABEL"
            return 0
        fi
        sleep 0.2
    done
    echo "错误: 等待 /health 超时，请查看 $LOG_DIR/ccextra.launchd.err.log" >&2
    return 1
}

echo "=== Starting ccextra (LaunchAgent) ==="

if is_loaded && health_ok; then
    pid=$(listen_pids | head -1 || true)
    echo "ccextra already running (PID: ${pid:-?}) via LaunchAgent $LABEL"
    exit 0
fi

fail_if_foreign_listener

if is_loaded; then
    echo "kickstart -k $SERVICE"
    launchctl kickstart -k "$SERVICE"
else
    kill_stray_orphans
    fail_if_foreign_listener
    if lsof -ti :8222 -sTCP:LISTEN >/dev/null 2>&1; then
        # 仍被本仓库孤儿占着：再清一次监听者
        for pid in $(listen_pids); do
            echo "Stopping leftover listener on 8222 (PID: $pid)"
            kill "$pid" 2>/dev/null || true
        done
        sleep 0.5
    fi
    echo "bootstrap $DOMAIN <- ~/Library/LaunchAgents/${LABEL}.plist"
    launchctl bootstrap "$DOMAIN" "${HOME}/Library/LaunchAgents/${LABEL}.plist"
fi

wait_health
