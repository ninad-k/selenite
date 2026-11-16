//! Windows adapter: scheduled toast notifications via WinRT (no unsafe code needed).

use chrono::{Local, NaiveDateTime, TimeZone};
use windows::Data::Xml::Dom::XmlDocument;
use windows::Foundation::DateTime;
use windows::UI::Notifications::{
    ScheduledToastNotification, ToastNotificationManager, ToastNotifier,
};
use windows::core::HSTRING;

use crate::{Notifier, Occurrence, ReminderError};

/// Seconds between 1601-01-01 and 1970-01-01 (Windows `DateTime` epoch offset).
const EPOCH_DIFF_SECS: i64 = 11_644_473_600;

/// Toast group for every reminder.
const GROUP: &str = "reminders";

/// Schedules reminders as Windows scheduled toasts for a given AppUserModelID.
///
/// The AUMID must be registered with Windows (the installed Tauri app registers its bundle
/// identifier, `app.selenite.desktop`). For unpackaged dev runs it may not be, in which case
/// scheduling can fail or the toast may never be shown. Nothing is written to the registry.
#[derive(Debug, Clone)]
pub struct WindowsToastNotifier {
    aumid: String,
}

fn notify_err(what: &str, e: &windows::core::Error) -> ReminderError {
    ReminderError::Notify(format!("{what}: {e}"))
}

fn xml_escape(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    for c in s.chars() {
        match c {
            '&' => out.push_str("&amp;"),
            '<' => out.push_str("&lt;"),
            '>' => out.push_str("&gt;"),
            '"' => out.push_str("&quot;"),
            '\'' => out.push_str("&apos;"),
            c if c.is_control() => out.push(' '),
            c => out.push(c),
        }
    }
    out
}

fn toast_xml(o: &Occurrence) -> String {
    let mut xml = format!(
        "<toast><visual><binding template=\"ToastGeneric\"><text>{}</text>",
        xml_escape(&o.title)
    );
    if let Some(due) = o.due {
        let line = due.format("%a %-d %b %H:%M").to_string();
        xml.push_str(&format!("<text>{}</text>", xml_escape(&line)));
    }
    xml.push_str("</binding></visual></toast>");
    xml
}

fn to_windows_datetime(local: NaiveDateTime) -> Option<DateTime> {
    let utc = Local
        .from_local_datetime(&local)
        .earliest()?
        .with_timezone(&chrono::Utc);
    let ticks = (utc.timestamp() + EPOCH_DIFF_SECS)
        .checked_mul(10_000_000)?
        .checked_add(i64::from(utc.timestamp_subsec_nanos() / 100))?;
    Some(DateTime {
        UniversalTime: ticks,
    })
}

impl WindowsToastNotifier {
    /// A notifier that schedules toasts for `aumid`.
    pub fn new(aumid: &str) -> Self {
        Self {
            aumid: aumid.to_string(),
        }
    }

    fn notifier(&self) -> Result<ToastNotifier, ReminderError> {
        ToastNotificationManager::CreateToastNotifierWithId(&HSTRING::from(self.aumid.as_str()))
            .map_err(|e| notify_err("create toast notifier (is the AUMID registered?)", &e))
    }

    fn build(o: &Occurrence, when: DateTime) -> Result<ScheduledToastNotification, ReminderError> {
        let doc = XmlDocument::new().map_err(|e| notify_err("create xml document", &e))?;
        doc.LoadXml(&HSTRING::from(toast_xml(o)))
            .map_err(|e| notify_err("load toast xml", &e))?;
        let toast = ScheduledToastNotification::CreateScheduledToastNotification(&doc, when)
            .map_err(|e| notify_err("create scheduled toast", &e))?;
        toast
            .SetTag(&HSTRING::from(o.id.as_str()))
            .map_err(|e| notify_err("set toast tag", &e))?;
        toast
            .SetGroup(&HSTRING::from(GROUP))
            .map_err(|e| notify_err("set toast group", &e))?;
        Ok(toast)
    }
}

impl Notifier for WindowsToastNotifier {
    fn kind(&self) -> &'static str {
        "windows-toast"
    }

    fn replace_all(&self, occurrences: &[Occurrence]) -> Result<usize, ReminderError> {
        let notifier = self.notifier()?;
        let existing = notifier
            .GetScheduledToastNotifications()
            .map_err(|e| notify_err("list scheduled toasts", &e))?;
        for toast in existing {
            notifier
                .RemoveFromSchedule(&toast)
                .map_err(|e| notify_err("remove scheduled toast", &e))?;
        }
        let now = Local::now().naive_local();
        let mut count = 0;
        for o in occurrences.iter().filter(|o| o.fire_at > now) {
            let Some(when) = to_windows_datetime(o.fire_at) else {
                tracing::warn!(id = %o.id, "reminder time not representable locally; skipped");
                continue;
            };
            let toast = Self::build(o, when)?;
            notifier
                .AddToSchedule(&toast)
                .map_err(|e| notify_err("schedule toast", &e))?;
            count += 1;
        }
        Ok(count)
    }

    fn scheduled_ids(&self) -> Result<Vec<String>, ReminderError> {
        let notifier = self.notifier()?;
        let existing = notifier
            .GetScheduledToastNotifications()
            .map_err(|e| notify_err("list scheduled toasts", &e))?;
        let mut ids = Vec::new();
        for toast in existing {
            let tag = toast.Tag().map_err(|e| notify_err("read toast tag", &e))?;
            ids.push(tag.to_string());
        }
        Ok(ids)
    }
}
