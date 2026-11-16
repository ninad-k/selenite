//! Task model: the front matter of a Markdown row file in a Tasks database folder.

use std::fmt;
use std::str::FromStr;

use chrono::{Duration, NaiveDate, NaiveDateTime, NaiveTime};
use serde::{Deserialize, Deserializer, Serialize, Serializer, de::Error as _};
use thiserror::Error;

use crate::recurrence::Recurrence;

/// Front matter keys of a task row, in canonical order.
pub const TASK_KEYS: &[&str] = &[
    "title",
    "done",
    "priority",
    "due",
    "recurrence",
    "reminders",
    "project",
    "section",
    "labels",
    "parent",
    "completed_at",
    "last_completed",
];

/// Errors from parsing task field values.
#[derive(Debug, Error, PartialEq, Eq)]
pub enum TaskError {
    /// Not one of `p1`..`p4` / `1`..`4`.
    #[error("invalid priority `{0}` (expected p1..p4)")]
    InvalidPriority(String),
    /// Not `YYYY-MM-DD` or `YYYY-MM-DDTHH:MM`.
    #[error("invalid due value `{0}` (expected YYYY-MM-DD or YYYY-MM-DDTHH:MM)")]
    InvalidDue(String),
    /// Not `YYYY-MM-DDTHH:MM` or a relative offset like `-15m`.
    #[error("invalid reminder `{0}` (expected YYYY-MM-DDTHH:MM or -15m/-1h/-2d)")]
    InvalidReminder(String),
}

/// Task priority. `P1` is the highest and sorts first; the default is `P4`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Default)]
pub enum Priority {
    /// Highest priority.
    P1,
    /// High priority.
    P2,
    /// Medium priority.
    P3,
    /// Normal priority (default).
    #[default]
    P4,
}

impl Priority {
    /// The integer 1..=4 stored in front matter.
    pub fn as_u8(self) -> u8 {
        match self {
            Priority::P1 => 1,
            Priority::P2 => 2,
            Priority::P3 => 3,
            Priority::P4 => 4,
        }
    }

    /// Build from the integer 1..=4.
    pub fn from_u8(n: u8) -> Option<Self> {
        match n {
            1 => Some(Priority::P1),
            2 => Some(Priority::P2),
            3 => Some(Priority::P3),
            4 => Some(Priority::P4),
            _ => None,
        }
    }
}

impl FromStr for Priority {
    type Err = TaskError;

    /// Accepts `p1`..`p4` (any case); also bare `1`..`4`.
    fn from_str(s: &str) -> Result<Self, Self::Err> {
        let t = s.trim().to_ascii_lowercase();
        let digits = t.strip_prefix('p').unwrap_or(&t);
        digits
            .parse::<u8>()
            .ok()
            .and_then(Priority::from_u8)
            .ok_or_else(|| TaskError::InvalidPriority(s.to_string()))
    }
}

impl fmt::Display for Priority {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "p{}", self.as_u8())
    }
}

impl Serialize for Priority {
    fn serialize<S: Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
        serializer.serialize_u8(self.as_u8())
    }
}

impl<'de> Deserialize<'de> for Priority {
    fn deserialize<D: Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        let n = u8::deserialize(deserializer)?;
        Priority::from_u8(n).ok_or_else(|| D::Error::custom(format!("invalid priority {n}")))
    }
}

const DATE_FMT: &str = "%Y-%m-%d";
const DATETIME_FMT: &str = "%Y-%m-%dT%H:%M";

fn parse_time(s: &str) -> Option<NaiveTime> {
    NaiveTime::parse_from_str(s, "%H:%M")
        .or_else(|_| NaiveTime::parse_from_str(s, "%H:%M:%S"))
        .ok()
}

fn parse_datetime(s: &str) -> Option<NaiveDateTime> {
    let (d, t) = s.split_once(['T', ' '])?;
    let date = NaiveDate::parse_from_str(d, DATE_FMT).ok()?;
    Some(date.and_time(parse_time(t)?))
}

/// A due date with an optional time of day.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct Due {
    /// The due day.
    pub date: NaiveDate,
    /// Time of day, when the task is time-specific.
    pub time: Option<NaiveTime>,
}

impl Due {
    /// A date-only due value.
    pub fn date(date: NaiveDate) -> Self {
        Due { date, time: None }
    }

    /// The due moment when a time is set, otherwise `None`.
    pub fn datetime(&self) -> Option<NaiveDateTime> {
        self.time.map(|t| self.date.and_time(t))
    }

    /// The due moment, using 00:00 when no time is set.
    pub fn or_start_of_day(&self) -> NaiveDateTime {
        self.date.and_time(self.time.unwrap_or(NaiveTime::MIN))
    }
}

impl FromStr for Due {
    type Err = TaskError;

