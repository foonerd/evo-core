#!/usr/bin/env bash
#
# setup-smb-stage.sh — opt-in operator helper that exposes the
# framework's plugin stage directory as a Samba share so
# bundles can be drag-dropped from any LAN machine without SSH.
#
# This script is OPERATOR-DOMAIN, not framework code. The
# framework itself has no knowledge of Samba (or any protocol —
# per CONCEPT.md "The steward has no knowledge of audio,
# networking, any service, any protocol"). This helper lives in
# scripts/install/ alongside prototype-install.sh because the
# validation distribution's prototype hardware is one place
# operators reasonably want SMB-drop convenience; the helper
# itself is a one-off convenience, not part of the framework's
# admission gate.
#
# What this script does:
#
#   1. Installs samba (if not already installed).
#   2. Drops a share definition at
#      /etc/samba/smb.conf.d/evo-plugins-stage.conf exposing
#      /var/lib/evo/plugins/stage as the share named
#      `evo-plugins-stage`. The share is read-write for an
#      operator-supplied SMB user; guest access is denied.
#   3. Adds the operator-supplied user to Samba (smbpasswd -a)
#      with a prompted password.
#   4. Restarts smbd so the share takes effect.
#
# Path-2 invariant (per the four-path admission ADR): SMB does
# not bypass signature verification. Bundles dropped via SMB
# land in the stage directory and go through the same admission
# gate as filesystem-dropped bundles. SMB is a convenience for
# operators who prefer drag-drop over scp; it does not elevate
# trust.
#
# Usage:
#
#   sudo scripts/install/setup-smb-stage.sh <smb-user>
#
#   <smb-user> is the LAN-side user for the share. The script
#   prompts for the user's SMB password (smbpasswd captures
#   it interactively). The user MUST already exist as a Linux
#   user on this device (Samba authenticates against /etc/passwd
#   for share access). Create the user first with `useradd`
#   if it does not already exist.
#
# Removal:
#
#   sudo rm /etc/samba/smb.conf.d/evo-plugins-stage.conf
#   sudo systemctl restart smbd
#   sudo smbpasswd -x <smb-user>           # if no longer needed elsewhere
#
# Defaults:
#   - Share name:    evo-plugins-stage
#   - Share path:    /var/lib/evo/plugins/stage
#   - Stewardship:   supplied as positional arguments 2 + 3 (the
#                    steward service user + group the distribution
#                    chose at packaging time).
#   - Read-only:     no  (operators drop bundles in)
#   - Browseable:    yes
#   - Guest:         no  (auth required)
#
# A distribution adjusts the steward-user / steward-group arguments
# to match its own service identity.

set -euo pipefail

if [[ "${EUID}" -ne 0 ]]; then
    echo "FAIL: setup-smb-stage.sh must run as root (try: sudo $0 $*)" >&2
    exit 1
fi

if [[ $# -ne 3 ]]; then
    echo "Usage: $0 <smb-user> <steward-user> <steward-group>" >&2
    echo "       The smb-user must already exist as a Linux user (useradd)." >&2
    echo "       The steward-user / -group are the service identity the" >&2
    echo "       distribution configured for the running steward." >&2
    exit 2
fi

SMB_USER="$1"
STEWARD_USER="$2"
STEWARD_GROUP="$3"
SHARE_NAME="evo-plugins-stage"
SHARE_PATH="/var/lib/evo/plugins/stage"
SHARE_CONF="/etc/samba/smb.conf.d/${SHARE_NAME}.conf"

echo "=== setup-smb-stage.sh ==="
echo "Share:    ${SHARE_NAME}"
echo "Path:     ${SHARE_PATH}"
echo "User:     ${SMB_USER}"
echo "Steward:  ${STEWARD_USER}:${STEWARD_GROUP}"
echo

# Sanity: the SMB user must exist as a Linux user (Samba shares
# authenticate against /etc/passwd; smbpasswd just adds the SMB
# password layer).
if ! getent passwd "${SMB_USER}" > /dev/null 2>&1; then
    echo "FAIL: Linux user '${SMB_USER}' does not exist on this device." >&2
    echo "Create it first: sudo useradd -M -s /usr/sbin/nologin ${SMB_USER}" >&2
    exit 3
fi

# Sanity: the steward user must own the stage directory so the
# steward can read incoming bundles. The deploy-showcase script's
# --chown takes care of the steward-side ownership; this script
# just verifies the directory exists.
if [[ ! -d "${SHARE_PATH}" ]]; then
    echo "FAIL: stage directory ${SHARE_PATH} does not exist." >&2
    echo "Run prototype-install.sh / prototype-deploy-showcase.sh first." >&2
    exit 4
fi

# Phase 1: install samba.
echo "[1/4] installing samba (apt) ..."
DEBIAN_FRONTEND=noninteractive apt-get install -y -qq samba >/dev/null
echo "  ok"

# Phase 2: stage the share definition.
echo "[2/4] writing share definition to ${SHARE_CONF} ..."
mkdir -p /etc/samba/smb.conf.d
cat > "${SHARE_CONF}" <<EOF
# evo plugin stage drop share. Operator-managed; framework has
# no knowledge of Samba. Bundles dropped here are picked up by
# the framework's stage watcher and admitted through the same
# admission gate as filesystem-dropped bundles. SMB is a
# convenience, not a trust elevation.
[${SHARE_NAME}]
   path = ${SHARE_PATH}
   browseable = yes
   read only = no
   guest ok = no
   valid users = ${SMB_USER}
   force user = ${STEWARD_USER}
   force group = ${STEWARD_GROUP}
   create mask = 0640
   directory mask = 0750
   comment = Drop signed plugin bundles here for the evo steward to admit.
EOF

# Make sure smb.conf includes the conf.d directory. Most
# packaging already does; idempotent guard for the cases that
# don't.
if ! grep -q '^[[:space:]]*include[[:space:]]*=[[:space:]]*/etc/samba/smb.conf.d/' /etc/samba/smb.conf 2>/dev/null \
   && ! grep -q '^[[:space:]]*include[[:space:]]*=[[:space:]]*/etc/samba/smb.conf.d/\\*\\.conf' /etc/samba/smb.conf 2>/dev/null; then
    cat >> /etc/samba/smb.conf <<'EOF'

# evo: include operator share drop-ins (added by setup-smb-stage.sh)
include = /etc/samba/smb.conf.d/evo-plugins-stage.conf
EOF
fi
echo "  ok"

# Phase 3: SMB password for the share user.
echo "[3/4] setting SMB password for user '${SMB_USER}' (interactive) ..."
smbpasswd -a "${SMB_USER}"
echo "  ok"

# Phase 4: restart smbd to pick up the new share.
echo "[4/4] restarting smbd ..."
systemctl restart smbd
echo "  ok"
echo
echo "=== setup-smb-stage.sh complete ==="
echo
echo "Share is now reachable at:"
echo "  smb://$(hostname).local/${SHARE_NAME}"
echo "  smb://$(hostname -I | awk '{print $1}')/${SHARE_NAME}"
echo
echo "Operators drag-drop bundles into the share; the framework's"
echo "stage watcher picks them up on the next poll tick (default"
echo "1 second) and admits through the same admission gate as"
echo "filesystem-dropped bundles. SMB does not bypass signature"
echo "verification; unsigned or invalid bundles land in"
echo "  ${SHARE_PATH}/rejected/<reason-slug>/"
