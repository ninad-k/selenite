//! Markdown documents with YAML front matter, byte-stable on round trip.

use std::fmt;

use serde::de::{self, MapAccess, SeqAccess, Visitor};
use serde::ser::{SerializeMap, SerializeSeq};
use serde::{Deserialize, Deserializer, Serialize, Serializer};

use crate::VaultError;
use crate::links::{extract_headings, extract_tags};

const BOM: char = '\u{feff}';

/// A front matter property value.
///
/// Serialises as plain JSON/YAML-style data (`Map` becomes a map, not a list of pairs).
#[derive(Debug, Clone, PartialEq)]
pub enum PropertyValue {
    /// YAML `null` / empty value.
    Null,
    /// Boolean.
    Bool(bool),
    /// Any number.
    Number(f64),
    /// String.
    Text(String),
    /// Sequence.
    List(Vec<PropertyValue>),
    /// Mapping, preserving key order.
    Map(Vec<(String, PropertyValue)>),
}

impl PropertyValue {
    /// The string if this is [`PropertyValue::Text`].
    pub fn as_str(&self) -> Option<&str> {
        match self {
            PropertyValue::Text(s) => Some(s),
            _ => None,
        }
    }

    /// Scalars as strings: `Text` gives one item, `List` gives each scalar item, others none.
    pub fn as_list_of_strings(&self) -> Vec<String> {
        fn scalar(v: &PropertyValue) -> Option<String> {
            match v {
                PropertyValue::Text(s) => Some(s.clone()),
                PropertyValue::Number(n) => Some(number_text(*n)),
                PropertyValue::Bool(b) => Some(b.to_string()),
                _ => None,
            }
        }
        match self {
            PropertyValue::List(items) => items.iter().filter_map(scalar).collect(),
            other => scalar(other).into_iter().collect(),
        }
    }
}

fn number_text(n: f64) -> String {
    if n.fract() == 0.0 && n.abs() < 1e15 {
        format!("{n:.0}")
    } else {
        n.to_string()
    }
}

impl Serialize for PropertyValue {
    fn serialize<S: Serializer>(&self, s: S) -> Result<S::Ok, S::Error> {
        match self {
            PropertyValue::Null => s.serialize_unit(),
            PropertyValue::Bool(b) => s.serialize_bool(*b),
            PropertyValue::Number(n) => {
                if n.fract() == 0.0 && n.abs() < 1e15 {
                    s.serialize_i64(*n as i64)
                } else {
                    s.serialize_f64(*n)
                }
            }
            PropertyValue::Text(t) => s.serialize_str(t),
            PropertyValue::List(items) => {
                let mut seq = s.serialize_seq(Some(items.len()))?;
                for i in items {
                    seq.serialize_element(i)?;
                }
                seq.end()
            }
            PropertyValue::Map(entries) => {
                let mut map = s.serialize_map(Some(entries.len()))?;
                for (k, v) in entries {
                    map.serialize_entry(k, v)?;
                }
                map.end()
            }
        }
    }
}

struct PvVisitor;

impl<'de> Visitor<'de> for PvVisitor {
    type Value = PropertyValue;

    fn expecting(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str("a property value")
    }
    fn visit_unit<E: de::Error>(self) -> Result<Self::Value, E> {
        Ok(PropertyValue::Null)
    }
    fn visit_none<E: de::Error>(self) -> Result<Self::Value, E> {
        Ok(PropertyValue::Null)
    }
    fn visit_some<D: Deserializer<'de>>(self, d: D) -> Result<Self::Value, D::Error> {
        PropertyValue::deserialize(d)
    }
    fn visit_bool<E: de::Error>(self, v: bool) -> Result<Self::Value, E> {
        Ok(PropertyValue::Bool(v))
    }
    fn visit_i64<E: de::Error>(self, v: i64) -> Result<Self::Value, E> {
        Ok(PropertyValue::Number(v as f64))
    }
    fn visit_u64<E: de::Error>(self, v: u64) -> Result<Self::Value, E> {
        Ok(PropertyValue::Number(v as f64))
    }
    fn visit_f64<E: de::Error>(self, v: f64) -> Result<Self::Value, E> {
        Ok(PropertyValue::Number(v))
    }
    fn visit_str<E: de::Error>(self, v: &str) -> Result<Self::Value, E> {
        Ok(PropertyValue::Text(v.to_string()))
    }
    fn visit_string<E: de::Error>(self, v: String) -> Result<Self::Value, E> {
        Ok(PropertyValue::Text(v))
    }
    fn visit_seq<A: SeqAccess<'de>>(self, mut a: A) -> Result<Self::Value, A::Error> {
        let mut items = Vec::new();
        while let Some(v) = a.next_element()? {
            items.push(v);
        }
        Ok(PropertyValue::List(items))
    }
    fn visit_map<A: MapAccess<'de>>(self, mut a: A) -> Result<Self::Value, A::Error> {
        let mut entries = Vec::new();
        while let Some((k, v)) = a.next_entry::<String, PropertyValue>()? {
            entries.push((k, v));
        }
        Ok(PropertyValue::Map(entries))
    }
}

