# warden — task runner

# Recipes run in `sh`, which doesn't inherit cargo from an interactive fish/zsh setup.
# Guarantee rustup's bin dir is on PATH so `cargo` is found.
export PATH := env_var('HOME') + "/.cargo/bin:" + env_var('PATH')

# List available recipes
default:
    @just --list

# Run the app against the repo's demo config (never touches your real ~/.config/warden config)
[group("dev")]
run:
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
# committing/merging. GitHub Actions CI (.github/workflows/ci.yml) runs the same
# fmt/clippy/test on push + PR (config crate on ubuntu, whole workspace incl.
# warden-app on macOS); this recipe is the local mirror, plus the `warden fmt --check`.
[group("check")]
gate:
    cargo fmt --all --check
    cargo clippy --workspace -- -D warnings
    cargo test --workspace
    cargo run -p warden-config --bin warden -- fmt --check examples/config.toml

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
