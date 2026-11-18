#!/usr/bin/env bash
# 一次构建全部 Linux 发布物：tarball、两个 deb、AppImage。
#
# 这是 desktop-acceptance workflow 的「build once」入口：下游每个桌面/服务
# 验收 job 只下载 artifact，不重复编译。人工在 Linux 上也能直接跑同一条命令，
# 因此 CI 与本地产出的文件名、断言完全一致。
#
# 用法：
#   bash packaging/ci/build-linux-artifacts.sh [--skip-build] [--root <repo>]
#
# 输入约定：
#   --skip-build  二进制已由调用方 `cargo build --release --locked` 产出
#   --root        仓库根（默认取脚本所在目录的 ../..）
#
# 输出（全部在 <root>/dist 与 <root>/packaging-out 下）：
#   commandcode-proxy-<ver>-linux-x86_64.tar.gz            便携 tarball（FHS）
#   commandcode-proxy_<ver>-1_amd64.deb                   代理 + unit + webui
#   commandcode-proxy-tray_<ver>-1_amd64.deb              托盘（依赖代理同版本）
#   commandcode-proxy-<ver>-x86_64.AppImage               单文件桌面包
#   desktop-acceptance/                                   摊平后的上传目录：
#       ← 上面四个产物 + artifacts.txt（文件名清单）+ SHA256SUMS
#
# 断言全部是「产物真的能用」级别：deb 字段/文件清单逐项核对、tool 脚本
# bash -n、AppImage squashfs 锚点由 build-appimage.sh 负责。任何一项不过即
# 非零退出——不允许把「构建返回 0」当成产物正确。

set -euo pipefail

SKIP_BUILD=0
ROOT=""

while [ "$#" -gt 0 ]; do
    case "$1" in
        --skip-build) SKIP_BUILD=1; shift ;;
        --root) ROOT="${2:?--root 需要目录}"; shift 2 ;;
        -h|--help)
            sed -n '2,20p' "${BASH_SOURCE[0]}" | sed 's/^# \{0,1\}//'
            exit 0
            ;;
        *)
            echo "error: 未知参数 '$1'" >&2
            exit 2
            ;;
    esac
done

if [ -z "$ROOT" ]; then
    ROOT="$(cd "$(dirname "${BASH_SOURCE[0]}")/../.." && pwd)"
fi
cd "$ROOT"

# 版本只来自 workspace Cargo.toml（单一来源）；不读 git ref，否则在特性分支
# 上会得到 `feat/desktop-acceptance-1_amd64.deb` 这种非法 Debian 版本。
VER="$(grep -m1 '^version = ' Cargo.toml | cut -d'"' -f2)"
echo "workspace version: $VER"
# Semver 形状：MAJOR.MINOR.PATCH，可带 -rcN/-betaN 预发布段。
echo "$VER" | grep -Eq '^[0-9]+\.[0-9]+\.[0-9]+(-[A-Za-z0-9.]+)?$' \
    || { echo "error: workspace version '$VER' is not semver" >&2; exit 1; }

# 幂等：同一个 workspace 重复跑不能残留上一轮的 deb/AppImage，否则下游可能
# 拿到两个版本的文件（下载 artifact 时 `tar`/`dpkg` 都不会报错）。
rm -rf dist/deb dist/appimage packaging-out
mkdir -p packaging-out

# ---- 1. 用唯一入口铺 tarball / deb root / AppDir ----
if [ "$SKIP_BUILD" = "0" ]; then
    cargo build --release --locked --workspace
fi
cargo xtask dist --platform linux --skip-build

TARBALL="dist/commandcode-proxy-${VER}-linux-x86_64.tar.gz"
test -s "$TARBALL" || { echo "error: tarball missing at $TARBALL" >&2; exit 1; }

# ---- 2. dpkg-shlibdeps 推导代理运行期依赖 ----
# 代理二进制只链 libc/libm 等基础库，但版本下限必须来自真实二进制，不能拍。
# dpkg-shlibdeps 要求 cwd 有 debian/control，所以造一个最小 control 再调用。
DEB_PROXY_ROOT="dist/deb/commandcode-proxy"
test -x "$DEB_PROXY_ROOT/usr/bin/commandcode-proxy" \
    || { echo "error: staged proxy binary missing" >&2; exit 1; }

TMP_SHIM="$(mktemp -d)"
trap 'rm -rf "$TMP_SHIM"' EXIT
mkdir -p "$TMP_SHIM/debian"
cat > "$TMP_SHIM/debian/control" <<'EOF'
Source: commandcode-proxy
Package: commandcode-proxy
Architecture: amd64
Description: dpkg-shlibdeps probe
EOF

DEPS_RAW="$(cd "$TMP_SHIM" && dpkg-shlibdeps -O -e "$ROOT/$DEB_PROXY_ROOT/usr/bin/commandcode-proxy")"
DEP_LINES="$(printf '%s\n' "$DEPS_RAW" | grep -c '^shlibs:Depends=' || true)"
test "$DEP_LINES" = "1" || {
    echo "error: expected exactly 1 shlibs:Depends line, got $DEP_LINES" >&2
    printf '%s\n' "$DEPS_RAW" >&2
    exit 1
}
DEPS="$(printf '%s\n' "$DEPS_RAW" | sed -n 's/^shlibs:Depends=//p')"
test -n "$DEPS" || { echo "error: empty shlibs:Depends" >&2; exit 1; }
echo "shlibs:Depends=$DEPS"

