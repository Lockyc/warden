#!/usr/bin/env bash
# Regenerate warden's raster icons from the SVG masters.
#
# Sources (hand-authored, the source of truth):
#   icon.svg      — transparent shield master (README / favicon)
#   icon-app.svg  — macOS squircle app-icon tile (the .icns)
#
# Outputs (committed, but reproducible from the above):
#   icon-1024.png, icon-512.png, favicon-32.png, warden.icns
#   crates/warden-app/icons/icon.icns  — the same .icns where the Tauri bundle consumes it
#
# Deps (macOS):  rsvg-convert  (brew install librsvg)
#                iconutil      (ships with macOS)
#
# Usage:  cd assets && ./build-icons.sh
set -euo pipefail
cd "$(dirname "$0")"

command -v rsvg-convert >/dev/null || { echo "need rsvg-convert (brew install librsvg)" >&2; exit 1; }
command -v iconutil     >/dev/null || { echo "need iconutil (macOS)" >&2; exit 1; }

# Transparent shield PNGs (README hero + favicon)
rsvg-convert -w 1024 -h 1024 icon.svg -o icon-1024.png
rsvg-convert -w 512  -h 512  icon.svg -o icon-512.png
rsvg-convert -w 32   -h 32   icon.svg -o favicon-32.png

# macOS .icns from the app tile, via a temporary .iconset
ICON=warden.iconset
rm -rf "$ICON"; mkdir "$ICON"
for sz in 16 32 64 128 256 512 1024; do
  rsvg-convert -w "$sz" -h "$sz" icon-app.svg -o "$ICON/_$sz.png"
done
cp "$ICON/_16.png"   "$ICON/icon_16x16.png"
cp "$ICON/_32.png"   "$ICON/icon_16x16@2x.png"
cp "$ICON/_32.png"   "$ICON/icon_32x32.png"
cp "$ICON/_64.png"   "$ICON/icon_32x32@2x.png"
cp "$ICON/_128.png"  "$ICON/icon_128x128.png"
cp "$ICON/_256.png"  "$ICON/icon_128x128@2x.png"
cp "$ICON/_256.png"  "$ICON/icon_256x256.png"
cp "$ICON/_512.png"  "$ICON/icon_256x256@2x.png"
cp "$ICON/_512.png"  "$ICON/icon_512x512.png"
cp "$ICON/_1024.png" "$ICON/icon_512x512@2x.png"
rm "$ICON"/_*.png
iconutil -c icns "$ICON" -o warden.icns
rm -rf "$ICON"

# Publish the .icns where the Tauri bundle consumes it (path relative to the app's tauri.conf.json).
APP_ICONS=../crates/warden-app/icons
mkdir -p "$APP_ICONS"
cp warden.icns "$APP_ICONS/icon.icns"

echo "regenerated: icon-1024.png icon-512.png favicon-32.png warden.icns crates/warden-app/icons/icon.icns"