impl<'de> Deserialize<'de> for PropertyValue {
    fn deserialize<D: Deserializer<'de>>(d: D) -> Result<Self, D::Error> {
        d.deserialize_any(PvVisitor)
    }
}

/// Ordered key -> value map preserving file order.
#[derive(Debug, Clone, PartialEq, Default)]
pub struct FrontMatter {
    entries: Vec<(String, PropertyValue)>,
}

impl FrontMatter {
    /// Iterate entries in file order.
    pub fn iter(&self) -> impl Iterator<Item = (&str, &PropertyValue)> {
        self.entries.iter().map(|(k, v)| (k.as_str(), v))
    }

    /// Look up a value by key.
    pub fn get(&self, key: &str) -> Option<&PropertyValue> {
        self.entries.iter().find(|(k, _)| k == key).map(|(_, v)| v)
    }

    /// True if there are no properties.
    pub fn is_empty(&self) -> bool {
        self.entries.is_empty()
    }

    /// Number of properties.
    pub fn len(&self) -> usize {
        self.entries.len()
    }
}

/// A parsed Markdown file: optional BOM, optional front matter block, body.
#[derive(Debug, Clone, PartialEq)]
pub struct Document {
    bom: bool,
    /// Whole front matter block including both fence lines and their line endings.
    block: Option<String>,
    front_matter: FrontMatter,
    fm_error: Option<String>,
    body: String,
}

/// Locate a front matter block at the start of `text`.
/// Returns `(block_len, yaml_start, yaml_end)` byte offsets.
fn find_front_matter(text: &str) -> Option<(usize, usize, usize)> {
    fn is_fence(line: &str, fences: &[&str]) -> bool {
        fences.contains(&line.trim_end_matches([' ', '\t', '\r']))
    }
    let first_end = text.find('\n')?;
    if !is_fence(&text[..first_end], &["---"]) {
        return None;
    }
    let yaml_start = first_end + 1;
    let mut pos = yaml_start;
    loop {
        let line_end = text[pos..].find('\n').map(|i| pos + i);
        let line = &text[pos..line_end.unwrap_or(text.len())];
        if is_fence(line, &["---", "..."]) {
            return Some((line_end.map_or(text.len(), |e| e + 1), yaml_start, pos));
        }
        pos = line_end? + 1;
    }
}

fn yaml_to_value(v: &serde_norway::Value) -> Result<PropertyValue, String> {
    use serde_norway::Value;
    Ok(match v {
        Value::Null => PropertyValue::Null,
        Value::Bool(b) => PropertyValue::Bool(*b),
        Value::Number(n) => PropertyValue::Number(n.as_f64().unwrap_or(f64::NAN)),
        Value::String(s) => PropertyValue::Text(s.clone()),
        Value::Sequence(seq) => PropertyValue::List(
            seq.iter()
                .map(yaml_to_value)
                .collect::<Result<Vec<_>, _>>()?,
        ),
        Value::Mapping(m) => PropertyValue::Map(mapping_entries(m)?),
        Value::Tagged(t) => yaml_to_value(&t.value)?,
    })
}

fn mapping_entries(m: &serde_norway::Mapping) -> Result<Vec<(String, PropertyValue)>, String> {
    use serde_norway::Value;
    m.iter()
        .map(|(k, v)| {
            let key = match k {
                Value::String(s) => s.clone(),
                Value::Number(n) => n.to_string(),
                Value::Bool(b) => b.to_string(),
                Value::Null => "null".to_string(),
                _ => return Err("unsupported mapping key".to_string()),
            };
            Ok((key, yaml_to_value(v)?))
        })
        .collect()
}

