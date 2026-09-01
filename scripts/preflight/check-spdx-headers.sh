#!/usr/bin/env bash
#
# check-spdx-headers.sh — preflight guard for the IP posture
# locked under the v0.1.13 release charter. Refuses any committed
# `.rs` file lacking an SPDX-License-Identifier on line 2, and
# refuses per-directory license inconsistency (more than one
# distinct SPDX identifier inside the same crate).
#
# Discipline mirrors `LICENSE` + `EXHIBIT_A` + the workspace
# `Cargo.toml` `license = "BUSL-1.1"` baseline + the nine
# per-crate `Apache-2.0` overrides recorded in the workspace
# manifest. Every shipped `.rs` source file in the framework
# carries one of these two identifiers, and every file inside a
# given crate carries the same identifier.
#
# Exits 0 when clean. Exits 1 with a punch list when any
# violation is detected. Run by CI on every PR; intended for
# pre-commit invocation when adding new files.
#
# Scope: `crates/**/*.rs` (the framework + SDK source). Tests
# under `tests/` and benches under `benches/` are included.
# Generated files under `target/` are excluded (the script never
# descends into `target/`).
#
# Failure modes the script catches:
#
#   1. New `.rs` file added without an SPDX-License-Identifier
#      header. The `dco.yml` CI workflow's DCO check is unrelated
#      — DCO catches missing `Signed-off-by` on the commit
#      message; this script catches missing license declaration
#      inside the source.
#
#   2. License inconsistency inside a crate. Each crate's
#      `Cargo.toml` `license = "..."` field is the contract; every
#      `.rs` file inside that crate must match. Mixing BUSL-1.1
#      and Apache-2.0 inside one crate is a configuration error
#      that publish.yml would surface as a license-detection
#      failure on crates.io.

set -euo pipefail

REPO_ROOT="${REPO_ROOT:-$(git rev-parse --show-toplevel)}"
cd "$REPO_ROOT"

ALLOWED_LICENSES=("BUSL-1.1" "Apache-2.0")

VIOLATIONS=()
SUMMARY_MISSING=0
SUMMARY_INCONSISTENT=0

is_allowed_license() {
    local needle="$1"
    for lic in "${ALLOWED_LICENSES[@]}"; do
        if [[ "$needle" == "$lic" ]]; then
            return 0
        fi
    done
    return 1
}

# Pass 1: every committed .rs under crates/ must carry an SPDX
# identifier on line 2 (line 1 is the Copyright header).
while IFS= read -r f; do
    # Skip non-existent files (deleted-on-this-commit edge).
    [[ -f "$f" ]] || continue
    spdx=$(sed -n '2p' "$f" | grep -oE 'SPDX-License-Identifier: \S+' | sed 's/^SPDX-License-Identifier: //' || true)
    if [[ -z "$spdx" ]]; then
        VIOLATIONS+=("MISSING_SPDX: $f")
        SUMMARY_MISSING=$((SUMMARY_MISSING + 1))
        continue
    fi
    if ! is_allowed_license "$spdx"; then
        VIOLATIONS+=("UNKNOWN_LICENSE: $f carries SPDX '$spdx' (allowed: ${ALLOWED_LICENSES[*]})")
        SUMMARY_MISSING=$((SUMMARY_MISSING + 1))
    fi
done < <(git ls-files 'crates/*/src/**/*.rs' 'crates/*/tests/**/*.rs' 'crates/*/benches/**/*.rs' 'crates/*/examples/**/*.rs' 2>/dev/null || true)

# Pass 2: per-crate consistency check. The set of SPDX identifiers
# inside one crate must be a singleton matching that crate's
# Cargo.toml `license = ...` field.
while IFS= read -r cargo_toml; do
    crate_root=$(dirname "$cargo_toml")
    declared=$(grep -E '^license\s*=' "$cargo_toml" | head -1 | sed -E 's/.*license\s*=\s*"([^"]+)".*/\1/' || true)
    if [[ -z "$declared" ]]; then
        # No explicit license field; the crate inherits from the
        # workspace. Skip the consistency check — the workspace
        # check belongs in the publish workflow's license-detection
        # step, not this preflight.
        continue
    fi
    found=$(
        find "$crate_root/src" "$crate_root/tests" "$crate_root/benches" "$crate_root/examples" \
            -maxdepth 32 -type f -name '*.rs' 2>/dev/null \
        | xargs -r -I{} sed -n '2p' {} 2>/dev/null \
        | grep -oE 'SPDX-License-Identifier: \S+' \
        | sort -u || true
    )
    distinct=$(echo "$found" | grep -c 'SPDX' || true)
    if [[ "$distinct" -gt 1 ]]; then
        VIOLATIONS+=("INCONSISTENT_LICENSE: $crate_root declares '$declared' but source files carry: $(echo "$found" | tr '\n' ' ')")
        SUMMARY_INCONSISTENT=$((SUMMARY_INCONSISTENT + 1))
    elif [[ "$distinct" -eq 1 ]]; then
        sole=$(echo "$found" | sed 's/^SPDX-License-Identifier: //')
        if [[ "$sole" != "$declared" ]]; then
            VIOLATIONS+=("DIVERGED_LICENSE: $crate_root Cargo.toml declares '$declared' but source files carry '$sole'")
            SUMMARY_INCONSISTENT=$((SUMMARY_INCONSISTENT + 1))
        fi
    fi
done < <(git ls-files 'crates/*/Cargo.toml' 2>/dev/null || true)

if [[ ${#VIOLATIONS[@]} -eq 0 ]]; then
    echo "check-spdx-headers.sh: OK (no SPDX-header violations)"
    exit 0
fi

echo "check-spdx-headers.sh: FAIL"
echo "  Missing-or-unknown SPDX on .rs files: $SUMMARY_MISSING"
echo "  Per-crate license inconsistencies:    $SUMMARY_INCONSISTENT"
echo
echo "Punch list (first 50):"
for v in "${VIOLATIONS[@]:0:50}"; do
    echo "  - $v"
done
if [[ ${#VIOLATIONS[@]} -gt 50 ]]; then
    echo "  ... and $((${#VIOLATIONS[@]} - 50)) more"
fi
echo
echo "Remediation:"
echo "  1. MISSING_SPDX: add the two-line header at the top of the file:"
echo "       // Copyright (c) <year> <name>"
echo "       // SPDX-License-Identifier: <BUSL-1.1|Apache-2.0>"
echo "     Pick the identifier matching the crate's Cargo.toml license field."
echo "  2. UNKNOWN_LICENSE: rewrite the SPDX line to one of:"
echo "       BUSL-1.1  (framework crates)"
echo "       Apache-2.0 (SDK / contract surface / pedagogical reference)"
echo "  3. INCONSISTENT_LICENSE / DIVERGED_LICENSE: choose the identifier the"
echo "     crate's Cargo.toml declares and rewrite every divergent source file"
echo "     to match. If the crate's intent has actually shifted, update"
echo "     Cargo.toml's license field AND every source file in lockstep."

exit 1
