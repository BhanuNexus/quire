#!/usr/bin/env bash
# Build the Windows x64 installer and portable zip.
#
#   packaging/windows/package.sh [gnu|msvc]
#
# gnu  (default): cross-compile from Linux/macOS with mingw-w64 + makensis.
# msvc: on Windows (CI, Git Bash), with the MSVC toolchain and NSIS installed.
#
# Optional Authenticode signing: set WINDOWS_CERT_PFX (path) and
# WINDOWS_CERT_PASSWORD; signs with signtool (Windows) or osslsigncode (Linux).
set -euo pipefail
cd "$(dirname "$0")/../.."

flavor="${1:-gnu}"
target="x86_64-pc-windows-$flavor"
version="$(sed -n 's/^version = "\(.*\)"/\1/p' Cargo.toml | head -1)"
out="dist"
stage="$out/windows-stage"
name="Quire-$version-windows-x64"

cargo build --release --locked --target "$target" -p quire-desktop -p quire-cli

rm -rf "$stage" && mkdir -p "$stage"
cp "target/$target/release/quire.exe" "target/$target/release/quire-cli.exe" "$stage/"
cp README.md "$stage/README.md"
# CRLF so Notepad and the installer's licence page show line breaks.
sed 's/$/\r/' LICENSE > "$stage/LICENSE.txt"

sign() {
  [[ -n "${WINDOWS_CERT_PFX:-}" ]] || return 0
  local f="$1"
  if command -v signtool >/dev/null 2>&1; then
    signtool sign /fd sha256 /tr http://timestamp.digicert.com /td sha256 \
      /f "$WINDOWS_CERT_PFX" /p "${WINDOWS_CERT_PASSWORD:-}" "$f"
  elif command -v osslsigncode >/dev/null 2>&1; then
    osslsigncode sign -pkcs12 "$WINDOWS_CERT_PFX" -pass "${WINDOWS_CERT_PASSWORD:-}" \
      -h sha256 -t http://timestamp.digicert.com -in "$f" -out "$f.signed" && mv "$f.signed" "$f"
  else
    echo "warning: WINDOWS_CERT_PFX set but no signtool/osslsigncode; leaving $f unsigned" >&2
  fi
}
sign "$stage/quire.exe"
sign "$stage/quire-cli.exe"

# makensis wants native paths on Windows.
# Absolute paths: makensis changes into the script's folder.
nsi="packaging/windows/quire.nsi"
src="$stage"
setup="$out/$name-setup.exe"
if command -v cygpath >/dev/null 2>&1; then
  src="$(cygpath -wa "$stage")"; nsi="$(cygpath -wa "$nsi")"; setup_native="$(cygpath -wa "$setup")"
else
  setup_native="$(pwd)/$setup"; src="$(pwd)/$stage"; nsi="$(pwd)/$nsi"
fi
makensis -V2 -DVERSION="$version" -DSRCDIR="$src" -DOUTFILE="$setup_native" "$nsi"
sign "$setup"

( cd "$stage" && rm -f "../$name-portable.zip" && \
  if command -v zip >/dev/null 2>&1; then zip -q -9 "../$name-portable.zip" quire.exe quire-cli.exe README.md LICENSE.txt; \
  else powershell -NoProfile -Command "Compress-Archive -Path quire.exe,quire-cli.exe,README.md,LICENSE.txt -DestinationPath ../$name-portable.zip"; fi )

( cd "$out" && sha256sum "$name-setup.exe" "$name-portable.zip" > "$name.sha256" )
echo "Built:"; ls -la "$out/$name"*
