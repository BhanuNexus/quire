#!/usr/bin/env bash
# Build the Windows x64 installer and portable zip.
#
#   packaging/windows/package.sh [gnu|msvc]
#   EDITION=lite packaging/windows/package.sh …   Quire Lite (Quire-Lite-<v>-windows-x64-…)
#
# Both editions install to the same place under the same name, so running
# either installer replaces the other and keeps all settings.
#
# gnu  (default): cross-compile from Linux/macOS with mingw-w64 + makensis.
# msvc: on Windows (CI, Git Bash), with the MSVC toolchain and NSIS installed.
#
# Optional Authenticode signing: set WINDOWS_CERT_PFX (path) and
# WINDOWS_CERT_PASSWORD; signs with signtool (Windows) or osslsigncode (Linux).
#
# ENGINE_BIN=<path to quire-ai-engine.exe> puts the on-device AI engine in
# the full edition's install folder and portable zip, next to quire.exe. It
# is signed first and the app is then built with the SHA-256 of the signed
# file (crates/quire-ai/engine.bundled.sha256), which the app checks before
# every start (design §D.3). Quire Lite never includes it.
set -euo pipefail
cd "$(dirname "$0")/../.."

flavor="${1:-gnu}"
target="x86_64-pc-windows-$flavor"
version="$(sed -n 's/^version = "\(.*\)"/\1/p' Cargo.toml | head -1)"
out="dist"
stage="$out/windows-stage"
edition="${EDITION:-full}"
case "$edition" in
  full) prefix="Quire"; brand="Quire"; features=() ;;
  lite) prefix="Quire-Lite"; brand="Quire Lite"; features=(--no-default-features) ;;
  *) echo "EDITION must be full or lite" >&2; exit 1 ;;
esac
name="$prefix-$version-windows-x64"

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

rm -rf "$stage" && mkdir -p "$stage"

# ---------------------------------------------------------------- engine
engine="${ENGINE_BIN:-}"
engine_pin="crates/quire-ai/engine.bundled.sha256"
rm -f "$engine_pin"
if [[ -n "$engine" && "$edition" != full ]]; then
  echo "note: Quire Lite doesn't include the engine; ignoring ENGINE_BIN" >&2
  engine=""
fi
files=(quire.exe quire-cli.exe README.md LICENSE.txt)
if [[ -n "$engine" ]]; then
  [[ -f "$engine" ]] || { echo "ENGINE_BIN: no file at $engine" >&2; exit 1; }
  cp "$engine" "$stage/quire-ai-engine.exe"
  # Signed before it is hashed: the app pins the file exactly as shipped.
  sign "$stage/quire-ai-engine.exe"
  sha256sum < "$stage/quire-ai-engine.exe" | cut -c1-64 > "$engine_pin"
  # Only these builds embed it; a later development build must not.
  trap 'rm -f "$engine_pin"' EXIT
  echo "Bundling the engine $engine ($(cat "$engine_pin"))"
  files+=(quire-ai-engine.exe)
fi

cargo build --release --locked --target "$target" -p quire-desktop -p quire-cli ${features[@]+"${features[@]}"}

cp "target/$target/release/quire.exe" "target/$target/release/quire-cli.exe" "$stage/"
cp README.md "$stage/README.md"
# CRLF so Notepad and the installer's licence page show line breaks.
sed 's/$/\r/' LICENSE > "$stage/LICENSE.txt"

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
engine_def=()
[[ -n "$engine" ]] && engine_def=(-DENGINE)
makensis -V2 -DVERSION="$version" -DBRAND="$brand" -DSRCDIR="$src" -DOUTFILE="$setup_native" ${engine_def[@]+"${engine_def[@]}"} "$nsi"
sign "$setup"

( cd "$stage" && rm -f "../$name-portable.zip" && \
  if command -v zip >/dev/null 2>&1; then zip -q -9 "../$name-portable.zip" "${files[@]}"; \
  else ps_files="$(IFS=,; echo "${files[*]}")"; powershell -NoProfile -Command "Compress-Archive -Path $ps_files -DestinationPath ../$name-portable.zip"; fi )

( cd "$out" && sha256sum "$name-setup.exe" "$name-portable.zip" > "$name.sha256" )
echo "Built:"; ls -la "$out/$name"*
