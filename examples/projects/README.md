# Mock project tree

Empty placeholder directories so the demo config (`../config.toml`) resolves
**warning-free** with zero setup. `examples/config.toml` points its tabs at these
repo-relative paths, and `just run` / `just validate` run from the repo root, so
the relative `dir`s resolve here.

Tracked as empty (`.gitkeep`). They mock a "personal + work" project layout; a
real `~/.config/warden/config.toml` points at absolute or `~/`-prefixed paths to
your actual project directories instead.

The demo's **"projects" window** points a `[[window.root]]` at this directory to
show off the auto-discovered project tree. That scanner only finds **git repos**,
so `just seed-examples` (run automatically by `just run`) `git init`s each of these
mock dirs into a real repo — idempotent, and the created `.git` dirs are git-ignored,
so they never show up in warden's own `git status`.
