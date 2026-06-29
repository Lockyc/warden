#!/usr/bin/env bash
# install.sh — build warden from source and install it to /Applications.
# Usage:  bash install.sh
#    or:  curl -fsSL https://raw.githubusercontent.com/lockyc/warden/main/install.sh | bash
#
# The curl URL and the git clone below require the GitHub repo to be public.
#
# Two modes, auto-detected:
#   • IN_REPO     — run from a warden checkout: builds from the current working
#                   tree (this is the mode `just deploy` uses, so local changes
#                   are picked up). No clone/pull.
#   • NOT_IN_REPO — otherwise: manages a persistent source clone at ~/.warden
#                   (clone if absent, git pull if present) and builds from it.
#
# Never relaunches the app (the caller decides) and never depends on `just`.
# For guided setup with prerequisite installation, use /warden:install in Claude Code.
set -euo pipefail

if [[ "$(uname)" != "Darwin" ]]; then
  echo "warden is a macOS-only app; install.sh only runs on macOS." >&2
  exit 1
fi

REPO_URL="https://github.com/lockyc/warden"
INSTALL_DIR="$HOME/.warden"

# 1. Resolve the source dir (IN_REPO vs clone at ~/.warden).
if [ -f install.sh ] && [ -f crates/warden-app/tauri.conf.json ]; then
  SRC="$(pwd)"
  echo "→ building from the current warden checkout: $SRC"
else
  if [ ! -e "$INSTALL_DIR" ]; then
    echo "→ cloning warden into $INSTALL_DIR"
    git clone "$REPO_URL" "$INSTALL_DIR"
  elif [ -d "$INSTALL_DIR/.git" ]; then
    echo "→ updating warden clone in $INSTALL_DIR"
    git -C "$INSTALL_DIR" pull --ff-only
  else
    echo "warden: $INSTALL_DIR exists but is not a git clone — move it aside and re-run." >&2
    exit 1
  fi
  SRC="$INSTALL_DIR"
fi

# 2. Hard prerequisites. /warden:install offers to install these; the bare
#    script only refuses with a hint (except the Tauri CLI, which it backstops).
missing=0
for c in git cargo; do
  if ! command -v "$c" >/dev/null 2>&1; then
    echo "warden: '$c' is required but not found on PATH" >&2
    missing=1
  fi
done
if [ "$missing" -ne 0 ]; then
  echo "warden: install Rust (https://rustup.rs) and Xcode Command Line Tools" >&2
  echo "        (xcode-select --install), then re-run." >&2
  exit 1
fi

# 3. Tauri CLI backstop — warden has no npm, so the CLI is a cargo global.
if ! command -v cargo-tauri >/dev/null 2>&1; then
  echo "→ installing the Tauri CLI (cargo install tauri-cli — this takes a while)"
  cargo install tauri-cli --version '^2' --locked
fi

# 4. Build the release bundle from crates/warden-app.
cd "$SRC"
echo "→ building release bundle (this takes a few minutes)"
( cd crates/warden-app && cargo tauri build )

# 5. Install the built app into /Applications.
bash scripts/install-app.sh "target/release/bundle/macos/warden.app"

# 6. Seed the user config from the example (never overwrite an existing one).
mkdir -p "$HOME/.config/warden"
if [ ! -f "$HOME/.config/warden/config.toml" ]; then
  cp examples/config.toml "$HOME/.config/warden/config.toml"
  echo "→ seeded ~/.config/warden/config.toml from the example"
else
  echo "→ ~/.config/warden/config.toml already exists — left untouched"
fi

echo ""
echo "✓ warden installed to /Applications/warden.app"
echo "  Edit ~/.config/warden/config.toml to define your windows + tabs (hot-reloads on save)."
echo "  Update any time by re-running this installer (it git-pulls + rebuilds)."
