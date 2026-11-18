#!/usr/bin/env bash
# 真实 Plasma Wayland 会话下的托盘验收（CI）。
#
# 流程：解包 Linux tarball → 起 kwin_wayland 虚拟会话 → 拉起 kded 注册
# StatusNotifierWatcher 插件 → plasmashell 起面板 → 起隔离端口代理 →
# 起 ksni 托盘 → spectacle 截全屏 → 像素扫描绿色圆点托盘图标。
#
# 这条链路证明的是“真实桌面可用”：面板（plasmashell）真实存在并提供
# watcher，托盘真实向会话总线注册 SNI，图标真实被合成到 Wayland 画面上，
# 而不是静态检查二进制或伪造一个假 watcher 走过场。
#
# 输入环境变量（必填，脚本自行校验非空）：
#   TARBALL   *.tar.gz 绝对路径
#   WORK_DIR  可写临时目录（脚本在其中建 root/ data/ logs/ state/ runtime/）
#   SHOTS_DIR 截图与证据日志输出目录（脚本自行 mkdir -p）
#   PORT      隔离端口（脚本不假设 8787）
#
# 退出码：0 全部断言通过；非 0 任一断言失败，证据（各进程日志、tray.log、
# DBus 名快照、窗口/进程快照）落在 SHOTS_DIR。

set -euo pipefail

# ---------------------------------------------------------------------------
# 0. 输入校验
# ---------------------------------------------------------------------------
for var in TARBALL WORK_DIR SHOTS_DIR PORT; do
    if [ -z "${!var:-}" ]; then
        echo "STEP fail: env-$var" >&2
        echo "error: 环境变量 $var 未设置或为空" >&2
        exit 1
    fi
done

if [ ! -f "$TARBALL" ]; then
    echo "STEP fail: tarball-exists" >&2
    echo "error: tarball 不存在: $TARBALL" >&2
    exit 1
fi

mkdir -p "$SHOTS_DIR"
mkdir -p "$WORK_DIR"

SCRIPT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
SCANNER="$SCRIPT_DIR/scan-tray-pixels.py"
SELF="$SCRIPT_DIR/$(basename "${BASH_SOURCE[0]}")"

# 跟踪本脚本拉起的后台进程，EXIT 时统一清理。
PIDS=()
EXIT_STATUS=0

# 清理必须在任何早期失败之前注册：kwin-socket 阶段就失败时同样要把已拉起的
# 进程收掉，否则残留的 kwin/plasmashell 会占住 socket 与显示，污染同机后续
# 运行。清理失败不掩盖前面已记录的真实断言失败（退出码由 fail() 决定）。
cleanup() {
    local pid
    for pid in ${PIDS[@]:-}; do
        kill "$pid" 2>/dev/null || true
    done
    sleep 1
    for pid in ${PIDS[@]:-}; do
        kill -9 "$pid" 2>/dev/null || true
    done
    # 兜底：按命令行匹配本会话拉起的残留（限定我们的 socket 名）。
    pkill -f "kwin_wayland.*${WAYLAND_SOCKET_NAME:-wayland-cc}" 2>/dev/null || true
    pkill -f 'plasmashell' 2>/dev/null || true
    pkill -f 'kded[56]?' 2>/dev/null || true
}
trap cleanup EXIT

# ---------------------------------------------------------------------------
# 1. 证据与断言辅助函数
# ---------------------------------------------------------------------------

# dump_evidence <断言名>：把 DBus 名、进程快照与托盘日志汇总进 SHOTS_DIR。
# 各进程的原始 stdout/stderr 已经单独落盘，这里只补一份失败时刻的快照。
dump_evidence() {
    local tag="$1"
    local evidence="$SHOTS_DIR/evidence-$tag.txt"
    {
        echo "===== evidence tag: $tag ====="
        echo "time: $(date -Iseconds 2>/dev/null || date)"
        echo "----- DBus 会话总线名 -----"
        if command -v busctl >/dev/null 2>&1; then
            busctl --user list 2>&1 || true
        else
            dbus-send --session --dest=org.freedesktop.DBus --type=method_call \
                /org/freedesktop/DBus org.freedesktop.DBus.ListNames 2>&1 || true
        fi
        echo "----- 相关进程 -----"
        ps -eo pid,comm,args 2>&1 \
            | grep -E 'kwin|plasma|commandcode|spectacle' | grep -v grep || true
        echo "----- tray.log -----"
        if [ -f "$WORK_DIR/state/ccproxy/tray.log" ]; then
            cat "$WORK_DIR/state/ccproxy/tray.log" 2>&1
        else
            echo "(tray.log 尚未生成)"
        fi
    } >"$evidence" 2>&1 || true
}

