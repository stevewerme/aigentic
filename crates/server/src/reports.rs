//! The text reports a client prints, rendered by the daemon so every
//! client shows the same thing: `/cost`, `/project`, `/policy`,
//! `/memory`, `/skills`, `/who`. Moved from the terminal binary in phase
//! 5 step 9, where `project show` still uses them directly.

use std::collections::BTreeMap;
use std::fmt;
use std::path::{Path, PathBuf};

use aigentic_api::ReportKind;
use aigentic_runtime::aigentic_core::{Author, Event, EventKind};
use aigentic_runtime::aigentic_log::{
    AssistantMessagePayload, CompactedPayload, CompactionStrategy, MemoryExtractedPayload,
};
use aigentic_runtime::aigentic_policy::Decision;
use aigentic_runtime::project::{DOT_DIR, INSTRUCTIONS_FILE};
use aigentic_runtime::{Decided, KnowledgeMode, Layers, Mode, Project, Runtime};

use crate::actor::Reports;

/// The daemon's renderer: every kind, with the global instructions path
/// for `/project`.
pub struct DefaultReports {
    pub global_instructions: PathBuf,
}

impl Reports for DefaultReports {
    fn render(&self, runtime: &Runtime, events: &[Event], kind: ReportKind) -> String {
        match kind {
            ReportKind::Cost => cost_of(events).to_string(),
            ReportKind::Project => project_report(runtime, &self.global_instructions),
            ReportKind::Policy => policy_report(runtime),
            ReportKind::Memory => memory_report(runtime, events),
            ReportKind::Skills => skills_report(runtime),
            ReportKind::Who => who_report(runtime),
            ReportKind::Diff => diff_report(runtime),
        }
    }
}

/// Token totals for a thread, with the estimated share kept apart.
///
/// `spent`, `priced_calls`, `unpriced_calls` and `models` come from the
/// `usage` lines' `model`/`cost_usd`, which only lines written since
/// issue #31 carry: an old thread leaves them empty and prints exactly
/// what it printed before.
#[derive(Debug, Default, Clone, PartialEq)]
pub struct Cost {
    pub input: u64,
    pub output: u64,
    pub estimated_input: u64,
    pub estimated_output: u64,
    pub cache_read: u64,
    pub cache_write: u64,
    pub reasoning: u64,
    pub calls: u32,
    pub estimated_calls: u32,
    pub truncations: u32,
    pub summaries: u32,
    pub summary_input: u64,
    pub summary_output: u64,
    pub extractions: u32,
    pub extraction_lines: u32,
    pub extraction_input: u64,
    pub extraction_output: u64,
    /// Sum of `cost_usd` over the calls that carry one.
    pub spent: Option<f64>,
    /// Calls that carried a price / calls that did not.
    pub priced_calls: u32,
    pub unpriced_calls: u32,
    /// Model name to (calls, spend) in name order, name-less lines under
    /// the empty key. A model's spend is `None` while no call of it is
    /// priced.
    pub models: BTreeMap<String, (u32, Option<f64>)>,
}

