#!/usr/bin/env bash
set -euo pipefail

# Cursor 订阅额度与模型列表查询脚本
# 数据源: api2.cursor.sh DashboardService/GetCurrentPeriodUsage (Connect JSON)
#         + AgentService/GetUsableModels (protobuf)
# 刷新: /auth/exchange_user_api_key (对齐 ccextra cursor/oauth.rs,提前 10 分钟)

# 默认配置
BASE_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
AUTH_DIR="${BASE_DIR}/.cache/cursor"
CONFIG_FILE="${BASE_DIR}/config.yaml"
DEFAULT_BASE_URL="https://api2.cursor.sh"
DEFAULT_CLIENT_VERSION="cli-2026.02.13-41ac335"
# 对齐 ccextra cursor/constants.rs REFRESH_SKEW_SECS
REFRESH_SKEW_SECS=600

# 从 config.yaml 读取 cursor_base_url / cursor_client_version(可选)
read_yaml_str() {
    [[ -f "$CONFIG_FILE" ]] || return 0
    awk -v key="$1" '
        index($0, key ":") == 1 {
            line = $0
            sub("^" key ":[[:space:]]*", "", line)
            gsub(/^"|"$/, "", line)
            print line
            exit
        }' "$CONFIG_FILE"
}

BASE_URL="$(read_yaml_str cursor_base_url)"
CLIENT_VERSION="$(read_yaml_str cursor_client_version)"
[[ -z "$BASE_URL" ]] && BASE_URL="$DEFAULT_BASE_URL"
[[ -z "$CLIENT_VERSION" ]] && CLIENT_VERSION="$DEFAULT_CLIENT_VERSION"

