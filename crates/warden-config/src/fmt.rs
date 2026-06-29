//! warden's house TOML formatter — wraps the taplo crate with a fixed style so
//! the config file stays tidy. `format_str` is pure; `format_file` applies it to
//! a file atomically and only when the bytes change (so a watcher driving it
//! can't loop).

use std::io::Write;
use std::path::Path;
use taplo::formatter::{format, Options};

/// Format TOML in warden's house style — reproduces and enforces the
/// hand-formatted look of `examples/config.toml`: nested-table indentation,
/// aligned `=` and trailing comments, authored key order preserved, and exactly
/// one blank line before every section (taplo handles all but the blank-line
/// normalization, which `separate_sections` adds on top — see its doc).
pub fn format_str(input: &str) -> String {
    let o = Options {
        indent_tables: true,
        indent_entries: true,
        align_entries: true,
        align_comments: true,
        reorder_keys: false,
        column_width: 100,
        // Cap runs of blank lines at one (taplo can collapse, not insert).
        allowed_blank_lines: 1,
        // Pin line-ending policy so the house style is self-documenting and a
        // CRLF paste can't sneak in.
        trailing_newline: true,
        crlf: false,
        ..Options::default()
    };
    separate_sections(&format(input, o))
}

/// Normalize vertical spacing to **exactly one blank line before every section
/// header** (a `[[…]]` / `[…]` line). taplo only *caps* blank lines (set to 1
/// above); this inserts the missing ones so sections are uniformly separated.
///
/// A comment block glued to a header (no blank between them) is treated as part
/// of that section — the blank goes *above* the comment, never between the
/// comment and its header. No blank is added at the very start of the file.
/// Idempotent: a file already spaced this way is returned unchanged.
fn separate_sections(formatted: &str) -> String {
    let lines: Vec<&str> = formatted.lines().collect();
    let is_header = |l: &str| l.trim_start().starts_with('[');
    let is_comment = |l: &str| l.trim_start().starts_with('#');
    let mut out: Vec<&str> = Vec::with_capacity(lines.len() + 16);
    for (i, &line) in lines.iter().enumerate() {
        // A section begins at a header that isn't glued to a comment above it,
        // or at the top of a comment block that leads (without a blank) into a
        // header — the latter carries the blank for the whole comment+header unit.
        let header_starts = is_header(line) && !(i > 0 && is_comment(lines[i - 1]));
        let comment_starts = is_comment(line) && !(i > 0 && is_comment(lines[i - 1])) && {
            let mut k = i;
            while k < lines.len() && is_comment(lines[k]) {
                k += 1;
            }
            k < lines.len() && is_header(lines[k])
        };
        if (header_starts || comment_starts)
            && !out.is_empty()
            && !out.last().is_some_and(|l| l.trim().is_empty())
        {
            out.push("");
        }
        out.push(line);
    }
    format!("{}\n", out.join("\n").trim_end())
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
        // A blank line is inserted before the [[w]] section.
        assert_eq!(out, "a = 1\n\n[[w]]\n  x = 2\n");
    }

    #[test]
    fn separates_sections_with_one_blank() {
        // No leading blank; a blank inserted before each header; a comment glued
        // to a header keeps the blank *above* the comment (not between).
        let input = "[[a]]\nx=1\n# note for b\n[[b]]\ny=2\n";
        let expected = "[[a]]\n  x = 1\n\n# note for b\n[[b]]\n  y = 2\n";
        assert_eq!(format_str(input), expected);
    }

    #[test]
    fn collapses_extra_blank_lines() {
        // Runs of blank lines are capped at one (taplo allowed_blank_lines=1).
        let input = "[[a]]\nx=1\n\n\n\n[[b]]\ny=2\n";
        assert_eq!(format_str(input), "[[a]]\n  x = 1\n\n[[b]]\n  y = 2\n");
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
name="w"
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
  name   = "w"
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
            "a = 1\n\n[[w]]\n  x = 2\n"
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
            "x = 1\n\n[[w]]\n  y = 2\n"
        );
        // ...and its mode survived (not reset to the tempfile's 0600).
        assert_eq!(
            std::fs::metadata(&real).unwrap().permissions().mode() & 0o777,
            0o644
        );
    }
}