# fail <断言名> <原因>：打印失败行、落证据、以非 0 退出。
fail() {
    local name="$1"
    local reason="$2"
    echo "STEP fail: $name" >&2
    echo "reason: $reason" >&2
    dump_evidence "$name"
    EXIT_STATUS=1
    exit 1
}

# wait_for <断言名> <超时秒> <轮询间隔秒> -- <命令...>
# 命令退出 0 即断言成立；超时返回 1。轮询间隔内的 sleep 只是等待节拍，
# 不是断言本身——断言由被检查命令的真实运行时状态决定。
wait_for() {
    local name="$1" timeout="$2" interval="$3"
    shift 3
    local deadline=$((SECONDS + timeout))
    until "$@"; do
        if [ "$SECONDS" -ge "$deadline" ]; then
            return 1
        fi
        sleep "$interval"
    done
    return 0
}

# dbus_has_name <正则>：会话总线上是否出现匹配的 well-known 名。
dbus_has_name() {
    local pattern="$1"
    local names
    if command -v busctl >/dev/null 2>&1; then
        names="$(busctl --user list 2>/dev/null || true)"
    else
        names="$(dbus-send --session --dest=org.freedesktop.DBus --type=method_call \
            /org/freedesktop/DBus org.freedesktop.DBus.ListNames 2>/dev/null \
            | grep 'string' || true)"
    fi
    printf '%s\n' "$names" | grep -E "$pattern" >/dev/null 2>&1
}

# ---------------------------------------------------------------------------
# 2. 会话总线：没有 DBUS_SESSION_BUS_ADDRESS 时自我重入到 dbus-run-session。
# kwin / plasmashell / 托盘 / spectacle 必须挂在同一个会话总线上，否则托盘
# 注册的 SNI 名面板根本看不到。
# ---------------------------------------------------------------------------
if [ -z "${DBUS_SESSION_BUS_ADDRESS:-}" ]; then
    echo "STEP info: 无会话总线，dbus-run-session 自我重入"
    exec dbus-run-session -- "$SELF" "$@"
fi

# ---------------------------------------------------------------------------
# 3. 解包 tarball 并核对 FHS 布局
# ---------------------------------------------------------------------------
ROOT="$WORK_DIR/root"
rm -rf "$ROOT"
mkdir -p "$ROOT"
tar -xzf "$TARBALL" -C "$ROOT" || fail "tarball-extract" "解包失败: $TARBALL"

PROXY_BIN="$ROOT/usr/bin/commandcode-proxy"
TRAY_BIN="$ROOT/usr/bin/commandcode-proxy-tray"
WEBUI_DIR="$ROOT/usr/share/commandcode-proxy/webui"

[ -x "$PROXY_BIN" ] || fail "tarball-layout" "缺少可执行代理: $PROXY_BIN"
[ -x "$TRAY_BIN" ] || fail "tarball-layout" "缺少可执行托盘: $TRAY_BIN"
[ -d "$WEBUI_DIR" ] || fail "tarball-layout" "缺少 webui 目录: $WEBUI_DIR"
echo "STEP ok: tarball-layout"

# ---------------------------------------------------------------------------
# 4. Wayland 运行时目录 + kwin_wayland 虚拟合成器
# ---------------------------------------------------------------------------
export XDG_RUNTIME_DIR="$WORK_DIR/runtime"
mkdir -p "$XDG_RUNTIME_DIR"
chmod 0700 "$XDG_RUNTIME_DIR"

# 软件渲染：virtual 后端不需要 /dev/dri，但 GL 路径必须走软件实现，
# 否则无 GPU 的 runner 上 kwin 起不来。
export LIBGL_ALWAYS_SOFTWARE=1

# --virtual：无头虚拟输出；--no-lockscreen：避免锁屏挡住托盘区；
# --socket 接收的是 socket **名**（文件落在 $XDG_RUNTIME_DIR 下），传绝对路径
# 会让 KWin 在 XDG_RUNTIME_DIR 里再造一层同名目录，socket 断言必然失败。
# 注意：不给 kwin_wayland 设 QT_QPA_PLATFORM——compositor 自己决定平台，
# 该变量只在后面给 plasmashell/spectacle/托盘等客户端用。
WAYLAND_SOCKET_NAME="wayland-cc"
kwin_wayland --virtual --no-lockscreen \
    --socket "$WAYLAND_SOCKET_NAME" \
    >"$SHOTS_DIR/kwin.log" 2>&1 &
