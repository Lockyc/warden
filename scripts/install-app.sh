#!/usr/bin/env bash
# install-app.sh <built-warden.app> — place a freshly built warden.app into /Applications
# (override with WARDEN_APP_DEST), quitting any running copy first and stripping the quarantine
# xattr so an unsigned local build isn't blocked by Gatekeeper. Does NOT relaunch — the caller decides.
set -euo pipefail

src="${1:?usage: install-app.sh <built warden.app>}"
dest="${WARDEN_APP_DEST:-/Applications/warden.app}"

[ -d "$src" ] || { echo "install-app.sh: no app bundle at $src" >&2; exit 1; }
[[ "$dest" == *.app ]] || { echo "install-app.sh: refusing — dest is not an .app bundle: $dest" >&2; exit 1; }

osascript -e 'quit app "warden"' 2>/dev/null || true
pkill -f "${dest}/" 2>/dev/null || true
sleep 1

rm -rf "$dest"
mkdir -p "$(dirname "$dest")"
cp -R "$src" "$dest"
xattr -dr com.apple.quarantine "$dest" 2>/dev/null || true

echo "install-app.sh: installed → $dest"
