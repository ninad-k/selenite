//! Workspace-mode database model: typed properties, formulas, rollups, queries and views.
//!
//! A database is a folder holding `_schema.yaml` plus one Markdown file per row (ADR-002). This
//! module is the pure-logic part: it parses and evaluates, and never reads files or the clock.
//! Callers pass `today`/`now` explicitly.

pub mod formula;
pub mod property;
pub mod query;
pub mod rollup;
pub mod value;
pub mod view;

use serde::{Deserialize, Serialize};

pub use formula::{Formula, FormulaContext, FormulaError, formula_order};
pub use property::{PropertyDef, PropertyType, RollupFn, SelectOption};
pub use query::{
    Condition, FilterGroup, FilterNode, FilterRule, GroupOp, NO_VALUE_LABEL, RowGroup, RowValues,
    SortRule, ViewQuery, condition_matches, group_rows, matches, sort_rows,
};
pub use rollup::rollup;
pub use value::Value;
pub use view::{Layout, ViewDef};

/// The typed shape of a table database: its properties and views.
#[derive(Debug, Clone, PartialEq, Default, Serialize, Deserialize)]
pub struct DatabaseDef {
    /// Columns, in display order.
    #[serde(default)]
    pub properties: Vec<PropertyDef>,
    /// Saved views.
    #[serde(default)]
    pub views: Vec<ViewDef>,
}

impl DatabaseDef {
    /// The first title property (the row name column).
    pub fn title_property(&self) -> Option<&PropertyDef> {
        self.properties
            .iter()
            .find(|p| matches!(p.kind, PropertyType::Title))
    }

    /// Look a property up by exact name.
    pub fn property(&self, name: &str) -> Option<&PropertyDef> {
        self.properties.iter().find(|p| p.name == name)
    }

    /// Formula property names in dependency order; see [`formula_order`].
    pub fn formula_order(&self) -> Result<Vec<String>, FormulaError> {
        formula_order(&self.properties)
    }

    /// Check the definition and describe every problem found (empty when valid): duplicate or
    /// blank property names, more than one title, formula syntax errors and cycles, rollups whose
    /// relation is missing or not a relation, and views naming unknown properties.
    pub fn validate(&self) -> Vec<String> {
        let mut problems = Vec::new();
        let mut seen: Vec<&str> = Vec::new();
        for p in &self.properties {
            if p.name.trim().is_empty() {
                problems.push("a property has an empty name".to_string());
            } else if seen.contains(&p.name.as_str()) {
                problems.push(format!("duplicate property name `{}`", p.name));
            } else {
                seen.push(&p.name);
            }
        }
        let titles = self
            .properties
            .iter()
            .filter(|p| matches!(p.kind, PropertyType::Title))
            .count();
        if titles > 1 {
            problems.push(format!("{titles} title properties (only one is allowed)"));
        }

        let mut parse_ok = true;
        for p in &self.properties {
            match &p.kind {
                PropertyType::Formula { expression } => {
                    if let Err(e) = Formula::parse(expression) {
                        parse_ok = false;
                        problems.push(format!("formula `{}`: {e}", p.name));
                    }
                }
                PropertyType::Rollup { relation, .. } => match self.property(relation) {
                    None => problems.push(format!(
                        "rollup `{}` refers to missing relation `{relation}`",
                        p.name
                    )),
                    Some(r) if !matches!(r.kind, PropertyType::Relation { .. }) => {
                        problems.push(format!(
                            "rollup `{}`: `{relation}` is not a relation property",
                            p.name
                        ))
                    }
                    Some(_) => {}
                },
                _ => {}
            }
        }
        if parse_ok {
            if let Err(FormulaError::Cycle(path)) = formula_order(&self.properties) {
                problems.push(format!("formula cycle: {}", path.join(" -> ")));
            }
        }

        for v in &self.views {
            for name in v.referenced_properties() {
                if self.property(name).is_none() {
                    problems.push(format!(
                        "view `{}` refers to unknown property `{name}`",
                        v.name
                    ));
                }
            }
        }
        problems
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn formula(name: &str, expr: &str) -> PropertyDef {
        PropertyDef::new(
            name,
            PropertyType::Formula {
                expression: expr.into(),
            },
        )
    }

    fn base() -> DatabaseDef {
        DatabaseDef {
            properties: vec![
                PropertyDef::new("Name", PropertyType::Title),
                PropertyDef::new("Price", PropertyType::Number { format: None }),
                PropertyDef::new(
                    "Client",
                    PropertyType::Relation {
                        database: "Clients".into(),
                    },
                ),
            ],
            views: vec![],
        }
    }

    #[test]
    fn lookups() {
        let db = base();
        assert_eq!(db.title_property().map(|p| p.name.as_str()), Some("Name"));
        assert!(db.property("Price").is_some());
        assert!(db.property("price").is_none());
    }

    #[test]
    fn valid_definition_has_no_problems() {
        let mut db = base();
        db.properties.push(formula("Double", "prop(\"Price\") * 2"));
        db.properties.push(PropertyDef::new(
            "N",
            PropertyType::Rollup {
                relation: "Client".into(),
                property: "Name".into(),
                function: RollupFn::Count,
            },
        ));
        let mut view = ViewDef::new("All", Layout::Table);
        view.query.group_by = Some("Name".into());
        db.views.push(view);
        assert_eq!(db.validate(), Vec::<String>::new());
        assert_eq!(db.formula_order().unwrap(), vec!["Double"]);
    }

    #[test]
    fn reports_duplicates_and_titles() {
        let mut db = base();
        db.properties
            .push(PropertyDef::new("Price", PropertyType::Text));
        db.properties
            .push(PropertyDef::new("Other", PropertyType::Title));
        let problems = db.validate();
        assert!(
            problems
                .iter()
                .any(|p| p.contains("duplicate property name `Price`"))
        );
        assert!(problems.iter().any(|p| p.contains("title properties")));
    }

    #[test]
    fn reports_bad_rollups() {
        let mut db = base();
        let rollup = |name: &str, relation: &str| {
            PropertyDef::new(
                name,
                PropertyType::Rollup {
                    relation: relation.into(),
                    property: "Name".into(),
                    function: RollupFn::Count,
                },
            )
        };
        db.properties.push(rollup("R1", "Nope"));
        db.properties.push(rollup("R2", "Price"));
        let problems = db.validate();
        assert!(
            problems
                .iter()
                .any(|p| p.contains("missing relation `Nope`"))
        );
        assert!(problems.iter().any(|p| p.contains("not a relation")));
    }

    #[test]
    fn reports_formula_errors_and_cycles() {
        let mut db = base();
        db.properties.push(formula("Bad", "1 +"));
        assert!(db.validate().iter().any(|p| p.contains("formula `Bad`")));

        let mut db = base();
        db.properties.push(formula("A", "prop(\"B\")"));
        db.properties.push(formula("B", "prop(\"A\")"));
        assert!(db.validate().iter().any(|p| p.contains("formula cycle")));
    }

    #[test]
    fn reports_unknown_view_properties() {
        let mut db = base();
        let mut v = ViewDef::new("V", Layout::Board);
        v.query.group_by = Some("Status".into());
        db.views.push(v);
        assert!(
            db.validate()
                .iter()
                .any(|p| p.contains("unknown property `Status`"))
        );
    }
}
