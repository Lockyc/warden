use std::path::PathBuf;
use warden_config::{config_path, format_file, format_str, load};

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
        Some("fmt") => {
            let mut check = false;
            let mut path: Option<PathBuf> = None;
            for a in &args[2..] {
                match a.as_str() {
                    "--check" => check = true,
                    p => path = Some(PathBuf::from(p)),
                }
            }
            let path = path.unwrap_or_else(config_path);
            let original = match std::fs::read_to_string(&path) {
                Ok(s) => s,
                Err(e) => {
                    eprintln!("error: {e}");
                    std::process::exit(1);
                }
            };
            // Refuse to "format" a non-TOML file: taplo error-recovers and would
            // return it unchanged, falsely reporting success.
            if let Err(e) = warden_config::raw::parse(&original) {
                eprintln!("error: {} is not valid TOML: {e}", path.display());
                std::process::exit(1);
            }
            if check {
                if format_str(&original) != original {
                    eprintln!("would reformat: {}", path.display());
                    std::process::exit(1);
                }
                println!("ok: {} already formatted", path.display());
            } else {
                match format_file(&path) {
                    Ok(true) => println!("formatted: {}", path.display()),
                    Ok(false) => println!("ok: {} already formatted", path.display()),
                    Err(e) => {
                        eprintln!("error: {e}");
                        std::process::exit(1);
                    }
                }
            }
        }
        _ => {
            eprintln!("usage: warden <validate|fmt> [--check] [path]");
            std::process::exit(2);
        }
    }
}
