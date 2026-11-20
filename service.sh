#!/bin/bash
set -euo pipefail

# 统一入口: ./service.sh start|stop|restart|status|official-init
# - 始终管理 8222 主实例(复用 start.sh / stop.sh,LaunchAgent com.wanghao.ccextra)
# - 按 config.yaml 顶层 official_instance(on/off,只读这一行)管理 8223 官方透传实例:
#   on  → official-init(补齐运行目录、同步二进制)后安装 plist 并 bootstrap
#         LaunchAgent com.wanghao.ccextra-official(登录自启、KeepAlive)
#   off → bootout 并移除该 plist,确保 8223 未运行
# - official-init:不看开关,只准备 $CCEXTRA_OFFICIAL_DIR(纯运行目录,可删后重建):
#   logs/、仓库根 ./ccextra 的副本、由 scripts/official/config.yaml.example 生成的 config.yaml

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
OFFICIAL_TEMPLATE="$SCRIPT_DIR/scripts/official/config.yaml.example"
MAIN_BIN="$SCRIPT_DIR/ccextra"
OFFICIAL_BIN_UPDATED=no
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

# 主实例 server.proxy_url 的值(只读 server 段这一行,不输出);找不到返回非 0
main_proxy_url() {
    local val
    val=$(awk '/^server:/{f=1;next} f&&/^[^[:space:]#]/{f=0} f&&/^[[:space:]]+proxy_url:/{print;exit}' "$CONFIG" 2>/dev/null || true)
    val=$(printf '%s' "$val" | sed -E 's/^[[:space:]]*proxy_url:[[:space:]]*//; s/[[:space:]]+#.*$//; s/[[:space:]]+$//')
    val=${val#\"}; val=${val%\"}; val=${val#\'}; val=${val%\'}
    [ -n "$val" ] || return 1
    printf '%s' "$val"
}

# 准备 8223 运行目录:logs/、二进制(与仓库根 ./ccextra 不同则原子替换)、config.yaml(缺失才生成,0600)
official_init() {
    local tmp proxy
    if [ ! -x "$MAIN_BIN" ]; then
        echo "错误: 未找到 $MAIN_BIN,请先 build.sh / deploy.sh" >&2
        return 1
    fi
    mkdir -p "$OFFICIAL_DIR/logs"
    if ! cmp -s "$MAIN_BIN" "$OFFICIAL_BIN"; then
        # 先复制到同目录临时文件再 rename:运行中的旧进程不受影响(不能原地覆盖正在执行的二进制)
        tmp="$OFFICIAL_BIN.tmp.$$"
        cp "$MAIN_BIN" "$tmp"
        chmod 755 "$tmp"
        mv -f "$tmp" "$OFFICIAL_BIN"
        OFFICIAL_BIN_UPDATED=yes
        echo "official binary synced from $MAIN_BIN"
    fi
    if [ ! -f "$OFFICIAL_CONFIG" ]; then
        if [ ! -f "$OFFICIAL_TEMPLATE" ]; then
            echo "错误: 找不到配置模板 $OFFICIAL_TEMPLATE" >&2
            return 1
        fi
        if ! proxy=$(main_proxy_url); then
            echo "错误: $CONFIG 中没有 server.proxy_url,无法生成 $OFFICIAL_CONFIG" >&2
            return 1
        fi
        tmp="$OFFICIAL_CONFIG.tmp.$$"
        (umask 077 && PROXY="$proxy" awk '{ i = index($0, "@PROXY_URL@"); if (i > 0) $0 = substr($0, 1, i - 1) ENVIRON["PROXY"] substr($0, i + 11); print }' "$OFFICIAL_TEMPLATE" > "$tmp")
        chmod 600 "$tmp"
        mv "$tmp" "$OFFICIAL_CONFIG"
        echo "generated: $OFFICIAL_CONFIG (0600, proxy_url 取自主实例 server.proxy_url)"
    fi
    echo "official dir ready: $OFFICIAL_DIR"
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
    official_init
    if [ ! -f "$OFFICIAL_SRC" ]; then
        echo "错误: 找不到 plist 模板 $OFFICIAL_SRC" >&2
        return 1
    fi
    mkdir -p "${HOME}/Library/LaunchAgents" "$OFFICIAL_DIR/logs"
    sed "s|@DIR@|${OFFICIAL_DIR}|g" "$OFFICIAL_SRC" > "$OFFICIAL_DST"

    if official_loaded && official_health_ok && [ "$OFFICIAL_BIN_UPDATED" = no ]; then
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
    official-init)
        official_init
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
        echo "用法: $0 start|stop|restart|status|official-init" >&2
        exit 1
        ;;
esac
