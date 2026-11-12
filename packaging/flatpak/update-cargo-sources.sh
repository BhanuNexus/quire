#!/usr/bin/env bash
# Write the offline Cargo sources a Flatpak build needs, from Cargo.lock:
# every crate as a flatpak-builder "archive" source with its checksum, and
# the Cargo config that points at them. flatpak-builder builds without a
# network, so this file has to sit next to the manifest:
#
#   packaging/flatpak/update-cargo-sources.sh [out-file]
#
# out-file defaults to packaging/flatpak/cargo-sources.json (git-ignored: the
# release workflow generates it for each build; for Flathub it is committed
# to the Flathub repository, next to the manifest, and regenerated on every
# release or Cargo.lock change).
#
# The generator is flatpak-builder-tools' flatpak-cargo-generator.py at a
# pinned commit, checked against its SHA-256 below. Needs python3 with venv
# and network access to GitHub and PyPI. It reads only Cargo.lock's names,
# versions and checksums; it downloads no crates.
set -euo pipefail
root="$(cd "$(dirname "$0")/../.." && pwd)"
out="${1:-$root/packaging/flatpak/cargo-sources.json}"
commit="9d86d336a39b9af38b86f114b6ce5820fe260f91"
sha256="0a2db6be87d75910facef28ab46d4d6460802e8419ab850d0caa6a364d26b380"

tmp="$(mktemp -d)"
trap 'rm -rf "$tmp"' EXIT
curl -sSfL --retry 3 -o "$tmp/flatpak-cargo-generator.py" \
  "https://raw.githubusercontent.com/flatpak/flatpak-builder-tools/$commit/cargo/flatpak-cargo-generator.py"
echo "$sha256  $tmp/flatpak-cargo-generator.py" | sha256sum -c --quiet -
python3 -m venv "$tmp/venv"
"$tmp/venv/bin/pip" install --quiet --disable-pip-version-check -r "$root/packaging/flatpak/requirements.txt"
# -I: nothing from the current directory or the environment is imported.
"$tmp/venv/bin/python" -I "$tmp/flatpak-cargo-generator.py" "$root/Cargo.lock" -o "$out"
echo "wrote $out ($(grep -o '"type": "archive"' "$out" | wc -l) crates)"
