#!/usr/bin/env bash
#
# check-acceptance-descriptor-leaks.sh — preflight guard catching
# host / user placeholder hygiene violations on committed
# acceptance target descriptors and the GitHub Actions workflows
# that consume them.
#
# Preflight-enforces the placeholder + overlay-loader discipline
# that keeps operator-specific host / user values out of committed
# acceptance descriptors.
#
# Rules:
#
#   1. Every connection-bearing field on a committed
#      `acceptance/targets/<name>.toml` MUST hold the sentinel
#      `REPLACE_ME`. Real host / user / key_path values live in
#      a sibling `<name>.local.toml` overlay file that is
#      gitignored and never committed. The loader merges the
#      overlay on top and refuses to start if `REPLACE_ME`
#      sentinels survive the merge.
#
#   2. No `.toml` / `.yml` / `.yaml` / `.sh` / `.py` file in the
#      tree (excluding under `acceptance/targets/*.local.toml`,
#      which is gitignored and unreachable from `git ls-files`)
#      may carry a literal `192.168.30.*` IP. The validation-rig
#      address book is canonical operator infrastructure and must
#      not bleed into the committed source.
#
#   3. No file may carry a literal `evoproto@` user/host pair.
#      Same rationale: operator-specific.
#
# Exits 0 when clean. Exits 1 with a punch list when any violation
# is detected. Run by CI on every PR; intended for pre-commit
# invocation when adding or editing acceptance descriptors,
# workflows, or scripts.

set -euo pipefail

REPO_ROOT="${REPO_ROOT:-$(git rev-parse --show-toplevel)}"
cd "$REPO_ROOT"

VIOLATIONS=()

# Rule 1 — committed acceptance descriptors must carry REPLACE_ME
# in connection-bearing fields. The set of guarded fields covers
# the SSH transport path the harness uses today; extend if a new
# connection_type lands.
GUARDED_FIELDS=("host" "user" "key_path")
while IFS= read -r f; do
    # Skip gitignored local overlays — they are operator
    # machine-specific and intentionally hold real values.
    if [[ "$f" == *.local.toml ]]; then
        continue
    fi
    in_ssh_block=0
    line_no=0
    while IFS= read -r line; do
        line_no=$((line_no + 1))
        if [[ "$line" =~ ^\[ssh\] ]]; then
            in_ssh_block=1
            continue
        fi
        # Reset on next [section].
        if [[ "$line" =~ ^\[[^][]+\] ]] && [[ "$in_ssh_block" -eq 1 ]] && [[ ! "$line" =~ ^\[ssh\] ]]; then
            in_ssh_block=0
            continue
        fi
        [[ "$in_ssh_block" -eq 1 ]] || continue
        for field in "${GUARDED_FIELDS[@]}"; do
            if [[ "$line" =~ ^[[:space:]]*${field}[[:space:]]*=[[:space:]]*\"([^\"]+)\" ]]; then
                value="${BASH_REMATCH[1]}"
                if [[ "$value" != "REPLACE_ME" ]]; then
                    VIOLATIONS+=("LIVE_VALUE_IN_DESCRIPTOR: $f:$line_no — field '$field' carries '$value' (must be REPLACE_ME on committed descriptor)")
                fi
            fi
        done
    done < "$f"
done < <(git ls-files 'acceptance/targets/*.toml' 2>/dev/null || true)

# Rule 2 — no literal 192.168.30.X IP outside gitignored
# local overlays. Pattern matches the operator rig's /24 only;
# example IPs in docs (e.g. RFC 5737 198.51.100.X or 192.0.2.X)
# are unrestricted.
while IFS= read -r f; do
    # The preflight script itself names the pattern in its rules
    # documentation; exempt this script.
    [[ "$f" == "scripts/preflight/check-acceptance-descriptor-leaks.sh" ]] && continue
    matches=$(grep -nE '192\.168\.30\.[0-9]+' "$f" 2>/dev/null || true)
    if [[ -n "$matches" ]]; then
        while IFS= read -r m; do
            VIOLATIONS+=("RIG_IP_LEAK: $f:$m")
        done <<< "$matches"
    fi
done < <(git ls-files '*.toml' '*.yml' '*.yaml' '*.sh' '*.py' 2>/dev/null | grep -v '\.local\.toml$' || true)

# Rule 3 — no literal evoproto@ in committed sources.
while IFS= read -r f; do
    [[ "$f" == "scripts/preflight/check-acceptance-descriptor-leaks.sh" ]] && continue
    matches=$(grep -nE 'evoproto@' "$f" 2>/dev/null || true)
    if [[ -n "$matches" ]]; then
        while IFS= read -r m; do
            VIOLATIONS+=("SERVICE_USER_LEAK: $f:$m")
        done <<< "$matches"
    fi
done < <(git ls-files '*.toml' '*.yml' '*.yaml' '*.sh' '*.py' '*.md' 2>/dev/null | grep -v '\.local\.toml$' || true)

if [[ ${#VIOLATIONS[@]} -eq 0 ]]; then
    echo "check-acceptance-descriptor-leaks.sh: OK (no leaks)"
    exit 0
fi

echo "check-acceptance-descriptor-leaks.sh: FAIL (${#VIOLATIONS[@]} violation(s))"
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
echo "  1. LIVE_VALUE_IN_DESCRIPTOR: rewrite the field to REPLACE_ME in the"
echo "     committed descriptor. Move the real value into a sibling"
echo "     '<name>.local.toml' overlay that .gitignore excludes."
echo "  2. RIG_IP_LEAK: replace the literal 192.168.30.X address with an"
echo "     environment-variable reference or move the value into a"
echo "     gitignored local overlay. The rig address book is operator"
echo "     infrastructure and must not appear in committed sources."
echo "  3. SERVICE_USER_LEAK: replace the literal evoproto@ user@host"
echo "     reference with an environment-variable substitution."

exit 1
