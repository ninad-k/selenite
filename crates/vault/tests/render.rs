//! Rendering tests: safety (XSS), wikilinks, embeds, tags, tasks, callouts, tables, code.

use selenite_vault::{RenderOptions, parse_blocks, render_block, render_markdown};

fn link(t: &str) -> Option<String> {
    match t {
        "Page" | "other.md" => Some("notes/Page.md".to_string()),
        _ => None,
    }
}

fn asset(t: &str) -> Option<String> {
    match t {
        "x.png" | "pics/a b.png" => Some("asset://localhost/x.png".to_string()),
        "doc.pdf" => Some("asset://localhost/doc.pdf".to_string()),
        "evil.png" => Some("javascript:alert(1)".to_string()),
        "evil2.png" => Some("data:image/svg+xml,<svg onload=alert(1)>".to_string()),
        _ => None,
    }
}

fn r(md: &str) -> String {
    let opts = RenderOptions {
        resolve_link: &link,
        resolve_asset: &asset,
    };
    render_markdown(md, &opts)
}

fn assert_inert(out: &str) {
    for bad in [
        "<script",
        "<img src=x",
        "<iframe",
        "<svg",
        "onerror=\"",
        "onclick=\"",
        "href=\"javascript",
    ] {
        assert!(!out.to_lowercase().contains(bad), "found `{bad}` in {out}");
    }
}

// ---- XSS ----

#[test]
fn xss_raw_html_block_is_escaped() {
    let out = r("<script>alert(1)</script>\n\ntext");
    assert!(
        out.contains("&lt;script&gt;alert(1)&lt;/script&gt;"),
        "{out}"
    );
    assert_inert(&out);
}

#[test]
fn xss_inline_html_is_escaped() {
    let out = r("a <img src=x onerror=alert(1)> b <b>bold</b>");
    assert!(out.contains("&lt;img src=x onerror=alert(1)&gt;"), "{out}");
    assert!(!out.contains("<b>"), "{out}");
    assert_inert(&out);
}

#[test]
fn xss_html_in_table_callout_heading_list() {
    for md in [
        "| a | b |\n|---|---|\n| <script>x</script> | <iframe src=y> |",
        "> [!note] <script>x</script>\n> <img src=x onerror=1>",
        "# <script>x</script>",
        "- <script>x</script>",
        "<div onclick=\"x\">y</div>",
        "<!-- c --><script>x</script>",
        "[^1]\n\n[^1]: <script>x</script>",
    ] {
        assert_inert(&r(md));
    }
}

#[test]
fn xss_dangerous_link_schemes_are_dropped() {
    for url in [
        "javascript:alert(1)",
        "JaVaScRiPt:alert(1)",
        "data:text/html,<script>alert(1)</script>",
        "vbscript:msgbox(1)",
        "file:///etc/passwd",
        "&#106;avascript:alert(1)",
        "&#x6A;avascript:alert(1)",
        "<java\tscript:alert(1)>",
        "< javascript:alert(1)>",
        "//evil.example/x",
        "ftp://host/x",
        "tel:123",
    ] {
        let out = r(&format!("[click me]({url})"));
        assert!(!out.contains("<a "), "link kept for `{url}`: {out}");
        assert!(out.contains("click me"), "text lost for `{url}`: {out}");
        assert_inert(&out);
    }
}

#[test]
fn xss_autolink_dangerous_scheme_dropped() {
    let out = r("<javascript:alert(1)> and <https://ok.example>");
    assert!(!out.contains("href=\"javascript"), "{out}");
    assert!(out.contains("href=\"https://ok.example\""), "{out}");
}

#[test]
fn xss_image_dangerous_sources_dropped() {
    for url in [
        "javascript:alert(1)",
        "data:image/svg+xml;base64,PHN2Zz4=",
        "data:text/html,x",
        "file:///c:/secret.png",
        "vbscript:x",
        "//evil.example/a.png",
    ] {
        let out = r(&format!("![alt text]({url})"));
        assert!(!out.contains("<img"), "image kept for `{url}`: {out}");
        assert!(out.contains("alt text"), "{out}");
    }
}

#[test]
fn xss_resolver_output_is_checked_too() {
    let out = r("![[evil.png]] ![[evil2.png]] ![e](evil.png) [f](evil.png)");
    assert!(!out.contains("javascript:"), "{out}");
    assert!(!out.contains("data:"), "{out}");
    assert!(!out.contains("<img"), "{out}");
}

