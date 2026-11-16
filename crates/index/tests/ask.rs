//! Cited retrieval (`Engine::ask`) on a small fixture vault.

#![allow(missing_docs, clippy::unwrap_used, clippy::expect_used, clippy::panic)]

use selenite_core::{APP_DIR, VaultPath};
use selenite_index::{Engine, INDEX_FILE, INDEX_VERSION};

fn p(s: &str) -> VaultPath {
    VaultPath::new(s).unwrap()
}

fn fixture() -> (tempfile::TempDir, Engine) {
    let dir = tempfile::tempdir().unwrap();
    let mut e = Engine::init_local(dir.path()).unwrap();
    e.create_page(
        &p("Travel/Berlin Trip.md"),
        "---\ntitle: Berlin Trip\ntags: [travel]\n---\n# Berlin Trip\n\nFlights are booked for May.\n\n## Hotel\n\nWe stay at Hotel Adlon in Berlin; checkout is at noon on Sunday.\n\n## Packing\n\nBring a raincoat and good shoes.\n",
    )
    .unwrap();
    e.create_page(
        &p("Cooking/Pasta.md"),
        "# Pasta\n\nBoil water, salt it, cook the spaghetti for nine minutes.\n\n## Sauce\n\nTomatoes, garlic and basil. No hotel needed.\n",
    )
    .unwrap();
    e.create_page(
        &p("Misc/Ideas.md"),
        "# Ideas\n\nA short note about gardening and birds.\n",
    )
    .unwrap();
    (dir, e)
}

fn file_lines(e: &Engine, path: &str, start: u32, end: u32) -> String {
    let (text, _) = e.read_page(&p(path)).unwrap();
    text.lines()
        .skip(start as usize - 1)
        .take((end - start + 1) as usize)
        .collect::<Vec<_>>()
        .join("\n")
}

#[test]
fn answering_passage_is_first_and_cites_file_lines() {
    let (_d, e) = fixture();
    let cites = e
        .ask("Where do we stay in Berlin and when is checkout?", 5)
        .unwrap();
    assert!(!cites.is_empty());
    let first = &cites[0];
    assert_eq!(first.path, "Travel/Berlin Trip.md");
    assert_eq!(first.heading.as_deref(), Some("Hotel"));
    assert!(first.excerpt.contains("checkout is at noon"));
    // Lines are file lines (front matter counted), so they quote the same text.
    assert_eq!((first.start_line, first.end_line), (11, 11));
    assert_eq!(
        file_lines(&e, &first.path, first.start_line, first.end_line),
        first.excerpt
    );
    for c in &cites {
        assert!(c.score > 0.0 && c.score <= 1.2);
    }
    // Deterministic.
    assert_eq!(
        cites,
        e.ask("Where do we stay in Berlin and when is checkout?", 5)
            .unwrap()
    );
}

#[test]
fn stopword_only_and_unmatched_questions_give_nothing() {
    let (_d, e) = fixture();
    assert!(e.ask("what is the", 5).unwrap().is_empty());
    assert!(e.ask("", 5).unwrap().is_empty());
    assert!(e.ask("quantum chromodynamics", 5).unwrap().is_empty());
}

#[test]
fn limit_overlap_and_per_page_caps() {
    let (_d, mut e) = fixture();
    let mut body = String::from("# Log\n");
    for i in 0..8 {
        body.push_str(&format!("\n## Entry {i}\n\nzebra sighting number {i}.\n"));
    }
    e.create_page(&p("Log.md"), &body).unwrap();
    let cites = e.ask("zebra sighting", 10).unwrap();
    assert_eq!(cites.len(), 3, "at most 3 passages per page");
    assert!(cites.iter().all(|c| c.path == "Log.md"));
    let mut ranges: Vec<_> = cites.iter().map(|c| (c.start_line, c.end_line)).collect();
    ranges.sort_unstable();
    for w in ranges.windows(2) {
        assert!(w[0].1 < w[1].0, "overlap: {ranges:?}");
    }
    assert_eq!(e.ask("zebra sighting", 2).unwrap().len(), 2);
}

#[test]
fn pages_one_link_hop_from_the_best_page_get_a_boost() {
    let (_d, mut e) = fixture();
    e.create_page(
        &p("Hub.md"),
        "# Hub\n\nThe quokka habitat tour is described in [[Zeta]].\n",
    )
    .unwrap();
    e.create_page(&p("Zeta.md"), "# Zeta\n\nA note on the quokka tour.\n")
        .unwrap();
    e.create_page(&p("Alpha.md"), "# Alpha\n\nA note on the quokka tour.\n")
        .unwrap();
    let cites = e.ask("quokka habitat tour", 5).unwrap();
    assert_eq!(cites[0].path, "Hub.md");
    let pos = |name: &str| cites.iter().position(|c| c.path == name).unwrap();
    // Alpha sorts before Zeta on a tie; the link from Hub lifts Zeta above it.
    assert!(pos("Zeta.md") < pos("Alpha.md"), "{cites:?}");
}

#[test]
fn edits_and_removals_keep_passages_in_sync() {
    let (_d, mut e) = fixture();
    let path = p("Misc/Ideas.md");
    let (_, rev) = e.read_page(&path).unwrap();
    e.save_page(
        &path,
        "# Ideas\n\nNow about volcanoes instead.\n",
        Some(rev),
    )
    .unwrap();
    assert!(e.ask("gardening birds", 5).unwrap().is_empty());
    assert_eq!(e.ask("volcanoes", 5).unwrap()[0].path, "Misc/Ideas.md");
    e.trash_page(&path).unwrap();
    assert!(e.ask("volcanoes", 5).unwrap().is_empty());
}

#[test]
fn old_index_version_is_rebuilt_with_passages() {
    let dir = tempfile::tempdir().unwrap();
    {
        let mut e = Engine::init_local(dir.path()).unwrap();
        e.create_page(
            &p("Note.md"),
            "# Note\n\nThe walrus keeps the lighthouse key.\n",
        )
        .unwrap();
    }
    assert_eq!(INDEX_VERSION, 4);
    // Make the on-disk index look like the previous schema: no passages table, version 3.
    let index_path = dir.path().join(APP_DIR).join(INDEX_FILE);
    {
        let conn = rusqlite::Connection::open(&index_path).unwrap();
        conn.execute_batch("DROP TABLE passages;").unwrap();
        conn.pragma_update(None, "user_version", 3).unwrap();
    }
    let e = Engine::open_local(dir.path()).unwrap();
    let cites = e.ask("who keeps the lighthouse key", 3).unwrap();
    assert_eq!(cites[0].path, "Note.md");
    assert_eq!((cites[0].start_line, cites[0].end_line), (3, 3));
}
