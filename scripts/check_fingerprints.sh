#!/usr/bin/env bash
# zsh/sh 直接执行时转投 bash
if [ -z "${BASH_VERSION:-}" ]; then
    exec bash "$0" "$@"
fi
set -uo pipefail

# 本机回环检查出站 TLS 指纹有没有漂移（不联网）：
#   Claude 车道：ccextra 的 claude_fp_loopback 测试，对比 TARGET_JA4 / TARGET_JA3_HASH
#   AG sidecar ：ccextra-gohttp 的 TestClientHelloMatchesTarget，对比官方 language_server
# 用法：scripts/check_fingerprints.sh [--claude-only | --ag-only]
# ccextra-gohttp 默认在 ccextra 同级目录，可用 GOHTTP_DIR 覆盖。
# 升级 wreq/btls（cargo update）或 sidecar 的 Go 版本后必跑；失败时看输出里的 ja4_r / ja3 原串。

BASE_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
GOHTTP_DIR="${GOHTTP_DIR:-${BASE_DIR}/../ccextra-gohttp}"
RUN_CLAUDE=true
RUN_AG=true

case "${1:-}" in
    "") ;;
    --claude-only) RUN_AG=false ;;
    --ag-only) RUN_CLAUDE=false ;;
    -h|--help) sed -n "8,13p" "$0"; exit 0 ;;
    *) echo "未知参数: $1（可用 --claude-only / --ag-only）" >&2; exit 2 ;;
esac

declare -a RESULTS=()
FAILED=0

if $RUN_CLAUDE; then
    echo "==> Claude 车道（ccextra $(git -C "$BASE_DIR" rev-parse --short HEAD 2>/dev/null || echo ?)）"
    if (cd "$BASE_DIR" && cargo test -q -p ccextra-server --test claude_fp_loopback); then
        RESULTS+=("Claude 车道  OK")
    else
        RESULTS+=("Claude 车道  FAIL"); FAILED=1
    fi
fi

if $RUN_AG; then
    echo "==> AG sidecar（${GOHTTP_DIR}）"
    if [ ! -f "${GOHTTP_DIR}/ja4_test.go" ]; then
        echo "找不到 ${GOHTTP_DIR}/ja4_test.go，请设置 GOHTTP_DIR" >&2
        RESULTS+=("AG sidecar   FAIL（目录不对）"); FAILED=1
    elif ! command -v go >/dev/null 2>&1; then
        echo "PATH 里没有 go" >&2
        RESULTS+=("AG sidecar   FAIL（没有 go）"); FAILED=1
    elif (cd "$GOHTTP_DIR" && go test -count=1 -run "^TestClientHelloMatchesTarget$" .); then
        RESULTS+=("AG sidecar   OK（$(go version | cut -d" " -f3)）")
    else
        RESULTS+=("AG sidecar   FAIL"); FAILED=1
    fi
fi

echo
echo "==> 汇总"
printf "  %s\n" "${RESULTS[@]}"
exit $FAILED
