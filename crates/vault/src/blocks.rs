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

/// Parser options shared by block splitting and rendering, so both see the same structure.
pub(crate) fn parser_options() -> Options {
    Options::ENABLE_TABLES
        | Options::ENABLE_TASKLISTS
        | Options::ENABLE_FOOTNOTES
        | Options::ENABLE_STRIKETHROUGH
}

/// Split a page body into its top-level blocks.
pub fn parse_blocks(body: &str) -> Vec<Block> {
    let opts = parser_options();
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
    let block = resolve(&blocks, r)?;
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

fn resolve<'a>(blocks: &'a [Block], r: &BlockRef) -> Result<&'a Block, VaultError> {
    match r {
        BlockRef::Index(i) => blocks.get(*i).ok_or_else(|| {
            VaultError::Block(format!(
                "block index {i} out of range (page has {} blocks)",
                blocks.len()
            ))
        }),
        BlockRef::Anchor(id) => blocks
            .iter()
            .find(|b| b.anchor.as_deref() == Some(id.as_str()))
            .ok_or_else(|| VaultError::Block(format!("no block with anchor ^{id}"))),
    }
}

/// Insert already-final `text` (no normalisation) as a block after block `after` (or at the
/// start), with one blank line between it and its neighbour.
fn insert_raw(body: &str, blocks: &[Block], after: Option<usize>, text: &str) -> String {
    let eol = eol_of(body);
    if blocks.is_empty() {
        return format!("{body}{text}{eol}");
    }
    let mut out = String::with_capacity(body.len() + text.len() + 4);
    match after.and_then(|i| blocks.get(i)) {
        Some(b) => {
            out.push_str(&body[..b.end]);
            out.push_str(eol);
            out.push_str(eol);
            out.push_str(text);
            out.push_str(&body[b.end..]);
        }
        None => {
            let at = blocks[0].start;
            out.push_str(&body[..at]);
            out.push_str(text);
            out.push_str(eol);
            out.push_str(eol);
            out.push_str(&body[at..]);
        }
    }
    out
}

/// Insert Markdown as a new block after `after` (`None` = at the start of the page), separated
/// by one blank line. Line endings in `markdown` follow the body. Trailing spaces on the last
/// line are kept so slash-menu templates such as `- ` survive. Bytes outside the insertion point
/// are untouched.
///
/// Note: inserting a list item right after another list of the same kind joins them into one
/// Markdown list.
pub fn insert_block(
    body: &str,
    after: Option<&BlockRef>,
    markdown: &str,
) -> Result<String, VaultError> {
    let eol = eol_of(body);
    let unix = markdown.replace("\r\n", "\n");
    let trimmed = unix.trim_matches(['\n', '\r']);
    if trimmed.trim().is_empty() {
        return Err(VaultError::Block(
            "inserted markdown must not be empty".into(),
        ));
    }
    let text = if eol == "\n" {
        trimmed.to_string()
    } else {
        trimmed.replace('\n', eol)
    };
    let blocks = parse_blocks(body);
    let idx = match after {
        Some(r) => Some(resolve(&blocks, r)?.index),
        None => None,
    };
    Ok(insert_raw(body, &blocks, idx, &text))
}

/// Byte range removed when deleting `blocks[idx]`: the block, its line ending and one blank
/// line after it; for the last block, the blank line(s) before it instead.
fn delete_range(body: &str, blocks: &[Block], idx: usize) -> (usize, usize) {
    let b = &blocks[idx];
    if idx + 1 == blocks.len() && idx > 0 && body[blocks[idx - 1].end..b.start].trim().is_empty() {
        return (blocks[idx - 1].end, b.end);
    }
    let mut end = b.end;
    let rest = &body[end..];
    let eol_len = if rest.starts_with("\r\n") {
        2
    } else {
        usize::from(rest.starts_with('\n'))
    };
    end += eol_len;
    if eol_len > 0 && idx + 1 < blocks.len() {
        let rest = &body[end..];
        let line_len = rest.find('\n').map_or(rest.len(), |p| p + 1);
        if rest[..line_len].trim().is_empty() && end + line_len <= blocks[idx + 1].start {
            end += line_len;
        }
    }
    (b.start, end)
}

