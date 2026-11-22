#!/usr/bin/env bash
#
# scripts/release/cut-release.sh — cut(V) workstation half.
#
# Operator input is --tag (+ channel). The machine:
#   0. Refuses unless every eng checkout is on the cut's own branch
#      (named vV) with a clean tree, before anything is written
#   1. Writes V into eng trees (CUT_VERSION / VERSION / pins)
#   2. Commits + tags eng tip as vV; tags the schemas and core eng
#      tips as vV too, so every promote archives the tip it is given
#   3. pre-tag-check + promote in order:
#        schemas → core → audio → evo-ui → evo-kiosk
#      Each promote pushes public tag AND creates GitHub Release
#      vV on that sibling. Tag alone is refused.
#   4. Dispatches public foonerd/evo-device-audio cut.yml with
#      tag + channel (sibling Releases belt-check → pieces →
#      wait → bake → channel __cut__)
#
# Each promote step is REFUSED if its pre-tag-check does not pass
# locally. Any refusal aborts the cut (no partial promotion).
#
# This script does not:
#   - Bootstrap missing artefact repos.
#   - Move GitHub Release Latest (acceptance is a later gate).
#   - Run the install/reset primitives on rigs. Evidence must
#     already exist under evo-device-audio/dist/release/evidence/
#     when the signed gate is active (v0.1.16+).
#
# Usage:
#   scripts/release/cut-release.sh \
#     --tag VERSION \
#     --config PATH \
#     [--channel {dev|test|prod}] \
#     [--prev-tag VERSION] \
#     [--allow-retag] \
#     [--dry-run] \
#     [--no-push] \
#     [--skip-public-cut]
#
# --config is a TOML file naming the eng and public checkout paths
# per repo + artefacts + signing keys. See cut-release.example.toml.
#
# --prev-tag is forwarded to every source-tree promote. Point releases
# must pass the last honest closed cut so auto-derive cannot latch onto premature shells.
#
# --allow-retag is required when a public tag of the same name
# already exists (remints a premature public tag of the same name).

set -euo pipefail

TAG=""
CONFIG=""
CHANNEL="dev"
PREV_TAG=""
ALLOW_RETAG=0
DRY_RUN=0
NO_PUSH=0
SKIP_PUBLIC_CUT=0

usage() {
    cat <<EOF >&2
Usage: $(basename "$0") \\
    --tag VERSION \\
    --config PATH \\
    [--channel {dev|test|prod}] \\
    [--prev-tag VERSION] \\
    [--allow-retag] \\
    [--dry-run] \\
    [--no-push] \\
    [--skip-public-cut]

cut(V): write V, promote five repos (tag + GitHub Release each),
dispatch public cut.yml (releases belt-check → pieces → bake → channel).
EOF
    exit 1
}

while [[ $# -gt 0 ]]; do
    case "$1" in
        --tag)             TAG="$2"; shift 2 ;;
        --config)          CONFIG="$2"; shift 2 ;;
        --channel)         CHANNEL="$2"; shift 2 ;;
        --prev-tag)        PREV_TAG="$2"; shift 2 ;;
        --allow-retag)     ALLOW_RETAG=1; shift ;;
        --dry-run)         DRY_RUN=1; shift ;;
        --no-push)         NO_PUSH=1; shift ;;
        --skip-public-cut) SKIP_PUBLIC_CUT=1; shift ;;
        -h|--help)         usage ;;
        *) echo "unknown argument: $1" >&2; usage ;;
    esac
done

[[ -z "${TAG}" ]]    && { echo "--tag required" >&2; exit 1; }
[[ -z "${CONFIG}" ]] && { echo "--config required" >&2; exit 1; }
[[ -r "${CONFIG}" ]] || { echo "config not readable: ${CONFIG}" >&2; exit 1; }

SCRIPT_DIR="$(cd "$(dirname "$0")" && pwd)"

# The identity a cut's own commits carry.
CUT_COMMIT_NAME="Just a nerd"
CUT_COMMIT_EMAIL="157698061+foonerd@users.noreply.github.com"

