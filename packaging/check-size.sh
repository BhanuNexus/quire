#!/usr/bin/env bash
# Fails when a release binary is over its size budget (docs/design/0.9.md §2.3,
# lowered in 0.10 to the measured size plus about 10 %: 67.6 and 40.0 MiB,
# docs/design/0.10-perf.md §2).
#   packaging/check-size.sh <binary> full|lite|engine
# Budgets are for the stripped release binary, in MiB.
set -euo pipefail
bin="$1"
edition="$2"
case "$edition" in
  full) budget=80 ;;
  lite) budget=50 ;;
  engine) budget=25 ;;
  *) echo "edition must be full, lite or engine" >&2; exit 2 ;;
esac
size=$(wc -c < "$bin")
mib=$(( (size + 1048575) / 1048576 ))
echo "$bin: ${mib} MiB (budget ${budget} MiB, $edition)"
if [ "$mib" -gt "$budget" ]; then
  echo "::error::$bin is ${mib} MiB, over the ${budget} MiB budget for $edition"
  exit 1
fi
# An ELF binary must be stripped: a symbol table is a sign that a profile
# override lost `strip = true` (0.10.1: 16 MiB of the Lite binary; see the
# comment above [profile.release.package.quire-cli] in Cargo.toml).
if command -v readelf > /dev/null && readelf -h "$bin" > /dev/null 2>&1 \
  && readelf -S -W "$bin" | grep -q ' \.symtab '; then
  echo "::error::$bin has a symbol table; release binaries are stripped (strip = true)"
  exit 1
fi
