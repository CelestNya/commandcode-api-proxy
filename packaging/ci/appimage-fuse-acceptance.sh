#!/usr/bin/env bash
# AppImage 真实 FUSE 运行验收。
#
# 关键点：直接执行 AppImage 本体，**不用** --appimage-extract-and-run。后者会
# 解开 squashfs 再执行，绕过了 FUSE 与 AppRun，等于没验。因此这里必须看到：
#   /dev/fuse 存在 → 运行期间出现指向 AppImage 的 fuse 挂载 → 挂载点里的二进制
#   就是真正在跑的那个进程 → 服务端口可用 → 退出后挂载消失。
#
# 用法：
#   APPIMAGE=/path/commandcode-proxy-<ver>-x86_64.AppImage PORT=18790 \
#   EVIDENCE_DIR=/path/out bash packaging/ci/appimage-fuse-acceptance.sh
#   （WORK_DIR 由脚本在建：默认 $RUNNER_TEMP/appimage-work）
#
# 环境变量：
#   APPIMAGE      AppImage 绝对路径（必填）
#   PORT          隔离端口，默认 18790
#   WORK_DIR      可写工作目录（默认 $RUNNER_TEMP/appimage-work）
#   EVIDENCE_DIR  证据输出目录（默认 $RUNNER_TEMP/appimage-evidence）

set -euo pipefail

APPIMAGE="${APPIMAGE:?APPIMAGE 必填}"
PORT="${PORT:-18790}"
WORK_DIR="${WORK_DIR:-${RUNNER_TEMP:-/tmp}/appimage-work}"
EVIDENCE_DIR="${EVIDENCE_DIR:-${RUNNER_TEMP:-/tmp}/appimage-evidence}"

mkdir -p "$WORK_DIR" "$EVIDENCE_DIR"
exec > >(tee -a "$EVIDENCE_DIR/appimage.log") 2>&1

SUDO=""
if [ "$(id -u)" -ne 0 ]; then SUDO="sudo"; fi

fail() {
    echo "STEP fail: $1" >&2
    shift
    for line in "$@"; do echo "  $line" >&2; done
    echo "--- appimage stdout/stderr ---" >&2
    cat "$WORK_DIR/appimage-run.log" 2>/dev/null || true
    echo "--- mounts ---" >&2
    findmnt -rn -o TARGET,SOURCE,FSTYPE 2>/dev/null | grep -i fuse || true
    echo "--- proxy log ---" >&2
    find "$WORK_DIR" -name '*.log' -type f -exec sh -c 'echo "== $1"; tail -50 "$1"' _ {} \; 2>/dev/null || true
    exit 1
}
ok() { echo "STEP ok: $1"; }

[ -f "$APPIMAGE" ] || fail "appimage-exists" "no file at $APPIMAGE"
# artifact 下载可能丢失可执行位；补上但记录原始模式，避免掩盖打包问题。
ORIG_MODE="$(stat -c '%a' "$APPIMAGE")"
chmod +x "$APPIMAGE"
ok "appimage-exists (mode was $ORIG_MODE)"

# ---- 1. FUSE 前提：包、模块、设备节点 ----
# Ubuntu 24.04 把 libfuse2 重命名为 libfuse2t64（time_t 64 位 ABI 迁移）。
# 两个名字都试，但安装失败必须报错——AppImage 没有 libfuse 就根本 mount 不了。
if ! command -v fusermount >/dev/null 2>&1 && ! command -v fusermount3 >/dev/null 2>&1; then
    $SUDO apt-get update -qq
    $SUDO apt-get install -y libfuse2t64 || $SUDO apt-get install -y libfuse2
fi
{ command -v fusermount || command -v fusermount3; } >/dev/null 2>&1 \
    || fail "libfuse" "neither fusermount nor fusermount3 available"
ok "libfuse ($( (command -v fusermount || command -v fusermount3) | head -1 ))"

$SUDO modprobe fuse 2>/dev/null || true
[ -e /dev/fuse ] || fail "dev-fuse" "/dev/fuse missing after modprobe fuse (kernel has no fuse module?)"
[ -c /dev/fuse ] || fail "dev-fuse" "/dev/fuse is not a character device"
ok "dev-fuse"

# ---- 2. 启动并捕获「指向 AppImage 的 FUSE 挂载」 ----
# TMPDIR 收进 WORK_DIR：AppImage runtime 的挂载点固定为 $TMPDIR/.mount_XXXX，
# 隔离后既能精确断言，也方便确认退出后没有残留。
TMPDIR="$WORK_DIR/tmp"
mkdir -p "$TMPDIR"
export TMPDIR

PORT="$PORT" \
CC_DATA_DIR="$WORK_DIR/data" \
CC_LOG_DIR="$WORK_DIR/logs" \
    setsid "$APPIMAGE" start > "$WORK_DIR/appimage-run.log" 2>&1 &
LAUNCH_PID=$!
ok "launched (pid=$LAUNCH_PID)"

MOUNT_POINT=""
for ((i = 0; i < 25; i++)); do
    # 只认「SOURCE 是本 AppImage 的 fuse 挂载」——别的 fuse 挂载（容器 overlay
    # 等）不算数。
    MOUNT_POINT="$(findmnt -rn -o TARGET,SOURCE,FSTYPE 2>/dev/null \
        | awk -v app="$APPIMAGE" '$0 ~ /fuse/ && index($0, app) > 0 { print $1; exit }')"
    if [ -n "$MOUNT_POINT" ]; then break; fi
    if ! kill -0 "$LAUNCH_PID" 2>/dev/null; then
        fail "appimage-mount" "process exited before any FUSE mount appeared"
    fi
    sleep 1
