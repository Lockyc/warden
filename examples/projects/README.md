# Mock project tree

Empty placeholder directories so the demo config (`../config.toml`) resolves
**warning-free** with zero setup. `examples/config.toml` points its tabs at these
repo-relative paths, and `just run` / `just validate` run from the repo root, so
the relative `dir`s resolve here.

Deliberately empty (`.gitkeep`). They mock a "personal + work" project layout; a
real `~/.config/warden/config.toml` points at absolute or `~/`-prefixed paths to
your actual project directories instead.
