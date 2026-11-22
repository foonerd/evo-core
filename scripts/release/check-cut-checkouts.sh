#!/usr/bin/env bash
#
# check-cut-checkouts.sh — refuse a cut whose eng checkouts are not on
# the cut's own branch.
#
# A cut writes its version into each eng tree, commits, and tags the
# tip. It does so on whatever branch each checkout has out, so a
# checkout left on another line (a completeness branch, say) would take
# the cut's commit and tag. This check runs before the cut changes
# anything.
#
# Usage:
#   scripts/release/check-cut-checkouts.sh TAG PATH [PATH ...]
#
# TAG is the cut's tag (vMAJOR.MINOR.PATCH.CUT). Every PATH must be a
# git checkout whose current branch is named TAG and whose working tree
# is clean. Each PATH that is not is named on stderr, and the script
# exits 1. It changes nothing.

set -euo pipefail

if [[ $# -lt 2 ]]; then
    echo "usage: $(basename "$0") TAG PATH [PATH ...]" >&2
    exit 2
fi

TAG="$1"
shift

failed=0
for path in "$@"; do
    if ! git -C "${path}" rev-parse --is-inside-work-tree >/dev/null 2>&1; then
        echo "REFUSE: ${path}: not a git checkout" >&2
        failed=1
        continue
    fi
    branch="$(git -C "${path}" branch --show-current)"
    if [[ "${branch}" != "${TAG}" ]]; then
        echo "REFUSE: ${path}: on branch '${branch:-<detached>}', not the cut's branch '${TAG}'" >&2
        failed=1
        continue
    fi
    if [[ -n "$(git -C "${path}" status --porcelain)" ]]; then
        echo "REFUSE: ${path}: working tree is not clean" >&2
        failed=1
        continue
    fi
    echo "ok: ${path}: on ${TAG} at $(git -C "${path}" rev-parse --short HEAD)"
done

exit "${failed}"
