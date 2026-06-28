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

# Validate a config and print the resolved profile/tab tree + warnings (defaults to the demo).
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

# Build the release .app bundle (needs the Tauri CLI: `cargo install tauri-cli --version ^2`)
[group("dist")]
build:
    cd crates/warden-app && cargo tauri build

# Build a release .app, install/replace it in /Applications (strips quarantine), then relaunch.
# Unsigned local build — no notarization; Gatekeeper is satisfied via the quarantine strip.
[group("dist")]
deploy: build
    #!/usr/bin/env bash
    set -euo pipefail
    bash scripts/install-app.sh "target/release/bundle/macos/warden.app"
    echo "→ launching"
    open "/Applications/warden.app"
    echo "✓ warden updated in /Applications"
