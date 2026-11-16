//! View definitions: how a database is shown (layout plus query).

use serde::{Deserialize, Serialize};

use super::query::ViewQuery;

/// How rows are laid out.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Layout {
    /// Rows and columns.
    #[default]
    Table,
    /// Kanban columns by a select property (`group_by`).
    Board,
    /// Month grid by `date_property`.
    Calendar,
    /// Compact list.
    List,
    /// Cards with an optional cover (`cover_property`).
    Gallery,
    /// Bars from `date_property` to `end_date_property`.
    Timeline,
}

/// A saved view of a database table.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct ViewDef {
    /// Display name.
    pub name: String,
    /// Layout.
    #[serde(default)]
    pub layout: Layout,
    /// Filter, sort and grouping.
    #[serde(flatten)]
    pub query: ViewQuery,
    /// Property names to show, in order. Empty means all properties in schema order.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub visible: Vec<String>,
    /// Date property placing rows in calendar and timeline layouts.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub date_property: Option<String>,
    /// End date property of timeline bars.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub end_date_property: Option<String>,
    /// Property holding the gallery cover image path or URL.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub cover_property: Option<String>,
}

impl ViewDef {
    /// A view with the given layout and no filter, sort or grouping.
    pub fn new(name: impl Into<String>, layout: Layout) -> Self {
        Self {
            name: name.into(),
            layout,
            query: ViewQuery::default(),
            visible: Vec::new(),
            date_property: None,
            end_date_property: None,
            cover_property: None,
        }
    }

    /// Every property name this view refers to.
    pub fn referenced_properties(&self) -> Vec<&str> {
        let mut out: Vec<&str> = Vec::new();
        out.extend(self.query.group_by.as_deref());
        out.extend(self.query.sorts.iter().map(|s| s.property.as_str()));
        out.extend(self.visible.iter().map(String::as_str));
        out.extend(self.date_property.as_deref());
        out.extend(self.end_date_property.as_deref());
        out.extend(self.cover_property.as_deref());
        if let Some(f) = &self.query.filter {
            collect_filter(f, &mut out);
        }
        out
    }
}

fn collect_filter<'a>(g: &'a super::query::FilterGroup, out: &mut Vec<&'a str>) {
    for node in &g.rules {
        match node {
            super::query::FilterNode::Rule(r) => out.push(r.property.as_str()),
            super::query::FilterNode::Group(inner) => collect_filter(inner, out),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::database::query::SortRule;

    #[test]
    fn referenced_properties_cover_every_field() {
        let mut v = ViewDef::new("V", Layout::Timeline);
        v.query.group_by = Some("G".into());
        v.query.sorts.push(SortRule {
            property: "S".into(),
            descending: false,
        });
        v.visible = vec!["A".into()];
        v.date_property = Some("D".into());
        v.end_date_property = Some("E".into());
        v.cover_property = Some("C".into());
        assert_eq!(
            v.referenced_properties(),
            vec!["G", "S", "A", "D", "E", "C"]
        );
    }

    #[test]
    fn layout_defaults_to_table() {
        assert_eq!(Layout::default(), Layout::Table);
        assert_eq!(ViewDef::new("x", Layout::default()).layout, Layout::Table);
    }
}
