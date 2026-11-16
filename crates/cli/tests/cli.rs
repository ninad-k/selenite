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
fn sync_without_remotes_explains_and_exits_zero() {
    let (_tmp, root) = new_vault();
    let out = vault_cmd(&root, &["sync"]);
    assert_eq!(out.status.code(), Some(0), "{}", stderr(&out));
    assert!(stdout(&out).contains("selenite remote add"));
    let out = vault_cmd(&root, &["remote", "list"]);
    assert!(out.status.success());
    assert!(stdout(&out).contains("No remotes"));
}

#[test]
fn remote_add_without_client_ids_fails_cleanly() {
    let (_tmp, root) = new_vault();
    let out = selenite()
        .arg("--vault")
        .arg(&root)
        .args(["remote", "add", "google-drive"])
        .env_remove("SELENITE_GOOGLE_CLIENT_ID")
        .env_remove("SELENITE_GOOGLE_CLIENT_SECRET")
        .env_remove("SELENITE_TEST_GOOGLE_CLIENT_ID")
        .env_remove("SELENITE_TEST_GOOGLE_CLIENT_SECRET")
        .output()
        .unwrap();
    // A build that bakes in ids (release) would try the browser; only assert for dev builds.
    if option_env!("SELENITE_GOOGLE_CLIENT_ID").is_none() {
        assert_eq!(out.status.code(), Some(1));
        assert!(stderr(&out).contains("sign-in is not configured for this build"));
        assert!(!root.join(".selenite").join("remotes.yaml").exists());
    }
}

fn json_of(out: &Output) -> serde_json::Value {
    serde_json::from_str(&stdout(out)).unwrap()
}

#[test]
fn task_add_recurring_rent_is_listed_with_priority_and_repeat_mark() {
    let (_tmp, root) = new_vault();
    let out = vault_cmd(
        &root,
        &["task", "add", "pay rent every 1st at 9am p1 #home"],
    );
    assert!(out.status.success(), "{}", stderr(&out));
    assert_eq!(stdout(&out).trim(), "Tasks/pay rent.md");
    assert!(root.join("Tasks").join("pay rent.md").is_file());

    let out = vault_cmd(&root, &["task", "list"]);
    assert!(out.status.success(), "{}", stderr(&out));
    let text = stdout(&out);
    assert!(text.contains("[ ] p1"), "{text}");
    assert!(text.contains("pay rent"), "{text}");
    assert!(text.contains("#home"), "{text}");
    assert!(text.contains('↻'), "{text}");

    let out = vault_cmd(&root, &["--json", "task", "list"]);
    let value = json_of(&out);
    assert_eq!(value[0]["fields"]["title"], "pay rent");
    assert_eq!(value[0]["fields"]["priority"], 1);
}

#[test]
fn task_list_today_shows_only_todays_task() {
    let (_tmp, root) = new_vault();
    vault_cmd(&root, &["task", "add", "buy milk today"]);
    vault_cmd(&root, &["task", "add", "water plants"]);

    let out = vault_cmd(&root, &["task", "list", "today"]);
    assert!(out.status.success(), "{}", stderr(&out));
    let text = stdout(&out);
    assert!(text.contains("buy milk"), "{text}");
    assert!(!text.contains("water plants"), "{text}");
}

#[test]
fn task_done_then_completed_list_contains_it() {
    let (_tmp, root) = new_vault();
    vault_cmd(&root, &["task", "add", "buy milk today"]);
    let out = vault_cmd(&root, &["task", "done", "buy milk"]);
    assert!(out.status.success(), "{}", stderr(&out));
    assert_eq!(stdout(&out).trim(), "completed");

    let out = vault_cmd(&root, &["task", "list", "completed"]);
    assert!(out.status.success(), "{}", stderr(&out));
    let text = stdout(&out);
    assert!(text.contains("[x]"), "{text}");
    assert!(text.contains("buy milk"), "{text}");
}

#[test]
fn task_done_on_recurring_task_prints_next_due() {
    let (_tmp, root) = new_vault();
    vault_cmd(
        &root,
        &["task", "add", "pay rent every 1st at 9am p1 #home"],
    );
    let out = vault_cmd(&root, &["task", "done", "pay rent"]);
    assert!(out.status.success(), "{}", stderr(&out));
    assert!(stdout(&out).contains("next:"), "{}", stdout(&out));
}

#[test]
fn task_reopen_and_archive_by_path() {
    let (_tmp, root) = new_vault();
    vault_cmd(&root, &["task", "add", "buy milk today"]);
    vault_cmd(&root, &["task", "done", "Tasks/buy milk.md"]);
    let out = vault_cmd(&root, &["--json", "task", "reopen", "Tasks/buy milk.md"]);
    assert!(out.status.success(), "{}", stderr(&out));
    // `done` is omitted from the JSON when false.
    assert!(json_of(&out)["fields"]["done"].is_null());

    let out = vault_cmd(&root, &["task", "archive", "buy milk"]);
    assert!(out.status.success(), "{}", stderr(&out));
    let out = vault_cmd(&root, &["task", "list"]);
    assert!(!stdout(&out).contains("buy milk"), "{}", stdout(&out));
}

#[test]
fn task_list_paths_flag_appends_path() {
    let (_tmp, root) = new_vault();
    vault_cmd(&root, &["task", "add", "buy milk"]);
    let out = vault_cmd(&root, &["task", "list", "--paths"]);
    assert!(
        stdout(&out).contains("(Tasks/buy milk.md)"),
        "{}",
        stdout(&out)
    );
}

#[test]
fn agenda_json_has_seven_days() {
    let (_tmp, root) = new_vault();
    vault_cmd(&root, &["task", "add", "buy milk today"]);
    let out = vault_cmd(&root, &["--json", "agenda"]);
    assert!(out.status.success(), "{}", stderr(&out));
    let value = json_of(&out);
    assert_eq!(value["days"].as_array().unwrap().len(), 7);
    assert!(value["overdue"].is_array());
}

#[test]
fn reminders_plan_runs_and_exits_zero() {
    let (_tmp, root) = new_vault();
    vault_cmd(
        &root,
        &["task", "add", "pay rent every 1st at 9am p1 #home"],
    );
    let out = vault_cmd(&root, &["reminders", "plan"]);
    assert!(out.status.success(), "{}", stderr(&out));
    let out = vault_cmd(&root, &["--json", "reminders", "plan"]);
    assert!(out.status.success(), "{}", stderr(&out));
    assert!(json_of(&out).is_array());
}

#[test]
fn view_save_then_views_round_trip() {
    let (_tmp, root) = new_vault();
    let out = vault_cmd(&root, &["task", "view-save", "Urgent", "p1 & open"]);
    assert!(out.status.success(), "{}", stderr(&out));
    let out = vault_cmd(&root, &["--json", "task", "views"]);
    assert!(out.status.success(), "{}", stderr(&out));
    let value = json_of(&out);
    let view = value
        .as_array()
        .unwrap()
        .iter()
        .find(|v| v["name"] == "Urgent")
        .unwrap();
    assert_eq!(view["filter"], "p1 & open");
    assert_eq!(view["layout"], "list");
}

#[test]
fn unknown_task_title_exits_with_code_1() {
    let (_tmp, root) = new_vault();
    let out = vault_cmd(&root, &["task", "done", "no such task"]);
    assert_eq!(out.status.code(), Some(1));
    assert!(stderr(&out).contains("no task matches"), "{}", stderr(&out));
}
