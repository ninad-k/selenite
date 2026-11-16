//! Domain model shared by every crate: spaces, modes, vault paths, pages and tasks.
//!
//! This crate holds the product name ([`APP_NAME`]); no other crate may hard-code it.

pub mod mode;
pub mod path;
pub mod quickadd;
pub mod recurrence;
pub mod task;

pub use mode::{Mode, ModeNames};
pub use path::{PathError, VaultPath};
pub use quickadd::{QuickAdd, parse_quick_add};
pub use recurrence::{Freq, Recurrence, RecurrenceError};
pub use task::{Due, Priority, ReminderSpec, TASK_KEYS, TaskError, TaskFields};

/// The product name. Change this (plus the desktop bundle config) to rename the product.
pub const APP_NAME: &str = "Selenite";

/// Name of the hidden per-vault folder holding app config and the disposable index.
pub const APP_DIR: &str = ".selenite";
