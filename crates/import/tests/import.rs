#![allow(missing_docs, clippy::unwrap_used, clippy::expect_used, clippy::panic)]

//! Importer tests with hand-built fixtures (no real exports).

use std::fs;
use std::io::Write;
use std::path::{Path, PathBuf};

use chrono::{NaiveDate, NaiveDateTime};
use selenite_core::{Priority, VaultPath};
use selenite_import::{ImportOptions, import_notion, import_obsidian, import_todoist};
use selenite_index::Engine;

fn now() -> NaiveDateTime {
    NaiveDate::from_ymd_opt(2026, 11, 16)
        .unwrap()
        .and_hms_opt(10, 0, 0)
        .unwrap()
}

fn opts() -> ImportOptions {
    ImportOptions::new(now())
}

fn vault() -> (tempfile::TempDir, Engine) {
    let tmp = tempfile::tempdir().unwrap();
    let engine = Engine::init_local(&tmp.path().join("vault")).unwrap();
    (tmp, engine)
}

fn write(path: &Path, data: &[u8]) {
    fs::create_dir_all(path.parent().unwrap()).unwrap();
    fs::write(path, data).unwrap();
}

fn read_vault(engine: &Engine, p: &str) -> String {
    fs::read_to_string(engine.root().unwrap().join(p)).unwrap()
}

fn exists(engine: &Engine, p: &str) -> bool {
    engine.root().unwrap().join(p).exists()
}

fn files_outside_app_dir(root: &Path) -> Vec<String> {
    let mut out: Vec<String> = walkdir_files(root)
        .into_iter()
        .filter(|p| !p.starts_with(".selenite"))
        .collect();
    out.sort();
    out
}

fn walkdir_files(root: &Path) -> Vec<String> {
    fn rec(dir: &Path, root: &Path, out: &mut Vec<String>) {
        for e in fs::read_dir(dir).unwrap() {
            let p = e.unwrap().path();
            if p.is_dir() {
                rec(&p, root, out);
            } else {
                out.push(
                    p.strip_prefix(root)
                        .unwrap()
                        .to_string_lossy()
                        .replace('\\', "/"),
                );
            }
        }
    }
    let mut out = Vec::new();
    rec(root, root, &mut out);
    out
}

// ---- Obsidian -----------------------------------------------------------------------------

fn obsidian_fixture(dir: &Path) -> Vec<(&'static str, Vec<u8>)> {
    let files: Vec<(&'static str, Vec<u8>)> = vec![
        (
            "Home.md",
            b"---\ntags: [a]\n---\r\n# Home\r\nSee [[Other note|alias]] and ![[pic.png]]\r\n"
                .to_vec(),
        ),
        (
            "sub/Other note.md",
            "Caf\u{e9} \u{1F600} no newline".as_bytes().to_vec(),
        ),
        ("sub/pic.png", vec![0x89, b'P', b'N', b'G', 0, 1, 2, 255]),
    ];
    for (p, b) in &files {
        write(&dir.join(p), b);
    }
    write(&dir.join(".obsidian/app.json"), b"{\"dailyNotes\":1}");
    write(&dir.join(".trash/old.md"), b"gone");
    files
}

#[test]
fn obsidian_copies_byte_identical_and_skips_settings() {
    let (tmp, mut engine) = vault();
    let src = tmp.path().join("src");
    let files = obsidian_fixture(&src);
    let report = import_obsidian(&mut engine, &src, &opts()).unwrap();
    assert_eq!(report.target, "Imported/Obsidian 2026-11-16");
    assert_eq!((report.pages, report.attachments), (2, 1));
    for (p, bytes) in &files {
        let got = fs::read(
            engine
                .root()
                .unwrap()
                .join("Imported/Obsidian 2026-11-16")
                .join(p),
        )
        .unwrap();
        assert_eq!(&got, bytes, "{p} must be byte-identical");
    }
    assert!(!exists(
        &engine,
        "Imported/Obsidian 2026-11-16/.obsidian/app.json"
    ));
    assert!(!exists(
        &engine,
        "Imported/Obsidian 2026-11-16/.trash/old.md"
    ));
    // the index sees the pages, links resolve
    let home = VaultPath::new("Imported/Obsidian 2026-11-16/Home.md").unwrap();
    let out = engine.outgoing(&home).unwrap();
    assert!(out.iter().any(|l| l.resolved.is_some()));
}

