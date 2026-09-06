use std::path::PathBuf;
use warden_config::{config_path, fmt_cli, load_with};

/// The shell warden defaults an unset tab to — the user's login shell, run as a login shell,
/// like a terminal. `$SHELL` (falling back to the macOS default), with `-l`. Detected here in
/// the binary so the pure crate stays env-free, matching what warden-app injects at runtime.
fn login_shell() -> String {
    let path = std::env::var("SHELL")
        .ok()
        .filter(|s| !s.trim().is_empty())
        .unwrap_or_else(|| "/bin/zsh".to_string());
    format!("{path} -l")
}

fn main() {
    let args: Vec<String> = std::env::args().collect();
    match args.get(1).map(String::as_str) {
        Some("validate") => {
            let path = args.get(2).map(PathBuf::from).unwrap_or_else(config_path);
            match load_with(&path, &login_shell()) {
                Ok(loaded) => {
                    println!(
                        "ok: {} ({} window(s))",
                        path.display(),
                        loaded.config.windows.len()
                    );
                    for p in &loaded.config.windows {
                        println!("  window {:?} {}", p.title, p.colour.hex());
                        for t in &p.tabs {
                            let group = t
                                .group
                                .as_deref()
                                .map(|g| format!(" group={g:?}"))
                                .unwrap_or_default();
                            println!(
                                "    tab {:?} dir={} shell={:?} startup={:?} load_on_open={} split={:?}{}",
                                t.title,
                                t.dir.display(),
                                t.shell,
                                t.startup,
                                t.load_on_open,
                                t.split,
                                group
                            );
                        }
                        // Roots are declarations the app expands at runtime (the CLI does
                        // no scanning), so print the root + its resolved cascade rather
                        // than discovered projects — otherwise a roots-only window looks
                        // deceptively empty.
                        for r in &p.roots {
                            println!(
                                "    root {:?} dir={} depth={} shell={:?} startup={:?} probe={:?} kill={:?} split={:?}",
                                r.name,
                                r.dir.display(),
                                r.depth,
                                r.shell,
                                r.startup,
                                r.probe,
                                r.kill,
                                r.split
                            );
                        }
                    }
                    for w in &loaded.warnings {
                        eprintln!("warning [{}]: {}", w.window, w.message);
                    }
                }
                Err(e) => {
                    eprintln!("error: {e}");
                    std::process::exit(1);
                }
            }
        }
        Some("fmt") => {
            let mut check = false;
            let mut path: Option<PathBuf> = None;
            for a in &args[2..] {
                match a.as_str() {
                    "--check" => check = true,
                    p => path = Some(PathBuf::from(p)),
                }
            }
            // `fmt` is schema-free (tidy any well-formed TOML), so it's the shared config-core
            // implementation both apps delegate to — only the default config path is warden's.
            let path = path.unwrap_or_else(config_path);
            std::process::exit(fmt_cli(check, &path));
        }
        _ => {
            eprintln!("usage: warden <validate|fmt> [--check] [path]");
            std::process::exit(2);
        }
    }
}
