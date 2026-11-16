//! Unit tests: call the tool methods directly against a temporary vault.

use rmcp::handler::server::wrapper::Parameters;
use rmcp::model::CallToolResult;
use selenite_index::Engine;
use serde_json::{Value, json};
use tempfile::TempDir;

use crate::server::*;

fn server() -> (TempDir, VaultMcp) {
    let dir = tempfile::tempdir().expect("tempdir");
    let engine = Engine::init_local(dir.path()).expect("init vault");
    (dir, VaultMcp::new(engine))
}

fn text_of(r: &CallToolResult) -> String {
    r.content
        .iter()
        .filter_map(|c| c.as_text())
        .map(|t| t.text.clone())
        .collect()
}

fn is_error(r: &CallToolResult) -> bool {
    r.is_error == Some(true)
}

/// Parse a successful result's JSON text, failing the test on a tool error.
fn ok(r: CallToolResult) -> Value {
    assert!(!is_error(&r), "unexpected tool error: {}", text_of(&r));
    serde_json::from_str(&text_of(&r)).expect("json result")
}

fn path_param(p: &str) -> Parameters<PathParams> {
    Parameters(PathParams { path: p.into() })
}

async fn create(s: &VaultMcp, path: &str, markdown: &str) -> Value {
    ok(s.create_page(Parameters(CreatePageParams {
        title: None,
        path: Some(path.into()),
        folder: None,
        markdown: markdown.into(),
    }))
    .await)
}

fn rev(v: &Value) -> String {
    v["revision"].as_str().expect("revision").to_string()
}

#[tokio::test]
async fn create_page_then_get_page() {
    let (_d, s) = server();
    let created = create(&s, "Notes/Alpha.md", "# Alpha\n\nHello [[Beta]]\n").await;
    assert_eq!(created["path"], "Notes/Alpha.md");
    create(&s, "Beta.md", "# Beta\n\nSee [[Alpha]]\n").await;

    let page = ok(s.get_page(path_param("Notes/Alpha")).await);
    assert_eq!(page["path"], "Notes/Alpha.md");
    assert_eq!(page["text"], "# Alpha\n\nHello [[Beta]]\n");
    assert_eq!(page["revision"], created["revision"]);
    assert_eq!(page["backlinks"][0]["source"], "Beta.md");
    assert_eq!(page["outgoing"][0]["target"], "Beta");
    assert_eq!(page["outgoing"][0]["resolved"], "Beta.md");

    let titled = ok(s
        .create_page(Parameters(CreatePageParams {
            title: Some("Gamma Ideas".into()),
            path: None,
            folder: Some("Inbox".into()),
            markdown: "Body".into(),
        }))
        .await);
    assert_eq!(titled["path"], "Inbox/Gamma Ideas.md");

    let hits = ok(s
        .search(Parameters(SearchParams {
            query: "hello".into(),
            limit: None,
        }))
        .await);
    assert_eq!(hits["results"][0]["path"], "Notes/Alpha.md");
}

#[tokio::test]
async fn create_page_errors_are_tool_errors() {
    let (_d, s) = server();
    create(&s, "A.md", "x").await;
    let dup = s
        .create_page(Parameters(CreatePageParams {
            title: None,
            path: Some("A.md".into()),
            folder: None,
            markdown: "y".into(),
        }))
        .await;
    assert!(is_error(&dup));
    let neither = s
        .create_page(Parameters(CreatePageParams {
            title: None,
            path: None,
            folder: None,
            markdown: "y".into(),
        }))
        .await;
    assert!(is_error(&neither));
    let hidden = s
        .create_page(Parameters(CreatePageParams {
            title: None,
            path: Some(".selenite/audit/x.md".into()),
            folder: None,
            markdown: "y".into(),
        }))
        .await;
    assert!(is_error(&hidden));
    let escape = s.get_page(path_param("../outside.md")).await;
    assert!(is_error(&escape));
}