KWIN_PID=$!
PIDS+=("$KWIN_PID")

# 断言：Wayland socket 文件真实出现。hosted runner 可能没有 /dev/dri，
# 渲染后端崩溃会在这里超时——如实失败并留下 kwin.log，绝不改判通过。
if ! wait_for "kwin-socket" 30 0.5 -- test -S "$XDG_RUNTIME_DIR/$WAYLAND_SOCKET_NAME"; then
    fail "kwin-socket" "kwin_wayland 30s 内未创建 $XDG_RUNTIME_DIR/$WAYLAND_SOCKET_NAME；见 kwin.log"
fi
echo "STEP ok: kwin-socket"

export WAYLAND_DISPLAY="$WAYLAND_SOCKET_NAME"
# 客户端（plasmashell / spectacle / 托盘）走 wayland 平台插件。
export QT_QPA_PLATFORM=wayland

# StatusNotifierWatcher 由 plasma-workspace 的 KDED 插件注册，不是 plasmashell
# 内置：不显式拉起 kded，会话总线上永远不会出现 watcher，托盘图标也进不了
# 面板。kded5（KF5）→ kded6（KF6）→ kded 依次解析；一个都找不到必须 fail，
# 不允许静默跳过。
KDED_BIN=""
for candidate in kded5 kded6 kded; do
    if command -v "$candidate" >/dev/null 2>&1; then
        KDED_BIN="$candidate"
        break
    fi
done
[ -n "$KDED_BIN" ] || fail "kded-binary" "未找到 kded5/kded6/kded，StatusNotifierWatcher 无法注册"
"$KDED_BIN" >"$SHOTS_DIR/kded.log" 2>&1 &
KDED_PID=$!
PIDS+=("$KDED_PID")
echo "STEP ok: kded-started ($KDED_BIN)"

# ---------------------------------------------------------------------------
# 5. plasmashell：真实面板（watcher 由上面的 kded 插件提供）
# ---------------------------------------------------------------------------
plasmashell >"$SHOTS_DIR/plasmashell.log" 2>&1 &
PLASMA_PID=$!
PIDS+=("$PLASMA_PID")

# 断言：会话总线上出现 org.kde.StatusNotifierWatcher。这是面板愿意接收
# SNI 注册的标志；watcher 不出现，托盘注册得再正确也没有面板可显示。
if ! wait_for "status-notifier-watcher" 60 0.5 -- \
    dbus_has_name '^org\.kde\.StatusNotifierWatcher$'; then
    fail "status-notifier-watcher" "60s 内未出现 StatusNotifierWatcher；见 plasmashell.log"
fi
echo "STEP ok: status-notifier-watcher"

# ---------------------------------------------------------------------------
# 6. 代理：隔离端口 / 隔离数据与日志目录，CC_WEBUI_DIR 指向解包出的面板
# ---------------------------------------------------------------------------
mkdir -p "$WORK_DIR/data" "$WORK_DIR/logs"
export CC_DATA_DIR="$WORK_DIR/data"
export CC_LOG_DIR="$WORK_DIR/logs"
export CC_WEBUI_DIR="$WEBUI_DIR"

"$PROXY_BIN" start >"$SHOTS_DIR/proxy.log" 2>&1 &
PROXY_PID=$!
PIDS+=("$PROXY_PID")

proxy_health_ok() {
    local code body="$WORK_DIR/health-body"
    code="$(curl -s -o "$body" -w '%{http_code}' \
        "http://127.0.0.1:$PORT/health" 2>/dev/null || true)"
    [ "$code" = "200" ] || return 1
    grep -qE '"status"[[:space:]]*:[[:space:]]*"ok"' "$body" 2>/dev/null
}

# 断言：/health 返回 200 且 JSON 状态为 ok——代理进程真的在监听隔离端口。
if ! wait_for "proxy-health" 30 0.5 -- proxy_health_ok; then
    fail "proxy-health" "代理 /health 30s 未返回 200 ok；见 proxy.log"
fi
echo "STEP ok: proxy-health"

proxy_webui_ok() {
    local code
    code="$(curl -s -o /dev/null -w '%{http_code}' \
        "http://127.0.0.1:$PORT/webui/" 2>/dev/null || true)"
    [ "$code" = "200" ]
}

