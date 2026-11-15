#!/usr/bin/env bash
# Builds dist/Rustcord-<version>-macos-universal.dmg: a universal (Apple silicon + Intel)
# Rustcord.app, ad-hoc signed, not notarized (README, "macOS"), and the same bundle as
# dist/Rustcord-<version>-macos-universal.app.tar.gz for the self-updater
# (docs/UPDATER_DESIGN.md §2). Runs only on macOS with the Xcode command line tools
# (CI: .github/workflows/release.yml).
#   bash scripts/build-macos.sh
set -euo pipefail

root=$(cd "$(dirname "$0")/.." && pwd)
cd "$root"
version=$(grep -m1 '^version = ' Cargo.toml | cut -d'"' -f2)
targets=(aarch64-apple-darwin x86_64-apple-darwin)
# Info.plist's LSMinimumSystemVersion, also for the C/C++ code cc-rs builds (openh264).
export MACOSX_DEPLOYMENT_TARGET=11.0

rustup target add "${targets[@]}"
bins=()
for t in "${targets[@]}"; do
	cargo build --release --locked --bin rustcord --target "$t"
	bins+=("${CARGO_TARGET_DIR:-target}/$t/release/rustcord")
done

dist=$root/dist
work=$dist/mac
rm -rf "$work"
# The dmg's contents: the app and a link to /Applications to drag it onto.
staging=$work/dmg
app=$staging/Rustcord.app
mkdir -p "$app/Contents/MacOS" "$app/Contents/Resources"
lipo -create -output "$app/Contents/MacOS/rustcord" "${bins[@]}"
lipo -info "$app/Contents/MacOS/rustcord"
sed "s/@VERSION@/$version/g" assets/macos/Info.plist >"$app/Contents/Info.plist"
plutil -lint "$app/Contents/Info.plist"

iconset=$work/Rustcord.iconset
mkdir -p "$iconset"
for icon in 16x16:16 16x16@2x:32 32x32:32 32x32@2x:64 128x128:128 128x128@2x:256 \
	256x256:256 256x256@2x:512 512x512:512 512x512@2x:1024; do
	cp "assets/icons/rustcord-${icon#*:}.png" "$iconset/icon_${icon%%:*}.png"
done
iconutil -c icns -o "$app/Contents/Resources/Rustcord.icns" "$iconset"

# Ad-hoc signature: Apple silicon does not run unsigned code.
codesign --force --sign - "$app"
codesign --verify --strict --verbose=2 "$app"

# The updater's package: the signed bundle alone, without AppleDouble files or extended
# attributes; extracted again to prove the signature survives the round trip.
appgz=$dist/Rustcord-$version-macos-universal.app.tar.gz
COPYFILE_DISABLE=1 tar --no-mac-metadata --no-xattrs -C "$staging" -czf "$appgz" Rustcord.app
mkdir "$work/verify"
tar -xzf "$appgz" -C "$work/verify"
codesign --verify --strict --verbose=2 "$work/verify/Rustcord.app"

ln -s /Applications "$staging/Applications"
dmg=$dist/Rustcord-$version-macos-universal.dmg
# hdiutil sometimes fails with "Resource busy" on CI runners: try three times.
for attempt in 1 2 3; do
	hdiutil create -volname Rustcord -srcfolder "$staging" -ov -format UDZO "$dmg" && break
	[ "$attempt" = 3 ] && exit 1
	sleep 10
done
ls -lh "$dmg" "$appgz"
