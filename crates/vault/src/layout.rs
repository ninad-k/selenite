//! Vault file layout rules: daily notes, page file names, welcome page, Obsidian markers.

use std::fmt::Write as _;

use chrono::NaiveDate;
use selenite_core::{PathError, VaultPath};

use crate::config::VaultConfig;

/// File name of the note created in a fresh vault.
pub const WELCOME_PAGE: &str = "Welcome.md";

fn join_folder(folder: Option<&str>, file: &str) -> String {
    match folder.map(|f| f.trim_matches(['/', '\\'])) {
        Some(f) if !f.is_empty() => format!("{f}/{file}"),
        _ => file.to_string(),
    }
}

/// Path of the daily note for `date`, per the vault's folder and format settings.
///
/// An invalid `daily_format` yields [`PathError::BadComponent`].
pub fn daily_note_path(cfg: &VaultConfig, date: NaiveDate) -> Result<VaultPath, PathError> {
    let mut name = String::new();
    write!(name, "{}", date.format(&cfg.daily_format)).map_err(|_| PathError::BadComponent)?;
    name.push_str(".md");
    VaultPath::new(&join_folder(Some(&cfg.daily_folder), &name))
}

/// Path for a new page called `title`, optionally inside `folder`.
///
/// Characters that are not portable in file names (`<>:"/\|?*` and control characters) become
/// `-`; leading/trailing dots and spaces are trimmed; `.md` is appended.
pub fn page_path_for_title(title: &str, folder: Option<&str>) -> Result<VaultPath, PathError> {
    let cleaned: String = title
        .chars()
        .map(|c| {
            if c.is_control() || matches!(c, '<' | '>' | ':' | '"' | '/' | '\\' | '|' | '?' | '*') {
                '-'
            } else {
                c
            }
        })
        .collect();
    let trimmed = cleaned.trim_matches(|c| c == '.' || c == ' ');
    if trimmed.is_empty() {
        return Err(PathError::Empty);
    }
    VaultPath::new(&join_folder(folder, &format!("{trimmed}.md")))
}

/// True if `path` is a visible Markdown page (not inside a hidden folder).
pub fn is_page(path: &VaultPath) -> bool {
    path.is_markdown() && !path.is_hidden()
}

/// Text of the welcome note written into a new vault.
pub fn welcome_text(app_name: &str) -> String {
    format!(
        "---\ntags: [welcome]\n---\n# Welcome to {app_name}\n\n\
Your notes are plain Markdown files in this folder, so they stay yours.\n\n\
- Link pages with double square brackets, for example `[[Another page]]`.\n\
- Open today's daily note to jot things down; daily notes live in the Daily folder.\n\
- Add tags anywhere in a note, like `#idea`, and nest them like `#project/ideas`.\n\n\
Delete or rewrite this note whenever you like.\n"
    )
}

/// Files that make the folder open as an Obsidian vault, as `(path, contents)` pairs.
pub fn obsidian_marker_files() -> Vec<(&'static str, &'static str)> {
    vec![(".obsidian/app.json", "{}\n")]
}

#[cfg(test)]
mod tests {
    use super::*;

    fn date() -> NaiveDate {
        NaiveDate::from_ymd_opt(2026, 11, 16).unwrap()
    }

    #[test]
    fn daily_default() {
        let p = daily_note_path(&VaultConfig::default(), date()).unwrap();
        assert_eq!(p.as_str(), "Daily/2026-11-16.md");
    }

    #[test]
    fn daily_custom_and_root() {
        let cfg = VaultConfig {
            daily_folder: String::new(),
            daily_format: "%Y/%m/%d".into(),
            ..VaultConfig::default()
        };
        assert_eq!(
            daily_note_path(&cfg, date()).unwrap().as_str(),
            "2026/11/16.md"
        );
        let cfg = VaultConfig {
            daily_folder: "/Journal/".into(),
            ..VaultConfig::default()
        };
        assert_eq!(
            daily_note_path(&cfg, date()).unwrap().as_str(),
            "Journal/2026-11-16.md"
        );
    }

    #[test]
    fn daily_bad_format() {
        let cfg = VaultConfig {
            daily_format: "%Q%".into(),
            ..VaultConfig::default()
        };
        assert!(daily_note_path(&cfg, date()).is_err());
    }

    #[test]
    fn title_sanitising() {
        let p = page_path_for_title("  What? A <great>: day/night | x  ", None).unwrap();
        assert_eq!(p.as_str(), "What- A -great-- day-night - x.md");
        let p = page_path_for_title("..Hidden..", Some("Notes/")).unwrap();
        assert_eq!(p.as_str(), "Notes/Hidden.md");
        assert_eq!(page_path_for_title(" . ", None), Err(PathError::Empty));
        assert_eq!(
            page_path_for_title("a\tb", None).unwrap().as_str(),
            "a-b.md"
        );
    }

    #[test]
    fn page_detection() {
        assert!(is_page(&VaultPath::new("a/b.md").unwrap()));
        assert!(!is_page(&VaultPath::new(".selenite/x.md").unwrap()));
        assert!(!is_page(&VaultPath::new("a/b.png").unwrap()));
    }

    #[test]
    fn welcome_and_markers() {
        let w = welcome_text("Selenite");
        assert!(w.contains("Selenite") && w.contains("[[Another page]]") && w.contains("`#idea`"));
        assert_eq!(WELCOME_PAGE, "Welcome.md");
        assert_eq!(obsidian_marker_files()[0].0, ".obsidian/app.json");
    }
}
