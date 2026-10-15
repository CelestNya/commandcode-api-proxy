#!/bin/sh
# commandcode-proxy deb postinst: state dirs, env defaults, unit enable.
# `env` is operator-owned: never overwrite an existing file (debian/conffiles
# would handle this, but the env file ships as an example and is copied only
# on first install).

set -e

for d in /var/lib/commandcode-proxy /var/log/commandcode-proxy; do
    if [ ! -d "$d" ]; then
        mkdir -p "$d"
    fi
    chown commandcode-proxy:commandcode-proxy "$d"
    chmod 750 "$d"
done

if [ ! -e /etc/commandcode-proxy/env ]; then
    cp /etc/commandcode-proxy/env.example /etc/commandcode-proxy/env
    chown root:commandcode-proxy /etc/commandcode-proxy/env
    chmod 640 /etc/commandcode-proxy/env
fi

if command -v systemctl >/dev/null 2>&1; then
    systemctl daemon-reload >/dev/null 2>&1 || true
fi

# Never auto-start on install: the operator decides with `systemctl enable`.
exit 0
