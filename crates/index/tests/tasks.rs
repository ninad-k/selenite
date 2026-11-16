//! Task database behaviour through the engine.

#![allow(missing_docs, clippy::unwrap_used, clippy::expect_used, clippy::panic)]

use chrono::{NaiveDate, NaiveDateTime};
use selenite_core::{Priority, TaskFields, VaultPath};
use selenite_index::Engine;
use selenite_vault::SavedView;

fn now() -> NaiveDateTime {
    NaiveDate::from_ymd_opt(2026, 11, 16)
        .unwrap()
        .and_hms_opt(10, 0, 0)
        .unwrap()
}

fn today() -> NaiveDate {
    now().date()
}

fn titles(items: &[selenite_index::TaskItem]) -> Vec<String> {
    items.iter().map(|t| t.fields.title.clone()).collect()
}

#[test]
fn quick_add_writes_a_readable_row_file() {
    let dir = tempfile::tempdir().unwrap();
    let mut e = Engine::init_local(dir.path()).unwrap();
    let (path, warnings) = e
        .quick_add("pay rent every 1st at 9am p1 #home", now())
        .unwrap();
    assert!(warnings.is_empty(), "{warnings:?}");
    assert_eq!(path, VaultPath::new("Tasks/pay rent.md").unwrap());
    assert!(dir.path().join("Tasks/_schema.yaml").is_file());
    let text = std::fs::read_to_string(dir.path().join("Tasks/pay rent.md")).unwrap();
    assert!(text.starts_with("---\n"), "{text}");
    assert!(
        text.contains("recurrence: FREQ=MONTHLY;BYMONTHDAY=1"),
        "{text}"
    );
    assert!(text.contains("project: home"), "{text}");
    let f = e.task(&path).unwrap();
    assert_eq!(f.title, "pay rent");
    assert_eq!(f.priority, Priority::P1);
    assert_eq!(f.due.unwrap().to_string(), "2026-12-01T09:00");
}

#[test]
fn builtin_views_and_custom_filters() {
    let dir = tempfile::tempdir().unwrap();
    let mut e = Engine::init_local(dir.path()).unwrap();
    e.quick_add("buy milk today @errand", now()).unwrap();
    e.quick_add("call bank tomorrow p2 #finance", now())
        .unwrap();
    e.quick_add("plan trip in 5 days #home/travel", now())
        .unwrap();
    e.quick_add("someday idea", now()).unwrap();
    let inbox = e.query_tasks("inbox & open", today()).unwrap();
    assert_eq!(titles(&inbox), vec!["buy milk", "someday idea"]);
    let todays = e.query_tasks("today & open", today()).unwrap();
    assert_eq!(titles(&todays), vec!["buy milk"]);
    let upcoming = e.query_tasks("next 7 days & open", today()).unwrap();
    assert_eq!(
        titles(&upcoming),
        vec!["buy milk", "call bank", "plan trip"]
    );
    assert_eq!(
        titles(&e.query_tasks("#home", today()).unwrap()),
        vec!["plan trip"]
    );
    assert_eq!(
        titles(&e.query_tasks("@errand | p2", today()).unwrap()),
        vec!["buy milk", "call bank"]
    );
    assert!(e.query_tasks("today p1", today()).is_err());
    let projects: Vec<String> = e
        .projects()
        .unwrap()
        .into_iter()
        .map(|p| p.project)
        .collect();
    assert_eq!(projects, vec!["finance", "home", "home/travel"]);
    assert_eq!(e.labels().unwrap(), vec!["errand"]);
}

#[test]
fn complete_reopen_and_recurring_advance() {
    let dir = tempfile::tempdir().unwrap();
    let mut e = Engine::init_local(dir.path()).unwrap();
    let (once, _) = e.quick_add("file taxes today", now()).unwrap();
    let (rent, _) = e.quick_add("pay rent every 1st at 9am", now()).unwrap();
    let f = e.complete_task(&once, now()).unwrap();
    assert!(f.done);
    assert_eq!(
        titles(&e.query_tasks("done", today()).unwrap()),
        vec!["file taxes"]
    );
    e.reopen_task(&once).unwrap();
    assert!(e.query_tasks("done", today()).unwrap().is_empty());
    let r = e.complete_task(&rent, now()).unwrap();
    assert!(!r.done);
    assert_eq!(r.due.unwrap().to_string(), "2027-01-01T09:00");
    assert_eq!(r.last_completed, NaiveDate::from_ymd_opt(2026, 12, 1));
}

#[test]
fn hand_written_rows_and_external_edits_are_indexed() {
    let dir = tempfile::tempdir().unwrap();
    let mut e = Engine::init_local(dir.path()).unwrap();
    e.ensure_tasks_db().unwrap();
    std::fs::write(
        dir.path().join("Tasks/Water plants.md"),
        "---\ndue: 2026-11-16\npriority: p2\nlabels: home\n---\nUse the blue can.\n",
    )
    .unwrap();
    e.refresh().unwrap();
    let all = e.all_tasks().unwrap();
    assert_eq!(titles(&all), vec!["Water plants"]);
    assert_eq!(all[0].fields.priority, Priority::P2);
    assert_eq!(all[0].fields.labels, vec!["home"]);
    // Editing keeps the body bytes.
    let path = all[0].path.clone();
    e.update_task(&path, |f| f.priority = Priority::P1).unwrap();
    let text = std::fs::read_to_string(dir.path().join("Tasks/Water plants.md")).unwrap();
    assert!(text.ends_with("---\nUse the blue can.\n"), "{text}");
    // Tasks are ordinary pages too: searchable and linkable.
    assert_eq!(e.search("blue", 5).unwrap().len(), 1);
}

#[test]
fn database_folders_survive_reopen_and_rebuild() {
    let dir = tempfile::tempdir().unwrap();
    {
        let mut e = Engine::init_local(dir.path()).unwrap();
        e.create_task(&TaskFields::new("alpha"), "").unwrap();
    }
    let mut e = Engine::open_local(dir.path()).unwrap();
    assert_eq!(e.all_tasks().unwrap().len(), 1);
    e.rebuild_index().unwrap();
    assert_eq!(e.all_tasks().unwrap().len(), 1);
    // A second task with the same title gets a distinct file.
    let p2 = e.create_task(&TaskFields::new("alpha"), "").unwrap();
    assert_eq!(p2, VaultPath::new("Tasks/alpha 2.md").unwrap());
    // Pages outside the database are not tasks.
    assert!(e.task(&VaultPath::new("Welcome.md").unwrap()).is_err());
}

#[test]
fn saved_views_round_trip_through_schema_file() {
    let dir = tempfile::tempdir().unwrap();
    let mut e = Engine::init_local(dir.path()).unwrap();
    e.save_view(
        SavedView {
            name: "Urgent".into(),
            filter: "p1 & open".into(),
            layout: "list".into(),
        },
        today(),
    )
    .unwrap();
    assert!(
        e.save_view(
            SavedView {
                name: "Bad".into(),
                filter: "((".into(),
                layout: "list".into()
            },
            today()
        )
        .is_err()
    );
    let views = e.saved_views().unwrap();
    assert_eq!(views.len(), 1);
    assert_eq!(views[0].name, "Urgent");
}

#[test]
fn archive_moves_task_to_trash() {
    let dir = tempfile::tempdir().unwrap();
    let mut e = Engine::init_local(dir.path()).unwrap();
    let (p, _) = e.quick_add("old chore", now()).unwrap();
    e.archive_task(&p).unwrap();
    assert!(e.all_tasks().unwrap().is_empty());
    assert!(dir.path().join(".selenite/trash").is_dir());
}
