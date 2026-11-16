//! Domain model shared by every crate: spaces, modes, vault paths, pages and tasks.
//!
//! This crate holds the product name ([`APP_NAME`]); no other crate may hard-code it.

pub mod mode;
pub mod path;

pub use mode::{Mode, ModeNames};
pub use path::{PathError, VaultPath};

/// The product name. Change this (plus the desktop bundle config) to rename the product.
pub const APP_NAME: &str = "Selenite";

/// Name of the hidden per-vault folder holding app config and the disposable index.
pub const APP_DIR: &str = ".selenite";
