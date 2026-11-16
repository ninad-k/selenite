//! Structured view queries: filter groups, sorting and grouping over rows.
//!
//! Queries are data (they live in `_schema.yaml`), not strings. A filter rule in YAML reads
//! `{property: Status, op: is, value: Done}`; a group is `{op: or, rules: [...]}`.

use std::cmp::Ordering;

use chrono::{Days, NaiveDate};
use serde::{Deserialize, Serialize};

use super::property::PropertyDef;
use super::value::Value;

/// Label of the group holding rows with no value.
pub const NO_VALUE_LABEL: &str = "No value";

/// How the rules of a group combine.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum GroupOp {
    /// Every rule must match.
    #[default]
    And,
    /// At least one rule must match.
    Or,
}

/// A test applied to one property value. Serialised as `op: <name>` plus `value: ...` when the
/// condition has an argument.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(tag = "op", content = "value", rename_all = "snake_case")]
pub enum Condition {
    /// Equals the value (forgiving: see [`Value::loose_eq`]; a list matches if any item does).
    Is(Value),
    /// Does not equal the value.
    IsNot(Value),
    /// Text contains the substring (case-insensitive); a list matches if any item does.
    Contains(String),
    /// Negation of [`Condition::Contains`].
    DoesNotContain(String),
    /// Text starts with the prefix (case-insensitive).
    StartsWith(String),
    /// Text ends with the suffix (case-insensitive).
    EndsWith(String),
    /// Greater than (numbers, dates or text).
    Gt(Value),
    /// Greater than or equal.
    Ge(Value),
    /// Less than.
    Lt(Value),
    /// Less than or equal.
    Le(Value),
    /// No value (null, blank text, empty list).
    IsEmpty,
    /// Has a value.
    IsNotEmpty,
    /// Date strictly before the day.
    Before(NaiveDate),
    /// Date strictly after the day.
    After(NaiveDate),
    /// Date on or before the day.
    OnOrBefore(NaiveDate),
    /// Date on or after the day.
    OnOrAfter(NaiveDate),
    /// Date within the last N days up to and including today.
    WithinPastDays(u32),
    /// Date from today up to and including N days ahead.
    WithinNextDays(u32),
    /// Checkbox is checked.
    Checked,
    /// Checkbox is not checked (also true for missing values).
    Unchecked,
}

/// One property test.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct FilterRule {
    /// Property name.
    pub property: String,
    /// The test.
    #[serde(flatten)]
    pub condition: Condition,
}

/// A rule or a nested group.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(untagged)]
pub enum FilterNode {
    /// A nested group.
    Group(FilterGroup),
    /// A single rule.
    Rule(FilterRule),
}

/// Rules combined with AND or OR. An empty group matches every row.
#[derive(Debug, Clone, PartialEq, Default, Serialize, Deserialize)]
pub struct FilterGroup {
    /// How the rules combine.
    #[serde(default)]
    pub op: GroupOp,
    /// Rules and nested groups.
    #[serde(default)]
    pub rules: Vec<FilterNode>,
}

/// One sort key.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct SortRule {
    /// Property name.
    pub property: String,
    /// Largest first when true.
    #[serde(default)]
    pub descending: bool,
}

/// Filter, sort and grouping of a view.
#[derive(Debug, Clone, PartialEq, Default, Serialize, Deserialize)]
pub struct ViewQuery {
    /// Optional filter.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub filter: Option<FilterGroup>,
    /// Sort keys, most significant first.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub sorts: Vec<SortRule>,
    /// Property to group by (board columns, grouped tables).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub group_by: Option<String>,
}

/// Read access to a row's property values by name.
pub trait RowValues {
    /// The value of the property, or `Null` when the row has none.
    fn value(&self, property: &str) -> Value;
}

fn text_lower(v: &Value) -> String {
    v.as_text().to_lowercase()
}

