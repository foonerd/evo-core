#!/usr/bin/env bash
#
# scripts/release/cut-release.sh — 4-repo release-cut orchestrator.
#
# Drives per-repo pre-tag-check + promote in the operator-agreed
# order:
#
#   evo-catalogue-schemas → evo-core → evo-device-audio → evo-device-audio-ui
#
# Each promote step is REFUSED if
# its pre-tag-check does not pass locally. Any refusal aborts the
# cut (no partial promotion).
#
# This script does not:
#   - Mint tags. The operator mints per repo in advance; the tag
#     format is enforced at each repo's promote.
#   - Bootstrap missing artefact repos. Both
#     evo-catalogue-schemas-artefacts and
#     evo-device-audio-ui-artefacts must already exist.
#   - Perform the release-cut install/reset primitives contract primitives on rigs. The evidence must
#     already exist under evo-device-audio/dist/release/evidence/.
#
# Usage:
#   scripts/release/cut-release.sh \
#     --tag VERSION \
#     --config PATH \
#     [--channel {dev|test|prod}] \
#     [--dry-run] \
#     [--no-push]
#
# --config is a TOML file naming the eng and public checkout paths
# per repo + the artefacts checkout path + the the release-cut install/reset primitives contract signing
# key. Example shape (see cut-release.example.toml):
#
#   [repos.evo-catalogue-schemas]
#   eng    = "/path/to/evo-catalogue-schemas"
#   artefacts = "/path/to/evo-catalogue-schemas-artefacts"
#
#   [repos.evo-core]
#   eng    = "/path/to/evo-core-eng"
#   public = "/path/to/evo-core"
#
#   [repos.evo-device-audio]
#   eng    = "/path/to/evo-device-audio"
#   public = "/path/to/evo-device-audio-public"
#   artefacts = "/path/to/evo-device-audio-artefacts"
#
#   [repos.evo-device-audio-ui]
#   eng    = "/path/to/evo-ui-eng"
#   public = "/path/to/evo-device-audio-ui"
#   artefacts = "/path/to/evo-device-audio-ui-artefacts"
#
#   [signing]
#   release_key = "/path/to/evo-core-release-signing-private.pem"
#   acceptance_key = "/path/to/evo-acceptance-signing-private.pem"
#   acceptance_public_key = "/path/to/evo-acceptance-signing-public.pem"

set -euo pipefail

TAG=""
CONFIG=""
CHANNEL="dev"
DRY_RUN=0
NO_PUSH=0

usage() {
    cat <<EOF >&2
Usage: $(basename "$0") \\
    --tag VERSION \\
    --config PATH \\
    [--channel {dev|test|prod}] \\
    [--dry-run] \\
    [--no-push]

Drives the four-repo release cut in order.
EOF
    exit 1
}

while [[ $# -gt 0 ]]; do
    case "$1" in
        --tag)      TAG="$2"; shift 2 ;;
        --config)   CONFIG="$2"; shift 2 ;;
        --channel)  CHANNEL="$2"; shift 2 ;;
        --dry-run)  DRY_RUN=1; shift ;;
        --no-push)  NO_PUSH=1; shift ;;
        -h|--help)  usage ;;
        *) echo "unknown argument: $1" >&2; usage ;;
    esac
done

[[ -z "${TAG}" ]]    && { echo "--tag required" >&2; exit 1; }
[[ -z "${CONFIG}" ]] && { echo "--config required" >&2; exit 1; }
[[ -r "${CONFIG}" ]] || { echo "config not readable: ${CONFIG}" >&2; exit 1; }

log() { printf '\n[cut-release] %s\n' "$*" >&2; }
die() { printf '\n[cut-release] REFUSE: %s\n' "$*" >&2; exit 1; }

# ---- Parse config ----

python_extract() {
    python3 - "$@" <<'PY'
import sys, tomllib
key = sys.argv[1]
with open(sys.argv[2], "rb") as f:
    data = tomllib.load(f)
cursor = data
for segment in key.split("."):
    cursor = cursor[segment]
print(cursor)
PY
}

get_config() { python_extract "$1" "${CONFIG}"; }

SCHEMAS_ENG=$(get_config repos.evo-catalogue-schemas.eng)
SCHEMAS_ARTS=$(get_config repos.evo-catalogue-schemas.artefacts)

CORE_ENG=$(get_config repos.evo-core.eng)
CORE_PUB=$(get_config repos.evo-core.public)

AUDIO_ENG=$(get_config repos.evo-device-audio.eng)
AUDIO_PUB=$(get_config repos.evo-device-audio.public)
AUDIO_ARTS=$(get_config repos.evo-device-audio.artefacts)

UI_ENG=$(get_config repos.evo-device-audio-ui.eng)
UI_PUB=$(get_config repos.evo-device-audio-ui.public)
UI_ARTS=$(get_config repos.evo-device-audio-ui.artefacts)

RELEASE_KEY=$(get_config signing.release_key)
ACCEPTANCE_KEY=$(get_config signing.acceptance_key)
ACCEPTANCE_PUB=$(get_config signing.acceptance_public_key)

for path in "${SCHEMAS_ENG}" "${SCHEMAS_ARTS}" "${CORE_ENG}" "${CORE_PUB}" \
             "${AUDIO_ENG}" "${AUDIO_PUB}" "${AUDIO_ARTS}" \
             "${UI_ENG}" "${UI_PUB}" "${UI_ARTS}"; do
    [[ -d "${path}/.git" ]] || die "not a git checkout: ${path}"
