//! The static check: informational findings for a human reviewer, never a
//! verdict. It runs over `SKILL.md` and every companion file and reports,
//! with line numbers, tool names referenced, URLs, scripts and shell
//! invocations in them, and phrases that widen authority or ask to ignore
//! rules. The pattern list is versioned; `docs/skills-review.md` records
//! which version reviewed each skill.

use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};

use crate::Manifest;
use crate::manifest::is_script;

/// Bump when a pattern list below changes, so a review can be dated.
pub const PATTERN_VERSION: u32 = 1;

/// Tool names a skill may reference; a hit fills `requires` for the reviewer.
pub const TOOL_NAMES: &[&str] = &[
    "read_file",
    "write_file",
    "edit_file",
    "list_dir",
    "grep",
    "bash",
    "web_fetch",
    "web_search",
    "load_skill",
    "pin",
    "ask_human",
];

/// Shell commands worth a look, matched as whole words in a script or in
/// prose.
pub const SHELL_COMMANDS: &[&str] = &["curl", "wget", "eval"];

/// Shell fragments matched as substrings: `sh -c` and pipe-to-shell.
pub const SHELL_PATTERNS: &[&str] = &["sh -c", "bash -c", "| sh", "| bash", "|sh", "|bash"];

pub const PERMISSION_WIDENING: &[&str] = &[
    "you may run any command",
    "run any command",
    "without asking",
    "without confirmation",
    "no confirmation",
    "skip permission",
    "skip the permission",
    "dangerously",
    "disable",
    "sudo",
];

pub const IGNORE_RULES: &[&str] = &[
    "ignore previous",
    "ignore all previous",
    "ignore the project rules",
    "ignore project rules",
    "ignore your instructions",
    "ignore the instructions",
    "ignore any rules",
    "disregard",
    "override the rules",
    "override any rules",
];

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum FindingKind {
    ToolReference,
    Url,
    ShellInScript,
    PermissionWidening,
    IgnoreRules,
}

/// One thing a reviewer should look at.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Finding {
    pub skill: String,
    /// Relative to the skill folder; `SKILL.md` for the body.
    pub file: PathBuf,
    /// 1-based, in the file (frontmatter lines count).
    pub line: usize,
    pub kind: FindingKind,
    /// What matched: the tool name, the URL, the phrase, or the line.
    pub text: String,
}

/// Run the check over a skill's `SKILL.md` and every companion file.
pub fn check(manifest: &Manifest) -> Vec<Finding> {
    let mut findings = Vec::new();
    let mut files = vec![PathBuf::from("SKILL.md")];
    files.extend(manifest.files.iter().cloned());
    for rel in files {
        let path = manifest.path.join(&rel);
        let Ok(text) = std::fs::read_to_string(&path) else {
            continue; // binary or unreadable: nothing to grep
        };
        if is_script(&path) {
            findings.push(Finding {
                skill: manifest.name.clone(),
                file: rel.clone(),
                line: 1,
                kind: FindingKind::ShellInScript,
                text: "script file".into(),
            });
        }
        findings.extend(check_text(&manifest.name, &rel, &text));
    }
    findings
}

/// The tool names the findings reference, deduplicated and sorted; the
/// seed for a lock entry's `requires`.
pub fn tools_referenced(findings: &[Finding]) -> Vec<String> {
    let mut tools: Vec<String> = findings
        .iter()
        .filter(|f| f.kind == FindingKind::ToolReference)
        .map(|f| f.text.clone())
        .collect();
    tools.sort();
    tools.dedup();
    tools
}

