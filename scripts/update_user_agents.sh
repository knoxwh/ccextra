#!/usr/bin/env bash
# zsh/sh 直接执行时转投 bash
if [ -z "${BASH_VERSION:-}" ]; then
    exec bash "$0" "$@"
fi
set -euo pipefail

# 检查并更新 config.yaml 中的 user_agents 与 cursor_client_version

BASE_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
DEFAULT_CONFIG="${BASE_DIR}/config.yaml"
CONFIG_FILE="${DEFAULT_CONFIG}"
CHECK_ONLY=false
DRY_RUN=false

usage() {
    cat <<EOF
用法: $0 [选项]

检查上游最新版本并更新 config.yaml 中的客户端标头。

数据源:
  - claude_cli:            npm @anthropic-ai/claude-code
  - codex_tui:             npm @openai/codex
  - grok_version:          npm @xai-official/grok
  - antigravity:           Google Antigravity Hub updater manifest
  - cursor_client_version: Cursor upstream build version (api2.cursor.sh)

选项:
  -c, --config <path>    指定配置文件路径 (默认: config.yaml)
  --check                仅检查并输出差异，不修改文件
  -n, --dry-run          模拟执行，显示更新结果但不写回
  -h, --help             显示帮助信息
EOF
    exit 0
}

while [[ $# -gt 0 ]]; do
    case "$1" in
        -c|--config)
            CONFIG_FILE="$2"
            shift 2
            ;;
        --check)
            CHECK_ONLY=true
            shift
            ;;
        -n|--dry-run)
            DRY_RUN=true
            shift
            ;;
        -h|--help)
            usage
            ;;
        *)
            echo "未知参数: $1" >&2
            exit 1
            ;;
    esac
done

# 工具依赖检查
for cmd in curl jq python3; do
    if ! command -v "$cmd" >/dev/null 2>&1; then
        echo "错误: 需要安装 $cmd" >&2
        exit 1
    fi
done

if [[ ! -f "$CONFIG_FILE" ]]; then
    echo "错误: 配置文件不存在: $CONFIG_FILE" >&2
    exit 1
fi

# 从 config.yaml 读取 proxy_url (如果有)
read_proxy() {
    local proxy
    proxy="$(sed -n 's/^[[:space:]]*proxy_url:[[:space:]]*["'\'' ]\{0,1\}\([^"'\'' ]*\)["'\'' ]\{0,1\}[[:space:]]*$/\1/p' "$CONFIG_FILE" | head -n 1 || true)"
    if [[ -n "$proxy" && "$proxy" != "direct" && "$proxy" != '""' && "$proxy" != "''" ]]; then
        echo "$proxy"
    elif [[ -n "${https_proxy:-}" ]]; then
        echo "$https_proxy"
    elif [[ -n "${http_proxy:-}" ]]; then
        echo "$http_proxy"
    elif [[ -n "${ALL_PROXY:-}" ]]; then
        echo "$ALL_PROXY"
    fi
}

PROXY_URL="$(read_proxy)"
CURL_ARGS=( -fsS --max-time 10 )
if [[ -n "$PROXY_URL" ]]; then
    CURL_ARGS+=( -x "$PROXY_URL" )
fi

http_get() {
    local url="$1"
    curl "${CURL_ARGS[@]}" "$url" 2>/dev/null || curl -fsS --max-time 10 "$url" 2>/dev/null || true
}

# 1. 获取最新版本
echo "🔍 检查上游最新版本..."

# 1.1 claude_cli
LATEST_CLAUDE_VER="$(http_get "https://registry.npmjs.org/@anthropic-ai/claude-code/latest" | jq -r '.version // empty' 2>/dev/null || true)"
if [[ -z "$LATEST_CLAUDE_VER" ]]; then
    LATEST_CLAUDE_VER="$(npm view @anthropic-ai/claude-code version 2>/dev/null || true)"
fi

