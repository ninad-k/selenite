//! A pure implementation of the RFC 5545 `RRULE` subset used by recurring tasks.
//!
//! Supported parts: `FREQ` (DAILY, WEEKLY, MONTHLY, YEARLY), `INTERVAL`, `BYDAY` (with optional
//! ordinals for monthly/yearly rules), `BYMONTHDAY`, `BYMONTH`, `COUNT` and `UNTIL`. Weeks start
//! on Monday. Occurrences are dates only; times live on the task's due value.

use std::fmt;

use chrono::{Datelike, Duration, NaiveDate, Weekday};
use serde::{Deserialize, Deserializer, Serialize, Serializer, de::Error as _};
use thiserror::Error;

/// Upper bound on the number of periods scanned, guarding against rules that never match.
const MAX_PERIODS: i64 = 200_000;

/// Recurrence frequency.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Freq {
    /// Every `interval` days.
    Daily,
    /// Every `interval` weeks.
    Weekly,
    /// Every `interval` months.
    Monthly,
    /// Every `interval` years.
    Yearly,
}

impl Freq {
    fn name(self) -> &'static str {
        match self {
            Freq::Daily => "DAILY",
            Freq::Weekly => "WEEKLY",
            Freq::Monthly => "MONTHLY",
            Freq::Yearly => "YEARLY",
        }
    }
}

/// Errors produced while parsing an `RRULE` string.
#[derive(Debug, Error, PartialEq, Eq)]
pub enum RecurrenceError {
    /// The rule text was empty.
    #[error("empty recurrence rule")]
    Empty,
    /// No `FREQ` part was present.
    #[error("recurrence rule has no FREQ")]
    MissingFreq,
    /// `FREQ` has a value this implementation does not support.
    #[error("unsupported FREQ value `{0}`")]
    UnsupportedFreq(String),
    /// A rule part this implementation does not support.
    #[error("unsupported rule part `{0}`")]
    UnsupportedPart(String),
    /// A part was not of the form `KEY=VALUE`.
    #[error("malformed rule part `{0}`")]
    Malformed(String),
    /// A value was out of range or unparsable.
    #[error("invalid value `{value}` for {key}")]
    InvalidValue {
        /// The rule key.
        key: &'static str,
        /// The offending value.
        value: String,
    },
}

/// A recurrence rule (the supported `RRULE` subset).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Recurrence {
    /// Base frequency.
    pub freq: Freq,
    /// Step between periods; at least 1.
    pub interval: u32,
    /// `BYDAY` entries: optional ordinal (1..=53 or negative from the end) and weekday.
    pub by_day: Vec<(Option<i8>, Weekday)>,
    /// `BYMONTHDAY` entries: 1..=31 or -1..=-31 (from the end of the month).
    pub by_month_day: Vec<i8>,
    /// `BYMONTH` entries: 1..=12.
    pub by_month: Vec<u32>,
    /// Maximum number of occurrences counted from `dtstart`.
    pub count: Option<u32>,
    /// Last date (inclusive) an occurrence may fall on.
    pub until: Option<NaiveDate>,
}

fn weekday_code(w: Weekday) -> &'static str {
    match w {
        Weekday::Mon => "MO",
        Weekday::Tue => "TU",
        Weekday::Wed => "WE",
        Weekday::Thu => "TH",
        Weekday::Fri => "FR",
        Weekday::Sat => "SA",
        Weekday::Sun => "SU",
    }
}

fn weekday_from_code(s: &str) -> Option<Weekday> {
    Some(match s {
        "MO" => Weekday::Mon,
        "TU" => Weekday::Tue,
        "WE" => Weekday::Wed,
        "TH" => Weekday::Thu,
        "FR" => Weekday::Fri,
        "SA" => Weekday::Sat,
        "SU" => Weekday::Sun,
        _ => return None,
    })
}

fn is_leap(y: i32) -> bool {
    (y % 4 == 0 && y % 100 != 0) || y % 400 == 0
}

