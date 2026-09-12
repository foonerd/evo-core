#!/usr/bin/env bash
#
# ui-deploy-release.sh - build the UI runtime + shell on the dev rig and ship
# a new release to a target. This script is the canonical per-release iteration
# step for the UI runtime: it cross-builds on the rig and ships binary + dist
# artefacts to an already-installed target footprint without touching the
# target's installed structure.
#
# Pre-condition: ui-install.sh has already run on the target. That script
# installs /opt/evo/ui directory tree, /etc/systemd/system/evo-ui.service,
# and runs daemon-reload. This script fails fast if those preconditions
# are missing.
#
# What this script does:
#
#   1. Cross-build evo-ui-runtime for aarch64-unknown-linux-gnu on the rig.
#   2. Build evo-ui-shell dist via npm (npm ci + npm run build) on the rig.
#   3. Allocate /opt/evo/ui/releases/<UTC-timestamp>/ on the target.
#   4. Rsync the shell dist into that release directory.
#   5. scp the runtime binary to /opt/evo/bin/evo-ui-runtime (root:root, 0755).
#   6. Repoint /opt/evo/ui/current symlink at the new release directory.
#   7. systemctl daemon-reload + enable + restart evo-ui.service.
#   8. Smoke /api/ui/v1/health + /api/ui/v1/capabilities + / from 127.0.0.1.
#
# Build is on the rig only. The target receives binary + dist artefacts. No
# build toolchain is installed on the target by this script; this is an
# invariant — runtime-only targets do not grow a build toolchain regardless
# of how many releases ship through them.
#
# Cross-repo dependency: this script lives in evo-core-eng but builds and
# ships artefacts from evo-ui-eng. Default resolution expects sibling repos
# under the same project root. Override with UI_REPO_ROOT=<path>.
#
# Usage:
#
#   scripts/install/ui-deploy-release.sh <TARGET_HOST> <TARGET_USER>
#
#   Both arguments are required. No baked-in default so a stale value
#   never silently reaches an unintended target.
#
#   Requires: ui-install.sh has run on TARGET_HOST. Build rig has the
#             Rust toolchain with the target's triple installed (aarch64-
#             or x86_64-unknown-linux-gnu), the matching cross linker, and
#             Node.js 22 LTS or newer. Target arch is detected over SSH.

set -euo pipefail

if [[ $# -lt 2 ]]; then
    echo "usage: $0 <TARGET_HOST> <TARGET_USER>" >&2
    exit 1
fi
TARGET_HOST="$1"
TARGET_USER="$2"
SSH_TARGET="${TARGET_USER}@${TARGET_HOST}"

# Repo root (script lives at REPO/scripts/install/, two levels up).
REPO_ROOT="$(cd "$(dirname "${BASH_SOURCE[0]}")/../.." && pwd)"

UI_REPO_ROOT="${UI_REPO_ROOT:-$(cd "${REPO_ROOT}/../evo-ui-eng" 2>/dev/null && pwd || echo "")}"
if [[ -z "${UI_REPO_ROOT}" || ! -d "${UI_REPO_ROOT}" ]]; then
    echo "FAIL: evo-ui-eng not found at sibling path ${REPO_ROOT}/../evo-ui-eng." >&2
    echo "      Set UI_REPO_ROOT=<absolute path to evo-ui-eng> and re-run." >&2
    exit 1
fi

RUNTIME_DIR="${UI_REPO_ROOT}/apps/evo-ui-runtime"
SHELL_DIR="${UI_REPO_ROOT}/apps/evo-ui-shell"
BIN_NAME="evo-ui-runtime"

if [[ ! -d "${RUNTIME_DIR}" ]]; then
    echo "FAIL: runtime crate missing at ${RUNTIME_DIR}" >&2; exit 1
fi
if [[ ! -d "${SHELL_DIR}" ]]; then
    echo "FAIL: shell package missing at ${SHELL_DIR}" >&2; exit 1
fi

# Detect the target architecture and select the matching Rust triple. The
# prototype fleet is mixed - aarch64 (Pi) and x86_64 (VM / NUC) - so a
# hardcoded triple would ship a binary the target cannot exec. Detection
# keeps this the one canonical deploy path for every target arch, with no
# manual side-deploy for the x86_64 boxes.
TARGET_ARCH="$(ssh -o BatchMode=yes -o ConnectTimeout=5 "${SSH_TARGET}" 'uname -m' 2>/dev/null || true)"
case "${TARGET_ARCH}" in
    aarch64) TARGET_TRIPLE="aarch64-unknown-linux-gnu" ;;
    x86_64)  TARGET_TRIPLE="x86_64-unknown-linux-gnu" ;;
    "")      echo "FAIL: could not reach ${SSH_TARGET} to detect target arch." >&2; exit 1 ;;
    *)       echo "FAIL: unsupported target arch '${TARGET_ARCH}' on ${SSH_TARGET}." >&2; exit 1 ;;
