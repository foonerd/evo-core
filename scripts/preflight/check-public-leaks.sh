#!/usr/bin/env bash
#
# check-public-leaks.sh — fail-fast guard against journal-voice leaks
# in framework source. Run before every commit that touches
# crates/evo*, crates/evo-plugin-sdk*, crates/evo-plugin-tool*,
# acceptance/, scripts/, or dist/. Run by CI on every PR.
#
# Catches the failure modes that have bitten this codebase before:
#
#   1. ADR references in source / config / SQL / scripts. The
#      framework source is published to the public release repo via
#      squash-and-scrub; ADR identifiers must not appear in the
#      shipped artefacts. Rewrite the constraint descriptively
#      ("plugin-defined ledger ids are not permitted") rather than
#      "(per ADR-XXXX)".
#
#   2. Internal-repo path references (the engineering-side
#      decision repository, its session log, its risk register, its
#      scope document, its parked-decisions file, its vendor
#      extension catalogue). These files live only in the engineering
#      repository; mentioning them in source leaks process narrative
#      that the public release should not carry.
#
#   3. The literal string "closure-debt" / "closure debt" — a
#      release-prep narrative term that describes how engineering
#      thinks about prior-release follow-on work, not a property of
#      the framework. Source code should describe the framework's
#      current behaviour, not its release history.
#
# Exits 0 when clean. Exits 1 with a punch list when any pattern
# matches. Patterns are intentionally tight: this gate is a
# discipline reinforcement, not a style checker, and false positives
# are worse than false negatives at this scope.

set -eo pipefail

REPO_ROOT="$(cd "$(dirname "${BASH_SOURCE[0]}")/../.." && pwd)"
cd "${REPO_ROOT}"

# Scopes to scan. Excludes the engineering-side journal repo
# (which is a sibling, not under this repo) and the test-data
# trees inside acceptance fixtures whose payload strings legitimately
# encode framework version references.
SCAN_PATHS=(
    "crates/evo"
    "crates/evo-plugin-sdk"
    "crates/evo-plugin-tool"
    "crates/evo-trust"
    "crates/evo-acceptance"
    "crates/evo-acceptance-synthetic"
    "crates/evo-acceptance-distribution"
    "crates/evo-coalesce-labels"
    "crates/evo-os-clock"
    "crates/evo-loom"
    "crates/evo-example-admin"
    "crates/evo-example-distribution"
    "crates/evo-example-echo"
    "crates/evo-example-factory"
    "crates/evo-example-warden"
    "crates/evo-plugin-test"
    "acceptance"
    "scripts"
    "dist"
)

# File extensions to scan.
SCAN_EXTS=(
    "*.rs"
    "*.toml"
    "*.md"
    "*.sql"
    "*.sh"
    "*.json"
    "*.yaml"
    "*.yml"
)

# Build a single grep include argument list.
INCLUDE_ARGS=()
for ext in "${SCAN_EXTS[@]}"; do
    INCLUDE_ARGS+=("--include=${ext}")
done

# Build a single grep exclude argument list (skip generated /
# build trees that may temporarily contain reformatted output).
EXCLUDE_ARGS=(
    "--exclude-dir=target"
    "--exclude-dir=.cargo"
    "--exclude-dir=node_modules"
    # The preflight script itself encodes the patterns it scans
    # for; excluding it is the only way the script can co-exist
    # with the gate.
    "--exclude=check-public-leaks.sh"
)

declare -a FAILURES=()

scan_pattern() {
    local label="$1"
    local pattern="$2"
    local hits
    hits=$(grep -rEn "${pattern}" \
        "${INCLUDE_ARGS[@]}" "${EXCLUDE_ARGS[@]}" \
        "${SCAN_PATHS[@]}" 2>/dev/null || true)
    if [[ -n "${hits}" ]]; then
        FAILURES+=("=== ${label} ===")
        FAILURES+=("${hits}")
        FAILURES+=("")
    fi
}

# Pattern 1: ADR-XXXX references (case-insensitive on the prefix
# so adr-0091 in a marker string is also caught).
scan_pattern \
    "ADR identifiers in source (rewrite descriptively)" \
    '[Aa][Dd][Rr]-[0-9]{3,}'

# Pattern 2: Engineering-side repository path references. The exact
# repository names are deliberately not hard-coded in this script
# (so the script does not itself encode them). Patterns target the
# document filenames that live only in that repository.
scan_pattern \
    "Engineering-side document filenames in source" \
    '\b(SESSION_LOG|RISKS|PARKED_DECISIONS|V0\.[0-9]+\.[0-9]+_SCOPE|VENDOR_EXTENSION_OPTIONS)\b'

# Pattern 3: "closure-debt" release-prep narrative term.
scan_pattern \
    "Release-prep narrative term 'closure-debt' (rewrite as the framework property)" \
    'closure-debt|closure debt'

# Pattern 4: Buildout-phase identifiers. "Phase 1.E", "Phase 2.K",
# "Phase A.1", etc. Refer to engineering-side phased plan documents
# that the public reader cannot consult. Rewrite as descriptive
# prose naming the framework primitive or behaviour the phase
# delivered.
scan_pattern \
    "Buildout-phase identifiers (Phase X.Y) — rewrite descriptively" \
    'Phase [0-9]+\.[A-Za-z0-9]+|Phase [A-Z]\.[0-9]+'

# Pattern 5: Parked-decision identifiers. Register entries live
# on the engineering side only.
scan_pattern \
    "Parked-decision identifiers (PD-NNN) — rewrite descriptively" \
    '\bPD-[0-9]+\b'

# Pattern 6: Risk-register identifiers. Register entries live on
# the engineering side only. Three-digit threshold avoids false
# matches on legitimate version-style tokens (R-1.2.3 etc.) and
# on the literal "R-NNN" pattern strings inside this script.
scan_pattern \
    "Risk-register identifiers (R-NNN) — rewrite descriptively" \
    '\bR-[0-9]{3,}\b'

# Pattern 7: GAPS references — engineering-side gaps document.
scan_pattern \
    "GAPS document references — rewrite descriptively" \
    '\bGAPS\b'

if [[ ${#FAILURES[@]} -gt 0 ]]; then
    echo
    echo "PUBLIC-LEAK CHECK FAILED."
    echo
    echo "The patterns below appear in framework source / config / scripts."
    echo "These trees ship to the public release repository; rewrite the"
    echo "matching lines as descriptive prose (state the constraint or the"
    echo "framework's current behaviour, not which engineering document"
    echo "decided it or which release first surfaced it)."
    echo
    printf '%s\n' "${FAILURES[@]}"
    echo
    echo "Run again after rewriting; the gate exits 0 only when zero hits."
    exit 1
fi

echo "public-leak check: clean."
