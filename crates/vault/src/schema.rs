//! Database folders: a folder containing [`SCHEMA_FILE`] plus one Markdown file per row.

use selenite_core::database::{PropertyDef, ViewDef};
use serde::{Deserialize, Serialize};

use crate::VaultError;

/// File name that turns a folder into a database.
pub const SCHEMA_FILE: &str = "_schema.yaml";

/// What kind of rows a database holds.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, Default)]
#[serde(rename_all = "lowercase")]
pub enum DatabaseKind {
    /// Generic rows with user-defined properties (Workspace mode databases).
    #[default]
    Table,
    /// Task rows (front matter keys from `selenite_core::TASK_KEYS`).
    Tasks,
}

/// A named, saved filter over a database.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct SavedView {
    /// Display name.
    pub name: String,
    /// Filter expression (task filter language for task databases).
    pub filter: String,
    /// Layout hint: `list`, `board`, `table`, `calendar`, `gallery`, `timeline`.
    #[serde(default = "default_layout")]
    pub layout: String,
}

fn default_layout() -> String {
    "list".into()
}

/// Contents of `_schema.yaml`. Unknown keys are ignored so newer files stay readable.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct DatabaseSchema {
    /// Row kind.
    pub kind: DatabaseKind,
    /// Schema format version.
    pub version: u32,
    /// Saved views.
    #[serde(skip_serializing_if = "Vec::is_empty")]
    pub views: Vec<SavedView>,
    /// Typed properties of a table database (Workspace mode).
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub properties: Vec<PropertyDef>,
    /// Structured views of a table database (layout, filter, sort, grouping).
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub table_views: Vec<ViewDef>,
}

impl Default for DatabaseSchema {
    fn default() -> Self {
        Self {
            kind: DatabaseKind::Table,
            version: 1,
            views: Vec::new(),
            properties: Vec::new(),
            table_views: Vec::new(),
        }
    }
}

impl DatabaseSchema {
    /// A new task database schema.
    pub fn tasks() -> Self {
        Self {
            kind: DatabaseKind::Tasks,
            ..Self::default()
        }
    }

    /// A new table database schema with the given properties.
    pub fn table(properties: Vec<PropertyDef>) -> Self {
        Self {
            kind: DatabaseKind::Table,
            properties,
            ..Self::default()
        }
    }

    /// The typed definition (properties and structured views) of a table database.
    pub fn definition(&self) -> selenite_core::database::DatabaseDef {
        selenite_core::database::DatabaseDef {
            properties: self.properties.clone(),
            views: self.table_views.clone(),
        }
    }

    /// Parse a schema file. An empty document yields the defaults.
    pub fn from_yaml(text: &str) -> Result<Self, VaultError> {
        if text.trim().is_empty() {
            return Ok(Self::default());
        }
        serde_norway::from_str(text).map_err(|e| VaultError::Config(e.to_string()))
    }

    /// Serialise to YAML.
    pub fn to_yaml(&self) -> Result<String, VaultError> {
        serde_norway::to_string(self).map_err(|e| VaultError::Config(e.to_string()))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn round_trip_and_defaults() {
        let mut s = DatabaseSchema::tasks();
        s.views.push(SavedView {
            name: "Urgent".into(),
            filter: "p1 & open".into(),
            layout: "board".into(),
        });
        let y = s.to_yaml().unwrap();
        assert_eq!(DatabaseSchema::from_yaml(&y).unwrap(), s);
        assert_eq!(
            DatabaseSchema::from_yaml("").unwrap().kind,
            DatabaseKind::Table
        );
        let partial = DatabaseSchema::from_yaml("kind: tasks\nfuture_key: 1\n").unwrap();
        assert_eq!(partial.kind, DatabaseKind::Tasks);
        let v = DatabaseSchema::from_yaml("views:\n  - name: A\n    filter: today\n").unwrap();
        assert_eq!(v.views[0].layout, "list");
    }

    const TABLE_YAML: &str = r#"kind: table
version: 1
properties:
  - name: Name
    type: title
  - name: Status
    type: select
    options: [{name: Todo}, {name: Doing}, {name: Done}]
  - name: Price
    type: number
  - name: Qty
    type: number
  - name: Total
    type: formula
    expression: prop("Price") * prop("Qty")
  - name: Client
    type: relation
    database: Clients
  - name: Client count
    type: rollup
    relation: Client
    property: Name
    function: count
table_views:
  - name: Board
    layout: board
    group_by: Status
    sorts: [{property: Total, descending: true}]
"#;

    #[test]
    fn table_schema_parses_and_round_trips() {
        use selenite_core::database::{Layout, PropertyType, RollupFn};
        let s = DatabaseSchema::from_yaml(TABLE_YAML).unwrap();
        assert_eq!(s.kind, DatabaseKind::Table);
        assert_eq!(s.properties.len(), 7);
        assert_eq!(s.properties[0].kind, PropertyType::Title);
        assert_eq!(s.properties[1].options().len(), 3);
        assert!(matches!(
            &s.properties[6].kind,
            PropertyType::Rollup { function: RollupFn::Count, relation, .. } if relation == "Client"
        ));
        assert_eq!(s.table_views[0].layout, Layout::Board);
        assert_eq!(s.table_views[0].query.group_by.as_deref(), Some("Status"));
        assert!(s.table_views[0].query.sorts[0].descending);
        assert!(s.definition().validate().is_empty());

        let y = s.to_yaml().unwrap();
        assert_eq!(DatabaseSchema::from_yaml(&y).unwrap(), s);
    }

    #[test]
    fn filters_survive_a_yaml_round_trip() {
        use selenite_core::database::{
            Condition, FilterGroup, FilterNode, FilterRule, GroupOp, Layout, Value, ViewDef,
        };
        let rule = |p: &str, c: Condition| {
            FilterNode::Rule(FilterRule {
                property: p.into(),
                condition: c,
            })
        };
        let mut view = ViewDef::new("Open", Layout::Calendar);
        view.date_property = Some("Due".into());
        view.query.filter = Some(FilterGroup {
            op: GroupOp::And,
            rules: vec![
                rule("Status", Condition::IsNot("Done".into())),
                rule("Price", Condition::Gt(Value::Number(9.5))),
                rule("Done", Condition::Unchecked),
                FilterNode::Group(FilterGroup {
                    op: GroupOp::Or,
                    rules: vec![
                        rule("Due", Condition::WithinNextDays(7)),
                        rule(
                            "Due",
                            Condition::Before(chrono::NaiveDate::from_ymd_opt(2026, 1, 1).unwrap()),
                        ),
                        rule("Name", Condition::IsEmpty),
                    ],
                }),
            ],
        });
        let mut s = DatabaseSchema::table(vec![]);
        s.table_views.push(view);
        let y = s.to_yaml().unwrap();
        assert!(y.contains("op: is_not"), "{y}");
        assert_eq!(DatabaseSchema::from_yaml(&y).unwrap(), s);
    }

    #[test]
    fn task_schema_omits_table_keys() {
        let y = DatabaseSchema::tasks().to_yaml().unwrap();
        assert!(!y.contains("properties") && !y.contains("table_views"));
    }
}
