//! Table databases through the engine.

#![allow(missing_docs, clippy::unwrap_used, clippy::expect_used, clippy::panic)]

use std::collections::BTreeMap;

use chrono::{NaiveDate, NaiveDateTime};
use selenite_core::database::{
    Condition, FilterGroup, FilterNode, FilterRule, GroupOp, Layout, PropertyDef, PropertyType,
    RollupFn, SelectOption, SortRule, Value, ViewDef, ViewQuery,
};
use selenite_index::Engine;
use serde_json::json;

fn now() -> NaiveDateTime {
    NaiveDate::from_ymd_opt(2026, 11, 16)
        .unwrap()
        .and_hms_opt(10, 0, 0)
        .unwrap()
}

fn vals(pairs: &[(&str, serde_json::Value)]) -> BTreeMap<String, serde_json::Value> {
    pairs
        .iter()
        .map(|(k, v)| (k.to_string(), v.clone()))
        .collect()
}

fn setup() -> (tempfile::TempDir, Engine) {
    let dir = tempfile::tempdir().unwrap();
    let mut e = Engine::init_local(dir.path()).unwrap();
    e.create_database(
        "Clients",
        vec![PropertyDef::new("Country", PropertyType::Text)],
    )
    .unwrap();
    e.create_database(
        "Orders",
        vec![
            PropertyDef::new(
                "Status",
                PropertyType::Select {
                    options: vec![
                        SelectOption::new("Todo"),
                        SelectOption::new("Doing"),
                        SelectOption::new("Done"),
                    ],
                },
            ),
            PropertyDef::new("Price", PropertyType::Number { format: None }),
            PropertyDef::new("Qty", PropertyType::Number { format: None }),
            PropertyDef::new(
                "Total",
                PropertyType::Formula {
                    expression: "prop(\"Price\") * prop(\"Qty\")".into(),
                },
            ),
            PropertyDef::new(
                "Client",
                PropertyType::Relation {
                    database: "Clients".into(),
                },
            ),
            PropertyDef::new("Due", PropertyType::Date),
            PropertyDef::new("Paid", PropertyType::Checkbox),
        ],
    )
    .unwrap();
    e.create_row("Clients", "Acme", &vals(&[("Country", json!("NZ"))]), now())
        .unwrap();
    e.create_row(
        "Orders",
        "Widgets",
        &vals(&[
            ("Status", json!("Doing")),
            ("Price", json!(2.5)),
            ("Qty", json!(4)),
            ("Client", json!("Acme")),
            ("Due", json!("2026-11-20")),
        ]),
        now(),
    )
    .unwrap();
    e.create_row(
        "Orders",
        "Gears",
        &vals(&[
            ("Status", json!("Todo")),
            ("Price", json!(10)),
            ("Qty", json!(1)),
            ("Client", json!(["Acme"])),
            ("Paid", json!(true)),
        ]),
        now(),
    )
    .unwrap();
    (dir, e)
}

#[test]
fn rows_are_plain_readable_files() {
    let (dir, e) = setup();
    let text = std::fs::read_to_string(dir.path().join("Orders/Widgets.md")).unwrap();
    assert!(text.starts_with("---\n"), "{text}");
    assert!(text.contains("Status: Doing"), "{text}");
    assert!(text.contains("Client:"), "{text}");
    assert!(text.contains("[[Acme]]"), "{text}");
    assert!(
        !text.contains("Total"),
        "computed values are never written: {text}"
    );
    let schema = std::fs::read_to_string(dir.path().join("Orders/_schema.yaml")).unwrap();
    assert!(schema.contains("kind: table"), "{schema}");
    let dbs: Vec<String> = e
        .databases()
        .unwrap()
        .into_iter()
        .map(|d| d.folder)
        .collect();
    assert_eq!(dbs, vec!["Clients", "Orders"]);
    // Relation links are real wikilinks: the client page gets backlinks.
    let back = e
        .backlinks(&selenite_core::VaultPath::new("Clients/Acme.md").unwrap())
        .unwrap();
    assert_eq!(back.len(), 2);
}

#[test]
fn formulas_relations_and_rollups_compute() {
    let (_dir, mut e) = setup();
    let mut schema = e.database_schema("Clients").unwrap();
    schema.properties.push(PropertyDef::new(
        "Orders",
        PropertyType::Relation {
            database: "Orders".into(),
        },
    ));
    schema.properties.push(PropertyDef::new(
        "Order total",
        PropertyType::Rollup {
            relation: "Orders".into(),
            property: "Price".into(),
            function: RollupFn::Sum,
        },
    ));
    e.save_database_schema("Clients", &schema, None).unwrap();
    let acme = selenite_core::VaultPath::new("Clients/Acme.md").unwrap();
    e.set_row_property(&acme, "Orders", &json!(["Widgets", "Gears"]))
        .unwrap();
    let clients = e.query_database_view("Clients", None, None, now()).unwrap();
    assert_eq!(clients.rows[0].values["Order total"], Value::Number(12.5));
    let orders = e.query_database_view("Orders", None, None, now()).unwrap();
    let widgets = orders.rows.iter().find(|r| r.title == "Widgets").unwrap();
    assert_eq!(widgets.values["Total"], Value::Number(10.0));
}

