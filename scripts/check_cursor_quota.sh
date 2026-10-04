#!/usr/bin/env bash
# zsh/sh 直接执行时转投 bash(脚本用 bash 语法,BASH_SOURCE 等)
if [ -z "${BASH_VERSION:-}" ]; then
    exec bash "$0" "$@"
fi
set -euo pipefail

# Cursor 订阅额度与模型目录查询脚本
# 额度: api2.cursor.sh DashboardService/GetCurrentPeriodUsage (Connect JSON)
# 模型: agent.v1.AgentService/GetUsableModels (原生 unary protobuf,默认附带;--models 仅列模型)
# 刷新: /auth/exchange_user_api_key (对齐 ccextra cursor/oauth.rs,提前 10 分钟)

# 默认配置
BASE_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
AUTH_DIR="${BASE_DIR}/.cache/cursor"
CONFIG_FILE="${BASE_DIR}/config.yaml"
DEFAULT_BASE_URL="https://api2.cursor.sh"
DEFAULT_CLIENT_VERSION="cli-2026.10.01-e373342"
# 对齐 ccextra cursor/constants.rs REFRESH_SKEW_SECS
REFRESH_SKEW_SECS=600

# 从 config.yaml 读取 cursor_base_url(可选)
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
[[ -z "$BASE_URL" ]] && BASE_URL="$DEFAULT_BASE_URL"
CLIENT_VERSION="$(read_yaml_str cursor_client_version)"
[[ -z "$CLIENT_VERSION" ]] && CLIENT_VERSION="$DEFAULT_CLIENT_VERSION"

usage() {
    echo "用法: $0 [选项]"
    echo "选项:"
    echo "  -m, --models         仅列出模型目录(默认额度+模型都展示)"
    echo "  -d, --dir <dir>      凭证目录 (默认: ${AUTH_DIR})"
    echo "  -f, --file <file>    指定单个凭证文件"
    echo "  -r, --raw            输出原始响应"
    echo "  -h, --help           显示帮助信息"
    exit 1
}

TARGET_FILE=""
RAW_OUTPUT=false
LIST_MODELS=false