/// Sum usage over every `assistant_message` in the log.
pub fn cost_of(events: &[Event]) -> Cost {
    let mut cost = Cost::default();
    for event in events.iter().filter(|e| e.kind == EventKind::Compacted) {
        let Ok(p) = serde_json::from_value::<CompactedPayload>(event.payload.clone()) else {
            continue;
        };
        match p.strategy {
            CompactionStrategy::TruncateResults { .. } => cost.truncations += 1,
            CompactionStrategy::Summary { usage, .. } => {
                cost.summaries += 1;
                cost.summary_input +=
                    usage.input_tokens + usage.cache_read_tokens + usage.cache_write_tokens;
                cost.summary_output += usage.output_tokens;
            }
        }
    }
    for event in events
        .iter()
        .filter(|e| e.kind == EventKind::MemoryExtracted)
    {
        let Ok(p) = serde_json::from_value::<MemoryExtractedPayload>(event.payload.clone()) else {
            continue;
        };
        cost.extractions += 1;
        cost.extraction_lines += p.written.len() as u32;
        cost.extraction_input +=
            p.usage.input_tokens + p.usage.cache_read_tokens + p.usage.cache_write_tokens;
        cost.extraction_output += p.usage.output_tokens;
    }
    for event in events
        .iter()
        .filter(|e| e.kind == EventKind::AssistantMessage)
    {
        let Ok(payload) = serde_json::from_value::<AssistantMessagePayload>(event.payload.clone())
        else {
            continue;
        };
        let Some(u) = payload.usage else { continue };
        if u.estimated {
            cost.estimated_input += u.input_tokens;
            cost.estimated_output += u.output_tokens;
            cost.estimated_calls += 1;
        } else {
            cost.input += u.input_tokens;
            cost.output += u.output_tokens;
            cost.cache_read += u.cache_read_tokens;
            cost.cache_write += u.cache_write_tokens;
            cost.reasoning += u.reasoning_tokens.unwrap_or(0);
            cost.calls += 1;
            // The price the runtime stamped on the line (issue #31): a
            // thread on an unpriced endpoint, or one written before this
            // existed, counts as unpriced rather than as free.
            match u.cost_usd {
                Some(usd) => {
                    cost.spent = Some(cost.spent.unwrap_or(0.0) + usd);
                    cost.priced_calls += 1;
                    let entry = cost
                        .models
                        .entry(u.model.clone().unwrap_or_default())
                        .or_insert((0, None));
                    entry.0 += 1;
                    entry.1 = Some(entry.1.unwrap_or(0.0) + usd);
                }
                None => {
                    cost.unpriced_calls += 1;
                    cost.models
                        .entry(u.model.clone().unwrap_or_default())
                        .or_insert((0, None))
                        .0 += 1;
                }
            }
        }
    }
    cost
}

impl fmt::Display for Cost {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        writeln!(
            f,
            "reported   in {:>9}  out {:>9}  ({} calls)",
            self.input, self.output, self.calls
        )?;
        writeln!(
            f,
            "estimated  in {:>9}  out {:>9}  ({} calls without provider usage)",
            self.estimated_input, self.estimated_output, self.estimated_calls
        )?;
        writeln!(
            f,
            "cache      read {:>7}  write {:>7}  (zero on backends without caching)",
            self.cache_read, self.cache_write
        )?;
        if self.reasoning > 0 {
            writeln!(
                f,
                "reasoning  {:>12}  (share of reported output tokens)",
                self.reasoning
            )?;
        }
        if self.truncations + self.summaries > 0 {
            writeln!(
                f,
                "compactions {} ({} truncate, {} summary)   summary tokens in {} out {}",
                self.truncations + self.summaries,
                self.truncations,
                self.summaries,
                self.summary_input,
                self.summary_output
            )?;
        }
        if self.extractions > 0 {
            writeln!(
                f,
                "memory     {} extractions, {} lines   tokens in {} out {}",
                self.extractions,
                self.extraction_lines,
                self.extraction_input,
                self.extraction_output
            )?;
        }
        // Only when the thread has prices to report, so a thread without
        // them is byte-identical to before (issue #31).
        if self.priced_calls + self.unpriced_calls > 0
            && (self.priced_calls > 0 || !self.models.is_empty())
        {
            match self.spent {
                Some(usd) => writeln!(
                    f,
                    "cost       ${usd:.4} ({} of {} calls priced)",
                    self.priced_calls,
                    self.priced_calls + self.unpriced_calls
                )?,
                None => writeln!(
                    f,
                    "cost       unpriced ({} calls, no usable price)",
                    self.unpriced_calls
                )?,
            }
            for (model, (calls, usd)) in &self.models {
                let name = if model.is_empty() {
                    "(no model)"
                } else {
                    model
                };
                match usd {
                    Some(usd) => writeln!(f, "  {name:<28} {calls:>5} calls  ${usd:.4}")?,
                    None => writeln!(f, "  {name:<28} {calls:>5} calls  unpriced")?,
                }
            }
        }
        write!(
            f,
            "total      in {:>9}  out {:>9}",
            self.input
                + self.cache_read
                + self.cache_write
                + self.estimated_input
                + self.summary_input
                + self.extraction_input,
            self.output + self.estimated_output + self.summary_output + self.extraction_output
        )
    }
}

