#![allow(missing_docs, clippy::unwrap_used, clippy::expect_used, clippy::panic)]

//! End-to-end tests for `selenite import`.

use std::path::{Path, PathBuf};
use std::process::{Command, Output};

fn selenite() -> Command {
    Command::new(env!("CARGO_BIN_EXE_selenite"))
}

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

fn new_vault() -> (tempfile::TempDir, PathBuf) {
    let tmp = tempfile::tempdir().unwrap();
    let root = tmp.path().join("vault");
    let out = selenite().arg("init").arg(&root).output().unwrap();
    assert!(out.status.success(), "init failed: {}", stderr(&out));
    (tmp, root)
}

#[test]
fn import_obsidian_reports_json_and_dry_run_writes_nothing() {
    let (tmp, root) = new_vault();
    let src = tmp.path().join("obs");
    std::fs::create_dir_all(src.join(".obsidian")).unwrap();
    std::fs::write(src.join("A.md"), "hello [[B]]").unwrap();
    std::fs::write(src.join("B.md"), "b").unwrap();
    std::fs::write(src.join(".obsidian").join("x.json"), "{}").unwrap();

    let out = vault_cmd(
        &root,
        &[
            "--json",
            "import",
            "obsidian",
            src.to_str().unwrap(),
            "--into",
            "In",
            "--dry-run",
        ],
    );
    assert!(out.status.success(), "{}", stderr(&out));
    let v: serde_json::Value = serde_json::from_str(&stdout(&out)).unwrap();
    assert_eq!(v["pages"], 2);
    assert_eq!(v["dry_run"], true);
    assert!(!root.join("In").exists());

    let out = vault_cmd(
        &root,
        &["import", "obsidian", src.to_str().unwrap(), "--into", "In"],
    );
    assert!(out.status.success(), "{}", stderr(&out));
    assert!(stdout(&out).contains("2 pages"), "{}", stdout(&out));
    assert_eq!(
        std::fs::read_to_string(root.join("In/A.md")).unwrap(),
        "hello [[B]]"
    );
    assert!(!root.join("In/.obsidian").exists());
}

#[test]
fn import_todoist_creates_tasks_and_bad_source_exits_1() {
    let (tmp, root) = new_vault();
    let csv = tmp.path().join("Errands.csv");
    std::fs::write(&csv, "TYPE,CONTENT,PRIORITY,INDENT\ntask,Buy milk,4,1\n").unwrap();
    let out = vault_cmd(
        &root,
        &["import", "todoist", csv.to_str().unwrap(), "--into", "T"],
    );
    assert!(out.status.success(), "{}", stderr(&out));
    assert!(root.join("T/Errands/Buy milk.md").is_file());

    let out = vault_cmd(&root, &["import", "notion", "/definitely/missing.zip"]);
    assert_eq!(out.status.code(), Some(1));
}