log() { printf '\n[cut-release] %s\n' "$*" >&2; }
die() { printf '\n[cut-release] REFUSE: %s\n' "$*" >&2; exit 1; }

# Accept v0.1.13.1 or 0.1.13.1; promote always wants the v-prefixed tag.
RAW_TAG="${TAG}"
case "${TAG}" in
    v*) ;;
    *) TAG="v${TAG}" ;;
esac
CUT_V="${TAG#v}"
if ! [[ "${CUT_V}" =~ ^[0-9]+\.[0-9]+\.[0-9]+\.[0-9]+$ ]]; then
    die "--tag must be vMAJOR.MINOR.PATCH.CUT (got ${RAW_TAG})"
fi

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

try_config() {
    python3 - "$1" "${CONFIG}" <<'PY' 2>/dev/null || true
import sys, tomllib
key = sys.argv[1]
with open(sys.argv[2], "rb") as f:
    data = tomllib.load(f)
cursor = data
try:
    for segment in key.split("."):
        cursor = cursor[segment]
    print(cursor)
except KeyError:
    pass
PY
}

SCHEMAS_ENG=$(get_config repos.evo-catalogue-schemas.eng)
SCHEMAS_ARTS=$(get_config repos.evo-catalogue-schemas.artefacts)

CORE_ENG=$(get_config repos.evo-core.eng)
CORE_PUB=$(get_config repos.evo-core.public)

AUDIO_ENG=$(get_config repos.evo-device-audio.eng)
AUDIO_PUB=$(get_config repos.evo-device-audio.public)
AUDIO_ARTS=$(get_config repos.evo-device-audio.artefacts)

# Prefer repos.evo-ui; accept legacy repos.evo-device-audio-ui and refuse
# if it still points at the retired evo-device-audio-ui public twin.
UI_ENG=$(try_config repos.evo-ui.eng)
UI_PUB=$(try_config repos.evo-ui.public)
if [[ -z "${UI_ENG}" || -z "${UI_PUB}" ]]; then
    UI_ENG=$(try_config repos.evo-device-audio-ui.eng)
    UI_PUB=$(try_config repos.evo-device-audio-ui.public)
fi
[[ -n "${UI_ENG}" && -n "${UI_PUB}" ]] || die "config must name repos.evo-ui.{eng,public}"
case "${UI_PUB}" in
    */evo-device-audio-ui|*/evo-device-audio-ui/)
        die "public glass ships from evo-ui, not evo-device-audio-ui. Point repos.evo-ui.public at evo-ui."
        ;;
esac

KIOSK_ENG=$(get_config repos.evo-kiosk.eng)
KIOSK_PUB=$(get_config repos.evo-kiosk.public)

RELEASE_KEY=$(get_config signing.release_key)
ACCEPTANCE_KEY=$(get_config signing.acceptance_key)
ACCEPTANCE_PUB=$(get_config signing.acceptance_public_key)

for path in "${SCHEMAS_ENG}" "${SCHEMAS_ARTS}" "${CORE_ENG}" "${CORE_PUB}" \
             "${AUDIO_ENG}" "${AUDIO_PUB}" "${AUDIO_ARTS}" \
             "${UI_ENG}" "${UI_PUB}" "${KIOSK_ENG}" "${KIOSK_PUB}"; do
    # Worktrees use a .git file, not a directory.
    git -C "${path}" rev-parse --is-inside-work-tree >/dev/null 2>&1 \
        || die "not a git checkout: ${path}"
done
[[ -r "${RELEASE_KEY}" ]] || die "release key not readable"
[[ -r "${ACCEPTANCE_KEY}" ]] || die "acceptance key not readable"
[[ -r "${ACCEPTANCE_PUB}" ]] || die "acceptance public key not readable"

