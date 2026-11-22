#!/usr/bin/env bash
# Package the universal release build as Lenslet.app inside
# lenslet-<version>-macos.dmg in <dist>.
set -euo pipefail
version="$1"; dist="$2"
work="$(mktemp -d)"
app="$work/Lenslet.app"
mkdir -p "$app/Contents/MacOS" "$app/Contents/Resources" "$dist"
# Universal when both architectures were built, else the one that was.
built=()
for arch in aarch64 x86_64; do
  bin="target/${arch}-apple-darwin/release/lenslet"
  [ -f "$bin" ] && built+=("$bin")
done
[ "${#built[@]}" -gt 0 ] || { echo "no macOS release build found" >&2; exit 1; }
lipo -create -output "$app/Contents/MacOS/lenslet" "${built[@]}"
iconset="$work/AppIcon.iconset"
mkdir -p "$iconset"
for size in 16 32 128 256 512; do
  sips -z $size $size assets/app-icon.png --out "$iconset/icon_${size}x${size}.png" >/dev/null
  double=$((size * 2))
  sips -z $double $double assets/app-icon.png --out "$iconset/icon_${size}x${size}@2x.png" >/dev/null
done
iconutil -c icns "$iconset" -o "$app/Contents/Resources/AppIcon.icns"
cat > "$app/Contents/Info.plist" <<PLIST
<?xml version="1.0" encoding="UTF-8"?>
<!DOCTYPE plist PUBLIC "-//Apple//DTD PLIST 1.0//EN" "http://www.apple.com/DTDs/PropertyList-1.0.dtd">
<plist version="1.0">
<dict>
  <key>CFBundleName</key><string>Lenslet</string>
  <key>CFBundleDisplayName</key><string>Lenslet</string>
  <key>CFBundleIdentifier</key><string>dev.lenslet.Lenslet</string>
  <key>CFBundleExecutable</key><string>lenslet</string>
  <key>CFBundleIconFile</key><string>AppIcon</string>
  <key>CFBundlePackageType</key><string>APPL</string>
  <key>CFBundleShortVersionString</key><string>${version}</string>
  <key>CFBundleVersion</key><string>${version}</string>
  <key>LSMinimumSystemVersion</key><string>12.0</string>
  <key>NSHighResolutionCapable</key><true/>
</dict>
</plist>
PLIST
codesign --force --deep --sign - "$app"
ln -s /Applications "$work/Applications"
rm -rf "$iconset"
hdiutil create -volname Lenslet -srcfolder "$work" -ov -format UDZO "$dist/lenslet-${version}-macos.dmg"
echo "$dist/lenslet-${version}-macos.dmg"