fn days_in_month(y: i32, m: u32) -> u32 {
    match m {
        1 | 3 | 5 | 7 | 8 | 10 | 12 => 31,
        4 | 6 | 9 | 11 => 30,
        _ => {
            if is_leap(y) {
                29
            } else {
                28
            }
        }
    }
}

fn invalid(key: &'static str, value: &str) -> RecurrenceError {
    RecurrenceError::InvalidValue {
        key,
        value: value.to_string(),
    }
}

fn parse_until(v: &str) -> Result<NaiveDate, RecurrenceError> {
    let err = || invalid("UNTIL", v);
    let date_part = v.get(..8).ok_or_else(err)?;
    let rest = v.get(8..).ok_or_else(err)?;
    let date = NaiveDate::parse_from_str(date_part, "%Y%m%d").map_err(|_| err())?;
    if !rest.is_empty() {
        let body = rest.strip_prefix(['T', 't']).ok_or_else(err)?;
        let body = body.strip_suffix(['Z', 'z']).unwrap_or(body);
        if body.len() != 6 || !body.bytes().all(|b| b.is_ascii_digit()) {
            return Err(err());
        }
    }
    Ok(date)
}

impl Recurrence {
    /// A rule with the given frequency, interval 1 and no restrictions.
    pub fn new(freq: Freq) -> Self {
        Recurrence {
            freq,
            interval: 1,
            by_day: Vec::new(),
            by_month_day: Vec::new(),
            by_month: Vec::new(),
            count: None,
            until: None,
        }
    }

    /// Parse an `RRULE` value, with or without the `RRULE:` prefix.
    ///
    /// # Errors
    /// Returns a [`RecurrenceError`] for missing, unsupported or invalid parts.
    pub fn parse(rrule: &str) -> Result<Self, RecurrenceError> {
        let mut s = rrule.trim();
        if let Some(prefix) = s.get(..6)
            && prefix.eq_ignore_ascii_case("RRULE:")
        {
            s = s.get(6..).unwrap_or("").trim();
        }
        if s.is_empty() {
            return Err(RecurrenceError::Empty);
        }
        let mut freq = None;
        let mut rule = Recurrence::new(Freq::Daily);
        for part in s.split(';').map(str::trim).filter(|p| !p.is_empty()) {
            let (key, value) = part
                .split_once('=')
                .ok_or_else(|| RecurrenceError::Malformed(part.to_string()))?;
            let value = value.trim();
            match key.trim().to_ascii_uppercase().as_str() {
                "FREQ" => {
                    freq = Some(match value.to_ascii_uppercase().as_str() {
                        "DAILY" => Freq::Daily,
                        "WEEKLY" => Freq::Weekly,
                        "MONTHLY" => Freq::Monthly,
                        "YEARLY" => Freq::Yearly,
                        _ => return Err(RecurrenceError::UnsupportedFreq(value.to_string())),
                    });
                }
                "INTERVAL" => {
                    rule.interval = value
                        .parse::<u32>()
                        .ok()
                        .filter(|n| *n >= 1)
                        .ok_or_else(|| invalid("INTERVAL", value))?;
                }
                "BYDAY" => {
                    for item in value.split(',') {
                        rule.by_day.push(Self::parse_byday(item.trim())?);
                    }
                }
                "BYMONTHDAY" => {
                    for item in value.split(',') {
                        let n = item
                            .trim()
                            .parse::<i8>()
                            .ok()
                            .filter(|n| *n != 0 && n.unsigned_abs() <= 31)
                            .ok_or_else(|| invalid("BYMONTHDAY", value))?;
                        rule.by_month_day.push(n);
                    }
                }
                "BYMONTH" => {
                    for item in value.split(',') {
                        let n = item
                            .trim()
                            .parse::<u32>()
                            .ok()
                            .filter(|n| (1..=12).contains(n))
                            .ok_or_else(|| invalid("BYMONTH", value))?;
                        rule.by_month.push(n);
                    }
                }
                "COUNT" => {
                    rule.count = Some(
                        value
                            .parse::<u32>()
                            .ok()
                            .filter(|n| *n >= 1)
                            .ok_or_else(|| invalid("COUNT", value))?,
                    );
                }
                "UNTIL" => rule.until = Some(parse_until(value)?),
                _ => return Err(RecurrenceError::UnsupportedPart(part.to_string())),
            }
        }
        rule.freq = freq.ok_or(RecurrenceError::MissingFreq)?;
        if matches!(rule.freq, Freq::Daily | Freq::Weekly)
            && rule.by_day.iter().any(|(o, _)| o.is_some())
        {
            return Err(invalid("BYDAY", "ordinal not allowed for DAILY/WEEKLY"));
        }
        Ok(rule)
    }

