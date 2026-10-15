#!/bin/sh
# commandcode-proxy deb preinst: create the unprivileged service account.
# Runs before files are unpacked, so the unit's User=/Group= always exist.

set -e

if ! getent group commandcode-proxy >/dev/null 2>&1; then
    groupadd --system commandcode-proxy
fi

if ! getent passwd commandcode-proxy >/dev/null 2>&1; then
    # System account, no login shell, no home; state lives under /var/lib.
    useradd --system --gid commandcode-proxy --no-create-home \
        --shell /usr/sbin/nologin commandcode-proxy
fi

exit 0
