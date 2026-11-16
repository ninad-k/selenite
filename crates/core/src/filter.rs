//! Task filter query language: parse a text expression into a [`Filter`] and match tasks.
//!
//! Grammar: operators `&` (and), `|` (or), `!` (not) and parentheses; precedence `!` > `&` > `|`.
//! Terms are case-insensitive words or phrases. A phrase runs up to the next operator or
//! parenthesis, so multi-word terms such as `no date & p1` need no quoting. Double quotes protect
//! operator characters and spaces inside a term (`#"Big Project"`, `search: "a & b"`).
//!
//! | Term | Meaning |
//! |------|---------|
//! | `today` | due date <= today (includes overdue) |
//! | `overdue` | due date < today (date only; times are ignored) |
//! | `tomorrow` | due date is today + 1 |
//! | `no date` / `has date` | without / with a due date |
//! | `recurring` | has a recurrence rule |
//! | `p1`..`p4` | exact priority |
//! | `#Home` / `##Home` | project `Home` and its `Home/*` sub-projects / exactly `Home` |
//! | `inbox` | no project |
//! | `@errand` | has the label |
//! | `/Groceries` | in the section |
//! | `next N days` | today <= due < today + N |
//! | `due before: D` / `due after: D` | strictly before / after the date `D` |
//! | `2026-11-20`, `nov 20`, `20 nov` | due on that date (year-less dates roll forward) |
//! | `search: text` | title contains the text |
//! | `done` / `completed`, `open` | completed / not completed |
//! | `subtask: Parent` | the task's parent is `Parent` |
//! | `all`, `*` | every task |
//!
//! Views do not exclude completed tasks on their own (except [`BuiltinView::Completed`] and
//! [`BuiltinView::All`], which are explicit): callers combine terms with `& open`.

use chrono::{Datelike, Days, NaiveDate};
use thiserror::Error;

use crate::task::{Priority, TaskFields};

/// A parsed filter expression.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Filter {
    /// Matches every task.
    All,
    /// A single term.
    Term(Term),
    /// Negation.
    Not(Box<Filter>),
    /// Both sides match.
    And(Box<Filter>, Box<Filter>),
    /// Either side matches.
    Or(Box<Filter>, Box<Filter>),
}

/// A leaf condition of a [`Filter`]. Dates are already resolved against the `today` given at parse time.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Term {
    /// Due date <= today, so overdue tasks are included.
    Today,
    /// Due date < today (date only; the time of day is ignored).
    Overdue,
    /// Due date is today + 1.
    Tomorrow,
    /// No due date.
    NoDate,
    /// Has a due date.
    HasDate,
    /// Has a recurrence rule.
    Recurring,
    /// Exactly this priority.
    Priority(Priority),
    /// `#Home`: project `Home` or any `Home/...` sub-project, case-insensitive.
    Project(String),
    /// `##Home`: exactly project `Home`, case-insensitive.
    ProjectExact(String),
    /// No project.
    Inbox,
    /// `@errand`: has the label (case-insensitive).
    Label(String),
    /// `/Groceries`: in the section (case-insensitive).
    Section(String),
    /// `next N days`: today <= due < today + N.
    NextDays(u32),
    /// Due date strictly before the date.
    DueBefore(NaiveDate),
    /// Due date strictly after the date.
    DueAfter(NaiveDate),
    /// Due on exactly this date.
    DueOn(NaiveDate),
    /// Title contains the text (case-insensitive).
    Search(String),
    /// Completed.
    Done,
    /// Not completed (`!done`).
    Open,
    /// The task's parent is this task (case-insensitive; wikilink brackets and aliases ignored).
    SubtaskOf(String),
}

/// Errors from [`Filter::parse`].
#[derive(Debug, Error, PartialEq, Eq)]
pub enum FilterError {
    /// The expression is empty or only whitespace.
    #[error("filter is empty")]
    Empty,
    /// A token appeared where it is not allowed (`found` is `end of input` for a dangling operator).
    #[error("unexpected `{found}` at position {pos}")]
    UnexpectedToken {
        /// Byte offset of the token in the input.
        pos: usize,
        /// The offending token text.
        found: String,
    },
    /// A parenthesis has no partner.
    #[error("unbalanced parentheses")]
    UnbalancedParens,
    /// A phrase is not a known term or has an invalid value.
    #[error("unknown filter term `{0}`")]
    UnknownTerm(String),
}

