#!/usr/bin/env bash
# 自动更新端到端验证的夹具（第一部分：两套安装 + 包内容）。
#
# 全部落在 .probe/upd/ 下，端口 8891、命名空间 updtest，不碰生产 8787，
# 也不碰 D:/Windows/Desktop/CCProxy-Release。
#
# 打包交给 packaging/windows/zip-package.ps1（与 CI 同一条路径），由调用方用
# PowerShell 执行；这一步只准备目录与文件。
set -euo pipefail
cd "$(dirname "$0")/.."          # 仓库根
ROOT=.probe/upd
TARGET=D:/Dev/cargo-target/release
NEW=${1:-0.7.10}                  # "发布出来"的版本（比当前新）
OLD=$(grep -m1 '^version' Cargo.toml | sed 's/.*"\(.*\)".*/\1/')

rm -rf "$ROOT/install_a" "$ROOT/pkg" "$ROOT/CCProxy-v$NEW-rust" "$ROOT/CCProxy-v$NEW.partial"
rm -f  "$ROOT"/*.zip
mkdir -p "$ROOT/install_a/service" "$ROOT/pkg/service"

# 两个二进制 + 松散 webui（与 release.yml 的 pkg 同构：托盘在根，代理在 service/）
install_into() { # $1 = 目标根, $2 = package.json 里的版本
  cp "$TARGET/CCProxyTray.exe" "$1/"
  cp "$TARGET/ccproxy.exe"     "$1/service/"
  cp -r crates/ccproxy/webui   "$1/service/webui"
  printf '{"version":"%s"}\n' "$2" > "$1/package.json"
}

install_into "$ROOT/install_a" "$OLD"
install_into "$ROOT/pkg"       "$NEW"

# 当前安装的配置：直连出站，日志留在安装目录里（继承逻辑会把它带过去）
cat > "$ROOT/install_a/service/ccproxy.json" <<'JSON'
{
  "proxy": "direct",
  "noProxy": "localhost,127.0.0.1"
}
JSON
# 包里带的那份是"默认配置"，用来证明继承是把用户的覆盖掉了，而不是被默认值冲掉
cat > "$ROOT/pkg/service/ccproxy.json" <<'JSON'
{
  "proxy": "default",
  "noProxy": "localhost,127.0.0.1"
}
JSON

# 造一条"历史日志"，用来验证日志继承
mkdir -p "$ROOT/install_a/service/logs" "$ROOT/install_a/logs"
printf 'sentinel-proxy-log %s\n' "$(date -u +%FT%TZ)" > "$ROOT/install_a/service/logs/proxy.log"
printf 'sentinel-tray-log\n'                          > "$ROOT/install_a/logs/tray.log"

echo "旧版本 $OLD  新版本 $NEW  夹具目录 $ROOT"
