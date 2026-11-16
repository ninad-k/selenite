//! Top-level Markdown blocks of a page body, for block-level edits (append, replace).
//!
//! Blocks come from CommonMark top-level structure (paragraphs, headings, lists, quotes, code
//! blocks, tables, rules, HTML). A block may end with an Obsidian-style anchor (`^id`) that
//! gives it a stable name. Edits splice the body text: everything outside the replaced byte
//! range is left byte-identical, including line endings.

use std::str::FromStr;

use pulldown_cmark::{Event, Options, Parser, Tag};

use crate::VaultError;

/// What kind of block this is.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum BlockKind {
    /// A paragraph.
    Paragraph,
    /// A heading of the given level (1-6).
    Heading(u8),
    /// A bullet, numbered or task list (the whole list is one block).
    List,
    /// A block quote.
    Quote,
    /// A fenced or indented code block.
    Code,
    /// A table.
    Table,
    /// A thematic break (`---`).
    Rule,
    /// A raw HTML block.
    Html,
    /// Anything else (footnote definitions, ...).
    Other,
}

impl BlockKind {
    /// Short lowercase name for display (`heading` for every level).
    pub fn name(&self) -> &'static str {
        match self {
            BlockKind::Paragraph => "paragraph",
            BlockKind::Heading(_) => "heading",
            BlockKind::List => "list",
            BlockKind::Quote => "quote",
            BlockKind::Code => "code",
            BlockKind::Table => "table",
            BlockKind::Rule => "rule",
            BlockKind::Html => "html",
            BlockKind::Other => "other",
        }
    }
}

/// One top-level block of a page body.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Block {
    /// Position among the page's blocks, from 0.
    pub index: usize,
    /// Kind of block.
    pub kind: BlockKind,
    /// Anchor id (without the `^`) when the block's last line ends with `^id`.
    pub anchor: Option<String>,
    /// Start byte offset in the body.
    pub start: usize,
    /// End byte offset in the body, excluding trailing blank lines.
    pub end: usize,
    /// The block's Markdown source (`body[start..end]`).
    pub text: String,
}

/// How a caller names a block.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum BlockRef {
    /// By position.
    Index(usize),
    /// By anchor id (without the `^`).
    Anchor(String),
}

impl FromStr for BlockRef {
    type Err = VaultError;

    /// Parses `^abc` (anchor) or `3` (index).
    fn from_str(s: &str) -> Result<Self, Self::Err> {
        let t = s.trim();
        if let Some(id) = t.strip_prefix('^') {
            return if is_anchor_id(id) {
                Ok(BlockRef::Anchor(id.to_string()))
            } else {
                Err(VaultError::Block(format!("invalid anchor `{t}`")))
            };
        }
        t.parse::<usize>().map(BlockRef::Index).map_err(|_| {
            VaultError::Block(format!(
                "invalid block reference `{t}` (use an index like `3` or an anchor like `^abc`)"
            ))
        })
    }
}

impl BlockRef {
    /// Parse `^abc` or `3`.
    pub fn parse(s: &str) -> Result<Self, VaultError> {
        s.parse()
    }
}

fn is_anchor_id(id: &str) -> bool {
    !id.is_empty() && id.chars().all(|c| c.is_ascii_alphanumeric() || c == '-')
}

/// The `^id` at the end of the last line of `text`, when it stands alone as a token.
fn trailing_anchor(text: &str) -> Option<&str> {
    let last = text.lines().next_back()?.trim_end();
    let token = last.rsplit(char::is_whitespace).next()?;
    let id = token.strip_prefix('^')?;
    // `^id` must be the whole line or be preceded by whitespace (so `a^2` is not an anchor).
    let before = &last[..last.len() - token.len()];
    (is_anchor_id(id) && (before.is_empty() || before.ends_with(char::is_whitespace))).then_some(id)
}

fn kind_of(tag: &Tag<'_>) -> BlockKind {
    match tag {
        Tag::Paragraph => BlockKind::Paragraph,
        Tag::Heading { level, .. } => BlockKind::Heading(*level as u8),
        Tag::List(_) => BlockKind::List,
        Tag::BlockQuote(_) => BlockKind::Quote,
        Tag::CodeBlock(_) => BlockKind::Code,
        Tag::Table(_) => BlockKind::Table,
        Tag::HtmlBlock => BlockKind::Html,
        _ => BlockKind::Other,
    }
}

