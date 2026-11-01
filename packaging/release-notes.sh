#!/usr/bin/env bash
# Print the CHANGELOG.md section for a version (used as GitHub release notes).
#   packaging/release-notes.sh 0.3.1
set -euo pipefail
version="${1:?usage: release-notes.sh <version>}"
cd "$(dirname "$0")/.."
notes="$(awk -v v="$version" '
  $0 ~ "^## \\[" v "\\]" { on = 1; next }
  on && /^## \[/ { exit }
  on && /^\[[^]]+\]: / { next }
  on { print }
' CHANGELOG.md)"
if [[ -z "${notes//[[:space:]]/}" ]]; then
  echo "error: no '## [$version]' section in CHANGELOG.md" >&2
  exit 1
fi
# Trim leading blank lines.
printf '%s\n' "$notes" | sed '/./,$!d'
cat <<FOOT

---

**Downloads:** Windows (installer or portable zip), macOS (universal .dmg), Linux (x86_64 tar.gz). Check a download with \`sha256sum -c SHA256SUMS --ignore-missing\`, and verify it was built by this repository's release workflow with \`gh attestation verify <file> --repo BhanuNexus/quire\`.
FOOT
