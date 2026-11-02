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

for t in aarch64-apple-darwin x86_64-apple-darwin; do
  cargo build --release --locked --target "$t" -p quire-desktop -p quire-cli "${features[@]}"
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

identity="${MACOS_SIGNING_IDENTITY:--}"
sign_flags=(--force --timestamp --options runtime --sign "$identity")
[[ "$identity" == "-" ]] && sign_flags=(--force --sign -)
# Inner executables first, then the bundle.
codesign "${sign_flags[@]}" "$app/Contents/MacOS/quire-cli"
codesign "${sign_flags[@]}" "$app"
codesign --verify --deep --strict --verbose=2 "$app"

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