#[tokio::test]
async fn append_and_update_block_are_byte_stable() {
    let (dir, s) = server();
    let original = "---\ntitle: Plan\ntags: [x]\n---\n# Plan\n\nIntro paragraph ^intro\n\n- one\n- two\n\nTail text\n";
    create(&s, "Plan.md", original).await;

    let blocks = ok(s.list_blocks(path_param("Plan.md")).await);
    assert_eq!(blocks["blocks"].as_array().expect("blocks").len(), 4);
    assert_eq!(blocks["blocks"][1]["anchor"], "intro");
    assert_eq!(blocks["blocks"][2]["kind"], "list");

    let appended = ok(s
        .append_blocks(Parameters(AppendBlocksParams {
            path: "Plan.md".into(),
            markdown: "Added paragraph".into(),
            revision: Some(rev(&blocks)),
        }))
        .await);
    let on_disk = std::fs::read_to_string(dir.path().join("Plan.md")).expect("read");
    assert_eq!(on_disk, format!("{original}\nAdded paragraph\n"));

    let updated = ok(s
        .update_block(Parameters(UpdateBlockParams {
            path: "Plan.md".into(),
            block: "^intro".into(),
            markdown: "Rewritten intro".into(),
            revision: Some(rev(&appended)),
        }))
        .await);
    let on_disk = std::fs::read_to_string(dir.path().join("Plan.md")).expect("read");
    assert_eq!(
        on_disk,
        "---\ntitle: Plan\ntags: [x]\n---\n# Plan\n\nRewritten intro ^intro\n\n- one\n- two\n\nTail text\n\nAdded paragraph\n"
    );
    assert_ne!(rev(&updated), rev(&appended));

    // By index too.
    ok(s.update_block(Parameters(UpdateBlockParams {
        path: "Plan.md".into(),
        block: "2".into(),
        markdown: "- uno\n- dos".into(),
        revision: None,
    }))
    .await);
    let on_disk = std::fs::read_to_string(dir.path().join("Plan.md")).expect("read");
    assert!(on_disk.contains("- uno\n- dos\n\nTail text"));

    let bad = s
        .update_block(Parameters(UpdateBlockParams {
            path: "Plan.md".into(),
            block: "^nope".into(),
            markdown: "x".into(),
            revision: None,
        }))
        .await;
    assert!(is_error(&bad));
}

#[tokio::test]
async fn stale_revision_is_a_conflict_and_writes_nothing() {
    let (dir, s) = server();
    let first = create(&s, "Doc.md", "one\n").await;
    let r1 = rev(&first);
    ok(s.append_blocks(Parameters(AppendBlocksParams {
        path: "Doc.md".into(),
        markdown: "two".into(),
        revision: Some(r1.clone()),
    }))
    .await);
    let before = std::fs::read_to_string(dir.path().join("Doc.md")).expect("read");

    let stale = s
        .update_block(Parameters(UpdateBlockParams {
            path: "Doc.md".into(),
            block: "0".into(),
            markdown: "overwritten".into(),
            revision: Some(r1.clone()),
        }))
        .await;
    assert!(is_error(&stale));
    assert!(text_of(&stale).contains("conflict"), "{}", text_of(&stale));
    let stale = s
        .append_blocks(Parameters(AppendBlocksParams {
            path: "Doc.md".into(),
            markdown: "three".into(),
            revision: Some(r1),
        }))
        .await;
    assert!(is_error(&stale));
    let after = std::fs::read_to_string(dir.path().join("Doc.md")).expect("read");
    assert_eq!(before, after);

    let entries = s.audit_entries(10).await.expect("audit");
    assert_eq!(entries.len(), 4);
    assert!(!entries[2].ok && entries[2].error.as_deref() == Some("conflict"));
}

#[tokio::test]
async fn tasks_quick_add_list_complete() {
    let (_d, s) = server();
    let created = ok(s
        .create_task(Parameters(CreateTaskParams {
            text: Some("Water plants today p2 #Home".into()),
            title: None,
            priority: None,
            due: None,
            project: None,
            section: None,
            labels: None,
            reminders: None,
        }))
        .await);
    let path = created["path"].as_str().expect("path").to_string();
    assert_eq!(created["task"]["priority"], "p2");
    assert_eq!(created["task"]["project"], "Home");

    let today = ok(s
        .list_tasks(Parameters(ListTasksParams {
            filter: Some("today".into()),
            limit: None,
        }))
        .await);
    assert_eq!(today["total"], 1);
    assert_eq!(today["tasks"][0]["path"], path.as_str());

    let done = ok(s.complete_task(path_param(&path)).await);
    assert_eq!(done["task"]["done"], true);
    let open = ok(s
        .list_tasks(Parameters(ListTasksParams {
            filter: None,
            limit: None,
        }))
        .await);
    assert_eq!(open["total"], 0);
    let completed = ok(s
        .list_tasks(Parameters(ListTasksParams {
            filter: Some("completed".into()),
            limit: None,
        }))
        .await);
    assert_eq!(completed["total"], 1);

    let reopened = ok(s.reopen_task(path_param(&path)).await);
    assert_eq!(reopened["task"]["done"], false);

    let bad = s
        .list_tasks(Parameters(ListTasksParams {
            filter: Some("p1 &".into()),
            limit: None,
        }))
        .await;
    assert!(is_error(&bad));
}

