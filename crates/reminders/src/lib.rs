//! Seven-day reminder planner and OS notification adapters.
//!
//! Phase 0: public API placeholder. Implemented in Phase 2.

/// Length of the planning window in days (locked decision ADR-005).
pub const WINDOW_DAYS: u32 = 7;

/// Errors produced by the planner or notification adapters.
#[derive(Debug, thiserror::Error)]
pub enum ReminderError {
    /// The OS notification service rejected a request.
    #[error("notification error: {0}")]
    Notify(String),
}
