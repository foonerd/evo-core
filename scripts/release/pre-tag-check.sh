#!/usr/bin/env bash
#
# pre-tag-check.sh — run the full pre-tag verification chain
# before the operator mints a release tag.
#
# Catches the failure modes that surface mid-release-cut and
# burn a CI cycle:
#
#   - cargo fmt drift (a manual edit landed without a follow-up
#     `cargo fmt --all` run; CI's --check fails on rewrap-style
#     differences).
#   - clippy regression under -D warnings.
#   - test failure under --locked (lockfile drift or genuine
#     regression).
#   - leak-gate hit (engineering-side identifiers or document
#     filenames appearing in framework source; full pattern list
#     lives in scripts/preflight/check-public-leaks.sh).
#   - Cargo.lock drift (manifest changed without a lockfile
#     refresh; CI's --locked guard refuses).
#   - plugin-manifest shelf-coverage gap (a plugin declares a
#     shelf that no reachable distribution catalogue lists;
#     the framework's admission gate refuses the plugin at
#     boot). Framework CI cannot enforce this — the
#     distribution catalogue and plugin manifests live in
#     sibling evo-device-* repos not present on the CI runner.
#     This pre-tag gate walks those siblings on the dev box.
#
# Run this immediately before `git tag <release>`. The check
# exits 0 only when every gate is clean. Exits 1 with the
# offending tool's output otherwise.
#
# Usage:
#
#   scripts/release/pre-tag-check.sh
#
# No arguments. Run from any subdirectory of the eng repo; the
# script computes its own repo root and runs the workspace
# checks against it.

set -euo pipefail

REPO_ROOT="$(cd "$(dirname "${BASH_SOURCE[0]}")/../.." && pwd)"
cd "${REPO_ROOT}"

log_step() {
    printf '\n[pre-tag] %s\n' "$*" >&2
}

log_ok() {
    printf '[pre-tag] OK: %s\n' "$*" >&2
}

log_fail() {
    printf '[pre-tag] FAIL: %s\n' "$*" >&2
}

# -------------------------------------------------------------
# Gate 1: cargo fmt --all -- --check
# -------------------------------------------------------------

log_step "Gate 1/7: cargo fmt --all -- --check"
if ! cargo fmt --all -- --check; then
    log_fail "cargo fmt drift detected"
    log_fail "Fix: cargo fmt --all"
    log_fail "Then commit the fmt delta and re-run this gate."
    exit 1
fi
log_ok "fmt clean"

# -------------------------------------------------------------
# Gate 2: cargo clippy --workspace --all-targets --locked -- -D warnings
# -------------------------------------------------------------

log_step "Gate 2/7: cargo clippy --workspace --all-targets --locked -- -D warnings"
if ! cargo clippy --workspace --all-targets --locked -- -D warnings; then
    log_fail "clippy reported warnings (treated as errors via -D warnings)"
    log_fail "Fix: address the lints; do not allow individual hits without an"
    log_fail "explicit, justified #[allow] attribute and a code-comment explaining why."
    exit 1
fi
log_ok "clippy clean"

# -------------------------------------------------------------
# Gate 3: cargo test --workspace --locked
# -------------------------------------------------------------

log_step "Gate 3/7: cargo test --workspace --locked"
if ! cargo test --workspace --locked; then
    log_fail "test failure"
    log_fail "Fix: re-run with output captured (cargo test --workspace --locked -- --nocapture)"
    log_fail "to see the offending assertion; address before tag mint."
    exit 1
fi
log_ok "tests pass"

# -------------------------------------------------------------
# Gate 4: leak grep
# -------------------------------------------------------------

log_step "Gate 4/7: scripts/preflight/check-public-leaks.sh"
if ! bash "${REPO_ROOT}/scripts/preflight/check-public-leaks.sh"; then
    log_fail "leak gate hit"
    log_fail "Fix: rewrite the offending lines as descriptive prose."
    log_fail "The leak script (above) named the exact pattern category and"
    log_fail "matching line; consult its output for the rewrite target."
    exit 1