usage() {
    echo "用法: $0 [选项]"
    echo "选项:"
    echo "  -d, --dir <dir>      凭证目录 (默认: ${AUTH_DIR})"
    echo "  -f, --file <file>    指定单个凭证文件"
    echo "  -r, --raw            输出原始响应"
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

if ! command -v python3 >/dev/null 2>&1; then
    echo "错误: 需要安装 python3" >&2
    exit 1
fi

# 解析 JWT payload 提取 sub / exp(对齐 ccextra cursor/credential.rs::jwt_claims)
decode_jwt() {
    local token="$1"
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
print(json.dumps({
    "sub": val.get("sub") or "",
    "exp": val.get("exp") or 0,
}))
' "$token" 2>/dev/null || echo "{}"
}

refresh_token_if_needed() {
    local cred_file="$1"
    local access_token refresh_token expires_at
    access_token="$(jq -r '.accessToken // empty' "$cred_file")"
    refresh_token="$(jq -r '.refreshToken // empty' "$cred_file")"
    expires_at="$(jq -r '.expires_at // 0' "$cred_file")"

    local need_refresh=false
    if [[ -z "$access_token" ]]; then
        need_refresh=true
    else
        local now_ts
        now_ts="$(date +%s)"
        if [[ $(( expires_at - now_ts )) -lt ${REFRESH_SKEW_SECS} ]]; then
            need_refresh=true
        fi
    fi

    if [[ "$need_refresh" == true && -n "$refresh_token" ]]; then
        local refresh_resp
        refresh_resp="$(curl -sS -X POST "${BASE_URL}/auth/exchange_user_api_key" \
            -H "Authorization: Bearer ${refresh_token}" \
            -H "Content-Type: application/json" \
            --max-time 15 \
            -d '{}' 2>/dev/null || true)"

        local new_token
        new_token="$(echo "$refresh_resp" | jq -r '.accessToken // empty')"
        if [[ -n "$new_token" ]]; then
            local new_ref identity new_exp new_sub tmp_file
            new_ref="$(echo "$refresh_resp" | jq -r '.refreshToken // empty')"
            identity="$(decode_jwt "$new_token")"
            new_exp="$(echo "$identity" | jq -r '.exp // 0')"
            new_sub="$(echo "$identity" | jq -r '.sub // empty')"
            # exp 缺失时兜底 now+3600(对齐 apply_tokens)
            if [[ "$new_exp" == "0" ]]; then
                new_exp=$(( $(date +%s) + 3600 ))
            fi
            tmp_file="${cred_file}.tmp.$$"
            # 对齐 apply_tokens: 空 refreshToken 不覆盖;sub 仅在 JWT 携带时更新
            jq --arg tok "$new_token" \
               --arg ref "$new_ref" \
               --argjson exp "$new_exp" \
               --arg sub "$new_sub" \
               '.accessToken = $tok
               | (if $ref != "" then .refreshToken = $ref else . end)
               | .expires_at = $exp
               | (if $sub != "" then .sub = $sub else . end)' \
               "$cred_file" > "$tmp_file" && mv "$tmp_file" "$cred_file"
            echo "$new_token"
            return
        fi
    fi

    echo "$access_token"
}

check_account() {
    local cred_file="$1"
    local sub
    sub="$(jq -r '.sub // "-"' "$cred_file")"

    local token
    token="$(refresh_token_if_needed "$cred_file")"

    if [[ -z "$token" ]]; then
        echo "❌ [${sub}] 无法获取有效 accessToken"
        return
    fi

    # 查询当前周期用量 (DashboardService Connect JSON)
    local usage_resp
    usage_resp="$(curl -sS -X POST "${BASE_URL}/aiserver.v1.DashboardService/GetCurrentPeriodUsage" \
        -H "Authorization: Bearer ${token}" \
        -H "Content-Type: application/json" \
        -H "Connect-Protocol-Version: 1" \
        --compressed --max-time 15 \
        -d '{}' 2>/dev/null || true)"

    # 查询模型列表 (AgentService GetUsableModels, protobuf, 对齐 ccextra cursor/models.rs)
    local models_b64
    models_b64="$(curl -sS -X POST "${BASE_URL}/agent.v1.AgentService/GetUsableModels" \
        -H "Content-Type: application/proto" \
        -H "Te: trailers" \
        -H "X-Ghost-Mode: true" \
        -H "X-Cursor-Client-Type: cli" \
        -H "X-Cursor-Client-Version: ${CLIENT_VERSION}" \
        -H "Authorization: Bearer ${token}" \
        --max-time 15 \
        --data-binary '' 2>/dev/null | base64 | tr -d '\n' || true)"

    if [[ "$RAW_OUTPUT" == true ]]; then
        echo "=== 凭证: $(basename "$cred_file") ==="
        echo "--- GetCurrentPeriodUsage ---"
        echo "$usage_resp" | jq . 2>/dev/null || echo "$usage_resp"
        echo "--- GetUsableModels (protobuf, base64) ---"
        echo "$models_b64"
        return
    fi

    echo "=========================================================================================="
    echo "🆔 账号: ${sub}"
    echo "📁 文件: $(basename "$cred_file")"
    echo "🔗 端点: ${BASE_URL}"
    echo "------------------------------------------------------------------------------------------"

    # 解析用量 (字段对齐 cursor-sdk2api cursor-dashboard.ts / OmniRoute usage/cursor.ts)
    python3 -c '
import sys, json, datetime, zoneinfo

usage_raw = sys.argv[1]
try:
    usage = json.loads(usage_raw)
except Exception:
    usage = None
if not isinstance(usage, dict):
    print("⚠️ 用量: 响应解析失败")
    sys.exit(0)

plan = usage.get("planUsage") or {}
spend = usage.get("spendLimitUsage") or {}

def cents(record, key):
    v = record.get(key)
    if isinstance(v, bool) or not isinstance(v, (int, float)):
        return None
    return v / 100.0

def fmt_time(v):
    if v is None:
        return "-"
    if isinstance(v, bool):
        return "-"
    if isinstance(v, str):
        # Cursor 可能把 epoch 毫秒返回成字符串
        if v.isdigit():
            v = int(v)
        else:
            return v
    if isinstance(v, (int, float)):
        if v <= 0:
            return "-"
        dt = datetime.datetime.fromtimestamp(v / 1000, datetime.timezone.utc)
        return dt.astimezone(zoneinfo.ZoneInfo("Asia/Shanghai")).strftime("%Y-%m-%d %H:%M:%S")
    return str(v)

limit = cents(plan, "limit")
remaining = cents(plan, "remaining")
used = cents(plan, "includedSpend")
if used is None and limit is not None and remaining is not None:
    used = max(0.0, limit - remaining)
total_spend = cents(plan, "totalSpend")
total_pct = plan.get("totalPercentUsed")
api_pct = plan.get("apiPercentUsed")
auto_pct = plan.get("autoPercentUsed")

if limit is not None and used is not None:
    pct = (used / limit * 100) if limit > 0 else 0.0
    print(f"📊 本月额度: ${used:.2f} / ${limit:.2f} ({pct:.1f}%)")
elif total_pct is not None:
    print(f"📊 本月用量: {total_pct:.1f}%")
if total_spend is not None:
    print(f"💵 本月总花费: ${total_spend:.2f}")
if total_pct is not None or api_pct is not None or auto_pct is not None:
    print("📦 用量明细:")
    if total_pct is not None:
        print(f"  - Cursor 模型: {total_pct:.1f}%")
    if auto_pct is not None:
        print(f"  - Auto/Composer: {auto_pct:.1f}%")
    if api_pct is not None:
        print(f"  - API/其他模型: {api_pct:.1f}%")

on_demand_used = cents(spend, "totalSpend")
on_demand_limit = cents(spend, "individualLimit")
if on_demand_limit is None:
    on_demand_limit = cents(spend, "pooledLimit")
if on_demand_limit is not None:
    u = f"${on_demand_used:.2f}" if on_demand_used is not None else "$0"
    print(f"💰 按需额度: {u} / ${on_demand_limit:.2f}")
elif on_demand_used is not None:
    print(f"💰 按需已用: ${on_demand_used:.2f}")

cycle_start = fmt_time(usage.get("billingCycleStart"))
cycle_end = fmt_time(usage.get("billingCycleEnd"))
print(f"📅 账单周期: {cycle_start} ~ {cycle_end}")
' "$usage_resp" 2>/dev/null || echo "⚠️ 用量: 解析失败"

    # 解析模型列表 (GetUsableModelsResponse.models[].model_id/display_name)
    python3 -c '
import sys, base64

def read_varint(data, pos):
    result = 0
    shift = 0
    while pos < len(data):
        b = data[pos]
        pos += 1
        result |= (b & 0x7F) << shift
        if not (b & 0x80):
            return result, pos
        shift += 7
    raise ValueError("truncated varint")

def strip_frames(data):
    # Connect 信封: 1 字节 flags + 4 字节大端长度;flags 合法位 0x01/0x02
    # 原始 protobuf 首字节是字段 tag(如 0x0a),不会落进合法 flags 范围
    if not data or data[0] & ~0x03:
        return data
    out = b""
    pos = 0
    while pos < len(data):
        if pos + 5 > len(data):
            return data
        flags = data[pos]
        length = int.from_bytes(data[pos + 1:pos + 5], "big")
        end = pos + 5 + length
        if flags & ~0x03 or end > len(data):
            return data
        if not flags & 0x02:
            out += data[pos + 5:end]
        pos = end
    return out

def parse_model(blob):
    # ModelDetails: model_id = 1, display_name = 4
    m = {"model_id": "", "display_name": ""}
    pos = 0
    while pos < len(blob):
        tag, pos = read_varint(blob, pos)
        field, wire = tag >> 3, tag & 7
        if wire == 2:
            length, pos = read_varint(blob, pos)
            val = blob[pos:pos + length]
            pos += length
            if field == 1:
                m["model_id"] = val.decode("utf-8", "replace")
            elif field == 4:
                m["display_name"] = val.decode("utf-8", "replace")
        elif wire == 0:
            _, pos = read_varint(blob, pos)
        elif wire == 5:
            pos += 4
        elif wire == 1:
            pos += 8
        else:
            break
    return m

raw = base64.b64decode(sys.argv[1] or "")
data = strip_frames(raw)
models = []
pos = 0
while pos < len(data):
    tag, pos = read_varint(data, pos)
    field, wire = tag >> 3, tag & 7
    if field == 1 and wire == 2:
        length, pos = read_varint(data, pos)
        models.append(parse_model(data[pos:pos + length]))
        pos += length
    elif wire == 0:
        _, pos = read_varint(data, pos)
    elif wire == 2:
        length, pos = read_varint(data, pos)
        pos += length
    elif wire == 5:
        pos += 4
    elif wire == 1:
        pos += 8
    else:
        break

if models:
    print(f"🤖 可用模型 ({len(models)} 个):")
    for m in models:
        mid = m["model_id"] or "unknown"
        name = m["display_name"]
        if name and name != mid:
            print(f"  - {mid} ({name})")
        else:
            print(f"  - {mid}")
else:
    print("⚠️ 模型列表: 未解析到模型(响应可能为空或格式变化)")
' "$models_b64" 2>/dev/null || echo "⚠️ 模型列表: 解析失败"
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

    local cred_file="${AUTH_DIR}/cursor.json"
    if [[ ! -f "$cred_file" ]]; then
        echo "未找到 Cursor 凭证: ${cred_file}"
        exit 0
    fi
    check_account "$cred_file"
}

main
