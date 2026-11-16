//! Splitting a page body into citable passages for "ask the vault" retrieval.
//!
//! A passage is a run of whole blocks (paragraphs, lists, code fences) inside one heading
//! section, merged up to roughly [`TARGET_CHARS`] characters. Code fences are never split.
//! Line numbers are 1-based and relative to the text that was split.

/// Soft upper bound on a passage's length in characters. A single paragraph longer than this is
/// split on line (then word) boundaries; a code fence longer than this stays whole.
pub const TARGET_CHARS: usize = 800;

/// One citable piece of a page.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Passage {
    /// Text of the nearest heading above the passage, if any.
    pub heading: Option<String>,
    /// The passage text (original lines, joined with `\n`).
    pub text: String,
    /// 1-based first line of the passage.
    pub start_line: u32,
    /// 1-based last line of the passage (inclusive).
    pub end_line: u32,
}

/// A contiguous run of non-blank lines (or one whole code fence).
struct Block {
    text: String,
    start: u32,
    end: u32,
    /// Fence blocks are atomic: they are not split even when too long.
    atomic: bool,
}

fn heading_text(line: &str) -> Option<String> {
    let t = line.trim_start_matches(' ');
    if line.len() - t.len() > 3 {
        return None;
    }
    let hashes = t.chars().take_while(|&c| c == '#').count();
    if hashes == 0 || hashes > 6 {
        return None;
    }
    let rest = &t[hashes..];
    if !rest.is_empty() && !rest.starts_with([' ', '\t']) {
        return None;
    }
    Some(rest.trim().trim_end_matches('#').trim().to_string())
}

fn fence_marker(line: &str) -> Option<(char, usize)> {
    let t = line.trim_start_matches(' ');
    if line.len() - t.len() > 3 {
        return None;
    }
    let c = t.chars().next()?;
    if c != '`' && c != '~' {
        return None;
    }
    let n = t.chars().take_while(|&x| x == c).count();
    (n >= 3).then_some((c, n))
}

fn flush_block(cur: &mut Vec<(u32, &str)>, blocks: &mut Vec<Block>, atomic: bool) {
    if let (Some(first), Some(last)) = (cur.first(), cur.last()) {
        let text = cur.iter().map(|(_, l)| *l).collect::<Vec<_>>().join("\n");
        if !text.trim().is_empty() {
            blocks.push(Block {
                text,
                start: first.0,
                end: last.0,
                atomic,
            });
        }
    }
    cur.clear();
}

/// Split `body` into passages. Empty or whitespace-only input gives no passages.
pub fn split_passages(body: &str) -> Vec<Passage> {
    let mut out: Vec<Passage> = Vec::new();
    let mut heading: Option<String> = None;
    let mut blocks: Vec<Block> = Vec::new();
    let mut cur: Vec<(u32, &str)> = Vec::new();
    let mut fence: Option<(char, usize)> = None;

    for (i, line) in body.lines().enumerate() {
        let n = u32::try_from(i + 1).unwrap_or(u32::MAX);
        if let Some((c, len)) = fence {
            cur.push((n, line));
            let closes = fence_marker(line).is_some_and(|(c2, len2)| c2 == c && len2 >= len)
                && line.trim().chars().all(|x| x == c);
            if closes {
                fence = None;
                flush_block(&mut cur, &mut blocks, true);
            }
            continue;
        }
        if let Some(m) = fence_marker(line) {
            flush_block(&mut cur, &mut blocks, false);
            fence = Some(m);
            cur.push((n, line));
            continue;
        }
        if let Some(h) = heading_text(line) {
            flush_block(&mut cur, &mut blocks, false);
            pack(&mut out, &heading, std::mem::take(&mut blocks));
            heading = Some(h).filter(|s| !s.is_empty());
            continue;
        }
        if line.trim().is_empty() {
            flush_block(&mut cur, &mut blocks, false);
        } else {
            cur.push((n, line));
        }
    }
    // An unterminated fence runs to the end of the text.
    let atomic = fence.is_some();
    flush_block(&mut cur, &mut blocks, atomic);
    pack(&mut out, &heading, blocks);
    out
}

/// Merge the blocks of one section into passages.
fn pack(out: &mut Vec<Passage>, heading: &Option<String>, blocks: Vec<Block>) {
    let mut acc: Option<Passage> = None;
    for b in blocks {
        if !b.atomic && b.text.chars().count() > TARGET_CHARS {
            out.extend(acc.take());
            for piece in split_long(&b) {
                out.push(Passage {
                    heading: heading.clone(),
                    text: piece.text,
                    start_line: piece.start,
                    end_line: piece.end,
                });
            }
            continue;
        }
        match acc.as_mut() {
            Some(p) if p.text.chars().count() + 2 + b.text.chars().count() <= TARGET_CHARS => {
                p.text.push_str("\n\n");
                p.text.push_str(&b.text);
                p.end_line = b.end;
            }
            _ => {
                out.extend(acc.take());
                acc = Some(Passage {
                    heading: heading.clone(),
                    text: b.text,
                    start_line: b.start,
                    end_line: b.end,
                });
            }
        }
    }
    out.extend(acc);
}

