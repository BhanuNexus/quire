#!/usr/bin/env bash
# Checks the design notes in docs/design/ (CI's `packaging` job; blocking).
#
#   check-design-notes.sh [REPO_ROOT]
#
# Every note (docs/design/*.md and docs/design/handoff/*.md, except README.md):
#   - has `Status: current`, `Status: record` or `Status: superseded by <file>`,
#     and an `Area:` line, in its first 12 lines;
#   - a `current` note also has a `Code:` line; a `record` note says
#     "A record of a pass; not a description of today's code.";
#   - a `superseded` note names a file that exists;
#   - is listed in docs/design/README.md by its path;
#   - has no Markdown link to a missing file or to a file outside the repository,
#     and no path of this machine or of the scan folders (`/home/`, `opt-scan/`,
#     `hard-scan/`, `review-0...`), which an agent cannot open.
# README.md itself is checked for links only.
set -u

root="${1:-$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)}"
design="$root/docs/design"
readme="$design/README.md"
errors=0

fail() {
  echo "error: $1" >&2
  errors=$((errors + 1))
}

[ -f "$readme" ] || { echo "error: $readme is missing" >&2; exit 1; }
root_real="$(realpath -m "$root")"

check_links() {
  # $1 = file. Looks at inline links [text](target).
  local f="$1" rel dir target path links
  rel="${f#"$root"/}"
  dir="$(dirname "$f")"
  # Links outside code spans (the backticks are literal for sed).
  # shellcheck disable=SC2016
  links="$(sed 's/`[^`]*`//g' "$f" | grep -o '\]([^)]*)' | sed 's/^](//; s/)$//')"
  while IFS= read -r target; do
    case "$target" in
      http://* | https://* | mailto:* | '#'* | '') continue ;;
    esac
    path="${target%%#*}"
    path="${path%% *}"
    [ -z "$path" ] && continue
    local resolved
    resolved="$(realpath -m "$dir/$path")"
    case "$resolved" in
      "$root_real"/*) ;;
      *) fail "$rel links to $target, which is outside the repository"; continue ;;
    esac
    [ -e "$resolved" ] || fail "$rel links to $target, which does not exist"
  done <<< "$links"
}

# shellcheck disable=SC2016
# A wildcard in backticks on the README (for example `0.14-health-*.md`) lists a group of notes.
listed_by_glob() {
  local pat
  while IFS= read -r pat; do
    # shellcheck disable=SC2254
    case "$1" in $pat) return 0 ;; esac
  done < <(grep -oE '`[A-Za-z0-9._-]*\*[A-Za-z0-9._*-]*\.md`' "$readme" | tr -d '`')
  return 1
}

check_links "$readme"

for f in "$design"/*.md "$design"/handoff/*.md; do
  [ -f "$f" ] || continue
  rel="${f#"$design"/}"
  [ "$rel" = "README.md" ] && continue
  head="$(sed -n '1,12p' "$f")"

  status="$(printf '%s\n' "$head" | grep -E '^Status: ' | head -1)"
  case "$status" in
    'Status: current')
      printf '%s\n' "$head" | grep -qE '^Code: .+' || fail "$rel: a current note needs a 'Code:' line in its first 12 lines"
      ;;
    'Status: record')
      printf '%s\n' "$head" | grep -qF "A record of a pass; not a description of today's code." \
        || fail "$rel: a record needs the sentence \"A record of a pass; not a description of today's code.\""
      ;;
    'Status: superseded by '*)
      target="${status#Status: superseded by }"
      if [ -z "$target" ] || [ ! -f "$design/$target" ]; then
        fail "$rel: superseded by '$target', which is not a file in docs/design/"
      fi
      ;;
    '') fail "$rel: no 'Status:' line in the first 12 lines (current | record | superseded by <note>)" ;;
    *) fail "$rel: '$status' is not 'Status: current', 'Status: record' or 'Status: superseded by <note>'" ;;
  esac
  printf '%s\n' "$head" | grep -qE '^Area: .+' || fail "$rel: no 'Area:' line in the first 12 lines"

  if ! grep -qF "]($rel)" "$readme" && ! listed_by_glob "$rel"; then
    fail "$rel is not listed in docs/design/README.md (as a link to $rel)"
  fi

  check_links "$f"
  if grep -nE '/home/|opt-scan/|hard-scan/|review-0[0-9]' "$f" > /dev/null; then
    line="$(grep -nE '/home/|opt-scan/|hard-scan/|review-0[0-9]' "$f" | head -1)"
    fail "$rel cites a file outside the repository (line ${line%%:*}); summarise it instead"
  fi
done

if [ "$errors" -gt 0 ]; then
  echo "check-design-notes: $errors problem(s)" >&2
  exit 1
fi
echo "check-design-notes: ok"
