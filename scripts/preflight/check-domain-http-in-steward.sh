# check-domain-http-in-steward.sh — refuse domain policy in the steward.
#
# THE CONSTRAINT
#
#   The steward is domain-neutral fabric. Product HTTP, named
#   services, named protocols and domain vocabulary in framework
#   types are out of bounds — see docs/engineering/BOUNDARY.md
#   section 5, and section 11 invariant 4, which puts behaviour for
#   any specific service or protocol in a plugin rather than inline
#   in the framework.
#
#   A vanilla binary mounts schema-driven wire ops plus TLS, auth,
#   observatory and witness. Nothing else. Product routes belong to
#   the distribution that ships the product.
#
# WHY A SCRIPT AND NOT A REVIEW RULE
#
#   This boundary has been crossed twice, in both cases by moving a
#   domain walk into the framework because plugins cannot compose
#   with each other and the framework was the only place that
#   could. The second breach landed five days after the first was
#   corrected. A review-time rule was the agreed control after the
#   first; it did not survive the second. Convenience is not an
#   exemption, and a check that runs is the only form of the rule
#   that has held.
#
# WHAT IT REFUSES — STRUCTURE, NOT NAMES
#
#   It does not know what any particular music service is, and it
#   must never learn. A guard that enumerates providers is a list
#   to be appended to; a guard that refuses shapes is an immune
#   system. These four shapes are how domain policy actually
#   enters the framework crates:
#
#     1. A route mounted under a new namespace.
#     2. A dispatch envelope built around a hardcoded shelf name.
#     3. A new variant on the steward's happening vocabulary.
#     4. build_router attaching a new presenter.
#
# THE ALLOWLIST IS A DEMOLITION LIST
#
#   Every entry is known contamination awaiting extraction to the
#   distribution. Matches inside it pass, so that the extraction
#   work itself stays committable while the condemned mounts are
#   still in place — a guard that failed the current tree would
#   block the very change that removes the problem.
#
#   Entries are deleted as the files move. An EMPTY allowlist is
#   the terminal state and the definition of done.
#
# KNOWN BLIND SPOTS, STATED RATHER THAN HIDDEN
#
#   Allowlisting is per file, and per namespace for check 1. New
#   contamination added INSIDE an already-condemned file passes, as
#   does a new path under an already-condemned namespace when it is
#   added inside an existing attach site. Those modules are frozen
#   against expansion by policy; this script does not enforce the
#   freeze, a reviewer does.
#
#   Pinning per-file match counts would close the first gap at the
#   cost of failing on every legitimate shrink of a file that is
#   about to be deleted anyway. That noise is how guards get
#   switched off, so it is deliberately not done.
#
# SCOPE
#
#   crates/evo/src and crates/evo-runtime-http/src only. The
#   distribution is where this content belongs; scanning it would
#   refuse the correct end state.
#
# Exit: 0 clean, 1 violation, 2 usage/environment error.

set -uo pipefail

REPO_ROOT="$(cd "$(dirname "${BASH_SOURCE[0]}")/../.." && pwd)"
cd "$REPO_ROOT" || exit 2

SCAN_DIRS=(crates/evo/src crates/evo-runtime-http/src)
BASELINE="scripts/preflight/domain-http-steward.baseline"

# --- allowlist: empty, which is the terminal state -------------------
#
# No domain route namespace remains. The audio presenters left with
# the extraction that introduced the router hookup; the
# captive-portal session path followed once it was classified as
# product. A vanilla binary now mounts schema-driven wire ops plus
# TLS, auth, observatory and witness, and nothing else.
#
# Any entry appearing here again is a regression, not a staging
# post.
ALLOWED_DOMAIN_NAMESPACES=""

# Framework namespaces. These are not domain product and stay.
FRAMEWORK_NAMESPACES="_observatory _witness ws"

# Files permitted to build a dispatch envelope around a hardcoded
# shelf name. Empty: the cascade that composed shelves by name now
# lives in the distribution that owns those shelves.
ALLOWED_SHELF_LITERAL_FILES=""

