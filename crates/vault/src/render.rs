//! Safe HTML rendering of Markdown blocks for the workspace view.
//!
//! The output is an HTML *fragment* built from parser events by this module alone: raw HTML in
//! the source is escaped to text, URLs are filtered by scheme, attribute values are escaped and
//! headings carry no ids. Wikilinks, `#tags`, task checkboxes and Obsidian-style callouts are
//! recognised on top of CommonMark. Nothing here touches the file system; resolving link and
//! asset targets is delegated to the caller through [`RenderOptions`].

use std::ops::Range;

use pulldown_cmark::{Alignment, CodeBlockKind, Event, LinkType, Parser, Tag, TagEnd};

use crate::blocks::{Block, parser_options};

/// Callbacks that map vault references to things the UI can use.
pub struct RenderOptions<'a> {
    /// Maps a wikilink target (or a relative Markdown link path) to a vault path, or `None`
    /// when the page does not exist.
    pub resolve_link: &'a dyn Fn(&str) -> Option<String>,
    /// Maps an image or file target to a URL the UI can load, or `None` when it cannot be found.
    pub resolve_asset: &'a dyn Fn(&str) -> Option<String>,
}

const IMAGE_EXTENSIONS: &[&str] = &[
    "png", "jpg", "jpeg", "gif", "webp", "svg", "bmp", "avif", "ico",
];

/// Schemes a resolver may never hand back, as a second line of defence.
const BLOCKED_RESOLVED_SCHEMES: &[&str] =
    &["javascript", "data", "vbscript", "file", "blob", "about"];

/// Render Markdown to a safe HTML fragment.
pub fn render_markdown(markdown: &str, opts: &RenderOptions) -> String {
    let events = collect_events(markdown);
    let mut r = Renderer {
        src: markdown,
        opts,
        out: String::with_capacity(markdown.len() * 2),
        in_code: false,
        quotes: Vec::new(),
        links: Vec::new(),
        aligns: Vec::new(),
        in_head: false,
        body_open: false,
        cell: 0,
    };
    r.run(&events);
    r.out
}

/// Render one block. A trailing `^anchor` is not shown.
pub fn render_block(block: &Block, opts: &RenderOptions) -> String {
    let mut text = block.text.as_str();
    if let Some(id) = &block.anchor
        && let Some(stripped) = text.trim_end().strip_suffix(&format!("^{id}"))
    {
        text = stripped.trim_end();
    }
    render_markdown(text, opts)
}

type Ev<'a> = (Event<'a>, Range<usize>);

/// Parse, merging adjacent literal text events so `[[a|b]]` is seen whole.
fn collect_events(src: &str) -> Vec<Ev<'_>> {
    let mut out: Vec<Ev<'_>> = Vec::new();
    for (ev, range) in Parser::new_ext(src, parser_options()).into_offset_iter() {
        if let Event::Text(t) = &ev
            && src.get(range.clone()) == Some(&**t)
            && let Some((Event::Text(prev), prev_range)) = out.last_mut()
            && prev_range.end == range.start
            && src.get(prev_range.clone()) == Some(&**prev)
        {
            let mut merged = prev.to_string();
            merged.push_str(t);
            *prev = merged.into();
            prev_range.end = range.end;
            continue;
        }
        out.push((ev, range));
    }
    out
}

fn esc_into(out: &mut String, s: &str) {
    for c in s.chars() {
        match c {
            '&' => out.push_str("&amp;"),
            '<' => out.push_str("&lt;"),
            '>' => out.push_str("&gt;"),
            '"' => out.push_str("&quot;"),
            '\'' => out.push_str("&#39;"),
            _ => out.push(c),
        }
    }
}

