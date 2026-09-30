#!/usr/bin/env bash
set -euo pipefail

# Cursor SDK 模型目录查询脚本(cursor_models 白名单配置依据)
# 数据源: sidecar/cursor/list-models.mjs → SDK Cursor.models.list
# 认证: cursor_auth_dir/api_key.txt 的 User API Key(PKCE 凭证不适用,见 README)
# 安全: 只读取现有配置;不打印 apiKey

BASE_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
CONFIG_FILE="${BASE_DIR}/config.yaml"
AUTH_DIR="${BASE_DIR}/.cache/cursor"
SIDECAR_DIR="${BASE_DIR}/sidecar/cursor"

usage() {
    echo "用法: $0 [选项]"
    echo "选项:"
    echo "  -d, --dir <dir>      凭证目录 (默认: ${AUTH_DIR})"
    echo "  -r, --raw            输出原始 JSON"
    echo "  -h, --help           显示帮助信息"
    exit 1
}

# 从 config.yaml 读取 cursor_auth_dir 作为默认值(可选;命令行 -d 覆盖)
# 相对路径钉配置文件目录,绝对路径原样使用
if [[ -f "$CONFIG_FILE" ]]; then
    RAW_DIR="$(awk -F: '
        index($0, "cursor_auth_dir:") == 1 {
            line = $0
            sub(/^cursor_auth_dir:[[:space:]]*/, "", line)
            gsub(/^"|"$/, "", line)
            print line
            exit
        }' "$CONFIG_FILE")"
    if [[ "$RAW_DIR" == "/"* ]]; then
        AUTH_DIR="$RAW_DIR"
    elif [[ -n "$RAW_DIR" && "$RAW_DIR" != "~"* ]]; then
        # 目录尚未创建时 cd 失败,保留默认值而非清空
        RESOLVED="$(cd "$(dirname "$CONFIG_FILE")" && cd "$RAW_DIR" 2>/dev/null && pwd)" || true
        [[ -n "$RESOLVED" ]] && AUTH_DIR="$RESOLVED"
    fi
fi

RAW_OUTPUT=false

while [[ $# -gt 0 ]]; do
    case "$1" in
        -d|--dir)
            AUTH_DIR="$2"
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

if ! command -v node >/dev/null 2>&1; then
    echo "错误: 需要安装 node (>=24)" >&2
    exit 1
fi

if ! command -v python3 >/dev/null 2>&1; then
    echo "错误: 需要安装 python3" >&2
    exit 1
fi

if [[ ! -d "${SIDECAR_DIR}/node_modules/@cursor/sdk" ]]; then
    echo "错误: 缺少 SDK 依赖,请在 ${SIDECAR_DIR} 运行 npm install" >&2
    exit 1
fi

if [[ ! -f "${AUTH_DIR}/api_key.txt" ]]; then
    echo "未找到 User API Key: ${AUTH_DIR}/api_key.txt" >&2
    echo "在 cursor.com/settings → API Keys 生成后写入该文件(单行裸 key)" >&2
    exit 2
fi

# key 只经环境变量传给子进程,不进命令行参数
MODELS_JSON="$(CCEXTRA_CURSOR_API_KEY="$(cat "${AUTH_DIR}/api_key.txt")" \
    node "${SIDECAR_DIR}/list-models.mjs")" || exit 1

if [[ "$RAW_OUTPUT" == true ]]; then
    echo "$MODELS_JSON"
    exit 0
fi

python3 -c '
import sys, json

data = json.loads(sys.argv[1])
models = data.get("models") or []
if not models:
    print("⚠️ 未解析到模型(响应可能为空或格式变化)")
    sys.exit(0)

rows = []
for m in models:
    mid = m.get("id") or "unknown"
    name = m.get("displayName") or ""
    parts = []
    for p in m.get("parameters") or []:
        pid = p.get("id")
        values = "|".join(p.get("values") or [])
        parts.append(f"{pid}: {values}")
    rows.append((mid, name, ", ".join(parts) or "-"))
rows.sort()

w_model = max(len("MODEL"), max(len(r[0]) for r in rows))
w_name = max(len("NAME"), max(len(r[1]) for r in rows))

print(f"SDK 模型目录 ({len(rows)} 个,白名单匹配目标):")
print()
hdr_model = "MODEL".ljust(w_model)
hdr_name = "NAME".ljust(w_name)
print(f"{hdr_model}  {hdr_name}  PARAMETERS")
sep_model = "-" * w_model
sep_name = "-" * w_name
print(f"{sep_model}  {sep_name}  ---------")
for mid, name, params in rows:
    print(f"{mid:<{w_model}}  {name:<{w_name}}  {params}")
' "$MODELS_JSON"
