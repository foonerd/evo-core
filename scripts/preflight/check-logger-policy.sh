#!/usr/bin/env bash
#
# check-logger-policy.sh — preflight gate for the LOGGING.md §2
# discipline. Refuses any new `tracing::error!` or `tracing::warn!`
# call site added since the last release tag that does not carry
# a paired rationale comment within the preceding window.
#
# Preflight-enforces the LOGGING.md §2 discipline that pins each
# level to a fixed meaning:
#
#   error = self-uncorrecting fault requiring operator attention
#   warn  = recoverable anomaly worth noticing
#   info  = lifecycle narrative (off by default)
#   debug = every action (off by default)
#   trace = fine-grained internals (off except chasing)
#
# Drift creeps in when developers reach for `error!` or `warn!`
# reflexively. The preflight catches new sites that lack the
# §2-aware classification justification.
#
# Heuristic: the script scans the diff between the most recent
# release tag (or `HEAD~1` as a fallback) and `HEAD` for added
# lines matching `tracing::(error|warn)!`. For each match, the
# script reads the 8 lines immediately preceding the call site
# and refuses the cut if NO line in that window matches one of:
#
#   - `LOGGING.md §2` (canonical rationale tag)
#   - `LOGGING §2`    (compact form)
#   - `logger-policy` (alternative rationale tag used in some
#                     legacy comments)
#
# The 8-line window matches the rationale comment styles already
# present in the repo (see `crates/evo/src/wire_client.rs`'s
# `is_wire_peer_disconnect` arm + adjacent comments).
#
# Files exempt from the check:
#   - Test modules: `tests/`, `*/tests.rs`, `tests/integration/`,
#     `#[cfg(test)]` blocks (per-line CFG detection is too
#     expensive for a preflight; tests as a directory exemption
#     covers the common case).
#   - Example crates under `crates/evo-example-*/` (pedagogical
#     references; their warn/error patterns deliberately mirror
#     production code shapes for teaching).
#
# Exits 0 when clean. Exits 1 with a punch list when any new call
# lacks rationale. Run by CI on every PR; intended for pre-tag
# invocation as part of `scripts/release/pre-tag-check.sh`.

set -euo pipefail

REPO_ROOT="${REPO_ROOT:-$(git rev-parse --show-toplevel)}"
cd "$REPO_ROOT"

# Resolve the baseline ref for the diff. Precedence:
#   1. LOGGER_POLICY_BASE_REF env var (release-cut sets this to
#      the previous closure tag, e.g. v0.1.12.1).
#   2. scripts/preflight/.logger-policy-baseline (one line, ref
#      name). Used to pin the baseline for the entire cycle so
#      every PR's diff is small.
#   3. Most recent reachable RELEASE tag (matches `v[0-9]*`).
#      Skips internal tags like `base/...`.
#   4. HEAD~1 fallback for shallow / no-tag scenarios.
BASE_REF="${LOGGER_POLICY_BASE_REF:-}"
if [[ -z "$BASE_REF" ]] && [[ -f "scripts/preflight/.logger-policy-baseline" ]]; then
    BASE_REF=$(head -1 "scripts/preflight/.logger-policy-baseline" | tr -d ' \t\n\r')
fi
if [[ -z "$BASE_REF" ]]; then
    # `git describe --match` filters tags via glob; we want only
    # release-shaped tags. `git for-each-ref` over `refs/tags/v*`
    # sorted by creator-date desc is the more reliable mechanism
    # because it ignores branch / non-release tags entirely.
    BASE_REF=$(git for-each-ref --sort=-creatordate --format='%(refname:short)' --count=1 'refs/tags/v[0-9]*' 2>/dev/null || true)
fi
if [[ -z "$BASE_REF" ]]; then
    BASE_REF="HEAD~1"
fi

# Guard the rationale window size (lines above the call site).
WINDOW="${LOGGER_POLICY_WINDOW:-8}"

# Rationale-marker regex (case-insensitive). The script accepts
# either the canonical `LOGGING.md §2` form or a compact form.
RATIONALE_RE='LOGGING\.md §2|LOGGING §2|logger-policy|LOGGING\.md section 2'

