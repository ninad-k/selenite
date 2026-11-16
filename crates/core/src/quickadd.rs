//! Natural-language quick add: one line becomes a task.
//!
//! Tokens are whitespace separated and matched case-insensitively; matched tokens are removed and
//! the rest (whitespace-collapsed) is the title. Unknown text never errors.
//!
//! Recognised, in any position:
//! - priority `p1`..`p4`; project `#Name` / `#Name/Sub` (last wins); section `/Section` directly
//!   after a project token; labels `@label`;
//! - dates: `today`/`tod`, `tomorrow`/`tmr`, weekday names (`mon`, `monday`, ...), `next week`,
//!   `next <weekday>`, `in N days|weeks|months`, ISO `2026-11-20`, `nov 20`, `20 nov`,
//!   `november 20th`, each optionally followed by a 4-digit year;
//! - times: `at 9am`, `9am`, `9:30pm`, `at 14:00`, `14:00`, `noon`, `midnight`;
//! - recurrence: `daily`, `every day|weekday|weekend|week|month|year`, `every other <unit>`,
//!   `every N days|weeks|months|years`, `every <weekday>[, and ...]`, `every 1st|15th|last day`,
//!   `every 2nd tue|last fri`, `every <month> <day>`;
//! - reminders: `remind 15m before` (`m`, `h` or `d`).
//!
//! Weekday rule: a bare weekday is the next such day strictly after today (1..=7 days ahead).
//! `next <weekday>` is the same date, pushed a further 7 days if that date is still in the current
//! Monday-to-Sunday week. `next week` is the coming Monday. A date with no year that has already
//! passed this year means next year. A time with no date means today if still in the future,
//! otherwise tomorrow. A recurring task's due date is its first occurrence on or after today
//! (or on or after the explicit date), skipping today if the given time has already passed.

use chrono::{Datelike, Duration, Months, NaiveDate, NaiveDateTime, NaiveTime, Weekday};

use crate::recurrence::{Freq, Recurrence};
use crate::task::{Due, Priority, ReminderSpec, TaskFields};

/// The result of parsing a quick-add line.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct QuickAdd {
    /// The task fields extracted from the line.
    pub fields: TaskFields,
    /// Non-fatal notes about ambiguous or ignored input.
    pub warnings: Vec<String>,
}

struct Toks {
    raw: Vec<String>,
    norm: Vec<String>,
    comma: Vec<bool>,
}

impl Toks {
    fn new(input: &str) -> Self {
        let raw: Vec<String> = input.split_whitespace().map(str::to_string).collect();
        let norm = raw
            .iter()
            .map(|t| {
                t.to_lowercase()
                    .trim_end_matches([',', ';', '.', '!', '?'])
                    .to_string()
            })
            .collect();
        let comma = raw.iter().map(|t| t.ends_with(',')).collect();
        Toks { raw, norm, comma }
    }

    fn n(&self, i: usize) -> Option<&str> {
        self.norm.get(i).map(String::as_str)
    }
}

fn weekday_of(s: &str) -> Option<Weekday> {
    Some(match s {
        "mon" | "monday" => Weekday::Mon,
        "tue" | "tues" | "tuesday" => Weekday::Tue,
        "wed" | "weds" | "wednesday" => Weekday::Wed,
        "thu" | "thur" | "thurs" | "thursday" => Weekday::Thu,
        "fri" | "friday" => Weekday::Fri,
        "sat" | "saturday" => Weekday::Sat,
        "sun" | "sunday" => Weekday::Sun,
        _ => return None,
    })
}

fn month_of(s: &str) -> Option<u32> {
    Some(match s {
        "jan" | "january" => 1,
        "feb" | "february" => 2,
        "mar" | "march" => 3,
        "apr" | "april" => 4,
        "may" => 5,
        "jun" | "june" => 6,
        "jul" | "july" => 7,
        "aug" | "august" => 8,
        "sep" | "sept" | "september" => 9,
        "oct" | "october" => 10,
        "nov" | "november" => 11,
        "dec" | "december" => 12,
        _ => return None,
    })
}

