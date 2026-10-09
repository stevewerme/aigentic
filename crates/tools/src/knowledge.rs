//! `search_knowledge`: ranked sections over a snapshot of a project's
//! knowledge folder, or over a corpus a resolver hands in for a sibling
//! project or the workspace. The runtime builds and refreshes the
//! snapshot and resolves the scopes; the tool never reads the
//! filesystem itself, and never builds a path from an argument. Ranking
//! is an IDF-weighted term count over sections: BM25's shape without
//! length normalisation, which a folder of a hundred sections does not
//! need.

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

/// Resolves a search scope to the corpus it covers. The runtime
/// implements this over the daemon's rows and workspace; a tool built
/// without one reaches no scope at all.
pub trait KnowledgeSources: Send + Sync {
    /// The label and sections of a scope. The label's first line names
    /// the scope (`q`, `workspace ops`) — the result marks itself with
    /// it and a miss names it — and a further line is a note for the
    /// reader, such as files the resolver had to leave out. An
    /// unresolvable scope is the message the call answers with.
    fn resolve(
        &self,
        project: Option<&str>,
        workspace: bool,
    ) -> Result<(String, Vec<Section>), String>;
}

/// Refuses every scope: what a tool with no resolver reaches. The tool
/// tests and any caller that has only a snapshot get this.
#[derive(Debug)]
struct NoSources;

impl KnowledgeSources for NoSources {
    fn resolve(
        &self,
        project: Option<&str>,
        workspace: bool,
    ) -> Result<(String, Vec<Section>), String> {
        let scope = match (project, workspace) {
            (Some(name), _) => format!("project `{name}`"),
            _ => "the workspace".to_owned(),
        };
        Err(format!(
            "this thread has no way to reach {scope}: only its own knowledge is searchable here"
        ))
    }
}

#[derive(Debug, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
struct SearchArgs {
    /// Words to look for; sections mentioning more of them rank higher.
    query: String,
    /// At most this many sections (default from the project file).
    #[serde(default)]
    max_hits: Option<usize>,
    /// A project this thread understands, by the name the system prompt
    /// lists it under; searches that project's knowledge and memory.
    #[serde(default)]
    project: Option<String>,
    /// Search the thread's workspace instead of a project.
    #[serde(default)]
    workspace: Option<bool>,
}

/// Search a knowledge folder and the memory beside it.
pub struct SearchKnowledgeTool {
    snapshot: KnowledgeSnapshot,
    max_hits: usize,
    output_cap: usize,
    sources: Arc<dyn KnowledgeSources>,
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
            sources: Arc::new(NoSources),
        }
    }

    /// Install the resolver that reaches a sibling project or the
    /// workspace. Without it, only the snapshot is searchable.
    pub fn with_sources(mut self, sources: Arc<dyn KnowledgeSources>) -> Self {
        self.sources = sources;
        self
    }
}

/// The read-only line a scoped result opens with, and any note the
/// resolver added after the scope's name.
fn scope_header(label: &str) -> String {
    let mut lines = label.lines();
    let mut header = format!("[read-only · {}]", lines.next().unwrap_or_default());
    for note in lines {
        header.push('\n');
        header.push_str(note);
    }
    header
}

impl Tool for SearchKnowledgeTool {
    fn name(&self) -> &str {
        SEARCH_KNOWLEDGE
    }

