#!/usr/bin/env bash
set -euo pipefail

# 默认配置
BASE_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
AUTH_DIR="${BASE_DIR}/.cache/xai"
DEFAULT_TOKEN_ENDPOINT="https://auth.x.ai/oauth2/token"
CLIENT_ID="b1a00492-073a-47ea-816f-4c329264a828"
CLI_CHAT_PROXY_BASE_URL="https://cli-chat-proxy.grok.com/v1"
CLI_VERSION="0.2.114"
CLI_USER_AGENT="grok-pager/${CLI_VERSION} grok-shell/${CLI_VERSION} (macos; aarch64)"

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

format_time_shanghai() {
    local utc_time="$1"
    if [[ -z "$utc_time" || "$utc_time" == "null" ]]; then
        echo "-"
        return
    fi
    python3 -c '
import sys, datetime, zoneinfo
t = sys.argv[1].replace("Z", "+00:00")
dt = datetime.datetime.fromisoformat(t).astimezone(zoneinfo.ZoneInfo("Asia/Shanghai"))
print(dt.strftime("%Y-%m-%d %H:%M:%S"))
' "$utc_time" 2>/dev/null || echo "$utc_time"
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

    # 查询周账单配额 (credits)
    local weekly_resp
    weekly_resp="$(curl -sS -X GET "${CLI_CHAT_PROXY_BASE_URL}/billing?format=credits" \
        -H "Authorization: Bearer ${token}" \
        -H "Accept: application/json" \
        -H "Content-Type: application/json" \
        -H "x-xai-token-auth: xai-grok-cli" \
        -H "x-grok-client-version: ${CLI_VERSION}" \
        -H "User-Agent: ${CLI_USER_AGENT}" 2>/dev/null || true)"

    # 查询月账单配额 (monthly)
    local monthly_resp
    monthly_resp="$(curl -sS -X GET "${CLI_CHAT_PROXY_BASE_URL}/billing" \
        -H "Authorization: Bearer ${token}" \
        -H "Accept: application/json" \
        -H "Content-Type: application/json" \
        -H "x-xai-token-auth: xai-grok-cli" \
        -H "x-grok-client-version: ${CLI_VERSION}" \
        -H "User-Agent: ${CLI_USER_AGENT}" 2>/dev/null || true)"

    # 查询模型列表
    local models_resp
    models_resp="$(curl -sS -X GET "${base_url}/models" \
        -H "Authorization: Bearer ${token}" \
        -H "Accept: application/json" \
        -H "Content-Type: application/json" \
        -H "x-xai-token-auth: xai-grok-cli" \
        -H "x-grok-client-version: ${CLI_VERSION}" \
        -H "User-Agent: ${CLI_USER_AGENT}" 2>/dev/null || true)"

    if [[ "$RAW_OUTPUT" == true ]]; then
        echo "=== 凭证: $(basename "$cred_file") ==="
        echo "--- 周额度 (credits) ---"
        echo "$weekly_resp"
        echo "--- 月额度 (monthly) ---"
        echo "$monthly_resp"
        echo "--- 模型列表 ---"
        echo "$models_resp"
        return
    fi

    echo "=========================================================================================="
    echo "📧 账号: ${email} (sub: ${sub})"
    echo "📁 文件: $(basename "$cred_file")"
    echo "🔗 端点: ${base_url}"
    echo "------------------------------------------------------------------------------------------"

    # 解析账单与配额
    python3 -c '
import sys, json, math

weekly_raw = sys.argv[1]
monthly_raw = sys.argv[2]

try:
    weekly = json.loads(weekly_raw).get("config", {})
except Exception:
    weekly = {}

try:
    monthly = json.loads(monthly_raw).get("config", {})
except Exception:
    monthly = {}

def parse_val(v):
    if v is None:
        return None
    if isinstance(v, dict):
        v = v.get("val")
    try:
        return float(v)
    except Exception:
        return None

# Plan
monthly_limit = parse_val(monthly.get("monthlyLimit"))
plan = "未知"
if monthly_limit is not None:
    if abs(monthly_limit - 15000) < 100:
        plan = "SuperGrok ($150/月)"
    elif abs(monthly_limit - 150000) < 100:
        plan = "SuperGrok Heavy ($1,500/月)"
    else:
        plan = f"定制计划 (${monthly_limit/100:.2f}/月)"
elif weekly.get("isUnifiedBillingUser"):
    plan = "Unified Billing"

print(f"💎 订阅计划: {plan}")

# Weekly credit usage
weekly_pct = weekly.get("creditUsagePercent")
cur_period = weekly.get("currentPeriod") or {}
p_start = cur_period.get("start", "-")
p_end = cur_period.get("end", "-")

if weekly_pct is not None:
    print(f"📊 本周用量: {weekly_pct * 100:.1f}% (周期: {p_start} ~ {p_end})")
elif p_end != "-":
    print(f"📊 本周周期: {p_start} ~ {p_end}")

# Monthly usage
monthly_used = parse_val(monthly.get("used"))
if monthly_limit is not None and monthly_used is not None:
    used_pct = (monthly_used / monthly_limit) * 100 if monthly_limit > 0 else 0
    print(f"💵 本月额度: ${monthly_used/100:.2f} / ${monthly_limit/100:.2f} ({used_pct:.1f}%)")
elif monthly_used is not None:
    print(f"💵 本月已用: ${monthly_used/100:.2f}")

# Prepaid / On-demand
prepaid = parse_val(weekly.get("prepaidBalance"))
ondemand_cap = parse_val(weekly.get("onDemandCap") or monthly.get("onDemandCap"))
ondemand_used = parse_val(weekly.get("onDemandUsed") or monthly.get("onDemandUsed"))

extras = []
if prepaid is not None and prepaid > 0:
    extras.append(f"预付余额: ${prepaid:.2f}")
if ondemand_cap is not None and ondemand_cap > 0:
    used_str = f"${ondemand_used:.2f}" if ondemand_used is not None else "$0"
    extras.append(f"按需额度: {used_str} / ${ondemand_cap:.2f}")

if extras:
    print("💰 " + " | ".join(extras))

# Product breakdown
product_usages = weekly.get("productUsage") or []
if product_usages:
    print("\n📦 产品明细:")
    for item in product_usages:
        p_name = item.get("product", "未知")
        u_pct = item.get("usagePercent")
        if u_pct is not None:
            print(f"  - {p_name:<20}: {u_pct * 100:.1f}%")
        else:
            print(f"  - {p_name}")
' "$weekly_resp" "$monthly_resp" 2>/dev/null || true

    echo "------------------------------------------------------------------------------------------"
    # 模型列表展示
    if echo "$models_resp" | jq -e '.models // .data' >/dev/null 2>&1; then
        local model_count
        model_count="$(echo "$models_resp" | jq '(.models // .data) | length')"
        echo "🤖 可用模型 (${model_count} 个):"
        echo "$models_resp" | jq -r '(.models // .data)[] | "  - " + (.id // .name // "unknown")' 2>/dev/null || true
    else
        local err_msg
        err_msg="$(echo "$models_resp" | jq -r '.error.message // .error // .message // "未知响应"' 2>/dev/null || echo "$models_resp")"
        echo "⚠️ 模型列表: ${err_msg}"
    fi
    echo "=========================================================================================="
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
