use std::path::PathBuf;
use warden_config::{config_path, load};

fn main() {
    let args: Vec<String> = std::env::args().collect();
    match args.get(1).map(String::as_str) {
        Some("validate") => {
            let path = args.get(2).map(PathBuf::from).unwrap_or_else(config_path);
            match load(&path) {
                Ok(loaded) => {
                    println!(
                        "ok: {} ({} window(s))",
                        path.display(),
                        loaded.config.windows.len()
                    );
                    for p in &loaded.config.windows {
                        println!("  window {:?} {}", p.name, p.colour.hex());
                        for t in &p.tabs {
                            let group = t
                                .group
                                .as_deref()
                                .map(|g| format!(" group={g:?}"))
                                .unwrap_or_default();
                            println!(
                                "    tab {:?} dir={} shell={:?} startup={:?} keep_alive={}{}",
                                t.title,
                                t.dir.display(),
                                t.shell,
                                t.startup,
                                t.keep_alive,
                                group
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
        _ => {
            eprintln!("usage: warden validate [path]");
            std::process::exit(2);
        }
    }
}