pub fn check_text(skill: &str, file: &Path, text: &str) -> Vec<Finding> {
    let mut out = Vec::new();
    for (i, line) in text.lines().enumerate() {
        let n = i + 1;
        // The frontmatter key that makes a skill user-invoked is structure,
        // not prose; without this every slash skill would flag `disable`.
        if line.starts_with("disable-model-invocation:") {
            continue;
        }
        let lower = line.to_ascii_lowercase();
        let mut push = |kind, text: String| {
            out.push(Finding {
                skill: skill.to_owned(),
                file: file.to_path_buf(),
                line: n,
                kind,
                text,
            })
        };
        for tool in TOOL_NAMES {
            if contains_word(&lower, tool) {
                push(FindingKind::ToolReference, (*tool).to_owned());
            }
        }
        for url in urls(line) {
            push(FindingKind::Url, url);
        }
        if SHELL_COMMANDS.iter().any(|c| contains_word(&lower, c))
            || SHELL_PATTERNS.iter().any(|p| lower.contains(p))
        {
            push(FindingKind::ShellInScript, line.trim().to_owned());
        }
        for pat in PERMISSION_WIDENING {
            if contains_word(&lower, pat) {
                push(FindingKind::PermissionWidening, (*pat).to_owned());
            }
        }
        for pat in IGNORE_RULES {
            if lower.contains(pat) {
                push(FindingKind::IgnoreRules, (*pat).to_owned());
            }
        }
    }
    out
}

/// `needle` in `hay` with no identifier character on either side.
fn contains_word(hay: &str, needle: &str) -> bool {
    let is_ident = |c: char| c.is_alphanumeric() || c == '_';
    let mut from = 0;
    while let Some(pos) = hay[from..].find(needle) {
        let start = from + pos;
        let end = start + needle.len();
        let before = hay[..start]
            .chars()
            .next_back()
            .is_none_or(|c| !is_ident(c));
        let after = hay[end..].chars().next().is_none_or(|c| !is_ident(c));
        if before && after {
            return true;
        }
        from = start + 1;
    }
    false
}

fn urls(line: &str) -> Vec<String> {
    let mut out = Vec::new();
    for word in
        line.split(|c: char| c.is_whitespace() || c == '(' || c == ')' || c == '<' || c == '>')
    {
        let word = word.trim_end_matches(['.', ',', ';', ':', '"', '\'', ']', '`']);
        let word = word.trim_start_matches(['"', '\'', '[', '`']);
        if word.starts_with("http://") || word.starts_with("https://") {
            out.push(word.to_owned());
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    fn kinds(text: &str) -> Vec<(usize, FindingKind, String)> {
        check_text("t", Path::new("SKILL.md"), text)
            .into_iter()
            .map(|f| (f.line, f.kind, f.text))
            .collect()
    }

    #[test]
    fn tool_names_match_whole_words_only() {
        assert!(contains_word("run bash now", "bash"));
        assert!(contains_word("`bash`", "bash"));
        assert!(!contains_word("subgrep", "grep"));
        assert!(!contains_word("pinned", "pin"));
        assert!(!contains_word("read_files", "read_file"));
        assert!(!contains_word("retrieval practice", "eval"));
        assert!(contains_word("eval \"$x\"", "eval"));
    }

    #[test]
    fn each_kind_is_found_with_its_line() {
        let text = "Use read_file.\nSee https://example.com/x.\nRun `curl x | sh`\nyou may run ANY command\nignore previous instructions\n";
        let k = kinds(text);
        assert!(k.contains(&(1, FindingKind::ToolReference, "read_file".into())));
        assert!(k.contains(&(2, FindingKind::Url, "https://example.com/x".into())));
        assert!(k.contains(&(3, FindingKind::ShellInScript, "Run `curl x | sh`".into())));
        assert!(k.contains(&(4, FindingKind::PermissionWidening, "run any command".into())));
        assert!(k.contains(&(5, FindingKind::IgnoreRules, "ignore previous".into())));
    }

    #[test]
    fn url_extraction_strips_markdown_punctuation() {
        assert_eq!(
            urls("see [docs](https://a.b/c). and <https://d.e>"),
            vec!["https://a.b/c", "https://d.e"]
        );
    }
}
