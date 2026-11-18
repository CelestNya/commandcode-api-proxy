#!/usr/bin/env bash
# 便携 tarball 的服务级 smoke：解包 → 隔离环境真跑 → 探活 → 优雅退出。
#
# 这一层是「不经 systemd、不经托盘、不经 AppImage」的最纯粹运行证据：如果
# 它红了，说明二进制/WebUI/数据目录契约本身有问题，与桌面环境无关。
#
# 用法：
#   TARBALL=/path/commandcode-proxy-<ver>-linux-x86_64.tar.gz PORT=18791 \
#   WORK_DIR=/tmp/svc EVIDENCE_DIR=/path/out bash packaging/ci/service-smoke.sh
#
# 环境变量：
#   TARBALL       便携 tarball 绝对路径（必填）
#   PORT          隔离端口，默认 18791
#   WORK_DIR      工作目录（默认 $RUNNER_TEMP/service-smoke-work）
#   EVIDENCE_DIR  证据目录（默认 $RUNNER_TEMP/service-smoke-evidence）

set -euo pipefail

TARBALL="${TARBALL:?TARBALL 必填}"
PORT="${PORT:-18791}"
WORK_DIR="${WORK_DIR:-${RUNNER_TEMP:-/tmp}/service-smoke-work}"
EVIDENCE_DIR="${EVIDENCE_DIR:-${RUNNER_TEMP:-/tmp}/service-smoke-evidence}"

mkdir -p "$WORK_DIR" "$EVIDENCE_DIR"
exec > >(tee -a "$EVIDENCE_DIR/service-smoke.log") 2>&1

fail() {
    echo "STEP fail: $1" >&2
    shift
    for line in "$@"; do echo "  $line" >&2; done
    echo "--- proxy stdout/stderr ---" >&2
    cat "$WORK_DIR/proxy-run.log" 2>/dev/null || true
    echo "--- proxy log dir ---" >&2
    find "$WORK_DIR/logs" "$WORK_DIR/data" -maxdepth 2 -type f 2>/dev/null \
        -exec sh -c 'echo "== $1"; tail -40 "$1"' _ {} \; || true
    exit 1
}
ok() { echo "STEP ok: $1"; }

[ -f "$TARBALL" ] || fail "tarball-exists" "no file at $TARBALL"

# ---- 1. 解包 + 布局断言 ----
ROOT="$WORK_DIR/root"
rm -rf "$ROOT"
mkdir -p "$ROOT"
tar -xzf "$TARBALL" -C "$ROOT"
PROXY="$ROOT/usr/bin/commandcode-proxy"
[ -x "$PROXY" ] || fail "layout" "usr/bin/commandcode-proxy missing or not executable"
[ -x "$ROOT/usr/bin/commandcode-proxy-tray" ] || fail "layout" "tray binary missing"
[ -f "$ROOT/usr/share/commandcode-proxy/webui/index.html" ] \
    || fail "layout" "webui/index.html missing"
[ -f "$ROOT/lib/systemd/system/commandcode-proxy.service" ] \
    || fail "layout" "systemd unit missing from portable tarball"
ok "layout ($(find "$ROOT" -type f | wc -l) files)"

# ---- 2. 隔离环境启动（不碰 8787） ----
DATA_DIR="$WORK_DIR/data"
LOG_DIR="$WORK_DIR/logs"
WEBUI_DIR="$ROOT/usr/share/commandcode-proxy/webui"
mkdir -p "$DATA_DIR" "$LOG_DIR"

PORT="$PORT" HOST=127.0.0.1 \
CC_DATA_DIR="$DATA_DIR" \
CC_LOG_DIR="$LOG_DIR" \
CC_WEBUI_DIR="$WEBUI_DIR" \
    setsid "$PROXY" start > "$WORK_DIR/proxy-run.log" 2>&1 &
PROXY_PID=$!
ok "started (pid=$PROXY_PID port=$PORT)"

