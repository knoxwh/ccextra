#!/usr/bin/env bash
set -euo pipefail

# 默认配置
BASE_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
AUTH_DIR="${BASE_DIR}/.cache/xai"
DEFAULT_TOKEN_ENDPOINT="https://auth.x.ai/oauth2/token"
CLIENT_ID="b1a00492-073a-47ea-816f-4c329264a828"
CLI_CHAT_PROXY_BASE_URL="https://chat.x.ai/api"

usage() {
    echo "用法: $0 [选项]"
    echo "选项:"
    echo "  -d, --dir <dir>      凭证目录 (默认: ${AUTH_DIR})"
    echo "  -f, --file <file>    指定单个凭证文件"
    echo "  -r, --raw            输出原始 JSON 响应"
    echo "  -h, --help           显示帮助信息"
    exit 1
}

TARGET_FILE=""
RAW_OUTPUT=false

while [[ $# -gt 0 ]]; do
    case "$1" in
        -d|--dir)
            AUTH_DIR="$2"
            shift 2
            ;;
        -f|--file)
            TARGET_FILE="$2"
            shift 2
            ;;
        -r|--raw)
            RAW_OUTPUT=true
            shift
            ;;
        -h|--help)
            usage
            ;;
        *)
            echo "未知参数: $1"
            usage
            ;;
    esac
done

if ! command -v jq >/dev/null 2>&1; then
    echo "错误: 需要安装 jq" >&2
    exit 1
fi

if ! command -v curl >/dev/null 2>&1; then
    echo "错误: 需要安装 curl" >&2
    exit 1
fi

refresh_token_if_needed() {
    local cred_file="$1"
    local access_token refresh_token expired_str token_endpoint
    access_token="$(jq -r '.access_token // empty' "$cred_file")"
    refresh_token="$(jq -r '.refresh_token // empty' "$cred_file")"
    expired_str="$(jq -r '.expired // empty' "$cred_file")"
    token_endpoint="$(jq -r '.token_endpoint // empty' "$cred_file")"
    if [[ -z "$token_endpoint" ]]; then
        token_endpoint="$DEFAULT_TOKEN_ENDPOINT"
    fi

    local need_refresh=false
    if [[ -z "$access_token" ]]; then
        need_refresh=true
    elif [[ -n "$expired_str" ]]; then
        local now_ts exp_ts
        now_ts="$(date +%s)"
        exp_ts="$(python3 -c 'import sys, datetime; print(int(datetime.datetime.fromisoformat(sys.argv[1].replace("Z", "+00:00")).timestamp()))' "$expired_str" 2>/dev/null || echo 0)"
        if [[ $(( exp_ts - now_ts )) -lt 300 ]]; then
            need_refresh=true
        fi
    fi

    if [[ "$need_refresh" == true && -n "$refresh_token" ]]; then
        local refresh_resp
        refresh_resp="$(curl -sS -X POST "$token_endpoint" \
            -H "Content-Type: application/x-www-form-urlencoded" \
            -d "client_id=${CLIENT_ID}&refresh_token=${refresh_token}&grant_type=refresh_token" 2>/dev/null || true)"

        local new_token
        new_token="$(echo "$refresh_resp" | jq -r '.access_token // empty')"
        if [[ -n "$new_token" ]]; then
            local new_ref new_id new_exp new_expired_str now_iso tmp_file
            new_ref="$(echo "$refresh_resp" | jq -r '.refresh_token // empty')"
            new_id="$(echo "$refresh_resp" | jq -r '.id_token // empty')"
            new_exp="$(echo "$refresh_resp" | jq -r '.expires_in // 3600')"
            now_iso="$(date -u +"%Y-%m-%dT%H:%M:%SZ")"
            new_expired_str="$(python3 -c 'import datetime; print((datetime.datetime.now(datetime.timezone.utc) + datetime.timedelta(seconds='"$new_exp"')).strftime("%Y-%m-%dT%H:%M:%SZ"))')"
            tmp_file="${cred_file}.tmp.$$"

            jq --arg tok "$new_token" \
               --arg ref "$new_ref" \
               --arg idt "$new_id" \
               --argjson exp "$new_exp" \
               --arg exp_str "$new_expired_str" \
               --arg ref_str "$now_iso" \
               ' .access_token = $tok
               | (if $ref != "" then .refresh_token = $ref else . end)
               | (if $idt != "" then .id_token = $idt else . end)
               | .expires_in = $exp
               | .expired = $exp_str
               | .last_refresh = $ref_str' \
               "$cred_file" > "$tmp_file" && mv "$tmp_file" "$cred_file"
            echo "$new_token"
            return
        fi
    fi

    echo "$access_token"
}

check_account() {
    local cred_file="$1"
    local email sub base_url
    email="$(jq -r '.email // "-"' "$cred_file")"
    sub="$(jq -r '.sub // "-"' "$cred_file")"
    base_url="$(jq -r '.base_url // empty' "$cred_file")"
    if [[ -z "$base_url" ]]; then
        base_url="$CLI_CHAT_PROXY_BASE_URL"
    fi

    local token
    token="$(refresh_token_if_needed "$cred_file")"

    if [[ -z "$token" ]]; then
        echo "❌ [${email}] 无法获取有效 access_token"
        return
    fi

    # 查询 xAI API 测试连接与模型状态
    local resp
    resp="$(curl -sS -X GET "${base_url}/models" \
        -H "Authorization: Bearer ${token}" \
        -H "Accept: application/json" 2>/dev/null || true)"

    if [[ "$RAW_OUTPUT" == true ]]; then
        echo "=== 凭证: $(basename "$cred_file") ==="
        echo "$resp"
        return
    fi

    echo "=================================================================="
    echo "📧 账号: ${email} (sub: ${sub})"
    echo "📁 文件: $(basename "$cred_file")"
    echo "🔗 端点: ${base_url}"

    if echo "$resp" | jq -e '.models // .data' >/dev/null 2>&1; then
        echo "✅ 认证状态: 有效"
        local model_count
        model_count="$(echo "$resp" | jq '(.models // .data) | length')"
        echo "🤖 可用模型数量: ${model_count}"
        echo "$resp" | jq -r '(.models // .data)[] | "  - " + (.id // .name // "unknown")' 2>/dev/null || true
    else
        local err_msg
        err_msg="$(echo "$resp" | jq -r '.error.message // .error // .message // "未知响应"' 2>/dev/null || echo "$resp")"
        echo "⚠️ 状态: ${err_msg}"
    fi
    echo "=================================================================="
}

main() {
    if [[ -n "$TARGET_FILE" ]]; then
        if [[ ! -f "$TARGET_FILE" ]]; then
            echo "错误: 文件不存在: $TARGET_FILE" >&2
            exit 1
        fi
        check_account "$TARGET_FILE"
        return
    fi

    if [[ ! -d "$AUTH_DIR" ]]; then
        echo "未找到凭证目录: $AUTH_DIR"
        exit 0
    fi

    local files=()
    while IFS= read -r -d $'\0' f; do
        files+=("$f")
    done < <(find "$AUTH_DIR" -maxdepth 1 -name "xai-*.json" -print0 2>/dev/null || true)

    if [[ ${#files[@]} -eq 0 ]]; then
        echo "无 xAI 凭证文件: $AUTH_DIR"
        return
    fi

    for f in "${files[@]}"; do
        check_account "$f"
    done
}

main