fn value_to_yaml(v: &PropertyValue) -> serde_norway::Value {
    use serde_norway::{Number, Value};
    match v {
        PropertyValue::Null => Value::Null,
        PropertyValue::Bool(b) => Value::Bool(*b),
        PropertyValue::Number(n) => {
            if n.fract() == 0.0 && n.abs() < 1e15 {
                Value::Number(Number::from(*n as i64))
            } else {
                Value::Number(Number::from(*n))
            }
        }
        PropertyValue::Text(s) => Value::String(s.clone()),
        PropertyValue::List(items) => Value::Sequence(items.iter().map(value_to_yaml).collect()),
        PropertyValue::Map(entries) => Value::Mapping(entries_to_mapping(entries)),
    }
}

fn entries_to_mapping(entries: &[(String, PropertyValue)]) -> serde_norway::Mapping {
    let mut m = serde_norway::Mapping::new();
    for (k, v) in entries {
        m.insert(serde_norway::Value::String(k.clone()), value_to_yaml(v));
    }
    m
}

fn parse_yaml(yaml: &str) -> Result<FrontMatter, VaultError> {
    let value: serde_norway::Value =
        serde_norway::from_str(yaml).map_err(|e| VaultError::FrontMatter(e.to_string()))?;
    match value {
        serde_norway::Value::Null => Ok(FrontMatter::default()),
        serde_norway::Value::Mapping(m) => Ok(FrontMatter {
            entries: mapping_entries(&m).map_err(VaultError::FrontMatter)?,
        }),
        _ => Err(VaultError::FrontMatter(
            "front matter must be a mapping".to_string(),
        )),
    }
}

impl Document {
    /// Parse a Markdown file. Never fails: invalid YAML front matter is kept verbatim (raw) and
    /// reported via [`Document::front_matter_error`]; properties are then empty.
    pub fn parse(text: &str) -> Document {
        let (bom, rest) = match text.strip_prefix(BOM) {
            Some(r) => (true, r),
            None => (false, text),
        };
        let Some((block_len, ys, ye)) = find_front_matter(rest) else {
            return Document {
                bom,
                block: None,
                front_matter: FrontMatter::default(),
                fm_error: None,
                body: rest.to_string(),
            };
        };
        let (front_matter, fm_error) = match parse_yaml(&rest[ys..ye]) {
            Ok(fm) => (fm, None),
            Err(e) => (FrontMatter::default(), Some(e.to_string())),
        };
        Document {
            bom,
            block: Some(rest[..block_len].to_string()),
            front_matter,
            fm_error,
            body: rest[block_len..].to_string(),
        }
    }

    fn prefix_len(&self) -> usize {
        usize::from(self.bom) * BOM.len_utf8() + self.block.as_ref().map_or(0, String::len)
    }

    /// Serialise back. For an untouched document this equals the original input byte-for-byte.
    pub fn to_text(&self) -> String {
        let mut out = String::with_capacity(self.prefix_len() + self.body.len());
        if self.bom {
            out.push(BOM);
        }
        if let Some(b) = &self.block {
            out.push_str(b);
        }
        out.push_str(&self.body);
        out
    }

    /// Text after the closing front matter fence line (the whole file if there is none).
    pub fn body(&self) -> &str {
        &self.body
    }

    /// Byte offset of the body within [`Document::to_text`].
    pub fn body_offset(&self) -> usize {
        self.prefix_len()
    }

    /// Replace the body; the front matter block is untouched.
    pub fn set_body(&mut self, body: &str) {
        self.body = body.to_string();
    }

    /// Parsed front matter (empty if absent or invalid).
    pub fn front_matter(&self) -> &FrontMatter {
        &self.front_matter
    }

    /// Why the front matter could not be parsed, if it could not.
    pub fn front_matter_error(&self) -> Option<&str> {
        self.fm_error.as_deref()
    }

    /// Look up a property.
    pub fn property(&self, key: &str) -> Option<&PropertyValue> {
        self.front_matter.get(key)
    }

