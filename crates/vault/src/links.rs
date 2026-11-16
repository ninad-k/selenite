//! Wikilink, Markdown link, tag and heading extraction from a page body.

use std::ops::Range;

use pulldown_cmark::{Event, Options, Parser, Tag, TagEnd};

/// A link from one page to another.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Link {
    /// Raw target text before any `#` or `|`, trimmed (Markdown links: percent-decoded).
    pub target: String,
    /// Heading or block reference after `#`, if any.
    pub heading: Option<String>,
    /// Display text after `|`, if any.
    pub alias: Option<String>,
    /// True for `![[...]]` embeds.
    pub embed: bool,
    /// 1-based line number within the body.
    pub line: u32,
    /// Start byte offset within the body (includes the leading `!` of an embed).
    pub start: usize,
    /// End byte offset (exclusive) within the body.
    pub end: usize,
}

/// A Markdown heading.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Heading {
    /// Level 1 to 6.
    pub level: u8,
    /// Plain heading text.
    pub text: String,
    /// 1-based line number within the body.
    pub line: u32,
}

/// Byte ranges of code (fenced, indented, inline) in `body`, sorted and non-overlapping.
fn code_ranges(body: &str) -> Vec<Range<usize>> {
    let mut out = Vec::new();
    for (ev, range) in Parser::new_ext(body, Options::empty()).into_offset_iter() {
        match ev {
            Event::Start(Tag::CodeBlock(_)) => out.push(range),
            Event::Code(_) => out.push(range),
            _ => {}
        }
    }
    out.sort_by_key(|r| r.start);
    out
}

fn in_ranges(ranges: &[Range<usize>], pos: usize) -> bool {
    let idx = ranges.partition_point(|r| r.start <= pos);
    idx > 0 && pos < ranges[idx - 1].end
}

fn line_starts(body: &str) -> Vec<usize> {
    let mut v = vec![0];
    v.extend(body.match_indices('\n').map(|(i, _)| i + 1));
    v
}

fn line_of(starts: &[usize], pos: usize) -> u32 {
    let n = starts.partition_point(|&s| s <= pos);
    u32::try_from(n).unwrap_or(u32::MAX)
}

fn non_empty(s: &str) -> Option<String> {
    let t = s.trim();
    (!t.is_empty()).then(|| t.to_string())
}

fn percent_decode(s: &str) -> String {
    let b = s.as_bytes();
    let mut out = Vec::with_capacity(b.len());
    let mut i = 0;
    while i < b.len() {
        if b[i] == b'%'
            && i + 2 < b.len()
            && let (Some(h), Some(l)) = (hex(b[i + 1]), hex(b[i + 2]))
        {
            out.push(h * 16 + l);
            i += 3;
            continue;
        }
        out.push(b[i]);
        i += 1;
    }
    String::from_utf8_lossy(&out).into_owned()
}

fn hex(c: u8) -> Option<u8> {
    match c {
        b'0'..=b'9' => Some(c - b'0'),
        b'a'..=b'f' => Some(c - b'a' + 10),
        b'A'..=b'F' => Some(c - b'A' + 10),
        _ => None,
    }
}

fn has_scheme(s: &str) -> bool {
    let lower = s.to_ascii_lowercase();
    lower.contains("://") || lower.starts_with("mailto:") || lower.starts_with("tel:")
}

