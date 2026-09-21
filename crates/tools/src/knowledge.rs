//! `search_knowledge`: ranked sections over a snapshot of a project's
//! knowledge folder. The runtime builds and refreshes the snapshot; the
//! tool never reads the filesystem itself. Ranking is an IDF-weighted
//! term count over sections: BM25's shape without length normalisation,
//! which a folder of a hundred sections does not need.

use std::sync::{Arc, Mutex};

use aigentic_core::{BoxFuture, RiskClass, Tool, ToolError, ToolOutput};
use schemars::JsonSchema;
use serde::Deserialize;

use crate::files::parse_args;
use crate::truncate::{DEFAULT_OUTPUT_CAP, truncate_output};

/// One heading's worth of a markdown file.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Section {
    /// The file, relative to the knowledge folder, with `/` separators.
    pub file: String,
    /// The heading text without its `#`s; the file name for text before
    /// the first heading.
    pub heading: String,
    /// The section's text including its heading line.
    pub text: String,
    /// 1-based line of the heading (or 1).
    pub line: usize,
}

impl Section {
    /// `path#heading`, how a hit names itself.
    pub fn locator(&self) -> String {
        format!("{}#{}", self.file, self.heading)
    }
}

/// Split a markdown file into sections at its headings.
pub fn split_sections(file: &str, text: &str) -> Vec<Section> {
    let mut sections: Vec<Section> = Vec::new();
    let mut current: Option<Section> = None;
    let mut in_fence = false;
    for (i, line) in text.lines().enumerate() {
        if line.trim_start().starts_with("```") {
            in_fence = !in_fence;
        }
        let heading = (!in_fence && line.starts_with('#'))
            .then(|| line.trim_start_matches('#').trim())
            .filter(|h| !h.is_empty() && line.chars().take_while(|c| *c == '#').count() <= 6);
        match heading {
            Some(h) => {
                if let Some(s) = current.take() {
                    sections.push(s);
                }
                current = Some(Section {
                    file: file.to_owned(),
                    heading: h.to_owned(),
                    text: format!("{line}\n"),
                    line: i + 1,
                });
            }
            None => match &mut current {
                Some(s) => {
                    s.text.push_str(line);
                    s.text.push('\n');
                }
                None if line.trim().is_empty() => {}
                None => {
                    current = Some(Section {
                        file: file.to_owned(),
                        heading: file.to_owned(),
                        text: format!("{line}\n"),
                        line: i + 1,
                    });
                }
            },
        }
    }
    if let Some(s) = current {
        sections.push(s);
    }
    for s in &mut sections {
        s.text = s.text.trim_end().to_owned();
    }
    sections
}

const STOP_WORDS: &[&str] = &[
    "a", "an", "and", "are", "as", "at", "be", "by", "for", "from", "how", "i", "in", "is", "it",
    "of", "on", "or", "that", "the", "this", "to", "we", "what", "when", "where", "which", "who",
    "with", "you", "your",
];

/// A word as the scorer compares it: lowercase, and a plural's trailing
/// `s` dropped past three letters so `invariant` meets `invariants`.
/// Crude on purpose; a stemmer is the step after measured misses.
fn norm(word: &str) -> &str {
    if word.len() > 3 && word.ends_with('s') && !word.ends_with("ss") {
        &word[..word.len() - 1]
    } else {
        word
    }
}

/// Lowercased alphanumeric terms, singularised, with stop words dropped.
pub fn terms(query: &str) -> Vec<String> {
    let mut out: Vec<String> = query
        .to_lowercase()
        .split(|c: char| !c.is_alphanumeric())
        .filter(|t| t.len() > 1 && !STOP_WORDS.contains(t))
        .map(|t| norm(t).to_owned())
        .collect();
    out.dedup();
    out
}

fn words(hay_lower: &str) -> impl Iterator<Item = &str> {
    hay_lower
        .split(|c: char| !c.is_alphanumeric())
        .filter(|w| !w.is_empty())
        .map(norm)
}

fn count_word(hay_lower: &str, term: &str) -> usize {
    words(hay_lower).filter(|w| *w == term).count()
}

/// BM25's term-frequency saturation and length normalisation. A long
/// section that mentions every term must not outrank a short one that
/// is about them (phase 4 acceptance item 18).
const BM25_K1: f64 = 1.2;
const BM25_B: f64 = 0.75;

fn word_count(hay_lower: &str) -> usize {
    words(hay_lower).count()
}