/// What `/project` and `project show` print: the layers, the knowledge
/// mode, memory and skills, then every tool the registry and the harness
/// offer with the layer that decided it.
pub fn project_report(runtime: &Runtime, global_instructions: &Path) -> String {
    let layers = runtime.layers();
    let mut out = String::new();
    match &layers.project {
        Some(p) => out.push_str(&format!("project {} at {}\n", p.name, p.root.display())),
        None => out.push_str("no project (no aigentic.toml here or above)\n"),
    }
    out.push_str(&format!(
        "  global instructions   {}{}\n",
        global_instructions.display(),
        if layers.global.instructions.is_some() {
            ""
        } else {
            " (absent)"
        }
    ));
    if let Some(p) = &layers.project {
        out.push_str(&format!(
            "  project instructions  {}\n",
            instructions_source(p)
        ));
        let k = runtime.knowledge();
        let mode = match runtime.knowledge_mode() {
            KnowledgeMode::Inline => "inline",
            KnowledgeMode::Index => "index + search_knowledge",
        };
        out.push_str(&format!(
            "  knowledge             {} files, {} tokens, {mode}\n",
            k.files.len(),
            k.tokens
        ));
        let memory: Vec<String> = p
            .memory
            .iter()
            .map(|(name, text)| format!("{name} ({} lines)", text.lines().count()))
            .collect();
        out.push_str(&format!(
            "  memory                {}\n",
            if memory.is_empty() {
                "none".to_owned()
            } else {
                memory.join(", ")
            }
        ));
        let skills: Vec<String> = p
            .file
            .skills
            .enabled
            .iter()
            .map(|s| match layers.decided_skill(s) {
                Decided::Allowed => s.clone(),
                _ => format!("{s} (denied by global)"),
            })
            .collect();
        out.push_str(&format!(
            "  skills                {}\n",
            if skills.is_empty() {
                "none".to_owned()
            } else {
                skills.join(", ")
            }
        ));
    }
    let mut names = runtime.registry().names();
    names.extend(aigentic_runtime::harness_tools::harness_names());
    names.sort();
    names.dedup();
    out.push_str("tools\n");
    let width = names.iter().map(String::len).max().unwrap_or(0);
    let bash_visible = layers.decided_tool("bash") == Decided::Allowed;
    for name in &names {
        let fate = fate(layers, name);
        // Narrowing hides; it does not forbid. A hidden write tool with
        // the shell still visible is not a write ban (plan section 5).
        let note = if bash_visible
            && (name == "write_file" || name == "edit_file")
            && layers.decided_tool(name) != Decided::Allowed
        {
            "  (hidden, not a ban: bash is allowed)"
        } else {
            ""
        };
        out.push_str(&format!("  {name:<width$}  {fate}{note}\n"));
    }
    out.trim_end().to_owned()
}

fn fate(layers: &Layers, name: &str) -> &'static str {
    match layers.decided_tool(name) {
        Decided::Allowed => "allowed",
        Decided::DeniedByGlobal => "denied by global",
        Decided::NotInProjectAllow => "not in project allow",
    }
}

fn instructions_source(p: &Project) -> String {
    let ours = p.dot_dir().join(INSTRUCTIONS_FILE);
    if ours.is_file() {
        format!("{DOT_DIR}/{INSTRUCTIONS_FILE}")
    } else if p.root.join("AGENTS.md").is_file() {
        "AGENTS.md".into()
    } else {
        "none".into()
    }
}