# Presenters build_router may attach. All four are framework
# surfaces and stay; no product presenter remains on this list.
ALLOWED_ATTACH="attach_observatory_endpoints attach_ws_endpoint attach_witness_endpoints attach_static_assets"

fail=0
note() { printf '  %s\n' "$*"; }
violation() {
  fail=1
  printf '\nVIOLATION — %s\n' "$1"
  shift
  printf '  %s\n' "$@"
}

# Emit a file's lines as `LINENO:TEXT`, dropping comment-only lines and
# the interior of every `#[cfg(test)]` block.
#
# Test fixtures legitimately contain shelf names and route strings; a
# guard that flagged them would be noise, and noise is how guards get
# switched off. But the skip must be the BLOCK, not the rest of the
# file: an earlier version cut from the first `#[cfg(test)]` to EOF,
# which made every line after a mid-file test module invisible —
# including real code. A guard with a blind spot is worse than no
# guard, because it is trusted.
#
# Brace depth is tracked from the `mod`/`fn` that follows the
# attribute, so nested braces inside the test block do not end the
# skip early. The attribute always clears after exactly one line:
# a `#[cfg(test)] use foo::Bar;` opens no block, and leaving the
# flag set there would swallow the remainder of the file — the
# same blind spot in a different disguise.
live_lines() {
  awk '
    /^[[:space:]]*#\[cfg\(test\)\]/ {
      # Already inside a skipped block: stay inside it and keep
      # counting braces. Restarting the measurement here would
      # forget the enclosing depth and treat the remainder of that
      # body as live code. That reads as noise in one direction,
      # but the same reset can end a skip early and let real code
      # past unseen, which is the direction that matters.
      if (depth > 0) { depth += gsub(/\{/, "{") - gsub(/\}/, "}"); next }
      pending = 1; next
    }
    {
      if (pending) {
        opened = gsub(/\{/, "{"); closed = gsub(/\}/, "}")
        pending = 0
        if (opened > 0) {
          depth = opened - closed
          if (depth < 0) depth = 0
        }
        next
      }
      if (depth > 0) {
        depth += gsub(/\{/, "{") - gsub(/\}/, "}")
        next
      }
      if ($0 ~ /^[[:space:]]*\/\//) next
      if ($0 ~ /^[[:space:]]*$/) next
      print NR ":" $0
    }
  ' "$1"
}

each_rust_file() {
  local d
  for d in "${SCAN_DIRS[@]}"; do
    [ -d "$d" ] || continue
    find "$d" -name '*.rs' -type f
  done
}

printf 'check-domain-http-in-steward: scanning %s\n' "${SCAN_DIRS[*]}"

# --- 1. route namespaces --------------------------------------------
# A presenter mounts under `{api_prefix}/<namespace>/...`. A namespace
# that is neither framework nor already-condemned is new domain HTTP.
while IFS= read -r f; do
  while IFS= read -r hit; do
    ns="$(printf '%s' "$hit" | sed -nE 's/.*api_prefix\}\/([a-z0-9_]+).*/\1/p')"
    [ -n "$ns" ] || continue
    case " $FRAMEWORK_NAMESPACES $ALLOWED_DOMAIN_NAMESPACES " in
      *" $ns "*) continue ;;
    esac
    violation "route mounted under a new namespace: /$ns" \
      "$f:${hit%%:*}" \
      "A vanilla binary mounts schema-driven wire ops plus TLS, auth," \
      "observatory and witness. A product route belongs to the" \
      "distribution that ships the product, mounted through the" \
      "router hookup rather than from build_router."
  done < <(live_lines "$f" | grep -E 'api_prefix\}/[a-z0-9_]+')
done < <(each_rust_file)

# --- 2. hardcoded shelf in a dispatch envelope ----------------------
# Core composing `{"shelf": "<literal>"}` is core deciding which
# domain shelf answers — plugin sovereignty inverted.
while IFS= read -r f; do
  case " $ALLOWED_SHELF_LITERAL_FILES " in *" $f "*) continue ;; esac
  while IFS= read -r hit; do
    shelf="$(printf '%s' "$hit" | sed -nE 's/.*"shelf"[[:space:]]*:[[:space:]]*"([a-z0-9_.]+)".*/\1/p')"
    [ -n "$shelf" ] || continue
    violation "dispatch envelope built around a hardcoded shelf: $shelf" \
      "$f:${hit%%:*}" \
      "The steward routes by contract, it does not know which shelf" \
      "serves a domain. Take the shelf from the caller, or move the" \
      "composition to the distribution."
  done < <(live_lines "$f" | grep -E '"shelf"[[:space:]]*:[[:space:]]*"')