/// Split a page body into its top-level blocks.
pub fn parse_blocks(body: &str) -> Vec<Block> {
    let opts = Options::ENABLE_TABLES | Options::ENABLE_TASKLISTS | Options::ENABLE_FOOTNOTES;
    let mut raw: Vec<(BlockKind, usize, usize)> = Vec::new();
    let mut depth = 0usize;
    let mut open: Option<(BlockKind, usize)> = None;
    for (ev, range) in Parser::new_ext(body, opts).into_offset_iter() {
        match ev {
            Event::Start(tag) => {
                if depth == 0 {
                    open = Some((kind_of(&tag), range.start));
                }
                depth += 1;
            }
            Event::End(_) => {
                depth = depth.saturating_sub(1);
                if depth == 0
                    && let Some((kind, start)) = open.take()
                {
                    raw.push((kind, start, range.end));
                }
            }
            Event::Rule if depth == 0 => raw.push((BlockKind::Rule, range.start, range.end)),
            _ => {}
        }
    }
    raw.into_iter()
        .enumerate()
        .map(|(index, (kind, start, end))| {
            let end = start + body[start..end].trim_end().len();
            let text = body[start..end].to_string();
            let anchor = if kind == BlockKind::Code {
                None
            } else {
                trailing_anchor(&text).map(str::to_string)
            };
            Block {
                index,
                kind,
                anchor,
                start,
                end,
                text,
            }
        })
        .collect()
}

fn eol_of(body: &str) -> &'static str {
    if body.contains("\r\n") { "\r\n" } else { "\n" }
}

/// Convert every line ending in `s` to `eol` and trim surrounding blank lines.
fn normalise(s: &str, eol: &str) -> String {
    let unix = s.replace("\r\n", "\n");
    let trimmed = unix.trim_matches(['\n', '\r']).trim_end();
    if eol == "\n" {
        trimmed.to_string()
    } else {
        trimmed.replace('\n', eol)
    }
}

/// Append Markdown after the last block with exactly one blank line between, ending with a
/// newline. Trailing blank lines of the old body are normalised; nothing else changes.
/// Appending only whitespace returns the body unchanged.
pub fn append_blocks(body: &str, markdown: &str) -> String {
    let eol = eol_of(body);
    let new = normalise(markdown, eol);
    if new.is_empty() {
        return body.to_string();
    }
    let old = body.trim_end();
    if old.is_empty() {
        return format!("{new}{eol}");
    }
    format!("{old}{eol}{eol}{new}{eol}")
}

/// Replace one block with new Markdown. Bytes outside the block's range are untouched. When the
/// old block had an anchor and the new text has none, the anchor is re-attached to the end of
/// the new text (unless that ends in a code block).
pub fn replace_block(body: &str, r: &BlockRef, markdown: &str) -> Result<String, VaultError> {
    let blocks = parse_blocks(body);
    let block = match r {
        BlockRef::Index(i) => blocks.get(*i).ok_or_else(|| {
            VaultError::Block(format!(
                "block index {i} out of range (page has {} blocks)",
                blocks.len()
            ))
        })?,
        BlockRef::Anchor(id) => blocks
            .iter()
            .find(|b| b.anchor.as_deref() == Some(id.as_str()))
            .ok_or_else(|| VaultError::Block(format!("no block with anchor ^{id}")))?,
    };
    let eol = eol_of(body);
    let mut new = normalise(markdown, eol);
    if new.is_empty() {
        return Err(VaultError::Block(
            "replacement markdown must not be empty".into(),
        ));
    }
    if let Some(id) = &block.anchor {
        let new_blocks = parse_blocks(&new);
        let has_anchor = new_blocks.last().is_some_and(|b| b.anchor.is_some());
        let ends_in_code = new_blocks.last().is_some_and(|b| b.kind == BlockKind::Code);
        if !has_anchor && !ends_in_code {
            new.push_str(&format!(" ^{id}"));
        }
    }
    let mut out = String::with_capacity(body.len() + new.len());
    out.push_str(&body[..block.start]);
    out.push_str(&new);
    out.push_str(&body[block.end..]);
    Ok(out)
}

#[cfg(test)]
mod tests {
    use super::*;

    const SAMPLE: &str = "# Title\n\nFirst paragraph. ^intro\n\n- a\n- b\n\n> quote\n\n```rust\nfn main() {}\n```\n\n| a | b |\n|---|---|\n| 1 | 2 |\n\n---\n\n<div>x</div>\n\nLast one\n";

