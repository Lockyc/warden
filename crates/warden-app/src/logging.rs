//! warden's `log` backend: the macOS unified log, subsystem [`SUBSYSTEM`], one category per
//! module (`rex::spawn`, `manager`, …).
//!
//! A Dock/Finder/Spotlight launch points stdout/stderr at `/dev/null`, so the unified log is the
//! only place the installed app's diagnostics survive. Read it with
//! `log stream --predicate 'subsystem == "au.lsjc.warden"'` (add `--level debug` for debug lines),
//! or after the fact with `log show --last 1h --predicate …`.
//!
//! Level mapping onto os_log types, chosen for what the OS keeps: `info!` lands as *Default*, which
//! is persisted, so state-change lines are there to read back after something went wrong; `debug!`
//! and `trace!` land as *Info*/*Debug*, held in memory only and visible to a live `log stream`
//! with `--level debug` — cheap enough for hot paths. `warn!` and `error!` both land as *Error*.
//!
//! Other crates (tauri, tao, wry, the updater) log through the same facade; they pass only at
//! `warn!` and above so their chatter stays out of warden's subsystem.
//!
//! When stderr is a terminal (`just run`, a `cargo run`), each line is also echoed there.

use std::collections::HashMap;
use std::io::IsTerminal;
use std::sync::Mutex;

use log::{Level, LevelFilter, Log, Metadata, Record};
use oslog::OsLog;

/// The unified-log subsystem — the bundle identifier, so `log` predicates match the app's name.
pub const SUBSYSTEM: &str = "au.lsjc.warden";

/// Crates whose records pass at [`OWN_LEVEL`]; everything else needs [`FOREIGN_LEVEL`].
const OWN_TARGETS: &[&str] = &["warden_app"];
const OWN_LEVEL: LevelFilter = LevelFilter::Debug;
const FOREIGN_LEVEL: LevelFilter = LevelFilter::Warn;

struct Logger {
    /// One `os_log_t` per category, created on first use.
    logs: Mutex<HashMap<String, OsLog>>,
    echo: bool,
}

/// `warden_app::rex::spawn` → `rex::spawn`; a foreign target keeps its full name.
fn category(target: &str) -> &str {
    target
        .strip_prefix("warden_app::")
        .or_else(|| (target == "warden_app").then_some("app"))
        .unwrap_or(target)
}

fn is_own(target: &str) -> bool {
    OWN_TARGETS.iter().any(|p| {
        target
            .strip_prefix(p)
            .is_some_and(|rest| rest.is_empty() || rest.starts_with("::"))
    })
}

fn os_level(level: Level) -> oslog::Level {
    match level {
        Level::Error | Level::Warn => oslog::Level::Error,
        Level::Info => oslog::Level::Default,
        Level::Debug => oslog::Level::Info,
        Level::Trace => oslog::Level::Debug,
    }
}

impl Log for Logger {
    fn enabled(&self, metadata: &Metadata) -> bool {
        let max = if is_own(metadata.target()) {
            OWN_LEVEL
        } else {
            FOREIGN_LEVEL
        };
        metadata.level() <= max
    }

    fn log(&self, record: &Record) {
        if !self.enabled(record.metadata()) {
            return;
        }
        let category = category(record.target());
        let message = record.args().to_string();
        if let Ok(mut logs) = self.logs.lock() {
            logs.entry(category.to_owned())
                .or_insert_with(|| OsLog::new(SUBSYSTEM, category))
                .with_level(os_level(record.level()), &message);
        }
        if self.echo {
            eprintln!("warden {:<5} {category}: {message}", record.level());
        }
    }

    fn flush(&self) {}
}

/// Install the logger. Call first thing in `main`, before anything can log.
pub fn init() {
    let logger = Logger {
        logs: Mutex::new(HashMap::new()),
        echo: std::io::stderr().is_terminal(),
    };
    if log::set_boxed_logger(Box::new(logger)).is_ok() {
        log::set_max_level(OWN_LEVEL.max(FOREIGN_LEVEL));
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn category_strips_the_app_crate() {
        assert_eq!(category("warden_app::rex::spawn"), "rex::spawn");
        assert_eq!(category("warden_app"), "app");
        assert_eq!(category("tao::platform_impl"), "tao::platform_impl");
    }

    #[test]
    fn own_targets_match_whole_crate_names_only() {
        assert!(is_own("warden_app"));
        assert!(is_own("warden_app::manager"));
        assert!(!is_own("warden_appendix"));
        assert!(!is_own("tauri::app"));
    }

    #[test]
    fn info_is_persisted_debug_is_not() {
        assert!(matches!(os_level(Level::Info), oslog::Level::Default));
        assert!(matches!(os_level(Level::Debug), oslog::Level::Info));
        assert!(matches!(os_level(Level::Error), oslog::Level::Error));
    }
}
