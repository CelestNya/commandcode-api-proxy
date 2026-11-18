#!/usr/bin/env bash
# 真实 systemd 上的 deb 验收：安装 → 隔离端口启动 → 探活 → 停/启/停 → 禁用 → 卸载。
#
# 只跑在「完整 VM」上：脚本第一条断言就是 `test -d /run/systemd/system`。
# 容器里 /run/systemd/system 不存在，这一条会立刻失败——这正是我们要的，
# 因为 `systemd-analyze verify` 只能证明 unit 文法对，不能证明服务能起来。
#
# 用法（root 或 sudo 均可）：
#   DEB_DIR=/path/to/debs PORT=18789 EVIDENCE_DIR=/path/out \
#       bash packaging/ci/systemd-deb-acceptance.sh
#
# 环境变量：
#   DEB_DIR      包含 commandcode-proxy_*.deb 与 commandcode-proxy-tray_*.deb 的目录（必填）
#   PORT         隔离端口，默认 18789（绝不使用 8787）
#   EVIDENCE_DIR 证据输出目录，默认 $RUNNER_TEMP/systemd-deb-evidence
#
# 端口隔离靠 /etc/commandcode-proxy/env（unit 的 EnvironmentFile）注入 PORT，
# 因此这里同时验收了「运维可通过 env 文件改端口」这条文档承诺。

set -euo pipefail

DEB_DIR="${DEB_DIR:?DEB_DIR 必填（包含两个 deb 的目录）}"
PORT="${PORT:-18789}"
EVIDENCE_DIR="${EVIDENCE_DIR:-${RUNNER_TEMP:-/tmp}/systemd-deb-evidence}"

mkdir -p "$EVIDENCE_DIR"
EVIDENCE_LOG="$EVIDENCE_DIR/systemd-deb.log"
exec > >(tee -a "$EVIDENCE_LOG") 2>&1

SUDO=""
if [ "$(id -u)" -ne 0 ]; then
    SUDO="sudo"
fi

fail() {
    echo "STEP fail: $1" >&2
    shift
    for line in "$@"; do echo "  $line" >&2; done
    echo "--- journal (last 200) ---" >&2
    $SUDO journalctl -u commandcode-proxy --no-pager -n 200 2>&1 || true
    echo "--- env file ---" >&2
    $SUDO cat /etc/commandcode-proxy/env 2>&1 || true
    exit 1
}

ok() { echo "STEP ok: $1"; }

# 兜底：脚本用 set -e，任何未显式处理的非零退出都会直接终止，日志里只剩上一
# 个 STEP ok（首跑就是这样在 MainPID 段落静默死掉的）。ERR trap 把失败行号
# 打出来，保证「失败」永远带一个可定位的锚点。
trap 'rc=$?; echo "STEP fail: uncaught-error (rc=$rc at line ${BASH_LINENO[0]})" >&2' ERR

# ---- 0. 环境自证：这是不是完整 VM ----
# hosted runner 是 VM（PID 1 = systemd）；容器不是。没有这一条，后面所有
# systemctl 断言都会退化成「命令返回 0 但其实没启动任何东西」。
test -d /run/systemd/system \
    || fail "systemd-is-pid1" "no /run/systemd/system — this is a container, not a systemd VM"
test "$(ps -o comm= -p 1 | tr -d ' ')" = "systemd" \
    || fail "systemd-is-pid1" "PID 1 is '$(ps -o comm= -p 1)', not systemd"
# 机器刚启动时 is-system-running 会返回 startup/starting（单元还在拉起），
# hosted runner 的 job 常常落在这一段。这里允许有界等待，但必须收敛：
# 一直停在 starting 说明这台 VM 的系统状态本身异常，后续 start 的结论也不可信。
STATE=""
for ((i = 0; i < 90; i++)); do
    STATE="$(systemctl is-system-running 2>/dev/null || true)"
    case "$STATE" in
        running|degraded) break ;;
    esac
    sleep 1
done
case "$STATE" in
    running|degraded) ;;
    *) fail "systemd-is-pid1" "systemctl is-system-running stayed '$STATE' for 90s" ;;
esac
ok "systemd-is-pid1 ($(systemctl --version | head -1), state=$STATE)"

