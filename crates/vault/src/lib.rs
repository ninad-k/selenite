//! Markdown + YAML front matter parsing and serialisation, wikilink and tag extraction,
//! and vault file layout rules.
//!
//! Phase 0: public API placeholder. Implemented in Phase 1.

/// Errors produced while parsing or serialising vault files.
#[derive(Debug, thiserror::Error)]
pub enum VaultError {
    /// Front matter was present but not valid YAML.
    #[error("invalid front matter: {0}")]
    FrontMatter(String),
}
