//! Dynamically typed cell value used by database properties, formulas, filters and rollups.

use std::cmp::Ordering;
use std::fmt;

use chrono::{NaiveDate, NaiveDateTime, Timelike};
use serde::de::{self, SeqAccess, Visitor};
use serde::ser::SerializeSeq;
use serde::{Deserialize, Deserializer, Serialize, Serializer};

/// A cell value. Serialises untagged: `null`, booleans, numbers, strings (dates as ISO strings)
/// and lists.
///
/// When deserialising, a string that is exactly `YYYY-MM-DD` becomes [`Value::Date`] and one that is
/// `YYYY-MM-DDTHH:MM[:SS]` becomes [`Value::DateTime`], so values survive a YAML round trip;
/// every other string stays [`Value::Text`]. Non-finite numbers deserialise to [`Value::Null`].
#[derive(Debug, Clone, PartialEq, Default)]
pub enum Value {
    /// No value.
    #[default]
    Null,
    /// A checkbox.
    Bool(bool),
    /// A number.
    Number(f64),
    /// Text (also selects, URLs and relation targets).
    Text(String),
    /// A calendar date.
    Date(NaiveDate),
    /// A date with a time of day.
    DateTime(NaiveDateTime),
    /// Several values (multi-select, relation, rollup output).
    List(Vec<Value>),
}

const DATE_FMT: &str = "%Y-%m-%d";

/// Parse `YYYY-MM-DD`.
pub(crate) fn parse_date(s: &str) -> Option<NaiveDate> {
    NaiveDate::parse_from_str(s.trim(), DATE_FMT).ok()
}

/// Parse `YYYY-MM-DDTHH:MM[:SS]` (a space may replace the `T`).
pub(crate) fn parse_datetime(s: &str) -> Option<NaiveDateTime> {
    let s = s.trim();
    [
        "%Y-%m-%dT%H:%M:%S",
        "%Y-%m-%dT%H:%M",
        "%Y-%m-%d %H:%M:%S",
        "%Y-%m-%d %H:%M",
    ]
    .iter()
    .find_map(|f| NaiveDateTime::parse_from_str(s, f).ok())
}

fn format_datetime(dt: &NaiveDateTime) -> String {
    if dt.second() == 0 {
        dt.format("%Y-%m-%dT%H:%M").to_string()
    } else {
        dt.format("%Y-%m-%dT%H:%M:%S").to_string()
    }
}

fn format_number(n: f64) -> String {
    if n.fract() == 0.0 && n.abs() < 1e15 {
        format!("{}", n as i64)
    } else {
        format!("{n}")
    }
}

impl fmt::Display for Value {
    /// Plain-text rendering: `Null` is empty, numbers drop a zero fraction, lists join with `, `.
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Value::Null => Ok(()),
            Value::Bool(b) => write!(f, "{b}"),
            Value::Number(n) => f.write_str(&format_number(*n)),
            Value::Text(s) => f.write_str(s),
            Value::Date(d) => write!(f, "{}", d.format(DATE_FMT)),
            Value::DateTime(dt) => f.write_str(&format_datetime(dt)),
            Value::List(items) => {
                for (i, item) in items.iter().enumerate() {
                    if i > 0 {
                        f.write_str(", ")?;
                    }
                    write!(f, "{item}")?;
                }
                Ok(())
            }
        }
    }
}

impl From<&str> for Value {
    fn from(s: &str) -> Self {
        Value::Text(s.to_string())
    }
}

impl From<String> for Value {
    fn from(s: String) -> Self {
        Value::Text(s)
    }
}

impl From<f64> for Value {
    fn from(n: f64) -> Self {
        Value::Number(n)
    }
}

impl From<bool> for Value {
    fn from(b: bool) -> Self {
        Value::Bool(b)
    }
}

impl From<NaiveDate> for Value {
    fn from(d: NaiveDate) -> Self {
        Value::Date(d)
    }
}

impl From<NaiveDateTime> for Value {
    fn from(d: NaiveDateTime) -> Self {
        Value::DateTime(d)
    }
}

