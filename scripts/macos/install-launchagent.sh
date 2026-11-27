#!/bin/bash
set -euo pipefail

# 安装/更新 ~/Library/Application Support/ccextra/launchd/com.wanghao.ccextra.plist
# （不放 ~/Library/LaunchAgents：登录不自启，由 start.sh 手动 bootstrap；KeepAlive 只拉回异常退出）
# 用法: install-launchagent.sh [--bootstrap]
#   默认只写入 plist；加 --bootstrap 则 bootout(若已加载) 后再 bootstrap。

REPO="$(cd "$(dirname "$0")/../.." && pwd)"
LABEL="com.wanghao.ccextra"
UID_NUM="$(id -u)"
DOMAIN="gui/${UID_NUM}"
SRC="${REPO}/scripts/macos/${LABEL}.plist"
LAUNCHD_DIR="${HOME}/Library/Application Support/ccextra/launchd"
DST="${LAUNCHD_DIR}/${LABEL}.plist"
LEGACY_DST="${HOME}/Library/LaunchAgents/${LABEL}.plist"
DO_BOOTSTRAP=false

for arg in "$@"; do
    case "$arg" in
        --bootstrap|--start) DO_BOOTSTRAP=true ;;
        -h|--help)
            echo "用法: $0 [--bootstrap]"
            exit 0
            ;;
        *)
            echo "未知参数: $arg" >&2
            exit 1
            ;;
    esac
done

if [ ! -f "$SRC" ]; then
    echo "错误: 找不到 plist 模板 $SRC" >&2
    exit 1
fi

# 迁移：旧版 plist 在 ~/Library/LaunchAgents（登录自启）→ bootout（若已加载）后只删该确切路径
if [ -f "$LEGACY_DST" ]; then
    if launchctl print "${DOMAIN}/${LABEL}" >/dev/null 2>&1; then
        echo "bootout ${DOMAIN}/${LABEL} (legacy plist)"
        launchctl bootout "${DOMAIN}/${LABEL}" 2>/dev/null || true
    fi
    rm -f "$LEGACY_DST"
    echo "migrated: removed legacy ${LEGACY_DST}（不再登录自启，plist 改装到 ${LAUNCHD_DIR}）"
fi

# 被禁用（如在登录项里关掉）时无法 bootstrap：明确报错，不自动 enable
if launchctl print-disabled "$DOMAIN" 2>/dev/null \
    | grep -qF -e "\"${LABEL}\" => disabled" -e "\"${LABEL}\" => true"; then
    echo "错误: ${LABEL} 已被禁用，无法 bootstrap；请先执行: launchctl enable ${DOMAIN}/${LABEL}" >&2
    exit 1
fi

mkdir -p "$LAUNCHD_DIR" "${REPO}/logs"

# @REPO@ → 本机绝对路径（路径含 & \ 时 sed 会炸；本仓库路径无此类字符）
sed "s|@REPO@|${REPO}|g" "$SRC" > "$DST"
echo "installed: $DST"

if [ "$DO_BOOTSTRAP" = true ]; then
    if [ ! -x "${REPO}/ccextra" ]; then
        echo "错误: 未找到可执行文件 ${REPO}/ccextra，请先 build.sh / deploy.sh" >&2
        exit 1
    fi
    if launchctl print "${DOMAIN}/${LABEL}" >/dev/null 2>&1; then
        echo "bootout ${DOMAIN}/${LABEL}"
        launchctl bootout "${DOMAIN}/${LABEL}" 2>/dev/null || true
        # 等旧进程退出，避免端口残留
        for _ in $(seq 1 20); do
            if ! pgrep -f "${REPO}/ccextra" >/dev/null 2>&1 \
                && ! lsof -ti :8222 -sTCP:LISTEN >/dev/null 2>&1; then
                break
            fi
            sleep 0.25
        done
    fi
    echo "bootstrap ${DOMAIN} <- $DST"
    launchctl bootstrap "$DOMAIN" "$DST"
    echo "LaunchAgent ${LABEL} bootstrapped"
fi
