//! End-to-end engine tests on a real temporary folder.

#![allow(missing_docs, clippy::unwrap_used, clippy::expect_used, clippy::panic)]

mod support;

use std::time::Instant;

use selenite_core::{APP_DIR, VaultPath};
use selenite_index::{Engine, INDEX_FILE, engine::EngineError};
use selenite_storage::StorageError;

fn p(s: &str) -> VaultPath {
    VaultPath::new(s).unwrap()
}

#[test]
fn init_creates_obsidian_compatible_vault() {
    let dir = tempfile::tempdir().unwrap();
    let root = dir.path().join("v");
    let engine = Engine::init_local(&root).unwrap();
    assert!(root.join(".obsidian").is_dir());
    assert!(root.join(".selenite/config.yaml").is_file());
    assert!(root.join("Welcome.md").is_file());
    assert!(root.join("Daily").is_dir());
    assert_eq!(engine.index().page_count().unwrap(), 1);
    drop(engine);
    assert!(matches!(
        Engine::init_local(&root),
        Err(EngineError::NotAVault(_))
    ));
}

#[test]
fn two_linked_pages_yield_backlink() {
    let dir = tempfile::tempdir().unwrap();
    let mut e = Engine::init_local(dir.path()).unwrap();
    e.create_page(&p("Alpha.md"), "# Alpha\n\nSee [[Beta]] for details.\n")
        .unwrap();
    e.create_page(&p("Projects/Beta.md"), "# Beta\n\nNo links here.\n")
        .unwrap();
    let back = e.backlinks(&p("Projects/Beta.md")).unwrap();
    assert_eq!(back.len(), 1);
    assert_eq!(back[0].source, p("Alpha.md"));
    assert_eq!(back[0].line, 3);
    assert!(back[0].context.contains("[[Beta]]"));
    assert!(e.backlinks(&p("Alpha.md")).unwrap().is_empty());
}

#[test]
fn backlink_appears_when_target_created_later_and_via_alias() {
    let dir = tempfile::tempdir().unwrap();
    let mut e = Engine::init_local(dir.path()).unwrap();
    e.create_page(&p("A.md"), "[[Later]] and [[Nick|shown]]\n")
        .unwrap();
    assert_eq!(e.index().unresolved().unwrap().len(), 2);
    e.create_page(&p("Later.md"), "hi\n").unwrap();
    e.create_page(&p("Real Name.md"), "---\naliases: [Nick]\n---\nbody\n")
        .unwrap();
    assert_eq!(e.backlinks(&p("Later.md")).unwrap().len(), 1);
    assert_eq!(e.backlinks(&p("Real Name.md")).unwrap().len(), 1);
    assert!(e.index().unresolved().unwrap().is_empty());
}

#[test]
fn resolution_prefers_exact_path_then_shortest() {
    let dir = tempfile::tempdir().unwrap();
    let mut e = Engine::init_local(dir.path()).unwrap();
    e.create_page(&p("deep/nested/Same.md"), "x\n").unwrap();
    e.create_page(&p("top/Same.md"), "x\n").unwrap();
    assert_eq!(e.resolve("Same").unwrap(), Some(p("top/Same.md")));
    assert_eq!(
        e.resolve("nested/same").unwrap(),
        Some(p("deep/nested/Same.md"))
    );
    assert_eq!(
        e.resolve("deep/nested/Same.md").unwrap(),
        Some(p("deep/nested/Same.md"))
    );
    assert_eq!(e.resolve("missing").unwrap(), None);
}

#[test]
fn search_with_operators() {
    let dir = tempfile::tempdir().unwrap();
    let mut e = Engine::init_local(dir.path()).unwrap();
    e.create_page(
        &p("Rust.md"),
        "# Rust notes\n\nOwnership and borrowing. #lang/rust\n",
    )
    .unwrap();
    e.create_page(
        &p("Go.md"),
        "# Go notes\n\nGoroutines and borrowing ideas. #lang/go\n",
    )
    .unwrap();
    e.create_page(
        &p("Daily/2026-11-16.md"),
        "Learned about ownership today.\n",
    )
    .unwrap();
    let titles = |q: &str| -> Vec<String> {
        let mut v: Vec<String> = e
            .search(q, 20)
            .unwrap()
            .into_iter()
            .map(|h| h.path.to_string())
            .collect();
        v.sort();
        v
    };
    assert_eq!(titles("borrow"), vec!["Go.md", "Rust.md"]);
    assert_eq!(titles("borrowing -goroutines"), vec!["Rust.md"]);
    assert_eq!(titles("tag:lang"), vec!["Go.md", "Rust.md"]);
    assert_eq!(titles("tag:lang/go"), vec!["Go.md"]);
    assert_eq!(titles("ownership path:daily"), vec!["Daily/2026-11-16.md"]);
    assert_eq!(titles("\"ownership and borrowing\""), vec!["Rust.md"]);
    assert_eq!(titles("title:rust"), vec!["Rust.md"]);
    // FTS syntax in user input must not error.
    assert!(e.search("NEAR( \" * ^ OR AND", 10).is_ok());
    let hit = &e.search("goroutines", 1).unwrap()[0];
    assert!(hit.snippet.contains(selenite_index::HIGHLIGHT_START));
}

#[test]
fn stale_save_is_a_conflict_and_does_not_overwrite() {
    let dir = tempfile::tempdir().unwrap();
    let mut e = Engine::init_local(dir.path()).unwrap();
    let path = p("Note.md");
    e.create_page(&path, "v1\n").unwrap();
    let (_, rev1) = e.read_page(&path).unwrap();
    e.save_page(&path, "v2\n", Some(rev1.clone())).unwrap();
    let err = e.save_page(&path, "v3\n", Some(rev1)).unwrap_err();
    assert!(matches!(
        err,
        EngineError::Storage(StorageError::Conflict { .. })
    ));
    assert_eq!(e.read_page(&path).unwrap().0, "v2\n");
}

