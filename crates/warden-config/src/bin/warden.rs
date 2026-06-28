use std::path::PathBuf;
use warden_config::{config_path, load};

fn main() {
    let args: Vec<String> = std::env::args().collect();
    match args.get(1).map(String::as_str) {
        Some("validate") => {
            let path = args.get(2).map(PathBuf::from).unwrap_or_else(config_path);
            match load(&path) {
                Ok(loaded) => {
                    println!("ok: {} ({} profile(s))", path.display(), loaded.config.profiles.len());
                    for p in &loaded.config.profiles {
                        println!("  profile {:?} {}", p.name, p.colour.hex());
                        for t in &p.tabs {
                            println!(
                                "    tab {:?} dir={} shell={:?} startup={:?} keep_alive={}",
                                t.title, t.dir.display(), t.shell, t.startup, t.keep_alive
                            );
                        }
                    }
                    for w in &loaded.warnings {
                        eprintln!("warning [{}]: {}", w.profile, w.message);
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
