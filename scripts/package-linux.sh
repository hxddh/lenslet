#!/usr/bin/env bash
# Package the release build as lenslet-<version>-linux-<arch>.tar.gz in <dist>.
set -euo pipefail
version="$1"; dist="$2"
arch="$(uname -m)"
name="lenslet-${version}-linux-${arch}"
stage="$(mktemp -d)/${name}"
mkdir -p "$stage/share/applications" "$stage/share/icons/hicolor/512x512/apps" \
  "$stage/share/icons/hicolor/scalable/apps" "$dist"
install -Dm755 target/release/lenslet "$stage/bin/lenslet"
cp assets/app-icon-512.png "$stage/share/icons/hicolor/512x512/apps/dev.lenslet.Lenslet.png"
cp assets/app-icon.svg "$stage/share/icons/hicolor/scalable/apps/dev.lenslet.Lenslet.svg"
cat > "$stage/share/applications/dev.lenslet.Lenslet.desktop" <<DESKTOP
[Desktop Entry]
Type=Application
Name=Lenslet
Comment=A local AWS account in a window
Exec=lenslet
Icon=dev.lenslet.Lenslet
Categories=Development;
DESKTOP
cp README.md LICENSE "$stage/" 2>/dev/null || cp README.md "$stage/"
tar -C "$(dirname "$stage")" -czf "$dist/${name}.tar.gz" "$name"
echo "$dist/${name}.tar.gz"
