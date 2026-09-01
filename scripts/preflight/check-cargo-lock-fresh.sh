#!/usr/bin/env bash
#
# check-cargo-lock-fresh.sh — fail-fast guard against Cargo.lock
# drift before push. CI runs cargo commands with --locked, which
# refuses to regenerate Cargo.lock; if a commit changed a manifest
# (Cargo.toml in any crate, or the workspace root) without
# updating Cargo.lock, the locked-build CI step fails and burns
# the build matrix.
#
# This script runs `cargo metadata --locked` which validates the
# lockfile against the manifests without compiling. Fast (sub-
# second) so it slots cleanly into the pre-push checklist
# alongside check-public-leaks.sh / cargo fmt --check / cargo
# clippy / cargo test.
#
# Exits 0 when the lockfile is fresh. Exits 1 with cargo's own
# error output otherwise; the fix is `cargo metadata` (without
# --locked) to regenerate the lockfile, then `git add Cargo.lock
# && git commit --amend` (or a follow-on commit).

set -eo pipefail

REPO_ROOT="$(cd "$(dirname "${BASH_SOURCE[0]}")/../.." && pwd)"
cd "${REPO_ROOT}"

if cargo metadata --locked --format-version 1 > /dev/null 2>&1; then
    echo "cargo-lock check: clean (lockfile matches manifests)."
    exit 0
fi

echo
echo "CARGO.LOCK CHECK FAILED."
echo
echo "Cargo.lock is out of sync with the workspace manifests."
echo "CI runs cargo with --locked and will refuse to regenerate"
echo "the lockfile, breaking the build matrix."
echo
echo "Fix:"
echo
echo "  cargo metadata --format-version 1 > /dev/null  # regenerates Cargo.lock"
echo "  git add Cargo.lock"
echo "  git commit --amend                              # if not yet pushed"
echo "  # or"
echo "  git commit -m 'chore(deps): refresh Cargo.lock' # if already pushed"
echo
echo "Cargo's own error follows:"
echo
cargo metadata --locked --format-version 1 2>&1 | tail -10 || true
exit 1