# ---- 1. 安装两个 deb ----
test -d "$DEB_DIR" || fail "deb-dir" "DEB_DIR=$DEB_DIR does not exist"
shopt -s nullglob
DEBS=("$DEB_DIR"/commandcode-proxy_*_amd64.deb "$DEB_DIR"/commandcode-proxy-tray_*_amd64.deb)
test "${#DEBS[@]}" -eq 2 \
    || fail "deb-dir" "expected proxy+tray deb, found ${#DEBS[@]} files in $DEB_DIR"
# apt 而不是 dpkg -i：apt 会一并解决 Depends（dpkg-shlibdeps 推出的库版本）。
$SUDO apt-get install -y "${DEBS[@]}"

# ---- 2. 安装后的静态契约 ----
getent passwd commandcode-proxy >/dev/null \
    || fail "service-account" "user commandcode-proxy missing after install"
getent group commandcode-proxy >/dev/null \
    || fail "service-account" "group commandcode-proxy missing after install"
ok "service-account"

test -f /lib/systemd/system/commandcode-proxy.service \
    || fail "unit-installed" "unit file missing"
test -f /usr/bin/commandcode-proxy || fail "unit-installed" "proxy binary missing"
test -d /var/lib/commandcode-proxy || fail "state-dirs" "/var/lib/commandcode-proxy missing"
test -d /var/log/commandcode-proxy || fail "state-dirs" "/var/log/commandcode-proxy missing"
stat -c '%U:%G %a' /var/lib/commandcode-proxy | grep -q 'commandcode-proxy:commandcode-proxy 750' \
    || fail "state-dirs" "state dir owner/mode wrong: $(stat -c '%U:%G %a' /var/lib/commandcode-proxy)"
ok "unit-installed + state-dirs"

# postinst 的契约：安装不自动启动，由运维显式 enable/start。
if systemctl is-active --quiet commandcode-proxy; then
    fail "no-autostart-on-install" "service is active right after install; postinst contract says it must not autostart"
fi
ok "no-autostart-on-install"

# ---- 3. 隔离端口：写 EnvironmentFile ----
test -f /etc/commandcode-proxy/env \
    || fail "env-file" "/etc/commandcode-proxy/env missing (postinst should copy env.example)"
if $SUDO grep -q '^PORT=' /etc/commandcode-proxy/env; then
    $SUDO sed -i "s/^PORT=.*/PORT=$PORT/" /etc/commandcode-proxy/env
else
    echo "PORT=$PORT" | $SUDO tee -a /etc/commandcode-proxy/env >/dev/null
fi
$SUDO grep -qx "PORT=$PORT" /etc/commandcode-proxy/env \
    || fail "env-file" "PORT=$PORT not present in env file"
ok "env-file-port=$PORT"

# ---- 4. enable：必须真的写进 multi-user.target.wants ----
$SUDO systemctl enable commandcode-proxy >/dev/null
test -L /etc/systemd/system/multi-user.target.wants/commandcode-proxy.service \
    || fail "enable" "no symlink in multi-user.target.wants after enable"
[ "$(systemctl is-enabled commandcode-proxy)" = "enabled" ] \
    || fail "enable" "is-enabled=$(systemctl is-enabled commandcode-proxy)"
ok "enable"

wait_http() {
    # $1=url $2=期望状态码 $3=超时秒数 $4=步骤名
    local url="$1" want="$2" timeout="$3" step="$4" i code
    for ((i = 0; i < timeout; i++)); do
        code="$(curl -fsS -o /dev/null -m 2 -w '%{http_code}' "$url" 2>/dev/null || true)"
        if [ "$code" = "$want" ]; then
            echo "  $url -> $code after ${i}s"
            return 0
        fi
        sleep 1
    done
    fail "$step" "$url did not return $want within ${timeout}s (last='${code:-<no response>}')"
}

# ---- 5. start 并验证「systemd 真的在管这个进程」 ----
$SUDO systemctl start commandcode-proxy
wait_http "http://127.0.0.1:$PORT/health" 200 45 "start-health"
wait_http "http://127.0.0.1:$PORT/webui/" 200 15 "start-webui"
wait_http "http://127.0.0.1:$PORT/webui/css/tokens.css" 200 15 "start-static"

MAIN_PID="$($SUDO systemctl show -p MainPID --value commandcode-proxy)" \
    || fail "systemd-owns-process" "systemctl show -p MainPID exited non-zero"
