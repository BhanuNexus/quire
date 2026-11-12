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
| **Linux** (x86_64 and ARM64) | \`.deb\`, \`.AppImage\` or \`.tar.gz\` named \`Quire-$version-linux-…\`: \`amd64\` and \`x86_64\`, or \`arm64\` and \`aarch64\` | the same, named \`Quire-Lite-$version-linux-…\` |
| **Flatpak** (x86_64 and ARM64) | \`Quire-$version-linux-x86_64.flatpak\`, \`Quire-$version-linux-aarch64.flatpak\` | not built (Quire only) |

Not sure? Take **Quire**. Take **Quire Lite** if download size matters more to you than math and PDF export.

Quire is listed under "Open with" for \`.md\` files in the Linux packages, the Flatpak and the Windows installer; it never makes itself the default. The sync server (\`quire-sync-server-$version-linux-x86_64.tar.gz\`) is x86_64 only. The AI engine downloads (\`quire-ai-engine-<target>.gz\`) are what Quire Lite fetches; you don't need them.

Check a download with \`sha256sum -c SHA256SUMS --ignore-missing\`, and verify it was built by this repository's release workflow with \`gh attestation verify <file> --repo BhanuNexus/quire\`.
FOOT
