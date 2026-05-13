#!/usr/bin/env bash
# Copyright (c) 2026 Softside Tech Pty Ltd. All rights reserved.
# SPDX-License-Identifier: LicenseRef-Proprietary
#
# install-appear-x-gateway.sh — single-shot installer for
# bilbycast-appear-x-api-gateway. Mirrors the structure of the edge
# installer (`bilbycast-edge/packaging/install-edge.sh`) so operators
# who run both nodes have the same install / upgrade muscle memory.
#
# Operator usage on a fresh node (one sidecar process per Appear X chassis):
#
#   curl -fsSL https://github.com/Bilbycast/bilbycast-appear-x-api-gateway/releases/latest/download/install-appear-x-gateway.sh \\
#     | sudo bash -s -- \\
#         --manager wss://manager.example.com:8443/ws/node \\
#         --registration-token <token-from-manager-ui> \\
#         --appear-x-address 192.168.1.100 \\
#         --appear-x-username admin \\
#         --appear-x-password <chassis-password> \\
#         [--channel stable|nightly|beta]
#
# What the script does:
#   1. Detects the host arch (x86_64-linux / aarch64-linux).
#   2. Downloads `manifest.json` + `manifest.sig.bundle` from the
#      configured channel's GitHub release.
#   3. Verifies the Sigstore signature with cosign (installs cosign if
#      missing, with its own checksum verified against the upstream
#      release page). The verify pins the same identity allowlist the
#      gateway enforces — `Bilbycast/bilbycast-appear-x-api-gateway` repo,
#      the nightly-release workflow path, and the `refs/tags/v*` ref.
#   4. Reads the matching artefact's SHA-256 from the verified manifest,
#      downloads the tarball, verifies the hash.
#   5. Creates the `bilbycast-gateway` system user/group via systemd-sysusers
#      or useradd. Distinct from edge's `bilbycast` user so the two services
#      can coexist on the same host with separate filesystem permissions.
#   6. Lays out `/opt/bilbycast/appear-x-gateway/{versions/<v>/, current →
#      versions/<v>, state.json, credentials.json}`.
#   7. Writes the initial `config.toml` with the manager URL +
#      registration token + chassis credentials.
#   8. Installs the systemd unit, runs `systemctl daemon-reload` and
#      `systemctl enable --now`.
#   9. Polls `systemctl is-active` for ~30 s waiting for the service
#      to settle.

set -euo pipefail

# ── Defaults ──────────────────────────────────────────────────────────
RELEASE_REPO="${RELEASE_REPO:-Bilbycast/bilbycast-appear-x-api-gateway}"
INSTALL_ROOT="${INSTALL_ROOT:-/opt/bilbycast/appear-x-gateway}"
DATA_ROOT="${DATA_ROOT:-/var/lib/bilbycast/appear-x-gateway}"
CONFIG_DIR="${CONFIG_DIR:-/etc/bilbycast}"
SYSTEMD_UNIT_DIR="${SYSTEMD_UNIT_DIR:-/etc/systemd/system}"
COSIGN_VERSION="${COSIGN_VERSION:-v2.4.1}"

CHANNEL="stable"
MANAGER_URL=""
REGISTRATION_TOKEN=""
APPEAR_X_ADDRESS=""
APPEAR_X_USERNAME=""
APPEAR_X_PASSWORD=""
UPGRADE_INSTALLER=0

