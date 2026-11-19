#!/usr/bin/env bash
# shellcheck disable=SC2016  # backticks in the printf fixtures are literal
# Tests for check-design-notes.sh on made-up trees (no repository files needed).
set -u
here="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
script="$here/check-design-notes.sh"
tmp="$(mktemp -d)"
trap 'rm -rf "$tmp"' EXIT
failures=0

new_tree() {
  rm -rf "${tmp:?}/t"
  mkdir -p "$tmp/t/docs/design/handoff"
  cat > "$tmp/t/docs/design/README.md" <<'R'
# Design notes
| [a.md](a.md) | [b.md](b.md) | [c.md](c.md) | [d.md](d.md) | [handoff/h.md](handoff/h.md) | `0.14-x-*.md` |
R
  printf '# A\n\nStatus: current\nArea: x\nCode: `crates/`\n\nBody.\n' > "$tmp/t/docs/design/a.md"
  printf '# B\n\nStatus: record\nArea: x\n\nA record of a pass; not a description of today'"'"'s code.\n' > "$tmp/t/docs/design/b.md"
  printf '# C\n\nStatus: superseded by a.md\nArea: x\n' > "$tmp/t/docs/design/c.md"
  printf '# D\n\nStatus: proposed (awaiting approval)\nArea: x\nCode: `crates/`\n' > "$tmp/t/docs/design/d.md"
  printf '# H\n\nStatus: record\nArea: x\n\nA record of a pass; not a description of today'"'"'s code.\n' > "$tmp/t/docs/design/handoff/h.md"
}

expect() { # name, expected exit (0 or 1), grep pattern for output (optional)
  local name="$1" want="$2" pat="${3:-}" out rc
  out="$("$script" "$tmp/t" 2>&1)"
  rc=$?
  if [ "$want" = 0 ] && [ "$rc" != 0 ]; then echo "FAIL $name: expected ok, got: $out"; failures=$((failures + 1)); return; fi
  if [ "$want" = 1 ] && [ "$rc" = 0 ]; then echo "FAIL $name: expected failure, passed"; failures=$((failures + 1)); return; fi
  if [ -n "$pat" ] && ! printf '%s\n' "$out" | grep -qE "$pat"; then echo "FAIL $name: output lacks /$pat/: $out"; failures=$((failures + 1)); return; fi
  echo "ok   $name"
}

new_tree; expect "good tree" 0

new_tree; printf '# A\n\nArea: x\nCode: `crates/`\n' > "$tmp/t/docs/design/a.md"; expect "no status" 1 "a.md: no 'Status:'"
new_tree; sed -i 's/Status: current/Status: built on a branch/' "$tmp/t/docs/design/a.md"; expect "free-text status" 1 "not 'Status: current'"
new_tree; sed -i '/^Area/d' "$tmp/t/docs/design/a.md"; expect "no area" 1 "no 'Area:'"
new_tree; sed -i '/^Code/d' "$tmp/t/docs/design/a.md"; expect "current without code" 1 "needs a 'Code:'"
new_tree; sed -i '/^Code/d' "$tmp/t/docs/design/d.md"; expect "proposed without code" 1 "d.md: a current or proposed note needs a 'Code:'"
new_tree; sed -i '/^A record/d' "$tmp/t/docs/design/b.md"; expect "record without its sentence" 1 "b.md: a record needs"
new_tree; sed -i 's/superseded by a.md/superseded by gone.md/' "$tmp/t/docs/design/c.md"; expect "superseded names a missing file" 1 "gone.md"
new_tree; printf '# E\n\nStatus: record\nArea: x\n\nA record of a pass; not a description of today'"'"'s code.\n' > "$tmp/t/docs/design/e.md"; expect "note not in README" 1 "e.md is not listed"
new_tree; printf '# D\n\nStatus: record\nArea: x\n\nA record of a pass; not a description of today'"'"'s code.\n' > "$tmp/t/docs/design/0.14-x-one.md"; expect "listed by a wildcard" 0
new_tree; printf 'See [r](../../../outside.md).\n' >> "$tmp/t/docs/design/a.md"; : > "$tmp/outside.md"; expect "link outside the repository" 1 "outside the repository"
new_tree; printf 'See [r](nothing.md).\n' >> "$tmp/t/docs/design/a.md"; expect "link to a missing file" 1 "does not exist"
new_tree; printf 'See [r](b.md#heading) and [web](https://example.org/x.md) and `[t](url)`.\n' >> "$tmp/t/docs/design/a.md"; expect "good links, anchors and code spans" 0
new_tree; printf 'Details in `opt-scan/reliability.md`.\n' >> "$tmp/t/docs/design/a.md"; expect "cites a scan file" 1 "cites a file outside"
new_tree; printf 'Screens in /home/someone/x.\n' >> "$tmp/t/docs/design/a.md"; expect "cites a machine path" 1 "cites a file outside"

if [ "$failures" -gt 0 ]; then echo "$failures test(s) failed"; exit 1; fi
echo "test-check-design-notes: all passed"