done
[ -n "$MOUNT_POINT" ] \
    || fail "appimage-mount" "no FUSE mount referencing $APPIMAGE within 25s (did the runtime fall back to extraction?)"
# 挂载点必须落在我们隔离的 TMPDIR 里，否则说明 TMPDIR 没生效、不可信。
case "$MOUNT_POINT" in
    "$TMPDIR"/.mount_*) ;;
    *) fail "appimage-mount" "mount point '$MOUNT_POINT' is outside TMPDIR=$TMPDIR" ;;
esac
findmnt -rn -o TARGET,SOURCE,FSTYPE | grep -F "$MOUNT_POINT" | tee "$EVIDENCE_DIR/mount-during-run.txt"
ok "appimage-mount ($MOUNT_POINT)"

# 挂载点里的二进制必须可读、且就是被执行的路径——如果 runtime 偷偷解包到
# 别处再执行，这一条会失败。
test -x "$MOUNT_POINT/usr/bin/commandcode-proxy" \
    || fail "bundled-binary" "$MOUNT_POINT/usr/bin/commandcode-proxy not executable"
ok "bundled-binary"

# ---- 3. 服务可用性 ----
wait_http() {
    local url="$1" want="$2" timeout="$3" step="$4" i code
    for ((i = 0; i < timeout; i++)); do
        code="$(curl -fsS -o /dev/null -m 2 -w '%{http_code}' "$url" 2>/dev/null || true)"
        if [ "$code" = "$want" ]; then echo "  $url -> $code"; return 0; fi
        sleep 1
    done
    fail "$step" "$url did not return $want within ${timeout}s (last='${code:-<none>}')"
}
wait_http "http://127.0.0.1:$PORT/health" 200 45 "health"
wait_http "http://127.0.0.1:$PORT/webui/" 200 15 "webui"
wait_http "http://127.0.0.1:$PORT/webui/index.html" 200 15 "webui-index"

HEALTH_BODY="$(curl -fsS -m 2 "http://127.0.0.1:$PORT/health")"
echo "$HEALTH_BODY" > "$EVIDENCE_DIR/health.json"
grep -q '"status":"ok"' "$EVIDENCE_DIR/health.json" \
    || fail "health-body" "unexpected /health body: $HEALTH_BODY"
ok "health + webui"

# 真正的运行进程必须来自挂载点（AppRun exec 的就是它）。
PROXY_PID="$(pgrep -f "$MOUNT_POINT/usr/bin/commandcode-proxy" | head -1 || true)"
[ -n "$PROXY_PID" ] \
    || fail "process-from-mount" "no process running $MOUNT_POINT/usr/bin/commandcode-proxy (pgrep -af output below)"
[ -n "$PROXY_PID" ] && ps -o pid,args -p "$PROXY_PID" | tee "$EVIDENCE_DIR/proxy-process.txt"
ok "process-from-mount (pid=$PROXY_PID)"

# ---- 4. 退出必须卸载；残留挂载就是资源泄漏 ----
# 先按进程组发 TERM（setsid 让 AppImage runtime 自成会话）；runtime 有时把
# AppRun 再 fork 一层，所以补一次按挂载路径的 pkill。
kill -TERM -"$LAUNCH_PID" 2>/dev/null || true
pkill -TERM -f "$MOUNT_POINT/usr/bin/commandcode-proxy" 2>/dev/null || true
UNMOUNTED=0
for ((i = 0; i < 20; i++)); do
    if ! findmnt -rn -o TARGET 2>/dev/null | grep -qxF "$MOUNT_POINT"; then UNMOUNTED=1; break; fi
    sleep 1
done
if [ "$UNMOUNTED" = "0" ]; then
    echo "  mount still present after SIGTERM; trying fusermount -u" >&2
    { command -v fusermount3 >/dev/null 2>&1 && $SUDO fusermount3 -u "$MOUNT_POINT" 2>/dev/null; } \
        || { command -v fusermount >/dev/null 2>&1 && $SUDO fusermount -u "$MOUNT_POINT" 2>/dev/null; } \
        || true
    for ((i = 0; i < 10; i++)); do
        if ! findmnt -rn -o TARGET 2>/dev/null | grep -qxF "$MOUNT_POINT"; then UNMOUNTED=1; break; fi
        sleep 1
    done
fi
[ "$UNMOUNTED" = "1" ] || fail "cleanup" "FUSE mount $MOUNT_POINT leaked after process exit"
ok "cleanup (mount released)"

# 端口也必须释放（进程已退出）。
for ((i = 0; i < 15; i++)); do
    if ! ss -ltn "sport = :$PORT" 2>/dev/null | grep -q ":$PORT"; then break; fi
    sleep 1
done
ss -ltn "sport = :$PORT" 2>/dev/null | grep -q ":$PORT" \
    && fail "cleanup" "port $PORT still listening after exit"
ok "port-released"

# 残留的 .mount_* 目录会让同机后续运行误判，必须为空。
LEFT="$(find "$TMPDIR" -maxdepth 1 -name '.mount_*' -print 2>/dev/null || true)"
[ -z "$LEFT" ] || fail "cleanup" "leftover mount dirs: $LEFT"
ok "cleanup (no leftover .mount_*)"

echo "STEP ok: appimage-fuse-acceptance complete"