# ── Argument parsing ──────────────────────────────────────────────────
usage() {
    cat <<EOF
Usage: $0 --manager <wss://...> --registration-token <token> \\
          --appear-x-address <ip> --appear-x-username <user> --appear-x-password <pass> \\
          [options]

Options:
  --manager <url>              Manager WebSocket URL (must be wss://)
  --registration-token <tok>   One-shot registration token from manager UI
  --appear-x-address <ip>      IP / hostname of the Appear X chassis
  --appear-x-username <user>   JSON-RPC login username (typically "admin")
  --appear-x-password <pass>   JSON-RPC login password
  --channel <name>             Release channel (stable | nightly | beta), default stable
  --upgrade-installer          Refresh service unit + install script,
                               leave config and versions/ untouched
  -h, --help                   Show this message
EOF
}

while [[ $# -gt 0 ]]; do
    case "$1" in
        --manager) MANAGER_URL="$2"; shift 2;;
        --registration-token) REGISTRATION_TOKEN="$2"; shift 2;;
        --appear-x-address) APPEAR_X_ADDRESS="$2"; shift 2;;
        --appear-x-username) APPEAR_X_USERNAME="$2"; shift 2;;
        --appear-x-password) APPEAR_X_PASSWORD="$2"; shift 2;;
        --channel) CHANNEL="$2"; shift 2;;
        --upgrade-installer) UPGRADE_INSTALLER=1; shift;;
        -h|--help) usage; exit 0;;
        *) echo "Unknown argument: $1" >&2; usage; exit 1;;
    esac
done

# ── Pre-flight checks ─────────────────────────────────────────────────
if [[ "$(id -u)" -ne 0 ]]; then
    echo "install-appear-x-gateway.sh must run as root (sudo)." >&2
    exit 1
fi

if [[ "${UPGRADE_INSTALLER}" -eq 0 ]]; then
    if [[ -z "${MANAGER_URL}" || -z "${REGISTRATION_TOKEN}" ]]; then
        echo "--manager and --registration-token are required for fresh installs." >&2
        usage
        exit 1
    fi
    if [[ -z "${APPEAR_X_ADDRESS}" || -z "${APPEAR_X_USERNAME}" || -z "${APPEAR_X_PASSWORD}" ]]; then
        echo "--appear-x-address, --appear-x-username, and --appear-x-password are required for fresh installs." >&2
        usage
        exit 1
    fi
    if [[ "${MANAGER_URL}" != wss://* ]]; then
        echo "--manager URL must use wss:// (TLS required); got: ${MANAGER_URL}" >&2
        exit 1
    fi
fi

if [[ ! "${CHANNEL}" =~ ^(stable|nightly|beta)$ ]]; then
    echo "Channel must be stable | nightly | beta; got: ${CHANNEL}" >&2
    exit 1
fi

# Detect arch.
case "$(uname -m)-$(uname -s)" in
    x86_64-Linux)   ARCH="x86_64-linux";;
    aarch64-Linux)  ARCH="aarch64-linux";;
    *)
        echo "Unsupported host: $(uname -m) on $(uname -s)" >&2
        echo "bilbycast-appear-x-api-gateway releases are published for x86_64-linux and aarch64-linux." >&2
        exit 1
        ;;
esac

VARIANT="default"

echo "── bilbycast-appear-x-api-gateway installer ──"
echo "  Repo       : ${RELEASE_REPO}"
echo "  Channel    : ${CHANNEL}"
echo "  Arch       : ${ARCH}"
echo "  Install at : ${INSTALL_ROOT}"
echo

# ── Idempotency guard ─────────────────────────────────────────────────
if [[ -e "${INSTALL_ROOT}/current" && "${UPGRADE_INSTALLER}" -eq 0 ]]; then
    echo "Already installed at ${INSTALL_ROOT}/current → $(readlink -f "${INSTALL_ROOT}/current")."
    echo "Use --upgrade-installer to refresh the service unit + install script,"
    echo "or trigger an upgrade from the manager UI to advance the binary."
    exit 0
fi

# ── Tooling: jq + curl + cosign ───────────────────────────────────────
need_pkg() {
    local pkg="$1"
    if ! command -v "${pkg}" > /dev/null 2>&1; then
        echo "${pkg} is required but not installed. Install via your package manager." >&2
        exit 1
    fi
}
need_pkg curl
need_pkg jq
need_pkg sha256sum

