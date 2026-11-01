#!/usr/bin/env bash
# Download the AppImage tools at pinned versions and check their SHA-256,
# then print the environment package.sh needs:
#
#   eval "$(packaging/linux/fetch-appimagetool.sh <dir>)"
#
# Bump a version by updating both its URL and its checksum.
set -euo pipefail
dir="${1:?usage: fetch-appimagetool.sh <dir>}"
mkdir -p "$dir"

fetch() { # url sha256 file
  curl -sSfL --retry 3 -o "$dir/$3" "$1"
  echo "$2  $dir/$3" | sha256sum -c --quiet - >&2
}

fetch https://github.com/AppImage/appimagetool/releases/download/1.9.1/appimagetool-x86_64.AppImage \
  ed4ce84f0d9caff66f50bcca6ff6f35aae54ce8135408b3fa33abfc3cb384eb0 appimagetool
fetch https://github.com/AppImage/type2-runtime/releases/download/20251108/runtime-x86_64 \
  2fca8b443c92510f1483a883f60061ad09b46b978b2631c807cd873a47ec260d runtime-x86_64
chmod 755 "$dir/appimagetool"

echo "export APPIMAGETOOL='$(cd "$dir" && pwd)/appimagetool' APPIMAGE_RUNTIME='$(cd "$dir" && pwd)/runtime-x86_64'"