#[test]
fn xss_attribute_values_are_escaped() {
    let out = r("[[a\" onmouseover=\"x|b\" onfocus=\"y]]");
    assert!(!out.contains("\" onmouseover=\""), "{out}");
    assert!(out.contains("&quot; onmouseover=&quot;"), "{out}");
    let out = r("[t](http://a.example/x \"ti\\\" onclick=\\\"y\")");
    assert!(!out.contains("onclick=\""), "{out}");
    let out = r("[t](http://a.example/\"onmouseover=\"x)");
    assert!(!out.contains("\"onmouseover=\""), "{out}");
    let out = r("![a\" onerror=\"x](http://a.example/p.png \"t\" onerror=\"y\")");
    assert!(!out.contains("onerror=\""), "{out}");
    let out = r("```\"><script>x</script>\ncode\n```");
    assert_inert(&out);
    let out = r("> [!no\"te] t\n> body");
    assert_inert(&out);
    let out = r("#a\"onclick=\"x #ok");
    assert!(!out.contains("onclick=\""), "{out}");
}

#[test]
fn xss_headings_have_no_ids() {
    for md in [
        "# Title",
        "# Title {#custom}",
        "## <b>x</b> {.cls #id}",
        "Setext\n======",
    ] {
        let out = r(md);
        assert!(!out.contains(" id="), "{out}");
        assert!(!out.contains("class=\"cls"), "{out}");
    }
}

#[test]
fn xss_code_is_escaped_and_not_substituted() {
    let out = r("```html\n<script>[[Page]] #tag</script>\n```\n\n`<b>[[Page]]</b>`");
    assert!(
        out.contains("&lt;script&gt;[[Page]] #tag&lt;/script&gt;"),
        "{out}"
    );
    assert!(
        out.contains("<code>&lt;b&gt;[[Page]]&lt;/b&gt;</code>"),
        "{out}"
    );
    assert!(!out.contains("wikilink"), "{out}");
    assert!(!out.contains("class=\"tag\""), "{out}");
}

#[test]
fn xss_footnote_names_escaped() {
    let out = r("x[^\"><script>]\n\n[^\"><script>]: note");
    assert_inert(&out);
}

#[test]
fn allowed_link_schemes_work() {
    let out = r("[a](http://a.example) [b](https://b.example/p?q=1&r=2) [c](mailto:me@a.example)");
    assert!(out.contains("<a href=\"http://a.example\""), "{out}");
    assert!(
        out.contains("href=\"https://b.example/p?q=1&amp;r=2\""),
        "{out}"
    );
    assert!(out.contains("href=\"mailto:me@a.example\""), "{out}");
    let out = r("<me@a.example>");
    assert!(out.contains("href=\"mailto:me@a.example\""), "{out}");
}

// ---- features ----

#[test]
fn wikilinks_resolved_alias_heading_unresolved() {
    let out = r("see [[Page]], [[Page|the page]], [[Page#Intro]] and [[Nope]]");
    assert!(
        out.contains(
            "<a class=\"wikilink\" data-target=\"Page\" data-path=\"notes/Page.md\">Page</a>"
        ),
        "{out}"
    );
    assert!(out.contains(">the page</a>"), "{out}");
    assert!(out.contains("data-heading=\"Intro\""), "{out}");
    assert!(
        out.contains("<a class=\"wikilink unresolved\" data-target=\"Nope\">Nope</a>"),
        "{out}"
    );
}

#[test]
fn relative_markdown_link_to_page() {
    let out = r("[x](other.md#H) [y](missing.md) [z](doc.pdf) [w](nofile.zip)");
    assert!(out.contains("data-path=\"notes/Page.md\""), "{out}");
    assert!(
        out.contains("wikilink unresolved\" data-target=\"missing.md\""),
        "{out}"
    );
    assert!(out.contains("href=\"asset://localhost/doc.pdf\""), "{out}");
    assert!(!out.contains("nofile.zip\""), "{out}");
}

#[test]
fn embeds() {
    let out = r("![[x.png]] ![[x.png|120]] ![[gone.png]] ![[Page]] ![[doc.pdf]]");
    assert!(
        out.contains("<img src=\"asset://localhost/x.png\" alt=\"x.png\">"),
        "{out}"
    );
    assert!(out.contains("width=\"120\""), "{out}");
    assert!(
        out.contains("class=\"embed unresolved\" data-target=\"gone.png\""),
        "{out}"
    );
    assert!(
        out.contains(
            "<a class=\"embed\" data-target=\"Page\" data-path=\"notes/Page.md\">Page</a>"
        ),
        "{out}"
    );
    assert!(
        out.contains("class=\"embed\" data-target=\"doc.pdf\" href=\"asset://localhost/doc.pdf\""),
        "{out}"
    );
}

