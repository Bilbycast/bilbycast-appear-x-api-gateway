#!/usr/bin/env bash
# Copyright (c) 2026 Softside Tech Pty Ltd. All rights reserved.
# SPDX-License-Identifier: LicenseRef-Proprietary
#
# uninstall-appear-x-gateway.sh — fully removes the bilbycast-appear-x-api-gateway
# install. Stops the systemd service, removes the unit file, deletes the install
# root, and (optionally) drops the system user. Leaves /etc/bilbycast/ in place
# unless --purge-config is given so an operator can keep their TOML config and
# reinstall later without re-typing chassis credentials.

set -euo pipefail

INSTALL_ROOT="${INSTALL_ROOT:-/opt/bilbycast/appear-x-gateway}"
DATA_ROOT="${DATA_ROOT:-/var/lib/bilbycast/appear-x-gateway}"
CONFIG_DIR="${CONFIG_DIR:-/etc/bilbycast}"
SYSTEMD_UNIT_DIR="${SYSTEMD_UNIT_DIR:-/etc/systemd/system}"

PURGE_USER=0
PURGE_CONFIG=0

usage() {
    cat <<EOF
Usage: $0 [--purge-user] [--purge-config]

Options:
  --purge-user    Also delete the bilbycast-gateway system user
                  (skipped if other gateways still use it)
  --purge-config  Also delete /etc/bilbycast/appear-x-gateway.env and
                  ${INSTALL_ROOT}/config.toml
  -h, --help      Show this message
EOF
}

while [[ $# -gt 0 ]]; do
    case "$1" in
        --purge-user) PURGE_USER=1; shift;;
        --purge-config) PURGE_CONFIG=1; shift;;
        -h|--help) usage; exit 0;;
        *) echo "Unknown argument: $1" >&2; usage; exit 1;;
    esac
done

if [[ "$(id -u)" -ne 0 ]]; then
    echo "uninstall-appear-x-gateway.sh must run as root (sudo)." >&2
    exit 1
fi

echo "Stopping bilbycast-appear-x-gateway service…"
systemctl disable --now bilbycast-appear-x-gateway 2>/dev/null || true
rm -f "${SYSTEMD_UNIT_DIR}/bilbycast-appear-x-gateway.service"
systemctl daemon-reload

echo "Removing install root: ${INSTALL_ROOT}"
rm -rf "${INSTALL_ROOT}"

echo "Removing data root: ${DATA_ROOT}"
rm -rf "${DATA_ROOT}"

if [[ "${PURGE_CONFIG}" -eq 1 ]]; then
    echo "Removing config: ${CONFIG_DIR}/appear-x-gateway.env"
    rm -f "${CONFIG_DIR}/appear-x-gateway.env"
fi

if [[ "${PURGE_USER}" -eq 1 ]]; then
    # Don't delete the user if any other gateway-* install root is still on disk.
    if compgen -G "/opt/bilbycast/*-gateway" > /dev/null; then
        echo "Skipping user deletion — other gateway installs still present:"
        ls -d /opt/bilbycast/*-gateway 2>/dev/null
    elif id -u bilbycast-gateway > /dev/null 2>&1; then
        echo "Removing system user bilbycast-gateway…"
        userdel bilbycast-gateway || true
        rm -f /etc/sysusers.d/bilbycast-gateway.conf
    fi
fi

echo "Done."
