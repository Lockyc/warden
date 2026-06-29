//! warden's house TOML formatter — wraps the taplo crate with a fixed style so
//! the config file stays tidy. `format_str` is pure; `format_file` applies it to
//! a file atomically and only when the bytes change (so a watcher driving it
//! can't loop).

use std::io::Write;
use std::path::Path;
use taplo::formatter::{format, Options};

/// Format TOML in warden's house style — reproduces and enforces the
/// hand-formatted look of `examples/config.toml`: nested-table indentation,
/// aligned `=` and trailing comments, authored key order preserved.
pub fn format_str(input: &str) -> String {
    let o = Options {
        indent_tables: true,
        indent_entries: true,
        align_entries: true,
        align_comments: true,
        reorder_keys: false,
        column_width: 100,
        ..Options::default()
    };
    format(input, o)
}

/// Format `path` in place. Reads, formats, and rewrites **only if the bytes
/// change** (so a watcher driving this can't loop — an already-formatted file is
/// a no-op). The rewrite is atomic (temp file + rename) and **identity-preserving**:
/// it resolves symlinks so a linked config (e.g. one symlinked in from a dotfiles
/// repo) is rewritten in place rather than replaced by a regular file, and it
/// copies the original's permissions onto the replacement. Returns whether it wrote.
pub fn format_file(path: &Path) -> std::io::Result<bool> {
    let original = std::fs::read_to_string(path)?;
    let formatted = format_str(&original);
    if formatted == original {
        return Ok(false);
    }
    // Resolve symlinks → rewrite the real file, preserving the link.
    let target = std::fs::canonicalize(path)?;
    let dir = target.parent().unwrap_or_else(|| Path::new("."));
    let mut tmp = tempfile::NamedTempFile::new_in(dir)?;
    tmp.write_all(formatted.as_bytes())?;
    tmp.flush()?;
    // Carry the original's mode onto the temp, else persist would leave the
    // tempfile's owner-only 0600 default.
    if let Ok(meta) = std::fs::metadata(&target) {
        let _ = tmp.as_file().set_permissions(meta.permissions());
    }
    tmp.persist(&target).map_err(|e| e.error)?;
    Ok(true)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn indents_nested_tables_and_aligns() {
        let out = format_str("a=1\n[[w]]\nx=2\n");
        assert_eq!(out, "a = 1\n[[w]]\n  x = 2\n");
    }

    #[test]
    fn is_idempotent() {
        let messy = "a=1\n[[w]]\nx=2\ntitle=\"t\"\n";
        let once = format_str(messy);
        assert_eq!(format_str(&once), once);
    }

    #[test]
    fn golden_house_style_on_windows_groups_comments() {
        // A representative config (window + tab + group + comment + hex colour)
        // pinned to its exact house-style output — guards real-config formatting
        // against a taplo bump or Options change, which the minimal cases above
        // would not catch.
        let input = r##"# header
shell="fish"
[[window]]
title="w"
colour="#0f8a8a"
[[window.tab]]
dir="/tmp"
cmd="" # opt out
[[window.group]]
name="g"
[[window.group.tab]]
dir="/etc"
"##;
        let expected = r##"# header
shell = "fish"
[[window]]
  title  = "w"
  colour = "#0f8a8a"
  [[window.tab]]
    dir = "/tmp"
    cmd = ""     # opt out
  [[window.group]]
    name = "g"
    [[window.group.tab]]
      dir = "/etc"
"##;
        assert_eq!(format_str(input), expected);
    }

    #[test]
    fn format_file_rewrites_messy_then_is_a_noop() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("config.toml");
        std::fs::write(&path, "a=1\n[[w]]\nx=2\n").unwrap();

        // First pass: messy → rewritten.
        assert!(format_file(&path).unwrap());
        assert_eq!(
            std::fs::read_to_string(&path).unwrap(),
            "a = 1\n[[w]]\n  x = 2\n"
        );

        // Second pass: already formatted → no write, returns false.
        assert!(!format_file(&path).unwrap());
    }

    #[cfg(unix)]
    #[test]
    fn format_file_preserves_mode_and_symlink() {
        use std::os::unix::fs::PermissionsExt;
        let dir = tempfile::tempdir().unwrap();
        let real = dir.path().join("real.toml");
        std::fs::write(&real, "x=1\n[[w]]\ny=2\n").unwrap();
        std::fs::set_permissions(&real, std::fs::Permissions::from_mode(0o644)).unwrap();
        let link = dir.path().join("link.toml");
        std::os::unix::fs::symlink(&real, &link).unwrap();

        assert!(format_file(&link).unwrap());

        // The link is still a symlink (not clobbered into a regular file)...
        assert!(std::fs::symlink_metadata(&link)
            .unwrap()
            .file_type()
            .is_symlink());
        // ...the real file was formatted in place...
        assert_eq!(
            std::fs::read_to_string(&real).unwrap(),
            "x = 1\n[[w]]\n  y = 2\n"
        );
        // ...and its mode survived (not reset to the tempfile's 0600).
        assert_eq!(
            std::fs::metadata(&real).unwrap().permissions().mode() & 0o777,
            0o644
        );
    }
}