/// Sections scoring above zero for `query`, best first, ties in folder
/// order, at most `max_hits`. BM25 over whole-word matches in the
/// section's locator (`path#heading`) and text: IDF per term, term
/// frequency saturated by `k1` and normalised by the section's length
/// against the average. The locator counts so a query that names a
/// file or heading, as the index invites, finds it.
pub fn search<'a>(sections: &'a [Section], query: &str, max_hits: usize) -> Vec<&'a Section> {
    let terms = terms(query);
    if terms.is_empty() || sections.is_empty() {
        return Vec::new();
    }
    let lowered: Vec<String> = sections
        .iter()
        .map(|s| format!("{}\n{}", s.locator(), s.text).to_lowercase())
        .collect();
    let n = sections.len() as f64;
    let idf: Vec<f64> = terms
        .iter()
        .map(|t| {
            let df = lowered.iter().filter(|s| count_word(s, t) > 0).count() as f64;
            (1.0 + n / (1.0 + df)).ln()
        })
        .collect();
    let lengths: Vec<f64> = lowered.iter().map(|s| word_count(s) as f64).collect();
    let avg_len = (lengths.iter().sum::<f64>() / n).max(1.0);
    let mut scored: Vec<(f64, usize)> = lowered
        .iter()
        .enumerate()
        .map(|(i, s)| {
            let norm = 1.0 - BM25_B + BM25_B * lengths[i] / avg_len;
            let score: f64 = terms
                .iter()
                .zip(&idf)
                .map(|(t, w)| {
                    let tf = count_word(s, t) as f64;
                    w * tf * (BM25_K1 + 1.0) / (tf + BM25_K1 * norm)
                })
                .sum();
            (score, i)
        })
        .filter(|(score, _)| *score > 0.0)
        .collect();
    scored.sort_by(|a, b| b.0.partial_cmp(&a.0).unwrap().then(a.1.cmp(&b.1)));
    scored
        .into_iter()
        .take(max_hits)
        .map(|(_, i)| &sections[i])
        .collect()
}

/// The sections the tool searches; the runtime replaces the contents
/// when the folder changes.
pub type KnowledgeSnapshot = Arc<Mutex<Vec<Section>>>;

#[derive(Debug, Deserialize, JsonSchema)]
struct SearchArgs {
    /// Words to look for; sections mentioning more of them rank higher.
    query: String,
    /// At most this many sections (default from the project file).
    #[serde(default)]
    max_hits: Option<usize>,
}

/// Search the project's knowledge folder.
pub struct SearchKnowledgeTool {
    snapshot: KnowledgeSnapshot,
    max_hits: usize,
    output_cap: usize,
}

impl std::fmt::Debug for SearchKnowledgeTool {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("SearchKnowledgeTool")
            .field("max_hits", &self.max_hits)
            .finish()
    }
}

pub const SEARCH_KNOWLEDGE: &str = "search_knowledge";

impl SearchKnowledgeTool {
    pub fn new(snapshot: KnowledgeSnapshot, max_hits: usize) -> Self {
        Self {
            snapshot,
            max_hits,
            output_cap: DEFAULT_OUTPUT_CAP,
        }
    }
}

impl Tool for SearchKnowledgeTool {
    fn name(&self) -> &str {
        SEARCH_KNOWLEDGE
    }

    fn description(&self) -> &str {
        "Search the project's knowledge folder, whose files are listed in the system prompt. Returns the best-matching sections as `path#heading` followed by the text."
    }

    fn schema(&self) -> schemars::schema::RootSchema {
        schemars::schema_for!(SearchArgs)
    }

    fn risk_class(&self) -> RiskClass {
        RiskClass::Read
    }