impl Value {
    /// Coerce to a finite number: numbers, numeric text and booleans (1/0).
    pub fn as_number(&self) -> Option<f64> {
        match self {
            Value::Number(n) if n.is_finite() => Some(*n),
            Value::Bool(b) => Some(if *b { 1.0 } else { 0.0 }),
            Value::Text(s) => s.trim().parse::<f64>().ok().filter(|n| n.is_finite()),
            _ => None,
        }
    }

    /// Render as text. `Null` gives an empty string; see the [`Display`](fmt::Display) impl.
    pub fn as_text(&self) -> String {
        self.to_string()
    }

    /// Coerce to a boolean: booleans, non-zero numbers and the words
    /// `true/yes/y/on/checked/x/1` and `false/no/n/off/unchecked/0` (case-insensitive).
    pub fn as_bool(&self) -> Option<bool> {
        match self {
            Value::Bool(b) => Some(*b),
            Value::Number(n) => Some(*n != 0.0),
            Value::Text(s) => match s.trim().to_lowercase().as_str() {
                "true" | "yes" | "y" | "on" | "checked" | "x" | "1" => Some(true),
                "false" | "no" | "n" | "off" | "unchecked" | "0" | "" => Some(false),
                _ => None,
            },
            _ => None,
        }
    }

    /// Coerce to a date: dates, the date part of date-times, and ISO text.
    pub fn as_date(&self) -> Option<NaiveDate> {
        match self {
            Value::Date(d) => Some(*d),
            Value::DateTime(dt) => Some(dt.date()),
            Value::Text(s) => parse_date(s).or_else(|| parse_datetime(s).map(|dt| dt.date())),
            _ => None,
        }
    }

    /// Coerce to a date-time; a plain date becomes midnight.
    pub fn as_datetime(&self) -> Option<NaiveDateTime> {
        match self {
            Value::DateTime(dt) => Some(*dt),
            Value::Date(d) => d.and_hms_opt(0, 0, 0),
            Value::Text(s) => parse_datetime(s).or_else(|| parse_date(s)?.and_hms_opt(0, 0, 0)),
            _ => None,
        }
    }

    /// True for `Null`, blank text and empty lists. `false`, `0` and dates are not empty.
    pub fn is_empty(&self) -> bool {
        match self {
            Value::Null => true,
            Value::Text(s) => s.trim().is_empty(),
            Value::List(l) => l.is_empty(),
            _ => false,
        }
    }

    fn rank(&self) -> u8 {
        match self {
            Value::Bool(_) => 0,
            Value::Number(_) => 1,
            Value::Date(_) | Value::DateTime(_) => 2,
            Value::Text(_) => 3,
            Value::List(_) => 4,
            Value::Null => 5,
        }
    }

    /// Total order for sorting. `Null` sorts last; otherwise values order by kind
    /// (booleans, numbers, dates, text, lists), then within the kind. Text compares
    /// case-insensitively with the raw text as a tie-break; dates and date-times compare as
    /// date-times. Numbers use [`f64::total_cmp`].
    pub fn compare(&self, other: &Value) -> Ordering {
        match (self, other) {
            (Value::Bool(a), Value::Bool(b)) => a.cmp(b),
            (Value::Number(a), Value::Number(b)) => a.total_cmp(b),
            (Value::Text(a), Value::Text(b)) => cmp_text(a, b).then_with(|| a.cmp(b)),
            (Value::List(a), Value::List(b)) => {
                for (x, y) in a.iter().zip(b) {
                    let o = x.compare(y);
                    if o != Ordering::Equal {
                        return o;
                    }
                }
                a.len().cmp(&b.len())
            }
            (a, b) if a.rank() == 2 && b.rank() == 2 => a.as_datetime().cmp(&b.as_datetime()),
            (a, b) => a.rank().cmp(&b.rank()),
        }
    }

    /// Order two values of the same family (numbers, dates or text) for filters and formulas.
    /// Numeric text counts as a number and date strings as dates when the other side is one.
    /// Returns `None` when they are not comparable. Text compares case-insensitively.
    pub fn loose_cmp(&self, other: &Value) -> Option<Ordering> {
        let is_date = |v: &Value| matches!(v, Value::Date(_) | Value::DateTime(_));
        if is_date(self) || is_date(other) {
            return Some(self.as_datetime()?.cmp(&other.as_datetime()?));
        }
        if matches!(self, Value::Number(_)) || matches!(other, Value::Number(_)) {
            return self.as_number()?.partial_cmp(&other.as_number()?);
        }
        match (self, other) {
            (Value::Text(a), Value::Text(b)) => {
                match (a.trim().parse::<f64>(), b.trim().parse::<f64>()) {
                    (Ok(x), Ok(y)) => x.partial_cmp(&y),
                    _ => Some(cmp_text(a, b)),
                }
            }
            (Value::Bool(a), Value::Bool(b)) => Some(a.cmp(b)),
            _ => None,
        }
    }

