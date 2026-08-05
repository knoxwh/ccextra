#!/bin/bash
set -euo pipefail

SCRIPT_DIR="$(cd "$(dirname "$0")" && pwd)"

echo "=== Restarting ccextra ==="
"$SCRIPT_DIR/stop.sh"
"$SCRIPT_DIR/start.sh"