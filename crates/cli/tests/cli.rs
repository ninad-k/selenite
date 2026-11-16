#![allow(missing_docs, clippy::unwrap_used, clippy::expect_used, clippy::panic)]

//! End-to-end tests that run the `selenite` binary against temporary vault folders.

use std::path::{Path, PathBuf};
use std::process::{Command, Output};

fn selenite() -> Command {
    Command::new(env!("CARGO_BIN_EXE_selenite"))
}

/// Run `selenite --vault <dir> <args...>`.
fn vault_cmd(dir: &Path, args: &[&str]) -> Output {
    selenite()
        .arg("--vault")
        .arg(dir)
        .args(args)
        .output()
        .unwrap()
}

fn stdout(out: &Output) -> String {
    String::from_utf8(out.stdout.clone()).unwrap()
}

fn stderr(out: &Output) -> String {
    String::from_utf8(out.stderr.clone()).unwrap()
}

/// A fresh vault in a temporary folder. The `TempDir` must outlive the path.
fn new_vault() -> (tempfile::TempDir, PathBuf) {
    let tmp = tempfile::tempdir().unwrap();
    let root = tmp.path().join("vault");
    let out = selenite().arg("init").arg(&root).output().unwrap();
    assert!(out.status.success(), "init failed: {}", stderr(&out));
    (tmp, root)
}

#[test]
fn init_creates_obsidian_compatible_vault() {
    let tmp = tempfile::tempdir().unwrap();
    let root = tmp.path().join("v");
    let out = selenite().arg("init").arg(&root).output().unwrap();
    assert!(out.status.success(), "{}", stderr(&out));
    assert!(stdout(&out).contains("Created vault at"));
    assert!(root.join(".obsidian").is_dir());
    assert!(root.join(".selenite").join("config.yaml").is_file());
}

#[test]
fn backlinks_list_the_linking_page() {
    let (_tmp, root) = new_vault();
    let out = vault_cmd(&root, &["add", "Alpha", "--body", "See [[Beta]]"]);
    assert!(out.status.success(), "{}", stderr(&out));
    let out = vault_cmd(&root, &["add", "Beta", "--body", "b"]);
    assert!(out.status.success(), "{}", stderr(&out));

    let out = vault_cmd(&root, &["backlinks", "Beta"]);
    assert!(out.status.success(), "{}", stderr(&out));
    assert!(stdout(&out).contains("Alpha.md"), "{}", stdout(&out));
}

#[test]
fn json_backlinks_is_an_array_of_one() {
    let (_tmp, root) = new_vault();
    vault_cmd(&root, &["add", "Alpha", "--body", "See [[Beta]]"]);
    vault_cmd(&root, &["add", "Beta", "--body", "b"]);

    let out = vault_cmd(&root, &["--json", "backlinks", "Beta"]);
    assert!(out.status.success(), "{}", stderr(&out));
    let value: serde_json::Value = serde_json::from_str(&stdout(&out)).unwrap();
    let items = value.as_array().unwrap();
    assert_eq!(items.len(), 1);
    assert_eq!(items[0]["source"], "Alpha.md");
}

#[test]
fn search_finds_a_word() {
    let (_tmp, root) = new_vault();
    vault_cmd(&root, &["add", "Alpha", "--body", "The lantern is lit"]);

    let out = vault_cmd(&root, &["search", "lantern"]);
    assert!(out.status.success(), "{}", stderr(&out));
    let text = stdout(&out);
    assert!(text.contains("Alpha.md"), "{text}");
    assert!(text.contains("**lantern**"), "{text}");
}

#[test]
fn rebuilt_index_gives_identical_search_and_backlinks() {
    let (_tmp, root) = new_vault();
    vault_cmd(
        &root,
        &["add", "Alpha", "--body", "See [[Beta]] about the lantern"],
    );
    vault_cmd(&root, &["add", "Beta", "--body", "beta notes"]);

    let snapshot = || {
        let search = stdout(&vault_cmd(&root, &["search", "beta"]));
        let back = stdout(&vault_cmd(&root, &["backlinks", "Beta"]));
        (search, back)
    };
    let before = snapshot();
    assert!(before.0.contains("Beta.md"), "{}", before.0);
    assert!(before.1.contains("Alpha.md"), "{}", before.1);

    let state = root.join(".selenite");
    for name in ["index.sqlite", "index.sqlite-wal", "index.sqlite-shm"] {
        let _ = std::fs::remove_file(state.join(name));
    }
    let out = vault_cmd(&root, &["rebuild-index"]);
    assert!(out.status.success(), "{}", stderr(&out));
    assert!(stdout(&out).contains("Indexed 3 pages"), "{}", stdout(&out));

    assert_eq!(snapshot(), before);
}

#[test]
fn daily_prints_the_dated_path() {
    let (_tmp, root) = new_vault();
    let out = vault_cmd(&root, &["daily", "--date", "2026-11-16"]);
    assert!(out.status.success(), "{}", stderr(&out));
    assert_eq!(stdout(&out).trim(), "Daily/2026-11-16.md");
    assert!(root.join("Daily").join("2026-11-16.md").is_file());
}

#[test]
fn sync_is_reserved_and_exits_with_code_2() {
    let (_tmp, root) = new_vault();
    let out = vault_cmd(&root, &["sync"]);
    assert_eq!(out.status.code(), Some(2));
    assert!(stderr(&out).contains("not available until a later phase"));
}