/// A number 1..=31 with an optional ordinal suffix; the flag says whether a suffix was present.
fn day_num(s: &str) -> Option<(u32, bool)> {
    let (digits, suffixed) = match s.strip_suffix("st").or_else(|| s.strip_suffix("nd")) {
        Some(d) => (d, true),
        None => match s.strip_suffix("rd").or_else(|| s.strip_suffix("th")) {
            Some(d) => (d, true),
            None => (s, false),
        },
    };
    if digits.is_empty() || digits.len() > 2 || !digits.bytes().all(|b| b.is_ascii_digit()) {
        return None;
    }
    let n = digits.parse::<u32>().ok()?;
    (1..=31).contains(&n).then_some((n, suffixed))
}

fn year_num(s: &str) -> Option<i32> {
    if s.len() == 4 && s.bytes().all(|b| b.is_ascii_digit()) {
        s.parse::<i32>().ok().filter(|y| (2000..=2100).contains(y))
    } else {
        None
    }
}

fn next_weekday_after(today: NaiveDate, wd: Weekday) -> NaiveDate {
    let delta = (7 + i64::from(wd.num_days_from_monday())
        - i64::from(today.weekday().num_days_from_monday()))
        % 7;
    today + Duration::days(if delta == 0 { 7 } else { delta })
}

fn same_week(a: NaiveDate, b: NaiveDate) -> bool {
    a.iso_week() == b.iso_week()
}

/// Resolve month/day (and optional year) to a date; without a year, past dates roll to next year.
fn resolve_month_day(
    today: NaiveDate,
    month: u32,
    day: u32,
    year: Option<i32>,
) -> Option<NaiveDate> {
    match year {
        Some(y) => NaiveDate::from_ymd_opt(y, month, day),
        None => {
            let this = NaiveDate::from_ymd_opt(today.year(), month, day);
            match this {
                Some(d) if d >= today => Some(d),
                _ => NaiveDate::from_ymd_opt(today.year() + 1, month, day),
            }
        }
    }
}

fn parse_date(t: &Toks, i: usize, today: NaiveDate) -> Option<(NaiveDate, usize)> {
    let a = t.n(i)?;
    match a {
        "today" | "tod" => return Some((today, 1)),
        "tomorrow" | "tmr" | "tmrw" => return Some((today + Duration::days(1), 1)),
        "next" => {
            let b = t.n(i + 1)?;
            if b == "week" {
                let to_monday = 7 - i64::from(today.weekday().num_days_from_monday());
                return Some((today + Duration::days(to_monday), 2));
            }
            let wd = weekday_of(b)?;
            let mut d = next_weekday_after(today, wd);
            if same_week(d, today) {
                d += Duration::days(7);
            }
            return Some((d, 2));
        }
        "in" => {
            let n = t
                .n(i + 1)?
                .parse::<u32>()
                .ok()
                .filter(|n| (1..=3650).contains(n))?;
            let unit = t.n(i + 2)?;
            let date = match unit {
                "day" | "days" => today.checked_add_signed(Duration::days(i64::from(n)))?,
                "week" | "weeks" => today.checked_add_signed(Duration::weeks(i64::from(n)))?,
                "month" | "months" => today.checked_add_months(Months::new(n))?,
                _ => return None,
            };
            return Some((date, 3));
        }
        _ => {}
    }
    if let Some(wd) = weekday_of(a) {
        return Some((next_weekday_after(today, wd), 1));
    }
    if a.len() == 10
        && let Ok(d) = NaiveDate::parse_from_str(a, "%Y-%m-%d")
    {
        return Some((d, 1));
    }
    // month day [year]
    if let Some(m) = month_of(a)
        && let Some((d, _)) = t.n(i + 1).and_then(day_num)
    {
        let year = t.n(i + 2).and_then(year_num);
        let date = resolve_month_day(today, m, d, year)?;
        return Some((date, if year.is_some() { 3 } else { 2 }));
    }
    // day month [year]
    if let Some((d, _)) = day_num(a)
        && let Some(m) = t.n(i + 1).and_then(month_of)
    {
        let year = t.n(i + 2).and_then(year_num);
        let date = resolve_month_day(today, m, d, year)?;
        return Some((date, if year.is_some() { 3 } else { 2 }));
    }
    None
}

