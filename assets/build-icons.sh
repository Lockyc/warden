#!/usr/bin/env bash
# Regenerate warden's raster icons from the SVG masters.
#
# Sources (hand-authored, the source of truth):
#   icon.svg      — transparent shield master (README / favicon)
#   icon-app.svg  — macOS squircle app-icon tile (the .icns), designed edge to edge;
#                   `plate icon` places it in Apple's icon grid
#
# Outputs (committed, but reproducible from the above):
#   icon-1024.png, icon-512.png, favicon-32.png, warden.icns
#   crates/warden-app/icons/icon.icns  — the same .icns where the Tauri bundle consumes it
#   crates/warden-app/icons/icon.png   — the runtime default window icon Tauri's
#                                        generate_context! embeds (fixed convention path)
#
# Deps:  rsvg-convert  (brew install librsvg)
#        plate         (github.com/lockyc/plate)
#
# Usage:  cd assets && ./build-icons.sh
set -euo pipefail
cd "$(dirname "$0")"

command -v rsvg-convert >/dev/null || { echo "need rsvg-convert (brew install librsvg)" >&2; exit 1; }
command -v plate        >/dev/null || { echo "need plate (github.com/lockyc/plate)" >&2; exit 1; }

# Transparent shield PNGs (README hero + favicon)
rsvg-convert -w 1024 -h 1024 icon.svg -o icon-1024.png
rsvg-convert -w 512  -h 512  icon.svg -o icon-512.png
rsvg-convert -w 32   -h 32   icon.svg -o favicon-32.png

# Runtime default window icon. Tauri's generate_context! embeds icons/icon.png at a
# FIXED convention path (distinct from the bundle .icns listed in tauri.conf.json), so
# the build fails without it. It is the same icon as the .icns, warden-branded and
# reproducible, not the stale Tauri scaffold placeholder.
APP_ICONS=../crates/warden-app/icons
mkdir -p "$APP_ICONS"
plate icon --icns warden.icns --png 512:"$APP_ICONS/icon.png" icon-app.svg

# Publish the .icns where the Tauri bundle consumes it (path relative to the app's tauri.conf.json).
cp warden.icns "$APP_ICONS/icon.icns"

echo "regenerated: icon-1024.png icon-512.png favicon-32.png warden.icns crates/warden-app/icons/icon.icns crates/warden-app/icons/icon.png"