ensure_cosign() {
    if command -v cosign > /dev/null 2>&1; then
        echo "Using existing cosign: $(command -v cosign)"
        return
    fi
    echo "Installing cosign ${COSIGN_VERSION} into /usr/local/bin/cosign…"
    local cosign_arch
    case "${ARCH}" in
        x86_64-linux)  cosign_arch="amd64";;
        aarch64-linux) cosign_arch="arm64";;
    esac
    local asset="cosign-linux-${cosign_arch}"
    local url="https://github.com/sigstore/cosign/releases/download/${COSIGN_VERSION}/${asset}"
    local checksum_url="https://github.com/sigstore/cosign/releases/download/${COSIGN_VERSION}/cosign_checksums.txt"
    curl -fsSL -o /tmp/cosign "${url}"
    local expected
    expected="$(curl -fsSL "${checksum_url}" | awk -v a="${asset}" '$2 == a {print $1}')"
    if [[ -z "${expected}" ]]; then
        echo "Could not fetch cosign checksum for ${asset} from ${checksum_url}" >&2
        exit 1
    fi
    local got
    got="$(sha256sum /tmp/cosign | awk '{print $1}')"
    if [[ "${got}" != "${expected}" ]]; then
        echo "cosign checksum mismatch: expected ${expected}, got ${got}" >&2
        exit 1
    fi
    install -m 0755 /tmp/cosign /usr/local/bin/cosign
    rm /tmp/cosign
    echo "cosign installed."
}
ensure_cosign

# ── Resolve the latest release for the chosen channel ─────────────────
RELEASE_BASE="https://github.com/${RELEASE_REPO}/releases/latest/download"
WORK_DIR="$(mktemp -d)"
trap 'rm -rf "${WORK_DIR}"' EXIT
cd "${WORK_DIR}"

echo "Downloading manifest.json + manifest.sig.bundle from ${RELEASE_BASE}…"
curl -fsSL -o manifest.json        "${RELEASE_BASE}/manifest.json"
curl -fsSL -o manifest.sig.bundle  "${RELEASE_BASE}/manifest.sig.bundle"

echo "Verifying Sigstore signature against ALLOWED_SIGNERS allowlist…"
COSIGN_EXPERIMENTAL=1 cosign verify-blob \
    --bundle manifest.sig.bundle \
    --certificate-identity-regexp "https://github\\.com/${RELEASE_REPO//\//\\/}/\\.github/workflows/nightly-release\\.yml@refs/tags/v.*" \
    --certificate-oidc-issuer https://token.actions.githubusercontent.com \
    manifest.json

VERSION="$(jq -r '.version' manifest.json)"
CHANNEL_IN_MANIFEST="$(jq -r '.channel' manifest.json)"

if [[ "${CHANNEL_IN_MANIFEST}" != "${CHANNEL}" ]]; then
    echo "Manifest channel mismatch: requested ${CHANNEL}, got ${CHANNEL_IN_MANIFEST}." >&2
    echo "(Today every release targets the 'stable' channel; nightly / beta require dedicated workflows.)" >&2
    exit 1
fi

ARTEFACT_URL="$(jq -r --arg arch "${ARCH}" --arg variant "${VARIANT}" \
    '.artefacts[] | select(.arch == $arch and .variant == $variant) | .url' manifest.json)"
ARTEFACT_SHA256="$(jq -r --arg arch "${ARCH}" --arg variant "${VARIANT}" \
    '.artefacts[] | select(.arch == $arch and .variant == $variant) | .sha256' manifest.json)"

if [[ -z "${ARTEFACT_URL}" || "${ARTEFACT_URL}" == "null" ]]; then
    echo "No artefact for arch=${ARCH} variant=${VARIANT} in manifest." >&2
    echo "Available combinations:" >&2
    jq -r '.artefacts[] | "  \(.arch) / \(.variant)"' manifest.json >&2
    exit 1