#[tokio::test]
async fn update_task_fields_and_clear_due() {
    let (_d, s) = server();
    let created = ok(s
        .create_task(Parameters(CreateTaskParams {
            text: None,
            title: Some("Write report".into()),
            priority: Some("p3".into()),
            due: Some("2030-01-15".into()),
            project: Some("Work".into()),
            section: None,
            labels: Some(vec!["deep".into()]),
            reminders: None,
        }))
        .await);
    let path = created["path"].as_str().expect("path").to_string();
    assert_eq!(created["task"]["due"], "2030-01-15");

    let updated = ok(s
        .update_task(Parameters(
            serde_json::from_value(json!({
                "path": path, "priority": "p1", "due": null, "project": "Work/Q1", "labels": ["a", "b"]
            }))
            .expect("params"),
        ))
        .await);
    assert_eq!(updated["task"]["priority"], "p1");
    assert!(updated["task"].get("due").is_none());
    assert_eq!(updated["task"]["project"], "Work/Q1");
    assert_eq!(updated["task"]["labels"], json!(["a", "b"]));
    assert_eq!(updated["task"]["title"], "Write report");

    let bad = s
        .update_task(Parameters(
            serde_json::from_value(json!({"path": path, "due": "someday"})).expect("params"),
        ))
        .await;
    assert!(is_error(&bad));
}

#[tokio::test]
async fn set_reminder_replaces_the_list() {
    let (_d, s) = server();
    let created = ok(s
        .create_task(Parameters(CreateTaskParams {
            text: None,
            title: Some("Call dentist".into()),
            priority: None,
            due: Some("2030-03-01T09:00".into()),
            project: None,
            section: None,
            labels: None,
            reminders: Some(vec!["-1h".into()]),
        }))
        .await);
    let path = created["path"].as_str().expect("path").to_string();
    let set = ok(s
        .set_reminder(Parameters(SetReminderParams {
            path: path.clone(),
            reminders: vec!["-15m".into(), "2030-02-28T18:00".into()],
        }))
        .await);
    assert_eq!(
        set["task"]["reminders"],
        json!(["-15m", "2030-02-28T18:00"])
    );
    let bad = s
        .set_reminder(Parameters(SetReminderParams {
            path: path.clone(),
            reminders: vec!["soonish".into()],
        }))
        .await;
    assert!(is_error(&bad));
    let cleared = ok(s
        .set_reminder(Parameters(SetReminderParams {
            path,
            reminders: vec![],
        }))
        .await);
    assert!(cleared["task"].get("reminders").is_none());
}

#[tokio::test]
async fn agenda_lists_todays_and_overdue_tasks() {
    let (_d, s) = server();
    let yesterday = (chrono::Local::now().date_naive() - chrono::Duration::days(1)).to_string();
    for (title, due) in [("Due today", "today"), ("Was due", yesterday.as_str())] {
        let due = if due == "today" {
            chrono::Local::now().date_naive().to_string()
        } else {
            due.to_string()
        };
        ok(s.create_task(Parameters(CreateTaskParams {
            text: None,
            title: Some(title.into()),
            priority: None,
            due: Some(due),
            project: None,
            section: None,
            labels: None,
            reminders: None,
        }))
        .await);
    }
    let agenda = ok(s.get_agenda(Parameters(AgendaParams { days: None })).await);
    assert_eq!(agenda["days"].as_array().expect("days").len(), 7);
    assert_eq!(agenda["days"][0]["tasks"][0]["title"], "Due today");
    assert_eq!(agenda["overdue"][0]["title"], "Was due");
}

