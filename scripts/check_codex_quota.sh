#!/usr/bin/env bash
set -euo pipefail

# Codex (OpenAI ChatGPT 订阅) 额度查询脚本
# 数据源: chatgpt.com/backend-api/wham/usage (对齐 sub2api QueryUsage)
# 刷新: auth.openai.com/oauth/token (对齐 ccextra codex/oauth.rs 与 CPA RefreshLead=24h)

# 默认配置
BASE_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
AUTH_DIR="${BASE_DIR}/.cache/codex"
TOKEN_URL="https://auth.openai.com/oauth/token"
CLIENT_ID="app_EMoamEEZ73f0CkXaXp7hrann"
REFRESH_SCOPE="openid profile email"
USAGE_URL="https://chatgpt.com/backend-api/wham/usage"
RESET_CREDITS_URL="https://chatgpt.com/backend-api/wham/rate-limit-reset-credits"
# 对齐 ccextra DEFAULT_CODEX_TUI
CODEX_UA="codex_cli_rs/0.153.3 (Mac OS 26.6.2; arm64)"
# 提前刷新窗口 (秒): 对齐 CPA CodexAuthenticator.RefreshLead = 24h
REFRESH_SKEW_SECS=86400

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

# 解析 JWT ID Token 提取 email / chatgpt_account_id / chatgpt_plan_type
# (对齐 ccextra codex/credential.rs::parse_jwt_identity)
decode_jwt_identity() {
    local id_token="$1"
    python3 -c '
import sys, json, base64
parts = sys.argv[1].split(".")
if len(parts) < 2:
    print("{}")
    sys.exit(0)
payload = parts[1]
payload += "=" * (-len(payload) % 4)
try:
    val = json.loads(base64.urlsafe_b64decode(payload))
except Exception:
    print("{}")
    sys.exit(0)
auth = val.get("https://api.openai.com/auth") or {}
print(json.dumps({
    "email": val.get("email") or "",
    "account_id": auth.get("chatgpt_account_id") or "",
    "plan_type": auth.get("chatgpt_plan_type") or "",
}))
' "$id_token" 2>/dev/null || echo "{}"
}

