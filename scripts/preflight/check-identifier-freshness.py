#!/usr/bin/env python3
# SPDX-License-Identifier: LicenseRef-Evo-Source-1.0
#
# check-identifier-freshness.py — Status-line identifier gate for
# Realised decision records in the sibling engineering journal.
#
# Living claims only: Status paragraph of files whose Status
# contains "Realised". Extracts backtick identifiers that look
# like code and are positively claimed (skips negative clauses,
# brace-globs, templates, multi-word prose).
#
# Does NOT scan SESSION_LOG, RISKS bodies, design novels,
# CHANGELOG, or scratchpads.
#
# Modes (EVO_IDENTIFIER_FRESHNESS_MODE):
#   warn (default) — report, exit 0
#   fail           — exit 1 on missing positively-claimed ids

from __future__ import annotations

import os
import re
import subprocess
import sys
from pathlib import Path

SCRIPT_DIR = Path(__file__).resolve().parent
REPO_ROOT = SCRIPT_DIR.parent.parent

DEFAULT_INTERNAL = REPO_ROOT.parent.parent / "evo-internal"
if not DEFAULT_INTERNAL.is_dir():
    DEFAULT_INTERNAL = REPO_ROOT.parent / "evo-internal"

HEX_SHA = re.compile(r"^[0-9a-f]{7,40}$", re.I)
DATEISH = re.compile(r"^\d{4}-\d{2}-\d{2}")
ADR_REF = re.compile(r"^ADR-\d+", re.I)
TICK_RE = re.compile(r"`([^`]+)`")

NOISE_TOKENS = {
    "evo-core-eng",
    "evo-ui-eng",
    "evo-device-audio",
    "evo-plugin-sdk",
    "evo-witness",
    "Status",
    "Realised",
    "Accepted",
    "Decision",
    "Consequences",
}

# Sentence-level negative claim markers (token skipped if its
# enclosing sentence matches).
NEG_SENTENCE = re.compile(
    r"(?i)(\*\*not\*\*|not realised|not claimed|are absent|"
    r"remains? open|contrast only|never shipped|"
    r"are \*\*not\*\*|is \*\*not\*\*|not\s+Realised)"
)


def mode() -> str:
    m = os.environ.get("EVO_IDENTIFIER_FRESHNESS_MODE", "warn").strip().lower()
    return "fail" if m == "fail" else "warn"


def eng_roots() -> list[Path]:
    override = os.environ.get("EVO_ENG_ROOTS")
    if override:
        return [Path(p) for p in override.split(":") if p]
    siblings = REPO_ROOT.parent
    candidates = [
        REPO_ROOT,
        siblings / "evo-device-audio",
        siblings / "evo-catalogue-schemas",
        siblings / "evo-ui-eng",
    ]
    return [p for p in candidates if p.is_dir()]


def internal_root() -> Path:
    override = os.environ.get("EVO_INTERNAL_ROOT")
    if override:
        return Path(override)
    return DEFAULT_INTERNAL


def status_paragraph(text: str) -> str:
    """First Status block only — stop at blank line so bullet
    closing-chains are not treated as living Status claims."""
    m = re.search(r"(?im)^Status:\s*(.+)$", text)
    if not m:
        return ""
    # Start from Status line; accumulate until blank line or Date/
    # Deciders / heading.
    lines = text[m.start() :].splitlines()
    out: list[str] = []
    for i, line in enumerate(lines):
        if i == 0:
            # Strip leading "Status:"
            out.append(re.sub(r"(?i)^Status:\s*", "", line))
            continue
        if not line.strip():
            break
        if re.match(r"(?i)^(Date|Deciders|Related|Amends):", line):
            break
        if line.startswith("## "):
            break
        out.append(line)
    return " ".join(out).strip()


def sentence_for(status: str, start: int, end: int) -> str:
    left = max(status.rfind(".", 0, start), status.rfind(";", 0, start))
    right_candidates = [
        i for i in (status.find(".", end), status.find(";", end)) if i != -1
    ]
    right = min(right_candidates) if right_candidates else len(status)
    return status[left + 1 : right + 1]


def is_codeish(tok: str) -> bool:
    t = tok.strip()
    if not t or len(t) < 3:
        return False
    if t in NOISE_TOKENS:
        return False
    if HEX_SHA.match(t) or DATEISH.match(t) or ADR_REF.match(t):
        return False
    if t.startswith("http"):
        return False
    # Brace globs, templates, wildcards, expressions, multi-word prose
    if any(c in t for c in ("{", "}", "<", ">", "*", "==", " ", '"', "'")):
        return False
    if "..." in t or t.startswith("pub "):
        return False
    if any(c in t for c in (".", "/", "_", "::")):
        return True
    if re.search(r"[A-Z][a-z]+[A-Z]", t):
        return True
    return False


