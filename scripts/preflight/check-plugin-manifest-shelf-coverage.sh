#!/usr/bin/env bash
#
# check-plugin-manifest-shelf-coverage.sh — preflight guard
# catching plugin manifests whose declared target shelf is not
# in the shipped distribution catalogue.
#
# The framework's admission gate walks every discovered plugin's
# `[target].shelf` declaration against the loaded catalogue and
# refuses any plugin whose shelf is not declared with a
# structured `admission error: target shelf not in catalogue:
# <shelf>` message. Boot-time observation is the first evidence
# the operator sees — too late for the release cut, and too late
# for the CI cycle that thinks the build was green.
#
# The failure mode is subtle: authoring a plugin against a
# named shelf, cross-building, signing, staging, deploying —
# every step passes because the plugin bundle itself is
# internally coherent. Only when the framework's admission
# gate walks the shelf-vs-catalogue join at boot does the gap
# surface.
#
# This preflight enumerates every reachable plugin manifest's
# `[target].shelf` (from sibling `evo-device-*` repos), the
# multi-stocking `[[stockings]].shelf` entries, and the
# per-catalogue `[[racks.shelves]] name = "..."` list, then
# asserts every plugin-declared shelf appears in at least one
# reachable catalogue.
#
# Exclusions:
#
#   - Example / test plugins under `crates/evo-example-*` are
#     excluded — pedagogical fixtures, not shipped code.
#
# Catalogue paths:
#
# Resolved via `EVO_DISTRIBUTION_CATALOGUE_PATHS`
# (space-separated absolute paths). When unset, defaults to
# walking the sibling repo layout:
#
#   ../evo-device-audio/dist/catalogue/*.toml
#
# Plugin manifest paths: same env override
# (`EVO_PLUGIN_MANIFEST_PATHS`) with the same sibling-repo
# default:
#
#   ../evo-device-audio/plugins/*/manifest.toml
#   ../evo-device-audio/plugins/*/manifest.oop.toml
#
# Missing paths are reported as "skipped" — the preflight
# cannot enforce against catalogues / manifests it cannot see.
# When zero catalogues OR zero manifests resolve the preflight
# WARNs and returns 0.
#
# Exits 0 when clean OR when preflight cannot resolve enough
# input to enforce. Exits 1 with a punch list when a plugin
# manifest declares a shelf that no reachable catalogue lists.

set -eo pipefail

REPO_ROOT="${REPO_ROOT:-$(git rev-parse --show-toplevel)}"
cd "$REPO_ROOT"

SCRIPT_NAME="check-plugin-manifest-shelf-coverage.sh"

# --------------------------------------------------------------
# Step 1: resolve manifest + catalogue path lists.
# --------------------------------------------------------------

RAW_MANIFEST_PATHS=()
if [[ -n "${EVO_PLUGIN_MANIFEST_PATHS:-}" ]]; then
    read -r -a RAW_MANIFEST_PATHS <<< "$EVO_PLUGIN_MANIFEST_PATHS"
else
    parent="$(cd .. && pwd)"
    for candidate in \
        "$parent/evo-device-audio/plugins"/*/manifest.toml \
        "$parent/evo-device-audio/plugins"/*/manifest.oop.toml \
        ; do
        if [[ -f "$candidate" ]]; then
            RAW_MANIFEST_PATHS+=("$candidate")
        fi
    done
fi

RAW_CATALOGUE_PATHS=()
if [[ -n "${EVO_DISTRIBUTION_CATALOGUE_PATHS:-}" ]]; then
    read -r -a RAW_CATALOGUE_PATHS <<< "$EVO_DISTRIBUTION_CATALOGUE_PATHS"