    #[test]
    fn kinds_and_ranges_round_trip() {
        let blocks = parse_blocks(SAMPLE);
        let kinds: Vec<BlockKind> = blocks.iter().map(|b| b.kind).collect();
        assert_eq!(
            kinds,
            vec![
                BlockKind::Heading(1),
                BlockKind::Paragraph,
                BlockKind::List,
                BlockKind::Quote,
                BlockKind::Code,
                BlockKind::Table,
                BlockKind::Rule,
                BlockKind::Html,
                BlockKind::Paragraph,
            ]
        );
        for (i, b) in blocks.iter().enumerate() {
            assert_eq!(b.index, i);
            assert_eq!(&SAMPLE[b.start..b.end], b.text);
            assert!(!b.text.ends_with('\n'));
        }
        assert_eq!(blocks[2].text, "- a\n- b");
    }

    #[test]
    fn anchors_are_detected() {
        let blocks = parse_blocks(SAMPLE);
        assert_eq!(blocks[1].anchor.as_deref(), Some("intro"));
        assert_eq!(blocks[0].anchor, None);
        let b = parse_blocks("- one\n- two ^list-1\n\nmath a^2\n\n## Head ^h2\n");
        assert_eq!(b[0].anchor.as_deref(), Some("list-1"));
        assert_eq!(b[1].anchor, None);
        assert_eq!(b[2].anchor.as_deref(), Some("h2"));
    }

    #[test]
    fn block_ref_parsing() {
        assert_eq!(
            BlockRef::parse("^abc").unwrap(),
            BlockRef::Anchor("abc".into())
        );
        assert_eq!(BlockRef::parse(" 3 ").unwrap(), BlockRef::Index(3));
        assert!(BlockRef::parse("^").is_err());
        assert!(BlockRef::parse("x").is_err());
    }

    #[test]
    fn replace_is_byte_stable_outside_the_range() {
        let out = replace_block(SAMPLE, &BlockRef::Index(2), "- x\n- y\n- z").unwrap();
        let old = &parse_blocks(SAMPLE)[2];
        assert_eq!(&out[..old.start], &SAMPLE[..old.start]);
        assert_eq!(
            &out[out.len() - (SAMPLE.len() - old.end)..],
            &SAMPLE[old.end..]
        );
        assert!(out.contains("- x\n- y\n- z\n\n> quote"));
    }

    #[test]
    fn replace_keeps_or_overrides_anchor() {
        let out = replace_block(SAMPLE, &BlockRef::Anchor("intro".into()), "New text").unwrap();
        assert!(out.contains("New text ^intro\n"));
        let out = replace_block(SAMPLE, &BlockRef::Index(1), "New text ^other").unwrap();
        assert!(out.contains("New text ^other\n"));
        assert!(!out.contains("^intro"));
    }

    #[test]
    fn replace_crlf_body_keeps_crlf() {
        let body = "# T\r\n\r\npara one\r\n\r\npara two\r\n";
        let out = replace_block(body, &BlockRef::Index(1), "line a\nline b").unwrap();
        assert_eq!(out, "# T\r\n\r\nline a\r\nline b\r\n\r\npara two\r\n");
        let appended = append_blocks(body, "tail");
        assert_eq!(
            appended,
            "# T\r\n\r\npara one\r\n\r\npara two\r\n\r\ntail\r\n"
        );
    }

    #[test]
    fn replace_errors() {
        assert!(replace_block("a\n", &BlockRef::Index(5), "x").is_err());
        assert!(replace_block("a\n", &BlockRef::Anchor("zz".into()), "x").is_err());
        assert!(replace_block("a\n", &BlockRef::Index(0), "  \n").is_err());
    }

    #[test]
    fn append_spacing() {
        assert_eq!(append_blocks("a", "b"), "a\n\nb\n");
        assert_eq!(append_blocks("a\n", "\n\nb\n\n"), "a\n\nb\n");
        assert_eq!(append_blocks("a\n\n\n\n", "b"), "a\n\nb\n");
        assert_eq!(append_blocks("", "b"), "b\n");
        assert_eq!(append_blocks("\n\n", "b"), "b\n");
        assert_eq!(append_blocks("a\n", "  \n"), "a\n");
    }
}