# 断言：/webui/ 返回 200——解包出来的那份面板资源被代理正确挂出。
if ! wait_for "proxy-webui" 30 0.5 -- proxy_webui_ok; then
    fail "proxy-webui" "代理 /webui/ 30s 未返回 200；见 proxy.log"
fi
echo "STEP ok: proxy-webui"

# ---------------------------------------------------------------------------
# 7. 托盘：ksni 注册到会话总线
# ---------------------------------------------------------------------------
mkdir -p "$WORK_DIR/state"
export XDG_STATE_HOME="$WORK_DIR/state"
export CC_TRAY_NS="ci-wayland-$PORT"
# CC_PROXY_BIN 必须是解包代理的绝对路径；托盘不靠 PATH 猜代理位置。
CC_PROXY_BIN="$PROXY_BIN" "$TRAY_BIN" >"$SHOTS_DIR/tray-stdout.log" 2>&1 &
TRAY_PID=$!
PIDS+=("$TRAY_PID")

TRAY_LOG="$WORK_DIR/state/ccproxy/tray.log"

tray_registered() {
    # 两个条件同时成立才算注册成功：
    #  1) 会话总线上出现 ksni 注册的 SNI 服务名；
    #  2) tray.log 记录单实例锁取得（托盘走过了启动流程，不是崩在半路）。
    dbus_has_name '^org\.kde\.StatusNotifierItem-' || return 1
    [ -f "$TRAY_LOG" ] || return 1
    grep -q '单实例锁已取得' "$TRAY_LOG" 2>/dev/null || return 1
    return 0
}

if ! wait_for "tray-registered" 30 0.5 -- tray_registered; then
    fail "tray-registered" "托盘 30s 内未注册 SNI；见 tray-stdout.log 与 $TRAY_LOG"
fi
echo "STEP ok: tray-registered"

# 面板把 SNI 图标渲染进系统托盘区需要一点时间。这里是固定落屏等待，
# 不是断言；图标是否真的出现由后面的像素扫描判定。
sleep 4

# ---------------------------------------------------------------------------
# 8. 截图 + 像素扫描绿色圆点
# ---------------------------------------------------------------------------
# spectacle（Ubuntu 24.04 包名 kde-spectacle，可执行名仍是 spectacle）：
# 二进制缺失直接 fail；版本号落盘便于首跑定位版本差异。
command -v spectacle >/dev/null 2>&1 \
    || fail "spectacle-binary" "未找到 spectacle（安装 kde-spectacle）"
spectacle --version >"$SHOTS_DIR/spectacle-version.log" 2>&1 || true

DESKTOP_PNG="$SHOTS_DIR/wayland-desktop.png"
# spectacle 在后台模式下若 KWin 截图接口不应答会一直挂着；加硬超时，
# 让「无响应」表现为本次断言失败而不是把整个 job 拖到 45 分钟超时。
if ! timeout -k 10 120 spectacle -b -n -f -o "$DESKTOP_PNG" >"$SHOTS_DIR/spectacle.log" 2>&1; then
    fail "screenshot" "spectacle 截图命令失败或 120s 无响应（rc=$?）；见 spectacle.log"
fi
[ -s "$DESKTOP_PNG" ] || fail "screenshot" "截图文件为空: $DESKTOP_PNG"

# >10KB 只是空图/纯色图粗筛；绿点是否存在由像素扫描负责，不用文件大小冒充断言。
size_kb=$(( $(stat -c%s "$DESKTOP_PNG") / 1024 ))
if [ "$size_kb" -le 10 ]; then
    fail "screenshot" "截图仅 ${size_kb}KB，疑似空图；见 $DESKTOP_PNG"
fi
echo "STEP ok: screenshot"

if ! python3 "$SCANNER" "$DESKTOP_PNG" --json "$SHOTS_DIR/wayland-pixels.json"; then
    echo "STEP fail: tray-pixels" >&2
    echo "--- 扫描 JSON（含整屏主色统计）---" >&2
    cat "$SHOTS_DIR/wayland-pixels.json" >&2 || true
    dump_evidence "tray-pixels"
    EXIT_STATUS=1
    exit 1
fi
echo "STEP ok: tray-pixels"

# ---------------------------------------------------------------------------
# 9. 清理已由顶部注册的 trap 负责（见 cleanup）。
# ---------------------------------------------------------------------------
exit "$EXIT_STATUS"