    /// Forgiving equality used by filters: text ignores case and surrounding blanks, numeric text
    /// equals the number, dates compare by calendar day, and a list equals a scalar when any
    /// element does. Empty values (including empty lists) equal each other.
    pub fn loose_eq(&self, other: &Value) -> bool {
        match (self, other) {
            (Value::Null, _) | (_, Value::Null) => self.is_empty() && other.is_empty(),
            (Value::List(a), Value::List(b)) => {
                a.len() == b.len() && a.iter().zip(b).all(|(x, y)| x.loose_eq(y))
            }
            (Value::List(a), b) => a.iter().any(|x| x.loose_eq(b)),
            (a, Value::List(b)) => b.iter().any(|y| a.loose_eq(y)),
            (Value::Bool(a), b) | (b, Value::Bool(a)) => b.as_bool() == Some(*a),
            (a, b) if a.rank() == 2 || b.rank() == 2 => match (a.as_date(), b.as_date()) {
                (Some(x), Some(y)) => x == y,
                _ => false,
            },
            (a, b) if matches!(a, Value::Number(_)) || matches!(b, Value::Number(_)) => {
                match (a.as_number(), b.as_number()) {
                    (Some(x), Some(y)) => x == y,
                    _ => false,
                }
            }
            (Value::Text(a), Value::Text(b)) => a.trim().to_lowercase() == b.trim().to_lowercase(),
            _ => false,
        }
    }
}

fn cmp_text(a: &str, b: &str) -> Ordering {
    a.to_lowercase().cmp(&b.to_lowercase())
}

impl Serialize for Value {
    fn serialize<S: Serializer>(&self, s: S) -> Result<S::Ok, S::Error> {
        match self {
            Value::Null => s.serialize_unit(),
            Value::Bool(b) => s.serialize_bool(*b),
            Value::Number(n) => {
                if n.fract() == 0.0 && n.abs() < 1e15 {
                    s.serialize_i64(*n as i64)
                } else {
                    s.serialize_f64(*n)
                }
            }
            Value::Text(t) => s.serialize_str(t),
            Value::Date(d) => s.serialize_str(&d.format(DATE_FMT).to_string()),
            Value::DateTime(dt) => s.serialize_str(&format_datetime(dt)),
            Value::List(items) => {
                let mut seq = s.serialize_seq(Some(items.len()))?;
                for item in items {
                    seq.serialize_element(item)?;
                }
                seq.end()
            }
        }
    }
}

struct ValueVisitor;

impl<'de> Visitor<'de> for ValueVisitor {
    type Value = Value;

    fn expecting(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str("null, a boolean, a number, a string or a list")
    }

    fn visit_unit<E: de::Error>(self) -> Result<Value, E> {
        Ok(Value::Null)
    }

    fn visit_none<E: de::Error>(self) -> Result<Value, E> {
        Ok(Value::Null)
    }

    fn visit_some<D: Deserializer<'de>>(self, d: D) -> Result<Value, D::Error> {
        Value::deserialize(d)
    }

    fn visit_bool<E: de::Error>(self, v: bool) -> Result<Value, E> {
        Ok(Value::Bool(v))
    }

    fn visit_i64<E: de::Error>(self, v: i64) -> Result<Value, E> {
        Ok(Value::Number(v as f64))
    }

    fn visit_u64<E: de::Error>(self, v: u64) -> Result<Value, E> {
        Ok(Value::Number(v as f64))
    }

    fn visit_f64<E: de::Error>(self, v: f64) -> Result<Value, E> {
        Ok(if v.is_finite() {
            Value::Number(v)
        } else {
            Value::Null
        })
    }

    fn visit_str<E: de::Error>(self, v: &str) -> Result<Value, E> {
        if v.len() == 10 {
            if let Ok(d) = NaiveDate::parse_from_str(v, DATE_FMT) {
                return Ok(Value::Date(d));
            }
        } else if v.len() >= 16 && v.as_bytes().get(10) == Some(&b'T') {
            if let Some(dt) = parse_datetime(v) {
                return Ok(Value::DateTime(dt));
            }
        }
        Ok(Value::Text(v.to_string()))
    }

    fn visit_string<E: de::Error>(self, v: String) -> Result<Value, E> {
        self.visit_str(&v)
    }

    fn visit_seq<A: SeqAccess<'de>>(self, mut seq: A) -> Result<Value, A::Error> {
        let mut items = Vec::new();
        while let Some(item) = seq.next_element::<Value>()? {
            items.push(item);
        }
        Ok(Value::List(items))
    }
}

