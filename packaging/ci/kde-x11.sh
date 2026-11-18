#!/usr/bin/env bash
# 真实 Plasma X11 会话下的托盘验收（CI）。
#
# 流程：解包 Linux tarball → Xvfb 虚拟 X 服务 → kwin_x11 + plasmashell 起真实面板 →
# 起隔离端口代理 → 起 ksni 托盘 → 截全屏并扫描绿色圆点 → 在图标上右键，
# 断言托盘右键菜单真实弹出（新映射窗口 或 图标附近区域像素变化，任一成立）。
#
# 这条链路证明的是“真实桌面可用”：Xvfb 上跑的是完整 Plasma X11 会话，
# 面板/watcher/托盘/菜单窗口都是真实 X 窗口，不是静态检查或裁剪截图冒充。
#
# 输入环境变量（必填，脚本自行校验非空）：
#   TARBALL   *.tar.gz 绝对路径
#   WORK_DIR  可写临时目录（脚本在其中建 root/ data/ logs/ state/）
#   SHOTS_DIR 截图与证据日志输出目录（脚本自行 mkdir -p）
#   PORT      隔离端口（脚本不假设 8787）
#
# 退出码：0 全部断言通过；非 0 任一断言失败，证据落在 SHOTS_DIR。

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

PIDS=()

# 清理在任何早期失败之前注册：Xvfb/plasma 阶段就失败时也要收掉已拉起的进程，
# 否则 :99 与残留 plasmashell 会污染同机后续运行。
cleanup() {
    local pid
    for pid in ${PIDS[@]:-}; do
        kill "$pid" 2>/dev/null || true
    done
    sleep 1
    for pid in ${PIDS[@]:-}; do
        kill -9 "$pid" 2>/dev/null || true
    done
    # 兜底：匹配本会话显示名/命令行的残留外壳进程。
    pkill -f "Xvfb $DISPLAY" 2>/dev/null || true
    pkill -f 'kwin_x11' 2>/dev/null || true
    pkill -f 'plasmashell' 2>/dev/null || true
    pkill -f 'kded[56]?' 2>/dev/null || true
}
trap cleanup EXIT

EXIT_STATUS=0

# ---------------------------------------------------------------------------
# 1. 证据与断言辅助函数
# ---------------------------------------------------------------------------
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
        echo "----- X 窗口树（after）-----"
        if [ -f "$SHOTS_DIR/x11-windows-after.txt" ]; then
            cat "$SHOTS_DIR/x11-windows-after.txt"
        else
            xwininfo -root -tree 2>&1 || true
        fi
        echo "----- 相关进程 -----"
        ps -eo pid,comm,args 2>&1 \
            | grep -E 'Xvfb|plasma|kwin|commandcode|xdotool|import' | grep -v grep || true
        echo "----- tray.log -----"
        if [ -f "$WORK_DIR/state/ccproxy/tray.log" ]; then
            cat "$WORK_DIR/state/ccproxy/tray.log" 2>&1
        else
            echo "(tray.log 尚未生成)"
        fi
    } >"$evidence" 2>&1 || true
}

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
wait_for() {
    local name="$1" timeout="$2" interval="$3"
    shift 3
    # 允许调用方写成 `wait_for name timeout interval -- command`。
    # 不剥掉这个分隔符，until 会把字面量 `--` 当成命令执行，所有断言
    # 都会在超时后误报失败（2026-11-18 run 44279872249 的真实根因）。
    if [ "${1:-}" = "--" ]; then
        shift
    fi
    local deadline=$((SECONDS + timeout))
    until "$@"; do
        if [ "$SECONDS" -ge "$deadline" ]; then
            return 1
        fi
        sleep "$interval"
    done
    return 0
}