#[tokio::test]
async fn query_database_runs_task_filters_and_rejects_table_folders() {
    let (_d, s) = server();
    ok(s.create_task(Parameters(CreateTaskParams {
        text: None,
        title: Some("Urgent thing".into()),
        priority: Some("p1".into()),
        due: None,
        project: None,
        section: None,
        labels: None,
        reminders: None,
    }))
    .await);
    let all = ok(s
        .query_database(Parameters(QueryDatabaseParams {
            folder: None,
            filter: Some("p1".into()),
            view: None,
            query: None,
        }))
        .await);
    assert_eq!(all["total"], 1);
    let in_tasks = ok(s
        .query_database(Parameters(QueryDatabaseParams {
            folder: Some("Tasks".into()),
            filter: Some("p1".into()),
            view: None,
            query: None,
        }))
        .await);
    assert_eq!(in_tasks["total"], 1);
    let table = s
        .query_database(Parameters(QueryDatabaseParams {
            folder: Some("Books".into()),
            filter: Some("p1".into()),
            view: None,
            query: None,
        }))
        .await;
    assert!(is_error(&table));
    assert!(text_of(&table).contains("is not a database"));
    // `view` and `query` belong to table databases only.
    let misplaced = s
        .query_database(Parameters(QueryDatabaseParams {
            folder: None,
            filter: None,
            view: Some("Board".into()),
            query: None,
        }))
        .await;
    assert!(is_error(&misplaced));
}

fn item_props() -> Value {
    json!([
        {"name": "Status", "type": "select", "options": [{"name": "Todo"}, {"name": "Done"}]},
        {"name": "Price", "type": "number"},
        {"name": "Qty", "type": "number"},
        {"name": "Note", "type": "text"},
        {"name": "Total", "type": "formula", "expression": "prop(\"Price\") * prop(\"Qty\")"}
    ])
}

fn props_of(v: Value) -> Vec<Value> {
    v.as_array().expect("array").clone()
}

fn params<T: serde::de::DeserializeOwned>(v: Value) -> Parameters<T> {
    Parameters(serde_json::from_value(v).expect("params"))
}

/// A table database with two rows: Apple (Todo, 2 x 3) and Pear (Done, 5 x 4).
async fn items_db(s: &VaultMcp) {
    ok(s.create_database(params(json!({
        "folder": "Items",
        "properties": props_of(item_props()),
    })))
    .await);
    ok(s.create_row(params(json!({
        "folder": "Items",
        "title": "Apple",
        "values": {"Status": "Todo", "Price": 2, "Qty": 3, "Total": 999},
    })))
    .await);
    ok(s.create_row(params(json!({
        "folder": "Items",
        "title": "Pear",
        "values": {"Status": "Done", "Price": 5, "Qty": 4},
    })))
    .await);
}

#[tokio::test]
async fn table_database_create_rows_and_query() {
    let (dir, s) = server();
    items_db(&s).await;
    assert!(dir.path().join("Items").join("_schema.yaml").exists());
    assert!(dir.path().join("Items").join("Apple.md").exists());

    let listed = ok(s.list_databases().await);
    assert_eq!(listed["databases"][0]["folder"], "Items");
    assert_eq!(listed["databases"][0]["kind"], "table");
    assert_eq!(listed["databases"][0]["rows"], 2);

    let schema = ok(s
        .get_database_schema(params(json!({"folder": "Items"})))
        .await);
    assert_eq!(schema["kind"], "table");
    assert_eq!(schema["properties"][0]["name"], "Name");
    assert_eq!(schema["properties"][0]["type"], "title");
    assert_eq!(schema["views"][0]["name"], "Table");
    assert!(schema["revision"].is_string());

    // Ad-hoc query: sorted by the formula, which is computed (not stored).
    let sorted = ok(s
        .query_database(params(json!({
            "folder": "Items",
            "query": {"sorts": [{"property": "Total", "descending": true}]},
        })))
        .await);
    assert_eq!(sorted["rows"].as_array().expect("rows").len(), 2);
    assert_eq!(sorted["rows"][0]["title"], "Pear");
    assert_eq!(sorted["rows"][0]["values"]["Total"].as_f64(), Some(20.0));
    assert_eq!(sorted["rows"][1]["values"]["Total"].as_f64(), Some(6.0));
    assert!(sorted["groups"].is_null());

    // Grouped by a select property.
    let grouped = ok(s
        .query_database(params(json!({
            "folder": "Items",
            "query": {"group_by": "Status"},
        })))
        .await);
    let groups = grouped["groups"].as_array().expect("groups");
    assert_eq!(groups.len(), 2);
    assert_eq!(groups[0]["label"], "Todo");
    assert_eq!(groups[0]["rows"].as_array().expect("rows").len(), 1);
    assert_eq!(groups[1]["label"], "Done");

    // Filter with a condition.
    let filtered = ok(s
        .query_database(params(json!({
            "folder": "Items",
            "query": {"filter": {"op": "and", "rules": [
                {"property": "Status", "op": "is", "value": "Done"}
            ]}},
        })))
        .await);
    assert_eq!(filtered["rows"].as_array().expect("rows").len(), 1);
    assert_eq!(filtered["rows"][0]["title"], "Pear");

    // A stored view by name.
    ok(s.update_database_schema(params(json!({
        "folder": "Items",
        "views": [{"name": "By status", "layout": "board", "group_by": "Status"}],
    })))
    .await);
    let by_view = ok(s
        .query_database(params(json!({"folder": "Items", "view": "by status"})))
        .await);
    assert_eq!(by_view["view"]["layout"], "board");
    assert_eq!(by_view["groups"].as_array().expect("groups").len(), 2);
}