esac

echo "=== ui-deploy-release.sh ==="
echo "Target:        ${SSH_TARGET}"
echo "Runtime crate: ${RUNTIME_DIR}"
echo "Shell pkg:     ${SHELL_DIR}"
echo "Target triple: ${TARGET_TRIPLE}"
echo

# Phase 0: pre-flight. SSH, ui-install.sh footprint, privilege mode.
#
# The footprint checks come before the privilege check: a target that
# never had ui-install.sh run on it is not a sudo problem, and saying
# so first keeps the operator off the wrong trail.
echo "[0/8] pre-flight: target ready (ui-install.sh has run) ..."
ssh -o BatchMode=yes -o ConnectTimeout=5 "${SSH_TARGET}" 'true' \
    || { echo "  FAIL: keyless SSH refused." >&2; exit 1; }
ssh "${SSH_TARGET}" 'test -f /etc/systemd/system/evo-ui.service' \
    || { echo "  FAIL: evo-ui.service unit not installed. Run ui-install.sh first." >&2; exit 1; }
ssh "${SSH_TARGET}" 'test -d /opt/evo/ui/releases' \
    || { echo "  FAIL: /opt/evo/ui/releases missing. Run ui-install.sh first." >&2; exit 1; }

# Remote privilege mode. Passwordless sudo is not a precondition of
# this deploy, only the quieter of the two ways to reach root on the
# target. Where `sudo -n` answers, nothing about this run changes.
# Where it does not, the operator's own sudo is used over a terminal,
# which is a prompt, not a grant: this script never widens sudoers.
#
# The interactive credential is proved here, at phase 0, rather than
# discovered at phase 3 — after a cross-build and an npm build have
# already been paid for.
if ssh "${SSH_TARGET}" 'sudo -n true' 2>/dev/null; then
    SUDO_MODE="noninteractive"
    echo "  sudo: passwordless"
elif [[ ! -t 0 ]]; then
    echo "  FAIL: ${TARGET_HOST} has no passwordless sudo and this run" >&2
    echo "        has no terminal on which sudo could prompt." >&2
    echo "        Re-run from an interactive terminal." >&2
    exit 1
elif ssh -t "${SSH_TARGET}" 'sudo -v'; then
    SUDO_MODE="interactive"
    echo "  sudo: interactive (prompts on this terminal)"
else
    echo "  FAIL: sudo on ${TARGET_HOST} refused ${TARGET_USER}." >&2
    echo "        The account needs sudo rights there; a password is" >&2
    echo "        fine, NOPASSWD is not required." >&2
    exit 1
fi
echo "  ok"
echo

# Run a command on the target as root.
#
# One seam for every privileged remote step, so the two modes cannot
# drift apart per call site. In interactive mode ssh allocates a
# terminal so sudo has somewhere to prompt; otherwise the invocation
# is exactly what it has always been. Never use this for a command
# whose stdout is captured — a terminal adds carriage returns.
ssh_sudo() {
    if [[ "${SUDO_MODE}" == "interactive" ]]; then
        ssh -t "${SSH_TARGET}" "$@"
    else
        ssh "${SSH_TARGET}" "$@"
    fi
}

# Phase 1: cross-build evo-ui-runtime for the detected target arch. Build on
# rig only; the target never sees a compiler.
echo "[1/8] cross-build evo-ui-runtime (${TARGET_TRIPLE}) ..."
(cd "${RUNTIME_DIR}" && cargo build --release --target "${TARGET_TRIPLE}")
TARGET_DIR_CARGO="${CARGO_TARGET_DIR:-${RUNTIME_DIR}/target}"
LOCAL_BIN="${TARGET_DIR_CARGO}/${TARGET_TRIPLE}/release/${BIN_NAME}"
if [[ ! -x "${LOCAL_BIN}" ]]; then
    echo "  FAIL: binary missing at ${LOCAL_BIN}" >&2; exit 1