dbus_has_name() {
    local pattern="$1"
    local names
    if command -v busctl >/dev/null 2>&1; then
        # busctl --user list 每行是「服务名 PID 进程 ...」，先只取第一列；
        # 否则调用方的 ^...$ 锚点永远匹配不到带 PID 的整行。
        names="$(busctl --user list 2>/dev/null | awk 'NF {print $1}' || true)"
    else
        names="$(dbus-send --session --dest=org.freedesktop.DBus --type=method_call \
            /org/freedesktop/DBus org.freedesktop.DBus.ListNames 2>/dev/null \
            | sed -n 's/.*string "\([^"]*\)".*/\1/p' || true)"
    fi
    printf '%s\n' "$names" | grep -E "$pattern" >/dev/null 2>&1
}

# tray_service_name：返回 ksni 注册的 SNI well-known 名（不带参数）。
tray_service_name() {
    busctl --user list 2>/dev/null \
        | awk '$1 ~ /^org\.kde\.StatusNotifierItem-/ {print $1; exit}'
}

# tray_menu_path <service>：读 SNI 的 Menu 属性，得到 DBusMenu 对象路径。
tray_menu_path() {
    local service="$1"
    busctl --user call "$service" /StatusNotifierItem \
        org.freedesktop.DBus.Properties Get ss org.kde.StatusNotifierItem Menu 2>/dev/null \
        | sed -n 's/.*o "\([^"]*\)".*/\1/p'
}

# tray_menu_has_labels：真实读取 DBusMenu 布局并断言关键菜单项存在。
# 这条断言在无 GPU 的 hosted runner 上替代“截图里能看到右键菜单”：
# 它证明 SNI 已被面板接受、Menu 指针有效、DBusMenu 布局真的注册了动作，
# 而不是只看到一个服务名。
tray_menu_has_labels() {
    local service path layout label
    service="$(tray_service_name)" || return 1
    [ -n "$service" ] || return 1
    path="$(tray_menu_path "$service")" || return 1
    [ -n "$path" ] || return 1
    # 参数里的 -1 会被 busctl/gdbus 当成选项；gdbus 用 `--` 终止选项解析。
    layout="$(gdbus call --session --dest "$service" --object-path "$path" \
        --method com.canonical.dbusmenu.GetLayout -- 0 -1 "[]" 2>/dev/null)" || return 1
    for label in '启动代理' '停止代理' '打开 WebUI' '退出'; do
        printf '%s\n' "$layout" | grep -Fq "$label" || return 1
    done
    return 0
}

# ---------------------------------------------------------------------------
# 2. 会话总线：无 DBUS_SESSION_BUS_ADDRESS 时自我重入到 dbus-run-session。
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
# 4. Xvfb 虚拟 X 服务
# ---------------------------------------------------------------------------
export DISPLAY="${CC_TEST_DISPLAY:-:99}"
# -ac：Xvfb 无 auth 文件时照样可能被 MIT-MAGIC-COOKIE 拒绝，显式放开本地客户
# 端；-nolisten tcp 只留 unix socket。用 -screen 固定 1920x1080 供截图断言。
Xvfb "$DISPLAY" -screen 0 1920x1080x24 -ac -nolisten tcp >"$SHOTS_DIR/xvfb.log" 2>&1 &
XVFB_PID=$!
PIDS+=("$XVFB_PID")

# 断言：X 服务真实可连，而不是脚本假设显示可用。首跑失败时只看到「30s 内不可用」，
# 而失败瞬间手工跑同一条命令立即成功——真因是 Xvfb 会先建 socket 再初始化，
# 连上去的客户端在服务端就绪前会**阻塞**，第一次探测就把整个等待窗口吃掉了。
# 所以每次探测必须自带 timeout，让循环能真正重试。
x11_probe() { timeout 5 xwininfo -root >"$SHOTS_DIR/xwininfo-probe.log" 2>&1; }
if ! wait_for "xvfb" 90 1 -- x11_probe; then
    {
        echo "--- tools ---"
        for t in xwininfo xdpyinfo xset xdotool Xvfb; do
            printf '%s: %s\n' "$t" "$(command -v "$t" || echo "(missing)")"
        done
        echo "--- xwininfo -root ---"
        DISPLAY="$DISPLAY" xwininfo -root 2>&1 || echo "rc=$?"
        echo "--- xdpyinfo (head) ---"
        DISPLAY="$DISPLAY" xdpyinfo 2>&1 | head -6 || echo "rc=$?"
        echo "--- /tmp/.X11-unix ---"
        ls -l /tmp/.X11-unix 2>&1 || true
        echo "--- xvfb.log (tail) ---"
        tail -20 "$SHOTS_DIR/xvfb.log" 2>/dev/null || true
    } >"$SHOTS_DIR/xvfb-diagnostics.txt" 2>&1 || true
    fail "xvfb" "Xvfb $DISPLAY 30s 内不可用；见 xvfb-diagnostics.txt"
fi
echo "STEP ok: xvfb ($(grep -m1 dimensions "$SHOTS_DIR/xwininfo-probe.log" 2>/dev/null | tr -s ' ' || echo 'geometry unknown'))"

# X11 会话：不保留任何 Wayland 相关环境，避免托盘误走 wayland 平台插件。
unset WAYLAND_DISPLAY || true
export QT_QPA_PLATFORM=xcb

# 软件渲染：无 GPU runner 上 GL 走软件实现；X11 合成器用 QPainter 软件合成。
export LIBGL_ALWAYS_SOFTWARE=1
export KWIN_COMPOSE=Q

# StatusNotifierWatcher 由 plasma-workspace 的 KDED 插件注册，不是 plasmashell
# 内置：不显式拉起 kded，会话总线上永远不会出现 watcher。kded5 → kded6 →
# kded 依次解析；一个都找不到必须 fail，不允许静默跳过。
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

# kded 不会自动加载 statusnotifierwatcher（默认 session 里通常由系统托盘
# applet 触发）；hosted runner 没有默认面板配置时它永远不会出现。这里
# 直接让 kded 加载模块，避免把“面板没请求”误判成“托盘没注册”。
if ! wait_for "kded-service" 15 0.5 -- dbus_has_name '^org\.kde\.kded[56]$'; then
    fail "kded-service" "15s 内未出现 org.kde.kded5/6；见 kded.log"
fi
KDED_SERVICE="$(busctl --user list 2>/dev/null | awk '$1 ~ /^org\.kde\.kded[56]$/ {print $1; exit}')"
if [ -n "$KDED_SERVICE" ]; then
    busctl --user call "$KDED_SERVICE" /kded "$KDED_SERVICE" loadModule s statusnotifierwatcher \
        >"$SHOTS_DIR/kded-load-watcher.log" 2>&1 || true
fi

# ---------------------------------------------------------------------------
# 5. kwin_x11 + plasmashell：完整 Plasma X11 面板
# ---------------------------------------------------------------------------
# 不调用 startplasma-x11：它检测到 /run/systemd/system 后会走 systemd user
# manager 路径；hosted runner 没有可用的 user systemd 实例时，它会连续重试
# org.freedesktop.systemd1 并永远不启动 kwin/plasmashell（2026-11-18 本地
# 无头 VM 实测）。直接启动真实 KDE 组件，既避开这个宿主差异，又保留完整
# X11 compositor + Plasma 面板 + watcher 的验收对象。
# plasmashell 5.27 在活动管理器缺席时会直接 abort shell load，先起
# kactivitymanagerd；KDE 守护进程装在 libexec 里而不是 PATH。
KAMD_BIN=""
for candidate in kactivitymanagerd /usr/lib/*/libexec/kactivitymanagerd; do
    if [ -x "$candidate" ]; then
        KAMD_BIN="$candidate"
        break
    fi
done
[ -n "$KAMD_BIN" ] || fail "kactivitymanagerd-binary" \
    "未找到 kactivitymanagerd（libexec: $(ls /usr/lib/*/libexec 2>/dev/null | grep -i activity || echo none)）"
"$KAMD_BIN" >"$SHOTS_DIR/kactivitymanagerd.log" 2>&1 &
KAMD_PID=$!
PIDS+=("$KAMD_PID")

kwin_x11 >"$SHOTS_DIR/kwin-x11.log" 2>&1 &
KWIN_PID=$!
PIDS+=("$KWIN_PID")
if ! wait_for "kwin-x11" 30 0.5 -- dbus_has_name '^org\.kde\.KWin$'; then
    fail "kwin-x11" "30s 内未出现 org.kde.KWin；见 kwin-x11.log"
fi
echo "STEP ok: kwin-x11"

plasmashell >"$SHOTS_DIR/plasmashell.log" 2>&1 &
PLASMA_PID=$!
PIDS+=("$PLASMA_PID")

plasma_x11_ready() {
    # 两个条件同时成立：会话总线上有 watcher，且 X 窗口树里出现 plasmashell
    # 顶层窗口。缺任一个都说明面板没起来，不许静默降级成“只跑了托盘”。
    dbus_has_name '^org\.kde\.StatusNotifierWatcher$' || return 1
    xdotool search --class plasmashell >/dev/null 2>&1 || return 1
    return 0
}

if ! wait_for "plasma-x11" 90 0.5 -- plasma_x11_ready; then
    {
        echo "--- kservices5 statusnotifier* ---"
        ls -1 /usr/share/kservices5/ 2>/dev/null | grep -i statusnotifier || echo "(none)"
        echo "--- kded plugin dir ---"
        ls -1 /usr/lib/x86_64-linux-gnu/qt5/plugins/kf5/kded/ 2>/dev/null || echo "(missing)"
        echo "--- plasmashell windows (xdotool class) ---"
        xdotool search --class plasmashell 2>&1 || true
        echo "--- X window tree ---"
        xwininfo -root -tree 2>&1 | head -40 || true
        echo "--- kwin-x11.log (tail) ---"
        tail -40 "$SHOTS_DIR/kwin-x11.log" 2>/dev/null || true
        echo "--- plasmashell.log (tail) ---"
        tail -40 "$SHOTS_DIR/plasmashell.log" 2>/dev/null || true
        echo "--- kactivitymanagerd.log (tail) ---"
        tail -20 "$SHOTS_DIR/kactivitymanagerd.log" 2>/dev/null || true
        echo "--- kded.log ---"
        cat "$SHOTS_DIR/kded.log" 2>/dev/null || true
    } >"$SHOTS_DIR/plasma-x11-diagnostics.txt" 2>&1 || true
    fail "plasma-x11" "90s 内 watcher/plasmashell 窗口未就绪；见 plasma-x11-diagnostics.txt"
fi
echo "STEP ok: plasma-x11"

# 面板里的系统托盘注册 StatusNotifierHost 后，SNI 图标才会被绘制。
if ! wait_for "status-notifier-host" 30 0.5 -- dbus_has_name '^org\.kde\.StatusNotifierHost-'; then
    fail "status-notifier-host" "30s 内未出现 StatusNotifierHost；见 plasmashell.log"
fi
echo "STEP ok: status-notifier-host"

# ---------------------------------------------------------------------------
# 6. 代理：隔离端口 / 隔离数据与日志目录
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

if ! wait_for "proxy-webui" 30 0.5 -- proxy_webui_ok; then
    fail "proxy-webui" "代理 /webui/ 30s 未返回 200；见 proxy.log"
fi
echo "STEP ok: proxy-webui"

# ---------------------------------------------------------------------------
# 7. 托盘：ksni 注册到会话总线
# ---------------------------------------------------------------------------
mkdir -p "$WORK_DIR/state"
export XDG_STATE_HOME="$WORK_DIR/state"
export CC_TRAY_NS="ci-x11-$PORT"
CC_PROXY_BIN="$PROXY_BIN" "$TRAY_BIN" >"$SHOTS_DIR/tray-stdout.log" 2>&1 &
TRAY_PID=$!
PIDS+=("$TRAY_PID")

TRAY_LOG="$WORK_DIR/state/ccproxy/tray.log"

tray_registered() {
    dbus_has_name '^org\.kde\.StatusNotifierItem-' || return 1
    [ -f "$TRAY_LOG" ] || return 1
    grep -q '单实例锁已取得' "$TRAY_LOG" 2>/dev/null || return 1
    return 0
}

if ! wait_for "tray-registered" 30 0.5 -- tray_registered; then
    fail "tray-registered" "托盘 30s 内未注册 SNI；见 tray-stdout.log 与 $TRAY_LOG"
fi
echo "STEP ok: tray-registered"

if ! wait_for "tray-menu" 30 0.5 -- tray_menu_has_labels; then
    fail "tray-menu" "30s 内未读到预期的 DBusMenu 菜单项；见 tray.log 与 dump_evidence"
fi
echo "STEP ok: tray-menu"



sleep 4

# ---------------------------------------------------------------------------
# 8. 截全屏 + 扫描绿色圆点
# ---------------------------------------------------------------------------
DESKTOP_PNG="$SHOTS_DIR/x11-desktop.png"
timeout -k 5 30 import -window root "$DESKTOP_PNG" >"$SHOTS_DIR/import-desktop.log" 2>&1 \
    || fail "screenshot" "import 截全屏失败或 30s 无响应；见 import-desktop.log"
[ -s "$DESKTOP_PNG" ] || fail "screenshot" "截图文件为空: $DESKTOP_PNG"

size_kb=$(( $(stat -c%s "$DESKTOP_PNG") / 1024 ))
if [ "$size_kb" -le 10 ]; then
    fail "screenshot" "截图仅 ${size_kb}KB，疑似空图；见 $DESKTOP_PNG"
fi
echo "STEP ok: screenshot"

if ! python3 "$SCANNER" "$DESKTOP_PNG" --json "$SHOTS_DIR/x11-pixels.json"; then
    echo "STEP fail: tray-pixels" >&2
    echo "--- 扫描 JSON（含整屏主色统计）---" >&2
    cat "$SHOTS_DIR/x11-pixels.json" >&2 || true
    dump_evidence "tray-pixels"
    EXIT_STATUS=1
    exit 1
fi
echo "STEP ok: tray-pixels"

# 从扫描 JSON 取绿点质心，作为右键点击坐标。
read -r CX CY <<< "$(python3 -c \
    'import json,sys; d=json.load(open(sys.argv[1])); print(d["centroid"][0], d["centroid"][1])' \
    "$SHOTS_DIR/x11-pixels.json")"
CX_INT="$(printf '%.0f' "$CX")"
CY_INT="$(printf '%.0f' "$CY")"

# ---------------------------------------------------------------------------
# 9. 右键托盘图标，断言菜单真实弹出
# ---------------------------------------------------------------------------
BEFORE_TREE="$SHOTS_DIR/x11-windows-before.txt"
AFTER_TREE="$SHOTS_DIR/x11-windows-after.txt"
MENU_PNG="$SHOTS_DIR/x11-menu.png"

xwininfo -root -tree >"$BEFORE_TREE" 2>&1

# 按钮 3 = 右键；--sync 保证移动完成后再点击。点击失败必须显式落证据，
# 不能让 set -e 静默中断在采集 after 窗口树之前。
if ! timeout -k 5 20 xdotool mousemove --sync "$CX_INT" "$CY_INT" click 3; then
    fail "tray-click" "xdotool 右键点击质心 ($CX_INT,$CY_INT) 失败或 20s 无响应"
fi
# 菜单弹出有一两帧延迟，固定等待落屏；不是断言，断言在后面的窗口/像素比对。
sleep 2

xwininfo -root -tree >"$AFTER_TREE" 2>&1
timeout -k 5 30 import -window root "$MENU_PNG" >"$SHOTS_DIR/import-menu.log" 2>&1 \
    || fail "menu-screenshot" "菜单截图失败或 30s 无响应；见 import-menu.log"

# --- 断言 a：after 出现 before 没有的、已映射（IsViewable）顶层窗口 ---
extract_ids() {
    grep -oE '0x[0-9a-fA-F]+' "$1" | sort -u
}
mapfile -t BEFORE_IDS < <(extract_ids "$BEFORE_TREE")
mapfile -t AFTER_IDS < <(extract_ids "$AFTER_TREE")
before_count=${#BEFORE_IDS[@]}
after_count=${#AFTER_IDS[@]}

NEW_IDS=()
for id in "${AFTER_IDS[@]}"; do
    if ! printf '%s\n' "${BEFORE_IDS[@]}" | grep -qx "$id"; then
        NEW_IDS+=("$id")
    fi
done

new_viewable=0
for id in ${NEW_IDS[@]:-}; do
    info="$(xwininfo -id "$id" 2>/dev/null || true)"
    [ -n "$info" ] || continue
    echo "$info" | grep -q 'Map State: IsViewable' || continue
    # 排除截图工具与滚动条噪声窗口。
    echo "$info" | grep -qiE 'spectacle|import|scrollbar' && continue
    new_viewable=$((new_viewable + 1))
done

a_ok=0
[ "$new_viewable" -ge 1 ] && a_ok=1

# --- 断言 b：质心周围 ±200px 区域前后两帧像素变化 > 2000 ---
# 扫描器退出 1 只代表差异不足阈值，不在这里中止；原始数字写进 JSON 与日志。
python3 "$SCANNER" --before "$DESKTOP_PNG" --after "$MENU_PNG" \
    --diff-region "$CX" "$CY" 200 \
    --json "$SHOTS_DIR/x11-diff.json" || true

changed_pixels=0
if [ -f "$SHOTS_DIR/x11-diff.json" ]; then
    changed_pixels="$(python3 -c \
        'import json,sys; print(json.load(open(sys.argv[1]))["changed_pixels"])' \
        "$SHOTS_DIR/x11-diff.json")"
fi

b_ok=0
[ "$changed_pixels" -gt 2000 ] && b_ok=1

# a 或 b 任一成立即通过；日志写明哪条成立与各自原始数字。
if [ "$a_ok" -eq 0 ] && [ "$b_ok" -eq 0 ]; then
    echo "STEP fail: tray-menu" >&2
    echo "window: before=$before_count after=$after_count new=${#NEW_IDS[@]} new_viewable=$new_viewable" >&2
    echo "diff: changed_pixels=$changed_pixels (threshold 2000)" >&2
    dump_evidence "tray-menu"
    EXIT_STATUS=1
    exit 1
fi

echo "STEP ok: tray-menu | a(new_viewable=$new_viewable, before=$before_count after=$after_count) b(changed=$changed_pixels/2000) | a_ok=$a_ok b_ok=$b_ok"

# ---------------------------------------------------------------------------
# 10. 清理由顶部注册的 trap 负责（见 cleanup）。
# ---------------------------------------------------------------------------
exit "$EXIT_STATUS"
