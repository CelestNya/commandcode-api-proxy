#!/usr/bin/env bash
# Headless SNI smoke test: the contract the tray hands a desktop shell.
#
# Spawns a private session bus, starts the tray, and verifies over DBus —
# with no display, no Plasma, no X11 — everything a KDE panel does when it
# meets the icon:
#   1. a StatusNotifierItem bus name gets registered,
#   2. org.kde.StatusNotifierItem.Status is readable (Active/Passive),
#   3. the Menu property resolves and DBusMenu serves the menu layout,
#   4. activating 「启动代理」 over DBusMenu really spawns the proxy
#      (its /health starts answering on the production port).
# Rendering on a real Plasma panel still needs eyes; everything before that
# is machine-checked here, on every push.
#
# Usage: sni-smoke.sh <bin-dir>   (containing CCProxyTray and ccproxy)
#
# Requires: gdbus (libglib2.0-bin), dbus-run-session, curl. Expects
# XDG_RUNTIME_DIR to exist (it does on GitHub runners; locally
# `mkdir -p /run/user/$(id -u)`).

set -euo pipefail

fail() { echo "FAIL: $1" >&2; exit 1; }

PORT=8787

if [ "${1:-}" = "--session-inner" ]; then
    # Running inside dbus-run-session. $2 = tray binary, $3 = proxy binary.
    TRAY="$2"
    PROXY="$3"
    export CC_PROXY_BIN="$PROXY"

    "$TRAY" &
    TRAY_PID=$!
    trap 'kill "$TRAY_PID" 2>/dev/null || true' EXIT

    # 1) registration: a StatusNotifierItem bus name must appear.
    # `|| true` keeps set -e from killing the loop while the name has not
    # appeared yet (grep no-match inside $() counts as a failing pipeline).
    name=""
    for _ in $(seq 1 30); do
        name=$(gdbus call --session --dest org.freedesktop.DBus \
            --object-path /org/freedesktop/DBus \
            --method org.freedesktop.DBus.ListNames 2>/dev/null \
            | tr ',' '\n' \
            | grep -oE "org\.kde\.StatusNotifierItem-[0-9]+-[0-9]+" | head -1 || true)
        [ -n "$name" ] && break
        sleep 0.5
    done
    [ -n "$name" ] || fail "no StatusNotifierItem bus name registered"
    echo "OK: registered $name"

    # 2) Status property readable and sane.
    status=$(gdbus call --session --dest "$name" --object-path /StatusNotifierItem \
        --method org.freedesktop.DBus.Properties.Get org.kde.StatusNotifierItem Status) \
        || fail "Status property not readable"
    echo "$status" | grep -qE "Active|Passive" || fail "unexpected Status: $status"
    echo "OK: Status readable ($status)"

    # 3) menu layout served by DBusMenu, carrying the start item.
    layout=$(gdbus call --session --dest "$name" --object-path /MenuBar \
        --method com.canonical.dbusmenu.GetLayout 0 2 '[]') \
        || fail "GetLayout failed"
    echo "$layout" | grep -q "启动代理" || { echo "layout: $layout" >&2; fail "menu layout missing 启动代理"; }
    start_id=$(echo "$layout" | grep -oE "int32 [0-9]+, \{'[^}]*启动代理" \
        | grep -oE "[0-9]+" | head -1 || true)
    [ -n "$start_id" ] || { echo "layout: $layout" >&2; fail "could not find start item id"; }
    echo "OK: menu layout carries 启动代理 (id $start_id)"

    # 4) activation: clicking the item over DBusMenu must spawn the proxy.
    gdbus call --session --dest "$name" --object-path /MenuBar \
        --method com.canonical.dbusmenu.Event "$start_id" clicked '@v ""' 0 \
        >/dev/null || fail "Event(click) call failed"
    serving=""
    for _ in $(seq 1 30); do
        if curl -fsS --max-time 2 "http://127.0.0.1:$PORT/health" 2>/dev/null | grep -q '"status":"ok"'; then
            serving=1
            break
        fi
        sleep 0.5
    done
    [ -n "$serving" ] || fail "proxy not serving after menu activation"
    echo "OK: proxy serving on :$PORT after DBusMenu activation"

    echo "SNI_SMOKE_OK"
    exit 0
fi

# Outer invocation: set up the environment, then re-enter on a private bus.
BIN_DIR="${1:?usage: sni-smoke.sh <bin-dir>}"
TRAY="$BIN_DIR/CCProxyTray"
PROXY="$BIN_DIR/ccproxy"
[ -x "$TRAY" ] || fail "tray binary not found: $TRAY"
[ -x "$PROXY" ] || fail "proxy binary not found: $PROXY"
if [ -z "${XDG_RUNTIME_DIR:-}" ]; then
    XDG_RUNTIME_DIR="/tmp/ccproxy-sni-runtime"
    export XDG_RUNTIME_DIR
    mkdir -p "$XDG_RUNTIME_DIR"
    chmod 700 "$XDG_RUNTIME_DIR"
fi

dbus-run-session -- bash "$0" --session-inner "$TRAY" "$PROXY"
