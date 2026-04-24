#!/usr/bin/env bash
# Install a musl.cc cross toolchain tarball to /opt and add its bin/ to
# $GITHUB_PATH. Called by the msrv job in .github/workflows/ci.yml for musl
# target matrix entries that Ubuntu's archive does not cover (aarch64-musl,
# armv7-musl; x86_64-musl uses Ubuntu's musl-tools instead).
#
# Usage: ci-install-musl-cc-tarball.sh <toolchain-base-name>
#   e.g. ci-install-musl-cc-tarball.sh aarch64-linux-musl-cross
#        ci-install-musl-cc-tarball.sh armv7l-linux-musleabihf-cross
#
# The base name is the musl.cc tarball stem (without `.tgz`); it is also the
# directory name inside the tarball and the prefix for the gcc binary with
# the `-cross` suffix stripped (e.g. `aarch64-linux-musl-cross.tgz` extracts
# to `/opt/aarch64-linux-musl-cross/` and installs `aarch64-linux-musl-gcc`).
#
# musl.cc is a community mirror with periodic reliability dips. The curl
# invocation below is hardened against the failure mode observed in CI on
# 2026-04-24: a stalled connection that produces zero bytes, no HTTP error
# code, and hangs until the job-level timeout. `curl --retry N` alone does
# NOT retry on that failure mode - by default it only retries on a narrow
# set of HTTP response codes (408, 429, 5xx), not on connection timeouts
# or stalled transfers. The flags below fix that:
#
#   --connect-timeout 30   Fail fast on TCP-establishment hangs.
#   --max-time 300         Cap total transfer time; stalled connection
#                          escalates to an error that triggers retry.
#   --retry 5              More attempts before giving up (was 3).
#   --retry-all-errors     Make --retry apply to timeouts and non-HTTP
#                          failures, not just the default narrow set.
#                          This is the flag that actually fixes the
#                          observed failure mode.
#   --retry-delay 10       Space attempts so a transient blip clears.
#
# If musl.cc outages become frequent enough that even these retries aren't
# enough, the longer-term fix is to mirror the tarballs on a GitHub release
# of this repository (or similarly reliable artefact host) and change the
# URL here. See the discussion in the commit that introduced this script.

set -euo pipefail

BASE="${1:?expected musl.cc toolchain base name, e.g. aarch64-linux-musl-cross}"
URL="https://musl.cc/${BASE}.tgz"
OUT="/tmp/${BASE}.tgz"

curl -fSL \
  --connect-timeout 30 \
  --max-time 300 \
  --retry 5 \
  --retry-all-errors \
  --retry-delay 10 \
  "$URL" \
  -o "$OUT"

sudo tar -xzf "$OUT" -C /opt

# Derive the gcc name: BASE is e.g. `aarch64-linux-musl-cross`; the compiler
# binary is the same with `-cross` stripped, e.g. `aarch64-linux-musl-gcc`.
PREFIX="${BASE%-cross}"
test -x "/opt/${BASE}/bin/${PREFIX}-gcc"

echo "/opt/${BASE}/bin" >> "$GITHUB_PATH"
