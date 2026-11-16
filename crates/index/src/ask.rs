//! Retrieval with citations: [`Engine::ask`] turns a natural-language question into ranked
//! passages that an AI client can quote. No model runs here; answer generation is left to the
//! client (for example Claude over MCP).
//!
//! Pipeline: drop stopwords from the question, `OR` the remaining terms (prefix-matched) over the
//! `passages` FTS5 table (bm25, headings weighted above body text), then re-rank in Rust by term
//! coverage, term proximity, title and heading matches, and a small boost for pages one link hop
//! from the best page. Overlapping passages are merged away, at most [`MAX_PER_PAGE`] per page.
//! Ordering is deterministic: score, then path, then line.

use std::collections::{BTreeSet, HashMap};

use serde::Serialize;

use crate::db::PassageCandidate;
use crate::engine::Engine;
use crate::engine::Result;

/// Most passages returned from one page.
pub const MAX_PER_PAGE: usize = 3;
/// Most query terms used.
const MAX_TERMS: usize = 12;
/// Longest excerpt returned, in characters.
const MAX_EXCERPT_CHARS: usize = 1000;
/// Score added to passages from pages linked to or from the best page.
const LINK_BOOST: f64 = 0.08;

/// One cited passage.
#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct Citation {
    /// Vault-relative path of the page.
    pub path: String,
    /// Page title.
    pub title: String,
    /// Nearest heading above the passage.
    pub heading: Option<String>,
    /// 1-based first line of the passage in the file.
    pub start_line: u32,
    /// 1-based last line of the passage in the file (inclusive).
    pub end_line: u32,
    /// The passage text (truncated if very long).
    pub excerpt: String,
    /// Relevance in `0..=1.1`; higher is better.
    pub score: f64,
}

const STOPWORDS: &[&str] = &[
    "a", "about", "after", "again", "all", "also", "am", "an", "and", "any", "are", "as", "at",
    "be", "because", "been", "before", "being", "between", "both", "but", "by", "can", "could",
    "did", "do", "does", "doing", "done", "down", "during", "each", "few", "for", "from",
    "further", "get", "got", "had", "has", "have", "having", "he", "her", "here", "hers", "him",
    "his", "how", "i", "if", "in", "into", "is", "it", "its", "just", "me", "more", "most", "my",
    "no", "nor", "not", "of", "off", "on", "once", "only", "or", "other", "our", "ours", "out",
    "over", "own", "per", "s", "same", "she", "should", "so", "some", "such", "t", "than", "that",
    "the", "their", "theirs", "them", "then", "there", "these", "they", "this", "those", "through",
    "to", "too", "under", "until", "up", "us", "very", "was", "we", "were", "what", "when",
    "where", "which", "while", "who", "whom", "whose", "why", "will", "with", "would", "you",
    "your", "yours",
];

fn tokens(text: &str) -> Vec<String> {
    text.split(|c: char| !c.is_alphanumeric())
        .filter(|w| !w.is_empty())
        .map(str::to_lowercase)
        .collect()
}

/// The distinct, non-stopword terms of a question, in order of first appearance.
pub fn question_terms(question: &str) -> Vec<String> {
    let mut seen = BTreeSet::new();
    let mut out = Vec::new();
    for t in tokens(question) {
        let single_letter = t.chars().count() == 1 && !t.chars().all(|c| c.is_ascii_digit());
        if single_letter || STOPWORDS.contains(&t.as_str()) || !seen.insert(t.clone()) {
            continue;
        }
        out.push(t);
        if out.len() == MAX_TERMS {
            break;
        }
    }
    out
}

fn fts_expression(terms: &[String]) -> String {
    terms
        .iter()
        .map(|t| format!("\"{t}\"*"))
        .collect::<Vec<_>>()
        .join(" OR ")
}

/// Fraction of `terms` that prefix-match any of `toks`.
fn coverage(terms: &[String], toks: &[String]) -> f64 {
    if terms.is_empty() {
        return 0.0;
    }
    let hit = terms
        .iter()
        .filter(|t| toks.iter().any(|w| w.starts_with(t.as_str())))
        .count();
    hit as f64 / terms.len() as f64
}

/// `matched / window` where `window` is the length (in tokens) of the shortest stretch of
/// `toks` containing every matched term. Zero when fewer than two terms matched.
fn proximity(terms: &[String], toks: &[String]) -> f64 {
    let mut hits: Vec<(usize, usize)> = Vec::new();
    for (pos, w) in toks.iter().enumerate() {
        for (ti, t) in terms.iter().enumerate() {
            if w.starts_with(t.as_str()) {
                hits.push((pos, ti));
            }
        }
    }
    let present: BTreeSet<usize> = hits.iter().map(|&(_, t)| t).collect();
    if present.len() < 2 {
        return 0.0;
    }
    let mut counts: HashMap<usize, usize> = HashMap::new();
    let mut best = usize::MAX;
    let mut left = 0;
    for right in 0..hits.len() {
        *counts.entry(hits[right].1).or_insert(0) += 1;
        while counts.len() == present.len() {
            best = best.min(hits[right].0 - hits[left].0 + 1);
            let t = hits[left].1;
            if let Some(c) = counts.get_mut(&t) {
                *c -= 1;
                if *c == 0 {
                    counts.remove(&t);
                }
            }
            left += 1;
        }
    }
    if best == usize::MAX {
        return 0.0;
    }
    (present.len() as f64 / best as f64).min(1.0)
}