fi

echo "Downloading ${ARTEFACT_URL}…"
curl -fsSL -o release.tar.gz "${ARTEFACT_URL}"
got_sha="$(sha256sum release.tar.gz | awk '{print $1}')"
if [[ "${got_sha}" != "${ARTEFACT_SHA256}" ]]; then
    echo "Tarball checksum mismatch: expected ${ARTEFACT_SHA256}, got ${got_sha}" >&2
    exit 1
fi
echo "Tarball SHA-256 matches manifest."

# ── Lay out /opt/bilbycast/appear-x-gateway/ ──────────────────────────
mkdir -p "${INSTALL_ROOT}/versions"
mkdir -p "${DATA_ROOT}"
mkdir -p "${CONFIG_DIR}"

VERSION_DIR="${INSTALL_ROOT}/versions/${VERSION}"
if [[ ! -e "${VERSION_DIR}/bilbycast-appear-x-api-gateway" || "${UPGRADE_INSTALLER}" -eq 0 ]]; then
    rm -rf "${VERSION_DIR}.partial"
    mkdir "${VERSION_DIR}.partial"
    tar -xzf release.tar.gz -C "${VERSION_DIR}.partial"
    # Hoist the binary if it's in a nested dir (the release packaging
    # uses `bilbycast-appear-x-api-gateway-<version>-<arch>/bilbycast-appear-x-api-gateway`).
    nested="$(find "${VERSION_DIR}.partial" -maxdepth 2 -name bilbycast-appear-x-api-gateway -type f | head -1)"
    if [[ -z "${nested}" ]]; then
        echo "Tarball did not contain a bilbycast-appear-x-api-gateway binary." >&2
        exit 1
    fi
    if [[ "${nested}" != "${VERSION_DIR}.partial/bilbycast-appear-x-api-gateway" ]]; then
        mv "${nested%/*}"/* "${VERSION_DIR}.partial/"
        rmdir "${nested%/*}" 2>/dev/null || true
    fi
    chmod 0755 "${VERSION_DIR}.partial/bilbycast-appear-x-api-gateway"
    rm -rf "${VERSION_DIR}"
    mv "${VERSION_DIR}.partial" "${VERSION_DIR}"
fi

# Atomic symlink swap.
ln -sfn "${VERSION_DIR}" "${INSTALL_ROOT}/current.tmp"
mv -Tf "${INSTALL_ROOT}/current.tmp" "${INSTALL_ROOT}/current"

# ── Create system user + group ─────────────────────────────────────────
if ! id -u bilbycast-gateway > /dev/null 2>&1; then
    if command -v systemd-sysusers > /dev/null 2>&1; then
        # /etc/sysusers.d/ doesn't exist on minimal Ubuntu / Debian images
        # by default, even when systemd-sysusers is present. Pre-create it.
        mkdir -p /etc/sysusers.d
        cat > /etc/sysusers.d/bilbycast-gateway.conf <<'EOF'
u bilbycast-gateway - "bilbycast gateway sidecar service account" /var/lib/bilbycast/appear-x-gateway /usr/sbin/nologin
EOF
        systemd-sysusers
    else
        useradd --system --home /var/lib/bilbycast/appear-x-gateway --shell /usr/sbin/nologin bilbycast-gateway
    fi
fi

chown -R bilbycast-gateway:bilbycast-gateway "${INSTALL_ROOT}" "${DATA_ROOT}"

# ── Initial config + credentials ──────────────────────────────────────
CONFIG_FILE="${INSTALL_ROOT}/config.toml"
CREDS_FILE="${INSTALL_ROOT}/credentials.json"

if [[ "${UPGRADE_INSTALLER}" -eq 0 || ! -f "${CONFIG_FILE}" ]]; then
    cat > "${CONFIG_FILE}.tmp" <<EOF
# bilbycast-appear-x-api-gateway configuration
# Generated by install-appear-x-gateway.sh on $(date -u +%Y-%m-%dT%H:%M:%SZ)

[manager]
urls = ["${MANAGER_URL}"]
registration_token = "${REGISTRATION_TOKEN}"
credentials_file = "${INSTALL_ROOT}/credentials.json"
accept_self_signed_cert = false

[appear_x]
address = "${APPEAR_X_ADDRESS}"
username = "${APPEAR_X_USERNAME}"
password = "${APPEAR_X_PASSWORD}"
accept_self_signed_cert = true

[polling]
alarms_interval_secs = 10
chassis_interval_secs = 30
inputs_interval_secs = 15
outputs_interval_secs = 15
cards_interval_secs = 30
alarms_mmi_version = "2.8"
chassis_mmi_version = "4.1"
cards_mmi_version = "2.8"

# Remote upgrade — the gateway accepts \`upgrade_binary\` WS commands from
# the manager and stages a Sigstore-verified release tarball into
# \`versions/<v>/\`, atomically swaps the \`current\` symlink, then exits
# for systemd respawn. The boot watchdog rolls back automatically on
# failure. Set \`enabled = false\` to opt out.
[upgrade]
enabled = true
allowed_channels = ["stable"]
install_root = "${INSTALL_ROOT}"
EOF
    mv -f "${CONFIG_FILE}.tmp" "${CONFIG_FILE}"
    chown bilbycast-gateway:bilbycast-gateway "${CONFIG_FILE}"
    chmod 0640 "${CONFIG_FILE}"
fi

if [[ ! -f "${CREDS_FILE}" ]]; then
    : > "${CREDS_FILE}"
    chown bilbycast-gateway:bilbycast-gateway "${CREDS_FILE}"
    chmod 0600 "${CREDS_FILE}"
fi

# ── Install systemd unit ──────────────────────────────────────────────
UNIT_DEST="${SYSTEMD_UNIT_DIR}/bilbycast-appear-x-gateway.service"
if [[ -f "${VERSION_DIR}/packaging/bilbycast-appear-x-gateway.service" ]]; then
    install -m 0644 "${VERSION_DIR}/packaging/bilbycast-appear-x-gateway.service" "${UNIT_DEST}"
else
    curl -fsSL -o "${UNIT_DEST}" \
        "https://github.com/${RELEASE_REPO}/releases/latest/download/bilbycast-appear-x-gateway.service"
fi

# Default env file. Only seed it if missing.
ENV_FILE="${CONFIG_DIR}/appear-x-gateway.env"
if [[ ! -f "${ENV_FILE}" ]]; then
    cat > "${ENV_FILE}" <<'EOF'
# bilbycast-appear-x-api-gateway runtime environment.
# Tunable via systemctl restart bilbycast-appear-x-gateway (no daemon-reload needed).
RUST_LOG=info,bilbycast_appear_x_api_gateway=info
EOF
    chmod 0640 "${ENV_FILE}"
fi

systemctl daemon-reload
systemctl enable --now bilbycast-appear-x-gateway

# ── Wait for first manager registration ────────────────────────────────
echo
echo "Waiting up to 60 s for bilbycast-appear-x-gateway to come up + register with the manager…"
for _ in $(seq 1 30); do
    if systemctl is-active --quiet bilbycast-appear-x-gateway; then
        echo "bilbycast-appear-x-gateway is up. Verify in the manager UI under /admin/nodes."
        echo
        echo "Logs: journalctl -u bilbycast-appear-x-gateway -f"
        echo "Config: ${CONFIG_FILE}"
        exit 0
    fi
    sleep 2
done

echo
echo "bilbycast-appear-x-gateway service didn't reach a healthy state in 60 s."
echo "Inspect:"
echo "  journalctl -u bilbycast-appear-x-gateway -e"
echo "  systemctl status bilbycast-appear-x-gateway"
exit 1