#[test]
fn collisions_never_overwrite() {
    let (tmp, mut engine) = vault();
    let src = tmp.path().join("src");
    obsidian_fixture(&src);
    let mut o = opts();
    o.into = Some("In".into());
    import_obsidian(&mut engine, &src, &o).unwrap();
    let original = read_vault(&engine, "In/Home.md");
    // edit the imported file, then import again into the same folder
    fs::write(engine.root().unwrap().join("In/Home.md"), "edited").unwrap();
    import_obsidian(&mut engine, &src, &o).unwrap();
    import_obsidian(&mut engine, &src, &o).unwrap();
    assert_eq!(read_vault(&engine, "In/Home.md"), "edited");
    assert_eq!(read_vault(&engine, "In/Home (imported).md"), original);
    assert_eq!(read_vault(&engine, "In/Home (imported) 2.md"), original);
    assert!(exists(&engine, "In/sub/pic (imported).png"));
}

#[test]
fn dry_run_writes_nothing() {
    let (tmp, mut engine) = vault();
    let src = tmp.path().join("src");
    obsidian_fixture(&src);
    let before = files_outside_app_dir(engine.root().unwrap());
    let mut o = opts();
    o.dry_run = true;
    let r = import_obsidian(&mut engine, &src, &o).unwrap();
    assert!(r.dry_run);
    assert_eq!((r.pages, r.attachments), (2, 1));
    assert_eq!(files_outside_app_dir(engine.root().unwrap()), before);

    let csv = tmp.path().join("P.csv");
    write(
        &csv,
        b"TYPE,CONTENT,PRIORITY,INDENT,DATE\ntask,A,1,1,tomorrow\n",
    );
    let r = import_todoist(&mut engine, &csv, &o).unwrap();
    assert_eq!(r.tasks, 1);
    assert_eq!(files_outside_app_dir(engine.root().unwrap()), before);
}

// ---- Notion -------------------------------------------------------------------------------

const ID_A: &str = "0123456789abcdef0123456789abcdef";
const ID_B: &str = "aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa";
const ID_DB: &str = "bbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbb";
const ID_R1: &str = "11111111111111111111111111111111";
const ID_R2: &str = "22222222222222222222222222222222";

fn zip_bytes(entries: &[(String, Vec<u8>)]) -> Vec<u8> {
    let mut w = zip::ZipWriter::new(std::io::Cursor::new(Vec::new()));
    let o = zip::write::SimpleFileOptions::default()
        .compression_method(zip::CompressionMethod::Deflated);
    for (name, data) in entries {
        w.start_file(name.as_str(), o).unwrap();
        w.write_all(data).unwrap();
    }
    w.finish().unwrap().into_inner()
}

fn notion_zip(path: &Path) {
    let inner: Vec<(String, Vec<u8>)> = vec![
        (
            format!("Project Plan {ID_A}.md"),
            format!(
                "# Project Plan\n\nSee [Meeting Notes](Meeting%20Notes%20{ID_B}.md) and [the notes](Project%20Plan%20{ID_A}/Sub%20Page%20{ID_R1}.md).\n\n![diagram](Project%20Plan%20{ID_A}/diagram.png)\n\n[Tasks DB](Tasks%20{ID_DB}.csv) and [web](https://example.com/x%20y)\n"
            )
            .into_bytes(),
        ),
        (
            format!("Project Plan {ID_A}/diagram.png"),
            vec![1, 2, 3, 4, 250],
        ),
        (
            format!("Project Plan {ID_A}/Sub Page {ID_R1}.md"),
            b"# Sub Page\n\nsub body\n".to_vec(),
        ),
        (
            format!("Meeting Notes {ID_B}.md"),
            format!("# Meeting Notes\n\nBack to [Project Plan](Project%20Plan%20{ID_A}.md)\n").into_bytes(),
        ),
        (
            format!("Tasks {ID_DB}.csv"),
            "\u{feff}Name,Status,Estimate,Due,Done,Tags,Link\nWrite docs,Todo,3,\"November 20, 2026\",Yes,\"a, b\",https://x.io/1\nFix bug,Done,5,\"November 21, 2026\",No,\"b, c\",https://x.io/2\nShip it,Todo,8,\"December 1, 2026\",No,\"a, c\",https://x.io/3\n".as_bytes().to_vec(),
        ),
        (
            format!("Tasks {ID_DB}_all.csv"),
            b"Name,Status\nWrite docs,Todo\n".to_vec(),
        ),
        (
            format!("Tasks {ID_DB}/Write docs {ID_R2}.md"),
            format!("# Write docs\n\nStatus: Todo\nEstimate: 3\n\nRow body links [Meeting](../Meeting%20Notes%20{ID_B}.md)\n").into_bytes(),
        ),
    ];
    let inner_zip = zip_bytes(&inner);
    let outer = zip_bytes(&[("Export-1-Part-1.zip".to_string(), inner_zip)]);
    fs::write(path, outer).unwrap();
}