/// Apply `f` to the text of a value, or to each item of a list (true if any item matches).
fn any_text(v: &Value, f: &dyn Fn(&str) -> bool) -> bool {
    match v {
        Value::List(items) => items.iter().any(|i| f(&text_lower(i))),
        other => f(&text_lower(other)),
    }
}

/// Evaluate one condition against a value. `today` anchors the relative date conditions.
pub fn condition_matches(cond: &Condition, v: &Value, today: NaiveDate) -> bool {
    let date = v.as_date();
    let ord = |other: &Value| v.loose_cmp(other);
    match cond {
        Condition::Is(x) => v.loose_eq(x),
        Condition::IsNot(x) => !v.loose_eq(x),
        Condition::Contains(s) => {
            let s = s.to_lowercase();
            any_text(v, &|t| t.contains(&s))
        }
        Condition::DoesNotContain(s) => {
            !condition_matches(&Condition::Contains(s.clone()), v, today)
        }
        Condition::StartsWith(s) => {
            let s = s.to_lowercase();
            any_text(v, &|t| t.starts_with(&s))
        }
        Condition::EndsWith(s) => {
            let s = s.to_lowercase();
            any_text(v, &|t| t.ends_with(&s))
        }
        Condition::Gt(x) => ord(x) == Some(Ordering::Greater),
        Condition::Ge(x) => matches!(ord(x), Some(Ordering::Greater | Ordering::Equal)),
        Condition::Lt(x) => ord(x) == Some(Ordering::Less),
        Condition::Le(x) => matches!(ord(x), Some(Ordering::Less | Ordering::Equal)),
        Condition::IsEmpty => v.is_empty(),
        Condition::IsNotEmpty => !v.is_empty(),
        Condition::Before(d) => date.is_some_and(|x| x < *d),
        Condition::After(d) => date.is_some_and(|x| x > *d),
        Condition::OnOrBefore(d) => date.is_some_and(|x| x <= *d),
        Condition::OnOrAfter(d) => date.is_some_and(|x| x >= *d),
        Condition::WithinPastDays(n) => date.is_some_and(|x| {
            x <= today
                && today
                    .checked_sub_days(Days::new(u64::from(*n)))
                    .is_some_and(|s| x >= s)
        }),
        Condition::WithinNextDays(n) => date.is_some_and(|x| {
            x >= today
                && today
                    .checked_add_days(Days::new(u64::from(*n)))
                    .is_some_and(|e| x <= e)
        }),
        Condition::Checked => v.as_bool() == Some(true),
        Condition::Unchecked => v.as_bool() != Some(true),
    }
}

/// Whether the row satisfies the group. An empty group matches.
pub fn matches(group: &FilterGroup, row: &dyn RowValues, today: NaiveDate) -> bool {
    if group.rules.is_empty() {
        return true;
    }
    let test = |node: &FilterNode| match node {
        FilterNode::Rule(r) => condition_matches(&r.condition, &row.value(&r.property), today),
        FilterNode::Group(g) => matches(g, row, today),
    };
    match group.op {
        GroupOp::And => group.rules.iter().all(test),
        GroupOp::Or => group.rules.iter().any(test),
    }
}

/// Stable sort by the rules, most significant first. Empty values go last whichever the direction.
pub fn sort_rows<R: RowValues>(rows: &mut [R], sorts: &[SortRule]) {
    if sorts.is_empty() {
        return;
    }
    rows.sort_by(|a, b| {
        for rule in sorts {
            let (x, y) = (a.value(&rule.property), b.value(&rule.property));
            let ord = match (x.is_empty(), y.is_empty()) {
                (true, true) => Ordering::Equal,
                (true, false) => return Ordering::Greater,
                (false, true) => return Ordering::Less,
                (false, false) => {
                    let o = x.compare(&y);
                    if rule.descending { o.reverse() } else { o }
                }
            };
            if ord != Ordering::Equal {
                return ord;
            }
        }
        Ordering::Equal
    });
}