refresh_token_if_needed() {
    local cred_file="$1"
    local access_token refresh_token expired_str
    access_token="$(jq -r '.access_token // empty' "$cred_file")"
    refresh_token="$(jq -r '.refresh_token // empty' "$cred_file")"
    expired_str="$(jq -r '.expired // empty' "$cred_file")"

    local need_refresh=false
    if [[ -z "$access_token" ]]; then
        need_refresh=true
    elif [[ -n "$expired_str" ]]; then
        local now_ts exp_ts
        now_ts="$(date +%s)"
        exp_ts="$(python3 -c 'import sys, datetime; print(int(datetime.datetime.fromisoformat(sys.argv[1].replace("Z", "+00:00")).timestamp()))' "$expired_str" 2>/dev/null || echo 0)"
        if [[ $(( exp_ts - now_ts )) -lt $REFRESH_SKEW_SECS ]]; then
            need_refresh=true
        fi
    fi

    if [[ "$need_refresh" == true && -n "$refresh_token" ]]; then
        local refresh_resp
        refresh_resp="$(curl -sS -X POST "$TOKEN_URL" \
            -H "Content-Type: application/x-www-form-urlencoded" \
            -H "Accept: application/json" \
            -d "client_id=${CLIENT_ID}&grant_type=refresh_token&refresh_token=${refresh_token}&scope=${REFRESH_SCOPE}" 2>/dev/null || true)"

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

            # 对齐 apply_tokens: 空值不覆盖;id_token 变化时重解析身份
            local identity="{}"
            if [[ -n "$new_id" ]]; then
                identity="$(decode_jwt_identity "$new_id")"
            fi
            jq --argjson identity "$identity" \
               --arg tok "$new_token" \
               --arg ref "$new_ref" \
               --arg idt "$new_id" \
               --argjson exp "$new_exp" \
               --arg exp_str "$new_expired_str" \
               --arg ref_str "$now_iso" \
               ' .access_token = $tok
               | (if $ref != "" then .refresh_token = $ref else . end)
               | (if $idt != "" then .id_token = $idt else . end)
               | (if ($identity // null) != null then
                    (if ($identity.email // "") != "" then .email = $identity.email else . end)
                    | (if ($identity.account_id // "") != "" then .account_id = $identity.account_id else . end)
                    | (if ($identity.plan_type // "") != "" then .plan_type = $identity.plan_type else . end)
                  else . end)
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

# wham/usage 请求头 (对齐 sub2api buildCodexCommonHeaders + ccextra codex UA)
codex_quota_headers() {
    local token="$1"
    local account_id="$2"
    printf '%s' \
        "Authorization: Bearer ${token}" \
        "|chatgpt-account-id: ${account_id}" \
        "|openai-beta: codex-1" \
        "|oai-language: zh-CN" \
        "|originator: Codex Desktop" \
        "|accept: application/json" \
        "|sec-fetch-site: none" \
        "|sec-fetch-mode: no-cors" \
        "|sec-fetch-dest: empty" \
        "|priority: u=4, i" \
        "|User-Agent: ${CODEX_UA}"
}

check_account() {
    local cred_file="$1"
    local email account_id
    email="$(jq -r '.email // "-"' "$cred_file")"
    account_id="$(jq -r '.account_id // empty' "$cred_file")"
    # account_id 缺失时从 id_token JWT 恢复
    if [[ -z "$account_id" ]]; then
        local id_token identity
        id_token="$(jq -r '.id_token // empty' "$cred_file")"
        if [[ -n "$id_token" ]]; then
            identity="$(decode_jwt_identity "$id_token")"
            account_id="$(echo "$identity" | jq -r '.account_id // empty')"
            if [[ -z "$email" || "$email" == "-" ]]; then
                email="$(echo "$identity" | jq -r '.email // empty')"
                [[ -z "$email" ]] && email="-"
            fi
        fi
    fi

    local token
    token="$(refresh_token_if_needed "$cred_file")"

    if [[ -z "$token" ]]; then
        echo "❌ [${email}] 无法获取有效 access_token"
        return
    fi

    if [[ -z "$account_id" ]]; then
        echo "❌ [${email}] 缺少 account_id (chatgpt_account_id),请重新 codex-login"
        return
    fi

    local header_spec
    header_spec="$(codex_quota_headers "$token" "$account_id")"
    local curl_headers=()
    local IFS='|'
    for h in $header_spec; do
        curl_headers+=(-H "$h")
    done
    unset IFS

    # 查询用量与限额
    local usage_resp
    usage_resp="$(curl -sS -X GET "$USAGE_URL" "${curl_headers[@]}" 2>/dev/null || true)"

    # 查询可重置额度明细
    local credits_resp
    credits_resp="$(curl -sS -X GET "$RESET_CREDITS_URL" "${curl_headers[@]}" 2>/dev/null || true)"

    if [[ "$RAW_OUTPUT" == true ]]; then
        echo "=== 凭证: $(basename "$cred_file") ==="
        echo "--- wham/usage ---"
        echo "$usage_resp" | jq . 2>/dev/null || echo "$usage_resp"
        echo "--- rate-limit-reset-credits ---"
        echo "$credits_resp" | jq . 2>/dev/null || echo "$credits_resp"
        return
    fi

    if ! echo "$usage_resp" | jq . >/dev/null 2>&1; then
        echo "❌ [${email}] 响应解析失败: ${usage_resp}"
        return
    fi

    if echo "$usage_resp" | jq -e '.detail // .error' >/dev/null 2>&1; then
        local err_msg
        err_msg="$(echo "$usage_resp" | jq -r '.detail // .error.message // .error')"
        echo "❌ [${email}] 查询失败: ${err_msg}"
        return
    fi

    echo "=========================================================================================="
    echo "📧 账号: ${email} | 🆔 account: ${account_id}"
    echo "📁 文件: $(basename "$cred_file")"
    echo "------------------------------------------------------------------------------------------"

    python3 -c '
import sys, json, datetime, zoneinfo

usage = json.loads(sys.argv[1])
credits_raw = sys.argv[2]

def sh(epoch):
    if not epoch:
        return "-"
    dt = datetime.datetime.fromtimestamp(int(epoch), zoneinfo.ZoneInfo("UTC"))
    return dt.astimezone(zoneinfo.ZoneInfo("Asia/Shanghai")).strftime("%Y-%m-%d %H:%M:%S")

def window_label(seconds):
    if not seconds:
        return "-"
    minutes = seconds / 60
    if abs(minutes - 10080) < 1:
        return "周"
    if abs(minutes - 300) < 1:
        return "5小时"
    return f"{minutes/60:.1f}小时"

plan = usage.get("plan_type") or "-"
print(f"💎 订阅计划: {plan}")

rl = usage.get("rate_limit") or {}
if rl.get("allowed") is False:
    print("⛔ 当前已被限流 (allowed=false)")
if rl.get("limit_reached"):
    print("⛔ 限额已触达 (limit_reached=true)")

rows = []
for name, key in (("primary", "primary_window"), ("secondary", "secondary_window")):
    w = rl.get(key)
    if not isinstance(w, dict):
        continue
    used = w.get("used_percent")
    label = window_label(w.get("limit_window_seconds"))
    reset = sh(w.get("reset_at"))
    if used is None:
        rows.append(f"  - {name:<10} ({label}): 无数据")
    else:
        rows.append(f"  - {name:<10} ({label}): 已用 {used:.1f}% | 重置 {reset}")
if rows:
    print("📊 限额窗口:")
    print("\n".join(rows))

# additional_rate_limits: /wham/usage 为数组,websocket 事件为对象,两种都兼容
addl = usage.get("additional_rate_limits") or []
if isinstance(addl, dict):
    addl = [{"limit_name": k, **(v if isinstance(v, dict) else {})} for k, v in addl.items()]
if addl:
    print("📦 附加限额:")
    for item in addl:
        name = item.get("limit_name") or item.get("metered_feature") or "未知"
        sub = item.get("rate_limit") or {}
        w = sub.get("primary_window") or {}
        used = w.get("used_percent")
        if used is None:
            print(f"  - {name}")
        else:
            reset = sh(w.get("reset_at"))
            print(f"  - {name:<28}: 已用 {used:.1f}% | 重置 {reset}")

credits = usage.get("credits")
if isinstance(credits, dict):
    if credits.get("unlimited"):
        print("💰 余额: 无限 (unlimited)")
    elif credits.get("has_credits"):
        balance = credits.get("balance") or "未知"
        print(f"💰 余额: {balance}")
    else:
        print("💰 余额: 无 (订阅内额度)")

try:
    rc = json.loads(credits_raw)
except Exception:
    rc = None
if isinstance(rc, dict) and rc.get("available_count") is not None:
    count = rc.get("available_count")
    print(f"🔄 可重置额度: {count} 个")
    for c in (rc.get("credits") or [])[:5]:
        exp = c.get("expires_at") or "-"
        print(f"  - 过期: {exp}")
' "$usage_resp" "$credits_resp" 2>/dev/null || true
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
        echo "未找到凭证目录: ${AUTH_DIR}"
        exit 0
    fi

    local files=()
    while IFS= read -r -d $'\0' f; do
        files+=("$f")
    done < <(find "$AUTH_DIR" -maxdepth 1 -name "codex-*.json" -print0 2>/dev/null || true)

    if [[ ${#files[@]} -eq 0 ]]; then
        echo "无 Codex 凭证文件: ${AUTH_DIR}"
        return
    fi

    for f in "${files[@]}"; do
        check_account "$f"
    done
}

main