fn author_name(author: &Author) -> &str {
    match author {
        Author::User(u) => u.0.as_str(),
        Author::Agent(a) => a.0.as_str(),
        Author::System => "system",
    }
}

/// `/policy`: the rule table in order with each rule's name, decision
/// and reason, the bash allow patterns, the mode, and the session grants
/// with who gave them.
pub fn policy_report(runtime: &Runtime) -> String {
    let policy = runtime.policy();
    let mut out = String::from("policy rules, first match wins\n");
    let width = policy
        .rules
        .iter()
        .map(|r| r.name().len())
        .max()
        .unwrap_or(0);
    for (i, rule) in policy.rules.iter().enumerate() {
        let decision = match rule.decision {
            Decision::Allow => "allow",
            Decision::Ask => "ask",
            Decision::Deny => "deny",
        };
        out.push_str(&format!(
            "  {:>2}. {:<width$}  {decision:<5}  {}\n",
            i + 1,
            rule.name(),
            rule.reason
        ));
    }
    out.push_str(&format!(
        "bash allow patterns: {}\n",
        if policy.bash_allow.is_empty() {
            "none".to_owned()
        } else {
            policy.bash_allow.join(", ")
        }
    ));
    out.push_str(&format!(
        "mode {}: {}\n",
        runtime.mode(),
        mode_meaning(runtime.mode())
    ));
    let grants = runtime.session_grants();
    if grants.is_empty() {
        out.push_str("session grants: none");
    } else {
        out.push_str("session grants (this session only)\n");
        for g in grants {
            let what = match &g.command {
                Some(c) => format!("bash {c:?}"),
                None => g.tool.clone(),
            };
            out.push_str(&format!("  {what}  by {}\n", author_name(&g.author)));
        }
    }
    out.trim_end().to_owned()
}

/// `/memory`: the memory files with line counts, the `through_seq` of
/// the last extraction in `events`, then the block as the prefix carries
/// it.
pub fn memory_report(
    runtime: &Runtime,
    events: &[aigentic_runtime::aigentic_core::Event],
) -> String {
    let Some(project) = runtime.layers().project.as_ref() else {
        return "no project: memory needs an aigentic.toml".to_owned();
    };
    let files: Vec<String> = project
        .memory
        .iter()
        .map(|(name, text)| {
            let lines = text.lines().filter(|l| !l.trim().is_empty()).count();
            format!("{name} ({lines} lines)")
        })
        .collect();
    let mut out = format!(
        "memory files in {}: {}\n",
        project.memory_dir().display(),
        if files.is_empty() {
            "none".to_owned()
        } else {
            files.join(", ")
        }
    );
    let last = events
        .iter()
        .rev()
        .find(|e| e.kind == EventKind::MemoryExtracted)
        .and_then(|e| serde_json::from_value::<MemoryExtractedPayload>(e.payload.clone()).ok());
    match last {
        Some(p) => out.push_str(&format!(
            "last extraction: through seq {}, {} lines written by {}\n",
            p.through_seq,
            p.written.len(),
            p.model
        )),
        None => out.push_str("last extraction: none in this thread\n"),
    }
    match project.memory_prefix() {
        Some(block) => {
            out.push_str("as the prefix carries it:\n");
            out.push_str(&block);
        }
        None => out.push_str("nothing in the prefix: every file is empty"),
    }
    out.trim_end().to_owned()
}

/// What the mode does, for `/mode` and the banner.
pub fn mode_meaning(mode: Mode) -> &'static str {
    match mode {
        Mode::Manual => "every ask goes to you",
        Mode::AcceptEdits => "writes run without asking; the shell still asks",
        Mode::Auto => "anything the rules would ask about runs; denials stand",
    }
}