fi
echo "  ok ($(du -h "${LOCAL_BIN}" | awk '{print $1}'))"
echo

# Phase 2: build evo-ui-shell dist with npm. Requires Node 22 LTS+ on rig.
# npm ci uses the lockfile for a reproducible install; npm run build runs vite.
echo "[2/8] build evo-ui-shell dist (npm ci + npm run build) ..."
(cd "${SHELL_DIR}" && npm ci --no-audit --no-fund && npm run build)
if [[ ! -d "${SHELL_DIR}/dist" ]]; then
    echo "  FAIL: dist/ missing after npm build" >&2; exit 1
fi
echo "  ok ($(du -sh "${SHELL_DIR}/dist" | awk '{print $1}'))"
echo

# Whether a transferred release is usable.
#
# Takes the three facts gathered from the target after the copy:
# the byte size of index.html, the file count in the release
# directory, and the file count in the local dist that was sent.
# Returns 0 only when the release is populated and complete.
#
# This exists as a function so the test suite drives the same
# predicate the deploy runs — scripts/install/tests/ui-release-gate.test.sh
# extracts it from this file.
ui_release_is_populated() {
    local index_bytes="${1:-0}" remote_files="${2:-0}" local_files="${3:-0}"
    # A release without an index.html serves 404 for every route.
    [ "${index_bytes}" -gt 0 ] || return 1
    # An empty directory is the failure this gate exists for.
    [ "${remote_files}" -gt 0 ] || return 1
    # A short copy is as broken as no copy; the counts must agree.
    [ "${remote_files}" -eq "${local_files}" ] || return 1
    return 0
}

# Phase 3: prepare release directory on the target.
RELEASE_ID="$(date -u +%Y%m%dT%H%M%SZ)"
RELEASE_DIR="/opt/evo/ui/releases/${RELEASE_ID}"
echo "[3/8] prepare release ${RELEASE_ID} on target ..."
ssh_sudo "sudo install -d -m 0755 -o ${TARGET_USER} -g ${TARGET_USER} '${RELEASE_DIR}'"
echo "  ok"
echo

# Phase 4: copy the shell dist into the new release directory.
#
# rsync is not assumed present on the target: it is not part of the
# runtime-only footprint this deploy otherwise requires. Check for it
# by name and fail with the remedy, rather than surfacing a missing
# remote binary as an rsync protocol-stream error.
echo "[4/8] rsync UI dist to ${RELEASE_DIR} ..."
if ! ssh "${SSH_TARGET}" 'command -v rsync >/dev/null 2>&1'; then
    echo "FAIL: rsync is not installed on ${TARGET_HOST}." >&2
    echo "      This script copies the shell dist with rsync over ssh." >&2
    echo "      /opt/evo/ui/current has NOT been moved; the target is" >&2
    echo "      still serving its previous release." >&2
    echo "      Install rsync on the target, then re-run." >&2
    exit 4
fi
if [[ "${SUDO_MODE}" == "interactive" ]]; then
    # sudo cannot prompt through rsync's protocol stream, and it does
    # not need to: phase 3 created ${RELEASE_DIR} owned by
    # ${TARGET_USER}, so the transfer writes as the caller.
    rsync -a --delete "${SHELL_DIR}/dist/" "${SSH_TARGET}:${RELEASE_DIR}/"
else
    rsync -a --delete --rsync-path='sudo rsync' "${SHELL_DIR}/dist/" "${SSH_TARGET}:${RELEASE_DIR}/"
fi
ssh_sudo "sudo chown -R ${TARGET_USER}:${TARGET_USER} '${RELEASE_DIR}'"
echo "  ok"
echo