#[test]
fn notion_zip_maps_pages_links_databases_and_attachments() {
    let (tmp, mut engine) = vault();
    let zip = tmp.path().join("export.zip");
    notion_zip(&zip);
    let mut o = opts();
    o.into = Some("N".into());
    let r = import_notion(&mut engine, &zip, &o).unwrap();
    assert_eq!(r.pages, 3, "{r:?}");
    assert_eq!(r.rows, 3);
    assert_eq!(r.attachments, 1);
    assert!(
        r.skipped.iter().any(|(p, _)| p.ends_with("_all.csv")),
        "{:?}",
        r.skipped
    );

    // ids stripped from names
    assert!(exists(&engine, "N/Project Plan.md"));
    assert!(exists(&engine, "N/Meeting Notes.md"));
    assert!(exists(&engine, "N/Project Plan/Sub Page.md"));
    assert!(exists(&engine, "N/Project Plan/diagram.png"));
    let plan = read_vault(&engine, "N/Project Plan.md");
    assert!(plan.contains("[[Meeting Notes]]"), "{plan}");
    assert!(plan.contains("[[Sub Page|the notes]]"), "{plan}");
    assert!(
        plan.contains("![diagram](Project%20Plan/diagram.png)"),
        "{plan}"
    );
    assert!(plan.contains("[web](https://example.com/x%20y)"), "{plan}");
    assert!(plan.contains("Tasks DB and"), "{plan}");

    // backlinks via the index
    let meeting = VaultPath::new("N/Meeting Notes.md").unwrap();
    let bl = engine.backlinks(&meeting).unwrap();
    assert!(bl.iter().any(|b| b.source.as_str() == "N/Project Plan.md"));
    let plan_p = VaultPath::new("N/Project Plan.md").unwrap();
    let bl = engine.backlinks(&plan_p).unwrap();
    assert!(bl.iter().any(|b| b.source.as_str() == "N/Meeting Notes.md"));
    // row page links resolve too
    let bl = engine.backlinks(&meeting).unwrap();
    assert!(bl.iter().any(|b| b.source.as_str().starts_with("N/Tasks/")));

    // database
    let schema = read_vault(&engine, "N/Tasks/_schema.yaml");
    for needle in [
        "Status",
        "select",
        "number",
        "date",
        "checkbox",
        "multi_select",
        "url",
    ] {
        assert!(schema.contains(needle), "{needle} missing in\n{schema}");
    }
    let row = read_vault(&engine, "N/Tasks/Write docs.md");
    assert!(row.contains("Status: Todo"), "{row}");
    assert!(row.contains("Estimate: 3"), "{row}");
    assert!(row.contains("Done: true"), "{row}");
    assert!(row.contains("2026-11-20"), "{row}");
    assert!(
        row.contains("Row body links [[Meeting Notes|Meeting]]"),
        "{row}"
    );
    assert!(!row.contains("Status: Todo\nEstimate: 3\n\nRow"), "{row}");
    assert!(exists(&engine, "N/Tasks/Fix bug.md"));
    let rows = engine.database_rows("N/Tasks", now()).unwrap();
    assert_eq!(rows.len(), 3);
}

#[test]
fn notion_property_block_moves_to_front_matter_without_csv_row() {
    let (tmp, mut engine) = vault();
    let dir = tmp.path().join("exp");
    write(
        &dir.join(format!("DB {ID_DB}.csv")),
        b"Name,Status\nOther,Todo\n",
    );
    write(
        &dir.join(format!("DB {ID_DB}/Lonely {ID_R1}.md")),
        b"# Lonely\n\nOwner: Sam\nStage: 2\n\nHello\n",
    );
    let mut o = opts();
    o.into = Some("N".into());
    let r = import_notion(&mut engine, &dir, &o).unwrap();
    assert_eq!(r.rows, 2);
    let t = read_vault(&engine, "N/DB/Lonely.md");
    assert!(t.starts_with("---\n"), "{t}");
    assert!(t.contains("Owner: Sam") && t.ends_with("Hello\n"), "{t}");
}

#[test]
fn notion_collision_does_not_overwrite() {
    let (tmp, mut engine) = vault();
    let zip = tmp.path().join("export.zip");
    notion_zip(&zip);
    let mut o = opts();
    o.into = Some("N".into());
    import_notion(&mut engine, &zip, &o).unwrap();
    let first = read_vault(&engine, "N/Project Plan.md");
    import_notion(&mut engine, &zip, &o).unwrap();
    assert_eq!(read_vault(&engine, "N/Project Plan.md"), first);
    assert!(exists(&engine, "N/Project Plan (imported).md"));
    assert!(exists(&engine, "N/Tasks (imported)/_schema.yaml"));
}