/// The banner's mode note: nothing for `manual`.
pub fn mode_banner(mode: Mode) -> String {
    match mode {
        Mode::Manual => String::new(),
        other => format!(" · mode {other}"),
    }
}

/// `/skills`: the enabled set, user-invoked ones as slash commands.
pub fn skills_report(runtime: &Runtime) -> String {
    let set = runtime.skills();
    if set.is_empty() {
        return "no skills enabled; list them under [skills] in aigentic.toml".into();
    }
    let mut out = String::new();
    for m in set.user_invoked() {
        let hint = m.argument_hint.as_deref().unwrap_or("");
        out.push_str(&format!("/{:<24} {}  {}\n", m.name, m.description, hint));
    }
    for m in set.model_invoked() {
        out.push_str(&format!(
            " {:<24} {}  (model-invoked)\n",
            m.name, m.description
        ));
    }
    out.trim_end().to_owned()
}

/// `/who`: the project's participants and their roles.
pub fn who_report(runtime: &Runtime) -> String {
    match runtime.project() {
        None => "no project: no participants".into(),
        Some(p) => {
            let listed = p.file.participants.describe();
            if listed.is_empty() {
                "participants: none named in aigentic.toml; the daemon's owner alone, as admin"
                    .into()
            } else {
                format!("participants: {}", listed.join(", "))
            }
        }
    }
}

#[cfg(test)]
mod cost_tests {
    use super::*;
    use aigentic_runtime::aigentic_core::Author;
    use aigentic_runtime::aigentic_log::Usage;
    use serde_json::json;

    fn event(kind: EventKind, payload: serde_json::Value) -> Event {
        Event {
            id: ulid::Ulid::generate(),
            thread_id: ulid::Ulid::generate(),
            seq: 0,
            kind,
            author: Author::System,
            payload,
            parent_event: None,
            created_at: time::OffsetDateTime::now_utc(),
        }
    }

    fn assistant(input: u64, output: u64, estimated: bool) -> Event {
        let payload = AssistantMessagePayload {
            blocks: vec![],
            usage: Some(Usage {
                input_tokens: input,
                output_tokens: output,
                cache_read_tokens: if estimated { 0 } else { 50 },
                cache_write_tokens: if estimated { 0 } else { 5 },
                reasoning_tokens: if estimated { None } else { Some(3) },
                estimated,
                profile: None,
                model: None,
                effort: None,
                latency_ms: None,
                ttft_ms: None,
                cost_usd: None,
            }),
        };
        event(
            EventKind::AssistantMessage,
            serde_json::to_value(payload).unwrap(),
        )
    }