    /// Parses `YYYY-MM-DD` or `YYYY-MM-DDTHH:MM` (seconds tolerated and dropped).
    fn from_str(s: &str) -> Result<Self, Self::Err> {
        let err = || TaskError::InvalidDue(s.to_string());
        let t = s.trim();
        if t.contains(['T', ' ']) {
            let dt = parse_datetime(t).ok_or_else(err)?;
            Ok(Due {
                date: dt.date(),
                time: Some(dt.time()),
            })
        } else {
            let date = NaiveDate::parse_from_str(t, DATE_FMT).map_err(|_| err())?;
            Ok(Due::date(date))
        }
    }
}

impl fmt::Display for Due {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self.time {
            Some(t) => write!(f, "{}", self.date.and_time(t).format(DATETIME_FMT)),
            None => write!(f, "{}", self.date.format(DATE_FMT)),
        }
    }
}

impl Serialize for Due {
    fn serialize<S: Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
        serializer.serialize_str(&self.to_string())
    }
}

impl<'de> Deserialize<'de> for Due {
    fn deserialize<D: Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        let s = String::deserialize(deserializer)?;
        s.parse().map_err(D::Error::custom)
    }
}

/// When to remind about a task.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum ReminderSpec {
    /// An absolute local date-time.
    At(NaiveDateTime),
    /// A (positive) lead time before the due date-time; `-15m` in text form.
    Before(Duration),
}

impl ReminderSpec {
    /// The absolute reminder time given the task's due moment (`Before` needs one).
    pub fn resolve(&self, due: Option<NaiveDateTime>) -> Option<NaiveDateTime> {
        match self {
            ReminderSpec::At(dt) => Some(*dt),
            ReminderSpec::Before(d) => due?.checked_sub_signed(*d),
        }
    }
}

impl FromStr for ReminderSpec {
    type Err = TaskError;

    /// Parses `YYYY-MM-DDTHH:MM` or `-<N>m|h|d` (the leading `-` is optional).
    fn from_str(s: &str) -> Result<Self, Self::Err> {
        let err = || TaskError::InvalidReminder(s.to_string());
        let t = s.trim();
        let rel = t.strip_prefix('-').unwrap_or(t);
        if let Some(unit) = rel.chars().last()
            && let Some(body) = rel.get(..rel.len() - unit.len_utf8())
            && !body.is_empty()
            && body.bytes().all(|b| b.is_ascii_digit())
            && matches!(unit, 'm' | 'h' | 'd')
        {
            let n = body.parse::<i64>().map_err(|_| err())?;
            if n == 0 {
                return Err(err());
            }
            let d = match unit {
                'm' => Duration::try_minutes(n),
                'h' => Duration::try_hours(n),
                _ => Duration::try_days(n),
            };
            return d.map(ReminderSpec::Before).ok_or_else(err);
        }
        parse_datetime(t).map(ReminderSpec::At).ok_or_else(err)
    }
}

impl fmt::Display for ReminderSpec {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            ReminderSpec::At(dt) => write!(f, "{}", dt.format(DATETIME_FMT)),
            ReminderSpec::Before(d) => {
                let mins = d.num_minutes();
                if mins % 1440 == 0 {
                    write!(f, "-{}d", mins / 1440)
                } else if mins % 60 == 0 {
                    write!(f, "-{}h", mins / 60)
                } else {
                    write!(f, "-{mins}m")
                }
            }
        }
    }
}

impl Serialize for ReminderSpec {
    fn serialize<S: Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
        serializer.serialize_str(&self.to_string())
    }
}

impl<'de> Deserialize<'de> for ReminderSpec {
    fn deserialize<D: Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        let s = String::deserialize(deserializer)?;
        s.parse().map_err(D::Error::custom)
    }
}

fn is_false(b: &bool) -> bool {
    !*b
}

/// All fields of a task, as stored in the front matter of its row file.
#[derive(Debug, Clone, PartialEq, Eq, Default, Serialize, Deserialize)]
#[serde(default)]
pub struct TaskFields {
    /// Task title (the row's name).
    pub title: String,
    /// Whether the task is completed (recurring tasks stay open).
    #[serde(skip_serializing_if = "is_false")]
    pub done: bool,
    /// Priority; `P4` by default.
    pub priority: Priority,
    /// Due date and optional time.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub due: Option<Due>,
    /// Recurrence rule, if the task repeats.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub recurrence: Option<Recurrence>,
    /// Reminders for the task.
    #[serde(skip_serializing_if = "Vec::is_empty")]
    pub reminders: Vec<ReminderSpec>,
    /// Project path (`Home` or `Home/Garden`); `None` is the Inbox.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub project: Option<String>,
    /// Section within the project.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub section: Option<String>,
    /// Labels.
    #[serde(skip_serializing_if = "Vec::is_empty")]
    pub labels: Vec<String>,
    /// Wikilink target of the parent task.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub parent: Option<String>,
    /// When the task was last marked done.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub completed_at: Option<NaiveDateTime>,
    /// The due date that was last completed (recurring tasks).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub last_completed: Option<NaiveDate>,
}