fn parse_time_token(s: &str) -> Option<NaiveTime> {
    match s {
        "noon" => return NaiveTime::from_hms_opt(12, 0, 0),
        "midnight" => return NaiveTime::from_hms_opt(0, 0, 0),
        _ => {}
    }
    let (body, meridiem) = if let Some(b) = s.strip_suffix("am") {
        (b, Some(false))
    } else if let Some(b) = s.strip_suffix("pm") {
        (b, Some(true))
    } else {
        (s, None)
    };
    let (h, m) = match body.split_once(':') {
        Some((h, m)) => {
            if m.len() != 2 {
                return None;
            }
            (h, m)
        }
        None => {
            // A bare number is only a time with am/pm.
            meridiem?;
            (body, "0")
        }
    };
    if h.is_empty() || h.len() > 2 || !h.bytes().all(|b| b.is_ascii_digit()) {
        return None;
    }
    if !m.bytes().all(|b| b.is_ascii_digit()) {
        return None;
    }
    let hour = h.parse::<u32>().ok()?;
    let min = m.parse::<u32>().ok()?;
    let hour = match meridiem {
        Some(pm) => {
            if !(1..=12).contains(&hour) {
                return None;
            }
            (hour % 12) + if pm { 12 } else { 0 }
        }
        None => hour,
    };
    NaiveTime::from_hms_opt(hour, min, 0)
}

fn parse_time(t: &Toks, i: usize) -> Option<(NaiveTime, usize)> {
    let a = t.n(i)?;
    if a == "at"
        && let Some(time) = t.n(i + 1).and_then(parse_time_token)
    {
        return Some((time, 2));
    }
    parse_time_token(a).map(|time| (time, 1))
}

fn unit_freq(s: &str) -> Option<Freq> {
    match s {
        "day" | "days" => Some(Freq::Daily),
        "week" | "weeks" => Some(Freq::Weekly),
        "month" | "months" => Some(Freq::Monthly),
        "year" | "years" => Some(Freq::Yearly),
        _ => None,
    }
}

fn with_interval(freq: Freq, interval: u32) -> Recurrence {
    let mut r = Recurrence::new(freq);
    r.interval = interval;
    r
}

