//! Search query language.
//!
//! Plain words match by prefix, `"quoted phrases"` match exactly, `-word` excludes,
//! `tag:name` filters by tag (including nested children: `tag:area` matches `area/sub`),
//! `path:text` filters by path substring, `title:word` matches only titles.

/// A parsed search query.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct SearchQuery {
    /// Positive full-text terms (matched as prefixes).
    pub terms: Vec<String>,
    /// Exact phrases.
    pub phrases: Vec<String>,
    /// Terms that must not appear.
    pub excluded: Vec<String>,
    /// Terms matched against titles only.
    pub title_terms: Vec<String>,
    /// Required tags (normalised, without `#`).
    pub tags: Vec<String>,
    /// Required path substrings (normalised).
    pub paths: Vec<String>,
}

impl SearchQuery {
    /// Parse user input. Never fails; unknown syntax is treated as plain words.
    pub fn parse(input: &str) -> Self {
        let mut q = SearchQuery::default();
        let mut chars = input.chars().peekable();
        while let Some(&c) = chars.peek() {
            if c.is_whitespace() {
                chars.next();
                continue;
            }
            let negate = c == '-';
            if negate {
                chars.next();
            }
            let token = if chars.peek() == Some(&'"') {
                chars.next();
                let mut phrase = String::new();
                for ch in chars.by_ref() {
                    if ch == '"' {
                        break;
                    }
                    phrase.push(ch);
                }
                let phrase = phrase.trim().to_string();
                if !phrase.is_empty() {
                    if negate {
                        q.excluded.push(phrase);
                    } else {
                        q.phrases.push(phrase);
                    }
                }
                continue;
            } else {
                let mut word = String::new();
                while let Some(&ch) = chars.peek() {
                    if ch.is_whitespace() {
                        break;
                    }
                    word.push(ch);
                    chars.next();
                }
                word
            };
            if token.is_empty() {
                continue;
            }
            let lower = token.to_lowercase();
            if let Some(rest) = lower.strip_prefix("tag:") {
                let t = rest.trim_start_matches('#');
                if !t.is_empty() {
                    q.tags.push(t.to_string());
                }
            } else if let Some(rest) = lower.strip_prefix("path:") {
                if !rest.is_empty() {
                    q.paths.push(rest.replace('\\', "/"));
                }
            } else if let Some(rest) = token.strip_prefix("title:") {
                if !rest.is_empty() {
                    q.title_terms.push(rest.to_string());
                }
            } else if negate {
                q.excluded.push(token);
            } else {
                q.terms.push(token);
            }
        }
        q
    }

    /// True if the query has no full-text component (filters only, or nothing).
    pub fn is_filter_only(&self) -> bool {
        self.terms.is_empty() && self.phrases.is_empty() && self.title_terms.is_empty()
    }

    /// True if the query is completely empty.
    pub fn is_empty(&self) -> bool {
        self.is_filter_only() && self.tags.is_empty() && self.paths.is_empty()
    }

    /// Build an FTS5 MATCH expression. Every user string is quoted, so FTS5 syntax in the
    /// input cannot inject operators. Returns `None` when there is no full-text component.
    pub(crate) fn fts_expression(&self) -> Option<String> {
        if self.is_filter_only() {
            return None;
        }
        let mut parts: Vec<String> = Vec::new();
        for t in &self.terms {
            if let Some(tok) = fts_tokens(t, true) {
                parts.push(tok);
            }
        }
        for p in &self.phrases {
            if let Some(tok) = fts_tokens(p, false) {
                parts.push(tok);
            }
        }
        for t in &self.title_terms {
            if let Some(tok) = fts_tokens(t, true) {
                parts.push(format!("title : ({tok})"));
            }
        }
        if parts.is_empty() {
            return None;
        }
        let mut expr = parts.join(" AND ");
        for e in &self.excluded {
            if let Some(tok) = fts_tokens(e, false) {
                expr = format!("({expr}) NOT ({tok})");
            }
        }
        Some(expr)
    }
}

/// Split into FTS5 tokens (alphanumeric runs), quote each, optionally prefix-match the last.
fn fts_tokens(s: &str, prefix_last: bool) -> Option<String> {
    let words: Vec<&str> = s
        .split(|c: char| !c.is_alphanumeric())
        .filter(|w| !w.is_empty())
        .collect();
    if words.is_empty() {
        return None;
    }
    let mut out = format!("\"{}\"", words.join(" "));
    if prefix_last {
        out.push('*');
    }
    Some(out)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_operators() {
        let q = SearchQuery::parse(r#"rust "local first" -draft tag:#Area path:Daily title:plan"#);
        assert_eq!(q.terms, vec!["rust"]);
        assert_eq!(q.phrases, vec!["local first"]);
        assert_eq!(q.excluded, vec!["draft"]);
        assert_eq!(q.tags, vec!["area"]);
        assert_eq!(q.paths, vec!["daily"]);
        assert_eq!(q.title_terms, vec!["plan"]);
    }

    #[test]
    fn fts_expression_quotes_input() {
        let q = SearchQuery::parse("foo* OR bar");
        assert_eq!(
            q.fts_expression().as_deref(),
            Some(r#""foo"* AND "OR"* AND "bar"*"#)
        );
    }

    #[test]
    fn filter_only() {
        let q = SearchQuery::parse("tag:x");
        assert!(q.is_filter_only());
        assert!(q.fts_expression().is_none());
        assert!(SearchQuery::parse("   ").is_empty());
    }

    #[test]
    fn phrase_and_exclusion() {
        let q = SearchQuery::parse(r#""a b" -"c d""#);
        assert_eq!(
            q.fts_expression().as_deref(),
            Some(r#"("a b") NOT ("c d")"#)
        );
    }
}