impl TaskFields {
    /// A new open task with the given title and defaults elsewhere.
    pub fn new(title: impl Into<String>) -> Self {
        TaskFields {
            title: title.into(),
            ..TaskFields::default()
        }
    }

    /// The due moment when the due value has a time; `None` for date-only or no due.
    pub fn due_datetime(&self) -> Option<NaiveDateTime> {
        self.due.and_then(|d| d.datetime())
    }

    /// The due moment, using 00:00 for date-only dues.
    pub fn due_or_start_of_day(&self) -> Option<NaiveDateTime> {
        self.due.map(|d| d.or_start_of_day())
    }

    /// Mark the task complete at `now`; returns whether the task is still open.
    ///
    /// Non-recurring (or recurring without a due date): `done = true`, `completed_at = now`.
    /// Recurring with a due date: `due` advances to the next occurrence strictly after the current
    /// due date (the time is kept) anchored at the current due date, `last_completed` is the old
    /// due date and `done` stays false. A `COUNT` is decremented; when the series is exhausted
    /// (no more occurrences, `UNTIL` passed or `COUNT` used up) the task becomes done instead.
    pub fn complete(&mut self, now: NaiveDateTime) -> bool {
        if let (Some(rule), Some(due)) = (&self.recurrence, self.due) {
            let exhausted = rule.count.is_some_and(|c| c <= 1);
            let mut probe = rule.clone();
            probe.count = None;
            let next = if exhausted {
                None
            } else {
                probe.next_after(due.date, due.date)
            };
            self.last_completed = Some(due.date);
            if let Some(date) = next {
                self.due = Some(Due {
                    date,
                    time: due.time,
                });
                if let Some(rule) = self.recurrence.as_mut()
                    && let Some(c) = rule.count
                {
                    rule.count = Some(c - 1);
                }
                self.done = false;
                self.completed_at = None;
                return true;
            }
        }
        self.done = true;
        self.completed_at = Some(now);
        false
    }