# 1.2 codex_tui
LATEST_CODEX_VER="$(http_get "https://registry.npmjs.org/@openai/codex/latest" | jq -r '.version // empty' 2>/dev/null || true)"
if [[ -z "$LATEST_CODEX_VER" ]]; then
    LATEST_CODEX_VER="$(npm view @openai/codex version 2>/dev/null || true)"
fi

# 1.3 grok_version
LATEST_GROK_VER="$(http_get "https://registry.npmjs.org/@xai-official/grok/latest" | jq -r '.version // empty' 2>/dev/null || true)"
if [[ -z "$LATEST_GROK_VER" ]]; then
    LATEST_GROK_VER="$(npm view @xai-official/grok version 2>/dev/null || true)"
fi

# 1.4 antigravity
ANTIGRAVITY_MANIFEST="$(http_get "https://antigravity-hub-auto-updater-974169037036.us-central1.run.app/manifest/latest-arm64-mac.yml")"
LATEST_ANTIGRAVITY_VER="$(echo "$ANTIGRAVITY_MANIFEST" | sed -n 's/^version:[[:space:]]*//p' | head -n 1 | tr -d '\r\n ')"

# 1.5 cursor_client_version
parse_cursor_version() {
    local body="$1"
    if [[ "$body" =~ ([0-9]{4})([0-9]{2})([0-9]{2})-[0-9]{6}-([a-f0-9]{7}) ]]; then
        echo "cli-${BASH_REMATCH[1]}.${BASH_REMATCH[2]}.${BASH_REMATCH[3]}-${BASH_REMATCH[4]}"
    fi
}
LATEST_CURSOR_VERSION="$(parse_cursor_version "$(http_get "https://api2.cursor.sh")")"
if [[ -z "$LATEST_CURSOR_VERSION" ]]; then
    LATEST_CURSOR_VERSION="$(parse_cursor_version "$(http_get "https://api.cursor.com")")"
fi

# 2. 读取配置当前值
read_yaml_val() {
    local key="$1"
    python3 -c '
import sys, re
key = sys.argv[1]
with open(sys.argv[2], "r", encoding="utf-8") as f:
    for line in f:
        m = re.match(r"^[ \t]*" + re.escape(key) + r":[ \t]*[\"'\'' ]{0,1}([^\"'\''#\r\n]+)[\"'\'' ]{0,1}", line)
        if m:
            print(m.group(1).strip())
            break
' "$key" "$CONFIG_FILE" 2>/dev/null || true
}

CURRENT_CLAUDE="$(read_yaml_val "claude_cli")"
CURRENT_CODEX="$(read_yaml_val "codex_tui")"
CURRENT_GROK="$(read_yaml_val "grok_version")"
CURRENT_ANTIGRAVITY="$(read_yaml_val "antigravity")"
CURRENT_CURSOR="$(read_yaml_val "cursor_client_version")"

# 组装新目标值
NEW_CLAUDE=""
if [[ -n "$LATEST_CLAUDE_VER" ]]; then
    NEW_CLAUDE="claude-cli/${LATEST_CLAUDE_VER}"
fi

NEW_CODEX=""
if [[ -n "$LATEST_CODEX_VER" ]]; then
    if [[ "$CURRENT_CODEX" =~ ^codex_cli_rs/[^[:space:]]+(.*)$ ]]; then
        NEW_CODEX="codex_cli_rs/${LATEST_CODEX_VER}${BASH_REMATCH[1]}"
    else
        NEW_CODEX="codex_cli_rs/${LATEST_CODEX_VER} (Mac OS 27.0.1; arm64)"
    fi
fi

NEW_GROK=""
if [[ -n "$LATEST_GROK_VER" ]]; then
    NEW_GROK="${LATEST_GROK_VER}"
fi

NEW_ANTIGRAVITY=""
if [[ -n "$LATEST_ANTIGRAVITY_VER" ]]; then
    if [[ "$CURRENT_ANTIGRAVITY" =~ ^antigravity/hub/[^[:space:]]+(.*)$ ]]; then
        NEW_ANTIGRAVITY="antigravity/hub/${LATEST_ANTIGRAVITY_VER}${BASH_REMATCH[1]}"
    else
        NEW_ANTIGRAVITY="antigravity/hub/${LATEST_ANTIGRAVITY_VER} darwin/arm64"
    fi