/// One group of rows (by index into the input slice).
#[derive(Debug, Clone, PartialEq)]
pub struct RowGroup {
    /// Display label; [`NO_VALUE_LABEL`] for rows without a value.
    pub label: String,
    /// The group's value; `Null` for the no-value group.
    pub value: Value,
    /// Indices of the rows in the group, in input order.
    pub rows: Vec<usize>,
}

/// Group rows by a property.
///
/// For select and multi-select properties (pass the definition) every option gets a group in option
/// order, even when empty; values outside the options follow, sorted. Other properties produce one
/// group per distinct value, sorted with [`Value::compare`]. A multi-select (list) value puts the
/// row in each of its items' groups. Rows without a value form a final [`NO_VALUE_LABEL`] group,
/// present only when some row has no value.
pub fn group_rows<R: RowValues>(
    rows: &[R],
    property: &str,
    def: Option<&PropertyDef>,
) -> Vec<RowGroup> {
    let options = def.map_or(&[][..], PropertyDef::options);
    let mut groups: Vec<RowGroup> = options
        .iter()
        .map(|o| RowGroup {
            label: o.name.clone(),
            value: Value::Text(o.name.clone()),
            rows: Vec::new(),
        })
        .collect();
    let fixed = groups.len();
    let mut none = Vec::new();

    for (i, row) in rows.iter().enumerate() {
        let v = row.value(property);
        let keys: Vec<Value> = match v {
            Value::List(items) => items.into_iter().filter(|x| !x.is_empty()).collect(),
            other if other.is_empty() => Vec::new(),
            other => vec![other],
        };
        if keys.is_empty() {
            none.push(i);
            continue;
        }
        for key in keys {
            let slot = groups[..fixed]
                .iter()
                .position(|g| g.value.loose_eq(&key))
                .or_else(|| {
                    groups[fixed..]
                        .iter()
                        .position(|g| g.value.compare(&key) == Ordering::Equal)
                        .map(|p| p + fixed)
                });
            let idx = match slot {
                Some(p) => p,
                None => {
                    groups.push(RowGroup {
                        label: key.as_text(),
                        value: key,
                        rows: Vec::new(),
                    });
                    groups.len() - 1
                }
            };
            if groups[idx].rows.last() != Some(&i) {
                groups[idx].rows.push(i);
            }
        }
    }
    groups[fixed..].sort_by(|a, b| a.value.compare(&b.value));
    if !none.is_empty() {
        groups.push(RowGroup {
            label: NO_VALUE_LABEL.to_string(),
            value: Value::Null,
            rows: none,
        });
    }
    groups
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::database::property::{PropertyType, SelectOption};
    use std::collections::HashMap;

    #[derive(Debug, Clone)]
    struct Row(HashMap<String, Value>);

    impl RowValues for Row {
        fn value(&self, property: &str) -> Value {
            self.0.get(property).cloned().unwrap_or(Value::Null)
        }
    }

    fn row(pairs: &[(&str, Value)]) -> Row {
        Row(pairs
            .iter()
            .map(|(k, v)| (k.to_string(), v.clone()))
            .collect())
    }

    fn day(y: i32, m: u32, d: u32) -> NaiveDate {
        NaiveDate::from_ymd_opt(y, m, d).unwrap()
    }

    fn check(cond: Condition, v: Value) -> bool {
        condition_matches(&cond, &v, day(2026, 6, 15))
    }

    #[test]
    fn equality_conditions() {
        assert!(check(Condition::Is("done".into()), "Done".into()));
        assert!(!check(Condition::Is("done".into()), "Todo".into()));
        assert!(check(Condition::IsNot("done".into()), "Todo".into()));
        assert!(check(
            Condition::Is("b".into()),
            Value::List(vec!["a".into(), "B".into()])
        ));
        assert!(check(Condition::Is(Value::Number(3.0)), "3".into()));
    }

    #[test]
    fn text_conditions() {
        assert!(check(Condition::Contains("ell".into()), "Hello".into()));
        assert!(check(
            Condition::DoesNotContain("xyz".into()),
            "Hello".into()
        ));
        assert!(!check(
            Condition::DoesNotContain("ell".into()),
            "Hello".into()
        ));
        assert!(check(Condition::StartsWith("he".into()), "Hello".into()));
        assert!(check(Condition::EndsWith("LO".into()), "Hello".into()));
        assert!(check(
            Condition::Contains("ur".into()),
            Value::List(vec!["blue".into(), "urgent".into()])
        ));
    }

    #[test]
    fn comparison_conditions() {
        assert!(check(Condition::Gt(Value::Number(2.0)), Value::Number(3.0)));
        assert!(check(Condition::Ge(Value::Number(3.0)), Value::Number(3.0)));
        assert!(check(Condition::Lt(Value::Number(3.0)), Value::Number(2.0)));
        assert!(check(Condition::Le(Value::Number(3.0)), Value::Number(3.0)));
        assert!(!check(Condition::Gt(Value::Number(3.0)), Value::Null));
        assert!(check(
            Condition::Gt(Value::Date(day(2026, 1, 1))),
            Value::Date(day(2026, 2, 1))
        ));
    }

    #[test]
    fn emptiness_and_checkbox_conditions() {
        assert!(check(Condition::IsEmpty, Value::Null));
        assert!(check(Condition::IsEmpty, Value::List(vec![])));
        assert!(check(Condition::IsNotEmpty, "x".into()));
        assert!(check(Condition::Checked, Value::Bool(true)));
        assert!(!check(Condition::Checked, Value::Null));
        assert!(check(Condition::Unchecked, Value::Null));
        assert!(check(Condition::Unchecked, Value::Bool(false)));
    }

    #[test]
    fn date_conditions() {
        let v = Value::Date(day(2026, 6, 15));
        assert!(check(Condition::Before(day(2026, 6, 16)), v.clone()));
        assert!(!check(Condition::Before(day(2026, 6, 15)), v.clone()));
        assert!(check(Condition::After(day(2026, 6, 14)), v.clone()));
        assert!(check(Condition::OnOrBefore(day(2026, 6, 15)), v.clone()));
        assert!(check(Condition::OnOrAfter(day(2026, 6, 15)), v.clone()));
        assert!(!check(Condition::After(day(2026, 6, 15)), Value::Null));
    }

    #[test]
    fn relative_date_conditions() {
        let within_past =
            |n: u32, d: NaiveDate| check(Condition::WithinPastDays(n), Value::Date(d));
        assert!(within_past(7, day(2026, 6, 8)));
        assert!(within_past(7, day(2026, 6, 15)));
        assert!(!within_past(7, day(2026, 6, 7)));
        assert!(!within_past(7, day(2026, 6, 16)));
        let within_next =
            |n: u32, d: NaiveDate| check(Condition::WithinNextDays(n), Value::Date(d));
        assert!(within_next(3, day(2026, 6, 18)));
        assert!(!within_next(3, day(2026, 6, 19)));
        assert!(!within_next(3, day(2026, 6, 14)));
    }

    fn rule(property: &str, condition: Condition) -> FilterNode {
        FilterNode::Rule(FilterRule {
            property: property.into(),
            condition,
        })
    }

    #[test]
    fn nested_groups() {
        // Status = Doing AND (Price > 10 OR Tags contains rush)
        let g = FilterGroup {
            op: GroupOp::And,
            rules: vec![
                rule("Status", Condition::Is("Doing".into())),
                FilterNode::Group(FilterGroup {
                    op: GroupOp::Or,
                    rules: vec![
                        rule("Price", Condition::Gt(Value::Number(10.0))),
                        rule("Tags", Condition::Contains("rush".into())),
                    ],
                }),
            ],
        };
        let today = day(2026, 6, 15);
        let a = row(&[("Status", "Doing".into()), ("Price", Value::Number(20.0))]);
        let b = row(&[
            ("Status", "Doing".into()),
            ("Price", Value::Number(1.0)),
            ("Tags", Value::List(vec!["rush".into()])),
        ]);
        let c = row(&[("Status", "Doing".into()), ("Price", Value::Number(1.0))]);
        let d = row(&[("Status", "Done".into()), ("Price", Value::Number(99.0))]);
        assert!(matches(&g, &a, today));
        assert!(matches(&g, &b, today));
        assert!(!matches(&g, &c, today));
        assert!(!matches(&g, &d, today));
        assert!(matches(&FilterGroup::default(), &c, today));
    }

    #[test]
    fn sort_with_nulls_and_direction() {
        let mk = |n: &str, v: Value| row(&[("N", n.into()), ("V", v)]);
        let mut rows = vec![
            mk("a", Value::Number(2.0)),
            mk("b", Value::Null),
            mk("c", Value::Number(1.0)),
            mk("d", Value::Number(2.0)),
        ];
        let asc = [SortRule {
            property: "V".into(),
            descending: false,
        }];
        sort_rows(&mut rows, &asc);
        let names = |r: &[Row]| r.iter().map(|x| x.value("N").as_text()).collect::<String>();
        assert_eq!(names(&rows), "cadb");
        let desc = [SortRule {
            property: "V".into(),
            descending: true,
        }];
        sort_rows(&mut rows, &desc);
        assert_eq!(names(&rows), "adcb");
        let two = [
            SortRule {
                property: "V".into(),
                descending: false,
            },
            SortRule {
                property: "N".into(),
                descending: true,
            },
        ];
        sort_rows(&mut rows, &two);
        assert_eq!(names(&rows), "cdab");
    }

    fn select_def() -> PropertyDef {
        PropertyDef::new(
            "Status",
            PropertyType::Select {
                options: vec![
                    SelectOption::new("Todo"),
                    SelectOption::new("Doing"),
                    SelectOption::new("Done"),
                ],
            },
        )
    }

    #[test]
    fn group_by_select_follows_option_order() {
        let rows = vec![
            row(&[("Status", "Done".into())]),
            row(&[]),
            row(&[("Status", "todo".into())]),
            row(&[("Status", "Weird".into())]),
            row(&[("Status", "Done".into())]),
        ];
        let groups = group_rows(&rows, "Status", Some(&select_def()));
        let labels: Vec<&str> = groups.iter().map(|g| g.label.as_str()).collect();
        assert_eq!(labels, ["Todo", "Doing", "Done", "Weird", NO_VALUE_LABEL]);
        assert_eq!(groups[0].rows, vec![2]);
        assert!(groups[1].rows.is_empty());
        assert_eq!(groups[2].rows, vec![0, 4]);
        assert_eq!(groups[4].rows, vec![1]);
    }

    #[test]
    fn group_by_other_sorts_distinct_values() {
        let rows = vec![
            row(&[("N", Value::Number(3.0))]),
            row(&[("N", Value::Number(1.0))]),
            row(&[("N", Value::Number(3.0))]),
        ];
        let groups = group_rows(&rows, "N", None);
        let labels: Vec<&str> = groups.iter().map(|g| g.label.as_str()).collect();
        assert_eq!(labels, ["1", "3"]);
        assert_eq!(groups[1].rows, vec![0, 2]);
    }

    #[test]
    fn group_multi_select_repeats_rows() {
        let rows = vec![
            row(&[("T", Value::List(vec!["b".into(), "a".into()]))]),
            row(&[("T", Value::List(vec!["a".into()]))]),
        ];
        let groups = group_rows(&rows, "T", None);
        assert_eq!(groups.len(), 2);
        assert_eq!(groups[0].label, "a");
        assert_eq!(groups[0].rows, vec![0, 1]);
        assert_eq!(groups[1].rows, vec![0]);
    }
}
