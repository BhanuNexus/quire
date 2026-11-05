#!/usr/bin/env bash
# Unpack the engine release asset for one target and check it against the
# pins the engine job wrote (crates/quire-ai/engine.sha256.<target>:
# "<gz-sha256> <gz-size> <binary-sha256>"), so the packages bundle exactly
# the engine that was built and attested. Prints the unpacked path.
#
#   packaging/unpack-engine.sh <quire-ai-engine-<target>.gz> <target> <out-file>
set -euo pipefail
gz="$1"
target="$2"
out="$3"
pin_file="$(dirname "$0")/../crates/quire-ai/engine.sha256.$target"
sha() { if command -v sha256sum >/dev/null; then sha256sum "$1" | cut -d' ' -f1; else shasum -a 256 "$1" | cut -d' ' -f1; fi; }

[[ -f "$pin_file" ]] || { echo "no engine pin for $target ($pin_file)" >&2; exit 1; }
read -r want_gz _ want_bin _ < "$pin_file"
[[ "$(sha "$gz")" == "$want_gz" ]] || { echo "$gz doesn't match its pin" >&2; exit 1; }
mkdir -p "$(dirname "$out")"
gzip -dc "$gz" > "$out"
[[ "$(sha "$out")" == "$want_bin" ]] || { echo "the unpacked engine for $target doesn't match its pin" >&2; exit 1; }
chmod 755 "$out"
echo "$out"