fi

NEW_CURSOR=""
if [[ -n "$LATEST_CURSOR_VERSION" ]]; then
    NEW_CURSOR="${LATEST_CURSOR_VERSION}"
fi

# 3. 对比展示
CHANGES=0

report_item() {
    local label="$1"
    local cur="$2"
    local new="$3"
    if [[ -z "$new" ]]; then
        printf "  %-24s: %s (无法获取上游最新值)\n" "$label" "${cur:-<未配置>}"
    elif [[ -z "$cur" ]]; then
        printf "  %-24s: <未配置> (配置文件中未设置该字段，跳过)\n" "$label"
    elif [[ "$cur" == "$new" ]]; then
        printf "  %-24s: %s (最新)\n" "$label" "$cur"
    else
        CHANGES=$((CHANGES + 1))
        printf "  %-24s: %s -> %s\n" "$label" "$cur" "$new"
    fi
}

echo "📋 检查结果:"
report_item "claude_cli" "$CURRENT_CLAUDE" "$NEW_CLAUDE"
report_item "codex_tui" "$CURRENT_CODEX" "$NEW_CODEX"
report_item "grok_version" "$CURRENT_GROK" "$NEW_GROK"
report_item "antigravity" "$CURRENT_ANTIGRAVITY" "$NEW_ANTIGRAVITY"
report_item "cursor_client_version" "$CURRENT_CURSOR" "$NEW_CURSOR"

if [[ $CHANGES -eq 0 ]]; then
    echo "✅ 全部标头已是最新，无须更新。"
    exit 0
fi

if [[ "$CHECK_ONLY" == true ]]; then
    echo "ℹ️ --check 模式: 发现 $CHANGES 项更新，未写入文件。"
    exit 0
fi

if [[ "$DRY_RUN" == true ]]; then
    echo "ℹ️ --dry-run 模式: 模拟更新完成，未写入文件。"
    exit 0
fi

# 4. 执行替换回写 (使用 python 精确替换标头值并保留注释与排版，临时文件原子写入)
python3 -c '
import sys, re, os, tempfile

config_path = sys.argv[1]
updates = {
    "claude_cli": sys.argv[2],
    "codex_tui": sys.argv[3],
    "grok_version": sys.argv[4],
    "antigravity": sys.argv[5],
    "cursor_client_version": sys.argv[6],
}

with open(config_path, "r", encoding="utf-8") as f:
    content = f.read()

for key, new_val in updates.items():
    if not new_val:
        continue
    # 匹配 key: "..." 或 key: '\''...'\'' 或 key: val，保留 suffix（含空白与行尾注释）
    pattern = re.compile(
        r"^([ \t]*" + re.escape(key) + r":[ \t]*)(?:\"[^\r\n\"]*\"|'\''[^'\''\r\n]*'\''|[^#\r\n]+?)([ \t]*(?:#.*)?)$",
        re.MULTILINE
    )
    def repl(m):
        prefix = m.group(1)
        suffix = m.group(2)
        sep = " " if suffix and not suffix.startswith(" ") else ""
        return f"{prefix}\"{new_val}\"{sep}{suffix}"
    content = pattern.sub(repl, content)

dir_name = os.path.dirname(os.path.abspath(config_path))
fd, tmp_path = tempfile.mkstemp(dir=dir_name, prefix=".tmp_cfg_")
try:
    with os.fdopen(fd, "w", encoding="utf-8") as f:
        f.write(content)
    os.replace(tmp_path, config_path)
except Exception:
    if os.path.exists(tmp_path):
        os.remove(tmp_path)
    raise
' "$CONFIG_FILE" "$NEW_CLAUDE" "$NEW_CODEX" "$NEW_GROK" "$NEW_ANTIGRAVITY" "$NEW_CURSOR"

echo "✨ 已更新 $CHANGES 项到 $CONFIG_FILE"
