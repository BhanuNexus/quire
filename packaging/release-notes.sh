#!/usr/bin/env bash
# Print the CHANGELOG.md section for a version (used as GitHub release notes).
#   packaging/release-notes.sh 0.4.0
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

## Which download?

Every release comes in two editions. They're the same app working on the same files and settings, so you can switch between them in either direction at any time. Installing one replaces the other, and your notes, settings and sync carry over.

| | **Quire** | **Quire Lite** |
|---|---|---|
| Everything else (editor, links, search, sync, graph, callouts, spelling…) | ✓ | ✓ |
| Math and Mermaid diagrams drawn in place | ✓ | shown as their source |
| Export as PDF | ✓ | — |
| PDF viewer (pages, highlights) | ✓ | the PDF's text and highlights |
| Size | larger | about 60 % of Quire |

| | Quire | Quire Lite |
|---|---|---|
| **Windows** | \`Quire-$version-windows-x64-setup.exe\` (or \`-portable.zip\`) | \`Quire-Lite-$version-windows-x64-setup.exe\` (or \`-portable.zip\`) |
| **macOS** (Apple Silicon and Intel) | \`Quire-$version-macos-universal.dmg\` | \`Quire-Lite-$version-macos-universal.dmg\` |
| **Linux** | \`.deb\`, \`.AppImage\` or \`.tar.gz\` named \`Quire-$version-…\` | the same, named \`Quire-Lite-$version-…\` |

Not sure? Take **Quire**. Take **Quire Lite** if download size matters more to you than math and PDF export.

Check a download with \`sha256sum -c SHA256SUMS --ignore-missing\`, and verify it was built by this repository's release workflow with \`gh attestation verify <file> --repo BhanuNexus/quire\`.
FOOT