#[tokio::test]
async fn set_row_property_changes_front_matter_and_computed_values() {
    let (dir, s) = server();
    items_db(&s).await;
    let set = ok(s
        .set_row_property(params(json!({
            "path": "Items/Apple.md", "property": "Qty", "value": 7,
        })))
        .await);
    assert_eq!(set["property"], "Qty");
    let text = std::fs::read_to_string(dir.path().join("Items").join("Apple.md")).expect("read");
    assert!(!text.contains("Qty: 3"), "{text}");

    // Apple is now 2 x 7 = 14, Pear stays at 20.
    let query = ok(s
        .query_database(params(json!({
            "folder": "Items",
            "query": {"sorts": [{"property": "Total"}]},
        })))
        .await);
    assert_eq!(query["rows"][0]["title"], "Apple");
    assert_eq!(query["rows"][0]["values"]["Total"].as_f64(), Some(14.0));
    assert_eq!(query["rows"][1]["values"]["Total"].as_f64(), Some(20.0));

    // `null` clears a stored property.
    ok(s.set_row_property(params(json!({
        "path": "Items/Apple.md", "property": "Note", "value": "temporary",
    })))
    .await);
    ok(s.set_row_property(params(json!({
        "path": "Items/Apple.md", "property": "Note", "value": null,
    })))
    .await);
    let text = std::fs::read_to_string(dir.path().join("Items").join("Apple.md")).expect("read");
    assert!(!text.contains("Note"), "{text}");

    // Computed properties cannot be set.
    let computed = s
        .set_row_property(params(json!({
            "path": "Items/Apple.md", "property": "Total", "value": 1,
        })))
        .await;
    assert!(is_error(&computed));
}

#[tokio::test]
async fn invalid_schema_is_a_tool_error_and_writes_nothing() {
    let (dir, s) = server();
    items_db(&s).await;
    let schema_path = dir.path().join("Items").join("_schema.yaml");
    let before = std::fs::read_to_string(&schema_path).expect("read");

    let dup = s
        .update_database_schema(params(json!({
            "folder": "Items",
            "properties": [
                {"name": "Price", "type": "number"},
                {"name": "Price", "type": "text"}
            ],
        })))
        .await;
    assert!(is_error(&dup), "{}", text_of(&dup));
    assert!(text_of(&dup).contains("duplicate"), "{}", text_of(&dup));

    let bad_type = s
        .create_database(params(json!({
            "folder": "Other",
            "properties": [{"name": "X", "type": "hologram"}],
        })))
        .await;
    assert!(is_error(&bad_type));
    assert!(text_of(&bad_type).contains("invalid property 1"));

    let stale = s
        .update_database_schema(params(json!({
            "folder": "Items",
            "properties": props_of(item_props()),
            "revision": "not-the-revision",
        })))
        .await;
    assert!(is_error(&stale));
    assert!(text_of(&stale).contains("conflict"), "{}", text_of(&stale));

    assert_eq!(std::fs::read_to_string(&schema_path).expect("read"), before);
    assert!(!dir.path().join("Other").exists());
}

