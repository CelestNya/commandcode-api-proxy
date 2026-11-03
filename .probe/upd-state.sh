#!/usr/bin/env bash
# 自动更新端到端验证的夹具（第二部分：把"上次检查的结果"指向本地 HTTP 服务）。
# 在 zip 打好之后运行：bash .probe/upd-state.sh 0.7.10
set -euo pipefail
cd "$(dirname "$0")/.."
ROOT=.probe/upd
NEW=${1:-0.7.10}
ZIP="$ROOT/commandcode-proxy-v$NEW-windows-x86_64.zip"
[ -f "$ZIP" ] || { echo "缺少 $ZIP（先用 zip-package.ps1 打包）" >&2; exit 1; }
SIZE=$(stat -c %s "$ZIP")
NOW=$(date +%s)
cat > "$ROOT/install_a/service/.ccproxy-update.json" <<JSON
{
  "checked_at": $NOW,
  "latest": "$NEW",
  "url": "http://127.0.0.1:8899/commandcode-proxy-v$NEW-windows-x86_64.zip",
  "size": $SIZE,
  "error": ""
}
JSON
echo "状态已指向 v$NEW（$SIZE 字节）"
