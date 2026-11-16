//! Property definitions: the typed columns of a database, stored in `_schema.yaml`.

use serde::{Deserialize, Serialize};

use super::value::Value;

/// One choice of a select or multi-select property.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct SelectOption {
    /// Display name (also the value stored in front matter).
    pub name: String,
    /// Optional colour name or hex code.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub color: Option<String>,
}

impl SelectOption {
    /// An option without a colour.
    pub fn new(name: impl Into<String>) -> Self {
        Self {
            name: name.into(),
            color: None,
        }
    }
}

/// How a rollup folds the related rows' values; see [`rollup`](super::rollup::rollup).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum RollupFn {
    /// Number of related rows.
    Count,
    /// Number of related rows whose value is not empty.
    CountValues,
    /// Sum of the numeric values.
    Sum,
    /// Mean of the numeric values.
    Average,
    /// Smallest number (or earliest date).
    Min,
    /// Largest number (or latest date).
    Max,
    /// All non-empty values as a list.
    ShowOriginal,
    /// Share of checked values, 0 to 100.
    PercentChecked,
    /// Earliest date.
    Earliest,
    /// Latest date.
    Latest,
}

/// The type of a property, tagged `type: ...` in YAML (lowercase snake case).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum PropertyType {
    /// The row's name (its file name and first column). One per database.
    Title,
    /// Free text.
    Text,
    /// A number with an optional display format (for example `usd` or `percent`).
    Number {
        /// Display format hint.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        format: Option<String>,
    },
    /// One choice from a list.
    Select {
        /// Allowed choices, in display order.
        #[serde(default)]
        options: Vec<SelectOption>,
    },
    /// Several choices from a list.
    MultiSelect {
        /// Allowed choices, in display order.
        #[serde(default)]
        options: Vec<SelectOption>,
    },
    /// A date, optionally with a time.
    Date,
    /// A checkbox.
    Checkbox,
    /// A link.
    Url,
    /// Links to rows of another database.
    Relation {
        /// Vault folder of the target database.
        database: String,
    },
    /// A value summarised from related rows. Computed, not stored.
    Rollup {
        /// Name of a relation property in this database.
        relation: String,
        /// Property of the related database to summarise.
        property: String,
        /// How to fold the values.
        function: RollupFn,
    },
    /// A computed value; see [`Formula`](super::formula::Formula). Computed, not stored.
    Formula {
        /// Formula source text.
        expression: String,
    },
    /// When the row was created. Computed from file metadata, not stored.
    CreatedTime,
    /// When the row was last edited. Computed from file metadata, not stored.
    UpdatedTime,
}

/// A named, typed property (one column).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct PropertyDef {
    /// Property name; the front matter key.
    pub name: String,
    /// Type and type-specific settings.
    #[serde(flatten)]
    pub kind: PropertyType,
}

impl PropertyDef {
    /// Build a definition.
    pub fn new(name: impl Into<String>, kind: PropertyType) -> Self {
        Self {
            name: name.into(),
            kind,
        }
    }

    /// True for rollup, formula and created/updated time: values derived rather than typed in.
    pub fn is_computed(&self) -> bool {
        matches!(
            self.kind,
            PropertyType::Rollup { .. }
                | PropertyType::Formula { .. }
                | PropertyType::CreatedTime
                | PropertyType::UpdatedTime
        )
    }

    /// The options of a select or multi-select property.
    pub fn options(&self) -> &[SelectOption] {
        match &self.kind {
            PropertyType::Select { options } | PropertyType::MultiSelect { options } => options,
            _ => &[],
        }
    }

    /// Normalise a raw front matter value to this property's type.
    ///
    /// - title, text, url: text (`Null` stays `Null`); lists are joined with `, `
    /// - number: numeric text becomes a number, anything else `Null`
    /// - select: trimmed text (the first element of a list); blank is `Null`
    /// - multi-select: a list of trimmed names; text is split on commas; `Null` is an empty list
    /// - date: a date or date-time (ISO text is parsed), else `Null`
    /// - checkbox: a boolean (`true/yes/x/1` and friends); anything else is unchecked
    /// - relation: a list of target names with `[[...]]` brackets and `|alias` stripped; a text
    ///   containing links yields every link, plain text is a single target; `Null` is an empty list
    /// - rollup, formula: returned unchanged (they are computed elsewhere)
    /// - created/updated time: a date-time, else `Null`
    pub fn coerce(&self, raw: &Value) -> Value {
        self.kind.coerce(raw)
    }
}

impl PropertyType {
    /// See [`PropertyDef::coerce`].
    pub fn coerce(&self, raw: &Value) -> Value {
        match self {
            PropertyType::Title | PropertyType::Text | PropertyType::Url => coerce_text(raw),
            PropertyType::Number { .. } => raw.as_number().map_or(Value::Null, Value::Number),
            PropertyType::Select { .. } => {
                let first = match raw {
                    Value::List(items) => items.first().unwrap_or(&Value::Null),
                    other => other,
                };
                match coerce_text(first) {
                    Value::Text(s) if !s.trim().is_empty() => Value::Text(s.trim().to_string()),
                    _ => Value::Null,
                }
            }
            PropertyType::MultiSelect { .. } => {
                let mut out = Vec::new();
                collect_names(raw, true, &mut out);
                Value::List(out.into_iter().map(Value::Text).collect())
            }
            PropertyType::Date => match raw {
                Value::Date(_) | Value::DateTime(_) => raw.clone(),
                Value::Text(s) => super::value::parse_datetime(s)
                    .map(Value::DateTime)
                    .or_else(|| super::value::parse_date(s).map(Value::Date))
                    .unwrap_or(Value::Null),
                _ => Value::Null,
            },
            PropertyType::CreatedTime | PropertyType::UpdatedTime => {
                raw.as_datetime().map_or(Value::Null, Value::DateTime)
            }
            PropertyType::Checkbox => Value::Bool(raw.as_bool().unwrap_or(false)),
            PropertyType::Relation { .. } => {
                let mut out = Vec::new();
                collect_links(raw, &mut out);
                Value::List(out.into_iter().map(Value::Text).collect())
            }
            PropertyType::Rollup { .. } | PropertyType::Formula { .. } => raw.clone(),
        }
    }
}