    fn call(&self, args: serde_json::Value) -> BoxFuture<'_, Result<ToolOutput, ToolError>> {
        Box::pin(async move {
            let args: SearchArgs = parse_args(args)?;
            let max = args.max_hits.unwrap_or(self.max_hits).max(1);
            let sections = self.snapshot.lock().unwrap_or_else(|e| e.into_inner());
            let hits = search(&sections, &args.query, max);
            let content = if hits.is_empty() {
                format!("no sections match {:?}", args.query)
            } else {
                hits.iter()
                    .map(|s| format!("{}\n{}", s.locator(), s.text))
                    .collect::<Vec<_>>()
                    .join("\n\n---\n\n")
            };
            Ok(ToolOutput {
                content: truncate_output(&content, self.output_cap),
                is_error: false,
            })
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    const DOC: &str = "Intro line before any heading.\n\n# Deploys\n\nWe deploy on Fridays.\n```\n# not a heading\n```\n\n## Rollback\n\nRollback with vercel rollback.\n\n# Billing\n\nStripe handles billing and invoices.\n";

    #[test]
    fn splits_at_headings_and_keeps_fenced_hashes() {
        let s = split_sections("ops.md", DOC);
        let heads: Vec<(&str, usize)> = s.iter().map(|x| (x.heading.as_str(), x.line)).collect();
        assert_eq!(
            heads,
            vec![
                ("ops.md", 1),
                ("Deploys", 3),
                ("Rollback", 10),
                ("Billing", 14)
            ]
        );
        assert!(s[1].text.contains("# not a heading"));
        assert_eq!(s[0].locator(), "ops.md#ops.md");
        assert_eq!(s[2].locator(), "ops.md#Rollback");
        assert!(split_sections("e.md", "\n\n").is_empty());
    }

    #[test]
    fn ranks_more_terms_higher_and_drops_stop_words() {
        let s = split_sections("ops.md", DOC);
        assert_eq!(
            terms("How do we Rollback the deploy?"),
            vec!["do", "rollback", "deploy"]
        );
        let hits = search(&s, "rollback deploy", 5);
        assert_eq!(hits[0].heading, "Rollback", "mentions the rarer term");
        let hits = search(&s, "billing invoices", 5);
        assert_eq!(hits.len(), 1);
        assert_eq!(hits[0].heading, "Billing");
        assert!(search(&s, "the and of", 5).is_empty());
        assert!(search(&s, "kubernetes", 5).is_empty());
        assert_eq!(search(&s, "deploy rollback billing", 1).len(), 1);
    }

    #[test]
    fn locators_count_and_plurals_meet() {
        assert_eq!(
            terms("Invariants and the invariant class"),
            vec!["invariant", "class"]
        );
        let s = split_sections(
            "FOUNDATION.md",
            "# Product\n\nWhat it is.\n\n## Invariants\n\nEvery claim carries its evidence.\n",
        );
        let hits = search(&s, "foundation invariants", 5);
        assert_eq!(
            hits[0].locator(),
            "FOUNDATION.md#Invariants",
            "the locator is searchable"
        );
        let hits = search(&s, "invariant", 5);
        assert_eq!(hits.len(), 1, "singular finds the plural heading");
    }

    #[test]
    fn a_short_section_about_the_terms_beats_a_long_one_that_mentions_them() {
        // The shape from Vendela: a short invariants list holding the
        // answer, and a long context section that mentions the same
        // words twice among two hundred others.
        let filler = "lorem ipsum dolor sit amet consectetur ".repeat(30);
        let doc = format!(
            "# Invariants\n\nEvery claim carries its evidence: the provenance tag.\n\n# Context\n\n{filler}The provenance tag was earned; {filler}every claim carries its evidence, the provenance tag again. {filler}\n"
        );
        let s = split_sections("f.md", &doc);
        let hits = search(&s, "provenance tag evidence", 5);
        assert_eq!(hits.len(), 2);
        assert_eq!(hits[0].heading, "Invariants", "short and about it wins");
        assert_eq!(hits[1].heading, "Context");
    }

    #[tokio::test]
    async fn the_tool_returns_locators_and_text() {
        let snapshot: KnowledgeSnapshot = Arc::new(Mutex::new(split_sections("ops.md", DOC)));
        let tool = SearchKnowledgeTool::new(snapshot.clone(), 2);
        assert_eq!(tool.risk_class(), RiskClass::Read);
        let out = tool.call(json!({"query": "rollback"})).await.unwrap();
        assert!(
            out.content.starts_with("ops.md#Rollback\n## Rollback"),
            "{}",
            out.content
        );
        let out = tool.call(json!({"query": "nothing here"})).await.unwrap();
        assert!(out.content.starts_with("no sections match"));
        snapshot.lock().unwrap().clear();
        let out = tool.call(json!({"query": "rollback"})).await.unwrap();
        assert!(
            out.content.starts_with("no sections match"),
            "snapshot is shared"
        );
        assert!(matches!(
            tool.call(json!({})).await.unwrap_err(),
            ToolError::InvalidArgs(_)
        ));
    }
}
