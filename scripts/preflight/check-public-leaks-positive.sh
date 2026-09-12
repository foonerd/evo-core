#!/usr/bin/env bash
#
# check-public-leaks-positive.sh — prove the public-leak gate fires.
#
# A clean run of check-public-leaks.sh is not evidence. It is a
# negative from an instrument nobody has shown can report a
# positive, and a class that matches nothing passes every file ever
# written. A missing class and a broken class look identical from
# outside, and both look like success.
#
# This reads the class list out of the gate itself, plants a sample
# for each one in a throwaway tree laid out like the scanned paths,
# and asserts the gate reports that class by name. Being rejected is
# not enough on its own: without the name, one class could be
# credited for a hit that another class found.
#
# A class added to the gate without a plant here fails this control
# rather than passing unproven.
#
# The working tree is never touched: the gate is copied into a
# temporary directory and run from there.
#
# Exits 0 only when every class is proved and the unplanted tree is
# clean.

set -euo pipefail

REPO_ROOT="$(cd "$(dirname "${BASH_SOURCE[0]}")/../.." && pwd)"
GATE="${REPO_ROOT}/scripts/preflight/check-public-leaks.sh"
if [[ ! -f "${GATE}" ]]; then
    echo "positive-control: gate missing: ${GATE}" >&2
    exit 1
fi

# The class labels, in the gate's own words, read from the gate so
# the two cannot drift. Handles both layouts the gate uses: the
# label inline after `scan_pattern`, and the label on the
# continuation line below it.
mapfile -t CLASSES < <(
    awk '
        /scan_pattern/ {
            if (match($0, /scan_pattern[[:space:]]+"[^"]*"/)) {
                s = substr($0, RSTART, RLENGTH)
                sub(/^scan_pattern[[:space:]]+"/, "", s)
                sub(/"$/, "", s)
                print s
                want = 0
                next
            }
            if ($0 ~ /\\[[:space:]]*$/) { want = 1 }
            next
        }
        want {
            if (match($0, /"[^"]*"/)) {
                print substr($0, RSTART + 1, RLENGTH - 2)
            }
            want = 0
        }
    ' "${GATE}"
)
if [[ ${#CLASSES[@]} -eq 0 ]]; then
    echo "positive-control: no classes found in ${GATE}." >&2
    exit 1
fi

# Sample text for each class, keyed by the label the gate prints.
# Two parallel lists rather than a `case`, because a case label is a
# glob and these carry parentheses and other characters a glob would
# reinterpret.
SAMPLE_KEYS=(
    "ADR identifiers in source (rewrite descriptively)"
    "Engineering-side document filenames in source"
    "Release-prep narrative term 'closure-debt' (rewrite as the framework property)"
    "Buildout-phase identifiers (Phase X.Y) — rewrite descriptively"
    "Parked-decision identifiers (PD-NNN) — rewrite descriptively"
    "Risk-register identifiers (R-NNN) — rewrite descriptively"
    "GAPS document references — rewrite descriptively"
)
SAMPLE_VALS=(
    "see ADR-0161 for the rationale"
    "recorded in SESSION_LOG"
    "carried as closure-debt"
    "landed in Phase 2.1"
    "PD-017 covers this"
    "R-042 covers this"
    "listed in GAPS"
)

sample_for() {
    local want="$1" i
    for i in "${!SAMPLE_KEYS[@]}"; do
        # POSIX string equality: `[[ = ]]` would glob the right side.
        if [ "${SAMPLE_KEYS[$i]}" = "${want}" ]; then
            printf '%s' "${SAMPLE_VALS[$i]}"
            return 0
        fi
    done
    return 1
}

WORKDIR="$(mktemp -d "${TMPDIR:-/tmp}/leak-positive.XXXXXX")"
# shellcheck disable=SC2317  # invoked via the EXIT trap
cleanup() { rm -rf "${WORKDIR}"; }
trap cleanup EXIT

# Mirror enough of the repository for the gate to run: it resolves
# its own root from where it sits and scans a fixed list of paths
# relative to that root.
mkdir -p "${WORKDIR}/scripts/preflight"
cp "${GATE}" "${WORKDIR}/scripts/preflight/check-public-leaks.sh"
chmod +x "${WORKDIR}/scripts/preflight/check-public-leaks.sh"
# The scanned paths, however the gate happens to declare the array —
# all on one line, or one entry per line. Bounded to the array
# itself: a looser read picks up the exclusion list further down and
# plants into a directory nothing scans, which looks exactly like a
# class that does not fire.
mapfile -t SCANNED < <(
    awk '
        /SCAN_PATHS=\(/ { inarr = 1 }
        inarr {
            line = $0
            sub(/^.*SCAN_PATHS=\(/, "", line)
            sub(/#.*$/, "", line)
            if (match(line, /\)/)) {
                line = substr(line, 1, RSTART - 1)
                done = 1
            }
            while (match(line, /"[^"]*"/)) {
                print substr(line, RSTART + 1, RLENGTH - 2)
                line = substr(line, RSTART + RLENGTH)
            }
            if (done) { exit }
        }
    ' "${GATE}"
)
if [[ ${#SCANNED[@]} -eq 0 ]]; then
    echo "positive-control: no SCAN_PATHS found in ${GATE}." >&2
    exit 1
fi
for d in "${SCANNED[@]}"; do
    mkdir -p "${WORKDIR}/${d}"
done

# The plant lands in the first scanned path, under an extension the
# gate includes.
PLANT_FILE="${WORKDIR}/${SCANNED[0]}/positive-control-plant.md"

run_gate() {
    ( cd "${WORKDIR}" \
        && bash "scripts/preflight/check-public-leaks.sh" ) \
        >"${WORKDIR}/out.txt" 2>&1
}

# The unplanted case first: if an empty tree were reported as
# failing, every assertion below would pass for the wrong reason.
rm -f "${PLANT_FILE}"
if ! run_gate; then
    echo "positive-control: an unplanted tree FAILED." >&2
    cat "${WORKDIR}/out.txt" >&2
    exit 1
fi

for class in "${CLASSES[@]}"; do
    if ! sample="$(sample_for "${class}")"; then
        echo "positive-control: no plant for class: ${class}" >&2
        echo "The gate defines it and nothing here triggers it, so" >&2
        echo "it would pass unproven. Add a sample to SAMPLE_KEYS /" >&2
        echo "SAMPLE_VALS." >&2
        exit 1
    fi
    printf '%s\n' "${sample}" >"${PLANT_FILE}"
    if run_gate; then
        echo "positive-control: planted '${sample}' was CLEAN." >&2
        echo "The class ${class} matches nothing." >&2
        cat "${WORKDIR}/out.txt" >&2
        exit 1
    fi
    if ! grep -Fq "=== ${class} ===" "${WORKDIR}/out.txt"; then
        echo "positive-control: the tree was rejected but ${class}" >&2
        echo "was not the class that reported it." >&2
        cat "${WORKDIR}/out.txt" >&2
        exit 1
    fi
done

rm -f "${PLANT_FILE}"
if ! run_gate; then
    echo "positive-control: the tree still FAILS after unplanting." >&2
    cat "${WORKDIR}/out.txt" >&2
    exit 1
fi

echo "positive-control: ${#CLASSES[@]} classes caught; unplanted tree clean."
exit 0
