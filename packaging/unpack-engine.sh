#!/usr/bin/env bash
# Unpack the engine release asset for one target and check it against the
# pins the engine job wrote (crates/quire-ai/engine.sha256.<target>:
# "<gz-sha256> <gz-size> <binary-sha256>"), so the packages bundle exactly
# the engine that was built and attested. Prints the unpacked path.
#
#   packaging/unpack-engine.sh <quire-ai-engine-<target>.gz> <target> <out-file>
#
# Hashes are compared as 64 lowercase hex digits and nothing else. Files are
# hashed from stdin: GNU sha256sum escapes a file name with a backslash in it
# and then starts its line with "\" (Windows paths such as
# D:\a\_temp/engine/…, 0.10.1's failed release), and Git Bash's may print a
# "*" binary-mode marker. Pin files may have CRLF line ends or upper case.
# packaging/test-unpack-engine.sh tests this.
set -euo pipefail
gz="$1"
target="$2"
out="$3"
pin_file="$(dirname "$0")/../crates/quire-ai/engine.sha256.$target"

# The first 64-hex-digit word of stdin, in lower case (empty if none).
hex64() { tr -d '\r' | tr 'A-F' 'a-f' | grep -oE '[0-9a-f]{64}' | head -n 1 || true; }
sha() {
  if command -v sha256sum >/dev/null; then sha256sum < "$1" | hex64; else shasum -a 256 < "$1" | hex64; fi
}

[[ -f "$pin_file" ]] || { echo "no engine pin for $target ($pin_file)" >&2; exit 1; }
read -r want_gz _ want_bin _ < <(tr -d '\r' < "$pin_file") || true
want_gz="$(printf '%s' "${want_gz:-}" | hex64)"
want_bin="$(printf '%s' "${want_bin:-}" | hex64)"
[[ -n "$want_gz" && -n "$want_bin" ]] \
  || { echo "the engine pin for $target ($pin_file) isn't \"<gz-sha256> <gz-size> <binary-sha256>\"" >&2; exit 1; }
[[ -f "$gz" ]] || { echo "no engine asset at $gz" >&2; exit 1; }
got="$(sha "$gz")"
[[ "$got" == "$want_gz" ]] || { echo "$gz doesn't match its pin (sha256 $got, pinned $want_gz)" >&2; exit 1; }
mkdir -p "$(dirname "$out")"
gzip -dc "$gz" > "$out"
got="$(sha "$out")"
[[ "$got" == "$want_bin" ]] \
  || { echo "the unpacked engine for $target doesn't match its pin (sha256 $got, pinned $want_bin)" >&2; exit 1; }
chmod 755 "$out"
echo "$out"