#[test]
fn views_filter_sort_and_group() {
    let (_dir, mut e) = setup();
    let mut schema = e.database_schema("Orders").unwrap();
    let mut board = ViewDef::new("Board", Layout::Board);
    board.query = ViewQuery {
        filter: None,
        sorts: vec![SortRule {
            property: "Total".into(),
            descending: true,
        }],
        group_by: Some("Status".into()),
    };
    let mut unpaid = ViewDef::new("Unpaid", Layout::Table);
    unpaid.query.filter = Some(FilterGroup {
        op: GroupOp::And,
        rules: vec![FilterNode::Rule(FilterRule {
            property: "Paid".into(),
            condition: Condition::Unchecked,
        })],
    });
    schema.table_views.push(board);
    schema.table_views.push(unpaid);
    e.save_database_schema("Orders", &schema, None).unwrap();

    let r = e
        .query_database_view("Orders", Some("board"), None, now())
        .unwrap();
    let titles: Vec<&str> = r.rows.iter().map(|r| r.title.as_str()).collect();
    assert_eq!(titles, vec!["Gears", "Widgets"]);
    let groups = r.groups.unwrap();
    let labels: Vec<&str> = groups.iter().map(|g| g.label.as_str()).collect();
    assert_eq!(labels, vec!["Todo", "Doing", "Done"]);
    assert_eq!(groups[0].rows.len(), 1);
    assert!(groups[2].rows.is_empty());

    let r = e
        .query_database_view("Orders", Some("Unpaid"), None, now())
        .unwrap();
    let titles: Vec<&str> = r.rows.iter().map(|r| r.title.as_str()).collect();
    assert_eq!(titles, vec!["Widgets"]);
    assert!(
        e.query_database_view("Orders", Some("nope"), None, now())
            .is_err()
    );
}

#[test]
fn editing_values_keeps_body_and_rejects_computed() {
    let (dir, mut e) = setup();
    let p = selenite_core::VaultPath::new("Orders/Gears.md").unwrap();
    let (text, rev) = e.read_page(&p).unwrap();
    e.save_page(&p, &format!("{text}Notes about gears.\n"), Some(rev))
        .unwrap();
    e.set_row_property(&p, "Status", &json!("Done")).unwrap();
    e.set_row_property(&p, "Qty", &json!(null)).unwrap();
    let after = std::fs::read_to_string(dir.path().join("Orders/Gears.md")).unwrap();
    assert!(after.contains("Status: Done"), "{after}");
    assert!(!after.contains("Qty"), "{after}");
    assert!(after.ends_with("Notes about gears.\n"), "{after}");
    assert!(e.set_row_property(&p, "Total", &json!(1)).is_err());
    assert!(e.set_row_property(&p, "Missing", &json!(1)).is_err());
    // Renaming the title property writes a front matter title.
    e.set_row_property(&p, "Name", &json!("Big gears")).unwrap();
    let r = e.query_database_view("Orders", None, None, now()).unwrap();
    assert!(r.rows.iter().any(|r| r.title == "Big gears"));
}

#[test]
fn invalid_schemas_are_rejected() {
    let (_dir, mut e) = setup();
    let mut schema = e.database_schema("Orders").unwrap();
    schema.properties.push(PropertyDef::new(
        "Loop",
        PropertyType::Formula {
            expression: "prop(\"Loop\") + 1".into(),
        },
    ));
    assert!(e.save_database_schema("Orders", &schema, None).is_err());
    assert!(e.create_database("Orders", vec![]).is_err());
    assert!(e.query_database_view("Tasks", None, None, now()).is_err());
}

#[test]
fn block_editing_is_conditional_and_byte_stable() {
    use selenite_index::BlockEdit;
    let (dir, mut e) = setup();
    let p = selenite_core::VaultPath::new("Notes.md").unwrap();
    let original = "---\nicon: \"🌙\"\n---\n# Notes\r\n\r\nFirst <script>x</script> [[Acme]]\r\n\r\n- [ ] todo\r\n";
    e.create_page(&p, original).unwrap();
    let page = e.page_blocks(&p, "http://vault.localhost/").unwrap();
    assert_eq!(page.icon.as_deref(), Some("🌙"));
    assert_eq!(page.blocks.len(), 3);
    assert!(!page.blocks[1].html.contains("<script>"));
    assert!(
        page.blocks[1]
            .html
            .contains("data-path=\"Clients/Acme.md\"")
    );
    let rev = selenite_storage::Revision(page.revision.clone());
    let rev2 = e
        .edit_block(
            &p,
            &BlockEdit::ToggleTask {
                block: "2".into(),
                line: 0,
            },
            rev.clone(),
        )
        .unwrap();
    // Stale revision is refused.
    assert!(
        e.edit_block(&p, &BlockEdit::Delete { block: "0".into() }, rev)
            .is_err()
    );
    e.edit_block(
        &p,
        &BlockEdit::Insert {
            after: Some("0".into()),
            markdown: "Inserted".into(),
        },
        rev2,
    )
    .unwrap();
    let after = std::fs::read_to_string(dir.path().join("Notes.md")).unwrap();
    assert_eq!(
        after,
        "---\nicon: \"🌙\"\n---\n# Notes\r\n\r\nInserted\r\n\r\nFirst <script>x</script> [[Acme]]\r\n\r\n- [x] todo\r\n"
    );
}
