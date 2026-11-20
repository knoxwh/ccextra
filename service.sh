#!/bin/bash
set -euo pipefail

# 统一入口: ./service.sh start|stop|restart|status
# - 始终管理 8222 主实例(复用 start.sh / stop.sh,LaunchAgent com.wanghao.ccextra)
# - 按 config.yaml 顶层 official_instance(on/off,只读这一行)管理 8223 官方透传实例:
#   on  → 安装 plist 并 bootstrap LaunchAgent com.wanghao.ccextra-official(登录自启、KeepAlive)
#   off → bootout 并移除该 plist,确保 8223 未运行

SCRIPT_DIR="$(cd "$(dirname "$0")" && pwd)"
CONFIG="$SCRIPT_DIR/config.yaml"
UID_NUM="$(id -u)"
DOMAIN="gui/${UID_NUM}"
MAIN_SERVICE="${DOMAIN}/com.wanghao.ccextra"

OFFICIAL_DIR="${CCEXTRA_OFFICIAL_DIR:-$(dirname "$SCRIPT_DIR")/ccextra-official}"
OFFICIAL_BIN="$OFFICIAL_DIR/ccextra"
OFFICIAL_CONFIG="$OFFICIAL_DIR/config.yaml"
OFFICIAL_PID_FILE="$OFFICIAL_DIR/ccextra.pid"
OFFICIAL_PORT=8223
OFFICIAL_LABEL="com.wanghao.ccextra-official"
OFFICIAL_SERVICE="${DOMAIN}/${OFFICIAL_LABEL}"
OFFICIAL_SRC="$SCRIPT_DIR/scripts/macos/${OFFICIAL_LABEL}.plist"
OFFICIAL_DST="${HOME}/Library/LaunchAgents/${OFFICIAL_LABEL}.plist"
# 匹配 launchd 拉起的绝对路径与手工 ./ccextra 两种命令行
OFFICIAL_RE="ccextra --config ${OFFICIAL_CONFIG}( |\$)"

# 只解析 official_instance 一行(不输出 config.yaml 其他内容);缺省 off
official_switch() {
    local line val
    line=$(grep -E '^official_instance:' "$CONFIG" 2>/dev/null | tail -n 1 || true)
    if [ -z "$line" ]; then
        echo off
        return 0
    fi
    val=${line#official_instance:}
    val=${val%%#*}
    val=$(printf '%s' "$val" | tr -d " \t\"'")
    case "$val" in
        on|true) echo on ;;
        off|false) echo off ;;
        *)
            echo "错误: $CONFIG 中 official_instance 只接受 on/off" >&2
            return 1
            ;;
    esac
}

official_health_ok() {
    curl -sf "http://127.0.0.1:${OFFICIAL_PORT}/health" >/dev/null 2>&1
}

official_listen_pids() {
    lsof -ti ":${OFFICIAL_PORT}" -sTCP:LISTEN 2>/dev/null || true
}

official_pids() {
    pgrep -f "$OFFICIAL_RE" 2>/dev/null || true
}

official_loaded() {
    launchctl print "$OFFICIAL_SERVICE" >/dev/null 2>&1
}

official_fail_if_foreign_listener() {
    local pid cmd
    for pid in $(official_listen_pids); do
        cmd=$(ps -p "$pid" -o command= 2>/dev/null || true)
        if ! printf '%s\n' "$cmd" | grep -Eq "$OFFICIAL_RE"; then
            echo "错误: 端口 ${OFFICIAL_PORT} 已被其他进程占用 (PID: $pid)，请先处理" >&2
            return 1
        fi
    done
}

# 停 8223:bootout + 清掉非 launchd 残留;remove_plist=yes 时删 plist(登录不再自启)
official_stop() {
    local remove_plist="$1" pid
    if official_loaded; then
        echo "bootout $OFFICIAL_SERVICE"
        launchctl bootout "$OFFICIAL_SERVICE" 2>/dev/null || true
    fi
    for _ in $(seq 1 20); do
        [ -z "$(official_pids)" ] && break
        sleep 0.25
    done
    for pid in $(official_pids); do
        echo "Stopping stray official ccextra (PID: $pid)"
        kill "$pid" 2>/dev/null || true
    done
    for _ in $(seq 1 10); do
        [ -z "$(official_pids)" ] && break
        sleep 0.5
    done
    for pid in $(official_pids); do
        echo "Force killing official ccextra (PID: $pid)"
        kill -9 "$pid" 2>/dev/null || true
    done
    rm -f "$OFFICIAL_PID_FILE"
    if [ "$remove_plist" = yes ] && [ -f "$OFFICIAL_DST" ]; then
        echo "removed: $OFFICIAL_DST"
        rm -f "$OFFICIAL_DST"
    fi
    if [ -n "$(official_pids)" ]; then
        echo "错误: 8223 官方实例未能停止" >&2
        return 1
    fi
    echo "official instance stopped (${OFFICIAL_PORT})"
}

