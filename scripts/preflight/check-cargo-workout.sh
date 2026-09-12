#!/usr/bin/env bash
#
# check-cargo-workout.sh — compulsory commit entry gate.
#
# Runs the trio public evo-core CI runs (fmt, clippy, test, all
# --locked) and adds the two steps CI historically omitted and
# that therefore slipped: start from `cargo clean`, and rustdoc
# with RUSTDOCFLAGS='-D warnings'. `cargo test --doc` is not this
# gate; rustdoc link and HTML lints are.
#
# Toolchain. This gate runs on the workspace MSRV — `rust-version
# = "1.85"` in the root Cargo.toml — because the product must
# compile on 1.85. That is a floor the code has to keep clearing.
# It is not a claim about which compiler builds a release: release
# and fleet binaries are built with whatever the build host
# carries, and that is not 1.85. The host's `stable` is NOT the
# gate. On the box where this was written that is 1.98 — thirteen
# releases past the floor: it accepts syntax 1.85 rejects, and
# its clippy carries lints 1.85 has never heard of. Green on the
# host default therefore says nothing about whether the code
# still clears the floor, and a gate that reports it as a pass is
# worse than no gate, because someone will trust it. Public CI
# covers stable in its own job (`.github/workflows/ci.yml`) and
# 1.85 across every Primary target in the `msrv` matrix; this
# script is the MSRV one.
#
# Override deliberately, never by default:
#   CARGO_TOOLCHAIN=stable scripts/preflight/check-cargo-workout.sh
#
# Run from the workspace root before every commit. Exits 0 only
# when every step is clean. Do not #[allow] rustdoc or clippy
# to silence this gate.
#
# Usage:
#   scripts/preflight/check-cargo-workout.sh
#   scripts/preflight/check-cargo-workout.sh --no-locked
#   CARGO_TOOLCHAIN=stable scripts/preflight/check-cargo-workout.sh

set -euo pipefail

REPO_ROOT="$(cd "$(dirname "${BASH_SOURCE[0]}")/../.." && pwd)"
cd "${REPO_ROOT}"

# The workspace MSRV, not the host default. See "Toolchain"
# above before changing this.
TOOLCHAIN="${CARGO_TOOLCHAIN:-1.85}"
LOCKED=1
if [[ "${1:-}" == "--no-locked" ]]; then
    LOCKED=0
fi

lock_args=()
if [[ "${LOCKED}" -eq 1 ]]; then
    lock_args+=(--locked)
fi

log_step() { printf '\n[workout] %s\n' "$*" >&2; }
log_ok()   { printf '[workout] OK: %s\n' "$*" >&2; }
log_fail() { printf '[workout] FAIL: %s\n' "$*" >&2; }

unset CARGO_TARGET_DIR

log_step "1/5 cargo +${TOOLCHAIN} clean"
cargo "+${TOOLCHAIN}" clean
log_ok "clean"

log_step "2/5 cargo +${TOOLCHAIN} fmt --all -- --check"
if ! cargo "+${TOOLCHAIN}" fmt --all -- --check; then
    log_fail "fmt drift. Fix: cargo +${TOOLCHAIN} fmt --all"
    exit 1
fi
log_ok "fmt"

log_step "3/5 cargo +${TOOLCHAIN} clippy --workspace --all-targets ${lock_args[*]:-} -- -D warnings"
if ! cargo "+${TOOLCHAIN}" clippy --workspace --all-targets "${lock_args[@]}" -- -D warnings; then
    log_fail "clippy -D warnings"
    exit 1
fi
log_ok "clippy"

log_step "4/5 cargo +${TOOLCHAIN} test --workspace ${lock_args[*]:-}"
if ! cargo "+${TOOLCHAIN}" test --workspace "${lock_args[@]}"; then
    log_fail "tests"
    exit 1
fi
log_ok "test"

log_step "5/5 RUSTDOCFLAGS='-D warnings' cargo +${TOOLCHAIN} doc --workspace --no-deps ${lock_args[*]:-}"
if ! RUSTDOCFLAGS='-D warnings' cargo "+${TOOLCHAIN}" doc --workspace --no-deps "${lock_args[@]}"; then
    log_fail "rustdoc -D warnings (intra-doc links, private links, HTML, rustdoc lints)"
    exit 1
fi
log_ok "rustdoc"

printf '\n[workout] all five steps clean (toolchain +%s%s).\n' \
    "${TOOLCHAIN}" "$([[ ${LOCKED} -eq 1 ]] && echo ', --locked' || true)" >&2
