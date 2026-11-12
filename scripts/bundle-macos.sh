#!/usr/bin/env bash
# Builds dist/Wgitpui.app plus a .dmg and .zip.
#
#   scripts/bundle-macos.sh              # host architecture
#   scripts/bundle-macos.sh --universal  # arm64 + x86_64 (what CI ships)
#
# The executable inside the bundle is both the app and the CLI: run from a terminal it opens
# the repository in the running app (or starts it in the background) and returns the shell.
# VERSION overrides the version from Cargo.toml.
set -euo pipefail
cd "$(dirname "$0")/.."

APP=Wgitpui
BIN=wgitpui
BUNDLE_ID=dev.wgitpui.app
VERSION=${VERSION:-$(sed -n 's/^version = "\(.*\)"/\1/p' Cargo.toml | head -1)}
DIST=dist
APPDIR="$DIST/$APP.app"
CONTENTS="$APPDIR/Contents"

rm -rf "$DIST"
mkdir -p "$CONTENTS/MacOS" "$CONTENTS/Resources/bin"

# ---- binary -------------------------------------------------------------------------------
if [[ "${1:-}" == "--universal" ]]; then
  rustup target add aarch64-apple-darwin x86_64-apple-darwin >/dev/null
  cargo build --release --locked --target aarch64-apple-darwin --target x86_64-apple-darwin
  lipo -create \
    "target/aarch64-apple-darwin/release/$BIN" \
    "target/x86_64-apple-darwin/release/$BIN" \
    -output "$CONTENTS/MacOS/$BIN"
else
  cargo build --release --locked
  cp "target/release/$BIN" "$CONTENTS/MacOS/$BIN"
fi
# Convenience: add Contents/Resources/bin to PATH instead of symlinking.
ln -s "../../MacOS/$BIN" "$CONTENTS/Resources/bin/$BIN"

# ---- icon ---------------------------------------------------------------------------------
ICONSET="$DIST/AppIcon.iconset"
mkdir -p "$ICONSET"
render() { # size out
  if command -v rsvg-convert >/dev/null; then
    rsvg-convert -w "$1" -h "$1" assets/icon.svg -o "$2"
  else
    if [[ ! -f "$DIST/icon-1024.png" ]]; then
      local tmp; tmp=$(mktemp -d)
      qlmanage -t -s 1024 -o "$tmp" assets/icon.svg >/dev/null 2>&1
      mv "$tmp/icon.svg.png" "$DIST/icon-1024.png"
      rm -rf "$tmp"
    fi
    sips -z "$1" "$1" "$DIST/icon-1024.png" --out "$2" >/dev/null
  fi
}
for s in 16 32 128 256 512; do
  render "$s" "$ICONSET/icon_${s}x${s}.png"
  render $((s * 2)) "$ICONSET/icon_${s}x${s}@2x.png"
done
iconutil -c icns "$ICONSET" -o "$CONTENTS/Resources/AppIcon.icns"
rm -rf "$ICONSET" "$DIST/icon-1024.png"

# ---- Info.plist ---------------------------------------------------------------------------
cat > "$CONTENTS/Info.plist" <<PLIST
<?xml version="1.0" encoding="UTF-8"?>
<!DOCTYPE plist PUBLIC "-//Apple//DTD PLIST 1.0//EN" "http://www.apple.com/DTDs/PropertyList-1.0.dtd">
<plist version="1.0">
<dict>
  <key>CFBundleName</key><string>$APP</string>
  <key>CFBundleDisplayName</key><string>$APP</string>
  <key>CFBundleIdentifier</key><string>$BUNDLE_ID</string>
  <key>CFBundleExecutable</key><string>$BIN</string>
  <key>CFBundleIconFile</key><string>AppIcon</string>
  <key>CFBundlePackageType</key><string>APPL</string>
  <key>CFBundleShortVersionString</key><string>$VERSION</string>
  <key>CFBundleVersion</key><string>$VERSION</string>
  <key>CFBundleInfoDictionaryVersion</key><string>6.0</string>
  <key>LSMinimumSystemVersion</key><string>11.0</string>
  <key>LSApplicationCategoryType</key><string>public.app-category.developer-tools</string>
  <key>NSHighResolutionCapable</key><true/>
  <key>NSSupportsAutomaticGraphicsSwitching</key><true/>
</dict>
</plist>
PLIST

# Ad-hoc signature: required for arm64 binaries to launch at all (not notarized).
codesign --force --deep --sign - "$APPDIR"

# ---- archives -----------------------------------------------------------------------------
ditto -c -k --keepParent "$APPDIR" "$DIST/$APP-$VERSION-macos.zip"

STAGE=$(mktemp -d)
cp -R "$APPDIR" "$STAGE/"
ln -s /Applications "$STAGE/Applications"
cat > "$STAGE/Install command line tool.command" <<'CMD'
#!/bin/sh
# Puts `wgitpui` on your PATH. Then, in any repository: `wgitpui` opens it in the app.
set -e
APP="/Applications/Wgitpui.app"
[ -d "$APP" ] || { echo "Drag Wgitpui.app into Applications first."; exit 1; }
sudo mkdir -p /usr/local/bin
sudo ln -sf "$APP/Contents/MacOS/wgitpui" /usr/local/bin/wgitpui
echo "Installed /usr/local/bin/wgitpui — try it: cd some/repo && wgitpui"
CMD
chmod +x "$STAGE/Install command line tool.command"
hdiutil create -volname "$APP" -srcfolder "$STAGE" -ov -format UDZO "$DIST/$APP-$VERSION-macos.dmg" >/dev/null
rm -rf "$STAGE"

echo "Built:"
ls -1 "$DIST"
