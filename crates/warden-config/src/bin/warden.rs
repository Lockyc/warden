use std::path::PathBuf;
use warden_config::{config_path, fmt_cli, load_with, login_shell, Root, Split, SplitSide, Tab};

/// A compact `split=…` rendering for `validate`'s tree, in place of raw `{:?}` Debug
/// (`Some(Split { side: Left, size: 0.3, panes: [..] })`): `split=none` when unset, else
/// `split=left/0.3` and, with a declared `cmd`, `split=left/0.3/"amux"`.
fn fmt_split(split: &Option<Split>) -> String {
    match split {
        None => "split=none".to_string(),
        Some(s) => {
            let side = match s.side {
                SplitSide::Left => "left",
                SplitSide::Right => "right",
            };
            if s.panes.len() > 1 {
                let panes: Vec<String> = s
                    .panes
                    .iter()
                    .map(|p| match &p.startup {
                        Some(cmd) => format!("{:.2}:{cmd:?}", p.size),
                        None => format!("{:.2}", p.size),
                    })
                    .collect();
                return format!("split={side}/{}/[{}]", s.size, panes.join(","));
            }
            match s.scratch_startup() {
                Some(cmd) => format!("split={side}/{}/{cmd:?}", s.size),
                None => format!("split={side}/{}", s.size),
            }
        }
    }
}

/// `validate`'s line for one tab; `id=` and `colour=` appear only when the config sets them.
fn tab_line(t: &Tab) -> String {
    let id =
        t.id.as_deref()
            .map(|id| format!(" id={id:?}"))
            .unwrap_or_default();
    let colour = t
        .colour
        .map(|c| format!(" colour={}", c.hex()))
        .unwrap_or_default();
    let group = t
        .group
        .as_deref()
        .map(|g| format!(" group={g:?}"))
        .unwrap_or_default();
    format!(
        "    tab {:?}{id} dir={} shell={:?} startup={:?} probe={:?} kill={:?} suspend={:?} resume={:?} server={:?} load_on_open={} {}{group}{colour}",
        t.title,
        t.dir.display(),
        t.shell,
        t.startup,
        t.probe,
        t.kill,
        t.suspend,
        t.resume,
        t.server,
        t.load_on_open,
        fmt_split(&t.split),
    )
}

/// `validate`'s lines for one root: the root and its resolved cascade, then one line per
/// `[[window.root.tab]]` override. The CLI does no scanning, so this stands in for the projects
/// the app discovers — without it a roots-only window looks empty.
fn root_lines(r: &Root) -> Vec<String> {
    let mut lines = vec![format!(
        "    root {:?} dir={} depth={} shell={:?} startup={:?} probe={:?} kill={:?} suspend={:?} resume={:?} server={:?} {}",
        r.name,
        r.dir.display(),
        r.depth,
        r.shell,
        r.startup,
        r.probe,
        r.kill,
        r.suspend,
        r.resume,
        r.server,
        fmt_split(&r.split)
    )];
    lines.extend(r.overrides.iter().map(|o| {
        format!(
            "      override dir={} shell={:?} startup={:?} probe={:?} kill={:?} suspend={:?} resume={:?} {}",
            o.dir.display(),
            o.shell,
            o.startup,
            o.probe,
            o.kill,
            o.suspend,
            o.resume,
            fmt_split(&o.split)
        )
    }));
    lines
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
                            println!("{}", tab_line(t));
                        }
                        for r in &p.roots {
                            for line in root_lines(r) {
                                println!("{line}");
                            }
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

#[cfg(test)]
mod tests {
    use super::*;
    use warden_config::{raw::parse, resolve::resolve};

    const CFG: &str = r##"
[[window]]
title = "w"
  [[window.tab]]
  id = "core"
  dir = "/tmp/alpha"
  colour = "#112233"
  [[window.tab]]
  dir = "/tmp/beta"
  [[window.root]]
  name = "projects"
  dir = "/tmp/projects"
    [[window.root.tab]]
    dir = "/tmp/projects/one"
    cmd = "go"
"##;

    fn window() -> warden_config::Window {
        resolve(parse(CFG).unwrap()).unwrap().0.windows.remove(0)
    }

    #[test]
    fn a_tab_line_carries_its_id_and_colour_only_when_set() {
        let w = window();
        let set = tab_line(&w.tabs[0]);
        assert!(set.contains(" id=\"core\""), "{set}");
        assert!(set.contains(" colour=#112233"), "{set}");
        let unset = tab_line(&w.tabs[1]);
        assert!(
            !unset.contains(" id=") && !unset.contains(" colour="),
            "{unset}"
        );
    }

    #[test]
    fn a_root_prints_each_override_under_it() {
        let w = window();
        let lines = root_lines(&w.roots[0]);
        assert_eq!(lines.len(), 2, "{lines:?}");
        assert!(
            lines[0].starts_with("    root \"projects\""),
            "{}",
            lines[0]
        );
        assert!(
            lines[1].starts_with("      override dir=/tmp/projects/one")
                && lines[1].contains("startup=Some(\"go\")"),
            "{}",
            lines[1]
        );
    }
}
