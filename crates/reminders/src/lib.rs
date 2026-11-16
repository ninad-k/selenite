//! Seven-day reminder planner and OS notification adapters.
//!
//! ADR-005: on every connect, sync or app start the app computes every reminder occurrence for
//! the next [`WINDOW_DAYS`] days ([`plan`]) and registers them as OS-level local notifications
//! ([`Notifier::replace_all`]) so they fire offline with no server. [`replan`] does both and is
//! idempotent, so it can run after every task change.
//!
//! Privacy: task titles are only passed to the OS notification; logs carry counts and ids only.
//!
//! # Windows AUMID
//!
//! A scheduled toast needs an AppUserModelID registered with Windows. The installed desktop app
//! (Tauri NSIS/MSI) registers its bundle identifier `app.selenite.desktop`. In unpackaged dev runs
//! that ID may not be registered, so scheduling can fail or the toast may never show. This crate
//! never touches the registry or creates shortcuts; failures surface as
//! [`ReminderError::Notify`].

use std::collections::BTreeMap;
use std::sync::Mutex;

use chrono::{Duration, NaiveDate, NaiveDateTime, NaiveTime};
use selenite_core::{ReminderSpec, TaskFields};
use serde::Serialize;

#[cfg(windows)]
mod windows_toast;
#[cfg(windows)]
pub use windows_toast::WindowsToastNotifier;

/// Length of the planning window in days (locked decision ADR-005).
pub const WINDOW_DAYS: u32 = 7;

/// Local time used as the base for `Before` reminders on date-only due dates.
pub const DEFAULT_DAY_TIME: NaiveTime = match NaiveTime::from_hms_opt(9, 0, 0) {
    Some(t) => t,
    None => NaiveTime::MIN,
};

/// Errors produced by the planner or notification adapters.
#[derive(Debug, thiserror::Error)]
pub enum ReminderError {
    /// The OS notification service rejected a request.
    #[error("notification error: {0}")]
    Notify(String),
}

/// One notification to fire.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct Occurrence {
    /// Stable id: 16 lowercase hex digits of the FNV-1a 64 hash of
    /// `"{task_key}@{fire_at:%Y%m%dT%H%M}"` (fits the Windows toast tag limit of 64).
    pub id: String,
    /// Vault path of the task.
    pub task_key: String,
    /// Task title (shown in the notification; never logged).
    pub title: String,
    /// Local wall-clock time at which to fire.
    pub fire_at: NaiveDateTime,
    /// The due moment of the occurrence, when the due value has a time of day.
    pub due: Option<NaiveDateTime>,
}

/// A task to plan reminders for.
#[derive(Debug, Clone, Copy)]
pub struct PlannerInput<'a> {
    /// Vault path of the task (its identity).
    pub task_key: &'a str,
    /// The task's front matter.
    pub fields: &'a TaskFields,
}

fn fnv1a64(s: &str) -> u64 {
    let mut h: u64 = 0xcbf2_9ce4_8422_2325;
    for b in s.bytes() {
        h ^= u64::from(b);
        h = h.wrapping_mul(0x0000_0100_0000_01b3);
    }
    h
}

fn occurrence_id(task_key: &str, fire_at: NaiveDateTime) -> String {
    let key = format!("{task_key}@{}", fire_at.format("%Y%m%dT%H%M"));
    format!("{:016x}", fnv1a64(&key))
}