/// Extract wikilinks (`[[T]]`, `[[T|a]]`, `[[T#H]]`, `![[T]]`) and relative Markdown links to
/// `.md` files from a page body, skipping anything inside code.
pub fn extract_links(body: &str) -> Vec<Link> {
    let code = code_ranges(body);
    let starts = line_starts(body);
    let mut out = Vec::new();

    // Wikilinks.
    let mut i = 0;
    while let Some(rel) = body[i..].find("[[") {
        let open = i + rel;
        let after = open + 2;
        if in_ranges(&code, open) {
            i = after;
            continue;
        }
        let line_end = body[after..].find('\n').map_or(body.len(), |x| after + x);
        let Some(close_rel) = body[after..line_end].find("]]") else {
            i = after;
            continue;
        };
        let inner = &body[after..after + close_rel];
        if inner.contains("[[") {
            i = after;
            continue;
        }
        let end = after + close_rel + 2;
        i = end;
        let embed = open > 0 && body.as_bytes()[open - 1] == b'!';
        let start = if embed { open - 1 } else { open };
        let (left, alias) = match inner.split_once('|') {
            Some((l, a)) => (l, non_empty(a)),
            None => (inner, None),
        };
        let (target, heading) = match left.split_once('#') {
            Some((t, h)) => (t.trim(), non_empty(h)),
            None => (left.trim(), None),
        };
        if target.is_empty() {
            continue;
        }
        out.push(Link {
            target: target.to_string(),
            heading,
            alias,
            embed,
            line: line_of(&starts, start),
            start,
            end,
        });
    }

    // Markdown links to relative .md files.
    for (ev, range) in Parser::new_ext(body, Options::empty()).into_offset_iter() {
        let Event::Start(Tag::Link {
            dest_url,
            link_type,
            ..
        }) = ev
        else {
            continue;
        };
        if matches!(
            link_type,
            pulldown_cmark::LinkType::Autolink | pulldown_cmark::LinkType::Email
        ) {
            continue;
        }
        let dest: &str = &dest_url;
        if dest.is_empty() || dest.starts_with('#') || has_scheme(dest) {
            continue;
        }
        let decoded = percent_decode(dest);
        let (path, heading) = match decoded.split_once('#') {
            Some((p, h)) => (p.to_string(), non_empty(h)),
            None => (decoded.clone(), None),
        };
        let path = path.trim().to_string();
        if !path.to_ascii_lowercase().ends_with(".md") {
            continue;
        }
        out.push(Link {
            target: path,
            heading,
            alias: None,
            embed: false,
            line: line_of(&starts, range.start),
            start: range.start,
            end: range.end,
        });
    }

    out.sort_by_key(|l| l.start);
    out
}

/// Extract `#tag` and `#nested/tag` tags from a page body (without the `#`, original case,
/// deduplicated, in order of first appearance). Code is skipped.
pub fn extract_tags(body: &str) -> Vec<String> {
    let code = code_ranges(body);
    let mut out: Vec<String> = Vec::new();
    let mut iter = body.char_indices().peekable();
    let mut prev: Option<char> = None;
    while let Some((idx, c)) = iter.next() {
        let boundary = prev.is_none_or(char::is_whitespace);
        prev = Some(c);
        if c != '#' || !boundary || in_ranges(&code, idx) {
            continue;
        }
        let start = idx + 1;
        let mut end = start;
        while let Some(&(j, d)) = iter.peek() {
            if d.is_alphanumeric() || matches!(d, '_' | '-' | '/') {
                end = j + d.len_utf8();
                iter.next();
            } else {
                break;
            }
        }
        let name = body[start..end].trim_end_matches('/');
        if name.is_empty() || !name.chars().any(|ch| !ch.is_ascii_digit() && ch != '/') {
            prev = Some('#');
            continue;
        }
        prev = Some('a');
        if !out.iter().any(|t| t == name) {
            out.push(name.to_string());
        }
    }
    out
}

/// Extract headings (ATX and setext) from a page body.
pub fn extract_headings(body: &str) -> Vec<Heading> {
    let starts = line_starts(body);
    let mut out = Vec::new();
    let mut current: Option<(u8, u32, String)> = None;
    for (ev, range) in Parser::new_ext(body, Options::empty()).into_offset_iter() {
        match ev {
            Event::Start(Tag::Heading { level, .. }) => {
                current = Some((level as u8, line_of(&starts, range.start), String::new()));
            }
            Event::Text(t) | Event::Code(t) => {
                if let Some((_, _, s)) = current.as_mut() {
                    s.push_str(&t);
                }
            }
            Event::SoftBreak | Event::HardBreak => {
                if let Some((_, _, s)) = current.as_mut() {
                    s.push(' ');
                }
            }
            Event::End(TagEnd::Heading(_)) => {
                if let Some((level, line, text)) = current.take() {
                    out.push(Heading {
                        level,
                        text: text.trim().to_string(),
                        line,
                    });
                }
            }
            _ => {}
        }
    }
    out
}