echo "  MainPID=$MAIN_PID"
test "$MAIN_PID" != "0" || fail "systemd-owns-process" "MainPID=0"
test -d "/proc/$MAIN_PID" || fail "systemd-owns-process" "MainPID=$MAIN_PID is not alive"
# /proc/<pid>/exe 属于服务账号，读它需要 root：普通用户 readlink 会得到
# EACCES，从而把一次成功的启动误判成「exe 不是打包二进制」（首跑即此）。
EXE="$($SUDO readlink -f "/proc/$MAIN_PID/exe" 2>/dev/null || echo "<none>")"
test "$EXE" = "/usr/bin/commandcode-proxy" \
    || fail "systemd-owns-process" "MainPID exe=$EXE, expected /usr/bin/commandcode-proxy"
RUN_AS="$(ps -o user= -p "$MAIN_PID" 2>/dev/null | tr -d ' ' || true)"
test "$RUN_AS" = "commandcode-proxy" \
    || fail "systemd-owns-process" "service runs as '$RUN_AS', unit says commandcode-proxy"
ok "systemd-owns-process (pid=$MAIN_PID exe=$EXE user=$RUN_AS)"

# 端口必须落在隔离 PORT 上（而不是默认 8787）。
(ss -ltn "sport = :$PORT" 2>/dev/null | grep -q "127.0.0.1:$PORT") \
    || fail "isolated-port" "nothing listening on 127.0.0.1:$PORT"
ok "isolated-port=$PORT"

# ---- 6. journal 必须来自这个 unit 的真实运行 ----
$SUDO journalctl -u commandcode-proxy --no-pager --since "-5min" > "$EVIDENCE_DIR/journal-start.log"
grep -q 'cc-proxy listening on' "$EVIDENCE_DIR/journal-start.log" \
    || fail "journal" "journal has no 'cc-proxy listening on' line"
ok "journal (listening line present)"

# 进程自己写的日志也必须落在 unit 声明的 CC_LOG_DIR（不是二进制旁边的 logs/）。
LOG_FILE="$($SUDO find /var/log/commandcode-proxy -maxdepth 2 -name 'proxy.log' -type f | head -1)"
test -n "$LOG_FILE" || fail "log-dir" "no proxy.log under /var/log/commandcode-proxy"
test -s "$LOG_FILE" || fail "log-dir" "$LOG_FILE is empty"
ok "log-dir ($LOG_FILE)"

# ---- 7. stop → 端口必须真的释放 ----
$SUDO systemctl stop commandcode-proxy
[ "$(systemctl is-active commandcode-proxy)" != "active" ] \
    || fail "stop" "still active after stop"
for ((i = 0; i < 20; i++)); do
    if ! ss -ltn "sport = :$PORT" 2>/dev/null | grep -q "127.0.0.1:$PORT"; then
        break
    fi
    sleep 1
done
ss -ltn "sport = :$PORT" 2>/dev/null | grep -q "127.0.0.1:$PORT" \
    && fail "stop" "port $PORT still listening 20s after stop"
ok "stop (port released)"

# ---- 8. start 再 stop：证明可重复启停，不是一次性的 ----
$SUDO systemctl start commandcode-proxy
wait_http "http://127.0.0.1:$PORT/health" 200 45 "restart-health"
$SUDO systemctl stop commandcode-proxy
[ "$(systemctl is-active commandcode-proxy)" != "active" ] \
    || fail "restart-cycle" "still active after second stop"
ok "restart-cycle"

# ---- 9. disable ----
$SUDO systemctl disable commandcode-proxy >/dev/null
[ "$(systemctl is-enabled commandcode-proxy)" != "enabled" ] \
    || fail "disable" "still enabled"
test ! -L /etc/systemd/system/multi-user.target.wants/commandcode-proxy.service \
    || fail "disable" "wants symlink still present"
ok "disable"

# ---- 10. purge：postrm 必须清干净状态目录与 unit ----
$SUDO apt-get purge -y commandcode-proxy commandcode-proxy-tray
test ! -d /var/lib/commandcode-proxy || fail "purge" "/var/lib/commandcode-proxy survived purge"
test ! -d /etc/commandcode-proxy || fail "purge" "/etc/commandcode-proxy survived purge"
test ! -f /lib/systemd/system/commandcode-proxy.service || fail "purge" "unit file survived purge"
ok "purge"

$SUDO journalctl -u commandcode-proxy --no-pager --since "-30min" \
    > "$EVIDENCE_DIR/journal-full.log" 2>/dev/null || true

echo "STEP ok: systemd-deb-acceptance complete"
