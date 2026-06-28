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

# No `build`/`deploy` recipes yet: packaging is deferred. The Tauri bundle is inactive
# (`bundle.active = false`) and no tauri-cli is wired, so there is no .app/.dmg to install.
# Add them alongside enabling bundling — see docs/FOLLOWUPS.md (release packaging).
