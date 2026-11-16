//! Reminder notifications on Android and iOS, through `tauri-plugin-notification`.
//!
//! The helpers at the top are platform-independent so they are unit-tested on the desktop;
//! [`MobileNotifier`] itself only exists in mobile builds.

use chrono::{Local, NaiveDateTime, TimeZone};

/// Map an [`selenite_reminders::Occurrence`] id (16 hex digits) to the positive `i32` the mobile
/// notification APIs use. Ids are stable, so re-planning replaces rather than duplicates.
#[must_use]
pub fn notification_id(occurrence_id: &str) -> i32 {
    let hash = u64::from_str_radix(occurrence_id, 16).unwrap_or_else(|_| {
        // Fall back to FNV-1a over the bytes for ids that are not hex.
        occurrence_id
            .bytes()
            .fold(0xcbf2_9ce4_8422_2325_u64, |h, b| {
                (h ^ u64::from(b)).wrapping_mul(0x0100_0000_01b3)
            })
    });
    // Keep 31 bits so the id is positive; 0 is reserved by some Android launchers.
    let id = i32::try_from(hash & 0x7fff_ffff).unwrap_or(1);
    if id == 0 { 1 } else { id }
}

/// The local wall-clock time `fire_at` as an RFC 3339 string with its UTC offset, or `None` when
/// the time does not exist locally (skipped by a daylight-saving jump).
#[must_use]
pub fn fire_at_rfc3339(fire_at: NaiveDateTime) -> Option<String> {
    Local
        .from_local_datetime(&fire_at)
        .earliest()
        .map(|t| t.to_rfc3339())
}

/// The plugin's one-shot `Schedule::At` in its serialised form. Built as JSON so the app does not
/// need a direct dependency on the `time` crate the plugin uses.
#[must_use]
pub fn schedule_at_json(date: &str) -> serde_json::Value {
    serde_json::json!({ "at": { "date": date, "repeating": false, "allow_while_idle": true } })
}

#[cfg(mobile)]
pub use imp::MobileNotifier;

#[cfg(mobile)]
mod imp {
    use super::{fire_at_rfc3339, notification_id, schedule_at_json};
    use selenite_reminders::{Notifier, Occurrence, ReminderError};
    use std::collections::HashMap;
    use std::sync::Mutex;
    use tauri::{AppHandle, Runtime};
    use tauri_plugin_notification::{NotificationExt, Schedule};

    /// Schedules reminder notifications with the phone's notification system.
    pub struct MobileNotifier<R: Runtime> {
        app: AppHandle<R>,
        /// Mobile id -> occurrence id of what this app scheduled last.
        scheduled: Mutex<HashMap<i32, String>>,
    }

    impl<R: Runtime> MobileNotifier<R> {
        /// A notifier that schedules through `app`'s notification plugin.
        pub fn new(app: AppHandle<R>) -> Self {
            Self {
                app,
                scheduled: Mutex::new(HashMap::new()),
            }
        }
    }

    fn notify_err(e: impl std::fmt::Display) -> ReminderError {
        ReminderError::Notify(e.to_string())
    }

    impl<R: Runtime> Notifier for MobileNotifier<R> {
        fn kind(&self) -> &'static str {
            "mobile"
        }

        fn replace_all(&self, occurrences: &[Occurrence]) -> Result<usize, ReminderError> {
            let n = self.app.notification();
            let mut map = self.scheduled.lock().map_err(notify_err)?;
            let old: Vec<i32> = map.keys().copied().collect();
            if !old.is_empty() {
                n.cancel(old).map_err(notify_err)?;
            }
            map.clear();
            let now = chrono::Local::now().naive_local();
            for occ in occurrences.iter().filter(|o| o.fire_at > now) {
                let Some(date) = fire_at_rfc3339(occ.fire_at) else {
                    continue;
                };
                let schedule: Schedule =
                    serde_json::from_value(schedule_at_json(&date)).map_err(notify_err)?;
                let id = notification_id(&occ.id);
                let body = occ
                    .due
                    .map(|d| format!("Due {}", d.format("%H:%M")))
                    .unwrap_or_else(|| "Reminder".to_owned());
                n.builder()
                    .id(id)
                    .title(occ.title.clone())
                    .body(body)
                    .schedule(schedule)
                    .show()
                    .map_err(notify_err)?;
                map.insert(id, occ.id.clone());
            }
            Ok(map.len())
        }

        fn scheduled_ids(&self) -> Result<Vec<String>, ReminderError> {
            let pending = self.app.notification().pending().map_err(notify_err)?;
            let map = self.scheduled.lock().map_err(notify_err)?;
            Ok(pending
                .iter()
                .filter_map(|p| map.get(&p.id()).cloned())
                .collect())
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use chrono::NaiveDate;

    #[test]
    fn ids_are_stable_positive_and_distinct() {
        let a = notification_id("00000000ffffffff");
        assert_eq!(a, notification_id("00000000ffffffff"));
        assert!(a > 0);
        assert_ne!(a, notification_id("0000000012345678"));
        assert!(notification_id("not-hex") > 0);
        assert_eq!(notification_id("0000000080000000"), 1, "zero maps to 1");
    }

    #[test]
    fn schedule_json_has_the_plugin_shape() {
        let fire = NaiveDate::from_ymd_opt(2026, 11, 20)
            .and_then(|d| d.and_hms_opt(9, 30, 0))
            .unwrap();
        let date = fire_at_rfc3339(fire).unwrap();
        assert!(date.starts_with("2026-11-20T09:30:00"));
        let v = schedule_at_json(&date);
        assert_eq!(v["at"]["date"], date.as_str());
        assert_eq!(v["at"]["repeating"], false);
    }
}