fn esc(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    esc_into(&mut out, s);
    out
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

/// `None` for unparseable or unsafe URLs, `Some(None)` for relative ones and
/// `Some(Some(scheme))` (lower-case) for absolute ones.
fn url_scheme(url: &str) -> Option<Option<String>> {
    if url.is_empty() || url.chars().any(char::is_control) {
        return None;
    }
    if url.starts_with("//") || url.starts_with('\\') {
        return None;
    }
    match url.find([':', '/', '?', '#', '\\']) {
        Some(i) if url.as_bytes()[i] == b':' => {
            let scheme = &url[..i];
            let mut chars = scheme.chars();
            let ok = chars.next().is_some_and(|c| c.is_ascii_alphabetic())
                && chars.all(|c| c.is_ascii_alphanumeric() || matches!(c, '+' | '-' | '.'));
            ok.then(|| Some(scheme.to_ascii_lowercase()))
        }
        _ => Some(None),
    }
}

/// Whether a URL returned by a resolver may be placed in an attribute.
fn resolved_url_safe(url: &str) -> bool {
    match url_scheme(url) {
        Some(Some(s)) => !BLOCKED_RESOLVED_SCHEMES.contains(&s.as_str()),
        Some(None) => true,
        None => false,
    }
}

fn is_image_target(target: &str) -> bool {
    extension(target).is_some_and(|e| IMAGE_EXTENSIONS.contains(&e.as_str()))
}

fn extension(target: &str) -> Option<String> {
    let name = target.rsplit(['/', '\\']).next().unwrap_or(target);
    name.rsplit_once('.').map(|(_, e)| e.to_ascii_lowercase())
}

fn non_empty(s: &str) -> Option<&str> {
    let t = s.trim();
    (!t.is_empty()).then_some(t)
}

fn is_tag_char(c: char) -> bool {
    c.is_alphanumeric() || matches!(c, '_' | '-' | '/')
}

struct Callout {
    kind: String,
    fold: &'static str,
    title: String,
    /// Index of the event that ends the title line.
    end: usize,
}

struct Renderer<'a, 'o> {
    src: &'a str,
    opts: &'a RenderOptions<'o>,
    out: String,
    in_code: bool,
    /// One entry per open block quote: true when rendered as a callout.
    quotes: Vec<bool>,
    /// Closing markup for each open link.
    links: Vec<&'static str>,
    aligns: Vec<Alignment>,
    in_head: bool,
    body_open: bool,
    cell: usize,
}

impl Renderer<'_, '_> {
    fn run(&mut self, events: &[Ev<'_>]) {
        let mut i = 0;
        while i < events.len() {
            let (ev, range) = &events[i];
            i += 1;
            match ev {
                Event::Start(Tag::Image {
                    dest_url, title, ..
                }) => {
                    let mut alt = String::new();
                    let mut depth = 1usize;
                    while i < events.len() {
                        match &events[i].0 {
                            Event::Start(_) => depth += 1,
                            Event::End(_) => {
                                depth -= 1;
                                if depth == 0 {
                                    i += 1;
                                    break;
                                }
                            }
                            Event::Text(t) | Event::Code(t) => alt.push_str(t),
                            Event::SoftBreak | Event::HardBreak => alt.push(' '),
                            _ => {}
                        }
                        i += 1;
                    }
                    self.image(dest_url, title, &alt);
                }
                Event::Start(Tag::BlockQuote(_)) => {
                    if let Some(c) = self.callout_at(events, i - 1, range) {
                        self.out.push_str("<div class=\"callout\" data-kind=\"");
                        esc_into(&mut self.out, &c.kind);
                        self.out.push_str("\" data-fold=\"");
                        self.out.push_str(c.fold);
                        self.out.push_str("\"><div class=\"callout-title\">");
                        esc_into(&mut self.out, &c.title);
                        self.out.push_str("</div><div class=\"callout-body\">");
                        self.quotes.push(true);
                        // events[end] is a break (the paragraph continues) or the paragraph end.
                        if !matches!(events[c.end].0, Event::End(TagEnd::Paragraph)) {
                            self.out.push_str("<p>");
                        }
                        i = c.end + 1;
                    } else {
                        self.out.push_str("<blockquote>");
                        self.quotes.push(false);
                    }
                }
                Event::Start(tag) => {
                    let task = matches!(events.get(i), Some((Event::TaskListMarker(_), _)));
                    self.start(tag, task);
                }
                Event::End(tag) => self.end(tag),
                Event::Text(t) => {
                    if self.in_code {
                        esc_into(&mut self.out, t);
                    } else {
                        self.text(t);
                    }
                }
                Event::Code(t) => {
                    self.out.push_str("<code>");
                    esc_into(&mut self.out, t);
                    self.out.push_str("</code>");
                }
                Event::Html(t)
                | Event::InlineHtml(t)
                | Event::InlineMath(t)
                | Event::DisplayMath(t) => esc_into(&mut self.out, t),
                Event::SoftBreak => self.out.push('\n'),
                Event::HardBreak => self.out.push_str("<br>"),
                Event::Rule => self.out.push_str("<hr>"),
                Event::FootnoteReference(name) => {
                    self.out
                        .push_str("<sup class=\"footnote-ref\" data-footnote=\"");
                    esc_into(&mut self.out, name);
                    self.out.push_str("\">");
                    esc_into(&mut self.out, name);
                    self.out.push_str("</sup>");
                }
                Event::TaskListMarker(checked) => {
                    let line = self.src[..range.start].matches('\n').count();
                    self.out.push_str("<input type=\"checkbox\" disabled");
                    if *checked {
                        self.out.push_str(" checked");
                    }
                    self.out.push_str(&format!(" data-line=\"{line}\">"));
                }
            }
        }
    }

    fn start(&mut self, tag: &Tag<'_>, task: bool) {
        match tag {
            Tag::Paragraph => self.out.push_str("<p>"),
            Tag::Heading { level, .. } => self.out.push_str(&format!("<h{}>", *level as usize)),
            Tag::CodeBlock(kind) => {
                self.in_code = true;
                let lang = match kind {
                    CodeBlockKind::Fenced(info) => info
                        .split_whitespace()
                        .next()
                        .map(|l| {
                            l.chars()
                                .filter(|c| {
                                    c.is_ascii_alphanumeric()
                                        || matches!(c, '_' | '+' | '-' | '.' | '#')
                                })
                                .collect::<String>()
                        })
                        .filter(|l| !l.is_empty()),
                    CodeBlockKind::Indented => None,
                };
                match lang {
                    Some(l) => self
                        .out
                        .push_str(&format!("<pre><code class=\"language-{}\">", esc(&l))),
                    None => self.out.push_str("<pre><code>"),
                }
            }
            Tag::HtmlBlock => self.out.push_str("<pre class=\"raw-html\">"),
            Tag::List(None) => self.out.push_str("<ul>"),
            Tag::List(Some(1)) => self.out.push_str("<ol>"),
            Tag::List(Some(n)) => self.out.push_str(&format!("<ol start=\"{n}\">")),
            Tag::Item => self.out.push_str(if task {
                "<li class=\"task-item\">"
            } else {
                "<li>"
            }),
            Tag::FootnoteDefinition(name) => {
                self.out
                    .push_str("<div class=\"footnote-definition\" data-footnote=\"");
                esc_into(&mut self.out, name);
                self.out.push_str("\">");
            }
            Tag::Table(aligns) => {
                self.aligns = aligns.clone();
                self.body_open = false;
                self.out.push_str("<table>");
            }
            Tag::TableHead => {
                self.in_head = true;
                self.cell = 0;
                self.out.push_str("<thead><tr>");
            }
            Tag::TableRow => {
                if !self.body_open {
                    self.body_open = true;
                    self.out.push_str("<tbody>");
                }
                self.cell = 0;
                self.out.push_str("<tr>");
            }
            Tag::TableCell => {
                self.out.push_str(if self.in_head { "<th" } else { "<td" });
                match self.aligns.get(self.cell) {
                    Some(Alignment::Left) => self.out.push_str(" class=\"align-left\""),
                    Some(Alignment::Center) => self.out.push_str(" class=\"align-center\""),
                    Some(Alignment::Right) => self.out.push_str(" class=\"align-right\""),
                    _ => {}
                }
                self.out.push('>');
                self.cell += 1;
            }
            Tag::Emphasis => self.out.push_str("<em>"),
            Tag::Strong => self.out.push_str("<strong>"),
            Tag::Strikethrough => self.out.push_str("<del>"),
            Tag::Link {
                link_type,
                dest_url,
                ..
            } => match self.link_open(*link_type, dest_url) {
                Some(open) => {
                    self.out.push_str(&open);
                    self.links.push("</a>");
                }
                None => self.links.push(""),
            },
            _ => {}
        }
    }

    fn end(&mut self, tag: &TagEnd) {
        match tag {
            TagEnd::Paragraph => self.out.push_str("</p>"),
            TagEnd::Heading(level) => self.out.push_str(&format!("</h{}>", *level as usize)),
            TagEnd::BlockQuote(_) => {
                if self.quotes.pop().unwrap_or(false) {
                    self.out.push_str("</div></div>");
                } else {
                    self.out.push_str("</blockquote>");
                }
            }
            TagEnd::CodeBlock => {
                self.in_code = false;
                self.out.push_str("</code></pre>");
            }
            TagEnd::HtmlBlock => self.out.push_str("</pre>"),
            TagEnd::List(true) => self.out.push_str("</ol>"),
            TagEnd::List(false) => self.out.push_str("</ul>"),
            TagEnd::Item => self.out.push_str("</li>"),
            TagEnd::FootnoteDefinition => self.out.push_str("</div>"),
            TagEnd::Table => {
                if self.body_open {
                    self.out.push_str("</tbody>");
                }
                self.body_open = false;
                self.out.push_str("</table>");
            }
            TagEnd::TableHead => {
                self.in_head = false;
                self.out.push_str("</tr></thead>");
            }
            TagEnd::TableRow => self.out.push_str("</tr>"),
            TagEnd::TableCell => self
                .out
                .push_str(if self.in_head { "</th>" } else { "</td>" }),
            TagEnd::Emphasis => self.out.push_str("</em>"),
            TagEnd::Strong => self.out.push_str("</strong>"),
            TagEnd::Strikethrough => self.out.push_str("</del>"),
            TagEnd::Link => {
                if let Some(c) = self.links.pop() {
                    self.out.push_str(c);
                }
            }
            _ => {}
        }
    }

    /// Detect `> [!kind]± Title` at the start of the quote that begins at `events[i]`.
    fn callout_at(&self, events: &[Ev<'_>], i: usize, range: &Range<usize>) -> Option<Callout> {
        if !matches!(events.get(i + 1)?.0, Event::Start(Tag::Paragraph)) {
            return None;
        }
        let line = self.src.get(range.start..)?.lines().next()?;
        let rest = line.trim_start().strip_prefix('>')?.trim_start();
        let rest = rest.strip_prefix("[!")?;
        let close = rest.find(']')?;
        let kind = &rest[..close];
        if kind.is_empty()
            || !kind
                .chars()
                .all(|c| c.is_ascii_alphanumeric() || matches!(c, '-' | '_'))
        {
            return None;
        }
        let mut after = &rest[close + 1..];
        let fold = match after.chars().next() {
            Some('-') => {
                after = &after[1..];
                "collapsed"
            }
            Some('+') => {
                after = &after[1..];
                "open"
            }
            _ => "none",
        };
        // The title line ends at the first break (or the paragraph end) at inline depth 0.
        let mut depth = 0usize;
        let mut end = None;
        for (j, (ev, _)) in events.iter().enumerate().skip(i + 2) {
            match ev {
                Event::Start(_) => depth += 1,
                Event::End(TagEnd::Paragraph) if depth == 0 => {
                    end = Some(j);
                    break;
                }
                Event::End(_) => depth = depth.checked_sub(1)?,
                Event::SoftBreak | Event::HardBreak => {
                    if depth != 0 {
                        return None;
                    }
                    end = Some(j);
                    break;
                }
                _ => {}
            }
        }
        let kind = kind.to_ascii_lowercase();
        let title = match non_empty(after) {
            Some(t) => t.to_string(),
            None => {
                let mut cs = kind.chars();
                cs.next()
                    .map(|f| f.to_uppercase().collect::<String>() + cs.as_str())
                    .unwrap_or_default()
            }
        };
        Some(Callout {
            kind,
            fold,
            title,
            end: end?,
        })
    }

    /// Opening `<a ...>` tag for a Markdown link, or `None` when the link must be dropped.
    fn link_open(&self, link_type: LinkType, dest: &str) -> Option<String> {
        let mut dest = dest.to_string();
        if link_type == LinkType::Email && !dest.contains(':') {
            dest = format!("mailto:{dest}");
        }
        match url_scheme(&dest)? {
            Some(s) if matches!(s.as_str(), "http" | "https" | "mailto") => Some(format!(
                "<a href=\"{}\" rel=\"noopener noreferrer\">",
                esc(&dest)
            )),
            Some(_) => None,
            None => {
                let (path, heading) = match dest.split_once('#') {
                    Some((p, h)) => (p, non_empty(h)),
                    None => (dest.as_str(), None),
                };
                if path.is_empty() {
                    return None;
                }
                let decoded = percent_decode(path);
                if decoded.to_ascii_lowercase().ends_with(".md") {
                    let resolved = (self.opts.resolve_link)(&decoded);
                    Some(wikilink_open(&decoded, heading, resolved.as_deref()))
                } else {
                    let url =
                        (self.opts.resolve_asset)(&decoded).filter(|u| resolved_url_safe(u))?;
                    Some(format!(
                        "<a class=\"file\" href=\"{}\" data-target=\"{}\" rel=\"noopener noreferrer\">",
                        esc(&url),
                        esc(&decoded)
                    ))
                }
            }
        }
    }

    fn image(&mut self, dest: &str, title: &str, alt: &str) {
        let src = match url_scheme(dest) {
            Some(Some(s)) if matches!(s.as_str(), "http" | "https") => Some(dest.to_string()),
            Some(None) => {
                (self.opts.resolve_asset)(&percent_decode(dest)).filter(|u| resolved_url_safe(u))
            }
            _ => None,
        };
        match src {
            Some(url) => {
                self.out.push_str("<img src=\"");
                esc_into(&mut self.out, &url);
                self.out.push_str("\" alt=\"");
                esc_into(&mut self.out, alt);
                self.out.push('"');
                if !title.is_empty() {
                    self.out.push_str(" title=\"");
                    esc_into(&mut self.out, title);
                    self.out.push('"');
                }
                self.out.push('>');
            }
            None => esc_into(&mut self.out, alt),
        }
    }

    /// Plain text with wikilinks, embeds and tags substituted.
    fn text(&mut self, text: &str) {
        let mut i = 0;
        let mut lit = 0;
        let mut prev: Option<char> = None;
        while i < text.len() {
            let Some(c) = text[i..].chars().next() else {
                break;
            };
            let rest = &text[i..];
            if rest.starts_with("[[") || rest.starts_with("![[") {
                let embed = c == '!';
                let after = i + if embed { 3 } else { 2 };
                if let Some(rel) = text[after..].find("]]") {
                    let inner = &text[after..after + rel];
                    if !inner.contains(['\n', '['])
                        && let Some(markup) = self.wikilink(inner, embed)
                    {
                        esc_into(&mut self.out, &text[lit..i]);
                        self.out.push_str(&markup);
                        i = after + rel + 2;
                        lit = i;
                        prev = Some(']');
                        continue;
                    }
                }
            }
            if c == '#' && prev.is_none_or(char::is_whitespace) {
                let start = i + 1;
                let mut end = start;
                for (j, d) in text[start..].char_indices() {
                    if is_tag_char(d) {
                        end = start + j + d.len_utf8();
                    } else {
                        break;
                    }
                }
                let name = text[start..end].trim_end_matches('/');
                if !name.is_empty() && name.chars().any(|ch| !ch.is_ascii_digit() && ch != '/') {
                    esc_into(&mut self.out, &text[lit..i]);
                    self.out.push_str("<span class=\"tag\" data-tag=\"");
                    esc_into(&mut self.out, name);
                    self.out.push_str("\">#");
                    esc_into(&mut self.out, name);
                    self.out.push_str("</span>");
                    i = start + name.len();
                    lit = i;
                    prev = Some('a');
                    continue;
                }
            }
            prev = Some(c);
            i += c.len_utf8();
        }
        esc_into(&mut self.out, &text[lit..]);
    }

    /// Markup for the inside of `[[...]]` / `![[...]]`, or `None` when it is not a link.
    fn wikilink(&self, inner: &str, embed: bool) -> Option<String> {
        let (left, alias) = match inner.split_once('|') {
            Some((l, a)) => (l, non_empty(a)),
            None => (inner, None),
        };
        let (target, heading) = match left.split_once('#') {
            Some((t, h)) => (t.trim(), non_empty(h)),
            None => (left.trim(), None),
        };
        if target.is_empty() {
            return None;
        }
        if !embed {
            let resolved = (self.opts.resolve_link)(target);
            let mut s = wikilink_open(target, heading, resolved.as_deref());
            esc_into(&mut s, alias.unwrap_or(target));
            s.push_str("</a>");
            return Some(s);
        }
        if is_image_target(target) {
            return Some(
                match (self.opts.resolve_asset)(target).filter(|u| resolved_url_safe(u)) {
                    Some(url) => {
                        let mut s = format!("<img src=\"{}\" alt=\"{}\"", esc(&url), esc(target));
                        if let Some(w) = alias
                            && w.len() <= 4
                            && w.chars().all(|c| c.is_ascii_digit())
                        {
                            s.push_str(&format!(" width=\"{w}\""));
                        }
                        s.push('>');
                        s
                    }
                    None => format!(
                        "<a class=\"embed unresolved\" data-target=\"{}\">{}</a>",
                        esc(target),
                        esc(target)
                    ),
                },
            );
        }
        // Non-image embed: placeholder link (transclusion happens later).
        let is_page = extension(target).is_none_or(|e| e == "md");
        let mut s = String::from("<a class=\"embed");
        let mut attrs = format!(" data-target=\"{}\"", esc(target));
        if is_page {
            match (self.opts.resolve_link)(target) {
                Some(p) => attrs.push_str(&format!(" data-path=\"{}\"", esc(&p))),
                None => s.push_str(" unresolved"),
            }
            if let Some(h) = heading {
                attrs.push_str(&format!(" data-heading=\"{}\"", esc(h)));
            }
        } else {
            match (self.opts.resolve_asset)(target).filter(|u| resolved_url_safe(u)) {
                Some(url) => attrs.push_str(&format!(" href=\"{}\"", esc(&url))),
                None => s.push_str(" unresolved"),
            }
        }
        s.push('"');
        s.push_str(&attrs);
        s.push('>');
        esc_into(&mut s, alias.unwrap_or(target));
        s.push_str("</a>");
        Some(s)
    }
}

/// Opening tag of a wikilink anchor.
fn wikilink_open(target: &str, heading: Option<&str>, resolved: Option<&str>) -> String {
    let mut s = String::from("<a class=\"wikilink");
    if resolved.is_none() {
        s.push_str(" unresolved");
    }
    s.push_str("\" data-target=\"");
    esc_into(&mut s, target);
    s.push('"');
    if let Some(p) = resolved {
        s.push_str(" data-path=\"");
        esc_into(&mut s, p);
        s.push('"');
    }
    if let Some(h) = heading {
        s.push_str(" data-heading=\"");
        esc_into(&mut s, h);
        s.push('"');
    }
    s.push('>');
    s
}