/// Built-in task views, expressed as filters by [`builtin`].
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum BuiltinView {
    /// Open tasks without a project.
    Inbox,
    /// Open tasks due today or overdue.
    Today,
    /// Open tasks due within the next 7 days (overdue excluded).
    Upcoming,
    /// Completed tasks.
    Completed,
    /// Every task, completed or not.
    All,
}

/// The filter behind a built-in view. `today` is accepted for symmetry with [`Filter::parse`];
/// the views are resolved when matching ([`Filter::matches`]), so it is not needed to build them.
pub fn builtin(view: BuiltinView, _today: NaiveDate) -> Filter {
    let and = |a: Term, b: Term| Filter::And(Box::new(Filter::Term(a)), Box::new(Filter::Term(b)));
    match view {
        BuiltinView::Inbox => and(Term::Inbox, Term::Open),
        BuiltinView::Today => and(Term::Today, Term::Open),
        BuiltinView::Upcoming => and(Term::NextDays(7), Term::Open),
        BuiltinView::Completed => Filter::Term(Term::Done),
        BuiltinView::All => Filter::All,
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
enum Tok {
    And,
    Or,
    Not,
    LParen,
    RParen,
    Phrase(String),
}

fn tokenize(input: &str) -> Result<Vec<(usize, Tok)>, FilterError> {
    let mut out = Vec::new();
    let mut chars = input.char_indices().peekable();
    while let Some(&(pos, c)) = chars.peek() {
        let op = match c {
            '&' => Some(Tok::And),
            '|' => Some(Tok::Or),
            '!' => Some(Tok::Not),
            '(' => Some(Tok::LParen),
            ')' => Some(Tok::RParen),
            _ => None,
        };
        if let Some(t) = op {
            out.push((pos, t));
            chars.next();
        } else if c.is_whitespace() {
            chars.next();
        } else {
            let mut text = String::new();
            while let Some(&(p, ch)) = chars.peek() {
                if matches!(ch, '&' | '|' | '!' | '(' | ')') {
                    break;
                }
                chars.next();
                if ch == '"' {
                    let mut closed = false;
                    for (_, q) in chars.by_ref() {
                        if q == '"' {
                            closed = true;
                            break;
                        }
                        text.push(q);
                    }
                    if !closed {
                        return Err(FilterError::UnexpectedToken {
                            pos: p,
                            found: "\"".to_string(),
                        });
                    }
                } else {
                    text.push(ch);
                }
            }
            out.push((pos, Tok::Phrase(text.trim().to_string())));
        }
    }
    Ok(out)
}

struct Parser<'a> {
    toks: &'a [(usize, Tok)],
    i: usize,
    end: usize,
    today: NaiveDate,
}

impl Parser<'_> {
    fn peek(&self) -> Option<&Tok> {
        self.toks.get(self.i).map(|(_, t)| t)
    }

    fn unexpected(&self) -> FilterError {
        match self.toks.get(self.i) {
            Some((pos, t)) => FilterError::UnexpectedToken {
                pos: *pos,
                found: match t {
                    Tok::And => "&".to_string(),
                    Tok::Or => "|".to_string(),
                    Tok::Not => "!".to_string(),
                    Tok::LParen => "(".to_string(),
                    Tok::RParen => ")".to_string(),
                    Tok::Phrase(p) => p.clone(),
                },
            },
            None => FilterError::UnexpectedToken {
                pos: self.end,
                found: "end of input".to_string(),
            },
        }
    }

    fn or(&mut self) -> Result<Filter, FilterError> {
        let mut left = self.and()?;
        while self.peek() == Some(&Tok::Or) {
            self.i += 1;
            let right = self.and()?;
            left = Filter::Or(Box::new(left), Box::new(right));
        }
        Ok(left)
    }

    fn and(&mut self) -> Result<Filter, FilterError> {
        let mut left = self.not()?;
        while self.peek() == Some(&Tok::And) {
            self.i += 1;
            let right = self.not()?;
            left = Filter::And(Box::new(left), Box::new(right));
        }
        Ok(left)
    }

    fn not(&mut self) -> Result<Filter, FilterError> {
        if self.peek() == Some(&Tok::Not) {
            self.i += 1;
            return Ok(Filter::Not(Box::new(self.not()?)));
        }
        self.atom()
    }

    fn atom(&mut self) -> Result<Filter, FilterError> {
        match self.peek() {
            Some(Tok::LParen) => {
                self.i += 1;
                if self.peek().is_none() {
                    return Err(FilterError::UnbalancedParens);
                }
                let inner = self.or()?;
                if self.peek() == Some(&Tok::RParen) {
                    self.i += 1;
                    Ok(inner)
                } else if self.peek().is_none() {
                    Err(FilterError::UnbalancedParens)
                } else {
                    Err(self.unexpected())
                }
            }
            Some(Tok::Phrase(p)) => {
                let f = parse_phrase(p, self.today)?;
                self.i += 1;
                Ok(f)
            }
            _ => Err(self.unexpected()),
        }
    }
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

/// Day number with an optional ordinal suffix (`20`, `20th`).
fn day_num(s: &str) -> Option<u32> {
    let digits = ["st", "nd", "rd", "th"]
        .iter()
        .find_map(|suf| s.strip_suffix(suf))
        .unwrap_or(s);
    if digits.is_empty() || !digits.bytes().all(|b| b.is_ascii_digit()) {
        return None;
    }
    digits.parse().ok().filter(|d| (1..=31).contains(d))
}

fn year_num(s: &str) -> Option<i32> {
    if s.len() == 4 && s.bytes().all(|b| b.is_ascii_digit()) {
        s.parse().ok()
    } else {
        None
    }
}

/// Month/day without a year: the date this year if it is not in the past, otherwise next year
/// (the same convention as quick add).
fn resolve_month_day(
    today: NaiveDate,
    month: u32,
    day: u32,
    year: Option<i32>,
) -> Option<NaiveDate> {
    match year {
        Some(y) => NaiveDate::from_ymd_opt(y, month, day),
        None => match NaiveDate::from_ymd_opt(today.year(), month, day) {
            Some(d) if d >= today => Some(d),
            _ => NaiveDate::from_ymd_opt(today.year() + 1, month, day),
        },
    }
}

/// Parse an ISO date, `nov 20` or `20 nov` (each with an optional 4-digit year).
fn parse_date(text: &str, today: NaiveDate) -> Option<NaiveDate> {
    if let Ok(d) = NaiveDate::parse_from_str(text, "%Y-%m-%d") {
        return Some(d);
    }
    let words: Vec<&str> = text.split_whitespace().collect();
    let (month, day, rest) = match words.as_slice() {
        [a, b, rest @ ..] => {
            if let (Some(m), Some(d)) = (month_of(a), day_num(b)) {
                (m, d, rest)
            } else if let (Some(d), Some(m)) = (day_num(a), month_of(b)) {
                (m, d, rest)
            } else {
                return None;
            }
        }
        _ => return None,
    };
    let year = match rest {
        [] => None,
        [y] => Some(year_num(y)?),
        _ => return None,
    };
    resolve_month_day(today, month, day, year)
}

fn parse_phrase(raw: &str, today: NaiveDate) -> Result<Filter, FilterError> {
    let unknown = || FilterError::UnknownTerm(raw.to_string());
    let norm = raw.split_whitespace().collect::<Vec<_>>().join(" ");
    let lower = norm.to_lowercase();
    if lower.is_empty() {
        return Err(unknown());
    }
    let nonempty = |s: &str| -> Result<String, FilterError> {
        if s.is_empty() {
            Err(unknown())
        } else {
            Ok(s.to_string())
        }
    };
    let term = |t: Term| Ok(Filter::Term(t));

    // Prefixed forms keep the original case of their value.
    if let Some(v) = norm.strip_prefix("##") {
        return term(Term::ProjectExact(nonempty(v.trim())?));
    }
    if let Some(v) = norm.strip_prefix('#') {
        return term(Term::Project(nonempty(v.trim())?));
    }
    if let Some(v) = norm.strip_prefix('@') {
        return term(Term::Label(nonempty(v.trim())?));
    }
    if let Some(v) = norm.strip_prefix('/') {
        return term(Term::Section(nonempty(v.trim())?));
    }
    // Keyword prefixes are ASCII, so byte lengths match between `lower` and `norm`
    // unless lower-casing changed the length; `get` keeps that case panic-free.
    for (kw, kind) in [
        ("search:", 0),
        ("subtask:", 1),
        ("due before:", 2),
        ("due after:", 3),
    ] {
        if lower.starts_with(kw) {
            let v = norm.get(kw.len()..).unwrap_or("").trim();
            return match kind {
                0 => term(Term::Search(nonempty(v)?)),
                1 => term(Term::SubtaskOf(nonempty(v)?)),
                _ => {
                    let d = parse_date(&v.to_lowercase(), today).ok_or_else(unknown)?;
                    term(if kind == 2 {
                        Term::DueBefore(d)
                    } else {
                        Term::DueAfter(d)
                    })
                }
            };
        }
    }
    match lower.as_str() {
        "all" | "*" => return Ok(Filter::All),
        "today" => return term(Term::Today),
        "overdue" => return term(Term::Overdue),
        "tomorrow" => return term(Term::Tomorrow),
        "no date" | "no due date" => return term(Term::NoDate),
        "has date" | "has due date" => return term(Term::HasDate),
        "recurring" => return term(Term::Recurring),
        "inbox" => return term(Term::Inbox),
        "done" | "completed" => return term(Term::Done),
        "open" => return term(Term::Open),
        _ => {}
    }
    if lower.starts_with('p')
        && let Ok(p) = lower.parse::<Priority>()
    {
        return term(Term::Priority(p));
    }
    if let [a, n, b] = lower.split_whitespace().collect::<Vec<_>>().as_slice()
        && *a == "next"
        && matches!(*b, "day" | "days")
        && let Ok(n) = n.parse::<u32>()
    {
        return term(Term::NextDays(n));
    }
    if let Some(d) = parse_date(&lower, today) {
        return term(Term::DueOn(d));
    }
    Err(unknown())
}

/// Strip wikilink brackets and alias (`[[Parent|alias]]` -> `parent`); lower-cased.
fn link_target(s: &str) -> String {
    let t = s.trim();
    let t = t.strip_prefix("[[").unwrap_or(t);
    let t = t.strip_suffix("]]").unwrap_or(t);
    let t = t.split('|').next().unwrap_or(t);
    t.trim().to_lowercase()
}

impl Filter {
    /// Parse a filter expression. Relative dates (`nov 20`) are resolved against `today` now.
    pub fn parse(input: &str, today: NaiveDate) -> Result<Filter, FilterError> {
        let toks = tokenize(input)?;
        if toks.is_empty() {
            return Err(FilterError::Empty);
        }
        let mut p = Parser {
            toks: &toks,
            i: 0,
            end: input.len(),
            today,
        };
        let f = p.or()?;
        match p.peek() {
            None => Ok(f),
            Some(Tok::RParen) => Err(FilterError::UnbalancedParens),
            Some(_) => Err(p.unexpected()),
        }
    }

    /// Whether `task` satisfies the filter. `today` is the current local date.
    pub fn matches(&self, task: &TaskFields, today: NaiveDate) -> bool {
        match self {
            Filter::All => true,
            Filter::Term(t) => t.matches(task, today),
            Filter::Not(f) => !f.matches(task, today),
            Filter::And(a, b) => a.matches(task, today) && b.matches(task, today),
            Filter::Or(a, b) => a.matches(task, today) || b.matches(task, today),
        }
    }
}

impl Term {
    fn matches(&self, task: &TaskFields, today: NaiveDate) -> bool {
        let due = task.due.map(|d| d.date);
        match self {
            Term::Today => due.is_some_and(|d| d <= today),
            Term::Overdue => due.is_some_and(|d| d < today),
            Term::Tomorrow => due.is_some_and(|d| today.checked_add_days(Days::new(1)) == Some(d)),
            Term::NoDate => due.is_none(),
            Term::HasDate => due.is_some(),
            Term::Recurring => task.recurrence.is_some(),
            Term::Priority(p) => task.priority == *p,
            Term::Project(name) => task.project.as_deref().is_some_and(|p| {
                let (p, n) = (p.to_lowercase(), name.to_lowercase());
                p == n || p.strip_prefix(&n).is_some_and(|rest| rest.starts_with('/'))
            }),
            Term::ProjectExact(name) => task
                .project
                .as_deref()
                .is_some_and(|p| p.to_lowercase() == name.to_lowercase()),
            Term::Inbox => task.project.is_none(),
            Term::Label(l) => task
                .labels
                .iter()
                .any(|x| x.to_lowercase() == l.to_lowercase()),
            Term::Section(s) => task
                .section
                .as_deref()
                .is_some_and(|x| x.to_lowercase() == s.to_lowercase()),
            Term::NextDays(n) => due.is_some_and(|d| {
                d >= today
                    && today
                        .checked_add_days(Days::new(u64::from(*n)))
                        .is_some_and(|end| d < end)
            }),
            Term::DueBefore(x) => due.is_some_and(|d| d < *x),
            Term::DueAfter(x) => due.is_some_and(|d| d > *x),
            Term::DueOn(x) => due == Some(*x),
            Term::Search(s) => task.title.to_lowercase().contains(&s.to_lowercase()),
            Term::Done => task.done,
            Term::Open => !task.done,
            Term::SubtaskOf(name) => task
                .parent
                .as_deref()
                .is_some_and(|p| link_target(p) == link_target(name)),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::recurrence::Recurrence;
    use crate::task::Due;

    fn d(y: i32, m: u32, day: u32) -> NaiveDate {
        NaiveDate::from_ymd_opt(y, m, day).expect("valid date")
    }

    /// 2026-11-16, a Monday.
    fn today() -> NaiveDate {
        d(2026, 11, 16)
    }

    fn p(s: &str) -> Filter {
        Filter::parse(s, today()).expect("parses")
    }

    fn t(title: &str) -> TaskFields {
        TaskFields::new(title)
    }

    fn due(mut task: TaskFields, date: &str) -> TaskFields {
        task.due = date.parse::<Due>().ok();
        task
    }

    fn term(x: Term) -> Box<Filter> {
        Box::new(Filter::Term(x))
    }

    #[test]
    fn precedence_and_parens() {
        // & binds tighter than |
        assert_eq!(
            p("today | p1 & inbox"),
            Filter::Or(
                term(Term::Today),
                Box::new(Filter::And(
                    term(Term::Priority(Priority::P1)),
                    term(Term::Inbox)
                ))
            )
        );
        assert_eq!(
            p("(today | p1) & inbox"),
            Filter::And(
                Box::new(Filter::Or(
                    term(Term::Today),
                    term(Term::Priority(Priority::P1))
                )),
                term(Term::Inbox)
            )
        );
        // ! binds tighter than &
        assert_eq!(
            p("!done & today"),
            Filter::And(Box::new(Filter::Not(term(Term::Done))), term(Term::Today))
        );
        assert_eq!(
            p("!!done"),
            Filter::Not(Box::new(Filter::Not(term(Term::Done))))
        );
        assert_eq!(
            p("!(today | done)"),
            Filter::Not(Box::new(Filter::Or(term(Term::Today), term(Term::Done))))
        );
        // left associative
        assert_eq!(
            p("today & p1 & open"),
            Filter::And(
                Box::new(Filter::And(
                    term(Term::Today),
                    term(Term::Priority(Priority::P1))
                )),
                term(Term::Open)
            )
        );
    }

    #[test]
    fn terms_parse_table() {
        let cases: Vec<(&str, Filter)> = vec![
            ("today", Filter::Term(Term::Today)),
            ("  TODAY ", Filter::Term(Term::Today)),
            ("overdue", Filter::Term(Term::Overdue)),
            ("tomorrow", Filter::Term(Term::Tomorrow)),
            ("no date", Filter::Term(Term::NoDate)),
            ("No   Date", Filter::Term(Term::NoDate)),
            ("has date", Filter::Term(Term::HasDate)),
            ("recurring", Filter::Term(Term::Recurring)),
            ("p1", Filter::Term(Term::Priority(Priority::P1))),
            ("P4", Filter::Term(Term::Priority(Priority::P4))),
            ("#Home", Filter::Term(Term::Project("Home".into()))),
            ("##Home", Filter::Term(Term::ProjectExact("Home".into()))),
            (
                "#\"Big Project\"",
                Filter::Term(Term::Project("Big Project".into())),
            ),
            ("@errand", Filter::Term(Term::Label("errand".into()))),
            (
                "/Groceries",
                Filter::Term(Term::Section("Groceries".into())),
            ),
            ("inbox", Filter::Term(Term::Inbox)),
            ("next 7 days", Filter::Term(Term::NextDays(7))),
            ("next 1 day", Filter::Term(Term::NextDays(1))),
            (
                "due before: 2026-12-01",
                Filter::Term(Term::DueBefore(d(2026, 12, 1))),
            ),
            (
                "due after: nov 20",
                Filter::Term(Term::DueAfter(d(2026, 11, 20))),
            ),
            ("2026-11-20", Filter::Term(Term::DueOn(d(2026, 11, 20)))),
            ("nov 20", Filter::Term(Term::DueOn(d(2026, 11, 20)))),
            ("20 Nov", Filter::Term(Term::DueOn(d(2026, 11, 20)))),
            ("november 20th", Filter::Term(Term::DueOn(d(2026, 11, 20)))),
            ("nov 10", Filter::Term(Term::DueOn(d(2027, 11, 10)))),
            ("nov 10 2028", Filter::Term(Term::DueOn(d(2028, 11, 10)))),
            (
                "search: Buy Milk",
                Filter::Term(Term::Search("Buy Milk".into())),
            ),
            (
                "search: \"a & b\"",
                Filter::Term(Term::Search("a & b".into())),
            ),
            ("done", Filter::Term(Term::Done)),
            ("completed", Filter::Term(Term::Done)),
            ("open", Filter::Term(Term::Open)),
            (
                "subtask: Parent Task",
                Filter::Term(Term::SubtaskOf("Parent Task".into())),
            ),
            ("all", Filter::All),
            ("*", Filter::All),
        ];
        for (src, want) in cases {
            assert_eq!(p(src), want, "{src}");
        }
    }

    #[test]
    fn multiword_terms_next_to_operators() {
        assert_eq!(
            p("no date & p1"),
            Filter::And(term(Term::NoDate), term(Term::Priority(Priority::P1)))
        );
        assert_eq!(p("!no date"), Filter::Not(term(Term::NoDate)));
        assert_eq!(
            p("(next 3 days)|overdue"),
            Filter::Or(term(Term::NextDays(3)), term(Term::Overdue))
        );
    }

    #[test]
    fn errors_table() {
        let cases: Vec<(&str, FilterError)> = vec![
            ("", FilterError::Empty),
            ("   ", FilterError::Empty),
            ("(today", FilterError::UnbalancedParens),
            ("today)", FilterError::UnbalancedParens),
            ("((today)", FilterError::UnbalancedParens),
            ("(", FilterError::UnbalancedParens),
            (
                "today &",
                FilterError::UnexpectedToken {
                    pos: 7,
                    found: "end of input".into(),
                },
            ),
            (
                "& today",
                FilterError::UnexpectedToken {
                    pos: 0,
                    found: "&".into(),
                },
            ),
            (
                "today | | p1",
                FilterError::UnexpectedToken {
                    pos: 8,
                    found: "|".into(),
                },
            ),
            ("today p1 & x", FilterError::UnknownTerm("today p1".into())),
            (
                "()",
                FilterError::UnexpectedToken {
                    pos: 1,
                    found: ")".into(),
                },
            ),
            (
                "!",
                FilterError::UnexpectedToken {
                    pos: 1,
                    found: "end of input".into(),
                },
            ),
            ("banana", FilterError::UnknownTerm("banana".into())),
            ("p5", FilterError::UnknownTerm("p5".into())),
            ("#", FilterError::UnknownTerm("#".into())),
            ("search:", FilterError::UnknownTerm("search:".into())),
            (
                "due before: someday",
                FilterError::UnknownTerm("due before: someday".into()),
            ),
            (
                "next x days",
                FilterError::UnknownTerm("next x days".into()),
            ),
            ("feb 30", FilterError::UnknownTerm("feb 30".into())),
        ];
        for (src, want) in cases {
            assert_eq!(Filter::parse(src, today()), Err(want), "{src}");
        }
        assert!(matches!(
            Filter::parse("#\"open", today()),
            Err(FilterError::UnexpectedToken { .. })
        ));
    }

    #[test]
    fn date_terms_match() {
        let tod = today();
        let cases: Vec<(&str, &str, bool)> = vec![
            // (filter, due, expected)
            ("today", "2026-11-16", true),
            ("today", "2026-11-16T09:00", true),
            ("today", "2026-11-10", true),
            ("today", "2026-11-17", false),
            ("overdue", "2026-11-15", true),
            ("overdue", "2026-11-16", false),
            ("overdue", "2026-11-16T00:01", false),
            ("tomorrow", "2026-11-17", true),
            ("tomorrow", "2026-11-16", false),
            ("tomorrow", "2026-11-18", false),
            ("has date", "2026-11-18", true),
            ("next 7 days", "2026-11-16", true),
            ("next 7 days", "2026-11-22", true),
            ("next 7 days", "2026-11-23", false),
            ("next 7 days", "2026-11-15", false),
            ("next 0 days", "2026-11-16", false),
            ("due before: 2026-12-01", "2026-11-30", true),
            ("due before: 2026-12-01", "2026-12-01", false),
            ("due after: 2026-12-01", "2026-12-02", true),
            ("due after: 2026-12-01", "2026-12-01", false),
            ("nov 20", "2026-11-20", true),
            ("nov 20", "2026-11-21", false),
            ("20 nov", "2026-11-20T18:30", true),
            ("2026-11-20", "2026-11-20", true),
        ];
        for (src, due_s, want) in cases {
            let task = due(t("x"), due_s);
            assert_eq!(p(src).matches(&task, tod), want, "{src} vs {due_s}");
        }
        // Tasks without a due date never match date terms except no date.
        let none = t("x");
        for src in [
            "today",
            "overdue",
            "tomorrow",
            "has date",
            "next 7 days",
            "due before: 2030-01-01",
            "due after: 2000-01-01",
            "nov 20",
        ] {
            assert!(!p(src).matches(&none, tod), "{src}");
        }
        assert!(p("no date").matches(&none, tod));
        assert!(!p("no date").matches(&due(t("x"), "2026-11-16"), tod));
    }

    #[test]
    fn other_terms_match() {
        let tod = today();
        let mut task = t("Buy Milk");
        task.priority = Priority::P2;
        task.project = Some("Home/Garden".into());
        task.section = Some("Groceries".into());
        task.labels = vec!["Errand".into(), "weekend".into()];
        task.parent = Some("[[Weekly Shop|shop]]".into());
        task.recurrence = Recurrence::parse("FREQ=WEEKLY").ok();
        let cases: Vec<(&str, bool)> = vec![
            ("p2", true),
            ("p1", false),
            ("recurring", true),
            ("@errand", true),
            ("@ERRAND", true),
            ("@home", false),
            ("/groceries", true),
            ("/Produce", false),
            ("search: milk", true),
            ("search: MILK", true),
            ("search: bread", false),
            ("subtask: weekly shop", true),
            ("subtask: [[Weekly Shop]]", true),
            ("subtask: Other", false),
            ("open", true),
            ("done", false),
            ("inbox", false),
            ("all", true),
            ("@errand & p2 & !done", true),
            ("p1 | @weekend", true),
            ("(p1 | p3) & @weekend", false),
        ];
        for (src, want) in cases {
            assert_eq!(p(src).matches(&task, tod), want, "{src}");
        }
        assert!(!p("recurring").matches(&t("x"), tod));
        assert!(!p("subtask: x").matches(&t("x"), tod));
        let mut done = t("d");
        done.done = true;
        assert!(p("done").matches(&done, tod));
        assert!(p("completed").matches(&done, tod));
        assert!(!p("open").matches(&done, tod));
    }

    #[test]
    fn project_hierarchy() {
        let tod = today();
        let mk = |proj: Option<&str>| {
            let mut x = t("x");
            x.project = proj.map(str::to_string);
            x
        };
        let cases: Vec<(&str, Option<&str>, bool)> = vec![
            ("#home", Some("Home"), true),
            ("#home", Some("home/garden"), true),
            ("#home", Some("Home/Garden/Beds"), true),
            ("#home", Some("Homework"), false),
            ("#home", Some("Work/Home"), false),
            ("#home", None, false),
            ("##home", Some("Home"), true),
            ("##home", Some("HOME"), true),
            ("##home", Some("Home/Garden"), false),
            ("##home/garden", Some("Home/Garden"), true),
            ("#home/garden", Some("Home/Garden/Beds"), true),
            ("#\"Big Project\"", Some("big project/sub"), true),
            ("inbox", None, true),
            ("inbox", Some("Home"), false),
        ];
        for (src, proj, want) in cases {
            assert_eq!(p(src).matches(&mk(proj), tod), want, "{src} vs {proj:?}");
        }
    }

    #[test]
    fn builtin_views() {
        let tod = today();
        let open_inbox = t("a");
        let mut in_proj = t("b");
        in_proj.project = Some("Home".into());
        let due_today = due(t("c"), "2026-11-16");
        let overdue = due(t("d"), "2026-11-10");
        let soon = due(t("e"), "2026-11-20");
        let later = due(t("f"), "2026-12-25");
        let mut done_today = due(t("g"), "2026-11-16");
        done_today.done = true;

        let check = |view: BuiltinView, task: &TaskFields| builtin(view, tod).matches(task, tod);
        assert!(check(BuiltinView::Inbox, &open_inbox));
        assert!(!check(BuiltinView::Inbox, &in_proj));
        assert!(check(BuiltinView::Today, &due_today));
        assert!(check(BuiltinView::Today, &overdue));
        assert!(!check(BuiltinView::Today, &soon));
        assert!(!check(BuiltinView::Today, &done_today));
        assert!(check(BuiltinView::Upcoming, &due_today));
        assert!(check(BuiltinView::Upcoming, &soon));
        assert!(!check(BuiltinView::Upcoming, &overdue));
        assert!(!check(BuiltinView::Upcoming, &later));
        assert!(!check(BuiltinView::Upcoming, &done_today));
        assert!(check(BuiltinView::Completed, &done_today));
        assert!(!check(BuiltinView::Completed, &due_today));
        assert!(check(BuiltinView::All, &done_today));
        assert!(check(BuiltinView::All, &open_inbox));
        // Same filters as the textual forms.
        assert_eq!(builtin(BuiltinView::Today, tod), p("today & open"));
        assert_eq!(builtin(BuiltinView::Inbox, tod), p("inbox & open"));
        assert_eq!(builtin(BuiltinView::Upcoming, tod), p("next 7 days & open"));
        assert_eq!(builtin(BuiltinView::Completed, tod), p("done"));
        assert_eq!(builtin(BuiltinView::All, tod), p("*"));
    }

    #[test]
    fn relative_dates_use_given_today() {
        let other = d(2026, 12, 20);
        let f = Filter::parse("nov 20", other).expect("parses");
        assert_eq!(f, Filter::Term(Term::DueOn(d(2027, 11, 20))));
    }
}