# 插到 Description 之前（Debian control 的 canonical 顺序）；写临时文件，
# 避免 awk 一边读一边写同一个文件。
awk -v d="Depends: $DEPS" '/^Description:/ && !done { print d; done=1 } { print }' \
    "$DEB_PROXY_ROOT/DEBIAN/control" > "$DEB_PROXY_ROOT/DEBIAN/control.new"
mv "$DEB_PROXY_ROOT/DEBIAN/control.new" "$DEB_PROXY_ROOT/DEBIAN/control"

# ---- 3. 打包两个 deb 并逐项验收 ----
test -f dist/deb-names.txt || { echo "error: dist/deb-names.txt missing" >&2; exit 1; }
EXPECT_PROXY="commandcode-proxy_${VER}-1_amd64.deb"
EXPECT_TRAY="commandcode-proxy-tray_${VER}-1_amd64.deb"
GOT_PROXY="$(sed -n '1p' dist/deb-names.txt)"
GOT_TRAY="$(sed -n '2p' dist/deb-names.txt)"
test "$GOT_PROXY" = "$EXPECT_PROXY" \
    || { echo "error: xtask reported '$GOT_PROXY', expected '$EXPECT_PROXY'" >&2; exit 1; }
test "$GOT_TRAY" = "$EXPECT_TRAY" \
    || { echo "error: xtask reported '$GOT_TRAY', expected '$EXPECT_TRAY'" >&2; exit 1; }

expect_field() {
    local out="$1" field="$2" want="$3" got
    got="$(dpkg-deb --field "$out" "$field")"
    if [ "$got" != "$want" ]; then
        echo "error: $(basename "$out") field $field: got='$got' expected='$want'" >&2
        exit 1
    fi
}

expect_path() {
    local out="$1" regex="$2" want="$3" contents count
    contents="$(mktemp)"
    dpkg-deb --contents "$out" > "$contents"
    # `dpkg-deb --contents` 每行以完整路径结尾，因此用「行尾锚定」正则匹配：
    # 子串匹配会让 `./usr/bin/commandcode-proxy` 同时命中 `…-tray` 行。
    count="$(grep -c -E -- "$regex" "$contents" || true)"
    rm -f "$contents"
    if [ "$count" != "$want" ]; then
        echo "error: $(basename "$out") expected $want match(es) for /$regex/, got $count" >&2
        dpkg-deb --contents "$out" >&2
        exit 1
    fi
    return 0
}

for pair in "commandcode-proxy:$GOT_PROXY" "commandcode-proxy-tray:$GOT_TRAY"; do
    pkg="${pair%%:*}"
    out="dist/${pair#*:}"
    dpkg-deb --build "dist/deb/$pkg" "$out"

    expect_field "$out" Package "$pkg"
    expect_field "$out" Version "${VER}-1"
    expect_field "$out" Architecture "amd64"
    if [ "$pkg" = "commandcode-proxy-tray" ]; then
        expect_field "$out" Depends "commandcode-proxy (= ${VER}-1)"
    fi

    case "$pkg" in
        commandcode-proxy)
            expect_path "$out" './usr/bin/commandcode-proxy$' 1
            expect_path "$out" './usr/bin/commandcode-proxy-tray$' 0
            expect_path "$out" './lib/systemd/system/commandcode-proxy.service$' 1
            expect_path "$out" './etc/commandcode-proxy/env.example$' 1
            expect_path "$out" './usr/share/commandcode-proxy/webui/index.html$' 1
            ;;
        commandcode-proxy-tray)
            expect_path "$out" './usr/bin/commandcode-proxy-tray$' 1
            expect_path "$out" './usr/share/applications/commandcode-proxy-tray.desktop$' 1
            expect_path "$out" './usr/bin/commandcode-proxy$' 0
            ;;
    esac
    echo "deb ok: $out"
done

# maintainer script 语法：dpkg 会在安装时执行它们，bash -n 是最低门槛。
for s in preinst postinst prerm postrm; do
    bash -n "dist/deb/commandcode-proxy/DEBIAN/$s"
    bash -n "dist/deb/commandcode-proxy-tray/DEBIAN/$s"
done

# ---- 4. AppImage ----
bash packaging/appimage/build-appimage.sh "$VER" "$ROOT"
APPIMAGE="packaging-out/commandcode-proxy-${VER}-x86_64.AppImage"
test -s "$APPIMAGE" || { echo "error: AppImage missing at $APPIMAGE" >&2; exit 1; }

# ---- 5. 摊平到一个目录，供 upload-artifact 原样上传 ----
# 直接把 dist/ 与 packaging-out/ 一起上传会让 artifact 内保留 dist/... 与
# packaging-out/... 两套相对路径，下游每个 job 都得知道这些内部目录名。
# 摊平后 artifact 根目录就是 4 个产物 + 清单，接口只有文件名。
OPT_DIR="dist/desktop-acceptance"
rm -rf "$OPT_DIR"
mkdir -p "$OPT_DIR"
cp "$TARBALL" "dist/$GOT_PROXY" "dist/$GOT_TRAY" "$APPIMAGE" "$OPT_DIR/"

{
    echo "version=$VER"
    echo "tarball=$(basename "$TARBALL")"
    echo "deb_proxy=$GOT_PROXY"
    echo "deb_tray=$GOT_TRAY"
    echo "appimage=$(basename "$APPIMAGE")"
} > "$OPT_DIR/artifacts.txt"

# 哈希在摊平目录内计算：下游下载的就是这些字节，下载后可直接复核。
(
    cd "$OPT_DIR"
    sha256sum ./*.tar.gz ./*.deb ./*.AppImage > SHA256SUMS
)

cat "$OPT_DIR/artifacts.txt"
cat "$OPT_DIR/SHA256SUMS"
echo "STEP ok: linux artifacts built"
