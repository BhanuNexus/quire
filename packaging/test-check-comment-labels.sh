#!/usr/bin/env bash
# shellcheck disable=SC2016  # backticks in the fixtures are literal
# Tests for check-comment-labels.sh on made-up trees (no repository files needed).
set -u
here="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
script="$here/check-comment-labels.sh"
tmp="$(mktemp -d)"
trap 'rm -rf "$tmp"' EXIT
failures=0

tree() { # one line of Rust, put in crates/x/src/lib.rs
  rm -rf "${tmp:?}/t"
  mkdir -p "$tmp/t/crates/x/src" "$tmp/t/apps/y/src"
  printf '%s\n' "$1" > "$tmp/t/crates/x/src/lib.rs"
}

expect() { # name, expected exit, grep pattern for the output (optional)
  local name="$1" want="$2" pat="${3:-}" out rc
  out="$("$script" "$tmp/t" 2>&1)"
  rc=$?
  if [ "$want" = 0 ] && [ "$rc" != 0 ]; then echo "FAIL $name: expected ok, got: $out"; failures=$((failures + 1)); return; fi
  if [ "$want" = 1 ] && [ "$rc" = 0 ]; then echo "FAIL $name: expected failure, passed"; failures=$((failures + 1)); return; fi
  if [ -n "$pat" ] && ! printf '%s\n' "$out" | grep -qE "$pat"; then echo "FAIL $name: output lacks /$pat/: $out"; failures=$((failures + 1)); return; fi
  echo "ok   $name"
}

tree '//! A clean header.'; expect "clean" 0
tree '//! Notes: `docs/design/0.9.md` §C.4 and §D.1.'; expect "note path and section" 0
tree '// RFC 5869 §3.1 allows it.'; expect "RFC section" 0
tree 'let k = "F2"; // F2 renames, F3 finds'; expect "function keys" 0
tree '//! See `docs/design/F0-notes.md`.'; expect "the F0 notes file" 0
tree 'let s = "design §9 measures"; // usage text is not a comment'; expect "string without a comment" 0

tree '//! WS-D owns this.'; expect "workstream label" 1 "lib.rs:1"
tree '    // WS-G: citations'; expect "label after indent" 1 "workstream or phase"
tree 'fn f() {} // frozen after F0'; expect "F0 in a trailing comment" 1 "F0"
tree '/// The thing (design §C.4).'; expect "design section" 1 'design §'
tree '/// The thing (Design 0.9 §C.4).'; expect "design 0.9 section" 1 'design §'
tree '//! Zones (§A.2).'; expect "bare section" 1 "bare §"
tree '/// Budget §9: 50 ms.'; expect "bare numeric section" 1 "bare §"
rm -rf "${tmp:?}/t"; mkdir -p "$tmp/t/apps/y/src"; printf '// WS-A\n' > "$tmp/t/apps/y/src/main.rs"; expect "apps are scanned" 1 "apps/y/src/main.rs:1"

[ "$failures" = 0 ] && echo "all passed" || { echo "$failures failed"; exit 1; }
