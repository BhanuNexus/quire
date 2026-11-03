#!/usr/bin/env bash
# Fails when a release binary is over its size budget (docs/design/0.9.md §2.3).
#   packaging/check-size.sh <binary> full|lite|engine
# Budgets are for the stripped release binary, in MiB.
set -euo pipefail
bin="$1"
edition="$2"
case "$edition" in
  full) budget=96 ;;
  lite) budget=56 ;;
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
