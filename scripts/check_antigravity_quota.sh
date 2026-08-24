#!/usr/bin/env bash
set -euo pipefail

# 默认配置
BASE_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
AUTH_DIR="${BASE_DIR}/.cache/antigravity"
API_ENDPOINT="https://cloudcode-pa.googleapis.com"
DAILY_API_ENDPOINT="https://daily-cloudcode-pa.googleapis.com"
USER_AGENT="antigravity/hub/1.23.2 darwin/arm64"
OAUTH_TOKEN_URL="https://oauth2.googleapis.com/token"
CLIENT_ID="$(sed -n 's/.*pub const CLIENT_ID: &str = "\([^"]*\)".*/\1/p' "${BASE_DIR}/crates/ccextra-server/src/antigravity/constants.rs" 2>/dev/null || true)"
CLIENT_SECRET="$(sed -n 's/.*pub const CLIENT_SECRET: &str = "\([^"]*\)".*/\1/p' "${BASE_DIR}/crates/ccextra-server/src/antigravity/constants.rs" 2>/dev/null || true)"

usage() {
    echo "用法: $0 [选项]"
    echo "选项:"
    echo "  -d, --dir <dir>      凭证目录 (默认: ${AUTH_DIR})"
    echo "  -f, --file <file>    指定单个凭证文件"
    echo "  -a, --all            包含内部/隐藏模型 (默认过滤 chat_* / tab_*)"
    echo "  -r, --raw            输出原始 JSON 响应"
    echo "  -h, --help           显示帮助信息"
    exit 1
}

TARGET_FILE=""
RAW_OUTPUT=false
SHOW_ALL=false

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
        -a|--all)
            SHOW_ALL=true
            shift
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
    local access_token refresh_token timestamp expires_in now_ms exp_ms

    access_token="$(jq -r '.access_token // empty' "$cred_file")"
    refresh_token="$(jq -r '.refresh_token // empty' "$cred_file")"
    timestamp="$(jq -r '.timestamp // 0' "$cred_file")"
    expires_in="$(jq -r '.expires_in // 0' "$cred_file")"

    now_ms="$(python3 -c 'import time; print(int(time.time() * 1000))' 2>/dev/null || echo "$(($(date +%s) * 1000))")"
    exp_ms=$(( timestamp + expires_in * 1000 ))

    # 提前 3000 秒 (3,000,000 ms) 换票
    if [[ -z "$access_token" || $(( exp_ms - now_ms )) -lt 3000000 ]]; then
        if [[ -z "$refresh_token" ]]; then
            echo "$access_token"
            return
        fi

        local refresh_resp
        refresh_resp="$(curl -sS -X POST "$OAUTH_TOKEN_URL" \
            -H "Content-Type: application/x-www-form-urlencoded" \
            -d "client_id=${CLIENT_ID}&client_secret=${CLIENT_SECRET}&refresh_token=${refresh_token}&grant_type=refresh_token" 2>/dev/null || true)"

        local new_token
        new_token="$(echo "$refresh_resp" | jq -r '.access_token // empty')"
        if [[ -n "$new_token" ]]; then
            local new_exp
            new_exp="$(echo "$refresh_resp" | jq -r '.expires_in // 3599')"
            local tmp_file="${cred_file}.tmp.$$"
            local expired_str
            expired_str="$(date -u +"%Y-%m-%dT%H:%M:%SZ")"

            jq --arg token "$new_token" \
               --argjson ts "$now_ms" \
               --argjson exp "$new_exp" \
               --arg exp_str "$expired_str" \
               '.access_token = $token | .timestamp = $ts | .expires_in = $exp | .expired = $exp_str' \
               "$cred_file" > "$tmp_file" && mv "$tmp_file" "$cred_file"
            echo "$new_token"
            return
        fi
    fi

    echo "$access_token"
}

format_reset_time() {
    local utc_time="$1"
    if [[ -z "$utc_time" || "$utc_time" == "null" ]]; then
        echo "-"
        return
    fi
    # 转换为中国时区 (Asia/Shanghai)
    python3 -c '
import sys, datetime, zoneinfo
t = sys.argv[1].replace("Z", "+00:00")
dt = datetime.datetime.fromisoformat(t).astimezone(zoneinfo.ZoneInfo("Asia/Shanghai"))
print(dt.strftime("%Y-%m-%d %H:%M:%S"))
' "$utc_time" 2>/dev/null \
        || echo "$utc_time"
}

query_quota() {
    local cred_file="$1"
    local email project_id
    email="$(jq -r '.email // "未知"' "$cred_file")"
    project_id="$(jq -r '.project_id // ""' "$cred_file")"

    local token
    token="$(refresh_token_if_needed "$cred_file")"

    if [[ -z "$token" ]]; then
        echo "❌ [${email}] 无法获取有效 access_token"
        return
    fi

    local req_body
    if [[ -n "$project_id" ]]; then
        req_body="$(printf '{"project":"%s"}' "$project_id")"
    else
        req_body="{}"
    fi

    local resp=""
    for endpoint in "$API_ENDPOINT" "$DAILY_API_ENDPOINT"; do
        resp="$(curl -sS -X POST "${endpoint}/v1internal:fetchAvailableModels" \
            -H "Authorization: Bearer ${token}" \
            -H "Content-Type: application/json" \
            -H "User-Agent: ${USER_AGENT}" \
            -d "$req_body" 2>/dev/null || true)"
        if [[ -n "$resp" ]] && echo "$resp" | jq -e '.models' >/dev/null 2>&1; then
            break
        fi
    done

    if [[ "$RAW_OUTPUT" == true ]]; then
        echo "$resp" | jq .
        return
    fi

    if [[ -z "$resp" ]] || ! echo "$resp" | jq . >/dev/null 2>&1; then
        echo "❌ [${email}] 响应解析失败: ${resp}"
        return
    fi

    if echo "$resp" | jq -e '.error' >/dev/null 2>&1; then
        local err_msg
        err_msg="$(echo "$resp" | jq -r '.error.message // .error')"
        echo "❌ [${email}] 查询失败: ${err_msg}"
        return
    fi

    echo "=========================================================================================="
    echo "📧 账号: ${email} | 🆔 项目: ${project_id:-无}"
    echo "------------------------------------------------------------------------------------------"
    printf "%-32s %-10s %-20s %s\n" "模型 ID" "剩余额度" "重置时间" "模型名称"
    echo "------------------------------------------------------------------------------------------"

    echo "$resp" | jq -c --argjson show_all "$SHOW_ALL" '
        .models | to_entries[] |
        select($show_all or (.value.isInternal != true and (.key | startswith("chat_") or startswith("tab_") | not))) |
        {
            id: .key,
            display_name: (.value.displayName // .key),
            remaining: ((.value.quotaInfo.remainingFraction // 0) * 100 | floor),
            reset_time: (.value.quotaInfo.resetTime // "")
        }
    ' | while read -r item; do
        local m_id m_name m_rem m_reset m_reset_fmt
        m_id="$(echo "$item" | jq -r '.id')"
        m_name="$(echo "$item" | jq -r '.display_name')"
        m_rem="$(echo "$item" | jq -r '.remaining')%"
        m_reset="$(echo "$item" | jq -r '.reset_time')"
        m_reset_fmt="$(format_reset_time "$m_reset")"

        printf "%-32s %-10s %-20s %s\n" "$m_id" "$m_rem" "$m_reset_fmt" "$m_name"
    done
    echo "=========================================================================================="
}

if [[ -n "$TARGET_FILE" ]]; then
    if [[ ! -f "$TARGET_FILE" ]]; then
        echo "错误: 文件不存在: $TARGET_FILE" >&2
        exit 1
    fi
    query_quota "$TARGET_FILE"
else
    if [[ ! -d "$AUTH_DIR" ]]; then
        echo "错误: 凭证目录不存在: $AUTH_DIR" >&2
        exit 1
    fi

    files=()
    while IFS= read -r f; do
        files+=("$f")
    done < <(find "$AUTH_DIR" -maxdepth 1 -type f -name "*.json" 2>/dev/null || true)

    if [[ ${#files[@]} -eq 0 ]]; then
        echo "未在 ${AUTH_DIR} 找到任何 JSON 凭证文件"
        exit 0
    fi

    for f in "${files[@]}"; do
        query_quota "$f"
    done
fi