PROMOTE_FLAGS=()
SCHEMAS_PROMOTE_FLAGS=()
(( DRY_RUN == 1 )) && { PROMOTE_FLAGS+=(--dry-run); SCHEMAS_PROMOTE_FLAGS+=(--dry-run); }
(( NO_PUSH == 1 )) && { PROMOTE_FLAGS+=(--no-push); SCHEMAS_PROMOTE_FLAGS+=(--no-push); }
# --allow-retag / --prev-tag are source-tree promote flags.
# schemas promote is artefacts-only and refuses unknown args.
(( ALLOW_RETAG == 1 )) && PROMOTE_FLAGS+=(--allow-retag)
if [[ -n "${PREV_TAG}" ]]; then
    PROMOTE_FLAGS+=(--prev-tag "${PREV_TAG}")
fi

run_pre_tag_check() {
    local repo_path="$1" repo_name="$2"
    local script="${repo_path}/scripts/release/pre-tag-check.sh"
    if [[ ! -x "${script}" ]]; then
        die "${repo_name}: pre-tag-check.sh missing at ${script}"
    fi
    log "${repo_name}: pre-tag-check"
    (cd "${repo_path}" && bash "${script}") || die "${repo_name}: pre-tag-check failed"
}

# Write V into eng tip, commit if dirty, mint/move eng tag vV.
# promote.sh archives that eng tag — without this, CUT_VERSION
# never reaches the public squash.
commit_cut_version_and_tag() {
    local repo_path="$1" repo_name="$2" writer="$3"
    log "${repo_name}: write cut(${CUT_V}) and tag ${TAG}"
    if (( DRY_RUN == 1 )); then
        log "${repo_name}: dry-run — would run ${writer} ${CUT_V} and tag ${TAG}"
        return 0
    fi
    (
        cd "${repo_path}"
        if [[ -n "${writer}" ]]; then
            [[ -x "${writer}" ]] || die "${repo_name}: missing ${writer}"
            bash "${writer}" "${CUT_V}"
        else
            # kiosk: VERSION file only
            printf '%s\n' "${CUT_V}" > VERSION
        fi
        if [[ -n "$(git status --porcelain)" ]]; then
            git add -A
            GIT_AUTHOR_NAME="${CUT_COMMIT_NAME}" \
                GIT_AUTHOR_EMAIL="${CUT_COMMIT_EMAIL}" \
                GIT_COMMITTER_NAME="${CUT_COMMIT_NAME}" \
                GIT_COMMITTER_EMAIL="${CUT_COMMIT_EMAIL}" \
                git commit -m "cut(${CUT_V}): set cut version"
        fi
        tag_head "${repo_name}"
    ) || die "${repo_name}: write cut version / tag failed"
}

# Mint (or, with --allow-retag, move) eng tag vV at HEAD of the current
# directory. Refuses a tag that already points elsewhere unless
# --allow-retag was given.
tag_head() {
    local repo_name="$1"
    if git rev-parse -q --verify "refs/tags/${TAG}" >/dev/null; then
        if (( ALLOW_RETAG == 1 )); then
            git tag -f "${TAG}" HEAD
        else
            local tip tagged
            tip="$(git rev-parse HEAD)"
            tagged="$(git rev-parse "${TAG}^{}")"
            if [[ "${tip}" != "${tagged}" ]]; then
                echo "${repo_name}: tag ${TAG} exists at ${tagged}, HEAD is ${tip}; pass --allow-retag" >&2
                exit 1
            fi
        fi
    else
        git tag "${TAG}" HEAD
    fi
}

# Tag an eng tip that carries no cut-version file (schemas, core), so
# its promote archives the tip the cut was given.
tag_cut_tip() {
    local repo_path="$1" repo_name="$2"
    log "${repo_name}: tag ${TAG} at its tip"
    if (( DRY_RUN == 1 )); then
        log "${repo_name}: dry-run — would tag ${TAG} at $(git -C "${repo_path}" rev-parse --short HEAD)"
        return 0
    fi
    (cd "${repo_path}" && tag_head "${repo_name}") \
        || die "${repo_name}: tag failed"
}

