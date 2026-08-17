#!/usr/bin/env bash
# 查看 Antigravity 额度分组(不打印 token)
set -euo pipefail

DIR=$(cd "$(dirname "$0")" && pwd)
ROOT=$(cd "$DIR/../.." && pwd)
AUTH_DIR="$ROOT/.cache/antigravity"
UA="antigravity/cli/1.0.13 (aidev_client; os_type=darwin; arch=arm64)"

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
email=$(jq -r '.email // empty' "$cred")
project=$(jq -r '.project_id // empty' "$cred")
if [[ -z "$token" ]]; then
  echo "凭证无 access_token,重新登录" >&2
  exit 1
fi
if [[ -z "$project" ]]; then
  echo "凭证无 project_id,重新登录" >&2
  exit 1
fi

proxy="${PROXY_URL:-}"
if [[ -z "$proxy" && -f "$ROOT/config.yaml" ]]; then
  proxy=$(sed -n '/^server:/,/^[^[:space:]#]/s/^[[:space:]]*proxy_url:[[:space:]]*//p' "$ROOT/config.yaml" | head -1)
  proxy=${proxy%%#*}
  proxy=$(printf '%s' "$proxy" | tr -d '[:space:]' | tr -d "\"'")
fi

curl_opts=( -sS )
case "${proxy}" in
  "" ) ;;
  direct|none|DIRECT|NONE) curl_opts+=(--noproxy '*') ;;
  *) curl_opts+=(-x "$proxy") ;;
esac

post() {
  curl "${curl_opts[@]}" -o "$1" -w '%{http_code}' \
    -H "Authorization: Bearer ${token}" \
    -H "Accept: */*" \
    -H "Content-Type: application/json" \
    -H "User-Agent: ${UA}" \
    -d "$3" \
    "$2" || true
}

quota_body=$(jq -nc --arg p "$project" '{project:$p}')
tmp=$(mktemp)
tier=$(mktemp)
trap 'rm -f "$tmp" "$tier"' EXIT

got=""
for url in \
  "https://daily-cloudcode-pa.googleapis.com/v1internal:retrieveUserQuotaSummary" \
  "https://daily-cloudcode-pa.sandbox.googleapis.com/v1internal:retrieveUserQuotaSummary" \
  "https://cloudcode-pa.googleapis.com/v1internal:retrieveUserQuotaSummary"
do
  code=$(post "$tmp" "$url" "$quota_body")
  [[ "$code" == 2* ]] || continue
  got=1
  jq -e '(.groups // []) | length > 0' "$tmp" >/dev/null && break
done

if [[ -z "$got" ]]; then
  echo "retrieveUserQuotaSummary 失败(token 过期则重新登录)" >&2
  exit 1
fi

tier_url="https://daily-cloudcode-pa.googleapis.com/v1internal:loadCodeAssist"
tier_code=$(post "$tier" "$tier_url" '{"metadata":{"ideType":"ANTIGRAVITY"}}')
if [[ "$tier_code" != 2* ]]; then
  printf '%s\n' '{}' >"$tier"
fi

jq --arg email "$email" --arg project "$project" --slurpfile tier "$tier" '
  def trim:
    if . == null then null
    elif type == "string" then
      (gsub("^\\s+|\\s+$"; "")) | if . == "" then null else . end
    else .
    end;

  def parse_frac:
    if . == null then null
    elif type == "number" then .
    elif type == "string" then
      (try (gsub("^\\s+|\\s+$"; "") | tonumber) catch null)
    else null
    end;

  def plan_of:
    if . == "free-tier" then "free"
    elif . == "g1-pro-tier" then "pro"
    elif . == "g1-ultra-tier" then "ultra"
    elif . == "g1-ultra-lite-tier" then "ultra-lite"
    elif . == null then null
    else "unknown"
    end;

  def buckets_of:
    [
      (.buckets // [])[]
      | . as $b
      | ($b.remainingFraction // $b.remaining_fraction | parse_frac) as $frac
      | select($frac != null)
      | {
          id: (($b.bucketId // $b.bucket_id // $b.displayName // $b.display_name) | trim),
          label: (($b.displayName // $b.display_name // $b.bucketId // $b.bucket_id) | trim),
          window: (($b.window // null) | trim),
          remainingFraction: $frac,
          remainingPercent: (($frac * 10000 | floor) / 100),
          resetTime: (($b.resetTime // $b.reset_time // null) | trim)
        }
    ];

  ($tier[0] // {}) as $t
  | ($t.paidTier // $t.paid_tier) as $paid
  | ($t.currentTier // $t.current_tier) as $cur
  | (if ($paid.id // null) != null then $paid else $cur end) as $tier
  | {
      email: $email,
      project: $project,
      plan: (($tier.id // null) | plan_of),
      tierId: ($tier.id // null),
      tierName: ($tier.name // null),
      groups: [
        (.groups // [])[]
        | . as $g
        | (buckets_of) as $buckets
        | select(($buckets | length) > 0)
        | {
            label: (($g.displayName // $g.display_name) | trim),
            description: (($g.description // null) | trim),
            buckets: $buckets
          }
      ]
    }
' "$tmp"
