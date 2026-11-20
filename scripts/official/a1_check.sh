#!/bin/bash
# A1 自检:确认本机 Claude Code(订阅登录)发往自定义 BASE_URL 的 /v1/messages 带 Bearer Authorization。
# 只起本地 mock(a1_mock.py),不打真实上游;只输出存在性/布尔值与 UA 前缀,绝不输出任何头的值。
# 用法: scripts/official/a1_check.sh [port]   (默认 18299;a1_mock.py 与本脚本同目录)
set -euo pipefail
DIR="$(cd "$(dirname "$0")" && pwd)"
PORT="${1:-18299}"
LOG="$(mktemp /tmp/ccx_a1.XXXXXX)"
python3 "$DIR/a1_mock.py" "$PORT" "$LOG" >/dev/null 2>&1 &
MOCK_PID=$!
trap 'kill "$MOCK_PID" 2>/dev/null || true; wait "$MOCK_PID" 2>/dev/null || true; rm -f "$LOG"' EXIT
for _ in $(seq 1 25); do
    curl -s -o /dev/null "http://127.0.0.1:${PORT}/" && break
    sleep 0.2
done
: > "$LOG"

# 只用单条命令环境变量:清掉 token/key/model,跳过用户级 settings.json(其 env 会注入 token)
env -u ANTHROPIC_AUTH_TOKEN -u ANTHROPIC_API_KEY -u ANTHROPIC_MODEL \
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
ok = bool(msgs) and all(r["authorization_present"] and r["authorization_is_bearer"]
                        and not r["x_api_key_present"] for r in msgs)
print("A1:", "PASS(带 Bearer Authorization、无 x-api-key,可走 8223 透传)" if ok
      else "FAIL(没有带 Bearer 的 /v1/messages;检查 claude auth status 与环境变量)")
sys.exit(0 if ok else 1)
PY