watch_workflow_run() {
    local repo="$1" workflow="$2" label="$3"
    local before run_id cand
    before="$(gh run list --repo "${repo}" --workflow="${workflow}" \
        --limit 1 --json databaseId --jq '.[0].databaseId // empty')"
    run_id=""
    for _ in $(seq 1 45); do
        sleep 4
        cand="$(gh run list --repo "${repo}" --workflow="${workflow}" \
            --limit 1 --json databaseId --jq '.[0].databaseId')"
        if [[ -n "${cand}" && "${cand}" != "${before}" ]]; then
            run_id="${cand}"
            break
        fi
    done
    [[ -n "${run_id}" ]] || die "no ${label} run appeared for ${workflow}"
    log "watching ${label} run ${run_id}"
    gh run watch "${run_id}" --repo "${repo}" --exit-status \
        || die "${label} run ${run_id} failed"
}

dispatch_public_cut() {
    log "=== Public cut.yml (mint → wait → bake) for ${TAG} ==="
    if (( SKIP_PUBLIC_CUT == 1 )); then
        log "skip-public-cut set; not dispatching public cut"
        return 0
    fi
    if (( DRY_RUN == 1 )) || (( NO_PUSH == 1 )); then
        log "dry-run/no-push — would: gh workflow run cut.yml -f tag=${TAG} -f channel=${CHANNEL}"
        return 0
    fi
    command -v gh >/dev/null || die "gh CLI required to dispatch public cut"

    # Piece mints + sibling Release belt-check + channel live inside
    # cut.yml. Workstation only kicks it; tag alone is not a cut.
    log "dispatch cut.yml -f tag=${TAG} -f channel=${CHANNEL}"
    gh workflow run cut.yml \
        --repo foonerd/evo-device-audio \
        -f "tag=${TAG}" \
        -f "channel=${CHANNEL}" \
        || die "failed to dispatch public cut.yml for ${TAG}"
    watch_workflow_run foonerd/evo-device-audio cut.yml "cut.yml through-and-through"
    log "public cut PASS for ${TAG} (releases + pieces + bake + channel ${CHANNEL})"
}

log "Cut order:"
log "  0. every eng checkout on ${TAG} and clean; write cut(${CUT_V}) on eng"
log "     audio / ui / kiosk; eng tag ${TAG} on all five"
log "  1. evo-catalogue-schemas"
log "  2. evo-core"
log "  3. evo-device-audio"
log "  4. evo-ui"
log "  5. evo-kiosk"
log "  6. public cut.yml (sibling Releases → pieces → bake → channel ${CHANNEL})"
log "Tag: ${TAG}"
log "Channel: ${CHANNEL}"
log "Prev-tag: ${PREV_TAG:-<auto>}"
log "Allow-retag: $([[ ${ALLOW_RETAG} -eq 1 ]] && echo yes || echo no)"
if (( DRY_RUN == 1 )); then
    log "Mode: dry-run (no repo mutations)"
fi

# ---- Step 0: every eng checkout on the cut's branch, then V reaches eng tags ----
log "=== Step 0: eng checkouts on ${TAG} ==="
bash "${SCRIPT_DIR}/check-cut-checkouts.sh" "${TAG}" \
    "${SCHEMAS_ENG}" "${CORE_ENG}" "${AUDIO_ENG}" "${UI_ENG}" "${KIOSK_ENG}" \
    || die "every eng checkout must be on branch ${TAG} with a clean tree before the cut writes anything"
commit_cut_version_and_tag "${AUDIO_ENG}" "evo-device-audio (eng)" \
    "${AUDIO_ENG}/scripts/release/set-cut-version.sh"
commit_cut_version_and_tag "${UI_ENG}" "evo-ui-eng" \
    "${UI_ENG}/scripts/release/set-cut-version.sh"
commit_cut_version_and_tag "${KIOSK_ENG}" "evo-kiosk-eng" ""
tag_cut_tip "${SCHEMAS_ENG}" "evo-catalogue-schemas"
tag_cut_tip "${CORE_ENG}" "evo-core (eng)"

# ---- Step 1: evo-catalogue-schemas ----

