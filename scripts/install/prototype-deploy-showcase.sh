#!/usr/bin/env bash
#
# prototype-deploy-showcase.sh — deploy the framework's validation
# distribution to a reference aarch64 prototype already brought up by
# `prototype-install.sh`. Idempotent in shape: rerunning re-uploads +
# re-admits cleanly.
#
# The reference prototype runs the framework's VALIDATION DISTRIBUTION:
# a curated catalogue + plugin set + branding whose purpose is to
# exercise the fabric end-to-end on real hardware. It is distinct
# from `evo-example-distribution` (which owns the skeleton catalogue
# + example.* racks) and from any domain distribution like
# `evo-device-audio`.
#
# Steward-version policy: the validation distribution runs the
# latest eng-tree steward by deliberate choice — it is a
# development/showcase device, not a release-channel device. The
# previous binary is preserved as `evo.prev` for single-step
# rollback after deploy. Release-channel devices receive
# release-tagged artefacts via the publish workflow, not this
# script.
#
# What this script does (Pi-side post-bring-up; runs over SSH from
# the dev box):
#
#   1. Cross-builds the steward, the operator CLI, and every plugin
#      whose canonical name is in the validation distribution's
#      plugin set. Today: synthetic-test-source.
#   2. Slot-stocking preflight: enumerates shelves declared by the
#      validation catalogue, enumerates plugin manifests in the
#      built set, refuses to deploy if any declared shelf has no
#      stocking plugin (catalogue lying about a concern is the
#      fabric inconsistency this preflight prevents).
#   3. Stops the steward, replaces binaries (preserves the previous
#      as `evo.prev`).
#   4. Removes any plugins on /var/lib/evo/plugins/ that are NOT in
#      the validation distribution's plugin set. Plugins from other
#      distributions (e.g. evo-example-distribution's example.echo)
#      do not belong in the validation distribution; their presence
#      here would be a distribution-set drift.
#   5. Signs + packs + installs every plugin in the validation set
#      via `evo-plugin-tool install --chown <user>:<user>` so
#      admissions admit at platform trust class.
#   6. Drops the showcase plan at `/var/lib/evo/plans/showcase.toml`.
#      UserCommand trigger; sits dormant until the operator fires
#      it via `evo-plugin-tool plan fire showcase`.
#   7. Starts the steward + verifies the boot trace shows every
#      declared slot stocked.
#
# Usage:
#
#   scripts/install/prototype-deploy-showcase.sh <TARGET_HOST> <TARGET_USER>
#
#   Both arguments are required. No baked-in default so a stale value
#   never silently reaches an unintended target.
#
# After this script completes, fire the plan with:
#
#   ssh ${TARGET_HOST} '/opt/evo/bin/evo-plugin-tool plan fire \
#       showcase --socket /var/run/evo/evo.sock'
#
# And observe the chain in the journal:
#
#   ssh ${TARGET_HOST} 'sudo journalctl -u evo --since "30 seconds ago" \
#       --no-pager | grep -E "plan engine fire|plan execution|playback"'
#
# Plus the plugin's per-fire event log:
#
#   ssh ${TARGET_HOST} 'sudo cat \
#     /var/lib/evo/plugins/org.evoframework.acceptance.synthetic-test-source/state/test-source-events.log'

set -euo pipefail