    /// Reopen a completed task (clears `done` and `completed_at`).
    pub fn reopen(&mut self) {
        self.done = false;
        self.completed_at = None;
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde::de::IntoDeserializer;
    use serde::de::value::{Error as DeError, StrDeserializer, U8Deserializer};

    fn d(y: i32, m: u32, day: u32) -> NaiveDate {
        NaiveDate::from_ymd_opt(y, m, day).expect("valid date")
    }

    fn dt(y: i32, m: u32, day: u32, h: u32, min: u32) -> NaiveDateTime {
        d(y, m, day).and_hms_opt(h, min, 0).expect("valid time")
    }

    #[test]
    fn task_keys_are_complete() {
        assert_eq!(TASK_KEYS.len(), 12);
        assert_eq!(TASK_KEYS[0], "title");
    }

    #[test]
    fn priority_parse_display_serde() {
        assert_eq!("p1".parse::<Priority>(), Ok(Priority::P1));
        assert_eq!("P3".parse::<Priority>(), Ok(Priority::P3));
        assert_eq!("4".parse::<Priority>(), Ok(Priority::P4));
        assert!("p5".parse::<Priority>().is_err());
        assert!("x".parse::<Priority>().is_err());
        assert_eq!(Priority::default(), Priority::P4);
        assert!(Priority::P1 < Priority::P4);
        assert_eq!(Priority::P2.to_string(), "p2");
        let de: U8Deserializer<DeError> = 2u8.into_deserializer();
        assert_eq!(Priority::deserialize(de), Ok(Priority::P2));
        let de: U8Deserializer<DeError> = 9u8.into_deserializer();
        assert!(Priority::deserialize(de).is_err());
    }

    #[test]
    fn due_parse_and_format() {
        let a: Due = "2026-11-20".parse().expect("date");
        assert_eq!(a, Due::date(d(2026, 11, 20)));
        assert_eq!(a.to_string(), "2026-11-20");
        let b: Due = "2026-11-20T09:00".parse().expect("datetime");
        assert_eq!(b.datetime(), Some(dt(2026, 11, 20, 9, 0)));
        assert_eq!(b.to_string(), "2026-11-20T09:00");
        let c: Due = "2026-11-20T09:00:30".parse().expect("with seconds");
        assert_eq!(c.to_string(), "2026-11-20T09:00");
        assert_eq!(a.datetime(), None);
        assert_eq!(a.or_start_of_day(), dt(2026, 11, 20, 0, 0));
        assert!("2026-13-01".parse::<Due>().is_err());
        assert!("tomorrow".parse::<Due>().is_err());
        assert!("2026-11-20Tnoon".parse::<Due>().is_err());
        let de: StrDeserializer<DeError> = "2026-11-20".into_deserializer();
        assert_eq!(Due::deserialize(de), Ok(a));
    }

    #[test]
    fn reminder_parse_and_format() {
        let at: ReminderSpec = "2026-11-20T08:45".parse().expect("at");
        assert_eq!(at, ReminderSpec::At(dt(2026, 11, 20, 8, 45)));
        assert_eq!(at.to_string(), "2026-11-20T08:45");
        for (text, mins) in [("-15m", 15), ("-1h", 60), ("-2d", 2880), ("-90m", 90)] {
            let r: ReminderSpec = text.parse().expect("relative");
            assert_eq!(r, ReminderSpec::Before(Duration::minutes(mins)), "{text}");
            assert_eq!(r.to_string(), text);
        }
        assert_eq!(
            "30m".parse::<ReminderSpec>(),
            Ok(ReminderSpec::Before(Duration::minutes(30)))
        );
        for bad in ["-0m", "-m", "-5x", "soon", "-1.5h", ""] {
            assert!(bad.parse::<ReminderSpec>().is_err(), "{bad}");
        }
        let r = ReminderSpec::Before(Duration::minutes(15));
        assert_eq!(
            r.resolve(Some(dt(2026, 11, 20, 9, 0))),
            Some(dt(2026, 11, 20, 8, 45))
        );
        assert_eq!(r.resolve(None), None);
        assert_eq!(at.resolve(None), Some(dt(2026, 11, 20, 8, 45)));
    }

    #[test]
    fn complete_non_recurring() {
        let mut t = TaskFields::new("x");
        t.due = Some(Due::date(d(2026, 11, 20)));
        let now = dt(2026, 11, 16, 10, 0);
        assert!(!t.complete(now));
        assert!(t.done);
        assert_eq!(t.completed_at, Some(now));
        t.reopen();
        assert!(!t.done);
        assert_eq!(t.completed_at, None);
    }

    #[test]
    fn complete_recurring_advances_due_and_keeps_time() {
        let mut t = TaskFields::new("rent");
        t.recurrence = Some(Recurrence::parse("FREQ=MONTHLY;BYMONTHDAY=1").expect("rule"));
        t.due = "2026-12-01T09:00".parse().ok();
        assert!(t.complete(dt(2026, 11, 30, 12, 0)));
        assert!(!t.done);
        assert_eq!(t.completed_at, None);
        assert_eq!(t.due, "2027-01-01T09:00".parse().ok());
        assert_eq!(t.last_completed, Some(d(2026, 12, 1)));
    }

    #[test]
    fn complete_recurring_without_due_is_plain_completion() {
        let mut t = TaskFields::new("x");
        t.recurrence = Some(Recurrence::parse("FREQ=DAILY").expect("rule"));
        assert!(!t.complete(dt(2026, 11, 16, 10, 0)));
        assert!(t.done);
    }

    #[test]
    fn complete_recurring_count_and_until_exhaust() {
        let mut t = TaskFields::new("x");
        t.recurrence = Some(Recurrence::parse("FREQ=DAILY;COUNT=2").expect("rule"));
        t.due = Some(Due::date(d(2026, 11, 16)));
        assert!(t.complete(dt(2026, 11, 16, 10, 0)));
        assert_eq!(t.due, Some(Due::date(d(2026, 11, 17))));
        assert_eq!(t.recurrence.as_ref().and_then(|r| r.count), Some(1));
        assert!(!t.complete(dt(2026, 11, 17, 10, 0)));
        assert!(t.done);
        assert_eq!(t.last_completed, Some(d(2026, 11, 17)));

        let mut u = TaskFields::new("y");
        u.recurrence = Some(Recurrence::parse("FREQ=DAILY;UNTIL=20261117").expect("rule"));
        u.due = Some(Due::date(d(2026, 11, 17)));
        assert!(!u.complete(dt(2026, 11, 17, 10, 0)));
        assert!(u.done);
    }

    #[test]
    fn due_helpers() {
        let mut t = TaskFields::new("x");
        assert_eq!(t.due_datetime(), None);
        assert_eq!(t.due_or_start_of_day(), None);
        t.due = Some(Due::date(d(2026, 11, 20)));
        assert_eq!(t.due_datetime(), None);
        assert_eq!(t.due_or_start_of_day(), Some(dt(2026, 11, 20, 0, 0)));
        t.due = "2026-11-20T09:00".parse().ok();
        assert_eq!(t.due_datetime(), Some(dt(2026, 11, 20, 9, 0)));
    }
}