    fn parse_byday(item: &str) -> Result<(Option<i8>, Weekday), RecurrenceError> {
        let err = || invalid("BYDAY", item);
        let split = item.len().checked_sub(2).ok_or_else(err)?;
        let prefix = item.get(..split).ok_or_else(err)?;
        let code = item.get(split..).ok_or_else(err)?.to_ascii_uppercase();
        let wd = weekday_from_code(&code).ok_or_else(err)?;
        if prefix.is_empty() {
            return Ok((None, wd));
        }
        let n = prefix
            .parse::<i8>()
            .ok()
            .filter(|n| *n != 0 && n.unsigned_abs() <= 53)
            .ok_or_else(err)?;
        Ok((Some(n), wd))
    }

    /// Canonical `RRULE` text (no prefix):
    /// `FREQ;INTERVAL(if >1);BYDAY;BYMONTHDAY;BYMONTH;COUNT;UNTIL`.
    pub fn to_rrule(&self) -> String {
        let mut parts = vec![format!("FREQ={}", self.freq.name())];
        if self.interval > 1 {
            parts.push(format!("INTERVAL={}", self.interval));
        }
        if !self.by_day.is_empty() {
            let items: Vec<String> = self
                .by_day
                .iter()
                .map(|(o, w)| match o {
                    Some(n) => format!("{n}{}", weekday_code(*w)),
                    None => weekday_code(*w).to_string(),
                })
                .collect();
            parts.push(format!("BYDAY={}", items.join(",")));
        }
        if !self.by_month_day.is_empty() {
            let items: Vec<String> = self.by_month_day.iter().map(i8::to_string).collect();
            parts.push(format!("BYMONTHDAY={}", items.join(",")));
        }
        if !self.by_month.is_empty() {
            let items: Vec<String> = self.by_month.iter().map(u32::to_string).collect();
            parts.push(format!("BYMONTH={}", items.join(",")));
        }
        if let Some(c) = self.count {
            parts.push(format!("COUNT={c}"));
        }
        if let Some(u) = self.until {
            parts.push(format!("UNTIL={}", u.format("%Y%m%d")));
        }
        parts.join(";")
    }

    /// Occurrences on or after `from`, anchored at `dtstart`, in ascending order.
    ///
    /// `COUNT` is counted from `dtstart` (not from `from`); `UNTIL` is inclusive. Returns at most
    /// `limit` dates. Scanning is capped so a rule that never matches cannot loop forever.
    pub fn occurrences(&self, dtstart: NaiveDate, from: NaiveDate, limit: usize) -> Vec<NaiveDate> {
        let mut out = Vec::new();
        if limit == 0 {
            return out;
        }
        let interval = i64::from(self.interval.max(1));
        let mut emitted: u32 = 0;
        for k in 0..MAX_PERIODS {
            let Some(dates) = self.period_dates(dtstart, k, interval) else {
                break;
            };
            for d in dates {
                if d < dtstart {
                    continue;
                }
                if self.until.is_some_and(|u| d > u) {
                    return out;
                }
                if self.count.is_some_and(|c| emitted >= c) {
                    return out;
                }
                emitted = emitted.saturating_add(1);
                if d >= from {
                    out.push(d);
                    if out.len() >= limit {
                        return out;
                    }
                }
            }
        }
        out
    }