if [[ $# -lt 2 ]]; then
    echo "usage: $0 <TARGET_HOST> <TARGET_USER>" >&2
    exit 1
fi
TARGET_HOST="$1"
TARGET_USER="$2"
SSH_TARGET="${TARGET_USER}@${TARGET_HOST}"

REPO_ROOT="$(cd "$(dirname "${BASH_SOURCE[0]}")/../.." && pwd)"
KEYS_ROOT="${KEYS_ROOT:?KEYS_ROOT must be set to the directory containing the plugin signing key}"
TARGET_TRIPLE="aarch64-unknown-linux-gnu"

# The validation distribution's catalogue is the single source of
# truth for what shelves the prototype declares.
VALIDATION_CAT="${REPO_ROOT}/dist/catalogue/prototype-validation.toml"

# The validation distribution's plugin set. Each entry is:
#   <plugin_canonical_name>:<crate_name>:<bin_name>:<manifest_path>:<signing_key>
# The script enumerates this list to build, sign, install, AND
# preflight slot-stocking against the catalogue's shelf
# declarations. Adding a plugin to the validation distribution
# means appending an entry here AND ensuring its manifest's
# `[target] shelf` matches a shelf declared in the validation
# catalogue.
VALIDATION_PLUGINS=(
    "org.evoframework.acceptance.synthetic-test-source:evo-acceptance-synthetic:synthetic-test-source-wire:crates/evo-acceptance-synthetic/manifests/synthetic-test-source/manifest.oop.toml:evo-acceptance-signing-private.pem"
)

echo "=== prototype-deploy-showcase.sh (validation distribution) ==="
echo "Target:    ${SSH_TARGET}"
echo "Repo:      ${REPO_ROOT}"
echo "Keys:      ${KEYS_ROOT}"
echo "Catalogue: ${VALIDATION_CAT}"
echo

# -------------------------------------------------------------
# [0/7] Pre-flight: bring-up artefacts must be present.
# -------------------------------------------------------------
echo "[0/7] Pre-flight: bring-up artefacts on target ..."
ssh -o BatchMode=yes -o ConnectTimeout=5 "${SSH_TARGET}" "
    set -e
    test -f /etc/evo/trust.d/evo-acceptance-signing-public.pem \
      || { echo 'FAIL: acceptance trust key missing; run prototype-install.sh first' >&2; exit 1; }
    test -f /opt/evo/catalogue/default.toml \
      || { echo 'FAIL: default catalogue missing; run prototype-install.sh first' >&2; exit 1; }
    test -f /etc/systemd/system/evo.service \
      || { echo 'FAIL: systemd unit missing; run prototype-install.sh first' >&2; exit 1; }
" >/dev/null
test -f "${VALIDATION_CAT}" \
    || { echo "FAIL: validation catalogue ${VALIDATION_CAT} missing"; exit 1; }
echo "  ok"
echo

# -------------------------------------------------------------
# [1/7] Slot-stocking preflight: every declared shelf has a stocking
# plugin in the validation set, and every plugin in the set targets
# a shelf the catalogue declares. The fabric rule "PLUGINS stock
# slots" enforced before any deploy work.
# -------------------------------------------------------------
echo "[1/7] Slot-stocking preflight ..."

# Enumerate fully-qualified shelf names declared by the catalogue.
declared_shelves=$(python3 - "${VALIDATION_CAT}" <<'PY'
import sys, tomllib
with open(sys.argv[1], "rb") as f:
    cat = tomllib.load(f)
for rack in cat.get("racks", []):
    for shelf in rack.get("shelves", []):
        print(f"{rack['name']}.{shelf['name']}")
PY
)

# Enumerate target shelves of plugins in the validation set.
plugin_shelves=""
plugin_names=""
for entry in "${VALIDATION_PLUGINS[@]}"; do
    IFS=":" read -r name crate bin manifest_rel _key <<<"${entry}"
    manifest_abs="${REPO_ROOT}/${manifest_rel}"
    if [[ ! -f "${manifest_abs}" ]]; then
        echo "  FAIL: manifest ${manifest_abs} missing for ${name}" >&2
        exit 1
    fi
    shelf=$(python3 - "${manifest_abs}" <<'PY'
import sys, tomllib
with open(sys.argv[1], "rb") as f:
    m = tomllib.load(f)
print(m["target"]["shelf"])
PY
)
    plugin_shelves="${plugin_shelves}${shelf}"$'\n'
    plugin_names="${plugin_names}${name}"$'\n'
done

# Reciprocal containment.
fail=0
while IFS= read -r shelf; do
    [[ -z "${shelf}" ]] && continue
    if ! grep -Fxq "${shelf}" <<<"${plugin_shelves}"; then
        echo "  FAIL: shelf ${shelf} declared in validation catalogue but no plugin in the validation set stocks it" >&2
        fail=1
    fi
done <<<"${declared_shelves}"
while IFS= read -r shelf; do
    [[ -z "${shelf}" ]] && continue
    if ! grep -Fxq "${shelf}" <<<"${declared_shelves}"; then
        echo "  FAIL: a validation-set plugin targets shelf ${shelf} which is NOT declared in the validation catalogue" >&2
        fail=1
    fi
done <<<"${plugin_shelves}"
[[ ${fail} -eq 1 ]] && exit 1
echo "  ok ($(echo "${declared_shelves}" | grep -c .) shelves declared, all stocked)"
echo

# -------------------------------------------------------------
# [2/7] Cross-build steward + operator CLI + plugin wire bins.
# -------------------------------------------------------------
echo "[2/7] cross-build steward + operator CLI + plugin wire binaries ..."
cd "${REPO_ROOT}"
build_args=(--release --target "${TARGET_TRIPLE}" \
    -p evo --bin evo \
    -p evo-plugin-tool --bin evo-plugin-tool)
for entry in "${VALIDATION_PLUGINS[@]}"; do
    IFS=":" read -r _name crate bin _manifest _key <<<"${entry}"
    build_args+=(-p "${crate}" --bin "${bin}")
done
cargo build "${build_args[@]}" >/dev/null 2>&1
TARGET_DIR="${REPO_ROOT}/target/${TARGET_TRIPLE}/release"
test -x "${TARGET_DIR}/evo"
test -x "${TARGET_DIR}/evo-plugin-tool"
for entry in "${VALIDATION_PLUGINS[@]}"; do
    IFS=":" read -r _name _crate bin _manifest _key <<<"${entry}"
    test -x "${TARGET_DIR}/${bin}" || { echo "FAIL: missing ${bin}" >&2; exit 1; }
done
echo "  ok"
echo

# -------------------------------------------------------------
# [3/7] Pack + sign every plugin in the validation set.
# -------------------------------------------------------------
echo "[3/7] pack + sign plugin bundles ..."
STAGING="$(mktemp -d -t evo-deploy-XXXXXX)"
trap 'rm -rf "${STAGING}"' EXIT

PLUGIN_TOOL_HOST="${REPO_ROOT}/target/release/evo-plugin-tool"
if [[ ! -x "${PLUGIN_TOOL_HOST}" ]]; then
    cargo build --release -p evo-plugin-tool --bin evo-plugin-tool >/dev/null 2>&1
fi

bundle_archives=()
for entry in "${VALIDATION_PLUGINS[@]}"; do
    IFS=":" read -r name _crate bin manifest_rel key_filename <<<"${entry}"
    bundle_dir="${STAGING}/${name}"
    mkdir -p "${bundle_dir}"
    cp "${REPO_ROOT}/${manifest_rel}" "${bundle_dir}/manifest.toml"
    cp "${TARGET_DIR}/${bin}" "${bundle_dir}/plugin.bin"
    chmod +x "${bundle_dir}/plugin.bin"
    "${PLUGIN_TOOL_HOST}" sign "${bundle_dir}" \
        --key "${KEYS_ROOT}/${key_filename}" >/dev/null
    archive="${STAGING}/${name}.tar.gz"
    "${PLUGIN_TOOL_HOST}" pack "${bundle_dir}" --out "${archive}" >/dev/null
    bundle_archives+=("${archive}")
done
echo "  ok"
echo

# -------------------------------------------------------------
# [4/7] Stop steward + replace binaries (preserve previous as
# evo.prev for single-step rollback).
# -------------------------------------------------------------
echo "[4/7] stop steward + install fresh binaries ..."
ssh "${SSH_TARGET}" '
    set -e
    sudo systemctl stop evo || true
    if [ -f /opt/evo/bin/evo ]; then
        sudo cp /opt/evo/bin/evo /opt/evo/bin/evo.prev
    fi
'
scp -q "${TARGET_DIR}/evo" "${SSH_TARGET}:/tmp/evo.deploy"
scp -q "${TARGET_DIR}/evo-plugin-tool" "${SSH_TARGET}:/tmp/evo-plugin-tool.deploy"
ssh "${SSH_TARGET}" '
    set -e
    sudo install -m 0755 /tmp/evo.deploy /opt/evo/bin/evo
    sudo install -m 0755 /tmp/evo-plugin-tool.deploy /opt/evo/bin/evo-plugin-tool
    rm -f /tmp/evo.deploy /tmp/evo-plugin-tool.deploy
'
echo "  ok"
echo

# -------------------------------------------------------------
# [5/7] Apply validation catalogue + drop foreign-distribution
# plugins from /var/lib/evo/plugins/ + install validation-set
# bundles.
# -------------------------------------------------------------
echo "[5/7] apply validation catalogue + drop foreign plugins + install validation set ..."
scp -q "${VALIDATION_CAT}" "${SSH_TARGET}:/tmp/default.toml"
# Build the list of plugin canonical names this distribution
# expects to be installed on the target.
expected_names=""
for entry in "${VALIDATION_PLUGINS[@]}"; do
    IFS=":" read -r name _ <<<"${entry}"
    expected_names="${expected_names}${name}"$'\n'
done
ssh "${SSH_TARGET}" "
    set -e
    sudo install -m 0644 /tmp/default.toml /opt/evo/catalogue/default.toml
    rm -f /tmp/default.toml
    # Remove any plugin in /var/lib/evo/plugins/ that is not in the
    # validation distribution's plugin set. Foreign plugins from
    # other distributions (e.g. evo-example-distribution's
    # example.echo) do not belong in this distribution.
    if [ -d /var/lib/evo/plugins ]; then
        for d in /var/lib/evo/plugins/*/; do
            [ -d \"\$d\" ] || continue
            name=\$(basename \"\$d\")
            keep=0
            while IFS= read -r expected; do
                [ -z \"\$expected\" ] && continue
                if [ \"\$name\" = \"\$expected\" ]; then keep=1; break; fi
            done <<EOF
${expected_names}
EOF
            if [ \"\$keep\" = \"0\" ]; then
                echo \"  removing foreign plugin: \$name\"
                sudo rm -rf \"\$d\"
            fi
        done
    fi
"
# Install (or reinstall) every validation-set bundle. Each
# bundle is sent to a per-deploy staging dir on the target so a
# stale archive lying in /tmp from a prior session cannot
# silently re-install a foreign plugin. The staging dir is
# wiped before scp + after install.
REMOTE_STAGING="/tmp/evo-deploy-validation"
ssh "${SSH_TARGET}" "rm -rf ${REMOTE_STAGING} && mkdir -p ${REMOTE_STAGING}"
for archive in "${bundle_archives[@]}"; do
    archive_base="$(basename "${archive}")"
    scp -q "${archive}" "${SSH_TARGET}:${REMOTE_STAGING}/${archive_base}"
done
ssh "${SSH_TARGET}" "
    set -e
    for archive in ${REMOTE_STAGING}/*.tar.gz; do
        sudo /opt/evo/bin/evo-plugin-tool install \
            \"\$archive\" \
            --to /var/lib/evo/plugins \
            --chown ${TARGET_USER}:${TARGET_USER} >/dev/null
    done
    rm -rf ${REMOTE_STAGING}
"
echo "  ok"
echo

# -------------------------------------------------------------
# [6/7] Stage the showcase plan TOML.
# -------------------------------------------------------------
echo "[6/7] stage showcase plan TOML ..."
ssh "${SSH_TARGET}" "
    set -e
    sudo install -d -m 0755 /var/lib/evo/plans
    sudo chown ${TARGET_USER}:${TARGET_USER} /var/lib/evo/plans
    sudo -u ${TARGET_USER} tee /var/lib/evo/plans/showcase.toml > /dev/null <<'PLAN'
id = \"showcase\"
name = \"Showcase Plan\"
description = \"End-to-end plan-firing showcase: dispatches play_now to the synthetic test source plugin, awaits natural end-of-playback via AudioPlaybackEnded, completes.\"
preempt = false
last_modified_ms = 1715000000000

[trigger]
kind = \"user_command\"

[on_complete]
kind = \"stop\"

[authored_by]
kind = \"user\"

[[segments]]

[segments.content]
kind = \"item\"
uri = \"evo-test:track:42\"

[segments.duration]
kind = \"until_completion\"

[segments.transition]
kind = \"hard\"
PLAN
"
echo "  ok"
echo

# -------------------------------------------------------------
# [7/7] Start steward + verify boot trace.
# -------------------------------------------------------------
echo "[7/7] start steward + verify boot ..."
ssh "${SSH_TARGET}" 'sudo systemctl start evo'
sleep 3
ssh "${SSH_TARGET}" 'sudo journalctl -u evo --since "5 seconds ago" --no-pager 2>&1 \
    | grep -E "plugin admitted|plans_loaded|server listening|fast path listening|catalogue declares shelves" \
    | grep -v "^[A-Z][a-z][a-z] " \
    | head -10'
echo
echo "=== prototype-deploy-showcase.sh complete ==="
echo
echo "Fire the showcase plan and observe the chain:"
echo "  ssh ${SSH_TARGET} '/opt/evo/bin/evo-plugin-tool plan fire showcase \\"
echo "    --socket /var/run/evo/evo.sock'"
