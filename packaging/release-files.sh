#!/usr/bin/env bash
# The release's file set and its SHA256SUMS (see packaging/release-files.txt).
#   packaging/release-files.sh list   <version> [glob]   the expected file names
#   packaging/release-files.sh check  <dir> <version> [glob]
#       the files in <dir> are exactly the expected ones (only those matching
#       <glob>, when given: the Flatpak attest job checks '*.flatpak')
#   packaging/release-files.sh write  <dir> <version>
#       check, write <dir>/SHA256SUMS, then verify it
#   packaging/release-files.sh verify <dir> <version>
#       the files are the expected ones, SHA256SUMS lists exactly them (no line
#       for itself, none with the hash of an empty file, no duplicates, no
#       path) and `sha256sum -c` passes
# Exits 1 and says what is missing, extra or wrong. Needs GNU coreutils.
set -euo pipefail

here="$(cd "$(dirname "$0")" && pwd)"
list_file="${RELEASE_FILES_LIST:-$here/release-files.txt}"
empty_hash="e3b0c44298fc1c149afbf4c8996fb92427ae41e4649b934ca495991b7852b855"

fail() { echo "::error::$*" >&2; exit 1; }

version_ok() {
  printf '%s' "$1" | grep -Eq '^[0-9]+\.[0-9]+\.[0-9]+([-+][0-9A-Za-z.-]+)?$' \
    || fail "The version must look like 1.2.3 (got: $1)"
}

# The expected names for a version, optionally only those matching a glob.
expected() {
  local v="$1" glob="${2:-*}" line
  while IFS= read -r line || [ -n "$line" ]; do
    line="${line%%#*}"
    line="${line//[[:space:]]/}"
    [ -n "$line" ] || continue
    line="${line//<v>/$v}"
    # shellcheck disable=SC2254 # the glob is meant to be a pattern
    case "$line" in $glob) printf '%s\n' "$line" ;; esac
  done < "$list_file"
}

# Entries of a folder, never SHA256SUMS itself, sorted the same way as expected().
present() {
  local dir="$1" glob="${2:-*}" name
  while IFS= read -r name; do
    [ "$name" = SHA256SUMS ] && continue
    # shellcheck disable=SC2254
    case "$name" in $glob) printf '%s\n' "$name" ;; esac
  done < <(find "$dir" -mindepth 1 -maxdepth 1 -printf '%f\n') | LC_ALL=C sort
}

# Compare two sorted lists of names ($1 expected, $2 found); $3 says what the found list is.
compare() {
  local exp="$1" got="$2" what="$3" missing extra
  missing="$(LC_ALL=C comm -23 <(printf '%s\n' "$exp") <(printf '%s\n' "$got") | sed '/^$/d')"
  extra="$(LC_ALL=C comm -13 <(printf '%s\n' "$exp") <(printf '%s\n' "$got") | sed '/^$/d')"
  if [ -n "$missing" ] || [ -n "$extra" ]; then
    [ -z "$missing" ] || echo "::error::Missing from $what: $(echo "$missing" | tr '\n' ' ')" >&2
    [ -z "$extra" ] || echo "::error::Not expected in $what (add it to packaging/release-files.txt if it is meant to be released): $(echo "$extra" | tr '\n' ' ')" >&2
    return 1
  fi
}

# The list itself: no duplicates, and something in it.
list_sane() {
  local v="$1" dups
  [ -n "$(expected "$v")" ] || fail "$list_file lists no files"
  dups="$(expected "$v" | LC_ALL=C sort | uniq -d)"
  [ -z "$dups" ] || fail "$list_file lists a name twice: $dups"
}

check() {
  local dir="$1" v="$2" glob="${3:-*}"
  [ -d "$dir" ] || fail "$dir is not a folder"
  version_ok "$v"
  list_sane "$v"
  compare "$(expected "$v" "$glob" | LC_ALL=C sort)" "$(present "$dir" "$glob")" "$dir" \
    || fail "The files in $dir are not the release's files (packaging/release-files.txt)"
  echo "$dir holds exactly the $(expected "$v" "$glob" | wc -l | tr -d ' ') expected files."
}

verify() {
  local dir="$1" v="$2" sums names bad
  check "$dir" "$v"
  sums="$dir/SHA256SUMS"
  [ -f "$sums" ] || fail "$sums is missing"
  [ -s "$sums" ] || fail "$sums is empty"
  # Every line is "<64 hex digits>  <plain file name>": two spaces (text mode),
  # no "*" marker, no path.
  bad="$(grep -Ev '^[0-9a-f]{64}  [^/ *][^/ ]*$' "$sums" || true)"
  [ -z "$bad" ] || fail "SHA256SUMS has lines that are not '<hash>  <file name>': $bad"
  names="$(sed 's/^[0-9a-f]\{64\}  //' "$sums")"
  if printf '%s\n' "$names" | grep -qx 'SHA256SUMS'; then
    fail "SHA256SUMS lists itself (0.13.1's did, with the hash of an empty file)"
  fi
  if grep -q "^$empty_hash " "$sums"; then
    fail "SHA256SUMS has a line with the hash of an empty file"
  fi
  [ -z "$(printf '%s\n' "$names" | LC_ALL=C sort | uniq -d)" ] || fail "SHA256SUMS names a file twice"
  compare "$(expected "$v" | LC_ALL=C sort)" "$(printf '%s\n' "$names" | LC_ALL=C sort)" SHA256SUMS \
    || fail "SHA256SUMS does not list exactly the release's files"
  (cd "$dir" && sha256sum --strict --check SHA256SUMS > /dev/null) \
    || { (cd "$dir" && sha256sum --check SHA256SUMS | grep -v ': OK$' >&2) || true; fail "sha256sum -c failed"; }
  echo "SHA256SUMS is correct: $(printf '%s\n' "$names" | wc -l | tr -d ' ') files, sha256sum -c passes."
}

write() {
  local dir="$1" v="$2" tmp
  check "$dir" "$v"
  [ ! -e "$dir/SHA256SUMS" ] || fail "$dir/SHA256SUMS exists already"
  # Written outside the folder first: `… > SHA256SUMS` in a pipeline can create
  # the file before the names are listed, and then it lists itself (0.13.1's does).
  tmp="$(mktemp)"
  (cd "$dir" && present . | while IFS= read -r name; do sha256sum -- "$name"; done) > "$tmp"
  mv "$tmp" "$dir/SHA256SUMS"
  cat "$dir/SHA256SUMS"
  verify "$dir" "$v"
}

case "${1:-}" in
  list) [ $# -ge 2 ] || fail "usage: release-files.sh list <version> [glob]"; version_ok "$2"; expected "$2" "${3:-*}" ;;
  check) [ $# -ge 3 ] || fail "usage: release-files.sh check <dir> <version> [glob]"; check "$2" "$3" "${4:-*}" ;;
  write) [ $# -eq 3 ] || fail "usage: release-files.sh write <dir> <version>"; write "$2" "$3" ;;
  verify) [ $# -eq 3 ] || fail "usage: release-files.sh verify <dir> <version>"; verify "$2" "$3" ;;
  *) echo "usage: release-files.sh list|check|write|verify …" >&2; exit 2 ;;
esac