    /// The first occurrence strictly after `after`, anchored at `dtstart`.
    pub fn next_after(&self, dtstart: NaiveDate, after: NaiveDate) -> Option<NaiveDate> {
        let from = after.succ_opt()?;
        self.occurrences(dtstart, from, 1).into_iter().next()
    }

    fn month_ok(&self, m: u32) -> bool {
        self.by_month.is_empty() || self.by_month.contains(&m)
    }

    fn month_day_ok(&self, d: NaiveDate) -> bool {
        if self.by_month_day.is_empty() {
            return true;
        }
        let dim = days_in_month(d.year(), d.month());
        self.by_month_day
            .iter()
            .any(|n| resolve_month_day(*n, dim) == Some(d.day()))
    }

    /// Sorted, de-duplicated candidate dates of period `k`; `None` when dates overflow.
    fn period_dates(&self, dtstart: NaiveDate, k: i64, interval: i64) -> Option<Vec<NaiveDate>> {
        let step = k.checked_mul(interval)?;
        let mut dates = match self.freq {
            Freq::Daily => {
                let d = dtstart.checked_add_signed(Duration::try_days(step)?)?;
                let ok = self.month_ok(d.month())
                    && self.month_day_ok(d)
                    && (self.by_day.is_empty()
                        || self.by_day.iter().any(|(_, w)| *w == d.weekday()));
                if ok { vec![d] } else { Vec::new() }
            }
            Freq::Weekly => {
                let back = i64::from(dtstart.weekday().num_days_from_monday());
                let week_start = dtstart.checked_sub_signed(Duration::try_days(back)?)?;
                let ws = week_start.checked_add_signed(Duration::try_weeks(step)?)?;
                let offsets: Vec<i64> = if self.by_day.is_empty() {
                    vec![back]
                } else {
                    self.by_day
                        .iter()
                        .map(|(_, w)| i64::from(w.num_days_from_monday()))
                        .collect()
                };
                let mut v = Vec::new();
                for off in offsets {
                    let d = ws.checked_add_signed(Duration::try_days(off)?)?;
                    if self.month_ok(d.month()) && self.month_day_ok(d) {
                        v.push(d);
                    }
                }
                v
            }
            Freq::Monthly => {
                let idx = i64::from(dtstart.year())
                    .checked_mul(12)?
                    .checked_add(i64::from(dtstart.month0()))?
                    .checked_add(step)?;
                let y = i32::try_from(idx.div_euclid(12)).ok()?;
                let m = u32::try_from(idx.rem_euclid(12)).ok()? + 1;
                NaiveDate::from_ymd_opt(y, m, 1)?;
                if self.month_ok(m) {
                    self.month_dates(y, m, Some(dtstart.day()))
                } else {
                    Vec::new()
                }
            }
            Freq::Yearly => {
                let y = i32::try_from(i64::from(dtstart.year()).checked_add(step)?).ok()?;
                NaiveDate::from_ymd_opt(y, 1, 1)?;
                let plain = self.by_month_day.is_empty() && self.by_day.is_empty();
                let months: Vec<u32> = if !self.by_month.is_empty() {
                    self.by_month.clone()
                } else if plain {
                    vec![dtstart.month()]
                } else if !self.by_month_day.is_empty() {
                    (1..=12).collect()
                } else {
                    Vec::new()
                };
                if months.is_empty() {
                    self.year_by_day(y)
                } else {
                    let default_day = if plain { Some(dtstart.day()) } else { None };
                    months
                        .into_iter()
                        .flat_map(|m| self.month_dates(y, m, default_day))
                        .collect()
                }
            }
        };
        dates.sort();
        dates.dedup();
        Some(dates)
    }