fn coerce_text(raw: &Value) -> Value {
    match raw {
        Value::Null => Value::Null,
        other => Value::Text(other.as_text()),
    }
}

fn collect_names(raw: &Value, split_commas: bool, out: &mut Vec<String>) {
    match raw {
        Value::List(items) => {
            for item in items {
                collect_names(item, false, out);
            }
        }
        Value::Null => {}
        other => {
            let text = other.as_text();
            if split_commas {
                out.extend(
                    text.split(',')
                        .map(str::trim)
                        .filter(|s| !s.is_empty())
                        .map(str::to_string),
                );
            } else {
                let t = text.trim();
                if !t.is_empty() {
                    out.push(t.to_string());
                }
            }
        }
    }
}

fn collect_links(raw: &Value, out: &mut Vec<String>) {
    match raw {
        Value::List(items) => items.iter().for_each(|i| collect_links(i, out)),
        Value::Null => {}
        other => {
            let text = other.as_text();
            if text.contains("[[") {
                let mut rest = text.as_str();
                while let Some(start) = rest.find("[[") {
                    let after = &rest[start + 2..];
                    let Some(end) = after.find("]]") else { break };
                    let target = after[..end].split('|').next().unwrap_or("").trim();
                    if !target.is_empty() {
                        out.push(target.to_string());
                    }
                    rest = &after[end + 2..];
                }
            } else {
                let t = text.trim();
                if !t.is_empty() {
                    out.push(t.to_string());
                }
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use chrono::NaiveDate;

    fn num() -> PropertyType {
        PropertyType::Number { format: None }
    }

    fn list(items: &[&str]) -> Value {
        Value::List(items.iter().map(|s| Value::from(*s)).collect())
    }

    #[test]
    fn number_coercion() {
        assert_eq!(num().coerce(&"12".into()), Value::Number(12.0));
        assert_eq!(num().coerce(&"abc".into()), Value::Null);
        assert_eq!(num().coerce(&Value::Null), Value::Null);
    }

    #[test]
    fn text_and_select_coercion() {
        assert_eq!(PropertyType::Text.coerce(&Value::Number(4.0)), "4".into());
        assert_eq!(PropertyType::Text.coerce(&Value::Null), Value::Null);
        let sel = PropertyType::Select { options: vec![] };
        assert_eq!(sel.coerce(&" Todo ".into()), "Todo".into());
        assert_eq!(sel.coerce(&list(&["A", "B"])), "A".into());
        assert_eq!(sel.coerce(&"  ".into()), Value::Null);
    }

    #[test]
    fn multi_select_coercion() {
        let ms = PropertyType::MultiSelect { options: vec![] };
        assert_eq!(ms.coerce(&"x, y".into()), list(&["x", "y"]));
        assert_eq!(ms.coerce(&list(&["x", " y "])), list(&["x", "y"]));
        assert_eq!(ms.coerce(&Value::Null), list(&[]));
    }

    #[test]
    fn relation_coercion() {
        let rel = PropertyType::Relation {
            database: "Clients".into(),
        };
        assert_eq!(rel.coerce(&"[[Acme]]".into()), list(&["Acme"]));
        assert_eq!(rel.coerce(&"Acme".into()), list(&["Acme"]));
        assert_eq!(
            rel.coerce(&"[[Acme|the client]] and [[Beta]]".into()),
            list(&["Acme", "Beta"])
        );
        assert_eq!(rel.coerce(&list(&["[[A]]", "B"])), list(&["A", "B"]));
        assert_eq!(rel.coerce(&Value::Null), list(&[]));
    }

    #[test]
    fn checkbox_coercion() {
        assert_eq!(
            PropertyType::Checkbox.coerce(&"yes".into()),
            Value::Bool(true)
        );
        assert_eq!(
            PropertyType::Checkbox.coerce(&"true".into()),
            Value::Bool(true)
        );
        assert_eq!(
            PropertyType::Checkbox.coerce(&Value::Null),
            Value::Bool(false)
        );
        assert_eq!(
            PropertyType::Checkbox.coerce(&"zzz".into()),
            Value::Bool(false)
        );
    }

    #[test]
    fn date_coercion() {
        let day = NaiveDate::from_ymd_opt(2026, 5, 6).unwrap();
        assert_eq!(
            PropertyType::Date.coerce(&"2026-05-06".into()),
            Value::Date(day)
        );
        assert!(matches!(
            PropertyType::Date.coerce(&"2026-05-06T09:30".into()),
            Value::DateTime(_)
        ));
        assert_eq!(PropertyType::Date.coerce(&"soon".into()), Value::Null);
        assert!(matches!(
            PropertyType::CreatedTime.coerce(&Value::Date(day)),
            Value::DateTime(_)
        ));
    }

    #[test]
    fn computed_values_pass_through() {
        let f = PropertyType::Formula {
            expression: "1".into(),
        };
        assert_eq!(f.coerce(&Value::Number(1.0)), Value::Number(1.0));
        assert!(PropertyDef::new("F", f).is_computed());
        assert!(!PropertyDef::new("T", PropertyType::Title).is_computed());
    }
}
