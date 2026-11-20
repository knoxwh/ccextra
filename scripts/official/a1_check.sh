#!/bin/bash
# A1 自检:确认本机 Claude Code(订阅登录)发往自定义 BASE_URL 的 /v1/messages 带 Bearer Authorization。
# 只起本地 mock(a1_mock.py),不打真实上游;只输出存在性/布尔值与 UA 前缀,绝不输出任何头的值。
# 用法: scripts/official/a1_check.sh [port]   (默认 18299;a1_mock.py 与本脚本同目录)
set -euo pipefail
DIR="$(cd "$(dirname "$0")" && pwd)"
PORT="${1:-18299}"

# 启动 mock 前预检端口占用(含 0.0.0.0 通配监听):macOS 下 HTTPServer 的 SO_REUSEADDR 会允许叠加绑定,
# 而关掉 allow_reuse_address 会因 HTTP/1.0 TIME_WAIT 导致连续执行报错;预检 LISTEN 与建连探测两全。
OCCUPIED_PIDS="$(lsof -tiTCP:"$PORT" -sTCP:LISTEN 2>/dev/null || true)"
if [ -n "$OCCUPIED_PIDS" ]; then
    echo "错误: 端口 ${PORT} 已被进程占用 (PID: $(printf '%s' "$OCCUPIED_PIDS" | tr '\n' ' ' | sed 's/ $//'))，请换端口或先释放" >&2
    exit 1
fi
if python3 -c 'import socket,sys; s=socket.socket(); s.settimeout(0.2); rc=s.connect_ex(("127.0.0.1",int(sys.argv[1]))); s.close(); sys.exit(0 if rc==0 else 1)' "$PORT" 2>/dev/null; then
    echo "错误: 127.0.0.1:${PORT} 已有监听进程(可能属其他用户)，请换端口或先释放" >&2
    exit 1
fi

LOG="$(mktemp /tmp/ccx_a1.XXXXXX)"
MOCK_ERR="$(mktemp /tmp/ccx_a1_err.XXXXXX)"
READY_TOKEN="a1-ready-$$-${RANDOM}"
A1_READY_TOKEN="$READY_TOKEN" python3 "$DIR/a1_mock.py" "$PORT" "$LOG" >/dev/null 2>"$MOCK_ERR" &
MOCK_PID=$!
trap 'kill "$MOCK_PID" 2>/dev/null || true; wait "$MOCK_PID" 2>/dev/null || true; rm -f "$LOG" "$MOCK_ERR"' EXIT
READY=no
for _ in $(seq 1 25); do
    if ! kill -0 "$MOCK_PID" 2>/dev/null; then
        echo "错误: 本地 mock 启动失败(端口 ${PORT} 可能已被占用):" >&2
        cat "$MOCK_ERR" >&2
        exit 1
    fi
    if [ "$(curl -sf "http://127.0.0.1:${PORT}/__a1_ready" 2>/dev/null || true)" = "$READY_TOKEN" ]; then
        READY=yes
        break
    fi
    sleep 0.2
done
if [ "$READY" != yes ]; then
    echo "错误: 等待本地 mock(127.0.0.1:${PORT})就绪超时,中止检查以避免请求发往未知进程" >&2
    cat "$MOCK_ERR" >&2
    exit 1
fi
: > "$LOG"

# 只用单条命令环境变量:清掉 token/key/model,跳过用户级 settings.json(其 env 会注入 token)
env -u ANTHROPIC_AUTH_TOKEN -u ANTHROPIC_API_KEY -u ANTHROPIC_MODEL -u ANTHROPIC_DEFAULT_HAIKU_MODEL \
    ANTHROPIC_BASE_URL="http://127.0.0.1:${PORT}" TZ=America/New_York \
    claude --setting-sources project,local --no-session-persistence --model haiku \
    -p "reply with ok" >/dev/null 2>&1 || echo "注意: claude 退出码非 0(未登录时会这样,先 claude auth login)"

python3 - "$LOG" <<'PY'
import json, sys
rows = [json.loads(l) for l in open(sys.argv[1]) if l.strip()]
msgs = [r for r in rows if r["method"] == "POST" and r["path"] == "/v1/messages"]
print(f"mock 收到请求 {len(rows)} 个,其中 POST /v1/messages {len(msgs)} 个")
for r in msgs[:3]:
    print("  authorization_present=%s is_bearer=%s looks_oauth=%s x_api_key_present=%s "
          "anthropic_beta_present=%s beta_has_oauth=%s ua_prefix=%s" % (
          r["authorization_present"], r["authorization_is_bearer"], r["authorization_looks_oauth"],
          r["x_api_key_present"], r["anthropic_beta_present"], r["anthropic_beta_has_oauth"],
          r["user_agent_prefix"]))
bearer_only = bool(msgs) and all(
    r["authorization_present"] and r["authorization_is_bearer"] and not r["x_api_key_present"]
    for r in msgs
)
ok = bearer_only and all(r["authorization_looks_oauth"] for r in msgs)
if ok:
    print("A1: PASS(带 claude.ai OAuth Bearer Authorization、无 x-api-key,可走 8223 透传)")
elif bearer_only:
    print("A1: FAIL(收到了 Bearer 但非 sk-ant-oat 前缀,可能命中了项目/本地 .claude/settings*.json 的 ANTHROPIC_AUTH_TOKEN 或 apiKeyHelper)")
else:
    print("A1: FAIL(没有带 Bearer 的 /v1/messages;检查 claude auth status 与环境变量)")
sys.exit(0 if ok else 1)
PY
