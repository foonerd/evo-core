#!/usr/bin/env bash
#
# prototype-install.sh — bring a fresh Trixie Lite (or Trixie) reference
# aarch64 prototype to evo-ready state. Idempotent: running multiple
# times leaves the end state the same.
#
# The reference prototype is a disposable test fixture: it is meant to
# be wiped and rebuilt as needed. This script is the rebuild contract —
# the asset worth capturing is the recipe that recreates the prototype
# from a fresh OS, not the prototype's installed state.
#
# What this script DOES (Pi-side bring-up; runs over SSH from the dev box):
#
#   1. apt update + install base packages (curl, jq, sqlite3, python3,
#      ca-certificates, openssh-server, plus build deps for the optional
#      native-build path).
#   2. Configure NOPASSWD sudo for the bring-up user (idempotent — does
#      nothing if already configured).
#   3. Create the framework's filesystem layout (/opt/evo, /etc/evo,
#      /etc/evo/trust.d, /var/lib/evo-acceptance-fixtures). The state
#      directory /var/lib/evo is created by systemd via the unit's
#      StateDirectory= directive at first start, not by this script.
#   4. Install the acceptance signing public key into /etc/evo/trust.d/
#      so the steward can verify acceptance plugin bundles.
#   5. Install a default skeleton catalogue at
#      /opt/evo/catalogue/default.toml.
#   6. Install the systemd unit at /etc/systemd/system/evo.service with
#      User=root for development / acceptance use (production
#      distributions choose a service user per BOUNDARY.md §6).
#
# What this script does NOT do (handled separately):
#
#   - Install any build toolchain on the Pi. The Pi prototype is a
#     **TEST target, not a build target**. Builds happen on the dev
#     box (cross-compile to aarch64-unknown-linux-gnu); resulting
#     binaries are scp'd to the Pi. Building on the Pi is forbidden
#     discipline (slow, and skips audio-device-specific runtime
#     requirements that surface on real reference hardware).
#   - Deploy the steward binary. After this script runs the Pi has
#     filesystem layout + trust roots + catalogue + systemd unit
#     ready; the binary is scp'd separately:
#       scp target/aarch64-unknown-linux-gnu/release/evo \
#           target/aarch64-unknown-linux-gnu/release/evo-plugin-tool \
#           ${SSH_TARGET}:/tmp/
#       ssh ${SSH_TARGET} 'sudo install -m 0755 -o root -g root /tmp/evo /opt/evo/bin/ && \
#                          sudo install -m 0755 -o root -g root /tmp/evo-plugin-tool /opt/evo/bin/ && \
#                          rm -f /tmp/evo /tmp/evo-plugin-tool'
#   - Stage acceptance fixtures (bundle .tar.gz files at
#     /var/lib/evo-acceptance-fixtures/). Bundles are produced by the
#     bundle-build pipeline on the dev box and staged separately.
#   - Enable + start the steward service. Done after the binary is in
#     place.
#
# Usage:
#
#   scripts/install/prototype-install.sh <TARGET_HOST> <TARGET_USER>
#
#   Both arguments are required. The script does not bake in defaults
#   so a stale value never silently reaches an unintended target.
#
#   Requires: keyless SSH already established to TARGET_USER@TARGET_HOST
#   and NOPASSWD sudo configured. The one-time `ssh-copy-id` +
#   interactive `visudo`-style sudoers install precede this script on
#   a truly fresh box.

set -euo pipefail