#[tokio::test]
async fn database_writes_are_audited_without_values() {
    let (dir, s) = server();
    let secret = "Walrus-confidential-note-text";
    items_db(&s).await;
    ok(s.create_row(params(json!({
        "folder": "Items",
        "title": "Secret row",
        "values": {"Note": secret, "Price": 1, "Qty": 1},
    })))
    .await);
    ok(s.set_row_property(params(json!({
        "path": "Items/Secret row.md", "property": "Note", "value": secret,
    })))
    .await);
    ok(s.update_database_schema(params(json!({
        "folder": "Items",
        "views": [{"name": "All", "layout": "table"}],
    })))
    .await);
    // A rejected schema is a logged failure, too.
    assert!(is_error(
        &s.update_database_schema(params(json!({
            "folder": "Items",
            "properties": [{"name": "A", "type": "text"}, {"name": "A", "type": "text"}],
        })))
        .await
    ));
    // Reads are not logged.
    ok(s.query_database(params(json!({"folder": "Items"}))).await);
    ok(s.get_database_schema(params(json!({"folder": "Items"})))
        .await);
    ok(s.list_databases().await);

    let entries = s.audit_entries(100).await.expect("audit");
    let tools: Vec<&str> = entries.iter().map(|e| e.tool.as_str()).collect();
    assert_eq!(
        tools,
        vec![
            "create_database",
            "create_row",
            "create_row",
            "create_row",
            "set_row_property",
            "update_database_schema",
            "update_database_schema",
        ]
    );
    assert!(!entries[6].ok);
    assert_eq!(entries[6].error.as_deref(), Some("invalid_arguments"));
    assert_eq!(entries[0].targets, vec!["Items/_schema.yaml".to_string()]);
    assert_eq!(entries[1].targets, vec!["Items/Apple.md".to_string()]);
    assert_eq!(entries[4].targets, vec!["Items/Secret row.md".to_string()]);
    assert!(entries[4].revision_before.is_some() && entries[4].revision_after.is_some());

    let audit_dir = dir.path().join(".selenite").join("audit");
    let mut raw = String::new();
    for file in std::fs::read_dir(&audit_dir).expect("audit dir") {
        raw.push_str(&std::fs::read_to_string(file.expect("entry").path()).expect("read"));
    }
    assert_eq!(raw.lines().count(), 7);
    assert!(!raw.contains(secret), "row value leaked into the audit log");
    assert!(!raw.contains("Note"), "property names must not leak either");
}

#[tokio::test]
async fn daily_note_tags_tree_and_archive() {
    let (dir, s) = server();
    let note = ok(s
        .daily_note(Parameters(DailyNoteParams {
            date: Some("2030-05-06".into()),
        }))
        .await);
    let note_path = note["path"].as_str().expect("path").to_string();
    assert!(note_path.ends_with("2030-05-06.md"), "{note_path}");
    assert!(note["text"].as_str().expect("text").contains("6 May 2030"));
    // Opening it again is a read: no second audit entry.
    ok(s.daily_note(Parameters(DailyNoteParams {
        date: Some("2030-05-06".into()),
    }))
    .await);
    assert_eq!(s.audit_entries(10).await.expect("audit").len(), 1);
    assert!(
        s.daily_note(Parameters(DailyNoteParams {
            date: Some("tomorrowish".into())
        }))
        .await
        .is_error
            == Some(true)
    );

    create(&s, "Tagged.md", "Text #alpha #beta\n").await;
    let tags = ok(s.list_tags().await);
    assert!(
        tags["tags"]
            .as_array()
            .expect("tags")
            .iter()
            .any(|t| t["tag"] == "alpha")
    );
    let tree = ok(s.get_tree().await);
    assert_eq!(tree["tree"]["is_dir"], true);

    let archived = ok(s.archive_page(path_param("Tagged.md")).await);
    assert!(
        archived["archived_to"]
            .as_str()
            .expect("trash path")
            .starts_with(".selenite/trash/")
    );
    assert!(!dir.path().join("Tagged.md").exists());
    assert!(is_error(&s.get_page(path_param("Tagged.md")).await));
}