#[test]
fn markdown_image_resolves_relative() {
    let out = r("![pic](pics/a%20b.png) ![remote](https://a.example/p.png \"T\")");
    assert!(
        out.contains("<img src=\"asset://localhost/x.png\" alt=\"pic\">"),
        "{out}"
    );
    assert!(
        out.contains("<img src=\"https://a.example/p.png\" alt=\"remote\" title=\"T\">"),
        "{out}"
    );
}

#[test]
fn tags() {
    let out = r("hello #todo and #a/b-c, not C#, not #123, `#code`\n\n#start");
    assert!(
        out.contains("<span class=\"tag\" data-tag=\"todo\">#todo</span>"),
        "{out}"
    );
    assert!(out.contains("data-tag=\"a/b-c\""), "{out}");
    assert!(out.contains("C#,"), "{out}");
    assert!(!out.contains("data-tag=\"123\""), "{out}");
    assert!(out.contains("<code>#code</code>"), "{out}");
    assert!(out.contains("data-tag=\"start\""), "{out}");
}

#[test]
fn task_items_carry_block_relative_line() {
    let out = r("- [ ] one\n- [x] two\n  - [ ] nested");
    assert!(
        out.contains("<input type=\"checkbox\" disabled data-line=\"0\">"),
        "{out}"
    );
    assert!(
        out.contains("<input type=\"checkbox\" disabled checked data-line=\"1\">"),
        "{out}"
    );
    assert!(out.contains("data-line=\"2\""), "{out}");
}

#[test]
fn callouts() {
    let out = r("> [!note] Title here\n> body *text*\n> more");
    assert_eq!(
        out,
        "<div class=\"callout\" data-kind=\"note\" data-fold=\"none\"><div class=\"callout-title\">Title here</div><div class=\"callout-body\"><p>body <em>text</em>\nmore</p></div></div>"
    );
    let out = r("> [!warning]- Careful\n> hidden");
    assert!(
        out.contains("data-kind=\"warning\" data-fold=\"collapsed\""),
        "{out}"
    );
    let out = r("> [!tip]+ Open one\n> shown");
    assert!(out.contains("data-fold=\"open\""), "{out}");
    let out = r("> [!info]\n> body");
    assert!(out.contains("callout-title\">Info<"), "{out}");
    let out = r("> [!note] Only title");
    assert!(out.contains("<div class=\"callout-body\"></div>"), "{out}");
    let out = r("> [!note] T\n>\n> second para\n>\n> - item");
    assert!(
        out.contains("<p>second para</p><ul><li>item</li></ul>"),
        "{out}"
    );
    let out = r("> plain quote\n> text");
    assert!(out.starts_with("<blockquote>"), "{out}");
}

#[test]
fn tables_strikethrough_footnotes_code() {
    let out = r("| a | b |\n|:--|--:|\n| 1 | ~~2~~ |");
    assert!(out.contains("<th class=\"align-left\">a</th>"), "{out}");
    assert!(
        out.contains("<td class=\"align-right\"><del>2</del></td>"),
        "{out}"
    );
    assert!(out.contains("<tbody>") && out.contains("</table>"), "{out}");
    let out = r("text[^n]\n\n[^n]: the note");
    assert!(
        out.contains("<sup class=\"footnote-ref\" data-footnote=\"n\">n</sup>"),
        "{out}"
    );
    assert!(out.contains("footnote-definition"), "{out}");
    let out = r("```rust\nfn a() -> &str { \"x\" }\n```");
    assert_eq!(
        out,
        "<pre><code class=\"language-rust\">fn a() -&gt; &amp;str { &quot;x&quot; }\n</code></pre>"
    );
    assert!(r("```\nplain\n```").starts_with("<pre><code>"));
}

#[test]
fn render_block_hides_anchor() {
    let blocks = parse_blocks("Hello world ^intro\n\nNext\n");
    let opts = RenderOptions {
        resolve_link: &link,
        resolve_asset: &asset,
    };
    assert_eq!(render_block(&blocks[0], &opts), "<p>Hello world</p>");
    assert_eq!(render_block(&blocks[1], &opts), "<p>Next</p>");
}