#[test]
fn refresh_picks_up_external_edits() {
    let dir = tempfile::tempdir().unwrap();
    let mut e = Engine::init_local(dir.path()).unwrap();
    std::fs::write(dir.path().join("Outside.md"), "links to [[Welcome]]\n").unwrap();
    let stats = e.refresh().unwrap();
    assert_eq!(stats.added, 1);
    assert_eq!(e.backlinks(&p("Welcome.md")).unwrap().len(), 1);
    std::fs::remove_file(dir.path().join("Outside.md")).unwrap();
    assert_eq!(e.refresh().unwrap().removed, 1);
    assert!(e.backlinks(&p("Welcome.md")).unwrap().is_empty());
}

#[test]
fn daily_note_is_created_once() {
    let dir = tempfile::tempdir().unwrap();
    let mut e = Engine::init_local(dir.path()).unwrap();
    let date = chrono::NaiveDate::from_ymd_opt(2026, 11, 16).unwrap();
    let path = e.daily_note(date).unwrap();
    assert_eq!(path, p("Daily/2026-11-16.md"));
    let (text, _) = e.read_page(&path).unwrap();
    e.save_page(
        &path,
        &format!("{text}edited\n"),
        Some(e.read_page(&path).unwrap().1),
    )
    .unwrap();
    assert_eq!(e.daily_note(date).unwrap(), path);
    assert!(e.read_page(&path).unwrap().0.ends_with("edited\n"));
}

#[test]
fn trash_and_rename_keep_index_consistent() {
    let dir = tempfile::tempdir().unwrap();
    let mut e = Engine::init_local(dir.path()).unwrap();
    e.create_page(&p("A.md"), "[[B]]\n").unwrap();
    e.create_page(&p("B.md"), "b\n").unwrap();
    e.rename_page(&p("B.md"), &p("folder/B.md")).unwrap();
    assert_eq!(e.backlinks(&p("folder/B.md")).unwrap().len(), 1);
    e.trash_page(&p("A.md")).unwrap();
    assert!(e.backlinks(&p("folder/B.md")).unwrap().is_empty());
    assert!(!dir.path().join("A.md").exists());
    assert!(e.search("tag:none", 5).unwrap().is_empty());
    let tree = e.tree().unwrap();
    assert!(tree.children.iter().all(|c| !c.name.starts_with('.')));
}

/// Acceptance: deleting the index and rebuilding restores identical search and backlinks.
#[test]
fn rebuild_after_deleting_index_is_identical() {
    let dir = tempfile::tempdir().unwrap();
    support::vaultgen::generate_vault(dir.path(), 500).unwrap();
    let queries = [
        "lantern",
        "harbor quartz",
        "\"lunar orbit\"",
        "tag:river",
        "meadow -valley",
    ];
    let snapshot = |e: &Engine| {
        let search: Vec<Vec<(String, String)>> = queries
            .iter()
            .map(|q| {
                e.search(q, 100)
                    .unwrap()
                    .into_iter()
                    .map(|h| (h.path.to_string(), h.snippet))
                    .collect()
            })
            .collect();
        let back: Vec<Vec<String>> = (0..500)
            .step_by(37)
            .map(|i| {
                let path = p(&format!(
                    "notes/b{:02}/{}.md",
                    i % 50,
                    support::vaultgen::note_name(i)
                ));
                e.backlinks(&path)
                    .unwrap()
                    .into_iter()
                    .map(|b| format!("{}:{}", b.source, b.line))
                    .collect()
            })
            .collect();
        (search, back)
    };
    let before = {
        let e = Engine::open_local(dir.path()).unwrap();
        snapshot(&e)
    };
    assert!(before.0.iter().any(|r| !r.is_empty()));
    assert!(before.1.iter().any(|r| !r.is_empty()));
    let index_file = dir.path().join(APP_DIR).join(INDEX_FILE);
    std::fs::remove_file(&index_file).unwrap();
    let _ = std::fs::remove_file(dir.path().join(APP_DIR).join(format!("{INDEX_FILE}-wal")));
    let _ = std::fs::remove_file(dir.path().join(APP_DIR).join(format!("{INDEX_FILE}-shm")));
    let mut e = Engine::open_local(dir.path()).unwrap();
    assert_eq!(snapshot(&e), before);
    e.rebuild_index().unwrap();
    assert_eq!(snapshot(&e), before);
}

/// Smoke check of the latency target (the criterion bench is the real evidence).
#[test]
fn search_10k_under_budget_in_release() {
    if cfg!(debug_assertions) {
        return; // timing is only meaningful in optimised builds: `cargo test --release`
    }
    let dir = tempfile::tempdir().unwrap();
    support::vaultgen::generate_vault(dir.path(), 10_000).unwrap();
    let e = Engine::open_local(dir.path()).unwrap();
    assert_eq!(e.index().page_count().unwrap(), 10_000);
    for q in [
        "lantern",
        "harbor quartz",
        "obsi",
        "garden tag:river",
        "\"lunar orbit\"",
    ] {
        let t = Instant::now();
        let hits = e.search(q, 50).unwrap();
        let ms = t.elapsed().as_secs_f64() * 1000.0;
        assert!(!hits.is_empty(), "{q}");
        assert!(ms < 100.0, "{q} took {ms:.1} ms");
    }
}
