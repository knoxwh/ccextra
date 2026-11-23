#!/usr/bin/env bash
# zsh/sh 直接执行时转投 bash
if [ -z "${BASH_VERSION:-}" ]; then
    exec bash "$0" "$@"
fi
set -uo pipefail

# 本机回环检查出站 TLS 指纹有没有漂移（不联网）：
#   Claude 车道：ccextra 的 claude_fp_loopback 测试，对比 TARGET_JA4 / TARGET_JA3_HASH
# 用法：scripts/check_fingerprints.sh
# 升级 wreq/btls（cargo update）后必跑；失败时看输出里的 ja4_r / ja3 原串。

BASE_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"

case "${1:-}" in
    "") ;;
    -h|--help) sed -n "8,11p" "$0"; exit 0 ;;
    *) echo "未知参数: $1" >&2; exit 2 ;;
esac

declare -a RESULTS=()
FAILED=0

echo "==> Claude 车道（ccextra $(git -C "$BASE_DIR" rev-parse --short HEAD 2>/dev/null || echo ?)）"
if (cd "$BASE_DIR" && cargo test -q -p ccextra-server --test claude_fp_loopback); then
    RESULTS+=("Claude 车道  OK")
else
    RESULTS+=("Claude 车道  FAIL"); FAILED=1
fi

echo
echo "==> 汇总"
printf "  %s\n" "${RESULTS[@]}"
exit $FAILED