done
[[ -r "${RELEASE_KEY}" ]] || die "release key not readable"
[[ -r "${ACCEPTANCE_KEY}" ]] || die "acceptance key not readable"
[[ -r "${ACCEPTANCE_PUB}" ]] || die "acceptance public key not readable"

PROMOTE_FLAGS=()
(( DRY_RUN == 1 )) && PROMOTE_FLAGS+=(--dry-run)
(( NO_PUSH == 1 )) && PROMOTE_FLAGS+=(--no-push)

run_pre_tag_check() {
    local repo_path="$1" repo_name="$2"
    local script="${repo_path}/scripts/release/pre-tag-check.sh"
    if [[ ! -x "${script}" ]]; then
        die "${repo_name}: pre-tag-check.sh missing at ${script}"
    fi
    log "${repo_name}: pre-tag-check"
    (cd "${repo_path}" && bash "${script}") || die "${repo_name}: pre-tag-check failed"
}

log "Cut order:"
log "  1. evo-catalogue-schemas"
log "  2. evo-core"
log "  3. evo-device-audio"
log "  4. evo-device-audio-ui"
log "Tag: ${TAG}"
log "Channel: ${CHANNEL}"
if (( DRY_RUN == 1 )); then
    log "Mode: dry-run (no repo mutations)"
fi

# ---- Step 1: evo-catalogue-schemas ----

log "=== Step 1/4: evo-catalogue-schemas ==="
run_pre_tag_check "${SCHEMAS_ENG}" "evo-catalogue-schemas"
(cd "${SCHEMAS_ENG}" && bash scripts/release/promote.sh \
    --tag "${TAG}" \
    --artefacts-repo "${SCHEMAS_ARTS}" \
    --channel "${CHANNEL}" \
    --signing-key "${RELEASE_KEY}" \
    "${PROMOTE_FLAGS[@]}") || die "evo-catalogue-schemas promote failed"

# ---- Step 2: evo-core ----

log "=== Step 2/4: evo-core ==="
run_pre_tag_check "${CORE_ENG}" "evo-core (eng)"
(cd "${CORE_ENG}" && bash scripts/release/promote.sh \
    --tag "${TAG}" \
    --public-repo "${CORE_PUB}" \
    "${PROMOTE_FLAGS[@]}") || die "evo-core promote failed"

# ---- Step 3: evo-device-audio ----

log "=== Step 3/4: evo-device-audio ==="

# Signed acceptance-evidence gate scope.
#
# The gate itself (dist/release/preflight-cut.sh) checks that every
# install/reset primitive × per-arch pair carries a signed evidence
# descriptor, and refuses the cut if any pair is missing or has a bad
# signature. The end-state design intent is production-tier
# supply-chain integrity: device maintainers verify that the shipped
# bundles were exercised on real hardware and the proof is signed
# with the acceptance key.
#
# The gate is scoped to activate at v0.1.16 and later cuts. For
# earlier cuts (v0.1.13, v0.1.14, v0.1.15) the operator performs a
# per-release readiness assessment out of band (documented alongside
# the release notes) and the automated gate does not block. This
# banner and the version guard below make the interim path
# unambiguous — the gate is not silently skipped; the operator sees
# in the log which path is active and why.
EVIDENCE_GATE_MIN_TAG="v0.1.16"
if [[ "$(printf '%s\n%s\n' "${TAG}" "${EVIDENCE_GATE_MIN_TAG}" | sort -V | head -1)" == "${EVIDENCE_GATE_MIN_TAG}" ]]; then
    log "  signed acceptance-evidence gate ACTIVE (TAG=${TAG} >= ${EVIDENCE_GATE_MIN_TAG})"
    log "  running dist/release/preflight-cut.sh"
    (cd "${AUDIO_ENG}" && bash dist/release/preflight-cut.sh \
        --version "${TAG}" \
        --arches "aarch64-unknown-linux-gnu x86_64-unknown-linux-gnu" \
        --evidence-dir "${AUDIO_ENG}/dist/release/evidence" \
        --public-key "${ACCEPTANCE_PUB}") || die "evo-device-audio: the release-cut install/reset primitives contract preflight refused"
else
    log "  ------------------------------------------------------------"
    log "  Signed acceptance-evidence gate is scoped for ${EVIDENCE_GATE_MIN_TAG}+."
    log "  This cut (TAG=${TAG}) predates that scope. The gate is not"
    log "  invoked. The operator is responsible for the per-release"
    log "  readiness assessment described alongside the release notes;"
    log "  continuing with the mechanical cut path."
    log "  ------------------------------------------------------------"
fi

run_pre_tag_check "${AUDIO_ENG}" "evo-device-audio (eng)"
(cd "${AUDIO_ENG}" && bash scripts/release/promote.sh \
    --tag "${TAG}" \
    --public-repo "${AUDIO_PUB}" \
    "${PROMOTE_FLAGS[@]}") || die "evo-device-audio promote failed"

# ---- Step 4: evo-device-audio-ui ----

log "=== Step 4/4: evo-device-audio-ui ==="
run_pre_tag_check "${UI_ENG}" "evo-ui-eng"
(cd "${UI_ENG}" && bash scripts/release/promote.sh \
    --tag "${TAG}" \
    --public-repo "${UI_PUB}" \
    "${PROMOTE_FLAGS[@]}") || die "evo-device-audio-ui promote failed"

log "PASS. Four-repo cut complete for ${TAG}."