# Phase 4b: verify the release landed before anything points at it.
#
# The symlink is the commit point of this deploy. Nothing may reach
# it on the strength of a copy that did not happen.
echo "[4b/8] verify ${RELEASE_DIR} is populated ..."
LOCAL_FILES="$(find "${SHELL_DIR}/dist" -type f | wc -l | tr -d ' ')"
REMOTE_FACTS="$(ssh "${SSH_TARGET}" "
    stat -c %s '${RELEASE_DIR}/index.html' 2>/dev/null || echo 0
    find '${RELEASE_DIR}' -type f 2>/dev/null | wc -l
")"
INDEX_BYTES="$(printf '%s\n' "${REMOTE_FACTS}" | sed -n 1p | tr -d ' ')"
REMOTE_FILES="$(printf '%s\n' "${REMOTE_FACTS}" | sed -n 2p | tr -d ' ')"
if ! ui_release_is_populated "${INDEX_BYTES}" "${REMOTE_FILES}" "${LOCAL_FILES}"; then
    echo "FAIL: ${RELEASE_DIR} is not a usable release." >&2
    echo "      index.html: ${INDEX_BYTES} bytes" >&2
    echo "      files on target: ${REMOTE_FILES}, sent: ${LOCAL_FILES}" >&2
    echo "      /opt/evo/ui/current has NOT been moved; the target is" >&2
    echo "      still serving its previous release." >&2
    exit 4
fi
echo "  ok (index.html ${INDEX_BYTES} bytes, ${REMOTE_FILES} files)"
echo

# Phase 5: scp the runtime binary into /opt/evo/bin/ as root:root, 0755.
# The binary needs CAP_NET_BIND_SERVICE to bind port 80 as the <service-user> user;
# that capability comes from the systemd unit's AmbientCapabilities, not from
# file ownership, so root:root with 0755 is correct.
echo "[5/8] push runtime binary to /opt/evo/bin/${BIN_NAME} ..."
scp -q "${LOCAL_BIN}" "${SSH_TARGET}:/tmp/${BIN_NAME}"
ssh_sudo "
    set -e
    sudo install -m 0755 -o root -g root /tmp/${BIN_NAME} /opt/evo/bin/${BIN_NAME}
    rm -f /tmp/${BIN_NAME}
"
echo "  ok"
echo

# Phase 6: repoint /opt/evo/ui/current symlink and bounce the service.
# The systemd unit's StateDirectory and ReadWritePaths point into /opt/evo/ui/
# subpaths; the runtime serves static files from /opt/evo/ui/current.
#
# reset-failed before restart: the unit is Restart=always/RestartSec=2. If the
# binary was absent (e.g. a target wiped by a reset before its first deploy),
# the unit crash-loops and trips systemd's start rate limit (StartLimitBurst).
# Once start-limit-hit, systemctl restart is refused until the counter is
# cleared. reset-failed clears it and is a harmless no-op on a healthy unit,
# so it belongs unconditionally in the canonical deploy path.
echo "[6/8] repoint /opt/evo/ui/current and bounce evo-ui.service ..."
ssh_sudo "
    set -e
    sudo ln -sfn '${RELEASE_DIR}' /opt/evo/ui/current
    sudo systemctl daemon-reload
    sudo systemctl enable evo-ui.service >/dev/null 2>&1 || true
    sudo systemctl reset-failed evo-ui.service >/dev/null 2>&1 || true
    sudo systemctl restart evo-ui.service
"
echo "  ok"
echo

# Phase 7: verify service active.
echo "[7/8] verify systemctl status ..."
ssh_sudo '
    sudo systemctl --no-pager --full status evo-ui.service | sed -n "1,15p"
'
echo

# Phase 8: smoke the bootstrap endpoints from the target's loopback. Using
# 127.0.0.1 confirms the service is listening locally; external clients then
# can also hit http://<target-host>/.
echo "[8/8] smoke /api/ui/v1/health + capabilities + / ..."
ssh "${SSH_TARGET}" '
    for path in /api/ui/v1/health /api/ui/v1/capabilities / ; do
        curl -sS -o /dev/null -w "    ${path}  http=%{http_code}  time=%{time_total}s\n" "http://127.0.0.1${path}" || true
    done
'
echo

echo "=== ui-deploy-release.sh complete ==="
echo "Release:  ${RELEASE_ID}"
echo "Symlink:  /opt/evo/ui/current -> ${RELEASE_DIR}"
echo "Smoke:    curl http://${TARGET_HOST}/api/ui/v1/health"