/// Pure planner: every reminder occurrence with `now <= fire_at < now + window`.
///
/// Done tasks are skipped. A recurring task contributes each occurrence of its rule (anchored at
/// the due date, keeping the due time) whose reminders can land in the window. `Before(d)` fires
/// at `due - d` per occurrence (date-only dues use [`DEFAULT_DAY_TIME`]); `At(dt)` fires once at
/// `dt`. A task with a due time and no reminders fires at the due time; a date-only due with no
/// reminders produces nothing. The result is sorted by `fire_at` then id and de-duplicated by id.
pub fn plan<'a>(
    tasks: impl IntoIterator<Item = PlannerInput<'a>>,
    now: NaiveDateTime,
    window: Duration,
) -> Vec<Occurrence> {
    let end = now.checked_add_signed(window).unwrap_or(now);
    let mut out: BTreeMap<(NaiveDateTime, String), Occurrence> = BTreeMap::new();
    let mut push =
        |input: &PlannerInput<'_>, fire_at: NaiveDateTime, due: Option<NaiveDateTime>| {
            if fire_at < now || fire_at >= end {
                return;
            }
            let id = occurrence_id(input.task_key, fire_at);
            out.entry((fire_at, id.clone()))
                .or_insert_with(|| Occurrence {
                    id,
                    task_key: input.task_key.to_string(),
                    title: input.fields.title.clone(),
                    fire_at,
                    due,
                });
        };

    for input in tasks {
        let f = input.fields;
        if f.done {
            continue;
        }
        // Absolute reminders fire once, independent of recurrence.
        for r in &f.reminders {
            if let ReminderSpec::At(dt) = r {
                push(&input, *dt, f.due_datetime());
            }
        }
        let Some(due) = f.due else { continue };
        let leads: Vec<Duration> = f
            .reminders
            .iter()
            .filter_map(|r| match r {
                ReminderSpec::Before(d) => Some(*d),
                ReminderSpec::At(_) => None,
            })
            .collect();
        let max_lead = leads
            .iter()
            .copied()
            .max()
            .unwrap_or_else(Duration::zero)
            .max(Duration::zero());

        let from_date = now
            .checked_sub_signed(max_lead)
            .map_or(now.date(), |t| t.date());
        let to_date = end.date();
        let dates: Vec<NaiveDate> = match &f.recurrence {
            Some(rule) => {
                if from_date > to_date {
                    Vec::new()
                } else {
                    // At most one occurrence per day.
                    let span = usize::try_from((to_date - from_date).num_days() + 1).unwrap_or(1);
                    rule.occurrences(due.date, from_date, span)
                        .into_iter()
                        .take_while(|d| *d <= to_date)
                        .collect()
                }
            }
            None => vec![due.date],
        };
        for date in dates {
            let base = date.and_time(due.time.unwrap_or(DEFAULT_DAY_TIME));
            let shown = due.time.map(|t| date.and_time(t));
            for lead in &leads {
                if let Some(at) = base.checked_sub_signed(*lead) {
                    push(&input, at, shown);
                }
            }
            if f.reminders.is_empty()
                && let Some(t) = shown
            {
                push(&input, t, shown);
            }
        }
    }
    out.into_values().collect()
}

/// Abstraction over an OS notification scheduler.
pub trait Notifier: Send + Sync {
    /// Short adapter name, e.g. `"memory"` or `"windows-toast"`.
    fn kind(&self) -> &'static str;

    /// Replace everything previously scheduled by this app with exactly `occurrences`.
    ///
    /// Returns the number of notifications actually scheduled (OS adapters skip past times).
    ///
    /// # Errors
    /// [`ReminderError::Notify`] when the OS rejects the request.
    fn replace_all(&self, occurrences: &[Occurrence]) -> Result<usize, ReminderError>;

    /// Ids of the currently scheduled notifications.
    ///
    /// # Errors
    /// [`ReminderError::Notify`] when the OS cannot be queried.
    fn scheduled_ids(&self) -> Result<Vec<String>, ReminderError>;
}

/// In-memory notifier for tests and desktops without an OS adapter.
///
/// Linux and macOS adapters are future work; until then those platforms use this and nothing
/// fires.
#[derive(Debug, Default)]
pub struct MemoryNotifier {
    scheduled: Mutex<Vec<Occurrence>>,
}

impl MemoryNotifier {
    /// An empty notifier.
    pub fn new() -> Self {
        Self::default()
    }

    /// A copy of what is currently scheduled.
    pub fn snapshot(&self) -> Vec<Occurrence> {
        self.scheduled.lock().map(|g| g.clone()).unwrap_or_default()
    }
}

impl Notifier for MemoryNotifier {
    fn kind(&self) -> &'static str {
        "memory"
    }

    fn replace_all(&self, occurrences: &[Occurrence]) -> Result<usize, ReminderError> {
        let mut g = self
            .scheduled
            .lock()
            .map_err(|_| ReminderError::Notify("memory notifier lock poisoned".into()))?;
        *g = occurrences.to_vec();
        Ok(g.len())
    }

    fn scheduled_ids(&self) -> Result<Vec<String>, ReminderError> {
        let g = self
            .scheduled
            .lock()
            .map_err(|_| ReminderError::Notify("memory notifier lock poisoned".into()))?;
        Ok(g.iter().map(|o| o.id.clone()).collect())
    }
}