done < <(each_rust_file)

# --- 3. happening vocabulary ----------------------------------------
# The steward's event vocabulary is public contract. A domain variant
# there is BOUNDARY §5 "domain vocabulary in framework types" — the
# same shape as an ItemType enumerating Podcast.
variants_now() {
  awk '/^pub enum Happening \{/,/^\}/' crates/evo/src/happenings.rs 2>/dev/null \
    | sed -nE 's/^    ([A-Z][A-Za-z0-9]*) \{.*/H:\1/p'
  awk '/^enum HappeningWire \{/,/^\}/' crates/evo/src/server.rs 2>/dev/null \
    | sed -nE 's/^    ([A-Z][A-Za-z0-9]*) \{.*/W:\1/p'
}

if [ -f "$BASELINE" ]; then
  added="$(comm -13 <(sort "$BASELINE") <(variants_now | sort))"
  if [ -n "$added" ]; then
    violation "new happening variant(s) on the steward vocabulary" \
      "$(printf '%s' "$added" | tr '\n' ' ')" \
      "A landing or state signal for a domain belongs on PluginEvent" \
      "with an opaque event_type and payload, emitted by whoever owns" \
      "the domain. If this variant is genuinely domain-neutral fabric," \
      "add it to $BASELINE in the same commit and say why in the" \
      "message."
  fi
  removed="$(comm -23 <(sort "$BASELINE") <(variants_now | sort))"
  [ -n "$removed" ] && note "variants removed since baseline (expected as extraction proceeds): $(printf '%s' "$removed" | tr '\n' ' ')"
else
  note "no baseline at $BASELINE — run with --write-baseline to create it"
fi

# --- 4. presenters attached by build_router -------------------------
ROUTER="crates/evo-runtime-http/src/router.rs"
if [ -f "$ROUTER" ]; then
  while IFS= read -r hit; do
    fn="$(printf '%s' "$hit" | sed -nE 's/.*(attach_[a-z0-9_]+)\(.*/\1/p')"
    [ -n "$fn" ] || continue
    case " $ALLOWED_ATTACH " in *" $fn "*) continue ;; esac
    violation "build_router attaches a new presenter: $fn" \
      "$ROUTER:${hit%%:*}" \
      "Presenters are distribution product. Mount it through the" \
      "router hookup the distribution supplies, not from build_router."
  done < <(live_lines "$ROUTER" | grep -E '\battach_[a-z0-9_]+\(')
fi

# --- baseline writer ------------------------------------------------
if [ "${1:-}" = "--write-baseline" ]; then
  variants_now | sort > "$BASELINE"
  printf 'wrote %s (%s entries)\n' "$BASELINE" "$(wc -l < "$BASELINE" | tr -d ' ')"
  exit 0
fi

if [ "$fail" -eq 0 ]; then
  printf 'check-domain-http-in-steward: PASS\n'
  printf '  allowlist still holds %s route namespace(s), %s shelf-literal file(s), %s attach site(s).\n' \
    "$(printf '%s' "$ALLOWED_DOMAIN_NAMESPACES" | wc -w | tr -d ' ')" \
    "$(printf '%s' "$ALLOWED_SHELF_LITERAL_FILES" | wc -w | tr -d ' ')" \
    "$(printf '%s' "$ALLOWED_ATTACH" | wc -w | tr -d ' ')"
  printf '  An empty allowlist is the terminal state: extraction complete.\n'
  exit 0
fi

printf '\ncheck-domain-http-in-steward: FAIL\n'
printf '  docs/engineering/BOUNDARY.md section 5; section 11 invariant 4.\n'
printf '  Convenience is not an exemption.\n'
exit 1