    /// Dates inside one month according to BYMONTHDAY / BYDAY, or `default_day` if neither is set.
    fn month_dates(&self, y: i32, m: u32, default_day: Option<u32>) -> Vec<NaiveDate> {
        let dim = days_in_month(y, m);
        let by_day_days: Vec<u32> = self
            .by_day
            .iter()
            .flat_map(|(ord, wd)| {
                let matching: Vec<u32> = (1..=dim)
                    .filter(|d| {
                        NaiveDate::from_ymd_opt(y, m, *d).is_some_and(|x| x.weekday() == *wd)
                    })
                    .collect();
                match ord {
                    None => matching,
                    Some(n) => pick_nth(&matching, *n).into_iter().collect(),
                }
            })
            .collect();
        let days: Vec<u32> = if !self.by_month_day.is_empty() {
            let md: Vec<u32> = self
                .by_month_day
                .iter()
                .filter_map(|n| resolve_month_day(*n, dim))
                .collect();
            if self.by_day.is_empty() {
                md
            } else {
                md.into_iter().filter(|d| by_day_days.contains(d)).collect()
            }
        } else if !self.by_day.is_empty() {
            by_day_days
        } else {
            default_day.filter(|d| *d <= dim).into_iter().collect()
        };
        days.into_iter()
            .filter_map(|d| NaiveDate::from_ymd_opt(y, m, d))
            .collect()
    }

    /// BYDAY expansion across a whole year (yearly rule with no BYMONTH/BYMONTHDAY).
    fn year_by_day(&self, y: i32) -> Vec<NaiveDate> {
        let mut all = Vec::new();
        for m in 1..=12 {
            for d in 1..=days_in_month(y, m) {
                if let Some(date) = NaiveDate::from_ymd_opt(y, m, d) {
                    all.push(date);
                }
            }
        }
        let mut out = Vec::new();
        for (ord, wd) in &self.by_day {
            let matching: Vec<NaiveDate> =
                all.iter().copied().filter(|d| d.weekday() == *wd).collect();
            match ord {
                None => out.extend(matching),
                Some(n) => out.extend(pick_nth(&matching, *n)),
            }
        }
        out
    }
}

fn resolve_month_day(n: i8, dim: u32) -> Option<u32> {
    let abs = u32::from(n.unsigned_abs());
    if abs == 0 || abs > dim {
        return None;
    }
    Some(if n > 0 { abs } else { dim + 1 - abs })
}

fn pick_nth<T: Copy>(items: &[T], n: i8) -> Option<T> {
    let abs = usize::from(n.unsigned_abs());
    if abs == 0 {
        return None;
    }
    if n > 0 {
        items.get(abs - 1).copied()
    } else {
        items
            .len()
            .checked_sub(abs)
            .and_then(|i| items.get(i))
            .copied()
    }
}

impl fmt::Display for Recurrence {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.to_rrule())
    }
}

impl std::str::FromStr for Recurrence {
    type Err = RecurrenceError;

    fn from_str(s: &str) -> Result<Self, Self::Err> {
        Recurrence::parse(s)
    }
}

impl Serialize for Recurrence {
    fn serialize<S: Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
        serializer.serialize_str(&self.to_rrule())
    }
}

impl<'de> Deserialize<'de> for Recurrence {
    fn deserialize<D: Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        let s = String::deserialize(deserializer)?;
        Recurrence::parse(&s).map_err(D::Error::custom)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn d(y: i32, m: u32, day: u32) -> NaiveDate {
        NaiveDate::from_ymd_opt(y, m, day).expect("valid test date")
    }

    fn occ(rule: &str, start: NaiveDate, n: usize) -> Vec<NaiveDate> {
        Recurrence::parse(rule)
            .expect("valid rule")
            .occurrences(start, start, n)
    }

    #[test]
    fn daily_count_10() {
        let got = occ("FREQ=DAILY;COUNT=10", d(1997, 9, 2), 100);
        let want: Vec<_> = (2..=11).map(|x| d(1997, 9, x)).collect();
        assert_eq!(got, want);
    }