    fn description(&self) -> &str {
        "Search knowledge and memory for the sections matching `query`. `query` is required; `project` names another project this thread understands, by the name the system prompt lists it under; `workspace: true` searches the thread's workspace. Returns the best-matching sections as `path#heading` followed by the text."
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
            if args.project.is_some() && args.workspace == Some(true) {
                return Err(ToolError::InvalidArgs(
                    "`project` and `workspace` name one scope each: pass one of them, not both"
                        .into(),
                ));
            }
            let max = args.max_hits.unwrap_or(self.max_hits).max(1);
            let scope = if args.project.is_some() || args.workspace == Some(true) {
                match self
                    .sources
                    .resolve(args.project.as_deref(), args.workspace == Some(true))
                {
                    Ok(scope) => Some(scope),
                    Err(message) => {
                        return Ok(ToolOutput {
                            content: message,
                            is_error: true,
                        });
                    }
                }
            } else {
                None
            };
            let own;
            let sections: &[Section] = match &scope {
                Some((_, sections)) => sections,
                None => {
                    own = self.snapshot.lock().unwrap_or_else(|e| e.into_inner());
                    own.as_slice()
                }
            };
            let hits = search(sections, &args.query, max);
            let body = if hits.is_empty() {
                match &scope {
                    Some((label, _)) => format!(
                        "no sections match {:?} in {}",
                        args.query,
                        label.lines().next().unwrap_or_default()
                    ),
                    None => format!("no sections match {:?}", args.query),
                }
            } else {
                hits.iter()
                    .map(|s| format!("{}\n{}", s.locator(), s.text))
                    .collect::<Vec<_>>()
                    .join("\n\n---\n\n")
            };
            let content = match &scope {
                Some((label, _)) => format!("{}\n{body}", scope_header(label)),
                None => body,
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

    #[test]
    fn rejects_unknown_argument_keys() {
        let err = parse_args::<SearchArgs>(json!({
            "query": "deploys",
            "bogus": "leaked tool-call template",
        }))
        .unwrap_err();
        assert!(matches!(err, ToolError::InvalidArgs(_)), "{err:?}");
        assert!(err.to_string().contains("bogus"), "{err}");
    }

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

    /// A section with just enough of a body to be found.
    fn section(file: &str, heading: &str, text: &str) -> Section {
        Section {
            file: file.to_owned(),
            heading: heading.to_owned(),
            text: text.to_owned(),
            line: 1,
        }
    }

    /// The scopes a thread may reach, as the runtime would resolve them.
    #[derive(Debug)]
    struct Scopes {
        project: Option<(String, Vec<Section>)>,
        workspace: Option<(String, Vec<Section>)>,
    }

    impl KnowledgeSources for Scopes {
        fn resolve(
            &self,
            project: Option<&str>,
            workspace: bool,
        ) -> Result<(String, Vec<Section>), String> {
            if workspace {
                return self
                    .workspace
                    .clone()
                    .map(|(name, sections)| (format!("workspace {name}"), sections))
                    .ok_or_else(|| "this thread is in no workspace".to_owned());
            }
            let name = project.unwrap_or_default();
            if name.contains('/') {
                return Err(format!("`{name}` is a path, not a project name"));
            }
            match &self.project {
                Some((n, sections)) if n == name => Ok((n.clone(), sections.clone())),
                _ => Err(format!("{name} is not a project this thread understands")),
            }
        }
    }

    /// The tool over `DOC`, reaching a sibling `q` and the workspace `ops`.
    fn scoped() -> SearchKnowledgeTool {
        let sibling = vec![
            section("deploy.md", "Deploys", "We deploy on Fridays."),
            section(
                "memory/decisions.md",
                "Storage",
                "We chose Postgres for storage.",
            ),
        ];
        let workspace = vec![section(
            "speed.md",
            "Speed",
            "The workspace deploys twice a day.",
        )];
        SearchKnowledgeTool::new(Arc::new(Mutex::new(split_sections("ops.md", DOC))), 5)
            .with_sources(Arc::new(Scopes {
                project: Some(("q".to_owned(), sibling)),
                workspace: Some(("ops".to_owned(), workspace)),
            }))
    }

    #[test]
    fn the_schema_has_the_two_new_optional_properties() {
        let schema = SearchKnowledgeTool::new(Arc::new(Mutex::new(Vec::new())), 5).schema();
        let mut props: Vec<&str> = schema
            .schema
            .object
            .as_ref()
            .unwrap()
            .properties
            .keys()
            .map(String::as_str)
            .collect();
        props.sort_unstable();
        assert_eq!(props, vec!["max_hits", "project", "query", "workspace"]);
        let mut required: Vec<&str> = schema
            .schema
            .object
            .as_ref()
            .unwrap()
            .required
            .iter()
            .map(String::as_str)
            .collect();
        required.sort_unstable();
        assert_eq!(required, vec!["query"]);
        let bare = SearchKnowledgeTool::new(Arc::new(Mutex::new(Vec::new())), 5);
        let description = bare.description();
        for argument in ["query", "project", "workspace"] {
            assert!(description.contains(argument), "{description}");
        }
    }

    #[test]
    fn project_and_workspace_parse_and_a_stranger_key_does_not() {
        let args = parse_args::<SearchArgs>(json!({"query": "x", "project": "q"})).unwrap();
        assert_eq!(args.project.as_deref(), Some("q"));
        let args = parse_args::<SearchArgs>(json!({"query": "x", "workspace": true})).unwrap();
        assert_eq!(args.workspace, Some(true));
        let args = parse_args::<SearchArgs>(json!({"query": "x", "workspace": false})).unwrap();
        assert_eq!(args.workspace, Some(false));
        let err = parse_args::<SearchArgs>(json!({"query": "x", "scope": "q"})).unwrap_err();
        assert!(err.to_string().contains("scope"), "{err}");
    }

    #[tokio::test]
    async fn passing_both_project_and_workspace_is_refused() {
        let tool = scoped();
        let err = tool
            .call(json!({"query": "deploy", "project": "q", "workspace": true}))
            .await
            .unwrap_err();
        assert!(matches!(err, ToolError::InvalidArgs(_)), "{err:?}");
    }

    #[tokio::test]
    async fn workspace_false_is_the_same_call_as_absent() {
        let tool = scoped();
        let absent = tool.call(json!({"query": "rollback"})).await.unwrap();
        let false_ = tool
            .call(json!({"query": "rollback", "workspace": false}))
            .await
            .unwrap();
        assert_eq!(absent.content, false_.content);
        assert!(!false_.content.starts_with("[read-only"));
    }

    #[tokio::test]
    async fn a_project_scope_marks_its_hits_read_only_and_locates_memory() {
        let out = scoped()
            .call(json!({"query": "storage postgres", "project": "q"}))
            .await
            .unwrap();
        assert!(
            out.content
                .starts_with("[read-only · q]\nmemory/decisions.md#Storage\n"),
            "{}",
            out.content
        );
        let out = scoped()
            .call(json!({"query": "fridays deploy", "project": "q"}))
            .await
            .unwrap();
        assert!(
            out.content
                .starts_with("[read-only · q]\ndeploy.md#Deploys"),
            "{}",
            out.content
        );
    }

    #[tokio::test]
    async fn the_workspace_scope_names_the_workspace() {
        let out = scoped()
            .call(json!({"query": "deploys twice", "workspace": true}))
            .await
            .unwrap();
        assert!(
            out.content
                .starts_with("[read-only · workspace ops]\nspeed.md#Speed"),
            "{}",
            out.content
        );
    }

    #[tokio::test]
    async fn a_miss_names_the_scope_it_searched() {
        let out = scoped()
            .call(json!({"query": "kubernetes", "project": "q"}))
            .await
            .unwrap();
        assert!(!out.is_error);
        assert_eq!(
            out.content,
            "[read-only · q]\nno sections match \"kubernetes\" in q"
        );
    }

    #[tokio::test]
    async fn a_scope_the_resolver_cannot_reach_is_an_error() {
        let out = scoped()
            .call(json!({"query": "x", "workspace": true}))
            .await
            .unwrap();
        assert!(
            out.content.starts_with("[read-only · workspace ops]"),
            "{}",
            out.content
        );
        let tool = SearchKnowledgeTool::new(Arc::new(Mutex::new(Vec::new())), 5).with_sources(
            Arc::new(Scopes {
                project: None,
                workspace: None,
            }),
        );
        let out = tool
            .call(json!({"query": "x", "workspace": true}))
            .await
            .unwrap();
        assert!(out.is_error);
        assert!(out.content.contains("no workspace"), "{}", out.content);
        let out = tool
            .call(json!({"query": "x", "project": "q"}))
            .await
            .unwrap();
        assert!(out.is_error);
        assert!(
            out.content
                .contains("not a project this thread understands"),
            "{out:?}"
        );
    }

    #[tokio::test]
    async fn a_path_like_project_name_is_refused() {
        let out = scoped()
            .call(json!({"query": "x", "project": "../q"}))
            .await
            .unwrap();
        assert!(out.is_error);
        assert!(
            out.content.contains("is a path, not a project name"),
            "{}",
            out.content
        );
    }

    #[tokio::test]
    async fn a_tool_with_no_resolver_refuses_a_scope() {
        let tool = SearchKnowledgeTool::new(Arc::new(Mutex::new(split_sections("ops.md", DOC))), 5);
        let out = tool
            .call(json!({"query": "rollback", "project": "q"}))
            .await
            .unwrap();
        assert!(out.is_error);
        assert!(out.content.contains("no way to reach"), "{}", out.content);
        let out = tool.call(json!({"query": "rollback"})).await.unwrap();
        assert!(
            out.content.starts_with("ops.md#Rollback"),
            "{}",
            out.content
        );
    }
}