official_start() {
    local pid
    if [ ! -x "$OFFICIAL_BIN" ] || [ ! -f "$OFFICIAL_CONFIG" ]; then
        echo "错误: 缺少 $OFFICIAL_BIN 或 $OFFICIAL_CONFIG" >&2
        return 1
    fi
    if [ ! -f "$OFFICIAL_SRC" ]; then
        echo "错误: 找不到 plist 模板 $OFFICIAL_SRC" >&2
        return 1
    fi
    mkdir -p "${HOME}/Library/LaunchAgents" "$OFFICIAL_DIR/logs"
    sed "s|@DIR@|${OFFICIAL_DIR}|g" "$OFFICIAL_SRC" > "$OFFICIAL_DST"

    if official_loaded && official_health_ok; then
        echo "official instance already running via LaunchAgent $OFFICIAL_LABEL"
    else
        official_fail_if_foreign_listener
        if official_loaded; then
            echo "kickstart -k $OFFICIAL_SERVICE"
            launchctl kickstart -k "$OFFICIAL_SERVICE"
        else
            # 非 launchd 残留(如手工 nohup 起的)先停掉,避免端口冲突
            for pid in $(official_pids); do
                echo "Stopping stray official ccextra (PID: $pid)"
                kill "$pid" 2>/dev/null || true
            done
            for _ in $(seq 1 20); do
                [ -z "$(official_pids)" ] && [ -z "$(official_listen_pids)" ] && break
                sleep 0.25
            done
            official_fail_if_foreign_listener
            echo "bootstrap $DOMAIN <- $OFFICIAL_DST"
            launchctl bootstrap "$DOMAIN" "$OFFICIAL_DST"
        fi
    fi

    for _ in $(seq 1 75); do
        if official_health_ok; then
            pid=$(official_listen_pids | head -1 || true)
            echo "${pid:-}" > "$OFFICIAL_PID_FILE"
            echo "official instance ready (PID: ${pid:-?}) on 127.0.0.1:${OFFICIAL_PORT}"
            return 0
        fi
        sleep 0.2
    done
    echo "错误: 等待 8223 /health 超时，请查看 $OFFICIAL_DIR/logs/ccextra.launchd.log" >&2
    return 1
}

apply_switch() {
    local sw
    sw=$(official_switch)
    echo "official_instance: $sw"
    if [ "$sw" = on ]; then
        official_start
    else
        official_stop yes
    fi
}

status_line() {
    local name="$1" service="$2" port="$3" loaded=no health=down pid
    launchctl print "$service" >/dev/null 2>&1 && loaded=yes
    curl -sf "http://127.0.0.1:${port}/health" >/dev/null 2>&1 && health=ok
    pid=$(lsof -ti ":${port}" -sTCP:LISTEN 2>/dev/null | head -1 || true)
    echo "$name: port=$port launchd_loaded=$loaded health=$health pid=${pid:--}"
}

case "${1:-}" in
    start)
        sw=$(official_switch)
        "$SCRIPT_DIR/start.sh"
        apply_switch
        ;;
    stop)
        sw=$(official_switch 2>/dev/null || echo unknown)
        "$SCRIPT_DIR/stop.sh"
        # 开关为 on 时保留 plist(与 8222 一致:stop 只 bootout,登录仍自启)
        if [ "$sw" = off ]; then official_stop yes; else official_stop no; fi
        ;;
    restart)
        sw=$(official_switch)
        "$SCRIPT_DIR/stop.sh"
        "$SCRIPT_DIR/start.sh"
        if [ "$sw" = on ]; then official_stop no; fi
        apply_switch
        ;;
    status)
        sw=$(official_switch 2>/dev/null || echo invalid)
        echo "official_instance: $sw"
        status_line main "$MAIN_SERVICE" 8222
        status_line official "$OFFICIAL_SERVICE" "$OFFICIAL_PORT"
        if [ -f "$OFFICIAL_DST" ]; then
            echo "official plist: installed ($OFFICIAL_DST)"
        else
            echo "official plist: not installed"
        fi
        ;;
    *)
        echo "用法: $0 start|stop|restart|status" >&2
        exit 1
        ;;
esac
