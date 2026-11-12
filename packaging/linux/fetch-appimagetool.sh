#!/usr/bin/env bash
# Download the AppImage tools at pinned versions and check their SHA-256,
# then print the environment package.sh needs:
#
#   eval "$(packaging/linux/fetch-appimagetool.sh <dir> [x86_64|aarch64])"
#
# The architecture defaults to this machine's: the tool runs here, so it
# has to be this machine's. Bump a version by updating its URLs and the
# checksums of every architecture (the x86_64 pair is what 0.10 shipped;
# the aarch64 pair is the same release's).
set -euo pipefail
dir="${1:?usage: fetch-appimagetool.sh <dir> [x86_64|aarch64]}"
arch="${2:-$(uname -m)}"
[[ "$arch" == arm64 ]] && arch=aarch64
case "$arch" in
  x86_64)
    tool_sha=ed4ce84f0d9caff66f50bcca6ff6f35aae54ce8135408b3fa33abfc3cb384eb0
    runtime_sha=2fca8b443c92510f1483a883f60061ad09b46b978b2631c807cd873a47ec260d ;;
  aarch64)
    tool_sha=f0837e7448a0c1e4e650a93bb3e85802546e60654ef287576f46c71c126a9158
    runtime_sha=00cbdfcf917cc6c0ff6d3347d59e0ca1f7f45a6df1a428a0d6d8a78664d87444 ;;
  *) echo "unsupported architecture $arch (x86_64 or aarch64)" >&2; exit 1 ;;
esac
mkdir -p "$dir"

fetch() { # url sha256 file
  curl -sSfL --retry 3 -o "$dir/$3" "$1"
  echo "$2  $dir/$3" | sha256sum -c --quiet - >&2
}

fetch "https://github.com/AppImage/appimagetool/releases/download/1.9.1/appimagetool-$arch.AppImage" "$tool_sha" appimagetool
fetch "https://github.com/AppImage/type2-runtime/releases/download/20251108/runtime-$arch" "$runtime_sha" "runtime-$arch"
chmod 755 "$dir/appimagetool"

echo "export APPIMAGETOOL='$(cd "$dir" && pwd)/appimagetool' APPIMAGE_RUNTIME='$(cd "$dir" && pwd)/runtime-$arch'"