# Build the diff against the base ref. `git diff -U0` strips
# context lines so we only see added/removed; the script then
# filters to added (+) `tracing::error!`/`tracing::warn!` lines
# and resolves their file + line number.
diff_output=$(git diff -U0 "$BASE_REF"...HEAD 2>/dev/null || true)
if [[ -z "$diff_output" ]]; then
    echo "check-logger-policy.sh: OK (no diff between $BASE_REF and HEAD)"
    exit 0
fi

VIOLATIONS=()

current_file=""
current_line=0

while IFS= read -r line; do
    # File header: +++ b/path/to/file
    if [[ "$line" =~ ^\+\+\+\ b/(.+) ]]; then
        current_file="${BASH_REMATCH[1]}"
        # Skip test exemptions + example crates.
        if [[ "$current_file" =~ /tests/ ]] || [[ "$current_file" =~ tests\.rs$ ]] || [[ "$current_file" =~ ^crates/evo-example- ]]; then
            current_file=""
        fi
        continue
    fi
    # Hunk header: @@ -A,B +C,D @@
    if [[ "$line" =~ ^@@\ -[0-9,]+\ \+([0-9]+) ]]; then
        current_line="${BASH_REMATCH[1]}"
        continue
    fi
    # Skip if we're not in a file we care about.
    [[ -z "$current_file" ]] && continue
    # Track line numbers through the hunk. Each non-removed line
    # increments the counter; removed lines (-) do not.
    if [[ "$line" =~ ^\+([^+]|$) ]] && [[ ! "$line" =~ ^\+\+\+ ]]; then
        # Added line. Check if it's a tracing::error! or tracing::warn! call.
        added_content="${line:1}"
        if [[ "$added_content" =~ tracing::(error|warn)! ]]; then
            level="${BASH_REMATCH[1]}"
            # Only consider .rs files. Skip non-source diffs.
            if [[ "$current_file" =~ \.rs$ ]]; then
                # Read the rationale window from the working tree.
                if [[ -f "$current_file" ]]; then
                    start=$((current_line - WINDOW))
                    [[ "$start" -lt 1 ]] && start=1
                    window_text=$(sed -n "${start},${current_line}p" "$current_file" 2>/dev/null || true)
                    if ! echo "$window_text" | grep -qiE "$RATIONALE_RE"; then
                        VIOLATIONS+=("MISSING_RATIONALE: $current_file:$current_line — new tracing::$level! call lacks LOGGING.md §2 rationale within $WINDOW lines above")
                    fi
                fi
            fi
        fi
        current_line=$((current_line + 1))
    elif [[ "$line" =~ ^\  ]]; then
        # Context line; counts for line numbering.
        current_line=$((current_line + 1))
    fi
done <<< "$diff_output"

if [[ ${#VIOLATIONS[@]} -eq 0 ]]; then
    echo "check-logger-policy.sh: OK (no new tracing::error!/warn! sites without §2 rationale; base: $BASE_REF)"
    exit 0
fi

echo "check-logger-policy.sh: FAIL (${#VIOLATIONS[@]} violation(s); base: $BASE_REF)"
echo
echo "Punch list:"
for v in "${VIOLATIONS[@]}"; do
    echo "  - $v"
done
echo
echo "Remediation:"
echo "  Per docs/engineering/LOGGING.md §2:"
echo "    error = self-uncorrecting fault requiring operator attention"
echo "    warn  = recoverable anomaly worth noticing"
echo "    info  = lifecycle narrative (off by default)"
echo "    debug = every action (off by default)"
echo "    trace = fine-grained internals"
echo
echo "  For each call site flagged above:"
echo "    (a) confirm the chosen level matches the §2 meaning, then"
echo "    (b) add a comment within $WINDOW lines above naming 'LOGGING.md §2'"
echo "        and the specific classification reasoning, e.g."
echo "          // LOGGING.md §2: warn (recoverable; we retry below)"
echo
echo "  If the chosen level is wrong (most common: error used for"
echo "  peer-disconnect / lifecycle, warn used for normal exit), demote"
echo "  to info/debug and the preflight will pass."

exit 1
