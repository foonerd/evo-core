#!/usr/bin/env bash
# SPDX-License-Identifier: LicenseRef-Evo-Source-1.0
#
# Wrapper for check-identifier-freshness.py.
# Default mode is warn (exit 0). Set
#   EVO_IDENTIFIER_FRESHNESS_MODE=fail
# to refuse when Realised Status identifiers are missing from eng trees.
#
# Exempt by design: activity logs, risk narratives, design
# documents, changelogs, scratchpads. Status paragraphs of
# Realised decision records only.

set -euo pipefail

SCRIPT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
export EVO_IDENTIFIER_FRESHNESS_MODE="${EVO_IDENTIFIER_FRESHNESS_MODE:-warn}"
exec python3 "${SCRIPT_DIR}/check-identifier-freshness.py"
