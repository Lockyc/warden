#!/usr/bin/env bash
# launch-app.sh — launch the installed ${APP_NAME}.app the way the Dock/Spotlight would, with a
# clean GUI environment. The counterpart to install-app.sh, which installs but deliberately never
# launches ("the caller decides"); this is what the caller should decide to use.
#
# GENERIC — shared verbatim across every consuming app via shell-core; app-specific values come
# from the tracked per-app scripts/tooling.env. Materialized git-ignored by the app's build.rs from
# the pinned shell-core rev — edit it in shell-core, never in the consuming app.
#
# FOOTGUN — why this exists rather than a bare `open`:
#   `open` looks like a clean LaunchServices launch, and the process *parentage* is genuinely clean
#   (the app comes up under launchd, PPID 1, not as a child of the shell). But `open` FORWARDS THE
#   CALLER'S FULL ENVIRONMENT to the launched app. Deploying from a terminal therefore hands the app
#   that terminal's context — TERM/TERM_PROGRAM/TERMINFO, GHOSTTY_*, TMUX*, ATUIN_*/STARSHIP_*
#   session keys, any agent/tooling vars, and SHELL — none of which are present when a user launches
#   the app normally. So `just deploy` produces an app running in an environment that no real launch
#   ever reproduces, and bugs appear (or vanish) purely by launch method.
#
#   It bites hardest in a terminal host: warden gives every libghostty surface's shell its own
#   environment verbatim, so a leaked SHELL means tabs open the *deploying* shell instead of the
#   login shell, and leaked TMUX/GHOSTTY vars make nested-session detection misfire. An app can
#   scrub specific vars it knows about (warden's main.rs scrubs TMUX/TMUX_PANE and
#   GHOSTTY_RESOURCES_DIR), but it cannot chase an open-ended set — the fix belongs at the launch.
#
#   `env -i` clears the caller side, so the app inherits only the launchd GUI-session environment:
#   verified byte-for-byte identical to a Spotlight launch (same key set, zero leaked vars).
#   Absolute paths below are required — there is no PATH left to resolve them with.
#
# Verify at any time by comparing a deploy-launched process against a Spotlight-launched one:
#   ps eww -p "$(pgrep -f "/Applications/${APP_NAME}.app/Contents/MacOS/" | head -1)"
set -euo pipefail
source "$(dirname "$0")/tooling.env"

: "${APP_NAME:?tooling.env must set APP_NAME}"

dest="${APP_DEST:-/Applications/${APP_NAME}.app}"

[ -d "$dest" ] || { echo "launch-app.sh: no app bundle at $dest — install it first" >&2; exit 1; }

/usr/bin/env -i /usr/bin/open "$dest"
echo "launch-app.sh: launched $dest with a clean GUI environment"
