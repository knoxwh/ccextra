#!/usr/bin/env bash
# 浏览器登录 Antigravity,凭证写仓库 .cache/antigravity
set -euo pipefail

DIR=$(cd "$(dirname "$0")" && pwd)
ROOT=$(cd "$DIR/../.." && pwd)
CONFIG="$ROOT/config.yaml"
AUTH_DIR="$ROOT/.cache/antigravity"

if [[ ! -f "$CONFIG" ]]; then
  echo "无配置: $CONFIG" >&2
  exit 1
fi

mkdir -p "$AUTH_DIR"
chmod 700 "$AUTH_DIR" 2>/dev/null || true

args=(--config "$CONFIG" antigravity-login --auth-dir "$AUTH_DIR")
if [[ "${1:-}" == "--no-browser" ]]; then
  args+=(--no-browser)
fi

if [[ -x "${CCEXTRA:-}" ]]; then
  exec "$CCEXTRA" "${args[@]}"
fi

cd "$ROOT"
if [[ -x "$ROOT/target/debug/ccextra" ]]; then
  exec "$ROOT/target/debug/ccextra" "${args[@]}"
fi
if [[ -x "$ROOT/target/release/ccextra" ]]; then
  exec "$ROOT/target/release/ccextra" "${args[@]}"
fi

exec cargo run --offline --bin ccextra -- "${args[@]}"