else
    parent="$(cd .. && pwd)"
    for candidate in \
        "$parent/evo-device-audio/dist/catalogue"/*.toml \
        ; do
        if [[ -f "$candidate" ]]; then
            RAW_CATALOGUE_PATHS+=("$candidate")
        fi
    done
fi

# Filter to files that actually exist. Missing env-supplied
# entries surface with WARN so an operator using an explicit
# list sees which paths could not be resolved.
MANIFEST_PATHS=()
SKIPPED_MANIFESTS=()
for candidate in "${RAW_MANIFEST_PATHS[@]}"; do
    if [[ -f "$candidate" ]]; then
        MANIFEST_PATHS+=("$candidate")
    else
        SKIPPED_MANIFESTS+=("$candidate")
    fi
done
CATALOGUE_PATHS=()
SKIPPED_CATALOGUES=()
for candidate in "${RAW_CATALOGUE_PATHS[@]}"; do
    if [[ -f "$candidate" ]]; then
        CATALOGUE_PATHS+=("$candidate")
    else
        SKIPPED_CATALOGUES+=("$candidate")
    fi
done
for skipped in "${SKIPPED_MANIFESTS[@]}"; do
    echo "$SCRIPT_NAME: WARN skipping unresolved manifest path: $skipped" >&2
done
for skipped in "${SKIPPED_CATALOGUES[@]}"; do
    echo "$SCRIPT_NAME: WARN skipping unresolved catalogue path: $skipped" >&2
done

if [[ ${#MANIFEST_PATHS[@]} -eq 0 ]]; then
    echo "$SCRIPT_NAME: WARN (no plugin manifests resolved; set EVO_PLUGIN_MANIFEST_PATHS to enforce)"
    exit 0
fi
if [[ ${#CATALOGUE_PATHS[@]} -eq 0 ]]; then
    echo "$SCRIPT_NAME: WARN (no distribution catalogues resolved; set EVO_DISTRIBUTION_CATALOGUE_PATHS to enforce)"
    exit 0
fi

# --------------------------------------------------------------
# Step 2: extract every plugin-declared shelf.
# --------------------------------------------------------------
#
# Manifests carry shelf declarations in one of two shapes:
#
#   1. `[target] shelf = "<name>"` — the single-shelf shape used
#      by plugins that stock exactly one shelf.
#   2. `[[stockings]] shelf = "<name>"` — the multi-stocking
#      shape used by plugins that stock several shelves from one
#      process (playback.mpd is the reference).
#
# Both are collected. A single plugin may declare both shapes
# (the top-level `[target]` co-exists with `[[stockings]]` in
# some legacy manifests); we treat each declaration
# independently.

# Extract shelf name from a manifest section. Uses awk to walk
# top-level and nested tables — the shelf key can appear inside
# `[target]` or `[[stockings]]`; both are `shelf = "..."` at
# their first-level.

extract_manifest_shelves() {
    local manifest="$1"
    awk '
        BEGIN { in_target=0; in_stocking=0 }
        /^\[target\]$/                    { in_target=1; in_stocking=0; next }
        /^\[\[stockings\]\]$/             { in_target=0; in_stocking=1; next }
        /^\[/ && !/^\[target\]$/ && !/^\[\[stockings\]\]$/ {
            in_target=0; in_stocking=0
        }
        (in_target || in_stocking) && /^shelf = "/ {
            match($0, /"[^"]*"/)
            name = substr($0, RSTART+1, RLENGTH-2)
            print name
        }
    ' "$manifest"
}

# Build "manifest_path\tshelf" list for every shelf a plugin
# declares. Duplicates across the two manifest variants
# (in-process / OOP) of the same plugin are expected and
# harmless — the coverage assertion is a set membership check.

declare -A DECLARED_SHELF_SEEN=()
DECLARED_SHELVES=()
for manifest in "${MANIFEST_PATHS[@]}"; do
    # Skip example / test crates.
    if [[ "$manifest" =~ /crates/evo-example- ]]; then
        continue
    fi
    while IFS= read -r shelf; do
        [[ -z "$shelf" ]] && continue
        key="$manifest|$shelf"
        if [[ -z "${DECLARED_SHELF_SEEN[$key]:-}" ]]; then
            DECLARED_SHELF_SEEN[$key]=1
            DECLARED_SHELVES+=("$key")
        fi
    done < <(extract_manifest_shelves "$manifest")
done

if [[ ${#DECLARED_SHELVES[@]} -eq 0 ]]; then
    echo "$SCRIPT_NAME: WARN (no plugin manifests declared any shelf; nothing to check)"
    exit 0
fi

# --------------------------------------------------------------
# Step 3: extract every catalogue-declared shelf.
# --------------------------------------------------------------
#
# Catalogue TOML shape:
#
#   [[racks]]
#   name = "<rack>"
#   ...
#   [[racks.shelves]]
#   name = "<shelf>"
#
# Each rack + shelf pair projects to "rack.shelf" full names —
# which is what plugin manifests reference in `shelf = "..."`.

declare -A KNOWN_SHELVES=()

for catalogue in "${CATALOGUE_PATHS[@]}"; do
    while IFS= read -r pair; do
        [[ -z "$pair" ]] && continue
        KNOWN_SHELVES[$pair]=1
    done < <(awk '
        BEGIN { rack=""; in_rack=0; in_shelf=0 }
        /^\[\[racks\]\]$/                       { in_rack=1; in_shelf=0; rack=""; next }
        /^\[\[racks\.shelves\]\]$/              { in_shelf=1; next }
        /^\[/ && !/^\[\[racks\]\]$/ && !/^\[\[racks\.shelves\]\]$/ {
            in_shelf=0
        }
        in_rack && !in_shelf && /^name = "/     {
            match($0, /"[^"]*"/)
            rack = substr($0, RSTART+1, RLENGTH-2)
        }
        in_shelf && /^name = "/ {
            match($0, /"[^"]*"/)
            shelf = substr($0, RSTART+1, RLENGTH-2)
            if (rack != "" && shelf != "") {
                print rack "." shelf
            }
        }
    ' "$catalogue")
done

if [[ ${#KNOWN_SHELVES[@]} -eq 0 ]]; then
    echo "$SCRIPT_NAME: WARN (no catalogue shelves declared across ${#CATALOGUE_PATHS[@]} catalogue(s); nothing to check)"
    exit 0
fi

# --------------------------------------------------------------
# Step 4: assert every declared shelf appears in the catalogue.
# --------------------------------------------------------------

VIOLATIONS=()

for entry in "${DECLARED_SHELVES[@]}"; do
    manifest="${entry%|*}"
    shelf="${entry#*|}"
    if [[ -z "${KNOWN_SHELVES[$shelf]:-}" ]]; then
        rel_manifest="${manifest#"$REPO_ROOT/"}"
        # For absolute paths outside the repo root, keep them
        # absolute (parameter-expansion strip returned original).
        VIOLATIONS+=("MISSING_SHELF: plugin manifest '$rel_manifest' declares shelf '$shelf' which no reachable distribution catalogue lists — the framework's admission gate will refuse the plugin at boot with 'target shelf not in catalogue: $shelf'")
    fi
done

# --------------------------------------------------------------
# Step 5: report.
# --------------------------------------------------------------

if [[ ${#VIOLATIONS[@]} -eq 0 ]]; then
    total_shelves=${#DECLARED_SHELVES[@]}
    total_known=${#KNOWN_SHELVES[@]}
    total_manifests=${#MANIFEST_PATHS[@]}
    total_catalogues=${#CATALOGUE_PATHS[@]}
    echo "$SCRIPT_NAME: OK ($total_shelves plugin-shelf declaration(s) across $total_manifests manifest(s) all satisfied by $total_known catalogue shelf/shelves across $total_catalogues catalogue(s))"
    exit 0
fi

echo "$SCRIPT_NAME: FAIL (${#VIOLATIONS[@]} violation(s))"
echo "  manifests scanned:"
for m in "${MANIFEST_PATHS[@]}"; do
    echo "    - $m"
done
echo "  catalogues scanned:"
for c in "${CATALOGUE_PATHS[@]}"; do
    echo "    - $c"
done
echo
echo "Punch list:"
for v in "${VIOLATIONS[@]}"; do
    echo "  - $v"
done
echo
echo "Remediation:"
echo "  Two ways to close each violation:"
echo
echo "  1. Add the missing shelf to the distribution catalogue:"
echo
echo "         [[racks.shelves]]"
echo "         name = \"<shelf-name>\""
echo "         shape = <n>"
echo "         description = \"...\""
echo
echo "     Add it inside the appropriate [[racks]] block so the"
echo "     full 'rack.shelf' name matches the plugin's declaration."
echo
echo "  2. Fix the plugin manifest's shelf declaration if the"
echo "     plugin should target a different existing shelf, or"
echo "     retire the plugin if no distribution owns that surface."
echo
echo "  The framework's admission gate walks the same join at"
echo "  boot; this preflight catches the gap at push time so"
echo "  operators do not surface it via 'admin diagnose returns"
echo "  state: not admitted' after a signed-and-deployed cycle."

exit 1
