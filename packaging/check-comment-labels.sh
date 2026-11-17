#!/usr/bin/env bash
# Fails on stale labels in comments of the Rust sources (CI's `packaging` job; blocking).
#
#   check-comment-labels.sh [REPO_ROOT]
#
# In crates/ and apps/, a `//`, `///` or `//!` comment must not contain:
#   - a workstream or phase label: `WS-A` to `WS-G`, `F0` (they come from the 0.9
#     hand-off, which docs/design/README.md says is not a rule for today's work);
#   - "design §…": say which note, as `docs/design/0.9.md` §C.4;
#   - a bare `§C.4` or `§7` with no `docs/design/` path on the same line
#     (RFC references such as "RFC 5869 §3.1" are fine).
# Name the feature instead of a label. A comment that is complete without the
# reference needs none. The path form is checked by hand: name a section that exists.
set -u

root="${1:-$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)}"
errors=0
dirs=()
for d in crates apps; do [ -d "$root/$d" ] && dirs+=("$root/$d"); done
[ "${#dirs[@]}" -gt 0 ] || { echo "error: no crates/ or apps/ under $root" >&2; exit 1; }

scan() { # message, PCRE
  local msg="$1" pat="$2" hits
  hits="$(grep -rnP --include='*.rs' -e "$pat" "${dirs[@]}" 2>/dev/null | sed "s#^$root/##")"
  if [ -n "$hits" ]; then
    echo "error: $msg" >&2
    printf '%s\n' "$hits" | cut -c1-200 >&2
    errors=$((errors + 1))
  fi
}

scan "workstream or phase label in a comment (WS-A..WS-G, F0): name the feature instead" \
  '//.*(\bWS-[A-G]\b|\bF0\b(?!-notes))'
scan "\"design §\" in a comment: write the note and section, e.g. \`docs/design/0.9.md\` §C.4, or drop it" \
  '//.*[Dd]esign (0\.9 )?§'
scan "bare § reference in a comment: put the note's path on the same line, e.g. \`docs/design/0.9.md\` §C.4" \
  '//(?!.*(docs/design/|RFC)).*§[0-9A-G]'

[ "$errors" = 0 ] || exit 1
echo "ok: no stale labels in comments"
