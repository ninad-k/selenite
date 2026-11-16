//! Markdown + YAML front matter parsing and serialisation, wikilink and tag extraction,
//! and vault file layout rules.
//!
//! Round-trip rule: an untouched [`Document`] serialises back to the exact bytes it was parsed
//! from. Editing a property rewrites only the front matter block.

pub mod config;
pub mod document;
pub mod layout;
pub mod links;

pub use config::VaultConfig;
pub use document::{Document, FrontMatter, PropertyValue};
pub use links::{Heading, Link, extract_headings, extract_links, extract_tags, normalize_target};

/// Errors produced while parsing or serialising vault files.
#[derive(Debug, thiserror::Error)]
pub enum VaultError {
    /// Front matter was present but not valid YAML.
    #[error("invalid front matter: {0}")]
    FrontMatter(String),
    /// The vault configuration file could not be parsed or serialised.
    #[error("invalid vault config: {0}")]
    Config(String),
}