/// The best notifier for this platform: Windows toasts on Windows, [`MemoryNotifier`] elsewhere.
pub fn default_notifier(aumid: &str) -> Box<dyn Notifier> {
    #[cfg(windows)]
    {
        Box::new(WindowsToastNotifier::new(aumid))
    }
    #[cfg(not(windows))]
    {
        let _ = aumid;
        Box::new(MemoryNotifier::new())
    }
}

/// Plan the next [`WINDOW_DAYS`] days and replace the notifier's schedule with the result.
///
/// # Errors
/// Propagates [`ReminderError`] from the notifier.
pub fn replan<'a>(
    notifier: &dyn Notifier,
    tasks: impl IntoIterator<Item = PlannerInput<'a>>,
    now: NaiveDateTime,
) -> Result<Vec<Occurrence>, ReminderError> {
    let planned = plan(tasks, now, Duration::days(i64::from(WINDOW_DAYS)));
    let scheduled = notifier.replace_all(&planned)?;
    tracing::info!(
        kind = notifier.kind(),
        planned = planned.len(),
        scheduled,
        "reminders re-planned"
    );
    Ok(planned)
}

#[cfg(test)]
mod tests {
    use super::*;
    use selenite_core::Recurrence;

    fn dt(y: i32, m: u32, d: u32, h: u32, min: u32) -> NaiveDateTime {
        NaiveDate::from_ymd_opt(y, m, d)
            .and_then(|d| d.and_hms_opt(h, min, 0))
            .expect("valid datetime")
    }

    fn now() -> NaiveDateTime {
        dt(2026, 11, 16, 8, 0)
    }

    fn week() -> Duration {
        Duration::days(7)
    }

    fn task(due: &str) -> TaskFields {
        let mut t = TaskFields::new("Task");
        t.due = due.parse().ok();
        t
    }

    fn run(fields: &TaskFields) -> Vec<Occurrence> {
        plan(
            [PlannerInput {
                task_key: "Tasks/a.md",
                fields,
            }],
            now(),
            week(),
        )
    }

    #[test]
    fn default_reminder_at_due_time() {
        let o = run(&task("2026-11-17T10:30"));
        assert_eq!(o.len(), 1);
        assert_eq!(o[0].fire_at, dt(2026, 11, 17, 10, 30));
        assert_eq!(o[0].due, Some(dt(2026, 11, 17, 10, 30)));
        assert_eq!(o[0].title, "Task");
    }

    #[test]
    fn date_only_without_reminders_is_silent() {
        assert!(run(&task("2026-11-17")).is_empty());
    }

    #[test]
    fn before_reminder() {
        let mut t = task("2026-11-17T10:30");
        t.reminders = vec![ReminderSpec::Before(Duration::minutes(15))];
        let o = run(&t);
        assert_eq!(o.len(), 1);
        assert_eq!(o[0].fire_at, dt(2026, 11, 17, 10, 15));
    }

    #[test]
    fn before_on_date_only_uses_default_day_time() {
        let mut t = task("2026-11-17");
        t.reminders = vec![ReminderSpec::Before(Duration::hours(1))];
        let o = run(&t);
        assert_eq!(o.len(), 1);
        assert_eq!(o[0].fire_at, dt(2026, 11, 17, 8, 0));
        assert_eq!(o[0].due, None);
    }

    #[test]
    fn absolute_reminder() {
        let mut t = task("2026-11-30");
        t.reminders = vec![ReminderSpec::At(dt(2026, 11, 18, 7, 0))];
        let o = run(&t);
        assert_eq!(o.len(), 1);
        assert_eq!(o[0].fire_at, dt(2026, 11, 18, 7, 0));
    }

    #[test]
    fn done_is_skipped() {
        let mut t = task("2026-11-17T10:30");
        t.done = true;
        assert!(run(&t).is_empty());
    }

    #[test]
    fn weekly_recurring_one_per_week() {
        let mut t = task("2026-11-02T09:00");
        t.recurrence = Recurrence::parse("FREQ=WEEKLY").ok();
        let o = run(&t);
        assert_eq!(o.len(), 1);
        assert_eq!(o[0].fire_at, dt(2026, 11, 16, 9, 0));
        // An absolute reminder is not repeated per occurrence.
        t.reminders = vec![ReminderSpec::At(dt(2026, 11, 20, 6, 0))];
        assert_eq!(run(&t).len(), 1);
    }