log "=== Step 1/5: evo-catalogue-schemas ==="
run_pre_tag_check "${SCHEMAS_ENG}" "evo-catalogue-schemas"
SCHEMAS_CHANNEL="${CHANNEL}"
if [[ ! -f "${SCHEMAS_ARTS}/channels/${SCHEMAS_CHANNEL}.toml" ]]; then
    # Artefacts today seed only channels/dev.toml. Do not invent a
    # missing pointer mid-cut; fall back to the seeded channel.
    log "schemas artefacts has no channels/${SCHEMAS_CHANNEL}.toml; using dev"
    SCHEMAS_CHANNEL="dev"
fi
(cd "${SCHEMAS_ENG}" && bash scripts/release/promote.sh \
    --tag "${TAG}" \
    --artefacts-repo "${SCHEMAS_ARTS}" \
    --channel "${SCHEMAS_CHANNEL}" \
    --signing-key "${RELEASE_KEY}" \
    "${SCHEMAS_PROMOTE_FLAGS[@]+"${SCHEMAS_PROMOTE_FLAGS[@]}"}") || die "evo-catalogue-schemas promote failed"

# ---- Step 2: evo-core ----

log "=== Step 2/5: evo-core ==="
run_pre_tag_check "${CORE_ENG}" "evo-core (eng)"
(cd "${CORE_ENG}" && bash scripts/release/promote.sh \
    --tag "${TAG}" \
    --public-repo "${CORE_PUB}" \
    "${PROMOTE_FLAGS[@]}") || die "evo-core promote failed"

# ---- Step 3: evo-device-audio ----

log "=== Step 3/5: evo-device-audio ==="

EVIDENCE_GATE_MIN_TAG="v0.1.16"
if [[ "$(printf '%s\n%s\n' "${TAG}" "${EVIDENCE_GATE_MIN_TAG}" | sort -V | head -1)" == "${EVIDENCE_GATE_MIN_TAG}" ]]; then
    log "  signed acceptance-evidence gate ACTIVE (TAG=${TAG} >= ${EVIDENCE_GATE_MIN_TAG})"
    log "  running dist/release/preflight-cut.sh"
    (cd "${AUDIO_ENG}" && bash dist/release/preflight-cut.sh \
        --version "${TAG}" \
        --arches "aarch64-unknown-linux-gnu x86_64-unknown-linux-gnu" \
        --evidence-dir "${AUDIO_ENG}/dist/release/evidence" \
        --public-key "${ACCEPTANCE_PUB}") || die "evo-device-audio: install/reset evidence preflight refused"
else
    log "  ------------------------------------------------------------"
    log "  Signed acceptance-evidence gate is scoped for ${EVIDENCE_GATE_MIN_TAG}+."
    log "  This cut (TAG=${TAG}) predates that scope. The gate is not"
    log "  invoked. The readiness report records the matrix + evidence"
    log "  posture out of band; continuing with the mechanical cut path."
    log "  ------------------------------------------------------------"
fi

run_pre_tag_check "${AUDIO_ENG}" "evo-device-audio (eng)"
(cd "${AUDIO_ENG}" && bash scripts/release/promote.sh \
    --tag "${TAG}" \
    --public-repo "${AUDIO_PUB}" \
    "${PROMOTE_FLAGS[@]}") || die "evo-device-audio promote failed"

# ---- Step 4: evo-ui (public glass) ----

log "=== Step 4/5: evo-ui ==="
run_pre_tag_check "${UI_ENG}" "evo-ui-eng"
(cd "${UI_ENG}" && bash scripts/release/promote.sh \
    --tag "${TAG}" \
    --public-repo "${UI_PUB}" \
    "${PROMOTE_FLAGS[@]}") || die "evo-ui promote failed"

# ---- Step 5: evo-kiosk ----

log "=== Step 5/5: evo-kiosk ==="
run_pre_tag_check "${KIOSK_ENG}" "evo-kiosk-eng"
(cd "${KIOSK_ENG}" && bash scripts/release/promote.sh \
    --tag "${TAG}" \
    --public-repo "${KIOSK_PUB}" \
    "${PROMOTE_FLAGS[@]}") || die "evo-kiosk promote failed"

dispatch_public_cut

log "PASS. cut(${CUT_V}) complete: eng promote + public pieces/bake for ${TAG}."
