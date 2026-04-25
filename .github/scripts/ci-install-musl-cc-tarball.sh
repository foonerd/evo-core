#!/usr/bin/env bash
# Install a musl cross toolchain tarball (musl-cc-produced, mirrored on
# this repo's GitHub releases) to /opt and add its bin/ to $GITHUB_PATH.
# Called by the msrv job in .github/workflows/ci.yml for musl target
# matrix entries that Ubuntu's archive does not cover (aarch64-musl,
# armv7-musl; x86_64-musl uses Ubuntu's musl-tools instead).
#
# Usage: ci-install-musl-cc-tarball.sh <toolchain-base-name>
#   e.g. ci-install-musl-cc-tarball.sh aarch64-linux-musl-cross
#        ci-install-musl-cc-tarball.sh armv7l-linux-musleabihf-cross
#
# The base name is the tarball stem (without `.tgz`); it is also the
# directory name inside the tarball and the prefix for the gcc binary
# with `-cross` stripped. Same naming convention as upstream musl.cc.
#
# Source of truth: a GitHub release on this repo (foonerd/evo-core).
# The tarballs are verbatim musl.cc output (2021-11-23 build),
# mirrored because musl.cc is not reliably reachable from GitHub
# Actions runners on Azure (observed: CURLE_OPERATION_TIMEDOUT on
# connect, repeated across CI runs, while musl.cc itself was up from
# other networks). The mirror eliminates this external dependency.
#
# Re-mirroring is a deliberate two-step change: upload the new
# tarball to a new release tag, then update the URL (RELEASE_TAG
# below) and the pinned SHA-512 in the same commit. The hash check
# protects against silent asset rotation - CI fails immediately on
# mismatch with a clear diagnostic.
#
# SHA-512 pinning: the expected hash per toolchain is hardcoded
# below, taken from musl.cc's own SHA512SUMS file at mirror time.
# `sha512sum -c` validates the downloaded bytes before extraction.
#
# Curl flags: --connect-timeout fails fast on TCP hangs, --max-time
# caps transfer time, --retry-all-errors makes --retry apply to
# timeouts (not just HTTP errors), --retry-delay spaces attempts.
# GitHub's release CDN is highly reliable; these flags are a
# secondary defence against transient blips, kept from the prior
# iteration against musl.cc.

set -euo pipefail

BASE="${1:?expected toolchain base name, e.g. aarch64-linux-musl-cross}"

case "$BASE" in
  aarch64-linux-musl-cross)
    SHA512="8695ff86979cdf30fbbcd33061711f5b1ebc3c48a87822b9ca56cde6d3a22abd4dab30fdcd1789ac27c6febbaeb9e5bde59d79d66552fae53d54cc1377a19272"
    ;;
  armv7l-linux-musleabihf-cross)
    SHA512="1bb399a61da425faac521df9b8d303e60ad101f6c7827469e0b4bc685ce1f3dedc606ac7b1e8e34d79f762a3bfe3e8ab479a97e97d9f36fbd9fc5dc9d7ed6fd1"
    ;;
  *)
    echo "ci-install-musl-cc-tarball: unknown toolchain base '$BASE'" >&2
    exit 1
    ;;
esac

RELEASE_TAG="ci-toolchains-v1"
URL="https://github.com/foonerd/evo-core/releases/download/${RELEASE_TAG}/${BASE}.tgz"
OUT="/tmp/${BASE}.tgz"

curl -fSL \
  --connect-timeout 30 \
  --max-time 300 \
  --retry 5 \
  --retry-all-errors \
  --retry-delay 10 \
  "$URL" \
  -o "$OUT"

echo "${SHA512}  ${OUT}" | sha512sum -c -

sudo tar -xzf "$OUT" -C /opt

# Derive the gcc name: BASE is e.g. `aarch64-linux-musl-cross`; the compiler
# binary is the same with `-cross` stripped, e.g. `aarch64-linux-musl-gcc`.
PREFIX="${BASE%-cross}"
test -x "/opt/${BASE}/bin/${PREFIX}-gcc"

echo "/opt/${BASE}/bin" >> "$GITHUB_PATH"
