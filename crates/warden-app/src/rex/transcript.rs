//! Where an agent's conversation transcript lives on disk.
//!
//! The path is derived from a stored resume id, never reported, so a forged report can't point
//! warden at an arbitrary file. Claude Code keeps a transcript at
//! `<profile>/projects/<encoded dir>/<id>.jsonl`, where a profile is a `~/.claude*` directory.

use std::path::{Path, PathBuf};

/// The `~/.claude*` directories directly under `$HOME`, canonical. Several can exist (one per
/// account profile), and the transcript of a given id lives in whichever ran it.
#[allow(dead_code, reason = "consumed by the summary task")]
pub fn claude_profiles() -> Vec<PathBuf> {
    let Some(home) = std::env::var_os("HOME").map(PathBuf::from) else {
        return Vec::new();
    };
    profiles_in(&home)
}

#[allow(dead_code, reason = "consumed by the summary task")]
fn profiles_in(home: &Path) -> Vec<PathBuf> {
    let Ok(entries) = std::fs::read_dir(home) else {
        return Vec::new();
    };
    let mut out: Vec<PathBuf> = entries
        .flatten()
        .filter(|e| e.file_name().to_string_lossy().starts_with(".claude"))
        .filter_map(|e| e.path().canonicalize().ok())
        .filter(|p| p.is_dir())
        .collect();
    out.sort();
    out.dedup();
    out
}

/// The first `<profile>/projects/*/<id>.jsonl` that exists, canonical. A hit whose real path
/// leaves the profile it was found in (a symlink out) is skipped, so the id can only ever name a
/// file inside a profile. `id` is trusted to be a `resume_id` (alphanumerics and `-`).
#[allow(dead_code, reason = "consumed by the summary task")]
pub fn locate(id: &str, profiles: &[PathBuf]) -> Option<PathBuf> {
    let file = format!("{id}.jsonl");
    for profile in profiles {
        let Ok(root) = profile.canonicalize() else {
            continue;
        };
        let Ok(projects) = std::fs::read_dir(root.join("projects")) else {
            continue;
        };
        let mut dirs: Vec<PathBuf> = projects.flatten().map(|e| e.path()).collect();
        dirs.sort();
        for dir in dirs {
            let Ok(real) = dir.join(&file).canonicalize() else {
                continue;
            };
            if real.starts_with(&root) && real.is_file() {
                return Some(real);
            }
        }
    }
    None
}

#[cfg(test)]
mod tests {
    use super::*;

    fn put(profile: &Path, dir: &str, id: &str) -> PathBuf {
        let d = profile.join("projects").join(dir);
        std::fs::create_dir_all(&d).unwrap();
        let f = d.join(format!("{id}.jsonl"));
        std::fs::write(&f, "{}\n").unwrap();
        f
    }

    #[test]
    fn locate_finds_id_across_profiles() {
        let t = tempfile::tempdir().unwrap();
        let a = t.path().join(".claude");
        let b = t.path().join(".claude-work");
        std::fs::create_dir_all(&a).unwrap();
        let want = put(&b, "-Users-x-proj", "abc-1");
        std::fs::create_dir(t.path().join("other")).unwrap();
        let profiles = profiles_in(t.path());
        assert_eq!(profiles.len(), 2);
        assert_eq!(
            locate("abc-1", &profiles),
            Some(want.canonicalize().unwrap())
        );
    }

    #[test]
    fn locate_ignores_other_ids_and_symlinks_out_of_profile() {
        let t = tempfile::tempdir().unwrap();
        let p = t.path().join(".claude");
        put(&p, "d1", "other");
        assert_eq!(locate("abc", std::slice::from_ref(&p)), None);

        let outside = t.path().join("outside.jsonl");
        std::fs::write(&outside, "secret").unwrap();
        let d = p.join("projects").join("d2");
        std::fs::create_dir_all(&d).unwrap();
        std::os::unix::fs::symlink(&outside, d.join("abc.jsonl")).unwrap();
        assert_eq!(locate("abc", std::slice::from_ref(&p)), None);
    }
}