fn parse_recurrence(t: &Toks, i: usize) -> Option<(Recurrence, usize)> {
    let a = t.n(i)?;
    match a {
        "daily" => return Some((Recurrence::new(Freq::Daily), 1)),
        "weekly" => return Some((Recurrence::new(Freq::Weekly), 1)),
        "monthly" => return Some((Recurrence::new(Freq::Monthly), 1)),
        "yearly" | "annually" => return Some((Recurrence::new(Freq::Yearly), 1)),
        "every" => {}
        _ => return None,
    }
    let b = t.n(i + 1)?;
    match b {
        "weekday" | "weekdays" => {
            let mut r = Recurrence::new(Freq::Weekly);
            r.by_day = [
                Weekday::Mon,
                Weekday::Tue,
                Weekday::Wed,
                Weekday::Thu,
                Weekday::Fri,
            ]
            .into_iter()
            .map(|w| (None, w))
            .collect();
            return Some((r, 2));
        }
        "weekend" | "weekends" => {
            let mut r = Recurrence::new(Freq::Weekly);
            r.by_day = vec![(None, Weekday::Sat), (None, Weekday::Sun)];
            return Some((r, 2));
        }
        "other" => {
            let freq = t.n(i + 2).and_then(unit_freq)?;
            return Some((with_interval(freq, 2), 3));
        }
        "last" => {
            let c = t.n(i + 2)?;
            if c == "day" {
                let mut r = Recurrence::new(Freq::Monthly);
                r.by_month_day = vec![-1];
                return Some((r, 3));
            }
            let wd = weekday_of(c)?;
            let mut r = Recurrence::new(Freq::Monthly);
            r.by_day = vec![(Some(-1), wd)];
            return Some((r, 3));
        }
        _ => {}
    }
    if let Some(freq) = unit_freq(b)
        && !b.ends_with('s')
    {
        return Some((Recurrence::new(freq), 2));
    }
    // every N <unit>
    if let Ok(n) = b.parse::<u32>()
        && n >= 1
        && let Some(freq) = t.n(i + 2).and_then(unit_freq)
    {
        return Some((with_interval(freq, n), 3));
    }
    // every 1st / every 2nd tue
    if let Some((n, true)) = day_num(b) {
        if let Some(wd) = t.n(i + 2).and_then(weekday_of)
            && n <= 5
        {
            let mut r = Recurrence::new(Freq::Monthly);
            r.by_day = vec![(i8::try_from(n).ok(), wd)];
            return Some((r, 3));
        }
        let mut r = Recurrence::new(Freq::Monthly);
        r.by_month_day = vec![i8::try_from(n).ok()?];
        return Some((r, 2));
    }
    // every <month> <day> / every <day> <month>
    if let Some(m) = month_of(b)
        && let Some((d, _)) = t.n(i + 2).and_then(day_num)
    {
        return Some((yearly_on(m, d)?, 3));
    }
    if let Some((d, _)) = day_num(b)
        && let Some(m) = t.n(i + 2).and_then(month_of)
    {
        return Some((yearly_on(m, d)?, 3));
    }
    // every <weekday>[, and ...]
    let mut days: Vec<Weekday> = Vec::new();
    let mut j = i + 1;
    let mut end = i + 1;
    while let Some(wd) = t.n(j).and_then(weekday_of) {
        if !days.contains(&wd) {
            days.push(wd);
        }
        end = j + 1;
        let mut k = j + 1;
        let mut sep = t.comma.get(j).copied().unwrap_or(false);
        while matches!(t.n(k), Some("and" | "&" | "")) {
            sep = true;
            k += 1;
        }
        if sep && t.n(k).and_then(weekday_of).is_some() {
            j = k;
        } else {
            break;
        }
    }
    if days.is_empty() {
        return None;
    }
    let mut r = Recurrence::new(Freq::Weekly);
    r.by_day = days.into_iter().map(|w| (None, w)).collect();
    Some((r, end - i))
}

fn yearly_on(month: u32, day: u32) -> Option<Recurrence> {
    // Validate against a leap year so "feb 29" is allowed.
    NaiveDate::from_ymd_opt(2024, month, day)?;
    let mut r = Recurrence::new(Freq::Yearly);
    r.by_month = vec![month];
    r.by_month_day = vec![i8::try_from(day).ok()?];
    Some(r)
}

fn parse_relative(s: &str) -> Option<Duration> {
    let unit = s.chars().last()?;
    let body = s.get(..s.len() - unit.len_utf8())?;
    if body.is_empty() || !body.bytes().all(|b| b.is_ascii_digit()) {
        return None;
    }
    let n = body.parse::<i64>().ok().filter(|n| *n > 0)?;
    match unit {
        'm' => Duration::try_minutes(n),
        'h' => Duration::try_hours(n),
        'd' => Duration::try_days(n),
        _ => None,
    }
}

fn parse_remind(t: &Toks, i: usize) -> Option<(ReminderSpec, usize)> {
    if t.n(i)? != "remind" {
        return None;
    }
    let mut j = i + 1;
    if t.n(j) == Some("me") {
        j += 1;
    }
    let d = parse_relative(t.n(j)?)?;
    j += 1;
    if t.n(j) == Some("before") {
        j += 1;
    }
    Some((ReminderSpec::Before(d), j - i))
}