    #[test]
    fn sums_reported_and_estimated_separately() {
        let events = vec![
            event(EventKind::UserMessage, json!({"blocks": []})),
            assistant(100, 10, false),
            assistant(7, 7, true),
            assistant(200, 20, false),
            event(EventKind::TurnEnded, json!({"reason": "done"})),
            event(
                EventKind::Compacted,
                json!({"from_seq": 0, "to_seq": 4, "strategy": {"kind": "truncate_results", "max_bytes": 100}}),
            ),
            event(
                EventKind::Compacted,
                json!({"from_seq": 0, "to_seq": 4, "strategy": {"kind": "summary", "text": "s", "model": "m",
                       "usage": {"input_tokens": 1000, "output_tokens": 50, "cache_read_tokens": 0, "cache_write_tokens": 0, "reasoning_tokens": null, "estimated": false}}}),
            ),
            event(
                EventKind::MemoryExtracted,
                json!({"through_seq": 5, "written": [{"file": "decisions.md", "text": "x", "stated_by": {"kind": "system"}, "at_seq": 0}, {"file": "facts.md", "text": "y", "stated_by": {"kind": "system"}, "at_seq": 0}],
                       "model": "m", "usage": {"input_tokens": 300, "output_tokens": 20}}),
            ),
        ];
        let cost = cost_of(&events);
        assert_eq!(
            cost,
            Cost {
                input: 300,
                output: 30,
                estimated_input: 7,
                estimated_output: 7,
                cache_read: 100,
                cache_write: 10,
                reasoning: 6,
                calls: 2,
                estimated_calls: 1,
                truncations: 1,
                summaries: 1,
                summary_input: 1000,
                summary_output: 50,
                extractions: 1,
                extraction_lines: 2,
                extraction_input: 300,
                extraction_output: 20,
                // No usage line in this fixture carries a model or a
                // price, so nothing is claimed about spend (issue #31)
                // and the text report stays what it always was. The two
                // calls are counted under the empty model name: the
                // lines predate the field.
                spent: None,
                priced_calls: 0,
                unpriced_calls: 2,
                models: BTreeMap::from([(String::new(), (2, None))]),
            }
        );
        assert!(
            cost.to_string()
                .contains("memory     1 extractions, 2 lines   tokens in 300 out 20"),
            "{cost}"
        );
        let text_all = cost.to_string();
        assert!(
            text_all
                .contains("compactions 2 (1 truncate, 1 summary)   summary tokens in 1000 out 50"),
            "{text_all}"
        );
        let text = cost.to_string();
        assert!(text.contains("reported   in       300"), "{text}");
        assert!(text.contains("estimated  in         7"), "{text}");
        assert!(
            text.contains("cache      read     100  write      10"),
            "{text}"
        );
        assert!(text.contains("reasoning             6"), "{text}");
        assert!(text.contains("total      in      1717"), "{text}");
    }

    /// Issue #31: a stamped thread's `/cost` shows the spend the usage
    /// lines carry, the split of priced and unpriced calls, and one row
    /// per model. Every figure here is the fixture summed in-test.
    #[test]
    fn a_priced_thread_reports_its_spend_and_models() {
        let stamped = |model: &str, cost: f64| {
            let mut e = assistant(100, 10, false);
            let mut payload: AssistantMessagePayload =
                serde_json::from_value(e.payload.clone()).unwrap();
            let u = payload.usage.as_mut().unwrap();
            u.model = Some(model.to_owned());
            u.cost_usd = Some(cost);
            e.payload = serde_json::to_value(payload).unwrap();
            e
        };
        // Two model names — one twice for a summed row — and one more
        // line with a price but no model, which the report must count
        // and call "(no model)". An estimated line is neither: its
        // tokens are a guess, so it carries no price and no model row.
        let events = vec![
            stamped("gpt-4o", 0.25),
            stamped("gpt-4o", 0.50),
            stamped("claude-sonnet", 1.25),
            stamped("", 0.10),
            assistant(100, 10, true),
        ];
        let cost = cost_of(&events);

        // Recomputed from the fixture, not read off the code: the three
        // real calls are all priced here, the estimated one is neither
        // priced nor unpriced (it is counted as `estimated_calls`).
        let priced: u32 = 4;
        let unpriced: u32 = 0;
        let spent: f64 = 0.25 + 0.50 + 1.25 + 0.10;
        assert_eq!(cost.priced_calls, priced);
        assert_eq!(cost.unpriced_calls, unpriced);
        assert_eq!(cost.estimated_calls, 1);
        assert!((cost.spent.unwrap() - spent).abs() < 1e-9, "{cost:?}");
        let models: Vec<&str> = cost.models.keys().map(String::as_str).collect();
        assert_eq!(models, vec!["", "claude-sonnet", "gpt-4o"], "{models:?}");
        assert_eq!(cost.models["gpt-4o"], (2, Some(0.75)));
        assert_eq!(cost.models["claude-sonnet"], (1, Some(1.25)));
        assert_eq!(cost.models[""], (1, Some(0.10)));

        let text = cost.to_string();
        assert!(text.contains(&format!("cost       ${spent:.4}")), "{text}");
        assert!(
            text.contains(&format!("({priced} of {} calls priced)", priced + unpriced)),
            "{text}"
        );
        for name in ["gpt-4o", "claude-sonnet"] {
            assert!(text.contains(name), "{name} is missing: {text}");
        }
        assert!(text.contains("(no model)"), "{text}");
    }