/// Split one over-long block on line boundaries; a single over-long line is cut at spaces.
fn split_long(b: &Block) -> Vec<Block> {
    let mut pieces: Vec<Block> = Vec::new();
    let mut cur = String::new();
    let mut start = b.start;
    let mut last = b.start;
    for (offset, line) in b.text.split('\n').enumerate() {
        let line_no = b.start + u32::try_from(offset).unwrap_or(0);
        for chunk in cut_line(line) {
            let len = cur.chars().count();
            if len > 0 && len + 1 + chunk.chars().count() > TARGET_CHARS {
                pieces.push(Block {
                    text: std::mem::take(&mut cur),
                    start,
                    end: last,
                    atomic: false,
                });
                start = line_no;
            }
            if !cur.is_empty() {
                cur.push('\n');
            }
            cur.push_str(&chunk);
            last = line_no;
        }
    }
    if !cur.trim().is_empty() {
        pieces.push(Block {
            text: cur,
            start,
            end: last,
            atomic: false,
        });
    }
    pieces
}

fn cut_line(line: &str) -> Vec<String> {
    if line.chars().count() <= TARGET_CHARS {
        return vec![line.to_string()];
    }
    let mut out = Vec::new();
    let mut cur = String::new();
    for word in line.split(' ') {
        if !cur.is_empty() && cur.chars().count() + 1 + word.chars().count() > TARGET_CHARS {
            out.push(std::mem::take(&mut cur));
        }
        if !cur.is_empty() {
            cur.push(' ');
        }
        cur.push_str(word);
        while cur.chars().count() > TARGET_CHARS {
            let cut: String = cur.chars().take(TARGET_CHARS).collect();
            cur = cur.chars().skip(TARGET_CHARS).collect();
            out.push(cut);
        }
    }
    if !cur.is_empty() {
        out.push(cur);
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn splits_by_heading_with_lines() {
        let body = "Intro line\n\n# Alpha\nfirst\nsecond\n\n## Beta\nthird\n";
        let p = split_passages(body);
        assert_eq!(p.len(), 3);
        assert_eq!(p[0].heading, None);
        assert_eq!((p[0].start_line, p[0].end_line), (1, 1));
        assert_eq!(p[1].heading.as_deref(), Some("Alpha"));
        assert_eq!((p[1].start_line, p[1].end_line), (4, 5));
        assert_eq!(p[1].text, "first\nsecond");
        assert_eq!(p[2].heading.as_deref(), Some("Beta"));
        assert_eq!((p[2].start_line, p[2].end_line), (8, 8));
    }

    #[test]
    fn merges_small_paragraphs_in_a_section() {
        let p = split_passages("# H\none\n\ntwo\n\nthree\n");
        assert_eq!(p.len(), 1);
        assert_eq!(p[0].text, "one\n\ntwo\n\nthree");
        assert_eq!((p[0].start_line, p[0].end_line), (2, 6));
    }

    #[test]
    fn code_fence_is_never_split_and_ignores_hashes() {
        let big = "x ".repeat(600);
        let body =
            format!("intro\n\n```rust\n# not a heading\n\nlet a = \"{big}\";\n```\n\nafter\n");
        let p = split_passages(&body);
        let fence = p
            .iter()
            .find(|p| p.text.contains("```rust"))
            .expect("fence");
        assert!(fence.text.contains("# not a heading"));
        assert!(fence.text.trim_end().ends_with("```"));
        assert_eq!((fence.start_line, fence.end_line), (3, 7));
        assert!(p.iter().all(|p| p.heading.is_none()));
        assert_eq!(p.last().map(|p| p.text.as_str()), Some("after"));
    }

    #[test]
    fn long_paragraph_is_split_within_limit() {
        let lines: Vec<String> = (0..40)
            .map(|i| format!("line number {i} with some padding text"))
            .collect();
        let p = split_passages(&lines.join("\n"));
        assert!(p.len() > 1);
        for w in &p {
            assert!(w.text.chars().count() <= TARGET_CHARS);
        }
        assert_eq!(p[0].start_line, 1);
        assert_eq!(p.last().map(|p| p.end_line), Some(40));
        for pair in p.windows(2) {
            assert_eq!(pair[0].end_line + 1, pair[1].start_line);
        }
    }

    #[test]
    fn single_huge_line_is_cut() {
        let p = split_passages(&"word ".repeat(500));
        assert!(p.len() >= 3);
        assert!(p.iter().all(|p| p.text.chars().count() <= TARGET_CHARS));
        assert!(p.iter().all(|p| p.start_line == 1 && p.end_line == 1));
    }

    #[test]
    fn empty_and_heading_only_bodies() {
        assert!(split_passages("").is_empty());
        assert!(split_passages("\n\n  \n").is_empty());
        assert!(split_passages("# Only a heading\n").is_empty());
    }
}