impl<'de> Deserialize<'de> for Value {
    fn deserialize<D: Deserializer<'de>>(d: D) -> Result<Self, D::Error> {
        d.deserialize_any(ValueVisitor)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn d(y: i32, m: u32, day: u32) -> NaiveDate {
        NaiveDate::from_ymd_opt(y, m, day).unwrap()
    }

    #[test]
    fn display_forms() {
        assert_eq!(Value::Null.to_string(), "");
        assert_eq!(Value::Number(3.0).to_string(), "3");
        assert_eq!(Value::Number(3.5).to_string(), "3.5");
        assert_eq!(Value::Date(d(2026, 1, 2)).to_string(), "2026-01-02");
        let l = Value::List(vec!["a".into(), Value::Number(2.0)]);
        assert_eq!(l.to_string(), "a, 2");
    }

    #[test]
    fn coercions() {
        assert_eq!(Value::from(" 12 ").as_number(), Some(12.0));
        assert_eq!(Value::from("x").as_number(), None);
        assert_eq!(Value::Bool(true).as_number(), Some(1.0));
        assert_eq!(Value::Number(f64::NAN).as_number(), None);
        assert_eq!(Value::from("Yes").as_bool(), Some(true));
        assert_eq!(Value::from("maybe").as_bool(), None);
        assert_eq!(Value::Number(0.0).as_bool(), Some(false));
        assert_eq!(Value::from("2026-03-04").as_date(), Some(d(2026, 3, 4)));
        assert_eq!(
            Value::from("2026-03-04T10:30").as_date(),
            Some(d(2026, 3, 4))
        );
        assert_eq!(
            Value::Date(d(2026, 3, 4)).as_datetime(),
            d(2026, 3, 4).and_hms_opt(0, 0, 0)
        );
    }

    #[test]
    fn emptiness() {
        assert!(Value::Null.is_empty());
        assert!(Value::from("  ").is_empty());
        assert!(Value::List(vec![]).is_empty());
        assert!(!Value::Bool(false).is_empty());
        assert!(!Value::Number(0.0).is_empty());
    }

    #[test]
    fn compare_is_total_with_null_last() {
        let mut v = [
            Value::Null,
            Value::from("b"),
            Value::Number(2.0),
            Value::from("A"),
            Value::Number(-1.0),
            Value::Date(d(2026, 1, 1)),
        ];
        v.sort_by(|a, b| a.compare(b));
        assert_eq!(v[0], Value::Number(-1.0));
        assert_eq!(v[1], Value::Number(2.0));
        assert_eq!(v[2], Value::Date(d(2026, 1, 1)));
        assert_eq!(v[3], Value::from("A"));
        assert_eq!(v[4], Value::from("b"));
        assert_eq!(v[5], Value::Null);
        assert_eq!(Value::Null.compare(&Value::Null), Ordering::Equal);
    }

    #[test]
    fn loose_comparisons() {
        assert_eq!(
            Value::from("10").loose_cmp(&Value::Number(9.0)),
            Some(Ordering::Greater)
        );
        assert_eq!(Value::from("a").loose_cmp(&Value::Number(9.0)), None);
        assert!(Value::from(" Done ").loose_eq(&Value::from("done")));
        assert!(Value::from("5").loose_eq(&Value::Number(5.0)));
        assert!(Value::List(vec!["a".into(), "b".into()]).loose_eq(&"B".into()));
        assert!(Value::Null.loose_eq(&Value::List(vec![])));
        assert!(!Value::Null.loose_eq(&Value::Number(0.0)));
    }
}