/// Remove a block and one separating blank line. Everything else is byte-identical.
pub fn delete_block(body: &str, r: &BlockRef) -> Result<String, VaultError> {
    let blocks = parse_blocks(body);
    let b = resolve(&blocks, r)?;
    let (start, end) = delete_range(body, &blocks, b.index);
    Ok(format!("{}{}", &body[..start], &body[end..]))
}

/// Move a block so it ends up at `to_index` among the blocks that remain after it is removed
/// (`0` = first, number of other blocks = last). The block's own bytes are preserved.
pub fn move_block(body: &str, r: &BlockRef, to_index: usize) -> Result<String, VaultError> {
    let blocks = parse_blocks(body);
    let b = resolve(&blocks, r)?;
    let others = blocks.len() - 1;
    if to_index > others {
        return Err(VaultError::Block(format!(
            "move target {to_index} out of range (0..={others})"
        )));
    }
    if to_index == b.index {
        return Ok(body.to_string());
    }
    let (start, end) = delete_range(body, &blocks, b.index);
    let removed = format!("{}{}", &body[..start], &body[end..]);
    let rest = parse_blocks(&removed);
    Ok(insert_raw(
        &removed,
        &rest,
        to_index.checked_sub(1),
        &b.text,
    ))
}

/// If `line` is a task list item, the byte offset of the character inside its `[ ]`.
fn task_box(line: &str) -> Option<usize> {
    let bytes = line.as_bytes();
    let mut i = 0;
    while i < bytes.len() && matches!(bytes[i], b' ' | b'\t' | b'>') {
        i += 1;
    }
    match bytes.get(i)? {
        b'-' | b'*' | b'+' => i += 1,
        b'0'..=b'9' => {
            while bytes.get(i).is_some_and(u8::is_ascii_digit) {
                i += 1;
            }
            if !matches!(bytes.get(i)?, b'.' | b')') {
                return None;
            }
            i += 1;
        }
        _ => return None,
    }
    let ws = i;
    while bytes.get(i).is_some_and(|c| matches!(c, b' ' | b'\t')) {
        i += 1;
    }
    if i == ws || *bytes.get(i)? != b'[' || *bytes.get(i + 2)? != b']' {
        return None;
    }
    let ok_after = bytes
        .get(i + 3)
        .is_none_or(|c| matches!(c, b' ' | b'\t' | b'\r' | b'\n'));
    (matches!(bytes[i + 1], b' ' | b'x' | b'X') && ok_after).then_some(i + 1)
}

/// Flip `[ ]` and `[x]` on one line of a block (`line` is 0-based within the block).
pub fn toggle_task_item(body: &str, r: &BlockRef, line: usize) -> Result<String, VaultError> {
    let blocks = parse_blocks(body);
    let b = resolve(&blocks, r)?;
    let mut offset = 0;
    for (n, l) in b.text.split_inclusive('\n').enumerate() {
        if n == line {
            let pos = task_box(l).ok_or_else(|| {
                VaultError::Block(format!("line {line} of the block is not a task item"))
            })?;
            let at = b.start + offset + pos;
            let flipped = if body.as_bytes()[at] == b' ' {
                'x'
            } else {
                ' '
            };
            let mut out = String::with_capacity(body.len());
            out.push_str(&body[..at]);
            out.push(flipped);
            out.push_str(&body[at + 1..]);
            return Ok(out);
        }
        offset += l.len();
    }
    Err(VaultError::Block(format!(
        "line {line} out of range for the block"
    )))
}

/// Markdown skeleton for a slash-menu block type, or `None` for an unknown type.
pub fn block_template(kind: &str) -> Option<&'static str> {
    Some(match kind {
        "text" => "",
        "h1" => "# ",
        "h2" => "## ",
        "h3" => "### ",
        "bullet" => "- ",
        "numbered" => "1. ",
        "todo" => "- [ ] ",
        "toggle" => "> [!note]- Toggle\n> ",
        "quote" => "> ",
        "callout" => "> [!note] Note\n> ",
        "code" => "```\n\n```",
        "divider" => "---",
        "image" => "![]()",
        "file" => "[file]()",
        "embed" => "![[]]",
        "table" => "| Column | Column |\n| --- | --- |\n|  |  |",
        _ => return None,
    })
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