    #[test]
    fn every_other_week_tu_th() {
        let got = occ("FREQ=WEEKLY;INTERVAL=2;BYDAY=TU,TH", d(1997, 9, 2), 8);
        let want = vec![
            d(1997, 9, 2),
            d(1997, 9, 4),
            d(1997, 9, 16),
            d(1997, 9, 18),
            d(1997, 9, 30),
            d(1997, 10, 2),
            d(1997, 10, 14),
            d(1997, 10, 16),
        ];
        assert_eq!(got, want);
    }

    #[test]
    fn monthly_first_friday() {
        let got = occ("FREQ=MONTHLY;COUNT=10;BYDAY=1FR", d(1997, 9, 5), 100);
        let want = vec![
            d(1997, 9, 5),
            d(1997, 10, 3),
            d(1997, 11, 7),
            d(1997, 12, 5),
            d(1998, 1, 2),
            d(1998, 2, 6),
            d(1998, 3, 6),
            d(1998, 4, 3),
            d(1998, 5, 1),
            d(1998, 6, 5),
        ];
        assert_eq!(got, want);
    }

    #[test]
    fn last_day_of_month() {
        let got = occ("FREQ=MONTHLY;BYMONTHDAY=-1", d(1997, 9, 30), 6);
        let want = vec![
            d(1997, 9, 30),
            d(1997, 10, 31),
            d(1997, 11, 30),
            d(1997, 12, 31),
            d(1998, 1, 31),
            d(1998, 2, 28),
        ];
        assert_eq!(got, want);
    }

    #[test]
    fn yearly_feb_29_skips_non_leap_years() {
        let got = occ("FREQ=YEARLY", d(2024, 2, 29), 3);
        assert_eq!(got, vec![d(2024, 2, 29), d(2028, 2, 29), d(2032, 2, 29)]);
    }

    #[test]
    fn weekdays() {
        let got = occ("FREQ=WEEKLY;BYDAY=MO,TU,WE,TH,FR", d(1997, 9, 2), 7);
        let want = vec![
            d(1997, 9, 2),
            d(1997, 9, 3),
            d(1997, 9, 4),
            d(1997, 9, 5),
            d(1997, 9, 8),
            d(1997, 9, 9),
            d(1997, 9, 10),
        ];
        assert_eq!(got, want);
    }

    #[test]
    fn monthly_day_31_skips_short_months() {
        let got = occ("FREQ=MONTHLY;BYMONTHDAY=31", d(2026, 1, 31), 4);
        assert_eq!(
            got,
            vec![
                d(2026, 1, 31),
                d(2026, 3, 31),
                d(2026, 5, 31),
                d(2026, 7, 31)
            ]
        );
    }

    #[test]
    fn monthly_last_friday_and_yearly_by_month() {
        let got = occ("FREQ=MONTHLY;BYDAY=-1FR", d(2026, 11, 1), 2);
        assert_eq!(got, vec![d(2026, 11, 27), d(2026, 12, 25)]);
        let got = occ("FREQ=YEARLY;BYMONTH=3;BYMONTHDAY=15", d(2026, 1, 1), 2);
        assert_eq!(got, vec![d(2026, 3, 15), d(2027, 3, 15)]);
    }

    #[test]
    fn until_inclusive_and_count_from_dtstart() {
        let got = occ("FREQ=DAILY;UNTIL=20260105", d(2026, 1, 3), 100);
        assert_eq!(got, vec![d(2026, 1, 3), d(2026, 1, 4), d(2026, 1, 5)]);
        let r = Recurrence::parse("FREQ=DAILY;COUNT=3").expect("valid");
        assert_eq!(
            r.occurrences(d(2026, 1, 1), d(2026, 1, 2), 10),
            vec![d(2026, 1, 2), d(2026, 1, 3)]
        );
        assert_eq!(r.next_after(d(2026, 1, 1), d(2026, 1, 3)), None);
    }