    #[test]
    fn daily_recurring_seven() {
        let mut t = task("2026-11-01T09:00");
        t.recurrence = Recurrence::parse("FREQ=DAILY").ok();
        let o = run(&t);
        assert_eq!(o.len(), 7);
        assert_eq!(o[0].fire_at, dt(2026, 11, 16, 9, 0));
        assert_eq!(o[6].fire_at, dt(2026, 11, 22, 9, 0));
    }

    #[test]
    fn recurring_lead_pulls_next_occurrence_into_window() {
        let mut t = task("2026-11-23T08:30"); // due is just past the window end
        t.recurrence = Recurrence::parse("FREQ=WEEKLY").ok();
        t.reminders = vec![ReminderSpec::Before(Duration::hours(1))];
        let o = run(&t);
        assert_eq!(o.len(), 1);
        assert_eq!(o[0].fire_at, dt(2026, 11, 23, 7, 30));
    }

    #[test]
    fn window_boundaries() {
        let mut t = task("2026-11-16T08:00"); // exactly now: included
        assert_eq!(run(&t).len(), 1);
        t.due = "2026-11-23T08:00".parse().ok(); // exactly now + 7d: excluded
        assert!(run(&t).is_empty());
        t.due = "2026-11-23T07:59".parse().ok();
        assert_eq!(run(&t).len(), 1);
        t.due = "2026-11-16T07:59".parse().ok();
        assert!(run(&t).is_empty());
    }

    #[test]
    fn sorted_stable_unique_ids() {
        let mut a = task("2026-11-18T10:00");
        a.reminders = vec![
            ReminderSpec::Before(Duration::minutes(15)),
            ReminderSpec::Before(Duration::minutes(15)),
            ReminderSpec::Before(Duration::days(1)),
        ];
        let mut b = task("2026-11-17T10:00");
        b.recurrence = Recurrence::parse("FREQ=DAILY").ok();
        let inputs = || {
            vec![
                PlannerInput {
                    task_key: "Tasks/a.md",
                    fields: &a,
                },
                PlannerInput {
                    task_key: "Tasks/b.md",
                    fields: &b,
                },
            ]
        };
        let o = plan(inputs(), now(), week());
        assert_eq!(o, plan(inputs(), now(), week()));
        assert!(
            o.windows(2)
                .all(|w| (w[0].fire_at, &w[0].id) < (w[1].fire_at, &w[1].id))
        );
        let mut ids: Vec<_> = o.iter().map(|x| x.id.clone()).collect();
        assert!(ids.iter().all(|i| i.len() <= 64
            && !i.is_empty()
            && i.chars().all(|c| c.is_ascii_alphanumeric() || c == '-')));
        let n = ids.len();
        ids.sort();
        ids.dedup();
        assert_eq!(ids.len(), n);
        assert_eq!(n, 2 + 6); // a: two distinct lead times; b: 17th..22nd
    }

    #[test]
    fn memory_notifier_replaces() {
        let n = MemoryNotifier::new();
        assert_eq!(n.kind(), "memory");
        let t = task("2026-11-17T10:30");
        let planned = replan(
            &n,
            [PlannerInput {
                task_key: "Tasks/a.md",
                fields: &t,
            }],
            now(),
        )
        .expect("replan");
        assert_eq!(planned.len(), 1);
        assert_eq!(n.scheduled_ids().expect("ids").len(), 1);
        assert_eq!(n.snapshot().len(), 1);
        assert_eq!(n.replace_all(&[]).expect("replace"), 0);
        assert!(n.scheduled_ids().expect("ids").is_empty());
    }

    #[cfg(windows)]
    #[test]
    #[ignore = "schedules a real toast; run manually"]
    fn windows_toast_roundtrip() {
        let n = WindowsToastNotifier::new("app.selenite.desktop");
        let fire = chrono::Local::now().naive_local() + Duration::minutes(10);
        let occ = Occurrence {
            id: occurrence_id("Tasks/test.md", fire),
            task_key: "Tasks/test.md".into(),
            title: "Selenite test <&> reminder".into(),
            fire_at: fire,
            due: Some(fire),
        };
        let scheduled = n.replace_all(std::slice::from_ref(&occ)).expect("schedule");
        assert_eq!(scheduled, 1);
        let ids = n.scheduled_ids().expect("list");
        assert!(ids.contains(&occ.id), "{ids:?}");
        n.replace_all(&[]).expect("cleanup");
        assert!(n.scheduled_ids().expect("list").is_empty());
    }
}
