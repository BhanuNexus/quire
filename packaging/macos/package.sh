#!/usr/bin/env bash
# Build Quire.app (universal: Apple Silicon + Intel) and a .dmg. Runs on macOS.
#
#   packaging/macos/package.sh             the full edition
#   EDITION=lite packaging/macos/package.sh  Quire Lite (Quire-Lite-<v>-macos-universal.dmg)
#
# Both editions are Quire.app with the same bundle id, so dragging either
# into Applications replaces the other and keeps all settings.
#
# Signing and notarization are optional and driven by environment variables:
#   MACOS_SIGNING_IDENTITY   e.g. "Developer ID Application: Your Name (TEAMID)"
#   APPLE_ID, APPLE_TEAM_ID, APPLE_APP_PASSWORD   (app-specific password) for notarytool
# Without them the app is ad-hoc signed: it runs on the machine that built it,
# and elsewhere after right-click → Open (Gatekeeper warns about unsigned apps).
#
# ENGINE_BIN=<universal quire-ai-engine> (both arm64 and x86_64, made with
# lipo) puts the on-device AI engine in the full edition's
# Quire.app/Contents/MacOS/, next to `quire`. It is signed first and the
# app is then built with the SHA-256 of that signed universal file
# (crates/quire-ai/engine.bundled.sha256), which the app checks before every
# start (design §D.3): signing changes the bytes, so the per-target download
# pins can't be used for it. Quire Lite never includes it.
set -euo pipefail
cd "$(dirname "$0")/../.."

[[ "$(uname)" == "Darwin" ]] || { echo "macOS packaging needs macOS (lipo, codesign, hdiutil)"; exit 1; }

version="$(sed -n 's/^version = "\(.*\)"/\1/p' Cargo.toml | head -1)"
export MACOSX_DEPLOYMENT_TARGET=11.0
out="dist"
edition="${EDITION:-full}"
case "$edition" in
  full) prefix="Quire"; volume="Quire $version"; features=() ;;
  lite) prefix="Quire-Lite"; volume="Quire Lite $version"; features=(--no-default-features) ;;
  *) echo "EDITION must be full or lite" >&2; exit 1 ;;
esac
name="$prefix-$version-macos-universal"
app="$out/Quire.app"
mkdir -p "$out"

identity="${MACOS_SIGNING_IDENTITY:--}"
sign_flags=(--force --timestamp --options runtime --sign "$identity")
[[ "$identity" == "-" ]] && sign_flags=(--force --sign -)

# ---------------------------------------------------------------- engine
engine="${ENGINE_BIN:-}"
engine_pin="crates/quire-ai/engine.bundled.sha256"
signed_engine="$out/quire-ai-engine.signed"
rm -f "$engine_pin" "$signed_engine"
if [[ -n "$engine" && "$edition" != full ]]; then
  echo "note: Quire Lite doesn't include the engine; ignoring ENGINE_BIN" >&2
  engine=""
fi
if [[ -n "$engine" ]]; then
  [[ -f "$engine" ]] || { echo "ENGINE_BIN: no file at $engine" >&2; exit 1; }
  lipo "$engine" -verify_arch arm64 x86_64 || { echo "ENGINE_BIN must be universal (arm64 and x86_64)" >&2; exit 1; }
  cp "$engine" "$signed_engine"
  chmod 755 "$signed_engine"
  # Signed before it is hashed: the app pins the file exactly as shipped.
  codesign "${sign_flags[@]}" --identifier dev.quire.ai-engine "$signed_engine"
  shasum -a 256 "$signed_engine" | cut -d' ' -f1 > "$engine_pin"
  # Only these builds embed it; a later development build must not.
  trap 'rm -f "$engine_pin" "$signed_engine"' EXIT
  echo "Bundling the engine $engine ($(cat "$engine_pin"))"
fi

for t in aarch64-apple-darwin x86_64-apple-darwin; do
  cargo build --release --locked --target "$t" -p quire-desktop -p quire-cli ${features[@]+"${features[@]}"}
done

rm -rf "$app" && mkdir -p "$app/Contents/MacOS" "$app/Contents/Resources"
for bin in quire quire-cli; do
  lipo -create -output "$app/Contents/MacOS/$bin" \
    "target/aarch64-apple-darwin/release/$bin" "target/x86_64-apple-darwin/release/$bin"
done
cp packaging/icons/quire.icns "$app/Contents/Resources/quire.icns"
cp LICENSE "$app/Contents/Resources/LICENSE"
sed "s/@VERSION@/$version/g" packaging/macos/Info.plist > "$app/Contents/Info.plist"
printf 'APPL????' > "$app/Contents/PkgInfo"
plutil -lint "$app/Contents/Info.plist"
# Already signed, and never signed again (that would change its hash).
[[ -n "$engine" ]] && cp -p "$signed_engine" "$app/Contents/MacOS/quire-ai-engine"

# Inner executables first, then the bundle (without --deep, which would
# re-sign the engine).
codesign "${sign_flags[@]}" "$app/Contents/MacOS/quire-cli"
codesign "${sign_flags[@]}" "$app"
codesign --verify --deep --strict --verbose=2 "$app"
if [[ -n "$engine" ]]; then
  [[ "$(shasum -a 256 "$app/Contents/MacOS/quire-ai-engine" | cut -d' ' -f1)" == "$(cat "$engine_pin")" ]] \
    || { echo "the bundled engine changed after it was pinned" >&2; exit 1; }
fi

dmg="$out/$name.dmg"
staging="$(mktemp -d)"
cp -R "$app" "$staging/"
ln -s /Applications "$staging/Applications"
rm -f "$dmg"
hdiutil create -volname "$volume" -srcfolder "$staging" -ov -format UDZO "$dmg"
rm -rf "$staging"
[[ "$identity" != "-" ]] && codesign --force --timestamp --sign "$identity" "$dmg"

if [[ "$identity" != "-" && -n "${APPLE_ID:-}" && -n "${APPLE_TEAM_ID:-}" && -n "${APPLE_APP_PASSWORD:-}" ]]; then
  xcrun notarytool submit "$dmg" --apple-id "$APPLE_ID" --team-id "$APPLE_TEAM_ID" \
    --password "$APPLE_APP_PASSWORD" --wait
  xcrun stapler staple "$dmg"
  spctl --assess --type open --context context:primary-signature --verbose "$dmg" || true
else
  echo "note: not notarized (set MACOS_SIGNING_IDENTITY, APPLE_ID, APPLE_TEAM_ID, APPLE_APP_PASSWORD)"
fi

( cd "$out" && shasum -a 256 "$name.dmg" > "$name.sha256" )
echo "Built:"; ls -la "$out/$name"*