def positively_claimed_ticks(status: str) -> list[str]:
    out: list[str] = []
    seen: set[str] = set()
    for m in TICK_RE.finditer(status):
        tok = m.group(1)
        if not is_codeish(tok):
            continue
        sent = sentence_for(status, m.start(), m.end())
        if NEG_SENTENCE.search(sent):
            continue
        if tok not in seen:
            seen.add(tok)
            out.append(tok)
    return out


def rg_content(needle: str, roots: list[Path]) -> bool:
    """Fixed-string content search across eng roots, excluding
    build outputs and Markdown (so doc drift does not self-confirm).
    Uses grep -r which is universally present on Unix; no external
    tooling dep."""
    cmd = [
        "grep",
        "-r",
        "-F",
        "-l",
        "-m",
        "1",
        "--exclude-dir=.git",
        "--exclude-dir=target",
        "--exclude-dir=node_modules",
        "--exclude-dir=dist",
        "--exclude-dir=build",
        "--exclude=*.md",
        "--",
        needle,
    ]
    for root in roots:
        try:
            r = subprocess.run(
                cmd + [str(root)],
                capture_output=True,
                text=True,
                check=False,
            )
        except FileNotFoundError:
            print(
                "check-identifier-freshness: grep not found on PATH",
                file=sys.stderr,
            )
            sys.exit(2)
        # grep exits 0 on match, 1 on no match, >1 on error.
        if r.returncode == 0 and r.stdout.strip():
            return True
    return False


def rg_exists(needle: str, roots: list[Path]) -> bool:
    if rg_content(needle, roots):
        return True

    # Path / basename existence under eng roots
    rel = needle
    for prefix in (
        "evo-core-eng/",
        "evo-ui-eng/",
        "evo-device-audio/",
    ):
        if rel.startswith(prefix):
            rel = rel[len(prefix) :]
    base = Path(needle).name
    look_for_file = "/" in needle or base.endswith(
        (".rs", ".tsx", ".ts", ".sh", ".toml", ".py", ".md")
    )
    if look_for_file:
        for root in roots:
            if (root / rel).exists():
                return True
            if not base:
                continue
            file_base = base.split("::")[0]
            r = subprocess.run(
                [
                    "find",
                    str(root),
                    "-name",
                    file_base,
                    "-not",
                    "-path",
                    "*/target/*",
                    "-not",
                    "-path",
                    "*/node_modules/*",
                    "-print",
                    "-quit",
                ],
                capture_output=True,
                text=True,
                check=False,
            )
            if r.stdout.strip():
                return True
    # Type::field — search the type name alone (no recursion)
    if "::" in needle:
        head = needle.split("::", 1)[0]
        if head and head != needle and rg_content(head, roots):
            return True
    return False


def main() -> int:
    m = mode()
    internal = internal_root()
    adr_dir = internal / "adr"
    if not adr_dir.is_dir():
        print(
            f"check-identifier-freshness: journal adr/ not found at "
            f"{adr_dir} (set EVO_INTERNAL_ROOT). Skipping.",
            file=sys.stderr,
        )
        return 0

    roots = eng_roots()
    if not roots:
        print(
            "check-identifier-freshness: no eng roots found. Skipping.",
            file=sys.stderr,
        )
        return 0

    realised: list[Path] = []
    for path in sorted(adr_dir.glob("*.md")):
        if path.name == "README.md":
            continue
        text = path.read_text(encoding="utf-8", errors="replace")
        status = status_paragraph(text)
        if re.search(r"\bRealised\b", status, re.I):
            realised.append(path)

    print(
        f"check-identifier-freshness: mode={m} "
        f"realised_adrs={len(realised)} eng_roots={len(roots)}",
        file=sys.stderr,
    )

    failures: list[tuple[str, str]] = []
    checked = 0
    for path in realised:
        text = path.read_text(encoding="utf-8", errors="replace")
        status = status_paragraph(text)
        for tok in positively_claimed_ticks(status):
            checked += 1
            if not rg_exists(tok, roots):
                failures.append((path.name, tok))

    if not failures:
        print(
            f"check-identifier-freshness: OK "
            f"({checked} Status identifiers resolved)",
            file=sys.stderr,
        )
        return 0

    print(
        f"check-identifier-freshness: {len(failures)} missing "
        f"Status identifier(s) (of {checked} checked):",
        file=sys.stderr,
    )
    for fname, tok in failures:
        print(f"  {fname}: `{tok}`", file=sys.stderr)

    if m == "fail":
        print(
            "check-identifier-freshness: FAIL "
            "(EVO_IDENTIFIER_FRESHNESS_MODE=fail)",
            file=sys.stderr,
        )
        return 1
    print(
        "check-identifier-freshness: WARN only "
        "(set EVO_IDENTIFIER_FRESHNESS_MODE=fail to refuse)",
        file=sys.stderr,
    )
    return 0


if __name__ == "__main__":
    sys.exit(main())
