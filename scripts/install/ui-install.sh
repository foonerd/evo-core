#!/usr/bin/env bash
#
# ui-install.sh - idempotent base install of the UI runtime footprint on a target.
#
# This script is the canonical install record for the UI runtime. It captures
# the one-time-per-target footprint that every UI-runtime-capable device must
# have before ui-deploy-release.sh can ship a release into it. The footprint
# is intentionally narrow — directory tree + systemd unit + daemon-reload —
# so subsequent per-release iterations carry no install drift.
#
# Pre-condition: prototype-install.sh has run against the target. That script
# establishes the <service-user> user, NOPASSWD sudo, the /opt/evo base directory,
# and the framework's filesystem layout. This script does NOT recreate any of
# that; it fails fast if those preconditions are missing.
#
# What this script installs:
#
#   - /opt/evo/ui                            (root; mode 0755; <service-user>:<service-user>)
#   - /opt/evo/ui/releases                   (release payloads land here; mode 0755)
#   - /opt/evo/ui/data                       (authoritative settings.json + .bak)
#   - /opt/evo/ui/logs                       (runtime.log per the systemd unit)
#   - /etc/systemd/system/evo-ui.service     (sourced from evo-ui-eng working tree)
#   - systemd daemon-reload (so the unit is visible to systemctl)
#
# What this script does NOT do (handled by ui-deploy-release.sh):
#
#   - Ship the evo-ui-runtime binary to /opt/evo/bin/.
#   - Push a UI release into /opt/evo/ui/releases/<timestamp>/.
#   - Create the /opt/evo/ui/current symlink.
#   - Enable or start evo-ui.service. The service has no binary to exec until
#     a release is deployed, so enabling here would create a failing service.
#
# Idempotency: re-running this script on a target where the install is already
# in place leaves the same end state. install -d creates directories without
# erroring on existing ones; sudo install -m overwrites the unit file safely.
#
# Cross-repo dependency: this script lives in evo-core-eng but references the
# systemd unit in evo-ui-eng. Default resolution expects sibling repos under
# the same project root (../evo-ui-eng relative to evo-core-eng). Override
# with UI_REPO_ROOT=<path> when the repos are checked out elsewhere.
#
# Usage:
#
#   scripts/install/ui-install.sh <TARGET_HOST> <TARGET_USER>
#
#   Both arguments are required. No baked-in default so a stale value
#   never silently reaches an unintended target.
#
#   Requires: prototype-install.sh has already run on TARGET_HOST.

set -euo pipefail