fi
# (The leak script prints its own success / failure status; no
# additional log_ok line so the operator sees the script's voice
# rather than the wrapper's.)

# -------------------------------------------------------------
# Gate 5: cargo-lock fresh
# -------------------------------------------------------------

log_step "Gate 5/7: scripts/preflight/check-cargo-lock-fresh.sh"
if ! bash "${REPO_ROOT}/scripts/preflight/check-cargo-lock-fresh.sh"; then
    log_fail "Cargo.lock drift"
    log_fail "Fix: cargo metadata --format-version 1 > /dev/null"
    log_fail "Then commit the refreshed Cargo.lock."
    exit 1
fi

# -------------------------------------------------------------
# Gate 6: plugin-manifest shelf coverage against sibling
# distribution catalogues.
# -------------------------------------------------------------
#
# Walks every reachable plugin manifest's declared [target].shelf
# / [[stockings]].shelf against every reachable distribution
# catalogue's [[racks.shelves]] list, and refuses when any
# plugin declares a shelf that no distribution catalogue lists.
#
# The framework's admission gate walks the same join at boot;
# without this preflight the mismatch surfaces as
# `admission error: target shelf not in catalogue: <name>`
# only after a signed-and-deployed cycle to the rig.
#
# The script scans sibling `evo-device-*` repos by default; the
# dev-box layout has those siblings present. Override with
# EVO_PLUGIN_MANIFEST_PATHS + EVO_DISTRIBUTION_CATALOGUE_PATHS
# for non-default layouts.
#
# If no siblings resolve the script WARNs and returns 0 rather
# than hard-failing; on the tagging box the sibling repos are
# expected to be present, so operators should watch the WARN
# line if it appears — it flags a broken checkout layout that
# will let a shelf-gap regression through.

log_step "Gate 6/7: scripts/preflight/check-plugin-manifest-shelf-coverage.sh"
if ! bash "${REPO_ROOT}/scripts/preflight/check-plugin-manifest-shelf-coverage.sh"; then
    log_fail "plugin-manifest shelf coverage gap"
    log_fail "Fix: either add the missing shelf to the offending"
    log_fail "distribution catalogue under the correct [[racks]]"
    log_fail "block, or fix the plugin manifest's shelf declaration"
    log_fail "to reference an already-declared shelf. Punch list"
    log_fail "with per-manifest violations is above."
    exit 1
fi

# -------------------------------------------------------------
# Gate 7: Realised Status identifier freshness (journal ↔ eng)
#
# Greps Status-paragraph identifiers from Realised decision
# records in the sibling journal against eng source trees.
# Historical prose (activity logs, design documents, risk
# narratives) is exempt. Mode fail refuses the tag when a
# positively-claimed Status identifier is missing from HEAD source.
# -------------------------------------------------------------

log_step "Gate 7/7: scripts/preflight/check-identifier-freshness.sh (fail)"
if ! EVO_IDENTIFIER_FRESHNESS_MODE=fail \
    bash "${REPO_ROOT}/scripts/preflight/check-identifier-freshness.sh"; then
    log_fail "identifier freshness gap on Realised Status claims"
    log_fail "Fix: narrow the Status line to greppable shipped names,"
    log_fail "or ship the missing surface. Activity logs / design"
    log_fail "documents are not in scope of this gate."
    exit 1
fi

# -------------------------------------------------------------
# All gates clean
# -------------------------------------------------------------

cat >&2 <<'BANNER'

[pre-tag] All seven gates clean. Ready for tag mint.

Next step: mint the tag with the agreed format
  v<MAJOR>.<MINOR>.<PATCH>[.<CLOSURE>][-<PRERELEASE>]
e.g.
  v1.2.3       release
  v1.2.3.1     closure tag (point release)
  v1.2.4-rc.1  release-candidate prerelease

The tag-format regex enforced at the publish boundary:
  ^v[0-9]+\.[0-9]+\.[0-9]+(\.[0-9]+)?(-[0-9A-Za-z.-]+)?$

After tagging, run scripts/release/promote.sh to drive the
eng → public squash-and-scrub.
BANNER