if [[ $# -lt 2 ]]; then
    echo "usage: $0 <TARGET_HOST> <TARGET_USER>" >&2
    echo "  TARGET_HOST: IP or hostname of the target reachable via ssh" >&2
    echo "  TARGET_USER: operator-configured service user on the target" >&2
    exit 1
fi
TARGET_HOST="$1"
TARGET_USER="$2"
SSH_TARGET="${TARGET_USER}@${TARGET_HOST}"

# Repo root (script lives at REPO/scripts/install/, so two levels up).
REPO_ROOT="$(cd "$(dirname "${BASH_SOURCE[0]}")/../.." && pwd)"
KEYS_ROOT="${KEYS_ROOT:?KEYS_ROOT must be set to the directory containing evo-core-release-signing-private.pem}"

echo "=== prototype-install.sh ==="
echo "Target:   ${SSH_TARGET}"
echo "Repo:     ${REPO_ROOT}"
echo "Keys:     ${KEYS_ROOT}"
echo

# Sanity — the script does not configure SSH or sudo bootstrapping.
# A truly-fresh Pi needs `ssh-copy-id` and a one-time NOPASSWD
# sudoers install before this runs; failing either of these checks
# is a setup-incomplete signal, not a script bug.
echo "[0/6] Pre-flight: keyless SSH + NOPASSWD sudo ..."
ssh -o BatchMode=yes -o ConnectTimeout=5 "${SSH_TARGET}" 'true' \
    || { echo "  FAIL: keyless SSH refused. Run ssh-copy-id first." >&2; exit 1; }
ssh "${SSH_TARGET}" 'sudo -n true' \
    || { echo "  FAIL: NOPASSWD sudo not configured for ${TARGET_USER}." >&2; exit 1; }
echo "  ok"
echo

# Phase 1: apt update + base packages.
echo "[1/6] apt update + base packages ..."
ssh "${SSH_TARGET}" '
    set -e
    sudo apt-get update -qq
    sudo apt-get install -y -qq \
        ca-certificates \
        curl \
        jq \
        sqlite3 \
        python3 \
        rsync \
        openssh-server
' >/dev/null
echo "  ok"
echo

# Phase 2: filesystem layout.
echo "[2/6] filesystem layout (/opt/evo, /etc/evo, /etc/evo/trust.d, /var/lib/evo-acceptance-fixtures) ..."
ssh "${SSH_TARGET}" '
    set -e
    sudo install -d -m 0755 /opt/evo
    sudo install -d -m 0755 /opt/evo/bin
    sudo install -d -m 0755 /opt/evo/catalogue
    sudo install -d -m 0755 /etc/evo
    sudo install -d -m 0755 /etc/evo/trust.d
    sudo install -d -m 0755 /var/lib/evo-acceptance-fixtures
'
echo "  ok"
echo

# Phase 3: trust roots. Two keys land:
#   - evo-acceptance-signing — gates org.evoframework.acceptance.*
#     (synthetic plugins under crates/evo-acceptance-synthetic).
#   - vendor-plugin-signing — gates org.evo.example.* (reference
#     example plugins under crates/evo-example-*).
# A prototype that opts in to neither has no admittable bundles
# and the steward boots with shelves-declared-no-plugins-admitted.
echo "[3/6] install signing public keys (+ sidecar meta.toml files) ..."
install_trust_key() {
    local pubkey="$1"
    local meta="$2"
    local label="$3"
    if [[ ! -f "${pubkey}" ]]; then
        echo "  WARN: ${pubkey} missing; skipping ${label} trust-root install" >&2
        return 0
    fi
    if [[ ! -f "${meta}" ]]; then
        echo "  FAIL: ${pubkey} present but sidecar ${meta} missing." >&2
        echo "  The framework refuses to load a trust key without its .meta.toml sidecar." >&2
        exit 1
    fi
    local pubkey_base
    local meta_base
    pubkey_base="$(basename "${pubkey}")"
    meta_base="$(basename "${meta}")"
    scp -q "${pubkey}" "${SSH_TARGET}:/tmp/${pubkey_base}"
    scp -q "${meta}" "${SSH_TARGET}:/tmp/${meta_base}"
    ssh "${SSH_TARGET}" "
        set -e
        sudo install -m 0644 /tmp/${pubkey_base} /etc/evo/trust.d/${pubkey_base}
        sudo install -m 0644 /tmp/${meta_base} /etc/evo/trust.d/${meta_base}
        rm -f /tmp/${pubkey_base} /tmp/${meta_base}
    "
    echo "  ok: ${label}"
}
install_trust_key \
    "${KEYS_ROOT}/evo-acceptance-signing-public.pem" \
    "${KEYS_ROOT}/evo-acceptance-signing-public.meta.toml" \
    "acceptance"
install_trust_key \
    "${KEYS_ROOT}/vendor-plugin-signing-public.pem" \
    "${KEYS_ROOT}/vendor-plugin-signing-public.meta.toml" \
    "vendor-plugin"
echo

# Phase 4: default catalogue. The prototype runs the framework's
# validation distribution; its catalogue declares the racks the
# distribution's curated plugin set actually stocks. The
# v0-skeleton catalogue belongs to evo-example-distribution
# (a different distribution); installing it here would put the
# prototype in a frankendistribution state with declared shelves
# no plugin in the validation set stocks.
echo "[4/6] install validation-distribution catalogue ..."
VALIDATION_CAT="${REPO_ROOT}/dist/catalogue/prototype-validation.toml"
if [[ ! -f "${VALIDATION_CAT}" ]]; then
    echo "  FAIL: ${VALIDATION_CAT} missing" >&2
    exit 1
fi
scp -q "${VALIDATION_CAT}" "${SSH_TARGET}:/tmp/default.toml"
ssh "${SSH_TARGET}" '
    set -e
    sudo install -m 0644 /tmp/default.toml /opt/evo/catalogue/default.toml
    rm -f /tmp/default.toml
'
echo "  ok"
echo

# Phase 5: systemd unit + drop-ins (User=, RUST_LOG=info) + PATH
# symlinks. Runs steward as the test user so StateDirectory=evo
# under /var/lib/evo is owned by <service-user> — `evo-plugin-tool
# install` (which runs as <service-user>, not root) needs write access
# to /var/lib/evo/plugins/. RUST_LOG=info so acceptance scenarios
# can grep boot info lines. /usr/local/bin symlinks so non-login
# SSH (which the acceptance harness uses for scenario shells)
# resolves `evo` and `evo-plugin-tool` without sourcing
# /etc/profile.d/.
echo "[5/6] install systemd unit + drop-ins + PATH symlinks ..."
SERVICE_EXAMPLE="${REPO_ROOT}/dist/systemd/evo.service.example"
if [[ ! -f "${SERVICE_EXAMPLE}" ]]; then
    echo "  FAIL: ${SERVICE_EXAMPLE} missing" >&2
    exit 1
fi
# The example template runs as root if no User=/Group= is set. We
# override via a drop-in to run as ${TARGET_USER} so the test
# fixture's permissions match how a typical packaged appliance
# would deploy.
scp -q "${SERVICE_EXAMPLE}" "${SSH_TARGET}:/tmp/evo.service"
ssh "${SSH_TARGET}" "
    set -e
    sudo install -m 0644 /tmp/evo.service /etc/systemd/system/evo.service
    rm -f /tmp/evo.service
    sudo install -d -m 0755 /etc/systemd/system/evo.service.d
    sudo tee /etc/systemd/system/evo.service.d/user.conf > /dev/null <<UCONF
[Service]
User=${TARGET_USER}
Group=${TARGET_USER}
UCONF
    # The framework reference unit deliberately ships without a baked
    # ExecStart so every deployment makes an explicit choice about
    # which steward binary to run. The framework's own integration rig
    # runs /opt/evo/bin/evo (the framework's steward binary); each
    # domain or vendor distribution layers its own exec-start.conf
    # with its own binary and overwrites this drop-in.
    sudo tee /etc/systemd/system/evo.service.d/exec-start.conf > /dev/null <<XCONF
[Service]
ExecStart=
ExecStart=/opt/evo/bin/evo
XCONF
    sudo tee /etc/systemd/system/evo.service.d/log-level.conf > /dev/null <<LCONF
[Service]
Environment=RUST_LOG=info
LCONF
    # If /var/lib/evo was created previously by systemd while the
    # unit ran as root, chown it now so the test user can write
    # under /var/lib/evo/plugins/ at evo-plugin-tool install time.
    if [ -d /var/lib/evo ]; then
      sudo chown -R ${TARGET_USER}:${TARGET_USER} /var/lib/evo
    fi
    # PATH for non-login SSH: symlink under /usr/local/bin so the
    # acceptance harness resolves bare 'evo-plugin-tool' invocations.
    sudo ln -sf /opt/evo/bin/evo /usr/local/bin/evo
    sudo ln -sf /opt/evo/bin/evo-plugin-tool /usr/local/bin/evo-plugin-tool
    sudo systemctl daemon-reload
"
echo "  ok"
echo

# Phase 6: report state.
echo "[6/6] verifying installed state ..."
ssh "${SSH_TARGET}" '
    set -e
    echo "    /opt/evo:                   $(ls -la /opt/evo | tail -n +2 | wc -l) entries"
    echo "    /opt/evo/catalogue:         $(ls /opt/evo/catalogue/ 2>/dev/null | tr "\n" " ")"
    echo "    /etc/evo/trust.d:           $(ls /etc/evo/trust.d/ 2>/dev/null | tr "\n" " ")"
    echo "    /var/lib/evo-acceptance-fixtures: $(ls /var/lib/evo-acceptance-fixtures/ 2>/dev/null | wc -l) entries"
    echo "    /etc/systemd/system/evo.service: $(test -f /etc/systemd/system/evo.service && echo present || echo MISSING)"
    echo "    evo binary:                 $(test -f /opt/evo/bin/evo && echo present || echo MISSING - deploy separately)"
'
echo
echo "=== prototype-install.sh complete ==="
echo
echo "Next steps:"
echo "  1. Build + deploy evo binary to ${SSH_TARGET}:/opt/evo/bin/evo"
echo "     (cross-compile from dev box, or native-build on Pi)"
echo "  2. sudo systemctl enable --now evo"
echo "  3. journalctl -u evo --since 'now' -f  (verify boot)"