    #[test]
    fn next_after_is_strict() {
        let r = Recurrence::parse("FREQ=MONTHLY;BYMONTHDAY=1").expect("valid");
        assert_eq!(
            r.next_after(d(2026, 11, 1), d(2026, 11, 1)),
            Some(d(2026, 12, 1))
        );
        assert_eq!(
            r.next_after(d(2026, 11, 1), d(2026, 11, 20)),
            Some(d(2026, 12, 1))
        );
    }

    #[test]
    fn never_matching_rule_terminates() {
        let r = Recurrence::parse("FREQ=YEARLY;BYMONTH=2;BYMONTHDAY=30").expect("valid");
        assert!(r.occurrences(d(2026, 1, 1), d(2026, 1, 1), 5).is_empty());
        assert!(r.occurrences(d(2026, 1, 1), d(2026, 1, 1), 0).is_empty());
    }

    #[test]
    fn parse_prefix_until_formats_and_roundtrip() {
        let rules = [
            "RRULE:FREQ=DAILY",
            "FREQ=WEEKLY;INTERVAL=2;BYDAY=TU,TH;COUNT=5",
            "FREQ=MONTHLY;BYDAY=1MO,-1FR",
            "FREQ=MONTHLY;BYMONTHDAY=1,15,-1",
            "FREQ=YEARLY;BYMONTH=2,3;BYMONTHDAY=29;UNTIL=20300101",
            "freq=yearly;until=20301231T235959Z",
        ];
        for text in rules {
            let r = Recurrence::parse(text).expect("parses");
            let again = Recurrence::parse(&r.to_rrule()).expect("canonical reparses");
            assert_eq!(r, again, "{text}");
        }
        let r = Recurrence::parse("FREQ=MONTHLY;BYMONTHDAY=1").expect("valid");
        assert_eq!(r.to_rrule(), "FREQ=MONTHLY;BYMONTHDAY=1");
        let r = Recurrence::parse("COUNT=2;BYDAY=MO;INTERVAL=3;FREQ=WEEKLY").expect("valid");
        assert_eq!(r.to_rrule(), "FREQ=WEEKLY;INTERVAL=3;BYDAY=MO;COUNT=2");
    }

    #[test]
    fn parse_errors() {
        assert_eq!(Recurrence::parse("  "), Err(RecurrenceError::Empty));
        assert_eq!(Recurrence::parse("RRULE:"), Err(RecurrenceError::Empty));
        assert_eq!(
            Recurrence::parse("INTERVAL=2"),
            Err(RecurrenceError::MissingFreq)
        );
        assert!(matches!(
            Recurrence::parse("FREQ=HOURLY"),
            Err(RecurrenceError::UnsupportedFreq(_))
        ));
        assert!(matches!(
            Recurrence::parse("FREQ=DAILY;BYSETPOS=1"),
            Err(RecurrenceError::UnsupportedPart(_))
        ));
        assert!(matches!(
            Recurrence::parse("FREQ"),
            Err(RecurrenceError::Malformed(_))
        ));
        for bad in [
            "FREQ=DAILY;INTERVAL=0",
            "FREQ=DAILY;COUNT=0",
            "FREQ=MONTHLY;BYMONTHDAY=0",
            "FREQ=MONTHLY;BYMONTHDAY=32",
            "FREQ=YEARLY;BYMONTH=13",
            "FREQ=WEEKLY;BYDAY=XX",
            "FREQ=WEEKLY;BYDAY=1MO",
            "FREQ=DAILY;UNTIL=2026",
            "FREQ=DAILY;UNTIL=20261301",
            "FREQ=DAILY;BYDAY=é",
        ] {
            assert!(
                matches!(
                    Recurrence::parse(bad),
                    Err(RecurrenceError::InvalidValue { .. })
                ),
                "{bad}"
            );
        }
    }
}