#[tokio::test]
async fn audit_log_has_one_entry_per_write_and_no_content() {
    let (dir, s) = server();
    let secret = "Zebra-crossing-secret-body-text";
    let created = create(&s, "Log.md", &format!("# T\n\n{secret}\n")).await;
    ok(s.append_blocks(Parameters(AppendBlocksParams {
        path: "Log.md".into(),
        markdown: format!("another {secret}"),
        revision: None,
    }))
    .await);
    ok(s.update_block(Parameters(UpdateBlockParams {
        path: "Log.md".into(),
        block: "0".into(),
        markdown: format!("# Retitled {secret}"),
        revision: None,
    }))
    .await);
    let quick = "Buy quokka-secret-widgets today p2";
    ok(s.create_task(Parameters(CreateTaskParams {
        text: Some(quick.into()),
        title: None,
        priority: None,
        due: None,
        project: None,
        section: None,
        labels: None,
        reminders: None,
    }))
    .await);
    // A failing write is logged too.
    assert!(is_error(
        &s.append_blocks(Parameters(AppendBlocksParams {
            path: "Missing.md".into(),
            markdown: secret.into(),
            revision: None,
        }))
        .await
    ));
    // Reads are not logged.
    ok(s.get_page(path_param("Log.md")).await);
    ok(s.search(Parameters(SearchParams {
        query: "zebra".into(),
        limit: None,
    }))
    .await);
    ok(s.list_blocks(path_param("Log.md")).await);

    let entries = s.audit_entries(100).await.expect("audit");
    let tools: Vec<&str> = entries.iter().map(|e| e.tool.as_str()).collect();
    assert_eq!(
        tools,
        vec![
            "create_page",
            "append_blocks",
            "update_block",
            "create_task",
            "append_blocks"
        ]
    );
    assert!(entries[..4].iter().all(|e| e.ok));
    assert!(!entries[4].ok);
    assert_eq!(entries[0].targets, vec!["Log.md".to_string()]);
    assert_eq!(entries[0].revision_after, Some(rev(&created)));
    assert_eq!(entries[1].revision_before, entries[0].revision_after);
    assert_eq!(entries[1].revision_after, entries[2].revision_before);
    assert!(entries.iter().all(|e| e.args_digest.len() == 64));
    assert!(chrono::DateTime::parse_from_rfc3339(&entries[0].ts).is_ok());

    let audit_dir = dir.path().join(".selenite").join("audit");
    let files: Vec<_> = std::fs::read_dir(&audit_dir)
        .expect("audit dir")
        .map(|e| e.expect("entry").path())
        .collect();
    assert_eq!(files.len(), 1);
    let name = files[0]
        .file_name()
        .expect("name")
        .to_string_lossy()
        .to_string();
    assert!(
        name.starts_with("mcp-") && name.ends_with(".jsonl"),
        "{name}"
    );
    let raw = std::fs::read_to_string(&files[0]).expect("read log");
    assert_eq!(raw.lines().count(), 5);
    assert!(
        !raw.contains(secret),
        "page content leaked into the audit log"
    );
    assert!(!raw.contains(quick), "task text leaked into the audit log");
    assert!(!raw.contains("Retitled"));
}

#[tokio::test]
async fn there_is_no_delete_tool() {
    let (_d, s) = server();
    let names = s.tool_names();
    for name in &names {
        let lower = name.to_lowercase();
        assert!(
            !lower.contains("delete") && !lower.contains("remove") && !lower.contains("trash"),
            "destructive tool name: {name}"
        );
    }
    for expected in [
        "search",
        "get_page",
        "create_page",
        "append_blocks",
        "update_block",
        "list_blocks",
        "list_tasks",
        "create_task",
        "update_task",
        "complete_task",
        "reopen_task",
        "archive_task",
        "set_reminder",
        "get_agenda",
        "query_database",
        "daily_note",
        "list_tags",
        "get_tree",
        "archive_page",
        "list_databases",
        "get_database_schema",
        "create_database",
        "update_database_schema",
        "create_row",
        "set_row_property",
    ] {
        assert!(
            names.iter().any(|n| n == expected),
            "missing tool {expected}"
        );
    }
    assert_eq!(names.len(), 25);
}

#[tokio::test]
async fn tool_schemas_carry_parameter_descriptions() {
    let (_d, s) = server();
    let tools = s.tools();
    let append = tools
        .iter()
        .find(|t| t.name == "append_blocks")
        .expect("append_blocks");
    let schema = serde_json::to_string(&*append.input_schema).expect("schema");
    assert!(
        schema.contains("Markdown to add after the page's last block"),
        "{schema}"
    );
    assert!(schema.contains("revision"));
}

#[test]
fn resource_uris_round_trip() {
    let p = selenite_core::VaultPath::new("Notes/Plan & Ideas #1.md").expect("path");
    let uri = uri_for(&p);
    assert!(
        uri.starts_with("vault:///Notes/Plan%20%26%20Ideas%20%231.md"),
        "{uri}"
    );
    assert_eq!(path_from_uri(&uri), Some(p));
    assert_eq!(path_from_uri("file:///x.md"), None);
    assert_eq!(path_from_uri("vault:///../x.md"), None);
}