/// Parse one quick-add line into task fields. `now` supplies today's date and the current time.
pub fn parse_quick_add(input: &str, now: NaiveDateTime) -> QuickAdd {
    let t = Toks::new(input);
    let today = now.date();
    let mut fields = TaskFields::default();
    let mut warnings = Vec::new();
    let mut used = vec![false; t.raw.len()];
    let mut date: Option<NaiveDate> = None;
    let mut time: Option<NaiveTime> = None;
    let mut recurrence: Option<Recurrence> = None;
    let mut projects = 0usize;

    let mut i = 0;
    while i < t.raw.len() {
        let mut consumed = 0usize;
        let raw = t.raw.get(i).map(String::as_str).unwrap_or("");
        let norm = t.n(i).unwrap_or("");

        if let Some(p) = norm
            .strip_prefix('p')
            .filter(|d| d.len() == 1)
            .and_then(|d| d.parse::<u8>().ok())
            .and_then(Priority::from_u8)
        {
            fields.priority = p;
            consumed = 1;
        } else if let Some(name) = raw
            .strip_prefix('#')
            .map(|n| n.trim_end_matches([',', ';']))
            .filter(|n| !n.is_empty())
        {
            fields.project = Some(name.to_string());
            fields.section = None;
            projects += 1;
            consumed = 1;
            if let Some(sec) = t
                .raw
                .get(i + 1)
                .and_then(|s| s.strip_prefix('/'))
                .map(|s| s.trim_end_matches([',', ';']))
                .filter(|s| !s.is_empty())
            {
                fields.section = Some(sec.to_string());
                consumed = 2;
            }
        } else if let Some(label) = raw
            .strip_prefix('@')
            .map(|n| n.trim_end_matches([',', ';']))
            .filter(|n| !n.is_empty())
        {
            fields.labels.push(label.to_string());
            consumed = 1;
        } else if let Some((r, n)) = parse_remind(&t, i) {
            fields.reminders.push(r);
            consumed = n;
        } else if recurrence.is_none()
            && let Some((r, n)) = parse_recurrence(&t, i)
        {
            recurrence = Some(r);
            consumed = n;
        } else if date.is_none()
            && let Some((d, n)) = parse_date(&t, i, today)
        {
            date = Some(d);
            consumed = n;
        } else if time.is_none()
            && let Some((tm, n)) = parse_time(&t, i)
        {
            time = Some(tm);
            consumed = n;
        }

        if consumed == 0 {
            i += 1;
        } else {
            for u in used.iter_mut().skip(i).take(consumed) {
                *u = true;
            }
            i += consumed;
        }
    }

    if projects > 1 {
        warnings.push("multiple projects given; using the last one".to_string());
    }

    fields.title = t
        .raw
        .iter()
        .zip(&used)
        .filter(|(_, u)| !**u)
        .map(|(w, _)| w.as_str())
        .collect::<Vec<_>>()
        .join(" ");
    if fields.title.is_empty() {
        warnings.push("empty title".to_string());
    }

    if let Some(rule) = recurrence {
        let start = date.unwrap_or(today);
        let occ = rule.occurrences(start, start, 2);
        let mut first = occ.first().copied();
        if date.is_none() && first == Some(today) && time.is_some_and(|tm| tm <= now.time()) {
            first = occ.get(1).copied();
        }
        match first {
            Some(d) => fields.due = Some(Due { date: d, time }),
            None => {
                warnings.push("recurrence has no upcoming occurrence".to_string());
                fields.due = Some(Due { date: start, time });
            }
        }
        fields.recurrence = Some(rule);
    } else if let Some(d) = date {
        fields.due = Some(Due { date: d, time });
    } else if let Some(tm) = time {
        let d = if tm > now.time() {
            today
        } else {
            today + Duration::days(1)
        };
        fields.due = Some(Due {
            date: d,
            time: Some(tm),
        });
    }

    if !fields.reminders.is_empty()
        && fields
            .reminders
            .iter()
            .any(|r| matches!(r, ReminderSpec::Before(_)))
        && fields.due.and_then(|d| d.time).is_none()
    {
        warnings.push("relative reminder without a due time".to_string());
    }

    QuickAdd { fields, warnings }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Monday 2026-11-16 10:00.
    fn now() -> NaiveDateTime {
        NaiveDate::from_ymd_opt(2026, 11, 16)
            .and_then(|d| d.and_hms_opt(10, 0, 0))
            .expect("valid now")
    }

    fn q(s: &str) -> QuickAdd {
        parse_quick_add(s, now())
    }

    fn due(s: &str) -> Option<Due> {
        Some(s.parse().expect("valid due"))
    }

    #[test]
    fn required_example() {
        let r = q("pay rent every 1st at 9am p1 #home");
        assert_eq!(r.fields.title, "pay rent");
        assert_eq!(r.fields.priority, Priority::P1);
        assert_eq!(r.fields.project.as_deref(), Some("home"));
        assert_eq!(
            r.fields
                .recurrence
                .as_ref()
                .map(Recurrence::to_rrule)
                .as_deref(),
            Some("FREQ=MONTHLY;BYMONTHDAY=1")
        );
        assert_eq!(r.fields.due, due("2026-12-01T09:00"));
        assert!(r.warnings.is_empty());
    }

    #[test]
    fn dates_and_times_table() {
        let cases: &[(&str, &str, Option<&str>)] = &[
            ("buy milk", "buy milk", None),
            ("call mom tomorrow", "call mom", Some("2026-11-17")),
            ("call mom tmr at 5pm", "call mom", Some("2026-11-17T17:00")),
            ("x today", "x", Some("2026-11-16")),
            ("x tod", "x", Some("2026-11-16")),
            ("report friday", "report", Some("2026-11-20")),
            ("x Fri", "x", Some("2026-11-20")),
            ("x monday", "x", Some("2026-11-23")),
            ("x next monday", "x", Some("2026-11-23")),
            ("x next friday", "x", Some("2026-11-27")),
            ("x next week", "x", Some("2026-11-23")),
            ("x in 3 days", "x", Some("2026-11-19")),
            ("x in 2 weeks", "x", Some("2026-11-30")),
            ("x in 1 month", "x", Some("2026-12-16")),
            ("x 2026-11-20", "x", Some("2026-11-20")),
            ("x nov 20", "x", Some("2026-11-20")),
            ("x 20 nov", "x", Some("2026-11-20")),
            ("x november 20th", "x", Some("2026-11-20")),
            ("x 20th November", "x", Some("2026-11-20")),
            ("x jan 5", "x", Some("2027-01-05")),
            ("x nov 10", "x", Some("2027-11-10")),
            ("x nov 16", "x", Some("2026-11-16")),
            ("x nov 20 2028", "x", Some("2028-11-20")),
            ("x at 9am", "x", Some("2026-11-17T09:00")),
            ("x 9:30pm", "x", Some("2026-11-16T21:30")),
            ("x at 14:00", "x", Some("2026-11-16T14:00")),
            ("x 14:00", "x", Some("2026-11-16T14:00")),
            ("x noon", "x", Some("2026-11-16T12:00")),
            ("x midnight", "x", Some("2026-11-17T00:00")),
            ("x at 10:00", "x", Some("2026-11-17T10:00")),
            ("x at 12am", "x", Some("2026-11-17T00:00")),
            ("x at 12pm", "x", Some("2026-11-16T12:00")),
            ("lunch friday at noon", "lunch", Some("2026-11-20T12:00")),
        ];
        for (input, title, want) in cases {
            let r = q(input);
            assert_eq!(r.fields.title, *title, "title of {input:?}");
            let want_due: Option<Due> = want.map(|s| s.parse().expect("valid"));
            assert_eq!(r.fields.due, want_due, "due of {input:?}");
            assert!(r.fields.recurrence.is_none(), "{input:?}");
        }
    }

    #[test]
    fn recurrence_table() {
        let cases: &[(&str, &str, &str, &str)] = &[
            ("a every day", "FREQ=DAILY", "2026-11-16", "a"),
            ("a daily", "FREQ=DAILY", "2026-11-16", "a"),
            (
                "a every weekday",
                "FREQ=WEEKLY;BYDAY=MO,TU,WE,TH,FR",
                "2026-11-16",
                "a",
            ),
            (
                "a every weekend",
                "FREQ=WEEKLY;BYDAY=SA,SU",
                "2026-11-21",
                "a",
            ),
            (
                "a every mon and thu",
                "FREQ=WEEKLY;BYDAY=MO,TH",
                "2026-11-16",
                "a",
            ),
            (
                "a every tue, fri",
                "FREQ=WEEKLY;BYDAY=TU,FR",
                "2026-11-17",
                "a",
            ),
            ("a every friday", "FREQ=WEEKLY;BYDAY=FR", "2026-11-20", "a"),
            (
                "a every 2 weeks",
                "FREQ=WEEKLY;INTERVAL=2",
                "2026-11-16",
                "a",
            ),
            ("a every 3 days", "FREQ=DAILY;INTERVAL=3", "2026-11-16", "a"),
            (
                "a every other week",
                "FREQ=WEEKLY;INTERVAL=2",
                "2026-11-16",
                "a",
            ),
            ("a every week", "FREQ=WEEKLY", "2026-11-16", "a"),
            ("a every month", "FREQ=MONTHLY", "2026-11-16", "a"),
            ("a every year", "FREQ=YEARLY", "2026-11-16", "a"),
            (
                "a every 15th",
                "FREQ=MONTHLY;BYMONTHDAY=15",
                "2026-12-15",
                "a",
            ),
            (
                "a every last day",
                "FREQ=MONTHLY;BYMONTHDAY=-1",
                "2026-11-30",
                "a",
            ),
            (
                "a every 2nd tue",
                "FREQ=MONTHLY;BYDAY=2TU",
                "2026-12-08",
                "a",
            ),
            (
                "a every last fri",
                "FREQ=MONTHLY;BYDAY=-1FR",
                "2026-11-27",
                "a",
            ),
            (
                "a every nov 20",
                "FREQ=YEARLY;BYMONTHDAY=20;BYMONTH=11",
                "2026-11-20",
                "a",
            ),
            (
                "a every 5 march",
                "FREQ=YEARLY;BYMONTHDAY=5;BYMONTH=3",
                "2027-03-05",
                "a",
            ),
        ];
        for (input, rrule, first_due, title) in cases {
            let r = q(input);
            assert_eq!(r.fields.title, *title, "{input:?}");
            assert_eq!(
                r.fields
                    .recurrence
                    .as_ref()
                    .map(Recurrence::to_rrule)
                    .as_deref(),
                Some(*rrule),
                "{input:?}"
            );
            assert_eq!(r.fields.due, due(first_due), "due of {input:?}");
        }
    }

    #[test]
    fn recurrence_with_time_respects_now() {
        assert_eq!(q("a every day at 9am").fields.due, due("2026-11-17T09:00"));
        assert_eq!(q("a every day at 5pm").fields.due, due("2026-11-16T17:00"));
        assert_eq!(
            q("a every monday at 9am").fields.due,
            due("2026-11-23T09:00")
        );
    }

    #[test]
    fn recurrence_with_explicit_start_date() {
        let r = q("a every 2 weeks tomorrow");
        assert_eq!(r.fields.due, due("2026-11-17"));
    }

    #[test]
    fn reminders() {
        let r = q("meeting tomorrow at 9am remind 15m before");
        assert_eq!(r.fields.title, "meeting");
        assert_eq!(r.fields.due, due("2026-11-17T09:00"));
        assert_eq!(
            r.fields.reminders,
            vec![ReminderSpec::Before(Duration::minutes(15))]
        );
        assert!(r.warnings.is_empty());
        let r = q("x tomorrow at 9am remind 1h");
        assert_eq!(
            r.fields.reminders,
            vec![ReminderSpec::Before(Duration::hours(1))]
        );
        let r = q("x remind 2d before");
        assert_eq!(
            r.fields.reminders,
            vec![ReminderSpec::Before(Duration::days(2))]
        );
        assert_eq!(r.warnings.len(), 1);
        let r = q("x remind soon");
        assert_eq!(r.fields.title, "x remind soon");
        assert!(r.fields.reminders.is_empty());
    }

    #[test]
    fn priority_tokens() {
        assert_eq!(q("a p2").fields.priority, Priority::P2);
        assert_eq!(q("P3 a").fields.priority, Priority::P3);
        assert_eq!(q("a").fields.priority, Priority::P4);
        let r = q("a p5");
        assert_eq!(r.fields.priority, Priority::P4);
        assert_eq!(r.fields.title, "a p5");
        assert_eq!(q("a p1 p3").fields.priority, Priority::P3);
    }

    #[test]
    fn projects_sections_labels() {
        let r = q("a #Home/Garden");
        assert_eq!(r.fields.project.as_deref(), Some("Home/Garden"));
        assert_eq!(r.fields.section, None);
        let r = q("a #work /Meetings @Deep @x");
        assert_eq!(r.fields.project.as_deref(), Some("work"));
        assert_eq!(r.fields.section.as_deref(), Some("Meetings"));
        assert_eq!(r.fields.labels, vec!["Deep", "x"]);
        assert_eq!(r.fields.title, "a");
        let r = q("a #one #two");
        assert_eq!(r.fields.project.as_deref(), Some("two"));
        assert_eq!(r.warnings.len(), 1);
        let r = q("a /notasection");
        assert_eq!(r.fields.section, None);
        assert_eq!(r.fields.title, "a /notasection");
        let r = q("a # b @ c");
        assert_eq!(r.fields.title, "a # b @ c");
    }

    #[test]
    fn ambiguous_words_stay_in_title() {
        for s in [
            "read Monday's paper",
            "meet in may",
            "look at the sky",
            "do every thing",
            "call 20 people",
            "ratio 3:1 notes",
            "win at 7",
            "save in 5 minutes",
            "!!! ???",
            "may the force",
            "every",
        ] {
            let r = q(s);
            assert_eq!(r.fields.title, s, "{s:?}");
            assert_eq!(r.fields.due, None, "{s:?}");
            assert!(r.fields.recurrence.is_none(), "{s:?}");
        }
    }

    #[test]
    fn empty_and_whitespace() {
        let r = q("");
        assert_eq!(r.fields.title, "");
        assert_eq!(r.warnings, vec!["empty title".to_string()]);
        assert_eq!(q("   ").fields.title, "");
        assert_eq!(q("a   b\t c").fields.title, "a b c");
        let r = q("tomorrow");
        assert_eq!(r.fields.title, "");
        assert_eq!(r.fields.due, due("2026-11-17"));
    }

    #[test]
    fn combined_line_and_case_insensitivity() {
        let r = q("Write essay P1 #School @deep TOMORROW AT 3PM");
        assert_eq!(r.fields.title, "Write essay");
        assert_eq!(r.fields.priority, Priority::P1);
        assert_eq!(r.fields.project.as_deref(), Some("School"));
        assert_eq!(r.fields.labels, vec!["deep"]);
        assert_eq!(r.fields.due, due("2026-11-17T15:00"));
    }

    #[test]
    fn only_first_date_and_time_are_used() {
        let r = q("a tomorrow friday");
        assert_eq!(r.fields.due, due("2026-11-17"));
        assert_eq!(r.fields.title, "a friday");
        let r = q("a at 9am at 11am");
        assert_eq!(r.fields.title, "a at 11am");
    }
}
