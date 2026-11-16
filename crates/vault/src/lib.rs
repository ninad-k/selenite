//! Markdown + YAML front matter parsing and serialisation, wikilink and tag extraction,
//! and vault file layout rules.
//!
//! Round-trip rule: an untouched [`Document`] serialises back to the exact bytes it was parsed
//! from. Editing a property rewrites only the front matter block.

pub mod blocks;
pub mod config;
pub mod document;
pub mod layout;
pub mod links;
pub mod schema;

pub use blocks::{Block, BlockKind, BlockRef, append_blocks, parse_blocks, replace_block};
pub use config::VaultConfig;
pub use document::{Document, FrontMatter, PropertyValue};
pub use links::{Heading, Link, extract_headings, extract_links, extract_tags, normalize_target};
pub use schema::{DatabaseKind, DatabaseSchema, SCHEMA_FILE, SavedView};

/// Errors produced while parsing or serialising vault files.
#[derive(Debug, thiserror::Error)]
pub enum VaultError {
    /// Front matter was present but not valid YAML.
    #[error("invalid front matter: {0}")]
    FrontMatter(String),
    /// The vault configuration file could not be parsed or serialised.
    #[error("invalid vault config: {0}")]
    Config(String),
    /// A block reference or block edit was invalid.
    #[error("block error: {0}")]
    Block(String),
}
