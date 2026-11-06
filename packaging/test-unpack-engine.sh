#!/usr/bin/env bash
# Tests packaging/unpack-engine.sh against the pin files and paths the
# release runners give it (CI's test job runs this on Linux, macOS and
# Windows' Git Bash):
#
# * a pin file with CRLF line ends, upper-case hex, or a "*" or "\" in front
#   of a hash (sha256sum's binary marker and file-name escape);
# * an asset path with backslashes in it, like $RUNNER_TEMP on Windows
#   (D:\a\_temp), which makes GNU sha256sum print "\<hash>" (0.10.1);
# * a changed asset, a changed unpacked binary, a malformed or missing pin,
#   which must all fail.
#
#   packaging/test-unpack-engine.sh
set -euo pipefail
root="$(cd "$(dirname "$0")/.." && pwd)"
tmp="$(mktemp -d)"
trap 'rm -rf "$tmp"' EXIT

# A copy of the script next to its own crates/quire-ai, so the test never
# touches the real pin files.
mkdir -p "$tmp/packaging" "$tmp/crates/quire-ai"
cp "$root/packaging/unpack-engine.sh" "$tmp/packaging/"
script="$tmp/packaging/unpack-engine.sh"
pins="$tmp/crates/quire-ai"
target="x86_64-test-target"

hash() { if command -v sha256sum >/dev/null; then sha256sum < "$1"; else shasum -a 256 < "$1"; fi | cut -c1-64; }
upper() { tr 'a-f' 'A-F'; }

# The asset lives under a folder with backslashes in its name where the file
# system allows it (not on Windows, where $RUNNER_TEMP already has them).
if [[ -n "${RUNNER_TEMP:-}" && "$RUNNER_TEMP" == *\\* ]]; then
  dir="$RUNNER_TEMP/quire-unpack-test-$$"
  trap 'rm -rf "$tmp" "$dir"' EXIT
elif mkdir -p "$tmp/D:\\a\\_temp" 2>/dev/null; then
  dir="$tmp/D:\\a\\_temp/engine"
else
  dir="$tmp/engine"
fi
mkdir -p "$dir"
printf 'quire-ai-engine test binary\n%.0s' {1..64} > "$tmp/bin"
gzip -9 -n -c "$tmp/bin" > "$dir/quire-ai-engine-$target.gz"
gz="$dir/quire-ai-engine-$target.gz"
gz_sha="$(hash "$gz")"
bin_sha="$(hash "$tmp/bin")"
size="$(wc -c < "$gz" | tr -d ' ')"

fails=0
pass() { echo "ok: $1"; }
fail() { echo "FAIL: $1" >&2; fails=$((fails + 1)); }
expect_ok() {
  local name="$1" out="$tmp/out/$1/quire-ai-engine"
  if printed="$("$script" "$gz" "$target" "$out" 2> "$tmp/err")" \
    && [[ "$printed" == "$out" ]] && cmp -s "$out" "$tmp/bin"; then
    pass "$name"
  else
    fail "$name: $(cat "$tmp/err")"
  fi
}
expect_fail() {
  local name="$1" want="$2"
  if "$script" "$gz" "$target" "$tmp/out/$name/quire-ai-engine" > /dev/null 2> "$tmp/err"; then
    fail "$name: accepted"
  elif ! grep -q -- "$want" "$tmp/err"; then
    fail "$name: wrong error: $(cat "$tmp/err")"
  else
    pass "$name"
  fi
}
pin() { printf '%b' "$1" > "$pins/engine.sha256.$target"; }  # \r, \n, \t and \\ as escapes

pin "$gz_sha $size $bin_sha\n";                         expect_ok plain
pin "$gz_sha $size $bin_sha\r\n";                       expect_ok crlf
pin "$gz_sha $size $bin_sha";                           expect_ok no-final-newline
pin "$(upper <<< "$gz_sha") $size $(upper <<< "$bin_sha")\r\n"; expect_ok upper-case
pin "*$gz_sha $size *$bin_sha\r\n";                     expect_ok star-marker
pin "\\\\$gz_sha $size \\\\$bin_sha\n";                 expect_ok backslash-escape
pin "$gz_sha\t$size  $bin_sha \n";                      expect_ok tabs-and-spaces

pin "$bin_sha $size $bin_sha\n";                        expect_fail other-gz "doesn't match its pin"
pin "$gz_sha $size $gz_sha\n";                          expect_fail other-binary "unpacked engine for $target doesn't match"
pin "$gz_sha $size\n";                                  expect_fail short-pin "isn't"
pin "not-a-hash $size $bin_sha\n";                      expect_fail malformed-pin "isn't"
pin "";                                                 expect_fail empty-pin "isn't"
rm -f "$pins/engine.sha256.$target";                    expect_fail missing-pin "no engine pin"

if (( fails )); then echo "$fails failed" >&2; exit 1; fi
echo "all unpack-engine tests passed"
