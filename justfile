# warden — task runner

# Recipes run in `sh`, which doesn't inherit cargo from an interactive fish/zsh setup.
# Guarantee rustup's bin dir is on PATH so `cargo` is found.
export PATH := env_var('HOME') + "/.cargo/bin:" + env_var('PATH')

# List available recipes
default:
    @just --list

# git-init the mock example project dirs into real repos so the demo config's [[window.root]]
# tree section has projects to discover. Idempotent (git init is safe to re-run); the created
# .git dirs are git-ignored. Run automatically by `just run`.
[group("dev")]
seed-examples:
    @for d in "{{justfile_directory()}}"/examples/projects/*/*/; do git -C "$d" init -q; done

# Run the app against the repo's demo config (never touches your real ~/.config/warden config)
[group("dev")]
run: seed-examples
    WARDEN_CONFIG="{{justfile_directory()}}/examples/config.toml" cargo run -p warden-app

# Validate a config and print the resolved window/tab tree + warnings (defaults to the demo).
[group("dev")]
validate path="examples/config.toml":
    cargo run -p warden-config --bin warden -- validate "{{path}}"

# Run the workspace tests
[group("check")]
test:
    cargo test --workspace

# Type-check the workspace without producing binaries
[group("check")]
check:
    cargo check --workspace

# Format all sources
[group("check")]
fmt:
    cargo fmt --all

# Lint with clippy (warnings as errors)
[group("check")]
clippy:
    cargo clippy --workspace -- -D warnings

# Full pre-merge gate: format check (non-mutating), clippy, tests. Run before
# committing/merging — nothing runs this automatically (no hook, no CI yet).
[group("check")]
gate:
    @grep -qE '^\[patch\.' Cargo.toml && { echo "✗ active [patch] in Cargo.toml — run 'just chrome-pin' before committing"; exit 1; } || true
    cargo fmt --all --check
    cargo clippy --workspace -- -D warnings
    cargo test --workspace
    cargo run -p warden-config --bin warden -- fmt --check examples/config.toml

# ── shared chrome-core dev loop ─────────────────────────────────────────────────────────────────
# Build warden against a LOCAL ../chrome-core checkout (incl. uncommitted edits) instead of the
# pinned git rev — the fast loop for chrome work. Activates the (normally-commented) [patch] in
# Cargo.toml, then `just run`. Run `just chrome-pin` before committing to re-pin + re-comment.
[group("chrome")]
chrome-dev:
    #!/usr/bin/env bash
    set -euo pipefail
    cd "{{justfile_directory()}}"
    [ -d ../chrome-core ] || { echo "✗ ../chrome-core not found — ghq get github.com/Lockyc/chrome-core"; exit 1; }
    tmp=$(mktemp); sed 's/^#PATCH#//' Cargo.toml > "$tmp" && mv "$tmp" Cargo.toml
    echo "✓ chrome-core → local ../chrome-core (patch active). Iterate, then: just run"
    echo "  ⚠ NEVER commit an active patch — run 'just chrome-pin' first ('just gate' will block it)."

# Re-pin chrome-core to ../chrome-core's pushed HEAD and deactivate the local patch. Run after you've
# committed+pushed chrome-core so warden tracks the new rev. Refuses if chrome-core is dirty/unpushed.
[group("chrome")]
chrome-pin:
    #!/usr/bin/env bash
    set -euo pipefail
    cd "{{justfile_directory()}}"
    cc=../chrome-core
    [ -d "$cc" ] || { echo "✗ $cc not found"; exit 1; }
    [ -z "$(git -C "$cc" status --porcelain)" ] || { echo "✗ chrome-core has uncommitted changes — commit + push it first"; exit 1; }
    git -C "$cc" fetch -q origin
    rev=$(git -C "$cc" rev-parse HEAD)
    git -C "$cc" branch -r --contains "$rev" | grep -q origin/ || { echo "✗ chrome-core HEAD ($rev) isn't pushed — push it first"; exit 1; }
    dep=crates/warden-app/Cargo.toml
    tmp=$(mktemp); sed -E 's|(chrome-core = \{ git = "https://github.com/Lockyc/chrome-core", rev = ")[0-9a-f]+|\1'"$rev"'|' "$dep" > "$tmp" && mv "$tmp" "$dep"
    tmp=$(mktemp); sed -E 's|^\[patch\."https://github.com/Lockyc/chrome-core"\]$|#PATCH#&|; s|^chrome-core = \{ path = "\.\./chrome-core" \}$|#PATCH#&|' Cargo.toml > "$tmp" && mv "$tmp" Cargo.toml
    cargo update -p chrome-core
    echo "✓ pinned chrome-core → $rev (patch deactivated). Commit Cargo.toml + Cargo.lock."

# Open chrome-core's visual preview loop (requires ../chrome-core checked out)
[group("chrome")]
chrome-preview:
    @[ -f ../chrome-core/justfile ] && just -f ../chrome-core/justfile preview || echo "✗ ../chrome-core not found — ghq get github.com/Lockyc/chrome-core"

# Build the release .app bundle (needs the Tauri CLI: `cargo install tauri-cli --version ^2`)
[group("dist")]
build:
    cd crates/warden-app && cargo tauri build

# Build a NOTARIZED warden.app and attach it to its GitHub release (version from Cargo.toml).
# Run AFTER the release is tagged/pushed and `gh release create v<version>` published the notes
# (see CLAUDE.md › Releases). Refuses to run without the Apple signing/notary env vars.
[group("dist")]
release:
    bash scripts/release.sh

# Build a release .app, install/replace it in /Applications, then relaunch.
# Delegates build+install to install.sh (seeds ~/.config/warden/config.toml only if absent);
# the relaunch stays here because install.sh never launches the app.
[group("dist")]
deploy:
    #!/usr/bin/env bash
    set -euo pipefail
    bash install.sh
    echo "→ launching"
    open "/Applications/warden.app"
    echo "✓ warden updated in /Applications"
