#!/usr/bin/env bash
set -euo pipefail

# Cursor SDK sidecar 端到端验证脚本
# 前置: ccextra 已运行且配置 cursor_auth_dir,api_key.txt 已写入 User API Key
# (cursor.com/settings → API Keys 生成;PKCE 登录凭证不适用,见 README)。
# 入口认证: CCEXTRA_API_KEY 环境变量;未设且 secret_key 已哈希时提示输入明文。
# 依次验证: /health、/v1/models 含 cursor provider、文本终态、
#           工具 tool_use、tool_result 下一轮、sidecar kill 后冷续接。
# 安全: 只读取现有配置;不打印 apiKey、token 或对话内容。

BASE_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
CONFIG_FILE="${BASE_DIR}/config.yaml"
BASE_URL="${CCEXTRA_BASE_URL:-http://127.0.0.1:8222}"
AUTH_DIR="${BASE_DIR}/.cache/cursor"
MODEL="${CCEXTRA_CURSOR_MODEL:-auto}"
RUN_TAG="$$-$(date +%s)"

# 从 config.yaml 读取 cursor_auth_dir(可选,相对路径钉配置文件目录)
if [[ -f "$CONFIG_FILE" ]]; then
    RAW_DIR="$(awk -F: '
        index($0, "cursor_auth_dir:") == 1 {
            line = $0
            sub(/^cursor_auth_dir:[[:space:]]*/, "", line)
            gsub(/^"|"$/, "", line)
            print line
            exit
        }' "$CONFIG_FILE")"
    if [[ -n "$RAW_DIR" && "$RAW_DIR" != "~"* && "$RAW_DIR" != "/"* ]]; then
        AUTH_DIR="$(cd "$(dirname "$CONFIG_FILE")" && cd "$RAW_DIR" 2>/dev/null && pwd)" || true
    fi
fi

fail() { echo "FAIL: $1" >&2; exit 1; }
pass() { echo "PASS: $1"; }

# 入口认证:优先 CCEXTRA_API_KEY 环境变量;否则读 config.yaml secret_key
# (仅作请求头,不回显)。config 中已是 bcrypt 哈希时交互式询问明文。
API_KEY="${CCEXTRA_API_KEY:-}"
if [[ -z "$API_KEY" && -f "$CONFIG_FILE" ]]; then
    API_KEY="$(awk -F: '
        index($0, "secret_key:") == 1 {
            line = $0
            sub(/^secret_key:[[:space:]]*/, "", line)
            gsub(/^"|"$/, "", line)
            print line
            exit
        }' "$CONFIG_FILE")"
fi
# bcrypt 哈希无法还原明文;改为提示输入(不回显,无 tty 时给出明确指引)
# 提示走 printf 到 stderr:read -p 的提示同样写 stderr,会被下面的 2>/dev/null 吞掉
if [[ "$API_KEY" =~ ^\$2[aby]\$ ]]; then
    printf "config.yaml 的 secret_key 已是 bcrypt 哈希,请输入明文 API key: " >&2
    read -rs API_KEY 2>/dev/null </dev/tty || API_KEY=""
    [[ -n "$API_KEY" ]] && echo >&2
    [[ -n "$API_KEY" ]] || fail "未获得明文 API key;请设 CCEXTRA_API_KEY 环境变量后重跑"
fi
AUTH_ARGS=()
if [[ -n "$API_KEY" ]]; then
    AUTH_ARGS=(-H "x-api-key: ${API_KEY}")
fi

# 前置检查:凭证存在(不读取内容)
if [[ ! -f "$AUTH_DIR/api_key.txt" ]]; then
    echo "SKIP: 未找到 Cursor 凭证 $AUTH_DIR/api_key.txt" >&2
    echo "      在 cursor.com/settings → API Keys 生成 User API Key 写入该目录 api_key.txt," >&2
    echo "      并在 config.yaml 配置 cursor_auth_dir(格式见 config.example.yaml)" >&2
    exit 2
fi

# 1. 服务健康
curl -sf "$BASE_URL/health" >/dev/null || fail "ccextra 未运行于 $BASE_URL"
pass "ccextra /health 正常"

# 2. cursor provider 已发布
MODELS_JSON="$(curl -sf ${AUTH_ARGS[@]+"${AUTH_ARGS[@]}"} "$BASE_URL/v1/models")" || \
    fail "/v1/models 请求失败(若配置了 secret_key 请设 CCEXTRA_API_KEY)"
echo "$MODELS_JSON" | grep -q '"cursor"' || fail "/v1/models 不含 cursor provider"
pass "/v1/models 含 cursor provider"

# 3. 文本终态(流式);首消息嵌 RUN_TAG,避免与历史残留会话撞哈希
#    带 system 字段:覆盖 systemPrompt 正文前缀路径(账号无 SDK systemPrompt 权限)
STREAM="$(curl -sf -X POST "$BASE_URL/v1/messages" \
    ${AUTH_ARGS[@]+"${AUTH_ARGS[@]}"} \
    -H 'content-type: application/json' \
    -H "x-claude-code-session-id: e2e-cursor-text-$RUN_TAG" \
    -d '{"model":"'"$MODEL"'","max_tokens":256,"stream":true,
         "system":"You are a helpful assistant.",
         "messages":[{"role":"user","content":"Reply with exactly: ok (run '"$RUN_TAG"')"}]}')" \
    || fail "文本流式请求失败"