    /// A line that predates the stamping fields is counted and called
    /// unpriced, never priced at a guess: its `/cost` keeps the old
    /// lines and adds one honest `unpriced` row.
    #[test]
    fn an_unstamped_line_is_counted_but_not_priced() {
        let cost = cost_of(&[assistant(100, 10, false), assistant(100, 10, false)]);
        assert_eq!(
            (cost.calls, cost.priced_calls, cost.unpriced_calls),
            (2, 0, 2)
        );
        assert_eq!(cost.spent, None);
        let text = cost.to_string();
        assert!(
            text.contains("cost       unpriced (2 calls, no usable price)"),
            "{text}"
        );
        assert!(text.contains("(2 calls)"), "{text}");
        // And an estimated-only thread says nothing about spend at all.
        let estimated = cost_of(&[assistant(100, 10, true)]);
        assert_eq!(estimated.spent, None);
        assert!(!estimated.to_string().contains("cost "), "{estimated}");
    }
}

/// `/diff`: `git diff` in the project root, then every untracked file as
/// a diff against nothing, so a new file reads as additions. A root
/// without a repository says so.
pub fn diff_report(runtime: &Runtime) -> String {
    let Some(project) = runtime.project() else {
        return "no project: nothing to diff".into();
    };
    diff_in(&project.root)
}

pub fn diff_in(root: &std::path::Path) -> String {
    use std::process::Command;
    let git = |args: &[&str]| -> Option<String> {
        let out = Command::new("git")
            .args(args)
            .current_dir(root)
            .output()
            .ok()?;
        out.status
            .success()
            .then(|| String::from_utf8_lossy(&out.stdout).into_owned())
    };
    if git(&["rev-parse", "--is-inside-work-tree"]).is_none() {
        return "no repository".into();
    }
    let mut text = git(&["diff"]).unwrap_or_default();
    let untracked = git(&["ls-files", "--others", "--exclude-standard"]).unwrap_or_default();
    for file in untracked.lines().filter(|l| !l.is_empty()) {
        // `--no-index` exits 1 when the files differ, so the status is
        // not the signal here.
        if let Ok(out) = Command::new("git")
            .args(["diff", "--no-index", "--", "/dev/null", file])
            .current_dir(root)
            .output()
        {
            text.push_str(&String::from_utf8_lossy(&out.stdout));
        }
    }
    if text.trim().is_empty() {
        "clean: nothing changed".into()
    } else {
        text
    }
}

#[cfg(test)]
mod diff_tests {
    use super::diff_in;

    fn git(root: &std::path::Path, args: &[&str]) {
        let ok = std::process::Command::new("git")
            .args(args)
            .current_dir(root)
            .output()
            .unwrap()
            .status
            .success();
        assert!(ok, "git {args:?}");
    }

    #[test]
    fn tracked_changes_and_untracked_files_both_show() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path();
        git(root, &["init", "-q"]);
        git(root, &["config", "user.email", "t@t"]);
        git(root, &["config", "user.name", "t"]);
        std::fs::write(root.join("a.txt"), "one\n").unwrap();
        git(root, &["add", "a.txt"]);
        git(root, &["commit", "-q", "-m", "a"]);
        assert_eq!(diff_in(root), "clean: nothing changed");
        std::fs::write(root.join("a.txt"), "two\n").unwrap();
        std::fs::write(root.join("b.txt"), "new\n").unwrap();
        let d = diff_in(root);
        assert!(d.contains("-one\n+two\n"), "{d}");
        assert!(d.contains("+new\n"), "{d}");
        assert!(d.contains("b.txt"), "{d}");
        let plain = tempfile::tempdir().unwrap();
        assert_eq!(diff_in(plain.path()), "no repository");
    }
}