    fn newline(&self) -> &'static str {
        let crlf = match &self.block {
            Some(b) => b.find('\n').is_some_and(|i| b[..i].ends_with('\r')),
            None => self
                .body
                .find('\n')
                .is_some_and(|i| self.body[..i].ends_with('\r')),
        };
        if crlf { "\r\n" } else { "\n" }
    }

    fn rewrite_block(&mut self) {
        let nl = self.newline();
        let yaml = if self.front_matter.is_empty() {
            String::new()
        } else {
            let m = entries_to_mapping(&self.front_matter.entries);
            serde_norway::to_string(&serde_norway::Value::Mapping(m)).unwrap_or_default()
        };
        let yaml = if nl == "\r\n" {
            yaml.replace('\n', "\r\n")
        } else {
            yaml
        };
        self.block = Some(format!("---{nl}{yaml}---{nl}"));
    }

    /// Set or replace a property, rewriting only the front matter block (created if absent).
    /// Body bytes are untouched. Does nothing if the existing front matter is invalid YAML, so
    /// that the user's raw text is never destroyed.
    pub fn set_property(&mut self, key: &str, value: PropertyValue) {
        if self.fm_error.is_some() {
            return;
        }
        match self.front_matter.entries.iter_mut().find(|(k, _)| k == key) {
            Some((_, v)) => *v = value,
            None => self.front_matter.entries.push((key.to_string(), value)),
        }
        self.rewrite_block();
    }

    /// Remove a property. Returns true if it existed. Same invalid-YAML rule as
    /// [`Document::set_property`].
    pub fn remove_property(&mut self, key: &str) -> bool {
        if self.fm_error.is_some() {
            return false;
        }
        let before = self.front_matter.entries.len();
        self.front_matter.entries.retain(|(k, _)| k != key);
        let removed = self.front_matter.entries.len() != before;
        if removed {
            self.rewrite_block();
        }
        removed
    }

    /// `title` property, else the first `# H1` in the body, else `None`.
    pub fn title(&self) -> Option<String> {
        if let Some(t) = self.property("title").and_then(PropertyValue::as_str) {
            let t = t.trim();
            if !t.is_empty() {
                return Some(t.to_string());
            }
        }
        extract_headings(&self.body)
            .into_iter()
            .find(|h| h.level == 1 && !h.text.is_empty())
            .map(|h| h.text)
    }

    /// Union of front matter `tags` (string or list; leading `#` stripped) and inline body
    /// tags, deduplicated in order of first appearance.
    pub fn tags(&self) -> Vec<String> {
        let mut out: Vec<String> = Vec::new();
        let mut push = |t: String| {
            if !t.is_empty() && !out.contains(&t) {
                out.push(t);
            }
        };
        if let Some(v) = self.property("tags") {
            let raw = match v {
                PropertyValue::Text(s) => s
                    .split(|c: char| c == ',' || c.is_whitespace())
                    .map(str::to_string)
                    .collect(),
                other => other.as_list_of_strings(),
            };
            for t in raw {
                push(t.trim().trim_start_matches('#').to_string());
            }
        }
        for t in extract_tags(&self.body) {
            push(t);
        }
        out
    }

    /// Front matter `aliases` (or `alias`), as a string or list.
    pub fn aliases(&self) -> Vec<String> {
        self.property("aliases")
            .or_else(|| self.property("alias"))
            .map(|v| {
                v.as_list_of_strings()
                    .into_iter()
                    .map(|s| s.trim().to_string())
                    .filter(|s| !s.is_empty())
                    .collect()
            })
            .unwrap_or_default()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parse_basic() {
        let d = Document::parse("---\ntitle: Hi\ntags: [a, b]\n---\n# Body\n");
        assert_eq!(d.body(), "# Body\n");
        assert_eq!(d.body_offset(), "---\ntitle: Hi\ntags: [a, b]\n---\n".len());
        assert_eq!(
            d.property("title").and_then(PropertyValue::as_str),
            Some("Hi")
        );
        assert_eq!(d.front_matter().len(), 2);
        assert_eq!(d.title().as_deref(), Some("Hi"));
        assert!(d.front_matter_error().is_none());
    }

    #[test]
    fn body_offset_with_bom() {
        let t = "\u{feff}---\na: 1\n---\nx";
        let d = Document::parse(t);
        assert_eq!(&d.to_text()[d.body_offset()..], "x");
        assert_eq!(&t[d.body_offset()..], "x");
    }

    #[test]
    fn title_falls_back_to_h1() {
        let d = Document::parse("intro\n\n# Real Title\n## Sub\n");
        assert_eq!(d.title().as_deref(), Some("Real Title"));
        assert_eq!(Document::parse("no heading").title(), None);
    }

    #[test]
    fn tags_merge() {
        let d = Document::parse("---\ntags: [\"#a\", b]\n---\ntext #b #c\n");
        assert_eq!(d.tags(), vec!["a", "b", "c"]);
        let d = Document::parse("---\ntags: x, y z\n---\n");
        assert_eq!(d.tags(), vec!["x", "y", "z"]);
    }

    #[test]
    fn aliases_forms() {
        assert_eq!(
            Document::parse("---\naliases: [A, B]\n---\n").aliases(),
            vec!["A", "B"]
        );
        assert_eq!(
            Document::parse("---\nalias: Solo\n---\n").aliases(),
            vec!["Solo"]
        );
        assert!(Document::parse("x").aliases().is_empty());
    }

    #[test]
    fn invalid_yaml_kept_raw() {
        let t = "---\na: [unclosed\n---\nbody\n";
        let mut d = Document::parse(t);
        assert!(d.front_matter_error().is_some());
        assert!(d.front_matter().is_empty());
        d.set_property("x", PropertyValue::Bool(true));
        assert_eq!(d.to_text(), t);
    }

    #[test]
    fn non_mapping_front_matter_is_error() {
        let d = Document::parse("---\n- a\n- b\n---\nbody");
        assert!(d.front_matter_error().is_some());
    }

    #[test]
    fn set_property_keeps_crlf_and_body() {
        let t = "---\r\ntitle: A\r\n---\r\nBody line\r\nsecond  \r\n";
        let mut d = Document::parse(t);
        d.set_property("status", PropertyValue::Text("done".into()));
        let out = d.to_text();
        assert_eq!(
            out,
            "---\r\ntitle: A\r\nstatus: done\r\n---\r\nBody line\r\nsecond  \r\n"
        );
        assert_eq!(Document::parse(&out).body(), d.body());
    }

    #[test]
    fn set_property_replaces_in_place() {
        let mut d = Document::parse("---\na: 1\nb: 2\n---\nx");
        d.set_property("a", PropertyValue::Number(5.0));
        assert_eq!(d.to_text(), "---\na: 5\nb: 2\n---\nx");
    }

    #[test]
    fn set_property_creates_front_matter() {
        let mut d = Document::parse("\u{feff}# Hi\r\ntext");
        d.set_property(
            "tags",
            PropertyValue::List(vec![PropertyValue::Text("a".into())]),
        );
        let out = d.to_text();
        assert!(out.starts_with("\u{feff}---\r\ntags:\r\n- a\r\n---\r\n# Hi\r\ntext"));
        assert_eq!(d.body(), "# Hi\r\ntext");

        let mut d = Document::parse("plain\n");
        d.set_property("k", PropertyValue::Text("v".into()));
        assert_eq!(d.to_text(), "---\nk: v\n---\nplain\n");
    }

    #[test]
    fn remove_property_works() {
        let mut d = Document::parse("---\na: 1\nb: 2\n---\nx");
        assert!(d.remove_property("a"));
        assert!(!d.remove_property("a"));
        assert_eq!(d.to_text(), "---\nb: 2\n---\nx");
        assert!(d.remove_property("b"));
        assert_eq!(d.to_text(), "---\n---\nx");
        assert!(d.front_matter().is_empty());
    }

    #[test]
    fn set_body_keeps_front_matter_bytes() {
        let t = "---\n# comment\na:   'x'\n---\nold";
        let mut d = Document::parse(t);
        d.set_body("new");
        assert_eq!(d.to_text(), "---\n# comment\na:   'x'\n---\nnew");
    }

    #[test]
    fn property_value_serde() {
        let v = PropertyValue::Map(vec![
            ("a".into(), PropertyValue::Number(1.0)),
            ("b".into(), PropertyValue::List(vec![PropertyValue::Null])),
        ]);
        let y = serde_norway::to_string(&v).unwrap();
        let back: PropertyValue = serde_norway::from_str(&y).unwrap();
        assert_eq!(back, v);
    }

    #[test]
    fn list_of_strings_helper() {
        let v = PropertyValue::List(vec![
            PropertyValue::Text("a".into()),
            PropertyValue::Number(2.0),
            PropertyValue::Null,
        ]);
        assert_eq!(v.as_list_of_strings(), vec!["a", "2"]);
        assert_eq!(
            PropertyValue::Text("z".into()).as_list_of_strings(),
            vec!["z"]
        );
    }
}