echo "$STREAM" | grep -q 'message_stop' || fail "文本流未到 message_stop 终态"
pass "文本流式到达 message_stop 终态"

# 4. 工具调用;FIRST_MSG 供 4/5/6 共享同一会话前缀
FIRST_MSG="What is the weather in Tokyo? You must call the get_weather tool now; do not answer from memory. (run $RUN_TAG)"
TOOL_STREAM="$(curl -sf -X POST "$BASE_URL/v1/messages" \
    ${AUTH_ARGS[@]+"${AUTH_ARGS[@]}"} \
    -H 'content-type: application/json' \
    -H "x-claude-code-session-id: e2e-cursor-tool-$RUN_TAG" \
    -d '{"model":"'"$MODEL"'","max_tokens":512,"stream":true,
         "tools":[{"name":"get_weather","description":"Get current weather for a city",
                   "input_schema":{"type":"object","properties":{"city":{"type":"string"}},"required":["city"]}}],
         "messages":[{"role":"user","content":"'"$FIRST_MSG"'"}]}')" \
    || fail "工具流式请求失败"
echo "$TOOL_STREAM" | grep -q 'tool_use' || fail "未收到 tool_use 块"
pass "工具调用产生 tool_use 块"

# 提取真实 tool_use id(sidecar 按已 park 的 id 匹配 tool_result)
TOOL_ID="$(echo "$TOOL_STREAM" | grep -o '"type":"tool_use","id":"[^"]*"' | head -1 | sed 's/.*"id":"//;s/"$//')"
[[ -n "$TOOL_ID" ]] || fail "未能从工具流提取 tool_use id"

# 5. tool_result 下一轮(会话复用)
TOOL_RESULT_STREAM="$(curl -sf -X POST "$BASE_URL/v1/messages" \
    ${AUTH_ARGS[@]+"${AUTH_ARGS[@]}"} \
    -H 'content-type: application/json' \
    -H "x-claude-code-session-id: e2e-cursor-tool-$RUN_TAG" \
    -d '{"model":"'"$MODEL"'","max_tokens":256,"stream":true,
         "messages":[{"role":"user","content":"'"$FIRST_MSG"'"},
                     {"role":"assistant","content":[{"type":"tool_use","id":"'"$TOOL_ID"'","name":"get_weather","input":{"city":"Tokyo"}}]},
                     {"role":"user","content":[{"type":"tool_result","tool_use_id":"'"$TOOL_ID"'","content":"Sunny, 22C"}]}]}')" \
    || fail "tool_result 下一轮请求失败"
echo "$TOOL_RESULT_STREAM" | grep -q 'message_stop' || fail "tool_result 下一轮未到终态"
pass "tool_result 下一轮复用会话并到达终态"

# 6. sidecar kill 后冷续接(journal 恢复)
SIDECAR_PID="$(pgrep -f 'node.*main.mjs' | head -1 || true)"
if [[ -z "$SIDECAR_PID" ]]; then
    echo "WARN: 未找到 sidecar 进程,跳过冷续接检查" >&2
else
    kill "$SIDECAR_PID" 2>/dev/null || true
    # monitor 巡检 5s + 退避重启,轮询等待 sidecar 就绪后发冷续接请求
    RECOVERY_STREAM=""
    for _ in $(seq 1 15); do
        sleep 2
        RECOVERY_STREAM="$(curl -sf -X POST "$BASE_URL/v1/messages" \
            ${AUTH_ARGS[@]+"${AUTH_ARGS[@]}"} \
            -H 'content-type: application/json' \
            -H "x-claude-code-session-id: e2e-cursor-tool-$RUN_TAG" \
            -d '{"model":"'"$MODEL"'","max_tokens":256,"stream":true,
                 "messages":[{"role":"user","content":"'"$FIRST_MSG"'"},
                             {"role":"assistant","content":[{"type":"tool_use","id":"'"$TOOL_ID"'","name":"get_weather","input":{"city":"Tokyo"}}]},
                             {"role":"user","content":[{"type":"tool_result","tool_use_id":"'"$TOOL_ID"'","content":"Sunny, 22C"}]},
                             {"role":"user","content":"Reply with exactly: recovered"}]}')" \
            && break
        RECOVERY_STREAM=""
    done
    [[ -n "$RECOVERY_STREAM" ]] || fail "sidecar 重启后请求失败(30 秒内未就绪)"
    echo "$RECOVERY_STREAM" | grep -q 'message_stop' || fail "冷续接未到终态"
    pass "sidecar kill 后自动重启并完成冷续接"
fi

echo "=== Cursor sidecar E2E 全部通过 ==="
