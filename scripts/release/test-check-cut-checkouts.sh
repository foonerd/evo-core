#!/usr/bin/env bash
#
# test-check-cut-checkouts.sh — exercise check-cut-checkouts.sh on
# throwaway repositories: a checkout on the cut's branch and clean
# passes; one on another branch, one dirty, one detached and a path
# that is not a checkout are each refused by name.

set -euo pipefail

HERE="$(cd "$(dirname "$0")" && pwd)"
CHECK="${HERE}/check-cut-checkouts.sh"
WORK="$(mktemp -d)"
trap 'rm -rf "${WORK}"' EXIT

TAG="v9.9.9.9"

repo() {
    local dir="$1" branch="$2"
    git init -q -b "${branch}" "${dir}"
    git -C "${dir}" -c user.name=test -c user.email=test@invalid \
        commit -q --allow-empty -m init
}

repo "${WORK}/right" "${TAG}"
repo "${WORK}/other" "v9.9.9.10"
repo "${WORK}/dirty" "${TAG}"
echo change > "${WORK}/dirty/file"
repo "${WORK}/detached" "${TAG}"
git -C "${WORK}/detached" checkout -q --detach
mkdir -p "${WORK}/plain"

fail() { echo "FAIL: $*" >&2; exit 1; }

bash "${CHECK}" "${TAG}" "${WORK}/right" >/dev/null 2>&1 \
    || fail "a clean checkout on the cut's branch must pass"

for bad in other dirty detached plain; do
    if out="$(bash "${CHECK}" "${TAG}" "${WORK}/right" "${WORK}/${bad}" 2>&1)"; then
        fail "${bad} must be refused"
    fi
    grep -q "REFUSE: ${WORK}/${bad}:" <<<"${out}" \
        || fail "${bad} must be named in the refusal: ${out}"
    grep -q "REFUSE: ${WORK}/right:" <<<"${out}" \
        && fail "the right checkout must not be refused: ${out}"
done

echo "test-check-cut-checkouts: PASS"