if [[ $# -lt 2 ]]; then
    echo "usage: $0 <TARGET_HOST> <TARGET_USER>" >&2
    exit 1
fi
TARGET_HOST="$1"
TARGET_USER="$2"
SSH_TARGET="${TARGET_USER}@${TARGET_HOST}"

# Repo root (script lives at REPO/scripts/install/, so two levels up).
REPO_ROOT="$(cd "$(dirname "${BASH_SOURCE[0]}")/../.." && pwd)"

# Resolve the evo-ui-eng sibling repo where the systemd unit lives.
UI_REPO_ROOT="${UI_REPO_ROOT:-$(cd "${REPO_ROOT}/../evo-ui-eng" 2>/dev/null && pwd || echo "")}"
if [[ -z "${UI_REPO_ROOT}" || ! -d "${UI_REPO_ROOT}" ]]; then
    echo "FAIL: evo-ui-eng not found at sibling path ${REPO_ROOT}/../evo-ui-eng." >&2
    echo "      Set UI_REPO_ROOT=<absolute path to evo-ui-eng> and re-run." >&2
    exit 1
fi

SERVICE_SRC="${UI_REPO_ROOT}/apps/evo-ui-runtime/scripts/device/evo-ui.service"
if [[ ! -f "${SERVICE_SRC}" ]]; then
    echo "FAIL: systemd unit source missing at ${SERVICE_SRC}" >&2
    exit 1
fi

echo "=== ui-install.sh ==="
echo "Target:        ${SSH_TARGET}"
echo "evo-core-eng:  ${REPO_ROOT}"
echo "evo-ui-eng:    ${UI_REPO_ROOT}"
echo "systemd unit:  ${SERVICE_SRC}"
echo

# Phase 0: pre-flight. Keyless SSH, NOPASSWD sudo, and /opt/evo base must be
# in place. A failure here means prototype-install.sh has not run; this
# script is not the right entry point for a truly fresh box.
echo "[0/4] Pre-flight: keyless SSH + NOPASSWD sudo + /opt/evo base ..."
ssh -o BatchMode=yes -o ConnectTimeout=5 "${SSH_TARGET}" 'true' \
    || { echo "  FAIL: keyless SSH refused. Run ssh-copy-id first." >&2; exit 1; }
ssh "${SSH_TARGET}" 'sudo -n true' \
    || { echo "  FAIL: NOPASSWD sudo not configured for ${TARGET_USER}." >&2; exit 1; }
ssh "${SSH_TARGET}" 'test -d /opt/evo && test -d /opt/evo/bin' \
    || { echo "  FAIL: /opt/evo or /opt/evo/bin missing. Run prototype-install.sh first." >&2; exit 1; }
echo "  ok"
echo

# Phase 1: UI filesystem layout. Four directories under /opt/evo/ui, owned
# by the runtime user so the service (running as <service-user>) can write to
# data/ and logs/ per the unit's ReadWritePaths.
echo "[1/4] UI filesystem layout (/opt/evo/ui + releases + data + logs) ..."
ssh "${SSH_TARGET}" "
    set -e
    sudo install -d -m 0755 -o ${TARGET_USER} -g ${TARGET_USER} /opt/evo/ui
    sudo install -d -m 0755 -o ${TARGET_USER} -g ${TARGET_USER} /opt/evo/ui/releases
    sudo install -d -m 0755 -o ${TARGET_USER} -g ${TARGET_USER} /opt/evo/ui/data
    sudo install -d -m 0755 -o ${TARGET_USER} -g ${TARGET_USER} /opt/evo/ui/logs
"
echo "  ok"
echo

# Phase 2: systemd unit. The unit file in evo-ui-eng is the source of truth
# (it travels with the runtime codebase). Copy via scp + install rather than
# editing in place so the unit file is treated as artefact, not state.
echo "[2/4] install /etc/systemd/system/evo-ui.service + daemon-reload ..."
scp -q "${SERVICE_SRC}" "${SSH_TARGET}:/tmp/evo-ui.service"
ssh "${SSH_TARGET}" '
    set -e
    sudo install -m 0644 -o root -g root /tmp/evo-ui.service /etc/systemd/system/evo-ui.service
    rm -f /tmp/evo-ui.service
    sudo systemctl daemon-reload
'
echo "  ok"
echo

# Phase 3: report state.
echo "[3/4] verifying installed state ..."
ssh "${SSH_TARGET}" '
    set -e
    echo "    /opt/evo/ui:                  $(ls -la /opt/evo/ui | tail -n +2 | wc -l) entries"
    for d in /opt/evo/ui/releases /opt/evo/ui/data /opt/evo/ui/logs ; do
        echo "    $d:  $(stat -c "%U:%G %a" $d 2>/dev/null) $(test -d $d && echo present || echo MISSING)"
    done
    echo "    /opt/evo/ui/current:          $(readlink /opt/evo/ui/current 2>/dev/null || echo "(unset - deploy a release)")"
    echo "    /etc/systemd/system/evo-ui.service:  $(test -f /etc/systemd/system/evo-ui.service && echo present || echo MISSING)"
    echo "    evo-ui-runtime binary:        $(test -f /opt/evo/bin/evo-ui-runtime && echo present || echo MISSING - deploy via ui-deploy-release.sh)"
    echo "    evo-ui.service active:        $(systemctl is-active evo-ui.service 2>/dev/null || true)"
'
echo

# Phase 4: next steps.
echo "[4/4] complete"
echo
echo "Next steps:"
echo "  1. scripts/install/ui-deploy-release.sh ${SSH_TARGET}    # build runtime + shell, push, restart"
echo "  2. curl http://${TARGET_HOST}/api/ui/v1/health           # smoke the bootstrap endpoint"