/// Normalise a link target or page name for matching: backslashes become `/`, surrounding
/// whitespace and a trailing `.md` are removed, and the result is lower-cased (Unicode).
pub fn normalize_target(s: &str) -> String {
    let s = s.trim().replace('\\', "/");
    let s = s.trim();
    let stem = if s.len() >= 3
        && s.is_char_boundary(s.len() - 3)
        && s[s.len() - 3..].eq_ignore_ascii_case(".md")
    {
        &s[..s.len() - 3]
    } else {
        s
    };
    stem.trim().to_lowercase()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn targets(body: &str) -> Vec<String> {
        extract_links(body).into_iter().map(|l| l.target).collect()
    }

    #[test]
    fn wikilink_forms() {
        let body = "[[A]] [[B|alias]] [[C#Head]] [[D#Head|al]] ![[E]] [[ F ]]";
        let l = extract_links(body);
        assert_eq!(l.len(), 6);
        assert_eq!(
            (
                l[0].target.as_str(),
                l[0].heading.clone(),
                l[0].alias.clone()
            ),
            ("A", None, None)
        );
        assert_eq!(l[1].alias.as_deref(), Some("alias"));
        assert_eq!(l[2].heading.as_deref(), Some("Head"));
        assert_eq!(l[3].heading.as_deref(), Some("Head"));
        assert_eq!(l[3].alias.as_deref(), Some("al"));
        assert!(l[4].embed && l[4].target == "E");
        assert_eq!(l[5].target, "F");
        for link in &l {
            let s = &body[link.start..link.end];
            assert!(s.ends_with("]]"));
            assert_eq!(s.starts_with("![["), link.embed);
        }
    }

    #[test]
    fn markdown_links() {
        let body = "[x](Some%20Note.md) [y](sub/b.md#Sec) [w](http://e.com/a.md) [m](mailto:a@b.c) [h](#top) [i](img.png) <http://a.md>";
        let l = extract_links(body);
        assert_eq!(l.len(), 2);
        assert_eq!(l[0].target, "Some Note.md");
        assert_eq!(&body[l[0].start..l[0].end], "[x](Some%20Note.md)");
        assert_eq!(l[1].target, "sub/b.md");
        assert_eq!(l[1].heading.as_deref(), Some("Sec"));
    }

    #[test]
    fn code_is_excluded() {
        let body = "```\n[[fenced]]\n```\n\n    [[indented]]\n\n`[[inline]]` and [[real]]\n\n~~~rust\n[[tilde]] #nope\n~~~\n";
        assert_eq!(targets(body), vec!["real"]);
        assert!(extract_tags(body).is_empty());
    }

    #[test]
    fn line_numbers_and_ranges() {
        let body = "first\n\nsecond [[B]]\nthird ![[C|c]] é [[D]]\n";
        let l = extract_links(body);
        assert_eq!(l.iter().map(|x| x.line).collect::<Vec<_>>(), vec![3, 4, 4]);
        assert_eq!(&body[l[0].start..l[0].end], "[[B]]");
        assert_eq!(&body[l[1].start..l[1].end], "![[C|c]]");
        assert_eq!(&body[l[2].start..l[2].end], "[[D]]");
    }

    #[test]
    fn unclosed_and_multiline_ignored() {
        assert!(extract_links("[[a\nb]] [[c").is_empty());
        assert_eq!(targets("[[ [[ok]]"), vec!["ok"]);
    }

    #[test]
    fn tags_basic() {
        let body = "#one text #two/sub and #a-b_c.\n#end\nnot#tag http://x.com/#frag &#123; #2024 #1984 #y2k";
        assert_eq!(
            extract_tags(body),
            vec!["one", "two/sub", "a-b_c", "end", "y2k"]
        );
    }

    #[test]
    fn tags_exclude_headings_and_dedupe() {
        let body = "# Title\n## Sub\n#Tag and #Tag and #tag\n";
        assert_eq!(extract_tags(body), vec!["Tag", "tag"]);
    }

    #[test]
    fn tags_unicode_and_trailing_slash() {
        assert_eq!(
            extract_tags("#café #a/ #日本語"),
            vec!["café", "a", "日本語"]
        );
    }

    #[test]
    fn headings() {
        let h = extract_headings("# One\n\ntext\n\n## Two `code`\n\nSetext\n======\n");
        assert_eq!(h.len(), 3);
        assert_eq!((h[0].level, h[0].text.as_str(), h[0].line), (1, "One", 1));
        assert_eq!(
            (h[1].level, h[1].text.as_str(), h[1].line),
            (2, "Two code", 5)
        );
        assert_eq!(
            (h[2].level, h[2].text.as_str(), h[2].line),
            (1, "Setext", 7)
        );
    }

    #[test]
    fn normalise() {
        assert_eq!(normalize_target("  Sub\\Note.MD "), "sub/note");
        assert_eq!(normalize_target("ÉCOLE.md"), "école");
        assert_eq!(normalize_target("md"), "md");
        assert_eq!(normalize_target(""), "");
    }
}
