use crate::colour::Colour;
use std::path::PathBuf;

#[derive(Debug, Clone, PartialEq)]
pub struct Config {
    pub windows: Vec<Window>,
    /// When true, warden rewrites the config file formatted on each clean
    /// hot-reload. Default false. Whole-file concern — no per-window cascade.
    pub format_on_save: bool,
}

#[derive(Debug, Clone, PartialEq)]
pub struct Window {
    pub name: String,
    pub colour: Colour,
    pub icon: Option<PathBuf>,
    pub tabs: Vec<Tab>,
}

#[derive(Debug, Clone, PartialEq)]
pub struct Tab {
    pub key: String,
    pub title: String,
    pub dir: PathBuf,
    /// The shell to spawn in this tab (= `default_cmd`, e.g. `"fish -l"`). It runs as an
    /// interactive shell under the terminal's PTY, so shell functions/aliases resolve.
    pub shell: String,
    /// Optional startup command auto-run inside `shell` (the tab's `cmd`). `None` = bare
    /// shell. It is *typed into* the interactive shell — not exec'd directly — so a shell
    /// function like `amux` works and the shell stays live after the command exits.
    pub startup: Option<String>,
    pub keep_alive: bool,
    /// The name of the `[[window.group]]` this tab belongs to, or `None` for a loose
    /// (ungrouped) tab. Purely presentational — the chrome sections the sidebar by it;
    /// it carries no behaviour and is not part of `key` (identity stays the title).
    pub group: Option<String>,
}

#[derive(Debug, Clone, PartialEq)]
pub struct Warning {
    pub window: String,
    pub message: String,
}
