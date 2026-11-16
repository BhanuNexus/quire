#!/usr/bin/env bash
# Tests of packaging/release-files.sh with stand-in files (CI's `packaging` job):
# a correct release passes, and each way a release set can go wrong fails.
# Also checks the engine rows of release-files.txt against release.yml's matrix.
set -euo pipefail
here="$(cd "$(dirname "$0")" && pwd)"
tool="$here/release-files.sh"
v=9.8.7
work="$(mktemp -d)"
trap 'rm -rf "$work"' EXIT

fresh() {
  rm -rf "$work/dist" && mkdir "$work/dist"
  while IFS= read -r name; do printf 'content of %s\n' "$name" > "$work/dist/$name"; done < <("$tool" list "$v")
}
expect_fail() {
  local what="$1"; shift
  if "$@" > "$work/out.txt" 2>&1; then
    echo "FAIL: $what was accepted"; cat "$work/out.txt"; exit 1
  fi
  echo "ok: $what is refused"
}
expect_pass() {
  local what="$1"; shift
  if ! "$@" > "$work/out.txt" 2>&1; then
    echo "FAIL: $what was refused"; cat "$work/out.txt"; exit 1
  fi
  echo "ok: $what"
}
first="$("$tool" list "$v" | sed -n 1p)"

fresh
expect_pass "a complete set is written and verified" "$tool" write "$work/dist" "$v"
expect_pass "a complete set verifies again" "$tool" verify "$work/dist" "$v"
grep -q '^[0-9a-f]\{64\}  ' "$work/dist/SHA256SUMS"
if grep -q SHA256SUMS "$work/dist/SHA256SUMS"; then echo "FAIL: SHA256SUMS lists itself"; exit 1; fi
rm -rf "$work/flat" && mkdir "$work/flat"
cp "$work/dist/Quire-$v-linux-x86_64.flatpak" "$work/dist/Quire-$v-linux-aarch64.flatpak" "$work/flat/"
expect_pass "the Flatpak bundles alone" "$tool" check "$work/flat" "$v" '*.flatpak'
rm "$work/flat/Quire-$v-linux-aarch64.flatpak"
expect_fail "one Flatpak bundle missing" "$tool" check "$work/flat" "$v" '*.flatpak'

fresh
rm "$work/dist/$first"
expect_fail "a missing file" "$tool" write "$work/dist" "$v"

fresh
echo x > "$work/dist/surprise.txt"
expect_fail "an extra file" "$tool" write "$work/dist" "$v"

fresh
mkdir "$work/dist/subfolder"
expect_fail "an extra folder" "$tool" write "$work/dist" "$v"

fresh
"$tool" write "$work/dist" "$v" > /dev/null
echo tampered >> "$work/dist/$first"
expect_fail "a changed file" "$tool" verify "$work/dist" "$v"

fresh
"$tool" write "$work/dist" "$v" > /dev/null
self="$(cd "$work/dist" && sha256sum SHA256SUMS)"
echo "$self" >> "$work/dist/SHA256SUMS"
expect_fail "a line for SHA256SUMS itself" "$tool" verify "$work/dist" "$v"

fresh
: > "$work/dist/$first"
expect_fail "an empty file (the hash of nothing)" "$tool" write "$work/dist" "$v"

fresh
"$tool" write "$work/dist" "$v" > /dev/null
sed -i '1d' "$work/dist/SHA256SUMS"
expect_fail "a file with no line" "$tool" verify "$work/dist" "$v"

fresh
"$tool" write "$work/dist" "$v" > /dev/null
sed -i 's/^\([0-9a-f]\{64\}\)  /\1 */' "$work/dist/SHA256SUMS"
expect_fail "binary-mode markers" "$tool" verify "$work/dist" "$v"

fresh
expect_fail "no SHA256SUMS" "$tool" verify "$work/dist" "$v"

fresh
expect_fail "a bad version" "$tool" write "$work/dist" "not-a-version"

# The engine rows are the engine job's targets.
want="$(grep -o 'target: [A-Za-z0-9_-]*' "$here/../.github/workflows/release.yml" | sed 's/target: //' | sort -u)"
have="$("$tool" list "$v" 'quire-ai-engine-*.gz' | sed 's/^quire-ai-engine-//; s/\.gz$//' | sort -u)"
if [ "$want" != "$have" ]; then
  echo "FAIL: release-files.txt's engines differ from release.yml's targets"
  diff <(echo "$want") <(echo "$have") || true
  exit 1
fi
echo "ok: the engine rows match release.yml's matrix"
echo "All release-files tests passed."