// ---- Todoist ------------------------------------------------------------------------------

#[test]
fn todoist_csv_maps_sections_subtasks_priority_dates_labels() {
    let (tmp, mut engine) = vault();
    let csv = tmp.path().join("Home.csv");
    write(
        &csv,
        b"TYPE,CONTENT,DESCRIPTION,PRIORITY,INDENT,AUTHOR,RESPONSIBLE,DATE,DATE_LANG,TIMEZONE\n\
section,Chores,,,,,,,,\n\
task,Pay rent @money @home,Landlord,4,1,Me,,every 1st at 9am,en,UTC\n\
task,Sort receipts,,3,2,Me,,tomorrow,en,UTC\n\
task,Scan,,1,3,Me,,,en,UTC\n\
section,Later,,,,,,,,\n\
task,Garden,,2,1,Me,,whenever the moon is right,en,UTC\n\
meta,view_style,list,,,,,,,\n",
    );
    let mut o = opts();
    o.into = Some("T".into());
    let r = import_todoist(&mut engine, &csv, &o).unwrap();
    assert_eq!(r.tasks, 4, "{r:?}");

    let rent = engine
        .task(&VaultPath::new("T/Home/Pay rent.md").unwrap())
        .unwrap();
    assert_eq!(rent.priority, Priority::P1);
    assert_eq!(rent.section.as_deref(), Some("Chores"));
    assert_eq!(rent.project.as_deref(), Some("Home"));
    assert_eq!(rent.labels, vec!["money".to_string(), "home".to_string()]);
    assert!(rent.recurrence.is_some());
    assert!(rent.due.is_some());
    let text = read_vault(&engine, "T/Home/Pay rent.md");
    assert!(text.contains("Landlord"));

    let receipts = engine
        .task(&VaultPath::new("T/Home/Sort receipts.md").unwrap())
        .unwrap();
    assert_eq!(receipts.priority, Priority::P2);
    assert_eq!(receipts.parent.as_deref(), Some("[[Pay rent]]"));
    assert_eq!(
        receipts.due.unwrap().date,
        NaiveDate::from_ymd_opt(2026, 11, 17).unwrap()
    );
    let scan = engine
        .task(&VaultPath::new("T/Home/Scan.md").unwrap())
        .unwrap();
    assert_eq!(scan.priority, Priority::P4);
    assert_eq!(scan.parent.as_deref(), Some("[[Sort receipts]]"));

    let garden = engine
        .task(&VaultPath::new("T/Home/Garden.md").unwrap())
        .unwrap();
    assert_eq!(garden.priority, Priority::P3);
    assert_eq!(garden.section.as_deref(), Some("Later"));
    assert!(garden.parent.is_none());
    assert!(garden.due.is_none());
    assert!(read_vault(&engine, "T/Home/Garden.md").contains("whenever the moon is right"));

    // the folder is a Tasks database and the index sees the tasks
    assert!(engine.task_databases().contains(&"T/Home".to_string()));
    assert_eq!(engine.all_tasks().unwrap().len(), 4);
}

#[test]
fn todoist_folder_of_csvs_makes_one_database_each() {
    let (tmp, mut engine) = vault();
    let dir: PathBuf = tmp.path().join("exports");
    write(
        &dir.join("Work.csv"),
        b"TYPE,CONTENT,PRIORITY,INDENT\ntask,A,1,1\n",
    );
    write(
        &dir.join("Home.csv"),
        b"TYPE,CONTENT,PRIORITY,INDENT\ntask,B,1,1\n",
    );
    write(&dir.join("notes.txt"), b"ignored");
    let mut o = opts();
    o.into = Some("T".into());
    let r = import_todoist(&mut engine, &dir, &o).unwrap();
    assert_eq!(r.tasks, 2);
    assert!(exists(&engine, "T/Work/_schema.yaml"));
    assert!(exists(&engine, "T/Home/_schema.yaml"));
}

#[test]
fn todoist_rejects_non_todoist_csv() {
    let (tmp, mut engine) = vault();
    let csv = tmp.path().join("x.csv");
    write(&csv, b"a,b\n1,2\n");
    let r = import_todoist(&mut engine, &csv, &opts()).unwrap();
    assert_eq!(r.tasks, 0);
    assert_eq!(r.skipped.len(), 1);
}
