#!/usr/bin/env bash
# 列出 Antigravity 可用模型(不打印 token)
set -euo pipefail

DIR=$(cd "$(dirname "$0")" && pwd)
ROOT=$(cd "$DIR/../.." && pwd)
AUTH_DIR="$ROOT/.cache/antigravity"
UA="antigravity/hub/2.8.1 darwin/arm64"
PROD="https://cloudcode-pa.googleapis.com/v1internal:fetchAvailableModels"
DAILY="https://daily-cloudcode-pa.googleapis.com/v1internal:fetchAvailableModels"

if ! command -v curl >/dev/null || ! command -v jq >/dev/null; then
  echo "需要 curl 和 jq" >&2
  exit 1
fi

cred=$(ls -1 "$AUTH_DIR"/antigravity-*.json 2>/dev/null | head -1 || true)
if [[ -z "${cred:-}" ]]; then
  echo "无凭证: $AUTH_DIR" >&2
  echo "先: $DIR/login.sh" >&2
  exit 1
fi

token=$(jq -r '.access_token // empty' "$cred")
project=$(jq -r '.project_id // empty' "$cred")
if [[ -z "$token" ]]; then
  echo "凭证无 access_token,重新登录" >&2
  exit 1
fi

proxy="${PROXY_URL:-}"
if [[ -z "$proxy" && -f "$ROOT/config.yaml" ]]; then
  proxy=$(sed -n '/^server:/,/^[^[:space:]#]/s/^[[:space:]]*proxy_url:[[:space:]]*//p' "$ROOT/config.yaml" | head -1)
  proxy=${proxy%%#*}
  proxy=$(printf '%s' "$proxy" | tr -d '[:space:]' | tr -d "\"'")
fi

curl_opts=( -sS --fail-with-body )
case "${proxy}" in
  "" ) ;;
  direct|none|DIRECT|NONE) curl_opts+=(--noproxy '*') ;;
  *) curl_opts+=(-x "$proxy") ;;
esac

body='{}'
if [[ -n "$project" ]]; then
  body=$(jq -nc --arg p "$project" '{project:$p}')
fi

fetch() {
  curl "${curl_opts[@]}" \
    -H "Authorization: Bearer ${token}" \
    -H "Content-Type: application/json" \
    -H "User-Agent: ${UA}" \
    -d "$body" \
    "$1"
}

raw=$(fetch "$PROD" || fetch "$DAILY")
echo "$raw" | jq '
  (.models // {})
  | to_entries
  | map({
      id: .key,
      displayName: (.value.displayName // .key),
      maxTokens: .value.maxTokens,
      maxOutputTokens: .value.maxOutputTokens
    })
'