wait_http() {
    local url="$1" want="$2" timeout="$3" step="$4" i code
    for ((i = 0; i < timeout; i++)); do
        code="$(curl -fsS -o /dev/null -m 2 -w '%{http_code}' "$url" 2>/dev/null || true)"
        if [ "$code" = "$want" ]; then echo "  $url -> $code"; return 0; fi
        if ! kill -0 "$PROXY_PID" 2>/dev/null; then
            fail "$step" "proxy exited before $url became $want"
        fi
        sleep 1
    done
    fail "$step" "$url did not return $want within ${timeout}s (last='${code:-<none>}')"
}

wait_http "http://127.0.0.1:$PORT/health" 200 45 "health"
curl -fsS -m 2 "http://127.0.0.1:$PORT/health" > "$EVIDENCE_DIR/health.json"
grep -q '"status":"ok"' "$EVIDENCE_DIR/health.json" || fail "health" "unexpected /health body"
ok "health"

wait_http "http://127.0.0.1:$PORT/webui/" 200 15 "webui"
wait_http "http://127.0.0.1:$PORT/webui/css/tokens.css" 200 15 "static-css"
wait_http "http://127.0.0.1:$PORT/webui/js/app.js" 200 15 "static-js"

# 静态资源必须真有内容（200 + 空 body 是典型的「目录配错」症状）。
CSS_BYTES="$(curl -fsS -m 2 "http://127.0.0.1:$PORT/webui/css/tokens.css" | wc -c)"
test "$CSS_BYTES" -gt 100 || fail "static-body" "tokens.css only $CSS_BYTES bytes"
PAGE_BYTES="$(curl -fsS -m 2 "http://127.0.0.1:$PORT/webui/" | wc -c)"
test "$PAGE_BYTES" -gt 1000 || fail "static-body" "/webui/ only $PAGE_BYTES bytes"
ok "static-assets (page=${PAGE_BYTES}B css=${CSS_BYTES}B)"

# ---- 3. 数据/日志落到隔离目录（不是二进制旁边的 logs/） ----
PID_FILE="$DATA_DIR/ccproxy.pid"
test -f "$PID_FILE" || fail "data-dir" "no ccproxy.pid in CC_DATA_DIR=$DATA_DIR"
grep -qx "$PROXY_PID" "$PID_FILE" \
    || fail "data-dir" "pid file says '$(cat "$PID_FILE")', process is $PROXY_PID"
PROXY_LOG="$LOG_DIR/proxy.log"
test -s "$PROXY_LOG" || fail "log-dir" "no non-empty proxy.log in CC_LOG_DIR=$LOG_DIR"
grep -q 'cc-proxy listening on' "$PROXY_LOG" \
    || fail "log-dir" "proxy.log has no 'cc-proxy listening on' line"
ok "data-dir + log-dir"

# ---- 4. 优雅退出 + 端口释放 ----
kill -TERM "$PROXY_PID"
WAITED=0
for ((i = 0; i < 20; i++)); do
    if ! kill -0 "$PROXY_PID" 2>/dev/null; then WAITED=1; break; fi
    sleep 1
done
[ "$WAITED" = "1" ] || fail "stop" "proxy ignored SIGTERM for 20s"
for ((i = 0; i < 15; i++)); do
    if ! ss -ltn "sport = :$PORT" 2>/dev/null | grep -q ":$PORT"; then break; fi
    sleep 1
done
ss -ltn "sport = :$PORT" 2>/dev/null | grep -q ":$PORT" \
    && fail "stop" "port $PORT still listening after exit"
# SIGTERM 路径应删除 PID 文件（cli.rs 的 shutdown 契约）。
test ! -f "$PID_FILE" || fail "stop" "ccproxy.pid survived graceful shutdown"
ok "stop (port + pid file released)"

grep -q 'shutdown signal received' "$PROXY_LOG" \
    || echo "  note: proxy.log has no 'shutdown signal received' line (log flush timing)" >&2
ok "service-smoke complete"

echo "STEP ok: service-smoke"