fn excerpt(text: &str) -> String {
    let text = text.trim();
    if text.chars().count() <= MAX_EXCERPT_CHARS {
        return text.to_string();
    }
    let cut: String = text.chars().take(MAX_EXCERPT_CHARS).collect();
    format!("{}…", cut.trim_end())
}

struct Scored {
    cand: PassageCandidate,
    base: f64,
    score: f64,
}

fn round4(x: f64) -> f64 {
    (x * 10_000.0).round() / 10_000.0
}

impl Engine {
    /// Retrieve up to `limit` passages that best answer `question`, as citations.
    ///
    /// Returns an empty list when the question has no usable terms (only stopwords) or nothing
    /// matches. Never calls a model; the caller composes the answer from the excerpts.
    pub fn ask(&self, question: &str, limit: usize) -> Result<Vec<Citation>> {
        let limit = limit.max(1);
        let terms = question_terms(question);
        if terms.is_empty() {
            return Ok(Vec::new());
        }
        let candidates = self
            .index()
            .passage_candidates(&fts_expression(&terms), (limit * 10).max(60))?;
        if candidates.is_empty() {
            return Ok(Vec::new());
        }
        let best_bm = candidates.iter().map(|c| -c.bm25).fold(0.0_f64, f64::max);

        let mut scored: Vec<Scored> = candidates
            .into_iter()
            .map(|cand| {
                let bm = if best_bm > 0.0 {
                    (-cand.bm25).max(0.0) / best_bm
                } else {
                    0.0
                };
                let mut body = tokens(&cand.text);
                let head = cand.heading.as_deref().map(tokens).unwrap_or_default();
                body.extend(head.iter().cloned());
                let cov = coverage(&terms, &body);
                let prox = proximity(&terms, &tokens(&cand.text));
                let title_cov = coverage(&terms, &tokens(&cand.title));
                let head_cov = coverage(&terms, &head);
                let base =
                    0.40 * bm + 0.30 * cov + 0.10 * prox + 0.10 * title_cov + 0.10 * head_cov;
                Scored {
                    cand,
                    base,
                    score: base,
                }
            })
            .collect();

        // One-hop link boost around the page of the best passage.
        let top = scored
            .iter()
            .max_by(|a, b| {
                a.base
                    .total_cmp(&b.base)
                    .then_with(|| b.cand.path.cmp(&a.cand.path))
                    .then_with(|| b.cand.start_line.cmp(&a.cand.start_line))
            })
            .map(|s| s.cand.path.clone());
        if let Some(top) = top {
            let mut neighbours = BTreeSet::new();
            for l in self.outgoing(&top)? {
                neighbours.extend(l.resolved);
            }
            for b in self.backlinks(&top)? {
                neighbours.insert(b.source);
            }
            neighbours.remove(&top);
            for s in &mut scored {
                if neighbours.contains(&s.cand.path) {
                    s.score += LINK_BOOST;
                }
            }
        }

        scored.sort_by(|a, b| {
            b.score
                .total_cmp(&a.score)
                .then_with(|| a.cand.path.cmp(&b.cand.path))
                .then_with(|| a.cand.start_line.cmp(&b.cand.start_line))
        });

        let mut kept: Vec<Scored> = Vec::new();
        for s in scored {
            let same_page: Vec<&Scored> =
                kept.iter().filter(|k| k.cand.path == s.cand.path).collect();
            let overlaps = same_page.iter().any(|k| {
                s.cand.start_line <= k.cand.end_line && k.cand.start_line <= s.cand.end_line
            });
            if overlaps || same_page.len() >= MAX_PER_PAGE {
                continue;
            }
            kept.push(s);
            if kept.len() == limit {
                break;
            }
        }
        Ok(kept
            .into_iter()
            .map(|s| Citation {
                path: s.cand.path.as_str().to_string(),
                title: s.cand.title,
                heading: s.cand.heading,
                start_line: s.cand.start_line,
                end_line: s.cand.end_line,
                excerpt: excerpt(&s.cand.text),
                score: round4(s.score),
            })
            .collect())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn question_terms_drop_stopwords_and_dedupe() {
        assert_eq!(
            question_terms("What is the plan for the Berlin trip? Berlin!"),
            vec!["plan", "berlin", "trip"]
        );
        assert!(question_terms("what is the").is_empty());
        assert_eq!(question_terms("version 2 of x"), vec!["version", "2"]);
    }

    #[test]
    fn expression_quotes_and_prefixes() {
        let t = vec!["plan".to_string(), "trip".to_string()];
        assert_eq!(fts_expression(&t), "\"plan\"* OR \"trip\"*");
    }

    #[test]
    fn proximity_prefers_tight_matches() {
        let terms = vec!["alpha".to_string(), "beta".to_string()];
        let near = tokens("alpha beta gamma delta");
        let far = tokens("alpha x x x x x x beta");
        assert!(proximity(&terms, &near) > proximity(&terms, &far));
        assert_eq!(proximity(&terms, &tokens("alpha only")), 0.0);
    }
}