while [[ $# -gt 0 ]]; do
    case "$1" in
        -m|--models)
            LIST_MODELS=true
            shift
            ;;
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

# 查询当前周期用量并打印 (DashboardService Connect JSON)
query_usage() {
    local token="$1" sub="$2" file_label="$3"
    local usage_resp
    usage_resp="$(curl -sS -X POST "${BASE_URL}/aiserver.v1.DashboardService/GetCurrentPeriodUsage" \
        -H "Authorization: Bearer ${token}" \
        -H "Content-Type: application/json" \
        -H "Connect-Protocol-Version: 1" \
        --compressed --max-time 15 \
        -d '{}' 2>/dev/null || true)"

    if [[ "$RAW_OUTPUT" == true ]]; then
        echo "=== 凭证: ${file_label} ==="
        echo "--- GetCurrentPeriodUsage ---"
        echo "$usage_resp" | jq . 2>/dev/null || echo "$usage_resp"
        return
    fi

    # 上游拒绝凭证时直接点破,不打印空用量
    if echo "$usage_resp" | jq -e '.code == "unauthenticated"' >/dev/null 2>&1; then
        echo "❌ [${file_label}] 凭证被上游拒绝(unauthenticated)"
        return
    fi

    echo "=========================================================================================="
    echo "🆔 账号: ${sub}"
    echo "📁 文件: ${file_label}"
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

    echo "=========================================================================================="
}

# 模型目录:GetUsableModels unary,空 body protobuf,响应可能带 Connect 帧信封
# 二进制响应不能过 shell 变量(剥 null 字节),落临时文件
list_models() {
    local token="$1"
    local resp_file status=0
    resp_file="$(mktemp)"
    curl -sS -X POST "${BASE_URL}/agent.v1.AgentService/GetUsableModels" \
        -H "Authorization: Bearer ${token}" \
        -H "Content-Type: application/proto" \
        -H "Connect-Protocol-Version: 1" \
        -H "Te: trailers" \
        -H "X-Ghost-Mode: true" \
        -H "X-Cursor-Client-Type: cli" \
        -H "X-Cursor-Client-Version: ${CLIENT_VERSION}" \
        --max-time 15 \
        -o "$resp_file" \
        --data-binary @- <<'CURL_EOF' 2>/dev/null || true
CURL_EOF

    # AvailableModels 带思考档参数名(reasoning_effort 或 effort)。取不到仍列出目录。
    local avail_file raw_flag=""
    avail_file="$(mktemp)"
    curl -sS -X POST "${BASE_URL}/aiserver.v1.AiService/AvailableModels" \
        -H "Authorization: Bearer ${token}" \
        -H "Content-Type: application/proto" \
        -H "Connect-Protocol-Version: 1" \
        -H "Te: trailers" \
        -H "X-Ghost-Mode: true" \
        -H "X-Cursor-Client-Type: cli" \
        -H "X-Cursor-Client-Version: ${CLIENT_VERSION}" \
        --max-time 15 \
        -o "$avail_file" \
        --data-binary '' 2>/dev/null || true
    [[ "$RAW_OUTPUT" == true ]] && raw_flag="--raw"
    python3 - "$resp_file" "$avail_file" "$raw_flag" <<'PY_EOF' || status=$?
import sys, json, struct

data = open(sys.argv[1], "rb").read()

def read_varint(buf, pos):
    result = 0
    shift = 0
    while pos < len(buf):
        byte = buf[pos]
        result |= (byte & 0x7F) << shift
        pos += 1
        if not byte & 0x80:
            return result, pos
        shift += 7
    raise ValueError("truncated varint")

def fields(buf):
    pos = 0
    while pos < len(buf):
        tag, pos = read_varint(buf, pos)
        number, wire = tag >> 3, tag & 7
        if wire == 2:
            length, pos = read_varint(buf, pos)
            yield number, buf[pos:pos + length]
            pos += length
        elif wire == 0:
            value, pos = read_varint(buf, pos)
            yield number, value
        else:
            raise ValueError(f"unsupported wire type {wire}")

def unwrap_connect(payload):
    if len(payload) < 5 or payload[0] not in (0x00, 0x02):
        return payload
    frames = []
    offset = 0
    while offset < len(payload):
        if offset + 5 > len(payload):
            return payload
        flags = payload[offset]
        length = struct.unpack(">I", payload[offset + 1:offset + 5])[0]
        if offset + 5 + length > len(payload):
            return payload
        if flags == 0x00:
            frames.append(payload[offset + 5:offset + 5 + length])
        offset += 5 + length
    return frames[0] if frames else payload

def is_text(blob):
    return bool(blob) and all(32 <= byte < 127 for byte in blob)

def collect_option_ids(blob):
    found = []
    try:
        items = list(fields(blob))
    except ValueError:
        return found
    for number, value in items:
        if not isinstance(value, bytes) or is_text(value):
            continue
        if number == 1:
            try:
                inner = list(fields(value))
            except ValueError:
                continue
            label = next(
                (
                    item.decode()
                    for field, item in inner
                    if field == 1 and isinstance(item, bytes) and is_text(item)
                ),
                "",
            )
            if label:
                found.append(label)
            else:
                found.extend(collect_option_ids(value))
        else:
            found.extend(collect_option_ids(value))
    return found

def effort_param(model_blob):
    # AvailableModels 模型消息 field 29 = 参数定义
    for number, value in fields(model_blob):
        if number != 29 or not isinstance(value, bytes):
            continue
        param_id = ""
        values = []
        for field, sub in fields(value):
            if field == 1 and isinstance(sub, bytes) and is_text(sub):
                param_id = sub.decode()
            elif field == 4 and isinstance(sub, bytes):
                values = collect_option_ids(sub)
        if param_id in ("reasoning_effort", "effort"):
            return param_id, values
    return None

LEVELS = {"none", "auto", "minimal", "low", "medium", "high", "xhigh", "max"}

def variant_base(model_id):
    body = model_id[:-5] if model_id.endswith("-fast") else model_id
    if "-" not in body:
        return None
    base, level = body.rsplit("-", 1)
    if base and level in LEVELS:
        return base
    return None

data = unwrap_connect(data)

models = []
for number, value in fields(data):
    if number != 1 or not isinstance(value, bytes):
        continue
    model_id, display_name, aliases = "", "", []
    for field, sub in fields(value):
        if field == 1 and isinstance(sub, bytes):
            model_id = sub.decode("utf-8", "replace")
        elif field == 4 and isinstance(sub, bytes):
            display_name = sub.decode("utf-8", "replace")
        elif field == 3 and isinstance(sub, bytes) and not display_name:
            display_name = sub.decode("utf-8", "replace")
        elif field == 6 and isinstance(sub, bytes):
            aliases.append(sub.decode("utf-8", "replace"))
    if model_id:
        models.append({
            "model_id": model_id,
            "display_name": display_name,
            "aliases": aliases,
        })

if not models:
    sys.stderr.write("解析失败或目录为空(响应前 64 字节: %s)\n" % data[:64].hex())
    sys.exit(1)

effort_by_id = {}
try:
    available = unwrap_connect(open(sys.argv[2], "rb").read())
except OSError:
    available = b""
if available:
    try:
        available_models = list(fields(available))
    except ValueError:
        available_models = []
        sys.stderr.write("AvailableModels 解析失败，不标明思考档键\n")
    for number, value in available_models:
        if number != 2 or not isinstance(value, bytes):
            continue
        try:
            parts = list(fields(value))
            got = effort_param(value)
        except ValueError:
            continue
        model_id = next(
            (
                sub.decode()
                for field, sub in parts
                if field == 1 and isinstance(sub, bytes) and is_text(sub)
            ),
            "",
        )
        if model_id and got:
            effort_by_id[model_id] = {"key": got[0], "values": got[1]}

usable_ids = {model["model_id"] for model in models}
for model in models:
    base = model["model_id"] if model["model_id"] in effort_by_id else variant_base(model["model_id"])
    if base in effort_by_id:
        model["effort_base"] = base
        model["effort_key"] = effort_by_id[base]["key"]
        model["effort_values"] = effort_by_id[base]["values"]

if "--raw" in sys.argv[3:]:
    print(json.dumps(models, indent=2, ensure_ascii=False))
else:
    print(f"共 {len(models)} 个模型:")
    for model in models:
        line = f"  {model['model_id']}"
        if model["display_name"] and model["display_name"] != model["model_id"]:
            line += f"  ({model['display_name']})"
        if model["aliases"]:
            line += "  aliases: " + ", ".join(model["aliases"])
        print(line)
    if effort_by_id:
        print()
        print("思考档。变体 id 的档位由 models.json 决定并拼进上游 model id，白名单不配:")
        for model_id in sorted(effort_by_id):
            spec = effort_by_id[model_id]
            values = " | ".join(spec["values"])
            print(f"  {model_id}")
            print(f"    {spec['key']} = {values}")
            if any(variant_base(item) == model_id for item in usable_ids):
                entry = json.dumps({"id": model_id, "reasoning_levels": spec["values"]})
                print("    目录是变体 id，models.json 条目（force_effort 可选）:")
                print(f"    {entry}")
            elif model_id in usable_ids:
                print("    目录是裸 id，档位写入 parameters，可在白名单钉:")
                print(f'    "{model_id}:{spec["key"]}=<档>"')
            else:
                print("    GetUsableModels 未列出这个 id")
    else:
        print()
        print("未取到思考档参数名")
PY_EOF
    rm -f "$resp_file" "$avail_file"
    return $status
}

main() {
    local cred_file="${TARGET_FILE:-${AUTH_DIR}/cursor.json}"
    if [[ ! -f "$cred_file" ]]; then
        echo "未找到 Cursor 凭证: ${cred_file}" >&2
        exit 1
    fi
    local token
    token="$(refresh_token_if_needed "$cred_file")"
    if [[ -z "$token" ]]; then
        echo "❌ 无法获取有效 accessToken" >&2
        exit 1
    fi
    if [[ "$LIST_MODELS" == true ]]; then
        list_models "$token"
        return
    fi
    local sub
    sub="$(jq -r '.sub // "-"' "$cred_file")"
    query_usage "$token" "$sub" "$(basename "$cred_file")"
    # 默认附带模型目录
    list_models "$token"
}

main
